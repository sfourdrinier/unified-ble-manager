use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    future::Future,
    path::PathBuf,
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
use ubm_core::contracts::{AttachmentTuple, BleErrorCode, BleErrorDomain, CommitState};
use ubm_desktop::{
    AdapterAuthorization, AdapterAvailability, AdapterPowerState, Budget, CancelAck, CancelRequest,
    CentralProfile, CompletionOutcome, DeliveryMode, DesktopCentral, DesktopError, DiscoveredPath,
    InvalidationCause, LifecycleEvent, LifecycleKind, NotificationPoll, ObservedDelivery,
    OpControl, OpTicket, OperationId, PlatformDetail, PlatformValue, Retryability,
    ScanTerminalEvent, ServiceAccess, ShutdownReport, SourceUncertaintyEvent,
};
use uuid::Uuid;

use crate::capabilities;
use crate::desktop_core::{CoreAuthority, CoreSelector};
use crate::scan_plan::{decode_normalized_scan_query, diagnostic_scan_plan};
use crate::ATTACH_REQUEST_KIND;
use crate::{AuthenticatedCaller, DispatchFuture, IpcDispatcher, IpcEventSink, IpcValue};

const MAX_PENDING_EVENTS: usize = 256;
/// Ownerless native facts remain inspectable when no renderer is attached.
/// This host-owned ring is bounded; eviction is disclosed on replay.
const MAX_RETAINED_ATTACHMENT_DIAGNOSTICS: usize = 128;

/// Successful watch closes are replay metadata, not live resources. Retain
/// recent handles in insertion order; older unknown handles fail ownership
/// checks. Renderers must mint a fresh handle for every acquisition.
#[derive(Default)]
struct WatchReleaseHistory {
    handles: HashSet<String>,
    order: VecDeque<String>,
}

impl WatchReleaseHistory {
    fn insert(&mut self, handle: String) {
        if self.handles.insert(handle.clone()) {
            self.order.push_back(handle);
            if self.order.len() > MAX_PENDING_EVENTS {
                if let Some(oldest) = self.order.pop_front() {
                    self.handles.remove(&oldest);
                }
            }
        }
    }

    fn contains(&self, handle: &str) -> bool {
        self.handles.contains(handle)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.handles.len()
    }
}

impl Extend<String> for WatchReleaseHistory {
    fn extend<T: IntoIterator<Item = String>>(&mut self, handles: T) {
        for handle in handles {
            self.insert(handle);
        }
    }
}
#[path = "acquired_gatt.rs"]
mod acquired_gatt;
#[path = "connection_parameters.rs"]
mod connection_parameters;
#[path = "connection_phy.rs"]
mod connection_phy;
#[path = "ordered_delivery.rs"]
mod ordered_delivery;
#[path = "ordered_groups.rs"]
mod ordered_groups;
#[path = "ordered_markers.rs"]
mod ordered_markers;
#[path = "ordered_orphans.rs"]
mod ordered_orphans;
#[path = "ordered_reclaims.rs"]
mod ordered_reclaims;
#[path = "ordered_recordings.rs"]
mod ordered_recordings;
#[path = "ordered_recovery.rs"]
mod ordered_recovery;
#[path = "peer_directory.rs"]
mod peer_directory;
#[path = "security.rs"]
mod security;
#[path = "terminal_evidence.rs"]
mod terminal_evidence;
#[path = "write_readiness.rs"]
mod write_readiness;
const MAX_CORRELATIONS: usize = 256;
const COMPLETED_CORRELATION_TTL: Duration = Duration::from_secs(30);
/// Delivery pacing between core polls (scan observations and notification
/// forwarders). This paces delivery only: admission, deadlines, overflow,
/// and teardown all stay core-owned.
const FORWARD_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Largest integer JavaScript represents exactly (`Number.MAX_SAFE_INTEGER`):
/// the upper bound of a well-formed `budgetMs`.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const DIAGNOSTIC_PAGE_LIMIT: u32 = 65_536;
const DIAGNOSTIC_OCCURRENCE_LIMIT: usize = 128;
// The durable owner normally bounds a producer payload to 64 KiB, but a
// retained historical receipt can include six encoded fields plus framing.
// Keep the host transport bound aligned with the desktop control envelope;
// smaller per-transition limits would strand an otherwise page-readable row.
const DIAGNOSTIC_EXACT_PAYLOAD_LIMIT: usize = 18 * 1024 * 1024;

/// Authority retained by the trusted Tauri host for one renderer-visible
/// recovery handle.  The renderer sees only `handle_id`; keeping the
/// descriptor, original consumer, and recipient lease here prevents a
/// webview from widening a restored source-time recipient grant.
#[derive(Clone, Debug)]
struct RendererPeerFailureRecoveryHandle {
    /// The exact renderer admission generation that minted this handle.  A
    /// caller key can be reused after a window reload, so the key alone is
    /// never authority for a retained recovery capability.
    owner_generation: u64,
    descriptor: ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    peer_id: String,
    selector: CoreSelector,
    consumer: String,
    recipient_lease: String,
}

type ScanFailureRecoveryHandles =
    HashMap<String, HashMap<String, (u64, RendererScanFailureRecoveryHandle)>>;
type OrderedRecordingStarts = HashMap<(String, String), Arc<ordered_recordings::StartSlot>>;

/// Capability-bearing scan recovery values never leave the native dispatcher.
#[derive(Clone, PartialEq, Eq)]
enum RendererScanFailureRecoveryHandle {
    Owner(ubm_desktop::continuation_outbox::PendingScanFailure),
    Head(ubm_desktop::continuation_outbox::ScanFailureHead),
    /// A charged alias slot for a cleanup receipt that has not yet been created.
    /// Finalization mutates the durable outbox, so capacity is admitted first.
    ReservedCleanup,
    Cleanup {
        receipt: ubm_desktop::continuation_outbox::ScanFailureCleanupRetirement,
        /// The renderer alias to the durable owner remains valid until its
        /// exact cleanup receipt is retired. Retaining this private linkage
        /// lets completion reclaim only aliases that can no longer authorize
        /// a recovery operation, without evicting other live sources.
        owner: Option<ubm_desktop::continuation_outbox::PendingScanFailure>,
    },
}

#[derive(Default)]
struct ScanFailureRecoveryAdmission {
    generation: u64,
    owner_generation: Option<u64>,
    recording_id: Option<String>,
    lease: Option<Arc<ubm_desktop::scan_failure_recovery::ScanFailureRecoveryLease>>,
}

struct TauriScanRecoveryWake;
impl ubm_desktop::continuation_outbox::WakeSink for TauriScanRecoveryWake {
    fn wake(&self, _session_id: u64) {}
}

/// The durable diagnostic identifier is opaque host evidence, not a peer,
/// caller, or renderer identity. Keeping this decoding at the trusted host
/// boundary prevents a bridge from accepting a lossy UUID-shaped substitute.
fn diagnostic_occurrence_id(value: &str) -> Result<[u8; 40], serde_json::Value> {
    if value.len() != 80
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(
            serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"invalid diagnostic occurrence identity","platform":null}),
        );
    }
    let mut output = [0_u8; 40];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(|_| {
            serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"invalid diagnostic occurrence identity","platform":null})
        })?;
    }
    Ok(output)
}

fn diagnostic_occurrence_text(value: [u8; 40]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The retained peer-source descriptor is a stable `/1` protocol value. Keep
/// the binary identities in their canonical lowercase hexadecimal form at the
/// host boundary; serializing a byte array here would give each frontend a
/// different representation and would make a descriptor non-round-trippable.
fn peer_failure_identity_text(value: [u8; 16]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn peer_failure_descriptor_json(
    descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
) -> serde_json::Value {
    serde_json::json!({
        "protocol": descriptor.protocol,
        "representation": descriptor.representation,
        "host": peer_failure_identity_text(descriptor.identity.host),
        "peer": peer_failure_identity_text(descriptor.identity.peer),
        "sourceGenerationIdentity": peer_failure_identity_text(descriptor.identity.source_generation),
        "nativeGroup": peer_failure_identity_text(descriptor.identity.native_group),
        "transfer": peer_failure_identity_text(descriptor.identity.transfer),
        "recovery": peer_failure_identity_text(descriptor.identity.recovery),
        "encodedLength": descriptor.encoded_length.to_string(),
        "chunkBytes": descriptor.chunk_bytes,
    })
}

fn peer_failure_recovery_offset(
    value: &str,
    operation: &'static str,
) -> Result<u64, DispatchError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|number| number.to_string() == value && *number <= i64::MAX as u64)
        .ok_or_else(|| DispatchError::new(BleErrorCode::ArgumentInvalid, "restoration", operation))
}

fn diagnostic_offset(value: &str) -> Result<u64, serde_json::Value> {
    value
        .parse::<u64>()
        .ok()
        .filter(|number| number.to_string() == value && *number <= i64::MAX as u64)
        .ok_or_else(|| serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"invalid diagnostic offset","platform":null}))
}

fn diagnostic_action(
    value: &str,
) -> Result<ubm_desktop::continuation_journal::DiagnosticExactAction, serde_json::Value> {
    use ubm_desktop::continuation_journal::DiagnosticExactAction;
    match value {
        "acknowledge" => Ok(DiagnosticExactAction::Acknowledge),
        "retire" => Ok(DiagnosticExactAction::Retire),
        "confirm" => Ok(DiagnosticExactAction::Confirm),
        "release" => Ok(DiagnosticExactAction::Release),
        _ => Err(
            serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"unknown diagnostic lifecycle transition","platform":null}),
        ),
    }
}

fn btleplug_runtime() -> tokio::runtime::Handle {
    // One shared desktop executor per process, owned by `ubm-desktop`: the
    // central and its radio open and run there, never on Tauri's runtime.
    ubm_desktop::executor::desktop_runtime()
}

/// A directory selected by trusted Tauri host code for exact raw native
/// callback evidence. It is deliberately absent from the IPC request schema:
/// a webview must never choose a native filesystem location.
#[derive(Clone, Debug)]
pub struct FailureEvidenceStoreOptions {
    pub directory: PathBuf,
    pub max_bytes: usize,
}

#[derive(Clone, Debug, Default)]
pub struct BtleplugDispatcherOptions {
    /// Optional stricter Linux daemon-owner restriction, supplied only by trusted host code.
    /// Native authority resolves and pins the owner by default; this is not implementation attestation.
    pub connection_policy: Option<ubm_desktop::boundary::BluezConnectionPolicy>,
    /// The adapter the shared central runs on, by its selectable identity
    /// (`ubm_desktop::btleplug_backend::list_adapters` labels; BlueZ `hci0`,
    /// the Windows device id, `CoreBluetooth` on macOS). `None` selects the
    /// sole adapter; with several adapters and no name the open fails
    /// `adapter.ambiguous`, and a name that matches none fails
    /// `adapter.selection-required` — never a silent first pick.
    pub adapter_id: Option<String>,
    /// Optional private native evidence store, installed before the production
    /// manager opens. It is never selected by IPC input.
    pub failure_evidence_store: Option<FailureEvidenceStoreOptions>,
    /// Opt-in aggregate bound for the trusted registry-wide terminal-evidence
    /// ledger. The host first configures its recording directory through the
    /// Rust-only continuation API; a renderer never supplies a directory or
    /// chooses a ledger. `None` keeps the durable terminal-evidence capability
    /// unavailable.
    pub terminal_evidence_maximum_bytes: Option<u64>,
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
    authority_shutdown_admission: Arc<Mutex<()>>,
    lifecycle_pump: Arc<SyncMutex<Option<TauriJoinHandle<()>>>>,
    continuation: Arc<Mutex<Option<ubm_desktop::continuation_adapter::DesktopContinuation>>>,
    recordings: Arc<ubm_desktop::continuation_journal::JournalRegistry>,
    terminal_evidence_maximum_bytes: Option<u64>,
    terminal_evidence_ledger:
        Arc<SyncMutex<Option<Arc<ubm_desktop::terminal_evidence_ledger::TerminalEvidenceLedger>>>>,
    scan_failure_recovery: Arc<ubm_desktop::scan_failure_recovery::ScanFailureRecoveryRegistry>,
    /// Serializes trusted lease grants against final durable cleanup and cache
    /// reclamation. A grant cannot race a just-completed recording back into
    /// the registry after its last recovery receipt has been retired.
    scan_failure_recovery_lifecycle: Arc<Mutex<()>>,
    scan_failure_recovery_admission: Arc<SyncMutex<HashMap<String, ScanFailureRecoveryAdmission>>>,
    scan_failure_recovery_handles: Arc<SyncMutex<ScanFailureRecoveryHandles>>,
    /// Explicit host grants for renderer recovery access. A durable lease is
    /// never sufficient by itself: a trusted Tauri host must grant the exact
    /// authenticated caller after attaching its recording.
    /// One synchronous state machine fences trusted recovery grants across an
    /// asynchronous ledger attach and synchronous window teardown.  Keeping
    /// the generation and grant under the same mutex makes teardown win even
    /// when the attach future completes at exactly the same time.
    peer_failure_recovery_admission: Arc<SyncMutex<HashMap<String, PeerFailureRecoveryAdmission>>>,
    peer_failure_recovery_handles:
        Arc<SyncMutex<HashMap<String, HashMap<String, RendererPeerFailureRecoveryHandle>>>>,
    ordered_recording_starts: Arc<Mutex<OrderedRecordingStarts>>,
    ordered_recording_reservations: Arc<Mutex<ordered_recordings::Reservations>>,
    reclaim_admission: Arc<Mutex<()>>,
    reclaim_state: Arc<SyncMutex<ordered_reclaims::ReclaimState>>,
    #[cfg(test)]
    ordered_recording_panic_phase: Arc<std::sync::atomic::AtomicU8>,
    #[cfg(test)]
    terminal_evidence_recovery_pause: Arc<SyncMutex<Option<Arc<TerminalEvidenceRecoveryPause>>>>,
}

#[cfg(test)]
struct TerminalEvidenceRecoveryPause {
    claimed: std::sync::mpsc::SyncSender<()>,
    resume: SyncMutex<std::sync::mpsc::Receiver<()>>,
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
    /// Admission is closed, but cleanup remains owned until a clean report.
    Closing(Arc<dyn CoreAuthority>),
    ShutDown,
}

struct DispatcherState {
    /// The adapter's display label, read once through the central's
    /// boundary: a reset keeps the adapter (finding 57), only its
    /// generations move.
    adapter_name: Option<String>,
    callers: HashMap<String, CallerState>,
    terminal_evidence_owners: HashMap<String, TerminalEvidenceOwner>,
    /// Core resources admitted for a caller that could not take ownership
    /// (released, lease replaced, duplicate) and whose compensating
    /// release failed (PR210-08). Retried and reported by the owning
    /// caller key's next release and by authority shutdown; never dropped
    /// silently.
    orphan_debt: Vec<OrphanDebt>,
    ordered_orphans: Vec<ordered_groups::OrphanSlot>,
    /// Attachments an adapter reset replaced (IPC protocol 4). Work naming
    /// one is refused `backend.reset`, releases excepted; renderers route
    /// under the attachment the dispatcher rebound them to and announced.
    replaced_attachment_ids: HashSet<String>,
    retained_attachment_diagnostics: VecDeque<IpcValue>,
    dropped_attachment_diagnostics: u64,
}

#[derive(Clone, Debug)]
struct TerminalEvidenceOwner {
    producer: String,
    caller_key: String,
    lease_id: String,
    lease_generation: String,
    /// Private recovery claim derived from this controller's current lease.
    /// It never crosses the Tauri IPC grammar.
    recovery_lease_id: String,
    reservation_id: String,
    /// Only receipts orphaned by a completed caller release may be recovered.
    recoverable: bool,
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
    ordered_groups: HashMap<String, Arc<Mutex<ordered_groups::OrderedGroup>>>,
    acquired_writers: HashMap<String, acquired_gatt::CoreAcquiredWriter>,
    acquired_releases: WatchReleaseHistory,
    connection_events: HashMap<String, ConnectionEventResource>,
    security_watches: HashMap<String, security::SecurityWatch>,
    security_watch_releases: HashSet<String>,
    write_readiness_watches: HashMap<String, write_readiness::WriteReadinessWatch>,
    write_readiness_releases: WatchReleaseHistory,
    parameter_watches: HashMap<String, connection_parameters::ConnectionParameterWatch>,
    parameter_releases: WatchReleaseHistory,
    operations: HashMap<String, TrackedOperation>,
    completed_correlations: HashMap<String, Instant>,
    pending_events: HashSet<String>,
}

#[derive(Default)]
struct PeerFailureRecoveryAdmission {
    generation: u64,
    owner_generation: Option<u64>,
    /// ACK is destructive at the native ledger.  Once its exact handle has
    /// been consumed while holding this admission, it remains an admitted
    /// operation even if teardown advances the caller generation while the
    /// native future is running.  Teardown never transfers this lease to a
    /// replacement caller; completion removes it by its original generation.
    in_flight_acknowledgements: HashSet<(u64, String)>,
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
    terminal_evidence: Option<TerminalEvidenceBinding>,
    /// A core-ended scan whose authenticated stage-failure terminal could
    /// not enter the renderer channel. The mapping is cleanup debt: it keeps
    /// both the precise stage failure to re-send and the precise delivery
    /// failure a release retry must report.
    terminal_stage_failure: Option<DispatchError>,
    terminal_delivery_failure: Option<DispatchError>,
    /// The exact terminal that reached durable staging but not the renderer.
    /// A later ACK or owner cleanup retries this same terminal rather than
    /// replacing a source failure with an owner-release terminal.
    terminal_delivery_retry_item: Option<ScanTerminalRetry>,
    terminal_failure_claimed: bool,
    /// A source-failed stage-error terminal reached the renderer. The core
    /// already ended this scan, so owner cleanup must not manufacture a
    /// contradictory `owner-released` terminal afterward.
    terminal_failure_sent: bool,
    /// A concurrent release joins this retry before it can abandon evidence
    /// or emit a contradictory owner-release terminal.
    terminal_delivery_retry: Option<watch::Receiver<ReleaseAnswer>>,
}

#[derive(Clone)]
enum ScanTerminalRetry {
    StageFailure(DispatchError),
    Terminal {
        reason: String,
        error: Option<DispatchError>,
    },
}

#[derive(Clone)]
struct TerminalEvidenceBinding {
    reservation_id: String,
    scope: ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope,
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
    /// Confirmed native stage, retained until owner-channel cleanup succeeds.
    /// False means another lease still owns the physical link.
    native_release: Option<bool>,
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
    database_handle: String,
    peer_id: String,
    selector: CoreSelector,
    consumer: String,
    /// The delivery mode the radio reported for this enablement.
    delivery: ObservedDelivery,
    acquired: Option<(String, String)>,
    task: Option<TauriJoinHandle<()>>,
    phase: ReleasePhase,
    /// A terminal accepted by the channel has already ended this stream.
    /// A later explicit disconnect must not send a second terminal.
    terminal_sent: bool,
    terminal_evidence: Option<TerminalEvidenceBinding>,
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
const LINK_HOST_RESET: LinkTransition = LinkTransition {
    previous: "connected",
    current: "lost",
    cause: "backend-restart",
    terminal: "source-failed",
};

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminalAdmission {
    Any,
    ClaimedScan,
    ActiveSubscription,
    Connection,
    OwnerConnection,
}

struct TerminalRequest<'a> {
    expected_lease: (&'a str, &'a str),
    stream_id: &'a str,
    reason: &'a str,
    error: Option<&'a DispatchError>,
    counters: terminal_evidence::TerminalCounters,
    admission: TerminalAdmission,
}

struct ConnectionEventResource {
    platform: Option<PlatformDetail>,
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
#[derive(Clone)]
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
    release: Arc<Mutex<()>>,
}

/// Automatic release attempts per orphan, the compensation included
/// (Tauri 4.x `MAX_QUARANTINE_ATTEMPTS`).
const ORPHAN_RELEASE_ATTEMPTS: u32 = 8;
/// First retry delay; each later one doubles, capped at
/// [`ORPHAN_RETRY_MAX_DELAY`] (Tauri 4.x quarantine backoff).
const ORPHAN_RETRY_FIRST_DELAY: Duration = Duration::from_millis(100);
const ORPHAN_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);
const SHUTDOWN_ORPHAN_DRAIN: Duration = Duration::from_secs(5);

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
    Acquired {
        handle: String,
        lease: String,
    },
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
            Self::Acquired { .. } => "acquired-gatt",
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
            return native_platform_wire(native, self.platform.as_deref());
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
fn native_platform_wire(native: &PlatformDetail, fallback: Option<&str>) -> IpcValue {
    let mut fields = BTreeMap::from([
        ("domain".to_owned(), string(native.domain.clone())),
        ("code".to_owned(), string(native.code.clone())),
        (
            "safeMessage".to_owned(),
            string(native.message.as_deref().or(fallback).unwrap_or_default()),
        ),
        (
            "metadata".to_owned(),
            IpcValue::Object(
                native
                    .metadata
                    .iter()
                    .filter(|(key, _)| {
                        !matches!(key.as_str(), "__proto__" | "constructor" | "prototype")
                    })
                    .map(|(key, value)| (key.clone(), platform_value(value)))
                    .collect(),
            ),
        ),
    ]);
    if let Some(entries) = &native.metadata_entries {
        fields.insert(
            "metadataEntries".to_owned(),
            IpcValue::Array(
                entries
                    .iter()
                    .map(|entry| {
                        let (kind, value) = match &entry.value {
                            PlatformValue::Int(integer) => ("int", string(integer.to_string())),
                            PlatformValue::Text(text) => ("text", string(text.clone())),
                            PlatformValue::Bool(flag) => ("bool", IpcValue::Bool(*flag)),
                        };
                        object([
                            ("key", string(entry.key.clone())),
                            ("value", object([("kind", string(kind)), ("value", value)])),
                        ])
                    })
                    .collect(),
            ),
        );
    }
    IpcValue::Object(fields)
}

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
/// ended is the core's successful scoped lease-release answer, not an error;
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
type ProfiledOpener = Arc<
    dyn Fn(
            CentralProfile,
            Option<ubm_desktop::boundary::BluezConnectionPolicy>,
        ) -> AuthorityOpenFuture
        + Send
        + Sync,
>;

/// The production opener: the btleplug radio and its central open on the
/// shared desktop executor (never on Tauri's runtime), on the adapter the
/// profile names, with the core's selection rule (ambiguity refused).
fn btleplug_opener(failure_evidence_store: Option<FailureEvidenceStoreOptions>) -> ProfiledOpener {
    Arc::new(move |profile, connection_policy| {
        let failure_evidence_store = failure_evidence_store.clone();
        Box::pin(async move {
            let central = btleplug_runtime()
                .spawn(async move {
                    let failure_evidence_store = match failure_evidence_store {
                        None => None,
                        Some(options) => {
                            if options.directory.as_os_str().is_empty() || options.max_bytes == 0 {
                                return Err(DesktopError::new(
                                    BleErrorCode::ArgumentInvalid,
                                    BleErrorDomain::Restoration,
                                    "tauri.failure-evidence-store",
                                )
                                .with_detail("failure evidence directory and positive byte limit are required"));
                            }
                            Some(Arc::new(
                                btleplug::ubm_peer_notification_ingress::FileFailureEvidenceStore::open(
                                    options.directory,
                                    options.max_bytes,
                                )
                                .map_err(|error| {
                                    DesktopError::new(
                                        BleErrorCode::PlatformFailure,
                                        BleErrorDomain::Restoration,
                                        "tauri.failure-evidence-store",
                                    )
                                    .with_detail(error.to_string())
                                })?,
                            ) as Arc<dyn btleplug::ubm_peer_notification_ingress::FailureEvidenceStore>)
                        }
                    };
                    let central = DesktopCentral::open_btleplug_with_policy_and_failure_evidence_store(
                        profile,
                        connection_policy,
                        failure_evidence_store,
                    )
                    .await?;
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
        let opener = btleplug_opener(options.failure_evidence_store.clone());
        Self::with_profiled_opener(options, opener)
    }

    /// Dispatcher whose authority opens through `open` on first use, with
    /// the Tauri profile numbered by this dispatcher's own id counter.
    fn with_profiled_opener(options: BtleplugDispatcherOptions, open: ProfiledOpener) -> Self {
        let next_id = Arc::new(AtomicU64::new(1));
        let terminal_evidence_maximum_bytes = options.terminal_evidence_maximum_bytes;
        let opener: AuthorityOpener = {
            let next_id = Arc::clone(&next_id);
            Arc::new(move || {
                let mut profile = tauri_profile(Arc::clone(&next_id));
                profile.adapter_id = options.adapter_id.clone();
                open(profile, options.connection_policy.clone())
            })
        };
        Self::with_slot_and_ids(
            AuthoritySlot::Unopened(opener),
            next_id,
            terminal_evidence_maximum_bytes,
        )
    }

    fn with_slot(slot: AuthoritySlot) -> Self {
        Self::with_slot_and_ids(slot, Arc::new(AtomicU64::new(1)), None)
    }

    fn with_slot_and_ids(
        slot: AuthoritySlot,
        next_id: Arc<AtomicU64>,
        terminal_evidence_maximum_bytes: Option<u64>,
    ) -> Self {
        let recordings = Arc::new(ubm_desktop::continuation_journal::JournalRegistry::default());
        let scan_failure_recovery = Arc::new(
            ubm_desktop::scan_failure_recovery::ScanFailureRecoveryRegistry::new(
                Arc::clone(&recordings),
                Arc::new(TauriScanRecoveryWake),
            ),
        );
        Self {
            inner: Arc::new(Mutex::new(DispatcherState {
                adapter_name: None,
                callers: HashMap::new(),
                terminal_evidence_owners: HashMap::new(),
                orphan_debt: Vec::new(),
                ordered_orphans: Vec::new(),
                replaced_attachment_ids: HashSet::new(),
                retained_attachment_diagnostics: VecDeque::new(),
                dropped_attachment_diagnostics: 0,
            })),
            bootstrap_admission: Arc::new(Mutex::new(())),
            next_id,
            next_internal_id: Arc::new(AtomicU64::new(1)),
            next_revocation: Arc::new(AtomicU64::new(1)),
            started_at: Arc::new(Instant::now()),
            revoked_callers: Arc::new(SyncMutex::new(HashMap::new())),
            authority: Arc::new(Mutex::new(slot)),
            authority_shutdown_admission: Arc::new(Mutex::new(())),
            lifecycle_pump: Arc::new(SyncMutex::new(None)),
            continuation: Arc::new(Mutex::new(None)),
            recordings,
            terminal_evidence_maximum_bytes,
            terminal_evidence_ledger: Arc::default(),
            scan_failure_recovery,
            scan_failure_recovery_lifecycle: Arc::default(),
            scan_failure_recovery_admission: Arc::default(),
            scan_failure_recovery_handles: Arc::default(),
            peer_failure_recovery_admission: Arc::default(),
            peer_failure_recovery_handles: Arc::default(),
            ordered_recording_starts: Arc::default(),
            ordered_recording_reservations: Arc::default(),
            reclaim_admission: Arc::default(),
            reclaim_state: Arc::default(),
            #[cfg(test)]
            ordered_recording_panic_phase: Arc::default(),
            #[cfg(test)]
            terminal_evidence_recovery_pause: Arc::default(),
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
            AuthoritySlot::Closing(_) | AuthoritySlot::ShutDown => {
                return Err(DispatchError::new(
                    BleErrorCode::AdapterUnavailable,
                    "adapter",
                    "tauri.core-shutdown",
                ));
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
            AuthoritySlot::Unopened(_) | AuthoritySlot::Closing(_) | AuthoritySlot::ShutDown => {
                None
            }
        }
    }

    // Cleanup retries must distinguish a closed authority from a temporarily
    // contended mutex. Ordinary admission retains the nonblocking check above.
    async fn authority_open_for_retry(&self) -> bool {
        matches!(&*self.authority.lock().await, AuthoritySlot::Open(_))
    }

    async fn continuation_engine(
        &self,
    ) -> Result<ubm_desktop::continuation::NativeContinuation, serde_json::Value> {
        // Publish the retained engine before shutdown can inspect it. Do not
        // hold this lock during execution: shutdown must cancel held BLE work.
        let _admission = self.authority_shutdown_admission.lock().await;
        let authority = self
            .ensure_authority()
            .await
            .map_err(|error| error.normalized_error().into_wire())?;
        if let Some(existing) = self.continuation.lock().await.as_ref() {
            return Ok(existing.engine.clone());
        }
        let mut continuation = self.continuation.lock().await;
        Ok(continuation
            .get_or_insert_with(|| {
                authority.native_continuation(btleplug_runtime(), self.recordings.clone())
            })
            .engine
            .clone())
    }

    /// Trusted Rust-host entrypoint, usable from startup / OS wake integration.
    /// The process-owned order survives webview destruction and reload.
    pub async fn continuation_execute(
        &self,
        peer: &str,
        declaration: &str,
    ) -> Result<serde_json::Value, serde_json::Value> {
        // Retained handoff remains readable after shutdown, never executable.
        self.continuation_engine()
            .await?
            .execute(peer, declaration)
            .await
    }

    pub async fn continuation_prepare_claim(
        &self,
        max_items: u32,
        max_bytes: u32,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let engine = self
            .continuation
            .lock()
            .await
            .as_ref()
            .map(|owned| owned.engine.clone());
        match engine {
            Some(engine) => engine.prepare_claim(max_items, max_bytes).await,
            None if max_items == 0 || max_bytes == 0 => Err(
                serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation","detail":"claim bounds must be positive"}),
            ),
            None => Ok(
                serde_json::json!({"consumerCount":0,"batches":[],"disposed":false,"disposeFailure":null,"afterCutoffLoss":{"items":0,"bytes":0},"selectors":[]}),
            ),
        }
    }

    pub async fn continuation_acknowledge_claim(
        &self,
        token: &str,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let engine = self
            .continuation
            .lock()
            .await
            .as_ref()
            .map(|owned| owned.engine.clone());
        match engine {
            Some(engine) => engine.acknowledge_claim(token).await,
            None => Err(
                serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation","detail":"no prepared claim"}),
            ),
        }
    }

    pub async fn continuation_describe_backlog(
        &self,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let engine = self
            .continuation
            .lock()
            .await
            .as_ref()
            .map(|owned| owned.engine.clone());
        match engine {
            Some(engine) => engine.describe_backlog().await,
            None => Ok(serde_json::Value::Null),
        }
    }

    /// Bind a pre-existing recording from the trusted registry to durable
    /// retained-notification recovery.  This intentionally mirrors the
    /// desktop main-process path: the caller supplies an already-authorized
    /// recording identity, never a filesystem path, and this method creates
    /// no recording implicitly.
    pub async fn continuation_attach_peer_failure_recovery_recording(
        &self,
        id: &str,
    ) -> Result<(), serde_json::Value> {
        let recordings = self.recordings.clone();
        let id = id.to_owned();
        let ledger = ubm_desktop::continuation_journal::run_blocking_result(move || {
            recordings
                .get(&id)
                .and_then(|journal| journal.peer_failure_recovery_ledger())
                .map_err(ubm_desktop::continuation::recording_failure)
        })
        .await?;
        self.ensure_authority()
            .await
            .map_err(|error| error.normalized_error().into_wire())?
            .attach_peer_failure_recovery_ledger(ledger)
            .await
            .map_err(|error| {
                DispatchError::from_core(&error)
                    .normalized_error()
                    .into_wire()
            })
    }

    /// Trusted host grant for scan-failure recovery. The recording identifier
    /// is never accepted from renderer IPC and no path or durable capability
    /// is projected into the webview.
    pub async fn enable_scan_failure_recovery_for(
        &self,
        caller: AuthenticatedCaller,
        recording_id: &str,
    ) -> Result<(), serde_json::Value> {
        let key = caller_key(&caller);
        let supported = self
            .inner
            .lock()
            .await
            .callers
            .get(&key)
            .is_some_and(|state| {
                attachment_diagnostics_supported(&state.versions) && !state.retired
            });
        if !supported {
            return Err(DispatchError::new(
                BleErrorCode::CapabilityUnsupported,
                "restoration",
                "tauri.scan-failure-recovery.grant",
            )
            .normalized_error()
            .into_wire());
        }
        let _lifecycle = self.scan_failure_recovery_lifecycle.lock().await;
        let registry = Arc::clone(&self.scan_failure_recovery);
        let id = recording_id.to_owned();
        let lease = ubm_desktop::continuation_journal::run_blocking_result(move || {
            registry.attach_trusted(&id).map_err(|error| {
                DispatchError::from_core(&error)
                    .normalized_error()
                    .into_wire()
            })
        })
        .await?;
        let mut admission = self
            .scan_failure_recovery_admission
            .lock()
            .expect("scan recovery admission mutex poisoned");
        let entry = admission.entry(key).or_default();
        entry.owner_generation = Some(entry.generation);
        entry.recording_id = Some(recording_id.to_owned());
        entry.lease = Some(lease);
        Ok(())
    }

    fn revoke_scan_failure_recovery_admission(&self, key: &str) {
        let mut admission = self
            .scan_failure_recovery_admission
            .lock()
            .expect("scan recovery admission mutex poisoned");
        let entry = admission.entry(key.to_owned()).or_default();
        entry.generation = entry.generation.wrapping_add(1);
        entry.owner_generation = None;
        entry.recording_id = None;
        entry.lease = None;
        self.scan_failure_recovery_handles
            .lock()
            .expect("scan recovery handle mutex poisoned")
            .remove(key);
    }

    async fn scan_failure_recovery_lease(
        &self,
        caller: &AuthenticatedCaller,
        operation: &'static str,
    ) -> Result<
        (
            String,
            u64,
            Arc<ubm_desktop::scan_failure_recovery::ScanFailureRecoveryLease>,
        ),
        DispatchError,
    > {
        self.require_peer_failure_recovery_protocol(caller).await?;
        let key = caller_key(caller);
        self.scan_failure_recovery_admission
            .lock()
            .expect("scan recovery admission mutex poisoned")
            .get(&key)
            .and_then(|entry| {
                (entry.owner_generation == Some(entry.generation))
                    .then(|| {
                        entry
                            .lease
                            .as_ref()
                            .map(|lease| (key.clone(), entry.generation, Arc::clone(lease)))
                    })
                    .flatten()
            })
            .ok_or_else(|| {
                DispatchError::new(BleErrorCode::OwnershipDenied, "restoration", operation)
            })
    }

    fn scan_failure_recovery_handle(
        &self,
        key: &str,
        generation: u64,
        id: &str,
        operation: &'static str,
    ) -> Result<RendererScanFailureRecoveryHandle, DispatchError> {
        self.scan_failure_recovery_handles
            .lock()
            .expect("scan recovery handle mutex poisoned")
            .get(key)
            .and_then(|handles| handles.get(id))
            .filter(|(owner, _)| *owner == generation)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| {
                DispatchError::new(BleErrorCode::OwnershipDenied, "restoration", operation)
            })
    }

    /// Keep renderer aliases canonical for the lifetime of one admission.
    /// Repeated inventory is observational: it must neither invalidate a
    /// still-live alias nor allocate indefinitely. A distinct recovery value
    /// is retained only until caller revocation, and the same bounded quota as
    /// event ownership makes pressure explicit instead of evicting authority.
    fn retain_scan_failure_recovery_handle(
        &self,
        key: &str,
        generation: u64,
        handle: RendererScanFailureRecoveryHandle,
        prefix: &str,
        operation: &'static str,
    ) -> Result<String, DispatchError> {
        let admission = self
            .scan_failure_recovery_admission
            .lock()
            .expect("scan recovery admission mutex poisoned");
        if admission.get(key).is_none_or(|entry| {
            entry.owner_generation != Some(generation) || entry.generation != generation
        }) {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "restoration",
                operation,
            ));
        }
        let mut aliases = self
            .scan_failure_recovery_handles
            .lock()
            .expect("scan recovery handle mutex poisoned");
        let handles = aliases.entry(key.to_owned()).or_default();
        if let Some(id) = handles.iter().find_map(|(id, (owner, existing))| {
            let same_recovery_value = match (existing, &handle) {
                (
                    RendererScanFailureRecoveryHandle::Cleanup { receipt: left, .. },
                    RendererScanFailureRecoveryHandle::Cleanup { receipt: right, .. },
                ) => left == right,
                _ => existing == &handle,
            };
            (*owner == generation && same_recovery_value).then(|| id.clone())
        }) {
            return Ok(id);
        }
        if handles.len() >= MAX_PENDING_EVENTS {
            return Err(DispatchError::new(
                BleErrorCode::StreamQuota,
                "restoration",
                operation,
            ));
        }
        let id = self.id(prefix);
        handles.insert(id.clone(), (generation, handle));
        Ok(id)
    }

    /// Reserve a bounded cleanup alias before a finalizer mutates durable
    /// state. The reservation itself counts toward capacity, so concurrent
    /// finalizers cannot both pass a count check then strand a receipt.
    fn reserve_scan_failure_recovery_cleanup_handle(
        &self,
        key: &str,
        generation: u64,
        prefix: &str,
        operation: &'static str,
    ) -> Result<String, DispatchError> {
        let admission = self
            .scan_failure_recovery_admission
            .lock()
            .expect("scan recovery admission mutex poisoned");
        if admission.get(key).is_none_or(|entry| {
            entry.owner_generation != Some(generation) || entry.generation != generation
        }) {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "restoration",
                operation,
            ));
        }
        let mut aliases = self
            .scan_failure_recovery_handles
            .lock()
            .expect("scan recovery handle mutex poisoned");
        let handles = aliases.entry(key.to_owned()).or_default();
        if handles.len() >= MAX_PENDING_EVENTS {
            return Err(DispatchError::new(
                BleErrorCode::StreamQuota,
                "restoration",
                operation,
            ));
        }
        let id = self.id(prefix);
        handles.insert(
            id.clone(),
            (
                generation,
                RendererScanFailureRecoveryHandle::ReservedCleanup,
            ),
        );
        Ok(id)
    }

    fn cancel_scan_failure_recovery_cleanup_reservation(
        &self,
        key: &str,
        generation: u64,
        id: &str,
    ) {
        let mut aliases = self
            .scan_failure_recovery_handles
            .lock()
            .expect("scan recovery handle mutex poisoned");
        let Some(handles) = aliases.get_mut(key) else {
            return;
        };
        if matches!(handles.get(id), Some((owner, RendererScanFailureRecoveryHandle::ReservedCleanup)) if *owner == generation)
        {
            handles.remove(id);
        }
        if handles.is_empty() {
            aliases.remove(key);
        }
    }

    fn commit_scan_failure_recovery_cleanup_reservation(
        &self,
        key: &str,
        generation: u64,
        id: &str,
        cleanup: RendererScanFailureRecoveryHandle,
        operation: &'static str,
    ) -> Result<(), DispatchError> {
        let admission = self
            .scan_failure_recovery_admission
            .lock()
            .expect("scan recovery admission mutex poisoned");
        if admission.get(key).is_none_or(|entry| {
            entry.owner_generation != Some(generation) || entry.generation != generation
        }) {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "restoration",
                operation,
            ));
        }
        let mut aliases = self
            .scan_failure_recovery_handles
            .lock()
            .expect("scan recovery handle mutex poisoned");
        let Some(handles) = aliases.get_mut(key) else {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "restoration",
                operation,
            ));
        };
        if !matches!(handles.get(id), Some((owner, RendererScanFailureRecoveryHandle::ReservedCleanup)) if *owner == generation)
        {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "restoration",
                operation,
            ));
        }
        handles.insert(id.to_owned(), (generation, cleanup));
        Ok(())
    }

    fn retire_completed_scan_failure_aliases(&self, key: &str, generation: u64, cleanup_id: &str) {
        let mut aliases = self
            .scan_failure_recovery_handles
            .lock()
            .expect("scan recovery handle mutex poisoned");
        let Some(handles) = aliases.get_mut(key) else {
            return;
        };
        let owner = handles
            .get(cleanup_id)
            .and_then(|(entry_generation, handle)| {
                (*entry_generation == generation).then_some(match handle {
                    RendererScanFailureRecoveryHandle::Cleanup { owner, .. } => *owner,
                    _ => None,
                })
            })
            .flatten();
        handles.retain(|id, (entry_generation, handle)| {
            if *entry_generation != generation || id == cleanup_id {
                return id != cleanup_id;
            }
            let Some(owner) = owner else {
                return true;
            };
            match handle {
                RendererScanFailureRecoveryHandle::Owner(candidate) => *candidate != owner,
                RendererScanFailureRecoveryHandle::Head(candidate) => {
                    candidate.ordinal != owner.ordinal || candidate.capability != owner.capability
                }
                RendererScanFailureRecoveryHandle::ReservedCleanup => true,
                RendererScanFailureRecoveryHandle::Cleanup { .. } => true,
            }
        });
    }

    /// Reclaim the registry cache only after the durable recording has no
    /// owner and no cleanup replay receipt. All aliases for that recording are
    /// then stale by definition, so revoke them atomically with the trusted
    /// admission rather than evicting any still-actionable capability.
    async fn reclaim_fully_retired_scan_failure_recovery(
        &self,
        key: &str,
        lease: &Arc<ubm_desktop::scan_failure_recovery::ScanFailureRecoveryLease>,
    ) -> Result<(), DispatchError> {
        let _lifecycle = self.scan_failure_recovery_lifecycle.lock().await;
        if !lease
            .is_fully_retired()
            .map_err(|error| DispatchError::from_core(&error))?
        {
            return Ok(());
        }
        let recording_id = self
            .scan_failure_recovery_admission
            .lock()
            .expect("scan recovery admission mutex poisoned")
            .get(key)
            .filter(|entry| {
                entry
                    .lease
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, lease))
            })
            .and_then(|entry| entry.recording_id.clone());
        let Some(recording_id) = recording_id else {
            return Ok(());
        };
        self.scan_failure_recovery
            .detach_trusted(&recording_id)
            .map_err(|error| DispatchError::from_core(&error))?;
        let revoked = {
            let mut admission = self
                .scan_failure_recovery_admission
                .lock()
                .expect("scan recovery admission mutex poisoned");
            let keys = admission
                .iter()
                .filter(|&(_candidate, entry)| {
                    entry.recording_id.as_deref() == Some(recording_id.as_str())
                        && entry.owner_generation == Some(entry.generation)
                })
                .map(|(candidate, _entry)| candidate.clone())
                .collect::<Vec<_>>();
            for candidate in &keys {
                let entry = admission
                    .get_mut(candidate)
                    .expect("admission key was collected from this map");
                entry.generation = entry.generation.wrapping_add(1);
                entry.owner_generation = None;
                entry.recording_id = None;
                entry.lease = None;
            }
            keys
        };
        let mut handles = self
            .scan_failure_recovery_handles
            .lock()
            .expect("scan recovery handle mutex poisoned");
        for candidate in revoked {
            handles.remove(&candidate);
        }
        Ok(())
    }

    /// Trusted-main opt-in for one exact Tauri caller. The caller identity is
    /// supplied by the host, never read from IPC payload; page reload and
    /// window teardown revoke this grant with the caller's other resources.
    pub async fn enable_peer_failure_recovery_for(
        &self,
        caller: AuthenticatedCaller,
        recording_id: &str,
    ) -> Result<(), serde_json::Value> {
        self.enable_peer_failure_recovery_for_after(
            caller,
            self.continuation_attach_peer_failure_recovery_recording(recording_id),
        )
        .await
    }

    async fn enable_peer_failure_recovery_for_after<F>(
        &self,
        caller: AuthenticatedCaller,
        attach: F,
    ) -> Result<(), serde_json::Value>
    where
        F: Future<Output = Result<(), serde_json::Value>>,
    {
        let key = caller_key(&caller);
        let (lease_generation, protocol_supported) = self
            .inner
            .lock()
            .await
            .callers
            .get(&key)
            .filter(|caller| !caller.retired)
            .map(|caller| {
                (
                    caller.lease_generation.clone(),
                    attachment_diagnostics_supported(&caller.versions),
                )
            })
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "restoration",
                    "tauri.peer-failure-recovery.grant",
                )
                .normalized_error()
                .into_wire()
            })?;
        if !protocol_supported {
            return Err(DispatchError::new(
                BleErrorCode::CapabilityUnsupported,
                "restoration",
                "tauri.peer-failure-recovery.grant",
            )
            .normalized_error()
            .into_wire());
        }
        let generation = self
            .peer_failure_recovery_admission
            .lock()
            .expect("peer failure recovery admission mutex poisoned")
            .entry(key.clone())
            .or_default()
            .generation;

        attach.await?;

        // A release cannot await this admission mutex.  Its synchronous
        // generation advance therefore wins over this post-attach grant,
        // including when a new renderer replaces the same caller identity.
        let current = self
            .inner
            .lock()
            .await
            .callers
            .get(&key)
            .filter(|caller| !caller.retired)
            .map(|caller| {
                (
                    caller.lease_generation.clone(),
                    attachment_diagnostics_supported(&caller.versions),
                )
            });
        let mut admission = self
            .peer_failure_recovery_admission
            .lock()
            .expect("peer failure recovery admission mutex poisoned");
        let entry = admission.entry(key).or_default();
        if current.as_ref().is_none_or(|(current_lease, supported)| {
            current_lease != &lease_generation || !supported
        }) || entry.generation != generation
        {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "restoration",
                "tauri.peer-failure-recovery.grant",
            )
            .normalized_error()
            .into_wire());
        }
        entry.owner_generation = Some(generation);
        Ok(())
    }

    fn peer_failure_recovery_owner(
        &self,
        caller: &AuthenticatedCaller,
        operation: &'static str,
    ) -> Result<(String, u64), DispatchError> {
        let key = caller_key(caller);
        let generation = self
            .peer_failure_recovery_admission
            .lock()
            .expect("peer failure recovery admission mutex poisoned")
            .get(&key)
            .and_then(|entry| {
                (entry.owner_generation == Some(entry.generation)).then_some(entry.generation)
            });
        generation
            .map(|generation| (key, generation))
            .ok_or_else(|| {
                DispatchError::new(BleErrorCode::OwnershipDenied, "restoration", operation)
            })
    }

    fn revoke_peer_failure_recovery_admission(&self, key: &str) {
        let mut admission = self
            .peer_failure_recovery_admission
            .lock()
            .expect("peer failure recovery admission mutex poisoned");
        let entry = admission.entry(key.to_owned()).or_default();
        entry.generation = entry.generation.wrapping_add(1);
        entry.owner_generation = None;
    }

    async fn require_peer_failure_recovery_protocol(
        &self,
        caller: &AuthenticatedCaller,
    ) -> Result<(), DispatchError> {
        let key = caller_key(caller);
        if self
            .inner
            .lock()
            .await
            .callers
            .get(&key)
            .is_some_and(|state| attachment_diagnostics_supported(&state.versions))
        {
            Ok(())
        } else {
            Err(DispatchError::new(
                BleErrorCode::CapabilityUnsupported,
                "restoration",
                "tauri.peer-failure-recovery.protocol",
            ))
        }
    }

    async fn require_ordered_route_protocol(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
    ) -> Result<(), DispatchError> {
        if !is_ordered_route_command(command) {
            return Ok(());
        }
        let key = caller_key(caller);
        if self
            .inner
            .lock()
            .await
            .callers
            .get(&key)
            .and_then(|state| selected_ipc_protocol(&state.versions))
            .is_some_and(|version| version >= ORDERED_ROUTE_MINIMUM_IPC_PROTOCOL)
        {
            Ok(())
        } else {
            Err(DispatchError::new(
                BleErrorCode::CapabilityUnsupported,
                "gatt",
                "tauri.ordered.ipc-v9-required",
            ))
        }
    }

    /// The terminal-evidence grammar is deliberately a later opt-in than the
    /// ordinary IPC baseline. Until the trusted host has a recording-owned
    /// durable ledger, this route remains unavailable: it must never be
    /// emulated with a renderer-selected directory or an in-memory cache.
    async fn terminal_evidence_route(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        payload: &BTreeMap<String, IpcValue>,
        binary_payload: Option<Vec<u8>>,
    ) -> Result<IpcValue, DispatchError> {
        let owner = {
            let state = self.inner.lock().await;
            let key = caller_key(caller);
            let caller_state = state.callers.get(&key).ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "restoration",
                    "tauri.terminal-evidence-owner",
                )
            })?;
            if !selected_ipc_protocol(&caller_state.versions)
                .is_some_and(|value| value >= TERMINAL_EVIDENCE_ROUTE_MINIMUM_IPC_PROTOCOL)
            {
                return Err(DispatchError::new(
                    BleErrorCode::CapabilityUnsupported,
                    "restoration",
                    "tauri.terminal-evidence.ipc-v11-required",
                ));
            }
            TerminalEvidenceOwner {
                producer: String::new(),
                caller_key: key,
                lease_id: caller_state.lease_id.clone(),
                lease_generation: caller_state.lease_generation.clone(),
                recovery_lease_id: terminal_evidence_recovery_lease_id(
                    &caller_state.lease_id,
                    &caller_state.lease_generation,
                ),
                reservation_id: String::new(),
                recoverable: false,
            }
        };
        if owner.lease_id.is_empty() {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "restoration",
                "tauri.terminal-evidence-owner",
            ));
        }
        let ledger = self
            .terminal_evidence_ledger
            .lock()
            .expect("terminal evidence ledger mutex poisoned")
            .clone()
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::CapabilityUnsupported,
                    "restoration",
                    "tauri.terminal-evidence.ledger-unconfigured",
                )
            })?;
        match command {
            "terminalEvidence.reserve" => {
                if binary_payload.is_some() {
                    return Err(DispatchError::new(
                        BleErrorCode::ProtocolMalformed,
                        "ipc",
                        command,
                    ));
                }
                let request = terminal_evidence::parse_reserve(payload)?;
                let producer = request.producer.clone();
                let reservation = match ledger.reserve(
                    &ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
                        producer: request.producer,
                        manager_id: request.scope.manager_id,
                        operation_id: request.scope.operation_id,
                        incarnation: request.scope.incarnation,
                        scope_utf16le: request.scope.scope_utf16le,
                    },
                    request.maximum_bytes,
                ) {
                    Ok(reservation) => reservation,
                    Err(ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Quota) => {
                        return Ok(object([
                            ("kind", string("refused")),
                            ("reason", string("quota-exhausted")),
                        ]));
                    }
                    Err(error) => {
                        return Err(DispatchError::new(
                            BleErrorCode::PlatformFailure,
                            "restoration",
                            "tauri.terminal-evidence.reserve",
                        )
                        .platform(error.to_string()))
                    }
                };
                let mut state = self.inner.lock().await;
                let owner = TerminalEvidenceOwner {
                    producer,
                    reservation_id: reservation.reservation_id.clone(),
                    recoverable: false,
                    ..owner.clone()
                };
                if let Some(existing) = state
                    .terminal_evidence_owners
                    .get(&reservation.reservation_id)
                {
                    if existing.caller_key != owner.caller_key
                        || existing.lease_id != owner.lease_id
                        || existing.lease_generation != owner.lease_generation
                    {
                        return Err(DispatchError::new(
                            BleErrorCode::OwnershipDenied,
                            "restoration",
                            "tauri.terminal-evidence.reserve-owner",
                        ));
                    }
                }
                state
                    .terminal_evidence_owners
                    .insert(reservation.reservation_id.clone(), owner);
                Ok(object([
                    ("kind", string("reserved")),
                    ("reservationId", string(reservation.reservation_id)),
                    (
                        "maximumBytes",
                        number(i64::try_from(reservation.maximum_bytes).map_err(|_| {
                            DispatchError::new(
                                BleErrorCode::PlatformFailure,
                                "restoration",
                                "tauri.terminal-evidence.reserve",
                            )
                        })?),
                    ),
                ]))
            }
            "terminalEvidence.commit" => {
                let request = terminal_evidence::parse_commit(payload, binary_payload)?;
                let reservation_owner = self
                    .terminal_evidence_owner(
                        &owner,
                        &request.reservation_id,
                        "tauri.terminal-evidence.commit",
                    )
                    .await?;
                let digest = terminal_evidence::digest_bytes(&request.digest)?;
                let committed = match ledger.commit(
                    &request.reservation_id,
                    &request.canonical_bytes,
                    digest,
                ) {
                    Ok(committed) => committed,
                    Err(ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Quota) => {
                        return Ok(object([
                            ("kind", string("refused")),
                            ("reason", string("quota-exhausted")),
                        ]));
                    }
                    Err(error) => {
                        return Err(DispatchError::new(
                            BleErrorCode::PlatformFailure,
                            "restoration",
                            "tauri.terminal-evidence.commit",
                        )
                        .platform(error.to_string()))
                    }
                };
                self.inner
                    .lock()
                    .await
                    .terminal_evidence_owners
                    .insert(committed.reference.clone(), reservation_owner);
                Ok(object([
                    ("kind", string("committed")),
                    ("ref", string(committed.reference)),
                    ("byteLength", string(committed.byte_length.to_string())),
                    ("digest", string(request.digest)),
                    ("formatVersion", number(i64::from(committed.format_version))),
                    ("durability", string("committed")),
                ]))
            }
            "terminalEvidence.read" => {
                if binary_payload.is_some() {
                    return Err(DispatchError::new(
                        BleErrorCode::ProtocolMalformed,
                        "ipc",
                        command,
                    ));
                }
                let reference =
                    terminal_evidence::parse_reference(payload, "tauri.terminal-evidence.read")?;
                self.terminal_evidence_owner(&owner, &reference, "tauri.terminal-evidence.read")
                    .await?;
                let (canonical_bytes, digest) =
                    ledger.read(&reference, u64::MAX).map_err(|error| {
                        DispatchError::new(
                            BleErrorCode::PlatformFailure,
                            "restoration",
                            "tauri.terminal-evidence.read",
                        )
                        .platform(error.to_string())
                    })?;
                Ok(object([
                    ("kind", string("committed")),
                    ("canonicalBytes", IpcValue::Bytes(canonical_bytes)),
                    (
                        "digest",
                        string(
                            digest
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<String>(),
                        ),
                    ),
                ]))
            }
            "terminalEvidence.release" => {
                if binary_payload.is_some() {
                    return Err(DispatchError::new(
                        BleErrorCode::ProtocolMalformed,
                        "ipc",
                        command,
                    ));
                }
                let reservation_id = terminal_evidence::parse_release(payload)?;
                let reservation_owner = self
                    .terminal_evidence_owner(
                        &owner,
                        &reservation_id,
                        "tauri.terminal-evidence.release",
                    )
                    .await?;
                ledger.abandon(&reservation_id).map_err(|error| {
                    DispatchError::new(
                        BleErrorCode::PlatformFailure,
                        "restoration",
                        "tauri.terminal-evidence.release",
                    )
                    .platform(error.to_string())
                })?;
                // Abandonment ends only the caller's source-time reservation.
                // A staged or committed receipt remains durable until its
                // authenticated recovery acknowledgement, so retain any
                // opaque reference mapping owned by this reservation.
                self.inner
                    .lock()
                    .await
                    .terminal_evidence_owners
                    .remove(&reservation_owner.reservation_id);
                Ok(object([("kind", string("released"))]))
            }
            command @ ("terminalEvidence.recovery.list"
            | "terminalEvidence.recovery.listWithStatus") => {
                let busy_producers = if command == "terminalEvidence.recovery.listWithStatus" {
                    terminal_evidence::parse_recovery_list_with_status(payload)?;
                    self.refresh_terminal_evidence_recovery(caller).await?
                } else {
                    terminal_evidence::parse_recovery_list(payload)?;
                    let busy_producers = self.refresh_terminal_evidence_recovery(caller).await?;
                    if !busy_producers.is_empty() {
                        let mut error = DispatchError::new(
                            BleErrorCode::PlatformFailure,
                            "restoration",
                            "tauri.terminal-evidence.recovery.list",
                        );
                        error.retryability = Retryability::CallerDecides;
                        return Err(error.platform("terminal evidence recovery is busy"));
                    }
                    busy_producers
                };
                let handles = self
                    .inner
                    .lock()
                    .await
                    .terminal_evidence_owners
                    .iter()
                    .filter(|(handle, mapped)| {
                        *handle != &mapped.reservation_id
                            && mapped.recoverable
                            && mapped.caller_key == owner.caller_key
                            && mapped.lease_id == owner.lease_id
                            && mapped.lease_generation == owner.lease_generation
                    })
                    .map(|(handle, mapped)| (handle.clone(), mapped.clone()))
                    .collect::<Vec<_>>();
                let mut records = Vec::with_capacity(handles.len());
                for (handle, mapped) in handles {
                    let (bytes, digest) = ledger.read(&handle, u64::MAX).map_err(|error| {
                        DispatchError::new(
                            BleErrorCode::PlatformFailure,
                            "restoration",
                            "tauri.terminal-evidence.recovery.list",
                        )
                        .platform(error.to_string())
                    })?;
                    records.push(object([
                        ("handle", string(handle)),
                        ("producer", string(mapped.producer)),
                        (
                            "byteLength",
                            number(i64::try_from(bytes.len()).map_err(|_| {
                                DispatchError::new(
                                    BleErrorCode::PlatformFailure,
                                    "restoration",
                                    "tauri.terminal-evidence.recovery.list",
                                )
                            })?),
                        ),
                        (
                            "digest",
                            string(
                                digest
                                    .iter()
                                    .map(|byte| format!("{byte:02x}"))
                                    .collect::<String>(),
                            ),
                        ),
                        ("formatVersion", number(1)),
                    ]));
                }
                if command == "terminalEvidence.recovery.listWithStatus" {
                    return Ok(object([
                        ("records", IpcValue::Array(records)),
                        (
                            "busyProducers",
                            IpcValue::Array(busy_producers.into_iter().map(string).collect()),
                        ),
                    ]));
                }
                Ok(object([("records", IpcValue::Array(records))]))
            }
            "terminalEvidence.recovery.read" => {
                let (handle, maximum_bytes) = terminal_evidence::parse_recovery_read(payload)?;
                let mapped = self
                    .terminal_evidence_owner(
                        &owner,
                        &handle,
                        "tauri.terminal-evidence.recovery.read",
                    )
                    .await?;
                if !mapped.recoverable {
                    return Err(DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "restoration",
                        "tauri.terminal-evidence.recovery.read",
                    ));
                }
                let (canonical_bytes, digest) =
                    ledger.read(&handle, maximum_bytes).map_err(|error| {
                        DispatchError::new(
                            BleErrorCode::PlatformFailure,
                            "restoration",
                            "tauri.terminal-evidence.recovery.read",
                        )
                        .platform(error.to_string())
                    })?;
                Ok(object([
                    ("kind", string("committed")),
                    ("canonicalBytes", IpcValue::Bytes(canonical_bytes)),
                    (
                        "digest",
                        string(
                            digest
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<String>(),
                        ),
                    ),
                ]))
            }
            "terminalEvidence.recovery.ack" => {
                let handle = terminal_evidence::parse_recovery_ack(payload)?;
                let mapped = self
                    .terminal_evidence_owner(
                        &owner,
                        &handle,
                        "tauri.terminal-evidence.recovery.ack",
                    )
                    .await?;
                if !mapped.recoverable {
                    return Err(DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "restoration",
                        "tauri.terminal-evidence.recovery.ack",
                    ));
                }
                // The webview supplies only its opaque recovery alias. Bind
                // the destructive acknowledgement to the producer and
                // recipient persisted by the trusted host at staging time;
                // current caller/lease strings are authorization checks, not
                // storage claims.
                ledger
                    .acknowledge_recovered(
                        &mapped.recovery_lease_id,
                        &mapped.reservation_id,
                        &handle,
                        &mapped.producer,
                        &mapped.caller_key,
                    )
                    .map_err(|error| {
                        DispatchError::new(
                            BleErrorCode::PlatformFailure,
                            "restoration",
                            "tauri.terminal-evidence.recovery.ack",
                        )
                        .platform(error.to_string())
                    })?;
                self.inner
                    .lock()
                    .await
                    .terminal_evidence_owners
                    .retain(|_, entry| entry.reservation_id != mapped.reservation_id);
                Ok(object([("state", string("released"))]))
            }
            _ => unreachable!("terminal evidence route guard admitted an unknown command"),
        }
    }

    async fn terminal_evidence_owner(
        &self,
        caller: &TerminalEvidenceOwner,
        handle: &str,
        operation: &'static str,
    ) -> Result<TerminalEvidenceOwner, DispatchError> {
        self.inner
            .lock()
            .await
            .terminal_evidence_owners
            .get(handle)
            .filter(|owner| {
                owner.caller_key == caller.caller_key
                    && owner.lease_id == caller.lease_id
                    && owner.lease_generation == caller.lease_generation
            })
            .cloned()
            .ok_or_else(|| {
                DispatchError::new(BleErrorCode::OwnershipDenied, "restoration", operation)
            })
    }

    async fn terminal_evidence_binding(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
        producer: &str,
        operation: &'static str,
    ) -> Result<Option<TerminalEvidenceBinding>, DispatchError> {
        let Some(scope) = terminal_evidence::parse_source_scope(payload, producer, operation)?
        else {
            return Ok(None);
        };
        let ledger = self
            .terminal_evidence_ledger
            .lock()
            .expect("terminal evidence ledger mutex poisoned")
            .clone()
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::CapabilityUnsupported,
                    "stream",
                    "tauri.terminal-evidence.ledger-unconfigured",
                )
            })?;
        let reservation = ledger.attach(&scope).map_err(|error| {
            DispatchError::new(BleErrorCode::PlatformFailure, "stream", operation)
                .platform(error.to_string())
        })?;
        let (lease_id, lease_generation) = expected_lease(payload, operation)?;
        self.terminal_evidence_owner(
            &TerminalEvidenceOwner {
                producer: producer.to_owned(),
                caller_key: caller_key(caller),
                recovery_lease_id: terminal_evidence_recovery_lease_id(
                    &lease_id,
                    &lease_generation,
                ),
                lease_id,
                lease_generation,
                reservation_id: String::new(),
                recoverable: false,
            },
            &reservation.reservation_id,
            operation,
        )
        .await?;
        Ok(Some(TerminalEvidenceBinding {
            reservation_id: reservation.reservation_id,
            scope,
        }))
    }

    fn renderer_peer_failure_recovery_handle(
        &self,
        caller: &AuthenticatedCaller,
        handle_id: &str,
        operation: &'static str,
    ) -> Result<RendererPeerFailureRecoveryHandle, DispatchError> {
        let (key, generation) = self.peer_failure_recovery_owner(caller, operation)?;
        self.peer_failure_recovery_handles
            .lock()
            .expect("peer failure recovery handle mutex poisoned")
            .get(&key)
            .and_then(|handles| handles.get(handle_id))
            .filter(|handle| handle.owner_generation == generation)
            .cloned()
            .ok_or_else(|| {
                DispatchError::new(BleErrorCode::OwnershipDenied, "restoration", operation)
            })
    }

    /// Atomically publish handles only for the exact grant generation that
    /// enumerated them.  Kept separate so the list/release interleaving is
    /// directly regression-tested without depending on scheduler timing.
    fn install_peer_failure_recovery_handles(
        &self,
        key: String,
        generation: u64,
        handles: HashMap<String, RendererPeerFailureRecoveryHandle>,
    ) -> Result<(), DispatchError> {
        let admission = self
            .peer_failure_recovery_admission
            .lock()
            .expect("peer failure recovery admission mutex poisoned");
        if admission.get(&key).is_none_or(|entry| {
            entry.owner_generation != Some(generation) || entry.generation != generation
        }) {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "restoration",
                "tauri.peer-failure-recovery.list",
            ));
        }
        self.peer_failure_recovery_handles
            .lock()
            .expect("peer failure recovery handle mutex poisoned")
            .insert(key, handles);
        Ok(())
    }

    fn reserve_peer_failure_recovery_acknowledgement(
        &self,
        key: &str,
        handle_id: &str,
    ) -> Result<(u64, RendererPeerFailureRecoveryHandle), DispatchError> {
        let mut admission = self
            .peer_failure_recovery_admission
            .lock()
            .expect("peer failure recovery admission mutex poisoned");
        let generation = admission
            .get(key)
            .and_then(|entry| {
                (entry.owner_generation == Some(entry.generation)).then_some(entry.generation)
            })
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "restoration",
                    "tauri.peer-failure-recovery.ack",
                )
            })?;
        let handle = self
            .peer_failure_recovery_handles
            .lock()
            .expect("peer failure recovery handle mutex poisoned")
            .get_mut(key)
            .and_then(|handles| {
                (handles
                    .get(handle_id)
                    .is_some_and(|handle| handle.owner_generation == generation))
                .then(|| handles.remove(handle_id))
                .flatten()
            })
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "restoration",
                    "tauri.peer-failure-recovery.ack",
                )
            })?;
        admission
            .get_mut(key)
            .expect("granted peer failure recovery admission disappeared")
            .in_flight_acknowledgements
            .insert((generation, handle_id.to_owned()));
        Ok((generation, handle))
    }

    fn finish_peer_failure_recovery_acknowledgement(
        &self,
        key: &str,
        generation: u64,
        handle_id: &str,
    ) {
        self.peer_failure_recovery_admission
            .lock()
            .expect("peer failure recovery admission mutex poisoned")
            .get_mut(key)
            .expect("peer failure recovery admission must outlive an admitted acknowledgement")
            .in_flight_acknowledgements
            .remove(&(generation, handle_id.to_owned()));
    }

    /// Renderer route: mint opaque, caller-bound handles for all entries in
    /// the trusted host's already-attached durable ledger.  The webview is
    /// deliberately not allowed to choose a recording, descriptor, consumer,
    /// or recipient lease.
    async fn renderer_peer_failure_recovery_list(
        &self,
        caller: &AuthenticatedCaller,
    ) -> Result<IpcValue, DispatchError> {
        let (key, generation) =
            self.peer_failure_recovery_owner(caller, "tauri.peer-failure-recovery.list")?;
        let authority = self.ensure_authority().await?;
        let mut handles = HashMap::new();
        let mut projected = Vec::new();
        for occurrence in authority.durable_peer_failure_recoveries() {
            let producer_ack = match occurrence.producer_ack {
                ubm_desktop::central::DurablePeerFailureProducerAck::NotStarted => "not-started",
                ubm_desktop::central::DurablePeerFailureProducerAck::InFlight => "in-flight",
                ubm_desktop::central::DurablePeerFailureProducerAck::Committed => "committed",
            };
            for recipient in occurrence.recipients {
                let handle_id = self.id("tauri-peer-failure-recovery");
                let selector = CoreSelector {
                    service_uuid: recipient.scope.1.clone(),
                    service_occurrence: Some(recipient.scope.2),
                    characteristic_uuid: Some(recipient.scope.3.clone()),
                    characteristic_occurrence: Some(recipient.scope.4),
                    descriptor_uuid: None,
                    descriptor_occurrence: None,
                };
                handles.insert(
                    handle_id.clone(),
                    RendererPeerFailureRecoveryHandle {
                        owner_generation: generation,
                        descriptor: occurrence.descriptor,
                        peer_id: recipient.scope.0.clone(),
                        selector: selector.clone(),
                        consumer: recipient.consumer,
                        recipient_lease: recipient.recipient_lease,
                    },
                );
                projected.push(object([
                    ("handleId", string(handle_id)),
                    ("peerId", string(recipient.scope.0)),
                    (
                        "selector",
                        object([
                            ("serviceUuid", string(selector.service_uuid)),
                            ("serviceOccurrence", string(recipient.scope.2.to_string())),
                            ("characteristicUuid", string(recipient.scope.3)),
                            (
                                "characteristicOccurrence",
                                string(recipient.scope.4.to_string()),
                            ),
                        ]),
                    ),
                    (
                        "contiguousRead",
                        string(recipient.contiguous_read.to_string()),
                    ),
                    ("acknowledged", IpcValue::Bool(recipient.acknowledged)),
                    ("producerAck", string(producer_ack)),
                ]));
            }
        }
        // Check the grant and install its handles under the same admission
        // lock.  A release or a re-bootstrap using the same caller key
        // therefore either happens before this block (and denies it), or
        // happens after it and removes these exact-generation handles.
        self.install_peer_failure_recovery_handles(key, generation, handles)?;
        Ok(object([("recoveries", IpcValue::Array(projected))]))
    }

    async fn renderer_peer_failure_recovery_read(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle_id = required_string(payload, "handleId", "tauri.peer-failure-recovery.read")?;
        let offset = peer_failure_recovery_offset(
            &required_string(payload, "offset", "tauri.peer-failure-recovery.read")?,
            "tauri.peer-failure-recovery.read",
        )?;
        let handle = self.renderer_peer_failure_recovery_handle(
            caller,
            &handle_id,
            "tauri.peer-failure-recovery.read",
        )?;
        let authority = self.ensure_authority().await?;
        let chunk = authority
            .read_peer_notification_failure_recovery(
                &handle.descriptor,
                &handle.peer_id,
                &handle.selector,
                &handle.consumer,
                &handle.recipient_lease,
                offset,
            )
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        self.renderer_peer_failure_recovery_handle(
            caller,
            &handle_id,
            "tauri.peer-failure-recovery.read",
        )?;
        Ok(object([
            ("offset", string(chunk.offset.to_string())),
            ("nextOffset", string(chunk.next_offset.to_string())),
            ("bytes", IpcValue::Bytes(chunk.bytes)),
            ("done", IpcValue::Bool(chunk.done)),
        ]))
    }

    async fn renderer_peer_failure_recovery_acknowledge(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle_id = required_string(payload, "handleId", "tauri.peer-failure-recovery.ack")?;
        let key = caller_key(caller);
        // Opening the shared authority may await.  Do it before reserving an
        // ACK admission so a release during authority startup wins without a
        // retained destructive-operation lease.
        let authority = self.ensure_authority().await?;
        // The removal is the ACK linearization point.  It is done while the
        // caller generation is still granted, before awaiting native work, so
        // release-first never consumes a retained record.  Release-after
        // this point revokes all future access but cannot transfer this exact
        // admitted operation to a replacement renderer with the same key.
        let (generation, handle) =
            self.reserve_peer_failure_recovery_acknowledgement(&key, &handle_id)?;
        let result = authority
            .acknowledge_peer_notification_failure_recovery(
                &handle.descriptor,
                &handle.peer_id,
                &handle.selector,
                &handle.consumer,
                &handle.recipient_lease,
            )
            .await
            .map_err(|error| DispatchError::from_core(&error));
        self.finish_peer_failure_recovery_acknowledgement(&key, generation, &handle_id);
        result?;
        Ok(object([("acknowledged", IpcValue::Bool(true))]))
    }

    async fn renderer_scan_failure_recovery(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let operation: &'static str = match command {
            "scanFailureRecovery.list" => "tauri.scan-failure-recovery.list",
            "scanFailureRecovery.owners" => "tauri.scan-failure-recovery.owners",
            "scanFailureRecovery.member" => "tauri.scan-failure-recovery.member",
            "scanFailureRecovery.head" => "tauri.scan-failure-recovery.head",
            "scanFailureRecovery.read" => "tauri.scan-failure-recovery.read",
            "scanFailureRecovery.ack" => "tauri.scan-failure-recovery.ack",
            "scanFailureRecovery.retire" => "tauri.scan-failure-recovery.retire",
            "scanFailureRecovery.release" => "tauri.scan-failure-recovery.release",
            "scanFailureRecovery.finalize" => "tauri.scan-failure-recovery.finalize",
            "scanFailureRecovery.cleanup.list" => "tauri.scan-failure-recovery.cleanup.list",
            "scanFailureRecovery.cleanup.retire" => "tauri.scan-failure-recovery.cleanup.retire",
            "scanFailureRecovery.raw.info" => "tauri.scan-failure-recovery.raw.info",
            "scanFailureRecovery.raw.member" => "tauri.scan-failure-recovery.raw.member",
            "scanFailureRecovery.raw.token" => "tauri.scan-failure-recovery.raw.token",
            "scanFailureRecovery.raw.retire" => "tauri.scan-failure-recovery.raw.retire",
            "scanFailureRecovery.raw.release" => "tauri.scan-failure-recovery.raw.release",
            "scanFailureRecovery.raw.finalize" => "tauri.scan-failure-recovery.raw.finalize",
            _ => {
                return Err(DispatchError::new(
                    BleErrorCode::ProtocolMalformed,
                    "ipc",
                    "tauri.scan-failure-recovery.route",
                ))
            }
        };
        let (key, generation, lease) = self.scan_failure_recovery_lease(caller, operation).await?;
        if command == "scanFailureRecovery.list" {
            // A recovery id is an alias only; owners are enumerated later. Its
            // sentinel is never passed to the lease.
            let recovery_id = self.retain_scan_failure_recovery_handle(
                &key,
                generation,
                RendererScanFailureRecoveryHandle::Owner(
                    ubm_desktop::continuation_outbox::PendingScanFailure {
                        ordinal: u64::MAX,
                        capability: [0; 16],
                    },
                ),
                "tauri-scan-failure-recovery",
                operation,
            )?;
            return Ok(object([(
                "recoveries",
                IpcValue::Array(vec![object([("handleId", string(recovery_id))])]),
            )]));
        }
        let recovery_id = required_string(payload, "recoveryId", operation)?;
        let recovery_alias =
            self.scan_failure_recovery_handle(&key, generation, &recovery_id, operation)?;
        if !matches!(
            recovery_alias,
            RendererScanFailureRecoveryHandle::Owner(
                ubm_desktop::continuation_outbox::PendingScanFailure {
                    ordinal: u64::MAX,
                    ..
                }
            )
        ) {
            return Err(DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "restoration",
                operation,
            ));
        }
        if command == "scanFailureRecovery.owners" {
            let owners = lease
                .owners()
                .into_iter()
                .map(|owner| {
                    let id = self.retain_scan_failure_recovery_handle(
                        &key,
                        generation,
                        RendererScanFailureRecoveryHandle::Owner(owner.key),
                        "tauri-scan-failure-owner",
                        operation,
                    )?;
                    Ok(object([
                        ("handleId", string(id)),
                        ("prepared", IpcValue::Bool(owner.prepared)),
                        ("producerFinished", IpcValue::Bool(owner.producer_finished)),
                        ("rawRetired", IpcValue::Bool(owner.raw_retired)),
                        ("rawReleased", IpcValue::Bool(owner.raw_released)),
                        ("memberCount", number(owner.member_count as i64)),
                        ("released", IpcValue::Bool(owner.released.is_some())),
                    ]))
                })
                .collect::<Result<Vec<_>, DispatchError>>()?;
            return Ok(object([("owners", IpcValue::Array(owners))]));
        }
        if command == "scanFailureRecovery.cleanup.list" {
            let receipts = lease
                .pending_cleanup_receipts()
                .map_err(|error| DispatchError::from_core(&error))?
                .into_iter()
                .map(|receipt| {
                    self.retain_scan_failure_recovery_handle(
                        &key,
                        generation,
                        RendererScanFailureRecoveryHandle::Cleanup {
                            receipt,
                            owner: None,
                        },
                        "tauri-scan-failure-cleanup",
                        operation,
                    )
                    .map(string)
                })
                .collect::<Result<Vec<_>, DispatchError>>()?;
            return Ok(object([("cleanupReceiptIds", IpcValue::Array(receipts))]));
        }
        let handle_id = required_string(payload, "handleId", operation)?;
        if command == "scanFailureRecovery.cleanup.retire" {
            let RendererScanFailureRecoveryHandle::Cleanup { receipt, .. } =
                self.scan_failure_recovery_handle(&key, generation, &handle_id, operation)?
            else {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "restoration",
                    operation,
                ));
            };
            lease
                .retire_cleanup(&receipt)
                .map_err(|error| DispatchError::from_core(&error))?;
            self.retire_completed_scan_failure_aliases(&key, generation, &handle_id);
            self.reclaim_fully_retired_scan_failure_recovery(&key, &lease)
                .await?;
            return Ok(object([("completed", IpcValue::Bool(true))]));
        }
        if command.starts_with("scanFailureRecovery.raw.") {
            let RendererScanFailureRecoveryHandle::Owner(owner) =
                self.scan_failure_recovery_handle(&key, generation, &handle_id, operation)?
            else {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "restoration",
                    operation,
                ));
            };
            return match command {
                "scanFailureRecovery.raw.info" => {
                    let info = lease
                        .raw_info(&owner)
                        .map_err(|error| DispatchError::from_core(&error))?;
                    Ok(object([(
                        "info",
                        object([
                            (
                                "identity",
                                object([
                                    (
                                        "host",
                                        string(peer_failure_identity_text(info.identity.host)),
                                    ),
                                    (
                                        "scan",
                                        string(peer_failure_identity_text(info.identity.scan)),
                                    ),
                                    (
                                        "start",
                                        string(peer_failure_identity_text(info.identity.start)),
                                    ),
                                    (
                                        "sourceGeneration",
                                        string(peer_failure_identity_text(
                                            info.identity.source_generation,
                                        )),
                                    ),
                                    (
                                        "occurrence",
                                        string(peer_failure_identity_text(
                                            info.identity.occurrence,
                                        )),
                                    ),
                                ]),
                            ),
                            ("memberCount", number(info.member_count as i64)),
                            ("tokenCount", number(info.token_count as i64)),
                        ]),
                    )]))
                }
                "scanFailureRecovery.raw.member" => {
                    let index = optional_recovery_index(payload, operation)?.ok_or_else(|| {
                        DispatchError::new(BleErrorCode::ProtocolMalformed, "ipc", operation)
                    })?;
                    let member = lease
                        .raw_member(&owner, index)
                        .map_err(|error| DispatchError::from_core(&error))?;
                    Ok(object([(
                        "member",
                        object([
                            (
                                "recipient",
                                string(peer_failure_identity_text(member.recipient)),
                            ),
                            (
                                "delivery",
                                string(peer_failure_identity_text(member.delivery)),
                            ),
                        ]),
                    )]))
                }
                "scanFailureRecovery.raw.token" => {
                    let index = optional_recovery_index(payload, operation)?.ok_or_else(|| {
                        DispatchError::new(BleErrorCode::ProtocolMalformed, "ipc", operation)
                    })?;
                    let offset = recovery_usize(payload, "offset", operation)?;
                    let max_bytes = recovery_usize(payload, "maxBytes", operation)?;
                    if !(1..=65_536).contains(&max_bytes) {
                        return Err(DispatchError::new(
                            BleErrorCode::ArgumentInvalid,
                            "restoration",
                            operation,
                        ));
                    }
                    let token = lease
                        .raw_token(&owner, index, offset, max_bytes)
                        .map_err(|error| DispatchError::from_core(&error))?;
                    Ok(object([("token", scan_failure_raw_token_value(token))]))
                }
                "scanFailureRecovery.raw.retire" => {
                    lease
                        .raw_retire(&owner)
                        .map_err(|error| DispatchError::from_core(&error))?;
                    Ok(object([("completed", IpcValue::Bool(true))]))
                }
                "scanFailureRecovery.raw.release" => {
                    lease
                        .raw_release(&owner)
                        .map_err(|error| DispatchError::from_core(&error))?;
                    Ok(object([("completed", IpcValue::Bool(true))]))
                }
                "scanFailureRecovery.raw.finalize" => {
                    let id = self.reserve_scan_failure_recovery_cleanup_handle(
                        &key,
                        generation,
                        "tauri-scan-failure-cleanup",
                        operation,
                    )?;
                    let receipt = match lease
                        .raw_finalize(&owner)
                        .map_err(|error| DispatchError::from_core(&error))
                    {
                        Ok(receipt) => receipt,
                        Err(error) => {
                            self.cancel_scan_failure_recovery_cleanup_reservation(
                                &key, generation, &id,
                            );
                            return Err(error);
                        }
                    };
                    self.commit_scan_failure_recovery_cleanup_reservation(
                        &key,
                        generation,
                        &id,
                        RendererScanFailureRecoveryHandle::Cleanup {
                            receipt,
                            owner: Some(owner),
                        },
                        operation,
                    )?;
                    Ok(object([("cleanupReceiptId", string(id))]))
                }
                _ => Err(DispatchError::new(
                    BleErrorCode::ProtocolMalformed,
                    "ipc",
                    operation,
                )),
            };
        }
        let index = optional_recovery_index(payload, operation)?;
        match command {
            "scanFailureRecovery.member" => {
                let RendererScanFailureRecoveryHandle::Owner(owner) =
                    self.scan_failure_recovery_handle(&key, generation, &handle_id, operation)?
                else {
                    return Err(DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "restoration",
                        operation,
                    ));
                };
                let member = lease
                    .member(
                        &owner,
                        index.ok_or_else(|| {
                            DispatchError::new(BleErrorCode::ProtocolMalformed, "ipc", operation)
                        })?,
                    )
                    .map_err(|error| DispatchError::from_core(&error))?;
                Ok(object([(
                    "member",
                    object([
                        (
                            "recipient",
                            string(peer_failure_identity_text(member.recipient)),
                        ),
                        (
                            "delivery",
                            string(peer_failure_identity_text(member.delivery)),
                        ),
                    ]),
                )]))
            }
            "scanFailureRecovery.head" => {
                let RendererScanFailureRecoveryHandle::Owner(owner) =
                    self.scan_failure_recovery_handle(&key, generation, &handle_id, operation)?
                else {
                    return Err(DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "restoration",
                        operation,
                    ));
                };
                let member = lease
                    .member(
                        &owner,
                        index.ok_or_else(|| {
                            DispatchError::new(BleErrorCode::ProtocolMalformed, "ipc", operation)
                        })?,
                    )
                    .map_err(|error| DispatchError::from_core(&error))?;
                match lease
                    .head(member)
                    .map_err(|error| DispatchError::from_core(&error))?
                {
                    None => Ok(object([("head", IpcValue::Null)])),
                    Some(head) => {
                        let contiguous_read = head.contiguous_read;
                        let acknowledged = head.acknowledged;
                        let id = self.retain_scan_failure_recovery_handle(
                            &key,
                            generation,
                            RendererScanFailureRecoveryHandle::Head(head),
                            "tauri-scan-failure-head",
                            operation,
                        )?;
                        Ok(object([(
                            "head",
                            object([
                                ("handleId", string(id)),
                                ("contiguousRead", string(contiguous_read.to_string())),
                                ("acknowledged", IpcValue::Bool(acknowledged)),
                            ]),
                        )]))
                    }
                }
            }
            "scanFailureRecovery.read" => {
                let RendererScanFailureRecoveryHandle::Head(head) =
                    self.scan_failure_recovery_handle(&key, generation, &handle_id, operation)?
                else {
                    return Err(DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "restoration",
                        operation,
                    ));
                };
                let offset = peer_failure_recovery_offset(
                    &required_string(payload, "offset", operation)?,
                    operation,
                )?;
                let chunk = lease
                    .read(&head, offset)
                    .map_err(|error| DispatchError::from_core(&error))?;
                Ok(object([
                    ("offset", string(chunk.offset.to_string())),
                    ("nextOffset", string(chunk.next_offset.to_string())),
                    ("bytes", IpcValue::Bytes(chunk.bytes)),
                    ("done", IpcValue::Bool(chunk.done)),
                ]))
            }
            "scanFailureRecovery.ack"
            | "scanFailureRecovery.retire"
            | "scanFailureRecovery.release"
            | "scanFailureRecovery.finalize" => {
                let RendererScanFailureRecoveryHandle::Head(head) =
                    self.scan_failure_recovery_handle(&key, generation, &handle_id, operation)?
                else {
                    return Err(DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "restoration",
                        operation,
                    ));
                };
                match command {
                    "scanFailureRecovery.ack" => {
                        lease
                            .acknowledge(&head)
                            .map_err(|error| DispatchError::from_core(&error))?;
                        Ok(object([("acknowledged", IpcValue::Bool(true))]))
                    }
                    "scanFailureRecovery.retire" => {
                        lease
                            .retire(&head)
                            .map_err(|error| DispatchError::from_core(&error))?;
                        Ok(object([("completed", IpcValue::Bool(true))]))
                    }
                    "scanFailureRecovery.release" => {
                        lease
                            .release(&head)
                            .map_err(|error| DispatchError::from_core(&error))?;
                        Ok(object([("completed", IpcValue::Bool(true))]))
                    }
                    _ => {
                        let id = self.reserve_scan_failure_recovery_cleanup_handle(
                            &key,
                            generation,
                            "tauri-scan-failure-cleanup",
                            operation,
                        )?;
                        let receipt = match lease
                            .finalize_release(&head)
                            .map_err(|error| DispatchError::from_core(&error))
                        {
                            Ok(receipt) => receipt,
                            Err(error) => {
                                self.cancel_scan_failure_recovery_cleanup_reservation(
                                    &key, generation, &id,
                                );
                                return Err(error);
                            }
                        };
                        let owner = ubm_desktop::continuation_outbox::PendingScanFailure {
                            ordinal: head.ordinal,
                            capability: head.capability,
                        };
                        self.commit_scan_failure_recovery_cleanup_reservation(
                            &key,
                            generation,
                            &id,
                            RendererScanFailureRecoveryHandle::Cleanup {
                                receipt,
                                owner: Some(owner),
                            },
                            operation,
                        )?;
                        Ok(object([("cleanupReceiptId", string(id))]))
                    }
                }
            }
            _ => Err(DispatchError::new(
                BleErrorCode::ProtocolMalformed,
                "ipc",
                operation,
            )),
        }
    }

    /// Enumerate durable recipient-scoped recovery handles. This is a trusted
    /// host API, not a webview command: it preserves the original opaque
    /// recipient lease and never turns a restored record into a new live
    /// subscription.
    pub async fn continuation_peer_failure_recoveries(
        &self,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let authority = self
            .ensure_authority()
            .await
            .map_err(|error| error.normalized_error().into_wire())?;
        let handles = authority
            .durable_peer_failure_recoveries()
            .into_iter()
            .flat_map(|occurrence| {
                let producer_ack = match occurrence.producer_ack {
                    ubm_desktop::central::DurablePeerFailureProducerAck::NotStarted => {
                        "not-started"
                    }
                    ubm_desktop::central::DurablePeerFailureProducerAck::InFlight => "in-flight",
                    ubm_desktop::central::DurablePeerFailureProducerAck::Committed => "committed",
                };
                occurrence.recipients.into_iter().map(move |recipient| {
                    serde_json::json!({
                        "descriptor": peer_failure_descriptor_json(&occurrence.descriptor),
                        "peerId": recipient.scope.0,
                        "selector": {
                            "serviceUuid": recipient.scope.1,
                            "serviceOccurrence": recipient.scope.2.to_string(),
                            "characteristicUuid": recipient.scope.3,
                            "characteristicOccurrence": recipient.scope.4.to_string(),
                        },
                        "consumer": recipient.consumer,
                        "recipientLease": recipient.recipient_lease,
                        "contiguousRead": recipient.contiguous_read.to_string(),
                        "acknowledged": recipient.acknowledged,
                        "producerAck": producer_ack,
                    })
                })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::Value::Array(handles))
    }

    /// Read an exact retained page using a durable handle returned by
    /// [`Self::continuation_peer_failure_recoveries`].  The host adapter
    /// keeps selector conversion and opaque lease validation in the central.
    pub async fn continuation_read_peer_failure_recovery(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        peer_id: &str,
        selector: &CoreSelector,
        consumer: &str,
        recipient_lease: &str,
        offset: u64,
    ) -> Result<ubm_core::oversized_evidence::PeerSourceFailureChunk, serde_json::Value> {
        self.ensure_authority()
            .await
            .map_err(|error| error.normalized_error().into_wire())?
            .read_peer_notification_failure_recovery(
                descriptor,
                peer_id,
                selector,
                consumer,
                recipient_lease,
                offset,
            )
            .await
            .map_err(|error| {
                DispatchError::from_core(&error)
                    .normalized_error()
                    .into_wire()
            })
    }

    /// Commit a fully read durable recovery handle. A recovered in-flight
    /// native source acknowledgement is surfaced by the central as its typed
    /// reconciliation failure rather than silently being treated as released.
    pub async fn continuation_acknowledge_peer_failure_recovery(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        peer_id: &str,
        selector: &CoreSelector,
        consumer: &str,
        recipient_lease: &str,
    ) -> Result<(), serde_json::Value> {
        self.ensure_authority()
            .await
            .map_err(|error| error.normalized_error().into_wire())?
            .acknowledge_peer_notification_failure_recovery(
                descriptor,
                peer_id,
                selector,
                consumer,
                recipient_lease,
            )
            .await
            .map_err(|error| {
                DispatchError::from_core(&error)
                    .normalized_error()
                    .into_wire()
            })
    }

    /// Trusted Rust-host configuration only. Paths never enter renderer commands.
    pub async fn continuation_configure_recording_directory(
        &self,
        directory: &std::path::Path,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let recordings = self.recordings.clone();
        let directory = directory.to_owned();
        let terminal_evidence_maximum_bytes = self.terminal_evidence_maximum_bytes;
        let configured = ubm_desktop::continuation_journal::run_blocking_result(move || {
            #[cfg(windows)]
            let configured = recordings.configure_directory_with_scan_recovery_storage(
                &directory,
                Arc::new(
                    ubm_desktop::continuation_journal::WindowsCurrentUserScanRecoveryStorageProtector::new(),
                ),
            );
            #[cfg(not(windows))]
            let configured = recordings.configure_directory(&directory);
            let configured = configured.map_err(ubm_desktop::continuation::recording_failure)?;
            let ledger = terminal_evidence_maximum_bytes
                .map(|maximum_bytes| {
                    recordings
                        .terminal_evidence_ledger(maximum_bytes)
                        .map(Arc::new)
                        .map_err(|error| {
                            serde_json::json!({
                                "code": "storage.unavailable",
                                "domain": "restoration",
                                "operation": "tauri.terminal-evidence.configure",
                                "detail": error.to_string(),
                            })
                        })
                })
                .transpose()?;
            Ok((configured, ledger))
        })
        .await?;
        if let Some(ledger) = configured.1 {
            *self
                .terminal_evidence_ledger
                .lock()
                .expect("terminal evidence ledger mutex poisoned") = Some(ledger);
        }
        Ok(configured.0)
    }
    pub async fn continuation_recording_status(
        &self,
        id: &str,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let recordings = self.recordings.clone();
        let id = id.to_owned();
        ubm_desktop::continuation_journal::run_blocking(move || {
            recordings
                .get(&id)
                .and_then(|journal| journal.status())
                .map_err(ubm_desktop::continuation::recording_failure)
        })
        .await
    }
    pub async fn continuation_recording_prepare(
        &self,
        id: &str,
        max_items: u32,
        max_bytes: u32,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let recordings = self.recordings.clone();
        let id = id.to_owned();
        ubm_desktop::continuation_journal::run_blocking(move || {
            recordings
                .get(&id)
                .and_then(|journal| journal.prepare(max_items, max_bytes))
                .map_err(ubm_desktop::continuation::recording_failure)
        })
        .await
    }
    pub async fn continuation_recording_acknowledge(
        &self,
        id: &str,
        token: &str,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let recordings = self.recordings.clone();
        let id = id.to_owned();
        let token = token.to_owned();
        ubm_desktop::continuation_journal::run_blocking(move || {
            recordings
                .get(&id)
                .and_then(|journal| journal.acknowledge(&token))
                .map_err(ubm_desktop::continuation::recording_failure)
        })
        .await
    }

    /// Trusted-host diagnostic export. This deliberately is not a plugin IPC
    /// route: the application must opt in with its own authenticated command
    /// and retain every returned raw byte before changing diagnostic state.
    pub async fn continuation_recording_diagnostic_summary_page(
        &self,
        id: &str,
        offset: &str,
        max_bytes: u32,
    ) -> Result<serde_json::Value, serde_json::Value> {
        if !(1..=DIAGNOSTIC_PAGE_LIMIT).contains(&max_bytes) {
            return Err(
                serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"invalid diagnostic page bounds","platform":null}),
            );
        }
        let offset = diagnostic_offset(offset)?;
        let recordings = self.recordings.clone();
        let id = id.to_owned();
        ubm_desktop::continuation_journal::run_blocking(move || {
            let page = recordings
                .get(&id)
                .and_then(|journal| journal.diagnostic_summary_page(offset, max_bytes))
                .map_err(ubm_desktop::continuation::recording_failure)?;
            Ok(serde_json::json!({"bytes":page.bytes,"totalBytes":page.total_bytes.to_string()}))
        })
        .await
    }

    pub async fn continuation_recording_diagnostic_occurrences_page(
        &self,
        id: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<serde_json::Value, serde_json::Value> {
        if limit == 0 || limit as usize > DIAGNOSTIC_OCCURRENCE_LIMIT {
            return Err(
                serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"invalid diagnostic occurrence page bounds","platform":null}),
            );
        }
        let after = after.map(diagnostic_occurrence_id).transpose()?;
        let recordings = self.recordings.clone();
        let id = id.to_owned();
        ubm_desktop::continuation_journal::run_blocking(move || {
            let occurrences = recordings
                .get(&id)
                .and_then(|journal| journal.diagnostic_occurrences_page(after, limit as usize))
                .map_err(ubm_desktop::continuation::recording_failure)?;
            Ok(serde_json::json!({"occurrences":occurrences.into_iter().map(diagnostic_occurrence_text).collect::<Vec<_>>() }))
        })
        .await
    }

    pub async fn continuation_recording_diagnostic_occurrence_page(
        &self,
        id: &str,
        occurrence: &str,
        offset: &str,
        max_bytes: u32,
    ) -> Result<serde_json::Value, serde_json::Value> {
        if !(1..=DIAGNOSTIC_PAGE_LIMIT).contains(&max_bytes) {
            return Err(
                serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"invalid diagnostic page bounds","platform":null}),
            );
        }
        let occurrence = diagnostic_occurrence_id(occurrence)?;
        let offset = diagnostic_offset(offset)?;
        let recordings = self.recordings.clone();
        let id = id.to_owned();
        ubm_desktop::continuation_journal::run_blocking(move || {
            let page = recordings
                .get(&id)
                .and_then(|journal| journal.diagnostic_occurrence_page(occurrence, offset, max_bytes))
                .map_err(ubm_desktop::continuation::recording_failure)?;
            Ok(serde_json::json!({"bytes":page.bytes,"totalBytes":page.total_bytes.to_string(),"durable":page.durable,"reclaimed":page.reclaimed}))
        })
        .await
    }

    pub async fn continuation_recording_retry_diagnostic_occurrence(
        &self,
        id: &str,
        occurrence: &str,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let occurrence = diagnostic_occurrence_id(occurrence)?;
        let recordings = self.recordings.clone();
        let id = id.to_owned();
        ubm_desktop::continuation_journal::run_blocking(move || {
            recordings.get(&id).and_then(|journal| journal.retry_production_loss(occurrence)).map_err(ubm_desktop::continuation::recording_failure)?;
            Ok(serde_json::json!({"occurrence":diagnostic_occurrence_text(occurrence),"retried":true}))
        }).await
    }

    pub async fn continuation_recording_transition_diagnostic_occurrence(
        &self,
        id: &str,
        occurrence: &str,
        payload: &str,
        transition: &str,
    ) -> Result<serde_json::Value, serde_json::Value> {
        if payload.len() > DIAGNOSTIC_EXACT_PAYLOAD_LIMIT {
            return Err(
                serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"diagnostic payload exceeds exact lifecycle bound","platform":null}),
            );
        }
        let occurrence = diagnostic_occurrence_id(occurrence)?;
        let id = id.to_owned();
        let payload = payload.to_owned();
        let transition = transition.to_owned();
        let recordings = self.recordings.clone();
        ubm_desktop::continuation_journal::run_blocking(move || {
            let journal = recordings.get(&id).map_err(ubm_desktop::continuation::recording_failure)?;
            match transition.as_str() {
                "acknowledge" => journal.acknowledge_diagnostic(occurrence, &payload).map(|_| ()),
                "retire" => journal.retire_diagnostic(occurrence, &payload).map(|_| ()),
                "confirm" => journal.confirm_diagnostic_retirement(occurrence, &payload),
                "release" => journal.release_diagnostic_confirmation(occurrence, &payload),
                _ => return Err(serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"unknown diagnostic lifecycle transition","platform":null})),
            }.map_err(ubm_desktop::continuation::recording_failure)?;
            Ok(serde_json::json!({"occurrence":diagnostic_occurrence_text(occurrence),"transition":transition}))
        }).await
    }

    /// Trusted host-only bounded exact lifecycle. The persisted offset lets a
    /// host resume after a crash without ever sending a whole diagnostic body.
    pub async fn continuation_recording_transition_diagnostic_occurrence_chunk(
        &self,
        id: &str,
        occurrence: &str,
        transition: &str,
        offset: &str,
        bytes: Vec<u8>,
    ) -> Result<serde_json::Value, serde_json::Value> {
        if bytes.is_empty() || bytes.len() > DIAGNOSTIC_PAGE_LIMIT as usize {
            return Err(
                serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"invalid diagnostic comparison chunk","platform":null}),
            );
        }
        let occurrence = diagnostic_occurrence_id(occurrence)?;
        let action = diagnostic_action(transition)?;
        let offset = diagnostic_offset(offset)?;
        let id = id.to_owned();
        let transition = transition.to_owned();
        let recordings = self.recordings.clone();
        ubm_desktop::continuation_journal::run_blocking(move || {
            let progress = recordings.get(&id).and_then(|journal| journal.diagnostic_transition_chunk(occurrence, action, offset, &bytes)).map_err(ubm_desktop::continuation::recording_failure)?;
            Ok(serde_json::json!({"occurrence":diagnostic_occurrence_text(occurrence),"transition":transition,"offset":progress.offset.to_string(),"totalBytes":progress.total.to_string(),"complete":progress.complete}))
        }).await
    }
    pub async fn continuation_recording_stop(
        &self,
        id: &str,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let engine = self
            .continuation
            .lock()
            .await
            .as_ref()
            .map(|continuation| continuation.engine.clone());
        let recordings = self.recordings.clone();
        let id = id.to_owned();
        ubm_desktop::continuation_journal::run_blocking(move || {
            if let Some(engine) = engine {
                engine.recording_stop(&id)
            } else {
                let mut receipt = recordings
                    .get(&id)
                    .and_then(|journal| journal.stop())
                    .map_err(ubm_desktop::continuation::recording_failure)?;
                receipt["radioRelease"] = serde_json::json!("not-requested");
                Ok(receipt)
            }
        })
        .await
    }
    /// Stop the supervised ordered producer and report its actual cleanup state.
    /// This trusted host control is separate from the legacy journal-only stop.
    pub async fn continuation_recording_stop_ordered(
        &self,
        id: &str,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let owner = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            if let Some(result) = owner.stop_owned_ordered_recording(&id).await {
                result
            } else {
                owner.recordings.stop_ordered_host_receipt(&id).await
            }
        })
        .await
        .map_err(|_| serde_json::json!({"kind":"worker-failed"}))?
    }

    /// Read the validated declaration persisted with an ordered recording.
    pub async fn continuation_recording_context_ordered(
        &self,
        id: &str,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let recordings = self.recordings.clone();
        let id = id.to_owned();
        ubm_desktop::continuation_journal::run_blocking(move || {
            recordings
                .ordered_context(&id)
                .map_err(ubm_desktop::continuation::recording_failure)
        })
        .await
    }

    pub async fn continuation_recording_clear(
        &self,
        id: &str,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let recordings = self.recordings.clone();
        let id = id.to_owned();
        ubm_desktop::continuation_journal::run_blocking(move || {
            recordings
                .get(&id)
                .and_then(|journal| journal.clear())
                .map_err(ubm_desktop::continuation::recording_failure)
        })
        .await
    }

    /// Shut the admitted authority down and refuse further BLE work loudly.
    /// Orphaned core resources get a release first; failures are reported
    /// and keep the closed authority owned for the next shutdown retry.
    /// Cancellation also leaves that ownership in the slot. Production never calls it (the
    /// process-lifetime central outlives every caller).
    pub async fn authority_shutdown(&self) -> AuthorityShutdown {
        let _shutdown = self.authority_shutdown_admission.lock().await;
        let authority = {
            let mut slot = self.authority.lock().await;
            match &*slot {
                AuthoritySlot::Open(authority) | AuthoritySlot::Closing(authority) => {
                    let authority = Arc::clone(authority);
                    *slot = AuthoritySlot::Closing(Arc::clone(&authority));
                    Some(authority)
                }
                AuthoritySlot::Unopened(_) | AuthoritySlot::ShutDown => {
                    *slot = AuthoritySlot::ShutDown;
                    None
                }
            }
        };
        if let Some(continuation) = self.continuation.lock().await.as_ref() {
            continuation.engine.stop_recovery();
        }
        let (core, orphan_failures) = match authority {
            Some(authority) => {
                let orphan_failures = match tokio::time::timeout(
                    SHUTDOWN_ORPHAN_DRAIN,
                    self.settle_orphan_debt(&authority, None),
                ).await {
                    Ok(failures) => failures
                    .into_iter()
                    .map(|failure| {
                        format!(
                            "{} ({}): {}",
                            failure.resource.kind(),
                            failure.operation(),
                            failure.describe()
                        )
                    })
                    .collect::<Vec<_>>(),
                    Err(_) => vec!["tauri.shutdown.orphan-drain: cleanup exceeded five seconds; identities remain owned until authoritative parent release".to_owned()],
                };
                let report = authority.shutdown().await;
                if report.is_released() {
                    // Current authoritative parent success retires child
                    // obligations, not the diagnostic failures above.
                    self.inner.lock().await.orphan_debt.clear();
                    *self.authority.lock().await = AuthoritySlot::ShutDown;
                }
                (Some(report), orphan_failures)
            }
            None => (None, Vec::new()),
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

    #[cfg(test)]
    fn pause_terminal_evidence_recovery_after_claim(&self) {
        let pause = self
            .terminal_evidence_recovery_pause
            .lock()
            .expect("terminal evidence recovery pause mutex poisoned")
            .clone();
        if let Some(pause) = pause {
            pause
                .claimed
                .send(())
                .expect("terminal evidence recovery test must await claim");
            pause
                .resume
                .lock()
                .expect("terminal evidence recovery pause receiver poisoned")
                .recv()
                .expect("terminal evidence recovery test must resume claim");
        }
    }

    /// Refreshes this controller's opaque recovery aliases. A busy row stays
    /// durable and is reported to the status route, while independent rows
    /// are claimed and made available immediately.
    async fn refresh_terminal_evidence_recovery(
        &self,
        caller: &AuthenticatedCaller,
    ) -> Result<Vec<String>, DispatchError> {
        let Some(ledger) = self
            .terminal_evidence_ledger
            .lock()
            .expect("terminal evidence ledger mutex poisoned")
            .clone()
        else {
            return Ok(Vec::new());
        };
        // The caller entry is the recovery lease lifecycle fence. Hold it
        // through the synchronous ledger claim and alias insertion: teardown
        // either retires this lease before any row is claimed, or observes all
        // aliases and releases their runtime authority before removing it.
        let mut state = self.inner.lock().await;
        let (recipient, lease_id, lease_generation) = {
            let caller_state = state
                .callers
                .get(&caller_key(caller))
                .filter(|caller_state| !caller_state.retired)
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::OwnershipDenied,
                        "restoration",
                        "tauri.terminal-evidence.recovery-owner",
                    )
                })?;
            (
                caller_key(caller),
                caller_state.lease_id.clone(),
                caller_state.lease_generation.clone(),
            )
        };
        let recovery_lease_id = terminal_evidence_recovery_lease_id(&lease_id, &lease_generation);
        let mut recovered = Vec::new();
        let mut busy_producers = Vec::new();
        for producer in ["scan", "subscription"] {
            let page = ledger
                .recoverable_page_for_lease_with_contention(
                    producer,
                    &recipient,
                    &recovery_lease_id,
                    u32::MAX,
                    u64::MAX,
                )
                .map_err(|error| {
                    DispatchError::new(
                        BleErrorCode::PlatformFailure,
                        "restoration",
                        "tauri.terminal-evidence.recovery-load",
                    )
                    .platform(error.to_string())
                })?;
            if page.busy {
                busy_producers.push(producer.to_owned());
            }
            for receipt in page.entries {
                let reference = match (receipt.state, receipt.reference) {
                    (
                        ubm_desktop::terminal_evidence_ledger::TerminalEvidenceRecoveryState::Committed,
                        Some(reference),
                    ) => reference,
                    (ubm_desktop::terminal_evidence_ledger::TerminalEvidenceRecoveryState::Staged, _) => {
                        use sha2::Digest;
                        ledger
                            .commit_recovered(
                                &recovery_lease_id,
                                &receipt.reservation_id,
                                &receipt.canonical_bytes,
                                sha2::Sha256::digest(&receipt.canonical_bytes).into(),
                            )
                            .map_err(|error| {
                                DispatchError::new(
                                    BleErrorCode::PlatformFailure,
                                    "restoration",
                                    "tauri.terminal-evidence.recovery-commit",
                                )
                                .platform(error.to_string())
                            })?
                            .reference
                    }
                    _ => continue,
                };
                recovered.push((reference, receipt.reservation_id, receipt.scope.producer));
            }
        }
        #[cfg(test)]
        self.pause_terminal_evidence_recovery_after_claim();
        for (reference, reservation_id, producer) in recovered {
            state.terminal_evidence_owners.insert(
                reference,
                TerminalEvidenceOwner {
                    producer,
                    caller_key: recipient.clone(),
                    lease_id: lease_id.clone(),
                    lease_generation: lease_generation.clone(),
                    recovery_lease_id: recovery_lease_id.clone(),
                    reservation_id,
                    recoverable: true,
                },
            );
        }
        Ok(busy_producers)
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
        if self.is_revoked(&key) {
            self.settle_revoked_ordered(&key).await;
            self.settle_revoked_reclaims(&key).await;
        }
        let cleanup = self.release_with_terminal_evidence(&key, true).await;
        if !is_released(&cleanup) {
            return Err(DispatchError::new(
                BleErrorCode::PlatformFailure,
                "cleanup",
                "tauri.bootstrap-prior-release",
            ));
        }
        let lease_id = self.id("tauri-lease");
        let lease_generation = self.id("tauri-lease-generation");
        self.revoke_peer_failure_recovery_admission(&key);
        self.revoke_scan_failure_recovery_admission(&key);
        self.revoked_callers
            .lock()
            .expect("revocation mutex poisoned")
            .remove(&key);
        self.inner.lock().await.callers.insert(
            key.clone(),
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
                ordered_groups: HashMap::new(),
                acquired_writers: HashMap::new(),
                acquired_releases: WatchReleaseHistory::default(),
                connection_events: HashMap::new(),
                security_watches: HashMap::new(),
                security_watch_releases: HashSet::new(),
                write_readiness_watches: HashMap::new(),
                write_readiness_releases: WatchReleaseHistory::default(),
                parameter_watches: HashMap::new(),
                parameter_releases: WatchReleaseHistory::default(),
                operations: HashMap::new(),
                completed_correlations: HashMap::new(),
                pending_events: HashSet::new(),
            },
        );
        {
            let mut state = self.inner.lock().await;
            for owner in state.terminal_evidence_owners.values_mut() {
                if owner.caller_key == key && owner.recoverable {
                    owner.lease_id.clone_from(&lease_id);
                    owner.lease_generation.clone_from(&lease_generation);
                }
            }
        }
        self.refresh_terminal_evidence_recovery(&caller).await?;
        self.replay_attachment_diagnostics(&caller_key(&caller))
            .await;

        let renderer = object([
            (
                "clientId",
                string(format!("{}:{}", caller.app_identifier, caller.window_label)),
            ),
            ("windowScope", string(caller.window_label)),
            ("sessionScope", string(lease_generation.clone())),
        ]);
        let attachment_record = attachment_record(&attachment);
        let native_capabilities = self
            .ensure_authority()
            .await?
            .capability_descriptors()
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
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
                        capabilities::snapshot(
                            &attachment.backend_generation,
                            &native_capabilities,
                        ),
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
                ));
            }
        };
        // Keep the exact attachment that this request validated. The caller
        // may be rebound by the lifecycle pump after validation but before
        // the operation task starts; consulting the caller again there would
        // let an envelope from the ended attachment ride the new binding.
        let route_attachment = self.validate_envelope(&caller, &command, &envelope).await?;
        // Ordered deliveries gained their per-observation failure grammar in
        // protocol 9. Refuse an authenticated pre-v9 request before it can
        // allocate a control, reserve GATT admission, open the authority, or
        // claim a caller correlation.
        self.require_ordered_route_protocol(&caller, &command)
            .await?;
        if command == "operation.cancel" {
            return self.cancel_operation(&caller, &payload).await;
        }

        let control = OpControl::new(budget, OpTicket::new());
        // Once the authenticated target is admitted, reserve the physical
        // connection queue before starting the asynchronous worker.
        // GATT handle resolution precedes operation tracking. Reject a reset attachment
        // before looking up its invalidated database. Other commands retain the worker
        // check so they can be tracked and cancelled while authority admission waits.
        if matches!(
            command.as_str(),
            "gatt.write"
                | "gatt.write-when-ready"
                | "gatt.read"
                | "gatt.subscribe"
                | "gatt.acquire-write"
                | "gatt.acquire-notifications"
                | "gatt.acquired-write"
                | "gatt.descriptor.read"
                | "gatt.descriptor.write"
                | "gatt.discover"
        ) {
            self.refuse_stale_attachment(&route_attachment, &command)
                .await?;
        }
        let gatt_peer = match command.as_str() {
            "gatt.write"
            | "gatt.write-when-ready"
            | "gatt.read"
            | "gatt.subscribe"
            | "gatt.acquire-write"
            | "gatt.acquire-notifications" => Some(
                self.resolve_after_attachment_check(&route_attachment, &command, || {
                    self.gatt_target(&caller, &payload)
                })
                .await?
                .peer_id,
            ),
            "gatt.acquired-write" => Some(
                self.resolve_after_attachment_check(&route_attachment, &command, || {
                    self.acquired_writer(&caller, &payload)
                })
                .await?
                .peer_id,
            ),
            "gatt.descriptor.read" | "gatt.descriptor.write" => Some(
                self.resolve_after_attachment_check(&route_attachment, &command, || {
                    self.descriptor_target(&caller, &payload, "tauri.gatt-admission")
                })
                .await?
                .0,
            ),
            "gatt.discover" => Some(
                self.resolve_after_attachment_check(&route_attachment, &command, || {
                    self.connection(&caller, &payload, "tauri.gatt-admission")
                })
                .await?
                .peer_id,
            ),
            "gatt.unsubscribe" => {
                let handle =
                    required_string(&payload, "subscriptionHandle", "tauri.gatt-admission")?;
                let state = self.inner.lock().await;
                state
                    .callers
                    .get(&caller_key(&caller))
                    .and_then(|owner| owner.subscriptions.get(&handle))
                    .map(|subscription| subscription.peer_id.clone())
            }
            _ => None,
        };
        let control = if let Some(peer_id) = gatt_peer {
            self.ensure_authority()
                .await?
                .bind_gatt_admission(&peer_id, control)
                .map_err(|error| DispatchError::from_core(&error))?
        } else {
            control
        };
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
                .execute_for_attachment(
                    &operation_caller,
                    &operation_command,
                    &route_attachment,
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
    ) -> Result<Attachment, DispatchError> {
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
        let validated_attachment = {
            let caller_state = state
                .callers
                .get(&caller_key(caller))
                .filter(|caller_state| {
                    !caller_state.retired
                        || matches!(
                            command,
                            "gatt.ordered.status"
                                | "gatt.ordered.next"
                                | "gatt.ordered.close"
                                | "gatt.ordered.forget"
                                | "gatt.ordered.retire"
                        )
                })
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
            caller_state.attachment.clone()
        };
        Ok(validated_attachment)
    }

    /// Refuse work on an attachment an adapter reset replaced (finding 57)
    /// before it reaches the core: `backend.reset`, nothing dispatched.
    /// Releases stay admitted — what the renderer owes for resources the
    /// reset ended still settles, and the core answers what is already
    /// gone.
    async fn refuse_stale_attachment(
        &self,
        route_attachment: &Attachment,
        command: &str,
    ) -> Result<(), DispatchError> {
        if is_release_command(command) {
            return Ok(());
        }
        let current = self.ensure_authority().await?.attachment();
        if current.attachment_id().as_str() != route_attachment.attachment_id {
            return Err(stale_attachment(route_attachment, &current));
        }
        Ok(())
    }

    async fn resolve_after_attachment_check<T, F, Fut>(
        &self,
        route_attachment: &Attachment,
        command: &str,
        resolve: F,
    ) -> Result<T, DispatchError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, DispatchError>>,
    {
        let result = resolve().await;
        self.refuse_stale_attachment(route_attachment, command)
            .await?;
        result
    }

    async fn validate_expected_lease(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<(), DispatchError> {
        self.validate_expected_lease_for_command(caller, payload, "")
            .await
    }

    async fn validate_expected_lease_for_command(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
        command: &str,
    ) -> Result<(), DispatchError> {
        let expected_id = required_string(payload, "__expectedLeaseId", "tauri.execute-lease")?;
        let expected_generation =
            required_string(payload, "__expectedLeaseGeneration", "tauri.execute-lease")?;
        let state = self.inner.lock().await;
        let caller_state = state
            .callers
            .get(&caller_key(caller))
            .filter(|caller_state| {
                !caller_state.retired
                    || matches!(
                        command,
                        "gatt.ordered.status"
                            | "gatt.ordered.next"
                            | "gatt.ordered.close"
                            | "gatt.ordered.forget"
                            | "gatt.ordered.retire"
                    )
            })
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

    #[cfg(test)]
    async fn execute(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        payload: BTreeMap<String, IpcValue>,
        binary_payload: Option<Vec<u8>>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let route_attachment = self
            .bound_attachment(caller, "tauri.route-attachment")
            .await?;
        self.execute_for_attachment(
            caller,
            command,
            &route_attachment,
            payload,
            binary_payload,
            ctl,
        )
        .await
    }

    async fn execute_for_attachment(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        route_attachment: &Attachment,
        payload: BTreeMap<String, IpcValue>,
        binary_payload: Option<Vec<u8>>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        self.validate_expected_lease_for_command(caller, &payload, command)
            .await?;
        self.refuse_stale_attachment(route_attachment, command)
            .await?;
        self.require_ordered_route_protocol(caller, command).await?;
        match command {
            "security.state"
            | "security.pair"
            | "security.cancel-pairing"
            | "security.unpair"
            | "security.watch.subscribe"
            | "security.watch.unsubscribe" => {
                if binary_payload.is_some() {
                    return Err(DispatchError::new(
                        BleErrorCode::ProtocolMalformed,
                        "ipc",
                        command,
                    ));
                }
                self.security_route(caller, command, payload, ctl).await
            }
            "peers.resolve" | "peers.known" | "peers.connected" | "peers.bonded"
            | "peers.authorized" | "peers.restored" => {
                if binary_payload.is_some() {
                    return Err(DispatchError::new(
                        BleErrorCode::ProtocolMalformed,
                        "ipc",
                        command,
                    ));
                }
                self.peer_directory(caller, command, route_attachment, payload, ctl)
                    .await
            }
            "adapter.state" => self.adapter_state(caller, ctl).await,
            command @ ("terminalEvidence.reserve"
            | "terminalEvidence.commit"
            | "terminalEvidence.read"
            | "terminalEvidence.release"
            | "terminalEvidence.recovery.list"
            | "terminalEvidence.recovery.listWithStatus"
            | "terminalEvidence.recovery.read"
            | "terminalEvidence.recovery.ack") => {
                // Route admission adds these private lease facts so ordinary
                // handlers can reject stale work.  The terminal-evidence
                // grammar is a signed, exact public payload; it must not see
                // dispatcher-private keys after the lease was already
                // validated above.
                let mut terminal_payload = payload;
                terminal_payload.remove("__expectedLeaseId");
                terminal_payload.remove("__expectedLeaseGeneration");
                self.terminal_evidence_route(caller, command, &terminal_payload, binary_payload)
                    .await
            }
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
            "connection.request-priority" => self.request_priority(caller, payload, ctl).await,
            "connection.effective-mtu" => self.read_effective_mtu(caller, payload, ctl).await,
            "connection.maximum-write-length" => {
                self.maximum_write_length(caller, payload, ctl).await
            }
            "connection.write-readiness.subscribe" => {
                self.subscribe_write_readiness(caller, payload, ctl).await
            }
            "connection.write-readiness.unsubscribe" => {
                self.unsubscribe_write_readiness(caller, payload).await
            }
            "connection.parameters" => self.read_connection_parameters(caller, payload, ctl).await,
            "connection.phy" => self.read_connection_phy(caller, payload, ctl).await,
            "connection.parameters.subscribe" => {
                self.subscribe_connection_parameters(caller, payload, ctl)
                    .await
            }
            "connection.parameters.unsubscribe" => {
                self.unsubscribe_connection_parameters(caller, payload)
                    .await
            }
            "gatt.discover" => self.discover(caller, payload, ctl).await,
            "gatt.database.release" => self.release_database(caller, payload).await,
            "gatt.read" => self.read(caller, payload, ctl).await,
            "gatt.write" => {
                self.write(caller, payload, binary_payload, ctl, false)
                    .await
            }
            "gatt.write-when-ready" => self.write(caller, payload, binary_payload, ctl, true).await,
            "gatt.acquire-write" => self.acquire_writer(caller, payload, ctl).await,
            "gatt.acquired-write" => {
                self.write_acquired(caller, payload, binary_payload, ctl)
                    .await
            }
            "gatt.acquired-write.close" => self.close_acquired_writer(caller, payload, ctl).await,
            "gatt.acquire-notifications" => self.subscribe(caller, payload, ctl, true).await,
            "gatt.subscribe" => self.subscribe(caller, payload, ctl, false).await,
            "gatt.unsubscribe" => self.unsubscribe(caller, payload, ctl).await,
            "peerFailureRecovery.list" => {
                self.require_peer_failure_recovery_protocol(caller).await?;
                if binary_payload.is_some() {
                    return Err(DispatchError::new(
                        BleErrorCode::ProtocolMalformed,
                        "ipc",
                        command,
                    ));
                }
                self.renderer_peer_failure_recovery_list(caller).await
            }
            "peerFailureRecovery.read" => {
                self.require_peer_failure_recovery_protocol(caller).await?;
                if binary_payload.is_some() {
                    return Err(DispatchError::new(
                        BleErrorCode::ProtocolMalformed,
                        "ipc",
                        command,
                    ));
                }
                self.renderer_peer_failure_recovery_read(caller, &payload)
                    .await
            }
            "peerFailureRecovery.ack" => {
                self.require_peer_failure_recovery_protocol(caller).await?;
                if binary_payload.is_some() {
                    return Err(DispatchError::new(
                        BleErrorCode::ProtocolMalformed,
                        "ipc",
                        command,
                    ));
                }
                self.renderer_peer_failure_recovery_acknowledge(caller, &payload)
                    .await
            }
            command if command.starts_with("scanFailureRecovery.") => {
                if binary_payload.is_some() {
                    return Err(DispatchError::new(
                        BleErrorCode::ProtocolMalformed,
                        "ipc",
                        command,
                    ));
                }
                self.renderer_scan_failure_recovery(caller, command, &payload)
                    .await
            }
            "gatt.ordered.orphans" => self.ordered_orphan_receipts(caller).await,
            "gatt.ordered.orphan-page" => self.ordered_orphan_page(caller, payload).await,
            "gatt.ordered.orphan-ack" => self.ack_ordered_orphan(caller, payload).await,
            "gatt.ordered.open" => self.open_ordered_group(caller, payload, ctl).await,
            "gatt.ordered.status" => self.ordered_open_status(caller, payload).await,
            "gatt.ordered.next" => self.next_ordered_group(caller, payload).await,
            "gatt.ordered.close" => self.close_ordered_group(caller, payload, ctl).await,
            "gatt.ordered.forget" => self.forget_ordered_group(caller, payload).await,
            "gatt.ordered.retire" => self.retire_ordered_group(caller, payload).await,
            "gatt.ordered.marker.reserve"
            | "gatt.ordered.marker.start"
            | "gatt.ordered.marker.status"
            | "gatt.ordered.marker.cancel"
            | "gatt.ordered.marker.forget"
            | "gatt.ordered-recording.markers"
            | "gatt.ordered-recording.marker.reserve"
            | "gatt.ordered-recording.marker.start"
            | "gatt.ordered-recording.marker.status"
            | "gatt.ordered-recording.marker.cancel"
            | "gatt.ordered-recording.marker.forget" => {
                self.ordered_marker_route(caller, command, payload, binary_payload, ctl)
                    .await
            }
            "gatt.ordered-recording.reserve" => {
                self.reserve_ordered_recording_start(caller, payload).await
            }
            "gatt.ordered-recording.starts" => {
                self.recover_ordered_recording_starts(caller, payload).await
            }
            "gatt.ordered-recording.recovery"
            | "gatt.ordered-recording.reclaim.pending"
            | "gatt.ordered-recording.reclaim.reserve"
            | "gatt.ordered-recording.reclaim.start"
            | "gatt.ordered-recording.reclaim.status"
            | "gatt.ordered-recording.reclaim.commit"
            | "gatt.ordered-recording.reclaim.cancel"
            | "gatt.ordered-recording.reclaim.release"
            | "gatt.ordered-recording.reclaim.retire"
            | "gatt.ordered-recording.reclaim.ack-retirement"
            | "gatt.ordered-recording.reclaim.forget-retirement"
            | "gatt.ordered-recording.reclaim.confirm-forgotten"
            | "gatt.ordered-recording.marker-owner" => {
                self.ordered_reclaim_route(caller, command, payload).await
            }
            "gatt.ordered-recording.start" => {
                self.start_ordered_recording(caller, payload, ctl).await
            }
            "gatt.ordered-recording.status" => {
                self.ordered_recording_start_status(caller, payload).await
            }
            "gatt.ordered-recording.retire" => {
                self.retire_ordered_recording_start(caller, payload).await
            }
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
            release: Arc::new(Mutex::new(())),
        });
        self.spawn_orphan_retries(Arc::clone(authority), id);
    }

    /// Retry one orphan's release automatically: 100 ms after the failed
    /// compensation, doubling to 5 s, until a release lands or
    /// [`ORPHAN_RELEASE_ATTEMPTS`] attempts were refused (then it is marked
    /// exhausted and stays owed). A round joins the resource's release
    /// admission; a debt that is gone for good ends the schedule. Runs on the ambient runtime, so it follows the
    /// caller's clock.
    fn spawn_orphan_retries(&self, authority: Arc<dyn CoreAuthority>, id: u64) {
        let dispatcher = self.clone();
        tokio::spawn(async move {
            let mut delay = ORPHAN_RETRY_FIRST_DELAY;
            for _ in 1..ORPHAN_RELEASE_ATTEMPTS {
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay.saturating_mul(2), ORPHAN_RETRY_MAX_DELAY);
                if !dispatcher.authority_open_for_retry().await {
                    return;
                }
                let debt = {
                    let state = dispatcher.inner.lock().await;
                    let Some(debt) = state.orphan_debt.iter().find(|debt| debt.id == id) else {
                        return;
                    };
                    debt.clone()
                };
                let _release = debt.release.lock().await;
                if !dispatcher.authority_open_for_retry().await {
                    return;
                }
                if !dispatcher
                    .inner
                    .lock()
                    .await
                    .orphan_debt
                    .iter()
                    .any(|entry| entry.id == id)
                {
                    return;
                }
                let result = release_orphan(&authority, &debt.resource).await;
                let mut state = dispatcher.inner.lock().await;
                if result.is_ok() {
                    state.orphan_debt.retain(|entry| entry.id != id);
                    return;
                }
                let Some(current) = state.orphan_debt.iter_mut().find(|entry| entry.id == id)
                else {
                    return;
                };
                current.attempts = current.attempts.saturating_add(1);
                current.exhausted = current.attempts >= ORPHAN_RELEASE_ATTEMPTS;
                if current.exhausted {
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
            self.inner
                .lock()
                .await
                .orphan_debt
                .iter()
                .filter(|debt| caller_key.is_none_or(|key| debt.caller_key == key))
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut failures = Vec::new();
        for debt in owed {
            let _release = debt.release.lock().await;
            let current = self
                .inner
                .lock()
                .await
                .orphan_debt
                .iter()
                .find(|entry| entry.id == debt.id)
                .cloned();
            let Some(current) = current else {
                continue;
            };
            if let Err(error) = release_orphan(authority, &debt.resource).await {
                failures.push(OrphanFailure {
                    resource: debt.resource.clone(),
                    error,
                    exhausted: current.exhausted,
                    attempts: current.attempts,
                });
            } else {
                self.inner
                    .lock()
                    .await
                    .orphan_debt
                    .retain(|entry| entry.id != debt.id);
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
        let windows = decode_windows_scan_options(payload.get("platform"))?;
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
        let attachment = self.ensure_adapter().await?;
        let key = caller_key(caller);
        let lease = expected_lease(&payload, "tauri.scan-lease")?;
        let terminal_evidence = self
            .terminal_evidence_binding(caller, &payload, "scan", "tauri.scan.terminal-evidence")
            .await?;
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
        let native_capabilities = authority
            .capability_descriptors()
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let addresses_visible = native_capabilities.iter().any(|capability| {
            capability.id() == "peer:address-targeting"
                && matches!(
                    capability.state(),
                    ubm_core::central::CapabilityState::Supported
                        | ubm_core::central::CapabilityState::Limited
                )
        });
        let diagnostic_plan = diagnostic_scan_plan(&decoded_query, addresses_visible);
        let service_uuid_strings: Vec<String> =
            service_uuids.iter().map(|uuid| uuid.to_string()).collect();
        let scan_id = authority
            .start_scan_platform(&key, &service_uuid_strings, windows, ctl)
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
                        terminal_evidence,
                        terminal_stage_failure: None,
                        terminal_delivery_failure: None,
                        terminal_delivery_retry_item: None,
                        terminal_failure_claimed: false,
                        terminal_failure_sent: false,
                        terminal_delivery_retry: None,
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
            let mut address_types = HashMap::new();
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
                        let observation = match typed_core_scan_observation(
                            authority.as_ref(),
                            &snapshot,
                            &mut address_types,
                        )
                        .await
                        {
                            Ok(observation) => observation,
                            Err(error) => {
                                if let Err(delivery_error) = forwarder
                                    .terminal(
                                        &key,
                                        (&lease.0, &lease.1),
                                        &handle,
                                        "source-failed",
                                        Some(&error),
                                    )
                                    .await
                                {
                                    eprintln!(
                                        "Tauri scan address-type failure terminal delivery failed: {delivery_error:?}; original failure: {error:?}"
                                    );
                                }
                                return;
                            }
                        };
                        // A supplemental native read must not publish into a retired scan.
                        loop {
                            match forwarder.scan_delivery(&key, &handle).await {
                                Delivery::Gone => return,
                                Delivery::Paused => tokio::time::sleep(FORWARD_POLL_INTERVAL).await,
                                Delivery::Active => break,
                            }
                        }
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
            Some(scan)
                if scan.handle == handle
                    && scan.phase.is_active()
                    && !scan.terminal_failure_sent
                    && !scan.terminal_failure_claimed =>
            {
                Delivery::Active
            }
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
        // A source-ended scan can still owe its authenticated stage-failure
        // terminal. Do not let the core's already-stopped verdict erase that
        // delivery debt: join/retry it before removing the mapping.
        let owns_handle = self
            .inner
            .lock()
            .await
            .callers
            .get(key)
            .and_then(|caller| caller.scan.as_ref())
            .is_some_and(|scan| scan.handle == handle);
        if !owns_handle {
            // An unknown/stale handle is already released and must not make
            // progress on an unrelated scan's retained terminal debt.
            return Ok(());
        }
        self.retry_scan_terminal_delivery_failure(key).await?;
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
        match payload.get("intent") {
            None => {}
            Some(IpcValue::String(intent)) if intent == "direct" => {}
            Some(IpcValue::String(intent)) if intent == "when-available" => {}
            _ => {
                return Err(DispatchError::new(
                    BleErrorCode::ArgumentInvalid,
                    "connection",
                    "tauri.connect-intent",
                ));
            }
        }
        if let Some(value) = payload.get("transport") {
            if !matches!(value, IpcValue::String(value) if value == "le" || value == "auto") {
                return Err(DispatchError::new(
                    BleErrorCode::ArgumentInvalid,
                    "connection",
                    "tauri.connect-transport",
                ));
            }
        }
        if payload
            .get("preferredPhy")
            .is_some_and(|value| !matches!(value, IpcValue::Array(values) if values.is_empty()))
        {
            return Err(DispatchError::new(
                BleErrorCode::CapabilityUnsupported,
                "capability",
                "connection.connect.phy",
            )
            .platform("The instantiated desktop authority has no connect PHY implementation"));
        }
        let authority = self.ensure_authority().await?;
        if payload.get("intent") == Some(&string("when-available")) {
            let states = authority
                .capability_descriptors()
                .await
                .map_err(|error| DispatchError::from_core(&error))?;
            match states.iter().find(|row| row.id() == "connection:when-available").map(|row| row.state()) {
                Some(ubm_core::central::CapabilityState::Supported | ubm_core::central::CapabilityState::Limited) => {},
                Some(ubm_core::central::CapabilityState::Unavailable) => return Err(DispatchError::new(BleErrorCode::CapabilityUnavailable, "connection", "connection.connect.when-available")),
                _ => return Err(DispatchError::new(BleErrorCode::CapabilityUnsupported, "connection", "connection.connect.when-available").platform("The instantiated native authority has no deferred initial-acquisition mechanism")),
            }
        }
        let peer_id = match (payload.get("peerId"), payload.get("address")) {
            (Some(IpcValue::String(peer)), None) if !peer.is_empty() => peer.clone(),
            (None, Some(address)) => {
                let address = into_object(address.clone(), "tauri.connect-address")?;
                if address
                    .keys()
                    .any(|key| !["address", "addressType"].contains(&key.as_str()))
                {
                    return Err(DispatchError::new(
                        BleErrorCode::ProtocolMalformed,
                        "ipc",
                        "tauri.connect-address",
                    ));
                }
                let target = required_string(&address, "address", "tauri.connect-address")?;
                let kind = match required_string(&address, "addressType", "tauri.connect-address")?
                    .as_str()
                {
                    "public" => ubm_desktop::AddressType::Public,
                    "random" => ubm_desktop::AddressType::Random,
                    _ => {
                        return Err(DispatchError::new(
                            BleErrorCode::ArgumentInvalid,
                            "connection",
                            "tauri.connect-address-type",
                        ));
                    }
                };
                // A core ticket names one operation, not the whole two-stage
                // acquisition. Preserve the original absolute budget and
                // bridge cancellation to the resolution's child ticket; the
                // parent ticket remains available for connection admission.
                let resolution_control = OpControl::new(ctl.budget, OpTicket::new());
                let resolution =
                    authority.resolve_address(&target, kind, resolution_control.clone());
                tokio::pin!(resolution);
                let resolved = tokio::select! {
                    answer = &mut resolution => answer,
                    _ = ctl.ticket.cancelled() => {
                        resolution_control.ticket.request_cancel();
                        resolution.await
                    }
                };
                resolved.map_err(|error| DispatchError::from_core(&error))?
            }
            _ => {
                return Err(DispatchError::new(
                    BleErrorCode::ProtocolMalformed,
                    "ipc",
                    "tauri.connect-target",
                ));
            }
        };
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
        let connection = if payload.get("intent") == Some(&string("when-available")) {
            authority
                .connect_when_available(&peer_id, &lease, ctl)
                .await
        } else {
            authority.connect(&peer_id, &lease, ctl).await
        }
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
                                native_release: None,
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
            self.invalidate_reclaims(key, None, Some(handle));
            if self.has_reclaim_debt(key, None, Some(handle)) {
                return Err(DispatchError::new(
                    BleErrorCode::LifecycleInvalidState,
                    "cleanup",
                    "tauri.reclaim.pending",
                ));
            }
            (
                begin_release(&mut connection.phase),
                connection.peer_id.clone(),
                connection.lease.clone(),
                connection.native_release,
            )
        };
        let (sender, peer_id, lease, native_release) = match step {
            (ReleaseStep::Join(receiver), _, _, _) => return join_release(receiver).await,
            (ReleaseStep::Lead(sender), peer_id, lease, native_release) => {
                (sender, peer_id, lease, native_release)
            }
        };
        let released = match native_release {
            Some(physical_ended) => Ok(physical_ended),
            None => match self.ensure_authority().await {
                Ok(authority) => match authority.disconnect(&peer_id, &lease, ctl).await {
                    Ok(physical_ended) => Ok(physical_ended),
                    Err(error) => {
                        let error = DispatchError::from_core(&error);
                        if is_released_link(&error) {
                            Ok(true)
                        } else {
                            Err(error)
                        }
                    }
                },
                Err(error) => Err(error),
            },
        };
        if let Ok(physical_ended) = released.as_ref() {
            let mut state = self.inner.lock().await;
            if let Some(connection) = state
                .callers
                .get_mut(key)
                .and_then(|caller| caller.connections.get_mut(handle))
                .filter(|connection| connection.lease == lease)
            {
                connection.native_release = Some(*physical_ended);
            }
        }
        let shared_link_retained = matches!(released, Ok(false));
        let mut result = released.map(|_| ());
        // A native release can invalidate an already-running notification
        // poll before it answers. Once it does answer, send each remaining
        // owner terminal before forgetting its mapping. A failed channel
        // send keeps that mapping and the confirmed native receipt so only
        // local cleanup retries; an earlier terminal is never sent again.
        // The snapshot cannot race a new
        // notification terminal: `begin_release` already set `Releasing`
        // under the same lock that guards its channel send and sent marker.
        if result.is_ok() {
            let targets = {
                let state = self.inner.lock().await;
                state.callers.get(key).and_then(|caller_state| {
                    (!caller_state.retired).then(|| {
                        (
                            caller_state.lease_id.clone(),
                            caller_state.lease_generation.clone(),
                            caller_state
                                .subscriptions
                                .iter()
                                .filter_map(|(subscription_handle, subscription)| {
                                    (subscription.connection_handle == handle
                                        && !subscription.terminal_sent)
                                        .then_some(subscription_handle.clone())
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                })
            };
            if let Some((owner_lease_id, owner_lease_generation, handles)) = targets {
                for subscription_handle in handles {
                    match self
                        .terminal(
                            key,
                            (&owner_lease_id, &owner_lease_generation),
                            &subscription_handle,
                            "owner-released",
                            None,
                        )
                        .await
                    {
                        Ok(()) => {
                            let mut state = self.inner.lock().await;
                            if let Some(subscription) =
                                state.callers.get_mut(key).and_then(|caller| {
                                    caller.subscriptions.get_mut(&subscription_handle)
                                })
                            {
                                subscription.terminal_sent = true;
                            }
                        }
                        Err(error) => {
                            result = Err(error);
                            break;
                        }
                    }
                }
            }
        }
        // A shared lease release need not produce a physical link event.
        // End only this owner's still-open streams, without inventing one.
        // A core-event delivery that already claimed a stream keeps ownership.
        if result.is_ok() && shared_link_retained {
            let targets = {
                let state = self.inner.lock().await;
                state
                    .callers
                    .get(key)
                    .filter(|caller| !caller.retired)
                    .map(|caller| {
                        (
                            caller.lease_id.clone(),
                            caller.lease_generation.clone(),
                            caller
                                .connection_events
                                .values()
                                .filter(|stream| stream.connection_handle == handle)
                                .map(|stream| stream.stream_handle.clone())
                                .collect::<Vec<_>>(),
                        )
                    })
            };
            if let Some((owner, generation, streams)) = targets {
                for stream in streams {
                    if let Err(error) = self
                        .terminal_with_admission(
                            key,
                            TerminalRequest {
                                expected_lease: (&owner, &generation),
                                stream_id: &stream,
                                reason: "owner-released",
                                error: None,
                                counters: terminal_evidence::TerminalCounters::ZERO,
                                admission: TerminalAdmission::OwnerConnection,
                            },
                        )
                        .await
                    {
                        result = Err(error);
                        break;
                    }
                }
            }
        }
        let detached = {
            let mut state = self.inner.lock().await;
            match state.callers.get_mut(key) {
                Some(caller_state) if caller_state.connections.contains_key(handle) => {
                    if result.is_ok() {
                        self.invalidate_reclaims(key, None, Some(handle));
                        caller_state.connections.remove(handle);
                        Self::detach_connection_mappings(caller_state, handle)
                    } else {
                        if let Some(connection) = caller_state.connections.get_mut(handle) {
                            connection.phase = ReleasePhase::ReleaseFailed;
                        }
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            }
        };
        for (_, subscription) in detached {
            if let Some(task) = subscription.task {
                task.abort();
            }
        }
        let _ = sender.send(Some(result.clone()));
        result
    }

    /// Drop the GATT mappings owned by one released connection handle and
    /// return the detached subscriptions for the caller to stop. The core
    /// released this lease's consumers, not another owner's shared link.
    /// A connection-event delivery already claimed by the core keeps ownership.
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
        let writer_handles = caller_state
            .acquired_writers
            .iter()
            .filter_map(|(handle, writer)| {
                (writer.connection_handle == connection_handle).then_some(handle.clone())
            })
            .collect::<Vec<_>>();
        for handle in writer_handles {
            caller_state.acquired_writers.remove(&handle);
            caller_state.acquired_releases.insert(handle);
        }
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
                platform: None,
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
        let mut source_uncertainty = authority.source_uncertainty_events();
        let mut scan_ends = authority.scan_terminal_events();
        let mut resets = authority.adapter_reset_events();
        let dispatcher = self.clone();
        let pump = tauri::async_runtime::spawn(async move {
            let (
                mut lifecycle_open,
                mut source_uncertainty_open,
                mut scan_ends_open,
                mut resets_open,
            ) = (true, true, true, true);
            while lifecycle_open || source_uncertainty_open || scan_ends_open || resets_open {
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
                    event = source_uncertainty.recv(), if source_uncertainty_open => match event {
                        Ok(event) => dispatcher.emit_source_uncertainty(event).await,
                        Err(broadcast::error::RecvError::Lagged(missed)) => {
                            dispatcher.emit_source_uncertainty_lagged(missed).await;
                        }
                        Err(broadcast::error::RecvError::Closed) => source_uncertainty_open = false,
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
                // The attachment-rebind event terminalizes renderer streams;
                // old-generation native security receivers must retire too.
                caller_state.security_watch_releases.extend(
                    caller_state
                        .security_watches
                        .drain()
                        .map(|(handle, _watch)| handle),
                );
                caller_state.write_readiness_releases.extend(
                    caller_state
                        .write_readiness_watches
                        .drain()
                        .map(|(handle, _watch)| handle),
                );
                caller_state.parameter_releases.extend(
                    caller_state
                        .parameter_watches
                        .drain()
                        .map(|(handle, _watch)| handle),
                );
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

    /// Forward a graph-less native callback on the attachment control stream.
    /// It is intentionally not routed through a connection stream: the UUID
    /// may belong to an older, already-retired native graph.
    async fn emit_source_uncertainty(&self, event: SourceUncertaintyEvent) {
        let error = DispatchError::from_core(&event.error);
        let value = object([
            ("kind", string("diagnostic-warning")),
            ("schemaVersion", number(1)),
            ("code", string("gatt.source-uncertain")),
            ("sequence", IpcValue::Number(Number::from(event.sequence))),
            ("peerId", string(event.peer_id)),
            ("error", error.normalized_error()),
        ]);
        self.emit_attachment_diagnostic(value).await;
    }

    /// A broadcast gap is itself a diagnostic fact.  The renderer cannot
    /// safely reconstruct the omitted ownerless callbacks, so it receives a
    /// typed gap rather than an invented connection transition.
    async fn emit_source_uncertainty_lagged(&self, missed: u64) {
        self.emit_attachment_diagnostic(object([
            ("kind", string("diagnostic-warning")),
            ("schemaVersion", number(1)),
            ("code", string("gatt.source-uncertain-lagged")),
            ("missed", IpcValue::Number(Number::from(missed))),
        ]))
        .await;
    }

    async fn emit_attachment_diagnostic(&self, value: IpcValue) {
        let callers = {
            let mut state = self.inner.lock().await;
            if state.retained_attachment_diagnostics.len() == MAX_RETAINED_ATTACHMENT_DIAGNOSTICS {
                state.retained_attachment_diagnostics.pop_front();
                state.dropped_attachment_diagnostics =
                    state.dropped_attachment_diagnostics.saturating_add(1);
            }
            state
                .retained_attachment_diagnostics
                .push_back(value.clone());
            state
                .callers
                .iter()
                .filter(|&(_key, caller)| attachment_diagnostics_supported(&caller.versions))
                .map(|(key, _caller)| key.clone())
                .collect::<Vec<_>>()
        };
        for caller in callers {
            // The host ring above is the retryable evidence path. A channel
            // failure is not an excuse to discard the source fact.
            let _ = self
                .emit(&caller, None, IPC_ATTACHMENT_STREAM_ID, value.clone())
                .await;
        }
    }

    async fn replay_attachment_diagnostics(&self, caller: &str) {
        let (dropped, values) = {
            let state = self.inner.lock().await;
            if !state
                .callers
                .get(caller)
                .is_some_and(|state| attachment_diagnostics_supported(&state.versions))
            {
                return;
            }
            (
                state.dropped_attachment_diagnostics,
                state
                    .retained_attachment_diagnostics
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        if dropped != 0 {
            let _ = self
                .emit(
                    caller,
                    None,
                    IPC_ATTACHMENT_STREAM_ID,
                    object([
                        ("kind", string("diagnostic-warning")),
                        ("schemaVersion", number(1)),
                        ("code", string("gatt.source-uncertain-retention-lagged")),
                        ("missed", IpcValue::Number(Number::from(dropped))),
                    ]),
                )
                .await;
        }
        for value in values {
            if self
                .emit(caller, None, IPC_ATTACHMENT_STREAM_ID, value)
                .await
                .is_err()
            {
                // The record remains in the host ring for the caller's next
                // authenticated attachment. Do not send it to another caller.
                return;
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
    /// and released its owner. The mapping remains until its terminal
    /// evidence has either been durably committed and delivered, or a caller
    /// receives the exact staging failure; otherwise a stage failure would
    /// silently discard the reservation needed for recovery.
    /// `source-failed` with the core's own words when the scan was aborted,
    /// `closed` otherwise (the desktop NAPI provider's vocabulary). A scan
    /// whose release is in flight is left to that release.
    async fn apply_scan_terminal(&self, ended: &ScanTerminalEvent) {
        let target = {
            let mut state = self.inner.lock().await;
            state.callers.iter_mut().find_map(|(key, caller_state)| {
                let owned = caller_state.scan.as_ref().is_some_and(|scan| {
                    scan.core_operation_id == ended.operation_id
                        && scan.phase.is_active()
                        && !scan.terminal_failure_sent
                        && !scan.terminal_failure_claimed
                });
                if !owned {
                    return None;
                }
                caller_state
                    .scan
                    .as_mut()
                    .expect("owned scan exists")
                    .terminal_failure_claimed = true;
                Some((
                    key.clone(),
                    (
                        caller_state.lease_id.clone(),
                        caller_state.lease_generation.clone(),
                    ),
                    caller_state.scan.as_ref()?.handle.clone(),
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
        match self
            .terminal_with_admission(
                &key,
                TerminalRequest {
                    expected_lease: (&lease.0, &lease.1),
                    stream_id: &handle,
                    reason,
                    error: error.as_ref(),
                    counters: terminal_evidence::TerminalCounters::ZERO,
                    admission: TerminalAdmission::ClaimedScan,
                },
            )
            .await
        {
            Ok(_) => {
                let mut state = self.inner.lock().await;
                if let Some(scan) = state
                    .callers
                    .get_mut(&key)
                    .filter(|caller| {
                        caller.lease_id == lease.0 && caller.lease_generation == lease.1
                    })
                    .and_then(|caller| caller.scan.as_ref())
                    .filter(|scan| {
                        scan.handle == handle && scan.core_operation_id == ended.operation_id
                    })
                {
                    if let Some(task) = scan.task.as_ref() {
                        task.abort();
                    }
                }
                if let Some(caller) = state.callers.get_mut(&key) {
                    if caller.scan.as_ref().is_some_and(|scan| {
                        scan.handle == handle && scan.core_operation_id == ended.operation_id
                    }) {
                        caller.scan.take();
                    }
                }
            }
            Err(stage_failure) => {
                // The core has already ended the physical scan. A durable
                // stage/commit failure has a narrowly authenticated public
                // representation; retain the resource/binding so a later
                // owner cleanup can retry the exact reservation instead of
                // forging a receipt or losing recovery authority. Transport
                // delivery failures are not evidence-stage failures and must
                // not be branded as one.
                if matches!(
                    stage_failure.operation.as_str(),
                    "tauri.terminal-evidence.stage-before-emit"
                        | "tauri.terminal-evidence.commit-before-emit"
                ) {
                    match self
                        .emit_scan_terminal_stage_failure(
                            &key,
                            (&lease.0, &lease.1),
                            &handle,
                            &stage_failure,
                        )
                        .await
                    {
                        Ok(()) => {
                            let mut state = self.inner.lock().await;
                            if let Some(scan) = state
                                .callers
                                .get_mut(&key)
                                .filter(|caller| {
                                    caller.lease_id == lease.0 && caller.lease_generation == lease.1
                                })
                                .and_then(|caller| caller.scan.as_mut())
                                .filter(|scan| {
                                    scan.handle == handle
                                        && scan.core_operation_id == ended.operation_id
                                })
                            {
                                scan.terminal_failure_sent = true;
                                if let Some(task) = scan.task.as_ref() {
                                    task.abort();
                                }
                            }
                        }
                        Err(delivery_failure) => {
                            let mut state = self.inner.lock().await;
                            if let Some(scan) = state
                                .callers
                                .get_mut(&key)
                                .filter(|caller| {
                                    caller.lease_id == lease.0 && caller.lease_generation == lease.1
                                })
                                .and_then(|caller| caller.scan.as_mut())
                                .filter(|scan| {
                                    scan.handle == handle
                                        && scan.core_operation_id == ended.operation_id
                                })
                            {
                                scan.terminal_stage_failure = Some(stage_failure);
                                scan.terminal_delivery_failure = Some(delivery_failure);
                                scan.terminal_delivery_retry_item =
                                    Some(ScanTerminalRetry::StageFailure(
                                        scan.terminal_stage_failure
                                            .as_ref()
                                            .expect("stored stage failure")
                                            .clone(),
                                    ));
                            }
                        }
                    }
                } else {
                    // A terminal-send failure happens after the terminal has
                    // been assembled (and may have committed evidence). The
                    // core will not publish another scan-end event, so retain
                    // that exact terminal for the same retry path rather
                    // than leaving the lifecycle claim to suppress all later
                    // cleanup or substituting an owner-release terminal.
                    let mut state = self.inner.lock().await;
                    if let Some(scan) = state
                        .callers
                        .get_mut(&key)
                        .filter(|caller| {
                            caller.lease_id == lease.0 && caller.lease_generation == lease.1
                        })
                        .and_then(|caller| caller.scan.as_mut())
                        .filter(|scan| {
                            scan.handle == handle && scan.core_operation_id == ended.operation_id
                        })
                    {
                        scan.terminal_delivery_failure = Some(stage_failure);
                        scan.terminal_delivery_retry_item = Some(ScanTerminalRetry::Terminal {
                            reason: reason.to_owned(),
                            error: error.clone(),
                        });
                    }
                }
            }
        }
    }

    /// Retry the exact native stage-failure terminal before ordinary caller
    /// cleanup. A full queue or closed channel is observable cleanup debt,
    /// never a reason to discard a committed-or-recoverable binding.
    async fn retry_scan_terminal_delivery_failure(&self, key: &str) -> Result<(), DispatchError> {
        enum RetryStep {
            Join(watch::Receiver<ReleaseAnswer>),
            Lead {
                sender: watch::Sender<ReleaseAnswer>,
                lease: (String, String),
                handle: String,
                operation_id: OperationId,
                item: ScanTerminalRetry,
            },
        }
        let step = {
            let mut state = self.inner.lock().await;
            state.callers.get_mut(key).and_then(|caller| {
                caller.scan.as_mut().and_then(|scan| {
                    if let Some(retry) = scan.terminal_delivery_retry.as_ref() {
                        return Some(RetryStep::Join(retry.clone()));
                    }
                    scan.terminal_delivery_retry_item.clone().map(|item| {
                        let (sender, receiver) = watch::channel(None);
                        scan.terminal_delivery_retry = Some(receiver);
                        RetryStep::Lead {
                            sender,
                            lease: (caller.lease_id.clone(), caller.lease_generation.clone()),
                            handle: scan.handle.clone(),
                            operation_id: scan.core_operation_id.clone(),
                            item,
                        }
                    })
                })
            })
        };
        let Some(step) = step else {
            return Ok(());
        };
        let RetryStep::Lead {
            sender,
            lease,
            handle,
            operation_id,
            item,
        } = step
        else {
            let RetryStep::Join(receiver) = step else {
                unreachable!("retry step is lead or join")
            };
            let result = join_release(receiver).await;
            if result.is_ok() {
                // The leading retry owns the actual send, but a joined
                // release must treat its successful answer as terminal too:
                // it may run only after the sender has cleared its in-flight
                // marker, so this closes the last owner-release race.
                if let Some(scan) = self
                    .inner
                    .lock()
                    .await
                    .callers
                    .get_mut(key)
                    .and_then(|caller| caller.scan.as_mut())
                {
                    scan.terminal_stage_failure = None;
                    scan.terminal_delivery_failure = None;
                    scan.terminal_delivery_retry_item = None;
                    scan.terminal_delivery_retry = None;
                    scan.terminal_failure_sent = true;
                    if let Some(task) = scan.task.as_ref() {
                        task.abort();
                    }
                }
            }
            return result;
        };
        let result = match &item {
            ScanTerminalRetry::StageFailure(stage_failure) => {
                self.emit_scan_terminal_stage_failure(
                    key,
                    (&lease.0, &lease.1),
                    &handle,
                    stage_failure,
                )
                .await
            }
            ScanTerminalRetry::Terminal { reason, error } => self
                .terminal_with_admission(
                    key,
                    TerminalRequest {
                        expected_lease: (&lease.0, &lease.1),
                        stream_id: &handle,
                        reason,
                        error: error.as_ref(),
                        counters: terminal_evidence::TerminalCounters::ZERO,
                        admission: TerminalAdmission::ClaimedScan,
                    },
                )
                .await
                .map(|_| ()),
        };
        match result {
            Ok(()) => {
                let mut state = self.inner.lock().await;
                if let Some(scan) = state
                    .callers
                    .get_mut(key)
                    .filter(|caller| {
                        caller.lease_id == lease.0 && caller.lease_generation == lease.1
                    })
                    .and_then(|caller| caller.scan.as_mut())
                    .filter(|scan| scan.handle == handle && scan.core_operation_id == operation_id)
                {
                    scan.terminal_stage_failure = None;
                    scan.terminal_delivery_failure = None;
                    scan.terminal_delivery_retry_item = None;
                    scan.terminal_delivery_retry = None;
                    scan.terminal_failure_sent = true;
                    if let Some(task) = scan.task.as_ref() {
                        task.abort();
                    }
                }
                let _ = sender.send(Some(Ok(())));
                Ok(())
            }
            Err(delivery_failure) => {
                let mut state = self.inner.lock().await;
                if let Some(scan) = state
                    .callers
                    .get_mut(key)
                    .filter(|caller| {
                        caller.lease_id == lease.0 && caller.lease_generation == lease.1
                    })
                    .and_then(|caller| caller.scan.as_mut())
                    .filter(|scan| scan.handle == handle && scan.core_operation_id == operation_id)
                {
                    scan.terminal_delivery_failure = Some(delivery_failure.clone());
                    scan.terminal_delivery_retry = None;
                }
                let _ = sender.send(Some(Err(delivery_failure.clone())));
                Err(delivery_failure)
            }
        }
    }

    async fn emit_scan_terminal_stage_failure(
        &self,
        caller_key: &str,
        expected_lease: (&str, &str),
        stream_id: &str,
        failure: &DispatchError,
    ) -> Result<(), DispatchError> {
        let mut state = self.inner.lock().await;
        let caller = state
            .callers
            .get_mut(caller_key)
            .filter(|caller| {
                !caller.retired
                    && caller.lease_id == expected_lease.0
                    && caller.lease_generation == expected_lease.1
            })
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "stream",
                    "tauri.scan-terminal-owner",
                )
            })?;
        if !caller
            .scan
            .as_ref()
            .is_some_and(|scan| scan.handle == stream_id && scan.phase.is_active())
        {
            return Ok(());
        }
        if caller.pending_events.len() >= MAX_PENDING_EVENTS {
            return Err(DispatchError::new(
                BleErrorCode::StreamQuota,
                "stream",
                "tauri.scan-terminal-stage-event",
            ));
        }
        let event_id = self.id("event-terminal-stage-failure");
        caller.pending_events.insert(event_id.clone());
        let result = caller.event_sink.send(object([
            (
                "rendererLease",
                object([
                    ("leaseId", string(caller.lease_id.clone())),
                    ("generation", string(caller.lease_generation.clone())),
                ]),
            ),
            ("eventId", string(event_id.clone())),
            ("streamId", string(stream_id)),
            (
                "item",
                object([
                    ("kind", string("terminal")),
                    ("reason", string("source-failed")),
                    ("droppedItems", number(0)),
                    ("droppedBytes", number(0)),
                    ("replacedItems", number(0)),
                    ("error", failure.normalized_error()),
                    // The authenticated Tauri transport brands this exact
                    // native-only marker before the public IPC manager accepts
                    // a scoped no-receipt terminal as a stage failure.
                    ("terminalEvidenceStageFailed", IpcValue::Bool(true)),
                ]),
            ),
        ]));
        if result.is_err() {
            caller.pending_events.remove(&event_id);
        }
        result.map_err(|error| {
            DispatchError::new(
                BleErrorCode::PlatformTransport,
                "stream",
                "tauri.scan-terminal-stage-send",
            )
            .platform(error.to_string())
        })
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
            LifecycleKind::HostReset => Some(LINK_HOST_RESET),
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
                for handle in &connection_handles {
                    self.invalidate_reclaims(caller_key, None, Some(handle));
                }
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
                    resource.platform = event.platform.clone();
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
        let key = caller_key(caller);
        let old_databases = self
            .inner
            .lock()
            .await
            .callers
            .get(&key)
            .map(|owner| {
                owner
                    .databases
                    .iter()
                    .filter_map(|(handle, database)| {
                        (database.connection_handle == connection_handle).then_some(handle.clone())
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for database in old_databases {
            self.release_acquired_database(&key, &database, "service-changed")
                .await?;
        }
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
                service_records.push(ipc_service_record(
                    &path.service_uuid,
                    path.service_occurrence,
                    service_level_access(&paths, &path.service_uuid, path.service_occurrence),
                    paths.iter().find(|service| {
                        service.characteristic_uuid.is_none()
                            && service.service_uuid == path.service_uuid
                            && service.service_occurrence == path.service_occurrence
                    }),
                ));
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
                    self.invalidate_reclaims(&key, None, Some(&connection_handle));
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
        // Acquired children must settle before their retry identity is removed.
        let handle = required_string(&payload, "databaseHandle", "tauri.database-release")?;
        let key = caller_key(caller);
        {
            let state = self.inner.lock().await;
            if !state.callers.contains_key(&key) {
                return Err(DispatchError::new(
                    BleErrorCode::OwnershipDenied,
                    "gatt",
                    "tauri.database-release-owner",
                ));
            }
            self.invalidate_reclaims(&key, Some(&handle), None);
        }
        if self.has_reclaim_debt(&key, Some(&handle), None) {
            return Err(DispatchError::new(
                BleErrorCode::LifecycleInvalidState,
                "cleanup",
                "tauri.reclaim.pending",
            ));
        }
        self.release_acquired_database(&key, &handle, "owner-released")
            .await?;
        let mut state = self.inner.lock().await;
        let caller_state = state.callers.get_mut(&key).ok_or_else(|| {
            DispatchError::new(
                BleErrorCode::OwnershipDenied,
                "gatt",
                "tauri.database-release-owner",
            )
        })?;
        self.invalidate_reclaims(&key, Some(&handle), None);
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
            .read(
                &target.peer_id,
                &target.characteristic.selector,
                ctl.with_connection_lease(target.lease),
            )
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
        wait_ready: bool,
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
        if wait_ready && mode != "without-response" {
            return Err(DispatchError::new(
                BleErrorCode::ArgumentInvalid,
                "gatt",
                "tauri.gatt-write-when-ready.mode",
            ));
        }
        let target = self.gatt_target(caller, &payload).await?;
        // Core first: MTU, properties, and the deadline are all core-owned.
        let authority = self.ensure_authority().await?;
        let control = ctl.with_connection_lease(target.lease);
        if wait_ready {
            authority
                .write_when_ready(
                    &target.peer_id,
                    &target.characteristic.selector,
                    bytes.clone(),
                    control,
                )
                .await
        } else {
            authority
                .write(
                    &target.peer_id,
                    &target.characteristic.selector,
                    bytes.clone(),
                    &mode,
                    control,
                )
                .await
        }
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
        acquired: bool,
    ) -> Result<IpcValue, DispatchError> {
        if acquired && !matches!(payload.get("deliveryMode"), None | Some(IpcValue::Null)) {
            return Err(DispatchError::new(
                BleErrorCode::ArgumentInvalid,
                "gatt",
                "tauri.acquire-notifications.delivery-mode",
            ));
        }
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
        let preference = match payload.get("deliveryMode").and_then(as_string) {
            Some("prefer-notification") => Some(DeliveryMode::Notification),
            Some("prefer-indication") => Some(DeliveryMode::Indication),
            _ => None,
        };
        let ctl = ctl.with_delivery_preference(preference);
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
        let terminal_evidence = self
            .terminal_evidence_binding(
                caller,
                &payload,
                "subscription",
                "tauri.subscribe.terminal-evidence",
            )
            .await?;
        // Core first: enablement is core-arbitrated (concurrent subscribers
        // share one physical enable); the consumer below addresses it.
        let authority = self.ensure_authority().await?;
        // The core consumer is internal: it takes no number from the 4.x counter.
        let consumer = self.internal_id("consumer");
        let selector = target.characteristic.selector.clone();
        // The requirement reaches the core on every host. Mac and Linux write
        // notification when both properties exist, so a hard indication
        // requirement is `capability.limited` before any radio effect. Windows
        // can write the requested mode. Dropping the requirement would let
        // that indication subscribe as a notification.
        let acquired_handle = if acquired {
            Some(
                authority
                    .acquire_gatt(
                        &target.peer_id,
                        &selector,
                        ubm_desktop::acquired_gatt::AcquisitionKind::Notify,
                        ctl.clone().with_connection_lease(target.lease.clone()),
                    )
                    .await
                    .map_err(|error| DispatchError::from_core(&error))?,
            )
        } else {
            None
        };
        let delivery = if acquired_handle.is_some() {
            ObservedDelivery::Unknown
        } else {
            authority
                .subscribe(
                    &target.peer_id,
                    &selector,
                    &consumer,
                    requirement,
                    ctl.with_connection_lease(target.lease.clone()),
                )
                .await
                .map_err(|error| DispatchError::from_core(&error))?
        };
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
                            database_handle: required_string(
                                &payload,
                                "databaseHandle",
                                "tauri.subscribe.database",
                            )?,
                            peer_id: target.peer_id.clone(),
                            selector: selector.clone(),
                            consumer: consumer.clone(),
                            delivery,
                            acquired: acquired_handle
                                .as_ref()
                                .map(|native| (native.handle.clone(), target.lease.clone())),
                            task: None,
                            phase: ReleasePhase::Active,
                            terminal_sent: false,
                            terminal_evidence,
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
                match &acquired_handle {
                    Some(native) => OrphanResource::Acquired {
                        handle: native.handle.clone(),
                        lease: target.lease.clone(),
                    },
                    None => OrphanResource::Subscription {
                        peer_id: target.peer_id,
                        selector,
                        consumer,
                    },
                },
            )
            .await;
            return Err(refusal);
        }
        let mut fields = vec![
            ("handle", string(handle)),
            ("observedDelivery", string(delivery.as_str())),
        ];
        if let Some(native) = acquired_handle {
            fields.push(("mtuBytes", number(i64::from(native.mtu))));
        }
        Ok(object(fields))
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
            let mut recovery_announced = false;
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
                let (peer_id, selector, consumer, delivery, acquired) = target;
                let acquired_transport = acquired.is_some();
                let polled = match acquired {
                    Some((native_handle, native_lease)) => authority
                        .acquired_receive(
                            &native_handle,
                            OpControl::unbounded().with_connection_lease(native_lease),
                        )
                        .await
                        .map(NotificationPoll::Value),
                    None => {
                        authority
                            .poll_notification(&peer_id, &selector, &consumer)
                            .await
                    }
                };
                let ending = match polled {
                    Ok(
                        poll @ (NotificationPoll::Value(_)
                        | NotificationPoll::ValueWithReceipt { .. }
                        | NotificationPoll::ValueWithProvenance { .. }),
                    ) => {
                        let (value, receipt, provenance, source_order) = match poll {
                            NotificationPoll::Value(value) => (value, None, None, None),
                            NotificationPoll::ValueWithReceipt { value, receipt } => {
                                (value, Some(receipt), None, None)
                            }
                            NotificationPoll::ValueWithProvenance {
                                value,
                                receipt,
                                provenance,
                                source_generation,
                                source_ordinal,
                            } => (
                                value,
                                receipt,
                                Some(provenance.as_str()),
                                Some((source_generation, source_ordinal)),
                            ),
                            _ => unreachable!("notification value pattern already matched"),
                        };
                        sequence = sequence.saturating_add(1);
                        let observed_at_monotonic_ms =
                            i64::try_from(dispatcher.started_at.elapsed().as_millis())
                                .unwrap_or(i64::MAX);
                        match notification_value_payload(
                            value,
                            delivery,
                            observed_at_monotonic_ms,
                            sequence,
                            receipt.as_ref(),
                            provenance,
                            source_order,
                        ) {
                            Err(error) => (
                                "source-failed",
                                Some(error),
                                terminal_evidence::TerminalCounters::ZERO,
                            ),
                            Ok(payload) => match dispatcher
                                .emit(&key, Some((&lease.0, &lease.1)), &handle, payload)
                                .await
                            {
                                Ok(()) => continue,
                                Err(error) => (
                                    "source-failed",
                                    Some(error),
                                    terminal_evidence::TerminalCounters::ZERO,
                                ),
                            },
                        }
                    }
                    Ok(NotificationPoll::PeerSourceLoss {
                        peer_id,
                        generation,
                        skipped_records,
                    }) => {
                        let payload = object([
                            ("kind", string("peer-source-loss")),
                            ("peerId", string(peer_id)),
                            ("generation", string(generation.to_string())),
                            (
                                "skippedRecords",
                                skipped_records
                                    .map_or(IpcValue::Null, |count| string(count.to_string())),
                            ),
                        ]);
                        match dispatcher
                            .emit(&key, Some((&lease.0, &lease.1)), &handle, payload)
                            .await
                        {
                            Ok(()) => continue,
                            Err(error) => (
                                "source-failed",
                                Some(error),
                                terminal_evidence::TerminalCounters::ZERO,
                            ),
                        }
                    }
                    Ok(NotificationPoll::Empty) => {
                        tokio::time::sleep(FORWARD_POLL_INTERVAL).await;
                        continue;
                    }
                    Ok(NotificationPoll::Terminal(terminal)) => (
                        "overflow",
                        None,
                        terminal_evidence::TerminalCounters::from(terminal),
                    ),
                    Ok(NotificationPoll::SourceFailed(error)) => (
                        "source-failed",
                        Some(DispatchError::from_core(&error)),
                        terminal_evidence::TerminalCounters::ZERO,
                    ),
                    // Do not convert a retained source-failure offer into a
                    // terminal `capability.unsupported`. The core deliberately
                    // holds its producer head until the host has attached an
                    // existing durable recording and a recipient reads and
                    // acknowledges the exact bytes.  Treating the offer as
                    // Empty would hide it; terminating would strand a normal
                    // Tauri attachment with a fabricated permanent failure.
                    //
                    // An opted-in owner receives a one-shot attachment
                    // diagnostic and can pull its opaque durable handles.
                    // Until the trusted host has granted this caller, retain
                    // the source and pace polling; no producer
                    // acknowledgement occurs here.
                    Ok(NotificationPoll::SourceFailureRecovery(_)) => {
                        // Recovery handles and this availability diagnostic
                        // were introduced in IPC 10.  The forwarder outlives
                        // a renderer bootstrap, so route-level gating alone
                        // is insufficient: inspect the active negotiated
                        // caller before making the v10-only fact observable.
                        let recovery_protocol_supported = dispatcher
                            .inner
                            .lock()
                            .await
                            .callers
                            .get(&key)
                            .is_some_and(|caller| {
                                attachment_diagnostics_supported(&caller.versions)
                            });
                        let granted = recovery_protocol_supported
                            && dispatcher
                                .peer_failure_recovery_admission
                                .lock()
                                .expect("peer failure recovery admission mutex poisoned")
                                .get(&key)
                                .is_some_and(|entry| {
                                    entry.owner_generation == Some(entry.generation)
                                });
                        if granted && !recovery_announced {
                            recovery_announced = true;
                            if let Err(error) = dispatcher
                                .emit(
                                    &key,
                                    Some((&lease.0, &lease.1)),
                                    IPC_ATTACHMENT_STREAM_ID,
                                    object([
                                        ("kind", string("diagnostic-warning")),
                                        ("schemaVersion", number(1)),
                                        ("code", string("gatt.peer-failure-recovery-available")),
                                    ]),
                                )
                                .await
                            {
                                return eprintln!("tauri-plugin-unified-ble-manager: durable recovery availability was not delivered: {}", error.describe());
                            }
                        }
                        tokio::time::sleep(FORWARD_POLL_INTERVAL).await;
                        continue;
                    }
                    Ok(NotificationPoll::Invalidated(InvalidationCause::ServicesChanged)) => (
                        "service-changed",
                        None,
                        terminal_evidence::TerminalCounters::ZERO,
                    ),
                    Ok(NotificationPoll::Invalidated(InvalidationCause::LinkEnded)) => (
                        "connection-lost",
                        None,
                        terminal_evidence::TerminalCounters::ZERO,
                    ),
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
                        terminal_evidence::TerminalCounters::ZERO,
                    ),
                    Ok(NotificationPoll::Closed) => (
                        "source-failed",
                        Some(DispatchError::new(
                            BleErrorCode::StreamClosed,
                            "stream",
                            "tauri.notifications",
                        )),
                        terminal_evidence::TerminalCounters::ZERO,
                    ),
                    // Core-side failure ends delivery with the verbatim core
                    // verdict (never a guessed stream error, never silent).
                    Err(error) => (
                        "source-failed",
                        Some(DispatchError::from_core(&error)),
                        terminal_evidence::TerminalCounters::ZERO,
                    ),
                };
                let (reason, error, counters) = ending;
                let mut reported_send_failure = false;
                loop {
                    match dispatcher
                        .notification_terminal_with_counters(
                            &key,
                            (&lease.0, &lease.1),
                            &handle,
                            reason,
                            error.as_ref(),
                            counters,
                        )
                        .await
                    {
                        Ok(true) if acquired_transport => {
                            // A terminal ends delivery; native cleanup remains owned until confirmed.
                            loop {
                                match dispatcher
                                    .release_subscription(&key, &handle, OpControl::unbounded())
                                    .await
                                {
                                    Ok(()) => return,
                                    Err(error) => eprintln!(
                                        "tauri: terminal subscription cleanup remains owned: {}",
                                        error.describe()
                                    ),
                                }
                                tokio::time::sleep(FORWARD_POLL_INTERVAL).await;
                            }
                        }
                        Ok(true) => return,
                        // A requested release took admission while this
                        // poll was in flight. Let that release own the
                        // terminal; if it fails, the mapping remains for
                        // the explicit retry path.
                        Ok(false) => break,
                        // Caller retirement or lease replacement owns this
                        // stream now; it no longer admits delivery here.
                        Err(failure) if failure.code == BleErrorCode::OwnershipDenied => return,
                        Err(failure) => {
                            if !reported_send_failure {
                                eprintln!(
                                    "tauri-plugin-unified-ble-manager: notification terminal delivery failed; retrying: {}",
                                    failure.describe()
                                );
                                reported_send_failure = true;
                            }
                            tokio::time::sleep(FORWARD_POLL_INTERVAL).await;
                        }
                    }
                }
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
        Option<(
            String,
            CoreSelector,
            String,
            ObservedDelivery,
            Option<(String, String)>,
        )>,
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
                subscription.acquired.clone(),
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
                subscription.acquired.clone(),
            )
        };
        let (sender, peer_id, selector, consumer, acquired) = match step {
            (ReleaseStep::Join(receiver), ..) => return join_release(receiver).await,
            (ReleaseStep::Lead(sender), peer_id, selector, consumer, acquired) => {
                (sender, peer_id, selector, consumer, acquired)
            }
        };
        let result = match self.ensure_authority().await {
            Ok(authority) => match acquired {
                Some((native, lease)) => authority
                    .close_acquired(&native, ctl.with_connection_lease(lease))
                    .await
                    .map_err(|error| DispatchError::from_core(&error)),
                None => release_consumer(&authority, &peer_id, &selector, &consumer, ctl).await,
            },
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
        let (peer_id, selector, lease) = self
            .descriptor_target(caller, &payload, "tauri.descriptor-read")
            .await?;
        let authority = self.ensure_authority().await?;
        let value = authority
            .read_descriptor(&peer_id, &selector, ctl.with_connection_lease(lease))
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
        let (peer_id, selector, lease) = self
            .descriptor_target(caller, &payload, "tauri.descriptor-write")
            .await?;
        let authority = self.ensure_authority().await?;
        authority
            .write_descriptor(
                &peer_id,
                &selector,
                bytes.clone(),
                ctl.with_connection_lease(lease),
            )
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
    async fn request_priority(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        use ubm_desktop::boundary::ConnectionPriority;
        let operation = "tauri.request-priority";
        let priority = match required_string(&payload, "priority", operation)?.as_str() {
            "balanced" => ConnectionPriority::Balanced,
            "low-power" => ConnectionPriority::LowPower,
            "high-throughput" => ConnectionPriority::HighThroughput,
            _ => {
                return Err(DispatchError::new(
                    BleErrorCode::ArgumentInvalid,
                    "connection",
                    operation,
                ));
            }
        };
        let connection = self.connection(caller, &payload, operation).await?;
        let accepted = self
            .ensure_authority()
            .await?
            .request_priority(&connection.peer_id, &connection.lease, priority, ctl)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        Ok(object([
            ("accepted", IpcValue::Bool(accepted)),
            ("connectionId", string(&connection.connection_id)),
            (
                "connectionGeneration",
                string(&connection.connection_generation),
            ),
        ]))
    }

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

    /// Effective ATT MTU of the link held under the caller's lease, when
    /// the OS observed one. Windows reads `GattSession.MaxPduSize`. Linux
    /// reads the BlueZ characteristic MTU. macOS sends null: CoreBluetooth
    /// write length is not an ATT MTU. Null is unobserved, not a link failure.
    async fn read_effective_mtu(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        let connection = self
            .connection(caller, &payload, "tauri.effective-mtu")
            .await?;
        let authority = self.ensure_authority().await?;
        let mtu = authority
            .read_effective_mtu(&connection.peer_id, &connection.lease, ctl)
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let mtu_value = match mtu {
            Some(value) => number(i64::from(value)),
            None => IpcValue::Null,
        };
        Ok(object([("mtu", mtu_value)]))
    }

    /// The largest single write the OS accepts on this link for the
    /// requested mode, answered by the core (finding 90): the same limit a
    /// write of that mode is admitted against, so the reported maximum is
    /// never refused. With an OS long write (Windows, Linux) a
    /// with-response write reaches 512 bytes; without response it stays one
    /// ATT payload. An unmeasured limit, or BlueZ's command admission bound
    /// without a reported MTU (finding 97), fails loudly, never guessed.
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
                    ));
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
            lease: connection.lease.clone(),
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
    ) -> Result<(String, CoreSelector, String), DispatchError> {
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
        Ok((
            connection.peer_id.clone(),
            selector,
            connection.lease.clone(),
        ))
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
        let retry_terminal = caller_state.scan.as_ref().is_some_and(|scan| {
            scan.terminal_delivery_retry_item.is_some() && scan.terminal_delivery_retry.is_none()
        });
        drop(state);
        if retry_terminal {
            let dispatcher = self.clone();
            let caller_key = caller_key(&caller);
            tokio::spawn(async move {
                // The mapping retains the precise failure and every retry
                // rechecks caller ownership. A closed/again-full channel
                // remains explicit cleanup debt for the next release.
                let _ = dispatcher
                    .retry_scan_terminal_delivery_failure(&caller_key)
                    .await;
            });
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
        // A renderer that has requested ordinary release no longer owns
        // recovery evidence, even if unrelated radio cleanup needs retries.
        // Do this before awaiting cleanup: a failed unsubscribe or disconnect
        // must not leave prior opaque recovery aliases usable by the retired
        // caller.
        self.revoke_peer_failure_recovery_admission(&key);
        self.revoke_scan_failure_recovery_admission(&key);
        let cleanup = self.release_with_terminal_evidence(&key, false).await;
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
                (
                    "platform",
                    resource
                        .platform
                        .as_ref()
                        .map_or(IpcValue::Null, |detail| native_platform_wire(detail, None)),
                ),
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
                .terminal_with_admission(
                    caller_key,
                    TerminalRequest {
                        expected_lease,
                        stream_id,
                        reason,
                        error: None,
                        counters: terminal_evidence::TerminalCounters::ZERO,
                        admission: TerminalAdmission::Connection,
                    },
                )
                .await
            {
                Ok(_) => break,
                Err(error) if error.code == BleErrorCode::OwnershipDenied => return Err(error),
                Err(_error) => {
                    tokio::time::sleep(terminal_delay).await;
                    terminal_delay =
                        std::cmp::min(terminal_delay.saturating_mul(2), Duration::from_secs(5));
                }
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
        self.terminal_with_admission(
            caller_key,
            TerminalRequest {
                expected_lease,
                stream_id,
                reason,
                error,
                counters: terminal_evidence::TerminalCounters::ZERO,
                admission: TerminalAdmission::Any,
            },
        )
        .await
        .map(|_| ())
    }

    /// Send a notification terminal only while its subscription and
    /// connection are still active. The phase check, synchronous channel
    /// send, and sent marker share one lock: a core poll started before
    /// `disconnect` cannot overtake or duplicate the owner-release terminal.
    async fn notification_terminal(
        &self,
        caller_key: &str,
        expected_lease: (&str, &str),
        stream_id: &str,
        reason: &str,
        error: Option<&DispatchError>,
    ) -> Result<bool, DispatchError> {
        self.notification_terminal_with_counters(
            caller_key,
            expected_lease,
            stream_id,
            reason,
            error,
            terminal_evidence::TerminalCounters::ZERO,
        )
        .await
    }

    async fn notification_terminal_with_counters(
        &self,
        caller_key: &str,
        expected_lease: (&str, &str),
        stream_id: &str,
        reason: &str,
        error: Option<&DispatchError>,
        counters: terminal_evidence::TerminalCounters,
    ) -> Result<bool, DispatchError> {
        self.terminal_with_admission(
            caller_key,
            TerminalRequest {
                expected_lease,
                stream_id,
                reason,
                error,
                counters,
                admission: TerminalAdmission::ActiveSubscription,
            },
        )
        .await
    }

    async fn terminal_with_admission(
        &self,
        caller_key: &str,
        request: TerminalRequest<'_>,
    ) -> Result<bool, DispatchError> {
        let TerminalRequest {
            expected_lease,
            stream_id,
            reason,
            error,
            counters,
            admission,
        } = request;
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
        if matches!(
            admission,
            TerminalAdmission::Connection | TerminalAdmission::OwnerConnection
        ) {
            let Some(stream) = caller_state.connection_events.get(stream_id) else {
                return Ok(false);
            };
            if admission == TerminalAdmission::OwnerConnection && stream.end != StreamEnd::Open {
                return Ok(false);
            }
        }
        if admission == TerminalAdmission::ActiveSubscription {
            let Some(subscription) = caller_state.subscriptions.get(stream_id) else {
                return Ok(false);
            };
            if subscription.terminal_sent
                || !subscription.phase.is_active()
                || !caller_state
                    .connections
                    .get(&subscription.connection_handle)
                    .is_some_and(|connection| connection.phase.is_active())
            {
                return Ok(false);
            }
        }
        if caller_state.scan.as_ref().is_some_and(|scan| {
            scan.handle == stream_id
                && (scan.terminal_failure_sent
                    || (scan.terminal_failure_claimed
                        && admission != TerminalAdmission::ClaimedScan))
        }) {
            return Ok(false);
        }
        let evidence_binding = caller_state
            .scan
            .as_ref()
            .filter(|scan| scan.handle == stream_id)
            .and_then(|scan| scan.terminal_evidence.clone())
            .or_else(|| {
                caller_state
                    .subscriptions
                    .get(stream_id)
                    .and_then(|subscription| subscription.terminal_evidence.clone())
            });
        let mut committed_owner = None;
        let evidence = if let Some(binding) = evidence_binding {
            let ledger = self
                .terminal_evidence_ledger
                .lock()
                .expect("terminal evidence ledger mutex poisoned")
                .clone()
                .ok_or_else(|| {
                    DispatchError::new(
                        BleErrorCode::CapabilityUnsupported,
                        "stream",
                        "tauri.terminal-evidence.ledger-unconfigured",
                    )
                })?;
            let normalized = error.map(|value| value.normalized_error());
            let bytes = terminal_evidence::encode_terminal(
                &binding.scope,
                reason,
                normalized.as_ref(),
                counters,
            );
            ledger
                .stage(&binding.reservation_id, caller_key, &bytes)
                .map_err(|error| {
                    DispatchError::new(
                        BleErrorCode::PlatformFailure,
                        "stream",
                        "tauri.terminal-evidence.stage-before-emit",
                    )
                    .platform(error.to_string())
                })?;
            use sha2::Digest;
            let digest: [u8; 32] = sha2::Sha256::digest(&bytes).into();
            let committed = ledger
                .commit(&binding.reservation_id, &bytes, digest)
                .map_err(|error| {
                    DispatchError::new(
                        BleErrorCode::PlatformFailure,
                        "stream",
                        "tauri.terminal-evidence.commit-before-emit",
                    )
                    .platform(error.to_string())
                })?;
            committed_owner = Some((
                committed.reference.clone(),
                TerminalEvidenceOwner {
                    producer: binding.scope.producer.clone(),
                    caller_key: caller_key.to_owned(),
                    lease_id: caller_state.lease_id.clone(),
                    lease_generation: caller_state.lease_generation.clone(),
                    recovery_lease_id: terminal_evidence_recovery_lease_id(
                        &caller_state.lease_id,
                        &caller_state.lease_generation,
                    ),
                    reservation_id: binding.reservation_id.clone(),
                    recoverable: false,
                },
            ));
            let mut receipt_scope = vec![
                ("managerId", string(binding.scope.manager_id)),
                ("operationId", string(binding.scope.operation_id)),
                ("incarnation", string(binding.scope.incarnation)),
            ];
            if let Some(scope_utf16le) = binding.scope.scope_utf16le {
                receipt_scope.push(("scopeUtf16le", IpcValue::Bytes(scope_utf16le)));
            }
            Some(object([
                ("ref", string(committed.reference)),
                (
                    "byteLength",
                    number(i64::try_from(committed.byte_length).map_err(|_| {
                        DispatchError::new(
                            BleErrorCode::PlatformFailure,
                            "stream",
                            "tauri.terminal-evidence.commit-before-emit",
                        )
                    })?),
                ),
                (
                    "digest",
                    string(
                        committed
                            .digest
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>(),
                    ),
                ),
                ("formatVersion", number(committed.format_version as i64)),
                ("durability", string("committed")),
                ("producer", string(binding.scope.producer)),
                ("scope", object(receipt_scope)),
            ]))
        } else {
            None
        };
        let event_id = self.id("event-terminal");
        caller_state.pending_events.insert(event_id.clone());
        let mut terminal_item = object([
            ("kind", string("terminal")),
            ("reason", string(reason)),
            (
                "droppedItems",
                number(i64::try_from(counters.dropped_items()).map_err(|_| {
                    DispatchError::new(
                        BleErrorCode::PlatformFailure,
                        "stream",
                        "tauri.terminal-counters",
                    )
                })?),
            ),
            (
                "droppedBytes",
                number(i64::try_from(counters.dropped_bytes()).map_err(|_| {
                    DispatchError::new(
                        BleErrorCode::PlatformFailure,
                        "stream",
                        "tauri.terminal-counters",
                    )
                })?),
            ),
            (
                "replacedItems",
                number(i64::try_from(counters.replaced_items()).map_err(|_| {
                    DispatchError::new(
                        BleErrorCode::PlatformFailure,
                        "stream",
                        "tauri.terminal-counters",
                    )
                })?),
            ),
        ]);
        // A committed receipt is the terminal error authority.  Sending a
        // second inline error lets a renderer bypass receipt validation and
        // makes a failed durable read indistinguishable from a complete
        // observation.
        if let (Some(error), None, IpcValue::Object(item)) = (error, &evidence, &mut terminal_item)
        {
            item.insert("error".to_owned(), error.normalized_error());
        }
        if let (Some(evidence), IpcValue::Object(item)) = (evidence, &mut terminal_item) {
            item.insert("evidence".to_owned(), evidence);
        }
        let event_sink = caller_state.event_sink.clone();
        let terminal_event = object([
            (
                "rendererLease",
                object([
                    ("leaseId", string(caller_state.lease_id.clone())),
                    ("generation", string(caller_state.lease_generation.clone())),
                ]),
            ),
            ("eventId", string(event_id.clone())),
            ("streamId", string(stream_id)),
            ("item", terminal_item),
        ]);
        // A channel refusal occurs after the receipt is durably committed.
        // Record its authority before attempting delivery so an exact retry
        // (whose ledger stage/commit is idempotent for these canonical bytes)
        // cannot lose the committed owner between attempts.
        if let Some((reference, owner)) = committed_owner.take() {
            state.terminal_evidence_owners.insert(reference, owner);
        }
        let send_result = event_sink.send(terminal_event);
        if send_result.is_err() {
            if let Some(caller_state) = state.callers.get_mut(caller_key) {
                caller_state.pending_events.remove(&event_id);
            }
        }
        send_result.map_err(|error| {
            DispatchError::new(
                BleErrorCode::PlatformTransport,
                "stream",
                "tauri.terminal-send",
            )
            .platform(error.to_string())
        })?;
        if matches!(
            admission,
            TerminalAdmission::Connection | TerminalAdmission::OwnerConnection
        ) {
            if let Some(caller_state) = state.callers.get_mut(caller_key) {
                caller_state.connection_events.remove(stream_id);
            }
        }
        if admission == TerminalAdmission::ActiveSubscription {
            if let Some(caller_state) = state.callers.get_mut(caller_key) {
                if let Some(subscription) = caller_state.subscriptions.get_mut(stream_id) {
                    subscription.terminal_sent = true;
                }
            }
        }
        Ok(true)
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
        self.release_with_terminal_evidence(key, false).await
    }

    async fn emit_owner_release_terminals(&self, key: &str) -> Result<(), DispatchError> {
        let (lease, scan, subscriptions) = {
            let state = self.inner.lock().await;
            let Some(caller) = state.callers.get(key).filter(|caller| !caller.retired) else {
                return Ok(());
            };
            (
                (caller.lease_id.clone(), caller.lease_generation.clone()),
                caller
                    .scan
                    .as_ref()
                    .filter(|scan| scan.phase.is_active() && !scan.terminal_failure_sent)
                    .map(|scan| scan.handle.clone()),
                caller
                    .subscriptions
                    .iter()
                    .filter_map(|(handle, subscription)| {
                        (subscription.phase.is_active() && !subscription.terminal_sent)
                            .then_some(handle.clone())
                    })
                    .collect::<Vec<_>>(),
            )
        };
        if let Some(handle) = scan {
            self.terminal(key, (&lease.0, &lease.1), &handle, "owner-released", None)
                .await?;
        }
        for handle in subscriptions {
            self.notification_terminal(key, (&lease.0, &lease.1), &handle, "owner-released", None)
                .await?;
        }
        Ok(())
    }

    async fn release_with_terminal_evidence(
        &self,
        key: &str,
        _preserve_terminal_evidence: bool,
    ) -> IpcValue {
        if let Err(error) = self.retry_scan_terminal_delivery_failure(key).await {
            return cleanup_record(vec![cleanup_failure(
                "terminal",
                "tauri.release.scan-terminal-delivery",
                error.describe(),
            )]);
        }
        if let Err(error) = self.emit_owner_release_terminals(key).await {
            return cleanup_record(vec![cleanup_failure(
                "terminal",
                "tauri.release.terminal",
                error.describe(),
            )]);
        }
        let present = {
            let mut state = self.inner.lock().await;
            match state.callers.get_mut(key) {
                Some(caller_state) => {
                    self.invalidate_reclaims(key, None, None);
                    caller_state.retired = true;
                    caller_state.security_watch_releases.extend(
                        caller_state
                            .security_watches
                            .drain()
                            .map(|(handle, _watch)| handle),
                    );
                    caller_state.write_readiness_releases.extend(
                        caller_state
                            .write_readiness_watches
                            .drain()
                            .map(|(handle, _watch)| handle),
                    );
                    caller_state.parameter_releases.extend(
                        caller_state
                            .parameter_watches
                            .drain()
                            .map(|(handle, _watch)| handle),
                    );
                    true
                }
                None => false,
            }
        };
        self.retire_unused_ordered_recording_reservations(key).await;
        let mut failures = Vec::new();
        if present {
            if self.has_reclaim_debt(key, None, None) {
                failures.push(cleanup_failure(
                    "ordered-reclaim",
                    "tauri.reclaim.pending",
                    "reclaim cleanup remains pending".to_owned(),
                ));
            } else {
                failures.extend(self.settle_caller(key).await);
            }
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
            {
                let mut state = self.inner.lock().await;
                for (handle, owner) in &mut state.terminal_evidence_owners {
                    if owner.caller_key == key && handle != &owner.reservation_id {
                        owner.recoverable = true;
                    }
                }
            }
            let reservations = {
                let state = self.inner.lock().await;
                state
                    .terminal_evidence_owners
                    .iter()
                    .filter(|(_, owner)| owner.caller_key == key)
                    .fold(
                        std::collections::HashMap::new(),
                        |mut entries, (handle, owner)| {
                            entries
                                .entry(owner.reservation_id.clone())
                                .and_modify(|committed| {
                                    *committed |= handle != &owner.reservation_id
                                })
                                .or_insert(handle != &owner.reservation_id);
                            entries
                        },
                    )
            };
            let terminal_evidence_ledger = self
                .terminal_evidence_ledger
                .lock()
                .expect("terminal evidence ledger mutex poisoned")
                .clone();
            if let Some(ledger) = terminal_evidence_ledger {
                // A recovered row is protected by a controller-specific
                // ledger authority, not merely this dispatcher's in-memory
                // alias. Release that claim before abandoning the prior
                // caller's source reservations: `abandon` must be able to
                // acquire source authority, and a fresh controller may then
                // claim the durable recovery row.
                let recovery_lease_id = {
                    let state = self.inner.lock().await;
                    state.callers.get(key).map(|caller_state| {
                        terminal_evidence_recovery_lease_id(
                            &caller_state.lease_id,
                            &caller_state.lease_generation,
                        )
                    })
                };
                let recovery_lease_released = match recovery_lease_id {
                    Some(recovery_lease_id) => {
                        match ledger.release_recovery_lease(&recovery_lease_id) {
                            Ok(()) => true,
                            Err(error) => {
                                failures.push(cleanup_failure(
                                    "terminal-evidence",
                                    "tauri.terminal-evidence.release-recovery-lease",
                                    error.to_string(),
                                ));
                                false
                            }
                        }
                    }
                    None => true,
                };
                if recovery_lease_released {
                    for (reservation_id, _committed) in reservations {
                        // `abandon` relinquishes the runtime authority for both
                        // unused and committed reservations. Its committed path
                        // deliberately retains the bytes for the authenticated
                        // recovery ACK; skipping it leaves the durable ledger
                        // believing the retired manager is still live and makes
                        // a same-process rebootstrap refuse the recovery lease.
                        if let Err(error) = ledger.abandon(&reservation_id) {
                            failures.push(cleanup_failure(
                                "terminal-evidence",
                                "tauri.terminal-evidence.release-owner",
                                error.to_string(),
                            ));
                        }
                    }
                }
            }
            if failures.is_empty() {
                self.inner
                    .lock()
                    .await
                    .terminal_evidence_owners
                    .retain(|handle, owner| {
                        owner.caller_key != key || handle != &owner.reservation_id
                    });
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
        loop {
            {
                // Do not hold admission while waiting for a native operation:
                // a replacement attachment may itself redrive cleanup.
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
                if matches!(
                    *self.authority.lock().await,
                    AuthoritySlot::Closing(_) | AuthoritySlot::ShutDown
                ) {
                    return;
                }
                let _ = self.release_with_terminal_evidence(&key, false).await;
                self.settle_revoked_ordered(&key).await;
                self.settle_revoked_reclaims(&key).await;
                let cleanup = self.release_with_terminal_evidence(&key, false).await;
                if is_released(&cleanup) {
                    let mut revoked = self
                        .revoked_callers
                        .lock()
                        .expect("revocation mutex poisoned");
                    if revoked.get(&key).copied() == Some(revocation) {
                        revoked.remove(&key);
                    }
                    return;
                }
                // Other cleanup failures retain their existing retry contract.
                // Reclaim operations can settle after this caller disappears,
                // so keep exactly one task for this revocation until they do.
                if !self.has_reclaim_debt(&key, None, None) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
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
        let writers = self
            .inner
            .lock()
            .await
            .callers
            .get(key)
            .map(|owner| owner.acquired_writers.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for handle in writers {
            if let Err(error) = self
                .release_acquired_writer(key, &handle, OpControl::unbounded())
                .await
            {
                failures.push(cleanup_failure(
                    "acquired-gatt-writer",
                    "tauri.release.acquired",
                    error.describe(),
                ));
            }
        }
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
        let groups = self
            .inner
            .lock()
            .await
            .callers
            .get(key)
            .map(|owner| {
                owner
                    .ordered_groups
                    .iter()
                    .map(|(handle, group)| (handle.clone(), group.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (handle, group) in groups {
            match self
                .close_ordered_resource(group.clone(), OpControl::unbounded())
                .await
            {
                Ok(IpcValue::Object(result))
                    if result.get("cleanupState") == Some(&string("released")) =>
                {
                    if self.is_revoked(key) {
                        failures.push(cleanup_failure(
                            "ordered-group",
                            "tauri.release.ordered-receipt",
                            "revoked group receipt remains owned".to_owned(),
                        ));
                    } else if let Err(error) = self
                        .retire_ordered_resource(key, &handle, group.clone())
                        .await
                    {
                        failures.push(cleanup_failure(
                            "ordered-group",
                            "tauri.release.ordered-ack",
                            error.describe(),
                        ));
                    } else {
                        self.remove_ordered_mapping(key, &handle, &group).await;
                    }
                }
                Ok(_) => failures.push(cleanup_failure(
                    "ordered-group",
                    "tauri.release.ordered",
                    "ordered group has undrained or pending source records".to_owned(),
                )),
                Err(error) => failures.push(cleanup_failure(
                    "ordered-group",
                    "tauri.release.ordered",
                    error.describe(),
                )),
            }
        }
        let remaining_groups = self
            .inner
            .lock()
            .await
            .callers
            .get(key)
            .map(|owner| owner.ordered_groups.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut blocked_connections = HashSet::new();
        for group in remaining_groups {
            blocked_connections.insert(group.lock().await.connection_handle.clone());
        }
        for handle in connections {
            if blocked_connections.contains(&handle) {
                failures.push(cleanup_failure(
                    "connection",
                    "tauri.release.ordered-pending",
                    "ordered group cleanup remains owned".to_owned(),
                ));
                continue;
            }
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
    lease: String,
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
        OrphanResource::Acquired { handle, lease } => authority
            .close_acquired(
                handle,
                OpControl::unbounded().with_connection_lease(lease.clone()),
            )
            .await
            .map_err(|error| DispatchError::from_core(&error)),
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
        self.revoke_peer_failure_recovery_admission(&key);
        self.revoke_scan_failure_recovery_admission(&key);
        self.peer_failure_recovery_handles
            .lock()
            .expect("peer failure recovery handle mutex poisoned")
            .remove(&key);
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
            | "gatt.ordered.orphans"
            | "gatt.ordered.orphan-ack"
            | "gatt.ordered.orphan-page"
            | "gatt.ordered.status"
            | "gatt.ordered.next"
            | "gatt.ordered.close"
            | "gatt.ordered.forget"
            | "gatt.ordered.retire"
            | "gatt.database.release"
            | "gatt.acquired-write.close"
            | "connection.disconnect"
            | "connection.events.unsubscribe"
            | "connection.write-readiness.unsubscribe"
            | "security.watch.unsubscribe"
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
        "scan.stop"
            | "gatt.unsubscribe"
            | "gatt.ordered.close"
            | "gatt.ordered.forget"
            | "gatt.ordered.retire"
            | "gatt.ordered.orphan-ack"
            | "connection.disconnect"
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

/// Produces an injective, host-private identity for a recovery controller.
/// `lease_id` and its generation are independently validated on every Tauri
/// route; their framed combination lets the durable ledger fence a controller
/// without granting the stable caller identity destructive recovery power.
fn terminal_evidence_recovery_lease_id(lease_id: &str, lease_generation: &str) -> String {
    format!("{}:{lease_id}{lease_generation}", lease_id.len())
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
/// IPC observation wire shape (`peerId`, `address`, `connectable`, `localName`, `rssi`,
/// `txPowerLevel`, `serviceUuids`, `manufacturerData`, `serviceData`, `origin`) with
/// no filtering, merging, or re-sampling. The radio facts cross unchanged;
/// delivery policy is the core's.
async fn typed_core_scan_observation(
    authority: &dyn CoreAuthority,
    snapshot: &ubm_desktop::PeerSnapshot,
    cache: &mut HashMap<String, Option<ubm_desktop::AddressType>>,
) -> Result<IpcValue, DispatchError> {
    let kind = if snapshot.address.is_none() {
        None
    } else if let Some(kind) = snapshot.extras.address_type {
        // Production BlueZ and WinRT reports capture this alongside the
        // sighting. A later peripheral lookup must not revise its identity.
        Some(kind)
    } else if let Some(kind) = cache.get(&snapshot.id) {
        *kind
    } else {
        let kind = match authority
            .address_type(&snapshot.id, OpControl::budget_ms(1000))
            .await
        {
            Ok(kind) => kind,
            Err(error) if error.code_str() == "capability.unsupported" => None,
            Err(error) => return Err(DispatchError::from_core(&error)),
        };
        // Address type is stable for a peer identity. Bound this scan-local cache;
        // exceeding it only repeats the native lookup, never drops observations.
        if cache.len() < MAX_PENDING_EVENTS {
            cache.insert(snapshot.id.clone(), kind);
        }
        kind
    };
    Ok(core_scan_observation(snapshot, kind))
}

fn core_scan_observation(
    snapshot: &ubm_desktop::PeerSnapshot,
    address_type: Option<ubm_desktop::AddressType>,
) -> IpcValue {
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
            "addressType",
            address_type.map_or(IpcValue::Null, |kind| string(kind.as_str())),
        ),
        (
            "address",
            snapshot.address.clone().map_or(IpcValue::Null, string),
        ),
        (
            "connectable",
            snapshot
                .extras
                .connectable
                .map_or(IpcValue::Null, IpcValue::Bool),
        ),
        ("origin", string(snapshot.extras.source.as_str())),
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

const UNSAMPLED_SNAPSHOT_REASON: &str = "This snapshot carries attachment identity only; availability, authorization, power, and the heard peer count are not sampled here, so route adapter.state for a live reading.";

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

const CLOCK_BEFORE_EPOCH_REASON: &str = "The host wall clock reads before the Unix epoch; updatedAt reports the newest timestamp this host has observed instead.";
const CLOCK_MOVED_BACKWARDS_REASON: &str = "The host wall clock moved backwards; updatedAt is held at the newest timestamp this host has observed.";

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
/// offering only 2 is refused at bootstrap as `protocol.incompatible`. Version 9
/// carries individual ordered failure observations and ordered platform metadata;
/// version 10 carries ownerless native-source diagnostics on the attachment stream.
pub(crate) const IPC_PROTOCOL_VERSION: i64 = 11;

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
        ("ipcProtocol", negotiate_ipc_protocol(remote_offer)?),
    ]))
}

const IPC_PROTOCOL_MINIMUM_VERSION: i64 = 6;
/// Ordered deliveries carry individual failure observations from protocol 9.
/// A pre-v9 renderer cannot safely distinguish those records from the older
/// terminal aggregate, so ordered routes are unavailable to it rather than
/// silently lossy.
const ORDERED_ROUTE_MINIMUM_IPC_PROTOCOL: i64 = 9;
/// Protocol 11 reserves the host-private durable terminal-evidence routes.
/// This plugin does not advertise 11 until a configured recording-owned
/// journal is wired to the public backend, so pre-11 callers receive an
/// explicit unsupported result rather than a volatile approximation.
const TERMINAL_EVIDENCE_ROUTE_MINIMUM_IPC_PROTOCOL: i64 = 11;

fn negotiate_ipc_protocol(
    remote_offer: &BTreeMap<String, IpcValue>,
) -> Result<IpcValue, DispatchError> {
    let range = into_object(
        required_value(remote_offer, "ipcProtocol", "tauri.bootstrap-offer-range")?.clone(),
        "tauri.bootstrap-offer-range",
    )?;
    if range.len() != 3
        || required_string(&range, "axis", "tauri.bootstrap-offer-axis")? != "ipc-protocol"
    {
        return Err(DispatchError::new(
            BleErrorCode::ProtocolMalformed,
            "ipc",
            "tauri.bootstrap-offer-range",
        ));
    }
    let minimum = offered_version(&range, "minimum", "ipc-protocol")?;
    let maximum = offered_version(&range, "maximum", "ipc-protocol")?;
    if minimum > maximum {
        return Err(DispatchError::new(
            BleErrorCode::ProtocolMalformed,
            "ipc",
            "tauri.bootstrap-offer-range-order",
        ));
    }
    let selected = maximum.min(IPC_PROTOCOL_VERSION);
    if selected < IPC_PROTOCOL_MINIMUM_VERSION || selected < minimum {
        return Err(DispatchError::new(
            BleErrorCode::ProtocolIncompatible,
            "ipc",
            "tauri.bootstrap-version-ipc-protocol",
        ));
    }
    Ok(object([
        ("axis", string("ipc-protocol")),
        ("selected", version_number("ipc-protocol", selected)),
        (
            "localRange",
            object([
                ("axis", string("ipc-protocol")),
                (
                    "minimum",
                    version_number("ipc-protocol", IPC_PROTOCOL_MINIMUM_VERSION),
                ),
                (
                    "maximum",
                    version_number("ipc-protocol", IPC_PROTOCOL_VERSION),
                ),
            ]),
        ),
        (
            "remoteRange",
            object([
                ("axis", string("ipc-protocol")),
                ("minimum", version_number("ipc-protocol", minimum)),
                ("maximum", version_number("ipc-protocol", maximum)),
            ]),
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

fn attachment_diagnostics_supported(versions: &IpcValue) -> bool {
    let IpcValue::Object(versions) = versions else {
        return false;
    };
    let Some(IpcValue::Object(protocol)) = versions.get("ipcProtocol") else {
        return false;
    };
    let Some(IpcValue::Object(selected)) = protocol.get("selected") else {
        return false;
    };
    matches!(selected.get("value"), Some(IpcValue::Number(value)) if value.as_i64().is_some_and(|value| value >= 10))
}

/// The bootstrap response is retained verbatim with the caller lease.  Read
/// the selected IPC revision from that authoritative result instead of
/// trusting a version supplied again by a webview route.
fn selected_ipc_protocol(versions: &IpcValue) -> Option<i64> {
    let IpcValue::Object(versions) = versions else {
        return None;
    };
    let Some(IpcValue::Object(protocol)) = versions.get("ipcProtocol") else {
        return None;
    };
    let Some(IpcValue::Object(selected)) = protocol.get("selected") else {
        return None;
    };
    match selected.get("value") {
        Some(IpcValue::Number(value)) => value.as_i64(),
        _ => None,
    }
}

/// Ordered records gained individual failure observations in IPC protocol 9.
/// Keep the classification centralized so every ordered group, marker,
/// recording, reclaim, and orphan route has the same fail-closed boundary.
fn is_ordered_route_command(command: &str) -> bool {
    command.starts_with("gatt.ordered.") || command.starts_with("gatt.ordered-recording.")
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

/// Service-level restriction from the service row, not from a
/// characteristic or descriptor row (those carry `None`).
fn service_level_access(
    paths: &[DiscoveredPath],
    uuid: &str,
    occurrence: u64,
) -> Option<ServiceAccess> {
    paths.iter().find_map(|path| {
        if path.characteristic_uuid.is_none()
            && path.service_uuid == uuid
            && path.service_occurrence == occurrence
        {
            path.service_access
        } else {
            None
        }
    })
}

/// IPC service record. `Open` and a missing note stay unrestricted, matching
/// the Electron serializer.
fn ipc_service_record(
    uuid: &str,
    occurrence: u64,
    access: Option<ServiceAccess>,
    metadata: Option<&DiscoveredPath>,
) -> IpcValue {
    let mut record = match object([
        ("uuid", string(uuid)),
        ("occurrence", string(occurrence.to_string())),
        (
            "primary",
            metadata
                .and_then(|path| path.service_primary)
                .map_or(IpcValue::Null, IpcValue::Bool),
        ),
        (
            "includedServices",
            metadata
                .and_then(|path| path.included_services.as_ref())
                .map_or(IpcValue::Null, |references| {
                    IpcValue::Array(
                        references
                            .iter()
                            .map(|reference| {
                                object([
                                    ("uuid", string(&reference.uuid)),
                                    ("occurrence", string(reference.occurrence.to_string())),
                                ])
                            })
                            .collect(),
                    )
                }),
        ),
    ]) {
        IpcValue::Object(record) => record,
        _ => unreachable!("service record is an object"),
    };
    if let Some(restriction) = service_restriction(access) {
        record.insert("restriction".to_owned(), restriction);
    }
    IpcValue::Object(record)
}

fn service_restriction(access: Option<ServiceAccess>) -> Option<IpcValue> {
    let (reason, gatt_status) = match access {
        Some(ServiceAccess::OsReserved) => ("os-reserved", IpcValue::Null),
        Some(ServiceAccess::AccessDenied) => ("access-denied", string("access-denied")),
        Some(ServiceAccess::Open) | None => return None,
    };
    Some(object([
        ("state", string("restricted")),
        ("reason", string(reason)),
        ("gattStatus", gatt_status),
        ("attError", IpcValue::Null),
    ]))
}

fn object<'a>(entries: impl IntoIterator<Item = (&'a str, IpcValue)>) -> IpcValue {
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

fn notification_value_payload(
    value: Vec<u8>,
    delivery: ObservedDelivery,
    observed_at_monotonic_ms: i64,
    sequence: u64,
    receipt: Option<&ubm_core::central::NotificationReceipt>,
    provenance: Option<&str>,
    source_order: Option<(u64, u64)>,
) -> Result<IpcValue, DispatchError> {
    let mut fields = vec![
        ("value", IpcValue::Bytes(value)),
        ("delivery", string(delivery.as_str())),
        ("observedAtMonotonicMs", number(observed_at_monotonic_ms)),
        ("sequence", number(sequence as i64)),
    ];
    if let Some((generation, ordinal)) = source_order {
        fields.push(("sourceGeneration", string(generation.to_string())));
        fields.push(("sourceOrdinal", string(ordinal.to_string())));
    }
    if let Some(provenance) = provenance {
        fields.push(("provenance", string(provenance)));
    }
    if let Some(receipt) = receipt {
        let received_at_utc_ms = Number::from_f64(receipt.received_at_utc_ms).ok_or_else(|| {
            DispatchError::new(
                BleErrorCode::ProtocolViolation,
                "notification",
                "tauri.notifications",
            )
            .platform("native notification receipt has a non-finite receivedAtUtcMs")
        })?;
        let received_at_monotonic_ms = Number::from_f64(receipt.received_at_monotonic_ms)
            .ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::ProtocolViolation,
                    "notification",
                    "tauri.notifications",
                )
                .platform("native notification receipt has a non-finite receivedAtMonotonicMs")
            })?;
        fields.push((
            "receipt",
            object([
                ("receivedAtUtcMs", IpcValue::Number(received_at_utc_ms)),
                (
                    "receivedAtMonotonicMs",
                    IpcValue::Number(received_at_monotonic_ms),
                ),
                ("clockId", string(receipt.clock_id.clone())),
                ("origin", string("native")),
            ]),
        ));
    }
    Ok(object(fields))
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

fn decode_windows_scan_options(
    value: Option<&IpcValue>,
) -> Result<Option<ubm_desktop::boundary::WindowsScanOptions>, DispatchError> {
    use ubm_desktop::boundary::{WindowsScanOptions, WindowsScanningMode};
    let Some(value) = value else {
        return Ok(None);
    };
    let operation = "scan.start.platform-options";
    let IpcValue::Object(fields) = value else {
        return Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid,
            "scan",
            operation,
        ));
    };
    if required_string(fields, "kind", operation)? != "winrt" {
        return Err(DispatchError::new(
            BleErrorCode::CapabilityUnsupported,
            "capability",
            operation,
        ));
    }
    if fields
        .keys()
        .any(|key| !["kind", "mode", "allowExtendedAdvertisements"].contains(&key.as_str()))
    {
        return Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid,
            "scan",
            operation,
        ));
    }
    let mode = match fields.get("mode") {
        None => WindowsScanningMode::Active,
        Some(IpcValue::String(mode)) if mode == "active" => WindowsScanningMode::Active,
        Some(IpcValue::String(mode)) if mode == "passive" => WindowsScanningMode::Passive,
        Some(IpcValue::String(mode)) if mode == "none" => WindowsScanningMode::None,
        _ => {
            return Err(DispatchError::new(
                BleErrorCode::ArgumentInvalid,
                "scan",
                operation,
            ));
        }
    };
    let allow_extended_advertisements = match fields.get("allowExtendedAdvertisements") {
        None => false,
        Some(IpcValue::Bool(value)) => *value,
        _ => {
            return Err(DispatchError::new(
                BleErrorCode::ArgumentInvalid,
                "scan",
                operation,
            ));
        }
    };
    Ok(Some(WindowsScanOptions {
        mode,
        allow_extended_advertisements,
    }))
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

fn optional_recovery_index(
    object: &BTreeMap<String, IpcValue>,
    operation: &'static str,
) -> Result<Option<usize>, DispatchError> {
    match object.get("index") {
        None => Ok(None),
        Some(IpcValue::Number(value)) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| {
                DispatchError::new(BleErrorCode::ArgumentInvalid, "restoration", operation)
            }),
        _ => Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid,
            "restoration",
            operation,
        )),
    }
}

fn recovery_usize(
    object: &BTreeMap<String, IpcValue>,
    key: &str,
    operation: &'static str,
) -> Result<usize, DispatchError> {
    match object.get(key) {
        Some(IpcValue::Number(value)) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| {
                DispatchError::new(BleErrorCode::ArgumentInvalid, "restoration", operation)
            }),
        _ => Err(DispatchError::new(
            BleErrorCode::ProtocolMalformed,
            "ipc",
            operation,
        )),
    }
}

fn scan_failure_raw_token_value(
    token: ubm_desktop::continuation_outbox::ScanFailureRawToken,
) -> IpcValue {
    use ubm_desktop::continuation_outbox::ScanFailureRawToken;
    match token {
        ScanFailureRawToken::Null => object([("kind", string("null"))]),
        ScanFailureRawToken::Bool(value) => {
            object([("kind", string("bool")), ("value", IpcValue::Bool(value))])
        }
        ScanFailureRawToken::I64(value) => object([
            ("kind", string("i64")),
            ("value", string(value.to_string())),
        ]),
        ScanFailureRawToken::U64(value) => object([
            ("kind", string("u64")),
            ("value", string(value.to_string())),
        ]),
        ScanFailureRawToken::F64Bits(value) => object([
            ("kind", string("f64Bits")),
            ("value", string(value.to_string())),
        ]),
        ScanFailureRawToken::Text { total, bytes } => object([
            ("kind", string("text")),
            ("total", string(total.to_string())),
            ("bytes", IpcValue::Bytes(bytes)),
        ]),
        ScanFailureRawToken::Bytes { total, bytes } => object([
            ("kind", string("bytes")),
            ("total", string(total.to_string())),
            ("bytes", IpcValue::Bytes(bytes)),
        ]),
        ScanFailureRawToken::Map(value) => object([
            ("kind", string("map")),
            ("count", string(value.to_string())),
        ]),
        ScanFailureRawToken::Array(value) => object([
            ("kind", string("array")),
            ("count", string(value.to_string())),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        characteristic_properties, core_identity, diagnostic_occurrence_id,
        diagnostic_occurrence_text, diagnostic_offset, negotiate_ipc_versions,
        notification_value_payload, object, released, scan_properties_match_optional, string,
        DispatchError, IpcValue,
    };
    use btleplug::api::CharPropFlags;
    use ubm_core::contracts::BleErrorCode;

    #[test]
    fn diagnostic_occurrence_host_boundary_requires_canonical_stable_identity() {
        let id = diagnostic_occurrence_id(&"ab".repeat(40)).unwrap();
        assert_eq!(diagnostic_occurrence_text(id), "ab".repeat(40));
        assert!(diagnostic_occurrence_id(&"AB".repeat(40)).is_err());
        assert!(diagnostic_occurrence_id(&"ab".repeat(39)).is_err());
        assert!(diagnostic_occurrence_id(&"gg".repeat(40)).is_err());
        assert_eq!(diagnostic_offset("0").unwrap(), 0);
        assert!(diagnostic_offset("01").is_err());
        assert!(diagnostic_offset("9223372036854775808").is_err());
    }

    #[test]
    fn notification_value_payload_preserves_native_receipt_and_absence() {
        let without_receipt = notification_value_payload(
            vec![0x42],
            ubm_desktop::ObservedDelivery::Notification,
            17,
            3,
            None,
            Some("read-or-notification"),
            Some((9, 42)),
        )
        .expect("value payload");
        let IpcValue::Object(fields) = without_receipt else {
            panic!("object")
        };
        assert_eq!(fields.get("receipt"), None);
        assert_eq!(
            fields.get("sourceGeneration"),
            Some(&IpcValue::String("9".into()))
        );
        assert_eq!(
            fields.get("sourceOrdinal"),
            Some(&IpcValue::String("42".into()))
        );
        assert_eq!(
            fields.get("provenance"),
            Some(&IpcValue::String("read-or-notification".into()))
        );

        let receipt = ubm_core::central::NotificationReceipt {
            received_at_utc_ms: -0.5,
            received_at_monotonic_ms: 42.25,
            clock_id: "native-receipt-clock".to_owned(),
        };
        let with_receipt = notification_value_payload(
            vec![0x42],
            ubm_desktop::ObservedDelivery::Indication,
            17,
            3,
            Some(&receipt),
            None,
            None,
        )
        .expect("receipt payload");
        let IpcValue::Object(fields) = with_receipt else {
            panic!("object")
        };
        let IpcValue::Object(receipt_fields) = fields.get("receipt").expect("receipt") else {
            panic!("receipt object")
        };
        assert_eq!(
            receipt_fields.get("receivedAtUtcMs"),
            Some(&IpcValue::Number(
                serde_json::Number::from_f64(-0.5).expect("finite")
            ))
        );
        assert_eq!(
            receipt_fields.get("receivedAtMonotonicMs"),
            Some(&IpcValue::Number(
                serde_json::Number::from_f64(42.25).expect("finite")
            ))
        );
        assert_eq!(
            receipt_fields.get("clockId"),
            Some(&IpcValue::String("native-receipt-clock".into()))
        );
        assert_eq!(
            receipt_fields.get("origin"),
            Some(&IpcValue::String("native".into()))
        );
    }

    #[test]
    fn notification_value_payload_rejects_nonfinite_receipt_numbers() {
        let receipt = ubm_core::central::NotificationReceipt {
            received_at_utc_ms: f64::NAN,
            received_at_monotonic_ms: 1.0,
            clock_id: "clock".to_owned(),
        };
        let error = notification_value_payload(
            vec![0x42],
            ubm_desktop::ObservedDelivery::Notification,
            17,
            3,
            Some(&receipt),
            None,
            None,
        )
        .expect_err("non-finite receipt must fail");
        assert_eq!(error.code, BleErrorCode::ProtocolViolation);
    }

    #[test]
    fn trusted_bluez_policy_is_not_a_renderer_option() {
        let options = super::BtleplugDispatcherOptions {
            connection_policy: Some(ubm_desktop::boundary::BluezConnectionPolicy::LeBearer {
                daemon_unique_owner: ":1.42".into(),
            }),
            ..Default::default()
        };
        let _dispatcher = super::BtleplugDispatcher::new(options);
        assert!(super::BtleplugDispatcherOptions::default()
            .connection_policy
            .is_none());
    }

    #[tokio::test]
    async fn trusted_bluez_policy_reaches_the_same_lazy_authority_opener() {
        let expected = ubm_desktop::boundary::BluezConnectionPolicy::LeBearer {
            daemon_unique_owner: ":1.42".into(),
        };
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = seen.clone();
        let dispatcher = super::BtleplugDispatcher::with_profiled_opener(
            super::BtleplugDispatcherOptions {
                adapter_id: Some("hci1".into()),
                connection_policy: Some(expected.clone()),
                failure_evidence_store: None,
                terminal_evidence_maximum_bytes: None,
            },
            std::sync::Arc::new(move |profile, policy| {
                captured
                    .lock()
                    .unwrap()
                    .push((profile.adapter_id.clone(), policy));
                Box::pin(async {
                    Err(super::DispatchError::new(
                        BleErrorCode::AdapterUnavailable,
                        "adapter",
                        "test.open",
                    ))
                })
            }),
        );
        assert!(seen.lock().unwrap().is_empty());
        assert!(dispatcher.ensure_authority().await.is_err());
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(Some("hci1".into()), Some(expected))]
        );
    }

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
            (
                "ipcProtocol",
                offer_range("ipc-protocol", super::IPC_PROTOCOL_VERSION),
            ),
        ]) else {
            panic!("the version offer must be an object");
        };
        offer
    }

    // Protocol 6 is the 5.2.1 common contract. Older hosts could silently
    // substitute defaults for routes they do not understand, so bootstrap
    // rejects them before any radio operation. Protocol 11 additionally
    // carries durable terminal evidence; it is a compatible extension, not a new minimum.
    #[test]
    fn version_offer_requires_ipc_protocol_11_and_refuses_an_old_webview() {
        assert_eq!(super::IPC_PROTOCOL_VERSION, 11);
        let mut old = current_offer();
        old.insert("ipcProtocol".to_owned(), offer_range("ipc-protocol", 5));
        let error = negotiate_ipc_versions(&old).expect_err("an old webview must be refused");
        assert_eq!(error.code, BleErrorCode::ProtocolIncompatible);

        let mut compatible_old = current_offer();
        compatible_old.insert("ipcProtocol".to_owned(), offer_range("ipc-protocol", 6));
        let IpcValue::Object(versions) = negotiate_ipc_versions(&compatible_old)
            .expect("protocol 6 common contract must negotiate")
        else {
            panic!("versions")
        };
        let Some(super::IpcValue::Object(protocol)) = versions.get("ipcProtocol") else {
            panic!("ipc protocol version")
        };
        let Some(super::IpcValue::Object(selected)) = protocol.get("selected") else {
            panic!("selected protocol version")
        };
        assert_eq!(selected.get("value"), Some(&super::number(6)));

        let mut newer = current_offer();
        newer.insert("ipcProtocol".to_owned(), offer_range("ipc-protocol", 12));
        let error =
            negotiate_ipc_versions(&newer).expect_err("a newer-only webview must be refused");
        assert_eq!(error.code, BleErrorCode::ProtocolIncompatible);

        let super::IpcValue::Object(versions) =
            negotiate_ipc_versions(&current_offer()).expect("protocol 10 must negotiate")
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
                ("value", super::number(super::IPC_PROTOCOL_VERSION))
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
                        ("value", super::number(super::IPC_PROTOCOL_VERSION))
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
                                ("value", super::number(super::IPC_PROTOCOL_MINIMUM_VERSION))
                            ])
                        ),
                        (
                            "maximum",
                            object([
                                ("axis", string("ipc-protocol")),
                                ("value", super::number(super::IPC_PROTOCOL_VERSION))
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
                                ("value", super::number(super::IPC_PROTOCOL_VERSION))
                            ])
                        ),
                        (
                            "maximum",
                            object([
                                ("axis", string("ipc-protocol")),
                                ("value", super::number(super::IPC_PROTOCOL_VERSION))
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
                ordered_groups: std::collections::HashMap::new(),
                acquired_writers: std::collections::HashMap::new(),
                acquired_releases: Default::default(),
                connection_events: std::collections::HashMap::new(),
                security_watches: std::collections::HashMap::new(),
                security_watch_releases: std::collections::HashSet::new(),
                write_readiness_watches: std::collections::HashMap::new(),
                write_readiness_releases: Default::default(),
                parameter_watches: std::collections::HashMap::new(),
                parameter_releases: Default::default(),
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

        #[tokio::test]
        async fn trusted_recording_controls_reopen_and_acknowledge_one_native_cursor() {
            let directory = std::env::temp_dir().join(format!(
                "ubm-tauri-recording-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&directory).unwrap();
            let journal = ubm_desktop::continuation_journal::ContinuationJournal::open(
                &directory.join("sample.sqlite"),
                "sample",
                &serde_json::json!({}),
                ubm_desktop::continuation_journal::JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 10,
                },
            )
            .unwrap();
            journal
                .append(&serde_json::json!({}), &serde_json::json!({"value":1}))
                .unwrap();
            drop(journal);
            let dispatcher = BtleplugDispatcher::default();
            dispatcher
                .continuation_configure_recording_directory(&directory)
                .await
                .unwrap();
            let batch = dispatcher
                .continuation_recording_prepare("sample", 1, 4096)
                .await
                .unwrap();
            assert_eq!(batch["records"][0]["record"]["value"], 1);
            let token = batch["token"].as_str().unwrap();
            assert_eq!(
                dispatcher
                    .continuation_recording_acknowledge("sample", token)
                    .await
                    .unwrap(),
                dispatcher
                    .continuation_recording_acknowledge("sample", token)
                    .await
                    .unwrap()
            );
            assert_eq!(
                dispatcher
                    .continuation_recording_status("sample")
                    .await
                    .unwrap()["records"],
                0
            );
            dispatcher
                .continuation_recording_stop("sample")
                .await
                .unwrap();
            assert_eq!(
                dispatcher
                    .continuation_recording_clear("sample")
                    .await
                    .unwrap()["cleared"],
                true
            );
            assert_eq!(
                dispatcher
                    .continuation_recording_status("../sample")
                    .await
                    .unwrap_err()["code"],
                "argument.invalid"
            );
            assert!(matches!(
                *dispatcher.authority.lock().await,
                super::super::AuthoritySlot::Unopened(_)
            ));
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
mod service_restriction_tests {
    use super::{ipc_service_record, service_level_access, string};
    use crate::IpcValue;
    use ubm_desktop::{DiscoveredPath, ServiceAccess};

    fn path(
        service_uuid: &str,
        characteristic_uuid: Option<&str>,
        access: Option<ServiceAccess>,
    ) -> DiscoveredPath {
        DiscoveredPath {
            service_primary: None,
            included_services: None,
            service_uuid: service_uuid.to_owned(),
            service_occurrence: 0,
            characteristic_uuid: characteristic_uuid.map(str::to_owned),
            characteristic_occurrence: characteristic_uuid.map(|_| 0),
            descriptor_uuid: None,
            descriptor_occurrence: None,
            properties: 0,
            access: None,
            service_access: access,
        }
    }

    #[test]
    fn characteristic_rows_do_not_hide_a_service_restriction() {
        let paths = vec![
            path("180f", Some("2a19"), None),
            path("180f", None, Some(ServiceAccess::AccessDenied)),
        ];
        assert_eq!(
            service_level_access(&paths, "180f", 0),
            Some(ServiceAccess::AccessDenied)
        );
        let record = ipc_service_record("180f", 0, service_level_access(&paths, "180f", 0), None);
        let IpcValue::Object(fields) = record else {
            panic!("service record");
        };
        let expected = super::object([
            ("state", string("restricted")),
            ("reason", string("access-denied")),
            ("gattStatus", string("access-denied")),
            ("attError", IpcValue::Null),
        ]);
        assert_eq!(fields.get("restriction"), Some(&expected));
    }

    #[test]
    fn an_open_service_omits_restriction() {
        let record = ipc_service_record("180d", 1, Some(ServiceAccess::Open), None);
        let IpcValue::Object(fields) = record else {
            panic!("service record");
        };
        assert!(!fields.contains_key("restriction"));
        assert_eq!(fields.get("primary"), Some(&IpcValue::Null));
        assert_eq!(fields.get("includedServices"), Some(&IpcValue::Null));
        assert_eq!(fields.get("occurrence"), Some(&string("1")));
    }

    #[test]
    fn os_reserved_restriction_has_no_gatt_status() {
        let record = ipc_service_record("1800", 0, Some(ServiceAccess::OsReserved), None);
        let IpcValue::Object(fields) = record else {
            panic!("service record");
        };
        let restriction = fields.get("restriction").expect("restriction");
        let IpcValue::Object(restriction) = restriction else {
            panic!("restriction object");
        };
        assert_eq!(restriction.get("reason"), Some(&string("os-reserved")));
        assert_eq!(restriction.get("gattStatus"), Some(&IpcValue::Null));
        assert_eq!(restriction.get("attError"), Some(&IpcValue::Null));
    }

    #[test]
    fn service_graph_keeps_observed_secondary_and_inclusion_occurrence() {
        let mut metadata = path("180d", None, Some(ServiceAccess::Open));
        metadata.service_primary = Some(false);
        metadata.included_services = Some(vec![ubm_desktop::boundary::IncludedServiceReference {
            uuid: "180d".to_owned(),
            occurrence: 1,
        }]);
        let record = ipc_service_record("180d", 0, Some(ServiceAccess::Open), Some(&metadata));
        let IpcValue::Object(fields) = record else {
            panic!("service record");
        };
        assert_eq!(fields.get("primary"), Some(&IpcValue::Bool(false)));
        assert_eq!(
            fields.get("includedServices"),
            Some(&IpcValue::Array(vec![super::object([
                ("uuid", string("180d")),
                ("occurrence", string("1")),
            ])]))
        );
    }
}

#[cfg(test)]
#[path = "dispatcher_packet_b_tests.rs"]
mod dispatcher_packet_b_tests;

#[cfg(test)]
mod terminal_evidence_owner_tests {
    use super::*;

    fn owner(caller_key: &str, lease_id: &str, lease_generation: &str) -> TerminalEvidenceOwner {
        TerminalEvidenceOwner {
            producer: "subscription".to_owned(),
            caller_key: caller_key.to_owned(),
            lease_id: lease_id.to_owned(),
            lease_generation: lease_generation.to_owned(),
            recovery_lease_id: terminal_evidence_recovery_lease_id(lease_id, lease_generation),
            reservation_id: "reservation".to_owned(),
            recoverable: false,
        }
    }

    #[tokio::test]
    async fn terminal_evidence_handles_reject_cross_caller_and_cross_lease_access() {
        let dispatcher = BtleplugDispatcher::default();
        let authorized = owner("app\0owner", "lease-1", "generation-1");
        dispatcher
            .inner
            .lock()
            .await
            .terminal_evidence_owners
            .insert("opaque-handle".to_owned(), authorized.clone());

        for unauthorized in [
            owner("app\0other", "lease-1", "generation-1"),
            owner("app\0owner", "lease-2", "generation-2"),
        ] {
            let error = dispatcher
                .terminal_evidence_owner(
                    &unauthorized,
                    "opaque-handle",
                    "tauri.terminal-evidence.read",
                )
                .await
                .expect_err("a leaked handle must stay bound to its caller lease");
            assert_eq!(error.code, BleErrorCode::OwnershipDenied);
        }

        dispatcher
            .terminal_evidence_owner(&authorized, "opaque-handle", "tauri.terminal-evidence.read")
            .await
            .expect("the owning caller and lease retain access");
    }
}

#[cfg(test)]
#[path = "tauri_identity_tests.rs"]
mod tauri_identity_tests;

#[cfg(test)]
#[path = "platform_metadata_transport_522_tests.rs"]
mod platform_metadata_transport_522_tests;

#[cfg(test)]
#[path = "ipc_v9_failure_observation_tests.rs"]
mod ipc_v9_failure_observation_tests;
