use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    sync::{
        atomic::{AtomicI64, AtomicU64, Ordering},
        Arc, Mutex as SyncMutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use btleplug::{
    api::{
        Central, CentralEvent, CharPropFlags, Characteristic, Descriptor, Manager as _,
        Peripheral as _, ScanFilter, WriteType,
    },
    platform::{Adapter, Manager, Peripheral},
};
use futures_util::StreamExt;
use serde_json::Number;
use tauri::async_runtime::JoinHandle as TauriJoinHandle;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::capabilities;
use crate::scan_plan::{decode_normalized_scan_query, diagnostic_scan_plan};
use crate::ATTACH_REQUEST_KIND;
use crate::{AuthenticatedCaller, DispatchFuture, IpcDispatcher, IpcEventSink, IpcValue};

const MAX_PENDING_EVENTS: usize = 256;
const MAX_CORRELATIONS: usize = 256;
const COMPLETED_CORRELATION_TTL: Duration = Duration::from_secs(30);
const MAX_QUARANTINE_WORKERS: usize = 4;
const MAX_QUARANTINE_ATTEMPTS: u32 = 8;
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Safety bound, not host policy.
///
/// btleplug's CoreBluetooth `disconnect()` never resolves when the peripheral
/// has already been dropped from its internal map, so an unbounded await hangs
/// the caller forever. This deadline converts that hang into a bounded,
/// classified outcome, and its expiry is reported as a cleanup failure rather
/// than swallowed.
///
/// It is deliberately not caller-tunable: it does not pace a peer's work, it
/// caps a wait on a future that may never complete. A slow-but-progressing
/// teardown is not misjudged by it, because btleplug maps a peripheral in the
/// disconnecting state to `is_connected() == false`, which this code treats as
/// released.
const DISCONNECT_COMPLETION_TIMEOUT: Duration = Duration::from_secs(1);

fn btleplug_runtime() -> tokio::runtime::Handle {
    static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("ubm-btleplug".to_owned())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .worker_threads(2)
                        .thread_name("ubm-btleplug-worker")
                        .build()
                        .expect("unified-ble-manager btleplug runtime");
                    tx.send(runtime.handle().clone())
                        .expect("unified-ble-manager btleplug handle");
                    runtime.block_on(std::future::pending::<()>());
                })
                .expect("unified-ble-manager btleplug thread");
            rx.recv().expect("unified-ble-manager btleplug handle")
        })
        .clone()
}

#[derive(Clone, Debug, Default)]
pub struct BtleplugDispatcherOptions {
    /// Exact `Adapter::adapter_info()` value to select when multiple adapters exist.
    pub adapter_id: Option<String>,
}

/// Production Tauri dispatcher backed by btleplug's CoreBluetooth, WinRT, and BlueZ hosts.
#[derive(Clone)]
pub struct BtleplugDispatcher {
    inner: Arc<Mutex<DispatcherState>>,
    bootstrap_admission: Arc<Mutex<()>>,
    next_id: Arc<AtomicU64>,
    next_revocation: Arc<AtomicU64>,
    started_at: Arc<Instant>,
    revoked_callers: Arc<SyncMutex<HashMap<String, u64>>>,
    options: BtleplugDispatcherOptions,
}

struct DispatcherState {
    manager: Option<Manager>,
    adapter: Option<Adapter>,
    attachment: Option<Attachment>,
    callers: HashMap<String, CallerState>,
    scan_owner: Option<String>,
    stopping_scan: Option<StoppingScan>,
    peer_owners: HashMap<String, String>,
    orphan_connections: HashMap<String, OrphanConnectionOwner>,
    orphan_subscription_owners: HashMap<String, OrphanSubscriptionOwner>,
    orphan_subscription_resources: HashMap<String, SubscriptionResource>,
}

#[derive(Clone)]
struct StoppingScan {
    caller_key: String,
    lease_id: String,
    lease_generation: String,
    handle: String,
}

#[derive(Clone)]
struct OrphanConnectionOwner {
    caller_key: String,
    #[allow(dead_code)]
    lease_id: String,
    lease_generation: String,
    peer_id: String,
    peripheral: Option<Peripheral>,
    attempts: u32,
}

struct OrphanSubscriptionOwner {
    caller_key: String,
    #[allow(dead_code)]
    lease_id: String,
    lease_generation: String,
    stream_id: String,
    attempts: u32,
}

#[derive(Clone)]
struct Attachment {
    attachment_id: String,
    backend_instance_id: String,
    backend_generation: String,
    adapter_id: String,
    adapter_name: String,
    adapter_generation: String,
}

struct CallerState {
    lease_id: String,
    lease_generation: String,
    versions: IpcValue,
    event_sink: IpcEventSink,
    scan_admitting: bool,
    scan: Option<ScanResource>,
    connections: HashMap<String, ConnectionResource>,
    databases: HashMap<String, DatabaseResource>,
    subscriptions: HashMap<String, SubscriptionResource>,
    connection_events: HashMap<String, ConnectionEventResource>,
    operations: HashMap<String, CancellationToken>,
    completed_correlations: HashMap<String, Instant>,
    pending_events: HashSet<String>,
    quarantine: QuarantineScheduler,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct QuarantineKey {
    command: String,
    handle: String,
}

struct QuarantineScheduler {
    keys: HashSet<QuarantineKey>,
    queue: std::collections::VecDeque<QuarantineKey>,
    jobs: HashMap<QuarantineKey, BTreeMap<String, IpcValue>>,
    attempts: HashMap<QuarantineKey, u32>,
    active_workers: usize,
    cancelled: bool,
    failures: Vec<QuarantineKey>,
}

enum QuarantineAdmit {
    Start,
    Coalesced,
    Queued,
    Cancelled,
}

impl QuarantineScheduler {
    fn new() -> Self {
        Self {
            keys: HashSet::new(),
            queue: std::collections::VecDeque::new(),
            jobs: HashMap::new(),
            attempts: HashMap::new(),
            active_workers: 0,
            cancelled: false,
            failures: Vec::new(),
        }
    }

    fn enqueue(
        &mut self,
        key: QuarantineKey,
        payload: BTreeMap<String, IpcValue>,
        queue_cap: usize,
    ) -> QuarantineAdmit {
        if self.cancelled {
            return QuarantineAdmit::Cancelled;
        }
        if self.keys.contains(&key) {
            return QuarantineAdmit::Coalesced;
        }
        self.keys.insert(key.clone());
        self.jobs.insert(key.clone(), payload);
        if self.active_workers >= MAX_QUARANTINE_WORKERS {
            let _ = queue_cap;
            self.queue.push_back(key);
            return QuarantineAdmit::Queued;
        }
        self.active_workers += 1;
        QuarantineAdmit::Start
    }

    fn record_attempt(&mut self, key: &QuarantineKey) -> u32 {
        let attempts = self.attempts.entry(key.clone()).or_insert(0);
        *attempts = attempts.saturating_add(1);
        *attempts
    }

    fn exhaust(
        &mut self,
        key: QuarantineKey,
    ) -> Option<(QuarantineKey, BTreeMap<String, IpcValue>)> {
        self.keys.remove(&key);
        self.attempts.remove(&key);
        self.jobs.remove(&key);
        self.failures.push(key);
        self.finish_worker()
    }

    fn succeed(
        &mut self,
        key: &QuarantineKey,
    ) -> Option<(QuarantineKey, BTreeMap<String, IpcValue>)> {
        self.keys.remove(key);
        self.attempts.remove(key);
        self.jobs.remove(key);
        self.failures.retain(|failure| failure != key);
        self.finish_worker()
    }

    fn finish_worker(&mut self) -> Option<(QuarantineKey, BTreeMap<String, IpcValue>)> {
        if self.active_workers > 0 {
            self.active_workers -= 1;
        }
        if self.cancelled {
            return None;
        }
        let next = self.queue.pop_front()?;
        let payload = self.jobs.get(&next).cloned()?;
        self.active_workers += 1;
        Some((next, payload))
    }

    fn cancel(&mut self) {
        self.cancelled = true;
        self.queue.clear();
        self.keys.clear();
        self.active_workers = 0;
    }
}

struct ScanResource {
    handle: String,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct ConnectionResource {
    peer_id: String,
    connection_id: String,
    owner_lease_id: String,
    connection_generation: String,
    peripheral: Peripheral,
}

struct DatabaseResource {
    connection_handle: String,
    database_id: String,
    database_generation: String,
    valid: bool,
    characteristics: HashMap<String, Characteristic>,
    descriptors: HashMap<String, Descriptor>,
}

struct SubscriptionResource {
    database_handle: String,
    body: SubscriptionBody,
    task: TauriJoinHandle<()>,
}

enum SubscriptionBody {
    Native {
        peripheral: Peripheral,
        characteristic: Characteristic,
    },
    #[cfg(test)]
    StandIn,
}

impl SubscriptionResource {
    fn native(&self) -> Option<(&Peripheral, &Characteristic)> {
        match &self.body {
            SubscriptionBody::Native {
                peripheral,
                characteristic,
            } => Some((peripheral, characteristic)),
            #[cfg(test)]
            SubscriptionBody::StandIn => None,
        }
    }
}

struct ConnectionEventResource {
    connection_handle: String,
    stream_handle: String,
    peer_id: String,
    connection_id: String,
    connection_generation: String,
    active: bool,
    sequence: u64,
    task: Option<TauriJoinHandle<()>>,
}

struct ConnectionEventIdentity<'a> {
    stream_id: &'a str,
    peer_id: &'a str,
    connection_id: &'a str,
    connection_generation: &'a str,
}

struct ScanObservation<'a> {
    owner: &'a str,
    expected_lease: (&'a str, &'a str),
    stream_handle: &'a str,
    requested_services: &'a [Uuid],
    local_name_prefix: Option<&'a str>,
    manufacturer_filters: &'a [ManufacturerFilter],
}

#[derive(Debug)]
struct DispatchError {
    code: &'static str,
    domain: &'static str,
    operation: String,
    platform: Option<String>,
    retryable: bool,
}

impl DispatchError {
    fn new(code: &'static str, domain: &'static str, operation: impl Into<String>) -> Self {
        Self {
            code,
            domain,
            operation: operation.into(),
            platform: None,
            retryable: matches!(code, "operation.aborted" | "operation.timed-out"),
        }
    }

    fn platform(mut self, message: impl Into<String>) -> Self {
        self.platform = Some(message.into());
        self
    }

    fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    fn into_response(self) -> IpcValue {
        let platform = self.platform.map_or(IpcValue::Null, |message| {
            object([
                ("domain", string("btleplug")),
                ("code", string("native-error")),
                ("safeMessage", string(message)),
                ("metadata", object([])),
            ])
        });
        object([
            ("kind", string("failure")),
            (
                "error",
                object([
                    ("code", string(self.code)),
                    ("domain", string(self.domain)),
                    ("operation", string(self.operation)),
                    ("platform", platform),
                    (
                        "retryability",
                        string(if self.retryable {
                            "caller-decides"
                        } else {
                            "never"
                        }),
                    ),
                ]),
            ),
        ])
    }
}

impl Default for BtleplugDispatcher {
    fn default() -> Self {
        Self::new(BtleplugDispatcherOptions::default())
    }
}

impl BtleplugDispatcher {
    pub fn new(options: BtleplugDispatcherOptions) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DispatcherState {
                manager: None,
                adapter: None,
                attachment: None,
                callers: HashMap::new(),
                scan_owner: None,
                stopping_scan: None,
                peer_owners: HashMap::new(),
                orphan_connections: HashMap::new(),
                orphan_subscription_owners: HashMap::new(),
                orphan_subscription_resources: HashMap::new(),
            })),
            bootstrap_admission: Arc::new(Mutex::new(())),
            next_id: Arc::new(AtomicU64::new(1)),
            next_revocation: Arc::new(AtomicU64::new(1)),
            started_at: Arc::new(Instant::now()),
            revoked_callers: Arc::new(SyncMutex::new(HashMap::new())),
            options,
        }
    }

    fn id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    async fn dispatch_request(
        &self,
        caller: AuthenticatedCaller,
        request: IpcValue,
        event_sink: Option<IpcEventSink>,
    ) -> Result<IpcValue, DispatchError> {
        let request = into_object(request, "tauri.request")?;
        let kind = required_string(&request, "kind", "tauri.request-kind")?;
        if kind != ATTACH_REQUEST_KIND && self.is_revoked(&caller_key(&caller)) {
            return Err(DispatchError::new(
                "ownership.denied",
                "ipc",
                "tauri.caller-revoked",
            ));
        }
        match kind.as_str() {
            ATTACH_REQUEST_KIND => {
                // Attaching is the one request that binds the event sink. A
                // caller that omits it could never receive events, so refuse
                // rather than attach a mute lease.
                let event_sink = event_sink.ok_or_else(|| {
                    DispatchError::new("protocol.malformed", "ipc", "tauri.bootstrap-event-channel")
                })?;
                let offer = into_object(
                    required_value(&request, "offer", "tauri.bootstrap-offer")?.clone(),
                    "tauri.bootstrap-offer",
                )?;
                self.bootstrap(caller, event_sink, offer).await
            }
            "route" => self.route(caller, request).await,
            "event.ack" => self.acknowledge(caller, request).await,
            "release" => self.release_request(caller, request).await,
            _ => Err(DispatchError::new(
                "protocol.malformed",
                "ipc",
                "tauri.request-kind",
            )),
        }
    }

    async fn ensure_adapter(&self) -> Result<Attachment, DispatchError> {
        {
            let state = self.inner.lock().await;
            if let Some(attachment) = &state.attachment {
                return Ok(attachment.clone());
            }
        }

        let requested = self.options.adapter_id.clone();
        let (manager, adapter, adapter_name) = btleplug_runtime()
            .spawn(async move { open_btleplug_adapter(requested).await })
            .await
            .map_err(|error| {
                DispatchError::new("adapter.unavailable", "adapter", "tauri.runtime")
                    .platform(error.to_string())
            })??;
        let attachment = Attachment {
            attachment_id: self.id("tauri-attachment"),
            backend_instance_id: self.id("tauri-btleplug"),
            backend_generation: self.id("tauri-backend-generation"),
            adapter_id: adapter_name.clone(),
            adapter_name,
            adapter_generation: self.id("tauri-adapter-generation"),
        };
        let mut state = self.inner.lock().await;
        if state.attachment.is_none() {
            state.manager = Some(manager);
            state.adapter = Some(adapter);
            state.attachment = Some(attachment.clone());
        }
        Ok(state.attachment.clone().unwrap_or(attachment))
    }

    async fn bootstrap(
        &self,
        caller: AuthenticatedCaller,
        event_sink: IpcEventSink,
        offer: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let _admission = self.bootstrap_admission.lock().await;
        let versions = negotiate_ipc_versions(&offer)?;
        let attachment = self.ensure_adapter().await?;
        let key = caller_key(&caller);
        let cleanup = self.release(&key).await;
        if !is_released(&cleanup) {
            return Err(DispatchError::new(
                "platform.failure",
                "cleanup",
                "tauri.bootstrap-prior-release",
            ));
        }
        let lease_id = self.id("tauri-lease");
        let lease_generation = self.id("tauri-lease-generation");
        self.revoked_callers
            .lock()
            .expect("revocation mutex poisoned")
            .remove(&key);
        self.inner.lock().await.callers.insert(
            key,
            CallerState {
                lease_id: lease_id.clone(),
                lease_generation: lease_generation.clone(),
                versions: versions.clone(),
                event_sink,
                scan_admitting: false,
                scan: None,
                connections: HashMap::new(),
                databases: HashMap::new(),
                subscriptions: HashMap::new(),
                connection_events: HashMap::new(),
                operations: HashMap::new(),
                completed_correlations: HashMap::new(),
                pending_events: HashSet::new(),
                quarantine: QuarantineScheduler::new(),
            },
        );

        let renderer = object([
            (
                "clientId",
                string(format!("{}:{}", caller.app_identifier, caller.window_label)),
            ),
            ("windowScope", string(caller.window_label)),
            ("sessionScope", string(lease_generation.clone())),
        ]);
        let attachment_record = attachment_record(&attachment);
        Ok(object([
            ("kind", string("bootstrap")),
            (
                "bootstrap",
                object([
                    ("attachment", attachment_record),
                    ("attachmentId", string(attachment.attachment_id)),
                    ("versions", versions),
                    (
                        "capabilities",
                        capabilities::snapshot(&attachment.backend_generation),
                    ),
                    ("discovery", object([("kind", string("continuous-scan"))])),
                    ("renderer", renderer),
                    (
                        "rendererLease",
                        object([
                            ("leaseId", string(lease_id)),
                            ("generation", string(lease_generation)),
                        ]),
                    ),
                ]),
            ),
        ]))
    }

    async fn route(
        &self,
        caller: AuthenticatedCaller,
        request: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let envelope = into_object(
            required_value(&request, "envelope", "tauri.route-envelope")?.clone(),
            "tauri.route-envelope",
        )?;
        let command = required_string(&envelope, "command", "tauri.route-command")?;
        let correlation = required_string(&envelope, "correlation", "tauri.route-correlation")?;
        let mut payload = into_object(
            required_value(&envelope, "payload", "tauri.route-payload")?.clone(),
            "tauri.route-payload",
        )?;
        let expected_lease = required_lease(&envelope, "tauri.route-lease")?;
        payload.insert(
            "__expectedLeaseId".to_owned(),
            string(expected_lease.0.clone()),
        );
        payload.insert(
            "__expectedLeaseGeneration".to_owned(),
            string(expected_lease.1.clone()),
        );
        let binary_payload = match envelope.get("binaryPayload") {
            Some(IpcValue::Bytes(bytes)) => Some(bytes.clone()),
            Some(IpcValue::Null) | None => None,
            _ => {
                return Err(DispatchError::new(
                    "bytes.invalid",
                    "ipc",
                    "tauri.route-binary",
                ))
            }
        };
        self.validate_envelope(&caller, &envelope).await?;

        if command == "operation.cancel" {
            let target = required_string(&payload, "targetCorrelation", "tauri.cancel")?;
            let state = self.inner.lock().await;
            let caller_state = state.callers.get(&caller_key(&caller)).ok_or_else(|| {
                DispatchError::new("ownership.denied", "ipc", "tauri.cancel-owner")
            })?;
            let cancellation = caller_state.operations.get(&target);
            if let Some(cancellation) = cancellation {
                cancellation.cancel();
            }
            return Ok(route_response(object([(
                "state",
                string(if cancellation.is_some() {
                    "cancellation-requested"
                } else {
                    "already-terminal"
                }),
            )])));
        }

        let cancellation = CancellationToken::new();
        {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(&caller_key(&caller)).ok_or_else(|| {
                DispatchError::new("ownership.denied", "ipc", "tauri.route-owner")
            })?;
            admit_caller_correlation(
                &caller_state.operations,
                &mut caller_state.completed_correlations,
                &correlation,
                &command,
                Instant::now(),
            )?;
            caller_state
                .operations
                .insert(correlation.clone(), cancellation.clone());
        }

        let operation_dispatcher = self.clone();
        let operation_caller = caller.clone();
        let operation_command = command.clone();
        let quarantine_lease = expected_lease.clone();
        let mut operation = tauri::async_runtime::spawn(async move {
            operation_dispatcher
                .execute(
                    &operation_caller,
                    &operation_command,
                    payload,
                    binary_payload,
                )
                .await
        });
        let result = tokio::select! {
            result = &mut operation => result.map_err(|error| {
                DispatchError::new("platform.failure", "ipc", format!("tauri.{command}.join"))
                    .platform(error.to_string())
            })?,
            () = cancellation.cancelled() => {
                let quarantine_dispatcher = self.clone();
                let quarantine_caller = caller.clone();
                let quarantine_command = command.clone();
                tauri::async_runtime::spawn(async move {
                    if let Ok(Ok(late_payload)) = operation.await {
                        quarantine_dispatcher
                            .quarantine_cancelled_success(
                                &quarantine_caller,
                                &quarantine_command,
                                &late_payload,
                                &quarantine_lease,
                            )
                            .await;
                    }
                });
                Err(DispatchError::new("operation.aborted", "ipc", format!("tauri.{command}")))
            },
        };
        if let Some(caller_state) = self
            .inner
            .lock()
            .await
            .callers
            .get_mut(&caller_key(&caller))
            .filter(|caller_state| {
                caller_state.lease_id == expected_lease.0
                    && caller_state.lease_generation == expected_lease.1
            })
        {
            remember_completed_correlation(
                &mut caller_state.operations,
                &mut caller_state.completed_correlations,
                correlation,
                Instant::now(),
            );
        }
        result.map(route_response)
    }

    async fn quarantine_cancelled_success(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        payload: &IpcValue,
        expected_lease: &(String, String),
    ) {
        let IpcValue::Object(payload) = payload else {
            return;
        };
        if command == "gatt.discover" {
            if let Some(handle) = payload.get("handle").and_then(as_string) {
                if let Some(caller_state) =
                    self.inner.lock().await.callers.get_mut(&caller_key(caller))
                {
                    if caller_state.lease_id == expected_lease.0
                        && caller_state.lease_generation == expected_lease.1
                    {
                        caller_state.databases.remove(handle);
                    }
                }
            }
            return;
        }
        let cleanup = match command {
            "scan.start" => payload.get("handle").and_then(as_string).map(|handle| {
                (
                    "scan.stop",
                    object([("scanHandle", string(handle.to_owned()))]),
                )
            }),
            "connection.connect" => connection_cleanup_payload(payload)
                .map(|cleanup_payload| ("connection.disconnect", cleanup_payload)),
            "gatt.subscribe" => payload.get("handle").and_then(as_string).map(|handle| {
                (
                    "gatt.unsubscribe",
                    object([("subscriptionHandle", string(handle.to_owned()))]),
                )
            }),
            _ => None,
        };
        if let Some((cleanup_command, IpcValue::Object(mut cleanup_payload))) = cleanup {
            cleanup_payload.insert(
                "__expectedLeaseId".to_owned(),
                string(expected_lease.0.clone()),
            );
            cleanup_payload.insert(
                "__expectedLeaseGeneration".to_owned(),
                string(expected_lease.1.clone()),
            );
            let handle =
                quarantine_handle(&cleanup_payload).unwrap_or_else(|| cleanup_command.to_owned());
            let should_start = if let Some(caller_state) =
                self.inner.lock().await.callers.get_mut(&caller_key(caller))
            {
                if caller_state.lease_id == expected_lease.0
                    && caller_state.lease_generation == expected_lease.1
                {
                    let queue_cap = caller_state
                        .connections
                        .len()
                        .saturating_add(caller_state.subscriptions.len())
                        .saturating_add(usize::from(caller_state.scan.is_some()))
                        .max(MAX_QUARANTINE_WORKERS);
                    matches!(
                        caller_state.quarantine.enqueue(
                            QuarantineKey {
                                command: cleanup_command.to_owned(),
                                handle,
                            },
                            cleanup_payload.clone(),
                            queue_cap,
                        ),
                        QuarantineAdmit::Start
                    )
                } else {
                    false
                }
            } else {
                false
            };
            if should_start {
                self.spawn_quarantine_worker(caller, cleanup_command, cleanup_payload);
            }
        }
    }

    fn spawn_quarantine_worker(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        payload: BTreeMap<String, IpcValue>,
    ) {
        let dispatcher = self.clone();
        let caller = caller.clone();
        let command = command.to_owned();
        btleplug_runtime().spawn(async move {
            dispatcher
                .retry_quarantined_cleanup(&caller, &command, payload)
                .await;
        });
    }

    async fn retry_quarantined_cleanup(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        payload: BTreeMap<String, IpcValue>,
    ) {
        let key = QuarantineKey {
            command: command.to_owned(),
            handle: quarantine_handle(&payload).unwrap_or_else(|| command.to_owned()),
        };
        let mut delay = Duration::ZERO;
        loop {
            if self.quarantine_cancelled(caller).await {
                return;
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            match self.execute(caller, command, payload.clone(), None).await {
                Ok(response) if cleanup_succeeded(&response) => {
                    self.complete_quarantine(caller, &key, true).await;
                    return;
                }
                Err(error) if error.code == "ownership.denied" => {
                    self.complete_quarantine(caller, &key, true).await;
                    return;
                }
                _ => {
                    let attempts = self.record_quarantine_attempt(caller, &key).await;
                    if attempts >= MAX_QUARANTINE_ATTEMPTS {
                        self.complete_quarantine(caller, &key, false).await;
                        return;
                    }
                    delay = if delay.is_zero() {
                        Duration::from_millis(100)
                    } else {
                        std::cmp::min(delay.saturating_mul(2), Duration::from_secs(5))
                    };
                }
            }
        }
    }

    async fn quarantine_cancelled(&self, caller: &AuthenticatedCaller) -> bool {
        self.inner
            .lock()
            .await
            .callers
            .get(&caller_key(caller))
            .map(|caller_state| caller_state.quarantine.cancelled)
            .unwrap_or(true)
    }

    async fn record_quarantine_attempt(
        &self,
        caller: &AuthenticatedCaller,
        key: &QuarantineKey,
    ) -> u32 {
        let mut state = self.inner.lock().await;
        let Some(caller_state) = state.callers.get_mut(&caller_key(caller)) else {
            return MAX_QUARANTINE_ATTEMPTS;
        };
        caller_state.quarantine.record_attempt(key)
    }

    async fn complete_quarantine(
        &self,
        caller: &AuthenticatedCaller,
        key: &QuarantineKey,
        success: bool,
    ) {
        let next = {
            let mut state = self.inner.lock().await;
            state
                .callers
                .get_mut(&caller_key(caller))
                .and_then(|caller_state| {
                    if success {
                        caller_state.quarantine.succeed(key)
                    } else {
                        caller_state.quarantine.exhaust(key.clone())
                    }
                })
        };
        if let Some((next_key, next_payload)) = next {
            self.spawn_quarantine_worker(caller, &next_key.command, next_payload);
        }
    }

    async fn validate_envelope(
        &self,
        caller: &AuthenticatedCaller,
        envelope: &BTreeMap<String, IpcValue>,
    ) -> Result<(), DispatchError> {
        let lease = into_object(
            required_value(envelope, "rendererLease", "tauri.route-lease")?.clone(),
            "tauri.route-lease",
        )?;
        let lease_id = required_string(&lease, "leaseId", "tauri.route-lease")?;
        let lease_generation = required_string(&lease, "generation", "tauri.route-lease")?;
        let attachment_id = required_string(envelope, "attachmentId", "tauri.route-attachment")?;
        let envelope_attachment = into_object(
            required_value(envelope, "attachment", "tauri.route-attachment")?.clone(),
            "tauri.route-attachment",
        )?;
        let renderer = into_object(
            required_value(envelope, "renderer", "tauri.route-renderer")?.clone(),
            "tauri.route-renderer",
        )?;
        let versions = required_value(envelope, "versions", "tauri.route-versions")?;
        let mut state = self.inner.lock().await;
        let attachment = state.attachment.as_ref().ok_or_else(|| {
            DispatchError::new("lifecycle.invalid-state", "ipc", "tauri.route-bootstrap")
        })?;
        let expected_attachment = into_object(
            attachment_record(attachment),
            "tauri.route-attachment-authority",
        )?;
        if attachment.attachment_id != attachment_id
            || !same_attachment_identity(&envelope_attachment, &expected_attachment)
        {
            return Err(DispatchError::new(
                "protocol.violation",
                "ipc",
                "tauri.route-attachment",
            ));
        }
        let caller_state = state
            .callers
            .get_mut(&caller_key(caller))
            .ok_or_else(|| DispatchError::new("ownership.denied", "ipc", "tauri.route-caller"))?;
        if caller_state.lease_id != lease_id || caller_state.lease_generation != lease_generation {
            return Err(DispatchError::new(
                "ownership.denied",
                "ipc",
                "tauri.route-lease",
            ));
        }
        if versions != &caller_state.versions
            || required_string(&renderer, "clientId", "tauri.route-renderer")?
                != format!("{}:{}", caller.app_identifier, caller.window_label)
            || required_string(&renderer, "windowScope", "tauri.route-renderer")?
                != caller.window_label
            || required_string(&renderer, "sessionScope", "tauri.route-renderer")?
                != caller_state.lease_generation
        {
            return Err(DispatchError::new(
                "protocol.violation",
                "ipc",
                "tauri.route-authority",
            ));
        }
        // The event sink is deliberately NOT reassigned here. It is bound once
        // by `bootstrap` and lives for the attachment; replacing it would drop
        // the previous Tauri Channel, and that drop ends the shared JS callback
        // which every later event depends on.
        let _ = caller_state;
        Ok(())
    }

    async fn validate_expected_lease(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<(), DispatchError> {
        let expected_id = required_string(payload, "__expectedLeaseId", "tauri.execute-lease")?;
        let expected_generation =
            required_string(payload, "__expectedLeaseGeneration", "tauri.execute-lease")?;
        let state = self.inner.lock().await;
        let caller_state = state.callers.get(&caller_key(caller)).ok_or_else(|| {
            DispatchError::new("ownership.denied", "ipc", "tauri.execute-lease-owner")
        })?;
        if caller_state.lease_id != expected_id
            || caller_state.lease_generation != expected_generation
        {
            return Err(DispatchError::new(
                "ownership.denied",
                "ipc",
                "tauri.execute-lease-stale",
            ));
        }
        Ok(())
    }

    async fn execute(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        payload: BTreeMap<String, IpcValue>,
        binary_payload: Option<Vec<u8>>,
    ) -> Result<IpcValue, DispatchError> {
        self.validate_expected_lease(caller, &payload).await?;
        match command {
            "adapter.state" => self.adapter_state().await,
            "scan.start" => self.start_scan(caller, payload).await,
            "scan.stop" => self.stop_scan(caller, payload).await,
            "connection.connect" => self.connect(caller, payload).await,
            "connection.disconnect" => self.disconnect(caller, payload).await,
            "connection.events.subscribe" => {
                self.subscribe_connection_events(caller, payload).await
            }
            "connection.events.ready" => self.ready_connection_events(caller, payload).await,
            "connection.events.unsubscribe" => {
                self.unsubscribe_connection_events(caller, payload).await
            }
            "connection.rssi" => self.read_rssi(caller, payload).await,
            "connection.maximum-write-length" => self.maximum_write_length(caller, payload).await,
            "gatt.discover" => self.discover(caller, payload).await,
            "gatt.database.release" => self.release_database(caller, payload).await,
            "gatt.read" => self.read(caller, payload).await,
            "gatt.write" => self.write(caller, payload, binary_payload).await,
            "gatt.subscribe" => self.subscribe(caller, payload).await,
            "gatt.unsubscribe" => self.unsubscribe(caller, payload).await,
            "gatt.descriptor.read" => self.read_descriptor(caller, payload).await,
            "gatt.descriptor.write" => self.write_descriptor(caller, payload, binary_payload).await,
            _ => Err(DispatchError::new(
                "argument.invalid",
                "ipc",
                "tauri.route-command",
            )),
        }
    }

    async fn adapter_state(&self) -> Result<IpcValue, DispatchError> {
        let attachment = {
            let state = self.inner.lock().await;
            state.attachment.clone().ok_or_else(|| {
                DispatchError::new("adapter.unavailable", "adapter", "tauri.adapter-state")
            })?
        };
        let adapter = self.adapter().await?;
        let power = match adapter.adapter_state().await {
            Ok(btleplug::api::CentralState::PoweredOn) => "on",
            Ok(btleplug::api::CentralState::PoweredOff) => "off",
            Ok(_) => "unknown",
            Err(error) => {
                return Err(DispatchError::new(
                    "adapter.unavailable",
                    "adapter",
                    "tauri.adapter-power",
                )
                .platform(error.to_string()));
            }
        };
        let heard = match adapter.peripherals().await {
            Ok(peripherals) => i64::try_from(peripherals.len()).unwrap_or(i64::MAX),
            Err(error) => {
                return Err(DispatchError::new(
                    "adapter.unavailable",
                    "adapter",
                    "tauri.adapter-heard",
                )
                .platform(error.to_string()));
            }
        };
        Ok(adapter_state_payload_live(&attachment, power, heard))
    }

    async fn start_scan(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let query_value = payload.get("query").ok_or_else(|| {
            DispatchError::new("protocol.violation", "scan", "tauri.scan-query-required")
        })?;
        let decoded_query = decode_normalized_scan_query(query_value).map_err(|error| {
            DispatchError::new("protocol.malformed", "scan", "tauri.scan-query").platform(error)
        })?;
        let service_uuids = decoded_query
            .native_service_uuids
            .iter()
            .map(|value| parse_uuid(value, "tauri.scan-services"))
            .collect::<Result<Vec<_>, _>>()?;
        let local_name_prefix: Option<String> = None;
        let manufacturer_filters: Vec<ManufacturerFilter> = Vec::new();
        let diagnostic_plan = diagnostic_scan_plan(&decoded_query);
        let attachment = self.ensure_adapter().await?;
        let adapter = self.adapter().await?;
        let key = caller_key(caller);
        let expected_lease_id = required_string(&payload, "__expectedLeaseId", "tauri.scan-lease")?;
        let expected_lease_generation =
            required_string(&payload, "__expectedLeaseGeneration", "tauri.scan-lease")?;
        let requested_services = service_uuids.clone();
        {
            let mut state = self.inner.lock().await;
            if scan_start_blocked(&state) {
                return Err(DispatchError::new(
                    "scan.already-active",
                    "scan",
                    "tauri.scan-global-owner",
                ));
            }
            let caller_state = state.callers.get_mut(&key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "scan", "tauri.scan-owner")
            })?;
            if caller_state.scan_admitting || caller_state.scan.is_some() {
                return Err(DispatchError::new(
                    "scan.already-active",
                    "scan",
                    "tauri.scan-start",
                ));
            }
            caller_state.scan_admitting = true;
            state.scan_owner = Some(key.clone());
        }
        let handle = self.id("scan");
        let dispatcher = self.clone();
        let stream_handle = handle.clone();
        let scan_adapter = adapter.clone();
        let stream_owner = key.clone();
        let stream_lease_id = expected_lease_id.clone();
        let stream_lease_generation = expected_lease_generation.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task = btleplug_runtime().spawn(async move {
            let mut events = match scan_adapter.events().await {
                Ok(events) => events,
                Err(error) => {
                    let _ = started_tx.send(Err(format!("tauri.scan-events: {error}")));
                    return;
                }
            };
            if let Err(error) = scan_adapter
                .start_scan(ScanFilter {
                    services: service_uuids,
                })
                .await
            {
                let _ = started_tx.send(Err(format!("tauri.scan-start: {error}")));
                return;
            }
            if started_tx.send(Ok(())).is_err() {
                return;
            }
            let mut interval = tokio::time::interval(SCAN_POLL_INTERVAL);
            let mut events_open = true;
            let mut polls: u32 = 0;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let Ok(peripherals) = scan_adapter.peripherals().await else {
                            continue;
                        };
                        polls = polls.saturating_add(1);
                        if polls % 20 == 0 {
                            eprintln!(
                                "ubm scan poll peripherals={} events_open={}",
                                peripherals.len(),
                                events_open
                            );
                        }
                        for peripheral in peripherals {
                            if dispatcher
                                .emit_scan_peripheral(
                                    &peripheral,
                                    ScanObservation {
                                        owner: &stream_owner,
                                        expected_lease: (&stream_lease_id, &stream_lease_generation),
                                        stream_handle: &stream_handle,
                                        requested_services: &requested_services,
                                        local_name_prefix: local_name_prefix.as_deref(),
                                        manufacturer_filters: &manufacturer_filters,
                                    },
                                )
                                .await
                                .is_err()
                            {
                                dispatcher
                                    .terminal(
                                        &stream_owner,
                                        (&stream_lease_id, &stream_lease_generation),
                                        &stream_handle,
                                        "source-failed",
                                    )
                                    .await
                                    .ok();
                                dispatcher
                                    .fail_scan_stream(
                                        &stream_owner,
                                        (&stream_lease_id, &stream_lease_generation),
                                        &stream_handle,
                                    )
                                    .await;
                                return;
                            }
                        }
                    }
                    event = events.next(), if events_open => {
                        let Some(event) = event else {
                            events_open = false;
                            continue;
                        };
                        let Some(peripheral_id) = scan_event_peripheral_id(event) else {
                            continue;
                        };
                        let Ok(peripheral) = scan_adapter.peripheral(&peripheral_id).await else {
                            continue;
                        };
                        if dispatcher
                            .emit_scan_peripheral(
                                &peripheral,
                                ScanObservation {
                                    owner: &stream_owner,
                                    expected_lease: (&stream_lease_id, &stream_lease_generation),
                                    stream_handle: &stream_handle,
                                    requested_services: &requested_services,
                                    local_name_prefix: local_name_prefix.as_deref(),
                                    manufacturer_filters: &manufacturer_filters,
                                },
                            )
                            .await
                            .is_err()
                        {
                            dispatcher
                                .terminal(
                                    &stream_owner,
                                    (&stream_lease_id, &stream_lease_generation),
                                    &stream_handle,
                                    "source-failed",
                                )
                                .await
                                .ok();
                            dispatcher
                                .fail_scan_stream(
                                    &stream_owner,
                                    (&stream_lease_id, &stream_lease_generation),
                                    &stream_handle,
                                )
                                .await;
                            return;
                        }
                    }
                }
            }
        });
        match started_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                self.abort_scan_admission(&key, (&expected_lease_id, &expected_lease_generation))
                    .await;
                return Err(
                    DispatchError::new("scan.start-failed", "scan", "tauri.scan-start")
                        .platform(error),
                );
            }
            Err(_) => {
                self.abort_scan_admission(&key, (&expected_lease_id, &expected_lease_generation))
                    .await;
                return Err(DispatchError::new(
                    "scan.start-failed",
                    "scan",
                    "tauri.scan-runtime",
                ));
            }
        }
        let mut state = self.inner.lock().await;
        let scan_owner_matches = state.scan_owner.as_deref() == Some(&key);
        let stale_lease = state
            .callers
            .get(&key)
            .is_some_and(|caller_state| !expected_lease_matches(caller_state, &payload));
        if stale_lease {
            task.abort();
            if let Some(caller_state) = state.callers.get_mut(&key) {
                caller_state.scan_admitting = false;
            }
            if scan_owner_matches {
                state.stopping_scan = Some(StoppingScan {
                    caller_key: key.clone(),
                    lease_id: expected_lease_id.clone(),
                    lease_generation: expected_lease_generation.clone(),
                    handle: handle.clone(),
                });
            }
            drop(state);
            let outcome = if scan_owner_matches {
                adapter.stop_scan().await.map_err(|error| error.to_string())
            } else {
                Ok(())
            };
            let mut state = self.inner.lock().await;
            if let Err(error) = apply_scan_stop_outcome(&mut state, outcome) {
                eprintln!(
                    "[unified-ble:tauri] scan stop after stale-lease start failed, \
                     resource retained for retry: {error}"
                );
            }
            return Err(DispatchError::new(
                "ownership.denied",
                "scan",
                "tauri.scan-stale-lease",
            ));
        }
        let Some(caller_state) = state.callers.get_mut(&key) else {
            task.abort();
            if state.scan_owner.as_deref() == Some(&key) {
                state.stopping_scan = Some(StoppingScan {
                    caller_key: key.clone(),
                    lease_id: expected_lease_id.clone(),
                    lease_generation: expected_lease_generation.clone(),
                    handle: handle.clone(),
                });
            }
            drop(state);
            let outcome = adapter.stop_scan().await.map_err(|error| error.to_string());
            let mut state = self.inner.lock().await;
            if let Err(error) = apply_scan_stop_outcome(&mut state, outcome) {
                eprintln!(
                    "[unified-ble:tauri] scan stop after missing caller failed, \
                     resource retained for retry: {error}"
                );
            }
            return Err(DispatchError::new(
                "ownership.denied",
                "scan",
                "tauri.scan-owner",
            ));
        };
        caller_state.scan_admitting = false;
        caller_state.scan = Some(ScanResource {
            handle: handle.clone(),
            task,
        });
        Ok(object([
            ("handle", string(handle)),
            ("backendGeneration", string(attachment.backend_generation)),
            ("plan", diagnostic_plan),
        ]))
    }

    async fn stop_scan(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "scanHandle", "tauri.scan-stop")?;
        let key = caller_key(caller);
        let expected_lease_id =
            required_string(&payload, "__expectedLeaseId", "tauri.scan-stop-lease")?;
        let expected_lease_generation = required_string(
            &payload,
            "__expectedLeaseGeneration",
            "tauri.scan-stop-lease",
        )?;
        {
            let mut state = self.inner.lock().await;
            match begin_scan_stop(
                &mut state,
                &key,
                (&expected_lease_id, &expected_lease_generation),
                &handle,
                true,
            )? {
                ScanStopBegin::AlreadyReleased => return Ok(released()),
                ScanStopBegin::Started => {}
            }
        }
        let outcome = self
            .adapter()
            .await?
            .stop_scan()
            .await
            .map_err(|error| error.to_string());
        let mut state = self.inner.lock().await;
        apply_scan_stop_outcome(&mut state, outcome).map_err(|error| {
            DispatchError::new("scan.stop-failed", "scan", "tauri.scan-stop").platform(error)
        })?;
        Ok(released())
    }

    async fn connect(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let peer_id = required_string(&payload, "peerId", "tauri.connect-peer")?;
        let adapter = self.adapter().await?;
        let peripherals = adapter.peripherals().await.map_err(|error| {
            DispatchError::new(
                "connection.failed",
                "connection",
                "tauri.connect-peripherals",
            )
            .platform(error.to_string())
        })?;
        let peripheral = peripherals
            .into_iter()
            .find(|peripheral| peripheral_id(peripheral) == peer_id)
            .ok_or_else(|| {
                DispatchError::new("connection.not-found", "connection", "tauri.connect-peer")
            })?;
        let key = caller_key(caller);
        {
            let mut state = self.inner.lock().await;
            if state.peer_owners.contains_key(&peer_id) {
                return Err(DispatchError::new(
                    "connection.already-owned",
                    "connection",
                    "tauri.connect-peer-owner",
                ));
            }
            if !state.callers.contains_key(&key) {
                return Err(DispatchError::new(
                    "ownership.denied",
                    "connection",
                    "tauri.connect-owner",
                ));
            }
            state.peer_owners.insert(peer_id.clone(), key.clone());
        }
        let already_connected = match peripheral.is_connected().await {
            Ok(connected) => connected,
            Err(error) => {
                self.clear_peer_owner(&peer_id, &key).await;
                return Err(DispatchError::new(
                    "connection.failed",
                    "connection",
                    "tauri.connect-state",
                )
                .platform(error.to_string()));
            }
        };
        if already_connected {
            self.clear_peer_owner(&peer_id, &key).await;
            return Err(DispatchError::new(
                "connection.already-owned",
                "connection",
                "tauri.connect-existing-link",
            ));
        }
        if let Err(error) = peripheral.connect().await {
            self.clear_peer_owner(&peer_id, &key).await;
            return Err(
                DispatchError::new("connection.failed", "connection", "tauri.connect")
                    .platform(error.to_string()),
            );
        }
        let handle = self.id("connection");
        let connection_id = self.id("connection-id");
        let connection_generation = self.id("connection-generation");
        let mut state = self.inner.lock().await;
        let peer_owner_matches = state
            .peer_owners
            .get(&peer_id)
            .is_some_and(|owner| owner == &key);
        let expected_lease = (
            payload
                .get("__expectedLeaseId")
                .and_then(as_string)
                .unwrap_or_default()
                .to_owned(),
            payload
                .get("__expectedLeaseGeneration")
                .and_then(as_string)
                .unwrap_or_default()
                .to_owned(),
        );
        let Some(caller_state) = state.callers.get_mut(&key) else {
            retain_orphan_connection(
                &mut state,
                OrphanConnectionOwner {
                    caller_key: key.clone(),
                    lease_id: expected_lease.0.clone(),
                    lease_generation: expected_lease.1.clone(),
                    peer_id: peer_id.clone(),
                    peripheral: Some(peripheral.clone()),
                    attempts: 0,
                },
            );
            drop(state);
            let residue = self
                .compensate_unowned_connection(
                    &peripheral,
                    &peer_id,
                    true,
                    Some(expected_lease.1.as_str()),
                )
                .await;
            return Err(compensation_failure("tauri.connect-owner", residue));
        };
        if !expected_lease_matches(caller_state, &payload) {
            let _ = caller_state;
            retain_orphan_connection(
                &mut state,
                OrphanConnectionOwner {
                    caller_key: key.clone(),
                    lease_id: expected_lease.0.clone(),
                    lease_generation: expected_lease.1.clone(),
                    peer_id: peer_id.clone(),
                    peripheral: Some(peripheral.clone()),
                    attempts: 0,
                },
            );
            drop(state);
            let residue = self
                .compensate_unowned_connection(
                    &peripheral,
                    &peer_id,
                    peer_owner_matches,
                    Some(expected_lease.1.as_str()),
                )
                .await;
            return Err(compensation_failure("tauri.connect-stale-lease", residue));
        }
        caller_state.connections.insert(
            handle.clone(),
            ConnectionResource {
                peer_id: peer_id.clone(),
                connection_id: connection_id.clone(),
                owner_lease_id: caller_state.lease_id.clone(),
                connection_generation: connection_generation.clone(),
                peripheral,
            },
        );
        Ok(object([
            ("handle", string(handle)),
            ("connectionId", string(connection_id)),
            ("ownerLeaseId", string(caller_state.lease_id.clone())),
            ("peerId", string(peer_id)),
            ("connectionGeneration", string(connection_generation)),
        ]))
    }

    /// Undo a physically-successful connect whose caller can no longer own it.
    ///
    /// The peer is connected and nobody is entitled to it, so it must come down.
    /// What matters is what happens when it will not: dropping the reservation
    /// anyway leaves a connected peripheral with no owner and no handle, which
    /// nothing can reach or retry until the process restarts - the peer is
    /// stranded, and the caller is told only that admission was denied.
    ///
    /// So the reservation is surrendered only when the platform PROVES the link
    /// is down, which is the rule `disconnect` already follows: a bounded wait,
    /// then a fresh state reading, and a D-Bus error naming a vanished device
    /// object read as evidence of release rather than as a failed question.
    /// Genuine indeterminacy keeps the reservation so a retry can reclaim it.
    ///
    /// Returns the failure to report alongside the admission error, or `None`
    /// when the peer was released cleanly.
    async fn compensate_unowned_connection(
        &self,
        peripheral: &Peripheral,
        peer_id: &str,
        owner_matches: bool,
        expected_generation: Option<&str>,
    ) -> Option<String> {
        let outcome = disconnect_with_state_check(
            async {
                peripheral
                    .disconnect()
                    .await
                    .map_err(|error| error.to_string())
            },
            || connected_after_failed_disconnect(peripheral),
        )
        .await;

        if let Some(expected_generation) = expected_generation {
            self.settle_orphan_connection(outcome, peer_id, expected_generation)
                .await
        } else {
            self.apply_compensation_outcome(outcome, peer_id, owner_matches)
                .await
        }
    }

    /// Settle the reservation from a compensating disconnect's outcome.
    ///
    /// Split from the I/O above so the rule can be tested without a live
    /// `Peripheral`: the defect this guards against is dropping the reservation
    /// regardless of outcome, which is a decision about state, not about radios.
    async fn apply_compensation_outcome(
        &self,
        outcome: Result<(), String>,
        peer_id: &str,
        owner_matches: bool,
    ) -> Option<String> {
        let mut state = self.inner.lock().await;
        match outcome {
            Ok(()) => {
                if owner_matches {
                    state.peer_owners.remove(peer_id);
                    state.orphan_connections.remove(peer_id);
                }
                None
            }
            Err(error) => {
                // Deliberately NOT removing the reservation: the peer may still
                // be connected, and an owner-less connected peer cannot be
                // reached or retried until the process restarts.
                if let Some(orphan) = state.orphan_connections.get_mut(peer_id) {
                    orphan.attempts = orphan.attempts.saturating_add(1);
                }
                eprintln!(
                    "[unified-ble:tauri] compensating disconnect for {peer_id} did not confirm \
                     release, so the reservation is retained for retry: {error}"
                );
                Some(error)
            }
        }
    }

    async fn settle_orphan_connection(
        &self,
        outcome: Result<(), String>,
        peer_id: &str,
        expected_generation: &str,
    ) -> Option<String> {
        let mut state = self.inner.lock().await;
        match outcome {
            Ok(()) => {
                let generation_matches = state
                    .orphan_connections
                    .get(peer_id)
                    .is_some_and(|orphan| orphan.lease_generation == expected_generation);
                if !generation_matches {
                    return None;
                }
                let caller_key = state
                    .orphan_connections
                    .get(peer_id)
                    .map(|orphan| orphan.caller_key.clone());
                state.orphan_connections.remove(peer_id);
                if let Some(caller_key) = caller_key {
                    if state
                        .peer_owners
                        .get(peer_id)
                        .is_some_and(|owner| owner == &caller_key)
                    {
                        state.peer_owners.remove(peer_id);
                    }
                }
                None
            }
            Err(error) => {
                if let Some(orphan) = state.orphan_connections.get_mut(peer_id) {
                    if orphan.lease_generation == expected_generation {
                        orphan.attempts = orphan.attempts.saturating_add(1);
                    }
                }
                eprintln!(
                    "[unified-ble:tauri] compensating disconnect for {peer_id} did not confirm \
                     release, so the reservation is retained for retry: {error}"
                );
                Some(error)
            }
        }
    }

    async fn compensate_unowned_subscription(
        &self,
        peripheral: &Peripheral,
        characteristic: &Characteristic,
        stream_id: &str,
        expected_generation: &str,
    ) -> Option<String> {
        let outcome = peripheral
            .unsubscribe(characteristic)
            .await
            .map_err(|error| error.to_string());
        self.settle_orphan_subscription(outcome, stream_id, expected_generation)
            .await
    }

    async fn settle_orphan_subscription(
        &self,
        outcome: Result<(), String>,
        stream_id: &str,
        expected_generation: &str,
    ) -> Option<String> {
        let mut state = self.inner.lock().await;
        match outcome {
            Ok(()) => {
                let generation_matches = state
                    .orphan_subscription_owners
                    .get(stream_id)
                    .is_some_and(|orphan| orphan.lease_generation == expected_generation);
                if !generation_matches {
                    return None;
                }
                state.orphan_subscription_owners.remove(stream_id);
                state.orphan_subscription_resources.remove(stream_id);
                None
            }
            Err(error) => {
                if let Some(orphan) = state.orphan_subscription_owners.get_mut(stream_id) {
                    if orphan.lease_generation == expected_generation {
                        orphan.attempts = orphan.attempts.saturating_add(1);
                    }
                }
                eprintln!(
                    "[unified-ble:tauri] compensating unsubscribe for {stream_id} did not confirm \
                     release, so the orphan owner is retained for retry: {error}"
                );
                Some(error)
            }
        }
    }

    async fn disconnect(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "connectionHandle", "tauri.disconnect")?;
        let key = caller_key(caller);
        let connection = {
            let state = self.inner.lock().await;
            let caller_state = state.callers.get(&key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "connection", "tauri.disconnect-owner")
            })?;
            let Some(connection) = caller_state.connections.get(&handle) else {
                return Ok(released());
            };
            validate_connection_identity(
                &payload,
                connection,
                &caller_state.lease_id,
                "tauri.disconnect",
            )?;
            connection.clone()
        };
        disconnect_peripheral(&connection.peripheral)
            .await
            .map_err(|error| {
                DispatchError::new("platform.failure", "connection", "tauri.disconnect")
                    .platform(error)
            })?;
        let resources = self
            .remove_connection_resources(&key, &handle, &connection.peer_id)
            .await;
        for subscription in resources.0 {
            subscription.task.abort();
        }
        for task in resources.1 {
            task.abort();
        }
        Ok(released())
    }

    async fn remove_connection_resources(
        &self,
        caller_key: &str,
        connection_handle: &str,
        peer_id: &str,
    ) -> (Vec<SubscriptionResource>, Vec<TauriJoinHandle<()>>) {
        let mut state = self.inner.lock().await;
        let Some(caller_state) = state.callers.get_mut(caller_key) else {
            if state
                .peer_owners
                .get(peer_id)
                .is_some_and(|owner| owner == caller_key)
            {
                state.peer_owners.remove(peer_id);
            }
            return (Vec::new(), Vec::new());
        };
        let removed_peer_id = caller_state
            .connections
            .remove(connection_handle)
            .map(|connection| connection.peer_id);
        let database_handles = caller_state
            .databases
            .iter()
            .filter_map(|(database_handle, database)| {
                (database.connection_handle == connection_handle).then_some(database_handle.clone())
            })
            .collect::<HashSet<_>>();
        let subscription_handles = caller_state
            .subscriptions
            .iter()
            .filter_map(|(subscription_handle, subscription)| {
                database_handles
                    .contains(&subscription.database_handle)
                    .then_some(subscription_handle.clone())
            })
            .collect::<Vec<_>>();
        let subscriptions = subscription_handles
            .into_iter()
            .filter_map(|subscription_handle| {
                caller_state.subscriptions.remove(&subscription_handle)
            })
            .collect::<Vec<_>>();
        caller_state
            .databases
            .retain(|database_handle, _| !database_handles.contains(database_handle));
        let event_handles = caller_state
            .connection_events
            .iter()
            .filter_map(|(event_handle, event)| {
                (event.connection_handle == connection_handle).then_some(event_handle.clone())
            })
            .collect::<Vec<_>>();
        let event_tasks = event_handles
            .into_iter()
            .filter_map(|event_handle| caller_state.connection_events.remove(&event_handle))
            .filter_map(|event| event.task)
            .collect::<Vec<_>>();
        if should_clear_peer_owner(
            removed_peer_id.as_deref(),
            peer_id,
            state.peer_owners.get(peer_id).map(String::as_str),
            caller_key,
        ) {
            state.peer_owners.remove(peer_id);
        }
        (subscriptions, event_tasks)
    }

    async fn subscribe_connection_events(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let stream_handle = required_string(
            &payload,
            "connectionEventsHandle",
            "tauri.connection-events-handle",
        )?;
        let connection = self
            .connection(caller, &payload, "tauri.connection-events-connection")
            .await?;
        let key = caller_key(caller);
        let mut state = self.inner.lock().await;
        let caller_state = state.callers.get_mut(&key).ok_or_else(|| {
            DispatchError::new(
                "ownership.denied",
                "connection",
                "tauri.connection-events-owner",
            )
        })?;
        if caller_state.connection_events.contains_key(&stream_handle) {
            return Err(DispatchError::new(
                "protocol.violation",
                "connection",
                "tauri.connection-events-duplicate",
            ));
        }
        caller_state.connection_events.insert(
            stream_handle.clone(),
            ConnectionEventResource {
                connection_handle: required_string(
                    &payload,
                    "connectionHandle",
                    "tauri.connection-events-connection",
                )?,
                stream_handle: stream_handle.clone(),
                peer_id: connection.peer_id,
                connection_id: connection.connection_id.clone(),
                connection_generation: connection.connection_generation.clone(),
                active: false,
                sequence: 0,
                task: None,
            },
        );
        Ok(object([
            ("handle", string(stream_handle)),
            ("connectionId", string(connection.connection_id)),
            (
                "connectionGeneration",
                string(connection.connection_generation),
            ),
            ("eventSchemaVersion", number(2)),
        ]))
    }

    async fn ready_connection_events(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let stream_handle = required_string(
            &payload,
            "connectionEventsHandle",
            "tauri.connection-events-ready-handle",
        )?;
        let key = caller_key(caller);
        let event = {
            let mut state = self.inner.lock().await;
            let attachment = state.attachment.clone().ok_or_else(|| {
                DispatchError::new(
                    "lifecycle.invalid-state",
                    "connection",
                    "tauri.connection-events-attachment",
                )
            })?;
            let caller_state = state.callers.get_mut(&key).ok_or_else(|| {
                DispatchError::new(
                    "ownership.denied",
                    "connection",
                    "tauri.connection-events-ready-owner",
                )
            })?;
            let resource = caller_state
                .connection_events
                .get_mut(&stream_handle)
                .ok_or_else(|| {
                    DispatchError::new(
                        "gatt.stale-handle",
                        "connection",
                        "tauri.connection-events-ready-handle",
                    )
                })?;
            if resource.active {
                return Err(DispatchError::new(
                    "lifecycle.invalid-state",
                    "connection",
                    "tauri.connection-events-ready-state",
                ));
            }
            resource.active = true;
            resource.sequence = 1;
            let peripheral = caller_state
                .connections
                .get(&resource.connection_handle)
                .map(|connection| connection.peripheral.clone())
                .ok_or_else(|| {
                    DispatchError::new(
                        "connection.stale",
                        "connection",
                        "tauri.connection-events-connection",
                    )
                })?;
            (
                resource.stream_handle.clone(),
                resource.peer_id.clone(),
                resource.connection_id.clone(),
                resource.connection_generation.clone(),
                caller_state.lease_id.clone(),
                resource.sequence,
                attachment,
                peripheral,
                caller_state.lease_generation.clone(),
                resource.connection_handle.clone(),
            )
        };
        let initial_event = object([
            ("kind", string("connection-lifecycle")),
            ("schemaVersion", number(2)),
            ("attachment", attachment_record(&event.6)),
            ("attachmentId", string(event.6.attachment_id.clone())),
            ("peerId", string(event.1.clone())),
            ("connectionId", string(event.2.clone())),
            ("connectionGeneration", string(event.3.clone())),
            ("ownerLeaseId", string(event.4.clone())),
            ("sequence", number(event.5 as i64)),
            ("backendIngressOrdinal", IpcValue::Null),
            ("previous", string("connecting")),
            ("current", string("connected")),
            ("cause", string("connected")),
        ]);
        if let Err(error) = self
            .emit(
                &key,
                Some((&event.4, &event.8)),
                &event.0,
                initial_event,
                false,
            )
            .await
        {
            let mut state = self.inner.lock().await;
            if let Some(caller) = state.callers.get_mut(&key) {
                if let Some(resource) = caller.connection_events.remove(&event.0) {
                    if let Some(task) = resource.task {
                        task.abort();
                    }
                }
            }
            return Err(error);
        }
        let dispatcher = self.clone();
        let stream_owner = key.clone();
        let stream_id = event.0.clone();
        let peer_id = event.1.clone();
        let connection_id = event.2.clone();
        let connection_generation = event.3.clone();
        let expected_lease_id = event.4.clone();
        let expected_lease_generation = event.8.clone();
        let connection_handle = event.9.clone();
        let stream_id_for_task = stream_id.clone();
        let task = tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(SCAN_POLL_INTERVAL).await;
                match event.7.is_connected().await {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = dispatcher
                            .emit_connection_lost(
                                &stream_owner,
                                (&expected_lease_id, &expected_lease_generation),
                                ConnectionEventIdentity {
                                    stream_id: &stream_id_for_task,
                                    peer_id: &peer_id,
                                    connection_id: &connection_id,
                                    connection_generation: &connection_generation,
                                },
                            )
                            .await;
                        let resources = dispatcher
                            .remove_connection_resources(
                                &stream_owner,
                                &connection_handle,
                                &peer_id,
                            )
                            .await;
                        for subscription in resources.0 {
                            subscription.task.abort();
                        }
                        for task in resources.1 {
                            task.abort();
                        }
                        break;
                    }
                    Err(_) => {
                        let _ = dispatcher
                            .emit_connection_failure(
                                &stream_owner,
                                (&expected_lease_id, &expected_lease_generation),
                                ConnectionEventIdentity {
                                    stream_id: &stream_id_for_task,
                                    peer_id: &peer_id,
                                    connection_id: &connection_id,
                                    connection_generation: &connection_generation,
                                },
                                "backend-failure",
                                "source-failed",
                            )
                            .await;
                        break;
                    }
                }
            }
        });
        let mut state = self.inner.lock().await;
        if let Some(caller_state) = state.callers.get_mut(&key) {
            let resource = caller_state.connection_events.get_mut(&stream_id);
            match resource {
                Some(resource)
                    if caller_state.lease_id == event.4
                        && caller_state.lease_generation == event.8
                        && resource.connection_id == event.2
                        && resource.connection_generation == event.3 =>
                {
                    resource.task = Some(task);
                }
                _ => task.abort(),
            }
        } else {
            task.abort();
        }
        Ok(object([("state", string("ready"))]))
    }

    async fn unsubscribe_connection_events(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let stream_handle = required_string(
            &payload,
            "connectionEventsHandle",
            "tauri.connection-events-unsubscribe-handle",
        )?;
        let mut state = self.inner.lock().await;
        let caller_state = state.callers.get_mut(&caller_key(caller)).ok_or_else(|| {
            DispatchError::new(
                "ownership.denied",
                "connection",
                "tauri.connection-events-unsubscribe-owner",
            )
        })?;
        if let Some(resource) = caller_state.connection_events.remove(&stream_handle) {
            if let Some(task) = resource.task {
                task.abort();
            }
        }
        Ok(released())
    }

    async fn discover(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let connection_handle = required_string(&payload, "connectionHandle", "tauri.discover")?;
        let peripheral = self
            .connection(caller, &payload, "tauri.discover")
            .await?
            .peripheral;
        peripheral.discover_services().await.map_err(|error| {
            DispatchError::new("gatt.discovery-required", "gatt", "tauri.discover")
                .platform(error.to_string())
        })?;
        let database_handle = self.id("database");
        let database_id = self.id("database-id");
        let database_generation = self.id("database-generation");
        let mut characteristic_map = HashMap::new();
        let mut descriptor_map = HashMap::new();
        let mut service_records = Vec::new();
        let mut characteristic_records = Vec::new();
        let mut descriptor_records = Vec::new();
        let mut service_occurrences: HashMap<Uuid, usize> = HashMap::new();
        let mut characteristic_occurrences: HashMap<(Uuid, usize, Uuid), usize> = HashMap::new();
        for service in peripheral.services() {
            let service_occurrence = service_occurrences.entry(service.uuid).or_default();
            let service_uuid = service.uuid.to_string();
            let current_service_occurrence = *service_occurrence;
            service_records.push(object([
                ("uuid", string(service_uuid.clone())),
                ("occurrence", string(current_service_occurrence.to_string())),
                ("primary", IpcValue::Bool(service.primary)),
                ("includedServices", IpcValue::Array(Vec::new())),
            ]));
            *service_occurrence += 1;
            for characteristic in &service.characteristics {
                let occurrence = characteristic_occurrences
                    .entry((
                        service.uuid,
                        current_service_occurrence,
                        characteristic.uuid,
                    ))
                    .or_default();
                let handle = self.id("characteristic");
                characteristic_records.push(object([
                    ("handle", string(handle.clone())),
                    ("serviceUuid", string(service_uuid.clone())),
                    (
                        "serviceOccurrence",
                        string(current_service_occurrence.to_string()),
                    ),
                    (
                        "characteristicUuid",
                        string(characteristic.uuid.to_string()),
                    ),
                    ("characteristicOccurrence", string(occurrence.to_string())),
                    (
                        "properties",
                        characteristic_properties(characteristic.properties),
                    ),
                ]));
                *occurrence += 1;
                for (index, descriptor) in characteristic.descriptors.iter().enumerate() {
                    let descriptor_handle = self.id("descriptor");
                    descriptor_records.push(object([
                        ("handle", string(descriptor_handle.clone())),
                        ("characteristicHandle", string(handle.clone())),
                        ("uuid", string(descriptor.uuid.to_string())),
                        ("occurrence", string(index.to_string())),
                    ]));
                    descriptor_map.insert(descriptor_handle, descriptor.clone());
                }
                characteristic_map.insert(handle, characteristic.clone());
            }
        }
        let mut state = self.inner.lock().await;
        let caller_state = state.callers.get_mut(&caller_key(caller)).ok_or_else(|| {
            DispatchError::new("ownership.denied", "gatt", "tauri.discover-owner")
        })?;
        if !expected_lease_matches(caller_state, &payload) {
            return Err(DispatchError::new(
                "ownership.denied",
                "gatt",
                "tauri.discover-stale-lease",
            ));
        }
        for database in caller_state.databases.values_mut() {
            if database.connection_handle == connection_handle {
                database.valid = false;
            }
        }
        caller_state.databases.insert(
            database_handle.clone(),
            DatabaseResource {
                connection_handle,
                database_id: database_id.clone(),
                database_generation: database_generation.clone(),
                valid: true,
                characteristics: characteristic_map,
                descriptors: descriptor_map,
            },
        );
        Ok(object([
            ("schemaVersion", number(2)),
            ("handle", string(database_handle)),
            ("databaseId", string(database_id)),
            ("databaseGeneration", string(database_generation)),
            ("services", IpcValue::Array(service_records)),
            ("characteristics", IpcValue::Array(characteristic_records)),
            ("descriptors", IpcValue::Array(descriptor_records)),
        ]))
    }

    async fn release_database(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "databaseHandle", "tauri.database-release")?;
        let key = caller_key(caller);
        let mut state = self.inner.lock().await;
        let caller_state = state.callers.get_mut(&key).ok_or_else(|| {
            DispatchError::new("ownership.denied", "gatt", "tauri.database-release-owner")
        })?;
        caller_state.databases.remove(&handle);
        Ok(released())
    }

    async fn read(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let (peripheral, characteristic) = self.characteristic(caller, &payload).await?;
        let value = peripheral.read(&characteristic).await.map_err(|error| {
            DispatchError::new("gatt.read-failed", "gatt", "tauri.gatt-read")
                .platform(error.to_string())
        })?;
        Ok(object([("value", IpcValue::Bytes(value))]))
    }

    async fn write(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        bytes: Option<Vec<u8>>,
    ) -> Result<IpcValue, DispatchError> {
        let bytes = bytes
            .ok_or_else(|| DispatchError::new("bytes.invalid", "gatt", "tauri.gatt-write-bytes"))?;
        let mode = required_string(&payload, "mode", "tauri.gatt-write-mode")?;
        let write_type = match mode.as_str() {
            "with-response" => WriteType::WithResponse,
            "without-response" => WriteType::WithoutResponse,
            _ => {
                return Err(DispatchError::new(
                    "argument.invalid",
                    "gatt",
                    "tauri.gatt-write-mode",
                ))
            }
        };
        let (peripheral, characteristic) = self.characteristic(caller, &payload).await?;
        peripheral
            .write(&characteristic, &bytes, write_type)
            .await
            .map_err(|error| {
                DispatchError::new("gatt.write-failed", "gatt", "tauri.gatt-write")
                    .platform(error.to_string())
            })?;
        let write_correlation = self.id("write-operation");
        let commit_state = if mode == "with-response" {
            "confirmed"
        } else {
            "accepted"
        };
        Ok(object([
            (
                "terminal",
                object([
                    ("correlation", string(write_correlation)),
                    ("outcome", string("succeeded")),
                    ("cause", IpcValue::Null),
                ]),
            ),
            ("mode", string(mode)),
            ("commitState", string(commit_state)),
            ("bytesSubmitted", number(bytes.len() as i64)),
        ]))
    }

    async fn subscribe(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let database_handle =
            required_string(&payload, "databaseHandle", "tauri.subscribe-database")?;
        let (peripheral, characteristic) = self.characteristic(caller, &payload).await?;
        if let Some(delivery_mode) = payload.get("deliveryMode").and_then(as_string) {
            match delivery_mode {
                "require-notification"
                    if !characteristic.properties.contains(CharPropFlags::NOTIFY) =>
                {
                    return Err(DispatchError::new(
                        "gatt.property-not-supported",
                        "gatt",
                        "tauri.subscribe.notification",
                    ));
                }
                "require-indication" => {
                    return Err(DispatchError::new(
                        "capability.limited",
                        "gatt",
                        "tauri.subscribe.indication-selection",
                    ));
                }
                "prefer-notification" | "prefer-indication" | "require-notification" => {}
                _ => {
                    return Err(DispatchError::new(
                        "argument.invalid",
                        "gatt",
                        "tauri.subscribe.delivery-mode",
                    ));
                }
            }
        }
        let mut notifications = peripheral.notifications().await.map_err(|error| {
            DispatchError::new("gatt.subscribe-failed", "gatt", "tauri.notifications")
                .platform(error.to_string())
        })?;
        peripheral
            .subscribe(&characteristic)
            .await
            .map_err(|error| {
                DispatchError::new("gatt.subscribe-failed", "gatt", "tauri.subscribe")
                    .platform(error.to_string())
            })?;
        let handle = self.id("subscription");
        let stream_handle = handle.clone();
        let key = caller_key(caller);
        let expected_lease_id =
            required_string(&payload, "__expectedLeaseId", "tauri.subscribe-lease")?;
        let expected_lease_generation = required_string(
            &payload,
            "__expectedLeaseGeneration",
            "tauri.subscribe-lease",
        )?;
        let subscription_lease_id = expected_lease_id.clone();
        let subscription_lease_generation = expected_lease_generation.clone();
        let uuid = characteristic.uuid;
        let dispatcher = self.clone();
        let task = tauri::async_runtime::spawn(async move {
            let mut sequence = 0_u64;
            while let Some(notification) = notifications.next().await {
                if notification.uuid != uuid {
                    continue;
                }
                sequence = sequence.saturating_add(1);
                let observed_at_monotonic_ms =
                    i64::try_from(dispatcher.started_at.elapsed().as_millis()).unwrap_or(i64::MAX);
                if dispatcher
                    .emit(
                        &key,
                        Some((&subscription_lease_id, &subscription_lease_generation)),
                        &stream_handle,
                        object([
                            ("value", IpcValue::Bytes(notification.value)),
                            ("delivery", string("unknown")),
                            ("observedAtMonotonicMs", number(observed_at_monotonic_ms)),
                            ("sequence", number(sequence as i64)),
                        ]),
                        false,
                    )
                    .await
                    .is_err()
                {
                    dispatcher
                        .terminal(
                            &key,
                            (&subscription_lease_id, &subscription_lease_generation),
                            &stream_handle,
                            "source-failed",
                        )
                        .await
                        .ok();
                    dispatcher
                        .fail_subscription_stream(
                            &key,
                            (&subscription_lease_id, &subscription_lease_generation),
                            &stream_handle,
                        )
                        .await;
                    return;
                }
            }
            dispatcher
                .terminal(
                    &key,
                    (&subscription_lease_id, &subscription_lease_generation),
                    &stream_handle,
                    "source-failed",
                )
                .await
                .ok();
            dispatcher
                .fail_subscription_stream(
                    &key,
                    (&subscription_lease_id, &subscription_lease_generation),
                    &stream_handle,
                )
                .await;
        });
        let mut state = self.inner.lock().await;
        let Some(caller_state) = state.callers.get_mut(&caller_key(caller)) else {
            task.abort();
            retain_orphan_subscription(
                &mut state,
                OrphanSubscriptionOwner {
                    caller_key: caller_key(caller),
                    lease_id: expected_lease_id.clone(),
                    lease_generation: expected_lease_generation.clone(),
                    stream_id: handle.clone(),
                    attempts: 0,
                },
                Some(SubscriptionResource {
                    database_handle: database_handle.clone(),
                    body: SubscriptionBody::Native {
                        peripheral: peripheral.clone(),
                        characteristic: characteristic.clone(),
                    },
                    task,
                }),
            );
            drop(state);
            let residue = self
                .compensate_unowned_subscription(
                    &peripheral,
                    &characteristic,
                    &handle,
                    &expected_lease_generation,
                )
                .await;
            return Err(subscription_compensation_failure(
                "tauri.subscribe-owner",
                residue,
            ));
        };
        if !expected_lease_matches(caller_state, &payload) {
            let _ = caller_state;
            task.abort();
            retain_orphan_subscription(
                &mut state,
                OrphanSubscriptionOwner {
                    caller_key: caller_key(caller),
                    lease_id: expected_lease_id.clone(),
                    lease_generation: expected_lease_generation.clone(),
                    stream_id: handle.clone(),
                    attempts: 0,
                },
                Some(SubscriptionResource {
                    database_handle: database_handle.clone(),
                    body: SubscriptionBody::Native {
                        peripheral: peripheral.clone(),
                        characteristic: characteristic.clone(),
                    },
                    task,
                }),
            );
            drop(state);
            let residue = self
                .compensate_unowned_subscription(
                    &peripheral,
                    &characteristic,
                    &handle,
                    &expected_lease_generation,
                )
                .await;
            return Err(subscription_compensation_failure(
                "tauri.subscribe-stale-lease",
                residue,
            ));
        }
        caller_state.subscriptions.insert(
            handle.clone(),
            SubscriptionResource {
                database_handle,
                body: SubscriptionBody::Native {
                    peripheral,
                    characteristic,
                },
                task,
            },
        );
        Ok(object([("handle", string(handle))]))
    }

    async fn unsubscribe(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "subscriptionHandle", "tauri.unsubscribe")?;
        let key = caller_key(caller);
        let expected_lease_id =
            required_string(&payload, "__expectedLeaseId", "tauri.unsubscribe-lease")?;
        let expected_lease_generation = required_string(
            &payload,
            "__expectedLeaseGeneration",
            "tauri.unsubscribe-lease",
        )?;
        let subscription = {
            let state = self.inner.lock().await;
            let caller_state = state.callers.get(&key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "gatt", "tauri.unsubscribe-owner")
            })?;
            if caller_state.lease_id != expected_lease_id
                || caller_state.lease_generation != expected_lease_generation
            {
                return Err(DispatchError::new(
                    "ownership.denied",
                    "gatt",
                    "tauri.unsubscribe-stale-lease",
                ));
            }
            caller_state
                .subscriptions
                .get(&handle)
                .and_then(|subscription| {
                    subscription.native().map(|(peripheral, characteristic)| {
                        (peripheral.clone(), characteristic.clone())
                    })
                })
        };
        if let Some(subscription) = subscription {
            subscription
                .0
                .unsubscribe(&subscription.1)
                .await
                .map_err(|error| {
                    DispatchError::new("gatt.subscribe-failed", "gatt", "tauri.unsubscribe")
                        .platform(error.to_string())
                })?;
            let mut state = self.inner.lock().await;
            if let Some(caller_state) = state.callers.get_mut(&key) {
                if caller_state.lease_id == expected_lease_id
                    && caller_state.lease_generation == expected_lease_generation
                {
                    if let Some(subscription) = caller_state.subscriptions.remove(&handle) {
                        subscription.task.abort();
                    }
                }
            }
        }
        Ok(released())
    }

    async fn read_descriptor(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let (peripheral, descriptor) = self.descriptor(caller, &payload).await?;
        let value = peripheral
            .read_descriptor(&descriptor)
            .await
            .map_err(|error| {
                DispatchError::new("gatt.read-failed", "gatt", "tauri.descriptor-read")
                    .platform(error.to_string())
            })?;
        Ok(object([("value", IpcValue::Bytes(value))]))
    }

    async fn write_descriptor(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        bytes: Option<Vec<u8>>,
    ) -> Result<IpcValue, DispatchError> {
        let mode = required_string(&payload, "mode", "tauri.descriptor-write-mode")?;
        if mode != "with-response" {
            return Err(DispatchError::new(
                "argument.invalid",
                "gatt",
                "tauri.descriptor-write-mode",
            ));
        }
        let bytes = bytes.ok_or_else(|| {
            DispatchError::new("bytes.invalid", "gatt", "tauri.descriptor-write-bytes")
        })?;
        let (peripheral, descriptor) = self.descriptor(caller, &payload).await?;
        peripheral
            .write_descriptor(&descriptor, &bytes)
            .await
            .map_err(|error| {
                DispatchError::new("gatt.write-failed", "gatt", "tauri.descriptor-write")
                    .platform(error.to_string())
            })?;
        Ok(object([
            (
                "terminal",
                object([
                    ("correlation", string(self.id("descriptor-write-operation"))),
                    ("outcome", string("succeeded")),
                    ("cause", IpcValue::Null),
                ]),
            ),
            ("mode", string("with-response")),
            ("commitState", string("confirmed")),
            ("bytesSubmitted", number(bytes.len() as i64)),
        ]))
    }

    async fn read_rssi(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let peripheral = self
            .connection(caller, &payload, "tauri.rssi")
            .await?
            .peripheral;
        let rssi = peripheral.read_rssi().await.map_err(|error| {
            DispatchError::new("platform.failure", "connection", "tauri.rssi")
                .platform(error.to_string())
        })?;
        Ok(object([("rssi", number(i64::from(rssi)))]))
    }

    async fn maximum_write_length(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let peripheral = self
            .connection(caller, &payload, "tauri.maximum-write-length")
            .await?
            .peripheral;
        let bytes = peripheral.mtu().saturating_sub(3);
        Ok(object([("bytes", number(i64::from(bytes)))]))
    }

    async fn adapter(&self) -> Result<Adapter, DispatchError> {
        self.inner
            .lock()
            .await
            .adapter
            .clone()
            .ok_or_else(|| DispatchError::new("adapter.unavailable", "adapter", "tauri.adapter"))
    }

    async fn clear_peer_owner(&self, peer_id: &str, owner: &str) {
        let mut state = self.inner.lock().await;
        if state
            .peer_owners
            .get(peer_id)
            .is_some_and(|value| value == owner)
        {
            state.peer_owners.remove(peer_id);
        }
    }

    async fn connection(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
        operation: &str,
    ) -> Result<ConnectionResource, DispatchError> {
        let handle = required_string(payload, "connectionHandle", operation)?;
        let state = self.inner.lock().await;
        let caller_state = state
            .callers
            .get(&caller_key(caller))
            .ok_or_else(|| DispatchError::new("ownership.denied", "connection", operation))?;
        let connection = caller_state
            .connections
            .get(&handle)
            .ok_or_else(|| DispatchError::new("connection.not-found", "connection", operation))?;
        validate_connection_identity(payload, connection, &caller_state.lease_id, operation)?;
        Ok(connection.clone())
    }

    async fn characteristic(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<(Peripheral, Characteristic), DispatchError> {
        let database_handle =
            required_string(payload, "databaseHandle", "tauri.characteristic-database")?;
        let characteristic_handle = required_string(
            payload,
            "characteristicHandle",
            "tauri.characteristic-handle",
        )?;
        let state = self.inner.lock().await;
        let caller_state = state.callers.get(&caller_key(caller)).ok_or_else(|| {
            DispatchError::new("ownership.denied", "gatt", "tauri.characteristic-owner")
        })?;
        let database = caller_state
            .databases
            .get(&database_handle)
            .ok_or_else(|| {
                DispatchError::new("gatt.stale-handle", "gatt", "tauri.characteristic-database")
            })?;
        if !database.valid {
            return Err(DispatchError::new(
                "gatt.stale-handle",
                "gatt",
                "tauri.characteristic-database-generation",
            ));
        }
        validate_database_identity(payload, database, "tauri.characteristic-database")?;
        let characteristic = database
            .characteristics
            .get(&characteristic_handle)
            .cloned()
            .ok_or_else(|| {
                DispatchError::new("gatt.not-found", "gatt", "tauri.characteristic-handle")
            })?;
        let connection = caller_state
            .connections
            .get(&database.connection_handle)
            .ok_or_else(|| {
                DispatchError::new(
                    "connection.stale",
                    "connection",
                    "tauri.characteristic-connection",
                )
            })?;
        validate_connection_identity(
            payload,
            connection,
            &caller_state.lease_id,
            "tauri.characteristic-connection",
        )?;
        Ok((connection.peripheral.clone(), characteristic))
    }

    async fn descriptor(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<(Peripheral, Descriptor), DispatchError> {
        let database_handle =
            required_string(payload, "databaseHandle", "tauri.descriptor-database")?;
        let descriptor_handle =
            required_string(payload, "descriptorHandle", "tauri.descriptor-handle")?;
        let state = self.inner.lock().await;
        let caller_state = state.callers.get(&caller_key(caller)).ok_or_else(|| {
            DispatchError::new("ownership.denied", "gatt", "tauri.descriptor-owner")
        })?;
        let database = caller_state
            .databases
            .get(&database_handle)
            .ok_or_else(|| {
                DispatchError::new("gatt.stale-handle", "gatt", "tauri.descriptor-database")
            })?;
        if !database.valid {
            return Err(DispatchError::new(
                "gatt.stale-handle",
                "gatt",
                "tauri.descriptor-database-generation",
            ));
        }
        validate_database_identity(payload, database, "tauri.descriptor-database")?;
        let descriptor = database
            .descriptors
            .get(&descriptor_handle)
            .cloned()
            .ok_or_else(|| {
                DispatchError::new("gatt.not-found", "gatt", "tauri.descriptor-handle")
            })?;
        let connection = caller_state
            .connections
            .get(&database.connection_handle)
            .ok_or_else(|| {
                DispatchError::new(
                    "connection.stale",
                    "connection",
                    "tauri.descriptor-connection",
                )
            })?;
        validate_connection_identity(
            payload,
            connection,
            &caller_state.lease_id,
            "tauri.descriptor-connection",
        )?;
        Ok((connection.peripheral.clone(), descriptor))
    }

    async fn acknowledge(
        &self,
        caller: AuthenticatedCaller,
        request: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let event_id = required_string(&request, "eventId", "tauri.event-ack")?;
        let lease = required_lease(&request, "tauri.event-ack-lease")?;
        let mut state = self.inner.lock().await;
        let caller_state = state.callers.get_mut(&caller_key(&caller)).ok_or_else(|| {
            DispatchError::new("ownership.denied", "ipc", "tauri.event-ack-owner")
        })?;
        validate_lease(caller_state, &lease, "tauri.event-ack-lease")?;
        if !caller_state.pending_events.remove(&event_id) {
            return Err(DispatchError::new(
                "protocol.violation",
                "ipc",
                "tauri.event-ack-id",
            ));
        }
        Ok(object([("kind", string("event.ack"))]))
    }

    async fn release_request(
        &self,
        caller: AuthenticatedCaller,
        request: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let lease = required_lease(&request, "tauri.release-lease")?;
        let key = caller_key(&caller);
        let _admission = self.bootstrap_admission.lock().await;
        {
            let state = self.inner.lock().await;
            let caller_state = state.callers.get(&key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "ipc", "tauri.release-owner")
            })?;
            validate_lease(caller_state, &lease, "tauri.release-lease")?;
        }
        let cleanup = self.release(&key).await;
        Ok(object([("kind", string("release")), ("cleanup", cleanup)]))
    }

    async fn abort_scan_admission(&self, key: &str, expected_lease: (&str, &str)) {
        let mut state = self.inner.lock().await;
        if let Some(caller_state) = state.callers.get_mut(key) {
            if caller_state.lease_id != expected_lease.0
                || caller_state.lease_generation != expected_lease.1
            {
                return;
            }
            caller_state.scan_admitting = false;
        }
        if state
            .stopping_scan
            .as_ref()
            .is_some_and(|stopping| stopping.caller_key == key)
        {
            return;
        }
        if state.scan_owner.as_deref() == Some(key) {
            state.scan_owner = None;
        }
    }

    async fn emit(
        &self,
        caller_key: &str,
        expected_lease: Option<(&str, &str)>,
        stream_id: &str,
        value: IpcValue,
        drop_if_full: bool,
    ) -> Result<(), DispatchError> {
        let (sink, lease_id, lease_generation, event_id) = {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(caller_key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "stream", "tauri.event-owner")
            })?;
            if expected_lease.is_some_and(|lease| {
                caller_state.lease_id != lease.0 || caller_state.lease_generation != lease.1
            }) {
                return Err(DispatchError::new(
                    "ownership.denied",
                    "stream",
                    "tauri.event-stale-lease",
                ));
            }
            if caller_state.pending_events.len() >= MAX_PENDING_EVENTS {
                if drop_if_full {
                    return Err(DispatchError::new(
                        "stream.quota",
                        "stream",
                        "tauri.event-retention",
                    ));
                }
                return Err(DispatchError::new(
                    "stream.quota",
                    "stream",
                    "tauri.event-retention",
                ));
            }
            let event_id = self.id("event");
            caller_state.pending_events.insert(event_id.clone());
            (
                caller_state.event_sink.clone(),
                caller_state.lease_id.clone(),
                caller_state.lease_generation.clone(),
                event_id,
            )
        };
        let send_result = sink.send(object([
            (
                "rendererLease",
                object([
                    ("leaseId", string(lease_id.clone())),
                    ("generation", string(lease_generation.clone())),
                ]),
            ),
            ("eventId", string(event_id.clone())),
            ("streamId", string(stream_id)),
            (
                "item",
                object([("kind", string("value")), ("value", value)]),
            ),
        ]));
        if send_result.is_err() {
            let mut state = self.inner.lock().await;
            if let Some(caller_state) = state.callers.get_mut(caller_key) {
                if caller_state.lease_id == lease_id
                    && caller_state.lease_generation == lease_generation
                {
                    caller_state.pending_events.remove(&event_id);
                }
            }
        }
        send_result.map_err(|error| {
            DispatchError::new("platform.transport", "stream", "tauri.event-send")
                .platform(error.to_string())
        })
    }

    async fn emit_connection_lost(
        &self,
        caller_key: &str,
        expected_lease: (&str, &str),
        identity: ConnectionEventIdentity<'_>,
    ) -> Result<(), DispatchError> {
        self.emit_connection_failure(
            caller_key,
            expected_lease,
            identity,
            "peer-link-loss",
            "connection-lost",
        )
        .await
    }

    async fn emit_connection_failure(
        &self,
        caller_key: &str,
        expected_lease: (&str, &str),
        identity: ConnectionEventIdentity<'_>,
        cause: &str,
        terminal_reason: &str,
    ) -> Result<(), DispatchError> {
        let event = {
            let mut state = self.inner.lock().await;
            let attachment = state.attachment.clone().ok_or_else(|| {
                DispatchError::new(
                    "lifecycle.invalid-state",
                    "connection",
                    "tauri.connection-events-attachment",
                )
            })?;
            let caller = state.callers.get_mut(caller_key).ok_or_else(|| {
                DispatchError::new(
                    "ownership.denied",
                    "connection",
                    "tauri.connection-events-owner",
                )
            })?;
            if caller.lease_id != expected_lease.0 || caller.lease_generation != expected_lease.1 {
                return Err(DispatchError::new(
                    "ownership.denied",
                    "connection",
                    "tauri.connection-events-stale-lease",
                ));
            }
            let resource = caller
                .connection_events
                .get_mut(identity.stream_id)
                .ok_or_else(|| {
                    DispatchError::new(
                        "gatt.stale-handle",
                        "connection",
                        "tauri.connection-events-stream",
                    )
                })?;
            if !resource.active {
                return Ok(());
            }
            resource.sequence = resource.sequence.saturating_add(1);
            object([
                ("kind", string("connection-lifecycle")),
                ("schemaVersion", number(2)),
                ("attachment", attachment_record(&attachment)),
                ("attachmentId", string(attachment.attachment_id)),
                ("peerId", string(identity.peer_id)),
                ("connectionId", string(identity.connection_id)),
                (
                    "connectionGeneration",
                    string(identity.connection_generation),
                ),
                ("ownerLeaseId", string(caller.lease_id.clone())),
                ("sequence", number(resource.sequence as i64)),
                ("backendIngressOrdinal", IpcValue::Null),
                ("previous", string("connected")),
                ("current", string("lost")),
                ("cause", string(cause)),
            ])
        };
        let send_result = self
            .emit(
                caller_key,
                Some(expected_lease),
                identity.stream_id,
                event,
                false,
            )
            .await;
        // A full event acknowledgement queue cannot silently remove the
        // lifecycle source. Keep retrying the terminal until it is delivered
        // or the renderer explicitly revokes the lease and ownership denial
        // proves that cleanup has taken over.
        let terminal_reason = if send_result.is_ok() {
            terminal_reason
        } else {
            "overflow"
        };
        let mut terminal_delay = Duration::from_millis(100);
        let terminal_result = loop {
            match self
                .terminal(
                    caller_key,
                    expected_lease,
                    identity.stream_id,
                    terminal_reason,
                )
                .await
            {
                Ok(()) => break Ok(()),
                Err(error) if error.code == "ownership.denied" => break Err(error),
                Err(_error) => {
                    tokio::time::sleep(terminal_delay).await;
                    terminal_delay =
                        std::cmp::min(terminal_delay.saturating_mul(2), Duration::from_secs(5));
                }
            }
        };
        terminal_result?;
        let mut state = self.inner.lock().await;
        if let Some(caller) = state.callers.get_mut(caller_key) {
            if caller.lease_id == expected_lease.0 && caller.lease_generation == expected_lease.1 {
                if let Some(resource) = caller.connection_events.remove(identity.stream_id) {
                    if let Some(task) = resource.task {
                        task.abort();
                    }
                }
            }
        }
        send_result
    }

    async fn emit_scan_peripheral(
        &self,
        peripheral: &Peripheral,
        request: ScanObservation<'_>,
    ) -> Result<(), DispatchError> {
        {
            let state = self.inner.lock().await;
            let caller = state.callers.get(request.owner).ok_or_else(|| {
                DispatchError::new("ownership.denied", "stream", "tauri.scan-event-owner")
            })?;
            if caller.lease_id != request.expected_lease.0
                || caller.lease_generation != request.expected_lease.1
            {
                return Err(DispatchError::new(
                    "ownership.denied",
                    "stream",
                    "tauri.scan-event-stale-lease",
                ));
            }
            if caller.scan_admitting {
                return Ok(());
            }
        }
        let properties = peripheral.properties().await.ok().flatten();
        if !scan_properties_match_optional(
            properties.as_ref(),
            request.requested_services,
            request.local_name_prefix,
            request.manufacturer_filters,
        ) {
            return Ok(());
        }
        let Some(properties) = properties else {
            return Ok(());
        };
        let observation = peripheral_observation(peripheral, properties);
        self.emit(
            request.owner,
            Some(request.expected_lease),
            request.stream_handle,
            observation,
            true,
        )
        .await
    }

    async fn terminal(
        &self,
        caller_key: &str,
        expected_lease: (&str, &str),
        stream_id: &str,
        reason: &str,
    ) -> Result<(), DispatchError> {
        let (sink, lease_id, lease_generation, event_id) = {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(caller_key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "stream", "tauri.terminal-owner")
            })?;
            if caller_state.lease_id != expected_lease.0
                || caller_state.lease_generation != expected_lease.1
            {
                return Err(DispatchError::new(
                    "ownership.denied",
                    "stream",
                    "tauri.terminal-stale-lease",
                ));
            }
            let event_id = self.id("event-terminal");
            caller_state.pending_events.insert(event_id.clone());
            (
                caller_state.event_sink.clone(),
                caller_state.lease_id.clone(),
                caller_state.lease_generation.clone(),
                event_id,
            )
        };
        let send_result = sink.send(object([
            (
                "rendererLease",
                object([
                    ("leaseId", string(lease_id.clone())),
                    ("generation", string(lease_generation.clone())),
                ]),
            ),
            ("eventId", string(event_id.clone())),
            ("streamId", string(stream_id)),
            (
                "item",
                object([("kind", string("terminal")), ("reason", string(reason))]),
            ),
        ]));
        if send_result.is_err() {
            let mut state = self.inner.lock().await;
            if let Some(caller_state) = state.callers.get_mut(caller_key) {
                if caller_state.lease_id == lease_id
                    && caller_state.lease_generation == lease_generation
                {
                    caller_state.pending_events.remove(&event_id);
                }
            }
        }
        send_result.map_err(|error| {
            DispatchError::new("platform.transport", "stream", "tauri.terminal-send")
                .platform(error.to_string())
        })
    }

    async fn fail_scan_stream(
        &self,
        caller_key: &str,
        expected_lease: (&str, &str),
        stream_id: &str,
    ) {
        let should_stop = {
            let mut state = self.inner.lock().await;
            matches!(
                begin_scan_stop(&mut state, caller_key, expected_lease, stream_id, false),
                Ok(ScanStopBegin::Started)
            )
        };
        if !should_stop {
            return;
        }
        let outcome = match self.adapter().await {
            Ok(adapter) => adapter.stop_scan().await.map_err(|error| error.to_string()),
            Err(error) => Err(error
                .platform
                .unwrap_or_else(|| "adapter unavailable".to_owned())),
        };
        let mut state = self.inner.lock().await;
        if let Err(error) = apply_scan_stop_outcome(&mut state, outcome) {
            eprintln!(
                "[unified-ble:tauri] scan stream stop failed, resource retained for retry: {error}"
            );
        }
    }

    async fn fail_subscription_stream(
        &self,
        caller_key: &str,
        expected_lease: (&str, &str),
        stream_id: &str,
    ) {
        let subscription = {
            let mut state = self.inner.lock().await;
            let Some(caller) = state.callers.get_mut(caller_key) else {
                return;
            };
            if caller.lease_id != expected_lease.0 || caller.lease_generation != expected_lease.1 {
                return;
            }
            caller.subscriptions.remove(stream_id)
        };
        if let Some(subscription) = subscription {
            let native = subscription
                .native()
                .map(|(peripheral, characteristic)| (peripheral.clone(), characteristic.clone()));
            let failed = match native {
                Some((peripheral, characteristic)) => {
                    peripheral.unsubscribe(&characteristic).await.is_err()
                }
                None => true,
            };
            if failed {
                let mut state = self.inner.lock().await;
                retain_failed_subscription(
                    &mut state,
                    caller_key,
                    expected_lease,
                    stream_id,
                    Some(subscription),
                );
            }
        }
    }

    async fn release(&self, key: &str) -> IpcValue {
        let caller = self.inner.lock().await.callers.remove(key);
        let caller_cleanup = if let Some(mut caller) = caller {
            let cleanup = self.settle_caller(key, &mut caller).await;
            if !is_released(&cleanup) {
                let mut state = self.inner.lock().await;
                state.callers.entry(key.to_owned()).or_insert(caller);
            }
            cleanup
        } else {
            released()
        };
        let orphan_cleanup = self.settle_orphans(key).await;
        merge_cleanup(caller_cleanup, orphan_cleanup)
    }

    fn is_revoked(&self, key: &str) -> bool {
        self.revoked_callers
            .lock()
            .expect("revocation mutex poisoned")
            .contains_key(key)
    }

    async fn release_revoked(&self, key: String, revocation: u64) {
        let _admission = self.bootstrap_admission.lock().await;
        if self
            .revoked_callers
            .lock()
            .expect("revocation mutex poisoned")
            .get(&key)
            .copied()
            != Some(revocation)
        {
            return;
        }
        let cleanup = self.release(&key).await;
        if is_released(&cleanup) {
            let mut revoked = self
                .revoked_callers
                .lock()
                .expect("revocation mutex poisoned");
            if revoked.get(&key).copied() == Some(revocation) {
                revoked.remove(&key);
            }
        }
    }

    async fn settle_caller(&self, key: &str, caller: &mut CallerState) -> IpcValue {
        let mut failures = Vec::new();
        caller.quarantine.cancel();
        for exhausted in caller.quarantine.failures.drain(..) {
            failures.push(cleanup_failure(
                "cleanup",
                "tauri.quarantine.exhausted",
                format!("{}:{}", exhausted.command, exhausted.handle),
            ));
        }
        for cancellation in caller.operations.values() {
            cancellation.cancel();
        }
        caller.operations.clear();
        if caller.scan_admitting {
            caller.scan_admitting = false;
            let should_stop = {
                let mut state = self.inner.lock().await;
                if state
                    .stopping_scan
                    .as_ref()
                    .is_some_and(|stopping| stopping.caller_key == key)
                {
                    false
                } else if state.scan_owner.as_deref() == Some(key) {
                    state.stopping_scan = Some(StoppingScan {
                        caller_key: key.to_owned(),
                        lease_id: caller.lease_id.clone(),
                        lease_generation: caller.lease_generation.clone(),
                        handle: String::new(),
                    });
                    true
                } else {
                    false
                }
            };
            if should_stop {
                let outcome = match self.adapter().await {
                    Ok(adapter) => adapter.stop_scan().await.map_err(|error| error.to_string()),
                    Err(error) => Err(error
                        .platform
                        .unwrap_or_else(|| "adapter unavailable".to_owned())),
                };
                let mut state = self.inner.lock().await;
                if let Err(error) = apply_scan_stop_outcome(&mut state, outcome) {
                    failures.push(cleanup_failure(
                        "scan",
                        "tauri.release.scan-admitting",
                        error,
                    ));
                }
            }
        }
        for resource in caller.connection_events.values_mut() {
            if let Some(task) = resource.task.take() {
                task.abort();
            }
        }
        caller.connection_events.clear();
        if let Some(scan) = caller.scan.as_ref() {
            scan.task.abort();
            match self.adapter().await {
                Ok(adapter) => match adapter.stop_scan().await {
                    Ok(()) => {
                        caller.scan.take();
                        let mut state = self.inner.lock().await;
                        if state.scan_owner.as_deref() == Some(key) {
                            state.scan_owner = None;
                        }
                    }
                    Err(error) => failures.push(cleanup_failure(
                        "scan",
                        "tauri.release.scan",
                        error.to_string(),
                    )),
                },
                Err(error) => failures.push(cleanup_failure(
                    "scan",
                    "tauri.release.scan-adapter",
                    error
                        .platform
                        .unwrap_or_else(|| "adapter unavailable".to_owned()),
                )),
            }
        }
        let subscription_handles = caller.subscriptions.keys().cloned().collect::<Vec<_>>();
        for handle in subscription_handles {
            let Some(subscription) = caller.subscriptions.get(&handle) else {
                continue;
            };
            subscription.task.abort();
            match subscription.native() {
                Some((peripheral, characteristic)) => {
                    match peripheral.unsubscribe(characteristic).await {
                        Ok(()) => {
                            caller.subscriptions.remove(&handle);
                        }
                        Err(error) => failures.push(cleanup_failure(
                            "subscription",
                            "tauri.release.subscription",
                            error.to_string(),
                        )),
                    }
                }
                None => failures.push(cleanup_failure(
                    "subscription",
                    "tauri.release.subscription",
                    "pending subscription has no native resource to release".to_owned(),
                )),
            }
        }
        let connection_handles = caller.connections.keys().cloned().collect::<Vec<_>>();
        for handle in connection_handles {
            let Some(connection) = caller.connections.get(&handle).cloned() else {
                continue;
            };
            match disconnect_peripheral(&connection.peripheral).await {
                Ok(()) => {
                    caller.connections.remove(&handle);
                    caller
                        .databases
                        .retain(|_, database| database.connection_handle != handle);
                    self.clear_peer_owner(&connection.peer_id, key).await;
                }
                Err(error) => failures.push(cleanup_failure(
                    "connection",
                    "tauri.release.connection",
                    error.to_string(),
                )),
            }
        }
        cleanup_record(failures)
    }

    async fn settle_orphans(&self, key: &str) -> IpcValue {
        let mut failures = Vec::new();
        let stopping = {
            let state = self.inner.lock().await;
            state
                .stopping_scan
                .as_ref()
                .is_some_and(|stopping| stopping.caller_key == key)
        };
        if stopping {
            match self.adapter().await {
                Ok(adapter) => match adapter.stop_scan().await {
                    Ok(()) => {
                        let mut state = self.inner.lock().await;
                        if let Err(error) = apply_scan_stop_outcome(&mut state, Ok(())) {
                            failures.push(cleanup_failure("scan", "tauri.release.scan", error));
                        }
                    }
                    Err(error) => failures.push(cleanup_failure(
                        "scan",
                        "tauri.release.scan",
                        error.to_string(),
                    )),
                },
                Err(error) => failures.push(cleanup_failure(
                    "scan",
                    "tauri.release.scan-adapter",
                    error
                        .platform
                        .unwrap_or_else(|| "adapter unavailable".to_owned()),
                )),
            }
        }
        let orphans = {
            let state = self.inner.lock().await;
            state
                .orphan_connections
                .values()
                .filter(|orphan| orphan.caller_key == key)
                .cloned()
                .collect::<Vec<_>>()
        };
        for orphan in orphans {
            if let Some(peripheral) = orphan.peripheral.clone() {
                let outcome = disconnect_with_state_check(
                    async {
                        peripheral
                            .disconnect()
                            .await
                            .map_err(|error| error.to_string())
                    },
                    || connected_after_failed_disconnect(&peripheral),
                )
                .await;
                if let Some(error) = self
                    .settle_orphan_connection(outcome, &orphan.peer_id, &orphan.lease_generation)
                    .await
                {
                    failures.push(cleanup_failure(
                        "connection",
                        "tauri.release.orphan-connection",
                        error,
                    ));
                }
            } else {
                failures.push(cleanup_failure(
                    "connection",
                    "tauri.release.orphan-connection",
                    format!(
                        "pending connection for {} has no peripheral to release",
                        orphan.peer_id
                    ),
                ));
            }
        }
        let subscription_orphans = {
            let state = self.inner.lock().await;
            state
                .orphan_subscription_owners
                .values()
                .filter(|orphan| orphan.caller_key == key)
                .map(|orphan| orphan.stream_id.clone())
                .collect::<Vec<_>>()
        };
        for stream_id in subscription_orphans {
            let resource = {
                let mut state = self.inner.lock().await;
                state.orphan_subscription_resources.remove(&stream_id)
            };
            let Some(subscription) = resource else {
                failures.push(cleanup_failure(
                    "subscription",
                    "tauri.release.orphan-subscription",
                    format!("pending subscription {stream_id} has no resource to release"),
                ));
                continue;
            };
            let native = subscription
                .native()
                .map(|(peripheral, characteristic)| (peripheral.clone(), characteristic.clone()));
            let Some((peripheral, characteristic)) = native else {
                let mut state = self.inner.lock().await;
                state
                    .orphan_subscription_resources
                    .insert(stream_id.clone(), subscription);
                failures.push(cleanup_failure(
                    "subscription",
                    "tauri.release.orphan-subscription",
                    format!("pending subscription {stream_id} has no native resource to release"),
                ));
                continue;
            };
            match peripheral.unsubscribe(&characteristic).await {
                Ok(()) => {
                    let generation = {
                        let state = self.inner.lock().await;
                        state
                            .orphan_subscription_owners
                            .get(&stream_id)
                            .map(|orphan| orphan.lease_generation.clone())
                    };
                    if let Some(generation) = generation {
                        let _ = self
                            .settle_orphan_subscription(Ok(()), &stream_id, &generation)
                            .await;
                    }
                }
                Err(error) => {
                    let mut state = self.inner.lock().await;
                    state
                        .orphan_subscription_resources
                        .insert(stream_id.clone(), subscription);
                    if let Some(owner) = state.orphan_subscription_owners.get_mut(&stream_id) {
                        owner.attempts = owner.attempts.saturating_add(1);
                    }
                    failures.push(cleanup_failure(
                        "subscription",
                        "tauri.release.orphan-subscription",
                        error.to_string(),
                    ));
                }
            }
        }
        cleanup_record(failures)
    }
}

/// Disconnects a peripheral while preserving retry ownership unless the
/// platform confirms that the physical link is already gone.
///
/// CoreBluetooth may report an error when its disconnect completion races the
/// connection-event monitor. A failed command is therefore idempotent only
/// when a fresh `is_connected` reading proves the requested end state.
async fn disconnect_peripheral(peripheral: &Peripheral) -> Result<(), String> {
    disconnect_with_state_check(
        async {
            peripheral
                .disconnect()
                .await
                .map_err(|error| error.to_string())
        },
        || async { connected_after_failed_disconnect(peripheral).await },
    )
    .await
}

/// Answer "is it still connected?" after a disconnect has already failed.
///
/// The state check is classified *before* the error is rendered to a string,
/// because the typed error carries the only evidence that distinguishes "the
/// peer is gone" from "we could not tell", and stringifying first throws it
/// away at the one call site that has it.
///
/// On BlueZ a removed D-Bus device object is not a failure of this question -
/// it is the answer. `is_connected()` reads `Device1` properties, so once BlueZ
/// has dropped the object the read errors, and treating that as indeterminate
/// retains ownership of a peer that is provably released, permanently: the
/// object never comes back, so every retry fails identically.
async fn connected_after_failed_disconnect(peripheral: &Peripheral) -> Result<bool, String> {
    match peripheral.is_connected().await {
        Ok(connected) => Ok(connected),
        Err(error) if error_confirms_device_released(&error) => {
            // Reported, not swallowed: the outcome is "released", but the error
            // is still the only account of why the disconnect failed.
            eprintln!(
                "[unified-ble:tauri] the peer's device object is gone, so it is released \
                 despite the state check erroring: {error}"
            );
            Ok(false)
        }
        Err(error) => Err(error.to_string()),
    }
}

/// The admission failure, plus what compensation could not undo.
///
/// A caller that is denied admission still needs to know whether a peer was
/// left connected on its behalf - that is the difference between "try again"
/// and "a peer is stranded until something reclaims it".
fn compensation_failure(operation: &str, residue: Option<String>) -> DispatchError {
    let error = DispatchError::new("ownership.denied", "connection", operation);
    match residue {
        None => error,
        Some(detail) => error.platform(format!(
            "the peer could not be confirmed released, so its reservation is retained: {detail}"
        )),
    }
}

fn subscription_compensation_failure(operation: &str, residue: Option<String>) -> DispatchError {
    let error = DispatchError::new("ownership.denied", "gatt", operation);
    match residue {
        None => error,
        Some(detail) => error.platform(format!(
            "the subscription could not be confirmed released, so its orphan owner is retained: {detail}"
        )),
    }
}

/// True when a state-check error proves the peer is no longer present.
///
/// The discriminator is the D-Bus error *name*, which is a protocol constant,
/// not rendered text - so this does not depend on how any crate in the chain
/// formats its errors.
#[cfg(target_os = "linux")]
fn error_confirms_device_released(error: &btleplug::Error) -> bool {
    let btleplug::Error::Other(inner) = error else {
        return false;
    };
    let Some(bluez_async::BluetoothError::DbusError(dbus_error)) =
        inner.downcast_ref::<bluez_async::BluetoothError>()
    else {
        return false;
    };
    matches!(
        dbus_error.name(),
        Some("org.freedesktop.DBus.Error.UnknownObject" | "org.bluez.Error.DoesNotExist")
    )
}

/// No equivalent evidence exists off Linux: CoreBluetooth and WinRT report a
/// missing peer as `Ok(false)` rather than as an error, so nothing reaches here.
#[cfg(not(target_os = "linux"))]
fn error_confirms_device_released(_error: &btleplug::Error) -> bool {
    false
}

async fn disconnect_with_state_check<DisconnectFuture, StateFuture>(
    disconnect: DisconnectFuture,
    connected_after_failure: impl FnOnce() -> StateFuture,
) -> Result<(), String>
where
    DisconnectFuture: Future<Output = Result<(), String>>,
    StateFuture: Future<Output = Result<bool, String>>,
{
    let disconnect_error =
        match tokio::time::timeout(DISCONNECT_COMPLETION_TIMEOUT, disconnect).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => error,
            Err(_) => format!(
                "disconnect completion timed out after {} ms",
                DISCONNECT_COMPLETION_TIMEOUT.as_millis()
            ),
        };
    let connected_after_failure =
        tokio::time::timeout(DISCONNECT_COMPLETION_TIMEOUT, connected_after_failure())
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "post-disconnect state check timed out after {} ms",
                    DISCONNECT_COMPLETION_TIMEOUT.as_millis()
                ))
            });
    resolve_disconnect_failure(disconnect_error, connected_after_failure)
}

/// Decide whether a failed disconnect nevertheless left the peer released.
///
/// The `Ok(false)` arm reports success, but the underlying platform error is
/// still the only account of why the disconnect failed in the first place.
/// Dropping it turns a specific fault into "nothing happened" - the failure
/// mode this repository explicitly forbids - and it is what made #145 take two
/// physical re-test sessions to characterise. So the error is recorded on the
/// way past even when the outcome is success.
fn resolve_disconnect_failure(
    disconnect_error: String,
    connected_after_failure: Result<bool, String>,
) -> Result<(), String> {
    match connected_after_failure {
        Ok(false) => {
            eprintln!(
                "[unified-ble:tauri] disconnect reported an error but the peer is no longer \
                 connected; releasing anyway: {disconnect_error}"
            );
            Ok(())
        }
        Ok(true) => Err(disconnect_error),
        Err(state_error) => Err(format!(
            "{disconnect_error}; post-disconnect state check failed: {state_error}"
        )),
    }
}

fn should_clear_peer_owner(
    removed_peer_id: Option<&str>,
    requested_peer_id: &str,
    current_owner: Option<&str>,
    caller_key: &str,
) -> bool {
    removed_peer_id == Some(requested_peer_id) && current_owner == Some(caller_key)
}

fn connection_cleanup_payload(payload: &BTreeMap<String, IpcValue>) -> Option<IpcValue> {
    let handle = payload.get("handle").and_then(as_string)?.to_owned();
    let peer_id = payload.get("peerId").and_then(as_string)?.to_owned();
    let connection_id = payload.get("connectionId").and_then(as_string)?.to_owned();
    let owner_lease_id = payload.get("ownerLeaseId").and_then(as_string)?.to_owned();
    let connection_generation = payload
        .get("connectionGeneration")
        .and_then(as_string)?
        .to_owned();
    Some(object([
        ("connectionHandle", string(handle)),
        ("peerId", string(peer_id)),
        ("connectionId", string(connection_id)),
        ("ownerLeaseId", string(owner_lease_id)),
        ("connectionGeneration", string(connection_generation)),
    ]))
}

impl IpcDispatcher for BtleplugDispatcher {
    fn dispatch<'a>(
        &'a self,
        caller: AuthenticatedCaller,
        request: IpcValue,
        event_sink: Option<IpcEventSink>,
    ) -> DispatchFuture<'a> {
        Box::pin(async move {
            self.dispatch_request(caller, request, event_sink)
                .await
                .unwrap_or_else(DispatchError::into_response)
        })
    }

    fn release_caller(&self, caller: AuthenticatedCaller) {
        let key = caller_key(&caller);
        let revocation = self.next_revocation.fetch_add(1, Ordering::Relaxed);
        self.revoked_callers
            .lock()
            .expect("revocation mutex poisoned")
            .insert(key.clone(), revocation);
        let dispatcher = self.clone();
        tauri::async_runtime::spawn(async move {
            dispatcher.release_revoked(key, revocation).await;
        });
    }
}

fn quarantine_handle(payload: &BTreeMap<String, IpcValue>) -> Option<String> {
    [
        "scanHandle",
        "connectionHandle",
        "subscriptionHandle",
        "handle",
    ]
    .into_iter()
    .find_map(|key| payload.get(key).and_then(as_string).map(ToOwned::to_owned))
}

fn retain_orphan_connection(state: &mut DispatcherState, owner: OrphanConnectionOwner) {
    state
        .orphan_connections
        .insert(owner.peer_id.clone(), owner);
}

fn retain_orphan_subscription(
    state: &mut DispatcherState,
    owner: OrphanSubscriptionOwner,
    resource: Option<SubscriptionResource>,
) {
    let stream_id = owner.stream_id.clone();
    state
        .orphan_subscription_owners
        .insert(stream_id.clone(), owner);
    if let Some(resource) = resource {
        state
            .orphan_subscription_resources
            .insert(stream_id, resource);
    }
}

fn scan_start_blocked(state: &DispatcherState) -> bool {
    state.scan_owner.is_some() || state.stopping_scan.is_some()
}

enum ScanStopBegin {
    Started,
    AlreadyReleased,
}

fn begin_scan_stop(
    state: &mut DispatcherState,
    caller_key: &str,
    expected_lease: (&str, &str),
    stream_id: &str,
    abort_pump: bool,
) -> Result<ScanStopBegin, DispatchError> {
    if let Some(stopping) = state.stopping_scan.as_ref() {
        if stopping.caller_key == caller_key
            && stopping.lease_id == expected_lease.0
            && stopping.lease_generation == expected_lease.1
            && stopping.handle == stream_id
        {
            return Ok(ScanStopBegin::Started);
        }
        if stopping.caller_key == caller_key && stopping.handle != stream_id {
            return Err(DispatchError::new(
                "ownership.denied",
                "scan",
                "tauri.scan-stop-handle",
            ));
        }
        return Err(DispatchError::new(
            "scan.already-active",
            "scan",
            "tauri.scan-stop-in-progress",
        ));
    }
    let scan = {
        let Some(caller) = state.callers.get_mut(caller_key) else {
            return Err(DispatchError::new(
                "ownership.denied",
                "scan",
                "tauri.scan-stop-owner",
            ));
        };
        if caller.lease_id != expected_lease.0 || caller.lease_generation != expected_lease.1 {
            return Err(DispatchError::new(
                "ownership.denied",
                "scan",
                "tauri.scan-stop-owner",
            ));
        }
        caller.scan.take()
    };
    match scan {
        Some(scan) if scan.handle == stream_id => {
            if abort_pump {
                scan.task.abort();
            }
            state.stopping_scan = Some(StoppingScan {
                caller_key: caller_key.to_owned(),
                lease_id: expected_lease.0.to_owned(),
                lease_generation: expected_lease.1.to_owned(),
                handle: stream_id.to_owned(),
            });
            Ok(ScanStopBegin::Started)
        }
        Some(scan) => {
            if let Some(caller) = state.callers.get_mut(caller_key) {
                caller.scan = Some(scan);
            }
            Err(DispatchError::new(
                "ownership.denied",
                "scan",
                "tauri.scan-stop-handle",
            ))
        }
        None => Ok(ScanStopBegin::AlreadyReleased),
    }
}

fn apply_scan_stop_outcome(
    state: &mut DispatcherState,
    outcome: Result<(), String>,
) -> Result<(), String> {
    match outcome {
        Ok(()) => {
            let Some(stopping) = state.stopping_scan.take() else {
                return Ok(());
            };
            let protect_replacement =
                state
                    .callers
                    .get(&stopping.caller_key)
                    .is_some_and(|caller| {
                        (caller.lease_id != stopping.lease_id
                            || caller.lease_generation != stopping.lease_generation)
                            && caller.scan.is_some()
                    });
            if state.scan_owner.as_deref() == Some(stopping.caller_key.as_str())
                && !protect_replacement
            {
                state.scan_owner = None;
            }
            if let Some(caller) = state.callers.get_mut(&stopping.caller_key) {
                if caller.lease_id == stopping.lease_id
                    && caller.lease_generation == stopping.lease_generation
                    && caller
                        .scan
                        .as_ref()
                        .is_some_and(|scan| scan.handle == stopping.handle)
                {
                    caller.scan.take();
                }
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn retain_failed_subscription(
    state: &mut DispatcherState,
    caller_key: &str,
    expected_lease: (&str, &str),
    stream_id: &str,
    subscription: Option<SubscriptionResource>,
) {
    let lease_matches = state.callers.get(caller_key).is_some_and(|caller| {
        caller.lease_id == expected_lease.0 && caller.lease_generation == expected_lease.1
    });
    if lease_matches {
        if let Some(subscription) = subscription {
            if let Some(caller) = state.callers.get_mut(caller_key) {
                caller
                    .subscriptions
                    .insert(stream_id.to_owned(), subscription);
            }
        }
        return;
    }
    retain_orphan_subscription(
        state,
        OrphanSubscriptionOwner {
            caller_key: caller_key.to_owned(),
            lease_id: expected_lease.0.to_owned(),
            lease_generation: expected_lease.1.to_owned(),
            stream_id: stream_id.to_owned(),
            attempts: 1,
        },
        subscription,
    );
}

fn merge_cleanup(left: IpcValue, right: IpcValue) -> IpcValue {
    let mut failures = Vec::new();
    for value in [left, right] {
        if let IpcValue::Object(record) = value {
            if let Some(IpcValue::Array(existing)) = record.get("failures") {
                failures.extend(existing.iter().cloned());
            }
        }
    }
    cleanup_record(failures)
}

fn prune_completed_correlations(completed: &mut HashMap<String, Instant>, now: Instant) {
    completed
        .retain(|_, completed_at| now.duration_since(*completed_at) < COMPLETED_CORRELATION_TTL);
}

fn is_cleanup_command(command: &str) -> bool {
    matches!(
        command,
        "scan.stop" | "gatt.unsubscribe" | "connection.disconnect"
    )
}

fn admit_caller_correlation(
    operations: &HashMap<String, CancellationToken>,
    completed: &mut HashMap<String, Instant>,
    correlation: &str,
    command: &str,
    now: Instant,
) -> Result<(), DispatchError> {
    prune_completed_correlations(completed, now);
    if operations.contains_key(correlation) || completed.contains_key(correlation) {
        return Err(DispatchError::new(
            "protocol.violation",
            "ipc",
            "tauri.correlation-replay",
        ));
    }
    if is_cleanup_command(command) {
        return Ok(());
    }
    if operations.len() >= MAX_CORRELATIONS {
        return Err(
            DispatchError::new("stream.quota", "ipc", "tauri.correlation-busy").retryable(),
        );
    }
    Ok(())
}

fn remember_completed_correlation(
    operations: &mut HashMap<String, CancellationToken>,
    completed: &mut HashMap<String, Instant>,
    correlation: String,
    now: Instant,
) {
    operations.remove(&correlation);
    completed.insert(correlation, now);
}

fn required_lease(
    request: &BTreeMap<String, IpcValue>,
    operation: &'static str,
) -> Result<(String, String), DispatchError> {
    let lease = into_object(
        required_value(request, "rendererLease", operation)?.clone(),
        operation,
    )?;
    Ok((
        required_string(&lease, "leaseId", operation)?,
        required_string(&lease, "generation", operation)?,
    ))
}

fn validate_lease(
    caller: &CallerState,
    lease: &(String, String),
    operation: &'static str,
) -> Result<(), DispatchError> {
    if caller.lease_id != lease.0 || caller.lease_generation != lease.1 {
        return Err(DispatchError::new("ownership.denied", "ipc", operation));
    }
    Ok(())
}

fn expected_lease_matches(caller: &CallerState, payload: &BTreeMap<String, IpcValue>) -> bool {
    payload
        .get("__expectedLeaseId")
        .and_then(as_string)
        .is_some_and(|lease_id| lease_id == caller.lease_id)
        && payload
            .get("__expectedLeaseGeneration")
            .and_then(as_string)
            .is_some_and(|generation| generation == caller.lease_generation)
}

async fn open_btleplug_adapter(
    requested: Option<String>,
) -> Result<(Manager, Adapter, String), DispatchError> {
    let manager = Manager::new().await.map_err(|error| {
        DispatchError::new("adapter.unavailable", "adapter", "tauri.manager")
            .platform(error.to_string())
    })?;
    let adapters = manager.adapters().await.map_err(|error| {
        DispatchError::new("adapter.unavailable", "adapter", "tauri.adapters")
            .platform(error.to_string())
    })?;
    if adapters.is_empty() {
        return Err(DispatchError::new(
            "adapter.unavailable",
            "adapter",
            "tauri.adapters-empty",
        ));
    }
    let mut candidates = Vec::with_capacity(adapters.len());
    for adapter in adapters {
        let info = adapter.adapter_info().await.map_err(|error| {
            DispatchError::new("adapter.unavailable", "adapter", "tauri.adapter-info")
                .platform(error.to_string())
        })?;
        candidates.push((info, adapter));
    }
    let (adapter_name, adapter) = match requested {
        Some(requested) => candidates
            .into_iter()
            .find(|(info, _)| info == &requested)
            .ok_or_else(|| {
                DispatchError::new(
                    "adapter.selection-required",
                    "adapter",
                    "tauri.adapter-selection",
                )
            })?,
        None if candidates.len() == 1 => candidates.remove(0),
        None => {
            return Err(DispatchError::new(
                "adapter.ambiguous",
                "adapter",
                "tauri.adapter-selection",
            ))
        }
    };
    Ok((manager, adapter, adapter_name))
}

fn caller_key(caller: &AuthenticatedCaller) -> String {
    format!("{}\0{}", caller.app_identifier, caller.window_label)
}

fn peripheral_id(peripheral: &Peripheral) -> String {
    format!("{:?}", peripheral.id())
}

fn scan_event_peripheral_id(event: CentralEvent) -> Option<btleplug::platform::PeripheralId> {
    match event {
        CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => Some(id),
        CentralEvent::ManufacturerDataAdvertisement { id, .. }
        | CentralEvent::ServiceDataAdvertisement { id, .. }
        | CentralEvent::ServicesAdvertisement { id, .. } => Some(id),
        _ => None,
    }
}

fn scan_properties_match(
    properties: &btleplug::api::PeripheralProperties,
    requested_services: &[Uuid],
    local_name_prefix: Option<&str>,
    manufacturer_filters: &[ManufacturerFilter],
) -> bool {
    if !requested_services.is_empty()
        && !requested_services
            .iter()
            .all(|uuid| properties.services.contains(uuid))
    {
        return false;
    }
    if let Some(prefix) = local_name_prefix {
        if !properties
            .local_name
            .as_deref()
            .is_some_and(|name| name.starts_with(prefix))
        {
            return false;
        }
    }
    manufacturer_filters.iter().all(|filter| {
        properties
            .manufacturer_data
            .get(&filter.company_id)
            .is_some_and(|data| {
                filter
                    .data_prefix
                    .as_ref()
                    .map_or(true, |prefix| data.starts_with(prefix))
            })
    })
}

fn scan_properties_match_optional(
    properties: Option<&btleplug::api::PeripheralProperties>,
    requested_services: &[Uuid],
    local_name_prefix: Option<&str>,
    manufacturer_filters: &[ManufacturerFilter],
) -> bool {
    properties.is_some_and(|properties| {
        scan_properties_match(
            properties,
            requested_services,
            local_name_prefix,
            manufacturer_filters,
        )
    })
}

fn peripheral_observation(
    peripheral: &Peripheral,
    properties: btleplug::api::PeripheralProperties,
) -> IpcValue {
    let manufacturer_data = properties
        .manufacturer_data
        .into_iter()
        .map(|(company_id, data)| {
            object([
                ("companyId", number(i64::from(company_id))),
                ("data", IpcValue::Bytes(data)),
            ])
        })
        .collect();
    let service_data = properties
        .service_data
        .into_iter()
        .map(|(uuid, data)| {
            object([
                ("uuid", string(uuid.to_string())),
                ("data", IpcValue::Bytes(data)),
            ])
        })
        .collect();
    object([
        ("peerId", string(peripheral_id(peripheral))),
        (
            "localName",
            properties.local_name.map_or(IpcValue::Null, string),
        ),
        (
            "rssi",
            properties
                .rssi
                .map_or(IpcValue::Null, |value| number(i64::from(value))),
        ),
        (
            "txPowerLevel",
            properties
                .tx_power_level
                .map_or(IpcValue::Null, |value| number(i64::from(value))),
        ),
        (
            "serviceUuids",
            IpcValue::Array(
                properties
                    .services
                    .into_iter()
                    .map(|uuid| string(uuid.to_string()))
                    .collect(),
            ),
        ),
        ("manufacturerData", IpcValue::Array(manufacturer_data)),
        ("serviceData", IpcValue::Array(service_data)),
    ])
}

fn characteristic_properties(properties: CharPropFlags) -> IpcValue {
    let mut values = Vec::new();
    for (flag, name) in [
        (CharPropFlags::READ, "read"),
        (CharPropFlags::WRITE, "write"),
        (
            CharPropFlags::WRITE_WITHOUT_RESPONSE,
            "write-without-response",
        ),
        (CharPropFlags::NOTIFY, "notify"),
        (CharPropFlags::INDICATE, "indicate"),
    ] {
        if properties.contains(flag) {
            values.push(string(name));
        }
    }
    IpcValue::Array(values)
}

fn parse_uuid(value: &str, operation: &'static str) -> Result<Uuid, DispatchError> {
    let canonical = match value.len() {
        4 => format!("0000{value}-0000-1000-8000-00805f9b34fb"),
        8 => format!("{value}-0000-1000-8000-00805f9b34fb"),
        _ => value.to_owned(),
    };
    Uuid::parse_str(&canonical)
        .map_err(|_| DispatchError::new("scan.filter-invalid", "scan", operation))
}

struct ManufacturerFilter {
    company_id: u16,
    data_prefix: Option<Vec<u8>>,
}

fn attachment_record(attachment: &Attachment) -> IpcValue {
    object([
        ("attachmentId", string(attachment.attachment_id.clone())),
        (
            "backendInstanceId",
            string(attachment.backend_instance_id.clone()),
        ),
        (
            "backendGeneration",
            string(attachment.backend_generation.clone()),
        ),
        (
            "adapter",
            object([
                ("adapterId", string(attachment.adapter_id.clone())),
                ("displayName", string(attachment.adapter_name.clone())),
                ("state", adapter_state(attachment)),
                (
                    "adapterGeneration",
                    string(attachment.adapter_generation.clone()),
                ),
                ("limitations", adapter_limitations()),
            ]),
        ),
    ])
}

/// Adapter limitations this host can state as fact.
///
/// Both are properties of this dispatcher, verifiable in this file:
/// `open_btleplug_adapter` selects one adapter when the attachment is created
/// and nothing re-selects it afterwards, and the only messages this host pushes
/// through an event sink are stream `value` and `terminal` messages, never an
/// adapter-state change.
const ADAPTER_LIMITATIONS: [&str; 2] = [
    "This host binds one adapter for the lifetime of the attachment; the adapter is selected when the attachment is created and other adapters are not reachable through it.",
    "This host does not observe adapter-state changes; every adapter.state response is a fresh sample and no adapter-state event is emitted.",
];

fn adapter_limitations() -> IpcValue {
    IpcValue::Array(
        ADAPTER_LIMITATIONS
            .iter()
            .map(|limitation| string(*limitation))
            .collect(),
    )
}

/// What an adapter-state snapshot was actually able to observe.
enum AdapterSample<'a> {
    /// Attachment identity only: nothing was read from the adapter.
    Unsampled,
    /// Values read from the adapter while building this snapshot.
    Live { power: &'a str, heard: i64 },
}

const UNSAMPLED_SNAPSHOT_REASON: &str =
    "This snapshot carries attachment identity only; availability, power, and the heard peer count are not sampled here, so route adapter.state for a live reading.";

/// Snapshot for the attachment record, which reads nothing from the adapter.
fn adapter_state(attachment: &Attachment) -> IpcValue {
    adapter_state_snapshot(attachment, AdapterSample::Unsampled)
}

/// Snapshot for `adapter.state`, built from values just read from the adapter.
fn live_adapter_state(attachment: &Attachment, power: &str, heard: i64) -> IpcValue {
    adapter_state_snapshot(attachment, AdapterSample::Live { power, heard })
}

/// Builds an adapter-state snapshot in which every field is either observed or
/// explicitly absent.
///
/// `availability` and `power` are asserted only from a live read: the caller of
/// [`live_adapter_state`] reaches it only after `Adapter::adapter_state`
/// succeeded, which proves the platform still hands this process the adapter.
/// The unsampled path observes nothing and says so. `authorization` comes from
/// [`platform_authorization`], `updatedAt` from [`sample_epoch_millis`], and
/// `safeReason` is the joined set of caveats those readings actually carry, or
/// null when there are none.
fn adapter_state_snapshot(attachment: &Attachment, sample: AdapterSample<'_>) -> IpcValue {
    let clock = sample_epoch_millis();
    let authorization = platform_authorization();
    let (availability, power, heard) = match sample {
        AdapterSample::Live { power, heard } => (string("available"), string(power), number(heard)),
        AdapterSample::Unsampled => (string("unknown"), string("unknown"), IpcValue::Null),
    };
    let mut caveats: Vec<&str> = Vec::new();
    if matches!(sample, AdapterSample::Unsampled) {
        caveats.push(UNSAMPLED_SNAPSHOT_REASON);
    }
    if let Some(reason) = authorization.reason {
        caveats.push(reason);
    }
    if let Some(reason) = clock.reason {
        caveats.push(reason);
    }
    object([
        ("availability", availability),
        ("authorization", string(authorization.value)),
        ("power", power),
        ("heard", heard),
        (
            "backendGeneration",
            string(attachment.backend_generation.clone()),
        ),
        ("updatedAt", number(clock.epoch_millis)),
        ("safeReason", safe_reason(&caveats)),
    ])
}

fn safe_reason(caveats: &[&str]) -> IpcValue {
    if caveats.is_empty() {
        IpcValue::Null
    } else {
        string(caveats.join(" "))
    }
}

/// One platform authorization reading.
///
/// `value` is always a wire token from the adapter-state vocabulary
/// (`granted | denied | restricted | not-determined | unavailable | unknown`).
/// `unknown` is reported when this host obtained no reading — because the
/// platform exposes no per-application authorization concept, or because it was
/// not queried. It matches how the sibling `availability` and `power`
/// vocabularies already spell "not determined by this host", and it is never a
/// denial: readiness must not gate on it. `reason` carries the caveat that
/// belongs in `safeReason`, and is set only when the reading needs one.
struct AuthorizationReport {
    value: &'static str,
    reason: Option<&'static str>,
}

/// The adapter-state token meaning "this host obtained no authorization
/// reading". Never a denial.
const AUTHORIZATION_UNKNOWN: &str = "unknown";

/// `CBManagerAuthorization` raw values, macOS 10.15+ / iOS 13+.
#[cfg(any(target_os = "macos", test))]
const CORE_BLUETOOTH_AUTHORIZATION_NOT_DETERMINED: isize = 0;
#[cfg(any(target_os = "macos", test))]
const CORE_BLUETOOTH_AUTHORIZATION_RESTRICTED: isize = 1;
#[cfg(any(target_os = "macos", test))]
const CORE_BLUETOOTH_AUTHORIZATION_DENIED: isize = 2;
#[cfg(any(target_os = "macos", test))]
const CORE_BLUETOOTH_AUTHORIZATION_ALLOWED_ALWAYS: isize = 3;

#[cfg(any(target_os = "macos", test))]
const CORE_BLUETOOTH_AUTHORIZATION_UNRECOGNIZED_REASON: &str =
    "CoreBluetooth reported an authorization value this host does not recognize, so adapter authorization is reported absent.";

/// Maps a raw `CBManagerAuthorization` to the adapter-state wire vocabulary.
///
/// Values outside the documented enum are not forced into a token: they are
/// reported absent, because this host cannot say what such a value means.
#[cfg(any(target_os = "macos", test))]
fn map_core_bluetooth_authorization(raw: isize) -> AuthorizationReport {
    let value = match raw {
        CORE_BLUETOOTH_AUTHORIZATION_ALLOWED_ALWAYS => "granted",
        CORE_BLUETOOTH_AUTHORIZATION_DENIED => "denied",
        CORE_BLUETOOTH_AUTHORIZATION_RESTRICTED => "restricted",
        CORE_BLUETOOTH_AUTHORIZATION_NOT_DETERMINED => "not-determined",
        _ => {
            return AuthorizationReport {
                value: AUTHORIZATION_UNKNOWN,
                reason: Some(CORE_BLUETOOTH_AUTHORIZATION_UNRECOGNIZED_REASON),
            }
        }
    };
    AuthorizationReport {
        value,
        reason: None,
    }
}

/// Reads the live CoreBluetooth authorization state.
///
/// `+[CBManager authorization]` is macOS 10.15+, so both the class and the
/// class method are checked before the message is sent; on an older system the
/// value is reported absent instead of crashing on an unrecognized selector.
/// Reading the property does not prompt the user; only radio use does.
#[cfg(target_os = "macos")]
fn platform_authorization() -> AuthorizationReport {
    use objc2::{runtime::AnyClass, sel};
    use objc2_core_bluetooth::CBManager;

    let Some(class) = AnyClass::get("CBManager") else {
        return AuthorizationReport {
            value: AUTHORIZATION_UNKNOWN,
            reason: Some(
                "CoreBluetooth is not loaded in this process, so adapter authorization is reported absent.",
            ),
        };
    };
    if !class.metaclass().responds_to(sel!(authorization)) {
        return AuthorizationReport {
            value: AUTHORIZATION_UNKNOWN,
            reason: Some(
                "This macOS version does not expose +[CBManager authorization], so adapter authorization is reported absent.",
            ),
        };
    }
    // SAFETY: `+[CBManager authorization]` was just verified to exist on the
    // metaclass. It takes no arguments and returns `CBManagerAuthorization`,
    // which is an `NSInteger` with the encoding this binding declares.
    let authorization = unsafe { CBManager::authorization_class() };
    map_core_bluetooth_authorization(authorization.0)
}

/// Reports the BlueZ authorization model.
///
/// BlueZ has no per-application Bluetooth authorization state to read: access
/// is decided by D-Bus policy when a process reaches the adapter, and a refusal
/// surfaces as a failure to obtain the adapter rather than as a state.
///
/// Reporting `granted` here would be a derivation rather than a measurement, so
/// the value is absent. Absence means "this platform exposes no such state", it
/// is never a denial, and readiness must not gate on it.
#[cfg(target_os = "linux")]
fn platform_authorization() -> AuthorizationReport {
    AuthorizationReport {
        value: AUTHORIZATION_UNKNOWN,
        reason: Some(
            "BlueZ exposes no per-application Bluetooth authorization state, so adapter authorization is reported absent; on this platform a refusal surfaces as a failure to obtain the adapter rather than as an authorization value.",
        ),
    }
}

/// Reports Windows authorization as absent.
///
/// Windows does have an authorization concept for radio access, and this host
/// does not query it, so no value is claimed for it.
#[cfg(target_os = "windows")]
fn platform_authorization() -> AuthorizationReport {
    AuthorizationReport {
        value: AUTHORIZATION_UNKNOWN,
        reason: Some(
            "Windows decides Bluetooth radio access through settings this host does not query, so adapter authorization is reported absent.",
        ),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn platform_authorization() -> AuthorizationReport {
    AuthorizationReport {
        value: AUTHORIZATION_UNKNOWN,
        reason: Some(
            "This host does not query a Bluetooth authorization state on this platform, so adapter authorization is reported absent.",
        ),
    }
}

/// Newest wall-clock reading this host has reported, in milliseconds since the
/// Unix epoch. It makes `updatedAt` non-decreasing across snapshots and across
/// threads even when the host clock steps backwards.
static LAST_REPORTED_EPOCH_MILLIS: AtomicI64 = AtomicI64::new(0);

const CLOCK_BEFORE_EPOCH_REASON: &str =
    "The host wall clock reads before the Unix epoch; updatedAt reports the newest timestamp this host has observed instead.";
const CLOCK_MOVED_BACKWARDS_REASON: &str =
    "The host wall clock moved backwards; updatedAt is held at the newest timestamp this host has observed.";

/// A wall-clock reading for one snapshot, with the caveat it carries.
struct SampleClock {
    epoch_millis: i64,
    reason: Option<&'static str>,
}

/// Stamps a snapshot with the current wall clock in milliseconds since the Unix
/// epoch, held non-decreasing.
///
/// The reading is taken at the moment the snapshot is built. The only deviation
/// from the raw clock is a backwards step, which is disclosed in `safeReason`
/// rather than silently smoothed.
fn sample_epoch_millis() -> SampleClock {
    let raw = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX));
    let floor = LAST_REPORTED_EPOCH_MILLIS.fetch_max(raw.unwrap_or(0), Ordering::Relaxed);
    resolve_sample_clock(raw, floor)
}

/// Resolves the reported timestamp from the raw reading and the newest value
/// this host has already reported.
fn resolve_sample_clock(raw: Option<i64>, floor: i64) -> SampleClock {
    match raw {
        None => SampleClock {
            epoch_millis: floor,
            reason: Some(CLOCK_BEFORE_EPOCH_REASON),
        },
        Some(millis) if millis < floor => SampleClock {
            epoch_millis: floor,
            reason: Some(CLOCK_MOVED_BACKWARDS_REASON),
        },
        Some(millis) => SampleClock {
            epoch_millis: millis,
            reason: None,
        },
    }
}

fn adapter_state_payload_live(attachment: &Attachment, power: &str, heard: i64) -> IpcValue {
    object([("state", live_adapter_state(attachment, power, heard))])
}

fn negotiate_ipc_versions(
    remote_offer: &BTreeMap<String, IpcValue>,
) -> Result<IpcValue, DispatchError> {
    if remote_offer.len() != 5
        || ![
            "backendContract",
            "capabilitySchema",
            "eventSchema",
            "traceFormat",
            "ipcProtocol",
        ]
        .iter()
        .all(|key| remote_offer.contains_key(*key))
    {
        return Err(DispatchError::new(
            "protocol.malformed",
            "ipc",
            "tauri.bootstrap-offer-shape",
        ));
    }
    Ok(object([
        (
            "backendContract",
            negotiate_axis(remote_offer, "backend-contract", "backendContract", 1)?,
        ),
        (
            "capabilitySchema",
            negotiate_axis(remote_offer, "capability-schema", "capabilitySchema", 1)?,
        ),
        (
            "eventSchema",
            negotiate_axis(remote_offer, "event-schema", "eventSchema", 1)?,
        ),
        (
            "traceFormat",
            negotiate_axis(remote_offer, "trace-format", "traceFormat", 1)?,
        ),
        (
            "ipcProtocol",
            negotiate_axis(remote_offer, "ipc-protocol", "ipcProtocol", 2)?,
        ),
    ]))
}

fn negotiate_axis(
    remote_offer: &BTreeMap<String, IpcValue>,
    axis: &str,
    key: &str,
    local_value: i64,
) -> Result<IpcValue, DispatchError> {
    let range = into_object(
        required_value(remote_offer, key, "tauri.bootstrap-offer-range")?.clone(),
        "tauri.bootstrap-offer-range",
    )?;
    if range.len() != 3 || required_string(&range, "axis", "tauri.bootstrap-offer-axis")? != axis {
        return Err(DispatchError::new(
            "protocol.malformed",
            "ipc",
            "tauri.bootstrap-offer-range",
        ));
    }
    let minimum = offered_version(&range, "minimum", axis)?;
    let maximum = offered_version(&range, "maximum", axis)?;
    if minimum > maximum {
        return Err(DispatchError::new(
            "protocol.malformed",
            "ipc",
            "tauri.bootstrap-offer-range-order",
        ));
    }
    if local_value < minimum || local_value > maximum {
        return Err(DispatchError::new(
            "protocol.incompatible",
            "ipc",
            format!("tauri.bootstrap-version-{axis}"),
        ));
    }
    let selected = version_number(axis, local_value);
    Ok(object([
        ("axis", string(axis)),
        ("selected", selected.clone()),
        (
            "localRange",
            object([
                ("axis", string(axis)),
                ("minimum", selected.clone()),
                ("maximum", selected),
            ]),
        ),
        (
            "remoteRange",
            object([
                ("axis", string(axis)),
                ("minimum", version_number(axis, minimum)),
                ("maximum", version_number(axis, maximum)),
            ]),
        ),
    ]))
}

fn offered_version(
    range: &BTreeMap<String, IpcValue>,
    key: &str,
    axis: &str,
) -> Result<i64, DispatchError> {
    let version = into_object(
        required_value(range, key, "tauri.bootstrap-offer-version")?.clone(),
        "tauri.bootstrap-offer-version",
    )?;
    if version.len() != 2
        || required_string(&version, "axis", "tauri.bootstrap-offer-version")? != axis
    {
        return Err(DispatchError::new(
            "protocol.malformed",
            "ipc",
            "tauri.bootstrap-offer-version",
        ));
    }
    match version.get("value") {
        Some(IpcValue::Number(value)) => {
            value.as_i64().filter(|value| *value >= 0).ok_or_else(|| {
                DispatchError::new("protocol.malformed", "ipc", "tauri.bootstrap-offer-version")
            })
        }
        _ => Err(DispatchError::new(
            "protocol.malformed",
            "ipc",
            "tauri.bootstrap-offer-version",
        )),
    }
}

fn version_number(axis: &str, value: i64) -> IpcValue {
    object([("axis", string(axis)), ("value", number(value))])
}

fn route_response(payload: IpcValue) -> IpcValue {
    object([("kind", string("route")), ("payload", payload)])
}

fn released() -> IpcValue {
    object([
        ("state", string("released")),
        ("failures", IpcValue::Array(Vec::new())),
    ])
}

fn cleanup_succeeded(value: &IpcValue) -> bool {
    let IpcValue::Object(record) = value else {
        return false;
    };
    matches!(record.get("state"), Some(IpcValue::String(state)) if state == "released")
        && matches!(record.get("failures"), Some(IpcValue::Array(failures)) if failures.is_empty())
}

fn cleanup_record(failures: Vec<IpcValue>) -> IpcValue {
    object([
        (
            "state",
            string(if failures.is_empty() {
                "released"
            } else {
                "release-failed"
            }),
        ),
        ("failures", IpcValue::Array(failures)),
    ])
}

fn cleanup_failure(resource_kind: &str, operation: &str, message: String) -> IpcValue {
    object([
        ("resourceKind", string(resource_kind)),
        (
            "error",
            object([
                ("code", string("platform.failure")),
                ("domain", string("cleanup")),
                ("operation", string(operation)),
                (
                    "platform",
                    object([
                        ("domain", string("btleplug")),
                        ("code", string("cleanup-failed")),
                        ("safeMessage", string(message)),
                        ("metadata", object([])),
                    ]),
                ),
                ("retryability", string("caller-decides")),
            ]),
        ),
    ])
}

fn is_released(value: &IpcValue) -> bool {
    matches!(
        value,
        IpcValue::Object(record)
            if matches!(record.get("state"), Some(IpcValue::String(state)) if state == "released")
    )
}

fn object<const N: usize>(entries: [(&str, IpcValue); N]) -> IpcValue {
    IpcValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn string(value: impl Into<String>) -> IpcValue {
    IpcValue::String(value.into())
}

fn as_string(value: &IpcValue) -> Option<&str> {
    match value {
        IpcValue::String(value) => Some(value),
        _ => None,
    }
}

fn same_attachment_identity(
    left: &BTreeMap<String, IpcValue>,
    right: &BTreeMap<String, IpcValue>,
) -> bool {
    let left_adapter = left.get("adapter").and_then(|value| match value {
        IpcValue::Object(record) => Some(record),
        _ => None,
    });
    let right_adapter = right.get("adapter").and_then(|value| match value {
        IpcValue::Object(record) => Some(record),
        _ => None,
    });
    ["attachmentId", "backendInstanceId", "backendGeneration"]
        .iter()
        .all(|key| string_field_equal(left, right, key))
        && left_adapter.is_some_and(|left_adapter| {
            right_adapter.is_some_and(|right_adapter| {
                string_field_equal(left_adapter, right_adapter, "adapterId")
                    && string_field_equal(left_adapter, right_adapter, "adapterGeneration")
            })
        })
}

fn string_field_equal(
    left: &BTreeMap<String, IpcValue>,
    right: &BTreeMap<String, IpcValue>,
    key: &str,
) -> bool {
    left.get(key).and_then(as_string) == right.get(key).and_then(as_string)
}

fn validate_connection_identity(
    payload: &BTreeMap<String, IpcValue>,
    connection: &ConnectionResource,
    owner_lease_id: &str,
    operation: &str,
) -> Result<(), DispatchError> {
    let peer_id = required_string(payload, "peerId", operation)?;
    let connection_id = required_string(payload, "connectionId", operation)?;
    let requested_owner_lease_id = required_string(payload, "ownerLeaseId", operation)?;
    let connection_generation = required_string(payload, "connectionGeneration", operation)?;
    let matches = peer_id == connection.peer_id
        && connection_id == connection.connection_id
        && requested_owner_lease_id == owner_lease_id
        && requested_owner_lease_id == connection.owner_lease_id
        && connection_generation == connection.connection_generation;
    if matches {
        Ok(())
    } else {
        Err(DispatchError::new(
            "protocol.violation",
            "connection",
            operation,
        ))
    }
}

fn validate_database_identity(
    payload: &BTreeMap<String, IpcValue>,
    database: &DatabaseResource,
    operation: &str,
) -> Result<(), DispatchError> {
    let matches = required_string(payload, "databaseId", operation)? == database.database_id
        && required_string(payload, "databaseGeneration", operation)?
            == database.database_generation
        && required_string(payload, "connectionHandle", operation)? == database.connection_handle;
    if matches {
        Ok(())
    } else {
        Err(DispatchError::new("protocol.violation", "gatt", operation))
    }
}

fn number(value: i64) -> IpcValue {
    IpcValue::Number(Number::from(value))
}

fn into_object(
    value: IpcValue,
    operation: impl Into<String>,
) -> Result<BTreeMap<String, IpcValue>, DispatchError> {
    match value {
        IpcValue::Object(object) => Ok(object),
        _ => Err(DispatchError::new("protocol.malformed", "ipc", operation)),
    }
}

fn required_value<'a>(
    object: &'a BTreeMap<String, IpcValue>,
    key: &str,
    operation: impl Into<String>,
) -> Result<&'a IpcValue, DispatchError> {
    object
        .get(key)
        .ok_or_else(|| DispatchError::new("protocol.malformed", "ipc", operation))
}

fn required_string(
    object: &BTreeMap<String, IpcValue>,
    key: &str,
    operation: impl Into<String>,
) -> Result<String, DispatchError> {
    match object.get(key) {
        Some(IpcValue::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(DispatchError::new("protocol.malformed", "ipc", operation)),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::error_confirms_device_released;
    use super::{
        characteristic_properties, disconnect_with_state_check, negotiate_ipc_versions, object,
        released, resolve_disconnect_failure, scan_properties_match_optional,
        should_clear_peer_owner, string, BtleplugDispatcher, BtleplugDispatcherOptions,
        CallerState, IpcEventSink, QuarantineScheduler, DISCONNECT_COMPLETION_TIMEOUT,
    };
    use btleplug::api::CharPropFlags;
    use std::collections::{HashMap, HashSet};
    use std::{cell::Cell, cell::RefCell, rc::Rc};

    #[test]
    fn capability_projection_is_data_only() {
        assert_eq!(
            characteristic_properties(CharPropFlags::READ | CharPropFlags::NOTIFY),
            super::IpcValue::Array(vec![string("read"), string("notify")])
        );
    }

    #[tokio::test]
    async fn disconnect_failure_checks_state_once_and_releases_an_already_disconnected_peer() {
        let state_checks = Cell::new(0);
        assert_eq!(
            disconnect_with_state_check(
                async { Err("already disconnected".to_owned()) },
                || async {
                    state_checks.set(state_checks.get() + 1);
                    Ok(false)
                }
            )
            .await,
            Ok(())
        );
        assert_eq!(state_checks.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn disconnect_timeout_checks_state_and_releases_an_already_disconnected_peer() {
        let state_checks = Cell::new(0);
        let events = Rc::new(RefCell::new(Vec::new()));
        let disconnect_events = Rc::clone(&events);
        let state_events = Rc::clone(&events);
        assert_eq!(
            disconnect_with_state_check(
                async move {
                    disconnect_events.borrow_mut().push("disconnect-started");
                    std::future::pending().await
                },
                || async {
                    state_events.borrow_mut().push("state-checked");
                    state_checks.set(state_checks.get() + 1);
                    Ok(false)
                }
            )
            .await,
            Ok(())
        );
        assert_eq!(state_checks.get(), 1);
        assert_eq!(
            events.borrow().as_slice(),
            ["disconnect-started", "state-checked"]
        );
    }

    /// A caller with no live connections, as it looks after its handle has
    /// already been torn down.
    fn caller_state_with_no_connections() -> CallerState {
        CallerState {
            lease_id: "lease-1".to_owned(),
            lease_generation: "generation-1".to_owned(),
            versions: object([]),
            event_sink: IpcEventSink::new(tauri::ipc::Channel::new(|_| Ok(()))),
            scan_admitting: false,
            scan: None,
            connections: HashMap::new(),
            databases: HashMap::new(),
            subscriptions: HashMap::new(),
            connection_events: HashMap::new(),
            operations: HashMap::new(),
            completed_correlations: HashMap::new(),
            pending_events: HashSet::new(),
            quarantine: QuarantineScheduler::new(),
        }
    }

    /// Exercises the guard through the dispatcher rather than through the pure
    /// predicate, which is the part a revert would actually break.
    ///
    /// The scenario is the one the fix exists for: a stale cleanup for handle
    /// `h1` arrives after the peer has been reconnected as `h2`. Because `h1`
    /// is already gone, `connections.remove` yields `None`, so the removed peer
    /// is `None` and ownership must be left alone. Passing the *requested* peer
    /// where the *removed* peer belongs - the original bug - makes the guard
    /// true here and clears an owner that a newer connection still holds, so
    /// this test fails if those arguments are ever swapped back.
    #[tokio::test]
    async fn a_stale_cleanup_does_not_clear_a_reconnected_peers_owner() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        {
            let mut state = dispatcher.inner.lock().await;
            state
                .callers
                .insert("caller-a".to_owned(), caller_state_with_no_connections());
            state
                .peer_owners
                .insert("peer-1".to_owned(), "caller-a".to_owned());
        }

        let (subscriptions, event_tasks) = dispatcher
            .remove_connection_resources("caller-a", "h1", "peer-1")
            .await;

        assert!(subscriptions.is_empty());
        assert!(event_tasks.is_empty());
        let state = dispatcher.inner.lock().await;
        assert_eq!(
            state.peer_owners.get("peer-1").map(String::as_str),
            Some("caller-a"),
            "a stale handle's cleanup must not release a peer a newer connection owns"
        );
    }

    /// A compensating disconnect that cannot confirm the link is down must keep
    /// the reservation. Dropping it leaves a connected peripheral with no owner
    /// and no handle - nothing can reach or retry it until the process
    /// restarts, and the caller is told only that admission was denied.
    ///
    /// The original code removed the reservation BEFORE attempting the
    /// disconnect and discarded its result, so this fails on a revert.
    #[tokio::test]
    async fn an_unconfirmed_compensating_disconnect_keeps_the_reservation() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        {
            let mut state = dispatcher.inner.lock().await;
            state
                .peer_owners
                .insert("peer-1".to_owned(), "caller-a".to_owned());
        }

        let residue = dispatcher
            .apply_compensation_outcome(Err("still connected".to_owned()), "peer-1", true)
            .await;

        assert_eq!(residue.as_deref(), Some("still connected"));
        let state = dispatcher.inner.lock().await;
        assert_eq!(
            state.peer_owners.get("peer-1").map(String::as_str),
            Some("caller-a"),
            "a peer that may still be connected must keep its reservation so a retry can reclaim it"
        );
    }

    /// The other half: a disconnect the platform confirms DOES surrender the
    /// reservation, so retaining it is evidence-driven rather than a blanket
    /// refusal to clean up.
    #[tokio::test]
    async fn a_confirmed_compensating_disconnect_releases_the_reservation() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        {
            let mut state = dispatcher.inner.lock().await;
            state
                .peer_owners
                .insert("peer-1".to_owned(), "caller-a".to_owned());
        }

        let residue = dispatcher
            .apply_compensation_outcome(Ok(()), "peer-1", true)
            .await;

        assert!(residue.is_none());
        let state = dispatcher.inner.lock().await;
        assert!(
            !state.peer_owners.contains_key("peer-1"),
            "a confirmed release must not leave a stale reservation behind"
        );
    }

    /// A reservation held by someone else is never touched, confirmed release
    /// or not - the same guard `remove_connection_resources` already applies.
    #[tokio::test]
    async fn compensation_never_clears_another_callers_reservation() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        {
            let mut state = dispatcher.inner.lock().await;
            state
                .peer_owners
                .insert("peer-1".to_owned(), "caller-b".to_owned());
        }

        dispatcher
            .apply_compensation_outcome(Ok(()), "peer-1", false)
            .await;

        let state = dispatcher.inner.lock().await;
        assert_eq!(
            state.peer_owners.get("peer-1").map(String::as_str),
            Some("caller-b"),
            "compensation must not release a peer another caller owns"
        );
    }

    fn orphan_owner(peer_id: &str, generation: &str) -> super::OrphanConnectionOwner {
        super::OrphanConnectionOwner {
            caller_key: "caller-a".to_owned(),
            lease_id: "lease-1".to_owned(),
            lease_generation: generation.to_owned(),
            peer_id: peer_id.to_owned(),
            peripheral: None,
            attempts: 0,
        }
    }

    /// Failed compensating disconnect must keep both the reservation and an
    /// inspectable cleanup owner. The original path retained `peer_owners`
    /// while dropping the Peripheral, so later release could not retry.
    #[tokio::test]
    async fn failed_compensation_keeps_an_inspectable_orphan_owner() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        {
            let mut state = dispatcher.inner.lock().await;
            state
                .peer_owners
                .insert("peer-1".to_owned(), "caller-a".to_owned());
            super::retain_orphan_connection(&mut state, orphan_owner("peer-1", "generation-1"));
        }

        let residue = dispatcher
            .settle_orphan_connection(Err("still connected".to_owned()), "peer-1", "generation-1")
            .await;

        assert_eq!(residue.as_deref(), Some("still connected"));
        let state = dispatcher.inner.lock().await;
        assert_eq!(
            state.peer_owners.get("peer-1").map(String::as_str),
            Some("caller-a"),
            "a peer that may still be connected must keep its reservation"
        );
        let orphan = state
            .orphan_connections
            .get("peer-1")
            .expect("failed compensation must keep an inspectable pending cleanup owner");
        assert_eq!(orphan.lease_id, "lease-1");
        assert_eq!(orphan.lease_generation, "generation-1");
        assert!(
            orphan.attempts >= 1,
            "failed compensation must record retry state"
        );
    }

    #[tokio::test]
    async fn a_later_cleanup_retry_releases_the_orphan_and_reservation() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        {
            let mut state = dispatcher.inner.lock().await;
            state
                .peer_owners
                .insert("peer-1".to_owned(), "caller-a".to_owned());
            super::retain_orphan_connection(&mut state, orphan_owner("peer-1", "generation-1"));
        }

        let residue = dispatcher
            .settle_orphan_connection(Ok(()), "peer-1", "generation-1")
            .await;

        assert!(residue.is_none());
        let state = dispatcher.inner.lock().await;
        assert!(
            !state.peer_owners.contains_key("peer-1"),
            "proven release must drop the reservation"
        );
        assert!(
            !state.orphan_connections.contains_key("peer-1"),
            "proven release must drop the orphan owner together with the reservation"
        );
    }

    #[tokio::test]
    async fn old_orphan_cleanup_does_not_release_a_newer_generations_reservation() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        {
            let mut state = dispatcher.inner.lock().await;
            state
                .peer_owners
                .insert("peer-1".to_owned(), "caller-a".to_owned());
            super::retain_orphan_connection(&mut state, orphan_owner("peer-1", "generation-2"));
        }

        dispatcher
            .settle_orphan_connection(Ok(()), "peer-1", "generation-1")
            .await;

        let state = dispatcher.inner.lock().await;
        assert_eq!(
            state.peer_owners.get("peer-1").map(String::as_str),
            Some("caller-a"),
            "old cleanup must not release a newer generation's reservation"
        );
        let orphan = state
            .orphan_connections
            .get("peer-1")
            .expect("newer generation orphan must remain");
        assert_eq!(orphan.lease_generation, "generation-2");
    }

    #[tokio::test]
    async fn release_without_a_caller_still_observes_orphan_connections() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        {
            let mut state = dispatcher.inner.lock().await;
            state
                .peer_owners
                .insert("peer-1".to_owned(), "caller-a".to_owned());
            super::retain_orphan_connection(&mut state, orphan_owner("peer-1", "generation-1"));
        }

        let cleanup = dispatcher.release("caller-a").await;
        let state = dispatcher.inner.lock().await;
        assert!(
            state.orphan_connections.contains_key("peer-1"),
            "release must keep an orphan whose physical release is unproven"
        );
        assert_eq!(
            state.peer_owners.get("peer-1").map(String::as_str),
            Some("caller-a")
        );
        assert!(
            !super::is_released(&cleanup),
            "unproven orphan cleanup cannot report a clean release that forgets the resource"
        );
    }

    fn caller_state_with_scan(handle: &str) -> CallerState {
        let mut caller = caller_state_with_no_connections();
        caller.scan = Some(super::ScanResource {
            handle: handle.to_owned(),
            task: tokio::spawn(std::future::pending()),
        });
        caller
    }

    /// fail_scan_stream used to take the scan, clear scan_owner, then
    /// `stop_scan().await.ok()`. The owner must stay until native stop
    /// settles so a second scan cannot cover an outstanding stop.
    #[tokio::test]
    async fn fail_scan_keeps_owner_until_native_stop_completes() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        let mut state = dispatcher.inner.lock().await;
        state
            .callers
            .insert("caller-a".to_owned(), caller_state_with_scan("scan-1"));
        state.scan_owner = Some("caller-a".to_owned());

        super::begin_scan_stop(
            &mut state,
            "caller-a",
            ("lease-1", "generation-1"),
            "scan-1",
            true,
        )
        .expect("owned scan must be claimable for stop");

        assert_eq!(
            state.scan_owner.as_deref(),
            Some("caller-a"),
            "must not clear scan_owner before native stop completes"
        );
        assert!(
            super::scan_start_blocked(&state),
            "a second scan must not be admitted over an outstanding stop"
        );
        assert!(
            state.stopping_scan.is_some(),
            "the exact scan owner must remain in a stopping state"
        );
    }

    /// fail_scan_stream runs on the stored pump JoinHandle. Aborting that
    /// handle inside begin_scan_stop cancels adapter()/stop_scan() at the next
    /// await, so apply_scan_stop_outcome never runs and the radio stays up.
    #[tokio::test]
    async fn fail_scan_stream_does_not_abort_the_pump_before_native_stop() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        let (armed_tx, armed_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let dispatcher_task = dispatcher.clone();
        let task = tokio::spawn(async move {
            let _ = armed_rx.await;
            dispatcher_task
                .fail_scan_stream("caller-a", ("lease-1", "generation-1"), "scan-1")
                .await;
            tokio::task::yield_now().await;
            let _ = done_tx.send(());
        });
        {
            let mut state = dispatcher.inner.lock().await;
            let mut caller = caller_state_with_no_connections();
            caller.scan = Some(super::ScanResource {
                handle: "scan-1".to_owned(),
                task,
            });
            state.callers.insert("caller-a".to_owned(), caller);
            state.scan_owner = Some("caller-a".to_owned());
        }
        armed_tx.send(()).expect("pump must be waiting to fail");
        done_rx
            .await
            .expect("fail_scan_stream must finish native stop on the pump instead of aborting it");
        let state = dispatcher.inner.lock().await;
        assert!(
            super::scan_start_blocked(&state),
            "adapter-unavailable stop must keep the scan blocked for retry"
        );
    }

    #[tokio::test]
    async fn fail_subscription_stream_does_not_abort_the_pump_before_unsubscribe() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        let (armed_tx, armed_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let dispatcher_task = dispatcher.clone();
        let task = tauri::async_runtime::spawn(async move {
            let _ = armed_rx.await;
            dispatcher_task
                .fail_subscription_stream("caller-a", ("lease-1", "generation-1"), "sub-1")
                .await;
            tokio::task::yield_now().await;
            let _ = done_tx.send(());
        });
        {
            let mut state = dispatcher.inner.lock().await;
            let mut caller = caller_state_with_no_connections();
            caller.subscriptions.insert(
                "sub-1".to_owned(),
                super::SubscriptionResource {
                    database_handle: "db-stand-in".to_owned(),
                    body: super::SubscriptionBody::StandIn,
                    task,
                },
            );
            state.callers.insert("caller-a".to_owned(), caller);
        }
        armed_tx.send(()).expect("pump must be waiting to fail");
        done_rx.await.expect(
            "fail_subscription_stream must finish unsubscribe on the pump instead of aborting it",
        );
    }

    #[tokio::test]
    async fn release_during_scan_admission_keeps_the_scan_blocked_until_stop() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        {
            let mut state = dispatcher.inner.lock().await;
            let mut caller = caller_state_with_no_connections();
            caller.scan_admitting = true;
            state.callers.insert("caller-a".to_owned(), caller);
            state.scan_owner = Some("caller-a".to_owned());
        }

        let _cleanup = dispatcher.release("caller-a").await;
        let state = dispatcher.inner.lock().await;
        assert!(
            super::scan_start_blocked(&state),
            "release while a scan is still admitting must keep a stopping owner, not clear scan_owner"
        );
    }

    #[tokio::test]
    async fn failed_scan_stop_remains_retryable() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        let mut state = dispatcher.inner.lock().await;
        state
            .callers
            .insert("caller-a".to_owned(), caller_state_with_scan("scan-1"));
        state.scan_owner = Some("caller-a".to_owned());
        super::begin_scan_stop(
            &mut state,
            "caller-a",
            ("lease-1", "generation-1"),
            "scan-1",
            true,
        )
        .expect("owned scan must be claimable for stop");

        let error = super::apply_scan_stop_outcome(&mut state, Err("adapter busy".to_owned()))
            .expect_err("failed stop must be retained, not discarded");
        assert_eq!(error, "adapter busy");
        assert_eq!(state.scan_owner.as_deref(), Some("caller-a"));
        assert!(
            state.stopping_scan.is_some(),
            "failed stop must keep the original resource available for retry"
        );
    }

    #[tokio::test]
    async fn successful_scan_stop_releases_owner() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        let mut state = dispatcher.inner.lock().await;
        state
            .callers
            .insert("caller-a".to_owned(), caller_state_with_scan("scan-1"));
        state.scan_owner = Some("caller-a".to_owned());
        super::begin_scan_stop(
            &mut state,
            "caller-a",
            ("lease-1", "generation-1"),
            "scan-1",
            true,
        )
        .expect("owned scan must be claimable for stop");

        super::apply_scan_stop_outcome(&mut state, Ok(()))
            .expect("confirmed stop must release the stopping owner");
        assert!(state.scan_owner.is_none());
        assert!(state.stopping_scan.is_none());
        assert!(!super::scan_start_blocked(&state));
    }

    fn stand_in_subscription() -> super::SubscriptionResource {
        super::SubscriptionResource {
            database_handle: "db-stand-in".to_owned(),
            body: super::SubscriptionBody::StandIn,
            task: tauri::async_runtime::spawn(async {}),
        }
    }

    fn subscription_orphan(stream_id: &str, generation: &str) -> super::OrphanSubscriptionOwner {
        super::OrphanSubscriptionOwner {
            caller_key: "caller-a".to_owned(),
            lease_id: "lease-1".to_owned(),
            lease_generation: generation.to_owned(),
            stream_id: stream_id.to_owned(),
            attempts: 0,
        }
    }

    /// After native subscribe succeeds, a gone/replaced caller must keep an
    /// inspectable orphan so a failed compensating unsubscribe can retry.
    /// The original path discarded that outcome with `.await.ok()`.
    #[tokio::test]
    async fn failed_subscribe_compensation_keeps_an_inspectable_orphan_subscription() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        {
            let mut state = dispatcher.inner.lock().await;
            super::retain_orphan_subscription(
                &mut state,
                subscription_orphan("sub-1", "generation-1"),
                Some(stand_in_subscription()),
            );
        }

        let residue = dispatcher
            .settle_orphan_subscription(
                Err("cccd still enabled".to_owned()),
                "sub-1",
                "generation-1",
            )
            .await;

        assert_eq!(residue.as_deref(), Some("cccd still enabled"));
        let state = dispatcher.inner.lock().await;
        let orphan = state
            .orphan_subscription_owners
            .get("sub-1")
            .expect("failed compensating unsubscribe must keep an inspectable orphan owner");
        assert_eq!(orphan.lease_generation, "generation-1");
        assert!(
            orphan.attempts >= 1,
            "failed compensating unsubscribe must record retry state"
        );
        assert!(
            state.orphan_subscription_resources.contains_key("sub-1"),
            "the native subscription resource must remain with the orphan for retry"
        );
        assert!(
            state
                .callers
                .get("caller-a")
                .map_or(true, |caller| caller.subscriptions.is_empty()),
            "compensation must not insert the old subscription into a replacement caller"
        );
    }

    #[tokio::test]
    async fn successful_subscribe_compensation_releases_the_matching_orphan_subscription() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        {
            let mut state = dispatcher.inner.lock().await;
            super::retain_orphan_subscription(
                &mut state,
                subscription_orphan("sub-1", "generation-1"),
                Some(stand_in_subscription()),
            );
        }

        let residue = dispatcher
            .settle_orphan_subscription(Ok(()), "sub-1", "generation-1")
            .await;

        assert!(residue.is_none());
        let state = dispatcher.inner.lock().await;
        assert!(
            !state.orphan_subscription_owners.contains_key("sub-1"),
            "proven unsubscribe must drop the orphan owner"
        );
        assert!(
            !state.orphan_subscription_resources.contains_key("sub-1"),
            "proven unsubscribe must drop the orphan resource"
        );
    }

    #[tokio::test]
    async fn old_subscribe_compensation_does_not_drop_a_newer_generation_orphan() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        {
            let mut state = dispatcher.inner.lock().await;
            super::retain_orphan_subscription(
                &mut state,
                subscription_orphan("sub-1", "generation-2"),
                Some(stand_in_subscription()),
            );
        }

        dispatcher
            .settle_orphan_subscription(Ok(()), "sub-1", "generation-1")
            .await;

        let state = dispatcher.inner.lock().await;
        let orphan = state
            .orphan_subscription_owners
            .get("sub-1")
            .expect("newer generation orphan must remain");
        assert_eq!(orphan.lease_generation, "generation-2");
        assert!(
            state.orphan_subscription_resources.contains_key("sub-1"),
            "old cleanup must not drop a newer generation's subscription resource"
        );
    }

    /// After unsubscribe fails, the old subscription must not land in a
    /// replacement lease. The original fail_subscription_stream reinserted
    /// into `callers[key]` with no generation check after the await.
    #[tokio::test]
    async fn failed_unsubscribe_does_not_reinsert_into_a_replacement_lease() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        let mut state = dispatcher.inner.lock().await;
        let mut replacement = caller_state_with_no_connections();
        replacement.lease_id = "lease-2".to_owned();
        replacement.lease_generation = "generation-2".to_owned();
        state.callers.insert("caller-a".to_owned(), replacement);

        super::retain_failed_subscription(
            &mut state,
            "caller-a",
            ("lease-1", "generation-1"),
            "sub-1",
            Some(stand_in_subscription()),
        );

        let caller = state
            .callers
            .get("caller-a")
            .expect("replacement caller remains");
        assert!(
            caller.subscriptions.is_empty(),
            "must not insert an old-generation subscription into the replacement caller"
        );
        assert_eq!(caller.lease_id, "lease-2");
        let orphan = state
            .orphan_subscription_owners
            .get("sub-1")
            .expect("old resource must keep a retry owner on the old generation");
        assert_eq!(orphan.lease_id, "lease-1");
        assert_eq!(orphan.lease_generation, "generation-1");
        assert!(
            state.orphan_subscription_resources.contains_key("sub-1"),
            "the stand-in resource must stay on the old generation, not the replacement caller"
        );
    }

    #[tokio::test]
    async fn failed_unsubscribe_reinserts_only_when_the_same_lease_still_owns_the_caller() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        let mut state = dispatcher.inner.lock().await;
        state
            .callers
            .insert("caller-a".to_owned(), caller_state_with_no_connections());

        super::retain_failed_subscription(
            &mut state,
            "caller-a",
            ("lease-1", "generation-1"),
            "sub-1",
            Some(stand_in_subscription()),
        );

        assert!(
            !state.orphan_subscription_owners.contains_key("sub-1"),
            "a still-current lease must not quarantine its own live subscription"
        );
        assert!(
            state
                .callers
                .get("caller-a")
                .is_some_and(|caller| caller.subscriptions.contains_key("sub-1")),
            "the still-current lease must keep the live subscription resource"
        );
    }

    #[tokio::test]
    async fn stop_scan_after_await_does_not_steal_a_replacement_callers_scan() {
        let dispatcher = BtleplugDispatcher::new(BtleplugDispatcherOptions { adapter_id: None });
        let mut state = dispatcher.inner.lock().await;
        state
            .callers
            .insert("caller-a".to_owned(), caller_state_with_scan("scan-1"));
        state.scan_owner = Some("caller-a".to_owned());
        super::begin_scan_stop(
            &mut state,
            "caller-a",
            ("lease-1", "generation-1"),
            "scan-1",
            true,
        )
        .expect("owned scan must be claimable for stop");

        let mut replacement = caller_state_with_scan("scan-2");
        replacement.lease_id = "lease-2".to_owned();
        replacement.lease_generation = "generation-2".to_owned();
        state.callers.insert("caller-a".to_owned(), replacement);

        super::apply_scan_stop_outcome(&mut state, Ok(()))
            .expect("old stop success must not fail closed on a replaced lease");
        let caller = state.callers.get("caller-a").expect("replacement remains");
        assert_eq!(
            caller.scan.as_ref().map(|scan| scan.handle.as_str()),
            Some("scan-2"),
            "successful old stop must not take a replacement caller's scan"
        );
    }

    /// Builds the error btleplug actually delivers when BlueZ has dropped a
    /// device object, by the same conversions the real path uses.
    ///
    /// This doubles as a version tripwire. It constructs the value through the
    /// *direct* bluez-async dependency and hands it to btleplug's `From`, so if
    /// that copy and btleplug's transitive copy ever diverge across a semver
    /// bump, this stops compiling - instead of the downcast silently returning
    /// None and the classification quietly reverting in production.
    #[cfg(target_os = "linux")]
    fn bluez_dbus_error(name: &str) -> btleplug::Error {
        let dbus_error = dbus::Error::new_custom(name, "device object is gone");
        btleplug::Error::from(bluez_async::BluetoothError::DbusError(dbus_error))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_removed_bluez_device_object_confirms_the_peer_is_released() {
        assert!(error_confirms_device_released(&bluez_dbus_error(
            "org.freedesktop.DBus.Error.UnknownObject"
        )));
        assert!(error_confirms_device_released(&bluez_dbus_error(
            "org.bluez.Error.DoesNotExist"
        )));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_transport_failure_is_not_evidence_that_the_peer_was_released() {
        // The distinction that matters: these mean "we could not tell", so
        // ownership must be retained rather than released.
        for name in [
            "org.freedesktop.DBus.Error.Timeout",
            "org.freedesktop.DBus.Error.NoReply",
            "org.freedesktop.DBus.Error.ServiceUnknown",
        ] {
            assert!(
                !error_confirms_device_released(&bluez_dbus_error(name)),
                "{name} does not prove the peer is gone"
            );
        }
        assert!(!error_confirms_device_released(
            &btleplug::Error::NotConnected
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn disconnect_timeout_is_preserved_when_the_peer_remains_connected() {
        assert_eq!(
            disconnect_with_state_check(std::future::pending(), || async { Ok(true) }).await,
            Err(format!(
                "disconnect completion timed out after {} ms",
                DISCONNECT_COMPLETION_TIMEOUT.as_millis()
            ))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn disconnect_and_state_timeouts_are_both_reported() {
        assert_eq!(
            disconnect_with_state_check(
                std::future::pending(),
                std::future::pending::<Result<bool, String>>,
            )
            .await,
            Err(format!(
                "disconnect completion timed out after {ms} ms; post-disconnect state check \
                 failed: post-disconnect state check timed out after {ms} ms",
                ms = DISCONNECT_COMPLETION_TIMEOUT.as_millis()
            ))
        );
    }

    #[tokio::test]
    async fn successful_disconnect_does_not_query_connection_state() {
        let state_checks = Cell::new(0);
        assert_eq!(
            disconnect_with_state_check(async { Ok(()) }, || async {
                state_checks.set(state_checks.get() + 1);
                Ok(true)
            })
            .await,
            Ok(())
        );
        assert_eq!(state_checks.get(), 0);
    }

    #[test]
    fn disconnect_failure_is_preserved_when_the_peripheral_remains_connected() {
        assert_eq!(
            resolve_disconnect_failure("transport failed".to_owned(), Ok(true)),
            Err("transport failed".to_owned())
        );
    }

    #[test]
    fn disconnect_failure_includes_an_indeterminate_post_failure_state() {
        assert_eq!(
            resolve_disconnect_failure(
                "transport failed".to_owned(),
                Err("state unavailable".to_owned())
            ),
            Err(
                "transport failed; post-disconnect state check failed: state unavailable"
                    .to_owned()
            )
        );
    }

    #[test]
    fn stale_connection_cleanup_does_not_clear_a_new_peer_owner() {
        assert!(!should_clear_peer_owner(
            None,
            "peer-1",
            Some("caller-1"),
            "caller-1"
        ));
        assert!(!should_clear_peer_owner(
            Some("peer-1"),
            "peer-1",
            Some("caller-2"),
            "caller-1"
        ));
        assert!(should_clear_peer_owner(
            Some("peer-1"),
            "peer-1",
            Some("caller-1"),
            "caller-1"
        ));
    }

    #[test]
    fn scan_filtering_fails_closed_when_properties_are_unavailable() {
        assert!(!scan_properties_match_optional(None, &[], None, &[]));
    }

    #[test]
    fn adapter_state_route_payload_nests_state_object() {
        let attachment = super::Attachment {
            attachment_id: "a".into(),
            backend_instance_id: "b".into(),
            backend_generation: "1".into(),
            adapter_id: "adapter".into(),
            adapter_name: "CoreBluetooth".into(),
            adapter_generation: "1".into(),
        };
        let payload = super::adapter_state_payload_live(&attachment, "on", 3);
        let super::IpcValue::Object(record) = payload else {
            panic!("adapter.state payload must be an object");
        };
        let Some(super::IpcValue::Object(state)) = record.get("state") else {
            panic!("adapter.state payload must nest a state object");
        };
        assert!(
            matches!(state.get("power"), Some(super::IpcValue::String(value)) if value == "on")
        );
        assert!(
            matches!(state.get("heard"), Some(super::IpcValue::Number(value) ) if value.as_i64() == Some(3))
        );
    }

    fn test_attachment() -> super::Attachment {
        super::Attachment {
            attachment_id: "a".into(),
            backend_instance_id: "b".into(),
            backend_generation: "1".into(),
            adapter_id: "adapter".into(),
            adapter_name: "CoreBluetooth".into(),
            adapter_generation: "1".into(),
        }
    }

    fn state_object(
        snapshot: super::IpcValue,
    ) -> std::collections::BTreeMap<String, super::IpcValue> {
        let super::IpcValue::Object(state) = snapshot else {
            panic!("an adapter state snapshot must be an object");
        };
        state
    }

    /// Every token this host may put on the wire is one the TypeScript
    /// `AdapterAuthorization` union already accepts.
    const WIRE_AUTHORIZATION_TOKENS: [&str; 6] = [
        "granted",
        "denied",
        "restricted",
        "not-determined",
        "unavailable",
        super::AUTHORIZATION_UNKNOWN,
    ];

    #[test]
    fn core_bluetooth_authorization_maps_to_the_wire_vocabulary() {
        for (raw, expected) in [
            (0isize, "not-determined"),
            (1, "restricted"),
            (2, "denied"),
            (3, "granted"),
        ] {
            let report = super::map_core_bluetooth_authorization(raw);
            assert_eq!(report.value, expected, "raw {raw} must map to {expected}");
            assert_eq!(
                report.reason, None,
                "a real CoreBluetooth reading carries no caveat"
            );
            assert!(WIRE_AUTHORIZATION_TOKENS.contains(&expected));
        }
    }

    #[test]
    fn unrecognized_core_bluetooth_authorization_is_reported_unknown() {
        for raw in [-1isize, 4, 99] {
            let report = super::map_core_bluetooth_authorization(raw);
            assert_eq!(
                report.value,
                super::AUTHORIZATION_UNKNOWN,
                "raw {raw} has no known meaning and must not be forced into a decision"
            );
            assert!(report.reason.is_some(), "an unknown value must say why");
        }
    }

    #[test]
    fn platform_authorization_is_always_a_wire_token() {
        let report = super::platform_authorization();
        assert!(
            WIRE_AUTHORIZATION_TOKENS.contains(&report.value),
            "{} is not part of the adapter authorization vocabulary",
            report.value
        );
        if report.value == super::AUTHORIZATION_UNKNOWN {
            assert!(
                report.reason.is_some(),
                "an unknown authorization must carry the reason this host has no reading"
            );
        }
    }

    // Spec change: this arm previously derived `granted` from the fact that
    // D-Bus handed this process an adapter. That is an inference, not a
    // measurement, and it made Linux the one platform reporting a value it had
    // never queried. It now reports `unknown`, with the reason disclosed.
    #[cfg(target_os = "linux")]
    #[test]
    fn bluez_authorization_is_unknown_because_the_platform_exposes_no_such_state() {
        let report = super::platform_authorization();
        assert_eq!(report.value, super::AUTHORIZATION_UNKNOWN);
        assert!(report
            .reason
            .is_some_and(|reason| reason.contains("BlueZ exposes no per-application")));
    }

    #[test]
    fn wall_clock_readings_report_the_raw_value_when_it_advances() {
        let resolved = super::resolve_sample_clock(Some(1_800_000_000_123), 1_800_000_000_000);
        assert_eq!(resolved.epoch_millis, 1_800_000_000_123);
        assert_eq!(resolved.reason, None);
    }

    #[test]
    fn wall_clock_regressions_hold_the_last_stamp_and_disclose_it() {
        let backwards = super::resolve_sample_clock(Some(1_700_000_000_000), 1_800_000_000_000);
        assert_eq!(backwards.epoch_millis, 1_800_000_000_000);
        assert_eq!(backwards.reason, Some(super::CLOCK_MOVED_BACKWARDS_REASON));

        let before_epoch = super::resolve_sample_clock(None, 1_800_000_000_000);
        assert_eq!(before_epoch.epoch_millis, 1_800_000_000_000);
        assert_eq!(before_epoch.reason, Some(super::CLOCK_BEFORE_EPOCH_REASON));
    }

    #[test]
    fn sampled_timestamps_are_real_and_non_decreasing() {
        let first = super::sample_epoch_millis();
        let second = super::sample_epoch_millis();
        assert!(
            first.epoch_millis > 1_700_000_000_000,
            "updatedAt must be a wall-clock epoch reading, got {}",
            first.epoch_millis
        );
        assert!(second.epoch_millis >= first.epoch_millis);
    }

    #[test]
    fn live_adapter_state_reports_only_what_it_observed() {
        let state = state_object(super::live_adapter_state(&test_attachment(), "on", 3));

        assert_eq!(state.get("availability"), Some(&string("available")));
        assert_eq!(state.get("power"), Some(&string("on")));
        assert_eq!(state.get("heard"), Some(&super::number(3)));

        let Some(super::IpcValue::Number(updated_at)) = state.get("updatedAt") else {
            panic!("updatedAt must be a number");
        };
        assert!(
            updated_at
                .as_i64()
                .is_some_and(|value| value > 1_700_000_000_000),
            "updatedAt must be stamped when the state is sampled"
        );

        match state.get("authorization") {
            Some(super::IpcValue::String(value)) => {
                assert!(WIRE_AUTHORIZATION_TOKENS.contains(&value.as_str()))
            }
            Some(super::IpcValue::Null) => {}
            other => panic!("authorization must be a wire token or null, got {other:?}"),
        }
    }

    #[test]
    fn unsampled_adapter_state_admits_that_it_sampled_nothing() {
        let state = state_object(super::adapter_state(&test_attachment()));

        assert_eq!(state.get("availability"), Some(&string("unknown")));
        assert_eq!(state.get("power"), Some(&string("unknown")));
        assert_eq!(state.get("heard"), Some(&super::IpcValue::Null));
        let Some(super::IpcValue::String(safe_reason)) = state.get("safeReason") else {
            panic!("an unsampled snapshot must disclose that it sampled nothing");
        };
        assert!(safe_reason.contains(super::UNSAMPLED_SNAPSHOT_REASON));
    }

    #[test]
    fn safe_reason_is_null_when_nothing_needs_disclosing() {
        assert_eq!(super::safe_reason(&[]), super::IpcValue::Null);
        assert_eq!(
            super::safe_reason(&["first.", "second."]),
            string("first. second.")
        );
    }

    #[test]
    fn adapter_limitations_are_stated_rather_than_left_empty() {
        let super::IpcValue::Array(limitations) = super::adapter_limitations() else {
            panic!("limitations must be an array");
        };
        assert_eq!(limitations.len(), super::ADAPTER_LIMITATIONS.len());
        assert!(limitations.iter().all(
            |limitation| matches!(limitation, super::IpcValue::String(value) if !value.is_empty())
        ));
    }

    fn offer_range(axis: &str, value: i64) -> super::IpcValue {
        let version = object([("axis", string(axis)), ("value", super::number(value))]);
        object([
            ("axis", string(axis)),
            ("minimum", version.clone()),
            ("maximum", version),
        ])
    }

    fn current_offer() -> std::collections::BTreeMap<String, super::IpcValue> {
        let super::IpcValue::Object(offer) = object([
            ("backendContract", offer_range("backend-contract", 1)),
            ("capabilitySchema", offer_range("capability-schema", 1)),
            ("eventSchema", offer_range("event-schema", 1)),
            ("traceFormat", offer_range("trace-format", 1)),
            ("ipcProtocol", offer_range("ipc-protocol", 2)),
        ]) else {
            panic!("the version offer must be an object");
        };
        offer
    }

    #[test]
    fn version_offer_rejects_disjoint_and_malformed_ranges() {
        let mut incompatible = current_offer();
        incompatible.insert("ipcProtocol".to_owned(), offer_range("ipc-protocol", 1));
        let error = negotiate_ipc_versions(&incompatible).expect_err("disjoint offers must fail");
        assert_eq!(error.code, "protocol.incompatible");

        let mut malformed = current_offer();
        malformed.remove("traceFormat");
        let error = negotiate_ipc_versions(&malformed).expect_err("missing axes must fail");
        assert_eq!(error.code, "protocol.malformed");
    }

    #[test]
    fn release_receipt_and_version_offer_are_explicit() {
        assert!(matches!(released(), super::IpcValue::Object(_)));
        let offer = current_offer();
        let super::IpcValue::Object(versions) =
            negotiate_ipc_versions(&offer).expect("the current offer must negotiate")
        else {
            panic!("the negotiated versions must be an object");
        };
        assert_eq!(
            versions.get("ipcProtocol"),
            Some(&object([
                ("axis", string("ipc-protocol")),
                (
                    "selected",
                    object([
                        ("axis", string("ipc-protocol")),
                        ("value", super::number(2))
                    ])
                ),
                (
                    "localRange",
                    object([
                        ("axis", string("ipc-protocol")),
                        (
                            "minimum",
                            object([
                                ("axis", string("ipc-protocol")),
                                ("value", super::number(2))
                            ])
                        ),
                        (
                            "maximum",
                            object([
                                ("axis", string("ipc-protocol")),
                                ("value", super::number(2))
                            ])
                        )
                    ])
                ),
                (
                    "remoteRange",
                    object([
                        ("axis", string("ipc-protocol")),
                        (
                            "minimum",
                            object([
                                ("axis", string("ipc-protocol")),
                                ("value", super::number(2))
                            ])
                        ),
                        (
                            "maximum",
                            object([
                                ("axis", string("ipc-protocol")),
                                ("value", super::number(2))
                            ])
                        )
                    ])
                )
            ]))
        );
    }

    #[test]
    fn completed_correlation_replay_is_protocol_violation() {
        let operations = std::collections::HashMap::new();
        let mut completed = std::collections::HashMap::new();
        let now = std::time::Instant::now();
        super::remember_completed_correlation(
            &mut std::collections::HashMap::new(),
            &mut completed,
            "c1".to_owned(),
            now,
        );
        let error =
            super::admit_caller_correlation(&operations, &mut completed, "c1", "gatt.read", now)
                .expect_err("completed correlation replay must fail");
        assert_eq!(error.code, "protocol.violation");
        assert_eq!(error.operation, "tauri.correlation-replay");
    }

    #[test]
    fn completed_scan_and_subscribe_correlations_are_also_rejected() {
        let operations = std::collections::HashMap::new();
        let mut completed = std::collections::HashMap::new();
        let now = std::time::Instant::now();
        for correlation in ["scan-c1", "subscribe-c1"] {
            completed.insert(correlation.to_owned(), now);
            let error = super::admit_caller_correlation(
                &operations,
                &mut completed,
                correlation,
                "scan.start",
                now,
            )
            .expect_err("completed scan/subscribe replay must fail");
            assert_eq!(error.operation, "tauri.correlation-replay");
        }
    }

    #[test]
    fn in_flight_duplicate_correlation_is_still_rejected() {
        let mut operations = std::collections::HashMap::new();
        operations.insert("c1".to_owned(), tokio_util::sync::CancellationToken::new());
        let mut completed = std::collections::HashMap::new();
        let error = super::admit_caller_correlation(
            &operations,
            &mut completed,
            "c1",
            "gatt.read",
            std::time::Instant::now(),
        )
        .expect_err("in-flight duplicate must fail");
        assert_eq!(error.operation, "tauri.correlation-replay");
    }

    #[test]
    fn new_correlation_on_same_lease_succeeds() {
        let operations = std::collections::HashMap::new();
        let mut completed = std::collections::HashMap::new();
        let now = std::time::Instant::now();
        completed.insert("c1".to_owned(), now);
        super::admit_caller_correlation(&operations, &mut completed, "c2", "gatt.read", now)
            .expect("a fresh correlation must admit");
    }

    #[test]
    fn replay_set_cleared_on_lease_drop() {
        let mut completed = std::collections::HashMap::new();
        completed.insert("c1".to_owned(), std::time::Instant::now());
        drop(completed);
        let mut completed = std::collections::HashMap::new();
        super::admit_caller_correlation(
            &std::collections::HashMap::new(),
            &mut completed,
            "c1",
            "gatt.read",
            std::time::Instant::now(),
        )
        .expect("a new lease must not inherit the prior replay window");
    }

    #[test]
    fn expired_completed_correlation_leaves_the_replay_window_after_30_seconds() {
        let operations = std::collections::HashMap::new();
        let mut completed = std::collections::HashMap::new();
        let now = std::time::Instant::now();
        completed.insert("c1".to_owned(), now - std::time::Duration::from_secs(31));
        super::admit_caller_correlation(&operations, &mut completed, "c1", "gatt.read", now)
            .expect("expired completed correlations must leave the window");
        assert!(!completed.contains_key("c1"));
    }

    #[test]
    fn cleanup_still_admits_after_more_than_256_unique_completed_routes() {
        let operations = std::collections::HashMap::new();
        let mut completed = std::collections::HashMap::new();
        let now = std::time::Instant::now();
        for index in 0..=super::MAX_CORRELATIONS {
            completed.insert(format!("done-{index}"), now);
        }
        assert!(completed.len() > super::MAX_CORRELATIONS);
        super::admit_caller_correlation(
            &operations,
            &mut completed,
            "scan-stop-1",
            "scan.stop",
            now,
        )
        .expect("scan.stop must remain usable after more than 256 completed routes");
        super::admit_caller_correlation(
            &operations,
            &mut completed,
            "unsubscribe-1",
            "gatt.unsubscribe",
            now,
        )
        .expect("gatt.unsubscribe must remain usable after more than 256 completed routes");
        super::admit_caller_correlation(
            &operations,
            &mut completed,
            "disconnect-1",
            "connection.disconnect",
            now,
        )
        .expect("connection.disconnect must remain usable after more than 256 completed routes");
    }

    #[test]
    fn replay_of_an_old_correlation_still_rejects_after_more_than_256_completed_routes() {
        let operations = std::collections::HashMap::new();
        let mut completed = std::collections::HashMap::new();
        let now = std::time::Instant::now();
        for index in 0..=super::MAX_CORRELATIONS {
            completed.insert(format!("done-{index}"), now);
        }
        let error = super::admit_caller_correlation(
            &operations,
            &mut completed,
            "done-0",
            "scan.stop",
            now,
        )
        .expect_err("replay of an old correlation must still reject");
        assert_eq!(error.code, "protocol.violation");
        assert_eq!(error.operation, "tauri.correlation-replay");
    }

    #[test]
    fn live_operation_exhaustion_is_backpressure_not_protocol_violation() {
        let mut operations = std::collections::HashMap::new();
        let mut completed = std::collections::HashMap::new();
        let now = std::time::Instant::now();
        for index in 0..super::MAX_CORRELATIONS {
            operations.insert(
                format!("live-{index}"),
                tokio_util::sync::CancellationToken::new(),
            );
        }
        let error = super::admit_caller_correlation(
            &operations,
            &mut completed,
            "overflow",
            "gatt.read",
            now,
        )
        .expect_err("live exhaustion must reject new ordinary work");
        assert_eq!(error.code, "stream.quota");
        assert_eq!(error.operation, "tauri.correlation-busy");
        assert!(
            error.retryable,
            "live exhaustion is backpressure, not a protocol violation"
        );
        assert_eq!(operations.len(), super::MAX_CORRELATIONS);
    }

    #[test]
    fn cleanup_still_admits_when_live_operations_are_exhausted() {
        let mut operations = std::collections::HashMap::new();
        let mut completed = std::collections::HashMap::new();
        let now = std::time::Instant::now();
        for index in 0..super::MAX_CORRELATIONS {
            operations.insert(
                format!("live-{index}"),
                tokio_util::sync::CancellationToken::new(),
            );
        }
        super::admit_caller_correlation(
            &operations,
            &mut completed,
            "scan-stop-cleanup",
            "scan.stop",
            now,
        )
        .expect("scan.stop must reserve admission when live work is at capacity");
        super::admit_caller_correlation(
            &operations,
            &mut completed,
            "unsubscribe-cleanup",
            "gatt.unsubscribe",
            now,
        )
        .expect("gatt.unsubscribe must reserve admission when live work is at capacity");
        super::admit_caller_correlation(
            &operations,
            &mut completed,
            "disconnect-cleanup",
            "connection.disconnect",
            now,
        )
        .expect("connection.disconnect must reserve admission when live work is at capacity");
    }

    fn quarantine_key(command: &str, handle: &str) -> super::QuarantineKey {
        super::QuarantineKey {
            command: command.to_owned(),
            handle: handle.to_owned(),
        }
    }

    fn quarantine_payload() -> std::collections::BTreeMap<String, super::IpcValue> {
        std::collections::BTreeMap::new()
    }

    #[test]
    fn persistent_cleanup_failure_stops_after_eight_attempts() {
        let mut scheduler = super::QuarantineScheduler::new();
        let key = quarantine_key("scan.stop", "scan-1");
        assert!(matches!(
            scheduler.enqueue(key.clone(), quarantine_payload(), 8),
            super::QuarantineAdmit::Start
        ));
        for _ in 0..super::MAX_QUARANTINE_ATTEMPTS {
            scheduler.record_attempt(&key);
        }
        scheduler.exhaust(key.clone());
        assert_eq!(scheduler.failures.len(), 1);
        assert!(!scheduler.keys.contains(&key));
    }

    #[test]
    fn repeated_cancelled_success_for_same_handle_coalesces_to_one_worker() {
        let mut scheduler = super::QuarantineScheduler::new();
        let key = quarantine_key("scan.stop", "scan-1");
        assert!(matches!(
            scheduler.enqueue(key.clone(), quarantine_payload(), 8),
            super::QuarantineAdmit::Start
        ));
        assert!(matches!(
            scheduler.enqueue(key, quarantine_payload(), 8),
            super::QuarantineAdmit::Coalesced
        ));
        assert_eq!(scheduler.active_workers, 1);
    }

    #[test]
    fn lease_drop_cancels_quarantine_workers() {
        let mut scheduler = super::QuarantineScheduler::new();
        scheduler.enqueue(
            quarantine_key("scan.stop", "scan-1"),
            quarantine_payload(),
            8,
        );
        scheduler.cancel();
        assert!(scheduler.cancelled);
        assert_eq!(scheduler.active_workers, 0);
        assert!(matches!(
            scheduler.enqueue(
                quarantine_key("scan.stop", "scan-2"),
                quarantine_payload(),
                8
            ),
            super::QuarantineAdmit::Cancelled
        ));
    }

    #[test]
    fn worker_count_per_lease_capped_at_four() {
        let mut scheduler = super::QuarantineScheduler::new();
        for index in 0..4 {
            assert!(matches!(
                scheduler.enqueue(
                    quarantine_key("scan.stop", &format!("scan-{index}")),
                    quarantine_payload(),
                    8,
                ),
                super::QuarantineAdmit::Start
            ));
        }
        assert_eq!(scheduler.active_workers, 4);
        assert!(matches!(
            scheduler.enqueue(
                quarantine_key("scan.stop", "scan-4"),
                quarantine_payload(),
                8
            ),
            super::QuarantineAdmit::Queued
        ));
        assert_eq!(scheduler.active_workers, 4);
    }

    #[test]
    fn fifth_distinct_cleanup_waits_in_the_bounded_queue_rather_than_disappearing() {
        let mut scheduler = super::QuarantineScheduler::new();
        for index in 0..4 {
            scheduler.enqueue(
                quarantine_key("scan.stop", &format!("scan-{index}")),
                quarantine_payload(),
                8,
            );
        }
        assert!(matches!(
            scheduler.enqueue(
                quarantine_key("scan.stop", "scan-4"),
                quarantine_payload(),
                8
            ),
            super::QuarantineAdmit::Queued
        ));
        assert_eq!(scheduler.queue.len(), 1);
        assert!(scheduler
            .keys
            .contains(&quarantine_key("scan.stop", "scan-4")));
        let first = quarantine_key("scan.stop", "scan-0");
        let next = scheduler.succeed(&first);
        assert!(next.is_some());
        assert_eq!(next.unwrap().0.handle, "scan-4");
    }

    #[test]
    fn exhausted_cleanup_appears_in_release_failed_and_is_retried_by_release() {
        let mut scheduler = super::QuarantineScheduler::new();
        let key = quarantine_key("connection.disconnect", "connection-1");
        scheduler.enqueue(key.clone(), quarantine_payload(), 8);
        scheduler.exhaust(key.clone());
        assert_eq!(scheduler.failures.len(), 1);
        scheduler.succeed(&key);
        assert!(scheduler.failures.is_empty());
    }

    #[test]
    fn ownership_denied_stops_retry_without_resurrecting_the_resource() {
        let mut scheduler = super::QuarantineScheduler::new();
        let key = quarantine_key("gatt.unsubscribe", "sub-1");
        scheduler.enqueue(key.clone(), quarantine_payload(), 8);
        scheduler.succeed(&key);
        assert!(!scheduler.keys.contains(&key));
        assert!(scheduler.failures.is_empty());
    }
}
