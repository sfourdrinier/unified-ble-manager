//! Host-neutral desktop central adapter (HOST-DESKTOP).
//!
//! [`DesktopCentral`] drives the real [`ubm_core::central::Central`] over a
//! [`RadioBoundary`](crate::boundary::RadioBoundary): validation, ownership,
//! generations, and subscription sharing stay in the core; this layer
//! translates radio outcomes into core settlements with contract error
//! identities. Scan cleanup, partial discovery failures, descriptor paths,
//! and cancellation are handled here, never in the radio backend.
//!
//! Operation control (PR210-05/06): every operation takes an
//! [`OpControl`]. Its budget bounds every radio wait of the operation (a
//! named liveness backstop applies only without a caller budget), and its
//! ticket receives the core operation id inside the admission critical
//! section, so [`DesktopCentral::cancel`] targets exactly one core
//! operation, even when the cancel arrives before the id exists. The core
//! lock is never held across a radio await.
//!
//! Lifecycle (PR210-11): connection loss, confirmed release and service
//! changes are published as [`LifecycleEvent`]s on
//! [`DesktopCentral::lifecycle_events`] and to the optional
//! [`CentralProfile::observer`] — one emission point, two consumers.
//!
//! Adapter loss (PR210 finding 57): on a radio that tears down on loss
//! ([`RadioBoundary::tears_down_on_adapter_loss`]), an adapter that powers
//! off, resets, becomes unsupported or unauthorized, is removed, or whose
//! daemon restarts ends every live thing on it — in-flight operations
//! answer `operation.reset`, the owned scan ends aborted, subscriptions
//! invalidate, links release with [`LifecycleKind::AdapterLost`] — and the
//! attachment moves to a new backend and adapter generation, published as
//! one [`AdapterResetEvent`]. Admission (finding 58) refuses before any
//! effect from the adapter facts the radio reported, per the legacy
//! backend of that OS ([`AdmissionPolicy`]).
//!
//! Execution: one shared desktop executor for the process
//! ([`crate::executor`]); this type never builds a runtime. Open the
//! central on that executor ([`crate::executor::desktop_runtime`]): its
//! event loop spawns on the ambient runtime. Physical proof stays queued
//! (see `PARITY_GAPS.md`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{
    Arc, Mutex as StdMutex, MutexGuard, OnceLock, PoisonError,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, broadcast, watch};
use ubm_core::central::{
    Central, CentralEffectKind, CentralResourceCounters, CompletionOutcome, ConnectionState,
    DatabaseState, PathSelector, StoredPath, canonical_uuid, validate_scan_request,
};
use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
    BackendInstanceId, BleErrorCode, BleErrorDomain, CommitState, ContenderKind, CoreError,
    Generation, OperationId, OperationTerminalKind,
};
use ubm_core::ownership::{CleanupRecord, EffectBatch};

use crate::boundary::{
    AdapterAuthorization, AdapterAvailability, AdapterLossCause, AdapterPowerState,
    AdmissionPolicy, CharacteristicAccess, DeliveryMode, InstanceKey, ObservedDelivery,
    PeerSnapshot, RadioBoundary, RadioCloseFailure, RadioEvent, ScanFilterSpec,
};
use ubm_core::central::ScanDuplicatePolicy;

#[path = "central_parity.rs"]
mod parity;
use crate::errors::{DesktopError, Retryability};
use crate::op_control::{
    Budget, COMPENSATION_TIMEOUT, CancelAck, CancelRequest, LIVENESS_BACKSTOP_DETAIL,
    LIVENESS_CLEANUP, LIVENESS_OP, LIVENESS_SCAN_START, OpControl, OpTicket, SettleOnDrop, Window,
};
pub use parity::{
    CancelPairingOutcome, ControllerFuture, PairRequest, PairingGeneration,
    PairingGenerationController, ScanTerminalEvent, SecureConnections, SecurityEvent,
    WriteReadinessEvent, cancel_outcome_for, canonical_address,
};

/// Effect batch capacity per core call (matches the core's own default).
const EFFECT_BATCH_CAP: usize = 64;
/// Compensation bound for background link cleanup (a half-open link after a
/// failed, expired or cancelled connect) and for shutdown link release.
/// Kept tight (1 s): a stuck wait must never stall the already-settled op
/// it cleans up for. Explicit disconnects are bounded by the caller budget
/// or [`LIVENESS_CLEANUP`] instead: BlueZ `Device1.Disconnect` only returns
/// after link teardown, which legitimately exceeds 1 s on real hardware
/// (measured 2.16 s against a wrist band).
pub const DISCONNECT_COMPLETION_TIMEOUT: Duration = COMPENSATION_TIMEOUT;
/// Per-consumer subscription buffer (items, bytes): the public stream
/// maximum, so the central never loses a value the caller's own stream
/// could still hold (finding 107 audit: legacy CoreBluetooth delivered
/// every value straight into the public stream, whose policy was the only
/// bound). It is a bound, not an allocation: the buffer grows only with
/// values the host has not taken yet.
const DEFAULT_SUB_ITEM_CAP: u64 = ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY;
const DEFAULT_SUB_BYTE_CAP: u64 = ubm_core::contracts::MAX_STREAM_BYTE_CAPACITY;
/// ATT protocol ceiling for one write: the attribute-value maximum (512,
/// Core Spec Vol 3 Part F 3.2.9), which a long write reaches. A protocol
/// constant, not a measurement: the OS-measured per-mode limit still gates
/// every write through [`Central::maximum_write_length`].
const ATT_MAX_WRITE: u64 = crate::boundary::ATT_MAX_ATTRIBUTE_VALUE as u64;
/// Bound for the per-central scan-observation queue (F22): every
/// advertisement the event loop ingests stays pollable FIFO until the host
/// takes it. The bound is the public stream maximum (finding 107 audit:
/// legacy CoreBluetooth queued advertisements for the public scan stream
/// with no bound below the caller's policy). Beyond it the oldest
/// observation evicts and every eviction counts through
/// [`DesktopCentral::advertisement_overflow_count`], never silently.
const ADVERTISEMENT_CAP: usize = ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY as usize;
/// Capacity of the lifecycle broadcast. A receiver that falls further
/// behind observes `RecvError::Lagged` (never a silent gap) and must treat
/// its streams as overflowed.
pub const LIFECYCLE_EVENT_CAPACITY: usize = 4096;

static EPOCH: OnceLock<Instant> = OnceLock::new();
static OPEN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Monotonic milliseconds for core calls.
fn now_ms() -> u64 {
    let start = EPOCH.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn batch() -> EffectBatch {
    EffectBatch::new(EFFECT_BATCH_CAP)
}

/// Drain typed observations to recycle ledger capacity (F02). Observations are
/// not hidden: callers needing them (F11) inspect the staged suffix before
/// this drain runs.
fn recycle_observations(core: &mut Central) {
    let _ = core.drain_typed_effects();
}

/// Report the actual host release for a terminal op (F02). Live ops stay live
/// (a release would fail with `live`); already-reaped ops are ignored so the
/// canceller and the op driver may both attempt the report idempotently.
fn report_terminal_release(
    core: &mut Central,
    op: &OperationId,
    ok: bool,
    code: Option<BleErrorCode>,
) {
    let terminal = matches!(
        core.operation_state(op),
        Some(ubm_core::ownership::OpStateView::Terminal(_))
    );
    if !terminal {
        return;
    }
    let _ = if ok {
        core.report_release_success(op)
    } else {
        core.report_release_failure(op, code.unwrap_or(BleErrorCode::PlatformFailure))
    };
}

/// Map an authoritative core terminal to a caller error (F03). The radio
/// result lost the race; the core outcome is the only caller result.
fn terminal_to_error(kind: OperationTerminalKind, operation: &'static str) -> DesktopError {
    match kind {
        OperationTerminalKind::Succeeded => contract_error(
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
            operation,
        )
        .with_detail("core succeeded without a radio value"),
        OperationTerminalKind::Failed => contract_error(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Core,
            operation,
        ),
        OperationTerminalKind::Aborted => DesktopError::cancelled(operation),
        OperationTerminalKind::TimedOut => contract_error(
            BleErrorCode::OperationTimedOut,
            BleErrorDomain::Connection,
            operation,
        ),
        OperationTerminalKind::Disconnected => contract_error(
            BleErrorCode::OperationDisconnected,
            BleErrorDomain::Connection,
            operation,
        ),
        OperationTerminalKind::Reset => contract_error(
            BleErrorCode::OperationReset,
            BleErrorDomain::Connection,
            operation,
        ),
        OperationTerminalKind::AdapterUnavailable => contract_error(
            BleErrorCode::OperationAdapterUnavailable,
            BleErrorDomain::Connection,
            operation,
        ),
        OperationTerminalKind::Destroyed => contract_error(
            BleErrorCode::LifecycleDestroyed,
            BleErrorDomain::Core,
            operation,
        ),
    }
}

/// Observe the current terminal kind for an op already settled (duplicate
/// settlement path). Returns `None` when the op is live or already reaped.
fn terminal_kind_of(core: &Central, op: &OperationId) -> Option<OperationTerminalKind> {
    match core.operation_state(op) {
        Some(ubm_core::ownership::OpStateView::Terminal(kind)) => Some(kind),
        // Shutdown-reaped while this driver still awaited its radio: the
        // tombstone keeps the genuine winner observable (F15).
        _ => core.shutdown_terminal_kind(op),
    }
}

/// Settle one op with a genuine valid contender, recycle observations, and
/// report the actual host release (F02/F25). Genuine radio outcomes are always
/// valid contenders; `invalid` is reserved for stale generations the core must
/// ignore. Returns the authoritative settlement outcome. A fresh settlement
/// releases immediately; a duplicate leaves the terminal in place so the caller
/// can observe the winning kind before reporting the release itself.
fn settle_and_release(
    core: &mut Central,
    op: &OperationId,
    kind: ContenderKind,
    cleanup_ok: bool,
    cleanup_code: Option<BleErrorCode>,
    out: &mut EffectBatch,
) -> Result<CompletionOutcome, DesktopError> {
    let outcome = core
        .settle_op(op, kind, true, 0, now_ms(), out)
        .map_err(DesktopError::from)?;
    let _ = out.drain();
    recycle_observations(core);
    if matches!(outcome, CompletionOutcome::Settled { .. }) {
        report_terminal_release(core, op, cleanup_ok, cleanup_code);
    }
    Ok(outcome)
}

/// Settle one op, then report the duplicate path's release after the caller
/// observes the winning terminal (F03). Returns the pre-release terminal kind.
fn release_duplicate(
    core: &mut Central,
    op: &OperationId,
    cleanup_ok: bool,
    cleanup_code: Option<BleErrorCode>,
) -> Option<OperationTerminalKind> {
    let kind = terminal_kind_of(core, op);
    report_terminal_release(core, op, cleanup_ok, cleanup_code);
    recycle_observations(core);
    kind
}

type ScanTickets = StdMutex<HashMap<OperationId, OperationTerminalKind>>;

fn lock_std<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Retain one scan's completed ticket (R15, first writer wins): call with
/// the core lock held, between settlement and release, so no racing
/// duplicate can observe a settled-but-unretained op. Sync: no await
/// between settle and retain, and none inside.
fn retain_completed_scan(tickets: &ScanTickets, op: &OperationId, kind: OperationTerminalKind) {
    lock_std(tickets).entry(op.clone()).or_insert(kind);
}

/// Retained terminal for a released scan op, if this central completed that
/// scan. Sync; safe under the core lock.
fn completed_scan_kind(tickets: &ScanTickets, op: &OperationId) -> Option<OperationTerminalKind> {
    lock_std(tickets).get(op).copied()
}

/// Which retryability rule an operation's outcome follows (PR210-22).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpKind {
    /// Scan start.
    Scan,
    /// Connect: a dispatched abort/expiry is compensated before returning.
    Connect,
    /// Discovery.
    Discover,
    /// Characteristic or descriptor read.
    Read,
    /// Characteristic or descriptor write: a dispatched write may have
    /// reached the peer, so its outcome is never caller-retryable.
    Write,
    /// Subscribe: a dispatched abort/expiry already failed the enable.
    Subscribe,
    /// Release (scan stop, unsubscribe, disconnect): retained ownership
    /// makes a retry the designed recovery.
    Cleanup,
}

/// Derive commit and retryability from what the central observed, never
/// from the code alone. Only `operation.aborted` / `operation.timed-out`
/// can be caller-retryable: not-dispatched always is; dispatched ones are
/// unless the op is a write, whose commit is then `unknown`. Every other
/// code keeps the default `never`.
fn classify(error: DesktopError, kind: OpKind, dispatched: bool) -> DesktopError {
    if !matches!(
        error.code(),
        BleErrorCode::OperationAborted | BleErrorCode::OperationTimedOut
    ) {
        return error;
    }
    if !dispatched {
        return error.with_outcome(
            Some(CommitState::NotDispatched),
            Retryability::CallerDecides,
        );
    }
    match kind {
        OpKind::Write => error.with_outcome(Some(CommitState::Unknown), Retryability::Never),
        OpKind::Scan
        | OpKind::Connect
        | OpKind::Discover
        | OpKind::Read
        | OpKind::Subscribe
        | OpKind::Cleanup => error.with_outcome(None, Retryability::CallerDecides),
    }
}

/// Finding 41: a write whose radio call was dispatched may have reached the
/// peer whatever code its failure carries (a transport error mid-write, a
/// stale path reported after the OS accepted it). Its commit is `unknown`
/// and it is never caller-retryable; an abort or expiry classifies the same
/// way through [`classify`]. A commit the error already states is kept.
fn classify_dispatched_write(error: DesktopError) -> DesktopError {
    let error = classify(error, OpKind::Write, true);
    if error.commit().is_some() {
        return error;
    }
    error.with_outcome(Some(CommitState::Unknown), Retryability::Never)
}

/// `operation.timed-out` for one window: a backstop expiry says so in its
/// detail, a caller-budget expiry does not.
fn timed_out(operation: &str, window: Window) -> DesktopError {
    let error = contract_error(
        BleErrorCode::OperationTimedOut,
        BleErrorDomain::Connection,
        operation,
    );
    if window.backstop {
        error.with_detail(LIVENESS_BACKSTOP_DETAIL)
    } else {
        error
    }
}

/// How one bounded radio wait ended.
enum Wait<T> {
    /// The radio answered.
    Done(T),
    /// The window's deadline won; the radio future was dropped.
    Expired,
    /// A cancel was requested; the radio future was dropped.
    Cancelled,
}

/// Race one radio call against the operation's cancel request and its
/// window. Cancel wins ties, so a cancel recorded before the call starts
/// never lets the call begin.
async fn drive<T>(ticket: &OpTicket, window: Window, work: impl Future<Output = T>) -> Wait<T> {
    tokio::select! {
        biased;
        () = ticket.cancelled() => Wait::Cancelled,
        outcome = async {
            match window.at {
                Some(at) => tokio::time::timeout_at(at, work).await.ok(),
                None => Some(work.await),
            }
        } => match outcome {
            Some(value) => Wait::Done(value),
            None => Wait::Expired,
        },
    }
}

/// Admission step 2-3 (PR210-05): publish the core id on the ticket inside
/// the admission critical section. A ticket cancelled before admission
/// refuses publication: the op is cancelled in the core (`not-dispatched`)
/// and released before any radio call, and the caller gets
/// `operation.aborted`.
fn publish_or_refuse(
    core: &mut Central,
    ticket: &OpTicket,
    id: &OperationId,
    operation: &'static str,
    scan_tickets: Option<&ScanTickets>,
) -> Result<(), DesktopError> {
    if ticket.publish(id).is_ok() {
        return Ok(());
    }
    let mut out = batch();
    core.cancel_op(id, now_ms(), &mut out)
        .map_err(DesktopError::from)?;
    let _ = out.drain();
    if let Some(tickets) = scan_tickets
        && let Some(kind) = terminal_kind_of(core, id)
    {
        retain_completed_scan(tickets, id, kind);
    }
    report_terminal_release(core, id, true, None);
    recycle_observations(core);
    Err(classify(
        DesktopError::cancelled(operation),
        OpKind::Cleanup,
        false,
    ))
}

/// Per-op detached cleanup for a dropped caller future (F03).
#[derive(Debug)]
enum DropCleanup {
    /// Plain ops (read/write/descriptors): cancel + reap.
    Op,
    /// Connect: also record peer loss + a compensating disconnect (mirrors
    /// the timeout arm, aborted instead of timed out).
    Connect { peer_id: String, peer_key: String },
    /// Subscribe: also fail the shared enable + remove routing + sweep
    /// (mirrors the timeout arm, aborted instead of timed out).
    Subscribe { key: InstanceKey, path_index: usize },
}

/// Cancels + reaps a dispatched op when its awaiting caller future is
/// dropped (task abort, select loss). Without this the core keeps the op
/// dispatched and live forever: the driver that would have settled it is
/// gone. Normal completion defuses the guard; only the drop path fires.
///
/// Abort cleanup runs on the runtime, so `Drop` can spawn the detached
/// task. An unpolled future dropped off-runtime has no in-flight radio to
/// reap, so the guard stays silent there instead of panicking in `Drop`.
struct CancelOnDrop<B: RadioBoundary> {
    central: Option<DesktopCentral<B>>,
    operation: Option<OperationId>,
    cleanup: Option<DropCleanup>,
}

impl<B: RadioBoundary> CancelOnDrop<B> {
    fn armed(central: &DesktopCentral<B>, operation: OperationId, cleanup: DropCleanup) -> Self {
        Self {
            central: Some(central.clone()),
            operation: Some(operation),
            cleanup: Some(cleanup),
        }
    }

    fn defuse(&mut self) {
        self.central.take();
    }
}

impl<B: RadioBoundary> Drop for CancelOnDrop<B> {
    fn drop(&mut self) {
        let (Some(central), Some(operation), Some(cleanup)) = (
            self.central.take(),
            self.operation.take(),
            self.cleanup.take(),
        ) else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(run_drop_cleanup(central, operation, cleanup));
    }
}

/// Report a terminal op's release when the driver is gone (drop path only).
/// Unlike `release_duplicate`, this never reports a live op.
async fn reap_if_terminal<B>(central: &DesktopCentral<B>, operation: &OperationId) {
    let mut core = central.inner.core.lock().await;
    if terminal_kind_of(&core, operation).is_some() {
        report_terminal_release(&mut core, operation, true, None);
        recycle_observations(&mut core);
    }
}

async fn run_drop_cleanup<B: RadioBoundary>(
    central: DesktopCentral<B>,
    operation: OperationId,
    cleanup: DropCleanup,
) {
    match cleanup {
        DropCleanup::Op => {
            let _ = central.cancel_operation(&operation).await;
            reap_if_terminal(&central, &operation).await;
        }
        DropCleanup::Connect { peer_id, peer_key } => {
            {
                let mut core = central.inner.core.lock().await;
                let mut out = batch();
                let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                let _ = out.drain();
            }
            let _ = central.cancel_operation(&operation).await;
            reap_if_terminal(&central, &operation).await;
            // A half-open OS link must not linger ownerless. Bounded and
            // outside the core lock per F24.
            central
                .compensate_half_open(&peer_id, &peer_key, BleErrorCode::OperationAborted)
                .await;
        }
        DropCleanup::Subscribe { key, path_index } => {
            central.inner.subscriptions.lock().await.remove(&key);
            let _ = central.cancel_operation(&operation).await;
            {
                let mut core = central.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_subscribe_enable(path_index, false, now_ms(), &mut out);
                let _ = out.drain();
                sweep_terminal_successes(&mut core);
            }
            reap_if_terminal(&central, &operation).await;
        }
    }
}

/// Release every terminal op whose cleanup already succeeded (F02): joiners
/// settled by a shared enable, immediate-success shares on an enabled hub,
/// and disable tickets settled by `settle_subscribe_disable`. Each has no
/// per-op OS resource beyond the shared CCCD the driver already handled, so
/// success is the truthful receipt. Ops with failed cleanup are reported by
/// their own driver with failure and are already gone from this list.
fn sweep_terminal_successes(core: &mut Central) {
    for id in core.terminal_operation_ids() {
        let _ = core.report_release_success(&id);
    }
    recycle_observations(core);
}

fn contract_error(code: BleErrorCode, domain: BleErrorDomain, operation: &str) -> DesktopError {
    DesktopError::new(code, domain, operation)
}

/// One live scan owned by this central (the core arbitrates one physical
/// scan; a second start fails with `scan.already-active`).
#[derive(Debug, Clone)]
pub struct ScanSession {
    id: OperationId,
}

impl ScanSession {
    /// Core operation id backing this scan.
    #[must_use]
    pub fn operation_id(&self) -> &OperationId {
        &self.id
    }
}

/// Outcome of [`DesktopCentral::stop_scan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStop {
    /// The OS confirmed the stop; the scan is released.
    Stopped,
    /// This central holds no scan under that id (never started here,
    /// already stopped, or another scan owns the radio): no radio call.
    NotActive,
}

/// Outcome of [`DesktopCentral::disconnect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRelease {
    /// The OS confirmed this release.
    Released,
    /// The link had already ended (released or lost) before this call;
    /// no radio call was made.
    AlreadyReleased,
}

/// Handle for one connected peer.
#[derive(Debug, Clone)]
pub struct ConnectionHandle {
    /// Session peer key in the core.
    pub peer_key: String,
    /// Connection generation minted by the core.
    pub connection_generation: Option<String>,
}

/// Check a radio snapshot before it becomes the current database (finding
/// 95): every UUID, then its size against the core's per-database bound. A
/// database is registered whole or not at all. A UUID the OS reported that
/// is not a UUID is `protocol.malformed` for the whole discovery, as the
/// legacy backends answered (BlueZ `canonicalUuid` threw, CoreBluetooth
/// `direct-gatt.gatt.snapshot.*` was `protocol.malformed`); a database past
/// the bound is `capability.limited`.
fn admit_snapshot(
    core: &Central,
    services: &[crate::boundary::ServiceSnapshot],
) -> Result<(), DesktopError> {
    let well_formed = |uuid: &str, level: &str| {
        ubm_core::central::canonical_uuid(uuid)
            .map(|_| ())
            .map_err(|_| {
                contract_error(
                    BleErrorCode::ProtocolMalformed,
                    BleErrorDomain::Gatt,
                    "discovery.snapshot.uuid",
                )
                .with_detail(format!(
                    "the OS reported a {level} UUID {uuid:?} that is not a UUID"
                ))
            })
    };
    let mut entries = 0usize;
    for service in services {
        well_formed(&service.uuid, "service")?;
        entries += 1;
        for characteristic in &service.characteristics {
            well_formed(&characteristic.uuid, "characteristic")?;
            entries += 1;
            for descriptor in &characteristic.descriptors {
                well_formed(&descriptor.uuid, "descriptor")?;
                entries += 1;
            }
        }
    }
    core.admit_database(entries).map_err(|error| {
        DesktopError::from(error).with_detail(format!(
            "the discovered database has {entries} attributes, more than one GATT database holds"
        ))
    })
}

/// The next per-UUID occurrence among siblings, in snapshot order.
fn next_occurrence<'a>(counts: &mut HashMap<&'a str, u64>, uuid: &'a str) -> u64 {
    let next = counts.entry(uuid).or_insert(0);
    let occurrence = *next;
    *next += 1;
    occurrence
}

/// A path of an admitted snapshot the core still refused: the database it
/// was joining is withdrawn (changed, rediscovery required) so no partial
/// snapshot stays current, and the refusal is the discovery's answer.
fn unregistrable(core: &mut Central, peer_key: &str, error: CoreError) -> DesktopError {
    let _ = core.services_changed(peer_key);
    let _ = core.require_rediscovery(peer_key);
    DesktopError::from(error)
}

/// What one discovery registered. The snapshot registers whole or the
/// discovery fails (finding 95): no entry is ever skipped.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryReport {
    /// Paths registered (service, characteristic, and descriptor levels).
    pub paths_registered: usize,
    /// Why characteristic facts beyond the core property bits could not be
    /// read for this discovery, when the radio supports them but the read
    /// failed. The paths still register; their
    /// [`DiscoveredPath::access`] stays `None`. A radio that does not
    /// report such facts at all leaves this `None` too.
    pub access_error: Option<DesktopError>,
}

/// One registered discovery path in the current snapshot (F01). Consumers
/// build [`PathSelector`] values and render databases from this whole-tree
/// read; `discover` alone returns counts only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPath {
    /// Canonical service UUID.
    pub service_uuid: String,
    /// Service occurrence among duplicate UUIDs.
    pub service_occurrence: u64,
    /// Characteristic UUID (`None` = service-level path).
    pub characteristic_uuid: Option<String>,
    /// Characteristic occurrence (`None` = service-level path).
    pub characteristic_occurrence: Option<u64>,
    /// Descriptor UUID (`None` = no descriptor level).
    pub descriptor_uuid: Option<String>,
    /// Descriptor occurrence (`None` = no descriptor level).
    pub descriptor_occurrence: Option<u64>,
    /// GATT property bits (`GATT_PROP_*`, characteristic level only).
    pub properties: u8,
    /// Characteristic facts beyond the core bits (characteristic level
    /// only), when the radio reports them for this platform.
    pub access: Option<CharacteristicAccess>,
}

/// Authoritative per-central shutdown outcome (F14/F15): the final
/// cleanup record plus every close-time release failure. A clean shutdown
/// reports `Released` with no radio failures; anything else names exactly
/// what did not release.
#[derive(Debug)]
pub struct ShutdownReport {
    /// Final core cleanup record, taken only after every queued op settled,
    /// every dispatched remainder was answered, and every terminal release
    /// was acknowledged (F15). `Released` only when disconnect failures and
    /// retained release failures are all absent; otherwise `ReleaseFailed`
    /// with every failure preserved. `Err` only when the destroy drive
    /// itself failed (a core invariant violation), never for radio faults —
    /// those land in the record or in `radio_close_failures`.
    pub record: Result<CleanupRecord, DesktopError>,
    /// Close-time native release failures drained from the radio (F14
    /// receipts): one entry per characteristic scope whose unsubscribe did
    /// not complete. Empty means every live scope released.
    pub radio_close_failures: Vec<RadioCloseFailure>,
    /// Incremental destroy passes executed (F15): more than one when the
    /// destroy workload exceeds one effect batch.
    pub destroy_steps: usize,
    /// The final OS scan stop failure, when the owned scan could not be
    /// stopped during shutdown (PR210-09). `None` when no scan was owned or
    /// the stop succeeded.
    pub scan_stop_failure: Option<DesktopError>,
}

/// Why a notification stream was invalidated (PR210-11), derived from the
/// connection state at poll time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidationCause {
    /// The peer's GATT database changed; rediscover and resubscribe.
    ServicesChanged,
    /// The link ended (lost, released, or releasing).
    LinkEnded,
    /// The adapter was lost under the stream (finding 57).
    AdapterReset,
}

/// Typed notification poll outcome (F17): empty-but-live is distinct from
/// terminal/closed, and exactly one overflow terminal stays observable even
/// when data capacity is exhausted. Values drain before the terminal; the
/// terminal is observed once, then the stream reports closed, never
/// live-empty again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationPoll {
    /// One buffered value (FIFO arrival order).
    Value(Vec<u8>),
    /// No value waiting, stream still live.
    Empty,
    /// Overflow terminal with loss details (exactly once).
    Terminal(ubm_core::central::SubscriptionTerminal),
    /// Hub invalidated: stale, resubscribe after the cause clears.
    Invalidated(InvalidationCause),
    /// Consumer removed/closed or failed terminal already consumed.
    Closed,
}

/// What one lifecycle event reports (PR210-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleKind {
    /// The link ended without a release request (the OS reported the
    /// disconnect, or the host reported peer loss).
    LinkLost,
    /// The link ended; `requested` is true when a disconnect had been
    /// requested (the platform confirmed a release).
    Released { requested: bool },
    /// The peer's GATT database changed: discovered paths and
    /// subscriptions of this connection are stale.
    ServicesChanged,
    /// The link ended because the adapter was lost (finding 57; legacy
    /// `connection-state-changed` with reason `'adapter'`).
    AdapterLost,
}

/// One connection-lifecycle transition, published after the core applied
/// it. `connection_generation` is the generation the transition applied
/// to, read before the transition, so a consumer matches it against the
/// stream it opened for that connection and ignores stale generations.
/// `sequence` increases by one per event of this central.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleEvent {
    pub sequence: u64,
    pub peer_id: String,
    pub peer_key: String,
    pub connection_generation: Option<String>,
    /// The database generation the transition applied to (read before it),
    /// when the peer had a discovered database.
    pub database_generation: Option<String>,
    pub kind: LifecycleKind,
}

/// Connection and database generations of one peer, captured under the
/// core lock before a lifecycle transition.
struct Generations {
    connection: Option<String>,
    database: Option<String>,
}

impl Generations {
    fn of(core: &Central, peer_key: &str) -> Self {
        Self {
            connection: core.connection_generation(peer_key),
            database: core.database_generation(peer_key),
        }
    }
}

/// One signal to a [`CentralProfile::observer`], delivered after every
/// central lock is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CentralSignal {
    /// A scan observation was queued ([`DesktopCentral::take_advertisement`]).
    Advertisement(PeerSnapshot),
    /// A notification value was admitted into a subscription hub for this
    /// characteristic instance.
    Value { scope: InstanceKey, value: Vec<u8> },
    /// The lifecycle event also published on
    /// [`DesktopCentral::lifecycle_events`].
    Lifecycle(LifecycleEvent),
    /// The adapter event also published on
    /// [`DesktopCentral::adapter_events`].
    Adapter(AdapterEvent),
    /// The reset also published on [`DesktopCentral::adapter_reset_events`].
    AdapterReset(AdapterResetEvent),
    /// The link-security change also published on
    /// [`DesktopCentral::security_events`] (finding 118).
    Security(parity::SecurityEvent),
    /// The write-readiness report also published on
    /// [`DesktopCentral::write_readiness_events`] (finding 118).
    WriteReadiness(parity::WriteReadinessEvent),
    /// The OS-ended scan also published on
    /// [`DesktopCentral::scan_terminal_events`] (finding 118).
    ScanTerminal(parity::ScanTerminalEvent),
}

/// One adapter power-state change the OS reported. `sequence` increases by
/// one per adapter event of this central.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterEvent {
    pub sequence: u64,
    pub state: AdapterPowerState,
}

/// One adapter reset (finding 57): what the loss ended, published after
/// the teardown on [`DesktopCentral::adapter_reset_events`]. `previous` and
/// `current` are the attachment before and after: the backend and adapter
/// generations advance, so every handle minted before is foreign now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterResetEvent {
    pub sequence: u64,
    pub cause: AdapterLossCause,
    pub previous: AttachmentTuple,
    pub current: AttachmentTuple,
    /// Live core operations the reset settled `operation.reset`.
    pub cancelled_operations: usize,
    /// The owned scan the reset ended (reported aborted on
    /// [`DesktopCentral::scan_terminal_events`]).
    pub ended_scan: Option<OperationId>,
    /// Radio peer ids whose link the reset released
    /// ([`LifecycleKind::AdapterLost`] each).
    pub released_links: Vec<String>,
    /// Per-instance subscription routes the reset ended.
    pub ended_subscriptions: usize,
    /// OS releases (scan stop, link release) that did not complete. The
    /// adapter took them down with it or kept them; each is named, never
    /// dropped.
    pub release_failures: Vec<DesktopError>,
}

/// The adapter facts admission reads (finding 58), as the radio last
/// reported them. `None` means nothing was reported yet: admission never
/// refuses on a fact it does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterStatus {
    pub power: Option<AdapterPowerState>,
    pub authorization: Option<AdapterAuthorization>,
    pub availability: AdapterAvailability,
    /// Whether an adapter loss is in effect (set by a loss, cleared when the
    /// adapter reports powered on again).
    pub lost: bool,
}

#[derive(Debug, Clone, Copy)]
struct AdapterFacts {
    power: Option<AdapterPowerState>,
    authorization: Option<AdapterAuthorization>,
    removed: bool,
    lost: bool,
}

impl AdapterFacts {
    fn status(self) -> AdapterStatus {
        let availability = if self.removed {
            AdapterAvailability::Unavailable
        } else {
            match self.power {
                None => AdapterAvailability::Unknown,
                Some(AdapterPowerState::Unsupported) => AdapterAvailability::Unsupported,
                Some(_) => AdapterAvailability::Available,
            }
        };
        AdapterStatus {
            power: self.power,
            authorization: self.authorization,
            availability,
            lost: self.lost,
        }
    }
}

/// Whether `operation` passes the adapter gate under `policy`: the
/// operations the legacy backend of that OS gated
/// (`corebluetooth-*` `assertOperational` call sites; WinRT
/// `assertGattUsable` / `assertWinRtAdapterReady` call sites, which also
/// gated unsubscribe).
fn gated(policy: AdmissionPolicy, operation: &str) -> bool {
    const RADIO_WORK: &[&str] = &[
        "scan.start",
        "connection.connect",
        "discovery.complete",
        "gatt.read",
        "gatt.write",
        "gatt.read-descriptor",
        "gatt.write-descriptor",
        "gatt.subscribe",
        "connection.rssi",
        "gatt.write-readiness",
        "gatt.maximum-write-length",
    ];
    match policy {
        AdmissionPolicy::LifecycleOnly => false,
        AdmissionPolicy::CoreBluetooth => RADIO_WORK.contains(&operation),
        AdmissionPolicy::WinRt => {
            RADIO_WORK.contains(&operation) || operation == "gatt.unsubscribe"
        }
    }
}

/// An admission refusal: nothing reached the radio, so the commit is
/// `not-dispatched` and a retry once the adapter is usable is the caller's
/// call.
fn adapter_refusal(code: BleErrorCode, operation: &str, detail: String) -> DesktopError {
    contract_error(code, BleErrorDomain::Adapter, operation)
        .with_detail(detail)
        .with_outcome(
            Some(CommitState::NotDispatched),
            Retryability::CallerDecides,
        )
}

/// The legacy per-OS adapter gate (finding 58), read from the reported
/// facts. A fact never reported admits.
fn admission_refusal(
    policy: AdmissionPolicy,
    status: AdapterStatus,
    operation: &str,
) -> Option<DesktopError> {
    let authorization = || match (status.authorization, status.power) {
        (Some(AdapterAuthorization::Denied), _) | (_, Some(AdapterPowerState::Unauthorized)) => {
            Some(adapter_refusal(
                BleErrorCode::PermissionDenied,
                operation,
                "the OS denies this process the Bluetooth adapter".to_owned(),
            ))
        }
        (Some(AdapterAuthorization::Restricted), _) => Some(adapter_refusal(
            BleErrorCode::PermissionRestricted,
            operation,
            "Bluetooth use is restricted on this host".to_owned(),
        )),
        _ => None,
    };
    let availability = || match status.availability {
        AdapterAvailability::Unavailable | AdapterAvailability::Unsupported => {
            Some(adapter_refusal(
                BleErrorCode::AdapterUnavailable,
                operation,
                format!("the adapter is {}", status.availability.as_str()),
            ))
        }
        AdapterAvailability::Available | AdapterAvailability::Unknown => None,
    };
    let power = || match status.power {
        Some(AdapterPowerState::PoweredOff) => Some(adapter_refusal(
            BleErrorCode::AdapterPoweredOff,
            operation,
            "the adapter is powered off".to_owned(),
        )),
        Some(AdapterPowerState::Resetting) => Some(adapter_refusal(
            BleErrorCode::AdapterResetting,
            operation,
            "the adapter is resetting".to_owned(),
        )),
        Some(AdapterPowerState::Unknown) => Some(adapter_refusal(
            BleErrorCode::AdapterUnavailable,
            operation,
            "the OS has not reported a usable adapter state".to_owned(),
        )),
        Some(
            AdapterPowerState::PoweredOn
            | AdapterPowerState::Unsupported
            | AdapterPowerState::Unauthorized,
        )
        | None => None,
    };
    match policy {
        AdmissionPolicy::LifecycleOnly => None,
        // `assertCoreBluetoothOperational`: authorization (including a
        // pending decision) before availability before power.
        AdmissionPolicy::CoreBluetooth => authorization()
            .or_else(|| {
                (status.authorization == Some(AdapterAuthorization::NotDetermined)).then(|| {
                    adapter_refusal(
                        BleErrorCode::PermissionNotDetermined,
                        operation,
                        "the user has not decided Bluetooth access for this process".to_owned(),
                    )
                })
            })
            .or_else(availability)
            .or_else(power),
        // `assertWinRtAdapterReady`: availability before authorization
        // before power; a pending decision reads the adapter's power as
        // unknown there (legacy `ReadAdapter`), so it is unavailable.
        AdmissionPolicy::WinRt => availability()
            .or_else(authorization)
            .or_else(|| {
                (status.authorization == Some(AdapterAuthorization::NotDetermined)).then(|| {
                    adapter_refusal(
                        BleErrorCode::AdapterUnavailable,
                        operation,
                        "Windows has not granted adapter access".to_owned(),
                    )
                })
            })
            .or_else(power),
    }
}

/// Whether the reported facts make the adapter usable (legacy
/// `isUsableAdapterState` / `winRtAdapterIsReady`): powered on, and the OS
/// has not refused this process.
fn usable(status: AdapterStatus) -> bool {
    status.power == Some(AdapterPowerState::PoweredOn)
        && !matches!(
            status.authorization,
            Some(AdapterAuthorization::Denied | AdapterAuthorization::Restricted)
        )
        && !matches!(
            status.availability,
            AdapterAvailability::Unavailable | AdapterAvailability::Unsupported
        )
}

/// Error detail of a macOS open whose adapter never became usable in time
/// (legacy `adapter-initialization-timed-out`).
pub const ADAPTER_INITIALIZATION_TIMED_OUT: &str = "adapter-initialization-timed-out";

/// Legacy CoreBluetooth first-state bound (`src/node-corebluetooth.ts`
/// `NATIVE_COREBLUETOOTH_INITIALIZATION_TIMEOUT_MILLISECONDS`).
pub const ADAPTER_INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Observer for [`CentralSignal`]s. Called synchronously from the central's
/// tasks with no central lock held; it must not block.
pub type CentralObserver = Arc<dyn Fn(CentralSignal) + Send + Sync>;

/// Identity and wiring for one central ([`DesktopCentral::open_with`]).
#[derive(Clone)]
pub struct CentralProfile {
    /// Attachment owner label (host identity, e.g. `"node"`). Non-empty.
    pub owner: String,
    /// Backend label in the attachment's backend instance id
    /// (`ubm-desktop-{backend_label}-{owner}`). Non-empty.
    pub backend_label: String,
    /// Projects the backend's capability truth into the core at open.
    pub register_capabilities: fn(&mut Central) -> Result<(), CoreError>,
    /// Optional synchronous observer of advertisements, values,
    /// lifecycle and adapter events.
    pub observer: Option<CentralObserver>,
    /// The adapter this central must run on (`Adapter::adapter_info`
    /// label). `None` accepts the boundary's adapter. When set, a boundary
    /// on any other adapter fails the open with `adapter.unavailable`
    /// (`adapter.select`), never opens silently on another adapter.
    pub adapter_id: Option<String>,
    /// The D-Bus bus BlueZ is reached on ([`crate::BluezBus`]; Linux,
    /// production btleplug radio only). A bus this build cannot honour
    /// fails the open with `capability.unsupported`, never falls back.
    pub bluez_bus: crate::boundary::BluezBus,
}

impl CentralProfile {
    /// The desktop btleplug profile: backend label `btleplug`, desktop
    /// capability registration, no observer.
    #[must_use]
    pub fn desktop(owner: &str) -> Self {
        Self {
            owner: owner.to_owned(),
            backend_label: "btleplug".to_owned(),
            register_capabilities: crate::capabilities::register_desktop_capabilities,
            observer: None,
            adapter_id: None,
            bluez_bus: crate::boundary::BluezBus::System,
        }
    }
}

impl std::fmt::Debug for CentralProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CentralProfile")
            .field("owner", &self.owner)
            .field("backend_label", &self.backend_label)
            .field("observer", &self.observer.is_some())
            .field("adapter_id", &self.adapter_id)
            .field("bluez_bus", &self.bluez_bus)
            .finish()
    }
}

/// One known radio peer and its core connection facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecord {
    /// Radio peripheral id.
    pub peer_id: String,
    /// Core session peer key.
    pub peer_key: String,
    /// Core connection state, when a connection record exists.
    pub connection_state: Option<ConnectionState>,
    /// Current connection generation, when a connection record exists.
    pub connection_generation: Option<String>,
    /// Current database generation, when the database is discovered
    /// (current or changed).
    pub database_generation: Option<String>,
    /// Core database state, when a connection record exists.
    pub database_state: Option<DatabaseState>,
}

/// Aggregate resource counts for one central: the core's counts plus what
/// this adapter holds around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceCounters {
    /// The core's own counts.
    pub core: CentralResourceCounters,
    /// Radio peer ids resolved to core peer keys.
    pub radio_peers: usize,
    /// Installed per-instance notification routes.
    pub routed_subscriptions: usize,
    /// Instances whose physical disable failed and awaits a retry.
    pub pending_disables: usize,
    /// Scan observations waiting to be taken.
    pub queued_advertisements: usize,
    /// Scan observations evicted past the queue cap.
    pub advertisement_drops: u64,
    /// Lifecycle events published while nobody observed them.
    pub lifecycle_unobserved: u64,
    /// Background compensations (half-open link release, orphan enable
    /// disable, orphan scan stop) that did not complete.
    pub compensation_failures: u64,
    /// Whether a scan is owned (including one retained after a failed
    /// stop).
    pub scan_owned: bool,
    /// Completed scan tickets retained for late duplicates (R15).
    pub retained_scan_tickets: usize,
    /// Pairing ceremonies in flight.
    pub pairings_in_flight: usize,
    /// Enablements a service change orphaned whose OS release is still
    /// owed (finding 40).
    pub retained_enablements: usize,
    /// Pairing-generation restores that failed after a pairing: the
    /// adapter was left at the held generation.
    pub generation_restore_failures: u64,
    /// Adapter events the OS event broadcast lost before the radio read
    /// them (vendored btleplug patch 10).
    pub radio_events_lost: u64,
    /// Notification values the radio's bounded ingress refused (finding
    /// 131); each is also reported to its subscription as upstream loss.
    pub ingress_notification_drops: u64,
}

type StopAnswer = Option<Result<(), DesktopError>>;

/// Ownership phase of the one scan this central holds (PR210-09). The
/// marker stays until the OS confirms the stop, so a failed stop keeps the
/// scan addressable for a real retry.
#[derive(Debug)]
enum ScanPhase {
    /// OS start in flight.
    Starting,
    /// OS confirmed the start.
    Active,
    /// One stop is in flight; concurrent stops wait for its answer.
    Stopping(watch::Receiver<StopAnswer>),
    /// The last OS stop failed or was abandoned; the kernel op stays live
    /// and the next stop calls the OS again.
    StopFailed,
}

#[derive(Debug)]
struct ActiveScan {
    id: OperationId,
    phase: ScanPhase,
}

/// A resolved characteristic-level path: core path index, radio instance
/// address, and the descriptor address (UUID, occurrence) when the verb
/// addresses a descriptor.
type ResolvedInstance = (usize, InstanceKey, Option<(String, u64)>);

/// Build the per-instance radio address from the resolved stored path
/// (F18). A level the selector leaves unspecified still resolved to
/// exactly one candidate, but that candidate is not necessarily occurrence
/// 0 — a characteristic can live only under the second instance of a
/// duplicated service — so the stored path carries the addressed
/// instance, never the request. `characteristic` is the caller-validated
/// characteristic UUID from that same stored path.
fn instance_key(peer_id: &str, stored: &StoredPath, characteristic: &str) -> InstanceKey {
    (
        peer_id.to_owned(),
        stored.service_uuid().to_owned(),
        stored.service_occurrence(),
        characteristic.to_owned(),
        stored.characteristic_occurrence().unwrap_or(0),
    )
}

struct Inner<B> {
    core: Mutex<Central>,
    boundary: B,
    /// The current attachment; a reset replaces it (finding 57).
    attachment: StdMutex<AttachmentTuple>,
    /// Open ordinal, fixed for the central's lifetime.
    ordinal: u64,
    /// Resets so far; names each new generation.
    resets: AtomicU64,
    /// The adapter facts admission reads (finding 58).
    adapter_facts: StdMutex<AdapterFacts>,
    admission: AdmissionPolicy,
    teardown_on_loss: bool,
    /// Tickets of operations admitted to run, woken with `operation.reset`
    /// when the adapter is lost under them. Settled tickets are pruned.
    tickets: StdMutex<Vec<crate::op_control::OpTicket>>,
    /// Core operations a reset settled `Reset` (the reset replaced the
    /// kernel, so a late driver finds them here).
    reset_ops: StdMutex<HashSet<OperationId>>,
    /// Peer keys whose streams a reset invalidated; cleared by the peer's
    /// next connect.
    reset_peers: StdMutex<HashSet<String>>,
    reset_events: broadcast::Sender<AdapterResetEvent>,
    reset_sequence: AtomicU64,
    /// The one owned scan. A plain mutex: held for short synchronous
    /// updates only, never across an await, and always taken after (never
    /// while waiting for) the core lock.
    scan: StdMutex<Option<ActiveScan>>,
    /// Completed scan tickets retained after release (R15): the kernel op
    /// is reaped by the release report, so a late duplicate completion for
    /// that scan would otherwise miss the forgotten op and report
    /// `argument.invalid` instead of suppressing onto the genuine settled
    /// terminal. First writer wins, matching the kernel's first-settlement
    /// rule. A plain mutex: held for a bare map insert/get with no await,
    /// never across radio work.
    completed_scans: ScanTickets,
    /// Radio peripheral id -> core session peer key.
    peers: Mutex<HashMap<String, String>>,
    /// Per-instance subscription routing: (peer, service uuid, service
    /// occurrence, characteristic uuid, characteristic occurrence) ->
    /// (core path, subscription epoch at install). Duplicate UUIDs never
    /// share routing, and a queued value whose epoch no longer matches the
    /// live routing never delivers (F10).
    subscriptions: Mutex<HashMap<InstanceKey, (usize, u64)>>,
    /// Current subscription epoch per radio peer. Bumped every time the
    /// peer's routing invalidates (disconnect, loss, service change), so
    /// forwarders installed before the bump stamp a dead generation.
    epochs: Mutex<HashMap<String, u64>>,
    /// Per-instance keys whose physical disable failed and is pending
    /// retry through `unsubscribe`. A pending key fails new subscribes
    /// closed until the disable completes.
    failed_disables: Mutex<HashSet<InstanceKey>>,
    /// Delivery mode the radio observed when each live instance was
    /// enabled; joiners are checked against it, never re-enable.
    deliveries: StdMutex<HashMap<InstanceKey, ObservedDelivery>>,
    /// Characteristic facts beyond the core bits from the last discovery,
    /// per instance.
    access: StdMutex<HashMap<InstanceKey, CharacteristicAccess>>,
    /// Physical enablements a service change orphaned (finding 40): the
    /// core paths and routing are gone, but the OS-side CCCD may still be
    /// live. `unsubscribe` releases them by the instance the enable
    /// addressed; a link loss releases them with the link.
    retained_enablements: StdMutex<HashSet<InstanceKey>>,
    /// Link-security changes (pair/unpair results and OS reports).
    security: broadcast::Sender<SecurityEvent>,
    security_sequence: AtomicU64,
    /// In-flight pairings per radio peer: the pairing's own answer, once it
    /// exists, for `cancel_pairing` to read.
    pairings: StdMutex<HashMap<String, watch::Receiver<Option<parity::PairAnswer>>>>,
    /// Pairing-generation restores that failed after a pairing (the adapter
    /// was left at the held generation). Reported, never swallowed.
    generation_restore_failures: AtomicU64,
    /// Write-without-response readiness reports.
    write_readiness: broadcast::Sender<WriteReadinessEvent>,
    write_readiness_sequence: AtomicU64,
    /// Scans the OS ended without a stop request.
    scan_terminal: broadcast::Sender<ScanTerminalEvent>,
    scan_terminal_sequence: AtomicU64,
    /// Scan observations ingested by the event loop, FIFO (F22): the full
    /// [`PeerSnapshot`] facts (name, services, manufacturer data, RSSI)
    /// the host matcher consumes. Bounded by [`ADVERTISEMENT_CAP`]; the
    /// oldest evicts past the cap and every eviction counts in
    /// `advertisement_drops`.
    advertisements: Mutex<VecDeque<QueuedSighting>>,
    /// Observations evicted past [`ADVERTISEMENT_CAP`] (F22): bounded
    /// ingress never grows memory, and drops are counted, never silent.
    advertisement_drops: AtomicU64,
    lifecycle: broadcast::Sender<LifecycleEvent>,
    lifecycle_sequence: AtomicU64,
    lifecycle_unobserved: AtomicU64,
    adapter: broadcast::Sender<AdapterEvent>,
    adapter_sequence: AtomicU64,
    /// Adapter events the OS broadcast lost (vendored btleplug patch 10).
    radio_events_lost: AtomicU64,
    /// Period of the known-peer re-read during a scan, in ms (0: off;
    /// finding 120, the Tauri 4.x cadence).
    known_peer_refresh_ms: AtomicU64,
    /// Known-peer re-reads the OS could not answer. Counted and logged.
    known_peer_refresh_failures: AtomicU64,
    /// Wakes the scan loop when the re-read period changes.
    refresh_changed: tokio::sync::Notify,
    compensation_failures: AtomicU64,
    observer: Option<CentralObserver>,
    shut_down: AtomicBool,
    /// Stop signal for the central-lifetime event loop.
    loop_stop: watch::Sender<bool>,
    /// Event-loop worker, joined at shutdown so no advertisement can race
    /// cleanup after the central is gone.
    loop_done: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl<B> Inner<B> {
    fn scan_slot(&self) -> MutexGuard<'_, Option<ActiveScan>> {
        lock_std(&self.scan)
    }

    /// Publish one lifecycle transition on the broadcast. Call with the
    /// core lock held, right after the transition, so the broadcast order is
    /// the transition order. Deliver the returned event to the observer
    /// with [`Inner::signal`] after the lock drops. An event nobody
    /// receives is counted, never an error.
    fn stage_lifecycle(
        &self,
        peer_id: &str,
        peer_key: &str,
        generation: Generations,
        kind: LifecycleKind,
    ) -> LifecycleEvent {
        let sequence = self.lifecycle_sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let event = LifecycleEvent {
            sequence,
            peer_id: peer_id.to_owned(),
            peer_key: peer_key.to_owned(),
            connection_generation: generation.connection,
            database_generation: generation.database,
            kind,
        };
        let received = self.lifecycle.send(event.clone()).is_ok();
        if !received && self.observer.is_none() {
            self.lifecycle_unobserved.fetch_add(1, Ordering::Relaxed);
        }
        event
    }

    /// Deliver one signal to the observer. Call with no central lock held.
    fn signal(&self, signal: CentralSignal) {
        if let Some(observer) = &self.observer {
            observer(signal);
        }
    }

    fn note_compensation_failure(&self) {
        self.compensation_failures.fetch_add(1, Ordering::Relaxed);
    }
}

/// Host-neutral desktop central: the real core over a mockable radio.
pub struct DesktopCentral<B> {
    inner: Arc<Inner<B>>,
}

/// Clone shares one central (one attachment scope, one scan owner).
impl<B> Clone for DesktopCentral<B> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Role one stop call takes for the owned scan.
enum StopRole {
    Lead(watch::Sender<StopAnswer>),
    Follow(watch::Receiver<StopAnswer>),
    NotActive,
}

/// Leader of one in-flight scan stop. Whatever ends the leader's wait —
/// answer, expiry, cancel, or a dropped future — the marker leaves
/// `Stopping` and every follower gets an answer.
struct StopLead<'a, B> {
    inner: &'a Inner<B>,
    id: OperationId,
    tx: Option<watch::Sender<StopAnswer>>,
}

impl<B> StopLead<'_, B> {
    /// The OS stop did not complete: keep the scan, answer the followers.
    fn fail(&mut self, error: DesktopError) {
        {
            let mut slot = self.inner.scan_slot();
            if let Some(active) = slot.as_mut()
                && active.id == self.id
            {
                active.phase = ScanPhase::StopFailed;
            }
        }
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Some(Err(error)));
        }
    }

    /// The OS confirmed the stop: the marker is already gone.
    fn succeed(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Some(Ok(())));
        }
    }
}

impl<B> Drop for StopLead<'_, B> {
    fn drop(&mut self) {
        if self.tx.is_some() {
            self.fail(DesktopError::scan_stop_failed(
                "stop abandoned before the OS answered",
            ));
        }
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Open a central over `boundary` with the desktop profile and a fresh
    /// attachment scope. `owner` labels the attachment (host identity, e.g.
    /// `"node"`). Same as [`DesktopCentral::open_with`] with
    /// [`CentralProfile::desktop`].
    ///
    /// Must be called on the shared desktop executor: the central-lifetime
    /// event loop spawns on the ambient runtime, and per-manager runtimes
    /// are forbidden.
    pub async fn open(boundary: B, owner: &str) -> Result<Self, DesktopError> {
        Self::open_with(boundary, CentralProfile::desktop(owner)).await
    }

    /// Open a central over `boundary` with an explicit profile (backend
    /// label, capability registration, optional observer). Must be called
    /// on the shared desktop executor (see [`DesktopCentral::open`]).
    pub async fn open_with(boundary: B, profile: CentralProfile) -> Result<Self, DesktopError> {
        // L4: a shut-down executor admits no new centrals — a post-shutdown
        // open fails closed instead of building a zombie central whose ops
        // refuse admission.
        if crate::executor::is_desktop_runtime_shut_down() {
            return Err(contract_error(
                BleErrorCode::AdapterUnavailable,
                BleErrorDomain::Adapter,
                "desktop.open",
            ));
        }
        if profile.owner.is_empty() {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "desktop.owner",
            ));
        }
        if profile.backend_label.is_empty() {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "desktop.backend-label",
            ));
        }
        // L6: never synthesize an adapter identity — a withheld readout
        // fails the open instead of labelling the adapter "unknown".
        let adapter_label = boundary.adapter_name().await?;
        if let Some(wanted) = &profile.adapter_id
            && *wanted != adapter_label
        {
            return Err(
                DesktopError::adapter_unavailable("adapter.select").with_detail(format!(
                    "requested adapter {wanted:?}, boundary is on {adapter_label:?}"
                )),
            );
        }
        let ordinal = OPEN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let attachment = AttachmentTuple::new(
            AttachmentId::new(format!("desktop-attachment-{ordinal}"))
                .map_err(DesktopError::from)?,
            BackendInstanceId::new(format!(
                "ubm-desktop-{}-{}",
                profile.backend_label, profile.owner
            ))
            .map_err(DesktopError::from)?,
            BackendGeneration::new(format!("desktop-backend-gen-{ordinal}"))
                .map_err(DesktopError::from)?,
            AdapterId::new(adapter_label).map_err(DesktopError::from)?,
            AdapterGeneration::new(format!("desktop-adapter-gen-{ordinal}"))
                .map_err(DesktopError::from)?,
        );
        let generation =
            Generation::new(format!("desktop-kernel-gen-{ordinal}")).map_err(DesktopError::from)?;
        let mut core = Central::new(
            attachment.clone(),
            generation,
            ubm_core::central::CentralConfig::default()
                .with_subscribe_property_gate(!boundary.os_answers_unflagged_subscribe()),
        )
        .map_err(DesktopError::from)?;
        // M4: project the backend's capability truth into the live core so
        // runtime gates match the parity report row for row.
        (profile.register_capabilities)(&mut core).map_err(DesktopError::from)?;
        let (loop_stop, loop_stop_rx) = watch::channel(false);
        let (lifecycle, _) = broadcast::channel(LIFECYCLE_EVENT_CAPACITY);
        let (adapter, _) = broadcast::channel(LIFECYCLE_EVENT_CAPACITY);
        let admission = boundary.admission_policy();
        let teardown_on_loss = boundary.tears_down_on_adapter_loss();
        let facts = seed_adapter_facts(&boundary, admission).await;
        let inner = Arc::new(Inner {
            core: Mutex::new(core),
            boundary,
            attachment: StdMutex::new(attachment),
            ordinal,
            resets: AtomicU64::new(0),
            adapter_facts: StdMutex::new(facts),
            admission,
            teardown_on_loss,
            tickets: StdMutex::new(Vec::new()),
            reset_ops: StdMutex::new(HashSet::new()),
            reset_peers: StdMutex::new(HashSet::new()),
            reset_events: broadcast::channel(LIFECYCLE_EVENT_CAPACITY).0,
            reset_sequence: AtomicU64::new(0),
            scan: StdMutex::new(None),
            completed_scans: StdMutex::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            epochs: Mutex::new(HashMap::new()),
            failed_disables: Mutex::new(HashSet::new()),
            deliveries: StdMutex::new(HashMap::new()),
            access: StdMutex::new(HashMap::new()),
            retained_enablements: StdMutex::new(HashSet::new()),
            security: broadcast::channel(LIFECYCLE_EVENT_CAPACITY).0,
            security_sequence: AtomicU64::new(0),
            pairings: StdMutex::new(HashMap::new()),
            generation_restore_failures: AtomicU64::new(0),
            write_readiness: broadcast::channel(LIFECYCLE_EVENT_CAPACITY).0,
            write_readiness_sequence: AtomicU64::new(0),
            scan_terminal: broadcast::channel(LIFECYCLE_EVENT_CAPACITY).0,
            scan_terminal_sequence: AtomicU64::new(0),
            advertisements: Mutex::new(VecDeque::new()),
            advertisement_drops: AtomicU64::new(0),
            lifecycle,
            lifecycle_sequence: AtomicU64::new(0),
            lifecycle_unobserved: AtomicU64::new(0),
            adapter,
            adapter_sequence: AtomicU64::new(0),
            radio_events_lost: AtomicU64::new(0),
            known_peer_refresh_ms: AtomicU64::new(0),
            known_peer_refresh_failures: AtomicU64::new(0),
            refresh_changed: tokio::sync::Notify::new(),
            compensation_failures: AtomicU64::new(0),
            observer: profile.observer,
            shut_down: AtomicBool::new(false),
            loop_stop,
            loop_done: Mutex::new(None),
        });
        let worker = tokio::spawn(scan_loop(Arc::clone(&inner), loop_stop_rx));
        *inner.loop_done.lock().await = Some(worker);
        Ok(Self { inner })
    }

    /// Borrow the radio boundary (event injection in tests runs through the
    /// boundary handle, not the central).
    #[must_use]
    pub fn boundary(&self) -> &B {
        &self.inner.boundary
    }

    /// The current attachment scope: minted at open, replaced by every
    /// adapter reset (finding 57).
    #[must_use]
    pub fn attachment(&self) -> AttachmentTuple {
        lock_std(&self.inner.attachment).clone()
    }

    /// The adapter facts admission reads (finding 58): power,
    /// authorization and availability as the radio last reported them.
    #[must_use]
    pub fn adapter_status(&self) -> AdapterStatus {
        lock_std(&self.inner.adapter_facts).status()
    }

    /// Subscribe to adapter resets (finding 57). Same lag rule as
    /// [`DesktopCentral::lifecycle_events`].
    #[must_use]
    pub fn adapter_reset_events(&self) -> broadcast::Receiver<AdapterResetEvent> {
        self.inner.reset_events.subscribe()
    }

    /// Wait until the adapter is usable — powered on, not refused to this
    /// process, present — for at most `within` (finding 59; the legacy
    /// CoreBluetooth first-state wait). A timeout is
    /// `capability.unavailable` with detail
    /// [`ADAPTER_INITIALIZATION_TIMED_OUT`] (operation
    /// `adapter.initialize`). Answers the usable power state.
    pub async fn await_usable_adapter(
        &self,
        within: Duration,
    ) -> Result<AdapterPowerState, DesktopError> {
        self.admit("adapter.initialize")?;
        let deadline = tokio::time::Instant::now() + within;
        // Subscribe before reading so a change between the read and the
        // wait still wakes it.
        let mut changes = self.inner.adapter.subscribe();
        loop {
            let status = self.adapter_status();
            if usable(status) {
                return Ok(AdapterPowerState::PoweredOn);
            }
            match tokio::time::timeout_at(deadline, changes.recv()).await {
                Ok(Ok(_) | Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                    let status = self.adapter_status();
                    if usable(status) {
                        return Ok(AdapterPowerState::PoweredOn);
                    }
                    return Err(contract_error(
                        BleErrorCode::CapabilityUnavailable,
                        BleErrorDomain::Platform,
                        "adapter.initialize",
                    )
                    .with_detail(format!(
                        "{ADAPTER_INITIALIZATION_TIMED_OUT}: the adapter did not become usable \
                         within {} ms (power {}, authorization {}, availability {})",
                        within.as_millis(),
                        status.power.map_or("unreported", AdapterPowerState::as_str),
                        status
                            .authorization
                            .map_or("unreported", AdapterAuthorization::as_str),
                        status.availability.as_str(),
                    )));
                }
            }
        }
    }

    /// Subscribe to connection-lifecycle events (PR210-11). Each receiver
    /// sees every event published after it subscribed, in transition order;
    /// a receiver more than [`LIFECYCLE_EVENT_CAPACITY`] events behind gets
    /// `RecvError::Lagged` and must treat its streams as overflowed.
    #[must_use]
    pub fn lifecycle_events(&self) -> broadcast::Receiver<LifecycleEvent> {
        self.inner.lifecycle.subscribe()
    }

    /// Subscribe to adapter power-state changes the OS reports. Same lag
    /// rule as [`DesktopCentral::lifecycle_events`].
    #[must_use]
    pub fn adapter_events(&self) -> broadcast::Receiver<AdapterEvent> {
        self.inner.adapter.subscribe()
    }

    /// Current adapter power state, read from the radio under the budget
    /// ([`LIVENESS_OP`] without one). An adapter reset never ends the read:
    /// it answers the post-transition state (finding 94). A radio that
    /// cannot read it answers `capability.unsupported`.
    pub async fn adapter_state(&self, ctl: OpControl) -> Result<AdapterPowerState, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck_adapter_read(&ctl, "adapter.state")?;
        let window = ctl.budget.window(LIVENESS_OP);
        match drive(&ctl.ticket, window, self.inner.boundary.adapter_state()).await {
            Wait::Done(outcome) => outcome,
            Wait::Expired => Err(classify(
                timed_out("adapter.state", window),
                OpKind::Read,
                true,
            )),
            Wait::Cancelled => Err(classify(
                ctl.ticket.interruption("adapter.state"),
                OpKind::Read,
                true,
            )),
        }
    }

    /// RSSI of the live link to `peer_id`, in dBm, for the lease holding
    /// it. Admitted like a read: a foreign lease is `ownership.denied`, a
    /// link that is not connected is `connection.stale` (no record:
    /// `connection.not-found`), both before any radio call; the radio call
    /// runs under the budget ([`LIVENESS_OP`] without one). A radio that
    /// cannot measure RSSI answers `capability.unsupported`.
    pub async fn read_rssi(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
    ) -> Result<i16, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "connection.rssi")?;
        let window = ctl.budget.window(LIVENESS_OP);
        let peer_key = self.known_peer_key(peer_id).await?;
        {
            let core = self.inner.core.lock().await;
            match core.connection_state(&peer_key) {
                None => {
                    return Err(contract_error(
                        BleErrorCode::ConnectionNotFound,
                        BleErrorDomain::Connection,
                        "connection.rssi",
                    ));
                }
                Some(_) if !core.holds_lease(&peer_key, lease) => {
                    return Err(contract_error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Core,
                        "connection.rssi",
                    ));
                }
                Some(ConnectionState::Connected) => {}
                Some(_) => {
                    return Err(contract_error(
                        BleErrorCode::ConnectionStale,
                        BleErrorDomain::Connection,
                        "connection.rssi",
                    ));
                }
            }
        }
        match drive(&ctl.ticket, window, self.inner.boundary.read_rssi(peer_id)).await {
            Wait::Done(outcome) => outcome,
            Wait::Expired => Err(classify(
                timed_out("connection.rssi", window),
                OpKind::Read,
                true,
            )),
            Wait::Cancelled => Err(classify(
                ctl.ticket.interruption("connection.rssi"),
                OpKind::Read,
                true,
            )),
        }
    }

    /// Lifecycle events published while neither a receiver nor an observer
    /// existed. Counted, never an error.
    #[must_use]
    pub fn lifecycle_unobserved_count(&self) -> u64 {
        self.inner.lifecycle_unobserved.load(Ordering::Relaxed)
    }

    /// Keep `ticket` reachable for an adapter reset (finding 57), pruning
    /// tickets whose operations settled.
    fn track(&self, ticket: &crate::op_control::OpTicket) {
        let mut tickets = lock_std(&self.inner.tickets);
        tickets.retain(|tracked| !tracked.is_settled());
        tickets.push(ticket.clone());
    }

    fn admit(&self, operation: &str) -> Result<(), DesktopError> {
        if self.inner.shut_down.load(Ordering::SeqCst)
            || crate::executor::is_desktop_runtime_shut_down()
        {
            return Err(DesktopError::adapter_unavailable(operation));
        }
        Ok(())
    }

    /// Refuse before admission: shutdown, a cancel already requested, or a
    /// budget already spent. Nothing reaches the core or the radio.
    fn precheck(&self, ctl: &OpControl, operation: &'static str) -> Result<(), DesktopError> {
        self.admit(operation)?;
        self.track(&ctl.ticket);
        self.refuse_before_admission(ctl, operation)
    }

    /// [`DesktopCentral::precheck`] for a read of the adapter's own state
    /// (power, authorization). Such a read is not tracked, so an adapter
    /// reset never ends it with `operation.reset`: the reset is the
    /// transition being read, and the read answers the OS's current,
    /// post-transition state, as every legacy host did (finding 94). The
    /// caller's cancel and budget still end it.
    fn precheck_adapter_read(
        &self,
        ctl: &OpControl,
        operation: &'static str,
    ) -> Result<(), DesktopError> {
        self.admit(operation)?;
        self.refuse_before_admission(ctl, operation)
    }

    fn refuse_before_admission(
        &self,
        ctl: &OpControl,
        operation: &'static str,
    ) -> Result<(), DesktopError> {
        if gated(self.inner.admission, operation)
            && let Some(refusal) =
                admission_refusal(self.inner.admission, self.adapter_status(), operation)
        {
            return Err(refusal);
        }
        if ctl.ticket.is_cancel_requested() {
            return Err(classify(
                DesktopError::cancelled(operation),
                OpKind::Cleanup,
                false,
            ));
        }
        if ctl.budget.is_expired() {
            return Err(classify(
                contract_error(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Connection,
                    operation,
                ),
                OpKind::Cleanup,
                false,
            ));
        }
        Ok(())
    }

    /// Settle a dispatched op that hit its end-to-end deadline (F03) and map
    /// the authoritative outcome to the caller error. A duplicate (already
    /// terminal via cancel/disconnect) returns the winning terminal, never a
    /// synthesized timeout.
    async fn settle_timeout(
        &self,
        operation: &OperationId,
        op_name: &'static str,
        window: Window,
    ) -> DesktopError {
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        let outcome = settle_and_release(
            &mut core,
            operation,
            ContenderKind::Timeout,
            true,
            None,
            &mut out,
        );
        match outcome {
            Ok(CompletionOutcome::Settled {
                kind: OperationTerminalKind::TimedOut,
                ..
            })
            | Ok(CompletionOutcome::ContenderIgnored)
            | Err(_) => timed_out(op_name, window),
            Ok(CompletionOutcome::Settled { kind, .. }) => terminal_to_error(kind, op_name),
            Ok(CompletionOutcome::DuplicateSuppressed { .. }) => {
                let kind = release_duplicate(&mut core, operation, true, None);
                match kind {
                    Some(winner) => terminal_to_error(winner, op_name),
                    None => timed_out(op_name, window),
                }
            }
        }
    }

    /// Settle a dispatched op whose caller cancelled it while its radio call
    /// was in flight (PR210-05): the core cancel settles `aborted` unless
    /// another terminal already won, and this driver reports the release.
    async fn settle_abort(&self, operation: &OperationId, op_name: &'static str) -> DesktopError {
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        let outcome = core.cancel_op(operation, now_ms(), &mut out);
        let _ = out.drain();
        let error = match outcome {
            Ok(CompletionOutcome::Settled { kind, .. }) => {
                report_terminal_release(&mut core, operation, true, None);
                terminal_to_error(kind, op_name)
            }
            Ok(CompletionOutcome::DuplicateSuppressed { .. }) => {
                match release_duplicate(&mut core, operation, true, None) {
                    Some(winner) => terminal_to_error(winner, op_name),
                    None => DesktopError::cancelled(op_name),
                }
            }
            Ok(CompletionOutcome::ContenderIgnored) => DesktopError::cancelled(op_name),
            Err(error) if error.code() == BleErrorCode::ArgumentInvalid => {
                // Reaped already (shutdown tombstone or retained scan): the
                // retained winner is the answer.
                match core
                    .shutdown_terminal_kind(operation)
                    .or_else(|| completed_scan_kind(&self.inner.completed_scans, operation))
                    .or_else(|| {
                        lock_std(&self.inner.reset_ops)
                            .contains(operation)
                            .then_some(OperationTerminalKind::Reset)
                    }) {
                    Some(winner) => terminal_to_error(winner, op_name),
                    None => DesktopError::cancelled(op_name),
                }
            }
            Err(error) => DesktopError::from(error),
        };
        recycle_observations(&mut core);
        error
    }

    /// Whether explicit shutdown has been recorded.
    #[must_use]
    pub fn is_shut_down(&self) -> bool {
        self.inner.shut_down.load(Ordering::SeqCst)
    }

    /// Whether a scan is currently owned, including one retained after a
    /// failed stop (PR210-09).
    pub async fn has_active_scan(&self) -> bool {
        self.inner.scan_slot().is_some()
    }

    /// Core operation id of the owned scan, if any (including one retained
    /// after a failed stop).
    #[must_use]
    pub fn active_scan_id(&self) -> Option<OperationId> {
        self.inner
            .scan_slot()
            .as_ref()
            .map(|active| active.id.clone())
    }

    /// Core session peer key for a radio peripheral id, if resolved.
    pub async fn peer_key_for(&self, peer_id: &str) -> Option<String> {
        self.inner.peers.lock().await.get(peer_id).cloned()
    }

    /// Every resolved radio peer with its core connection facts, ordered by
    /// radio peer id.
    pub async fn peer_records(&self) -> Vec<PeerRecord> {
        let peers: Vec<(String, String)> = {
            let peers = self.inner.peers.lock().await;
            peers
                .iter()
                .map(|(peer_id, peer_key)| (peer_id.clone(), peer_key.clone()))
                .collect()
        };
        let core = self.inner.core.lock().await;
        let mut records: Vec<PeerRecord> = peers
            .into_iter()
            .map(|(peer_id, peer_key)| PeerRecord {
                connection_state: core.connection_state(&peer_key),
                connection_generation: core.connection_generation(&peer_key),
                database_generation: core.database_generation(&peer_key),
                database_state: core.database_state(&peer_key),
                peer_id,
                peer_key,
            })
            .collect();
        records.sort_by(|left, right| left.peer_id.cmp(&right.peer_id));
        records
    }

    /// Aggregate resource counts: what the core and this adapter hold now.
    pub async fn resource_counters(&self) -> ResourceCounters {
        let core = self.inner.core.lock().await.resource_counters();
        let radio_peers = self.inner.peers.lock().await.len();
        let routed_subscriptions = self.inner.subscriptions.lock().await.len();
        let pending_disables = self.inner.failed_disables.lock().await.len();
        let queued_advertisements = self.inner.advertisements.lock().await.len();
        ResourceCounters {
            core,
            radio_peers,
            routed_subscriptions,
            pending_disables,
            queued_advertisements,
            advertisement_drops: self.inner.advertisement_drops.load(Ordering::Relaxed),
            lifecycle_unobserved: self.inner.lifecycle_unobserved.load(Ordering::Relaxed),
            compensation_failures: self.inner.compensation_failures.load(Ordering::Relaxed),
            scan_owned: self.inner.scan_slot().is_some(),
            retained_scan_tickets: lock_std(&self.inner.completed_scans).len(),
            pairings_in_flight: lock_std(&self.inner.pairings).len(),
            retained_enablements: lock_std(&self.inner.retained_enablements).len(),
            generation_restore_failures: self
                .inner
                .generation_restore_failures
                .load(Ordering::Relaxed),
            radio_events_lost: self.inner.radio_events_lost.load(Ordering::Relaxed),
            ingress_notification_drops: self.inner.boundary.ingress_notification_drops(),
        }
    }

    /// Take one ingested scan observation, FIFO arrival order (F22): the
    /// full [`PeerSnapshot`] facts (name, services, manufacturer data,
    /// RSSI) the host matcher consumes. `None` means no observation is
    /// waiting. Observations queue only while a scan is live and belong to
    /// that scan (finding 121); see [`DesktopCentral::take_scan_observation`].
    /// Shutdown seals the queue — the event loop is joined, so no new
    /// observation can arrive — and already-queued observations of the live
    /// scan stay drainable, so a racing host never loses the terminal
    /// sighting.
    pub async fn take_advertisement(&self) -> Option<PeerSnapshot> {
        self.take_scan_observation()
            .await
            .map(|observation| observation.snapshot)
    }

    /// Take the oldest observation of the live scan with its scan and age
    /// (finding 121). Sightings of an earlier scan are never delivered to a
    /// later one: they are discarded here, with the scan that owned them.
    pub async fn take_scan_observation(&self) -> Option<ScanObservation> {
        let live = self
            .inner
            .scan_slot()
            .as_ref()
            .map(|active| active.id.clone());
        // Shutdown seals the queue: what the last scan saw stays drainable.
        let sealed = self.inner.shut_down.load(Ordering::SeqCst);
        let mut queue = self.inner.advertisements.lock().await;
        while let Some(sighting) = queue.pop_front() {
            if sealed || live.as_ref() == Some(&sighting.scan) {
                return Some(ScanObservation {
                    age: sighting.received.elapsed(),
                    scan_operation_id: sighting.scan,
                    snapshot: sighting.snapshot,
                });
            }
        }
        None
    }

    /// Re-read every known peripheral each `period` while a scan runs and
    /// report each as an observation of the OS's device state (finding 120:
    /// Tauri 4.x re-read known peripherals every 2 s, so a peer that does
    /// not advertise again, or whose repeats the OS filters, stays visible).
    /// `None` turns it off (the default).
    pub fn set_known_peer_refresh(&self, period: Option<Duration>) {
        let ms = period.map_or(0, |period| {
            u64::try_from(period.as_millis()).unwrap_or(u64::MAX).max(1)
        });
        self.inner.known_peer_refresh_ms.store(ms, Ordering::SeqCst);
        self.inner.refresh_changed.notify_one();
    }

    /// Known-peer re-reads the OS could not answer (finding 120).
    #[must_use]
    pub fn known_peer_refresh_failures(&self) -> u64 {
        self.inner
            .known_peer_refresh_failures
            .load(Ordering::Relaxed)
    }

    /// Observations evicted past the queue cap (F22): a host that polls
    /// slower than the radio sees the loss explicitly here, never as
    /// silent memory growth or silent drops.
    #[must_use]
    pub fn advertisement_overflow_count(&self) -> u64 {
        self.inner.advertisement_drops.load(Ordering::Relaxed)
    }

    /// Current subscription epoch for a radio peer (F10): the generation
    /// a forwarder installed now would capture. Fresh peers start at 0;
    /// every routing invalidation bumps it.
    async fn routing_epoch(&self, peer_id: &str) -> u64 {
        *self
            .inner
            .epochs
            .lock()
            .await
            .entry(peer_id.to_owned())
            .or_insert(0)
    }

    /// Build a validated path selector with canonical UUIDs. Occurrence
    /// disambiguates duplicate UUIDs; UUID alone never identifies a path.
    pub fn selector(
        service_uuid: &str,
        service_occurrence: Option<u64>,
        characteristic_uuid: Option<&str>,
        characteristic_occurrence: Option<u64>,
        descriptor_uuid: Option<&str>,
        descriptor_occurrence: Option<u64>,
    ) -> Result<PathSelector, DesktopError> {
        let characteristic_uuid = characteristic_uuid
            .map(canonical_uuid)
            .transpose()
            .map_err(DesktopError::from)?;
        let descriptor_uuid = descriptor_uuid
            .map(canonical_uuid)
            .transpose()
            .map_err(DesktopError::from)?;
        Ok(PathSelector {
            service_uuid: canonical_uuid(service_uuid).map_err(DesktopError::from)?,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
            descriptor_uuid,
            descriptor_occurrence,
        })
    }

    /// Cancel the operation behind `ticket` (PR210-05). Before admission the
    /// request is recorded and the op ends `operation.aborted` without a
    /// radio call; after admission exactly that one core operation is
    /// cancelled, then its driver wakes; after settlement nothing happens.
    pub async fn cancel(&self, ticket: &OpTicket) -> Result<CancelAck, DesktopError> {
        match ticket.record_cancel() {
            CancelRequest::RecordedBeforeAdmission => {
                ticket.wake();
                Ok(CancelAck::RecordedBeforeAdmission)
            }
            CancelRequest::AlreadySettled => Ok(CancelAck::AlreadySettled),
            CancelRequest::Forward(operation) => {
                // Core first, then wake the driver: the caller's cancel is
                // the contender the core settles.
                let outcome = self.cancel_operation(&operation).await;
                ticket.wake();
                match outcome {
                    Ok(outcome) => Ok(CancelAck::Forwarded { operation, outcome }),
                    // Settled and reaped between publication and this
                    // cancel: nothing live remains under the ticket's id.
                    Err(error) if error.code() == BleErrorCode::ArgumentInvalid => {
                        Ok(CancelAck::AlreadySettled)
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }

    /// Start a scan: validate first (no radio effect on rejection), admit in
    /// the core and publish the op id, then start the OS scan under the
    /// budget ([`LIVENESS_SCAN_START`] without one). A radio failure settles
    /// the core session as failed and releases the scan owner — a failed
    /// start never wedges later scans. An expired or cancelled start stops
    /// the possibly-started OS scan before it returns.
    pub async fn start_scan(
        &self,
        owner: &str,
        service_uuids: &[&str],
        ctl: OpControl,
    ) -> Result<ScanSession, DesktopError> {
        self.start_scan_with(owner, service_uuids, ScanDuplicatePolicy::All, ctl)
            .await
    }

    /// [`DesktopCentral::start_scan`] with the caller's duplicate policy
    /// (finding 63): `All` asks the OS for every advertisement, `First` and
    /// `Merged` ask it to filter repeats (CoreBluetooth `AllowDuplicatesKey`,
    /// BlueZ `DuplicateData`), as the legacy backends did. Merging repeats
    /// into one observation stays the host's job.
    pub async fn start_scan_with(
        &self,
        owner: &str,
        service_uuids: &[&str],
        duplicates: ScanDuplicatePolicy,
        ctl: OpControl,
    ) -> Result<ScanSession, DesktopError> {
        self.start_scan_matching(owner, service_uuids, duplicates, None, ctl)
            .await
    }

    /// [`DesktopCentral::start_scan_with`] plus the caller's local-name
    /// prefix (finding 89), handed to the radio's scan filter so an OS that
    /// can narrow discovery by name does (BlueZ `Pattern`, as the legacy
    /// backend did). It only narrows: matching advertisements by name stays
    /// the host's job. An empty prefix is `argument.invalid` before any
    /// radio effect.
    pub async fn start_scan_matching(
        &self,
        owner: &str,
        service_uuids: &[&str],
        duplicates: ScanDuplicatePolicy,
        name_prefix: Option<&str>,
        ctl: OpControl,
    ) -> Result<ScanSession, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "scan.start")?;
        if name_prefix.is_some_and(str::is_empty) {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Scan,
                "scan.name-prefix",
            ));
        }
        let window = ctl.budget.window(LIVENESS_SCAN_START);
        let request = validate_scan_request(
            service_uuids,
            duplicates.as_str(),
            "none",
            window.core_timeout_ms(),
            true,
            &[],
        )
        .map_err(DesktopError::from)?;
        let filter = ScanFilterSpec {
            service_uuids: request.service_uuids().to_vec(),
            duplicates,
            name_prefix: name_prefix.map(str::to_owned),
        };
        let id = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let id = core
                .start_scan(&request, None, owner, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            publish_or_refuse(
                &mut core,
                &ctl.ticket,
                &id,
                "scan.start",
                Some(&self.inner.completed_scans),
            )?;
            // R14b/R14c: own the scan slot inside the admission critical
            // section, BEFORE the radio await (core arbitration already
            // refused a second live scan, so the slot is ours). A concurrent
            // stop/shutdown then observes — and wins over — this in-flight
            // start instead of seeing an empty slot.
            *self.inner.scan_slot() = Some(ActiveScan {
                id: id.clone(),
                phase: ScanPhase::Starting,
            });
            id
        };
        // Finding 121: a new scan starts from an empty queue; nothing an
        // earlier scan saw is delivered as this scan's observation.
        self.inner.advertisements.lock().await.clear();
        match drive(&ctl.ticket, window, self.inner.boundary.start_scan(filter)).await {
            Wait::Done(Ok(())) => {
                // R14b/R14c: only the verified owner may activate: a stop or
                // shutdown that took over the marker during the OS call
                // wins deterministically, and this start compensates.
                {
                    let mut core = self.inner.core.lock().await;
                    let mut slot = self.inner.scan_slot();
                    let owned = !self.inner.shut_down.load(Ordering::SeqCst)
                        && slot.as_ref().is_some_and(|active| {
                            active.id == id && matches!(active.phase, ScanPhase::Starting)
                        });
                    if owned && let Some(active) = slot.as_mut() {
                        active.phase = ScanPhase::Active;
                        drop(slot);
                        // `start_scan` returning `Ok` is the OS
                        // acknowledgement.
                        let _ = core.platform_scan_started(&id);
                        return Ok(ScanSession { id });
                    }
                }
                self.compensate_lost_start(&id).await;
                Err(classify(
                    DesktopError::cancelled("scan.start"),
                    OpKind::Scan,
                    true,
                ))
            }
            Wait::Done(Err(error)) => {
                self.fail_scan_start(&id).await;
                Err(error)
            }
            Wait::Expired => {
                {
                    let mut core = self.inner.core.lock().await;
                    let mut out = batch();
                    let _ =
                        core.settle_op(&id, ContenderKind::Timeout, true, 0, now_ms(), &mut out);
                    let _ = out.drain();
                }
                self.compensate_lost_start(&id).await;
                Err(classify(
                    timed_out("scan.start", window),
                    OpKind::Scan,
                    true,
                ))
            }
            Wait::Cancelled => {
                let _ = self.cancel_operation(&id).await;
                self.compensate_lost_start(&id).await;
                Err(classify(
                    ctl.ticket.interruption("scan.start"),
                    OpKind::Scan,
                    true,
                ))
            }
        }
    }

    /// The OS refused the start: drop our marker when it is still in the
    /// starting phase (a stop leader owns it otherwise), then fail and
    /// release the core session. The OS scan never started, so no stop.
    async fn fail_scan_start(&self, id: &OperationId) {
        {
            let mut slot = self.inner.scan_slot();
            if slot.as_ref().is_some_and(|active| {
                active.id == *id && matches!(active.phase, ScanPhase::Starting)
            }) {
                *slot = None;
            }
        }
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        let _ = core.note_scan_platform(
            id,
            ubm_core::central::ScanPlatformEvent::StartFailed,
            now_ms(),
            &mut out,
        );
        let _ = out.drain();
        let _ = core.settle_op(id, ContenderKind::Failure, true, 0, now_ms(), &mut out);
        let _ = out.drain();
        // R15: retain the completed ticket between settlement and release
        // (first writer wins), then release.
        if let Some(kind) = terminal_kind_of(&core, id) {
            retain_completed_scan(&self.inner.completed_scans, id, kind);
        }
        report_terminal_release(&mut core, id, true, None);
        recycle_observations(&mut core);
    }

    /// Settle a scan start that lost to a concurrent stop/shutdown, expired,
    /// or was cancelled (R14b/R14c): never activate. When a stop leader
    /// owns our marker, wait (bounded) for its answer first. Then take our
    /// marker when still present, stop the possibly-started OS scan only
    /// when no newer scan owns the radio (bounded; a failure is reported,
    /// never swallowed), and drive our session terminal. A marker retained
    /// after a failed stop stays: the next stop owns that cleanup.
    async fn compensate_lost_start(&self, id: &OperationId) {
        let deadline = tokio::time::Instant::now() + LIVENESS_CLEANUP;
        let radio_free = loop {
            let pending = {
                let mut slot = self.inner.scan_slot();
                match slot
                    .as_ref()
                    .map(|active| (active.id == *id, &active.phase))
                {
                    None => break true,
                    Some((false, _)) => break false,
                    Some((true, ScanPhase::StopFailed)) => {
                        // The failed stop keeps the scan for its retry.
                        return;
                    }
                    Some((true, ScanPhase::Stopping(rx))) => rx.clone(),
                    Some((true, ScanPhase::Starting | ScanPhase::Active)) => {
                        *slot = None;
                        break true;
                    }
                }
            };
            let mut pending = pending;
            let answered =
                tokio::time::timeout_at(deadline, pending.wait_for(Option::is_some)).await;
            if !matches!(answered, Ok(Ok(_))) {
                // The leader still owns the marker; its own bound decides.
                return;
            }
        };
        let (stop_ok, stop_code) = if radio_free {
            match tokio::time::timeout(COMPENSATION_TIMEOUT, self.inner.boundary.stop_scan()).await
            {
                Ok(Ok(())) => (true, None),
                Ok(Err(error)) => {
                    self.inner.note_compensation_failure();
                    (false, Some(error.code()))
                }
                Err(_) => {
                    self.inner.note_compensation_failure();
                    (false, Some(BleErrorCode::OperationTimedOut))
                }
            }
        } else {
            (true, None)
        };
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        let _ = core.stop_scan(id, now_ms(), &mut out);
        let _ = out.drain();
        let _ = core.note_scan_platform(
            id,
            ubm_core::central::ScanPlatformEvent::PlatformStopped,
            now_ms(),
            &mut out,
        );
        let _ = out.drain();
        let _ = core.settle_op(id, ContenderKind::Success, true, 0, now_ms(), &mut out);
        let _ = out.drain();
        // R15: retain the completed ticket between settlement and release
        // (first writer wins), then release with the compensation receipt.
        if let Some(kind) = terminal_kind_of(&core, id) {
            retain_completed_scan(&self.inner.completed_scans, id, kind);
        }
        report_terminal_release(&mut core, id, stop_ok, stop_code);
        recycle_observations(&mut core);
    }

    /// Stop the scan `scan` names (PR210-09). Only that scan: another id
    /// answers [`ScanStop::NotActive`] without a radio call. The scan stays
    /// owned until the OS confirms the stop, bounded by the budget
    /// ([`LIVENESS_CLEANUP`] without one): a failed, expired or cancelled
    /// stop keeps it (`has_active_scan` stays true, the kernel op stays
    /// live) and the next stop calls the OS again. Concurrent stops share
    /// one OS call and its answer. The central-lifetime event loop keeps
    /// running for connection events; it is joined only at
    /// [`DesktopCentral::shutdown`].
    pub async fn stop_scan(
        &self,
        scan: &OperationId,
        ctl: OpControl,
    ) -> Result<ScanStop, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.admit("scan.stop")?;
        self.track(&ctl.ticket);
        let window = ctl.budget.window(LIVENESS_CLEANUP);
        self.stop_scan_with(scan, window, &ctl.ticket).await
    }

    async fn stop_scan_with(
        &self,
        scan: &OperationId,
        window: Window,
        ticket: &OpTicket,
    ) -> Result<ScanStop, DesktopError> {
        let role = {
            let mut slot = self.inner.scan_slot();
            match slot.as_mut() {
                Some(active) if active.id == *scan => match &active.phase {
                    ScanPhase::Stopping(rx) => StopRole::Follow(rx.clone()),
                    ScanPhase::Starting | ScanPhase::Active | ScanPhase::StopFailed => {
                        let (tx, rx) = watch::channel(None);
                        active.phase = ScanPhase::Stopping(rx);
                        StopRole::Lead(tx)
                    }
                },
                _ => StopRole::NotActive,
            }
        };
        match role {
            StopRole::NotActive => Ok(ScanStop::NotActive),
            StopRole::Follow(mut rx) => {
                let answer = drive(ticket, window, async move {
                    rx.wait_for(Option::is_some)
                        .await
                        .map(|answer| answer.clone())
                })
                .await;
                match answer {
                    Wait::Done(Ok(Some(Ok(())))) => Ok(ScanStop::Stopped),
                    Wait::Done(Ok(Some(Err(error)))) => Err(error),
                    Wait::Done(Ok(None) | Err(_)) => Err(DesktopError::scan_stop_failed(
                        "concurrent stop ended without an answer",
                    )),
                    Wait::Expired => Err(classify(
                        timed_out("scan.stop", window),
                        OpKind::Cleanup,
                        true,
                    )),
                    Wait::Cancelled => Err(classify(
                        ticket.interruption("scan.stop"),
                        OpKind::Cleanup,
                        true,
                    )),
                }
            }
            StopRole::Lead(tx) => {
                let mut lead = StopLead {
                    inner: &self.inner,
                    id: scan.clone(),
                    tx: Some(tx),
                };
                // Core first: Active -> Stopping, outside the radio await so
                // a stuck OS stop never holds the core lock. Best-effort: a
                // retried stop or a session already settled via cancel or
                // source-close has no transition left to take.
                {
                    let mut core = self.inner.core.lock().await;
                    let mut out = batch();
                    let _ = core.stop_scan(scan, now_ms(), &mut out);
                    let _ = out.drain();
                }
                match drive(ticket, window, self.inner.boundary.stop_scan()).await {
                    Wait::Done(Ok(())) => {
                        {
                            let mut core = self.inner.core.lock().await;
                            let mut out = batch();
                            let _ = core.note_scan_platform(
                                scan,
                                ubm_core::central::ScanPlatformEvent::PlatformStopped,
                                now_ms(),
                                &mut out,
                            );
                            let _ = out.drain();
                            // `note_scan_platform` already settled the kernel
                            // op for the terminal session; the follow-up
                            // settle is a duplicate that only observes, then
                            // the actual OS-stop success releases.
                            let _ = core.settle_op(
                                scan,
                                ContenderKind::Success,
                                true,
                                0,
                                now_ms(),
                                &mut out,
                            );
                            let _ = out.drain();
                            // R15: retain the completed ticket between
                            // settlement and release, then release.
                            if let Some(kind) = terminal_kind_of(&core, scan) {
                                retain_completed_scan(&self.inner.completed_scans, scan, kind);
                            }
                            report_terminal_release(&mut core, scan, true, None);
                            recycle_observations(&mut core);
                            let mut slot = self.inner.scan_slot();
                            if slot.as_ref().is_some_and(|active| active.id == *scan) {
                                *slot = None;
                            }
                        }
                        lead.succeed();
                        Ok(ScanStop::Stopped)
                    }
                    Wait::Done(Err(error)) => {
                        lead.fail(error.clone());
                        Err(error)
                    }
                    Wait::Expired => {
                        let error = classify(timed_out("scan.stop", window), OpKind::Cleanup, true);
                        lead.fail(error.clone());
                        Err(error)
                    }
                    Wait::Cancelled => {
                        let error =
                            classify(ticket.interruption("scan.stop"), OpKind::Cleanup, true);
                        lead.fail(error.clone());
                        Err(error)
                    }
                }
            }
        }
    }

    /// Release a possibly half-open OS link that no caller owns (failed,
    /// expired, cancelled or lost connect). Bounded by
    /// [`COMPENSATION_TIMEOUT`] and outside the core lock (F24). A failed
    /// release is retained as a disconnect failure on the connection when
    /// its record exists, and always counted — never swallowed.
    async fn compensate_half_open(&self, peer_id: &str, peer_key: &str, code: BleErrorCode) {
        let cleanup = tokio::time::timeout(
            COMPENSATION_TIMEOUT,
            self.inner.boundary.disconnect(peer_id),
        )
        .await;
        let failure = match cleanup {
            Ok(Ok(())) => return,
            Ok(Err(_)) => code,
            Err(_) => BleErrorCode::OperationTimedOut,
        };
        self.inner.note_compensation_failure();
        let mut core = self.inner.core.lock().await;
        let _ = core.report_disconnect_failure(peer_key, failure);
    }

    /// Connect to a radio peer id (btleplug peripheral identity): resolve
    /// the peer, admit the connection and publish its id, dispatch, then
    /// drive the radio under the caller's budget; without one the connect
    /// waits as long as the OS does (finding 112), and a cancel ends it. A
    /// radio failure marks peer loss (Connecting -> Lost, no resurrection)
    /// and settles the op as failed; the radio error takes precedence over
    /// compensation bookkeeping. Every path that does not hand the caller a
    /// handle releases the possibly half-open link.
    pub async fn connect(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
    ) -> Result<ConnectionHandle, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "connection.connect")?;
        // Finding 112: without a caller budget a connect waits as long as
        // the OS does (legacy pending CoreBluetooth connect, Android
        // `autoConnect`); no liveness backstop ends it, a cancel does.
        let window = ctl.budget.window_without_backstop();
        let peer_key = {
            let mut core = self.inner.core.lock().await;
            core.resolve_peer("platform-guid", peer_id)
                .map_err(DesktopError::from)?
        };
        // The peer is known from here regardless of the link outcome, so a
        // failed connect still leaves a peer loss the host can observe.
        self.inner
            .peers
            .lock()
            .await
            .insert(peer_id.to_owned(), peer_key.clone());
        let operation = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let id = core
                .connect(
                    &peer_key,
                    lease,
                    window.core_timeout_ms(),
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            publish_or_refuse(&mut core, &ctl.ticket, &id, "connection.connect", None)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            id
        };
        let mut drop_guard = CancelOnDrop::armed(
            self,
            operation.clone(),
            DropCleanup::Connect {
                peer_id: peer_id.to_owned(),
                peer_key: peer_key.clone(),
            },
        );
        let result = match drive(&ctl.ticket, window, self.inner.boundary.connect(peer_id)).await {
            Wait::Done(Ok(())) => {
                let settled = {
                    let mut core = self.inner.core.lock().await;
                    let mut out = batch();
                    // Best-effort: the event loop may have recorded the
                    // DeviceConnected event first.
                    let _ = core.note_link_established(&peer_key);
                    let outcome = settle_and_release(
                        &mut core,
                        &operation,
                        ContenderKind::Success,
                        true,
                        None,
                        &mut out,
                    )?;
                    let winner = match outcome {
                        CompletionOutcome::Settled { kind, .. } => Some(kind),
                        CompletionOutcome::DuplicateSuppressed { .. } => {
                            release_duplicate(&mut core, &operation, true, None)
                        }
                        CompletionOutcome::ContenderIgnored => None,
                    };
                    match winner {
                        Some(OperationTerminalKind::Succeeded) => {
                            // A new link: streams from before a reset no
                            // longer name that reset as their end.
                            lock_std(&self.inner.reset_peers).remove(&peer_key);
                            Ok(ConnectionHandle {
                                connection_generation: core.connection_generation(&peer_key),
                                peer_key: peer_key.clone(),
                            })
                        }
                        Some(winner) => {
                            // The OS link came up but another terminal won
                            // (cancel, shutdown): nobody holds a handle, so
                            // the link is lost for the core and released
                            // below.
                            let mut out = batch();
                            let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                            Err(terminal_to_error(winner, "connection.connect"))
                        }
                        None => Err(contract_error(
                            BleErrorCode::LifecycleInvalidState,
                            BleErrorDomain::Core,
                            "connection.connect",
                        )),
                    }
                };
                if settled.is_err() {
                    self.compensate_half_open(peer_id, &peer_key, BleErrorCode::OperationAborted)
                        .await;
                }
                settled
            }
            Wait::Done(Err(error)) => {
                // Settle under the lock, then drop the guard before awaiting
                // the compensating radio disconnect (F24): a stuck cleanup
                // must never block unrelated peers behind the core lock.
                let error_code = error.code();
                {
                    let mut core = self.inner.core.lock().await;
                    let mut out = batch();
                    let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                    let _ = out.drain();
                    let _ = settle_and_release(
                        &mut core,
                        &operation,
                        ContenderKind::Failure,
                        true,
                        None,
                        &mut out,
                    );
                }
                // Partial-failure cleanup: a half-opened OS link must not
                // linger without an owner (L5).
                self.compensate_half_open(peer_id, &peer_key, error_code)
                    .await;
                Err(error)
            }
            Wait::Expired => {
                // Deadline won before the radio answered: mark loss, settle
                // as timeout, and clean the half-open link outside the lock.
                {
                    let mut core = self.inner.core.lock().await;
                    let mut out = batch();
                    let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                    let _ = out.drain();
                }
                let error = self
                    .settle_timeout(&operation, "connection.connect", window)
                    .await;
                self.compensate_half_open(peer_id, &peer_key, BleErrorCode::OperationTimedOut)
                    .await;
                Err(error)
            }
            Wait::Cancelled => {
                {
                    let mut core = self.inner.core.lock().await;
                    let mut out = batch();
                    let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                    let _ = out.drain();
                }
                let error = self.settle_abort(&operation, "connection.connect").await;
                self.compensate_half_open(peer_id, &peer_key, BleErrorCode::OperationAborted)
                    .await;
                Err(error)
            }
        };
        drop_guard.defuse();
        result.map_err(|error| classify(error, OpKind::Connect, true))
    }

    /// Explicit disconnect (PR210-09/24): request the release in the core,
    /// then drive the radio under the budget ([`LIVENESS_CLEANUP`] without
    /// one). Only the OS answer releases the link: a failed, expired or
    /// cancelled release keeps it `Disconnecting` with the caller's lease
    /// and records a disconnect failure, and a retry by the same lease
    /// drives the radio again. When the link already ended (released or
    /// lost — e.g. the OS finished a timed-out release, reported as a
    /// `Released` lifecycle event) the retry answers
    /// [`LinkRelease::AlreadyReleased`] without a radio call.
    pub async fn disconnect(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
    ) -> Result<LinkRelease, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "connection.disconnect")?;
        let window = ctl.budget.window(LIVENESS_CLEANUP);
        let peer_key = self.known_peer_key(peer_id).await?;
        {
            let mut core = self.inner.core.lock().await;
            let held = core.holds_lease(&peer_key, lease);
            match core.connection_state(&peer_key) {
                Some(
                    ConnectionState::Disconnected
                    | ConnectionState::Lost
                    | ConnectionState::Invalid,
                ) if held => return Ok(LinkRelease::AlreadyReleased),
                // A retained release: the same lease drives the radio again.
                Some(ConnectionState::Disconnecting) if held => {}
                _ => {
                    let mut out = batch();
                    core.disconnect(&peer_key, lease, now_ms(), &mut out)
                        .map_err(DesktopError::from)?;
                }
            }
        }
        let outcome = drive(&ctl.ticket, window, self.inner.boundary.disconnect(peer_id)).await;
        // Late radio completions must not resurrect the link: drop local
        // subscription routing for this peer now; the core already
        // invalidated its hubs at disconnect.
        self.drop_peer_subscriptions(peer_id).await;
        let (result, event) = {
            let mut core = self.inner.core.lock().await;
            match outcome {
                Wait::Done(Ok(())) => {
                    let generation = Generations::of(&core, &peer_key);
                    if core.connection_state(&peer_key) == Some(ConnectionState::Disconnecting) {
                        core.note_link_released(&peer_key)
                            .map_err(DesktopError::from)?;
                        let event = self.inner.stage_lifecycle(
                            peer_id,
                            &peer_key,
                            generation,
                            LifecycleKind::Released { requested: true },
                        );
                        (Ok(LinkRelease::Released), Some(event))
                    } else {
                        // The event loop recorded the release (or a loss)
                        // first and already published it.
                        (Ok(LinkRelease::Released), None)
                    }
                }
                Wait::Done(Err(error)) => {
                    let _ = core.report_disconnect_failure(&peer_key, error.code());
                    (Err(error), None)
                }
                Wait::Expired => {
                    let _ =
                        core.report_disconnect_failure(&peer_key, BleErrorCode::OperationTimedOut);
                    let error = if window.backstop {
                        timed_out("connection.disconnect", window)
                    } else {
                        timed_out("connection.disconnect", window)
                            .with_detail("disconnect completion deadline exceeded")
                    };
                    (Err(classify(error, OpKind::Cleanup, true)), None)
                }
                Wait::Cancelled => {
                    let _ =
                        core.report_disconnect_failure(&peer_key, BleErrorCode::OperationAborted);
                    (
                        Err(classify(
                            ctl.ticket.interruption("connection.disconnect"),
                            OpKind::Cleanup,
                            true,
                        )),
                        None,
                    )
                }
            }
        };
        if let Some(event) = event {
            self.inner.signal(CentralSignal::Lifecycle(event));
        }
        result
    }

    /// Radio-observed link loss reported by the host: exactly one terminal,
    /// no double release (CLN-02). Publishes a lifecycle event: `LinkLost`,
    /// or `Released { requested: true }` when a release was pending.
    pub async fn remote_peer_loss(&self, peer_id: &str) -> Result<(), DesktopError> {
        let peer_key = self.known_peer_key(peer_id).await?;
        self.drop_peer_subscriptions(peer_id).await;
        let event = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let generation = Generations::of(&core, &peer_key);
            let before = core.connection_state(&peer_key);
            core.note_peer_loss(&peer_key, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            let kind = if before == Some(ConnectionState::Disconnecting) {
                LifecycleKind::Released { requested: true }
            } else {
                LifecycleKind::LinkLost
            };
            self.inner
                .stage_lifecycle(peer_id, &peer_key, generation, kind)
        };
        self.inner.signal(CentralSignal::Lifecycle(event));
        Ok(())
    }

    /// Discover services and register service/characteristic/descriptor
    /// paths with the radio's per-UUID occurrence identity, under the
    /// budget ([`LIVENESS_OP`] without one). The snapshot registers whole
    /// or the discovery fails (finding 95): a database larger than one GATT
    /// database can hold is `capability.limited`, and an empty snapshot
    /// fails instead of completing an empty database. An expired or
    /// cancelled discovery fails the discovery in the core.
    pub async fn discover(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
    ) -> Result<DiscoveryReport, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "discovery.complete")?;
        let window = ctl.budget.window(LIVENESS_OP);
        let peer_key = self.known_peer_key(peer_id).await?;
        {
            let mut core = self.inner.core.lock().await;
            core.begin_discovery(&peer_key)
                .map_err(DesktopError::from)?;
        }
        let services = match drive(&ctl.ticket, window, self.inner.boundary.discover(peer_id)).await
        {
            Wait::Done(Ok(services)) => services,
            Wait::Done(Err(error)) => {
                let mut core = self.inner.core.lock().await;
                let _ = core.fail_discovery(&peer_key);
                return Err(error);
            }
            Wait::Expired => {
                let mut core = self.inner.core.lock().await;
                let _ = core.fail_discovery(&peer_key);
                return Err(classify(
                    timed_out("discovery.complete", window),
                    OpKind::Discover,
                    true,
                ));
            }
            Wait::Cancelled => {
                let mut core = self.inner.core.lock().await;
                let _ = core.fail_discovery(&peer_key);
                return Err(classify(
                    ctl.ticket.interruption("discovery.complete"),
                    OpKind::Discover,
                    true,
                ));
            }
        };
        let mut report = DiscoveryReport::default();
        // Characteristic facts beyond the core bits, read inside the same
        // window. A radio without them answers `capability.unsupported`
        // (no facts, nothing failed); any other failure is reported on the
        // discovery instead of failing it — the paths themselves are good.
        let facts = match drive(
            &ctl.ticket,
            window,
            self.inner.boundary.characteristic_access(peer_id),
        )
        .await
        {
            Wait::Done(Ok(facts)) => facts,
            Wait::Done(Err(error)) => {
                if error.code() != BleErrorCode::CapabilityUnsupported {
                    report.access_error = Some(error);
                }
                HashMap::new()
            }
            Wait::Expired => {
                report.access_error = Some(timed_out("gatt.characteristic-access", window));
                HashMap::new()
            }
            Wait::Cancelled => {
                let mut core = self.inner.core.lock().await;
                let _ = core.fail_discovery(&peer_key);
                return Err(classify(
                    ctl.ticket.interruption("discovery.complete"),
                    OpKind::Discover,
                    true,
                ));
            }
        };
        {
            let mut access = lock_std(&self.inner.access);
            access.retain(|key, _| key.0 != peer_id);
            access.extend(facts);
        }
        {
            let mut core = self.inner.core.lock().await;
            // Finding 95: the whole snapshot is checked before it becomes
            // current — its size against the per-database bound and every
            // UUID — so registration below cannot fail part-way and no
            // partial database is ever current. A refused snapshot fails
            // the discovery as a whole.
            if let Err(error) = admit_snapshot(&core, &services) {
                let _ = core.fail_discovery(&peer_key);
                return Err(error);
            }
            if services.is_empty() {
                let _ = core.fail_discovery(&peer_key);
                return Err(contract_error(
                    BleErrorCode::GattNotFound,
                    BleErrorDomain::Gatt,
                    "discovery.complete",
                )
                .with_detail("no GATT services in the radio snapshot"));
            }
            // The core registers paths against a Current database: the
            // radio snapshot completing is what advances Discovering to
            // Current; entries then register one by one underneath it.
            core.complete_discovery(&peer_key)
                .map_err(DesktopError::from)?;
            // Occurrences count per UUID in snapshot order, as before: the
            // radio's order is the discovery order (finding 96).
            let mut service_counts: HashMap<&str, u64> = HashMap::new();
            for service in &services {
                let service_occurrence = next_occurrence(&mut service_counts, &service.uuid);
                core.register_path(
                    &peer_key,
                    &service.uuid,
                    service_occurrence,
                    None,
                    None,
                    None,
                    None,
                    0,
                    lease,
                )
                .map_err(|error| unregistrable(&mut core, &peer_key, error))?;
                report.paths_registered += 1;
                let mut char_counts: HashMap<&str, u64> = HashMap::new();
                for characteristic in &service.characteristics {
                    let char_occurrence = next_occurrence(&mut char_counts, &characteristic.uuid);
                    core.register_path(
                        &peer_key,
                        &service.uuid,
                        service_occurrence,
                        Some(&characteristic.uuid),
                        Some(char_occurrence),
                        None,
                        None,
                        characteristic.properties.core_bits(),
                        lease,
                    )
                    .map_err(|error| unregistrable(&mut core, &peer_key, error))?;
                    report.paths_registered += 1;
                    let mut desc_counts: HashMap<&str, u64> = HashMap::new();
                    for descriptor in &characteristic.descriptors {
                        // Descriptor values travel explicit descriptor
                        // operations; the CCCD stays managed by
                        // subscribe/unsubscribe (core rejects direct CCCD
                        // writes with `gatt.cccd-managed`).
                        core.register_path(
                            &peer_key,
                            &service.uuid,
                            service_occurrence,
                            Some(&characteristic.uuid),
                            Some(char_occurrence),
                            Some(&descriptor.uuid),
                            Some(next_occurrence(&mut desc_counts, &descriptor.uuid)),
                            ubm_core::central::GATT_PROP_READ | ubm_core::central::GATT_PROP_WRITE,
                            lease,
                        )
                        .map_err(|error| unregistrable(&mut core, &peer_key, error))?;
                        report.paths_registered += 1;
                    }
                }
            }
        }
        Ok(report)
    }

    /// Read the peer's current discovery tree in registration order (F01).
    /// Pure read like [`Self::peer_key_for`]: no admission, no radio. An
    /// unknown peer fails with `peer.not-found`; a peer off-`current`
    /// fails with `gatt.discovery-required` (or `gatt.stale-handle` after
    /// a service change) — never an empty list mistaken for an empty
    /// database.
    pub async fn discovered_paths(
        &self,
        peer_id: &str,
    ) -> Result<Vec<DiscoveredPath>, DesktopError> {
        let peer_key = self.known_peer_key(peer_id).await?;
        let core = self.inner.core.lock().await;
        let stored = core.snapshot_paths(&peer_key).map_err(DesktopError::from)?;
        let access = lock_std(&self.inner.access);
        Ok(stored
            .iter()
            .map(|path| DiscoveredPath {
                service_uuid: path.service_uuid().to_owned(),
                service_occurrence: path.service_occurrence(),
                characteristic_uuid: path.characteristic_uuid().map(str::to_owned),
                characteristic_occurrence: path.characteristic_occurrence(),
                descriptor_uuid: path.descriptor_uuid().map(str::to_owned),
                descriptor_occurrence: path.descriptor_occurrence(),
                properties: path.properties(),
                access: path.characteristic_uuid().and_then(|characteristic| {
                    access
                        .get(&instance_key(peer_id, path, characteristic))
                        .copied()
                }),
            })
            .collect())
    }

    /// Settle a dispatched GATT verb whose radio call returned `Ok` (F03):
    /// the core winner is the caller result; a stale-handle cause reports
    /// `gatt.stale-handle`.
    fn settle_gatt_success<T>(
        core: &mut Central,
        operation: &OperationId,
        value: T,
        op_name: &'static str,
    ) -> Result<T, DesktopError> {
        let mut out = batch();
        let outcome = settle_and_release(
            core,
            operation,
            ContenderKind::Success,
            true,
            None,
            &mut out,
        )?;
        match outcome {
            CompletionOutcome::Settled {
                kind: OperationTerminalKind::Succeeded,
                ..
            } => Ok(value),
            CompletionOutcome::Settled { kind, cause, .. } => {
                if cause == Some(BleErrorCode::GattStaleHandle) {
                    Err(contract_error(
                        BleErrorCode::GattStaleHandle,
                        BleErrorDomain::Gatt,
                        op_name,
                    ))
                } else {
                    Err(terminal_to_error(kind, op_name))
                }
            }
            CompletionOutcome::DuplicateSuppressed { .. } => {
                match release_duplicate(core, operation, true, None) {
                    Some(OperationTerminalKind::Succeeded) => Ok(value),
                    Some(winner) => Err(terminal_to_error(winner, op_name)),
                    None => Err(DesktopError::cancelled(op_name)),
                }
            }
            CompletionOutcome::ContenderIgnored => Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                op_name,
            )),
        }
    }

    /// Settle a dispatched GATT verb whose radio call failed: the radio
    /// error stands unless another terminal already won.
    fn settle_gatt_failure(
        core: &mut Central,
        operation: &OperationId,
        error: DesktopError,
        op_name: &'static str,
    ) -> DesktopError {
        let mut out = batch();
        let outcome = match settle_and_release(
            core,
            operation,
            ContenderKind::Failure,
            true,
            None,
            &mut out,
        ) {
            Ok(outcome) => outcome,
            Err(settle_error) => return settle_error,
        };
        match outcome {
            CompletionOutcome::Settled { .. } | CompletionOutcome::ContenderIgnored => error,
            CompletionOutcome::DuplicateSuppressed { .. } => {
                match release_duplicate(core, operation, true, None) {
                    Some(OperationTerminalKind::Failed) | None => error,
                    Some(winner) => terminal_to_error(winner, op_name),
                }
            }
        }
    }

    /// Resolve a characteristic-level path (and its descriptor, when the
    /// verb addresses one) under the core lock.
    fn resolve_instance(
        core: &Central,
        peer_key: &str,
        peer_id: &str,
        selector: &PathSelector,
        op_name: &'static str,
        descriptor: bool,
    ) -> Result<ResolvedInstance, DesktopError> {
        let index = core
            .resolve_path(peer_key, selector)
            .map_err(DesktopError::from)?;
        let stored = core.stored_path(index).cloned().ok_or_else(|| {
            if descriptor {
                contract_error(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    "path.index",
                )
            } else {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    op_name,
                )
            }
        })?;
        let characteristic = stored
            .characteristic_uuid()
            .map(str::to_owned)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    op_name,
                )
            })?;
        let descriptor_address = if descriptor {
            let uuid = stored.descriptor_uuid().map(str::to_owned).ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    op_name,
                )
            })?;
            Some((uuid, stored.descriptor_occurrence().unwrap_or(0)))
        } else {
            None
        };
        Ok((
            index,
            instance_key(peer_id, &stored, &characteristic),
            descriptor_address,
        ))
    }

    /// GATT read through a validated path: freshness, discovery, lease, and
    /// property checks run before kernel admission, so a stale path never
    /// dispatches to the radio. Bounded by the budget ([`LIVENESS_OP`]
    /// without one); a cancel settles `aborted` in the core.
    pub async fn read(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        ctl: OpControl,
    ) -> Result<Vec<u8>, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "gatt.read")?;
        let window = ctl.budget.window(LIVENESS_OP);
        let peer_key = self.known_peer_key(peer_id).await?;
        let (operation, key) = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let (index, key, _) =
                Self::resolve_instance(&core, &peer_key, peer_id, selector, "gatt.read", false)?;
            let id = core
                .start_read(index, window.core_timeout_ms(), now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            publish_or_refuse(&mut core, &ctl.ticket, &id, "gatt.read", None)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            (id, key)
        };
        let mut drop_guard = CancelOnDrop::armed(self, operation.clone(), DropCleanup::Op);
        // F03: the budget is an end-to-end deadline for dispatched work.
        // `expire_sweep` only covers queued ops, so dispatched reads race the
        // radio against their own deadline; the winning core outcome is the
        // only caller result.
        let result = match drive(
            &ctl.ticket,
            window,
            self.inner
                .boundary
                .read_characteristic(peer_id, &key.1, key.2, &key.3, key.4),
        )
        .await
        {
            Wait::Done(Ok(bytes)) => {
                let mut core = self.inner.core.lock().await;
                // F03: a link that died mid-read wins over the late radio
                // bytes. Generations alone cannot catch this (disconnect keeps
                // them), so the live link state competes explicitly.
                let link_live = matches!(
                    core.connection_state(&peer_key),
                    Some(ConnectionState::Connected)
                );
                if link_live {
                    Self::settle_gatt_success(&mut core, &operation, bytes, "gatt.read")
                } else {
                    let mut out = batch();
                    let _ = settle_and_release(
                        &mut core,
                        &operation,
                        ContenderKind::Disconnect,
                        true,
                        None,
                        &mut out,
                    );
                    Err(contract_error(
                        BleErrorCode::OperationDisconnected,
                        BleErrorDomain::Connection,
                        "gatt.read",
                    ))
                }
            }
            Wait::Done(Err(error)) => {
                let mut core = self.inner.core.lock().await;
                Err(Self::settle_gatt_failure(
                    &mut core,
                    &operation,
                    error,
                    "gatt.read",
                ))
            }
            Wait::Expired => Err(self.settle_timeout(&operation, "gatt.read", window).await),
            Wait::Cancelled => Err(self.settle_abort(&operation, "gatt.read").await),
        };
        drop_guard.defuse();
        result.map_err(|error| classify(error, OpKind::Read, true))
    }

    /// Measure the OS single-write limit for one mode inside the operation
    /// window (M1 + F03): before the core lock (never stalls the loop), but
    /// never outside the deadline. An expiry or cancel here happens before
    /// admission. `None` is the OS withholding it.
    async fn measured_write_limit(
        &self,
        peer_id: &str,
        with_response: bool,
        ticket: &OpTicket,
        window: Window,
        op_name: &'static str,
    ) -> Result<Option<u16>, DesktopError> {
        match drive(ticket, window, self.inner.boundary.write_limits(peer_id)).await {
            Wait::Done(limits) => Ok(limits.map(|limits| limits.for_mode(with_response))),
            Wait::Expired => Err(classify(timed_out(op_name, window), OpKind::Write, false)),
            Wait::Cancelled => Err(classify(ticket.interruption(op_name), OpKind::Write, false)),
        }
    }

    /// GATT write. `"long-write"` is rejected up front: prepared-write
    /// transactions have no btleplug radio path (see `PARITY_GAPS.md`),
    /// and a long value must never silently degrade to a single ATT write.
    /// A write that expires or is cancelled after dispatch may have reached
    /// the peer: its error carries commit `unknown` and is never
    /// caller-retryable (PR210-22).
    pub async fn write(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        value: Vec<u8>,
        mode: &str,
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "gatt.write")?;
        if mode == "long-write" {
            return Err(contract_error(
                BleErrorCode::CapabilityLimited,
                BleErrorDomain::Capability,
                "gatt.write",
            )
            .with_detail("long-write needs a prepared-write radio path"));
        }
        let with_response = mode == "with-response";
        let value_len = value.len() as u64;
        let window = ctl.budget.window(LIVENESS_OP);
        let measured_limit = self
            .measured_write_limit(peer_id, with_response, &ctl.ticket, window, "gatt.write")
            .await?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let (operation, key) = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let (index, key, _) =
                Self::resolve_instance(&core, &peer_key, peer_id, selector, "gatt.write", false)?;
            let maximum = Self::write_maximum(&core, measured_limit, "gatt.write")?;
            let id = core
                .start_write(
                    index,
                    mode,
                    value_len,
                    Some(maximum),
                    true,
                    window.core_timeout_ms(),
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            publish_or_refuse(&mut core, &ctl.ticket, &id, "gatt.write", None)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            (id, key)
        };
        let mut drop_guard = CancelOnDrop::armed(self, operation.clone(), DropCleanup::Op);
        let result = match drive(
            &ctl.ticket,
            window,
            self.inner.boundary.write_characteristic(
                peer_id,
                &key.1,
                key.2,
                &key.3,
                key.4,
                value,
                with_response,
            ),
        )
        .await
        {
            Wait::Done(Ok(())) => {
                let mut core = self.inner.core.lock().await;
                Self::settle_gatt_success(&mut core, &operation, (), "gatt.write")
            }
            Wait::Done(Err(error)) => {
                let mut core = self.inner.core.lock().await;
                Err(Self::settle_gatt_failure(
                    &mut core,
                    &operation,
                    error,
                    "gatt.write",
                ))
            }
            Wait::Expired => Err(self.settle_timeout(&operation, "gatt.write", window).await),
            Wait::Cancelled => Err(self.settle_abort(&operation, "gatt.write").await),
        };
        drop_guard.defuse();
        result.map_err(classify_dispatched_write)
    }

    /// Descriptor read through a validated descriptor path.
    pub async fn read_descriptor(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        ctl: OpControl,
    ) -> Result<Vec<u8>, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "gatt.read-descriptor")?;
        let window = ctl.budget.window(LIVENESS_OP);
        let peer_key = self.known_peer_key(peer_id).await?;
        let (operation, key, descriptor, descriptor_occurrence) = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let (index, key, descriptor) = Self::resolve_instance(
                &core,
                &peer_key,
                peer_id,
                selector,
                "gatt.read-descriptor",
                true,
            )?;
            let (descriptor, descriptor_occurrence) = descriptor.ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    "gatt.read-descriptor",
                )
            })?;
            let id = core
                .start_read_descriptor(index, window.core_timeout_ms(), now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            publish_or_refuse(&mut core, &ctl.ticket, &id, "gatt.read-descriptor", None)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            (id, key, descriptor, descriptor_occurrence)
        };
        let mut drop_guard = CancelOnDrop::armed(self, operation.clone(), DropCleanup::Op);
        let result = match drive(
            &ctl.ticket,
            window,
            self.inner.boundary.read_descriptor(
                peer_id,
                &key.1,
                key.2,
                &key.3,
                key.4,
                &descriptor,
                descriptor_occurrence,
            ),
        )
        .await
        {
            Wait::Done(Ok(bytes)) => {
                let mut core = self.inner.core.lock().await;
                Self::settle_gatt_success(&mut core, &operation, bytes, "gatt.read-descriptor")
            }
            Wait::Done(Err(error)) => {
                let mut core = self.inner.core.lock().await;
                Err(Self::settle_gatt_failure(
                    &mut core,
                    &operation,
                    error,
                    "gatt.read-descriptor",
                ))
            }
            Wait::Expired => Err(self
                .settle_timeout(&operation, "gatt.read-descriptor", window)
                .await),
            Wait::Cancelled => Err(self.settle_abort(&operation, "gatt.read-descriptor").await),
        };
        drop_guard.defuse();
        result.map_err(|error| classify(error, OpKind::Read, true))
    }

    /// Descriptor write through a validated descriptor path. Direct CCCD
    /// writes fail closed in the core with `gatt.cccd-managed`: sharing
    /// rules stay with subscribe/unsubscribe. Commit rules follow
    /// [`DesktopCentral::write`].
    pub async fn write_descriptor(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        value: Vec<u8>,
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "gatt.write-descriptor")?;
        let value_len = value.len() as u64;
        let window = ctl.budget.window(LIVENESS_OP);
        // Descriptor writes are always ATT write requests (with response).
        let measured_limit = self
            .measured_write_limit(peer_id, true, &ctl.ticket, window, "gatt.write-descriptor")
            .await?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let (operation, key, descriptor, descriptor_occurrence) = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let (index, key, descriptor) = Self::resolve_instance(
                &core,
                &peer_key,
                peer_id,
                selector,
                "gatt.write-descriptor",
                true,
            )?;
            let (descriptor, descriptor_occurrence) = descriptor.ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    "gatt.write-descriptor",
                )
            })?;
            let maximum = Self::write_maximum(&core, measured_limit, "gatt.write-descriptor")?;
            let id = core
                .start_write_descriptor(
                    index,
                    value_len,
                    Some(maximum),
                    window.core_timeout_ms(),
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            publish_or_refuse(&mut core, &ctl.ticket, &id, "gatt.write-descriptor", None)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            (id, key, descriptor, descriptor_occurrence)
        };
        let mut drop_guard = CancelOnDrop::armed(self, operation.clone(), DropCleanup::Op);
        let result = match drive(
            &ctl.ticket,
            window,
            self.inner.boundary.write_descriptor(
                peer_id,
                &key.1,
                key.2,
                &key.3,
                key.4,
                &descriptor,
                descriptor_occurrence,
                value,
            ),
        )
        .await
        {
            Wait::Done(Ok(())) => {
                let mut core = self.inner.core.lock().await;
                Self::settle_gatt_success(&mut core, &operation, (), "gatt.write-descriptor")
            }
            Wait::Done(Err(error)) => {
                let mut core = self.inner.core.lock().await;
                Err(Self::settle_gatt_failure(
                    &mut core,
                    &operation,
                    error,
                    "gatt.write-descriptor",
                ))
            }
            Wait::Expired => Err(self
                .settle_timeout(&operation, "gatt.write-descriptor", window)
                .await),
            Wait::Cancelled => Err(self.settle_abort(&operation, "gatt.write-descriptor").await),
        };
        drop_guard.defuse();
        result.map_err(classify_dispatched_write)
    }

    /// `capability.limited` for a delivery requirement this subscription
    /// cannot prove it enforces.
    fn delivery_unenforced(
        required: DeliveryMode,
        observed: ObservedDelivery,
        why: &str,
    ) -> DesktopError {
        contract_error(
            BleErrorCode::CapabilityLimited,
            BleErrorDomain::Capability,
            "gatt.subscribe.delivery",
        )
        .with_detail(format!(
            "{} required, {} observed: {why}",
            required.as_str(),
            observed.as_str()
        ))
    }

    /// Subscribe one consumer: admit in the core and publish the op id,
    /// route early values through the hub (pre-ready values quarantine per
    /// GATT-04), then enable the physical CCCD under the budget
    /// ([`LIVENESS_OP`] without one). A radio failure settles the
    /// enablement as failed and removes routing — a failed subscribe never
    /// leaves a live CCCD.
    ///
    /// `delivery` carries a hard delivery requirement to the radio, which
    /// writes that CCCD mode or refuses before any effect; the returned
    /// [`ObservedDelivery`] is what the radio reported (`Unknown` when the
    /// platform does not say). A consumer joining an already-enabled (or
    /// enabling) CCCD never rewrites it: its requirement must match what
    /// the enabler observed, else it is refused with `capability.limited`
    /// and no effect.
    pub async fn subscribe(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
        delivery: Option<DeliveryMode>,
        ctl: OpControl,
    ) -> Result<ObservedDelivery, DesktopError> {
        self.subscribe_buffered(
            peer_id,
            selector,
            consumer,
            delivery,
            ubm_core::streams::OverflowPolicy::Error,
            (DEFAULT_SUB_ITEM_CAP, DEFAULT_SUB_BYTE_CAP),
            ctl,
        )
        .await
    }

    /// [`DesktopCentral::subscribe`] with the consumer's own overflow policy
    /// (finding 131): a loss the OS or the ingress reports is applied by it,
    /// `error` ending the stream with an overflow terminal, the lossy
    /// policies counting it ([`DesktopCentral::consumer_counters`]) while
    /// the stream continues, as each legacy consumer's own stream did.
    pub async fn subscribe_with_policy(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
        delivery: Option<DeliveryMode>,
        policy: ubm_core::streams::OverflowPolicy,
        ctl: OpControl,
    ) -> Result<ObservedDelivery, DesktopError> {
        self.subscribe_buffered(
            peer_id,
            selector,
            consumer,
            delivery,
            policy,
            (DEFAULT_SUB_ITEM_CAP, DEFAULT_SUB_BYTE_CAP),
            ctl,
        )
        .await
    }

    /// One consumer's stream counters (finding 131): items dropped, bytes
    /// dropped, items replaced, upstream (OS broadcast and ingress) loss,
    /// and whether an `error` overflow ended it. Readable on every poll,
    /// including after the stream ended, until the subscription is gone.
    pub async fn consumer_counters(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
    ) -> Result<Option<ubm_core::streams::StreamAccounting>, DesktopError> {
        self.admit("gatt.consumer-counters")?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let core = self.inner.core.lock().await;
        let index = match core.consumer_path(&peer_key, selector, consumer) {
            Some(index) => index,
            None => core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?,
        };
        Ok(core.consumer_accounting(index, consumer))
    }

    /// [`DesktopCentral::subscribe`] with an explicit overflow policy and
    /// per-consumer buffer `(items, bytes)`: the overflow rules are the same
    /// at any bound, so tests prove them at a small one.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn subscribe_buffered(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
        delivery: Option<DeliveryMode>,
        policy: ubm_core::streams::OverflowPolicy,
        (item_capacity, byte_capacity): (u64, u64),
        ctl: OpControl,
    ) -> Result<ObservedDelivery, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "gatt.subscribe")?;
        let window = ctl.budget.window(LIVENESS_OP);
        let peer_key = self.known_peer_key(peer_id).await?;
        // Resolve the instance first (pure read, no side effects) so the
        // pending-disable check below never nests locks: no path here ever
        // holds two mutexes at once.
        let key = {
            let core = self.inner.core.lock().await;
            let (_, key, _) = Self::resolve_instance(
                &core,
                &peer_key,
                peer_id,
                selector,
                "gatt.subscribe",
                false,
            )?;
            key
        };
        // L7 resubscribe semantics: a pending failed disable fails the
        // resubscribe closed — complete the disable with `unsubscribe`
        // first instead of racing it with an enable.
        if self.inner.failed_disables.lock().await.contains(&key) {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "gatt.subscribe",
            )
            .with_detail("physical disable pending; complete it with unsubscribe"));
        }
        // A requirement on an instance whose CCCD is already routed (live
        // or enabling) is checked before any effect: a join never rewrites
        // the CCCD.
        if let Some(required) = delivery {
            let routed = self.inner.subscriptions.lock().await.contains_key(&key);
            if routed {
                let observed = lock_std(&self.inner.deliveries)
                    .get(&key)
                    .copied()
                    .unwrap_or(ObservedDelivery::Unknown);
                if !observed.satisfies(required) {
                    return Err(Self::delivery_unenforced(
                        required,
                        observed,
                        "a join cannot rewrite the live CCCD",
                    ));
                }
            }
        }
        let (operation, path_index, drive_enable) = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let (index, resolved, _) = Self::resolve_instance(
                &core,
                &peer_key,
                peer_id,
                selector,
                "gatt.subscribe",
                false,
            )?;
            debug_assert_eq!(key, resolved);
            // The physical enable is driven exactly once, by the caller
            // whose subscribe staged the core's explicit enable effect
            // (F11). A ready CCCD shares without an effect, a pending
            // enablement joins it, and a repeated request reuses its op:
            // none of them may touch the radio. Inferring any of this
            // from `physical_cccd_enabled` would re-drive the enable for
            // every joiner that arrives mid-flight.
            let effects_before = core.typed_effects().len();
            let id = core
                .subscribe(
                    index,
                    policy.as_str(),
                    item_capacity,
                    byte_capacity,
                    consumer,
                    window.core_timeout_ms(),
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            let drive_enable = core.typed_effects()[effects_before..].iter().any(|effect| {
                effect.kind() == CentralEffectKind::SubscribeEnable && effect.operation_id() == &id
            });
            publish_or_refuse(&mut core, &ctl.ticket, &id, "gatt.subscribe", None)?;
            if !drive_enable && let Some(required) = delivery {
                // Raced another subscriber to the enable: this consumer
                // joined a CCCD whose mode it cannot prove. Roll the join
                // back inside the same critical section (no radio effect).
                let observed = lock_std(&self.inner.deliveries)
                    .get(&key)
                    .copied()
                    .unwrap_or(ObservedDelivery::Unknown);
                if !observed.satisfies(required) {
                    let _ = core.cancel_op(&id, now_ms(), &mut out);
                    let _ = out.drain();
                    let _ = core.unsubscribe(index, consumer, now_ms(), &mut out);
                    let _ = out.drain();
                    sweep_terminal_successes(&mut core);
                    return Err(Self::delivery_unenforced(
                        required,
                        observed,
                        "joined an enable whose mode is not yet observed",
                    ));
                }
            }
            // Dispatch only a freshly queued op: a joined consumer's op is
            // already complete, and a re-subscribed in-flight op is already
            // dispatched. Dispatching either again would fail closed.
            let dispatchable = matches!(
                core.operation_state(&id),
                Some(ubm_core::ownership::OpStateView::Queued)
            );
            if dispatchable {
                core.dispatch_op(&id, &mut out)
                    .map_err(DesktopError::from)?;
            }
            (id, index, drive_enable)
        };
        let mut drop_guard = CancelOnDrop::armed(
            self,
            operation.clone(),
            DropCleanup::Subscribe {
                key: key.clone(),
                path_index,
            },
        );
        // Route before the physical enable so values arriving mid-enable
        // quarantine in the hub instead of dropping on the floor. The
        // routing carries the epoch the forwarder is about to capture, so
        // a value queued under a dead generation never matches (F10).
        let epoch = self.routing_epoch(peer_id).await;
        // A new enable owns this instance's CCCD from here on (its failure
        // paths disable it), superseding any enablement a service change
        // orphaned (finding 40).
        lock_std(&self.inner.retained_enablements).remove(&key);
        self.inner
            .subscriptions
            .lock()
            .await
            .insert(key.clone(), (path_index, epoch));
        if !drive_enable {
            // Joiners never touch the radio (F11): an immediate-success share
            // on an enabled hub is already terminal and releases now; a
            // pending join stays live for the enabler to settle and sweep.
            let mut core = self.inner.core.lock().await;
            report_terminal_release(&mut core, &operation, true, None);
            recycle_observations(&mut core);
            drop_guard.defuse();
            return Ok(lock_std(&self.inner.deliveries)
                .get(&key)
                .copied()
                .unwrap_or(ObservedDelivery::Unknown));
        }
        let result = match drive(
            &ctl.ticket,
            window,
            self.inner
                .boundary
                .set_notifications(peer_id, &key.1, key.2, &key.3, key.4, true, epoch, delivery),
        )
        .await
        {
            Wait::Done(Ok(observed)) => {
                if let Some(required) = delivery
                    && !observed.satisfies(required)
                {
                    // The radio enabled the CCCD without proving the
                    // required mode: undo the enable and fail it, never
                    // accept a requirement nobody enforced.
                    self.disable_orphan_enable(peer_id, &key, epoch).await;
                    self.fail_enable(&key, path_index, &operation, ContenderKind::Failure)
                        .await;
                    Err(Self::delivery_unenforced(
                        required,
                        observed,
                        "the radio did not report the required mode",
                    ))
                } else {
                    self.settle_enable_success(
                        peer_id, &key, path_index, &operation, epoch, observed,
                    )
                    .await
                }
            }
            Wait::Done(Err(error)) => {
                self.fail_enable(&key, path_index, &operation, ContenderKind::Failure)
                    .await;
                Err(error)
            }
            Wait::Expired => {
                // Deadline won: settle our own op as timeout first (so it
                // stays TimedOut, not Failed), then fail the shared enable
                // for the hub and any joiners.
                self.fail_enable(&key, path_index, &operation, ContenderKind::Timeout)
                    .await;
                Err(timed_out("gatt.subscribe", window))
            }
            Wait::Cancelled => {
                self.inner.subscriptions.lock().await.remove(&key);
                let error = self.settle_abort(&operation, "gatt.subscribe").await;
                {
                    let mut core = self.inner.core.lock().await;
                    let mut out = batch();
                    let _ = core.settle_subscribe_enable(path_index, false, now_ms(), &mut out);
                    let _ = out.drain();
                    sweep_terminal_successes(&mut core);
                }
                Err(error)
            }
        };
        drop_guard.defuse();
        result.map_err(|error| classify(error, OpKind::Subscribe, true))
    }

    /// The physical enable succeeded: settle the hub and our op. A late
    /// success with nobody eligible stages a compensating disable (F12):
    /// the OS enable is live with no owner, so it is torn down, not leaked.
    async fn settle_enable_success(
        &self,
        peer_id: &str,
        key: &InstanceKey,
        path_index: usize,
        operation: &OperationId,
        epoch: u64,
        observed: ObservedDelivery,
    ) -> Result<ObservedDelivery, DesktopError> {
        let (compensating, own_kind) = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let effects_before = core.typed_effects().len();
            let _ = core.settle_subscribe_enable(path_index, true, now_ms(), &mut out);
            let _ = out.drain();
            let compensating = core.typed_effects()[effects_before..]
                .iter()
                .any(|effect| effect.kind() == CentralEffectKind::SubscribeDisable);
            let _ = core.settle_op(
                operation,
                ContenderKind::Success,
                true,
                0,
                now_ms(),
                &mut out,
            );
            let _ = out.drain();
            let own_kind = terminal_kind_of(&core, operation);
            sweep_terminal_successes(&mut core);
            (compensating, own_kind)
        };
        if compensating {
            self.disable_orphan_enable(peer_id, key, epoch).await;
            {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_subscribe_disable(path_index, now_ms(), &mut out);
                let _ = out.drain();
                sweep_terminal_successes(&mut core);
            }
            self.inner.subscriptions.lock().await.remove(key);
            return Err(DesktopError::cancelled("gatt.subscribe"));
        }
        match own_kind {
            Some(OperationTerminalKind::Succeeded) | None => {
                lock_std(&self.inner.deliveries).insert(key.clone(), observed);
                Ok(observed)
            }
            Some(winner) => Err(terminal_to_error(winner, "gatt.subscribe")),
        }
    }

    /// Fail one physical enable: remove routing, settle our op with
    /// `contender`, fail the shared enable for the hub and its joiners.
    async fn fail_enable(
        &self,
        key: &InstanceKey,
        path_index: usize,
        operation: &OperationId,
        contender: ContenderKind,
    ) {
        self.inner.subscriptions.lock().await.remove(key);
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        let _ = settle_and_release(&mut core, operation, contender, true, None, &mut out);
        let _ = core.settle_subscribe_enable(path_index, false, now_ms(), &mut out);
        let _ = out.drain();
        sweep_terminal_successes(&mut core);
    }

    /// Disable a CCCD this central enabled but nobody owns (late enable,
    /// unenforced delivery requirement). Bounded by
    /// [`COMPENSATION_TIMEOUT`]; a failure is counted (the radio keeps the
    /// forwarder, so shutdown's close still attempts the release and
    /// reports it), never swallowed.
    async fn disable_orphan_enable(&self, peer_id: &str, key: &InstanceKey, epoch: u64) {
        let disabled = tokio::time::timeout(
            COMPENSATION_TIMEOUT,
            self.inner
                .boundary
                .set_notifications(peer_id, &key.1, key.2, &key.3, key.4, false, epoch, None),
        )
        .await;
        if !matches!(disabled, Ok(Ok(_))) {
            self.inner.note_compensation_failure();
        }
    }

    /// Remove one consumer. Removing one consumer never disables another
    /// consumer's live CCCD: the physical disable fires only when the core
    /// reports the last removal issuing it. Returns whether the physical
    /// CCCD was disabled.
    ///
    /// Disable-failure semantics (L7, PR210-09): when the radio refuses,
    /// times out (budget, or [`LIVENESS_CLEANUP`] without one) or the
    /// caller cancels the physical disable, routing stays in place so
    /// values keep flowing (no silent drops), the hub truthfully stays
    /// `Disabling`, and the key parks in the pending-disable set. A later
    /// `unsubscribe` retries the disable; a `subscribe` on the same
    /// instance fails closed until the disable completes.
    pub async fn unsubscribe(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
        ctl: OpControl,
    ) -> Result<bool, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "gatt.unsubscribe")?;
        let window = ctl.budget.window(LIVENESS_CLEANUP);
        let peer_key = self.known_peer_key(peer_id).await?;
        let (disable_physical, path_index, key) = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let resolved = Self::resolve_instance(
                &core,
                &peer_key,
                peer_id,
                selector,
                "gatt.unsubscribe",
                false,
            );
            let (index, key, _) = match resolved {
                Ok(resolved) => resolved,
                Err(error) => {
                    // Finding 40: the path died with a service change, but
                    // the enablement it addressed may still be live.
                    drop(core);
                    let Some(key) = self.retained_enablement(peer_id, selector) else {
                        return Err(error);
                    };
                    return self
                        .release_retained(peer_id, &key, &ctl.ticket, window)
                        .await;
                }
            };
            let disable = core
                .unsubscribe(index, consumer, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            (disable, index, key)
        };
        if !disable_physical && !self.inner.failed_disables.lock().await.contains(&key) {
            // No radio work: still recycle any terminal shares (e.g. an
            // immediate-success join that released elsewhere) so the
            // ledger never grows across unsubscribe-only cycles.
            let mut core = self.inner.core.lock().await;
            recycle_observations(&mut core);
            return Ok(false);
        }
        self.drive_disable(peer_id, &key, path_index, &ctl.ticket, window)
            .await
    }

    /// Drive one physical disable (first attempt or a retry of a failed
    /// one). Teardown is keyed by instance: the boundary ignores the epoch
    /// on disable, so the current generation is passed through untouched.
    /// The orphaned enablement `selector` addresses, when exactly one
    /// matches (finding 40).
    fn retained_enablement(&self, peer_id: &str, selector: &PathSelector) -> Option<InstanceKey> {
        let characteristic = selector.characteristic_uuid.as_deref()?;
        let retained = lock_std(&self.inner.retained_enablements);
        let mut matches = retained.iter().filter(|key| {
            key.0 == peer_id
                && key.1 == selector.service_uuid
                && selector
                    .service_occurrence
                    .is_none_or(|occurrence| occurrence == key.2)
                && key.3 == characteristic
                && selector
                    .characteristic_occurrence
                    .is_none_or(|occurrence| occurrence == key.4)
        });
        let first = matches.next()?.clone();
        matches.next().is_none().then_some(first)
    }

    /// Disable an orphaned enablement at the radio (finding 40). Released
    /// only when the radio confirms; a failed release stays retained for a
    /// retry, a later link loss, or shutdown's close receipts.
    async fn release_retained(
        &self,
        peer_id: &str,
        key: &InstanceKey,
        ticket: &OpTicket,
        window: Window,
    ) -> Result<bool, DesktopError> {
        let outcome = drive(
            ticket,
            window,
            self.inner
                .boundary
                .set_notifications(peer_id, &key.1, key.2, &key.3, key.4, false, 0, None),
        )
        .await;
        match outcome {
            Wait::Done(Ok(_)) => {
                lock_std(&self.inner.retained_enablements).remove(key);
                Ok(true)
            }
            Wait::Done(Err(error)) => Err(error),
            Wait::Expired => Err(classify(
                timed_out("gatt.unsubscribe", window),
                OpKind::Cleanup,
                true,
            )),
            Wait::Cancelled => Err(classify(
                ticket.interruption("gatt.unsubscribe"),
                OpKind::Cleanup,
                true,
            )),
        }
    }

    async fn drive_disable(
        &self,
        peer_id: &str,
        key: &InstanceKey,
        path_index: usize,
        ticket: &OpTicket,
        window: Window,
    ) -> Result<bool, DesktopError> {
        let epoch = self.routing_epoch(peer_id).await;
        let outcome = drive(
            ticket,
            window,
            self.inner
                .boundary
                .set_notifications(peer_id, &key.1, key.2, &key.3, key.4, false, epoch, None),
        )
        .await;
        let error = match outcome {
            Wait::Done(Ok(_)) => {
                self.inner.subscriptions.lock().await.remove(key);
                self.inner.failed_disables.lock().await.remove(key);
                lock_std(&self.inner.deliveries).remove(key);
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_subscribe_disable(path_index, now_ms(), &mut out);
                let _ = out.drain();
                sweep_terminal_successes(&mut core);
                return Ok(true);
            }
            Wait::Done(Err(error)) => error,
            Wait::Expired => classify(timed_out("gatt.unsubscribe", window), OpKind::Cleanup, true),
            Wait::Cancelled => classify(
                ticket.interruption("gatt.unsubscribe"),
                OpKind::Cleanup,
                true,
            ),
        };
        // Routing stays: the CCCD may still be live, so values must still
        // reach the hub. The pending-disable set routes the next
        // `unsubscribe` into a retry.
        self.inner.failed_disables.lock().await.insert(key.clone());
        Err(error)
    }

    /// Take one buffered notification value for a consumer (M2, FIFO
    /// arrival order). Values buffer from subscription onward and stay
    /// observable after the radio delivers them; `None` means no value is
    /// waiting. Only the hub's admitted bytes cross here: values the
    /// radio never delivered are never synthesized.
    ///
    /// Note: `None` hides whether the stream is live-empty, terminal, or
    /// closed. Prefer [`DesktopCentral::poll_notification`] (F17), which
    /// distinguishes all three plus invalidation.
    pub async fn take_notification(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
    ) -> Result<Option<Vec<u8>>, DesktopError> {
        self.admit("gatt.take-notification")?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let mut core = self.inner.core.lock().await;
        let index = match core.consumer_path(&peer_key, selector, consumer) {
            Some(index) => index,
            None => core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?,
        };
        Ok(core.take_notification_value(index, consumer))
    }

    /// Poll one consumer's stream with a typed outcome (F17): value, live
    /// empty, overflow terminal (exactly once, with loss details),
    /// invalidation with its cause (service change or link end, read from
    /// the connection state), or closure. Values drain before the terminal;
    /// after the terminal is observed the stream reports closed, never
    /// live-empty again.
    pub async fn poll_notification(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
    ) -> Result<NotificationPoll, DesktopError> {
        self.admit("gatt.take-notification")?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let mut core = self.inner.core.lock().await;
        let cause = match core.connection_state(&peer_key) {
            Some(ConnectionState::Connected | ConnectionState::Connecting) => {
                InvalidationCause::ServicesChanged
            }
            _ if lock_std(&self.inner.reset_peers).contains(&peer_key) => {
                InvalidationCause::AdapterReset
            }
            _ => InvalidationCause::LinkEnded,
        };
        // Finding 111: the consumer's own subscription, whatever its
        // generation, so values it held at a service change, link loss or
        // adapter reset drain before the invalidation.
        let index = match core.consumer_path(&peer_key, selector, consumer) {
            Some(index) => index,
            None => match core.resolve_path(&peer_key, selector) {
                Ok(index) => index,
                Err(error) => {
                    // A selector that no longer resolves while the database
                    // is off-current is stale invalidation (service change,
                    // disconnect, rediscovery required), not a missing path.
                    let current =
                        matches!(core.database_state(&peer_key), Some(DatabaseState::Current));
                    if current {
                        return Err(DesktopError::from(error));
                    }
                    return Ok(NotificationPoll::Invalidated(cause));
                }
            },
        };
        if let Some(value) = core.take_notification_value(index, consumer) {
            return Ok(NotificationPoll::Value(value));
        }
        if let Some(terminal) = core.take_terminal(index, consumer) {
            return Ok(NotificationPoll::Terminal(terminal));
        }
        match core.consumer_state(index, consumer) {
            Some(
                ubm_core::central::ConsumerState::Ready
                | ubm_core::central::ConsumerState::Enabling
                | ubm_core::central::ConsumerState::Removing,
            ) => Ok(NotificationPoll::Empty),
            Some(ubm_core::central::ConsumerState::Invalid) => {
                Ok(NotificationPoll::Invalidated(cause))
            }
            Some(
                ubm_core::central::ConsumerState::Failed
                | ubm_core::central::ConsumerState::Removed,
            )
            | None => Ok(NotificationPoll::Closed),
        }
    }

    /// Notifications the OS lost before they reached one consumer's stream
    /// (vendored btleplug patch 10), counted under a lossy overflow policy.
    /// Under `error` the loss ends the stream with an overflow terminal
    /// instead ([`NotificationPoll::Terminal`]).
    pub async fn notification_loss(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
    ) -> Result<Option<u64>, DesktopError> {
        self.admit("gatt.notification-loss")?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let core = self.inner.core.lock().await;
        let index = core
            .resolve_path(&peer_key, selector)
            .map_err(DesktopError::from)?;
        Ok(core.upstream_lost_count(index, consumer))
    }

    /// Cancel one admitted operation by core id (`operation.aborted`
    /// discipline in the core). Prefer [`DesktopCentral::cancel`] with the
    /// operation's ticket, which also covers a cancel that arrives before
    /// the id exists. Mid-flight radio abort is an OS gap — btleplug exposes
    /// no abort — so the driver drops its radio future when it observes
    /// the cancel; a dispatched write stays commit-`unknown`. A queued
    /// cancellation releases immediately (no radio work exists); a
    /// dispatched cancellation leaves the release to the in-flight driver,
    /// which observes the abort as the winning outcome and reports it.
    pub async fn cancel_operation(
        &self,
        operation: &OperationId,
    ) -> Result<ubm_core::central::CompletionOutcome, DesktopError> {
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        let outcome = match core.cancel_op(operation, now_ms(), &mut out) {
            Ok(outcome) => outcome,
            Err(error) if error.code() == BleErrorCode::ArgumentInvalid => {
                // R15: the op settled and was released (reaped) before this
                // cancel arrived — the kernel forgot it. A retained scan
                // ticket or an F15 shutdown tombstone still names the
                // genuine settled terminal, so suppress onto it exactly as
                // a cancel against the still-present terminal would. Truly
                // unknown ids keep failing closed with `argument.invalid`.
                if completed_scan_kind(&self.inner.completed_scans, operation).is_some()
                    || core.shutdown_terminal_kind(operation).is_some()
                {
                    let suppressed = core.suppressed_count(operation).unwrap_or(0);
                    ubm_core::central::CompletionOutcome::DuplicateSuppressed { suppressed }
                } else {
                    return Err(DesktopError::from(error));
                }
            }
            Err(error) => return Err(DesktopError::from(error)),
        };
        let _ = out.drain();
        recycle_observations(&mut core);
        if let CompletionOutcome::Settled { commit, .. } = &outcome {
            // `NotDispatched` proves no radio work exists: release now.
            // `Released` (dispatched abort) stays for the driver, which will
            // see `DuplicateSuppressed` and report after observing the win.
            if *commit == CommitState::NotDispatched {
                // R15: retain a scan's completed ticket between settlement
                // and release (first writer wins), then release.
                if core.scan_session_state(operation).is_some()
                    && let Some(kind) = terminal_kind_of(&core, operation)
                {
                    retain_completed_scan(&self.inner.completed_scans, operation, kind);
                }
                report_terminal_release(&mut core, operation, true, None);
            }
        }
        Ok(outcome)
    }

    /// Per-central shutdown (F14/F15): close this attachment's admission
    /// first so no new work races cleanup, stop the owned scan (a final
    /// failed stop is reported in [`ShutdownReport::scan_stop_failure`]
    /// and as a release failure in the record), release owned radio
    /// subscriptions through the boundary teardown hook, release owned OS
    /// links with per-link receipts, join the event loop so nothing races
    /// teardown, then drive incremental destruction to acknowledged
    /// completion and return the authoritative report. Idempotent. Other
    /// centrals keep working and new centrals can open; process-executor
    /// shutdown is a separate explicit process-owner step
    /// ([`crate::executor::shutdown_desktop_runtime`]), never implied here.
    pub async fn shutdown(&self) -> ShutdownReport {
        // F14: admission closes before any cleanup starts, so a racing
        // starter cannot slip work in behind the scan stop.
        self.inner.shut_down.store(true, Ordering::SeqCst);
        // R14c: cancel scan ops still starting by id before the slot stop.
        // A starter admitted before admission closed may still be awaiting
        // its radio start: its kernel op goes terminal (and released,
        // ticket retained per R15) now, so when its radio resolves the
        // post-await check forces it into compensation instead of
        // activating behind cleanup. An active (or stop-failed) scan keeps
        // its op for the final stop below, whose failure the record names.
        let keep: Option<OperationId> = self.inner.scan_slot().as_ref().and_then(|active| {
            (!matches!(active.phase, ScanPhase::Starting)).then(|| active.id.clone())
        });
        let racing_scans: Vec<OperationId> = {
            let core = self.inner.core.lock().await;
            core.live_operation_ids()
                .into_iter()
                .filter(|id| core.scan_session_state(id).is_some() && Some(id) != keep.as_ref())
                .collect()
        };
        for id in racing_scans {
            let _ = self.cancel_operation(&id).await;
        }
        let scan_stop_failure = self.final_scan_stop().await;
        // M3: abort forwarders and best-effort release OS-side CCCDs so no
        // live subscription outlives the central. Per-scope release failures
        // are drained as receipts (F14), never swallowed.
        self.inner.boundary.close().await;
        let radio_close_failures = self.inner.boundary.take_close_failures();
        // F14: release owned OS links with per-link receipts before the
        // owner is destroyed.
        self.release_owned_links().await;
        // F03: cancel remaining in-flight ops so late radio work cannot
        // resurrect after teardown — queued releases now, dispatched
        // observes the abort as the winning outcome when its radio finishes.
        let live: Vec<OperationId> = {
            let core = self.inner.core.lock().await;
            core.live_operation_ids()
        };
        for id in live {
            let _ = self.cancel_operation(&id).await;
        }
        let worker = self.inner.loop_done.lock().await.take();
        let _ = self.inner.loop_stop.send(true);
        if let Some(worker) = worker {
            let _ = worker.await;
        }
        // F15: the final record is taken only after every destroy pass
        // executed, every dispatched remainder was answered, and every
        // terminal release was acknowledged — never from the legacy
        // unacknowledged `destroy()`.
        let (record, destroy_steps) = self.drive_destroy().await;
        ShutdownReport {
            record,
            radio_close_failures,
            destroy_steps,
            scan_stop_failure,
        }
    }

    /// Shutdown's final scan stop (PR210-09): one bounded attempt. When it
    /// fails, the retained scan op settles as failed with a release
    /// failure, so the destroy record names it, and the marker goes (the
    /// central is closing; nothing can retry it).
    async fn final_scan_stop(&self) -> Option<DesktopError> {
        let id = self.active_scan_id()?;
        let window = Budget::unbounded().window(LIVENESS_CLEANUP);
        let error = match self.stop_scan_with(&id, window, &OpTicket::new()).await {
            Ok(_) => return None,
            Err(error) => error,
        };
        {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let _ = core.note_scan_platform(
                &id,
                ubm_core::central::ScanPlatformEvent::StopFailed,
                now_ms(),
                &mut out,
            );
            let _ = out.drain();
            let _ = core.settle_op(&id, ContenderKind::Failure, true, 0, now_ms(), &mut out);
            let _ = out.drain();
            if let Some(kind) = terminal_kind_of(&core, &id) {
                retain_completed_scan(&self.inner.completed_scans, &id, kind);
            }
            report_terminal_release(&mut core, &id, false, Some(error.code()));
            recycle_observations(&mut core);
        }
        let mut slot = self.inner.scan_slot();
        if slot.as_ref().is_some_and(|active| active.id == id) {
            *slot = None;
        }
        Some(error)
    }

    /// Release every owned OS link (F14): each peer whose core connection is
    /// still live gets one bounded radio disconnect. Success confirms link
    /// release (`Disconnected`, published as `Released { requested: true }`);
    /// a radio failure or deadline marks the link `Disconnecting` and
    /// records a disconnect failure, so the final destroy record names it
    /// (receipt) instead of claiming a clean release. Skips peers that
    /// already released, so repeat shutdowns stay quiet and idempotent.
    async fn release_owned_links(&self) {
        let peers: Vec<(String, String)> = {
            let peers = self.inner.peers.lock().await;
            peers
                .iter()
                .map(|(peer_id, peer_key)| (peer_id.clone(), peer_key.clone()))
                .collect()
        };
        for (peer_id, peer_key) in peers {
            let live = {
                let core = self.inner.core.lock().await;
                matches!(
                    core.connection_state(&peer_key),
                    Some(
                        ConnectionState::Connected
                            | ConnectionState::Connecting
                            | ConnectionState::Disconnecting
                    )
                )
            };
            if !live {
                continue;
            }
            let outcome = tokio::time::timeout(
                DISCONNECT_COMPLETION_TIMEOUT,
                self.inner.boundary.disconnect(&peer_id),
            )
            .await;
            // Late radio completions must not resurrect the link: drop local
            // subscription routing for this peer now.
            self.drop_peer_subscriptions(&peer_id).await;
            let event = {
                let mut core = self.inner.core.lock().await;
                match outcome {
                    Ok(Ok(())) => {
                        let generation = Generations::of(&core, &peer_key);
                        core.shutdown_release_link(&peer_key).ok().map(|()| {
                            self.inner.stage_lifecycle(
                                &peer_id,
                                &peer_key,
                                generation,
                                LifecycleKind::Released { requested: true },
                            )
                        })
                    }
                    Ok(Err(error)) => {
                        core.note_shutdown_disconnect_failed(&peer_key, error.code());
                        None
                    }
                    Err(_) => {
                        core.note_shutdown_disconnect_failed(
                            &peer_key,
                            BleErrorCode::OperationTimedOut,
                        );
                        None
                    }
                }
            };
            if let Some(event) = event {
                self.inner.signal(CentralSignal::Lifecycle(event));
            }
        }
    }

    /// Drive incremental destruction to acknowledged completion (F15): one
    /// `destroy_step` pass per loop iteration, executing each pass's effects,
    /// settling dispatched remainders as destroyed, and acknowledging every
    /// terminal release — the host protocol `destroy_step` requires. The
    /// final record comes from `destroy_record`, which refuses while work
    /// pends and merges disconnect and release failures. Bounded: the pass
    /// budget is one per live op plus a final pass, so a wedged kernel
    /// surfaces `central.destroy.truncated` instead of looping forever.
    async fn drive_destroy(&self) -> (Result<CleanupRecord, DesktopError>, usize) {
        let mut steps = 0usize;
        let mut budget = {
            let core = self.inner.core.lock().await;
            core.live_operation_ids().len().saturating_add(2)
        };
        loop {
            let progress = {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let progress = match core.destroy_step(&mut out) {
                    Ok(progress) => progress,
                    Err(error) => return (Err(DesktopError::from(error)), steps),
                };
                // Execute the pass outside radio work: shutdown emits logical
                // StatePublish/CleanupRelease effects (OS releases already ran
                // above), so draining executes the batch.
                let _ = out.drain();
                // Settle dispatched remainders as destroyed before acking.
                for id in core.live_operation_ids() {
                    if core.operation_state(&id)
                        == Some(ubm_core::ownership::OpStateView::Dispatched)
                    {
                        let mut settle_out = batch();
                        let _ = core.settle_op(
                            &id,
                            ContenderKind::Destroy,
                            true,
                            0,
                            now_ms(),
                            &mut settle_out,
                        );
                        let _ = settle_out.drain();
                    }
                }
                // Ack every terminal: the host did release its tracking for
                // each (OS CCCDs via `close`, OS links via
                // `release_owned_links`), so success is the truthful report.
                // Shutdown acks keep tombstones: racing callers that settle
                // after the reap still observe their winning terminal.
                for id in core.terminal_operation_ids() {
                    let _ = core.report_shutdown_release(&id);
                }
                recycle_observations(&mut core);
                progress
            };
            steps += 1;
            if progress.done {
                break;
            }
            if budget == 0 {
                return (
                    Err(contract_error(
                        BleErrorCode::StreamQuota,
                        BleErrorDomain::Stream,
                        "central.destroy.truncated",
                    )),
                    steps,
                );
            }
            budget -= 1;
        }
        let record = {
            let mut core = self.inner.core.lock().await;
            core.destroy_record().map_err(DesktopError::from)
        };
        (record, steps)
    }

    /// Compose the effective single-write maximum for one peer and mode:
    /// the ATT protocol ceiling plus the OS-measured single-write limit as
    /// both the negotiated and the backend limit (the radio submits one
    /// write per call and the OS enforces its own limit; the limit is the
    /// OS's per-mode answer, `mtu - 3` where the OS has no per-mode
    /// readout). An unmeasured limit fails closed with
    /// `capability.unavailable`, never a guessed 20 bytes. Pure computation
    /// over a pre-fetched limit: callers fetch it before locking the core
    /// (M1), so this never awaits under the lock.
    fn write_maximum(
        core: &Central,
        measured_limit: Option<u16>,
        operation: &'static str,
    ) -> Result<u64, DesktopError> {
        let directional = measured_limit.map(u64::from).filter(|limit| *limit > 0);
        core.maximum_write_length(Some(ATT_MAX_WRITE), directional, directional, operation)
            .map_err(DesktopError::from)
    }

    async fn known_peer_key(&self, peer_id: &str) -> Result<String, DesktopError> {
        self.inner
            .peers
            .lock()
            .await
            .get(peer_id)
            .cloned()
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::PeerNotFound,
                    BleErrorDomain::Connection,
                    "peer.known",
                )
            })
    }

    async fn drop_peer_subscriptions(&self, peer_id: &str) {
        clear_peer_routing(&self.inner, peer_id).await;
    }
}

#[cfg(feature = "btleplug")]
impl DesktopCentral<crate::btleplug_backend::BtleplugRadio> {
    /// Open the production btleplug radio on the adapter the profile names
    /// ([`CentralProfile::adapter_id`]; `None` = the default adapter), then
    /// the central over it. Call on the shared desktop executor
    /// ([`crate::executor::desktop_runtime`]).
    ///
    /// macOS (finding 59): CoreBluetooth answers asynchronously, so the
    /// open waits — at most [`ADAPTER_INITIALIZATION_TIMEOUT`], as the
    /// legacy CoreBluetooth backend did — for CoreBluetooth to report a
    /// usable adapter (powered on, not refused to this process). Past the
    /// bound the open fails `capability.unavailable` with detail
    /// [`ADAPTER_INITIALIZATION_TIMED_OUT`] and the radio is closed.
    pub async fn open_btleplug(mut profile: CentralProfile) -> Result<Self, DesktopError> {
        let started = tokio::time::Instant::now();
        let open = crate::btleplug_backend::BtleplugRadio::open_on(
            crate::executor::desktop_runtime(),
            profile.adapter_id.clone(),
            profile.bluez_bus,
        );
        // btleplug's CoreBluetooth manager blocks until the first
        // `centralManagerDidUpdateState:` with no bound of its own.
        let radio = if cfg!(target_os = "macos") {
            tokio::time::timeout(ADAPTER_INITIALIZATION_TIMEOUT, open)
                .await
                .map_err(|_| {
                    contract_error(
                        BleErrorCode::CapabilityUnavailable,
                        BleErrorDomain::Platform,
                        "adapter.initialize",
                    )
                    .with_detail(format!(
                        "{ADAPTER_INITIALIZATION_TIMED_OUT}: CoreBluetooth reported no adapter \
                         state within {} ms",
                        ADAPTER_INITIALIZATION_TIMEOUT.as_millis()
                    ))
                })??
        } else {
            open.await?
        };
        // The radio already enforced the selection; a caller that named the
        // adapter by its full OS label is held to the selected identity.
        if profile.adapter_id.is_some() {
            profile.adapter_id = Some(radio.adapter_label().to_owned());
        }
        let central = Self::open_with(radio, profile).await?;
        if cfg!(target_os = "macos") {
            let remaining = ADAPTER_INITIALIZATION_TIMEOUT.saturating_sub(started.elapsed());
            if let Err(error) = central.await_usable_adapter(remaining).await {
                // Nothing was admitted yet: the teardown releases only the
                // radio this open created. A teardown that did not come
                // back clean is reported, never dropped.
                let report = central.shutdown().await;
                let clean = matches!(
                    &report.record,
                    Ok(record) if record.state() == ubm_core::ownership::CleanupState::Released
                ) && report.radio_close_failures.is_empty();
                if !clean {
                    eprintln!(
                        "ubm-desktop: the radio opened for an adapter that never became usable \
                         did not shut down cleanly: {report:?}"
                    );
                }
                return Err(error);
            }
        }
        Ok(central)
    }
}

/// Drop subscription routing, pending-disable retries and observed
/// delivery modes for one peer, and bump the peer's subscription epoch
/// (F10). Late radio completions must not resurrect the link: the core
/// already invalidated its hubs, the adapter drops its routing alongside,
/// and values still queued under the dead generation fail the routing check
/// after resubscribe.
async fn clear_peer_routing<B>(inner: &Arc<Inner<B>>, peer_id: &str) {
    lock_std(&inner.retained_enablements).retain(|key| key.0 != peer_id);
    let mut subscriptions = inner.subscriptions.lock().await;
    subscriptions.retain(|key, _| key.0 != peer_id);
    drop(subscriptions);
    let mut failed = inner.failed_disables.lock().await;
    failed.retain(|key| key.0 != peer_id);
    drop(failed);
    lock_std(&inner.deliveries).retain(|key, _| key.0 != peer_id);
    let mut epochs = inner.epochs.lock().await;
    let epoch = epochs.entry(peer_id.to_owned()).or_insert(0);
    *epoch = epoch.saturating_add(1);
}

/// Drive one central lifetime: resolve advertisements to platform-guid
/// peers, reconcile connection events with the core, and route
/// notifications to subscribed hubs. Every core settlement here is
/// best-effort — the explicit op paths own authoritative transitions, and a
/// racing explicit op must not fail because the loop saw the event first.
/// The loop owns core settlement only on the event-source-closed path;
/// [`DesktopCentral::stop_scan`] owns the stop path and
/// [`DesktopCentral::shutdown`] joins this worker, so cleanup cannot race.
async fn scan_loop<B: RadioBoundary>(inner: Arc<Inner<B>>, mut stop: watch::Receiver<bool>) {
    // Finding 120: the known-peer re-read runs on its own interval so a
    // steady stream of radio events never postpones it.
    let mut refresh: Option<(u64, tokio::time::Interval)> = None;
    loop {
        let period_ms = inner.known_peer_refresh_ms.load(Ordering::SeqCst);
        if refresh.as_ref().map(|(ms, _)| *ms) != Some(period_ms) {
            refresh = (period_ms > 0).then(|| {
                let period = Duration::from_millis(period_ms);
                let mut interval =
                    tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                (period_ms, interval)
            });
        }
        tokio::select! {
            biased;
            changed = stop.changed() => {
                let _ = changed;
                break;
            }
            () = inner.refresh_changed.notified() => {}
            _ = async {
                match refresh.as_mut() {
                    Some((_, interval)) => interval.tick().await,
                    None => std::future::pending().await,
                }
            } => {
                refresh_known_peers(&inner).await;
            }
            event = inner.boundary.next_event() => {
                match event {
                    None => {
                        // Event source closed: settle an owned scan session
                        // as source-closed so the owner is released even
                        // when the OS never confirms a stop.
                        let id = inner.scan_slot().as_ref().map(|active| active.id.clone());
                        if let Some(id) = id {
                            settle_os_ended_scan(&inner, &id, ContenderKind::Success).await;
                            parity::publish_scan_terminal(
                                &inner,
                                &id,
                                true,
                                "the OS event source closed",
                            );
                        }
                        break;
                    }
                    Some(RadioEvent::ScanTerminated { aborted, detail }) => {
                        // The OS stopped the scan without a stop request
                        // (legacy WinRT `OnScanTerminal`). Only an active,
                        // not-stopping scan is ours to end here: a stop in
                        // flight owns its own settlement.
                        let id = inner.scan_slot().as_ref().and_then(|active| {
                            matches!(active.phase, ScanPhase::Active).then(|| active.id.clone())
                        });
                        if let Some(id) = id {
                            let contender = if aborted {
                                ContenderKind::Failure
                            } else {
                                ContenderKind::Success
                            };
                            settle_os_ended_scan(&inner, &id, contender).await;
                            parity::publish_scan_terminal(&inner, &id, aborted, &detail);
                        }
                    }
                    Some(RadioEvent::WriteReadiness { peer_id, ready }) => {
                        parity::publish_write_readiness(&inner, &peer_id, ready).await;
                    }
                    Some(RadioEvent::Advertisement(snapshot)) => {
                        ingest_advertisement(&inner, snapshot).await;
                    }
                    Some(RadioEvent::Connected(peer_id)) => {
                        reconcile_connected(&inner, &peer_id).await;
                    }
                    Some(RadioEvent::Disconnected(peer_id)) => {
                        reconcile_disconnected(&inner, &peer_id).await;
                    }
                    Some(RadioEvent::ServicesChanged(peer_id)) => {
                        services_changed_invalidated(&inner, &peer_id).await;
                    }
                    Some(RadioEvent::SecurityChanged { peer_id, state }) => {
                        parity::publish_security(&inner, &peer_id, state);
                    }
                    Some(RadioEvent::AdapterState(state)) => {
                        let loss = note_power(&inner, state);
                        let sequence = inner.adapter_sequence.fetch_add(1, Ordering::SeqCst) + 1;
                        let event = AdapterEvent { sequence, state };
                        // Zero receivers is not a failure: the state stays
                        // readable through `adapter_state`.
                        let _ = inner.adapter.send(event);
                        inner.signal(CentralSignal::Adapter(event));
                        if let Some(cause) = loss {
                            adapter_reset(&inner, cause).await;
                        }
                    }
                    Some(RadioEvent::AdapterAuthorization(authorization)) => {
                        let loss = {
                            let mut facts = lock_std(&inner.adapter_facts);
                            facts.authorization = Some(authorization);
                            let refused = matches!(
                                authorization,
                                AdapterAuthorization::Denied | AdapterAuthorization::Restricted
                            );
                            let loss = refused && !facts.lost && inner.teardown_on_loss;
                            if loss {
                                facts.lost = true;
                            }
                            loss
                        };
                        // Waiters on a usable adapter re-read the facts.
                        wake_state_waiters(&inner);
                        if loss {
                            adapter_reset(&inner, AdapterLossCause::Unauthorized).await;
                        }
                    }
                    Some(RadioEvent::AdapterLost(cause)) => {
                        {
                            let mut facts = lock_std(&inner.adapter_facts);
                            if matches!(
                                cause,
                                AdapterLossCause::Removed | AdapterLossCause::DaemonRestarted
                            ) {
                                facts.removed = true;
                            }
                            facts.lost = true;
                        }
                        wake_state_waiters(&inner);
                        if inner.teardown_on_loss {
                            adapter_reset(&inner, cause).await;
                        }
                    }
                    Some(RadioEvent::AdapterRestored) => {
                        lock_std(&inner.adapter_facts).removed = false;
                        wake_state_waiters(&inner);
                    }
                    Some(RadioEvent::NotificationsLost {
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                        epoch,
                        lost,
                    }) => {
                        account_lost_notifications(
                            &inner,
                            (
                                peer_id,
                                service_uuid,
                                service_occurrence,
                                characteristic_uuid,
                                characteristic_occurrence,
                            ),
                            epoch,
                            lost,
                        )
                        .await;
                    }
                    Some(RadioEvent::EventsLost { skipped }) => {
                        inner
                            .radio_events_lost
                            .fetch_add(skipped, Ordering::Relaxed);
                        eprintln!(
                            "ubm-desktop: {skipped} adapter events were lost: the OS event \
                             broadcast outran the radio"
                        );
                    }
                    Some(RadioEvent::Notification {
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                        epoch,
                        value,
                    }) => {
                        deliver(
                            &inner,
                            (
                                peer_id,
                                service_uuid,
                                service_occurrence,
                                characteristic_uuid,
                                characteristic_occurrence,
                            ),
                            epoch,
                            value,
                        )
                        .await;
                    }
                }
            }
        }
    }
}

/// Record one reported power state (finding 58) and decide whether it is a
/// loss that tears down live work (finding 57): a loss state on a radio
/// that tears down, while no loss is in effect. Powered on ends the loss.
fn note_power<B>(inner: &Inner<B>, state: AdapterPowerState) -> Option<AdapterLossCause> {
    let mut facts = lock_std(&inner.adapter_facts);
    facts.power = Some(state);
    if state == AdapterPowerState::PoweredOn {
        facts.lost = false;
        facts.removed = false;
        return None;
    }
    let cause = AdapterLossCause::from_power(state)?;
    if facts.lost {
        return None;
    }
    facts.lost = true;
    inner.teardown_on_loss.then_some(cause)
}

/// Wake [`DesktopCentral::await_usable_adapter`] waiters after a fact
/// changed without a power report (authorization, presence).
fn wake_state_waiters<B>(inner: &Inner<B>) {
    let state = lock_std(&inner.adapter_facts)
        .power
        .unwrap_or(AdapterPowerState::Unknown);
    let sequence = inner.adapter_sequence.fetch_add(1, Ordering::SeqCst) + 1;
    let event = AdapterEvent { sequence, state };
    let _ = inner.adapter.send(event);
    inner.signal(CentralSignal::Adapter(event));
}

/// The adapter facts at open, read from the radio when it has an adapter
/// gate (a gate-less radio reads nothing). A fact the radio cannot report
/// stays unreported; a failed read is logged and stays unreported, so
/// admission never refuses on it.
fn reported_fact<T>(
    what: &str,
    outcome: Result<Result<T, DesktopError>, tokio::time::error::Elapsed>,
) -> Option<T> {
    match outcome {
        Ok(Ok(value)) => Some(value),
        Ok(Err(error)) => {
            if error.code() != BleErrorCode::CapabilityUnsupported {
                eprintln!(
                    "ubm-desktop: adapter {what} unread at open: {}",
                    error.detail().unwrap_or(error.code_str())
                );
            }
            None
        }
        Err(_) => {
            eprintln!("ubm-desktop: adapter {what} read at open timed out");
            None
        }
    }
}

async fn seed_adapter_facts<B: RadioBoundary>(
    boundary: &B,
    admission: AdmissionPolicy,
) -> AdapterFacts {
    let mut facts = AdapterFacts {
        power: None,
        authorization: None,
        removed: false,
        lost: false,
    };
    if admission == AdmissionPolicy::LifecycleOnly {
        return facts;
    }
    facts.power = reported_fact(
        "state",
        tokio::time::timeout(LIVENESS_CLEANUP, boundary.adapter_state()).await,
    );
    facts.authorization = reported_fact(
        "authorization",
        tokio::time::timeout(LIVENESS_CLEANUP, boundary.adapter_authorization()).await,
    );
    facts.lost = facts.power.is_some_and(AdapterPowerState::is_loss);
    facts
}

/// The attachment after reset `index` of the central opened as `ordinal`:
/// same backend instance and adapter, new attachment, backend and adapter
/// generations.
fn next_attachment(
    previous: &AttachmentTuple,
    ordinal: u64,
    index: u64,
) -> Result<AttachmentTuple, DesktopError> {
    Ok(AttachmentTuple::new(
        AttachmentId::new(format!("desktop-attachment-{ordinal}-r{index}"))
            .map_err(DesktopError::from)?,
        previous.backend_instance_id().clone(),
        BackendGeneration::new(format!("desktop-backend-gen-{ordinal}-r{index}"))
            .map_err(DesktopError::from)?,
        previous.adapter_id().clone(),
        AdapterGeneration::new(format!("desktop-adapter-gen-{ordinal}-r{index}"))
            .map_err(DesktopError::from)?,
    ))
}

/// Tear down everything live on a lost adapter (finding 57), in the legacy
/// order (CoreBluetooth `startAdapterLossCleanup`, WinRT
/// `handleAdapterState`, BlueZ `advanceBackendGeneration`):
///
/// 1. the core settles every live operation `Reset`, fails the scan,
///    clears the links and invalidates every subscription, under a new
///    attachment (backend and adapter generations advance);
/// 2. every link publishes [`LifecycleKind::AdapterLost`], the owned scan
///    ends aborted, routing drops, and every waiting driver wakes and
///    answers `operation.reset`;
/// 3. the OS scan stop and link releases are asked for, bounded; what the
///    adapter no longer holds counts as released, anything else is named;
/// 4. one [`AdapterResetEvent`] reports it all.
async fn adapter_reset<B: RadioBoundary>(inner: &Arc<Inner<B>>, cause: AdapterLossCause) {
    let scan = inner.scan_slot().take();
    let peers: Vec<(String, String)> = inner
        .peers
        .lock()
        .await
        .iter()
        .map(|(peer_id, peer_key)| (peer_id.clone(), peer_key.clone()))
        .collect();
    let ended_subscriptions = inner.subscriptions.lock().await.len();
    let index = inner.resets.fetch_add(1, Ordering::SeqCst) + 1;
    let mut release_failures = Vec::new();
    let previous = lock_std(&inner.attachment).clone();
    let (current, cancelled_operations, links, events) = {
        let mut core = inner.core.lock().await;
        let links: Vec<(String, String, Generations)> = peers
            .iter()
            .filter(|(_, peer_key)| {
                core.connection_state(peer_key)
                    .is_some_and(|state| !state.is_terminal())
            })
            .map(|(peer_id, peer_key)| {
                (
                    peer_id.clone(),
                    peer_key.clone(),
                    Generations::of(&core, peer_key),
                )
            })
            .collect();
        lock_std(&inner.reset_ops).extend(core.live_operation_ids());
        if let Some(active) = &scan {
            retain_completed_scan(
                &inner.completed_scans,
                &active.id,
                OperationTerminalKind::Reset,
            );
        }
        let reset = next_attachment(&previous, inner.ordinal, index).and_then(|current| {
            let generation =
                Generation::new(format!("desktop-kernel-gen-{}-r{index}", inner.ordinal))
                    .map_err(DesktopError::from)?;
            let mut out = batch();
            let settled = core
                .handle_adapter_reset(current.clone(), generation, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            let _ = out.drain();
            Ok((current, settled))
        });
        recycle_observations(&mut core);
        let (current, settled) = match reset {
            Ok(done) => done,
            Err(error) => {
                // The core could not move to a new scope: the teardown
                // below still runs, and the failure is reported.
                inner.note_compensation_failure();
                release_failures.push(error);
                (previous.clone(), 0)
            }
        };
        *lock_std(&inner.attachment) = current.clone();
        let mut reset_peers = lock_std(&inner.reset_peers);
        let events: Vec<LifecycleEvent> = links
            .iter()
            .map(|(peer_id, peer_key, generation)| {
                reset_peers.insert(peer_key.clone());
                inner.stage_lifecycle(
                    peer_id,
                    peer_key,
                    Generations {
                        connection: generation.connection.clone(),
                        database: generation.database.clone(),
                    },
                    LifecycleKind::AdapterLost,
                )
            })
            .collect();
        drop(reset_peers);
        (current, settled, links, events)
    };
    for (peer_id, _) in &peers {
        clear_peer_routing(inner, peer_id).await;
    }
    lock_std(&inner.pairings).clear();
    let tickets = std::mem::take(&mut *lock_std(&inner.tickets));
    for ticket in tickets {
        ticket.mark_reset();
    }
    for event in events {
        inner.signal(CentralSignal::Lifecycle(event));
    }
    let detail = format!("the adapter was lost ({})", cause.as_str());
    if let Some(active) = &scan {
        parity::publish_scan_terminal(inner, &active.id, true, &detail);
        match tokio::time::timeout(COMPENSATION_TIMEOUT, inner.boundary.stop_scan()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                inner.note_compensation_failure();
                release_failures.push(error);
            }
            Err(_) => {
                inner.note_compensation_failure();
                release_failures.push(DesktopError::scan_stop_failed(
                    "the OS did not answer the scan stop after the adapter loss",
                ));
            }
        }
    }
    for (peer_id, _, _) in &links {
        match tokio::time::timeout(COMPENSATION_TIMEOUT, inner.boundary.disconnect(peer_id)).await {
            // The adapter took the peer with it: nothing is left to release.
            Ok(Ok(())) => {}
            Ok(Err(error)) if error.code() == BleErrorCode::PeerNotFound => {}
            Ok(Err(error)) => {
                inner.note_compensation_failure();
                release_failures.push(error);
            }
            Err(_) => {
                inner.note_compensation_failure();
                release_failures.push(
                    contract_error(
                        BleErrorCode::OperationTimedOut,
                        BleErrorDomain::Connection,
                        "connection.disconnect",
                    )
                    .with_detail(format!(
                        "the OS did not release the link to {peer_id} after the adapter loss"
                    )),
                );
            }
        }
    }
    let event = AdapterResetEvent {
        sequence: inner.reset_sequence.fetch_add(1, Ordering::SeqCst) + 1,
        cause,
        previous,
        current,
        cancelled_operations,
        ended_scan: scan.map(|active| active.id),
        released_links: links.into_iter().map(|(peer_id, _, _)| peer_id).collect(),
        ended_subscriptions,
        release_failures,
    };
    let _ = inner.reset_events.send(event.clone());
    inner.signal(CentralSignal::AdapterReset(event));
}

/// Settle an owned scan the OS ended on its own (source closed, watcher
/// stopped): the platform event, the settlement, the retained ticket
/// (R15), the release, and the marker — so the owner is released even
/// when the OS never confirms a stop.
async fn settle_os_ended_scan<B>(
    inner: &Arc<Inner<B>>,
    id: &OperationId,
    contender: ContenderKind,
) {
    let mut core = inner.core.lock().await;
    let mut out = batch();
    let _ = core.note_scan_platform(
        id,
        ubm_core::central::ScanPlatformEvent::SourceClosed,
        now_ms(),
        &mut out,
    );
    let _ = out.drain();
    let _ = core.settle_op(id, contender, true, 0, now_ms(), &mut out);
    let _ = out.drain();
    // R15: retain the completed ticket between settlement and release
    // (first writer wins).
    if let Some(kind) = terminal_kind_of(&core, id) {
        retain_completed_scan(&inner.completed_scans, id, kind);
    }
    report_terminal_release(&mut core, id, true, None);
    recycle_observations(&mut core);
    drop(core);
    // No stale owner: clear the marker when it still names this session so
    // no scan reads owned after the source ended (a newer scan's marker
    // stays).
    let mut slot = inner.scan_slot();
    if slot.as_ref().is_some_and(|active| active.id == *id) {
        *slot = None;
    }
}

async fn ingest_advertisement<B: RadioBoundary>(inner: &Arc<Inner<B>>, snapshot: PeerSnapshot) {
    // L8: resolve under the core lock, then drop the guard before the map
    // insert — the central lock is never held across the peers await, and
    // the insert stays out of the core critical section.
    let peer_key = {
        let mut core = inner.core.lock().await;
        // Platform-guid is the desktop peer identity: btleplug exposes the
        // address type opaquely per platform, so address targeting stays a
        // narrow-OS-adapter gap rather than a guessed domain.
        core.resolve_peer("platform-guid", &snapshot.id).ok()
    };
    if let Some(peer_key) = peer_key {
        inner
            .peers
            .lock()
            .await
            .insert(snapshot.id.clone(), peer_key);
    }
    // Finding 121: a sighting is an observation only while a scan runs,
    // and belongs to that scan (legacy backends reported advertisements
    // only while scanning). Outside a scan it only makes the peer known.
    let Some(scan) = inner.scan_slot().as_ref().map(|active| active.id.clone()) else {
        return;
    };
    // F22: every advertisement stays pollable with its full facts until the
    // host takes it. Bounded FIFO: past the cap the oldest evicts and the
    // eviction counts, so a slow host sees loss explicitly.
    let signal = inner.observer.as_ref().map(|_| snapshot.clone());
    {
        let mut queue = inner.advertisements.lock().await;
        let sighting = QueuedSighting {
            snapshot,
            scan,
            received: Instant::now(),
        };
        if push_evicting(&mut queue, sighting, ADVERTISEMENT_CAP) {
            inner.advertisement_drops.fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(snapshot) = signal {
        inner.signal(CentralSignal::Advertisement(snapshot));
    }
}

/// One sighting queued for the host (finding 121): the scan that was live
/// when it arrived and when the central received it.
#[derive(Debug, Clone)]
struct QueuedSighting {
    snapshot: PeerSnapshot,
    scan: OperationId,
    received: Instant,
}

/// One scan observation (finding 121): the sighting, the scan it belongs
/// to, and how long ago the central received it from the radio (measured
/// when taken), so the host stamps it on its own clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanObservation {
    pub snapshot: PeerSnapshot,
    pub scan_operation_id: OperationId,
    pub age: Duration,
}

/// Finding 120: while a scan runs, every known peripheral is observed again
/// as the OS's device state. A re-read the OS cannot answer is counted and
/// logged, never silent.
async fn refresh_known_peers<B: RadioBoundary>(inner: &Arc<Inner<B>>) {
    if inner.scan_slot().is_none() {
        return;
    }
    match inner.boundary.peers().await {
        Ok(peers) => {
            for mut snapshot in peers {
                snapshot.extras.source = crate::boundary::ObservationSource::DeviceState;
                ingest_advertisement(inner, snapshot).await;
            }
        }
        Err(error) => {
            inner
                .known_peer_refresh_failures
                .fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "ubm-desktop: known peripherals could not be re-read during a scan: {}",
                error.detail().unwrap_or(error.code_str())
            );
        }
    }
}

/// Push onto a bounded FIFO: at `cap` the oldest entry evicts first.
/// Answers whether one did, so the caller counts every eviction.
fn push_evicting<T>(queue: &mut VecDeque<T>, item: T, cap: usize) -> bool {
    let evicted = queue.len() >= cap && queue.pop_front().is_some();
    queue.push_back(item);
    evicted
}

async fn reconcile_connected<B: RadioBoundary>(inner: &Arc<Inner<B>>, peer_id: &str) {
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    if let Some(peer_key) = peer_key {
        let mut core = inner.core.lock().await;
        let _ = core.note_link_established(&peer_key);
    }
}

/// The OS reported the link down (PR210-11). A pending release completes
/// (`Released { requested: true }`, `Disconnected`); a live link is lost
/// (`LinkLost`, `Lost`). A link already terminal is a stale event for an
/// older generation and publishes nothing.
async fn reconcile_disconnected<B: RadioBoundary>(inner: &Arc<Inner<B>>, peer_id: &str) {
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    let Some(peer_key) = peer_key else {
        return;
    };
    clear_peer_routing(inner, peer_id).await;
    let event = {
        let mut core = inner.core.lock().await;
        let generation = Generations::of(&core, &peer_key);
        let kind = match core.connection_state(&peer_key) {
            Some(ConnectionState::Disconnecting) => core
                .note_link_released(&peer_key)
                .ok()
                .map(|()| LifecycleKind::Released { requested: true }),
            Some(ConnectionState::Connected | ConnectionState::Connecting) => {
                let mut out = batch();
                core.note_peer_loss(&peer_key, now_ms(), &mut out)
                    .ok()
                    .map(|_| LifecycleKind::LinkLost)
            }
            _ => None,
        };
        kind.map(|kind| inner.stage_lifecycle(peer_id, &peer_key, generation, kind))
    };
    if let Some(event) = event {
        inner.signal(CentralSignal::Lifecycle(event));
    }
}

/// Route one radio notification into its per-instance hub with the full
/// value bytes (M2). Events without routing (unknown or unsubscribed
/// instance) drop on the floor: the hub never receives unattributable
/// bytes. Events whose install-time epoch no longer matches the live
/// routing are stale queue drainage from before a disconnect or service
/// change, and drop the same way even when the instance key matches again
/// (F10): the epoch is captured at forwarder install, never minted here.
async fn deliver<B: RadioBoundary>(
    inner: &Arc<Inner<B>>,
    scope: InstanceKey,
    epoch: u64,
    value: Vec<u8>,
) {
    let routed = inner.subscriptions.lock().await.get(&scope).copied();
    let Some((path_index, routing_epoch)) = routed else {
        return;
    };
    if epoch != routing_epoch {
        return;
    }
    let delivered = {
        let mut core = inner.core.lock().await;
        core.deliver_notification_value(path_index, &value).is_ok()
    };
    if delivered && inner.observer.is_some() {
        inner.signal(CentralSignal::Value { scope, value });
    }
}

/// Account notifications the OS broadcast lost for one subscription
/// (vendored btleplug patch 10) on its hub, by each consumer's overflow
/// policy (`error` ends with an overflow terminal counting them; lossy
/// policies count them). Same routing and epoch rule as [`deliver`]: a loss
/// reported for a routing that no longer exists is stale.
async fn account_lost_notifications<B: RadioBoundary>(
    inner: &Arc<Inner<B>>,
    scope: InstanceKey,
    epoch: u64,
    lost: u64,
) {
    let routed = inner.subscriptions.lock().await.get(&scope).copied();
    let Some((path_index, routing_epoch)) = routed else {
        return;
    };
    if epoch != routing_epoch {
        return;
    }
    let mut core = inner.core.lock().await;
    if let Err(error) = core.deliver_upstream_loss(path_index, lost) {
        drop(core);
        inner.note_compensation_failure();
        eprintln!(
            "ubm-desktop: {lost} lost notifications could not be accounted on {scope:?}: {error}"
        );
    }
}

/// Invalidate generations when the OS reports a changed GATT database
/// (L6): stale paths must fail closed and require rediscovery instead of
/// serving re-reads through dead handles. Routing drops alongside the
/// core hubs so late values cannot reach invalidated consumers. A live
/// connection publishes `ServicesChanged` (PR210-11).
async fn services_changed_invalidated<B: RadioBoundary>(inner: &Arc<Inner<B>>, peer_id: &str) {
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    let Some(peer_key) = peer_key else {
        return;
    };
    // Finding 40: the routing goes, but every enabled instance's OS CCCD
    // may still be live — keep each one releasable by its instance.
    let enabled: Vec<InstanceKey> = {
        let subscriptions = inner.subscriptions.lock().await;
        let failed = inner.failed_disables.lock().await;
        subscriptions
            .keys()
            .chain(failed.iter())
            .filter(|key| key.0 == peer_id)
            .cloned()
            .collect()
    };
    clear_peer_routing(inner, peer_id).await;
    lock_std(&inner.retained_enablements).extend(enabled);
    let event = {
        let mut core = inner.core.lock().await;
        let generation = Generations::of(&core, &peer_key);
        let live = core
            .connection_state(&peer_key)
            .is_some_and(|state| !state.is_terminal());
        let _ = core.services_changed(&peer_key);
        let _ = core.require_rediscovery(&peer_key);
        live.then(|| {
            inner.stage_lifecycle(
                peer_id,
                &peer_key,
                generation,
                LifecycleKind::ServicesChanged,
            )
        })
    };
    if let Some(event) = event {
        inner.signal(CentralSignal::Lifecycle(event));
    }
}

/// Adapter behavior over the mocked boundary: scan ownership and cleanup,
/// peer/connection/GATT mapping with contract identities, partial
/// discovery failures, descriptor paths, subscription sharing, and
/// cancellation. No radio is touched; the fake boundary is the only
/// evidence source on this host.
#[cfg(test)]
mod adapter_tests {
    use std::time::Duration;

    use ubm_core::central::{ConnectionState, ConsumerState, ScanSessionState};

    use crate::boundary::{
        CharacteristicSnapshot, DescriptorSnapshot, FakeRadio, FaultOp, PeerSnapshot,
        PropertyFlags, RadioEvent, ServiceSnapshot,
    };

    use super::DesktopCentral;
    use crate::op_control::OpControl;

    /// Stop whatever scan the central owns (the pre-PR210-09 test shape):
    /// `NotActive` when none is owned.
    async fn stop_owned_scan<B: crate::boundary::RadioBoundary>(
        central: &DesktopCentral<B>,
    ) -> Result<crate::central::ScanStop, crate::errors::DesktopError> {
        match central.active_scan_id() {
            Some(id) => central.stop_scan(&id, OpControl::unbounded()).await,
            None => Ok(crate::central::ScanStop::NotActive),
        }
    }

    const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
    const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";
    const BATTERY_SERVICE: &str = "0000180f-0000-1000-8000-00805f9b34fb";
    const BATTERY_LEVEL: &str = "00002a19-0000-1000-8000-00805f9b34fb";
    const USER_DESCRIPTION: &str = "00002901-0000-1000-8000-00805f9b34fb";
    const BODY_SENSOR_LOCATION: &str = "00002a38-0000-1000-8000-00805f9b34fb";
    const HEART_RATE_CONTROL_POINT: &str = "00002a39-0000-1000-8000-00805f9b34fb";

    fn notify_props() -> PropertyFlags {
        PropertyFlags {
            read: true,
            write: false,
            write_without_response: false,
            notify: true,
            indicate: false,
        }
    }

    fn rw_props() -> PropertyFlags {
        PropertyFlags {
            read: true,
            write: true,
            write_without_response: true,
            notify: false,
            indicate: false,
        }
    }

    fn hrm_service() -> ServiceSnapshot {
        ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![CharacteristicSnapshot {
                uuid: HRM_MEASUREMENT.to_owned(),
                occurrence: 0,
                properties: notify_props(),
                descriptors: vec![DescriptorSnapshot {
                    uuid: USER_DESCRIPTION.to_owned(),
                    occurrence: 0,
                }],
            }],
        }
    }

    /// One service carrying two same-UUID notify characteristics (wrist +
    /// chest strap): occurrence is the only instance identity.
    fn duplicate_hrm_service() -> ServiceSnapshot {
        ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![
                CharacteristicSnapshot {
                    uuid: HRM_MEASUREMENT.to_owned(),
                    occurrence: 0,
                    properties: notify_props(),
                    descriptors: Vec::new(),
                },
                CharacteristicSnapshot {
                    uuid: HRM_MEASUREMENT.to_owned(),
                    occurrence: 1,
                    properties: notify_props(),
                    descriptors: Vec::new(),
                },
            ],
        }
    }

    fn battery_service() -> ServiceSnapshot {
        ServiceSnapshot {
            uuid: BATTERY_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![CharacteristicSnapshot {
                uuid: BATTERY_LEVEL.to_owned(),
                occurrence: 0,
                properties: rw_props(),
                descriptors: Vec::new(),
            }],
        }
    }

    fn advertisement(peer_id: &str) -> RadioEvent {
        RadioEvent::Advertisement(PeerSnapshot {
            id: peer_id.to_owned(),
            address: None,
            service_uuids: vec![HRM_SERVICE.to_owned()],
            rssi: Some(-60),
            local_name: None,
            manufacturer_data: Vec::new(),
            service_data: Vec::new(),
            tx_power_level: None,
            extras: crate::boundary::AdvertisementExtras::default(),
        })
    }

    async fn open() -> DesktopCentral<FakeRadio> {
        DesktopCentral::open(FakeRadio::new(), "test-host")
            .await
            .expect("open central")
    }

    async fn wait_peer(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
        for _ in 0..200 {
            if central.peer_key_for(peer_id).await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for peer {peer_id}");
    }

    fn hrm_selector(service_occurrence: u64) -> crate::central::PathSelector {
        hrm_instance_selector(service_occurrence, 0)
    }

    fn hrm_instance_selector(
        service_occurrence: u64,
        characteristic_occurrence: u64,
    ) -> crate::central::PathSelector {
        DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(service_occurrence),
            Some(HRM_MEASUREMENT),
            Some(characteristic_occurrence),
            None,
            None,
        )
        .expect("selector")
    }

    fn notification(peer_id: &str, characteristic_occurrence: u64, value: Vec<u8>) -> RadioEvent {
        notification_epoch(peer_id, characteristic_occurrence, value, 0)
    }

    fn notification_epoch(
        peer_id: &str,
        characteristic_occurrence: u64,
        value: Vec<u8>,
        epoch: u64,
    ) -> RadioEvent {
        RadioEvent::Notification {
            peer_id: peer_id.to_owned(),
            service_uuid: HRM_SERVICE.to_owned(),
            service_occurrence: 0,
            characteristic_uuid: HRM_MEASUREMENT.to_owned(),
            characteristic_occurrence,
            epoch,
            value,
        }
    }

    #[tokio::test]
    async fn scan_ingests_advertisements_and_cleans_up() {
        let central = open().await;
        central
            .start_scan("owner-a", &[], OpControl::budget_ms(5000))
            .await
            .expect("start scan");
        assert!(central.has_active_scan().await);
        central.boundary().push_event(advertisement("peer-1"));
        wait_peer(&central, "peer-1").await;
        stop_owned_scan(&central).await.expect("stop scan");
        assert!(!central.has_active_scan().await);
        assert!(
            !central.boundary().scan_active(),
            "OS scan stopped on cleanup"
        );
        let calls = central.boundary().calls();
        let start = calls
            .iter()
            .position(|call| call == "start_scan")
            .expect("start recorded");
        let stop = calls
            .iter()
            .position(|call| call == "stop_scan")
            .expect("stop recorded");
        assert!(start < stop, "stop follows start");
        assert_eq!(
            calls.iter().filter(|call| *call == "stop_scan").count(),
            1,
            "cleanup stops the OS scan exactly once"
        );
    }

    #[tokio::test]
    async fn failed_scan_start_releases_the_owner() {
        let central = open().await;
        central
            .boundary()
            .fail_next(FaultOp::StartScan, "os denied");
        let error = central
            .start_scan("owner-a", &[], OpControl::budget_ms(5000))
            .await
            .expect_err("scripted start failure");
        assert_eq!(error.code_str(), "scan.start-failed");
        assert!(!central.has_active_scan().await);
        assert!(
            !central.boundary().calls().contains(&"stop_scan".to_owned()),
            "no stop without a start"
        );
        // The owner is released: a second start succeeds.
        central
            .start_scan("owner-a", &[], OpControl::budget_ms(5000))
            .await
            .expect("retry after failed start");
        stop_owned_scan(&central).await.expect("stop");
    }

    #[tokio::test]
    async fn second_scan_owner_is_rejected_without_radio_effect() {
        let central = open().await;
        central
            .start_scan("owner-a", &[], OpControl::budget_ms(5000))
            .await
            .expect("first");
        let error = central
            .start_scan("owner-b", &[], OpControl::budget_ms(5000))
            .await
            .expect_err("second owner rejected");
        assert_eq!(error.code_str(), "scan.already-active");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "start_scan")
                .count(),
            1,
            "rejected arbitration never reaches the radio"
        );
        stop_owned_scan(&central).await.expect("stop");
    }

    #[tokio::test]
    async fn event_source_close_settles_the_owned_scan() {
        let central = open().await;
        let session = central
            .start_scan("owner-a", &[], OpControl::budget_ms(5000))
            .await
            .expect("start");
        central.boundary().close_events();
        for _ in 0..200 {
            let state = central
                .with_core(|core| core.scan_session_state(session.operation_id()))
                .await;
            if state == Some(ScanSessionState::Stopped) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            central
                .with_core(|core| core.scan_session_state(session.operation_id()))
                .await,
            Some(ScanSessionState::Stopped),
            "source close releases the owner"
        );
        // Late stop stays a safe no-op cleanup, not a second settlement.
        stop_owned_scan(&central).await.expect("late stop");
    }

    #[tokio::test]
    async fn connect_does_not_share_without_rule() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-1"));
        let handle = central
            .connect("peer-1", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        assert!(!handle.peer_key.is_empty());
        assert!(handle.connection_generation.is_some());
        let state = central
            .with_core(|core| core.connection_state(&handle.peer_key))
            .await;
        assert_eq!(state, Some(ConnectionState::Connected));
        // No sharing support: a second lease is rejected before any radio call.
        let before = central.boundary().calls().len();
        let error = central
            .connect("peer-1", "lease-b", OpControl::budget_ms(5000))
            .await
            .expect_err("second lease rejected");
        assert_eq!(error.code_str(), "connection.already-owned");
        assert_eq!(
            central.boundary().calls().len(),
            before,
            "no radio on rejection"
        );
        central
            .disconnect("peer-1", "lease-a", OpControl::unbounded())
            .await
            .expect("disconnect");
        let state = central
            .with_core(|core| core.connection_state(&handle.peer_key))
            .await;
        assert_eq!(state, Some(ConnectionState::Disconnected));
    }

    #[tokio::test]
    async fn connect_failure_marks_loss_and_cleans_half_open_link() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-9"));
        central.boundary().fail_next(FaultOp::Connect, "os refused");
        let error = central
            .connect("peer-9", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect_err("scripted connect failure");
        assert_eq!(error.code_str(), "connection.failed");
        let peer_key = central.peer_key_for("peer-9").await.expect("peer known");
        let state = central
            .with_core(|core| core.connection_state(&peer_key))
            .await;
        assert_eq!(
            state,
            Some(ConnectionState::Lost),
            "Connecting -> Lost, no resurrection"
        );
        assert!(
            central
                .boundary()
                .calls()
                .contains(&"disconnect".to_owned()),
            "half-open OS link cleaned up"
        );
    }

    #[tokio::test]
    async fn disconnect_radio_failure_stays_disconnecting() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-2"));
        let handle = central
            .connect("peer-2", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        central
            .boundary()
            .fail_next(FaultOp::Disconnect, "os stuck");
        let error = central
            .disconnect("peer-2", "lease-a", OpControl::unbounded())
            .await
            .expect_err("scripted disconnect failure");
        assert_eq!(error.operation(), "connection.disconnect");
        // Not reported clean: the link never reaches Disconnected.
        let state = central
            .with_core(|core| core.connection_state(&handle.peer_key))
            .await;
        assert_eq!(
            state,
            Some(ConnectionState::Disconnecting),
            "failed cleanup retains ownership"
        );
    }

    /// PR210-24 (decision 1): a disconnect that outlives its budget reports
    /// `operation.timed-out` from its own answer — no post-timeout link
    /// probe — and keeps the release `Disconnecting` with the caller's lease.
    /// The OS finishing the release later arrives as a radio event, becomes
    /// a `Released { requested: true }` lifecycle event, and a retry answers
    /// `AlreadyReleased` without another radio call.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn disconnect_timeout_then_released_event_then_retry_already_released() {
        use crate::central::{LifecycleKind, LinkRelease};

        let central = open().await;
        central.boundary().push_event(advertisement("peer-9"));
        wait_peer(&central, "peer-9").await;
        let handle = central
            .connect("peer-9", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        let mut events = central.lifecycle_events();
        central.boundary().block_op(FaultOp::Disconnect);
        let error = central
            .disconnect("peer-9", "lease-a", OpControl::budget_ms(100))
            .await
            .expect_err("held release outlives the budget");
        assert_eq!(error.code_str(), "operation.timed-out");
        assert_eq!(
            error.retryability(),
            crate::errors::Retryability::CallerDecides
        );
        assert_eq!(
            central
                .with_core(|core| core.connection_state(&handle.peer_key))
                .await,
            Some(ConnectionState::Disconnecting),
            "timed-out release keeps ownership"
        );
        assert!(
            central
                .with_core(|core| core.holds_lease(&handle.peer_key, "lease-a"))
                .await,
            "the caller's lease is retained for the retry"
        );
        // The OS completes the release behind the abandoned call.
        central
            .boundary()
            .push_event(RadioEvent::Disconnected("peer-9".to_owned()));
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("lifecycle event arrives")
            .expect("event");
        assert_eq!(event.peer_id, "peer-9");
        assert_eq!(event.kind, LifecycleKind::Released { requested: true });
        assert_eq!(event.connection_generation, handle.connection_generation);
        assert_eq!(
            central
                .with_core(|core| core.connection_state(&handle.peer_key))
                .await,
            Some(ConnectionState::Disconnected)
        );
        let disconnect_calls = |central: &DesktopCentral<FakeRadio>| {
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "disconnect")
                .count()
        };
        let before = disconnect_calls(&central);
        let retry = central
            .disconnect("peer-9", "lease-a", OpControl::budget_ms(1000))
            .await
            .expect("retry answers from the core");
        assert_eq!(retry, LinkRelease::AlreadyReleased);
        assert_eq!(disconnect_calls(&central), before, "no radio call on retry");
        central.boundary().unblock_op(FaultOp::Disconnect);
    }

    #[tokio::test]
    async fn disconnect_timeout_with_live_link_reports_timeout() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-10"));
        let handle = central
            .connect("peer-10", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        central.boundary().block_op(FaultOp::Disconnect);
        let worker = central.clone();
        let pending = tokio::spawn(async move {
            worker
                .disconnect("peer-10", "lease-a", OpControl::budget_ms(300))
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!pending.is_finished(), "disconnect pends on the held radio");
        let outcome = tokio::time::timeout(Duration::from_secs(10), pending)
            .await
            .expect("disconnect settles")
            .expect("disconnect task");
        let error = outcome.expect_err("live link still times out");
        assert_eq!(error.operation(), "connection.disconnect");
        let state = central
            .with_core(|core| core.connection_state(&handle.peer_key))
            .await;
        assert_eq!(
            state,
            Some(ConnectionState::Disconnecting),
            "uncertain release retains ownership"
        );
    }

    #[tokio::test]
    async fn discovery_registers_duplicate_uuids_by_occurrence() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-3"));
        central
            .connect("peer-3", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-3", vec![hrm_service(), hrm_service()]);
        let report = central
            .discover("peer-3", "lease-a", OpControl::unbounded())
            .await
            .expect("discover");
        // Two services x (service + characteristic + descriptor).
        assert_eq!(report.paths_registered, 6);
        // Same UUID twice: occurrence selects each instance.
        let peer_key = central.peer_key_for("peer-3").await.expect("peer");
        for occurrence in [0u64, 1u64] {
            let selector = hrm_selector(occurrence);
            central
                .with_core(|core| core.resolve_path(&peer_key, &selector))
                .await
                .expect("occurrence resolves");
        }
        // Descriptor path reads through the validated descriptor level.
        let descriptor_selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            Some(USER_DESCRIPTION),
            Some(0),
        )
        .expect("descriptor selector");
        let value = central
            .read_descriptor("peer-3", &descriptor_selector, OpControl::budget_ms(5000))
            .await
            .expect("descriptor read");
        assert_eq!(value, vec![0x01]);
        // Characteristic read returns the fake payload.
        let value = central
            .read("peer-3", &hrm_selector(1), OpControl::budget_ms(5000))
            .await
            .expect("read");
        assert_eq!(value, vec![0x42]);
    }

    #[tokio::test]
    async fn read_after_peer_loss_fails_without_radio() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-4"));
        central
            .connect("peer-4", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-4", vec![battery_service()]);
        central
            .discover("peer-4", "lease-a", OpControl::unbounded())
            .await
            .expect("discover");
        central.remote_peer_loss("peer-4").await.expect("loss");
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let error = central
            .read("peer-4", &selector, OpControl::budget_ms(5000))
            .await
            .expect_err("stale path never dispatches");
        // The link gate fails the read closed (`lifecycle.invalid-state`:
        // the Invalid database never reaches property validation). What the
        // adapter pins is the fail-closed discipline, not the core's code
        // choice: no radio dispatch, attributed error.
        assert_eq!(error.code_str(), "lifecycle.invalid-state");
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"read_characteristic".to_owned()),
            "no radio call for a stale path"
        );
    }

    #[tokio::test]
    async fn write_without_measured_mtu_fails_closed() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-8"));
        central
            .connect("peer-8", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-8", vec![battery_service()]);
        central
            .discover("peer-8", "lease-a", OpControl::unbounded())
            .await
            .expect("discover");
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        // No MTU scripted: the maximum is unmeasured, never guessed.
        let error = central
            .write(
                "peer-8",
                &selector,
                vec![1],
                "with-response",
                OpControl::budget_ms(5000),
            )
            .await
            .expect_err("unmeasured maximum fails closed");
        assert_eq!(error.code_str(), "capability.unavailable");
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"write_characteristic".to_owned()),
            "no radio call without a measured maximum"
        );
    }

    #[tokio::test]
    async fn long_write_is_rejected_up_front() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-5"));
        central
            .connect("peer-5", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        central.boundary().set_mtu("peer-5", 23);
        central
            .boundary()
            .set_services("peer-5", vec![battery_service()]);
        central
            .discover("peer-5", "lease-a", OpControl::unbounded())
            .await
            .expect("discover");
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let error = central
            .write(
                "peer-5",
                &selector,
                vec![1, 2, 3],
                "long-write",
                OpControl::budget_ms(5000),
            )
            .await
            .expect_err("long-write has no radio path");
        assert_eq!(error.code_str(), "capability.limited");
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"write_characteristic".to_owned()),
            "never silently single-written"
        );
        // Ordinary write modes still flow.
        central
            .write(
                "peer-5",
                &selector,
                vec![1, 2, 3],
                "with-response",
                OpControl::budget_ms(5000),
            )
            .await
            .expect("plain write");
    }

    #[tokio::test]
    async fn subscription_sharing_keeps_one_cccd() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-6"));
        central
            .connect("peer-6", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-6", vec![hrm_service()]);
        central
            .discover("peer-6", "lease-a", OpControl::unbounded())
            .await
            .expect("discover");
        let selector = hrm_selector(0);
        central
            .subscribe(
                "peer-6",
                &selector,
                "consumer-a",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe a");
        central
            .subscribe(
                "peer-6",
                &selector,
                "consumer-b",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe b");
        let peer_key = central.peer_key_for("peer-6").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        assert!(
            central
                .with_core(|core| core.physical_cccd_enabled(path_index))
                .await,
            "physical CCCD enabled"
        );
        assert_eq!(
            central
                .with_core(|core| core.consumer_state(path_index, "consumer-b"))
                .await,
            Some(ConsumerState::Ready),
            "second consumer shares the physical enablement"
        );
        // First removal keeps the other consumer's live CCCD.
        let disabled = central
            .unsubscribe("peer-6", &selector, "consumer-a", OpControl::unbounded())
            .await
            .expect("unsubscribe a");
        assert!(!disabled, "CCCD stays for consumer-b");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            1,
            "no physical toggle while a consumer remains"
        );
        // Last removal disables the physical CCCD.
        let disabled = central
            .unsubscribe("peer-6", &selector, "consumer-b", OpControl::unbounded())
            .await
            .expect("unsubscribe b");
        assert!(disabled, "last removal issues the physical disable");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            2,
            "enable once, disable once"
        );
    }

    #[tokio::test]
    async fn notifications_reach_the_hub_stream() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-10"));
        central
            .connect("peer-10", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-10", vec![hrm_service()]);
        central
            .discover("peer-10", "lease-a", OpControl::unbounded())
            .await
            .expect("discover");
        let selector = hrm_selector(0);
        central
            .subscribe(
                "peer-10",
                &selector,
                "consumer-a",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe");
        let peer_key = central.peer_key_for("peer-10").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        // Overflow past the item bound under the error policy surfaces
        // exactly one terminal: values flow radio -> hub -> stream.
        for _ in 0..70 {
            central
                .boundary()
                .push_event(notification("peer-10", 0, vec![0x06, 0x40]));
        }
        for _ in 0..400 {
            let terminal = central
                .with_core_mut(|core| core.take_terminal(path_index, "consumer-a"))
                .await;
            if terminal.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            central
                .with_core_mut(|core| core.take_terminal(path_index, "consumer-a"))
                .await
                .is_none(),
            "terminal surfaces exactly once"
        );
        // M2: admitted values buffered before the overflow stay observable
        // through the take API — payloads are delivered, not dropped.
        let first = central
            .take_notification("peer-10", &selector, "consumer-a")
            .await
            .expect("take");
        assert_eq!(first, Some(vec![0x06, 0x40]), "buffered value observable");
        // M5 orphan-disable: removing the terminal (post-overflow Failed)
        // consumer releases the live CCCD instead of leaking it.
        let disabled = central
            .unsubscribe("peer-10", &selector, "consumer-a", OpControl::unbounded())
            .await
            .expect("unsubscribe");
        assert!(disabled, "terminal removal issues the orphan disable");
        assert!(
            !central
                .with_core(|core| core.physical_cccd_enabled(path_index))
                .await,
            "orphan CCCD released"
        );
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            2,
            "enable once, orphan-disable once"
        );
    }

    #[tokio::test]
    async fn failed_subscribe_leaves_no_live_cccd() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-7"));
        central
            .connect("peer-7", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-7", vec![hrm_service()]);
        central
            .discover("peer-7", "lease-a", OpControl::unbounded())
            .await
            .expect("discover");
        let selector = hrm_selector(0);
        central
            .boundary()
            .fail_next(FaultOp::Subscribe, "cccd refused");
        let error = central
            .subscribe(
                "peer-7",
                &selector,
                "consumer-a",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect_err("scripted subscribe failure");
        assert_eq!(error.code_str(), "gatt.subscribe-failed");
        let peer_key = central.peer_key_for("peer-7").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        assert!(
            !central
                .with_core(|core| core.physical_cccd_enabled(path_index))
                .await,
            "no live CCCD after failed subscribe"
        );
    }

    #[tokio::test]
    async fn cancel_unknown_operation_fails_closed() {
        use ubm_core::contracts::OperationId;
        let central = open().await;
        let unknown = OperationId::new("central-op-9999").expect("id");
        let error = central
            .cancel_operation(&unknown)
            .await
            .expect_err("unknown op cannot cancel");
        assert!(!error.operation().is_empty(), "attributed error");
    }

    /// Connect, discover, and leave `peer_id` ready for GATT ops.
    async fn ready_peer(
        central: &DesktopCentral<FakeRadio>,
        peer_id: &str,
        services: Vec<ServiceSnapshot>,
    ) {
        central.boundary().push_event(advertisement(peer_id));
        central
            .connect(peer_id, "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        central.boundary().set_services(peer_id, services);
        central
            .discover(peer_id, "lease-a", OpControl::unbounded())
            .await
            .expect("discover");
    }

    #[tokio::test]
    async fn h1_duplicate_uuid_instances_route_per_instance() {
        use ubm_core::central::ConsumerState;

        let central = open().await;
        ready_peer(&central, "peer-h1", vec![duplicate_hrm_service()]).await;
        // Per-instance payloads: wrist on occurrence 0, chest on 1.
        central.boundary().set_characteristic_value(
            "peer-h1",
            HRM_SERVICE,
            0,
            HRM_MEASUREMENT,
            0,
            vec![0x77],
        );
        central.boundary().set_characteristic_value(
            "peer-h1",
            HRM_SERVICE,
            0,
            HRM_MEASUREMENT,
            1,
            vec![0xc4, 0x35],
        );
        let wrist = central
            .read(
                "peer-h1",
                &hrm_instance_selector(0, 0),
                OpControl::budget_ms(5000),
            )
            .await
            .expect("read wrist");
        assert_eq!(wrist, vec![0x77], "occurrence 0 reads instance 0");
        let chest = central
            .read(
                "peer-h1",
                &hrm_instance_selector(0, 1),
                OpControl::budget_ms(5000),
            )
            .await
            .expect("read chest");
        assert_eq!(
            chest,
            vec![0xc4, 0x35],
            "occurrence 1 reads instance 1, not instance 0"
        );
        // Two same-UUID subscriptions keep distinct routing: each
        // instance's values reach only its own consumer.
        central
            .subscribe(
                "peer-h1",
                &hrm_instance_selector(0, 0),
                "wrist-app",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe wrist");
        central
            .subscribe(
                "peer-h1",
                &hrm_instance_selector(0, 1),
                "chest-app",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe chest");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            2,
            "two instances enable twice, never one shared forwarder"
        );
        central
            .boundary()
            .push_event(notification("peer-h1", 0, vec![0x01]));
        central
            .boundary()
            .push_event(notification("peer-h1", 1, vec![0x02]));
        let mut wrist_value = None;
        let mut chest_value = None;
        for _ in 0..200 {
            if wrist_value.is_none() {
                wrist_value = central
                    .take_notification("peer-h1", &hrm_instance_selector(0, 0), "wrist-app")
                    .await
                    .expect("take wrist");
            }
            if chest_value.is_none() {
                chest_value = central
                    .take_notification("peer-h1", &hrm_instance_selector(0, 1), "chest-app")
                    .await
                    .expect("take chest");
            }
            if wrist_value.is_some() && chest_value.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(wrist_value, Some(vec![0x01]), "wrist values reach wrist");
        assert_eq!(chest_value, Some(vec![0x02]), "chest values reach chest");
        let peer_key = central.peer_key_for("peer-h1").await.expect("peer");
        for (occurrence, consumer) in [(0u64, "wrist-app"), (1u64, "chest-app")] {
            let path = central
                .with_core(|core| {
                    core.resolve_path(&peer_key, &hrm_instance_selector(0, occurrence))
                        .expect("path")
                })
                .await;
            assert_eq!(
                central
                    .with_core(|core| core.consumer_state(path, consumer))
                    .await,
                Some(ConsumerState::Ready),
                "both consumers live on their own hub"
            );
        }
    }

    #[tokio::test]
    async fn m1_write_does_not_hold_core_lock_across_mtu() {
        let central = open().await;
        ready_peer(&central, "peer-m1", vec![battery_service()]).await;
        central.boundary().set_mtu("peer-m1", 23);
        central.boundary().block_op(FaultOp::Mtu);
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let writer = central.clone();
        let pending_write = tokio::spawn(async move {
            writer
                .write(
                    "peer-m1",
                    &selector,
                    vec![1],
                    "with-response",
                    OpControl::budget_ms(5000),
                )
                .await
        });
        // Let the write reach the gated MTU lookup, then prove the core
        // lock is free: link loss still completes while the write pends.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !pending_write.is_finished(),
            "write pends on the stuck MTU lookup"
        );
        tokio::time::timeout(Duration::from_secs(5), central.remote_peer_loss("peer-m1"))
            .await
            .expect("link loss never queues behind the write")
            .expect("loss recorded");
        central.boundary().unblock_op(FaultOp::Mtu);
        let outcome = tokio::time::timeout(Duration::from_secs(5), pending_write)
            .await
            .expect("write completes after unblock");
        let error = outcome
            .expect("write task")
            .expect_err("stale path fails closed");
        assert_eq!(error.code_str(), "lifecycle.invalid-state");
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"write_characteristic".to_owned()),
            "no radio call for the invalidated path"
        );
    }

    #[tokio::test]
    async fn m2_values_observable_after_subscribe() {
        let central = open().await;
        ready_peer(&central, "peer-m2", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe(
                "peer-m2",
                &selector,
                "consumer-a",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe");
        // Nothing before the radio delivers: no synthesized values.
        assert_eq!(
            central
                .take_notification("peer-m2", &selector, "consumer-a")
                .await
                .expect("take"),
            None,
            "no values before delivery"
        );
        central
            .boundary()
            .push_event(notification("peer-m2", 0, vec![0xde, 0xad]));
        central
            .boundary()
            .push_event(notification("peer-m2", 0, vec![0xbe, 0xef]));
        let mut first = None;
        let mut second = None;
        for _ in 0..200 {
            if first.is_none() {
                first = central
                    .take_notification("peer-m2", &selector, "consumer-a")
                    .await
                    .expect("take");
            } else if second.is_none() {
                second = central
                    .take_notification("peer-m2", &selector, "consumer-a")
                    .await
                    .expect("take");
            }
            if first.is_some() && second.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(first, Some(vec![0xde, 0xad]), "first value FIFO");
        assert_eq!(second, Some(vec![0xbe, 0xef]), "second value FIFO");
        assert_eq!(
            central
                .take_notification("peer-m2", &selector, "consumer-a")
                .await
                .expect("take"),
            None,
            "drained exactly, nothing invented"
        );
    }

    #[tokio::test]
    async fn m4_open_projects_desktop_capabilities() {
        use ubm_core::central::CapabilityAdmission;
        use ubm_core::contracts::BleErrorCode;

        let central = open().await;
        // A provided row gates open with its limitation...
        assert!(
            matches!(
                central
                    .with_core(
                        |core| core.check_capability("peer:resolve-reference", "desktop.probe")
                    )
                    .await,
                Ok(CapabilityAdmission::ProceedWithLimitation)
            ),
            "resolve-reference projects as provided-with-limitation"
        );
        // ...while open adapter work stays closed.
        let error = central
            .with_core(|core| core.check_capability("peer:address-targeting", "desktop.probe"))
            .await
            .expect_err("adapter work gates closed");
        assert_eq!(error.code(), BleErrorCode::CapabilityUnsupported);
    }

    #[tokio::test]
    async fn l5_connect_failure_cleanup_is_bounded() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-l5"));
        central.boundary().block_op(FaultOp::Disconnect);
        central.boundary().fail_next(FaultOp::Connect, "os refused");
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            central.connect("peer-l5", "lease-a", OpControl::budget_ms(5000)),
        )
        .await
        .expect("failing connect never hangs on cleanup")
        .expect_err("scripted connect failure");
        assert_eq!(error.code_str(), "connection.failed");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cleanup bounded by the 1 s discipline"
        );
        assert!(
            central
                .boundary()
                .calls()
                .contains(&"disconnect".to_owned()),
            "half-open link cleanup attempted despite the stuck radio"
        );
        central.boundary().unblock_op(FaultOp::Disconnect);
    }

    #[tokio::test]
    async fn l6_services_changed_invalidates_paths() {
        use ubm_core::central::DatabaseState;

        let central = open().await;
        ready_peer(&central, "peer-l6", vec![battery_service()]).await;
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        // A read works before the change...
        central
            .read("peer-l6", &selector, OpControl::budget_ms(5000))
            .await
            .expect("read before change");
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("peer-l6".to_owned()));
        let peer_key = central.peer_key_for("peer-l6").await.expect("peer");
        let mut invalidated = false;
        for _ in 0..200 {
            let state = central
                .with_core(|core| core.database_state(&peer_key))
                .await;
            if state == Some(DatabaseState::Undiscovered) {
                invalidated = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(invalidated, "database requires rediscovery after change");
        // ...and fails closed after it, with no radio dispatch through
        // the stale handle.
        let reads_before = central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "read_characteristic")
            .count();
        central
            .read("peer-l6", &selector, OpControl::budget_ms(5000))
            .await
            .expect_err("stale handle never dispatches");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "read_characteristic")
                .count(),
            reads_before,
            "no radio call for the invalidated path"
        );
    }

    #[tokio::test]
    async fn l6_disconnect_processed_under_notification_flood() {
        use ubm_core::central::ConnectionState;

        let central = open().await;
        ready_peer(&central, "peer-flood", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe_buffered(
                "peer-flood",
                &selector,
                "consumer-a",
                None,
                ubm_core::streams::OverflowPolicy::Error,
                (64, 8192),
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe");
        for _ in 0..100 {
            central
                .boundary()
                .push_event(notification("peer-flood", 0, vec![0x01]));
        }
        central
            .boundary()
            .push_event(RadioEvent::Disconnected("peer-flood".to_owned()));
        for _ in 0..100 {
            central
                .boundary()
                .push_event(notification("peer-flood", 0, vec![0x02]));
        }
        // The disconnect reconciles even though notifications surround it.
        let peer_key = central.peer_key_for("peer-flood").await.expect("peer");
        let mut lost = false;
        for _ in 0..200 {
            if central
                .with_core(|core| core.connection_state(&peer_key))
                .await
                == Some(ConnectionState::Lost)
            {
                lost = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(lost, "disconnect reconciled under flood");
        // Every flood value was accounted (bounded terminal or buffer),
        // never wedged behind the disconnect.
        let mut terminal_seen = false;
        for _ in 0..200 {
            let terminal = central
                .with_core_mut(|core| {
                    let path = core.resolve_path(&peer_key, &hrm_selector(0)).ok()?;
                    core.take_terminal(path, "consumer-a")
                })
                .await;
            if terminal.is_some() {
                terminal_seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(terminal_seen, "flood accounted with a bounded terminal");
    }

    #[tokio::test]
    async fn l7_cancel_live_scan_operation() {
        use ubm_core::central::CompletionOutcome;
        use ubm_core::contracts::OperationTerminalKind;

        let central = open().await;
        let session = central
            .start_scan("owner-a", &[], OpControl::budget_ms(5000))
            .await
            .expect("start scan");
        let outcome = central
            .cancel_operation(session.operation_id())
            .await
            .expect("cancel live scan");
        match outcome {
            CompletionOutcome::Settled { kind, .. } => {
                assert_eq!(kind, OperationTerminalKind::Aborted, "live cancel aborts");
            }
            CompletionOutcome::DuplicateSuppressed { .. } | CompletionOutcome::ContenderIgnored => {
                panic!("live cancel must settle the operation, got {outcome:?}");
            }
        }
        // Cleanup after cancel stays safe: the late stop settles nothing
        // twice.
        stop_owned_scan(&central).await.expect("late stop");
    }

    #[tokio::test]
    async fn l7_pre_ready_values_quarantine_before_enable() {
        use ubm_core::central::ConsumerState;

        let central = open().await;
        ready_peer(&central, "peer-q", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central.boundary().block_op(FaultOp::Subscribe);
        let subscriber = central.clone();
        let pending_subscribe = tokio::spawn(async move {
            subscriber
                .subscribe(
                    "peer-q",
                    &selector,
                    "consumer-a",
                    None,
                    OpControl::budget_ms(5000),
                )
                .await
        });
        // Wait until the enable reaches the (gated) radio...
        let mut reached_radio = false;
        for _ in 0..200 {
            if central
                .boundary()
                .calls()
                .contains(&"set_notifications".to_owned())
            {
                reached_radio = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(reached_radio, "enable attempted before quarantine check");
        // ...then prove values arriving mid-enable quarantine instead of
        // delivering or dropping.
        central
            .boundary()
            .push_event(notification("peer-q", 0, vec![0x09]));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let peer_key = central.peer_key_for("peer-q").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        assert_eq!(
            central
                .with_core(|core| core.quarantined_count(path_index, "consumer-a"))
                .await,
            Some(1),
            "pre-ready value quarantined (GATT-04 ordering)"
        );
        assert_eq!(
            central
                .take_notification("peer-q", &hrm_selector(0), "consumer-a")
                .await
                .expect("take"),
            None,
            "quarantined values never deliver early"
        );
        central.boundary().unblock_op(FaultOp::Subscribe);
        pending_subscribe
            .await
            .expect("subscribe task")
            .expect("subscribe completes after unblock");
        assert_eq!(
            central
                .with_core(|core| core.consumer_state(path_index, "consumer-a"))
                .await,
            Some(ConsumerState::Ready),
            "consumer ready after enable settles"
        );
    }

    #[tokio::test]
    async fn l7_unsubscribe_disable_failure_retries() {
        let central = open().await;
        ready_peer(&central, "peer-ud", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe(
                "peer-ud",
                &selector,
                "consumer-a",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe");
        central
            .boundary()
            .fail_next(FaultOp::Unsubscribe, "cccd stuck");
        let error = central
            .unsubscribe("peer-ud", &selector, "consumer-a", OpControl::unbounded())
            .await
            .expect_err("scripted disable failure");
        assert_eq!(error.code_str(), "gatt.subscribe-failed");
        // The CCCD is still live, so routing stays: values keep flowing
        // instead of dropping silently.
        let peer_key = central.peer_key_for("peer-ud").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        central
            .boundary()
            .push_event(notification("peer-ud", 0, vec![0x05]));
        let mut still_flows = false;
        for _ in 0..200 {
            if central
                .with_core(|core| core.pending_value_count(path_index, "consumer-a"))
                .await
                == Some(1)
            {
                still_flows = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            still_flows,
            "values still flow while the disable is pending"
        );
        // Resubscribing on the same instance fails closed until the
        // pending disable completes...
        let resubscribe = central
            .subscribe(
                "peer-ud",
                &selector,
                "consumer-a",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect_err("resubscribe races pending disable");
        assert_eq!(resubscribe.code_str(), "lifecycle.invalid-state");
        // ...and a later unsubscribe retries the disable to completion.
        let disabled = central
            .unsubscribe("peer-ud", &selector, "consumer-a", OpControl::unbounded())
            .await
            .expect("retry");
        assert!(disabled, "retry completes the pending disable");
        assert!(
            !central
                .with_core(|core| core.physical_cccd_enabled(path_index))
                .await,
            "CCCD released after retry"
        );
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            3,
            "enable, failed disable, retry disable"
        );
    }

    /// One service with four characteristics covering every combination of
    /// the two write capabilities (F08). Occurrences are 0 throughout:
    /// distinct UUIDs keep the addressing out of the picture.
    fn write_matrix_service() -> ServiceSnapshot {
        let characteristic =
            |uuid: &str, write: bool, without_response: bool| CharacteristicSnapshot {
                uuid: uuid.to_owned(),
                occurrence: 0,
                properties: PropertyFlags {
                    read: true,
                    write,
                    write_without_response: without_response,
                    notify: false,
                    indicate: false,
                },
                descriptors: Vec::new(),
            };
        ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![
                characteristic(HRM_MEASUREMENT, false, false),
                characteristic(BATTERY_LEVEL, true, false),
                characteristic(BODY_SENSOR_LOCATION, false, true),
                characteristic(HEART_RATE_CONTROL_POINT, true, true),
            ],
        }
    }

    fn write_matrix_selector(characteristic: &str) -> crate::central::PathSelector {
        DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(characteristic),
            Some(0),
            None,
            None,
        )
        .expect("selector")
    }

    #[tokio::test]
    async fn f08_write_property_bits_are_facts_the_os_answers() {
        use ubm_core::central::{GATT_PROP_READ, GATT_PROP_WRITE, GATT_PROP_WRITE_NO_RESPONSE};

        let central = open().await;
        ready_peer(&central, "peer-f08", vec![write_matrix_service()]).await;
        central.boundary().set_mtu("peer-f08", 23);
        let peer_key = central.peer_key_for("peer-f08").await.expect("peer");

        // The mapped bitmask itself: each write capability lands on its own
        // core bit, independently.
        for (characteristic, expected) in [
            (HRM_MEASUREMENT, GATT_PROP_READ),
            (BATTERY_LEVEL, GATT_PROP_READ | GATT_PROP_WRITE),
            (
                BODY_SENSOR_LOCATION,
                GATT_PROP_READ | GATT_PROP_WRITE_NO_RESPONSE,
            ),
            (
                HEART_RATE_CONTROL_POINT,
                GATT_PROP_READ | GATT_PROP_WRITE | GATT_PROP_WRITE_NO_RESPONSE,
            ),
        ] {
            let selector = write_matrix_selector(characteristic);
            let index = central
                .with_core(|core| core.resolve_path(&peer_key, &selector).expect("path"))
                .await;
            assert_eq!(
                central
                    .with_core(|core| core.stored_path(index).map(|path| path.properties()))
                    .await,
                Some(expected),
                "mapped bitmask for {characteristic}"
            );
        }

        // Legacy parity (finding 83): every mode reaches the OS on every
        // characteristic whatever its flags say, with the requested mode;
        // the OS answer is the result.
        let mut expected = Vec::new();
        for characteristic in [
            HRM_MEASUREMENT,
            BATTERY_LEVEL,
            BODY_SENSOR_LOCATION,
            HEART_RATE_CONTROL_POINT,
        ] {
            for (mode, with_response) in [("with-response", true), ("without-response", false)] {
                central
                    .write(
                        "peer-f08",
                        &write_matrix_selector(characteristic),
                        vec![1],
                        mode,
                        OpControl::budget_ms(5000),
                    )
                    .await
                    .unwrap_or_else(|error| panic!("{mode} on {characteristic}: {error:?}"));
                expected.push((
                    (
                        "peer-f08".to_owned(),
                        HRM_SERVICE.to_owned(),
                        0,
                        characteristic.to_owned(),
                        0,
                    ),
                    with_response,
                ));
            }
        }
        assert_eq!(
            central.boundary().writes(),
            expected,
            "each write reaches the boundary once, in its own mode"
        );
        central
            .boundary()
            .fail_next(FaultOp::Write, "write not permitted");
        let refused = central
            .write(
                "peer-f08",
                &write_matrix_selector(HRM_MEASUREMENT),
                vec![1],
                "with-response",
                OpControl::budget_ms(5000),
            )
            .await
            .expect_err("the OS refuses the write");
        assert_eq!(refused.code_str(), "gatt.write-failed");
        assert_eq!(
            refused.detail(),
            Some("write not permitted"),
            "the OS answer is the result"
        );
    }

    /// Duplicated service UUID where the target characteristic lives only
    /// under the second instance (F18): occurrence-0 addressing can never
    /// reach it, while an omitted service occurrence still resolves
    /// unambiguously.
    fn second_instance_service_pair() -> Vec<ServiceSnapshot> {
        vec![
            ServiceSnapshot {
                uuid: HRM_SERVICE.to_owned(),
                occurrence: 0,
                characteristics: vec![CharacteristicSnapshot {
                    uuid: BATTERY_LEVEL.to_owned(),
                    occurrence: 0,
                    properties: PropertyFlags {
                        read: true,
                        write: false,
                        write_without_response: false,
                        notify: false,
                        indicate: false,
                    },
                    descriptors: Vec::new(),
                }],
            },
            ServiceSnapshot {
                uuid: HRM_SERVICE.to_owned(),
                occurrence: 1,
                characteristics: vec![CharacteristicSnapshot {
                    uuid: HRM_MEASUREMENT.to_owned(),
                    occurrence: 0,
                    properties: PropertyFlags {
                        read: true,
                        write: true,
                        write_without_response: false,
                        notify: true,
                        indicate: false,
                    },
                    descriptors: vec![DescriptorSnapshot {
                        uuid: USER_DESCRIPTION.to_owned(),
                        occurrence: 0,
                    }],
                }],
            },
        ]
    }

    fn second_instance_notification(
        peer_id: &str,
        service_occurrence: u64,
        value: Vec<u8>,
    ) -> RadioEvent {
        RadioEvent::Notification {
            peer_id: peer_id.to_owned(),
            service_uuid: HRM_SERVICE.to_owned(),
            service_occurrence,
            characteristic_uuid: HRM_MEASUREMENT.to_owned(),
            characteristic_occurrence: 0,
            epoch: 0,
            value,
        }
    }

    #[tokio::test]
    async fn f18_omitted_occurrence_addresses_resolved_instance() {
        let central = open().await;
        ready_peer(&central, "peer-f18", second_instance_service_pair()).await;
        central.boundary().set_mtu("peer-f18", 23);
        // Instance 1 answers distinctly; instance 0 has no such
        // characteristic, so the canned default would expose misaddressing.
        central.boundary().set_characteristic_value(
            "peer-f18",
            HRM_SERVICE,
            1,
            HRM_MEASUREMENT,
            0,
            vec![0xc4, 0x35],
        );
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            None,
            Some(HRM_MEASUREMENT),
            None,
            None,
            None,
        )
        .expect("selector");
        let descriptor_selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            None,
            Some(HRM_MEASUREMENT),
            None,
            Some(USER_DESCRIPTION),
            None,
        )
        .expect("descriptor selector");

        // Read resolves the unique path and addresses instance 1.
        let value = central
            .read("peer-f18", &selector, OpControl::budget_ms(5000))
            .await
            .expect("read");
        assert_eq!(
            value,
            vec![0xc4, 0x35],
            "read addresses service occurrence 1, never the guessed 0"
        );

        // Write addresses instance 1 with the requested mode.
        central
            .write(
                "peer-f18",
                &selector,
                vec![1],
                "with-response",
                OpControl::budget_ms(5000),
            )
            .await
            .expect("write");
        assert_eq!(
            central.boundary().writes().as_slice(),
            &[(
                (
                    "peer-f18".to_owned(),
                    HRM_SERVICE.to_owned(),
                    1,
                    HRM_MEASUREMENT.to_owned(),
                    0,
                ),
                true,
            )],
            "write addresses service occurrence 1"
        );

        // Descriptor operations address instance 1 as well.
        central
            .read_descriptor("peer-f18", &descriptor_selector, OpControl::budget_ms(5000))
            .await
            .expect("descriptor read");
        assert_eq!(
            central.boundary().descriptor_reads().as_slice(),
            &[(
                (
                    "peer-f18".to_owned(),
                    HRM_SERVICE.to_owned(),
                    1,
                    HRM_MEASUREMENT.to_owned(),
                    0,
                ),
                USER_DESCRIPTION.to_owned(),
                0,
            )],
            "descriptor read addresses service occurrence 1"
        );
        central
            .write_descriptor(
                "peer-f18",
                &descriptor_selector,
                vec![1],
                OpControl::budget_ms(5000),
            )
            .await
            .expect("descriptor write");
        assert_eq!(
            central.boundary().descriptor_writes().len(),
            1,
            "descriptor write dispatches exactly once"
        );
        assert_eq!(
            central.boundary().descriptor_writes()[0].0.2,
            1,
            "descriptor write addresses service occurrence 1"
        );

        // Subscription routing registers under instance 1: its values
        // deliver, while instance-0 values for the same UUIDs do not.
        central
            .subscribe(
                "peer-f18",
                &selector,
                "consumer-a",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe");
        central
            .boundary()
            .push_event(second_instance_notification("peer-f18", 1, vec![0x09]));
        let mut delivered = None;
        for _ in 0..200 {
            delivered = central
                .take_notification("peer-f18", &selector, "consumer-a")
                .await
                .expect("take");
            if delivered.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            delivered,
            Some(vec![0x09]),
            "instance-1 values route to the subscriber"
        );
        central
            .boundary()
            .push_event(second_instance_notification("peer-f18", 0, vec![0x08]));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            central
                .take_notification("peer-f18", &selector, "consumer-a")
                .await
                .expect("take"),
            None,
            "instance-0 values never reach the instance-1 routing"
        );
    }

    #[tokio::test]
    async fn f11_concurrent_subscribe_enables_once() {
        use ubm_core::central::ConsumerState;

        let central = open().await;
        ready_peer(&central, "peer-f11", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        // Pause the first native enable behind the radio gate.
        central.boundary().block_op(FaultOp::Subscribe);
        let first = central.clone();
        let selector_clone = selector.clone();
        let pending_first = tokio::spawn(async move {
            first
                .subscribe(
                    "peer-f11",
                    &selector_clone,
                    "consumer-a",
                    None,
                    OpControl::budget_ms(5000),
                )
                .await
        });
        let mut reached_radio = false;
        for _ in 0..200 {
            if central
                .boundary()
                .calls()
                .contains(&"set_notifications".to_owned())
            {
                reached_radio = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(reached_radio, "first enable attempted before joining");
        assert!(
            !pending_first.is_finished(),
            "first subscribe pends on the stuck enable"
        );

        // A second consumer — and a repeat of the first — join the pending
        // enablement instead of driving their own native enable.
        let second = central.clone();
        let selector_clone = selector.clone();
        let pending_second = tokio::spawn(async move {
            second
                .subscribe(
                    "peer-f11",
                    &selector_clone,
                    "consumer-b",
                    None,
                    OpControl::budget_ms(5000),
                )
                .await
        });
        let repeat = central.clone();
        let selector_clone = selector.clone();
        let pending_repeat = tokio::spawn(async move {
            repeat
                .subscribe(
                    "peer-f11",
                    &selector_clone,
                    "consumer-a",
                    None,
                    OpControl::budget_ms(5000),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            pending_second.is_finished(),
            "joining consumer never waits on the radio"
        );
        assert!(
            pending_repeat.is_finished(),
            "repeated request never re-drives the radio"
        );
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            1,
            "exactly one native enable for all concurrent subscribers"
        );

        // One release completes every waiter with consistent readiness.
        central.boundary().unblock_op(FaultOp::Subscribe);
        pending_first
            .await
            .expect("first task")
            .expect("first subscribe");
        pending_second
            .await
            .expect("second task")
            .expect("second subscribe");
        pending_repeat
            .await
            .expect("repeat task")
            .expect("repeat subscribe");
        let peer_key = central.peer_key_for("peer-f11").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        for consumer in ["consumer-a", "consumer-b"] {
            assert_eq!(
                central
                    .with_core(|core| core.consumer_state(path_index, consumer))
                    .await,
                Some(ConsumerState::Ready),
                "{consumer} shares the one enablement"
            );
        }
        // Both consumers observe the same live stream.
        central
            .boundary()
            .push_event(notification("peer-f11", 0, vec![0x07]));
        let mut first_value = None;
        let mut second_value = None;
        for _ in 0..200 {
            if first_value.is_none() {
                first_value = central
                    .take_notification("peer-f11", &selector, "consumer-a")
                    .await
                    .expect("take a");
            }
            if second_value.is_none() {
                second_value = central
                    .take_notification("peer-f11", &selector, "consumer-b")
                    .await
                    .expect("take b");
            }
            if first_value.is_some() && second_value.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(first_value, Some(vec![0x07]));
        assert_eq!(second_value, Some(vec![0x07]));
        // Teardown stays single too: the shared CCCD disables once.
        central
            .unsubscribe("peer-f11", &selector, "consumer-a", OpControl::unbounded())
            .await
            .expect("unsubscribe a");
        central
            .unsubscribe("peer-f11", &selector, "consumer-b", OpControl::unbounded())
            .await
            .expect("unsubscribe b");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            2,
            "enable once, disable once"
        );
    }

    #[tokio::test]
    async fn f10_stale_notification_rejected_after_reconnect() {
        use ubm_core::central::DatabaseState;

        let central = open().await;
        ready_peer(&central, "peer-f10", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe(
                "peer-f10",
                &selector,
                "consumer-a",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe");
        // The forwarder install captured this epoch; the held event was
        // queued under it in native ingress and is released only later.
        let installed = central.boundary().enable_epochs();
        assert_eq!(installed.len(), 1, "one forwarder installed");
        let stale_epoch = installed[0].1;
        let stale = notification_epoch("peer-f10", 0, vec![0xAA], stale_epoch);

        // Disconnect, reconnect, rediscover, resubscribe: same peripheral
        // id, same instance key, new subscription under a new generation.
        central
            .disconnect("peer-f10", "lease-a", OpControl::unbounded())
            .await
            .expect("disconnect");
        central
            .connect("peer-f10", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("reconnect");
        central
            .discover("peer-f10", "lease-a", OpControl::unbounded())
            .await
            .expect("rediscover");
        central
            .subscribe(
                "peer-f10",
                &selector,
                "consumer-b",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("resubscribe");
        let reinstalled = central.boundary().enable_epochs();
        assert_eq!(reinstalled.len(), 2, "resubscribe reinstalls the forwarder");
        let fresh_epoch = reinstalled[1].1;

        // Release the held old value: it must never enter the new stream,
        // even though the peripheral id and instance key match again.
        central.boundary().push_event(stale);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            central
                .take_notification("peer-f10", &selector, "consumer-b")
                .await
                .expect("take"),
            None,
            "stale queued value never reaches the new consumer"
        );
        assert_ne!(
            fresh_epoch, stale_epoch,
            "reinstall captures a new generation"
        );
        // The new subscription is live: current-generation values deliver.
        central
            .boundary()
            .push_event(notification_epoch("peer-f10", 0, vec![0xBB], fresh_epoch));
        let mut delivered = None;
        for _ in 0..200 {
            delivered = central
                .take_notification("peer-f10", &selector, "consumer-b")
                .await
                .expect("take");
            if delivered.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(delivered, Some(vec![0xBB]), "fresh values still deliver");

        // Same protection across a service change: a value queued under
        // the old database never enters the post-rediscovery subscription.
        let pre_change = notification_epoch("peer-f10", 0, vec![0xCC], fresh_epoch);
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("peer-f10".to_owned()));
        let peer_key = central.peer_key_for("peer-f10").await.expect("peer");
        let mut invalidated = false;
        for _ in 0..200 {
            if central
                .with_core(|core| core.database_state(&peer_key))
                .await
                == Some(DatabaseState::Undiscovered)
            {
                invalidated = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(invalidated, "database requires rediscovery after change");
        central
            .discover("peer-f10", "lease-a", OpControl::unbounded())
            .await
            .expect("rediscover after change");
        central
            .subscribe(
                "peer-f10",
                &selector,
                "consumer-c",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe after change");
        let current_epoch = central
            .boundary()
            .enable_epochs()
            .last()
            .map(|installed| installed.1)
            .expect("forwarder installed");
        assert_ne!(
            current_epoch, fresh_epoch,
            "post-change install captures a new generation"
        );
        central.boundary().push_event(pre_change);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            central
                .take_notification("peer-f10", &selector, "consumer-c")
                .await
                .expect("take"),
            None,
            "pre-change value never reaches the new subscription"
        );
        central
            .boundary()
            .push_event(notification_epoch("peer-f10", 0, vec![0xDD], current_epoch));
        let mut redelivered = None;
        for _ in 0..200 {
            redelivered = central
                .take_notification("peer-f10", &selector, "consumer-c")
                .await
                .expect("take");
            if redelivered.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            redelivered,
            Some(vec![0xDD]),
            "post-change subscription stays live"
        );
    }

    #[tokio::test]
    async fn f25_failed_reads_settle_and_release() {
        let central = open().await;
        ready_peer(&central, "peer-f25", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        for i in 0..9u32 {
            central.boundary().fail_next(FaultOp::Read, "att error");
            let error = central
                .read("peer-f25", &selector, OpControl::budget_ms(5000))
                .await
                .expect_err("native failure surfaces");
            assert_eq!(
                error.code_str(),
                "gatt.read-failed",
                "iteration {i}: caller sees the radio failure, not a quota"
            );
            let live = central.with_core(|core| core.live_operation_count()).await;
            assert_eq!(
                live, baseline,
                "iteration {i}: failed op terminal and reaped, no live leak"
            );
        }
        let value = central
            .read("peer-f25", &selector, OpControl::budget_ms(5000))
            .await
            .expect("admission remains after failures");
        assert_eq!(value, vec![0x42]);
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            baseline,
            "baseline restored"
        );
    }

    #[tokio::test]
    async fn f25_failed_writes_and_descriptors_settle_and_release() {
        let central = open().await;
        ready_peer(&central, "peer-f25w", vec![battery_service()]).await;
        central.boundary().set_mtu("peer-f25w", 64);
        let write_selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        for i in 0..9u32 {
            central.boundary().fail_next(FaultOp::Write, "att error");
            let error = central
                .write(
                    "peer-f25w",
                    &write_selector,
                    vec![0x01],
                    "with-response",
                    OpControl::budget_ms(5000),
                )
                .await
                .expect_err("native write failure surfaces");
            assert_eq!(
                error.code_str(),
                "gatt.write-failed",
                "write iteration {i}: radio failure, not quota"
            );
            assert_eq!(
                central.with_core(|core| core.live_operation_count()).await,
                baseline,
                "write iteration {i}: reaped"
            );
        }
        ready_peer(&central, "peer-f25d", vec![hrm_service()]).await;
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        let descriptor_selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            Some(USER_DESCRIPTION),
            Some(0),
        )
        .expect("descriptor selector");
        for i in 0..9u32 {
            central.boundary().fail_next(FaultOp::Read, "att error");
            let error = central
                .read_descriptor(
                    "peer-f25d",
                    &descriptor_selector,
                    OpControl::budget_ms(5000),
                )
                .await
                .expect_err("native descriptor failure surfaces");
            assert_eq!(
                error.code_str(),
                "gatt.read-failed",
                "descriptor iteration {i}: radio failure, not quota"
            );
            assert_eq!(
                central.with_core(|core| core.live_operation_count()).await,
                baseline,
                "descriptor iteration {i}: reaped"
            );
        }
        central
            .write(
                "peer-f25w",
                &write_selector,
                vec![0x01],
                "with-response",
                OpControl::budget_ms(5000),
            )
            .await
            .expect("write admission remains");
    }

    #[tokio::test]
    async fn f03_read_timeout_is_end_to_end_deadline() {
        let central = open().await;
        ready_peer(&central, "peer-f03t", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Read);
        // The op deadline (100 ms) must settle the caller even though the
        // radio never resolves. The outer 5 s harness timeout only fails the
        // test fast on unfixed code; it must never fire on fixed code.
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            central.read("peer-f03t", &selector, OpControl::budget_ms(100)),
        )
        .await
        .expect("deadline driver settles before the harness");
        let error = outcome.expect_err("timeout surfaces as an error");
        assert_eq!(error.code_str(), "operation.timed-out");
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            baseline,
            "timed-out op terminal and reaped"
        );
        central.boundary().unblock_op(FaultOp::Read);
        // Late radio work cannot resurrect: the next read succeeds cleanly.
        let value = central
            .read("peer-f03t", &selector, OpControl::budget_ms(5000))
            .await
            .expect("admission remains after timeout");
        assert_eq!(value, vec![0x42]);
    }

    #[tokio::test]
    async fn f03_cancel_then_native_success_returns_cancelled() {
        let central = open().await;
        ready_peer(&central, "peer-f03c", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Read);
        let reader = central.clone();
        let selector_clone = selector.clone();
        let pending = tokio::spawn(async move {
            reader
                .read("peer-f03c", &selector_clone, OpControl::budget_ms(5000))
                .await
        });
        let mut op_id = None;
        for _ in 0..200 {
            let live = central.with_core(|core| core.live_operation_ids()).await;
            if !live.is_empty() {
                op_id = Some(live[0].clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let op_id = op_id.expect("in-flight read op");
        central
            .cancel_operation(&op_id)
            .await
            .expect("cancel in-flight read");
        central.boundary().unblock_op(FaultOp::Read);
        let outcome = pending.await.expect("read task");
        let error = outcome.expect_err("cancel wins over late radio success");
        assert_eq!(error.code_str(), "operation.aborted");
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            baseline,
            "cancelled op reaped"
        );
    }

    #[tokio::test]
    async fn f03_service_change_during_read_returns_stale() {
        use ubm_core::central::DatabaseState;

        let central = open().await;
        ready_peer(&central, "peer-f03s", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central.boundary().block_op(FaultOp::Read);
        let reader = central.clone();
        let selector_clone = selector.clone();
        let pending = tokio::spawn(async move {
            reader
                .read("peer-f03s", &selector_clone, OpControl::budget_ms(5000))
                .await
        });
        for _ in 0..200 {
            if !central
                .with_core(|core| core.live_operation_ids())
                .await
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("peer-f03s".to_owned()));
        let peer_key = central.peer_key_for("peer-f03s").await.expect("peer");
        let mut invalidated = false;
        for _ in 0..200 {
            if central
                .with_core(|core| core.database_state(&peer_key))
                .await
                == Some(DatabaseState::Undiscovered)
            {
                invalidated = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(invalidated, "service change invalidates");
        central.boundary().unblock_op(FaultOp::Read);
        let outcome = pending.await.expect("read task");
        let error = outcome.expect_err("stale wins over late radio success");
        assert_eq!(error.code_str(), "gatt.stale-handle");
    }

    #[tokio::test]
    async fn f03_mtu_lookup_inside_deadline() {
        let central = open().await;
        ready_peer(&central, "peer-f03m", vec![battery_service()]).await;
        central.boundary().set_mtu("peer-f03m", 64);
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Mtu);
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            central.write(
                "peer-f03m",
                &selector,
                vec![0x01],
                "with-response",
                OpControl::budget_ms(100),
            ),
        )
        .await
        .expect("mtu deadline settles");
        let error = outcome.expect_err("stuck mtu times out");
        assert_eq!(error.code_str(), "operation.timed-out");
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            baseline,
            "no op leaked by mtu timeout"
        );
        central.boundary().unblock_op(FaultOp::Mtu);
        central
            .write(
                "peer-f03m",
                &selector,
                vec![0x01],
                "with-response",
                OpControl::budget_ms(5000),
            )
            .await
            .expect("write succeeds after mtu unblocks");
    }

    #[tokio::test]
    async fn f03_disconnect_during_read_returns_disconnected() {
        let central = open().await;
        ready_peer(&central, "peer-f03d", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central.boundary().block_op(FaultOp::Read);
        let reader = central.clone();
        let selector_clone = selector.clone();
        let pending = tokio::spawn(async move {
            reader
                .read("peer-f03d", &selector_clone, OpControl::budget_ms(5000))
                .await
        });
        for _ in 0..200 {
            if !central
                .with_core(|core| core.live_operation_ids())
                .await
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        central
            .remote_peer_loss("peer-f03d")
            .await
            .expect("peer loss reconciles");
        central.boundary().unblock_op(FaultOp::Read);
        let outcome = pending.await.expect("read task");
        let error = outcome.expect_err("disconnect wins over late radio success");
        assert!(
            error.code_str() == "operation.disconnected" || error.code_str() == "gatt.stale-handle",
            "disconnect or stale, got {}",
            error.code_str()
        );
    }

    #[tokio::test]
    async fn f17_overflow_terminal_distinguishable() {
        // Overflow an error-policy subscription (64 items), drain all values
        // through the typed poll, and require exactly one terminal with
        // accurate loss details. Repeated polls must not invent a second
        // terminal or imply recovery.
        let central = open().await;
        ready_peer(&central, "peer-f17", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe_buffered(
                "peer-f17",
                &selector,
                "consumer-a",
                None,
                ubm_core::streams::OverflowPolicy::Error,
                (64, 8192),
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe");
        for _ in 0..70u32 {
            central
                .boundary()
                .push_event(notification("peer-f17", 0, vec![0x01]));
        }
        // Wait for the overflow terminal to land.
        let peer_key = central.peer_key_for("peer-f17").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        let mut failed = false;
        for _ in 0..200 {
            if central
                .with_core(|core| core.consumer_state(path_index, "consumer-a"))
                .await
                == Some(ubm_core::central::ConsumerState::Failed)
            {
                failed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(failed, "overflow terminates the error-policy stream");
        // Drain through the typed protocol: values, then one terminal, then
        // closed — never a second terminal, never live-empty again.
        let mut values = 0usize;
        let mut terminals = 0usize;
        let mut terminal_details = None;
        for _ in 0..100u32 {
            match central
                .poll_notification("peer-f17", &selector, "consumer-a")
                .await
                .expect("poll")
            {
                crate::central::NotificationPoll::Value(_) => values += 1,
                crate::central::NotificationPoll::Terminal(terminal) => {
                    terminals += 1;
                    terminal_details = Some((
                        terminal.dropped_items(),
                        terminal.dropped_bytes(),
                        terminal.replaced_items(),
                    ));
                }
                crate::central::NotificationPoll::Closed => break,
                crate::central::NotificationPoll::Empty => {
                    panic!("failed stream must not report live-empty")
                }
                crate::central::NotificationPoll::Invalidated(_) => {
                    panic!("overflow is terminal, not invalidated")
                }
            }
        }
        assert_eq!(values, 64, "all admitted values drain before the terminal");
        assert_eq!(terminals, 1, "exactly one terminal");
        assert_eq!(
            terminal_details,
            Some((1, 1, 0)),
            "accurate loss details for the rejected item"
        );
        // Second terminal poll stays closed, never a new terminal.
        assert!(
            matches!(
                central
                    .poll_notification("peer-f17", &selector, "consumer-a")
                    .await
                    .expect("poll"),
                crate::central::NotificationPoll::Closed
            ),
            "terminal observed exactly once"
        );
        // Resubscription after terminal consumption works: prune the observed
        // record, then subscribe fresh and deliver.
        central
            .unsubscribe("peer-f17", &selector, "consumer-a", OpControl::unbounded())
            .await
            .expect("unsubscribe prunes observed terminal");
        central
            .subscribe(
                "peer-f17",
                &selector,
                "consumer-a",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("resubscribe after terminal");
        central
            .boundary()
            .push_event(notification("peer-f17", 0, vec![0x09]));
        let mut redelivered = None;
        for _ in 0..200 {
            match central
                .poll_notification("peer-f17", &selector, "consumer-a")
                .await
                .expect("poll")
            {
                crate::central::NotificationPoll::Value(value) => {
                    redelivered = Some(value);
                    break;
                }
                crate::central::NotificationPoll::Empty => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                other => panic!("fresh subscription must deliver values, got {other:?}"),
            }
        }
        assert_eq!(redelivered, Some(vec![0x09]));
    }

    #[tokio::test]
    async fn f17_invalidation_and_closure_distinguishable() {
        use ubm_core::central::DatabaseState;

        let central = open().await;
        ready_peer(&central, "peer-f17i", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe(
                "peer-f17i",
                &selector,
                "consumer-a",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe");
        assert!(matches!(
            central
                .poll_notification("peer-f17i", &selector, "consumer-a")
                .await
                .expect("poll"),
            crate::central::NotificationPoll::Empty
        ));
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("peer-f17i".to_owned()));
        let peer_key = central.peer_key_for("peer-f17i").await.expect("peer");
        for _ in 0..200 {
            if central
                .with_core(|core| core.database_state(&peer_key))
                .await
                == Some(DatabaseState::Undiscovered)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            central
                .poll_notification("peer-f17i", &selector, "consumer-a")
                .await
                .expect("poll"),
            crate::central::NotificationPoll::Invalidated(
                crate::central::InvalidationCause::ServicesChanged
            ),
            "a service change on a live link names its cause"
        );
        central
            .discover("peer-f17i", "lease-a", OpControl::unbounded())
            .await
            .expect("rediscover");
        central
            .subscribe(
                "peer-f17i",
                &selector,
                "consumer-b",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("resubscribe after invalidation");
        let fresh_epoch = central
            .boundary()
            .enable_epochs()
            .last()
            .map(|installed| installed.1)
            .expect("forwarder installed");
        central
            .boundary()
            .push_event(notification_epoch("peer-f17i", 0, vec![0x0A], fresh_epoch));
        let mut got = None;
        for _ in 0..200 {
            match central
                .poll_notification("peer-f17i", &selector, "consumer-b")
                .await
                .expect("poll")
            {
                crate::central::NotificationPoll::Value(value) => {
                    got = Some(value);
                    break;
                }
                crate::central::NotificationPoll::Empty => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                other => panic!("fresh subscription must deliver, got {other:?}"),
            }
        }
        assert_eq!(got, Some(vec![0x0A]));
    }

    #[tokio::test]
    async fn f07_flood_bounds_ingress_and_preserves_control() {
        use ubm_core::central::{ConnectionState, ConsumerState};

        let central = open().await;
        ready_peer(&central, "peer-f07a", vec![hrm_service()]).await;
        ready_peer(&central, "peer-f07b", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe_buffered(
                "peer-f07a",
                &selector,
                "consumer-a",
                None,
                ubm_core::streams::OverflowPolicy::Error,
                (64, 8192),
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe A");
        central
            .subscribe(
                "peer-f07b",
                &selector,
                "consumer-b",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe B");
        // Flood A with 1000 notifications in a tight loop (no awaits, so the
        // event loop cannot drain mid-flood): the 256-item bounded ingress
        // must drop the excess explicitly instead of growing memory.
        for _ in 0..1000u32 {
            central
                .boundary()
                .push_event(notification("peer-f07a", 0, vec![0x01]));
        }
        assert!(
            central.boundary().dropped_notification_count() > 0,
            "overload drops counted, memory bounded instead of grown"
        );
        assert_eq!(
            central.resource_counters().await.ingress_notification_drops,
            central.boundary().dropped_notification_count(),
            "the radio's ingress drops reach the central's counters (finding 131)"
        );
        assert!(
            central.boundary().data_queued_bytes() <= 262_144,
            "queued bytes stay within the ingress bound"
        );
        // Control under flood: B's disconnect (separate control queue, push
        // order preserved) still reconciles.
        central
            .boundary()
            .push_event(RadioEvent::Disconnected("peer-f07b".to_owned()));
        let peer_b = central.peer_key_for("peer-f07b").await.expect("peer B");
        let mut lost = false;
        for _ in 0..400 {
            if central
                .with_core(|core| core.connection_state(&peer_b))
                .await
                == Some(ConnectionState::Lost)
            {
                lost = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(lost, "control delivers under data flood");
        // Other devices progress: B reconnects and reads while A's flood
        // drains.
        central
            .connect("peer-f07b", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("B reconnects under flood");
        central
            .discover("peer-f07b", "lease-a", OpControl::unbounded())
            .await
            .expect("B rediscovers");
        let value = central
            .read("peer-f07b", &hrm_selector(0), OpControl::budget_ms(5000))
            .await
            .expect("B reads under flood");
        assert_eq!(value, vec![0x42]);
        // A's lossless stream that cannot keep up shows a visible terminal,
        // not radio silence.
        let peer_a = central.peer_key_for("peer-f07a").await.expect("peer A");
        let path_a = central
            .with_core(|core| core.resolve_path(&peer_a, &hrm_selector(0)).expect("path"))
            .await;
        let mut failed = false;
        for _ in 0..400 {
            if central
                .with_core(|core| core.consumer_state(path_a, "consumer-a"))
                .await
                == Some(ConsumerState::Failed)
            {
                failed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(failed, "flooded lossless stream terminates visibly");
    }

    #[tokio::test]
    async fn f24_connect_cleanup_releases_core_lock() {
        let central = open().await;
        ready_peer(&central, "peer-f24b", vec![hrm_service()]).await;
        let selector_b = hrm_selector(0);
        // A stalls in half-open cleanup behind the disconnect gate.
        central.boundary().block_op(FaultOp::Disconnect);
        central.boundary().fail_next(FaultOp::Connect, "os refused");
        let failing = central.clone();
        let pending_a = tokio::spawn(async move {
            failing
                .connect("peer-f24a", "lease-a", OpControl::budget_ms(5000))
                .await
        });
        let mut cleaning = false;
        for _ in 0..200 {
            if central
                .boundary()
                .calls()
                .contains(&"disconnect".to_owned())
            {
                cleaning = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(cleaning, "A reaches compensating disconnect");
        // B progresses while A's radio cleanup is still stalled: its read
        // must not wait behind the core lock.
        let reader = central.clone();
        let selector_clone = selector_b.clone();
        let pending_b = tokio::spawn(async move {
            reader
                .read("peer-f24b", &selector_clone, OpControl::budget_ms(5000))
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            pending_b.is_finished(),
            "B's operation never waits on A's radio cleanup"
        );
        let value = pending_b.await.expect("B task").expect("B reads");
        assert_eq!(value, vec![0x42]);
        // B's notifications also flow while A cleans up.
        central
            .subscribe(
                "peer-f24b",
                &selector_b,
                "consumer-b",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("B subscribes under A's cleanup");
        central
            .boundary()
            .push_event(notification("peer-f24b", 0, vec![0x0B]));
        let mut delivered = None;
        for _ in 0..200 {
            delivered = central
                .take_notification("peer-f24b", &selector_b, "consumer-b")
                .await
                .expect("take");
            if delivered.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(delivered, Some(vec![0x0B]));
        central.boundary().unblock_op(FaultOp::Disconnect);
        let outcome_a = pending_a.await.expect("A task");
        assert_eq!(
            outcome_a.expect_err("A still fails").code_str(),
            "connection.failed"
        );
    }

    #[tokio::test]
    async fn f03_shutdown_during_connect_cancels() {
        let central = open().await;
        central.boundary().block_op(FaultOp::Connect);
        let connector = central.clone();
        let pending = tokio::spawn(async move {
            connector
                .connect("peer-f03x", "lease-a", OpControl::budget_ms(10_000))
                .await
        });
        for _ in 0..200 {
            if !central
                .with_core(|core| core.live_operation_ids())
                .await
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Shutdown must not hang on the gated radio: it cancels in-flight
        // work, joins the loop, and destroys promptly.
        tokio::time::timeout(Duration::from_secs(5), central.shutdown())
            .await
            .expect("shutdown completes despite gated radio");
        central.boundary().unblock_op(FaultOp::Connect);
        let outcome = pending.await.expect("connect task");
        let error = outcome.expect_err("shutdown wins over late radio success");
        assert!(
            error.code_str() == "operation.aborted"
                || error.code_str() == "adapter.unavailable"
                || error.code_str() == "lifecycle.destroyed",
            "cancelled/destroyed, got {}",
            error.code_str()
        );
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            0,
            "no live leak after shutdown race"
        );
    }

    #[tokio::test]
    async fn f03_dropped_caller_still_releases() {
        let central = open().await;
        ready_peer(&central, "peer-f03q", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Read);
        let reader = central.clone();
        let selector_clone = selector.clone();
        let pending = tokio::spawn(async move {
            reader
                .read("peer-f03q", &selector_clone, OpControl::budget_ms(5000))
                .await
        });
        for _ in 0..200 {
            if !central
                .with_core(|core| core.live_operation_ids())
                .await
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Drop the caller future while the radio is still gated: the runtime
        // keeps the native work owned until cleanup finishes.
        pending.abort();
        let _ = pending.await;
        central.boundary().unblock_op(FaultOp::Read);
        // Give any detached cleanup a moment, then require reclamation.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            baseline,
            "dropped caller still reaps its op"
        );
        let value = central
            .read("peer-f03q", &selector, OpControl::budget_ms(5000))
            .await
            .expect("admission remains after drop");
        assert_eq!(value, vec![0x42]);
    }

    #[tokio::test]
    async fn f03_dropped_write_still_releases() {
        let central = open().await;
        let peer = "peer-f03w".to_owned();
        ready_peer(&central, &peer, vec![battery_service()]).await;
        central.boundary().set_mtu(&peer, 23);
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Write);
        let caller = central.clone();
        let peer_clone = peer.clone();
        let selector_clone = selector.clone();
        let pending = tokio::spawn(async move {
            caller
                .write(
                    &peer_clone,
                    &selector_clone,
                    vec![0x02],
                    "with-response",
                    OpControl::budget_ms(5000),
                )
                .await
        });
        let mut saw_live = false;
        for _ in 0..200 {
            let live = central.with_core(|core| core.live_operation_count()).await;
            if live > baseline {
                saw_live = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(saw_live, "write in flight before abort (non-vacuous)");
        pending.abort();
        let _ = pending.await;
        central.boundary().unblock_op(FaultOp::Write);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let live = central.with_core(|core| core.live_operation_count()).await;
        assert_eq!(live, baseline, "dropped write still reaps its op");
        central
            .write(
                &peer,
                &selector,
                vec![0x02],
                "with-response",
                OpControl::budget_ms(5000),
            )
            .await
            .expect("post-drop admission");
    }

    #[tokio::test]
    async fn f03_dropped_connect_still_releases() {
        let central = open().await;
        let peer = "peer-f03c".to_owned();
        central.boundary().push_event(advertisement(&peer));
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Connect);
        let caller = central.clone();
        let peer_clone = peer.clone();
        let pending = tokio::spawn(async move {
            caller
                .connect(&peer_clone, "lease-x", OpControl::budget_ms(10_000))
                .await
        });
        let mut saw_live = false;
        for _ in 0..200 {
            let live = central.with_core(|core| core.live_operation_count()).await;
            if live > baseline {
                saw_live = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(saw_live, "connect in flight before abort (non-vacuous)");
        pending.abort();
        let _ = pending.await;
        central.boundary().unblock_op(FaultOp::Connect);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let live = central.with_core(|core| core.live_operation_count()).await;
        assert_eq!(live, baseline, "dropped connect still reaps its op");
        central
            .connect(&peer, "lease-x", OpControl::budget_ms(10_000))
            .await
            .expect("post-drop admission");
    }

    #[tokio::test]
    async fn f03_dropped_subscribe_still_releases() {
        let central = open().await;
        let peer = "peer-f03s".to_owned();
        ready_peer(&central, &peer, vec![hrm_service()]).await;
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Subscribe);
        let caller = central.clone();
        let peer_clone = peer.clone();
        let pending = tokio::spawn(async move {
            caller
                .subscribe(
                    &peer_clone,
                    &hrm_selector(0),
                    "consumer-drop",
                    None,
                    OpControl::budget_ms(5000),
                )
                .await
        });
        let mut saw_live = false;
        for _ in 0..200 {
            let live = central.with_core(|core| core.live_operation_count()).await;
            if live > baseline {
                saw_live = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(saw_live, "subscribe in flight before abort (non-vacuous)");
        pending.abort();
        let _ = pending.await;
        central.boundary().unblock_op(FaultOp::Subscribe);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let live = central.with_core(|core| core.live_operation_count()).await;
        assert_eq!(live, baseline, "dropped subscribe still reaps its op");
        central
            .subscribe(
                &peer,
                &hrm_selector(0),
                "consumer-drop",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("post-drop admission");
    }

    #[tokio::test]
    async fn f03_dropped_read_descriptor_still_releases() {
        let central = open().await;
        let peer = "peer-f03rd".to_owned();
        ready_peer(&central, &peer, vec![hrm_service()]).await;
        central.boundary().set_mtu(&peer, 23);
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            Some(USER_DESCRIPTION),
            Some(0),
        )
        .expect("descriptor selector");
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Read);
        let caller = central.clone();
        let peer_clone = peer.clone();
        let selector_clone = selector.clone();
        let pending = tokio::spawn(async move {
            caller
                .read_descriptor(&peer_clone, &selector_clone, OpControl::budget_ms(5000))
                .await
        });
        let mut saw_live = false;
        for _ in 0..200 {
            let live = central.with_core(|core| core.live_operation_count()).await;
            if live > baseline {
                saw_live = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            saw_live,
            "descriptor read in flight before abort (non-vacuous)"
        );
        pending.abort();
        let _ = pending.await;
        central.boundary().unblock_op(FaultOp::Read);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let live = central.with_core(|core| core.live_operation_count()).await;
        assert_eq!(live, baseline, "dropped descriptor read still reaps its op");
        central
            .read_descriptor(&peer, &selector, OpControl::budget_ms(5000))
            .await
            .expect("post-drop admission");
    }

    #[tokio::test]
    async fn f03_dropped_write_descriptor_still_releases() {
        let central = open().await;
        let peer = "peer-f03wd".to_owned();
        ready_peer(&central, &peer, vec![hrm_service()]).await;
        central.boundary().set_mtu(&peer, 23);
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            Some(USER_DESCRIPTION),
            Some(0),
        )
        .expect("descriptor selector");
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Write);
        let caller = central.clone();
        let peer_clone = peer.clone();
        let selector_clone = selector.clone();
        let pending = tokio::spawn(async move {
            caller
                .write_descriptor(
                    &peer_clone,
                    &selector_clone,
                    vec![1],
                    OpControl::budget_ms(5000),
                )
                .await
        });
        let mut saw_live = false;
        for _ in 0..200 {
            let live = central.with_core(|core| core.live_operation_count()).await;
            if live > baseline {
                saw_live = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            saw_live,
            "descriptor write in flight before abort (non-vacuous)"
        );
        pending.abort();
        let _ = pending.await;
        central.boundary().unblock_op(FaultOp::Write);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let live = central.with_core(|core| core.live_operation_count()).await;
        assert_eq!(
            live, baseline,
            "dropped descriptor write still reaps its op"
        );
        central
            .write_descriptor(&peer, &selector, vec![1], OpControl::budget_ms(5000))
            .await
            .expect("post-drop admission");
    }

    #[tokio::test]
    async fn f02_sequential_ops_reclaim_aggregate_and_observations() {
        let central = open().await;
        ready_peer(&central, "peer-f02", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        let baseline_live = central.with_core(|core| core.live_operation_count()).await;
        let baseline_typed = central.with_core(|core| core.typed_effects().len()).await;
        for i in 0..1000u32 {
            let value = central
                .read("peer-f02", &selector, OpControl::budget_ms(5000))
                .await
                .unwrap_or_else(|error| panic!("read {i} admitted: {error:?}"));
            assert_eq!(value, vec![0x42]);
            if i % 250 == 0 {
                assert_eq!(
                    central.with_core(|core| core.live_operation_count()).await,
                    baseline_live,
                    "live reclaimed at iteration {i}"
                );
                assert!(
                    central.with_core(|core| core.typed_effects().len()).await
                        <= baseline_typed + 4,
                    "typed ledger recycled at iteration {i}"
                );
            }
        }
        for _ in 0..20u32 {
            central
                .start_scan("owner-f02", &[], OpControl::budget_ms(5000))
                .await
                .expect("scan admitted");
            stop_owned_scan(&central).await.expect("scan stopped");
        }
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            baseline_live,
            "live returns to baseline after 1000 reads + scan cycles"
        );
        assert!(
            central.with_core(|core| core.typed_effects().len()).await <= baseline_typed + 4,
            "observations recycled"
        );
        let value = central
            .read("peer-f02", &selector, OpControl::budget_ms(5000))
            .await
            .expect("admission remains");
        assert_eq!(value, vec![0x42]);
    }

    #[tokio::test]
    async fn f15_shutdown_drives_incremental_destroy_with_pending_work() {
        use ubm_core::ownership::CleanupState;

        let central = open().await;
        central.boundary().block_op(FaultOp::Connect);
        // 16 dispatched connects pile up behind the blocked radio with
        // generous deadlines, so shutdown — not a timeout — must settle
        // every remainder and ack every terminal. (One gate waiter releases
        // per unblock, so the stuck callers resolve via their own deadline
        // arms after shutdown settles their ops.)
        let mut pending = Vec::new();
        for index in 0..16u32 {
            let task = central.clone();
            let peer = format!("peer-f15-{index}");
            let lease = format!("lease-{index}");
            pending.push(tokio::spawn(async move {
                task.connect(&peer, &lease, OpControl::budget_ms(5000))
                    .await
            }));
        }
        for _ in 0..200 {
            if central.with_core(|core| core.live_operation_count()).await == 16 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            16,
            "all 16 connects dispatched behind the blocked radio"
        );
        let report = tokio::time::timeout(Duration::from_secs(10), central.shutdown())
            .await
            .expect("shutdown completes while callers are stuck");
        let record = report.record.expect("destroy drive succeeds");
        assert_eq!(
            record.state(),
            CleanupState::Released,
            "cancelled-then-acked workload releases clean"
        );
        assert!(record.failures().is_empty(), "no failures preserved");
        assert!(
            report.destroy_steps >= 2,
            "settle+ack workload needs more than the single idle pass"
        );
        assert!(
            report.radio_close_failures.is_empty(),
            "no subscriptions were live, so no close receipts"
        );
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            0,
            "no live ops remain after acknowledged destroy"
        );
        // The stuck callers resolve via their deadline arms: their ops
        // already settled under shutdown, so they observe the winning
        // terminal (cancelled family) instead of hanging or succeeding late.
        for task in pending {
            let outcome = tokio::time::timeout(Duration::from_secs(15), task)
                .await
                .expect("caller resolves via deadline")
                .expect("task joins");
            assert!(outcome.is_err(), "stuck connect fails, never succeeds late");
        }
        // Admission stays closed after the acknowledged shutdown.
        let error = central
            .connect("peer-late", "lease-late", OpControl::budget_ms(5000))
            .await
            .expect_err("no connects after shutdown");
        assert_eq!(error.code_str(), "adapter.unavailable");
    }

    #[tokio::test]
    async fn f15_shutdown_record_preserves_disconnect_failure() {
        use ubm_core::ownership::CleanupState;

        let central = open().await;
        ready_peer(&central, "peer-f15d", vec![hrm_service()]).await;
        central
            .boundary()
            .fail_next(FaultOp::Disconnect, "os refused");
        let report = central.shutdown().await;
        assert!(
            central
                .boundary()
                .calls()
                .contains(&"disconnect".to_owned()),
            "shutdown releases the owned link"
        );
        let record = report.record.expect("destroy drive succeeds");
        assert_eq!(
            record.state(),
            CleanupState::ReleaseFailed,
            "refused disconnect flips the final record"
        );
        assert_eq!(
            record.failures().len(),
            1,
            "the refused disconnect is preserved exactly once"
        );
        let peer_key = central
            .peer_key_for("peer-f15d")
            .await
            .expect("peer still known");
        assert_eq!(
            central
                .with_core(|core| core.connection_state(&peer_key))
                .await,
            Some(ConnectionState::Disconnecting),
            "failed release leaves the link Disconnecting, never silently Connected"
        );
    }

    #[tokio::test]
    async fn f14_shutdown_releases_owned_link() {
        use ubm_core::ownership::CleanupState;

        let central = open().await;
        ready_peer(&central, "peer-f14l", vec![hrm_service()]).await;
        let report = central.shutdown().await;
        assert!(
            central
                .boundary()
                .calls()
                .contains(&"disconnect".to_owned()),
            "shutdown releases the owned OS link"
        );
        let peer_key = central
            .peer_key_for("peer-f14l")
            .await
            .expect("peer still known");
        assert_eq!(
            central
                .with_core(|core| core.connection_state(&peer_key))
                .await,
            Some(ConnectionState::Disconnected),
            "released link confirms Disconnected in the core"
        );
        let record = report.record.expect("destroy drive succeeds");
        assert_eq!(record.state(), CleanupState::Released);
        assert!(report.radio_close_failures.is_empty());
        // Idempotent: a second shutdown re-reports clean without new radio work.
        let disconnects = central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "disconnect")
            .count();
        let again = central.shutdown().await;
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "disconnect")
                .count(),
            disconnects,
            "repeat shutdown issues no new disconnects"
        );
        assert_eq!(
            again.record.expect("second drive succeeds").state(),
            CleanupState::Released
        );
    }

    #[tokio::test]
    async fn f14_shutdown_surfaces_close_failures() {
        use ubm_core::ownership::CleanupState;

        let central = open().await;
        ready_peer(&central, "peer-f14c", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe(
                "peer-f14c",
                &selector,
                "consumer-c",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe");
        central
            .boundary()
            .fail_next(FaultOp::Unsubscribe, "os stuck");
        let report = central.shutdown().await;
        assert_eq!(
            report.radio_close_failures.len(),
            1,
            "exactly one close receipt"
        );
        assert_eq!(report.radio_close_failures[0].detail, "os stuck");
        assert_eq!(report.radio_close_failures[0].scope.0, "peer-f14c");
        assert_eq!(
            central.boundary().live_subscription_count(),
            1,
            "failed scope stays live at the radio"
        );
        // The core-tracked cleanup still succeeded: the radio failure is
        // reported alongside in the same report, neither hidden inside the
        // core record nor conflated with it.
        let record = report.record.expect("destroy drive succeeds");
        assert_eq!(record.state(), CleanupState::Released);
    }

    /// PR210-05 admission race: a cancel that lands while the op waits for
    /// the core lock is recorded on the ticket, and the admission — which
    /// then publishes inside the same critical section — refuses the op
    /// before any radio call and releases its core op.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_while_admission_waits_refuses_publication() {
        use crate::op_control::CancelAck;

        let central = open().await;
        ready_peer(&central, "peer-race", vec![hrm_service()]).await;
        let live_before = central.with_core(|core| core.live_operation_count()).await;
        let ctl = OpControl::budget_ms(5000);
        let ticket = ctl.ticket.clone();
        let core_guard = central.inner.core.lock().await;
        let pending = tokio::spawn({
            let central = central.clone();
            async move { central.read("peer-race", &hrm_selector(0), ctl).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!pending.is_finished(), "the read waits on the core lock");
        assert_eq!(ticket.operation_id(), None, "not yet admitted");
        // `cancel` needs only the ticket lock before admission.
        let ack = ticket.record_cancel();
        assert_eq!(
            ack,
            crate::op_control::CancelRequest::RecordedBeforeAdmission
        );
        ticket.wake();
        drop(core_guard);
        let error = pending
            .await
            .expect("join")
            .expect_err("refused at publication");
        assert_eq!(error.code_str(), "operation.aborted");
        assert_eq!(
            error.commit(),
            Some(ubm_core::contracts::CommitState::NotDispatched)
        );
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"read_characteristic".to_owned()),
            "no radio call"
        );
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            live_before,
            "the refused op was cancelled and released"
        );
        assert_eq!(
            central.cancel(&ticket).await.expect("late cancel"),
            CancelAck::AlreadySettled
        );
    }
}

/// Test-only access to the core for state assertions. Real hosts observe
/// through the typed API, never through this lock.
#[cfg(test)]
pub(crate) mod test_support {
    use super::DesktopCentral;
    use crate::boundary::RadioBoundary;

    impl<B: RadioBoundary> DesktopCentral<B> {
        pub(crate) async fn with_core<T>(
            &self,
            view: impl FnOnce(&ubm_core::central::Central) -> T,
        ) -> T {
            let core = self.inner.core.lock().await;
            view(&core)
        }

        pub(crate) async fn with_core_mut<T>(
            &self,
            view: impl FnOnce(&mut ubm_core::central::Central) -> T,
        ) -> T {
            let mut core = self.inner.core.lock().await;
            view(&mut core)
        }
    }
}

#[cfg(test)]
mod bounded_queue_tests {
    use std::collections::VecDeque;

    /// The advertisement queue's eviction (F22) at a small cap: the oldest
    /// goes first and every eviction is reported.
    #[test]
    fn push_evicting_drops_the_oldest_and_reports_it() {
        let mut queue = VecDeque::new();
        let evictions: Vec<bool> = (0..5)
            .map(|item| super::push_evicting(&mut queue, item, 3))
            .collect();
        assert_eq!(evictions, vec![false, false, false, true, true]);
        assert_eq!(queue, VecDeque::from([2, 3, 4]));
    }
}
