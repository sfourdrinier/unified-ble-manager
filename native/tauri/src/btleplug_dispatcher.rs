use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicI64, AtomicU64, Ordering},
        Arc, Mutex as SyncMutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use btleplug::api::CharPropFlags;
use serde_json::Number;
use tauri::async_runtime::JoinHandle as TauriJoinHandle;
use tokio::sync::{broadcast, watch, Mutex};
use ubm_core::contracts::{AttachmentTuple, BleErrorCode, CommitState};
use ubm_desktop::{
    AdapterAuthorization, AdapterAvailability, AdapterPowerState, Budget, CancelAck, CancelRequest,
    CentralProfile, CompletionOutcome, DeliveryMode, DesktopCentral, DesktopError,
    InvalidationCause, LifecycleEvent, LifecycleKind, NotificationPoll, ObservedDelivery,
    OpControl, OpTicket, OperationId, PlatformDetail, PlatformValue, Retryability,
    ScanTerminalEvent, ShutdownReport,
};
use uuid::Uuid;

use crate::capabilities;
use crate::desktop_core::{CoreAuthority, CoreSelector};
use crate::scan_plan::{decode_normalized_scan_query, diagnostic_scan_plan};
use crate::ATTACH_REQUEST_KIND;
use crate::{AuthenticatedCaller, DispatchFuture, IpcDispatcher, IpcEventSink, IpcValue};

const MAX_PENDING_EVENTS: usize = 256;
const MAX_CORRELATIONS: usize = 256;
const COMPLETED_CORRELATION_TTL: Duration = Duration::from_secs(30);
/// Delivery pacing between core polls (scan observations and notification
/// forwarders). This paces delivery only: admission, deadlines, overflow,
/// and teardown all stay core-owned.
const FORWARD_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Largest integer JavaScript represents exactly (`Number.MAX_SAFE_INTEGER`):
/// the upper bound of a well-formed `budgetMs`.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

fn btleplug_runtime() -> tokio::runtime::Handle {
    // One shared desktop executor per process, owned by `ubm-desktop`: the
    // central and its radio open and run there, never on Tauri's runtime.
    ubm_desktop::executor::desktop_runtime()
}

#[derive(Clone, Debug, Default)]
pub struct BtleplugDispatcherOptions {
    /// The adapter the shared central runs on, by its selectable identity
    /// (`ubm_desktop::btleplug_backend::list_adapters` labels; BlueZ `hci0`,
    /// the Windows device id, `CoreBluetooth` on macOS). `None` selects the
    /// sole adapter; with several adapters and no name the open fails
    /// `adapter.ambiguous`, and a name that matches none fails
    /// `adapter.selection-required` — never a silent first pick.
    pub adapter_id: Option<String>,
}

/// Production Tauri dispatcher: IPC transport over the shared Rust core.
///
/// BLE scheduling executes one shared [`DesktopCentral`] through
/// [`CoreAuthority`]; this struct owns no scan policy, no retry, no timeout
/// timers, and no ownership generations — only IPC envelope admission,
/// caller leases, transport-handle mapping, and event delivery. Attachment
/// identity and `adapter.state` facts come from that same central: the
/// plugin opens no radio of its own (finding 43).
#[derive(Clone)]
pub struct BtleplugDispatcher {
    inner: Arc<Mutex<DispatcherState>>,
    bootstrap_admission: Arc<Mutex<()>>,
    next_id: Arc<AtomicU64>,
    next_internal_id: Arc<AtomicU64>,
    next_revocation: Arc<AtomicU64>,
    started_at: Arc<Instant>,
    revoked_callers: Arc<SyncMutex<HashMap<String, u64>>>,
    authority: Arc<Mutex<AuthoritySlot>>,
    lifecycle_pump: Arc<SyncMutex<Option<TauriJoinHandle<()>>>>,
}

type AuthorityOpenFuture =
    Pin<Box<dyn Future<Output = Result<Arc<dyn CoreAuthority>, DispatchError>> + Send>>;

/// Opens the scheduling authority exactly once per dispatcher.
type AuthorityOpener = Arc<dyn Fn() -> AuthorityOpenFuture + Send + Sync>;

/// Which scheduling authority the dispatcher serves. `Unopened` opens the
/// production core (btleplug radio) on the first BLE op and fails loudly
/// (`adapter.unavailable`) where no radio exists — never legacy direct
/// ownership. `ShutDown` refuses loudly after an explicit shutdown.
enum AuthoritySlot {
    Unopened(AuthorityOpener),
    Open(Arc<dyn CoreAuthority>),
    ShutDown,
}

struct DispatcherState {
    /// The adapter's display label, read once through the central's
    /// boundary: a reset keeps the adapter (finding 57), only its
    /// generations move.
    adapter_name: Option<String>,
    callers: HashMap<String, CallerState>,
    /// Core resources admitted for a caller that could not take ownership
    /// (released, lease replaced, duplicate) and whose compensating
    /// release failed (PR210-08). Retried and reported by the owning
    /// caller key's next release and by authority shutdown; never dropped
    /// silently.
    orphan_debt: Vec<OrphanDebt>,
    /// Attachments an adapter reset replaced (IPC protocol 4). Work naming
    /// one is refused `backend.reset`, releases excepted; renderers route
    /// under the attachment the dispatcher rebound them to and announced.
    replaced_attachment_ids: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// The attachment this caller bound at attach. Every route must name
    /// it; once an adapter reset replaced it, only releases pass (finding
    /// 57) and everything else fails `backend.reset`.
    attachment: Attachment,
    event_sink: IpcEventSink,
    /// Set when a release begins. A retired caller admits no new work and
    /// receives no events, while its resources stay mapped until their
    /// native release is confirmed (PR210-09): a failed release keeps them
    /// so the next release calls native again.
    retired: bool,
    scan: Option<ScanResource>,
    connections: HashMap<String, CoreConnection>,
    databases: HashMap<String, CoreDatabase>,
    subscriptions: HashMap<String, CoreSubscription>,
    connection_events: HashMap<String, ConnectionEventResource>,
    operations: HashMap<String, TrackedOperation>,
    completed_correlations: HashMap<String, Instant>,
    pending_events: HashSet<String>,
}

/// One live IPC correlation: the caller's budget (admitted on the plugin
/// clock when the route arrived) and the ticket that receives the core
/// operation id at admission, so `operation.cancel` targets exactly that
/// core operation — or is recorded before it exists (PR210-05).
struct TrackedOperation {
    control: OpControl,
}

/// The answer one native release produced, shared with every concurrent
/// release of the same resource.
type ReleaseAnswer = Option<Result<(), DispatchError>>;

/// Ownership phase of one released resource (PR210-09). Delivery runs only
/// while `Active`; the mapping — and with it the native identity needed to
/// retry — is removed only after the core confirms the release or answers
/// that the resource is already gone.
#[derive(Clone)]
enum ReleasePhase {
    Active,
    /// A release is in flight; concurrent releases wait for its answer.
    Releasing(watch::Receiver<ReleaseAnswer>),
    /// The last release failed; the next release calls native again.
    ReleaseFailed,
}

impl ReleasePhase {
    fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }
}

/// Whether a release call leads the native release or joins one in flight.
enum ReleaseStep {
    Lead(watch::Sender<ReleaseAnswer>),
    Join(watch::Receiver<ReleaseAnswer>),
}

/// Transport mapping for one live scan: the IPC handle the caller holds
/// plus the core scan operation id every stop addresses, so a stop can only
/// ever stop this caller's own scan. The forwarder only delivers core
/// observations verbatim — it filters, merges, and paces nothing.
struct ScanResource {
    handle: String,
    core_operation_id: OperationId,
    task: Option<TauriJoinHandle<()>>,
    phase: ReleasePhase,
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
    /// The 4.x public generation (`connection-generation-{n}`).
    connection_generation: String,
    /// The core's generation of this link, which its lifecycle events carry.
    core_generation: String,
    phase: ReleasePhase,
}

/// One discovered characteristic: the exact core path plus the GATT
/// property bits the core registered for it.
#[derive(Clone)]
struct CoreCharacteristic {
    selector: CoreSelector,
    properties: u8,
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
    characteristics: HashMap<String, CoreCharacteristic>,
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
    /// The delivery mode the radio reported for this enablement.
    delivery: ObservedDelivery,
    task: Option<TauriJoinHandle<()>>,
    phase: ReleasePhase,
}

/// How one link ended, as a connection-lifecycle transition plus the
/// terminal reason of the stream that reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LinkTransition {
    previous: &'static str,
    current: &'static str,
    cause: &'static str,
    terminal: &'static str,
}

const LINK_LOST: LinkTransition = LinkTransition {
    previous: "connected",
    current: "lost",
    cause: "peer-link-loss",
    terminal: "connection-lost",
};

const LINK_RELEASED: LinkTransition = LinkTransition {
    previous: "disconnecting",
    current: "disconnected",
    cause: "requested-disconnect",
    terminal: "owner-released",
};

const LINK_ENDED_UNREQUESTED: LinkTransition = LinkTransition {
    previous: "connected",
    current: "disconnected",
    cause: "backend-transition",
    terminal: "connection-lost",
};

/// The adapter took the link (finding 57; legacy CoreBluetooth/WinRT
/// `connection-state-changed` reason `adapter`, `connected → lost`).
const LINK_ADAPTER_LOST: LinkTransition = LinkTransition {
    previous: "connected",
    current: "lost",
    cause: "adapter-loss",
    terminal: "connection-lost",
};

/// Whether a connection-event stream still waits for its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamEnd {
    Open,
    /// The link ended before the stream was ready; `ready` delivers it.
    Pending(LinkTransition),
    /// The core event lagged past the broadcast bound before the stream
    /// was ready; `ready` ends it with `overflow`.
    PendingOverflow,
    /// A delivery task owns the stream's end.
    Claimed,
}

struct ConnectionEventResource {
    connection_handle: String,
    stream_handle: String,
    peer_id: String,
    connection_id: String,
    connection_generation: String,
    /// The core's generation of the link (lifecycle-event matching only).
    core_generation: String,
    /// The attachment the link lives on. A rebind (IPC protocol 4) moves
    /// the caller, never a link the adapter reset already ended.
    attachment: Attachment,
    active: bool,
    sequence: u64,
    end: StreamEnd,
}

struct ConnectionEventIdentity<'a> {
    stream_id: &'a str,
    peer_id: &'a str,
    connection_id: &'a str,
    connection_generation: &'a str,
}

/// A core resource nobody owns whose compensating release failed. It is
/// retried automatically on the Tauri 4.x quarantine schedule (finding
/// 114) and stays here, observable, until a release lands; the owning
/// window's release and authority shutdown retry it too.
struct OrphanDebt {
    /// Stable identity for the automatic retries.
    id: u64,
    caller_key: String,
    resource: OrphanResource,
    /// Automatic release attempts so far, the compensation's own included.
    attempts: u32,
    /// Every automatic attempt was refused: no further automatic retry;
    /// releases report it as `tauri.quarantine.exhausted`.
    exhausted: bool,
}

/// Automatic release attempts per orphan, the compensation included
/// (Tauri 4.x `MAX_QUARANTINE_ATTEMPTS`).
const ORPHAN_RELEASE_ATTEMPTS: u32 = 8;
/// First retry delay; each later one doubles, capped at
/// [`ORPHAN_RETRY_MAX_DELAY`] (Tauri 4.x quarantine backoff).
const ORPHAN_RETRY_FIRST_DELAY: Duration = Duration::from_millis(100);
const ORPHAN_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);

/// One orphan whose release failed, as a release or shutdown reports it.
struct OrphanFailure {
    resource: OrphanResource,
    error: DispatchError,
    exhausted: bool,
    attempts: u32,
}

impl OrphanFailure {
    /// The cleanup-record operation: an orphan whose automatic retries were
    /// exhausted keeps the Tauri 4.x name.
    fn operation(&self) -> &'static str {
        if self.exhausted {
            "tauri.quarantine.exhausted"
        } else {
            "tauri.release.orphan"
        }
    }

    fn describe(&self) -> String {
        if self.exhausted {
            format!(
                "{} automatic release attempts were refused; this release was refused too: {}",
                self.attempts,
                self.error.describe()
            )
        } else {
            self.error.describe()
        }
    }
}

#[derive(Clone, Debug)]
enum OrphanResource {
    Scan(OperationId),
    Link {
        peer_id: String,
        lease: String,
    },
    Subscription {
        peer_id: String,
        selector: CoreSelector,
        consumer: String,
    },
}

impl OrphanResource {
    fn kind(&self) -> &'static str {
        match self {
            Self::Scan(_) => "scan",
            Self::Link { .. } => "connection",
            Self::Subscription { .. } => "subscription",
        }
    }
}

/// What [`BtleplugDispatcher::authority_shutdown`] released and what it
/// could not: the core's own report plus every orphan whose final release
/// failed.
pub struct AuthorityShutdown {
    pub core: Option<ShutdownReport>,
    pub orphan_failures: Vec<String>,
}

#[derive(Debug, Clone)]
struct DispatchError {
    // F01: the frozen code identity is single-owned by `ubm-core`
    // (`BleErrorCode`); the plugin never spells code strings. The domain
    // stays plugin-local (`ipc` has no frozen member).
    code: BleErrorCode,
    domain: &'static str,
    operation: String,
    platform: Option<String>,
    /// The OS's own answer behind the failure, as the core typed it
    /// (finding 116): the error's platform identity on the wire, the same
    /// per-OS identity the Node desktop path reports.
    native: Option<Box<PlatformDetail>>,
    /// The core's answer about repeating the operation (PR210-22); the
    /// dispatcher never derives it from the code. `never` unless the core
    /// said otherwise.
    retryability: Retryability,
    /// The core's commit state for the failed operation, when it knows it
    /// (PR210-37): `not-dispatched` before any radio call, `unknown` for an
    /// operation that may have reached the peer.
    commit: Option<CommitState>,
}

impl DispatchError {
    fn new(code: BleErrorCode, domain: &'static str, operation: impl Into<String>) -> Self {
        Self {
            code,
            domain,
            operation: operation.into(),
            platform: None,
            native: None,
            retryability: Retryability::Never,
            commit: None,
        }
    }

    fn platform(mut self, message: impl Into<String>) -> Self {
        self.platform = Some(message.into());
        self
    }

    /// Lift a shared-core failure into an IPC failure without substitution:
    /// the frozen `code`, `domain`, and `operation` cross verbatim, the core
    /// transport detail (when present) rides as platform evidence, and the
    /// core's retryability and commit state cross unchanged.
    pub(crate) fn from_core(error: &ubm_desktop::DesktopError) -> Self {
        let mut dispatch = Self::new(error.code(), error.domain().as_str(), error.operation());
        if let Some(detail) = error.detail() {
            dispatch = dispatch.platform(detail);
        }
        dispatch.retryability = error.retryability();
        dispatch.commit = error.commit();
        dispatch.native = error.platform().cloned().map(Box::new);
        dispatch
    }

    /// Frozen identity triple for tests: `(code, domain, operation)`.
    #[cfg(test)]
    pub(crate) fn identity(&self) -> (&'static str, &'static str, String) {
        (self.code.as_str(), self.domain, self.operation.clone())
    }

    /// The wire `platform`: the OS's own identity when the core carried
    /// one (its message, else the core's detail, as the safe message), else
    /// the Tauri 4.x `btleplug` / `native-error` shape around the detail,
    /// else `null`.
    fn platform_wire(&self) -> IpcValue {
        if let Some(native) = &self.native {
            return object([
                ("domain", string(native.domain.clone())),
                ("code", string(native.code.clone())),
                (
                    "safeMessage",
                    string(
                        native
                            .message
                            .clone()
                            .or_else(|| self.platform.clone())
                            .unwrap_or_default(),
                    ),
                ),
                (
                    "metadata",
                    IpcValue::Object(
                        native
                            .metadata
                            .iter()
                            .map(|(key, value)| (key.clone(), platform_value(value)))
                            .collect(),
                    ),
                ),
            ]);
        }
        self.platform.as_ref().map_or(IpcValue::Null, |message| {
            object([
                ("domain", string("btleplug")),
                ("code", string("native-error")),
                ("safeMessage", string(message.clone())),
                ("metadata", object([])),
            ])
        })
    }

    fn normalized_error(&self) -> IpcValue {
        let platform = self.platform_wire();
        object([
            ("code", string(self.code.as_str())),
            ("domain", string(self.domain)),
            ("operation", string(self.operation.clone())),
            ("platform", platform),
            ("retryability", string(self.retryability.as_str())),
            ("commit", commit_wire(self.commit)),
        ])
    }

    fn into_response(self) -> IpcValue {
        object([
            ("kind", string("failure")),
            ("error", self.normalized_error()),
        ])
    }

    /// One-line identity for cleanup receipts and debt reports.
    fn describe(&self) -> String {
        match &self.platform {
            Some(detail) => format!(
                "{}:{}:{} ({detail})",
                self.code.as_str(),
                self.domain,
                self.operation
            ),
            None => format!("{}:{}:{}", self.code.as_str(), self.domain, self.operation),
        }
    }
}

/// One typed platform fact on the wire. An integer JavaScript cannot hold
/// exactly crosses as its decimal text (as on the Node desktop path).
fn platform_value(value: &PlatformValue) -> IpcValue {
    match value {
        PlatformValue::Int(integer) if integer.unsigned_abs() <= MAX_SAFE_INTEGER => {
            IpcValue::Number(Number::from(*integer))
        }
        PlatformValue::Int(integer) => string(integer.to_string()),
        PlatformValue::Text(text) => string(text.clone()),
        PlatformValue::Bool(flag) => IpcValue::Bool(*flag),
    }
}

/// The wire word for a core commit state on a failed operation, shared with
/// the mobile wire (`not-dispatched` / `uncertain`). A commit state that says
/// nothing about repeating the failed operation crosses as `null`.
fn commit_wire(commit: Option<CommitState>) -> IpcValue {
    match commit {
        Some(CommitState::NotDispatched) => string("not-dispatched"),
        Some(CommitState::Unknown) => string("uncertain"),
        Some(CommitState::Committed | CommitState::Released) | None => IpcValue::Null,
    }
}

/// The caller's budget for one route: `budgetMs` milliseconds from the
/// instant the route arrived on the plugin clock (PR210-06). Absent or
/// `null` means the caller gave no budget; anything but a non-negative safe
/// integer is malformed. The webview's own clock never crosses the wire.
fn parse_budget(
    payload: &BTreeMap<String, IpcValue>,
    admitted_at: tokio::time::Instant,
) -> Result<Budget, DispatchError> {
    match payload.get("budgetMs") {
        None | Some(IpcValue::Null) => Ok(Budget::unbounded()),
        Some(IpcValue::Number(value)) => value
            .as_u64()
            .filter(|milliseconds| *milliseconds <= MAX_SAFE_INTEGER)
            .map(|milliseconds| Budget::from_ms_at(admitted_at, milliseconds))
            .ok_or_else(|| {
                DispatchError::new(BleErrorCode::ProtocolMalformed, "ipc", "tauri.route-budget")
            }),
        Some(_) => Err(DispatchError::new(
            BleErrorCode::ProtocolMalformed,
            "ipc",
            "tauri.route-budget",
        )),
    }
}

/// Start a release of one resource: lead it, or join the one in flight. A
/// release whose leader vanished without answering is led again.
fn begin_release(phase: &mut ReleasePhase) -> ReleaseStep {
    if let ReleasePhase::Releasing(receiver) = phase {
        if receiver.has_changed().is_ok() {
            return ReleaseStep::Join(receiver.clone());
        }
    }
    let (sender, receiver) = watch::channel(None);
    *phase = ReleasePhase::Releasing(receiver);
    ReleaseStep::Lead(sender)
}

/// Wait for the answer of a release led elsewhere.
async fn join_release(mut receiver: watch::Receiver<ReleaseAnswer>) -> Result<(), DispatchError> {
    loop {
        if let Some(answer) = receiver.borrow_and_update().clone() {
            return answer;
        }
        if receiver.changed().await.is_err() {
            return receiver.borrow().clone().unwrap_or_else(|| {
                Err(DispatchError::new(
                    BleErrorCode::PlatformFailure,
                    "cleanup",
                    "tauri.release-abandoned",
                ))
            });
        }
    }
}

/// Core disconnect verdicts that mean "no link to release": the core holds
/// no record of the peer or of a connection to it. A link that already
/// ended is the core's `LinkRelease::AlreadyReleased` answer, not an error;
/// every error — including a radio failure worded `connection.lost` — keeps
/// the link owned for a real retry.
fn is_released_link(error: &DispatchError) -> bool {
    matches!(
        error.code,
        BleErrorCode::PeerNotFound | BleErrorCode::ConnectionNotFound
    )
}

/// Core unsubscribe verdicts that mean "no consumer to release": the core
/// no longer resolves the path (a service change invalidated the database
/// the consumer was registered in) or holds no record of the peer or of a
/// connection to it. A consumer on an ended link is the core's `Ok`, not an
/// error.
fn is_released_subscription(error: &DispatchError) -> bool {
    matches!(
        error.code,
        BleErrorCode::GattNotFound | BleErrorCode::PeerNotFound | BleErrorCode::ConnectionNotFound
    )
}

/// The IPC answer to `operation.cancel` for what the core did with it.
fn cancel_state(ack: &CancelAck) -> &'static str {
    match ack {
        CancelAck::RecordedBeforeAdmission
        | CancelAck::Forwarded {
            outcome: CompletionOutcome::Settled { .. },
            ..
        } => "cancellation-requested",
        CancelAck::Forwarded {
            outcome: CompletionOutcome::ContenderIgnored,
            ..
        } => "not-cancellable",
        CancelAck::Forwarded {
            outcome: CompletionOutcome::DuplicateSuppressed { .. },
            ..
        }
        | CancelAck::AlreadySettled => "already-terminal",
    }
}

/// The Tauri 4.x scan re-read period (2 s): 4.x re-read every known
/// peripheral on that interval while scanning.
const TAURI_KNOWN_PEER_REFRESH: std::time::Duration = std::time::Duration::from_secs(2);

/// The Tauri 4.x identity (origin/main `btleplug_dispatcher.rs:525-531`):
/// the dispatcher's own id counter numbers, in order, the attachment, the
/// backend instance and the backend and adapter generations; a reset takes
/// the next three numbers for the attachment and its generations and keeps
/// the instance.
#[derive(Debug)]
struct TauriIdentity {
    next_id: Arc<AtomicU64>,
    instance: std::sync::OnceLock<String>,
}

impl TauriIdentity {
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }
}

impl ubm_desktop::HostIdentity for TauriIdentity {
    fn namespace(&self) -> &str {
        "tauri"
    }

    fn log_tag(&self) -> &str {
        "tauri-plugin-unified-ble-manager"
    }

    fn attachment(
        &self,
        epoch: ubm_desktop::AttachmentEpoch<'_>,
    ) -> Result<AttachmentTuple, ubm_core::contracts::CoreError> {
        use ubm_core::contracts::{
            AdapterGeneration, AdapterId, AttachmentId, BackendGeneration, BackendInstanceId,
        };
        let attachment_id = self.id("tauri-attachment");
        let instance = self
            .instance
            .get_or_init(|| self.id("tauri-btleplug"))
            .clone();
        let backend_generation = self.id("tauri-backend-generation");
        let adapter_generation = self.id("tauri-adapter-generation");
        Ok(AttachmentTuple::new(
            AttachmentId::new(attachment_id)?,
            BackendInstanceId::new(instance)?,
            BackendGeneration::new(backend_generation)?,
            AdapterId::new(epoch.adapter)?,
            AdapterGeneration::new(adapter_generation)?,
        ))
    }

    fn kernel_generation(
        &self,
        epoch: ubm_desktop::AttachmentEpoch<'_>,
    ) -> Result<ubm_core::contracts::Generation, ubm_core::contracts::CoreError> {
        ubm_core::contracts::Generation::new(format!(
            "tauri-kernel-{}-{}",
            epoch.ordinal, epoch.resets
        ))
    }
}

/// The Tauri central profile: the desktop profile under the Tauri 4.x
/// identity, numbered by `next_id`.
fn tauri_profile(next_id: Arc<AtomicU64>) -> CentralProfile {
    let mut profile = CentralProfile::desktop("tauri");
    profile.identity = Arc::new(TauriIdentity {
        next_id,
        instance: std::sync::OnceLock::new(),
    });
    profile
}

/// Opens the scheduling authority for one Tauri profile.
type ProfiledOpener = Arc<dyn Fn(CentralProfile) -> AuthorityOpenFuture + Send + Sync>;

/// The production opener: the btleplug radio and its central open on the
/// shared desktop executor (never on Tauri's runtime), on the adapter the
/// profile names, with the core's selection rule (ambiguity refused).
fn btleplug_opener() -> ProfiledOpener {
    Arc::new(move |profile| {
        Box::pin(async move {
            let central = btleplug_runtime()
                .spawn(async move {
                    let central = DesktopCentral::open_btleplug(profile).await?;
                    // Finding 120: Tauri 4.x re-read every known peripheral
                    // every 2 s during a scan; its observation cadence stays.
                    central.set_known_peer_refresh(Some(TAURI_KNOWN_PEER_REFRESH));
                    Ok(central)
                })
                .await
                .map_err(|error| {
                    DispatchError::new(
                        BleErrorCode::AdapterUnavailable,
                        "adapter",
                        "tauri.core-open",
                    )
                    .platform(error.to_string())
                })?
                .map_err(|error| DispatchError::from_core(&error))?;
            let authority: Arc<dyn CoreAuthority> = Arc::new(central);
            Ok(authority)
        })
    })
}

/// Which delivery state a forwarder found its resource in.
enum Delivery {
    Active,
    Paused,
    Gone,
}

impl Default for BtleplugDispatcher {
    fn default() -> Self {
        Self::new(BtleplugDispatcherOptions::default())
    }
}

impl BtleplugDispatcher {
    pub fn new(options: BtleplugDispatcherOptions) -> Self {
        Self::with_profiled_opener(options.adapter_id, btleplug_opener())
    }

    /// Dispatcher whose authority opens through `open` on first use, with
    /// the Tauri profile numbered by this dispatcher's own id counter.
    fn with_profiled_opener(adapter_id: Option<String>, open: ProfiledOpener) -> Self {
        let next_id = Arc::new(AtomicU64::new(1));
        let opener: AuthorityOpener = {
            let next_id = Arc::clone(&next_id);
            Arc::new(move || {
                let mut profile = tauri_profile(Arc::clone(&next_id));
                profile.adapter_id = adapter_id.clone();
                open(profile)
            })
        };
        Self::with_slot_and_ids(AuthoritySlot::Unopened(opener), next_id)
    }

    fn with_slot(slot: AuthoritySlot) -> Self {
        Self::with_slot_and_ids(slot, Arc::new(AtomicU64::new(1)))
    }

    fn with_slot_and_ids(slot: AuthoritySlot, next_id: Arc<AtomicU64>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DispatcherState {
                adapter_name: None,
                callers: HashMap::new(),
                orphan_debt: Vec::new(),
                replaced_attachment_ids: HashSet::new(),
            })),
            bootstrap_admission: Arc::new(Mutex::new(())),
            next_id,
            next_internal_id: Arc::new(AtomicU64::new(1)),
            next_revocation: Arc::new(AtomicU64::new(1)),
            started_at: Arc::new(Instant::now()),
            revoked_callers: Arc::new(SyncMutex::new(HashMap::new())),
            authority: Arc::new(Mutex::new(slot)),
            lifecycle_pump: Arc::new(SyncMutex::new(None)),
        }
    }

    /// Dispatcher over an explicitly admitted scheduling authority. Tests
    /// inject a [`DesktopCentral`] over a scripted boundary here;
    /// production opens the btleplug-backed central lazily. Either way every
    /// BLE verdict comes from the authority — never from direct radio
    /// ownership.
    pub fn with_core_authority(authority: Arc<dyn CoreAuthority>) -> Self {
        let dispatcher = Self::with_slot(AuthoritySlot::Open(Arc::clone(&authority)));
        dispatcher.start_lifecycle_pump(&authority);
        dispatcher
    }

    /// Dispatcher whose authority opens through `opener` on first use.
    #[cfg(test)]
    fn with_authority_opener(opener: AuthorityOpener) -> Self {
        Self::with_slot(AuthoritySlot::Unopened(opener))
    }

    /// The admitted scheduling authority, opening it on first use. The slot
    /// stays locked across the open, so racing first calls share one radio
    /// and one central (PR210-32). Radio failures surface verbatim —
    /// `adapter.unavailable` where no adapter exists — never silent legacy.
    async fn ensure_authority(&self) -> Result<Arc<dyn CoreAuthority>, DispatchError> {
        let mut slot = self.authority.lock().await;
        let opener = match &*slot {
            AuthoritySlot::Open(authority) => return Ok(Arc::clone(authority)),
            AuthoritySlot::ShutDown => {
                return Err(DispatchError::new(
                    BleErrorCode::AdapterUnavailable,
                    "adapter",
                    "tauri.core-shutdown",
                ))
            }
            AuthoritySlot::Unopened(opener) => Arc::clone(opener),
        };
        let authority = opener().await?;
        // The lifecycle receiver exists before any operation can run, so no
        // transition of this central goes unobserved.
        self.start_lifecycle_pump(&authority);
        *slot = AuthoritySlot::Open(Arc::clone(&authority));
        Ok(authority)
    }

    /// The authority if it is open right now. Never opens the radio and
    /// never waits for an open in progress: while one is in progress no
    /// operation has reached the core yet.
    fn authority_if_open(&self) -> Option<Arc<dyn CoreAuthority>> {
        let slot = self.authority.try_lock().ok()?;
        match &*slot {
            AuthoritySlot::Open(authority) => Some(Arc::clone(authority)),
            AuthoritySlot::Unopened(_) | AuthoritySlot::ShutDown => None,
        }
    }

    /// Shut the admitted authority down and refuse further BLE work loudly.
    /// Orphaned core resources get a final release first; failures are
    /// reported, never dropped. Production never calls it (the
    /// process-lifetime central outlives every caller).
    pub async fn authority_shutdown(&self) -> AuthorityShutdown {
        let previous =
            std::mem::replace(&mut *self.authority.lock().await, AuthoritySlot::ShutDown);
        let (core, orphan_failures) = match previous {
            AuthoritySlot::Open(authority) => {
                let orphan_failures = self
                    .settle_orphan_debt(&authority, None)
                    .await
                    .into_iter()
                    .map(|failure| {
                        format!(
                            "{} ({}): {}",
                            failure.resource.kind(),
                            failure.operation(),
                            failure.describe()
                        )
                    })
                    .collect();
                (Some(authority.shutdown().await), orphan_failures)
            }
            AuthoritySlot::Unopened(_) | AuthoritySlot::ShutDown => (None, Vec::new()),
        };
        let pump = self
            .lifecycle_pump
            .lock()
            .expect("lifecycle pump mutex poisoned")
            .take();
        if let Some(pump) = pump {
            pump.abort();
        }
        AuthorityShutdown {
            core,
            orphan_failures,
        }
    }

    fn id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// A name only the plugin and the core see (never on the IPC surface);
    /// numbered apart from the 4.x public counter.
    fn internal_id(&self, prefix: &str) -> String {
        format!(
            "{prefix}-internal-{}",
            self.next_internal_id.fetch_add(1, Ordering::Relaxed)
        )
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

    /// The shared central's current attachment (finding 43): its tuple as
    /// the core holds it now — a reset replaces it (finding 57) — plus the
    /// adapter's display label, read once through the same boundary.
    async fn ensure_adapter(&self) -> Result<Attachment, DispatchError> {
        let authority = self.ensure_authority().await?;
        let cached = self.inner.lock().await.adapter_name.clone();
        let adapter_name = match cached {
            Some(name) => name,
            None => {
                let name = authority
                    .adapter_name()
                    .await
                    .map_err(|error| DispatchError::from_core(&error))?;
                self.inner
                    .lock()
                    .await
                    .adapter_name
                    .get_or_insert(name)
                    .clone()
            }
        };
        Ok(attachment_of(&authority.attachment(), adapter_name))
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
                attachment: attachment.clone(),
                event_sink,
                retired: false,
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
        // The budget counts from here: queueing behind the dispatcher lock,
        // the authority open, and the core lock all spend it (PR210-06).
        let admitted_at = tokio::time::Instant::now();
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
        let budget = parse_budget(&payload, admitted_at)?;
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
        self.validate_envelope(&caller, &command, &envelope).await?;

        if command == "operation.cancel" {
            return self.cancel_operation(&caller, &payload).await;
        }

        let control = OpControl::new(budget, OpTicket::new());
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
                    control: control.clone(),
                },
            );
        }

        // The operation runs to the core's settled outcome and the route
        // reports exactly that outcome: a cancel reaches the core through
        // the ticket (`operation.cancel`), and the core answers aborted,
        // timed out, or the result that won the race. It runs as its own
        // task so a dropped IPC future never drops a core operation midway.
        let operation_dispatcher = self.clone();
        let operation_caller = caller.clone();
        let operation_command = command.clone();
        let result = tauri::async_runtime::spawn(async move {
            operation_dispatcher
                .execute(
                    &operation_caller,
                    &operation_command,
                    payload,
                    binary_payload,
                    control,
                )
                .await
        })
        .await
        .map_err(|error| {
            DispatchError::new(
                BleErrorCode::PlatformFailure,
                "ipc",
                format!("tauri.{command}.join"),
            )
            .platform(error.to_string())
        })?;
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

    /// Cancel one in-flight correlation through its ticket. Before the core
    /// admitted the operation the request is recorded and the operation
    /// ends aborted without a radio call; after admission exactly that core
    /// operation is cancelled; after settlement nothing happens. The route
    /// of the cancelled operation reports the core's settled outcome. An
    /// unknown correlation is already terminal. Core failures propagate
    /// loudly — a cancel that cannot settle is not reported as settled.
    async fn cancel_operation(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let target = required_string(payload, "targetCorrelation", "tauri.cancel")?;
        let ticket = self
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
            .map(|tracked| tracked.control.ticket.clone());
        let Some(ticket) = ticket else {
            return Ok(route_response(object([(
                "state",
                string("already-terminal"),
            )])));
        };
        let state = self.cancel_ticket(&ticket).await?;
        Ok(route_response(object([("state", string(state))])))
    }

    /// Cancel through the core when it is open; otherwise nothing has been
    /// admitted yet and recording the request on the ticket is the cancel.
    /// Answers the IPC cancel state.
    async fn cancel_ticket(&self, ticket: &OpTicket) -> Result<&'static str, DispatchError> {
        match self.authority_if_open() {
            Some(authority) => authority
                .cancel(ticket)
                .await
                .map(|ack| cancel_state(&ack))
                .map_err(|error| DispatchError::from_core(&error)),
            None => Ok(match ticket.request_cancel() {
                CancelRequest::RecordedBeforeAdmission => "cancellation-requested",
                // The driver observes the request and settles the abort in
                // the core itself.
                CancelRequest::Forward(_) => "cancellation-requested",
                CancelRequest::AlreadySettled => "already-terminal",
            }),
        }
    }

    async fn validate_envelope(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
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
        let state = self.inner.lock().await;
        {
            let caller_state = state
                .callers
                .get(&caller_key(caller))
                .filter(|caller_state| !caller_state.retired)
                .ok_or_else(|| {
                    DispatchError::new(BleErrorCode::OwnershipDenied, "ipc", "tauri.route-caller")
                })?;
            // Identity only: comparing the attachment never samples platform
            // state (an adapter-state snapshot asks the OS, which must not
            // run on every route or under the dispatcher lock).
            // Protocol 4: an attachment the dispatcher replaced admits only
            // releases; the renderer routes new work under the attachment it
            // was rebound to. A renderer can never name one it was not given.
            if state.replaced_attachment_ids.contains(&attachment_id) {
                if !is_release_command(command) {
                    let mut error = DispatchError::new(
                        BleErrorCode::BackendReset,
                        "adapter",
                        "tauri.route-attachment",
                    )
                    .platform(format!(
                        "the adapter was reset: attachment {attachment_id} ended; the dispatcher \
                         rebound this caller to attachment {}",
                        caller_state.attachment.attachment_id
                    ));
                    error.commit = Some(CommitState::NotDispatched);
                    return Err(error);
                }
            } else if caller_state.attachment.attachment_id != attachment_id
                || !attachment_identity_matches(&envelope_attachment, &caller_state.attachment)
            {
                return Err(DispatchError::new(
                    BleErrorCode::ProtocolViolation,
                    "ipc",
                    "tauri.route-attachment",
                ));
            }
            if caller_state.lease_id != lease_id
                || caller_state.lease_generation != lease_generation
            {
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
            // The event sink is deliberately NOT reassigned here. It is bound
            // once by `bootstrap` and lives for the attachment; replacing it
            // would drop the previous Tauri Channel, and that drop ends the
            // shared JS callback which every later event depends on.
        }
        Ok(())
    }

    /// Refuse work on an attachment an adapter reset replaced (finding 57)
    /// before it reaches the core: `backend.reset`, nothing dispatched.
    /// Releases stay admitted — what the renderer owes for resources the
    /// reset ended still settles, and the core answers what is already
    /// gone.
    async fn refuse_stale_attachment(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
    ) -> Result<(), DispatchError> {
        if is_release_command(command) {
            return Ok(());
        }
        let bound = self
            .bound_attachment(caller, "tauri.route-attachment")
            .await?;
        let current = self.ensure_authority().await?.attachment();
        if current.attachment_id().as_str() != bound.attachment_id {
            return Err(stale_attachment(&bound, &current));
        }
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
        let caller_state = state
            .callers
            .get(&caller_key(caller))
            .filter(|caller_state| !caller_state.retired)
            .ok_or_else(|| {
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
        payload: BTreeMap<String, IpcValue>,
        binary_payload: Option<Vec<u8>>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        self.validate_expected_lease(caller, &payload).await?;
        self.refuse_stale_attachment(caller, command).await?;
        match command {
            "adapter.state" => self.adapter_state(caller, ctl).await,
            "scan.start" => self.start_scan(caller, payload, ctl).await,
            "scan.stop" => self.stop_scan(caller, payload, ctl).await,
            "connection.connect" => self.connect(caller, payload, ctl).await,
            "connection.disconnect" => self.disconnect(caller, payload, ctl).await,
            "connection.events.subscribe" => {
                self.subscribe_connection_events(caller, payload).await
            }
            "connection.events.ready" => self.ready_connection_events(caller, payload).await,
            "connection.events.unsubscribe" => {
                self.unsubscribe_connection_events(caller, payload).await
            }
            "connection.rssi" => self.read_rssi(caller, payload, ctl).await,
            "connection.maximum-write-length" => {
                self.maximum_write_length(caller, payload, ctl).await
            }
            "gatt.discover" => self.discover(caller, payload, ctl).await,
            "gatt.database.release" => self.release_database(caller, payload).await,
            "gatt.read" => self.read(caller, payload, ctl).await,
            "gatt.write" => self.write(caller, payload, binary_payload, ctl).await,
            "gatt.subscribe" => self.subscribe(caller, payload, ctl).await,
            "gatt.unsubscribe" => self.unsubscribe(caller, payload, ctl).await,
            "gatt.descriptor.read" => self.read_descriptor(caller, payload, ctl).await,
            "gatt.descriptor.write" => {
                self.write_descriptor(caller, payload, binary_payload, ctl)
                    .await
            }
            _ => Err(DispatchError::new(
                BleErrorCode::ArgumentInvalid,
                "ipc",
                "tauri.route-command",
            )),
        }
    }

    /// `adapter.state` from the shared central (findings 43, 60): power
    /// and authorization as the OS reported them through the core's radio,
    /// availability from the core's adapter facts, heard peers from the
    /// same boundary. A question the radio cannot answer on this platform
    /// (`capability.unsupported`) is reported unknown with its reason; any
    /// other failure crosses verbatim. A reset that lands during the read
    /// is the transition it observes: the answer is the post-transition
    /// snapshot under the current backend generation, as every legacy host
    /// answered (finding 94). A request that arrives after the reset is
    /// still refused `backend.reset` (finding 57).
    async fn adapter_state(
        &self,
        caller: &AuthenticatedCaller,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let bound = self.bound_attachment(caller, "tauri.adapter-state").await?;
        let authority = self.ensure_authority().await?;
        let mut reasons = Vec::new();
        // The authorization read shares the caller's budget; the power read
        // carries the caller's ticket, so a cancel reaches the radio wait.
        let budget = ctl.budget;
        let power = unless_unsupported(
            authority.adapter_state(ctl).await,
            "adapter power",
            &mut reasons,
        )?;
        let authorization = unless_unsupported(
            authority
                .adapter_authorization(OpControl::new(budget, OpTicket::new()))
                .await,
            "adapter authorization",
            &mut reasons,
        )?;
        let heard = authority
            .peers()
            .await
            .map_err(|error| DispatchError::from_core(&error))?
            .len();
        let removed = authority.adapter_status().availability == AdapterAvailability::Unavailable;
        // A reset that landed during the read is the transition the read
        // observed: the snapshot is the post-transition state under the
        // current generation (finding 94), never `backend.reset`.
        let read_under = Attachment {
            backend_generation: authority
                .attachment()
                .backend_generation()
                .as_str()
                .to_owned(),
            ..bound
        };
        Ok(adapter_state_payload_live(
            &read_under,
            &AdapterReading {
                power,
                authorization,
                removed,
                heard: i64::try_from(heard).unwrap_or(i64::MAX),
                reasons,
            },
        ))
    }

    /// The attachment `caller` bound at attach.
    async fn bound_attachment(
        &self,
        caller: &AuthenticatedCaller,
        operation: &'static str,
    ) -> Result<Attachment, DispatchError> {
        self.inner
            .lock()
            .await
            .callers
            .get(&caller_key(caller))
            .map(|caller_state| caller_state.attachment.clone())
            .ok_or_else(|| DispatchError::new(BleErrorCode::OwnershipDenied, "adapter", operation))
    }

    /// Release a core resource nobody can own; a failed release becomes
    /// orphan debt for `caller_key` (PR210-08), retried automatically on
    /// the Tauri 4.x schedule (finding 114) and by the window's release.
    async fn compensate(
        &self,
        authority: &Arc<dyn CoreAuthority>,
        caller_key: &str,
        resource: OrphanResource,
    ) {
        if release_orphan(authority, &resource).await.is_ok() {
            return;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().await.orphan_debt.push(OrphanDebt {
            id,
            caller_key: caller_key.to_owned(),
            resource,
            attempts: 1,
            exhausted: false,
        });
        self.spawn_orphan_retries(Arc::clone(authority), id);
    }

    /// Retry one orphan's release automatically: 100 ms after the failed
    /// compensation, doubling to 5 s, until a release lands or
    /// [`ORPHAN_RELEASE_ATTEMPTS`] attempts were refused (then it is marked
    /// exhausted and stays owed). A round that finds the debt held by a
    /// window release in flight skips; a debt that is gone for good ends
    /// the schedule. Runs on the ambient runtime, so it follows the
    /// caller's clock.
    fn spawn_orphan_retries(&self, authority: Arc<dyn CoreAuthority>, id: u64) {
        let dispatcher = self.clone();
        tokio::spawn(async move {
            let mut delay = ORPHAN_RETRY_FIRST_DELAY;
            for _ in 1..ORPHAN_RELEASE_ATTEMPTS {
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay.saturating_mul(2), ORPHAN_RETRY_MAX_DELAY);
                if dispatcher.authority_if_open().is_none() {
                    return;
                }
                let debt = {
                    let mut state = dispatcher.inner.lock().await;
                    let Some(index) = state.orphan_debt.iter().position(|debt| debt.id == id)
                    else {
                        continue;
                    };
                    state.orphan_debt.swap_remove(index)
                };
                if release_orphan(&authority, &debt.resource).await.is_ok() {
                    return;
                }
                let mut debt = debt;
                debt.attempts = debt.attempts.saturating_add(1);
                debt.exhausted = debt.attempts >= ORPHAN_RELEASE_ATTEMPTS;
                let exhausted = debt.exhausted;
                dispatcher.inner.lock().await.orphan_debt.push(debt);
                if exhausted {
                    return;
                }
            }
        });
    }

    /// Retry the release of every orphan owed by `caller_key` (every orphan
    /// with `None`). Released orphans leave the debt; each failure stays in
    /// it (its automatic retries continue unless exhausted) and is returned
    /// with its truthful error.
    async fn settle_orphan_debt(
        &self,
        authority: &Arc<dyn CoreAuthority>,
        caller_key: Option<&str>,
    ) -> Vec<OrphanFailure> {
        let owed = {
            let mut state = self.inner.lock().await;
            let (owed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut state.orphan_debt)
                .into_iter()
                .partition(|debt| caller_key.is_none_or(|key| debt.caller_key == key));
            state.orphan_debt = kept;
            owed
        };
        let mut failures = Vec::new();
        for debt in owed {
            if let Err(error) = release_orphan(authority, &debt.resource).await {
                failures.push(OrphanFailure {
                    resource: debt.resource.clone(),
                    error,
                    exhausted: debt.exhausted,
                    attempts: debt.attempts,
                });
                self.inner.lock().await.orphan_debt.push(debt);
            }
        }
        failures
    }

    async fn start_scan(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
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
        let lease = expected_lease(&payload, "tauri.scan-lease")?;
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
        let scan_id = authority
            .start_scan(&key, &service_uuid_strings, ctl)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let handle = self.id("scan");
        // Publication is one decision under the dispatcher lock, and the
        // forwarder spawns only once its entry exists (PR210-07): it can
        // never observe a missing entry and exit while the scan runs. Every
        // refusal stops exactly this scan by its core id (PR210-08).
        let refusal = {
            let mut state = self.inner.lock().await;
            match state.callers.get_mut(&key) {
                None => Some(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "scan",
                    "tauri.scan-owner",
                )),
                Some(caller_state) if caller_state.retired => Some(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "scan",
                    "tauri.scan-owner",
                )),
                Some(caller_state) if !lease_matches(caller_state, &lease) => {
                    Some(DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "scan",
                        "tauri.scan-stale-lease",
                    ))
                }
                Some(caller_state) if caller_state.scan.is_some() => Some(DispatchError::new(
                    BleErrorCode::ScanAlreadyActive,
                    "scan",
                    "tauri.scan-start",
                )),
                Some(caller_state) => {
                    caller_state.scan = Some(ScanResource {
                        handle: handle.clone(),
                        core_operation_id: scan_id.clone(),
                        task: None,
                        phase: ReleasePhase::Active,
                    });
                    let task = self.spawn_scan_forwarder(
                        Arc::clone(&authority),
                        key.clone(),
                        handle.clone(),
                        lease.clone(),
                    );
                    if let Some(scan) = caller_state.scan.as_mut() {
                        scan.task = Some(task);
                    }
                    None
                }
            }
        };
        if let Some(refusal) = refusal {
            self.compensate(&authority, &key, OrphanResource::Scan(scan_id))
                .await;
            return Err(refusal);
        }
        Ok(object([
            ("handle", string(handle)),
            ("backendGeneration", string(attachment.backend_generation)),
            ("plan", diagnostic_plan),
        ]))
    }

    /// Verbatim observation delivery: the forwarder takes core observations
    /// and emits them unchanged. It filters, merges, and paces nothing —
    /// duplicate/merge policy is the core's, view shaping stays TypeScript
    /// side. It delivers only while the scan is `Active`.
    fn spawn_scan_forwarder(
        &self,
        authority: Arc<dyn CoreAuthority>,
        key: String,
        handle: String,
        lease: (String, String),
    ) -> TauriJoinHandle<()> {
        let forwarder = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                match forwarder.scan_delivery(&key, &handle).await {
                    Delivery::Gone => return,
                    Delivery::Paused => {
                        tokio::time::sleep(FORWARD_POLL_INTERVAL).await;
                        continue;
                    }
                    Delivery::Active => {}
                }
                match authority.take_advertisement().await {
                    Ok(Some(snapshot)) => {
                        let observation = core_scan_observation(&snapshot);
                        match forwarder
                            .emit(&key, Some((&lease.0, &lease.1)), &handle, observation)
                            .await
                        {
                            Ok(()) => {}
                            Err(error) if error.code == BleErrorCode::StreamQuota => {
                                // Quota-drop: the observation is dropped,
                                // never aborting the scan (the core keeps
                                // producing).
                            }
                            Err(error) => {
                                forwarder
                                    .terminal(
                                        &key,
                                        (&lease.0, &lease.1),
                                        &handle,
                                        "source-failed",
                                        Some(&error),
                                    )
                                    .await
                                    .ok();
                                return;
                            }
                        }
                    }
                    Ok(None) => tokio::time::sleep(FORWARD_POLL_INTERVAL).await,
                    Err(error) => {
                        // Core-side failure ends delivery with the verbatim
                        // core verdict (never a guessed stream error, never
                        // silent).
                        let terminal_error = DispatchError::from_core(&error);
                        forwarder
                            .terminal(
                                &key,
                                (&lease.0, &lease.1),
                                &handle,
                                "source-failed",
                                Some(&terminal_error),
                            )
                            .await
                            .ok();
                        return;
                    }
                }
            }
        })
    }

    async fn scan_delivery(&self, key: &str, handle: &str) -> Delivery {
        let state = self.inner.lock().await;
        let Some(caller_state) = state.callers.get(key).filter(|caller| !caller.retired) else {
            return Delivery::Gone;
        };
        match caller_state.scan.as_ref() {
            Some(scan) if scan.handle == handle && scan.phase.is_active() => Delivery::Active,
            Some(scan) if scan.handle == handle => Delivery::Paused,
            _ => Delivery::Gone,
        }
    }

    async fn stop_scan(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "scanHandle", "tauri.scan-stop")?;
        let key = caller_key(caller);
        {
            let state = self.inner.lock().await;
            if !state.callers.contains_key(&key) {
                // A released caller owns nothing to stop.
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "scan",
                    "tauri.scan-stop-owner",
                ));
            }
        }
        self.release_scan(&key, &handle, ctl).await?;
        Ok(released())
    }

    /// Stop one mapped scan through the core by its own core id (PR210-09).
    /// Delivery pauses while the stop runs; the mapping — and with it the
    /// id a retry needs — is removed only when the core confirms the stop
    /// or answers that this scan is no longer active. An unknown handle is
    /// already released.
    async fn release_scan(
        &self,
        key: &str,
        handle: &str,
        ctl: OpControl,
    ) -> Result<(), DispatchError> {
        let step = {
            let mut state = self.inner.lock().await;
            let Some(scan) = state
                .callers
                .get_mut(key)
                .and_then(|caller_state| caller_state.scan.as_mut())
                .filter(|scan| scan.handle == handle)
            else {
                return Ok(());
            };
            (
                begin_release(&mut scan.phase),
                scan.core_operation_id.clone(),
            )
        };
        let (sender, scan_id) = match step {
            (ReleaseStep::Join(receiver), _) => return join_release(receiver).await,
            (ReleaseStep::Lead(sender), scan_id) => (sender, scan_id),
        };
        let result = match self.ensure_authority().await {
            Ok(authority) => authority
                .stop_scan(&scan_id, ctl)
                .await
                .map(|_| ())
                .map_err(|error| DispatchError::from_core(&error)),
            Err(error) => Err(error),
        };
        {
            let mut state = self.inner.lock().await;
            if let Some(caller_state) = state.callers.get_mut(key) {
                let owned = caller_state
                    .scan
                    .as_ref()
                    .is_some_and(|scan| scan.handle == handle);
                if owned {
                    if result.is_ok() {
                        if let Some(task) = caller_state.scan.take().and_then(|scan| scan.task) {
                            task.abort();
                        }
                    } else if let Some(scan) = caller_state.scan.as_mut() {
                        scan.phase = ReleasePhase::ReleaseFailed;
                    }
                }
            }
        }
        let _ = sender.send(Some(result.clone()));
        result
    }

    async fn connect(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let peer_id = required_string(&payload, "peerId", "tauri.connect-peer")?;
        let key = caller_key(caller);
        let expected = expected_lease(&payload, "tauri.connect-lease")?;
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
        // The core lease is internal: it takes no number from the 4.x counter.
        let lease = self.internal_id("lease");
        let authority = self.ensure_authority().await?;
        let connection = authority
            .connect(&peer_id, &lease, ctl)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let handle = self.id("connection");
        let connection_id = self.id("connection-id");
        // The 4.x public generation; the core's travels with it for matching.
        let public_generation = self.id("connection-generation");
        // Publication: a link nobody can address is released through the
        // core by its own lease, and a failed release becomes orphan debt
        // (PR210-08) — never a `let _` discard.
        let published = {
            let mut state = self.inner.lock().await;
            match state.callers.get_mut(&key) {
                None => Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.connect-owner",
                )),
                Some(caller_state) if caller_state.retired => Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.connect-owner",
                )),
                Some(caller_state) if !lease_matches(caller_state, &expected) => {
                    Err(DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "connection",
                        "tauri.connect-stale-lease",
                    ))
                }
                Some(caller_state) => match connection.connection_generation.clone() {
                    None => Err(DispatchError::new(
                        BleErrorCode::ProtocolMalformed,
                        "connection",
                        "tauri.connect-generation",
                    )),
                    Some(core_generation) => {
                        let owner_lease_id = caller_state.lease_id.clone();
                        caller_state.connections.insert(
                            handle.clone(),
                            CoreConnection {
                                peer_id: peer_id.clone(),
                                lease: lease.clone(),
                                connection_id: connection_id.clone(),
                                owner_lease_id: owner_lease_id.clone(),
                                connection_generation: public_generation.clone(),
                                core_generation,
                                phase: ReleasePhase::Active,
                            },
                        );
                        Ok((owner_lease_id, public_generation.clone()))
                    }
                },
            }
        };
        match published {
            Ok((owner_lease_id, connection_generation)) => Ok(object([
                ("handle", string(handle)),
                ("connectionId", string(connection_id)),
                ("ownerLeaseId", string(owner_lease_id)),
                ("peerId", string(peer_id)),
                ("connectionGeneration", string(connection_generation)),
            ])),
            Err(refusal) => {
                self.compensate(&authority, &key, OrphanResource::Link { peer_id, lease })
                    .await;
                Err(refusal)
            }
        }
    }

    /// Release one tracked connection through the shared core.
    ///
    /// The request is validated against the mapping without mutating it
    /// (PR210-10): a wrong identity leaves every mapping untouched and
    /// makes no native call. The core verdict then decides — a confirmed or
    /// already-ended release removes the connection and its GATT mappings;
    /// a failure keeps them for a real retry (PR210-09). The resulting
    /// `connection-lifecycle` transition reaches the connection-event
    /// streams from the core's lifecycle event, never from here. Unknown
    /// handle with a live caller is idempotent release; an unknown caller
    /// owns nothing.
    async fn disconnect(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "connectionHandle", "tauri.disconnect")?;
        let key = caller_key(caller);
        self.release_connection(&key, &handle, Some(&payload), ctl)
            .await?;
        Ok(released())
    }

    async fn release_connection(
        &self,
        key: &str,
        handle: &str,
        identity: Option<&BTreeMap<String, IpcValue>>,
        ctl: OpControl,
    ) -> Result<(), DispatchError> {
        let step = {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(key).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.disconnect-owner",
                )
            })?;
            let owner_lease_id = caller_state.lease_id.clone();
            let Some(connection) = caller_state.connections.get_mut(handle) else {
                return Ok(());
            };
            if let Some(payload) = identity {
                validate_connection_identity(
                    payload,
                    connection,
                    &owner_lease_id,
                    "tauri.disconnect",
                )?;
            }
            (
                begin_release(&mut connection.phase),
                connection.peer_id.clone(),
                connection.lease.clone(),
            )
        };
        let (sender, peer_id, lease) = match step {
            (ReleaseStep::Join(receiver), _, _) => return join_release(receiver).await,
            (ReleaseStep::Lead(sender), peer_id, lease) => (sender, peer_id, lease),
        };
        let result = match self.ensure_authority().await {
            Ok(authority) => match authority.disconnect(&peer_id, &lease, ctl).await {
                Ok(_) => Ok(()),
                Err(error) => {
                    let error = DispatchError::from_core(&error);
                    if is_released_link(&error) {
                        Ok(())
                    } else {
                        Err(error)
                    }
                }
            },
            Err(error) => Err(error),
        };
        let (detached, owner_lease) = {
            let mut state = self.inner.lock().await;
            match state.callers.get_mut(key) {
                Some(caller_state) if caller_state.connections.contains_key(handle) => {
                    if result.is_ok() {
                        caller_state.connections.remove(handle);
                        let lease = (
                            caller_state.lease_id.clone(),
                            caller_state.lease_generation.clone(),
                        );
                        (
                            Self::detach_connection_mappings(caller_state, handle),
                            Some(lease),
                        )
                    } else {
                        if let Some(connection) = caller_state.connections.get_mut(handle) {
                            connection.phase = ReleasePhase::ReleaseFailed;
                        }
                        (Vec::new(), None)
                    }
                }
                _ => (Vec::new(), None),
            }
        };
        // Finding 190a: the app released the link, so every detached
        // subscription ends with the vocabulary's requested-disconnect word
        // (`owner-released`, as on RN iOS/Android/tvOS) — never a bare close
        // the supervisor reads as a stop. The forwarder cannot race this:
        // the connection left `Active` at `begin_release`, so delivery stays
        // paused until the mappings below are gone.
        if let Some((owner_lease_id, owner_lease_generation)) = owner_lease {
            for (subscription_handle, _) in &detached {
                let _ = self
                    .terminal(
                        key,
                        (&owner_lease_id, &owner_lease_generation),
                        subscription_handle,
                        "owner-released",
                        None,
                    )
                    .await;
            }
        }
        for (_, subscription) in detached {
            if let Some(task) = subscription.task {
                task.abort();
            }
        }
        let _ = sender.send(Some(result.clone()));
        result
    }

    /// Drop the GATT mappings owned by one released connection handle and
    /// return the detached subscriptions for the caller to stop. The link's
    /// CCCDs ended with it in the core. Connection-event streams stay: the
    /// core's lifecycle event ends them with the transition that happened.
    fn detach_connection_mappings(
        caller_state: &mut CallerState,
        connection_handle: &str,
    ) -> Vec<(String, CoreSubscription)> {
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
                caller_state
                    .subscriptions
                    .remove(&subscription_handle)
                    .map(|subscription| (subscription_handle, subscription))
            })
            .collect::<Vec<_>>();
        caller_state
            .databases
            .retain(|_, database| database.connection_handle != connection_handle);
        subscriptions
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
        let attachment = caller_state.attachment.clone();
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
                core_generation: connection.core_generation.clone(),
                attachment,
                active: false,
                sequence: 0,
                end: StreamEnd::Open,
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
        let (event, pending_end) = {
            let mut state = self.inner.lock().await;
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
            // Events report the attachment the link lives on.
            let attachment = resource.attachment.clone();
            // A link that ended before the stream was ready is reported
            // right after the initial event; the delivery claims the end.
            let pending_end = match resource.end {
                StreamEnd::Pending(transition) => {
                    resource.end = StreamEnd::Claimed;
                    Some(Some(transition))
                }
                StreamEnd::PendingOverflow => {
                    resource.end = StreamEnd::Claimed;
                    Some(None)
                }
                StreamEnd::Open | StreamEnd::Claimed => None,
            };
            // Presence, not a handle: the link itself is core-owned, so
            // readiness only checks that the mapping still resolves. Link
            // loss reaches this stream from the core's lifecycle event.
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
                (
                    resource.stream_handle.clone(),
                    resource.peer_id.clone(),
                    resource.connection_id.clone(),
                    resource.connection_generation.clone(),
                    caller_state.lease_id.clone(),
                    resource.sequence,
                    attachment,
                    caller_state.lease_generation.clone(),
                ),
                pending_end,
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
            .emit(&key, Some((&event.4, &event.7)), &event.0, initial_event)
            .await
        {
            let mut state = self.inner.lock().await;
            if let Some(caller) = state.callers.get_mut(&key) {
                caller.connection_events.remove(&event.0);
            }
            return Err(error);
        }
        if let Some(end) = pending_end {
            self.spawn_stream_end(
                key,
                (event.4, event.7),
                StreamEndTarget {
                    stream_id: event.0,
                    peer_id: event.1,
                    connection_id: event.2,
                    connection_generation: event.3,
                },
                end,
            );
        }
        // No liveness monitor: the core senses link loss itself and
        // publishes it as a lifecycle event, which the lifecycle pump
        // forwards to this stream by its handle (PR210-11).
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
        caller_state.connection_events.remove(&stream_handle);
        Ok(released())
    }

    /// One lifecycle pump per authority, subscribed before any operation can
    /// run. Link loss, requested release and service changes reach the
    /// matching connection-event streams and databases by peer and
    /// connection generation; a stale generation matches nothing. A lagged
    /// receiver ends every connection-event stream with `overflow` instead
    /// of leaving a silent gap.
    fn start_lifecycle_pump(&self, authority: &Arc<dyn CoreAuthority>) {
        let mut events = authority.lifecycle_events();
        let mut scan_ends = authority.scan_terminal_events();
        let mut resets = authority.adapter_reset_events();
        let dispatcher = self.clone();
        let pump = tauri::async_runtime::spawn(async move {
            let (mut lifecycle_open, mut scan_ends_open, mut resets_open) = (true, true, true);
            while lifecycle_open || scan_ends_open || resets_open {
                tokio::select! {
                    reset = resets.recv(), if resets_open => match reset {
                        Ok(reset) => {
                            dispatcher
                                .rebind_callers(reset.previous.attachment_id().as_str(), &reset.current)
                                .await;
                        }
                        // A missed reset still left the central on its current
                        // attachment: rebind to that.
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            dispatcher.rebind_to_current().await;
                        }
                        Err(broadcast::error::RecvError::Closed) => resets_open = false,
                    },
                    event = events.recv(), if lifecycle_open => match event {
                        Ok(event) => dispatcher.apply_lifecycle_event(&event).await,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            dispatcher.overflow_connection_event_streams().await;
                        }
                        Err(broadcast::error::RecvError::Closed) => lifecycle_open = false,
                    },
                    ended = scan_ends.recv(), if scan_ends_open => match ended {
                        Ok(ended) => dispatcher.apply_scan_terminal(&ended).await,
                        Err(broadcast::error::RecvError::Lagged(missed)) => {
                            dispatcher.fail_unobserved_scans(missed).await;
                        }
                        Err(broadcast::error::RecvError::Closed) => scan_ends_open = false,
                    },
                }
            }
        });
        let previous = self
            .lifecycle_pump
            .lock()
            .expect("lifecycle pump mutex poisoned")
            .replace(pump);
        if let Some(previous) = previous {
            previous.abort();
        }
    }

    /// IPC protocol 4: an adapter reset replaced `previous` with `current`.
    /// The dispatcher (never a webview) rebinds every caller bound to
    /// `previous` and announces it on the `attachment` stream; until then
    /// work on `previous` is refused `backend.reset`, and afterwards too,
    /// releases excepted.
    async fn rebind_callers(&self, previous: &str, current: &AttachmentTuple) {
        let rebound = {
            let mut state = self.inner.lock().await;
            state.replaced_attachment_ids.insert(previous.to_owned());
            let adapter_name = state.adapter_name.clone();
            let mut rebound = Vec::new();
            for (key, caller_state) in &mut state.callers {
                if caller_state.retired || caller_state.attachment.attachment_id != previous {
                    continue;
                }
                let next = attachment_of(
                    current,
                    adapter_name
                        .clone()
                        .unwrap_or_else(|| caller_state.attachment.adapter_name.clone()),
                );
                let previous_id =
                    std::mem::replace(&mut caller_state.attachment, next.clone()).attachment_id;
                rebound.push((key.clone(), previous_id, next));
            }
            rebound
        };
        for (key, previous_id, next) in rebound {
            let value = object([
                ("kind", string("backend-restarted")),
                ("schemaVersion", number(1)),
                ("previousAttachmentId", string(previous_id)),
                ("attachmentId", string(next.attachment_id.clone())),
                ("attachment", attachment_record(&next)),
            ]);
            if let Err(error) = self.emit(&key, None, IPC_ATTACHMENT_STREAM_ID, value).await {
                // The caller keeps being refused on the replaced attachment
                // until it attaches again; the failure is reported.
                eprintln!(
                    "tauri-plugin-unified-ble-manager: attachment rebind for {key} was not delivered: {}",
                    error.describe()
                );
            }
        }
    }

    /// A reset event was missed: rebind every caller still bound to an
    /// attachment the central no longer holds.
    async fn rebind_to_current(&self) {
        let Some(authority) = self.authority_if_open() else {
            return;
        };
        let current = authority.attachment();
        let stale: HashSet<String> = {
            let state = self.inner.lock().await;
            state
                .callers
                .values()
                .filter(|caller_state| {
                    !caller_state.retired
                        && caller_state.attachment.attachment_id != current.attachment_id().as_str()
                })
                .map(|caller_state| caller_state.attachment.attachment_id.clone())
                .collect()
        };
        for previous in stale {
            self.rebind_callers(&previous, &current).await;
        }
    }

    /// The core ended a scan without a stop request: the OS stopped it, or
    /// an adapter loss took it (finding 57). The core already settled it
    /// and released its owner, so the mapping goes and the stream ends —
    /// `source-failed` with the core's own words when the scan was aborted,
    /// `closed` otherwise (the desktop NAPI provider's vocabulary). A scan
    /// whose release is in flight is left to that release.
    async fn apply_scan_terminal(&self, ended: &ScanTerminalEvent) {
        let target = {
            let mut state = self.inner.lock().await;
            state.callers.iter_mut().find_map(|(key, caller_state)| {
                let owned = caller_state.scan.as_ref().is_some_and(|scan| {
                    scan.core_operation_id == ended.operation_id && scan.phase.is_active()
                });
                if !owned {
                    return None;
                }
                let scan = caller_state.scan.take()?;
                if let Some(task) = scan.task {
                    task.abort();
                }
                Some((
                    key.clone(),
                    (
                        caller_state.lease_id.clone(),
                        caller_state.lease_generation.clone(),
                    ),
                    scan.handle,
                ))
            })
        };
        let Some((key, lease, handle)) = target else {
            return;
        };
        let (reason, error) = if ended.aborted {
            (
                "source-failed",
                Some(
                    DispatchError::new(
                        BleErrorCode::ScanStartFailed,
                        "scan",
                        "tauri.scan.terminated",
                    )
                    .platform(ended.detail.clone()),
                ),
            )
        } else {
            ("closed", None)
        };
        // A retired or re-leased caller has nobody left to tell.
        let _ = self
            .terminal(&key, (&lease.0, &lease.1), &handle, reason, error.as_ref())
            .await;
    }

    /// Scan-end reports were missed (the receiver lagged): which scan ended
    /// is unknown, so every mapped scan is stopped through the core by its
    /// own id and its stream ends `source-failed` with `stream.overflow` —
    /// never left delivering from a scan that may be gone.
    async fn fail_unobserved_scans(&self, missed: u64) {
        let scans: Vec<(String, (String, String), String)> = {
            let state = self.inner.lock().await;
            state
                .callers
                .iter()
                .filter_map(|(key, caller_state)| {
                    let scan = caller_state.scan.as_ref()?;
                    Some((
                        key.clone(),
                        (
                            caller_state.lease_id.clone(),
                            caller_state.lease_generation.clone(),
                        ),
                        scan.handle.clone(),
                    ))
                })
                .collect()
        };
        for (key, lease, handle) in scans {
            let mut error = DispatchError::new(
                BleErrorCode::StreamOverflow,
                "scan",
                "tauri.scan.terminal-events",
            )
            .platform(format!(
                "{missed} scan end reports were missed; the scan was stopped"
            ));
            if let Err(stop) = self
                .release_scan(&key, &handle, OpControl::unbounded())
                .await
            {
                error = error.platform(format!(
                    "{missed} scan end reports were missed; stopping the scan failed: {}",
                    stop.describe()
                ));
            }
            let _ = self
                .terminal(
                    &key,
                    (&lease.0, &lease.1),
                    &handle,
                    "source-failed",
                    Some(&error),
                )
                .await;
        }
    }

    async fn apply_lifecycle_event(&self, event: &LifecycleEvent) {
        // An event without a generation applied to no connection this
        // dispatcher mapped; it matches nothing.
        let Some(generation) = event.connection_generation.as_deref() else {
            return;
        };
        let transition = match event.kind {
            LifecycleKind::LinkLost => Some(LINK_LOST),
            LifecycleKind::Released { requested: true } => Some(LINK_RELEASED),
            LifecycleKind::Released { requested: false } => Some(LINK_ENDED_UNREQUESTED),
            LifecycleKind::AdapterLost => Some(LINK_ADAPTER_LOST),
            LifecycleKind::ServicesChanged => None,
        };
        let mut deliveries = Vec::new();
        {
            let mut state = self.inner.lock().await;
            for (caller_key, caller_state) in &mut state.callers {
                if caller_state.retired {
                    continue;
                }
                let connection_handles: HashSet<String> = caller_state
                    .connections
                    .iter()
                    .filter(|(_, connection)| {
                        connection.peer_id == event.peer_id
                            && connection.core_generation == generation
                    })
                    .map(|(handle, _)| handle.clone())
                    .collect();
                // The database of an ended or changed link is stale either way.
                for database in caller_state.databases.values_mut() {
                    if connection_handles.contains(&database.connection_handle) {
                        database.valid = false;
                    }
                }
                let Some(transition) = transition else {
                    continue;
                };
                let lease = (
                    caller_state.lease_id.clone(),
                    caller_state.lease_generation.clone(),
                );
                for resource in caller_state.connection_events.values_mut() {
                    if resource.peer_id != event.peer_id
                        || resource.core_generation != generation
                        || resource.end != StreamEnd::Open
                    {
                        continue;
                    }
                    if resource.active {
                        resource.end = StreamEnd::Claimed;
                        deliveries.push((
                            caller_key.clone(),
                            lease.clone(),
                            StreamEndTarget::of(resource),
                        ));
                    } else {
                        resource.end = StreamEnd::Pending(transition);
                    }
                }
            }
        }
        if let Some(transition) = transition {
            for (caller_key, lease, target) in deliveries {
                self.spawn_stream_end(caller_key, lease, target, Some(transition));
            }
        }
    }

    async fn overflow_connection_event_streams(&self) {
        let mut deliveries = Vec::new();
        {
            let mut state = self.inner.lock().await;
            for (caller_key, caller_state) in &mut state.callers {
                if caller_state.retired {
                    continue;
                }
                let lease = (
                    caller_state.lease_id.clone(),
                    caller_state.lease_generation.clone(),
                );
                for resource in caller_state.connection_events.values_mut() {
                    if resource.end != StreamEnd::Open {
                        continue;
                    }
                    if resource.active {
                        resource.end = StreamEnd::Claimed;
                        deliveries.push((
                            caller_key.clone(),
                            lease.clone(),
                            StreamEndTarget::of(resource),
                        ));
                    } else {
                        resource.end = StreamEnd::PendingOverflow;
                    }
                }
            }
        }
        for (caller_key, lease, target) in deliveries {
            self.spawn_stream_end(caller_key, lease, target, None);
        }
    }

    /// Deliver one stream's end on its own task: the transition (when
    /// there is one) then the terminal, keyed by the stream handle the
    /// renderer minted. `None` ends the stream with `overflow`.
    fn spawn_stream_end(
        &self,
        caller_key: String,
        lease: (String, String),
        target: StreamEndTarget,
        transition: Option<LinkTransition>,
    ) {
        let dispatcher = self.clone();
        tauri::async_runtime::spawn(async move {
            let identity = ConnectionEventIdentity {
                stream_id: &target.stream_id,
                peer_id: &target.peer_id,
                connection_id: &target.connection_id,
                connection_generation: &target.connection_generation,
            };
            // Ownership denial means release took the stream over; there is
            // nobody left to tell.
            let _ = match transition {
                Some(transition) => {
                    dispatcher
                        .emit_connection_transition(
                            &caller_key,
                            (&lease.0, &lease.1),
                            identity,
                            transition,
                        )
                        .await
                }
                None => {
                    dispatcher
                        .end_connection_event_stream(
                            &caller_key,
                            (&lease.0, &lease.1),
                            &target.stream_id,
                            "overflow",
                        )
                        .await
                }
            };
        });
    }

    async fn discover(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let connection_handle = required_string(&payload, "connectionHandle", "tauri.discover")?;
        let connection = self.connection(caller, &payload, "tauri.discover").await?;
        // Core first: discovery runs in the core and registers the whole
        // tree there or fails whole (finding 95); the dispatcher only
        // renders the registered paths into the IPC wire shape.
        let authority = self.ensure_authority().await?;
        authority
            .discover(&connection.peer_id, &connection.lease, ctl)
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
        // One IPC handle per characteristic identity, as the desktop N-API
        // path renders it (`groupCorePaths` merges descriptor rows into
        // their characteristic node). Descriptor-level core paths repeat
        // their characteristic's identity, so they must not mint a second
        // characteristic record — that fanned every characteristic with a
        // descriptor out once per descriptor and rejected the snapshot
        // downstream with public-gatt.duplicate-characteristic-path
        // (finding 182: every Polar H10 CCCD bearer).
        let mut characteristic_handles = HashMap::new();
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
        }
        // Characteristic rows render only from characteristic-level core
        // paths, so the record carries the characteristic's own property
        // bits (descriptor-level rows repeat the identity with the
        // descriptor row's bits). Two passes keep this independent of row
        // order in the whole-tree read.
        for path in &paths {
            let Some(characteristic_uuid) = path.characteristic_uuid.clone() else {
                continue;
            };
            if path.descriptor_uuid.is_some() {
                continue;
            }
            let characteristic_occurrence = path.characteristic_occurrence.unwrap_or(0);
            let key = (
                path.service_uuid.clone(),
                path.service_occurrence,
                characteristic_uuid.clone(),
                characteristic_occurrence,
            );
            if characteristic_handles.contains_key(&key) {
                continue;
            }
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
            characteristic_map.insert(
                characteristic_handle.clone(),
                CoreCharacteristic {
                    selector,
                    properties: path.properties,
                },
            );
            characteristic_handles.insert(key, characteristic_handle);
        }
        for path in &paths {
            let Some(descriptor_uuid) = path.descriptor_uuid.clone() else {
                continue;
            };
            let Some(characteristic_uuid) = path.characteristic_uuid.clone() else {
                continue;
            };
            let characteristic_occurrence = path.characteristic_occurrence.unwrap_or(0);
            let key = (
                path.service_uuid.clone(),
                path.service_occurrence,
                characteristic_uuid.clone(),
                characteristic_occurrence,
            );
            let Some(characteristic_handle) = characteristic_handles.get(&key).cloned() else {
                // The whole-tree read registers a descriptor under its
                // characteristic, so a descriptor row without one is a core
                // invariant violation, never an empty record: fail closed.
                return Err(DispatchError::new(
                    BleErrorCode::ProtocolViolation,
                    "gatt",
                    "tauri.discover-descriptor-parent",
                ));
            };
            let descriptor_occurrence = path.descriptor_occurrence.unwrap_or(0);
            let descriptor_handle = self.id("descriptor");
            descriptor_records.push(object([
                ("handle", string(descriptor_handle.clone())),
                ("characteristicHandle", string(characteristic_handle)),
                ("uuid", string(descriptor_uuid.clone())),
                ("occurrence", string(descriptor_occurrence.to_string())),
            ]));
            descriptor_map.insert(
                descriptor_handle,
                CoreSelector {
                    service_uuid: path.service_uuid.clone(),
                    service_occurrence: Some(path.service_occurrence),
                    characteristic_uuid: Some(characteristic_uuid),
                    characteristic_occurrence: Some(characteristic_occurrence),
                    descriptor_uuid: Some(descriptor_uuid),
                    descriptor_occurrence: Some(descriptor_occurrence),
                },
            );
        }
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
        ]))
    }

    async fn release_database(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        // Mapping-only: discovery trees live in the core, so releasing is
        // dropping the transport mapping — nothing native can fail, and the
        // caller is validated before anything is removed. Unknown handle
        // with a live caller is idempotent release.
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
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let target = self.gatt_target(caller, &payload).await?;
        let authority = self.ensure_authority().await?;
        let read = authority
            .read(&target.peer_id, &target.characteristic.selector, ctl)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        Ok(object([
            ("value", IpcValue::Bytes(read.value)),
            ("provenance", string(read.provenance.as_str())),
        ]))
    }

    async fn write(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        bytes: Option<Vec<u8>>,
        ctl: OpControl,
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
        let target = self.gatt_target(caller, &payload).await?;
        // Core first: MTU, properties, and the deadline are all core-owned.
        let authority = self.ensure_authority().await?;
        authority
            .write(
                &target.peer_id,
                &target.characteristic.selector,
                bytes.clone(),
                &mode,
                ctl,
            )
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let write_correlation = self.id("write-operation");
        // The contract `WriteReceipt` allows only `confirmed`/`unknown`: an
        // unconfirmed write reports `unknown` on every host (findings F1/F2).
        let commit_state = if mode == "with-response" {
            "confirmed"
        } else {
            "unknown"
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

    /// Subscribe one consumer through the core.
    ///
    /// Delivery mode (FIX-PLAN decision 3, the 4.x contract): preferences
    /// ride through without a requirement; a hard requirement is refused
    /// with `gatt.property-not-supported` only when the characteristic lacks
    /// that property, and is otherwise carried to the core, which has the
    /// radio write that CCCD mode or refuse it before any effect. The
    /// response reports the delivery the radio observed.
    async fn subscribe(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let requirement = match payload.get("deliveryMode") {
            None | Some(IpcValue::Null) => None,
            Some(value) => match as_string(value) {
                Some("prefer-notification" | "prefer-indication") => None,
                Some("require-notification") => Some(DeliveryMode::Notification),
                Some("require-indication") => Some(DeliveryMode::Indication),
                _ => {
                    return Err(DispatchError::new(
                        BleErrorCode::ArgumentInvalid,
                        "gatt",
                        "tauri.subscribe.delivery-mode",
                    ));
                }
            },
        };
        let target = self.gatt_target(caller, &payload).await?;
        if let Some(required) = requirement {
            let (property, operation) = match required {
                DeliveryMode::Notification => (
                    ubm_core::central::GATT_PROP_NOTIFY,
                    "tauri.subscribe.notification",
                ),
                DeliveryMode::Indication => (
                    ubm_core::central::GATT_PROP_INDICATE,
                    "tauri.subscribe.indication",
                ),
            };
            if target.characteristic.properties & property == 0 {
                return Err(DispatchError::new(
                    BleErrorCode::GattPropertyNotSupported,
                    "gatt",
                    operation,
                ));
            }
        }
        let key = caller_key(caller);
        let lease = expected_lease(&payload, "tauri.subscribe-lease")?;
        // Core first: enablement is core-arbitrated (concurrent subscribers
        // share one physical enable); the consumer below addresses it.
        let authority = self.ensure_authority().await?;
        // The core consumer is internal: it takes no number from the 4.x counter.
        let consumer = self.internal_id("consumer");
        let selector = target.characteristic.selector.clone();
        let delivery = authority
            .subscribe(&target.peer_id, &selector, &consumer, requirement, ctl)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let handle = self.id("subscription");
        // Publication is one decision under the dispatcher lock, and the
        // pump spawns only once its entry exists (PR210-07). An enablement
        // nobody can own is released by its exact consumer (PR210-08).
        let refusal = {
            let mut state = self.inner.lock().await;
            match state.callers.get_mut(&key) {
                None => Some(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "gatt",
                    "tauri.subscribe-owner",
                )),
                Some(caller_state) if caller_state.retired => Some(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "gatt",
                    "tauri.subscribe-owner",
                )),
                Some(caller_state) if !lease_matches(caller_state, &lease) => {
                    Some(DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "gatt",
                        "tauri.subscribe-stale-lease",
                    ))
                }
                Some(caller_state)
                    if !caller_state
                        .connections
                        .get(&target.connection_handle)
                        .is_some_and(|connection| connection.phase.is_active()) =>
                {
                    Some(DispatchError::new(
                        BleErrorCode::ConnectionStale,
                        "connection",
                        "tauri.subscribe-connection",
                    ))
                }
                Some(caller_state) => {
                    caller_state.subscriptions.insert(
                        handle.clone(),
                        CoreSubscription {
                            connection_handle: target.connection_handle.clone(),
                            peer_id: target.peer_id.clone(),
                            selector: selector.clone(),
                            consumer: consumer.clone(),
                            delivery,
                            task: None,
                            phase: ReleasePhase::Active,
                        },
                    );
                    let task = self.spawn_notification_forwarder(
                        Arc::clone(&authority),
                        key.clone(),
                        handle.clone(),
                        lease.clone(),
                    );
                    if let Some(subscription) = caller_state.subscriptions.get_mut(&handle) {
                        subscription.task = Some(task);
                    }
                    None
                }
            }
        };
        if let Some(refusal) = refusal {
            self.compensate(
                &authority,
                &key,
                OrphanResource::Subscription {
                    peer_id: target.peer_id,
                    selector,
                    consumer,
                },
            )
            .await;
            return Err(refusal);
        }
        Ok(object([
            ("handle", string(handle)),
            ("delivery", string(delivery.as_str())),
        ]))
    }

    /// Verbatim notification delivery: the pump polls the core with a
    /// typed outcome and emits values unchanged, each carrying the
    /// delivery the radio reported for the enablement. The stream ends
    /// with the core's own reason — `service-changed`, `connection-lost`,
    /// `overflow`, or `source-failed` when the core closed a stream that is
    /// still mapped. Sequence numbers are transport-side delivery ordinals,
    /// not radio facts. It delivers only while the subscription and its
    /// connection are `Active`.
    fn spawn_notification_forwarder(
        &self,
        authority: Arc<dyn CoreAuthority>,
        key: String,
        handle: String,
        lease: (String, String),
    ) -> TauriJoinHandle<()> {
        let dispatcher = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut sequence = 0_u64;
            loop {
                let target = match dispatcher
                    .subscription_delivery(&key, &handle, &lease)
                    .await
                {
                    (Delivery::Gone, _) => return,
                    (Delivery::Paused, _) | (Delivery::Active, None) => {
                        tokio::time::sleep(FORWARD_POLL_INTERVAL).await;
                        continue;
                    }
                    (Delivery::Active, Some(target)) => target,
                };
                let (peer_id, selector, consumer, delivery) = target;
                let ending = match authority
                    .poll_notification(&peer_id, &selector, &consumer)
                    .await
                {
                    Ok(NotificationPoll::Value(value)) => {
                        sequence = sequence.saturating_add(1);
                        let observed_at_monotonic_ms =
                            i64::try_from(dispatcher.started_at.elapsed().as_millis())
                                .unwrap_or(i64::MAX);
                        match dispatcher
                            .emit(
                                &key,
                                Some((&lease.0, &lease.1)),
                                &handle,
                                object([
                                    ("value", IpcValue::Bytes(value)),
                                    ("delivery", string(delivery.as_str())),
                                    ("observedAtMonotonicMs", number(observed_at_monotonic_ms)),
                                    ("sequence", number(sequence as i64)),
                                ]),
                            )
                            .await
                        {
                            Ok(()) => continue,
                            Err(error) => ("source-failed", Some(error)),
                        }
                    }
                    Ok(NotificationPoll::Empty) => {
                        tokio::time::sleep(FORWARD_POLL_INTERVAL).await;
                        continue;
                    }
                    Ok(NotificationPoll::Terminal(_)) => ("overflow", None),
                    Ok(NotificationPoll::Invalidated(InvalidationCause::ServicesChanged)) => {
                        ("service-changed", None)
                    }
                    Ok(NotificationPoll::Invalidated(InvalidationCause::LinkEnded)) => {
                        ("connection-lost", None)
                    }
                    // The adapter was lost under the stream (finding 57):
                    // the source failed, with the core's answer for live
                    // work a reset ended.
                    Ok(NotificationPoll::Invalidated(InvalidationCause::AdapterReset)) => (
                        "source-failed",
                        Some(
                            DispatchError::new(
                                BleErrorCode::OperationReset,
                                "adapter",
                                "tauri.notifications",
                            )
                            .platform("the adapter was lost under the subscription"),
                        ),
                    ),
                    Ok(NotificationPoll::Closed) => (
                        "source-failed",
                        Some(DispatchError::new(
                            BleErrorCode::StreamClosed,
                            "stream",
                            "tauri.notifications",
                        )),
                    ),
                    // Core-side failure ends delivery with the verbatim core
                    // verdict (never a guessed stream error, never silent).
                    Err(error) => ("source-failed", Some(DispatchError::from_core(&error))),
                };
                let (reason, error) = ending;
                dispatcher
                    .terminal(&key, (&lease.0, &lease.1), &handle, reason, error.as_ref())
                    .await
                    .ok();
                return;
            }
        })
    }

    async fn subscription_delivery(
        &self,
        key: &str,
        handle: &str,
        lease: &(String, String),
    ) -> (
        Delivery,
        Option<(String, CoreSelector, String, ObservedDelivery)>,
    ) {
        let state = self.inner.lock().await;
        let Some(caller_state) = state
            .callers
            .get(key)
            .filter(|caller| !caller.retired && lease_matches(caller, lease))
        else {
            return (Delivery::Gone, None);
        };
        let Some(subscription) = caller_state.subscriptions.get(handle) else {
            return (Delivery::Gone, None);
        };
        let connection_active = caller_state
            .connections
            .get(&subscription.connection_handle)
            .is_some_and(|connection| connection.phase.is_active());
        if !subscription.phase.is_active() || !connection_active {
            return (Delivery::Paused, None);
        }
        (
            Delivery::Active,
            Some((
                subscription.peer_id.clone(),
                subscription.selector.clone(),
                subscription.consumer.clone(),
                subscription.delivery,
            )),
        )
    }

    async fn unsubscribe(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "subscriptionHandle", "tauri.unsubscribe")?;
        let key = caller_key(caller);
        {
            let state = self.inner.lock().await;
            if !state.callers.contains_key(&key) {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "gatt",
                    "tauri.unsubscribe-owner",
                ));
            }
        }
        self.release_subscription(&key, &handle, ctl).await?;
        Ok(released())
    }

    /// Remove one mapped consumer through the core (PR210-09). Delivery
    /// pauses while the release runs; the mapping is removed only when the
    /// core confirms it or answers that the consumer is already gone, so a
    /// failed disable keeps the exact consumer a retry needs. An unknown
    /// handle is already released.
    async fn release_subscription(
        &self,
        key: &str,
        handle: &str,
        ctl: OpControl,
    ) -> Result<(), DispatchError> {
        let step = {
            let mut state = self.inner.lock().await;
            let Some(subscription) = state
                .callers
                .get_mut(key)
                .and_then(|caller_state| caller_state.subscriptions.get_mut(handle))
            else {
                return Ok(());
            };
            (
                begin_release(&mut subscription.phase),
                subscription.peer_id.clone(),
                subscription.selector.clone(),
                subscription.consumer.clone(),
            )
        };
        let (sender, peer_id, selector, consumer) = match step {
            (ReleaseStep::Join(receiver), ..) => return join_release(receiver).await,
            (ReleaseStep::Lead(sender), peer_id, selector, consumer) => {
                (sender, peer_id, selector, consumer)
            }
        };
        let result = match self.ensure_authority().await {
            Ok(authority) => {
                release_consumer(&authority, &peer_id, &selector, &consumer, ctl).await
            }
            Err(error) => Err(error),
        };
        let finished = {
            let mut state = self.inner.lock().await;
            state
                .callers
                .get_mut(key)
                .and_then(|caller_state| {
                    if result.is_ok() {
                        caller_state.subscriptions.remove(handle)
                    } else {
                        if let Some(subscription) = caller_state.subscriptions.get_mut(handle) {
                            subscription.phase = ReleasePhase::ReleaseFailed;
                        }
                        None
                    }
                })
                .and_then(|subscription| subscription.task)
        };
        if let Some(task) = finished {
            task.abort();
        }
        let _ = sender.send(Some(result.clone()));
        result
    }

    async fn read_descriptor(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let (peer_id, selector) = self
            .descriptor_target(caller, &payload, "tauri.descriptor-read")
            .await?;
        let authority = self.ensure_authority().await?;
        let value = authority
            .read_descriptor(&peer_id, &selector, ctl)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        Ok(object([("value", IpcValue::Bytes(value))]))
    }

    async fn write_descriptor(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        bytes: Option<Vec<u8>>,
        ctl: OpControl,
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
            .write_descriptor(&peer_id, &selector, bytes.clone(), ctl)
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

    /// Connected RSSI through the core, for the lease holding the link: the
    /// OS measurement, never a cached advertisement value. A radio that
    /// cannot measure it answers `capability.unsupported` verbatim.
    async fn read_rssi(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let connection = self.connection(caller, &payload, "tauri.rssi").await?;
        let authority = self.ensure_authority().await?;
        let rssi = authority
            .read_rssi(&connection.peer_id, &connection.lease, ctl)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        Ok(object([("rssi", number(i64::from(rssi)))]))
    }

    /// The largest single write the OS accepts on this link for the
    /// requested mode, answered by the core (finding 90): the same limit a
    /// write of that mode is admitted against, so the reported maximum is
    /// never refused. With an OS long write (Windows, Linux) a
    /// with-response write reaches 512 bytes; without response it stays one
    /// ATT payload. An unmeasured limit fails loudly, never guessed.
    async fn maximum_write_length(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let with_response =
            match required_string(&payload, "mode", "tauri.maximum-write-length")?.as_str() {
                "with-response" => true,
                "without-response" => false,
                _ => {
                    return Err(DispatchError::new(
                        BleErrorCode::ArgumentInvalid,
                        "gatt",
                        "tauri.maximum-write-length-mode",
                    ))
                }
            };
        let connection = self
            .connection(caller, &payload, "tauri.maximum-write-length")
            .await?;
        let authority = self.ensure_authority().await?;
        let bytes = authority
            .connection_maximum_write_length(
                &connection.peer_id,
                &connection.lease,
                with_response,
                ctl,
            )
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        Ok(object([(
            "bytes",
            number(i64::try_from(bytes).unwrap_or(i64::MAX)),
        )]))
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
        Ok(connection.clone())
    }

    /// Resolve `(databaseHandle, characteristicHandle)` to the exact
    /// `(peer_id, selector)` the core addresses plus the characteristic's
    /// registered properties, after full identity admission (database
    /// identity, database validity, connection presence, connection
    /// identity). Every failure pins its own per-path identity.
    async fn gatt_target(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<GattTarget, DispatchError> {
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
        let characteristic = database
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
        Ok(GattTarget {
            peer_id: connection.peer_id.clone(),
            characteristic,
            connection_handle: database.connection_handle.clone(),
        })
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
    ) -> Result<(), DispatchError> {
        let (sink, lease_id, lease_generation, event_id) = {
            let mut state = self.inner.lock().await;
            let caller_state = state
                .callers
                .get_mut(caller_key)
                .filter(|caller_state| !caller_state.retired)
                .ok_or_else(|| {
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
                // Scan forwarders treat quota as a dropped observation;
                // every other stream ends on it.
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

    /// Report one link transition on one connection-event stream, keyed by
    /// the stream handle the renderer minted (PR210-11: never the
    /// connection handle), then end the stream with the transition's
    /// terminal reason.
    async fn emit_connection_transition(
        &self,
        caller_key: &str,
        expected_lease: (&str, &str),
        identity: ConnectionEventIdentity<'_>,
        transition: LinkTransition,
    ) -> Result<(), DispatchError> {
        let event = {
            let mut state = self.inner.lock().await;
            let caller = state.callers.get_mut(caller_key).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.connection-events-owner",
                )
            })?;
            // The transition reports the attachment the link lived on (an
            // adapter loss ends it there, not on the attachment that
            // replaced it), and the record reads nothing from the OS.
            if caller.lease_id != expected_lease.0 || caller.lease_generation != expected_lease.1 {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "connection",
                    "tauri.connection-events-stale-lease",
                ));
            }
            let owner_lease_id = caller.lease_id.clone();
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
                ("attachment", attachment_record(&resource.attachment)),
                (
                    "attachmentId",
                    string(resource.attachment.attachment_id.clone()),
                ),
                ("peerId", string(identity.peer_id)),
                ("connectionId", string(identity.connection_id)),
                (
                    "connectionGeneration",
                    string(identity.connection_generation),
                ),
                ("ownerLeaseId", string(owner_lease_id)),
                ("sequence", number(resource.sequence as i64)),
                ("backendIngressOrdinal", IpcValue::Null),
                ("previous", string(transition.previous)),
                ("current", string(transition.current)),
                ("cause", string(transition.cause)),
            ])
        };
        let send_result = self
            .emit(caller_key, Some(expected_lease), identity.stream_id, event)
            .await;
        // An event the queue could not take still ends the stream: with
        // `overflow`, so the renderer knows it missed the transition.
        let terminal_reason = if send_result.is_ok() {
            transition.terminal
        } else {
            "overflow"
        };
        self.end_connection_event_stream(
            caller_key,
            expected_lease,
            identity.stream_id,
            terminal_reason,
        )
        .await?;
        send_result
    }

    /// End one connection-event stream with `reason`, then drop its
    /// mapping. A full acknowledgement queue cannot silently remove the
    /// lifecycle source: the terminal is retried until it is delivered or
    /// ownership denial proves release has taken over.
    async fn end_connection_event_stream(
        &self,
        caller_key: &str,
        expected_lease: (&str, &str),
        stream_id: &str,
        reason: &str,
    ) -> Result<(), DispatchError> {
        let mut terminal_delay = Duration::from_millis(100);
        loop {
            match self
                .terminal(caller_key, expected_lease, stream_id, reason, None)
                .await
            {
                Ok(()) => break,
                Err(error) if error.code == BleErrorCode::OwnershipDenied => return Err(error),
                Err(_error) => {
                    tokio::time::sleep(terminal_delay).await;
                    terminal_delay =
                        std::cmp::min(terminal_delay.saturating_mul(2), Duration::from_secs(5));
                }
            }
        }
        let mut state = self.inner.lock().await;
        if let Some(caller) = state.callers.get_mut(caller_key) {
            if caller.lease_id == expected_lease.0 && caller.lease_generation == expected_lease.1 {
                caller.connection_events.remove(stream_id);
            }
        }
        Ok(())
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
            let caller_state = state
                .callers
                .get_mut(caller_key)
                .filter(|caller_state| !caller_state.retired)
                .ok_or_else(|| {
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

    /// Release everything one caller owns, through the shared core.
    ///
    /// The caller is retired first — it admits no new work and receives no
    /// events — but stays mapped with its resources until each native
    /// release is confirmed (PR210-09). A failed release keeps the failed
    /// resources and the caller, so the next release really calls native
    /// again; orphan debt owed by this caller key is retried and reported
    /// here too (PR210-08). The caller leaves the map only when nothing
    /// failed.
    async fn release(&self, key: &str) -> IpcValue {
        let present = {
            let mut state = self.inner.lock().await;
            match state.callers.get_mut(key) {
                Some(caller_state) => {
                    caller_state.retired = true;
                    true
                }
                None => false,
            }
        };
        let mut failures = Vec::new();
        if present {
            failures.extend(self.settle_caller(key).await);
        }
        let owes_debt = self
            .inner
            .lock()
            .await
            .orphan_debt
            .iter()
            .any(|debt| debt.caller_key == key);
        if owes_debt {
            match self.ensure_authority().await {
                Ok(authority) => {
                    for failure in self.settle_orphan_debt(&authority, Some(key)).await {
                        failures.push(cleanup_failure(
                            failure.resource.kind(),
                            failure.operation(),
                            failure.describe(),
                        ));
                    }
                }
                Err(error) => failures.push(cleanup_failure(
                    "release",
                    "tauri.release.authority",
                    error.describe(),
                )),
            }
        }
        if present && failures.is_empty() {
            self.inner.lock().await.callers.remove(key);
        }
        cleanup_record(failures)
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

    /// Tear down everything one retired caller owns, through the shared
    /// core: cancel its in-flight operations, stop its scan, remove its
    /// consumers, release its links. Every radio verdict is core-made and
    /// each resource's mapping is removed only on a confirmed or
    /// already-gone release; every other outcome is returned as an explicit
    /// cleanup failure with the resource kept, never silence.
    async fn settle_caller(&self, key: &str) -> Vec<IpcValue> {
        let (tickets, scan, subscriptions, connections) = {
            let mut state = self.inner.lock().await;
            let Some(caller_state) = state.callers.get_mut(key) else {
                return Vec::new();
            };
            // Delivery-only mappings: the renderer they serve is leaving.
            caller_state.connection_events.clear();
            (
                caller_state
                    .operations
                    .values()
                    .map(|tracked| tracked.control.ticket.clone())
                    .collect::<Vec<_>>(),
                caller_state.scan.as_ref().map(|scan| scan.handle.clone()),
                caller_state
                    .subscriptions
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>(),
                caller_state.connections.keys().cloned().collect::<Vec<_>>(),
            )
        };
        let mut failures = Vec::new();
        for ticket in tickets {
            if let Err(error) = self.cancel_ticket(&ticket).await {
                failures.push(cleanup_failure(
                    "operation",
                    "tauri.release.operation",
                    error.describe(),
                ));
            }
        }
        if let Some(handle) = scan {
            if let Err(error) = self
                .release_scan(key, &handle, OpControl::unbounded())
                .await
            {
                failures.push(cleanup_failure(
                    "scan",
                    "tauri.release.scan",
                    error.describe(),
                ));
            }
        }
        for handle in subscriptions {
            if let Err(error) = self
                .release_subscription(key, &handle, OpControl::unbounded())
                .await
            {
                failures.push(cleanup_failure(
                    "subscription",
                    "tauri.release.subscription",
                    format!("{handle}: {}", error.describe()),
                ));
            }
        }
        for handle in connections {
            if let Err(error) = self
                .release_connection(key, &handle, None, OpControl::unbounded())
                .await
            {
                failures.push(cleanup_failure(
                    "connection",
                    "tauri.release.connection",
                    format!("{handle}: {}", error.describe()),
                ));
            }
        }
        failures
    }
}

/// One resolved characteristic target for a GATT verb.
struct GattTarget {
    peer_id: String,
    characteristic: CoreCharacteristic,
    connection_handle: String,
}

/// The owned identity of one connection-event stream whose end is being
/// delivered.
struct StreamEndTarget {
    stream_id: String,
    peer_id: String,
    connection_id: String,
    connection_generation: String,
}

impl StreamEndTarget {
    fn of(resource: &ConnectionEventResource) -> Self {
        Self {
            stream_id: resource.stream_handle.clone(),
            peer_id: resource.peer_id.clone(),
            connection_id: resource.connection_id.clone(),
            connection_generation: resource.connection_generation.clone(),
        }
    }
}

/// Remove one consumer through the core. A consumer the core no longer
/// resolves is already released.
async fn release_consumer(
    authority: &Arc<dyn CoreAuthority>,
    peer_id: &str,
    selector: &CoreSelector,
    consumer: &str,
    ctl: OpControl,
) -> Result<(), DispatchError> {
    match authority
        .unsubscribe(peer_id, selector, consumer, ctl)
        .await
    {
        Ok(_) => Ok(()),
        Err(error) => {
            let error = DispatchError::from_core(&error);
            if is_released_subscription(&error) {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

/// Release one orphaned core resource by its exact core identity.
async fn release_orphan(
    authority: &Arc<dyn CoreAuthority>,
    resource: &OrphanResource,
) -> Result<(), DispatchError> {
    match resource {
        OrphanResource::Scan(scan_id) => authority
            .stop_scan(scan_id, OpControl::unbounded())
            .await
            .map(|_| ())
            .map_err(|error| DispatchError::from_core(&error)),
        OrphanResource::Link { peer_id, lease } => {
            match authority
                .disconnect(peer_id, lease, OpControl::unbounded())
                .await
            {
                Ok(_) => Ok(()),
                Err(error) => {
                    let error = DispatchError::from_core(&error);
                    if is_released_link(&error) {
                        Ok(())
                    } else {
                        Err(error)
                    }
                }
            }
        }
        OrphanResource::Subscription {
            peer_id,
            selector,
            consumer,
        } => {
            release_consumer(
                authority,
                peer_id,
                selector,
                consumer,
                OpControl::unbounded(),
            )
            .await
        }
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

fn prune_completed_correlations(completed: &mut HashMap<String, Instant>, now: Instant) {
    completed
        .retain(|_, completed_at| now.duration_since(*completed_at) < COMPLETED_CORRELATION_TTL);
}

/// Commands that only release what a caller already holds (or cancel its
/// own in-flight work). They stay admitted for an attachment an adapter
/// reset ended, so the renderer can settle its handles; every other
/// command on that attachment is refused `backend.reset`.
fn is_release_command(command: &str) -> bool {
    matches!(
        command,
        "operation.cancel"
            | "scan.stop"
            | "gatt.unsubscribe"
            | "gatt.database.release"
            | "connection.disconnect"
            | "connection.events.unsubscribe"
    )
}

/// A route on the attachment an adapter reset replaced (finding 57): the
/// backend the renderer bound to was reset and every handle it holds is
/// foreign now. Refused before any native I/O; the renderer recreates its
/// manager (re-attach) to reach the new generation.
fn stale_attachment(bound: &Attachment, current: &AttachmentTuple) -> DispatchError {
    let mut error = DispatchError::new(
        BleErrorCode::BackendReset,
        "adapter",
        "tauri.route-attachment",
    )
    .platform(format!(
        "the adapter was reset: attachment {} (backend generation {}) ended; the current \
         attachment is {} (backend generation {}); re-attach to continue",
        bound.attachment_id,
        bound.backend_generation,
        current.attachment_id().as_str(),
        current.backend_generation().as_str(),
    ));
    error.commit = Some(CommitState::NotDispatched);
    error
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
        // Backpressure, not a protocol violation: `stream.quota` is the
        // code whose recovery is retry-with-backoff. Retryability stays
        // `never` — the wire vocabulary reserves `caller-decides` for an
        // aborted or timed-out operation the core never dispatched.
        return Err(DispatchError::new(
            BleErrorCode::StreamQuota,
            "ipc",
            "tauri.correlation-busy",
        ));
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

/// The renderer lease `execute()` admitted this route under.
fn expected_lease(
    payload: &BTreeMap<String, IpcValue>,
    operation: &'static str,
) -> Result<(String, String), DispatchError> {
    Ok((
        required_string(payload, "__expectedLeaseId", operation)?,
        required_string(payload, "__expectedLeaseGeneration", operation)?,
    ))
}

/// Whether the caller still holds `lease` (a caller key survives reattach,
/// so a live entry may belong to a newer lease).
fn lease_matches(caller: &CallerState, lease: &(String, String)) -> bool {
    caller.lease_id == lease.0 && caller.lease_generation == lease.1
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

/// A core fact read: the value, or `None` with the radio's reason when it
/// cannot answer that question on this platform. Every other failure
/// crosses verbatim.
fn unless_unsupported<T>(
    outcome: Result<T, DesktopError>,
    what: &str,
    reasons: &mut Vec<String>,
) -> Result<Option<T>, DispatchError> {
    match outcome {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == BleErrorCode::CapabilityUnsupported => {
            reasons.push(format!(
                "The {what} is not readable on this radio: {}.",
                error.detail().unwrap_or("unsupported")
            ));
            Ok(None)
        }
        Err(error) => Err(DispatchError::from_core(&error)),
    }
}

/// The IPC attachment for one core attachment tuple.
fn attachment_of(tuple: &AttachmentTuple, adapter_name: String) -> Attachment {
    Attachment {
        attachment_id: tuple.attachment_id().as_str().to_owned(),
        backend_instance_id: tuple.backend_instance_id().as_str().to_owned(),
        backend_generation: tuple.backend_generation().as_str().to_owned(),
        adapter_id: tuple.adapter_id().as_str().to_owned(),
        adapter_name,
        adapter_generation: tuple.adapter_generation().as_str().to_owned(),
    }
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
/// Both are properties of this dispatcher, verifiable in this file: the
/// shared central opens on one adapter (`btleplug_opener`) and an adapter
/// reset keeps that adapter, moving only its generations; the only messages
/// this host pushes through an event sink are stream `value` and `terminal`
/// messages, never an adapter-state change.
const ADAPTER_LIMITATIONS: [&str; 2] = [
    "This host binds one adapter for the lifetime of the attachment; the adapter is selected when the attachment is created and other adapters are not reachable through it.",
    "This host emits no adapter-state event; every adapter.state response is a fresh sample. An adapter loss ends every stream of the attachment and replaces the attachment: later requests on it fail backend.reset until the renderer attaches again.",
];

fn adapter_limitations() -> IpcValue {
    IpcValue::Array(
        ADAPTER_LIMITATIONS
            .iter()
            .map(|limitation| string(*limitation))
            .collect(),
    )
}

const UNSAMPLED_SNAPSHOT_REASON: &str =
    "This snapshot carries attachment identity only; availability, authorization, power, and the heard peer count are not sampled here, so route adapter.state for a live reading.";

/// What one `adapter.state` read measured through the shared central.
/// `None` means the radio cannot answer that question on this platform;
/// its reason says why and rides in `safeReason`.
struct AdapterReading {
    power: Option<AdapterPowerState>,
    authorization: Option<AdapterAuthorization>,
    /// Whether the adapter object is gone (the core's availability fact).
    removed: bool,
    heard: i64,
    reasons: Vec<String>,
}

/// Snapshot for the attachment record, which reads nothing from the adapter.
fn adapter_state(attachment: &Attachment) -> IpcValue {
    let clock = sample_epoch_millis();
    let mut caveats = vec![UNSAMPLED_SNAPSHOT_REASON.to_owned()];
    caveats.extend(clock.reason.map(str::to_owned));
    object([
        ("availability", string("unknown")),
        ("authorization", string(AUTHORIZATION_UNKNOWN)),
        ("power", string("unknown")),
        ("heard", IpcValue::Null),
        (
            "backendGeneration",
            string(attachment.backend_generation.clone()),
        ),
        ("updatedAt", number(clock.epoch_millis)),
        ("safeReason", safe_reason(&caveats)),
    ])
}

/// Snapshot for `adapter.state`, in the IPC vocabulary the desktop NAPI
/// provider also speaks (one vocabulary): `unsupported` power makes the
/// adapter unsupported and its authorization unavailable; CoreBluetooth's
/// `Unauthorized` is the legacy `authorization: denied` with power
/// unknown; a removed adapter is unavailable; an unreadable power is
/// unknown availability, never assumed available.
fn live_adapter_state(attachment: &Attachment, reading: &AdapterReading) -> IpcValue {
    let clock = sample_epoch_millis();
    let mut caveats = reading.reasons.clone();
    let (availability, authorization, power) = match reading.power {
        Some(AdapterPowerState::Unsupported) => {
            caveats.push("The OS reports Bluetooth LE is unsupported on this host.".to_owned());
            ("unsupported", "unavailable", "unsupported")
        }
        Some(AdapterPowerState::Unauthorized) => {
            caveats.push("The OS denies this process the Bluetooth adapter.".to_owned());
            (availability_of(reading), "denied", "unknown")
        }
        power => (
            availability_of(reading),
            reading
                .authorization
                .map_or(AUTHORIZATION_UNKNOWN, AdapterAuthorization::as_str),
            power.map_or("unknown", power_wire),
        ),
    };
    caveats.extend(clock.reason.map(str::to_owned));
    object([
        ("availability", string(availability)),
        ("authorization", string(authorization)),
        ("power", string(power)),
        ("heard", number(reading.heard)),
        (
            "backendGeneration",
            string(attachment.backend_generation.clone()),
        ),
        ("updatedAt", number(clock.epoch_millis)),
        ("safeReason", safe_reason(&caveats)),
    ])
}

fn availability_of(reading: &AdapterReading) -> &'static str {
    if reading.removed {
        "unavailable"
    } else if reading.power.is_none() {
        "unknown"
    } else {
        "available"
    }
}

/// The IPC power token for a power state the OS reported (`Unsupported`
/// and `Unauthorized` are mapped by [`live_adapter_state`]).
fn power_wire(power: AdapterPowerState) -> &'static str {
    match power {
        AdapterPowerState::PoweredOn => "on",
        AdapterPowerState::PoweredOff => "off",
        AdapterPowerState::Resetting => "resetting",
        AdapterPowerState::Unsupported => "unsupported",
        AdapterPowerState::Unauthorized | AdapterPowerState::Unknown => "unknown",
    }
}

fn safe_reason(caveats: &[String]) -> IpcValue {
    if caveats.is_empty() {
        IpcValue::Null
    } else {
        string(caveats.join(" "))
    }
}

/// The adapter-state token meaning "this host obtained no authorization
/// reading". Never a denial.
const AUTHORIZATION_UNKNOWN: &str = "unknown";

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

fn adapter_state_payload_live(attachment: &Attachment, reading: &AdapterReading) -> IpcValue {
    object([("state", live_adapter_state(attachment, reading))])
}

/// The webview IPC protocol this plugin speaks. Version 3 carries the caller
/// deadline as a relative `budgetMs`, `commit` on every normalized error,
/// `delivery` on subscriptions and connection-lifecycle events; a webview
/// offering only 2 is refused at bootstrap as `protocol.incompatible`.
pub(crate) const IPC_PROTOCOL_VERSION: i64 = 4;

/// The reserved stream of attachment rebinds (IPC protocol 4; TypeScript
/// `IPC_ATTACHMENT_STREAM_ID`).
const IPC_ATTACHMENT_STREAM_ID: &str = "attachment";

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
            negotiate_axis(
                remote_offer,
                "ipc-protocol",
                "ipcProtocol",
                IPC_PROTOCOL_VERSION,
            )?,
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
                // A failed release is retried by releasing again, which
                // calls native again; the wire vocabulary reserves
                // `caller-decides` for undispatched aborts and timeouts.
                ("retryability", string("never")),
                ("commit", IpcValue::Null),
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

/// Whether the attachment a route names is the one this dispatcher holds:
/// the identity fields of the attachment record, compared field by field
/// without building (and sampling) a record.
fn attachment_identity_matches(
    envelope: &BTreeMap<String, IpcValue>,
    attachment: &Attachment,
) -> bool {
    let field = |record: &BTreeMap<String, IpcValue>, key: &str, expected: &str| {
        record.get(key).and_then(as_string) == Some(expected)
    };
    let adapter = envelope.get("adapter").and_then(|value| match value {
        IpcValue::Object(record) => Some(record),
        _ => None,
    });
    field(envelope, "attachmentId", &attachment.attachment_id)
        && field(
            envelope,
            "backendInstanceId",
            &attachment.backend_instance_id,
        )
        && field(
            envelope,
            "backendGeneration",
            &attachment.backend_generation,
        )
        && adapter.is_some_and(|adapter| {
            field(adapter, "adapterId", &attachment.adapter_id)
                && field(adapter, "adapterGeneration", &attachment.adapter_generation)
        })
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
        let payload = super::adapter_state_payload_live(&attachment, &powered_on_reading());
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

    fn powered_on_reading() -> super::AdapterReading {
        super::AdapterReading {
            power: Some(ubm_desktop::AdapterPowerState::PoweredOn),
            authorization: Some(ubm_desktop::AdapterAuthorization::Granted),
            removed: false,
            heard: 3,
            reasons: Vec::new(),
        }
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
    fn every_core_authorization_is_a_wire_token() {
        use ubm_desktop::AdapterAuthorization;
        for authorization in [
            AdapterAuthorization::Granted,
            AdapterAuthorization::Denied,
            AdapterAuthorization::Restricted,
            AdapterAuthorization::NotDetermined,
        ] {
            let reading = super::AdapterReading {
                authorization: Some(authorization),
                ..powered_on_reading()
            };
            let state = state_object(super::live_adapter_state(&test_attachment(), &reading));
            let Some(super::IpcValue::String(token)) = state.get("authorization") else {
                panic!("authorization must be a string");
            };
            assert!(
                WIRE_AUTHORIZATION_TOKENS.contains(&token.as_str()),
                "{token}"
            );
            assert_eq!(token, authorization.as_str());
        }
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
        let state = state_object(super::live_adapter_state(
            &test_attachment(),
            &powered_on_reading(),
        ));

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

        assert_eq!(state.get("authorization"), Some(&string("granted")));
        assert_eq!(state.get("safeReason"), Some(&super::IpcValue::Null));
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
            super::safe_reason(&["first.".to_owned(), "second.".to_owned()]),
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
            ("ipcProtocol", offer_range("ipc-protocol", 4)),
        ]) else {
            panic!("the version offer must be an object");
        };
        offer
    }

    // IPC protocol 4 adds the host-announced attachment rebind (the
    // `attachment` stream, `backend-restarted`) after an adapter reset; a
    // protocol-3 webview would keep routing on a replaced attachment, so the
    // plugin speaks 4 only. An older or newer-only webview fails at bootstrap
    // as protocol.incompatible, before any operation can be admitted.
    #[test]
    fn version_offer_requires_ipc_protocol_4_and_refuses_an_old_webview() {
        assert_eq!(super::IPC_PROTOCOL_VERSION, 4);
        let mut old = current_offer();
        old.insert("ipcProtocol".to_owned(), offer_range("ipc-protocol", 3));
        let error = negotiate_ipc_versions(&old).expect_err("an old webview must be refused");
        assert_eq!(error.code, BleErrorCode::ProtocolIncompatible);

        let mut newer = current_offer();
        newer.insert("ipcProtocol".to_owned(), offer_range("ipc-protocol", 5));
        let error =
            negotiate_ipc_versions(&newer).expect_err("a newer-only webview must be refused");
        assert_eq!(error.code, BleErrorCode::ProtocolIncompatible);

        let super::IpcValue::Object(versions) =
            negotiate_ipc_versions(&current_offer()).expect("protocol 4 must negotiate")
        else {
            panic!("the negotiated versions must be an object");
        };
        let Some(super::IpcValue::Object(ipc)) = versions.get("ipcProtocol") else {
            panic!("the negotiated IPC protocol must be an object");
        };
        assert_eq!(
            ipc.get("selected"),
            Some(&object([
                ("axis", string("ipc-protocol")),
                ("value", super::number(4))
            ]))
        );
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
                        ("value", super::number(4))
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
                                ("value", super::number(4))
                            ])
                        ),
                        (
                            "maximum",
                            object([
                                ("axis", string("ipc-protocol")),
                                ("value", super::number(4))
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
                                ("value", super::number(4))
                            ])
                        ),
                        (
                            "maximum",
                            object([
                                ("axis", string("ipc-protocol")),
                                ("value", super::number(4))
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
            control: ubm_desktop::OpControl::unbounded(),
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
        // Backpressure is carried by the code (`stream.quota` recovers with
        // retry-with-backoff); the wire reserves `caller-decides` for
        // undispatched aborts and timeouts, and the TypeScript transport
        // rejects it on any other code as malformed.
        assert_eq!(error.retryability, ubm_desktop::Retryability::Never);
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
    // drive the live dispatcher over one shared `DesktopCentral` on a
    // scripted `FakeRadio` boundary: connect verdicts equal a direct core
    // drive bit-for-bit, scans start headless, and a shut-down core fails
    // loudly instead of falling back to silent legacy radio ownership.
    mod core_authority_cutover {
        use std::collections::BTreeMap;
        use std::sync::Arc;

        use ubm_desktop::{DesktopCentral, FakeRadio, OpControl};

        use crate::desktop_core::CoreAuthority;
        use crate::{AuthenticatedCaller, IpcValue};

        use super::super::{
            caller_key, object, string, Attachment, BtleplugDispatcher, CallerState, IpcEventSink,
        };

        const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";

        fn caller() -> AuthenticatedCaller {
            AuthenticatedCaller::new("test-app".to_owned(), "main".to_owned())
        }

        fn test_caller_state(attachment: Attachment) -> CallerState {
            CallerState {
                lease_id: "lease-1".to_owned(),
                lease_generation: "generation-1".to_owned(),
                versions: object([]),
                attachment,
                event_sink: IpcEventSink::new(tauri::ipc::Channel::new(|_| Ok(()))),
                retired: false,
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

        async fn open_central(owner: &'static str) -> DesktopCentral<FakeRadio> {
            ubm_desktop::executor::desktop_runtime()
                .spawn(DesktopCentral::open(FakeRadio::new(), owner))
                .await
                .expect("open task joins")
                .expect("fake radio opens without hardware")
        }

        async fn dispatcher_with_caller() -> (BtleplugDispatcher, AuthenticatedCaller) {
            let authority: Arc<dyn CoreAuthority> = Arc::new(open_central("tauri-test").await);
            let dispatcher = BtleplugDispatcher::with_core_authority(authority);
            let caller = caller();
            let attachment = dispatcher
                .ensure_adapter()
                .await
                .expect("the central's attachment");
            dispatcher
                .inner
                .lock()
                .await
                .callers
                .insert(caller_key(&caller), test_caller_state(attachment));
            (dispatcher, caller)
        }

        /// BLE work executes the core: connecting headless succeeds through
        /// the admitted authority and records the core-owned mapping. The
        /// legacy path cannot do this: it reads `adapter.peripherals()`
        /// from raw btleplug first, which fails without hardware.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn core_authority_connects_without_radio_hardware() {
            let (dispatcher, caller) = dispatcher_with_caller().await;
            let mut payload = lease_payload();
            payload.insert("peerId".to_owned(), string("peer-unknown"));

            let response = dispatcher
                .connect(&caller, payload, OpControl::budget_ms(5000))
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
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn core_arbitration_verdicts_cross_verbatim() {
            let (dispatcher, caller_a) = dispatcher_with_caller().await;
            let caller_b = AuthenticatedCaller::new("test-app".to_owned(), "second".to_owned());
            let attachment = dispatcher
                .ensure_adapter()
                .await
                .expect("the central's attachment");
            dispatcher
                .inner
                .lock()
                .await
                .callers
                .insert(caller_key(&caller_b), test_caller_state(attachment));
            let mut first = lease_payload();
            first.insert("query".to_owned(), empty_scan_query());
            let started = dispatcher
                .start_scan(&caller_a, first, OpControl::budget_ms(5000))
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
                .start_scan(&caller_b, second, OpControl::budget_ms(5000))
                .await
                .expect_err("second owner is refused by core arbitration");
            let (code, domain, operation) = error.identity();

            let direct = {
                let core = open_central("tauri-direct").await;
                core.start_scan("owner-a", &[], OpControl::budget_ms(5000))
                    .await
                    .expect("first");
                core.start_scan("owner-b", &[], OpControl::budget_ms(5000))
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
                .stop_scan(&caller_a, stop, OpControl::budget_ms(5000))
                .await
                .expect("first scan stops");
        }

        /// Scan admission and execution run through the core without touching
        /// raw btleplug: start succeeds headless and mints a handle.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn core_authority_starts_scans_without_radio_hardware() {
            let (dispatcher, caller) = dispatcher_with_caller().await;
            let mut payload = lease_payload();
            payload.insert("query".to_owned(), empty_scan_query());

            let response = dispatcher
                .start_scan(&caller, payload, OpControl::budget_ms(5000))
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
            let stopped = dispatcher
                .stop_scan(&caller, stop, OpControl::budget_ms(5000))
                .await;
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
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn missing_core_fails_loudly_never_silent_legacy() {
            let (dispatcher, caller) = dispatcher_with_caller().await;
            dispatcher.authority_shutdown().await;
            let mut payload = lease_payload();
            payload.insert("peerId".to_owned(), string("peer-1"));

            let error = dispatcher
                .connect(&caller, payload, OpControl::budget_ms(5000))
                .await
                .expect_err("post-shutdown ops must fail loudly");
            let (code, domain, operation) = error.identity();
            assert_eq!(code, "adapter.unavailable");
            assert_eq!(domain, "adapter");
            assert_eq!(operation, "tauri.core-shutdown");
        }
    }
}

#[cfg(test)]
#[path = "dispatcher_packet_b_tests.rs"]
mod dispatcher_packet_b_tests;
#[cfg(test)]
#[path = "tauri_identity_tests.rs"]
mod tauri_identity_tests;
