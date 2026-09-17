use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        atomic::{AtomicI64, AtomicU64, Ordering},
        Arc, Mutex as SyncMutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use btleplug::api::CharPropFlags;
use btleplug::{
    api::{Central, Manager as _},
    platform::{Adapter, Manager},
};
use serde_json::Number;
use tauri::async_runtime::JoinHandle as TauriJoinHandle;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use ubm_core::contracts::BleErrorCode;
use uuid::Uuid;

use crate::capabilities;
use crate::desktop_core::{CoreAuthority, CoreSelector, DesktopCore};
use crate::scan_plan::{decode_normalized_scan_query, diagnostic_scan_plan};
use crate::ATTACH_REQUEST_KIND;
use crate::{AuthenticatedCaller, DispatchFuture, IpcDispatcher, IpcEventSink, IpcValue};

const MAX_PENDING_EVENTS: usize = 256;
const MAX_CORRELATIONS: usize = 256;
const COMPLETED_CORRELATION_TTL: Duration = Duration::from_secs(30);
/// Delivery pacing between core `take` polls (scan observations and
/// notification forwarders). This paces delivery only: admission, deadlines,
/// overflow, and teardown all stay core-owned.
const FORWARD_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Safety backstops, not caller promises.
///
/// IPC `deadline` values arrive on the webview monotonic clock, which the
/// plugin cannot read (no shared epoch crosses the wire), so the dispatcher
/// cannot convert them into core-relative timeouts without guessing — and it
/// never guesses. Every op therefore crosses with a documented backstop
/// below, while the TypeScript outer bound still fires first and every abort
/// settles through core `cancel_operation`. A backstop only decides an op the
/// caller abandoned: no live caller ever waits on one.
const SCAN_BACKSTOP_TIMEOUT_MS: u64 = 86_400_000;
const OP_BACKSTOP_TIMEOUT_MS: u64 = 30_000;

fn btleplug_runtime() -> tokio::runtime::Handle {
    // Delegated: one shared desktop executor per process, owned by
    // `ubm-desktop` (a real crate dependency since the F01 authority
    // migration; see `desktop_core.rs`). Behavior is unchanged (same
    // dedicated thread, same two workers); only the owner moved.
    ubm_desktop::executor::desktop_runtime()
}

#[derive(Clone, Debug, Default)]
pub struct BtleplugDispatcherOptions {
    /// Exact `Adapter::adapter_info()` value to select when multiple adapters exist.
    pub adapter_id: Option<String>,
}

/// Production Tauri dispatcher: IPC transport over the shared Rust core.
///
/// BLE scheduling executes [`DesktopCore`] (the shared-core authority)
/// through [`CoreAuthority`]; this struct owns no scan policy, no retry, no
/// timeout timers, and no ownership generations — only IPC envelope
/// admission, caller leases, transport-handle mapping, and event delivery.
/// The legacy btleplug `Adapter` handle survives solely as a passive
/// power-state fact source for `adapter.state` (no scheduled work flows
/// through it: no scan, connect, discover, read, write, or subscribe call
/// touches it); every scheduled op delegates to the authority.
#[derive(Clone)]
pub struct BtleplugDispatcher {
    inner: Arc<Mutex<DispatcherState>>,
    bootstrap_admission: Arc<Mutex<()>>,
    next_id: Arc<AtomicU64>,
    next_revocation: Arc<AtomicU64>,
    started_at: Arc<Instant>,
    revoked_callers: Arc<SyncMutex<HashMap<String, u64>>>,
    options: BtleplugDispatcherOptions,
    authority: Arc<Mutex<AuthoritySlot>>,
}

/// Which scheduling authority the dispatcher serves. `Unopened` opens the
/// production core (btleplug radio) on the first BLE op and fails loudly
/// (`adapter.unavailable`) where no radio exists — never legacy direct
/// ownership. `ShutDown` refuses loudly after an explicit shutdown.
#[derive(Clone)]
enum AuthoritySlot {
    Unopened,
    Open(Arc<dyn CoreAuthority>),
    ShutDown,
}

struct DispatcherState {
    manager: Option<Manager>,
    adapter: Option<Adapter>,
    attachment: Option<Attachment>,
    callers: HashMap<String, CallerState>,
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
    scan: Option<ScanResource>,
    connections: HashMap<String, CoreConnection>,
    databases: HashMap<String, CoreDatabase>,
    subscriptions: HashMap<String, CoreSubscription>,
    connection_events: HashMap<String, ConnectionEventResource>,
    operations: HashMap<String, TrackedOperation>,
    completed_correlations: HashMap<String, Instant>,
    pending_events: HashSet<String>,
}

/// One waiter on an IPC correlation: the token aborts the local waiter, and
/// the core operation id (present for core-issued ops, notably scans) lets
/// cancellation settle through the core instead of stranding the op.
struct TrackedOperation {
    token: CancellationToken,
    core_operation_id: Option<String>,
}

/// Transport mapping for one live scan: the IPC handle the caller holds.
/// The core scan operation id (which cancellations address) lives on the
/// tracked correlation in `CallerState.operations`, not here. The forwarder
/// task only delivers core observations verbatim — it filters, merges, and
/// paces nothing.
struct ScanResource {
    handle: String,
    task: TauriJoinHandle<()>,
}

/// Transport mapping for one core-owned connection: the IPC handle plus the
/// exact `(peer_id, lease)` the core addresses. Lifecycle (generations,
/// ownership, timeouts) lives in the core; the generation here is the core's
/// own, echoed for wire compatibility.
#[derive(Clone)]
struct CoreConnection {
    peer_id: String,
    lease: String,
    connection_id: String,
    owner_lease_id: String,
    connection_generation: String,
}

/// Transport mapping for one core-registered discovery tree: the IPC handle
/// plus the per-handle selectors resolved from the whole-tree paths the
/// core registered. Later ops resolve against these stored selectors —
/// never against a parallel radio handle.
struct CoreDatabase {
    connection_handle: String,
    database_id: String,
    database_generation: String,
    valid: bool,
    characteristics: HashMap<String, CoreSelector>,
    descriptors: HashMap<String, CoreSelector>,
}

/// Transport mapping for one core-arbitrated subscription: the exact
/// `(peer_id, selector, consumer)` triple the core addresses, served by a
/// verbatim notification forwarder.
struct CoreSubscription {
    connection_handle: String,
    peer_id: String,
    selector: CoreSelector,
    consumer: String,
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

#[derive(Debug)]
struct DispatchError {
    // F01: the frozen code identity is single-owned by `ubm-core`
    // (`BleErrorCode`); the plugin never spells code strings. The domain
    // stays plugin-local (`ipc` has no frozen member).
    code: BleErrorCode,
    domain: &'static str,
    operation: String,
    platform: Option<String>,
    retryable: bool,
}

impl DispatchError {
    fn new(code: BleErrorCode, domain: &'static str, operation: impl Into<String>) -> Self {
        Self {
            code,
            domain,
            operation: operation.into(),
            platform: None,
            retryable: matches!(
                code,
                BleErrorCode::OperationAborted | BleErrorCode::OperationTimedOut
            ),
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

    /// Lift a shared-core failure into an IPC failure without substitution:
    /// the frozen `code`, `domain`, and `operation` cross verbatim, and the
    /// core transport detail (when present) rides as platform evidence.
    pub(crate) fn from_core(error: &ubm_desktop::DesktopError) -> Self {
        let mut dispatch = Self::new(error.code(), error.domain().as_str(), error.operation());
        if let Some(detail) = error.detail() {
            dispatch = dispatch.platform(detail);
        }
        dispatch
    }

    /// Frozen identity triple for tests: `(code, domain, operation)`.
    #[cfg(test)]
    pub(crate) fn identity(&self) -> (&'static str, &'static str, String) {
        (self.code.as_str(), self.domain, self.operation.clone())
    }

    fn normalized_error(&self) -> IpcValue {
        let platform = self.platform.as_ref().map_or(IpcValue::Null, |message| {
            object([
                ("domain", string("btleplug")),
                ("code", string("native-error")),
                ("safeMessage", string(message.clone())),
                ("metadata", object([])),
            ])
        });
        object([
            ("code", string(self.code.as_str())),
            ("domain", string(self.domain)),
            ("operation", string(self.operation.clone())),
            ("platform", platform),
            (
                "retryability",
                string(if self.retryable {
                    "caller-decides"
                } else {
                    "never"
                }),
            ),
        ])
    }

    fn into_response(self) -> IpcValue {
        object([
            ("kind", string("failure")),
            ("error", self.normalized_error()),
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
            })),
            bootstrap_admission: Arc::new(Mutex::new(())),
            next_id: Arc::new(AtomicU64::new(1)),
            next_revocation: Arc::new(AtomicU64::new(1)),
            started_at: Arc::new(Instant::now()),
            revoked_callers: Arc::new(SyncMutex::new(HashMap::new())),
            options,
            authority: Arc::new(Mutex::new(AuthoritySlot::Unopened)),
        }
    }

    /// Dispatcher over an explicitly admitted scheduling authority. Tests
    /// inject [`DesktopCore`] over a scripted boundary here; production
    /// opens the btleplug-backed core lazily. Either way every BLE verdict
    /// comes from the authority — never from direct radio ownership.
    pub fn with_core_authority(authority: Arc<dyn CoreAuthority>) -> Self {
        let mut dispatcher = Self::new(BtleplugDispatcherOptions::default());
        dispatcher.authority = Arc::new(Mutex::new(AuthoritySlot::Open(authority)));
        dispatcher
    }

    /// The admitted scheduling authority, opening the production core
    /// (btleplug radio) on first use. Radio failures surface verbatim —
    /// `adapter.unavailable` where no adapter exists — never silent legacy.
    async fn ensure_authority(&self) -> Result<Arc<dyn CoreAuthority>, DispatchError> {
        let slot = self.authority.lock().await.clone();
        match slot {
            AuthoritySlot::Open(authority) => Ok(authority),
            AuthoritySlot::ShutDown => Err(DispatchError::new(
                BleErrorCode::AdapterUnavailable,
                "adapter",
                "tauri.core-shutdown",
            )),
            AuthoritySlot::Unopened => {
                let handle = btleplug_runtime();
                let radio =
                    ubm_desktop::BtleplugRadio::open(handle, self.options.adapter_id.clone())
                        .await
                        .map_err(|error| DispatchError::from_core(&error))?;
                let mut core = DesktopCore::new(radio, "tauri");
                core.ensure_open()
                    .await
                    .map_err(|error| DispatchError::from_core(&error))?;
                let authority: Arc<dyn CoreAuthority> = Arc::new(tokio::sync::Mutex::new(core));
                *self.authority.lock().await = AuthoritySlot::Open(authority.clone());
                Ok(authority)
            }
        }
    }

    /// Shut the admitted authority down and refuse further BLE work loudly.
    /// Test seam for the missing-core proof; production never calls it
    /// (the process-lifetime central outlives every caller).
    pub async fn authority_shutdown(&self) {
        let slot = self.authority.lock().await.clone();
        if let AuthoritySlot::Open(authority) = slot {
            let _ = authority.shutdown().await;
        }
        *self.authority.lock().await = AuthoritySlot::ShutDown;
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
                BleErrorCode::OwnershipDenied,
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
                    DispatchError::new(
                        BleErrorCode::ProtocolMalformed,
                        "ipc",
                        "tauri.bootstrap-event-channel",
                    )
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
                BleErrorCode::ProtocolMalformed,
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
                DispatchError::new(BleErrorCode::AdapterUnavailable, "adapter", "tauri.runtime")
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
                BleErrorCode::PlatformFailure,
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
                scan: None,
                connections: HashMap::new(),
                databases: HashMap::new(),
                subscriptions: HashMap::new(),
                connection_events: HashMap::new(),
                operations: HashMap::new(),
                completed_correlations: HashMap::new(),
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
                    ("core", core_identity()),
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
                    BleErrorCode::BytesInvalid,
                    "ipc",
                    "tauri.route-binary",
                ))
            }
        };
        self.validate_envelope(&caller, &envelope).await?;

        if command == "operation.cancel" {
            return self.cancel_operation(&caller, &payload).await;
        }

        let cancellation = CancellationToken::new();
        {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(&caller_key(&caller)).ok_or_else(|| {
                DispatchError::new(BleErrorCode::OwnershipDenied, "ipc", "tauri.route-owner")
            })?;
            admit_caller_correlation(
                &caller_state.operations,
                &mut caller_state.completed_correlations,
                &correlation,
                &command,
                Instant::now(),
            )?;
            caller_state.operations.insert(
                correlation.clone(),
                TrackedOperation {
                    token: cancellation.clone(),
                    core_operation_id: None,
                },
            );
        }

        let operation_dispatcher = self.clone();
        let operation_caller = caller.clone();
        let operation_command = command.clone();
        let operation_correlation = correlation.clone();
        let mut operation = tauri::async_runtime::spawn(async move {
            operation_dispatcher
                .execute(
                    &operation_caller,
                    &operation_command,
                    &operation_correlation,
                    payload,
                    binary_payload,
                )
                .await
        });
        // Cancellation aborts the local waiter; the core op is NOT stranded:
        // every op method tracks its resources (scan/connection/subscription
        // handles) before answering, so a late success stays releasable and
        // every abort still settles through core `cancel_operation` where the
        // core issued an id. The detached task runs to core settlement and
        // its verdict is dropped only because the caller already left.
        let result = tokio::select! {
            result = &mut operation => result.map_err(|error| {
                DispatchError::new(BleErrorCode::PlatformFailure, "ipc", format!("tauri.{command}.join"))
                    .platform(error.to_string())
            })?,
            () = cancellation.cancelled() => {
                tauri::async_runtime::spawn(async move {
                    let _ = operation.await;
                });
                Err(DispatchError::new(BleErrorCode::OperationAborted, "ipc", format!("tauri.{command}")))
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

    /// Settle one waiter through the core. An in-flight correlation aborts
    /// the local waiter and, where the core already issued an operation id
    /// (recorded by the op method after admission), cancels through the
    /// core: the settled core outcome decides, never a stranded waiter. An
    /// unknown correlation is already terminal. Core failures propagate
    /// loudly — a cancel that cannot settle is not reported as settled.
    async fn cancel_operation(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let target = required_string(payload, "targetCorrelation", "tauri.cancel")?;
        let tracked = self
            .inner
            .lock()
            .await
            .callers
            .get(&caller_key(caller))
            .ok_or_else(|| {
                DispatchError::new(BleErrorCode::OwnershipDenied, "ipc", "tauri.cancel-owner")
            })?
            .operations
            .get(&target)
            .map(|tracked| (tracked.token.clone(), tracked.core_operation_id.clone()));
        let Some((token, core_operation_id)) = tracked else {
            return Ok(route_response(object([(
                "state",
                string("already-terminal"),
            )])));
        };
        token.cancel();
        if let Some(operation_id) = core_operation_id {
            let authority = self.ensure_authority().await?;
            match authority.cancel_operation(&operation_id).await {
                Ok(_) => {}
                Err(error) => return Err(DispatchError::from_core(&error)),
            }
        }
        Ok(route_response(object([(
            "state",
            string("cancellation-requested"),
        )])))
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
            DispatchError::new(
                BleErrorCode::LifecycleInvalidState,
                "ipc",
                "tauri.route-bootstrap",
            )
        })?;
        let expected_attachment = into_object(
            attachment_record(attachment),
            "tauri.route-attachment-authority",
        )?;
        if attachment.attachment_id != attachment_id
            || !same_attachment_identity(&envelope_attachment, &expected_attachment)
        {
            return Err(DispatchError::new(
                BleErrorCode::ProtocolViolation,
                "ipc",
                "tauri.route-attachment",
            ));
        }
        let caller_state = state.callers.get_mut(&caller_key(caller)).ok_or_else(|| {
            DispatchError::new(BleErrorCode::OwnershipDenied, "ipc", "tauri.route-caller")
        })?;
        if caller_state.lease_id != lease_id || caller_state.lease_generation != lease_generation {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
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
                BleErrorCode::ProtocolViolation,
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
            DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "ipc",
                "tauri.execute-lease-owner",
            )
        })?;
        if caller_state.lease_id != expected_id
            || caller_state.lease_generation != expected_generation
        {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
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
        correlation: &str,
        payload: BTreeMap<String, IpcValue>,
        binary_payload: Option<Vec<u8>>,
    ) -> Result<IpcValue, DispatchError> {
        self.validate_expected_lease(caller, &payload).await?;
        match command {
            "adapter.state" => self.adapter_state().await,
            "scan.start" => self.start_scan(caller, correlation, payload).await,
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
                BleErrorCode::ArgumentInvalid,
                "ipc",
                "tauri.route-command",
            )),
        }
    }

    async fn adapter_state(&self) -> Result<IpcValue, DispatchError> {
        let attachment = {
            let state = self.inner.lock().await;
            state.attachment.clone().ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::AdapterUnavailable,
                    "adapter",
                    "tauri.adapter-state",
                )
            })?
        };
        let adapter = self.adapter().await?;
        let power = match adapter.adapter_state().await {
            Ok(btleplug::api::CentralState::PoweredOn) => "on",
            Ok(btleplug::api::CentralState::PoweredOff) => "off",
            Ok(_) => "unknown",
            Err(error) => {
                return Err(DispatchError::new(
                    BleErrorCode::AdapterUnavailable,
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
                    BleErrorCode::AdapterUnavailable,
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
        correlation: &str,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let query_value = payload.get("query").ok_or_else(|| {
            DispatchError::new(
                BleErrorCode::ProtocolViolation,
                "scan",
                "tauri.scan-query-required",
            )
        })?;
        let decoded_query = decode_normalized_scan_query(query_value).map_err(|error| {
            DispatchError::new(BleErrorCode::ProtocolMalformed, "scan", "tauri.scan-query")
                .platform(error)
        })?;
        let service_uuids = decoded_query
            .native_service_uuids
            .iter()
            .map(|value| parse_uuid(value, "tauri.scan-services"))
            .collect::<Result<Vec<_>, _>>()?;
        let diagnostic_plan = diagnostic_scan_plan(&decoded_query);
        let attachment = self.ensure_adapter().await?;
        let key = caller_key(caller);
        let expected_lease_id = required_string(&payload, "__expectedLeaseId", "tauri.scan-lease")?;
        let expected_lease_generation =
            required_string(&payload, "__expectedLeaseGeneration", "tauri.scan-lease")?;
        {
            // execute() already admitted the caller lease globally; the only
            // remaining admission is one scan per caller. Global overlap is
            // the core's verdict, never a dispatcher guess.
            let state = self.inner.lock().await;
            let caller_state = state.callers.get(&key).ok_or_else(|| {
                DispatchError::new(BleErrorCode::OwnershipDenied, "scan", "tauri.scan-owner")
            })?;
            if caller_state.scan.is_some() {
                return Err(DispatchError::new(
                    BleErrorCode::ScanAlreadyActive,
                    "scan",
                    "tauri.scan-start",
                ));
            }
        }
        // Core first: admission, duplicate/merge/timeout policy, and the scan
        // operation id are all core-owned. A concurrent scan fails here with
        // the core's own verdict — verbatim, never a dispatcher guess.
        let authority = self.ensure_authority().await?;
        let service_uuid_strings: Vec<String> =
            service_uuids.iter().map(|uuid| uuid.to_string()).collect();
        let core_operation_id = authority
            .start_scan(&key, &service_uuid_strings, SCAN_BACKSTOP_TIMEOUT_MS)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        // Record the core id under this correlation so a racing
        // operation.cancel settles through the core instead of stranding it.
        {
            let mut state = self.inner.lock().await;
            if let Some(caller_state) = state.callers.get_mut(&key) {
                if let Some(tracked) = caller_state.operations.get_mut(correlation) {
                    tracked.core_operation_id = Some(core_operation_id.clone());
                }
            }
        }
        let handle = self.id("scan");
        let forwarder = self.clone();
        let forward_key = key.clone();
        let forward_handle = handle.clone();
        let forward_lease_id = expected_lease_id.clone();
        let forward_lease_generation = expected_lease_generation.clone();
        let forward_authority = authority.clone();
        // Verbatim observation delivery: the forwarder takes core
        // observations and emits them unchanged. It filters, merges, and
        // paces nothing — duplicate/merge policy is the core's (the service
        // filter above crossed into the core admission), and view shaping
        // stays on the TypeScript side.
        let task = tauri::async_runtime::spawn(async move {
            loop {
                let live = {
                    forwarder
                        .inner
                        .lock()
                        .await
                        .callers
                        .get(&forward_key)
                        .is_some_and(|caller_state| {
                            caller_state
                                .scan
                                .as_ref()
                                .is_some_and(|scan| scan.handle == forward_handle)
                        })
                };
                if !live {
                    return;
                }
                match forward_authority.take_advertisement().await {
                    Ok(Some(snapshot)) => {
                        let observation = core_scan_observation(&snapshot);
                        let _ = forwarder
                            .emit(
                                &forward_key,
                                Some((&forward_lease_id, &forward_lease_generation)),
                                &forward_handle,
                                observation,
                                true,
                            )
                            .await;
                    }
                    Ok(None) => tokio::time::sleep(FORWARD_POLL_INTERVAL).await,
                    Err(_) => {
                        // Core-side failure ends delivery; the scan verdict
                        // itself surfaces through stop_scan/release, never as
                        // a guessed stream error.
                        return;
                    }
                }
            }
        });
        {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(&key).ok_or_else(|| {
                DispatchError::new(BleErrorCode::OwnershipDenied, "scan", "tauri.scan-owner")
            })?;
            // Late validation: the caller may have been released while the
            // core admitted — stop the core scan instead of stranding it.
            if caller_state.scan.is_some() {
                task.abort();
                let _ = authority.stop_scan().await;
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "scan",
                    "tauri.scan-start",
                ));
            }
            caller_state.scan = Some(ScanResource {
                handle: handle.clone(),
                task,
            });
        }
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
        // Detach the tracked scan first so a racing forwarder exits on its
        // next check. Unknown handle with a live caller is idempotent
        // release (the core stop below is idempotent too); an unknown caller
        // is denied (a released caller owns nothing to stop).
        let tracked = {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(&key).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "scan",
                    "tauri.scan-stop-owner",
                )
            })?;
            match caller_state.scan.take() {
                Some(scan) if scan.handle == handle => Some(scan),
                Some(scan) => {
                    caller_state.scan = Some(scan);
                    None
                }
                None => None,
            }
        };
        if let Some(scan) = tracked {
            scan.task.abort();
            let authority = self.ensure_authority().await?;
            authority
                .stop_scan()
                .await
                .map_err(|error| DispatchError::from_core(&error))?;
        }
        Ok(released())
    }

    async fn connect(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let peer_id = required_string(&payload, "peerId", "tauri.connect-peer")?;
        let key = caller_key(caller);
        {
            // execute() already admitted the caller lease globally; the only
            // remaining admission is caller presence. Peer ownership and link
            // generations are core-owned: a doubly-owned peer fails here with
            // the core's own verdict — verbatim, never a dispatcher guess.
            let state = self.inner.lock().await;
            if !state.callers.contains_key(&key) {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.connect-owner",
                ));
            }
        }
        // Core first: the link, its generation, and the connection lease are
        // all core-owned. The lease below is the exact string later ops must
        // echo back to the core.
        let lease = self.id("lease");
        let authority = self.ensure_authority().await?;
        let connection = authority
            .connect(&peer_id, &lease, OP_BACKSTOP_TIMEOUT_MS)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let connection_generation = connection.connection_generation.ok_or_else(|| {
            DispatchError::new(
                BleErrorCode::ProtocolMalformed,
                "connection",
                "tauri.connect-generation",
            )
        })?;
        let handle = self.id("connection");
        let connection_id = self.id("connection-id");
        {
            let mut state = self.inner.lock().await;
            let Some(caller_state) = state.callers.get_mut(&key) else {
                // Late validation: the caller was released while the core
                // connected — disconnect through the core instead of
                // stranding a link nobody can address.
                drop(state);
                let _ = authority.disconnect(&peer_id, &lease).await;
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.connect-owner",
                ));
            };
            let owner_lease_id = caller_state.lease_id.clone();
            caller_state.connections.insert(
                handle.clone(),
                CoreConnection {
                    peer_id: peer_id.clone(),
                    lease,
                    connection_id: connection_id.clone(),
                    owner_lease_id: owner_lease_id.clone(),
                    connection_generation: connection_generation.clone(),
                },
            );
            Ok(object([
                ("handle", string(handle)),
                ("connectionId", string(connection_id)),
                ("ownerLeaseId", string(owner_lease_id)),
                ("peerId", string(peer_id)),
                ("connectionGeneration", string(connection_generation)),
            ]))
        }
    }

    /// Tear down one tracked connection through the shared core.
    ///
    /// The mapping detaches first so racing forwarders exit; the core
    /// verdict then decides — clean release, already-gone, or a loud
    /// failure. A loss verdict additionally emits `connection-lifecycle:
    /// disconnected` on the owning stream. Unknown handle with a live
    /// caller is idempotent release; an unknown caller owns nothing.
    async fn disconnect(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "connectionHandle", "tauri.disconnect")?;
        let key = caller_key(caller);
        // Detach the tracked connection first so racing forwarders exit on
        // their next check. Unknown handle with a live caller is idempotent
        // release; an unknown caller is denied (a released caller owns
        // nothing to disconnect).
        let detached = {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(&key).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.disconnect-owner",
                )
            })?;
            let Some(connection) = caller_state.connections.remove(&handle) else {
                return Ok(released());
            };
            validate_connection_identity(
                &payload,
                &connection,
                &caller_state.lease_id,
                "tauri.disconnect",
            )?;
            let lease = (
                caller_state.lease_id.clone(),
                caller_state.lease_generation.clone(),
            );
            let detached = Self::detach_connection_mappings(caller_state, &handle);
            (connection, lease, detached)
        };
        let (connection, lease, (subscriptions, event_tasks)) = detached;
        for subscription in &subscriptions {
            subscription.task.abort();
        }
        for task in event_tasks {
            task.abort();
        }
        // The core verdict decides: clean release, already-gone, or a loud
        // failure — never a dispatcher state probe. Loss observed through
        // the verdict surfaces on the connection-event streams.
        let authority = self.ensure_authority().await?;
        match authority
            .disconnect(&connection.peer_id, &connection.lease)
            .await
        {
            Ok(()) => Ok(released()),
            Err(error) => {
                if matches!(
                    error.code(),
                    ubm_core::contracts::BleErrorCode::ConnectionLost
                        | ubm_core::contracts::BleErrorCode::PeerNotFound
                        | ubm_core::contracts::BleErrorCode::ConnectionNotFound
                ) {
                    let _ = self
                        .emit_connection_lost(
                            &key,
                            (&lease.0, &lease.1),
                            ConnectionEventIdentity {
                                stream_id: &handle,
                                peer_id: &connection.peer_id,
                                connection_id: &connection.connection_id,
                                connection_generation: &connection.connection_generation,
                            },
                        )
                        .await;
                }
                Err(DispatchError::from_core(&error))
            }
        }
    }

    /// Drop every IPC mapping owned by one connection handle and return the
    /// detached forwarders (notification pumps plus connection-event tasks)
    /// for the caller to abort. Radio teardown stays with the core op that
    /// detached them.
    fn detach_connection_mappings(
        caller_state: &mut CallerState,
        connection_handle: &str,
    ) -> (Vec<CoreSubscription>, Vec<TauriJoinHandle<()>>) {
        let subscription_handles = caller_state
            .subscriptions
            .iter()
            .filter_map(|(subscription_handle, subscription)| {
                (subscription.connection_handle == connection_handle)
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
            .retain(|_, database| database.connection_handle != connection_handle);
        let event_tasks = caller_state
            .connection_events
            .iter()
            .filter_map(|(event_handle, event)| {
                (event.connection_handle == connection_handle).then_some(event_handle.clone())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|event_handle| caller_state.connection_events.remove(&event_handle))
            .filter_map(|event| event.task)
            .collect::<Vec<_>>();
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
                BleErrorCode::OwnershipDenied,
                "connection",
                "tauri.connection-events-owner",
            )
        })?;
        if caller_state.connection_events.contains_key(&stream_handle) {
            return Err(DispatchError::new(
                BleErrorCode::ProtocolViolation,
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
                    BleErrorCode::LifecycleInvalidState,
                    "connection",
                    "tauri.connection-events-attachment",
                )
            })?;
            let caller_state = state.callers.get_mut(&key).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.connection-events-ready-owner",
                )
            })?;
            let resource = caller_state
                .connection_events
                .get_mut(&stream_handle)
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::GattStaleHandle,
                        "connection",
                        "tauri.connection-events-ready-handle",
                    )
                })?;
            if resource.active {
                return Err(DispatchError::new(
                    BleErrorCode::LifecycleInvalidState,
                    "connection",
                    "tauri.connection-events-ready-state",
                ));
            }
            resource.active = true;
            resource.sequence = 1;
            // Presence, not a handle: the link itself is core-owned, so
            // readiness only checks that the mapping still resolves. Link
            // sensing is the core's (`DeviceDisconnected` ends core-side
            // peer state in `btleplug_backend`); loss surfaces through the
            // next core verdict, never through a dispatcher-side poll.
            if !caller_state
                .connections
                .contains_key(&resource.connection_handle)
            {
                return Err(DispatchError::new(
                    BleErrorCode::ConnectionStale,
                    "connection",
                    "tauri.connection-events-connection",
                ));
            }
            (
                resource.stream_handle.clone(),
                resource.peer_id.clone(),
                resource.connection_id.clone(),
                resource.connection_generation.clone(),
                caller_state.lease_id.clone(),
                resource.sequence,
                attachment,
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
                Some((&event.4, &event.7)),
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
        // No liveness monitor: the core senses remote loss itself, and the
        // next core verdict against this peer reports it (notably
        // `connection.disconnect`, which already maps loss verdicts to
        // `emit_connection_lost`). A dispatcher-side `is_connected` poll
        // would be the second sensing authority this cutover forbids, so
        // the stream stays open with no task until explicit unsubscribe,
        // disconnect, or release drops it.
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
                BleErrorCode::OwnershipDenied,
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
        let connection = self.connection(caller, &payload, "tauri.discover").await?;
        // Core first: discovery runs in the core and registers the whole
        // tree there; the dispatcher only renders the registered paths into
        // the IPC wire shape. Skipped entries ride the response explicitly
        // (additive `skipped` field the TypeScript side ignores), never
        // silently dropped.
        let authority = self.ensure_authority().await?;
        let report = authority
            .discover(&connection.peer_id, &connection.lease)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let paths = authority
            .discovered_paths(&connection.peer_id)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let database_handle = self.id("database");
        let database_id = self.id("database-id");
        let database_generation = self.id("database-generation");
        let mut service_records = Vec::new();
        let mut characteristic_records = Vec::new();
        let mut descriptor_records = Vec::new();
        let mut characteristic_map = HashMap::new();
        let mut descriptor_map = HashMap::new();
        let mut seen_services = HashSet::new();
        for path in &paths {
            if seen_services.insert((path.service_uuid.clone(), path.service_occurrence)) {
                service_records.push(object([
                    ("uuid", string(path.service_uuid.clone())),
                    ("occurrence", string(path.service_occurrence.to_string())),
                    // The core does not model primary/secondary services, so
                    // the key carries the only truthful contract value the
                    // wire allows; the gap is a documented radio-seam
                    // follow-up (surface `primary` through the boundary).
                    ("primary", IpcValue::Bool(true)),
                    ("includedServices", IpcValue::Array(Vec::new())),
                ]));
            }
            let Some(characteristic_uuid) = path.characteristic_uuid.clone() else {
                continue;
            };
            let characteristic_occurrence = path.characteristic_occurrence.unwrap_or(0);
            let characteristic_handle = self.id("characteristic");
            let selector = CoreSelector {
                service_uuid: path.service_uuid.clone(),
                service_occurrence: Some(path.service_occurrence),
                characteristic_uuid: Some(characteristic_uuid.clone()),
                characteristic_occurrence: Some(characteristic_occurrence),
                descriptor_uuid: None,
                descriptor_occurrence: None,
            };
            characteristic_records.push(object([
                ("handle", string(characteristic_handle.clone())),
                ("serviceUuid", string(path.service_uuid.clone())),
                (
                    "serviceOccurrence",
                    string(path.service_occurrence.to_string()),
                ),
                ("characteristicUuid", string(characteristic_uuid)),
                (
                    "characteristicOccurrence",
                    string(characteristic_occurrence.to_string()),
                ),
                (
                    "properties",
                    core_characteristic_properties(path.properties),
                ),
            ]));
            if let Some(descriptor_uuid) = path.descriptor_uuid.clone() {
                let descriptor_occurrence = path.descriptor_occurrence.unwrap_or(0);
                let descriptor_handle = self.id("descriptor");
                descriptor_records.push(object([
                    ("handle", string(descriptor_handle.clone())),
                    (
                        "characteristicHandle",
                        string(characteristic_handle.clone()),
                    ),
                    ("uuid", string(descriptor_uuid.clone())),
                    ("occurrence", string(descriptor_occurrence.to_string())),
                ]));
                descriptor_map.insert(
                    descriptor_handle,
                    CoreSelector {
                        descriptor_uuid: Some(descriptor_uuid),
                        descriptor_occurrence: Some(descriptor_occurrence),
                        ..selector.clone()
                    },
                );
            }
            characteristic_map.insert(characteristic_handle, selector);
        }
        let skipped = IpcValue::Array(
            report
                .skipped
                .iter()
                .map(|(uuid, code)| {
                    object([
                        ("uuid", string(uuid.clone())),
                        ("code", string(code.clone())),
                    ])
                })
                .collect(),
        );
        {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(&caller_key(caller)).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "gatt",
                    "tauri.discover-owner",
                )
            })?;
            if !expected_lease_matches(caller_state, &payload) {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
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
                CoreDatabase {
                    connection_handle,
                    database_id: database_id.clone(),
                    database_generation: database_generation.clone(),
                    valid: true,
                    characteristics: characteristic_map,
                    descriptors: descriptor_map,
                },
            );
        }
        Ok(object([
            ("schemaVersion", number(2)),
            ("handle", string(database_handle)),
            ("databaseId", string(database_id)),
            ("databaseGeneration", string(database_generation)),
            ("services", IpcValue::Array(service_records)),
            ("characteristics", IpcValue::Array(characteristic_records)),
            ("descriptors", IpcValue::Array(descriptor_records)),
            ("skipped", skipped),
        ]))
    }

    async fn release_database(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        // Validation-only: discovery trees live in the core, so releasing is
        // dropping the transport mapping. Unknown handle with a live caller
        // is idempotent release.
        let handle = required_string(&payload, "databaseHandle", "tauri.database-release")?;
        let key = caller_key(caller);
        let mut state = self.inner.lock().await;
        let caller_state = state.callers.get_mut(&key).ok_or_else(|| {
            DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "gatt",
                "tauri.database-release-owner",
            )
        })?;
        caller_state.databases.remove(&handle);
        Ok(released())
    }

    async fn read(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let (peer_id, selector) = self
            .gatt_target(caller, &payload, "tauri.gatt-read")
            .await?;
        let authority = self.ensure_authority().await?;
        let value = authority
            .read(&peer_id, &selector, OP_BACKSTOP_TIMEOUT_MS)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        Ok(object([("value", IpcValue::Bytes(value))]))
    }

    async fn write(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        bytes: Option<Vec<u8>>,
    ) -> Result<IpcValue, DispatchError> {
        let bytes = bytes.ok_or_else(|| {
            DispatchError::new(BleErrorCode::BytesInvalid, "gatt", "tauri.gatt-write-bytes")
        })?;
        let mode = required_string(&payload, "mode", "tauri.gatt-write-mode")?;
        if mode != "with-response" && mode != "without-response" {
            return Err(DispatchError::new(
                BleErrorCode::ArgumentInvalid,
                "gatt",
                "tauri.gatt-write-mode",
            ));
        }
        let (peer_id, selector) = self
            .gatt_target(caller, &payload, "tauri.gatt-write")
            .await?;
        // Core first: MTU, properties, and the deadline are all core-owned.
        let authority = self.ensure_authority().await?;
        authority
            .write(
                &peer_id,
                &selector,
                bytes.clone(),
                &mode,
                OP_BACKSTOP_TIMEOUT_MS,
            )
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
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
        // Delivery-mode admission mirrors the legacy contract: preferences
        // ride through (enablement stays core-arbitrated), hard requirements
        // the core cannot express fail closed, and unknown modes are
        // rejected. Property support itself is core-enforced inside the op.
        if let Some(delivery_mode) = payload.get("deliveryMode").and_then(as_string) {
            match delivery_mode {
                "require-indication" => {
                    return Err(DispatchError::new(
                        BleErrorCode::CapabilityLimited,
                        "gatt",
                        "tauri.subscribe.indication-selection",
                    ));
                }
                "prefer-notification" | "prefer-indication" | "require-notification" => {}
                _ => {
                    return Err(DispatchError::new(
                        BleErrorCode::ArgumentInvalid,
                        "gatt",
                        "tauri.subscribe.delivery-mode",
                    ));
                }
            }
        }
        let (peer_id, selector, connection_handle) = self
            .gatt_target_with_connection(caller, &payload, "tauri.subscribe")
            .await?;
        // Core first: enablement is core-arbitrated (concurrent subscribers
        // share one physical enable); the consumer below addresses it.
        let authority = self.ensure_authority().await?;
        let consumer = self.id("consumer");
        authority
            .subscribe(&peer_id, &selector, &consumer, OP_BACKSTOP_TIMEOUT_MS)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let handle = self.id("subscription");
        let key = caller_key(caller);
        let expected_lease_id =
            required_string(&payload, "__expectedLeaseId", "tauri.subscribe-lease")?;
        let expected_lease_generation = required_string(
            &payload,
            "__expectedLeaseGeneration",
            "tauri.subscribe-lease",
        )?;
        // Verbatim notification delivery: the pump takes core notifications
        // and emits them unchanged. Sequence numbers are transport-side
        // delivery ordinals, not radio facts.
        let dispatcher = self.clone();
        let forward_key = key.clone();
        let forward_handle = handle.clone();
        let forward_lease_id = expected_lease_id.clone();
        let forward_lease_generation = expected_lease_generation.clone();
        let forward_peer_id = peer_id.clone();
        let forward_selector = selector.clone();
        let forward_consumer = consumer.clone();
        let forward_authority = authority.clone();
        let task = tauri::async_runtime::spawn(async move {
            let mut sequence = 0_u64;
            loop {
                let live = {
                    dispatcher
                        .inner
                        .lock()
                        .await
                        .callers
                        .get(&forward_key)
                        .is_some_and(|caller_state| {
                            caller_state.lease_id == forward_lease_id
                                && caller_state.lease_generation == forward_lease_generation
                                && caller_state.subscriptions.contains_key(&forward_handle)
                        })
                };
                if !live {
                    return;
                }
                match forward_authority
                    .take_notification(&forward_peer_id, &forward_selector, &forward_consumer)
                    .await
                {
                    Ok(Some(value)) => {
                        sequence = sequence.saturating_add(1);
                        let observed_at_monotonic_ms =
                            i64::try_from(dispatcher.started_at.elapsed().as_millis())
                                .unwrap_or(i64::MAX);
                        let failed = dispatcher
                            .emit(
                                &forward_key,
                                Some((&forward_lease_id, &forward_lease_generation)),
                                &forward_handle,
                                object([
                                    ("value", IpcValue::Bytes(value)),
                                    ("delivery", string("unknown")),
                                    ("observedAtMonotonicMs", number(observed_at_monotonic_ms)),
                                    ("sequence", number(sequence as i64)),
                                ]),
                                false,
                            )
                            .await
                            .is_err();
                        if failed {
                            dispatcher
                                .terminal(
                                    &forward_key,
                                    (&forward_lease_id, &forward_lease_generation),
                                    &forward_handle,
                                    "source-failed",
                                    None,
                                )
                                .await
                                .ok();
                            return;
                        }
                    }
                    Ok(None) => tokio::time::sleep(FORWARD_POLL_INTERVAL).await,
                    Err(_) => {
                        // Core-side failure ends delivery; the verdict
                        // surfaces through unsubscribe/release, never as a
                        // guessed stream error.
                        dispatcher
                            .terminal(
                                &forward_key,
                                (&forward_lease_id, &forward_lease_generation),
                                &forward_handle,
                                "source-failed",
                                None,
                            )
                            .await
                            .ok();
                        return;
                    }
                }
            }
        });
        {
            let mut state = self.inner.lock().await;
            let Some(caller_state) = state.callers.get_mut(&key) else {
                // Late validation: the caller was released while the core
                // enabled — unsubscribe through the core instead of
                // stranding an enablement nobody can address.
                task.abort();
                drop(state);
                let _ = authority.unsubscribe(&peer_id, &selector, &consumer).await;
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "gatt",
                    "tauri.subscribe-owner",
                ));
            };
            if !expected_lease_matches(caller_state, &payload) {
                task.abort();
                let _ = authority.unsubscribe(&peer_id, &selector, &consumer).await;
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "gatt",
                    "tauri.subscribe-stale-lease",
                ));
            }
            caller_state.subscriptions.insert(
                handle.clone(),
                CoreSubscription {
                    connection_handle,
                    peer_id,
                    selector,
                    consumer,
                    task,
                },
            );
        }
        let _ = database_handle;
        Ok(object([("handle", string(handle))]))
    }

    async fn unsubscribe(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "subscriptionHandle", "tauri.unsubscribe")?;
        let key = caller_key(caller);
        // Detach first so the racing pump exits on its next check. Unknown
        // handle with a live caller is idempotent release.
        let tracked = {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(&key).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "gatt",
                    "tauri.unsubscribe-owner",
                )
            })?;
            caller_state.subscriptions.remove(&handle)
        };
        if let Some(subscription) = tracked {
            subscription.task.abort();
            // The core verdict decides (last consumer disables the physical
            // CCCD); failures propagate loudly, never swallowed.
            let authority = self.ensure_authority().await?;
            authority
                .unsubscribe(
                    &subscription.peer_id,
                    &subscription.selector,
                    &subscription.consumer,
                )
                .await
                .map_err(|error| DispatchError::from_core(&error))?;
        }
        Ok(released())
    }

    async fn read_descriptor(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let (peer_id, selector) = self
            .descriptor_target(caller, &payload, "tauri.descriptor-read")
            .await?;
        let authority = self.ensure_authority().await?;
        let value = authority
            .read_descriptor(&peer_id, &selector, OP_BACKSTOP_TIMEOUT_MS)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
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
                BleErrorCode::ArgumentInvalid,
                "gatt",
                "tauri.descriptor-write-mode",
            ));
        }
        let bytes = bytes.ok_or_else(|| {
            DispatchError::new(
                BleErrorCode::BytesInvalid,
                "gatt",
                "tauri.descriptor-write-bytes",
            )
        })?;
        let (peer_id, selector) = self
            .descriptor_target(caller, &payload, "tauri.descriptor-write")
            .await?;
        let authority = self.ensure_authority().await?;
        authority
            .write_descriptor(&peer_id, &selector, bytes.clone(), OP_BACKSTOP_TIMEOUT_MS)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
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
        // No core path: the shared radio seam (`RadioBoundary`) exposes no
        // live-link RSSI readout, and reaching around it through a parallel
        // peripheral handle would restore the second radio ownership this
        // cutover removes. The capability snapshot reports `connection:rssi`
        // as unsupported, so compliant callers fail closed before arriving
        // here; direct callers fail loudly here instead of reading a guess.
        let _ = self.connection(caller, &payload, "tauri.rssi").await?;
        Err(DispatchError::new(
            BleErrorCode::CapabilityUnsupported,
            "connection",
            "tauri.rssi",
        ))
    }

    async fn maximum_write_length(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        // Thin fact read under a core verdict: the connection must exist in
        // the caller's mappings (admitted above), then the MTU crosses from
        // the core boundary — the OS-measured value, never synthesized. A
        // withheld MTU fails loudly instead of guessing 23.
        let connection = self
            .connection(caller, &payload, "tauri.maximum-write-length")
            .await?;
        let authority = self.ensure_authority().await?;
        let mtu = authority
            .mtu(&connection.peer_id)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let Some(mtu) = mtu else {
            return Err(DispatchError::new(
                BleErrorCode::CapabilityUnavailable,
                "connection",
                "tauri.maximum-write-length",
            ));
        };
        let bytes = mtu.saturating_sub(3);
        Ok(object([("bytes", number(i64::from(bytes)))]))
    }

    async fn adapter(&self) -> Result<Adapter, DispatchError> {
        self.inner.lock().await.adapter.clone().ok_or_else(|| {
            DispatchError::new(BleErrorCode::AdapterUnavailable, "adapter", "tauri.adapter")
        })
    }

    async fn connection(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
        operation: &str,
    ) -> Result<CoreConnection, DispatchError> {
        let handle = required_string(payload, "connectionHandle", operation)?;
        let state = self.inner.lock().await;
        let caller_state = state.callers.get(&caller_key(caller)).ok_or_else(|| {
            DispatchError::new(BleErrorCode::OwnershipDenied, "connection", operation)
        })?;
        let connection = caller_state.connections.get(&handle).ok_or_else(|| {
            DispatchError::new(BleErrorCode::ConnectionNotFound, "connection", operation)
        })?;
        validate_connection_identity(payload, connection, &caller_state.lease_id, operation)?;
        Ok(CoreConnection {
            peer_id: connection.peer_id.clone(),
            lease: connection.lease.clone(),
            connection_id: connection.connection_id.clone(),
            owner_lease_id: connection.owner_lease_id.clone(),
            connection_generation: connection.connection_generation.clone(),
        })
    }

    /// Resolve `(databaseHandle, characteristicHandle)` to the exact
    /// `(peer_id, selector)` the core addresses, after full identity
    /// admission (database identity, database validity, connection presence,
    /// connection identity). Unknown or stale mappings fail with the same
    /// wire identities as before — only the radio behind them changed.
    async fn gatt_target(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
        operation: &str,
    ) -> Result<(String, CoreSelector), DispatchError> {
        let (peer_id, selector, _) = self
            .gatt_target_with_connection(caller, payload, operation)
            .await?;
        Ok((peer_id, selector))
    }

    async fn gatt_target_with_connection(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
        // Reserved: every failure below pins its own per-path identity, so
        // the caller's operation name never renames a wire error.
        _operation: &str,
    ) -> Result<(String, CoreSelector, String), DispatchError> {
        let database_handle =
            required_string(payload, "databaseHandle", "tauri.characteristic-database")?;
        let characteristic_handle = required_string(
            payload,
            "characteristicHandle",
            "tauri.characteristic-handle",
        )?;
        let state = self.inner.lock().await;
        let caller_state = state.callers.get(&caller_key(caller)).ok_or_else(|| {
            DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "gatt",
                "tauri.characteristic-owner",
            )
        })?;
        let database = caller_state
            .databases
            .get(&database_handle)
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::GattStaleHandle,
                    "gatt",
                    "tauri.characteristic-database",
                )
            })?;
        if !database.valid {
            return Err(DispatchError::new(
                BleErrorCode::GattStaleHandle,
                "gatt",
                "tauri.characteristic-database-generation",
            ));
        }
        validate_database_identity(payload, database, "tauri.characteristic-database")?;
        let selector = database
            .characteristics
            .get(&characteristic_handle)
            .cloned()
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::GattNotFound,
                    "gatt",
                    "tauri.characteristic-handle",
                )
            })?;
        let connection = caller_state
            .connections
            .get(&database.connection_handle)
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::ConnectionStale,
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
        Ok((
            connection.peer_id.clone(),
            selector,
            database.connection_handle.clone(),
        ))
    }

    async fn descriptor_target(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
        // Reserved: every failure below pins its own per-path identity, so
        // the caller's operation name never renames a wire error.
        _operation: &str,
    ) -> Result<(String, CoreSelector), DispatchError> {
        let database_handle =
            required_string(payload, "databaseHandle", "tauri.descriptor-database")?;
        let descriptor_handle =
            required_string(payload, "descriptorHandle", "tauri.descriptor-handle")?;
        let state = self.inner.lock().await;
        let caller_state = state.callers.get(&caller_key(caller)).ok_or_else(|| {
            DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "gatt",
                "tauri.descriptor-owner",
            )
        })?;
        let database = caller_state
            .databases
            .get(&database_handle)
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::GattStaleHandle,
                    "gatt",
                    "tauri.descriptor-database",
                )
            })?;
        if !database.valid {
            return Err(DispatchError::new(
                BleErrorCode::GattStaleHandle,
                "gatt",
                "tauri.descriptor-database-generation",
            ));
        }
        validate_database_identity(payload, database, "tauri.descriptor-database")?;
        let selector = database
            .descriptors
            .get(&descriptor_handle)
            .cloned()
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::GattNotFound,
                    "gatt",
                    "tauri.descriptor-handle",
                )
            })?;
        let connection = caller_state
            .connections
            .get(&database.connection_handle)
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::ConnectionStale,
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
        Ok((connection.peer_id.clone(), selector))
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
            DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "ipc",
                "tauri.event-ack-owner",
            )
        })?;
        validate_lease(caller_state, &lease, "tauri.event-ack-lease")?;
        if !caller_state.pending_events.remove(&event_id) {
            return Err(DispatchError::new(
                BleErrorCode::ProtocolViolation,
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
                DispatchError::new(BleErrorCode::OwnershipDenied, "ipc", "tauri.release-owner")
            })?;
            validate_lease(caller_state, &lease, "tauri.release-lease")?;
        }
        let cleanup = self.release(&key).await;
        Ok(object([("kind", string("release")), ("cleanup", cleanup)]))
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
                DispatchError::new(BleErrorCode::OwnershipDenied, "stream", "tauri.event-owner")
            })?;
            if expected_lease.is_some_and(|lease| {
                caller_state.lease_id != lease.0 || caller_state.lease_generation != lease.1
            }) {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "stream",
                    "tauri.event-stale-lease",
                ));
            }
            if caller_state.pending_events.len() >= MAX_PENDING_EVENTS {
                if drop_if_full {
                    return Err(DispatchError::new(
                        BleErrorCode::StreamQuota,
                        "stream",
                        "tauri.event-retention",
                    ));
                }
                return Err(DispatchError::new(
                    BleErrorCode::StreamQuota,
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
            DispatchError::new(
                BleErrorCode::PlatformTransport,
                "stream",
                "tauri.event-send",
            )
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
                    BleErrorCode::LifecycleInvalidState,
                    "connection",
                    "tauri.connection-events-attachment",
                )
            })?;
            let caller = state.callers.get_mut(caller_key).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.connection-events-owner",
                )
            })?;
            if caller.lease_id != expected_lease.0 || caller.lease_generation != expected_lease.1 {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.connection-events-stale-lease",
                ));
            }
            let resource = caller
                .connection_events
                .get_mut(identity.stream_id)
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::GattStaleHandle,
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
                    None,
                )
                .await
            {
                Ok(()) => break Ok(()),
                Err(error) if error.code == BleErrorCode::OwnershipDenied => break Err(error),
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

    async fn terminal(
        &self,
        caller_key: &str,
        expected_lease: (&str, &str),
        stream_id: &str,
        reason: &str,
        error: Option<&DispatchError>,
    ) -> Result<(), DispatchError> {
        let (sink, lease_id, lease_generation, event_id) = {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(caller_key).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "stream",
                    "tauri.terminal-owner",
                )
            })?;
            if caller_state.lease_id != expected_lease.0
                || caller_state.lease_generation != expected_lease.1
            {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
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
        let mut terminal_item = object([("kind", string("terminal")), ("reason", string(reason))]);
        if let (Some(error), IpcValue::Object(item)) = (error, &mut terminal_item) {
            item.insert("error".to_owned(), error.normalized_error());
        }
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
            ("item", terminal_item),
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
            DispatchError::new(
                BleErrorCode::PlatformTransport,
                "stream",
                "tauri.terminal-send",
            )
            .platform(error.to_string())
        })
    }

    async fn release(&self, key: &str) -> IpcValue {
        let caller = self.inner.lock().await.callers.remove(key);
        if let Some(mut caller) = caller {
            let cleanup = self.settle_caller(&mut caller).await;
            if !is_released(&cleanup) {
                let mut state = self.inner.lock().await;
                state.callers.entry(key.to_owned()).or_insert(caller);
            }
            cleanup
        } else {
            released()
        }
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

    /// Tear down everything one caller owned, through the shared core.
    ///
    /// Every radio verdict here is core-made — scan stop, unsubscribe, and
    /// disconnect all execute the admitted authority — while the dispatcher
    /// only drops its transport mappings. Already-gone peers release
    /// quietly (the core treats loss as released); anything else the core
    /// reports becomes an explicit cleanup failure, never silence.
    async fn settle_caller(&self, caller: &mut CallerState) -> IpcValue {
        let mut failures = Vec::new();
        for tracked in caller.operations.values() {
            tracked.token.cancel();
        }
        caller.operations.clear();
        for resource in caller.connection_events.values_mut() {
            if let Some(task) = resource.task.take() {
                task.abort();
            }
        }
        caller.connection_events.clear();
        // The authority is admitted once: every teardown below settles
        // through it. A missing authority fails loudly as one release
        // failure while the mappings still drop — never a silent legacy
        // radio teardown.
        let authority = match self.ensure_authority().await {
            Ok(authority) => Some(authority),
            Err(error) => {
                failures.push(cleanup_failure(
                    "release",
                    "tauri.release.authority",
                    format!(
                        "{}:{}:{}",
                        error.code.as_str(),
                        error.domain,
                        error.operation
                    ),
                ));
                None
            }
        };
        if let Some(scan) = caller.scan.take() {
            scan.task.abort();
            if let Some(authority) = &authority {
                if let Err(error) = authority.stop_scan().await {
                    failures.push(cleanup_failure(
                        "scan",
                        "tauri.release.scan",
                        core_failure_message(&error),
                    ));
                }
            }
        }
        let subscriptions = std::mem::take(&mut caller.subscriptions);
        for subscription in subscriptions.values() {
            subscription.task.abort();
        }
        if let Some(authority) = &authority {
            for (handle, subscription) in subscriptions {
                if let Err(error) = authority
                    .unsubscribe(
                        &subscription.peer_id,
                        &subscription.selector,
                        &subscription.consumer,
                    )
                    .await
                {
                    failures.push(cleanup_failure(
                        "subscription",
                        "tauri.release.subscription",
                        format!("{handle}: {}", core_failure_message(&error)),
                    ));
                }
            }
        }
        let connections = std::mem::take(&mut caller.connections);
        caller.databases.clear();
        if let Some(authority) = &authority {
            for (handle, connection) in connections {
                match authority
                    .disconnect(&connection.peer_id, &connection.lease)
                    .await
                {
                    Ok(()) => {}
                    Err(error) if is_released_loss(&error) => {}
                    Err(error) => failures.push(cleanup_failure(
                        "connection",
                        "tauri.release.connection",
                        format!("{handle}: {}", core_failure_message(&error)),
                    )),
                }
            }
        }
        cleanup_record(failures)
    }
}

/// Render one shared-core failure for a cleanup receipt: the frozen
/// identity triple plus the transport detail, so a failed release still
/// says exactly what the core reported — never a bare operation name.
fn core_failure_message(error: &ubm_desktop::DesktopError) -> String {
    match error.detail() {
        Some(detail) => format!(
            "{}:{}:{} ({})",
            error.code_str(),
            error.domain().as_str(),
            error.operation(),
            detail
        ),
        None => format!(
            "{}:{}:{}",
            error.code_str(),
            error.domain().as_str(),
            error.operation()
        ),
    }
}

/// Core verdicts that already mean "released": a lost, unfound, or unknown
/// connection needs no teardown — release treats it as gone instead of
/// recording a failure for a peer that is provably down.
fn is_released_loss(error: &ubm_desktop::DesktopError) -> bool {
    matches!(
        error.code(),
        ubm_core::contracts::BleErrorCode::ConnectionLost
            | ubm_core::contracts::BleErrorCode::PeerNotFound
            | ubm_core::contracts::BleErrorCode::ConnectionNotFound
    )
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
    operations: &HashMap<String, TrackedOperation>,
    completed: &mut HashMap<String, Instant>,
    correlation: &str,
    command: &str,
    now: Instant,
) -> Result<(), DispatchError> {
    prune_completed_correlations(completed, now);
    if operations.contains_key(correlation) || completed.contains_key(correlation) {
        return Err(DispatchError::new(
            BleErrorCode::ProtocolViolation,
            "ipc",
            "tauri.correlation-replay",
        ));
    }
    if is_cleanup_command(command) {
        return Ok(());
    }
    if operations.len() >= MAX_CORRELATIONS {
        return Err(
            DispatchError::new(BleErrorCode::StreamQuota, "ipc", "tauri.correlation-busy")
                .retryable(),
        );
    }
    Ok(())
}

fn remember_completed_correlation(
    operations: &mut HashMap<String, TrackedOperation>,
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
        return Err(DispatchError::new(
            BleErrorCode::OwnershipDenied,
            "ipc",
            operation,
        ));
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
        DispatchError::new(BleErrorCode::AdapterUnavailable, "adapter", "tauri.manager")
            .platform(error.to_string())
    })?;
    let adapters = manager.adapters().await.map_err(|error| {
        DispatchError::new(
            BleErrorCode::AdapterUnavailable,
            "adapter",
            "tauri.adapters",
        )
        .platform(error.to_string())
    })?;
    if adapters.is_empty() {
        return Err(DispatchError::new(
            BleErrorCode::AdapterUnavailable,
            "adapter",
            "tauri.adapters-empty",
        ));
    }
    let mut candidates = Vec::with_capacity(adapters.len());
    for adapter in adapters {
        let info = adapter.adapter_info().await.map_err(|error| {
            DispatchError::new(
                BleErrorCode::AdapterUnavailable,
                "adapter",
                "tauri.adapter-info",
            )
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
                    BleErrorCode::AdapterSelectionRequired,
                    "adapter",
                    "tauri.adapter-selection",
                )
            })?,
        None if candidates.len() == 1 => candidates.remove(0),
        None => {
            return Err(DispatchError::new(
                BleErrorCode::AdapterAmbiguous,
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

/// Dispatcher-side scan matching, kept as unit-pinned semantics for the
/// transport's fails-closed filter contract. No live path calls these: scan
/// admission crosses into the core and view shaping stays TypeScript-side.
#[cfg(test)]
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
                    .is_none_or(|prefix| data.starts_with(prefix))
            })
    })
}

#[cfg(test)]
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

/// Verbatim core observation mapping: a [`PeerSnapshot`] becomes the exact
/// IPC observation wire shape (`peerId`, `localName`, `rssi`,
/// `txPowerLevel`, `serviceUuids`, `manufacturerData`, `serviceData`) with
/// no filtering, merging, or re-sampling. The radio facts cross unchanged;
/// delivery policy is the core's.
fn core_scan_observation(snapshot: &ubm_desktop::PeerSnapshot) -> IpcValue {
    let manufacturer_data = snapshot
        .manufacturer_data
        .iter()
        .map(|section| {
            object([
                ("companyId", number(i64::from(section.company_id))),
                ("data", IpcValue::Bytes(section.payload.clone())),
            ])
        })
        .collect();
    let service_data = snapshot
        .service_data
        .iter()
        .map(|section| {
            object([
                ("uuid", string(section.uuid.clone())),
                ("data", IpcValue::Bytes(section.payload.clone())),
            ])
        })
        .collect();
    object([
        ("peerId", string(snapshot.id.clone())),
        (
            "localName",
            snapshot.local_name.clone().map_or(IpcValue::Null, string),
        ),
        (
            "rssi",
            snapshot
                .rssi
                .map_or(IpcValue::Null, |value| number(i64::from(value))),
        ),
        (
            "txPowerLevel",
            snapshot
                .tx_power_level
                .map_or(IpcValue::Null, |value| number(i64::from(value))),
        ),
        (
            "serviceUuids",
            IpcValue::Array(
                snapshot
                    .service_uuids
                    .iter()
                    .map(|uuid| string(uuid.clone()))
                    .collect(),
            ),
        ),
        ("manufacturerData", IpcValue::Array(manufacturer_data)),
        ("serviceData", IpcValue::Array(service_data)),
    ])
}

/// Core GATT property bits (`GATT_PROP_*` in `ubm-core`) rendered as the IPC
/// property-name array. Same names as the legacy btleplug mapping; only the
/// source changed (core-registered bits, not a peripheral handle read).
fn core_characteristic_properties(bits: u8) -> IpcValue {
    let mut values = Vec::new();
    for (flag, name) in [
        (ubm_core::central::GATT_PROP_READ, "read"),
        (ubm_core::central::GATT_PROP_WRITE, "write"),
        (
            ubm_core::central::GATT_PROP_WRITE_NO_RESPONSE,
            "write-without-response",
        ),
        (ubm_core::central::GATT_PROP_NOTIFY, "notify"),
        (ubm_core::central::GATT_PROP_INDICATE, "indicate"),
    ] {
        if bits & flag != 0 {
            values.push(string(name));
        }
    }
    IpcValue::Array(values)
}

#[cfg(test)]
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
        .map_err(|_| DispatchError::new(BleErrorCode::ScanFilterInvalid, "scan", operation))
}

#[cfg(test)]
struct ManufacturerFilter {
    company_id: u16,
    data_prefix: Option<Vec<u8>>,
}

/// F01 shared-core identity reported at bootstrap: the linked `ubm-core`
/// contract revision plus this plugin's implementation version. The
/// TypeScript factory admits the exact pinned pair and fails loudly
/// otherwise, so a silently substituted (non-candidate) native host cannot
/// serve traffic.
fn core_identity() -> IpcValue {
    object([
        (
            "contractRevision",
            string(ubm_core::contracts::CONTRACT_REVISION),
        ),
        ("implementationVersion", string(env!("CARGO_PKG_VERSION"))),
    ])
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
            BleErrorCode::ProtocolMalformed,
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
            BleErrorCode::ProtocolMalformed,
            "ipc",
            "tauri.bootstrap-offer-range",
        ));
    }
    let minimum = offered_version(&range, "minimum", axis)?;
    let maximum = offered_version(&range, "maximum", axis)?;
    if minimum > maximum {
        return Err(DispatchError::new(
            BleErrorCode::ProtocolMalformed,
            "ipc",
            "tauri.bootstrap-offer-range-order",
        ));
    }
    if local_value < minimum || local_value > maximum {
        return Err(DispatchError::new(
            BleErrorCode::ProtocolIncompatible,
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
            BleErrorCode::ProtocolMalformed,
            "ipc",
            "tauri.bootstrap-offer-version",
        ));
    }
    match version.get("value") {
        Some(IpcValue::Number(value)) => {
            value.as_i64().filter(|value| *value >= 0).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::ProtocolMalformed,
                    "ipc",
                    "tauri.bootstrap-offer-version",
                )
            })
        }
        _ => Err(DispatchError::new(
            BleErrorCode::ProtocolMalformed,
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
                ("code", string(BleErrorCode::PlatformFailure.as_str())),
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
    connection: &CoreConnection,
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
            BleErrorCode::ProtocolViolation,
            "connection",
            operation,
        ))
    }
}

fn validate_database_identity(
    payload: &BTreeMap<String, IpcValue>,
    database: &CoreDatabase,
    operation: &str,
) -> Result<(), DispatchError> {
    let matches = required_string(payload, "databaseId", operation)? == database.database_id
        && required_string(payload, "databaseGeneration", operation)?
            == database.database_generation
        && required_string(payload, "connectionHandle", operation)? == database.connection_handle;
    if matches {
        Ok(())
    } else {
        Err(DispatchError::new(
            BleErrorCode::ProtocolViolation,
            "gatt",
            operation,
        ))
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
        _ => Err(DispatchError::new(
            BleErrorCode::ProtocolMalformed,
            "ipc",
            operation,
        )),
    }
}

fn required_value<'a>(
    object: &'a BTreeMap<String, IpcValue>,
    key: &str,
    operation: impl Into<String>,
) -> Result<&'a IpcValue, DispatchError> {
    object
        .get(key)
        .ok_or_else(|| DispatchError::new(BleErrorCode::ProtocolMalformed, "ipc", operation))
}

fn required_string(
    object: &BTreeMap<String, IpcValue>,
    key: &str,
    operation: impl Into<String>,
) -> Result<String, DispatchError> {
    match object.get(key) {
        Some(IpcValue::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(DispatchError::new(
            BleErrorCode::ProtocolMalformed,
            "ipc",
            operation,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        characteristic_properties, core_identity, negotiate_ipc_versions, object, released,
        scan_properties_match_optional, string, DispatchError,
    };
    use btleplug::api::CharPropFlags;
    use ubm_core::contracts::BleErrorCode;

    #[test]
    fn capability_projection_is_data_only() {
        assert_eq!(
            characteristic_properties(CharPropFlags::READ | CharPropFlags::NOTIFY),
            super::IpcValue::Array(vec![string("read"), string("notify")])
        );
    }

    #[test]
    fn native_dispatch_error_keeps_structured_platform_diagnostics_for_terminals() {
        let error = DispatchError::new(
            BleErrorCode::PlatformTransport,
            "stream",
            "tauri.event-send",
        )
        .platform("native channel closed");
        let super::IpcValue::Object(record) = error.normalized_error() else {
            panic!("normalized error must be an object");
        };

        assert_eq!(record.get("code"), Some(&string("platform.transport")));
        assert_eq!(record.get("domain"), Some(&string("stream")));
        assert_eq!(record.get("operation"), Some(&string("tauri.event-send")));
        let Some(super::IpcValue::Object(platform)) = record.get("platform") else {
            panic!("platform diagnostics must be preserved");
        };
        assert_eq!(platform.get("domain"), Some(&string("btleplug")));
        assert_eq!(platform.get("code"), Some(&string("native-error")));
        assert_eq!(
            platform.get("safeMessage"),
            Some(&string("native channel closed"))
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
        assert_eq!(error.code, BleErrorCode::ProtocolIncompatible);

        let mut malformed = current_offer();
        malformed.remove("traceFormat");
        let error = negotiate_ipc_versions(&malformed).expect_err("missing axes must fail");
        assert_eq!(error.code, BleErrorCode::ProtocolMalformed);
    }

    #[test]
    fn bootstrap_core_identity_reports_the_linked_contract() {
        // F01: the revision is the LINKED ubm-core value, never a
        // plugin-spelled string; a drifted literal fails here.
        let identity = core_identity();
        let super::IpcValue::Object(fields) = &identity else {
            panic!("core identity must be an object");
        };
        assert_eq!(
            fields.get("contractRevision"),
            Some(&string(ubm_core::contracts::CONTRACT_REVISION))
        );
        assert_eq!(
            fields.get("implementationVersion"),
            Some(&string(env!("CARGO_PKG_VERSION")))
        );
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
        assert_eq!(error.code, BleErrorCode::ProtocolViolation);
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

    fn tracked_operation() -> super::TrackedOperation {
        super::TrackedOperation {
            token: tokio_util::sync::CancellationToken::new(),
            core_operation_id: None,
        }
    }

    #[test]
    fn in_flight_duplicate_correlation_is_still_rejected() {
        let mut operations = std::collections::HashMap::new();
        operations.insert("c1".to_owned(), tracked_operation());
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
        assert_eq!(error.code, BleErrorCode::ProtocolViolation);
        assert_eq!(error.operation, "tauri.correlation-replay");
    }

    #[test]
    fn live_operation_exhaustion_is_backpressure_not_protocol_violation() {
        let mut operations = std::collections::HashMap::new();
        let mut completed = std::collections::HashMap::new();
        let now = std::time::Instant::now();
        for index in 0..super::MAX_CORRELATIONS {
            operations.insert(format!("live-{index}"), tracked_operation());
        }
        let error = super::admit_caller_correlation(
            &operations,
            &mut completed,
            "overflow",
            "gatt.read",
            now,
        )
        .expect_err("live exhaustion must reject new ordinary work");
        assert_eq!(error.code, BleErrorCode::StreamQuota);
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
            operations.insert(format!("live-{index}"), tracked_operation());
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

    // R03 cutover: BLE scheduling executes the shared core. These tests
    // drive the live dispatcher over `DesktopCore` on a scripted `FakeRadio`
    // boundary: connect verdicts equal a direct core drive bit-for-bit,
    // scans start headless, and a shut-down core fails loudly instead of
    // falling back to silent legacy radio ownership.
    mod core_authority_cutover {
        use std::collections::BTreeMap;
        use std::sync::Arc;

        use ubm_desktop::FakeRadio;

        use crate::desktop_core::{CoreAuthority, DesktopCore};
        use crate::{AuthenticatedCaller, IpcValue};

        use super::super::{
            caller_key, object, string, Attachment, BtleplugDispatcher, CallerState, IpcEventSink,
        };

        const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";

        fn caller() -> AuthenticatedCaller {
            AuthenticatedCaller::new("test-app".to_owned(), "main".to_owned())
        }

        fn test_caller_state() -> CallerState {
            CallerState {
                lease_id: "lease-1".to_owned(),
                lease_generation: "generation-1".to_owned(),
                versions: object([]),
                event_sink: IpcEventSink::new(tauri::ipc::Channel::new(|_| Ok(()))),
                scan: None,
                connections: std::collections::HashMap::new(),
                databases: std::collections::HashMap::new(),
                subscriptions: std::collections::HashMap::new(),
                connection_events: std::collections::HashMap::new(),
                operations: std::collections::HashMap::new(),
                completed_correlations: std::collections::HashMap::new(),
                pending_events: std::collections::HashSet::new(),
            }
        }

        /// Staged transport attachment so `ensure_adapter` short-circuits:
        /// attachment identity is a transport fact, not BLE work, and tests
        /// must not touch radio hardware.
        fn test_attachment() -> Attachment {
            Attachment {
                attachment_id: "attachment-test".to_owned(),
                backend_instance_id: "backend-test".to_owned(),
                backend_generation: "generation-test".to_owned(),
                adapter_id: "adapter-test".to_owned(),
                adapter_name: "fake".to_owned(),
                adapter_generation: "adapter-generation-test".to_owned(),
            }
        }

        fn lease_payload() -> BTreeMap<String, IpcValue> {
            let mut payload = BTreeMap::new();
            payload.insert("__expectedLeaseId".to_owned(), string("lease-1"));
            payload.insert(
                "__expectedLeaseGeneration".to_owned(),
                string("generation-1"),
            );
            payload
        }

        /// Canonical empty normalized scan query (`{"anyOf":null,"exclude":null}`)
        /// with a valid digest (FNV-1a over UTF-16, `scan-query-v1:` prefixed).
        fn empty_scan_query() -> IpcValue {
            let canonical = "{\"anyOf\":null,\"exclude\":null}";
            let mut hash = 0xcbf29ce484222325_u64;
            for code_unit in canonical.encode_utf16() {
                hash ^= u64::from(code_unit);
                hash = hash.wrapping_mul(0x100000001b3_u64);
            }
            let mut fields = BTreeMap::new();
            fields.insert("anyOf".to_owned(), IpcValue::Null);
            fields.insert("exclude".to_owned(), IpcValue::Null);
            fields.insert(
                "digest".to_owned(),
                string(format!("scan-query-v1:{hash:016x}")),
            );
            IpcValue::Object(fields)
        }

        fn dispatcher_over_fake_radio() -> BtleplugDispatcher {
            let authority: Arc<dyn CoreAuthority> = Arc::new(tokio::sync::Mutex::new(
                DesktopCore::new(FakeRadio::new(), "tauri-test"),
            ));
            BtleplugDispatcher::with_core_authority(authority)
        }

        async fn dispatcher_with_caller() -> (BtleplugDispatcher, AuthenticatedCaller) {
            let dispatcher = dispatcher_over_fake_radio();
            let caller = caller();
            {
                let mut state = dispatcher.inner.lock().await;
                state.attachment = Some(test_attachment());
                state
                    .callers
                    .insert(caller_key(&caller), test_caller_state());
            }
            (dispatcher, caller)
        }

        /// BLE work executes the core: connecting headless succeeds through
        /// the admitted authority and records the core-owned mapping. The
        /// legacy path cannot do this: it reads `adapter.peripherals()`
        /// from raw btleplug first, which fails without hardware.
        #[tokio::test]
        async fn core_authority_connects_without_radio_hardware() {
            let (dispatcher, caller) = dispatcher_with_caller().await;
            let mut payload = lease_payload();
            payload.insert("peerId".to_owned(), string("peer-unknown"));

            let response = dispatcher
                .connect(&caller, payload)
                .await
                .expect("core connects without hardware");
            let IpcValue::Object(fields) = response else {
                panic!("connect must answer an object");
            };
            let handle = fields
                .get("handle")
                .and_then(|value| match value {
                    IpcValue::String(handle) => Some(handle.clone()),
                    _ => None,
                })
                .expect("core-backed connect must mint a handle");
            let state = dispatcher.inner.lock().await;
            let stored = state
                .callers
                .get(&caller_key(&caller))
                .and_then(|caller_state| caller_state.connections.get(&handle))
                .expect("connect must record the core-owned mapping");
            assert_eq!(stored.peer_id, "peer-unknown");
        }

        /// Core verdicts cross verbatim: a second scan owner is refused by
        /// core arbitration, and the dispatcher's failure equals a direct
        /// core drive — code, domain, and operation.
        #[tokio::test]
        async fn core_arbitration_verdicts_cross_verbatim() {
            let (dispatcher, caller_a) = dispatcher_with_caller().await;
            let caller_b = AuthenticatedCaller::new("test-app".to_owned(), "second".to_owned());
            {
                let mut state = dispatcher.inner.lock().await;
                state
                    .callers
                    .insert(caller_key(&caller_b), test_caller_state());
            }
            let mut first = lease_payload();
            first.insert("query".to_owned(), empty_scan_query());
            let started = dispatcher
                .start_scan(&caller_a, "corr-scan-a", first)
                .await
                .expect("first owner starts through the core");
            let IpcValue::Object(started_fields) = started else {
                panic!("scan.start must answer an object");
            };
            let first_handle = started_fields
                .get("handle")
                .and_then(|value| match value {
                    IpcValue::String(handle) => Some(handle.clone()),
                    _ => None,
                })
                .unwrap_or_default();

            let mut second = lease_payload();
            second.insert("query".to_owned(), empty_scan_query());
            let error = dispatcher
                .start_scan(&caller_b, "corr-scan-b", second)
                .await
                .expect_err("second owner is refused by core arbitration");
            let (code, domain, operation) = error.identity();

            let direct = {
                let executor = ubm_desktop::executor::desktop_runtime();
                let _guard = executor.enter();
                let mut core = DesktopCore::new(FakeRadio::new(), "tauri-direct");
                core.start_scan("owner-a", &[], 5000).await.expect("first");
                core.start_scan("owner-b", &[], 5000)
                    .await
                    .expect_err("direct core verdict")
            };
            let (direct_code, direct_domain, _, _) = crate::desktop_core::error_identity(&direct);
            assert_eq!(
                code, direct_code,
                "dispatcher verdict must equal the core verdict"
            );
            assert_eq!(
                domain, direct_domain,
                "dispatcher domain must equal the core domain"
            );
            assert_eq!(code, "scan.already-active");
            assert_eq!(operation, direct.operation());

            let mut stop = lease_payload();
            stop.insert("scanHandle".to_owned(), string(first_handle));
            dispatcher
                .stop_scan(&caller_a, stop)
                .await
                .expect("first scan stops");
        }

        /// Scan admission and execution run through the core without touching
        /// raw btleplug: start succeeds headless and mints a handle.
        #[tokio::test]
        async fn core_authority_starts_scans_without_radio_hardware() {
            let (dispatcher, caller) = dispatcher_with_caller().await;
            let mut payload = lease_payload();
            payload.insert("query".to_owned(), empty_scan_query());

            let response = dispatcher
                .start_scan(&caller, "corr-scan-1", payload)
                .await
                .expect("core admits scans without hardware");
            let IpcValue::Object(fields) = response else {
                panic!("scan.start must answer an object");
            };
            let handle = fields.get("handle").and_then(|value| match value {
                IpcValue::String(handle) => Some(handle.as_str()),
                _ => None,
            });
            assert!(
                handle.is_some_and(|handle| !handle.is_empty()),
                "core-backed scan.start must mint a scan handle"
            );

            let mut stop = lease_payload();
            stop.insert("scanHandle".to_owned(), string(handle.unwrap_or_default()));
            let stopped = dispatcher.stop_scan(&caller, stop).await;
            assert!(
                stopped.is_ok(),
                "core-backed scan.stop must release the scan"
            );
            let _ = HRM_SERVICE;
        }

        /// A missing core fails loudly with `adapter.unavailable`, never by
        /// falling back to silent legacy radio ownership. `connect` always
        /// reaches the authority (unlike idempotent release paths), so it
        /// is the proof op.
        #[tokio::test]
        async fn missing_core_fails_loudly_never_silent_legacy() {
            let (dispatcher, caller) = dispatcher_with_caller().await;
            dispatcher.authority_shutdown().await;
            let mut payload = lease_payload();
            payload.insert("peerId".to_owned(), string("peer-1"));

            let error = dispatcher
                .connect(&caller, payload)
                .await
                .expect_err("post-shutdown ops must fail loudly");
            let (code, domain, operation) = error.identity();
            assert_eq!(code, "adapter.unavailable");
            assert_eq!(domain, "adapter");
            assert_eq!(operation, "tauri.core-shutdown");
        }
    }
}
