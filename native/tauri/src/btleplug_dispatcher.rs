use std::{
    collections::{BTreeMap, HashMap, HashSet},
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
use crate::ATTACH_REQUEST_KIND;
use crate::{AuthenticatedCaller, DispatchFuture, IpcDispatcher, IpcEventSink, IpcValue};

const MAX_PENDING_EVENTS: usize = 256;
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(250);

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
    peer_owners: HashMap<String, String>,
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
    pending_events: HashSet<String>,
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
    characteristics: HashMap<String, Characteristic>,
    descriptors: HashMap<String, Descriptor>,
}

struct SubscriptionResource {
    database_handle: String,
    peripheral: Peripheral,
    characteristic: Characteristic,
    task: TauriJoinHandle<()>,
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
                peer_owners: HashMap::new(),
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
                pending_events: HashSet::new(),
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
            if caller_state.operations.contains_key(&correlation) {
                return Err(DispatchError::new(
                    "protocol.violation",
                    "ipc",
                    "tauri.correlation-replay",
                ));
            }
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
            caller_state.operations.remove(&correlation);
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
            "connection.connect" => payload.get("handle").and_then(as_string).map(|handle| {
                (
                    "connection.disconnect",
                    object([("connectionHandle", string(handle.to_owned()))]),
                )
            }),
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
            self.execute(caller, cleanup_command, cleanup_payload, None)
                .await
                .ok();
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
        let service_uuids = optional_string_array(&payload, "serviceUuids", "tauri.scan-services")?
            .into_iter()
            .map(|value| parse_uuid(&value, "tauri.scan-services"))
            .collect::<Result<Vec<_>, _>>()?;
        let local_name_prefix =
            optional_nullable_string(&payload, "localNamePrefix", "tauri.scan-name")?;
        let manufacturer_filters = manufacturer_filters(&payload)?;
        let adapter = self.adapter().await?;
        let key = caller_key(caller);
        let expected_lease_id = required_string(&payload, "__expectedLeaseId", "tauri.scan-lease")?;
        let expected_lease_generation =
            required_string(&payload, "__expectedLeaseGeneration", "tauri.scan-lease")?;
        let requested_services = service_uuids.clone();
        {
            let mut state = self.inner.lock().await;
            if state.scan_owner.is_some() {
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
            if scan_owner_matches {
                state.scan_owner = None;
            }
            drop(state);
            if scan_owner_matches {
                adapter.stop_scan().await.ok();
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
                state.scan_owner = None;
            }
            drop(state);
            adapter.stop_scan().await.ok();
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
        Ok(object([("handle", string(handle))]))
    }

    async fn stop_scan(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "scanHandle", "tauri.scan-stop")?;
        let key = caller_key(caller);
        {
            let state = self.inner.lock().await;
            let caller_state = state.callers.get(&key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "scan", "tauri.scan-stop-owner")
            })?;
            match caller_state.scan.as_ref() {
                Some(scan) if scan.handle == handle => scan.task.abort(),
                Some(_) => {
                    return Err(DispatchError::new(
                        "ownership.denied",
                        "scan",
                        "tauri.scan-stop-handle",
                    ))
                }
                None => return Ok(released()),
            }
        }
        self.adapter().await?.stop_scan().await.map_err(|error| {
            DispatchError::new("scan.stop-failed", "scan", "tauri.scan-stop")
                .platform(error.to_string())
        })?;
        let mut state = self.inner.lock().await;
        if let Some(caller_state) = state.callers.get_mut(&key) {
            caller_state.scan.take();
        }
        if state.scan_owner.as_deref() == Some(&key) {
            state.scan_owner = None;
        }
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
        let Some(caller_state) = state.callers.get_mut(&key) else {
            state.peer_owners.remove(&peer_id);
            drop(state);
            peripheral.disconnect().await.ok();
            return Err(DispatchError::new(
                "ownership.denied",
                "connection",
                "tauri.connect-owner",
            ));
        };
        if !expected_lease_matches(caller_state, &payload) {
            let _ = caller_state;
            if peer_owner_matches {
                state.peer_owners.remove(&peer_id);
            }
            drop(state);
            peripheral.disconnect().await.ok();
            return Err(DispatchError::new(
                "ownership.denied",
                "connection",
                "tauri.connect-stale-lease",
            ));
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
        connection.peripheral.disconnect().await.map_err(|error| {
            DispatchError::new("platform.failure", "connection", "tauri.disconnect")
                .platform(error.to_string())
        })?;
        let subscriptions = {
            let mut state = self.inner.lock().await;
            let Some(caller_state) = state.callers.get_mut(&key) else {
                state.peer_owners.remove(&connection.peer_id);
                drop(state);
                return Ok(released());
            };
            caller_state.connections.remove(&handle);
            let database_handles = caller_state
                .databases
                .iter()
                .filter_map(|(database_handle, database)| {
                    (database.connection_handle == handle).then_some(database_handle.clone())
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
                    (event.connection_handle == handle).then_some(event_handle.clone())
                })
                .collect::<Vec<_>>();
            let event_tasks = event_handles
                .into_iter()
                .filter_map(|event_handle| caller_state.connection_events.remove(&event_handle))
                .filter_map(|event| event.task)
                .collect::<Vec<_>>();
            state.peer_owners.remove(&connection.peer_id);
            (subscriptions, event_tasks)
        };
        for subscription in subscriptions.0 {
            subscription.task.abort();
        }
        for task in subscriptions.1 {
            task.abort();
        }
        Ok(released())
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
        let mut characteristic_records = Vec::new();
        let mut descriptor_records = Vec::new();
        let mut occurrences: HashMap<(Uuid, Uuid), usize> = HashMap::new();
        for characteristic in peripheral.characteristics() {
            let occurrence = occurrences
                .entry((characteristic.service_uuid, characteristic.uuid))
                .or_default();
            let handle = self.id("characteristic");
            characteristic_records.push(object([
                ("handle", string(handle.clone())),
                (
                    "serviceUuid",
                    string(characteristic.service_uuid.to_string()),
                ),
                ("serviceOccurrence", string("0")),
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
            characteristic_map.insert(handle, characteristic);
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
        caller_state.databases.insert(
            database_handle.clone(),
            DatabaseResource {
                connection_handle,
                database_id: database_id.clone(),
                database_generation: database_generation.clone(),
                characteristics: characteristic_map,
                descriptors: descriptor_map,
            },
        );
        Ok(object([
            ("handle", string(database_handle)),
            ("databaseId", string(database_id)),
            ("databaseGeneration", string(database_generation)),
            ("characteristics", IpcValue::Array(characteristic_records)),
            ("descriptors", IpcValue::Array(descriptor_records)),
        ]))
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
            drop(state);
            peripheral.unsubscribe(&characteristic).await.ok();
            return Err(DispatchError::new(
                "ownership.denied",
                "gatt",
                "tauri.subscribe-owner",
            ));
        };
        if !expected_lease_matches(caller_state, &payload) {
            task.abort();
            let _ = caller_state;
            drop(state);
            peripheral.unsubscribe(&characteristic).await.ok();
            return Err(DispatchError::new(
                "ownership.denied",
                "gatt",
                "tauri.subscribe-stale-lease",
            ));
        }
        caller_state.subscriptions.insert(
            handle.clone(),
            SubscriptionResource {
                database_handle,
                peripheral,
                characteristic,
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
        let subscription = {
            let state = self.inner.lock().await;
            let caller_state = state.callers.get(&key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "gatt", "tauri.unsubscribe-owner")
            })?;
            caller_state.subscriptions.get(&handle).map(|subscription| {
                (
                    subscription.peripheral.clone(),
                    subscription.characteristic.clone(),
                )
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
                if let Some(subscription) = caller_state.subscriptions.remove(&handle) {
                    subscription.task.abort();
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
                    return Ok(());
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
        if send_result.is_ok() {
            self.terminal(
                caller_key,
                expected_lease,
                identity.stream_id,
                terminal_reason,
            )
            .await
            .ok();
        }
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
            let lease_matches = state.callers.get(caller_key).is_some_and(|caller| {
                caller.lease_id == expected_lease.0 && caller.lease_generation == expected_lease.1
            });
            if !lease_matches {
                return;
            }
            let scan = state
                .callers
                .get_mut(caller_key)
                .and_then(|caller| caller.scan.take());
            let owned = scan.as_ref().is_some_and(|scan| scan.handle == stream_id);
            if !owned {
                if let Some(scan) = scan {
                    if let Some(caller) = state.callers.get_mut(caller_key) {
                        caller.scan = Some(scan);
                    }
                }
                false
            } else {
                if state.scan_owner.as_deref() == Some(caller_key) {
                    state.scan_owner = None;
                }
                true
            }
        };
        if should_stop {
            if let Ok(adapter) = self.adapter().await {
                adapter.stop_scan().await.ok();
            }
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
            subscription.task.abort();
            if subscription
                .peripheral
                .unsubscribe(&subscription.characteristic)
                .await
                .is_err()
            {
                let mut state = self.inner.lock().await;
                if let Some(caller) = state.callers.get_mut(caller_key) {
                    caller
                        .subscriptions
                        .insert(stream_id.to_owned(), subscription);
                }
            }
        }
    }

    async fn release(&self, key: &str) -> IpcValue {
        let caller = self.inner.lock().await.callers.remove(key);
        let Some(mut caller) = caller else {
            return released();
        };
        let cleanup = self.settle_caller(key, &mut caller).await;
        if !is_released(&cleanup) {
            let mut state = self.inner.lock().await;
            state.callers.entry(key.to_owned()).or_insert(caller);
        }
        cleanup
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
        for cancellation in caller.operations.values() {
            cancellation.cancel();
        }
        caller.operations.clear();
        if caller.scan_admitting {
            caller.scan_admitting = false;
            let mut state = self.inner.lock().await;
            if state.scan_owner.as_deref() == Some(key) {
                state.scan_owner = None;
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
            match subscription
                .peripheral
                .unsubscribe(&subscription.characteristic)
                .await
            {
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
        let connection_handles = caller.connections.keys().cloned().collect::<Vec<_>>();
        for handle in connection_handles {
            let Some(connection) = caller.connections.get(&handle).cloned() else {
                continue;
            };
            match connection.peripheral.disconnect().await {
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

fn manufacturer_filters(
    payload: &BTreeMap<String, IpcValue>,
) -> Result<Vec<ManufacturerFilter>, DispatchError> {
    let Some(value) = payload.get("manufacturerData") else {
        return Ok(Vec::new());
    };
    let IpcValue::Array(filters) = value else {
        return Err(DispatchError::new(
            "scan.filter-invalid",
            "scan",
            "tauri.scan-manufacturer-filter",
        ));
    };
    filters
        .iter()
        .map(|filter| {
            let IpcValue::Object(filter) = filter else {
                return Err(DispatchError::new(
                    "scan.filter-invalid",
                    "scan",
                    "tauri.scan-manufacturer-filter",
                ));
            };
            let company_id = match filter.get("companyId") {
                Some(IpcValue::Number(value)) => value
                    .as_u64()
                    .and_then(|value| u16::try_from(value).ok())
                    .ok_or_else(|| {
                        DispatchError::new(
                            "scan.filter-invalid",
                            "scan",
                            "tauri.scan-manufacturer-company",
                        )
                    })?,
                _ => {
                    return Err(DispatchError::new(
                        "scan.filter-invalid",
                        "scan",
                        "tauri.scan-manufacturer-company",
                    ))
                }
            };
            let data_prefix = match filter.get("dataPrefix") {
                None | Some(IpcValue::Null) => None,
                Some(IpcValue::Bytes(bytes)) => Some(bytes.clone()),
                _ => {
                    return Err(DispatchError::new(
                        "scan.filter-invalid",
                        "scan",
                        "tauri.scan-manufacturer-prefix",
                    ))
                }
            };
            Ok(ManufacturerFilter {
                company_id,
                data_prefix,
            })
        })
        .collect()
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

fn optional_string_array(
    object: &BTreeMap<String, IpcValue>,
    key: &str,
    operation: impl Into<String>,
) -> Result<Vec<String>, DispatchError> {
    let operation = operation.into();
    match object.get(key) {
        None => Ok(Vec::new()),
        Some(IpcValue::Array(values)) => values
            .iter()
            .map(|value| match value {
                IpcValue::String(value) => Ok(value.clone()),
                _ => Err(DispatchError::new(
                    "protocol.malformed",
                    "ipc",
                    operation.clone(),
                )),
            })
            .collect(),
        _ => Err(DispatchError::new("protocol.malformed", "ipc", operation)),
    }
}

fn optional_nullable_string(
    object: &BTreeMap<String, IpcValue>,
    key: &str,
    operation: impl Into<String>,
) -> Result<Option<String>, DispatchError> {
    match object.get(key) {
        None | Some(IpcValue::Null) => Ok(None),
        Some(IpcValue::String(value)) => Ok(Some(value.clone())),
        _ => Err(DispatchError::new("protocol.malformed", "ipc", operation)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        characteristic_properties, negotiate_ipc_versions, object, released,
        scan_properties_match_optional, string,
    };
    use btleplug::api::CharPropFlags;

    #[test]
    fn capability_projection_is_data_only() {
        assert_eq!(
            characteristic_properties(CharPropFlags::READ | CharPropFlags::NOTIFY),
            super::IpcValue::Array(vec![string("read"), string("notify")])
        );
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
}
