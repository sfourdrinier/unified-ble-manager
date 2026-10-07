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

#[path = "central_acquired.rs"]
mod acquired;
pub use acquired::AcquiredGattHandle;

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{
    Arc, Mutex as StdMutex, MutexGuard, OnceLock, PoisonError, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, broadcast, watch};
use ubm_core::central::{
    Central, CentralEffectKind, CentralResourceCounters, CompletionOutcome, ConnectionState,
    DatabaseState, PathSelector, StoredPath, canonical_uuid, validate_scan_request,
};
use ubm_core::contracts::{
    AttachmentTuple, BleErrorCode, BleErrorDomain, CommitState, ContenderKind, CoreError,
    Generation, OperationId, OperationTerminalKind,
};
use ubm_core::ownership::{CleanupFailure, CleanupRecord, CleanupState, EffectBatch};

use crate::boundary::{
    AdapterAuthorization, AdapterAvailability, AdapterLossCause, AdapterPowerState,
    AdmissionPolicy, CharacteristicAccess, CharacteristicRead, DeliveryMode, GattSnapshotIdentity,
    InstanceKey, ObservedDelivery, PeerSnapshot, RadioBoundary, RadioCloseFailure, RadioEvent,
    ScanFilterSpec, ServiceAccess,
};
use ubm_core::central::ScanDuplicatePolicy;

#[path = "central_parity.rs"]
mod parity;
use crate::errors::{DesktopError, Retryability};
use crate::identity::{AttachmentEpoch, DesktopIdentity, HostIdentity};
use crate::op_control::{
    Budget, COMPENSATION_TIMEOUT, CancelAck, CancelRequest, LIVENESS_BACKSTOP_DETAIL,
    LIVENESS_CLEANUP, LIVENESS_OP, LIVENESS_SCAN_START, OpControl, OpTicket, SettleOnDrop, Window,
};
pub use parity::{
    CancelPairingOutcome, ConnectionParametersEvent, ControllerFuture, PairRequest,
    PairingGeneration, PairingGenerationController, ScanTerminalEvent, SecureConnections,
    SecurityEvent, WriteReadinessEvent, cancel_outcome_for, canonical_address,
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

fn drain_consumer_before_retirement(
    core: &mut Central,
    index: usize,
    consumer: &str,
    drain: Option<&(dyn Fn(NotificationPoll) + Send + Sync)>,
) {
    if let Some(drain) = drain {
        while let Some(value) = core.take_notification_value(index, consumer) {
            drain(NotificationPoll::Value(value));
        }
        if let Some(terminal) = core.take_terminal(index, consumer) {
            drain(NotificationPoll::Terminal(terminal));
        }
    }
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

/// Maximum retained completed-scan tickets per central.
///
/// This is the same 256-entry bound as the public scan-state contract. A
/// ticket is only a duplicate-cleanup acknowledgement, so retaining more
/// historical tickets than one bounded scan-state window would make a
/// long-lived central grow with its lifetime. The newest 256 completed scans
/// retain their exact terminal; an older ticket from this central's immutable
/// operation namespace answers the explicit expired-lifecycle outcome below.
pub const COMPLETED_SCAN_TICKET_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletedScanTicket {
    Settled(OperationTerminalKind),
    Local,
    Expired,
    Foreign,
    Unknown,
}

struct CompletedScanTickets {
    tickets: HashMap<OperationId, OperationTerminalKind>,
    order: VecDeque<OperationId>,
    operation_namespace: String,
}

impl CompletedScanTickets {
    fn new(scope: &str) -> Self {
        Self {
            tickets: HashMap::new(),
            order: VecDeque::new(),
            operation_namespace: scope.to_owned(),
        }
    }

    fn retain(&mut self, op: &OperationId, kind: OperationTerminalKind) {
        if self.tickets.contains_key(op) {
            return;
        }
        while self.tickets.len() >= COMPLETED_SCAN_TICKET_CAPACITY {
            let Some(expired) = self.order.pop_front() else {
                break;
            };
            self.tickets.remove(&expired);
        }
        self.order.push_back(op.clone());
        self.tickets.insert(op.clone(), kind);
    }

    fn status(&self, op: &OperationId) -> CompletedScanTicket {
        if let Some(kind) = self.tickets.get(op) {
            return CompletedScanTicket::Settled(*kind);
        }
        match scan_operation_scope(op) {
            Some(scope) if scope == self.operation_namespace => CompletedScanTicket::Local,
            Some(_) => CompletedScanTicket::Foreign,
            None => CompletedScanTicket::Unknown,
        }
    }
}

/// Extract the immutable central namespace from an opaque *scan* operation id.
/// The length prefix makes this unambiguous even when a host's attachment id
/// contains a separator used by the serialized form.
fn scan_operation_scope(operation: &OperationId) -> Option<&str> {
    let rest = operation.as_str().strip_prefix("central-op/")?;
    let (length, rest) = rest.split_once('/')?;
    let length = length.parse::<usize>().ok()?;
    let scope = rest.get(..length)?;
    let suffix = rest.get(length..)?.strip_prefix('/')?;
    let mut fields = suffix.split('/');
    let (Some(class), Some(ordinal), Some(tag), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return None;
    };
    (class == "scan")
        .then(|| {
            ordinal
                .parse::<u64>()
                .ok()
                .zip(u64::from_str_radix(tag, 16).ok())
                .map(|_| scope)
        })
        .flatten()
}

type ScanTickets = StdMutex<CompletedScanTickets>;

fn lock_std<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Retain one scan's completed ticket (R15, first writer wins): call with
/// the core lock held, between settlement and release, so no racing
/// duplicate can observe a settled-but-unretained op. Sync: no await
/// between settle and retain, and none inside.
fn retain_completed_scan(tickets: &ScanTickets, op: &OperationId, kind: OperationTerminalKind) {
    lock_std(tickets).retain(op, kind);
}

fn completed_scan_ticket(tickets: &ScanTickets, op: &OperationId) -> CompletedScanTicket {
    lock_std(tickets).status(op)
}

fn resolved_scan_ticket(
    core: &Central,
    tickets: &ScanTickets,
    operation: &OperationId,
) -> CompletedScanTicket {
    match completed_scan_ticket(tickets, operation) {
        CompletedScanTicket::Local if core.issued_scan_operation_id(operation) => {
            CompletedScanTicket::Expired
        }
        CompletedScanTicket::Local => CompletedScanTicket::Unknown,
        ticket => ticket,
    }
}

fn expired_scan_ticket_error() -> DesktopError {
    DesktopError::new(
        BleErrorCode::LifecycleInvalidState,
        BleErrorDomain::Scan,
        "scan.ticket",
    )
    .with_detail("scan duplicate acknowledgement window expired")
}

fn foreign_scan_ticket_error() -> DesktopError {
    DesktopError::new(
        BleErrorCode::OwnershipDenied,
        BleErrorDomain::Scan,
        "scan.ticket",
    )
    .with_detail("scan ticket belongs to another central attachment")
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
    // One word per event on every host, the platform's answer kept (owner
    // decision, 5.0): a link the platform says is gone is `connection.lost`,
    // a refusal for lack of security is `platform.security`.
    let error = error.classify_link_loss().classify_security();
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

fn link_end_count<B>(inner: &Inner<B>, peer_id: &str) -> u64 {
    lock_std(&inner.link_ends)
        .get(peer_id)
        .copied()
        .unwrap_or(0)
}

fn confirmed_release_count<B>(inner: &Inner<B>, peer_id: &str) -> u64 {
    lock_std(&inner.confirmed_releases)
        .get(peer_id)
        .copied()
        .unwrap_or(0)
}

fn note_confirmed_release<B>(inner: &Inner<B>, peer_id: &str) {
    let mut releases = lock_std(&inner.confirmed_releases);
    let count = releases.entry(peer_id.to_owned()).or_insert(0);
    *count = count.wrapping_add(1);
}

/// Record that the OS reported `peer_id`'s link ended and wake every link
/// operation waiting on it. Called after the core state moved, so the
/// woken operation names the end from that state (`name_link_end`).
fn note_link_end<B>(inner: &Inner<B>, peer_id: &str) {
    {
        let mut ends = lock_std(&inner.link_ends);
        let count = ends.entry(peer_id.to_owned()).or_insert(0);
        *count = count.wrapping_add(1);
    }
    inner.link_end.notify_waiters();
}

/// [`drive`] for an operation on `peer_id`'s link: it also ends when the
/// OS reports that link ended, as `connection.lost` (renamed
/// `operation.disconnected` when the app's own release ended it). The radio
/// answer wins a tie.
async fn drive_link<B, T>(
    inner: &Inner<B>,
    peer_id: &str,
    operation: &'static str,
    ticket: &OpTicket,
    window: Window,
    work: impl Future<Output = Result<T, DesktopError>>,
) -> Wait<Result<T, DesktopError>> {
    let start = link_end_count(inner, peer_id);
    let ended = async {
        loop {
            let notified = inner.link_end.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if link_end_count(inner, peer_id) != start {
                return;
            }
            notified.await;
        }
    };
    tokio::select! {
        biased;
        outcome = drive(ticket, window, work) => outcome,
        () = ended => Wait::Done(Err(contract_error(
            BleErrorCode::ConnectionLost,
            BleErrorDomain::Connection,
            operation,
        )
        .with_detail("the OS reported the link ended while the operation waited on the radio"))),
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
    Connect {
        peer_id: String,
        peer_key: String,
        generation: Option<String>,
        adapter_epoch: u64,
        release_serial: u64,
    },
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

    fn note_native_acquisition(&mut self, confirmed_release: u64) {
        if let Some(DropCleanup::Connect { release_serial, .. }) = self.cleanup.as_mut() {
            *release_serial = confirmed_release;
        }
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
        DropCleanup::Connect {
            peer_id,
            peer_key,
            generation,
            adapter_epoch,
            release_serial,
        } => {
            {
                let mut core = central.inner.core.lock().await;
                let same = central.retain_half_open_cleanup(
                    &core,
                    &peer_id,
                    &peer_key,
                    &generation,
                    adapter_epoch,
                    release_serial,
                );
                let mut out = batch();
                if same {
                    let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                }
                let _ = out.drain();
            }
            let _ = central.cancel_operation(&operation).await;
            reap_if_terminal(&central, &operation).await;
            // A half-open OS link must not linger ownerless. Bounded and
            // outside the core lock per F24.
            central.compensate_half_open(&peer_id, &generation).await;
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

/// The original operation's release answer, independent of event delivery.
#[derive(Debug, Clone)]
pub struct ConnectionReleaseReport {
    /// Whether the final local lease drove link release.
    pub physical: bool,
    /// The exact public generation admitted before the native request.
    pub connection_generation: Option<String>,
    /// Actual native detail, never inferred from requested intent.
    pub platform: Option<crate::errors::PlatformDetail>,
}

struct DisconnectReport {
    release: LinkRelease,
    connection_generation: Option<String>,
    platform: Option<crate::errors::PlatformDetail>,
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
    let mut service_identities = HashSet::new();
    for service in services {
        well_formed(&service.uuid, "service")?;
        let uuid = ubm_core::central::canonical_uuid(&service.uuid).map_err(DesktopError::from)?;
        if !service_identities.insert((uuid, service.occurrence)) {
            return Err(contract_error(
                BleErrorCode::ProtocolViolation,
                BleErrorDomain::Gatt,
                "discovery.snapshot.service-identity",
            ));
        }
    }
    for service in services {
        well_formed(&service.uuid, "service")?;
        entries += 1;
        if let Some(included) = &service.included_services {
            for reference in included {
                well_formed(&reference.uuid, "included service")?;
                let uuid = ubm_core::central::canonical_uuid(&reference.uuid)
                    .map_err(DesktopError::from)?;
                if !service_identities.contains(&(uuid, reference.occurrence)) {
                    return Err(contract_error(
                        BleErrorCode::ProtocolViolation,
                        BleErrorDomain::Gatt,
                        "discovery.snapshot.included-service",
                    ));
                }
                entries += 1;
            }
        }
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
    /// Service-level restriction. `None` is an open service, or any
    /// characteristic or descriptor path.
    pub service_access: Option<ServiceAccess>,
    pub service_primary: Option<bool>,
    pub included_services: Option<Vec<crate::boundary::IncludedServiceReference>>,
}

/// Observed service graph facts belonging to one current database.
#[derive(Clone)]
struct ServiceGraphFacts {
    access: ServiceAccess,
    primary: Option<bool>,
    included_services: Option<Vec<crate::boundary::IncludedServiceReference>>,
}

/// Authoritative per-central shutdown outcome (F14/F15): the final
/// cleanup record plus every close-time release failure. A clean shutdown
/// reports `Released` with no radio failures; anything else names exactly
/// what did not release.
#[derive(Debug)]
pub struct ShutdownReport {
    /// Logical core cleanup component, taken only after every queued op settled,
    /// every dispatched remainder was answered, and every terminal release
    /// was acknowledged (F15). `Released` only when disconnect failures and
    /// retained release failures are all absent; otherwise `ReleaseFailed`
    /// with every failure preserved. `Err` only when the destroy drive
    /// itself failed (a core invariant violation), never for radio faults —
    /// those land in the record or the separately named physical failures.
    /// This component alone is not an overall release verdict; use `is_released`.
    pub record: Result<CleanupRecord, DesktopError>,
    /// Close-time native release failures drained from the radio (F14
    /// receipts): one entry per characteristic scope whose unsubscribe did
    /// not complete. Empty means every live scope released.
    pub radio_close_failures: Vec<RadioCloseFailure>,
    /// Event-transport cleanup after the event consumer joined. This is not
    /// a GATT scope; a failure retains backend ownership for a later retry.
    pub transport_close_failures: Vec<DesktopError>,
    /// Current physical cleanup debt from failed or cancelled connection
    /// acquisition. These original native causes are separate from the
    /// immutable core cleanup record; an overall release requires this
    /// vector to be empty. A confirmed retry retires only the exact debt.
    pub half_open_close_failures: Vec<DesktopError>,
    /// Incremental destroy passes executed (F15): more than one when the
    /// destroy workload exceeds one effect batch.
    pub destroy_steps: usize,
    /// The final OS scan stop failure, when the owned scan could not be
    /// stopped during shutdown (PR210-09). `None` when no scan was owned or
    /// the stop succeeded.
    pub scan_stop_failure: Option<DesktopError>,
}

impl ShutdownReport {
    /// True only when logical ownership and every physical release stage
    /// confirmed cleanup. Historical diagnostics do not override this answer.
    pub fn is_released(&self) -> bool {
        matches!(&self.record, Ok(record) if record.state() == CleanupState::Released)
            && self.radio_close_failures.is_empty()
            && self.transport_close_failures.is_empty()
            && self.half_open_close_failures.is_empty()
            && self.scan_stop_failure.is_none()
    }
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
    /// The OS's own observed detail, when available; never inferred from a request.
    pub platform: Option<crate::errors::PlatformDetail>,
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
    /// The connection-parameter report also published on
    /// [`DesktopCentral::connection_parameter_events`].
    ConnectionParameters(parity::ConnectionParametersEvent),
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
    /// The adapter power fact that caused this reset. This is the core's
    /// observation at the reset boundary, not a later status read.
    pub power: Option<AdapterPowerState>,
    /// The matching adapter-event sequence for the causal power, authorization,
    /// or direct-loss state observation.
    pub adapter_sequence: Option<u64>,
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
        "peers.connected",
        "peers.bonded",
        "peers.resolve",
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
pub(crate) fn admission_refusal(
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
    /// Identity vocabulary of the selected native directory mechanism.
    /// Scripted hosts set this explicitly alongside their capability profile.
    pub directory_os: crate::capabilities::DesktopOs,
    /// The owning host's names for every scope the central opens
    /// ([`HostIdentity`]): the central mints no identity of its own.
    pub identity: Arc<dyn HostIdentity>,
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
    /// The desktop btleplug profile: the desktop identity with backend
    /// label `btleplug` for `owner` (host identity, e.g. `"node"`), desktop
    /// capability registration, no observer.
    #[must_use]
    pub fn desktop(owner: &str) -> Self {
        Self {
            directory_os: crate::capabilities::DesktopOs::current()
                .unwrap_or(crate::capabilities::DesktopOs::MacOs),
            identity: Arc::new(DesktopIdentity::new("btleplug", owner)),
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
            .field("identity", &self.identity)
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
    /// Synchronous native GATT slots, including workers waiting to start.
    pub native_gatt_admissions: usize,
    /// Acquired descriptors still owned, including cleanup debt.
    pub acquired_gatt_transports: usize,
    /// FD acquisitions awaiting their native answer.
    pub pending_gatt_acquisitions: usize,
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
    /// A stop won while the OS start was still in flight. Keep the identity
    /// until that start settles, because it can turn the radio on afterward.
    StartCancelled,
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
    /// Native start has authoritatively refused while an earlier stop is
    /// still in flight. Once that stop settles, even a failed stop leaves
    /// no scan to retain.
    start_refused: bool,
    /// The start returned no session, so only this central owns its retained
    /// compensating stop. A later start may retry this debt, never a published
    /// scan owner's failed stop.
    unpublished_cleanup: bool,
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

#[derive(Default)]
struct DiscoverySnapshot {
    generations: Option<(String, String)>,
    identity: Option<GattSnapshotIdentity>,
    report: Option<DiscoveryReport>,
    leases: HashSet<String>,
}

#[derive(Default)]
struct DiscoveryCoordinator {
    completed: AtomicU64,
    snapshot: Mutex<DiscoverySnapshot>,
}

fn discovery_coordinator<B>(inner: &Inner<B>, peer_id: &str) -> Arc<DiscoveryCoordinator> {
    lock_std(&inner.discoveries)
        .entry(peer_id.to_owned())
        .or_default()
        .clone()
}

struct HalfOpenCleanup {
    peer_key: String,
    generation: Option<String>,
    release_serial: AtomicU64,
    adapter_epoch: u64,
    // Serializes compensation and shutdown retries; false means not confirmed.
    released: Mutex<bool>,
    failure: StdMutex<Option<DesktopError>>,
}

type LeaseReleaseGate = Mutex<Option<ConnectionReleaseReport>>;
type LeaseReleaseGates = StdMutex<HashMap<(String, String), Weak<LeaseReleaseGate>>>;
type LeaseChild = (PathSelector, String);
type PendingLeaseChildren = StdMutex<HashMap<(String, String), Vec<LeaseChild>>>;
type DiscoveredLeaseGenerations = (String, Option<String>, Option<String>);
type DiscoveredLeases = StdMutex<HashMap<(String, String), DiscoveredLeaseGenerations>>;

struct Inner<B> {
    acquired: Arc<crate::acquired_gatt::ownership::Registry>,
    /// Completed discovery admission per live lease, with exact generations.
    discovered_leases: DiscoveredLeases,
    directory_os: crate::capabilities::DesktopOs,
    core: Mutex<Central>,
    boundary: B,
    /// The current attachment; a reset replaces it (finding 57).
    attachment: StdMutex<AttachmentTuple>,
    /// The owning host's names for each new scope.
    identity: Arc<dyn HostIdentity>,
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
    /// Exact `(peer key, lease)` pairs confirmed loss or adapter resets ended: the core
    /// cleared them, and their release answers
    /// already-released once (legacy adapter-loss cleanup left
    /// terminalized handles).
    retired_leases: StdMutex<HashMap<(String, String), Option<String>>>,
    /// Consumers whose physical obligation ended with a confirmed link loss
    /// or adapter reset. A later failed reconnect may erase their old paths.
    retired_consumers: StdMutex<HashSet<(String, String)>>,
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
    /// One physical snapshot per peer; lease attachments do not rediscover
    /// or invalidate another owner's current logical database.
    discoveries: StdMutex<HashMap<String, Arc<DiscoveryCoordinator>>>,
    /// Actual observation failure: retained for this host lifetime. Read-only
    /// FIFO drainage and teardown remain available after GATT admission stops.
    gatt_watch_failure: StdMutex<Option<DesktopError>>,
    /// Peer-scoped observation failures are repairable by explicit discovery.
    gatt_observation_failures: StdMutex<HashMap<String, DesktopError>>,
    lease_releases: LeaseReleaseGates,
    pending_lease_children: PendingLeaseChildren,
    half_open_cleanup: StdMutex<HashMap<String, Arc<HalfOpenCleanup>>>,
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
    /// How many times the OS reported each peer's link ended. A link
    /// operation still waiting on the radio when this moves ends at once
    /// (owner decision, 5.0), as Android's stack fails pending work at a
    /// disconnect: a radio that never answers (CoreBluetooth) no longer
    /// holds it until its deadline.
    link_ends: StdMutex<HashMap<String, u64>>,
    // Unlike link_ends (which also wakes operations at release REQUEST),
    // this serial advances only on confirmed physical release. It orders
    // native acquisition versus later release for delayed driver drops.
    confirmed_releases: StdMutex<HashMap<String, u64>>,
    /// Woken on every [`Inner::link_ends`] change.
    link_end: tokio::sync::Notify,
    gatt_admission: Arc<crate::gatt_admission::GattAdmissionQueue>,
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
    /// Service restrictions from the last successful discovery, keyed by
    /// peer, service UUID, and service occurrence. Open services are absent.
    /// A failed discovery leaves the previous notes in place.
    service_access: StdMutex<HashMap<(String, String, u64), ServiceGraphFacts>>,
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
    /// Observed connection-parameter reports.
    connection_parameters: broadcast::Sender<parity::ConnectionParametersEvent>,
    connection_parameters_sequence: AtomicU64,
    parameter_source_failures: StdMutex<HashMap<String, (Option<String>, DesktopError)>>,
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
    native_wake: broadcast::Sender<()>,
    shut_down: AtomicBool,
    shutdown_release_confirmed: AtomicBool,
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
        self.stage_lifecycle_with_platform(peer_id, peer_key, generation, kind, None)
    }

    fn stage_lifecycle_with_platform(
        &self,
        peer_id: &str,
        peer_key: &str,
        generation: Generations,
        kind: LifecycleKind,
        platform: Option<crate::errors::PlatformDetail>,
    ) -> LifecycleEvent {
        let sequence = self.lifecycle_sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let event = LifecycleEvent {
            sequence,
            peer_id: peer_id.to_owned(),
            peer_key: peer_key.to_owned(),
            connection_generation: generation.connection,
            database_generation: generation.database,
            kind,
            platform,
        };
        let received = self.lifecycle.send(event.clone()).is_ok();
        if !received && self.observer.is_none() {
            self.lifecycle_unobserved.fetch_add(1, Ordering::Relaxed);
        }
        event
    }

    /// Deliver one signal to the observer. Call with no central lock held.
    fn signal(&self, signal: CentralSignal) {
        let _ = self.native_wake.send(());
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
    Lead(watch::Sender<StopAnswer>, bool),
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
                if active.start_refused {
                    *slot = None;
                } else {
                    active.phase = ScanPhase::StopFailed;
                }
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
                &format!("{}.open", profile.identity.namespace()),
            ));
        }
        profile.identity.validate()?;
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
        let epoch = AttachmentEpoch {
            ordinal,
            resets: 0,
            adapter: &adapter_label,
        };
        let attachment = profile
            .identity
            .attachment(epoch)
            .map_err(DesktopError::from)?;
        let generation = profile
            .identity
            .kernel_generation(epoch)
            .map_err(DesktopError::from)?;
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
        crate::capabilities::apply_connection_capability_limitation(
            &mut core,
            boundary.connection_capability_limitation(),
        )
        .map_err(DesktopError::from)?;
        crate::capabilities::apply_when_available_capability_limitation(
            &mut core,
            boundary.when_available_capability_limitation(),
        )
        .map_err(DesktopError::from)?;
        let (loop_stop, loop_stop_rx) = watch::channel(false);
        crate::capabilities::apply_connection_parameters_capability_limitation(
            &mut core,
            boundary.connection_parameters_capability_limitation()?,
        )
        .map_err(DesktopError::from)?;
        let (lifecycle, _) = broadcast::channel(LIFECYCLE_EVENT_CAPACITY);
        crate::capabilities::apply_control_capability_limitation(
            &mut core,
            "connection:priority",
            boundary.priority_capability_limitation()?,
        )
        .map_err(DesktopError::from)?;
        let (known_limit, connected_limit) =
            boundary.peer_directory_capability_limitations().await?;
        crate::capabilities::apply_control_capability_limitation(
            &mut core,
            "peer:known",
            known_limit,
        )
        .map_err(DesktopError::from)?;
        crate::capabilities::apply_control_capability_limitation(
            &mut core,
            "peer:system-connected",
            connected_limit,
        )
        .map_err(DesktopError::from)?;
        let (adapter, _) = broadcast::channel(LIFECYCLE_EVENT_CAPACITY);
        let admission = boundary.admission_policy();
        let teardown_on_loss = boundary.tears_down_on_adapter_loss();
        let facts = seed_adapter_facts(&boundary, admission, profile.identity.log_tag()).await;
        let ticket_scope = attachment.attachment_id().as_str().to_owned();
        let inner = Arc::new(Inner {
            acquired: crate::acquired_gatt::ownership::Registry::new(ordinal),
            discovered_leases: StdMutex::new(HashMap::new()),
            directory_os: profile.directory_os,
            core: Mutex::new(core),
            boundary,
            attachment: StdMutex::new(attachment),
            identity: profile.identity,
            ordinal,
            resets: AtomicU64::new(0),
            adapter_facts: StdMutex::new(facts),
            admission,
            teardown_on_loss,
            tickets: StdMutex::new(Vec::new()),
            reset_ops: StdMutex::new(HashSet::new()),
            reset_peers: StdMutex::new(HashSet::new()),
            retired_leases: StdMutex::new(HashMap::new()),
            retired_consumers: StdMutex::new(HashSet::new()),
            reset_events: broadcast::channel(LIFECYCLE_EVENT_CAPACITY).0,
            reset_sequence: AtomicU64::new(0),
            scan: StdMutex::new(None),
            completed_scans: StdMutex::new(CompletedScanTickets::new(&ticket_scope)),
            peers: Mutex::new(HashMap::new()),
            discoveries: StdMutex::new(HashMap::new()),
            gatt_admission: crate::gatt_admission::GattAdmissionQueue::new(4096),
            gatt_watch_failure: StdMutex::new(None),
            gatt_observation_failures: StdMutex::new(HashMap::new()),
            lease_releases: StdMutex::new(HashMap::new()),
            pending_lease_children: StdMutex::new(HashMap::new()),
            half_open_cleanup: StdMutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            epochs: Mutex::new(HashMap::new()),
            link_ends: StdMutex::new(HashMap::new()),
            confirmed_releases: StdMutex::new(HashMap::new()),
            link_end: tokio::sync::Notify::new(),
            failed_disables: Mutex::new(HashSet::new()),
            deliveries: StdMutex::new(HashMap::new()),
            access: StdMutex::new(HashMap::new()),
            service_access: StdMutex::new(HashMap::new()),
            retained_enablements: StdMutex::new(HashSet::new()),
            security: broadcast::channel(LIFECYCLE_EVENT_CAPACITY).0,
            security_sequence: AtomicU64::new(0),
            pairings: StdMutex::new(HashMap::new()),
            generation_restore_failures: AtomicU64::new(0),
            write_readiness: broadcast::channel(LIFECYCLE_EVENT_CAPACITY).0,
            write_readiness_sequence: AtomicU64::new(0),
            connection_parameters: broadcast::channel(LIFECYCLE_EVENT_CAPACITY).0,
            connection_parameters_sequence: AtomicU64::new(0),
            parameter_source_failures: StdMutex::new(HashMap::new()),
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
            native_wake: broadcast::channel(1).0,
            shut_down: AtomicBool::new(false),
            shutdown_release_confirmed: AtomicBool::new(false),
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

    /// Coalesced native-consumer wake. Payloads remain in their bounded core
    /// queues; lag is a wake, never a discarded data record. Subscribe before
    /// collecting to close the collect/wait race.
    pub fn native_wakes(&self) -> broadcast::Receiver<()> {
        self.inner.native_wake.subscribe()
    }

    /// Reserve a native queue position before an asynchronous worker starts.
    pub fn admit_gatt(&self, peer_id: &str) -> Result<crate::GattAdmission, DesktopError> {
        self.inner.gatt_admission.reserve(peer_id)
    }

    pub fn bind_gatt_admission(
        &self,
        peer_id: &str,
        ctl: OpControl,
    ) -> Result<OpControl, DesktopError> {
        match ctl.gatt_admission() {
            Some(admission) if admission.belongs_to(&self.inner.gatt_admission, peer_id) => Ok(ctl),
            Some(_) => Err(contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Gatt,
                "gatt.admission",
            )),
            None => Ok(ctl.with_gatt_admission(self.admit_gatt(peer_id)?)),
        }
    }

    async fn wait_gatt_admission(
        &self,
        peer_id: &str,
        ctl: &OpControl,
        operation: &'static str,
        window: Window,
    ) -> Result<Arc<crate::GattAdmission>, DesktopError> {
        let admission = match ctl.gatt_admission() {
            Some(admission) if admission.belongs_to(&self.inner.gatt_admission, peer_id) => {
                admission
            }
            Some(_) => {
                return Err(contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    operation,
                ));
            }
            None => Arc::new(self.admit_gatt(peer_id)?),
        };
        let mut wakes = self.native_wakes();
        let waiting = async {
            loop {
                if operation == "gatt.write-when-ready" {
                    self.readiness_source_admission(peer_id, operation)?;
                } else {
                    self.gatt_watch_admission(operation)?;
                    self.gatt_peer_observation_admission(peer_id, operation)?;
                }
                tokio::select! {
                    result = admission.wait() => return result,
                    wake = wakes.recv() => {
                        if matches!(wake, Err(broadcast::error::RecvError::Closed)) {
                            return Err(contract_error(BleErrorCode::PlatformFailure, BleErrorDomain::Gatt, operation)
                                .with_detail("the native GATT admission source closed"));
                        }
                    }
                }
            }
        };
        match drive_link(
            &self.inner,
            peer_id,
            operation,
            &ctl.ticket,
            window,
            waiting,
        )
        .await
        {
            Wait::Done(result) => result.map_err(|error| classify(error, OpKind::Read, false))?,
            Wait::Expired => {
                return Err(classify(timed_out(operation, window), OpKind::Read, false));
            }
            Wait::Cancelled => {
                return Err(classify(
                    ctl.ticket.interruption(operation),
                    OpKind::Read,
                    false,
                ));
            }
        }
        self.precheck(ctl, operation)
            .map_err(|error| classify(error, OpKind::Read, false))?;
        Ok(admission)
    }

    /// Subscribe to adapter power-state changes the OS reports. Same lag
    /// rule as [`DesktopCentral::lifecycle_events`].
    #[must_use]
    pub fn adapter_events(&self) -> broadcast::Receiver<AdapterEvent> {
        self.inner.adapter.subscribe()
    }

    /// Snapshot capability states already registered on this authority.
    pub async fn capability_states(&self) -> Vec<(String, ubm_core::central::CapabilityState)> {
        self.inner.core.lock().await.registered_capability_states()
    }

    /// Registered instance descriptors, including the radio's actual refusal reasons.
    pub async fn capability_descriptors(&self) -> Vec<ubm_core::central::CapabilityDescriptor> {
        self.inner
            .core
            .lock()
            .await
            .registered_capability_descriptors()
    }

    /// Read OS-known cached identities without adopting a connection lease.
    /// Native directory identity scope, fixed by the instantiated host profile.
    pub fn directory_os(&self) -> crate::capabilities::DesktopOs {
        self.inner.directory_os
    }

    pub async fn known_directory_peers(
        &self,
        ctl: OpControl,
    ) -> Result<Vec<crate::boundary::DirectoryPeer>, DesktopError> {
        self.directory_query(
            ctl,
            "peers.known",
            self.inner.boundary.known_directory_peers(),
        )
        .await
    }

    /// Read-only system directory facts; never acquires connection ownership.
    pub async fn bonded_peers(
        &self,
        ctl: OpControl,
    ) -> Result<Vec<crate::boundary::DirectoryPeer>, DesktopError> {
        self.directory_query(ctl, "peers.bonded", self.inner.boundary.bonded_peers())
            .await
    }

    /// Read-only system directory facts; never acquires connection ownership.
    pub async fn connected_peers(
        &self,
        services: &[String],
        ctl: OpControl,
    ) -> Result<Vec<crate::boundary::DirectoryPeer>, DesktopError> {
        self.directory_query(
            ctl,
            "peers.connected",
            self.inner.boundary.connected_peers(services),
        )
        .await
    }

    pub async fn resolve_peer(
        &self,
        peer_id: &str,
        ctl: OpControl,
    ) -> Result<Option<crate::boundary::DirectoryPeer>, DesktopError> {
        self.directory_query(
            ctl,
            "peers.resolve",
            self.inner.boundary.resolve_peer(peer_id),
        )
        .await
    }

    async fn directory_query<T>(
        &self,
        ctl: OpControl,
        operation: &'static str,
        work: impl std::future::Future<Output = Result<T, DesktopError>>,
    ) -> Result<T, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        let mut shutdown = self.inner.loop_stop.subscribe();
        let epoch = self.inner.resets.load(Ordering::SeqCst);
        self.precheck(&ctl, operation)?;
        if self.inner.resets.load(Ordering::SeqCst) != epoch {
            return Err(DesktopError::new(
                BleErrorCode::OperationReset,
                BleErrorDomain::Connection,
                operation,
            ));
        }
        let feature = match operation {
            "peers.known" | "peers.resolve" => "peer:known",
            "peers.connected" => "peer:system-connected",
            _ => "peer:bonded",
        };
        {
            let core = self.inner.core.lock().await;
            if core
                .registered_capability_states()
                .iter()
                .any(|(id, state)| {
                    id == feature && *state == ubm_core::central::CapabilityState::Unavailable
                })
            {
                core.check_capability(feature, operation)
                    .map_err(DesktopError::from)?;
            }
        }
        let window = ctl.budget.window(LIVENESS_OP);
        let answer = tokio::select! {
            answer = drive(&ctl.ticket, window, work) => answer,
            _ = shutdown.wait_for(|closed| *closed) => return Err(DesktopError::adapter_unavailable(operation)),
        };
        match answer {
            Wait::Done(Ok(value)) => {
                self.admit(operation)?;
                if self.inner.resets.load(Ordering::SeqCst) != epoch {
                    return Err(DesktopError::new(
                        BleErrorCode::OperationReset,
                        BleErrorDomain::Connection,
                        operation,
                    ));
                }
                Ok(value)
            }
            Wait::Done(Err(error)) => Err(error),
            Wait::Expired => Err(classify(timed_out(operation, window), OpKind::Read, true)),
            Wait::Cancelled => Err(classify(
                ctl.ticket.interruption(operation),
                OpKind::Read,
                true,
            )),
        }
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
        match drive_link(
            &self.inner,
            peer_id,
            "connection.rssi",
            &ctl.ticket,
            window,
            self.inner.boundary.read_rssi(peer_id),
        )
        .await
        {
            Wait::Done(Ok(rssi)) => Ok(rssi),
            Wait::Done(Err(error)) => {
                return self
                    .name_link_end(&peer_key, Err(classify(error, OpKind::Read, true)))
                    .await;
            }
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
        self.refuse_before_admission(ctl, operation)?;
        if let Some(admission) = ctl.gatt_admission() {
            admission.assert_current()?;
        }
        Ok(())
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
        self.gatt_watch_admission(operation)
    }

    fn readiness_source_admission(
        &self,
        peer_id: &str,
        operation: &'static str,
    ) -> Result<(), DesktopError> {
        if let Some(cause) = lock_std(&self.inner.gatt_watch_failure).as_ref() {
            return Err(cause.clone());
        }
        self.gatt_peer_observation_admission(peer_id, operation)
    }

    fn gatt_watch_admission(&self, operation: &'static str) -> Result<(), DesktopError> {
        if matches!(
            operation,
            "discovery.complete"
                | "gatt.read"
                | "gatt.write"
                | "gatt.write-when-ready"
                | "gatt.read-descriptor"
                | "gatt.write-descriptor"
                | "gatt.subscribe"
                | "gatt.acquire-write"
                | "gatt.acquire-notify"
                | "gatt.acquired-write"
                | "gatt.acquired-receive"
        ) && let Some(cause) = lock_std(&self.inner.gatt_watch_failure).as_ref()
        {
            return Err(observation_refusal(cause, operation));
        }
        Ok(())
    }

    fn gatt_peer_observation_admission(
        &self,
        peer_id: &str,
        operation: &'static str,
    ) -> Result<(), DesktopError> {
        if !matches!(operation, "gatt.unsubscribe" | "discovery.complete")
            && let Some(cause) = lock_std(&self.inner.gatt_observation_failures).get(peer_id)
        {
            return Err(observation_refusal(cause, operation));
        }
        Ok(())
    }

    fn annotate_observation_failure(&self, peer_id: &str, mut error: DesktopError) -> DesktopError {
        if matches!(
            error.code(),
            BleErrorCode::GattStaleHandle | BleErrorCode::GattDiscoveryRequired
        ) && error.platform().is_none()
            && let Some(cause) = lock_std(&self.inner.gatt_observation_failures).get(peer_id)
        {
            if error.detail().is_none()
                && let Some(detail) = cause.detail()
            {
                error = error.with_detail(detail);
            }
            if let Some(platform) = cause.platform() {
                error = error.with_platform(platform.clone());
            }
        }
        error
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
                if let Some(winner) = core.shutdown_terminal_kind(operation) {
                    terminal_to_error(winner, op_name)
                } else {
                    match resolved_scan_ticket(&core, &self.inner.completed_scans, operation) {
                        CompletedScanTicket::Settled(winner) => terminal_to_error(winner, op_name),
                        CompletedScanTicket::Expired => expired_scan_ticket_error(),
                        CompletedScanTicket::Foreign => foreign_scan_ticket_error(),
                        CompletedScanTicket::Local | CompletedScanTicket::Unknown => {
                            lock_std(&self.inner.reset_ops)
                                .contains(operation)
                                .then_some(OperationTerminalKind::Reset)
                                .map_or_else(
                                    || DesktopError::cancelled(op_name),
                                    |winner| terminal_to_error(winner, op_name),
                                )
                        }
                    }
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
            native_gatt_admissions: self.inner.gatt_admission.active_slots(),
            acquired_gatt_transports: self.inner.acquired.counts().0,
            pending_gatt_acquisitions: self.inner.acquired.counts().1,
            routed_subscriptions,
            pending_disables,
            queued_advertisements,
            advertisement_drops: self.inner.advertisement_drops.load(Ordering::Relaxed),
            lifecycle_unobserved: self.inner.lifecycle_unobserved.load(Ordering::Relaxed),
            compensation_failures: self.inner.compensation_failures.load(Ordering::Relaxed),
            scan_owned: self.inner.scan_slot().is_some(),
            retained_scan_tickets: lock_std(&self.inner.completed_scans).tickets.len(),
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
    /// every routing invalidation bumps it. Synthetic staging defaults an
    /// omitted epoch to this value, so a staged live value delivers after
    /// reconnects while an explicitly stale epoch still drops (G1/W7).
    pub async fn routing_epoch(&self, peer_id: &str) -> u64 {
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
    /// start never wedges later scans. An expired or cancelled start attempts
    /// to stop the possibly-started OS scan before it returns. Refused cleanup
    /// stays centrally owned and a later start retries that unpublished debt;
    /// published scan ownership is never bypassed by this recovery.
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
        self.start_scan_platform(owner, service_uuids, duplicates, name_prefix, None, ctl)
            .await
    }

    pub async fn start_scan_platform(
        &self,
        owner: &str,
        service_uuids: &[&str],
        duplicates: ScanDuplicatePolicy,
        name_prefix: Option<&str>,
        windows: Option<crate::boundary::WindowsScanOptions>,
        ctl: OpControl,
    ) -> Result<ScanSession, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "scan.start")?;
        if windows.is_some() {
            self.inner
                .core
                .lock()
                .await
                .check_capability("scan:platform-options", "scan.platform-options")
                .map_err(DesktopError::from)?;
        }
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
            windows,
        };
        self.retry_unpublished_scan_cleanup(window, &ctl.ticket)
            .await?;
        // Cleanup may have spent the caller's budget or raced shutdown. A
        // successful old stop is not admission for a replacement operation.
        self.precheck(&ctl, "scan.start")?;
        let id = {
            let mut core = self.inner.core.lock().await;
            if let Some(occupant) = self.inner.scan_slot().as_ref() {
                return Err(DesktopError::new(
                    BleErrorCode::ScanAlreadyActive,
                    BleErrorDomain::Scan,
                    "scan.start",
                )
                .with_detail(format!("scan {} still owns radio cleanup", occupant.id)));
            }
            let mut out = batch();
            let id = match core.start_scan(&request, None, owner, now_ms(), &mut out) {
                Ok(id) => id,
                Err(error) => {
                    let mut failure = DesktopError::from(error);
                    if failure.code() == BleErrorCode::ScanAlreadyActive {
                        // Finding 209: name the occupying scan so the
                        // refusal is diagnosable. The slot holds the scan
                        // admitted above (core arbitration already refused
                        // a second live scan, so the slot is the occupant).
                        if let Some(occupant) = self.inner.scan_slot().as_ref() {
                            failure = failure.with_detail(format!(
                                "scan {} is still active; stop it before starting a new scan",
                                occupant.id
                            ));
                        }
                    }
                    return Err(failure);
                }
            };
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
                start_refused: false,
                unpublished_cleanup: false,
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
                    // Activate inside the slot scope so no guard lives
                    // across the re-read await below (the future is Send).
                    let activated = {
                        let mut slot = self.inner.scan_slot();
                        let owned = !self.inner.shut_down.load(Ordering::SeqCst)
                            && slot.as_ref().is_some_and(|active| {
                                active.id == id && matches!(active.phase, ScanPhase::Starting)
                            });
                        if owned && let Some(active) = slot.as_mut() {
                            active.phase = ScanPhase::Active;
                            true
                        } else {
                            false
                        }
                    };
                    if activated {
                        // `start_scan` returning `Ok` is the OS
                        // acknowledgement.
                        let _ = core.platform_scan_started(&id);
                        drop(core);
                        // Finding 205: the OS may report no new sighting
                        // for a known peer (CoreBluetooth withholds
                        // repeats), so a fresh scan re-reads every known
                        // peripheral once as this scan's device-state
                        // observation. A re-read the OS cannot answer is
                        // counted, never silent, and never fails the start.
                        reobserve_known_peers(&self.inner).await;
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

    /// The OS refused the start: no late start can follow this answer. Drop
    /// either the untouched starting marker or an early stop's completed
    /// or failed marker; an in-flight stop leader still owns its answer.
    /// The OS scan never started, so no further stop is owed.
    async fn fail_scan_start(&self, id: &OperationId) {
        {
            let mut slot = self.inner.scan_slot();
            if let Some(active) = slot.as_mut()
                && active.id == *id
            {
                if matches!(active.phase, ScanPhase::Stopping(_)) {
                    active.start_refused = true;
                } else {
                    *slot = None;
                }
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

    /// A start that lost admission can still have turned on the OS scan.
    /// Compensation uses the same generation-bound stop owner and retry
    /// state as an explicit stop; a refused stop never discards its handle.
    async fn compensate_lost_start(&self, id: &OperationId) {
        let deadline = tokio::time::Instant::now() + LIVENESS_CLEANUP;
        loop {
            let pending = {
                let mut slot = self.inner.scan_slot();
                if let Some(active) = slot.as_mut()
                    && active.id == *id
                {
                    active.unpublished_cleanup = true;
                }
                match slot
                    .as_ref()
                    .map(|active| (active.id == *id, &active.phase))
                {
                    None => {
                        if self.inner.shut_down.load(Ordering::SeqCst) {
                            return;
                        }
                        *slot = Some(ActiveScan {
                            id: id.clone(),
                            phase: ScanPhase::StopFailed,
                            start_refused: false,
                            unpublished_cleanup: true,
                        });
                        None
                    }
                    Some((false, _)) => return,
                    Some((true, ScanPhase::Stopping(rx))) => Some(rx.clone()),
                    Some((true, _)) => None,
                }
            };
            if let Some(mut pending) = pending {
                if !matches!(
                    tokio::time::timeout_at(deadline, pending.wait_for(Option::is_some)).await,
                    Ok(Ok(_))
                ) {
                    // The leader retains the identity and its own deadline.
                    return;
                }
                continue;
            }
            let control = OpControl::unbounded();
            if self
                .stop_scan_with(
                    id,
                    control.budget.window(COMPENSATION_TIMEOUT),
                    &control.ticket,
                )
                .await
                .is_err()
            {
                self.inner.note_compensation_failure();
            } else {
                // The original start has now answered or its future was
                // cancelled by the caller's budget. The compensating OS
                // stop succeeded after that point, so the protective
                // early-stop marker has no remaining start to guard.
                let mut slot = self.inner.scan_slot();
                if slot.as_ref().is_some_and(|active| {
                    active.id == *id && matches!(active.phase, ScanPhase::StartCancelled)
                }) {
                    *slot = None;
                }
            }
            return;
        }
    }

    /// Retry only a session nobody acquired. The ordinary generation-bound
    /// stop owner supplies single-flight and retention; the new caller's
    /// original budget/cancellation bounds this drain before radio admission.
    async fn retry_unpublished_scan_cleanup(
        &self,
        window: Window,
        ticket: &OpTicket,
    ) -> Result<(), DesktopError> {
        let retained = self
            .inner
            .scan_slot()
            .as_ref()
            .filter(|active| active.unpublished_cleanup)
            .map(|active| active.id.clone());
        if let Some(id) = retained {
            self.stop_scan_with(&id, window, ticket).await?;
            let mut slot = self.inner.scan_slot();
            if slot.as_ref().is_some_and(|active| {
                active.id == id
                    && active.unpublished_cleanup
                    && matches!(active.phase, ScanPhase::StartCancelled)
            }) {
                *slot = None;
            }
        }
        Ok(())
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
                    ScanPhase::Starting
                    | ScanPhase::StartCancelled
                    | ScanPhase::Active
                    | ScanPhase::StopFailed => {
                        let was_starting = matches!(active.phase, ScanPhase::Starting);
                        let (tx, rx) = watch::channel(None);
                        active.phase = ScanPhase::Stopping(rx);
                        StopRole::Lead(tx, was_starting)
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
            StopRole::Lead(tx, was_starting) => {
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
                            if let Some(active) = slot.as_mut()
                                && active.id == *scan
                            {
                                if was_starting && !active.start_refused {
                                    active.phase = ScanPhase::StartCancelled;
                                } else {
                                    *slot = None;
                                }
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
    /// release remains generation-scoped physical debt independently of the
    /// terminal logical connection. Shutdown reports current debt; historical
    /// compensation counters remain diagnostic after a successful retry.
    fn retain_half_open_cleanup(
        &self,
        core: &Central,
        peer_id: &str,
        peer_key: &str,
        generation: &Option<String>,
        adapter_epoch: u64,
        release_serial: u64,
    ) -> bool {
        let current = core.connection_generation(peer_key);
        if (current.is_some() && current != *generation)
            || self.inner.resets.load(Ordering::SeqCst) != adapter_epoch
            || confirmed_release_count(&self.inner, peer_id) != release_serial
        {
            return false;
        }
        let mut retained = lock_std(&self.inner.half_open_cleanup);
        let debt = retained.entry(peer_id.to_owned()).or_insert_with(|| {
            Arc::new(HalfOpenCleanup {
                peer_key: peer_key.to_owned(),
                generation: generation.clone(),
                release_serial: AtomicU64::new(release_serial),
                adapter_epoch,
                released: Mutex::new(false),
                failure: StdMutex::new(None),
            })
        });
        // A pre-admitted native acquisition may finish after an earlier
        // release in this same generation. Refresh the obligation, not its
        // single-flight gate or Arc identity: old cleanup remains owned.
        debt.release_serial.store(release_serial, Ordering::SeqCst);
        true
    }

    async fn retry_half_open_cleanup(
        &self,
        peer_id: &str,
        debt: &Arc<HalfOpenCleanup>,
    ) -> Result<(), DesktopError> {
        let work = async {
            let mut released = debt.released.lock().await;
            if *released {
                return Ok(());
            }
            let retired = self.inner.resets.load(Ordering::SeqCst) != debt.adapter_epoch
                || confirmed_release_count(&self.inner, peer_id)
                    != debt.release_serial.load(Ordering::SeqCst);
            if !retired {
                let core = self.inner.core.lock().await;
                let current = core.connection_generation(&debt.peer_key);
                if current.is_some() && current != debt.generation {
                    return Err(contract_error(
                        BleErrorCode::ConnectionStale,
                        BleErrorDomain::Cleanup,
                        "connection.compensate",
                    )
                    .with_detail("retained cleanup belongs to an older connection generation"));
                }
            }
            if !retired {
                self.inner.boundary.disconnect(peer_id).await?;
                note_confirmed_release(&self.inner, peer_id);
            }
            let mut retained = lock_std(&self.inner.half_open_cleanup);
            // Retention can refresh the serial while this native release
            // is in flight. Never erase a later acquisition's obligation.
            if self.inner.resets.load(Ordering::SeqCst) == debt.adapter_epoch
                && confirmed_release_count(&self.inner, peer_id)
                    == debt.release_serial.load(Ordering::SeqCst)
            {
                return Ok(());
            }
            *released = true;
            if retained
                .get(peer_id)
                .is_some_and(|entry| Arc::ptr_eq(entry, debt))
            {
                retained.remove(peer_id);
            }
            Ok(())
        };
        let failure = match tokio::time::timeout(COMPENSATION_TIMEOUT, work).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => error,
            Err(_) => contract_error(
                BleErrorCode::OperationTimedOut,
                BleErrorDomain::Cleanup,
                "connection.compensate",
            )
            .with_detail("half-open link cleanup remains owned after its bound"),
        };
        *lock_std(&debt.failure) = Some(failure.clone());
        self.inner.note_compensation_failure();
        Err(failure)
    }

    async fn compensate_half_open(&self, peer_id: &str, generation: &Option<String>) {
        let debt = lock_std(&self.inner.half_open_cleanup)
            .get(peer_id)
            .filter(|debt| debt.generation == *generation)
            .cloned();
        if let Some(debt) = debt {
            let _ = self.retry_half_open_cleanup(peer_id, &debt).await;
        }
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
        self.connect_intent(peer_id, lease, ctl, false).await
    }

    /// Native deferred acquisition, over the same lease/compensation authority.
    pub async fn connect_when_available(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
    ) -> Result<ConnectionHandle, DesktopError> {
        self.connect_intent(peer_id, lease, ctl, true).await
    }

    async fn connect_intent(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
        deferred: bool,
    ) -> Result<ConnectionHandle, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "connection.connect")?;
        self.inner
            .boundary
            .validate_peer_identity(peer_id, "connection.connect")?;
        // Finding 112: without a caller budget a connect waits as long as
        // the OS does (legacy pending CoreBluetooth connect, Android
        // `autoConnect`); no liveness backstop ends it, a cancel does.
        let window = ctl.budget.window_without_backstop();
        let previous = lock_std(&self.inner.half_open_cleanup)
            .get(peer_id)
            .cloned();
        if let Some(previous) = previous {
            match drive(
                &ctl.ticket,
                window,
                self.retry_half_open_cleanup(peer_id, &previous),
            )
            .await
            {
                Wait::Done(result) => result.map_err(|error| {
                    classify(error, OpKind::Connect, false)
                        .classify_connect_failure()
                        .classify_establishment()
                })?,
                Wait::Expired => {
                    return Err(classify(
                        timed_out("connection.connect", window),
                        OpKind::Connect,
                        false,
                    ));
                }
                Wait::Cancelled => {
                    return Err(classify(
                        ctl.ticket.interruption("connection.connect"),
                        OpKind::Connect,
                        false,
                    ));
                }
            }
            self.refuse_before_admission(&ctl, "connection.connect")?;
        }
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
        let (operation, connection_generation, adapter_epoch, mut release_serial) = {
            let mut core = self.inner.core.lock().await;
            if lock_std(&self.inner.half_open_cleanup).contains_key(peer_id) {
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "connection.connect",
                )
                .with_detail("previous half-open connection cleanup remains owned"));
            }
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
            lock_std(&self.inner.retired_leases).remove(&(peer_key.clone(), lease.to_owned()));
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            (
                id,
                core.connection_generation(&peer_key),
                self.inner.resets.load(Ordering::SeqCst),
                confirmed_release_count(&self.inner, peer_id),
            )
        };
        // Finding 161: the bound the attempt runs under, for the deadline
        // fact when it expires before any link came up. A connect without
        // a caller budget waits as long as the OS does, so an expiry always
        // had a caller deadline.
        let budget_ms = ctl
            .budget
            .remaining()
            .map(|left| u64::try_from(left.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let mut drop_guard = CancelOnDrop::armed(
            self,
            operation.clone(),
            DropCleanup::Connect {
                peer_id: peer_id.to_owned(),
                peer_key: peer_key.clone(),
                generation: connection_generation.clone(),
                adapter_epoch,
                release_serial,
            },
        );
        let acquisition = async {
            if deferred {
                self.inner.boundary.connect_when_available(peer_id).await
            } else {
                self.inner.boundary.connect(peer_id).await
            }
        };
        let result = match drive(&ctl.ticket, window, acquisition).await {
            Wait::Done(Ok(())) => {
                // This fact must survive a caller drop while waiting for
                // core settlement, including success after shutdown release.
                release_serial = confirmed_release_count(&self.inner, peer_id);
                drop_guard.note_native_acquisition(release_serial);
                let settled = {
                    let mut core = self.inner.core.lock().await;
                    let mut out = batch();
                    if core.connection_generation(&peer_key) != connection_generation {
                        return Err(contract_error(
                            BleErrorCode::ConnectionStale,
                            BleErrorDomain::Connection,
                            "connection.connect",
                        ));
                    }
                    // A prior OS event may already have established this
                    // generation. A terminal generation is different: a
                    // successful late native call cannot revive its handles.
                    let establishment_error = match core.connection_state(&peer_key) {
                        Some(ConnectionState::Connected) => None,
                        Some(ConnectionState::Connecting) => core
                            .note_link_established(&peer_key)
                            .err()
                            .map(DesktopError::from),
                        _ => Some(contract_error(
                            BleErrorCode::ConnectionStale,
                            BleErrorDomain::Connection,
                            "connection.connect",
                        )),
                    };
                    let outcome = settle_and_release(
                        &mut core,
                        &operation,
                        if establishment_error.is_some() {
                            ContenderKind::Failure
                        } else {
                            ContenderKind::Success
                        },
                        true,
                        establishment_error.as_ref().map(DesktopError::code),
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
                            let same = self.retain_half_open_cleanup(
                                &core,
                                peer_id,
                                &peer_key,
                                &connection_generation,
                                adapter_epoch,
                                release_serial,
                            );
                            if same {
                                let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                            }
                            Err(if winner == OperationTerminalKind::Failed {
                                establishment_error.unwrap_or_else(|| {
                                    terminal_to_error(winner, "connection.connect")
                                })
                            } else {
                                terminal_to_error(winner, "connection.connect")
                            })
                        }
                        None => Err(contract_error(
                            BleErrorCode::LifecycleInvalidState,
                            BleErrorDomain::Core,
                            "connection.connect",
                        )),
                    }
                };
                if settled.is_err() {
                    self.compensate_half_open(peer_id, &connection_generation)
                        .await;
                }
                settled
            }
            Wait::Done(Err(error)) => {
                // Settle under the lock, then drop the guard before awaiting
                // the compensating radio disconnect (F24): a stuck cleanup
                // must never block unrelated peers behind the core lock.
                {
                    let mut core = self.inner.core.lock().await;
                    let same = self.retain_half_open_cleanup(
                        &core,
                        peer_id,
                        &peer_key,
                        &connection_generation,
                        adapter_epoch,
                        release_serial,
                    );
                    let mut out = batch();
                    if same {
                        let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                    }
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
                self.compensate_half_open(peer_id, &connection_generation)
                    .await;
                Err(error)
            }
            Wait::Expired => {
                // Finding 161: the deadline expired before any link came
                // up — the same physical event as the controller giving up,
                // one name (`connection.failed`) on every host. Mark loss,
                // settle the core op, clean the half-open link outside the
                // lock, and report the deadline fact. A caller-supplied
                // abort still reports `operation.aborted` (only a timed-out
                // connect is renamed).
                {
                    let mut core = self.inner.core.lock().await;
                    let same = self.retain_half_open_cleanup(
                        &core,
                        peer_id,
                        &peer_key,
                        &connection_generation,
                        adapter_epoch,
                        release_serial,
                    );
                    let mut out = batch();
                    if same {
                        let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                    }
                    let _ = out.drain();
                }
                let error = self
                    .settle_timeout(&operation, "connection.connect", window)
                    .await;
                self.compensate_half_open(peer_id, &connection_generation)
                    .await;
                Err(error.classify_connect_deadline(budget_ms))
            }
            Wait::Cancelled => {
                {
                    let mut core = self.inner.core.lock().await;
                    let same = self.retain_half_open_cleanup(
                        &core,
                        peer_id,
                        &peer_key,
                        &connection_generation,
                        adapter_epoch,
                        release_serial,
                    );
                    let mut out = batch();
                    if same {
                        let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                    }
                    let _ = out.drain();
                }
                let error = self.settle_abort(&operation, "connection.connect").await;
                self.compensate_half_open(peer_id, &connection_generation)
                    .await;
                Err(error)
            }
        };
        drop_guard.defuse();
        // A link the platform could not establish is the caller's to retry
        // (owner decision, 5.0); the central never retries it.
        result.map_err(|error| {
            classify(error, OpKind::Connect, true)
                .classify_connect_failure()
                .classify_establishment()
        })
    }

    /// Release only this lease. `false` means another lease retains the link;
    /// `true` means the final release was confirmed. Failed final cleanup keeps
    /// the lease so the same owner can retry. Admission and the last-lease test
    /// share the core lock, so a joining client cannot race the decision.
    pub async fn release_connection_lease(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
    ) -> Result<bool, DesktopError> {
        self.release_connection_lease_report(peer_id, lease, ctl)
            .await
            .map(|report| report.physical)
    }

    /// Release a lease and return its own observation without waiting for event consumers.
    pub async fn release_connection_lease_report(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
    ) -> Result<ConnectionReleaseReport, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        if self.inner.shutdown_release_confirmed.load(Ordering::SeqCst) {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let retired_generation = lock_std(&self.inner.retired_leases)
                .get(&(peer_key.clone(), lease.to_owned()))
                .cloned();
            let retired = retired_generation.is_some();
            if (core.connection_state(&peer_key) == Some(ConnectionState::Disconnected) || retired)
                && !lock_std(&self.inner.half_open_cleanup).contains_key(peer_id)
            {
                let result = if core.connection_state(&peer_key).is_none() && retired {
                    Ok(true)
                } else {
                    core.release_lease(&peer_key, lease, now_ms(), &mut batch())
                        .map_err(DesktopError::from)
                };
                if result.is_ok() {
                    lock_std(&self.inner.retired_leases)
                        .remove(&(peer_key.clone(), lease.to_owned()));
                }
                return result.map(|physical| ConnectionReleaseReport {
                    physical,
                    connection_generation: retired_generation
                        .flatten()
                        .or_else(|| Generations::of(&core, &peer_key).connection),
                    platform: None,
                });
            }
        }
        self.precheck(&ctl, "connection.release")?;
        let window = ctl.budget.window(LIVENESS_CLEANUP);
        let release_budget = window
            .at
            .map_or_else(Budget::unbounded, |at| Budget::from_ms_at(at, 0));
        self.inner.acquired.retire(
            Some(peer_id),
            Some(lease),
            contract_error(
                BleErrorCode::OperationDisconnected,
                BleErrorDomain::Connection,
                "gatt.acquired",
            ),
        );
        match drive(
            &ctl.ticket,
            window,
            self.inner
                .acquired
                .release_scope(Some(peer_id), Some(lease)),
        )
        .await
        {
            Wait::Done(failures) => {
                crate::errors::cleanup_result("connection.acquired-gatt", failures)?
            }
            Wait::Expired => {
                return Err(timed_out("connection.release", window)
                    .with_detail("acquired child cleanup remains owned before link release"));
            }
            Wait::Cancelled => return Err(ctl.ticket.interruption("connection.release")),
        }
        let gate = {
            let mut gates = lock_std(&self.inner.lease_releases);
            gates.retain(|_, gate| gate.strong_count() != 0);
            let slot = gates
                .entry((peer_id.to_owned(), lease.to_owned()))
                .or_default();
            if let Some(gate) = slot.upgrade() {
                gate
            } else {
                let gate = Arc::new(Mutex::new(None));
                *slot = Arc::downgrade(&gate);
                gate
            }
        };
        let mut released = match drive(&ctl.ticket, window, gate.lock()).await {
            Wait::Done(guard) => guard,
            Wait::Expired => {
                return Err(classify(
                    timed_out("connection.release", window),
                    OpKind::Cleanup,
                    false,
                ));
            }
            Wait::Cancelled => {
                return Err(classify(
                    ctl.ticket.interruption("connection.release"),
                    OpKind::Cleanup,
                    false,
                ));
            }
        };
        self.refuse_before_admission(&ctl, "connection.release")?;
        if let Some(report) = released.as_ref() {
            return Ok(report.clone());
        }
        let peer_key = self.known_peer_key(peer_id).await?;
        let children = {
            let mut core = self.inner.core.lock().await;
            if !core.holds_lease(&peer_key, lease)
                && let Some(original_generation) = lock_std(&self.inner.retired_leases)
                    .remove(&(peer_key.clone(), lease.to_owned()))
            {
                let report = ConnectionReleaseReport {
                    physical: true,
                    connection_generation: original_generation,
                    platform: None,
                };
                *released = Some(report.clone());
                return Ok(report);
            }
            let another = core
                .held_leases()
                .iter()
                .any(|(peer, held)| peer == &peer_key && held != lease);
            if another {
                core.begin_lease_release(&peer_key, lease)
                    .map_err(DesktopError::from)?;
                let children = core
                    .consumers_for_lease(&peer_key, lease)
                    .into_iter()
                    .map(|(index, consumer)| {
                        let path = core.stored_path(index).ok_or_else(|| {
                            contract_error(
                                BleErrorCode::LifecycleInvariantViolation,
                                BleErrorDomain::Core,
                                "connection.release.path",
                            )
                        })?;
                        Ok((
                            PathSelector {
                                service_uuid: path.service_uuid().to_owned(),
                                service_occurrence: Some(path.service_occurrence()),
                                characteristic_uuid: path.characteristic_uuid().map(str::to_owned),
                                characteristic_occurrence: path.characteristic_occurrence(),
                                descriptor_uuid: path.descriptor_uuid().map(str::to_owned),
                                descriptor_occurrence: path.descriptor_occurrence(),
                            },
                            consumer,
                        ))
                    })
                    .collect::<Result<Vec<_>, DesktopError>>()?;
                let mut pending = lock_std(&self.inner.pending_lease_children);
                let retained = pending
                    .entry((peer_id.to_owned(), lease.to_owned()))
                    .or_default();
                for child in children {
                    if !retained.contains(&child) {
                        retained.push(child);
                    }
                }
                retained.clone()
            } else {
                if matches!(
                    core.connection_state(&peer_key),
                    Some(ConnectionState::Connected | ConnectionState::Connecting)
                ) {
                    core.disconnect(&peer_key, lease, now_ms(), &mut batch())
                        .map_err(DesktopError::from)?;
                }
                Vec::new()
            }
        };
        for (selector, consumer) in children {
            // A child gets its own settlement ticket, while the parent's
            // original window/cancellation bounds the whole drain.
            let child = OpControl::new(release_budget, OpTicket::new())
                .with_connection_lease(lease.to_owned());
            let ticket = child.ticket.clone();
            let pending = self.unsubscribe(peer_id, &selector, &consumer, child);
            tokio::pin!(pending);
            tokio::select! {
                result = &mut pending => { result?; }
                () = ctl.ticket.cancelled() => {
                    self.cancel(&ticket).await?;
                    // Let the child's own cancellation path retain failed
                    // native disable ownership; never drop it at the deadline.
                    pending.await?;
                }
            }
            if let Some(retained) = lock_std(&self.inner.pending_lease_children)
                .get_mut(&(peer_id.to_owned(), lease.to_owned()))
            {
                retained.retain(|child| child != &(selector.clone(), consumer.clone()));
            }
        }
        {
            let mut core = self.inner.core.lock().await;
            let another = core
                .held_leases()
                .iter()
                .any(|(peer, held)| peer == &peer_key && held != lease);
            if another {
                core.release_lease(&peer_key, lease, now_ms(), &mut batch())
                    .map_err(DesktopError::from)?;
                lock_std(&self.inner.pending_lease_children)
                    .remove(&(peer_id.to_owned(), lease.to_owned()));
                let report = ConnectionReleaseReport {
                    physical: false,
                    connection_generation: Generations::of(&core, &peer_key).connection,
                    platform: None,
                };
                *released = Some(report.clone());
                return Ok(report);
            }
            if matches!(
                core.connection_state(&peer_key),
                Some(ConnectionState::Connected | ConnectionState::Connecting)
            ) {
                core.disconnect(&peer_key, lease, now_ms(), &mut batch())
                    .map_err(DesktopError::from)?;
            }
        }
        let result = self
            .disconnect_report(
                peer_id,
                lease,
                OpControl::new(release_budget, ctl.ticket.clone()),
            )
            .await
            .map(|report| ConnectionReleaseReport {
                physical: true,
                connection_generation: report.connection_generation,
                platform: report.platform,
            })
            .map_err(|error| {
                if window.backstop && error.code() == BleErrorCode::OperationTimedOut {
                    error.with_detail(LIVENESS_BACKSTOP_DETAIL)
                } else {
                    error
                }
            });
        if let Ok(report) = &result {
            *released = Some(report.clone());
            lock_std(&self.inner.retired_leases).remove(&(peer_key.clone(), lease.to_owned()));
            lock_std(&self.inner.pending_lease_children).retain(|(peer, _), _| peer != peer_id);
        }
        result
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
        self.disconnect_report(peer_id, lease, ctl)
            .await
            .map(|report| report.release)
    }
    async fn disconnect_report(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
    ) -> Result<DisconnectReport, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "connection.disconnect")?;
        let window = ctl.budget.window(LIVENESS_CLEANUP);
        let peer_key = self.known_peer_key(peer_id).await?;
        let release_generation = {
            let mut core = self.inner.core.lock().await;
            if core.connection_state(&peer_key).is_none()
                && let Some(original_generation) = lock_std(&self.inner.retired_leases)
                    .remove(&(peer_key.clone(), lease.to_owned()))
            {
                return Ok(DisconnectReport {
                    release: LinkRelease::AlreadyReleased,
                    connection_generation: original_generation,
                    platform: None,
                });
            }
            let held = core.holds_lease(&peer_key, lease);
            match core.connection_state(&peer_key) {
                Some(
                    ConnectionState::Disconnected
                    | ConnectionState::Lost
                    | ConnectionState::Invalid,
                ) if held => {
                    return Ok(DisconnectReport {
                        release: LinkRelease::AlreadyReleased,
                        connection_generation: Generations::of(&core, &peer_key).connection,
                        platform: None,
                    });
                }
                // A retained release: the same lease drives the radio again.
                Some(ConnectionState::Disconnecting) if held => {}
                _ => {
                    let mut out = batch();
                    core.disconnect(&peer_key, lease, now_ms(), &mut out)
                        .map_err(DesktopError::from)?;
                }
            }
            Generations::of(&core, &peer_key)
        };
        // The release is underway: operations still waiting on this link end
        // now, `operation.disconnected`, as Android's stack ends them at an
        // app disconnect (owner decision, 5.0).
        note_link_end(&self.inner, peer_id);
        self.inner.acquired.retire(
            Some(peer_id),
            None,
            contract_error(
                BleErrorCode::OperationDisconnected,
                BleErrorDomain::Connection,
                "gatt.acquired",
            ),
        );
        match drive(
            &ctl.ticket,
            window,
            self.inner.acquired.release_scope(Some(peer_id), None),
        )
        .await
        {
            Wait::Done(failures) => {
                crate::errors::cleanup_result("connection.acquired-gatt", failures)?
            }
            Wait::Expired => {
                return Err(timed_out("connection.disconnect", window).with_detail(
                    "acquired child cleanup remains owned before physical disconnect",
                ));
            }
            Wait::Cancelled => return Err(ctl.ticket.interruption("connection.disconnect")),
        }
        let outcome = drive(
            &ctl.ticket,
            window,
            self.inner.boundary.disconnect_with_observation(peer_id),
        )
        .await;
        let platform = match &outcome {
            Wait::Done(Ok(observation)) => observation.platform.clone(),
            _ => None,
        };
        // Late radio completions must not resurrect the link: drop local
        // subscription routing for this peer now; the core already
        // invalidated its hubs at disconnect.
        clear_peer_routing_scoped(
            &self.inner,
            peer_id,
            Some((&peer_key, release_generation.connection.as_deref())),
        )
        .await;
        let (result, event) = {
            let mut core = self.inner.core.lock().await;
            match outcome {
                Wait::Done(Ok(observation)) => {
                    let generation = Generations::of(&core, &peer_key);
                    if Generations::of(&core, &peer_key).connection == release_generation.connection
                        && core.connection_state(&peer_key) == Some(ConnectionState::Disconnecting)
                    {
                        note_confirmed_release(&self.inner, peer_id);
                        core.note_link_released(&peer_key)
                            .map_err(DesktopError::from)?;
                        self.inner
                            .boundary
                            .consume_disconnect_observation(peer_id, &observation);
                        let event = self.inner.stage_lifecycle_with_platform(
                            peer_id,
                            &peer_key,
                            generation,
                            LifecycleKind::Released { requested: true },
                            observation.platform,
                        );
                        (
                            observation
                                .cleanup_failure
                                .map_or(Ok(LinkRelease::Released), Err),
                            Some(event),
                        )
                    } else {
                        // The event loop recorded the release (or a loss)
                        // first and already published it.
                        (
                            observation
                                .cleanup_failure
                                .map_or(Ok(LinkRelease::Released), Err),
                            None,
                        )
                    }
                }
                Wait::Done(Err(error)) => {
                    if Generations::of(&core, &peer_key).connection == release_generation.connection
                    {
                        let _ = core.report_disconnect_failure(&peer_key, error.code());
                    }
                    (Err(error), None)
                }
                Wait::Expired => {
                    if Generations::of(&core, &peer_key).connection == release_generation.connection
                    {
                        let _ = core
                            .report_disconnect_failure(&peer_key, BleErrorCode::OperationTimedOut);
                    }
                    let error = if window.backstop {
                        timed_out("connection.disconnect", window)
                    } else {
                        timed_out("connection.disconnect", window)
                            .with_detail("disconnect completion deadline exceeded")
                    };
                    (Err(classify(error, OpKind::Cleanup, true)), None)
                }
                Wait::Cancelled => {
                    if Generations::of(&core, &peer_key).connection == release_generation.connection
                    {
                        let _ = core
                            .report_disconnect_failure(&peer_key, BleErrorCode::OperationAborted);
                    }
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
        result.map(|release| DisconnectReport {
            release,
            connection_generation: release_generation.connection,
            platform,
        })
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
            let consumers: Vec<_> = core
                .held_consumers()
                .into_iter()
                .filter(|(owner, _)| owner == &peer_key)
                .collect();
            core.note_peer_loss(&peer_key, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            if before.is_some_and(|state| !state.is_terminal()) {
                lock_std(&self.inner.retired_consumers).extend(consumers);
                lock_std(&self.inner.retired_leases).extend(
                    core.held_leases()
                        .into_iter()
                        .filter(|(peer, _)| peer == &peer_key)
                        .map(|key| {
                            let generation = Generations::of(&core, &key.0).connection;
                            (key, generation)
                        }),
                );
            }
            let kind = if before == Some(ConnectionState::Disconnecting) {
                LifecycleKind::Released { requested: true }
            } else {
                LifecycleKind::LinkLost
            };
            self.inner
                .stage_lifecycle(peer_id, &peer_key, generation, kind)
        };
        note_link_end(&self.inner, peer_id);
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
    /// Concurrent callers share the physical snapshot. A newly joining lease
    /// attaches to an already-current snapshot without rotating its generation;
    /// a later explicit discovery by an attached lease refreshes it. Cached
    /// topology is accepted only while the authoritative connection/database
    /// generations and current state still match. Each wait retains its own
    /// cancellation ticket and original deadline.
    pub async fn discover(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
    ) -> Result<DiscoveryReport, DesktopError> {
        let ctl = self.bind_gatt_admission(peer_id, ctl)?;
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "discovery.complete")?;
        let window = ctl.budget.window(LIVENESS_OP);
        let peer_key = self.known_peer_key(peer_id).await?;
        let rediscover = {
            let core = self.inner.core.lock().await;
            if !core.lease_accepts_work(&peer_key, lease) {
                return Err(contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Core,
                    "discovery.lease",
                ));
            }
            let generations = Generations::of(&core, &peer_key);
            let mut admitted = lock_std(&self.inner.discovered_leases);
            admitted.retain(|(_, lease), (key, connection, _)| {
                core.holds_lease(key, lease) && core.connection_generation(key) == *connection
            });
            admitted
                .get(&(peer_id.to_owned(), lease.to_owned()))
                .is_some_and(|(_, connection, database)| {
                    *connection == generations.connection
                        && *database == generations.database
                        && core.database_state(&peer_key) == Some(DatabaseState::Current)
                })
        };
        if rediscover {
            if let Some(admission) = ctl.gatt_admission() {
                self.inner
                    .gatt_admission
                    .invalidate_except(&admission, BleErrorCode::GattStaleHandle);
            }
            self.inner.acquired.retire(
                Some(peer_id),
                None,
                contract_error(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    "gatt.acquired",
                ),
            );
            let _ = self.inner.native_wake.send(());
        }
        let coordinator = discovery_coordinator(&self.inner, peer_id);
        let entered = coordinator.completed.load(Ordering::Acquire);
        let _gatt_admission = self
            .wait_gatt_admission(peer_id, &ctl, "discovery.complete", window)
            .await
            .map_err(|error| classify(error, OpKind::Discover, false))?;
        let mut snapshot = match drive_link(
            &self.inner,
            peer_id,
            "discovery.complete",
            &ctl.ticket,
            window,
            _gatt_admission.dispatch(async { Ok(coordinator.snapshot.lock().await) }),
        )
        .await
        {
            Wait::Done(Ok(guard)) => guard,
            Wait::Done(Err(error)) => return Err(error),
            Wait::Expired => {
                return Err(classify(
                    timed_out("discovery.complete", window),
                    OpKind::Discover,
                    false,
                ));
            }
            Wait::Cancelled => {
                return Err(classify(
                    ctl.ticket.interruption("discovery.complete"),
                    OpKind::Discover,
                    false,
                ));
            }
        };
        self.refuse_before_admission(&ctl, "discovery.complete")?;
        {
            let core = self.inner.core.lock().await;
            if !core.lease_accepts_work(&peer_key, lease) {
                return Err(contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Core,
                    "discovery.lease",
                ));
            }
            let generations = core
                .connection_generation(&peer_key)
                .zip(core.database_generation(&peer_key));
            snapshot
                .leases
                .retain(|held| core.holds_lease(&peer_key, held));
            if core.database_state(&peer_key) == Some(DatabaseState::Current)
                && generations == snapshot.generations
                && (entered != coordinator.completed.load(Ordering::Acquire)
                    || !snapshot.leases.contains(lease))
                && let Some(report) = snapshot.report.clone()
            {
                snapshot.leases.insert(lease.to_owned());
                let generation = Generations::of(&core, &peer_key);
                lock_std(&self.inner.discovered_leases).insert(
                    (peer_id.to_owned(), lease.to_owned()),
                    (peer_key.clone(), generation.connection, generation.database),
                );
                return Ok(report);
            }
        }
        let result = self
            .discover_physical(peer_id, lease, peer_key.clone(), &ctl, window)
            .await;
        snapshot.report = result.as_ref().ok().map(|(report, _)| report.clone());
        snapshot.leases.clear();
        snapshot.generations = None;
        snapshot.identity = None;
        if result.is_ok() {
            let core = self.inner.core.lock().await;
            snapshot.generations = core
                .connection_generation(&peer_key)
                .zip(core.database_generation(&peer_key));
            let generation = Generations::of(&core, &peer_key);
            lock_std(&self.inner.discovered_leases).insert(
                (peer_id.to_owned(), lease.to_owned()),
                (peer_key.clone(), generation.connection, generation.database),
            );
            snapshot.leases.insert(lease.to_owned());
            snapshot.identity = result
                .as_ref()
                .ok()
                .and_then(|(_, identity)| identity.clone());
        }
        coordinator.completed.fetch_add(1, Ordering::Release);
        result.map(|(report, _)| report)
    }

    async fn discover_physical(
        &self,
        peer_id: &str,
        lease: &str,
        peer_key: String,
        ctl: &OpControl,
        window: Window,
    ) -> Result<(DiscoveryReport, Option<GattSnapshotIdentity>), DesktopError> {
        {
            let mut core = self.inner.core.lock().await;
            core.begin_discovery(&peer_key)
                .map_err(DesktopError::from)?;
        }
        let (services, identity) = match drive_link(
            &self.inner,
            peer_id,
            "discovery.complete",
            &ctl.ticket,
            window,
            async {
                // The original deadline includes local retirement. Even a
                // retry after failed traversal must retire surviving routes.
                retire_database_routing(&self.inner, peer_id).await;
                self.inner.boundary.discover_scoped(peer_id).await
            },
        )
        .await
        {
            Wait::Done(Ok(services)) => services,
            Wait::Done(Err(error)) => {
                {
                    let mut core = self.inner.core.lock().await;
                    let _ = core.fail_discovery(&peer_key);
                }
                let failed = Err(classify(error, OpKind::Discover, true));
                return self.name_link_end(&peer_key, failed).await;
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
            if let Err(error) = self.gatt_watch_admission("discovery.complete") {
                let _ = core.fail_discovery(&peer_key);
                return Err(error);
            }
            let current_identity = match self.inner.boundary.gatt_snapshot_identity(peer_id) {
                Ok(identity) => identity,
                Err(error) => {
                    let _ = core.fail_discovery(&peer_key);
                    return Err(classify(error, OpKind::Discover, true));
                }
            };
            if current_identity != identity {
                let _ = core.fail_discovery(&peer_key);
                return Err(contract_error(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    "discovery.complete",
                )
                .with_detail("the accepted radio snapshot changed before publication"));
            }
            // Finding 95: the whole snapshot is checked before it becomes
            // current — its size against the per-database bound and every
            // UUID — so registration below cannot fail part-way and no
            // partial database is ever current. A refused snapshot fails
            // the discovery as a whole.
            if core.database_state(&peer_key) != Some(DatabaseState::Discovering) {
                return Err(contract_error(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    "discovery.complete",
                )
                .with_detail("the database changed during discovery"));
            }
            if !core.lease_accepts_work(&peer_key, lease) {
                core.fail_discovery(&peer_key).map_err(DesktopError::from)?;
                return Err(contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "discovery.complete",
                )
                .with_detail("the discovery owner was released before publication"));
            }
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
            let mut canonical_counts: HashMap<String, u64> = HashMap::new();
            let mut published_service_identities = HashMap::new();
            for service in &services {
                let uuid =
                    ubm_core::central::canonical_uuid(&service.uuid).map_err(DesktopError::from)?;
                let next = canonical_counts.entry(uuid.clone()).or_insert(0);
                published_service_identities.insert((uuid, service.occurrence), *next);
                *next += 1;
            }
            let mut service_notes: Vec<(String, u64, ServiceGraphFacts)> = Vec::new();
            for service in &services {
                let service_uuid =
                    ubm_core::central::canonical_uuid(&service.uuid).map_err(DesktopError::from)?;
                let service_occurrence =
                    published_service_identities[&(service_uuid.clone(), service.occurrence)];
                let included_services = service
                    .included_services
                    .as_ref()
                    .map(|references| {
                        references
                            .iter()
                            .map(|reference| {
                                let uuid = ubm_core::central::canonical_uuid(&reference.uuid)
                                    .map_err(DesktopError::from)?;
                                let occurrence = published_service_identities
                                    .get(&(uuid.clone(), reference.occurrence))
                                    .copied()
                                    .ok_or_else(|| {
                                        contract_error(
                                            BleErrorCode::ProtocolViolation,
                                            BleErrorDomain::Gatt,
                                            "discovery.snapshot.included-service",
                                        )
                                    })?;
                                Ok(crate::boundary::IncludedServiceReference { uuid, occurrence })
                            })
                            .collect::<Result<Vec<_>, DesktopError>>()
                    })
                    .transpose()?;
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
                service_notes.push((
                    service_uuid,
                    service_occurrence,
                    ServiceGraphFacts {
                        access: service.access,
                        primary: service.primary,
                        included_services,
                    },
                ));
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
            lock_std(&self.inner.gatt_observation_failures).remove(peer_id);
            let mut notes = lock_std(&self.inner.service_access);
            notes.retain(|(peer, _, _), _| peer != peer_id);
            for (uuid, occurrence, access) in service_notes {
                notes.insert((peer_id.to_owned(), uuid, occurrence), access);
            }
        }
        Ok((report, identity))
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
        self.gatt_peer_observation_admission(peer_id, "discovery.snapshot")?;
        let stored = core.snapshot_paths(&peer_key).map_err(DesktopError::from)?;
        let access = lock_std(&self.inner.access);
        let service_access = lock_std(&self.inner.service_access);
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
                service_access: path
                    .characteristic_uuid()
                    .is_none()
                    .then(|| {
                        service_access
                            .get(&(
                                peer_id.to_owned(),
                                path.service_uuid().to_owned(),
                                path.service_occurrence(),
                            ))
                            .map(|facts| facts.access)
                    })
                    .flatten(),
                service_primary: path
                    .characteristic_uuid()
                    .is_none()
                    .then(|| {
                        service_access
                            .get(&(
                                peer_id.to_owned(),
                                path.service_uuid().to_owned(),
                                path.service_occurrence(),
                            ))
                            .and_then(|facts| facts.primary)
                    })
                    .flatten(),
                included_services: path
                    .characteristic_uuid()
                    .is_none()
                    .then(|| {
                        service_access
                            .get(&(
                                peer_id.to_owned(),
                                path.service_uuid().to_owned(),
                                path.service_occurrence(),
                            ))
                            .and_then(|facts| facts.included_services.clone())
                    })
                    .flatten(),
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
        &self,
        core: &Central,
        peer_key: &str,
        peer_id: &str,
        selector: &PathSelector,
        op_name: &'static str,
        descriptor: bool,
    ) -> Result<ResolvedInstance, DesktopError> {
        self.gatt_peer_observation_admission(peer_id, op_name)?;
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
    /// without one); a cancel settles `aborted` in the core. The answer
    /// carries the radio's own [`crate::boundary::ReadProvenance`].
    pub async fn read(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        ctl: OpControl,
    ) -> Result<CharacteristicRead, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "gatt.read")?;
        let window = ctl.budget.window(LIVENESS_OP);
        self.validate_gatt_prerequisite(peer_id, selector, &ctl, "gatt.read", false)
            .await?;
        let _gatt_admission = self
            .wait_gatt_admission(peer_id, &ctl, "gatt.read", window)
            .await?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let (operation, key) = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let (index, key, _) =
                self.resolve_instance(&core, &peer_key, peer_id, selector, "gatt.read", false)?;
            let id = core
                .start_read(
                    ctl.gatt_path(index),
                    window.core_timeout_ms(),
                    now_ms(),
                    &mut out,
                )
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
        let result = match drive_link(
            &self.inner,
            peer_id,
            "gatt.read",
            &ctl.ticket,
            window,
            _gatt_admission.dispatch(
                self.inner
                    .boundary
                    .read_characteristic(peer_id, &key.1, key.2, &key.3, key.4),
            ),
        )
        .await
        {
            Wait::Done(Ok(read)) => {
                let mut core = self.inner.core.lock().await;
                // F03: a link that died mid-read wins over the late radio
                // bytes. Generations alone cannot catch this (disconnect keeps
                // them), so the live link state competes explicitly.
                let link_live = matches!(
                    core.connection_state(&peer_key),
                    Some(ConnectionState::Connected)
                );
                if link_live {
                    Self::settle_gatt_success(&mut core, &operation, read, "gatt.read")
                } else {
                    // The OS reported the link lost: the same word every
                    // host reports (owner decision, 5.0). A release the app
                    // requested is `operation.disconnected`.
                    let lost = core.connection_state(&peer_key) == Some(ConnectionState::Lost);
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
                        if lost {
                            BleErrorCode::ConnectionLost
                        } else {
                            BleErrorCode::OperationDisconnected
                        },
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
        let result = result.map_err(|error| classify(error, OpKind::Read, true));
        self.name_link_end(
            &peer_key,
            result.map_err(|error| self.annotate_observation_failure(peer_id, error)),
        )
        .await
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

    async fn validate_gatt_prerequisite(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        ctl: &OpControl,
        operation: &'static str,
        descriptor: bool,
    ) -> Result<(), DesktopError> {
        let peer_key = self.known_peer_key(peer_id).await?;
        let core = self.inner.core.lock().await;
        let (index, _, _) =
            self.resolve_instance(&core, &peer_key, peer_id, selector, operation, descriptor)?;
        core.validate_gatt_admission(ctl.gatt_path(index), operation)
            .map_err(DesktopError::from)
    }

    async fn wait_write_ready(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        ctl: &OpControl,
        window: Window,
        admission: &crate::GattAdmission,
    ) -> Result<(), DesktopError> {
        const OP: &str = "gatt.write-when-ready";
        let peer_key = self.known_peer_key(peer_id).await?;
        let generations = {
            let core = self.inner.core.lock().await;
            // Use the same runtime boundary as write_readiness. Mobile
            // owners do not register desktop capability rows; their native
            // boundary answers unsupported when this API is unavailable.
            Generations::of(&core, &peer_key)
        };
        let mut wakes = self.native_wakes();
        let waiting = async {
            loop {
                self.readiness_source_admission(peer_id, OP)?;
                admission.assert_current()?;
                {
                    let core = self.inner.core.lock().await;
                    let current = Generations::of(&core, &peer_key);
                    if current.connection != generations.connection
                        || current.database != generations.database
                    {
                        return Err(contract_error(
                            BleErrorCode::GattStaleHandle,
                            BleErrorDomain::Gatt,
                            OP,
                        ));
                    }
                }
                self.validate_gatt_prerequisite(peer_id, selector, ctl, OP, false)
                    .await?;
                let ready = tokio::select! {
                    biased;
                    ready = self.inner.boundary.write_without_response_ready(peer_id) => ready?,
                    wake = wakes.recv() => {
                        if matches!(wake, Err(broadcast::error::RecvError::Closed)) {
                            return Err(contract_error(BleErrorCode::PlatformFailure, BleErrorDomain::Platform, OP)
                                .with_detail("the native readiness observation source closed"));
                        }
                        continue;
                    }
                };
                {
                    let core = self.inner.core.lock().await;
                    let current = Generations::of(&core, &peer_key);
                    if current.connection != generations.connection
                        || current.database != generations.database
                    {
                        return Err(contract_error(
                            BleErrorCode::GattStaleHandle,
                            BleErrorDomain::Gatt,
                            OP,
                        ));
                    }
                }
                self.readiness_source_admission(peer_id, OP)?;
                admission.assert_current()?;
                if ready {
                    return Ok(());
                }
                if matches!(wakes.recv().await, Err(broadcast::error::RecvError::Closed)) {
                    return Err(contract_error(
                        BleErrorCode::PlatformFailure,
                        BleErrorDomain::Platform,
                        OP,
                    )
                    .with_detail("the native readiness observation source closed"));
                }
            }
        };
        let result = match drive_link(&self.inner, peer_id, OP, &ctl.ticket, window, waiting).await
        {
            Wait::Done(result) => result,
            Wait::Expired => Err(timed_out(OP, window)),
            Wait::Cancelled => Err(ctl.ticket.interruption(OP)),
        };
        self.name_link_end(
            &peer_key,
            result.map_err(|error| {
                let retryability = error.retryability();
                error.with_outcome(Some(CommitState::NotDispatched), retryability)
            }),
        )
        .await
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
        self.write_admitted(peer_id, selector, value, mode, false, ctl)
            .await
    }

    /// Write without response after native readiness, in the original queue
    /// position and budget. The owned payload never waits in JavaScript.
    pub async fn write_when_ready(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        value: Vec<u8>,
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        self.write_admitted(peer_id, selector, value, "without-response", true, ctl)
            .await
    }

    async fn write_admitted(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        value: Vec<u8>,
        mode: &str,
        wait_ready: bool,
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
        self.validate_gatt_prerequisite(peer_id, selector, &ctl, "gatt.write", false)
            .await?;
        let _admission = self
            .wait_gatt_admission(
                peer_id,
                &ctl,
                if wait_ready {
                    "gatt.write-when-ready"
                } else {
                    "gatt.write"
                },
                window,
            )
            .await
            .map_err(|error| classify(error, OpKind::Write, false))?;
        self.validate_gatt_prerequisite(peer_id, selector, &ctl, "gatt.write", false)
            .await?;
        {
            let peer_key = self.known_peer_key(peer_id).await?;
            let core = self.inner.core.lock().await;
            let (_, scope, _) =
                self.resolve_instance(&core, &peer_key, peer_id, selector, "gatt.write", false)?;
            self.inner
                .acquired
                .assert_available(&scope, crate::acquired_gatt::AcquisitionKind::Write)?;
        }
        if wait_ready {
            self.wait_write_ready(peer_id, selector, &ctl, window, &_admission)
                .await?;
        }
        let measured_limit = self
            .measured_write_limit(peer_id, with_response, &ctl.ticket, window, "gatt.write")
            .await?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let (operation, key) = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let (index, key, _) =
                self.resolve_instance(&core, &peer_key, peer_id, selector, "gatt.write", false)?;
            let maximum = Self::write_maximum(&core, measured_limit, "gatt.write")?;
            let id = core
                .start_write(
                    ctl.gatt_path(index),
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
        let result = match drive_link(
            &self.inner,
            peer_id,
            "gatt.write",
            &ctl.ticket,
            window,
            _admission.dispatch(self.inner.boundary.write_characteristic(
                peer_id,
                &key.1,
                key.2,
                &key.3,
                key.4,
                value,
                with_response,
            )),
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
        let result = result.map_err(classify_dispatched_write);
        self.name_link_end(
            &peer_key,
            result.map_err(|error| self.annotate_observation_failure(peer_id, error)),
        )
        .await
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
        self.validate_gatt_prerequisite(peer_id, selector, &ctl, "gatt.read-descriptor", true)
            .await?;
        let _gatt_admission = self
            .wait_gatt_admission(peer_id, &ctl, "gatt.read-descriptor", window)
            .await?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let (operation, key, descriptor, descriptor_occurrence) = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let (index, key, descriptor) = self.resolve_instance(
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
                .start_read_descriptor(
                    ctl.gatt_path(index),
                    window.core_timeout_ms(),
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            publish_or_refuse(&mut core, &ctl.ticket, &id, "gatt.read-descriptor", None)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            (id, key, descriptor, descriptor_occurrence)
        };
        let mut drop_guard = CancelOnDrop::armed(self, operation.clone(), DropCleanup::Op);
        let result = match drive_link(
            &self.inner,
            peer_id,
            "gatt.read-descriptor",
            &ctl.ticket,
            window,
            _gatt_admission.dispatch(self.inner.boundary.read_descriptor(
                peer_id,
                &key.1,
                key.2,
                &key.3,
                key.4,
                &descriptor,
                descriptor_occurrence,
            )),
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
        let result = result.map_err(|error| classify(error, OpKind::Read, true));
        self.name_link_end(
            &peer_key,
            result.map_err(|error| self.annotate_observation_failure(peer_id, error)),
        )
        .await
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
        self.validate_gatt_prerequisite(peer_id, selector, &ctl, "gatt.write-descriptor", true)
            .await?;
        let _gatt_admission = self
            .wait_gatt_admission(peer_id, &ctl, "gatt.write-descriptor", window)
            .await
            .map_err(|error| classify(error, OpKind::Write, false))?;
        // Descriptor writes are always ATT write requests (with response).
        self.validate_gatt_prerequisite(peer_id, selector, &ctl, "gatt.write-descriptor", true)
            .await?;
        let measured_limit = self
            .measured_write_limit(peer_id, true, &ctl.ticket, window, "gatt.write-descriptor")
            .await?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let (operation, key, descriptor, descriptor_occurrence) = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let (index, key, descriptor) = self.resolve_instance(
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
                    ctl.gatt_path(index),
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
        let result = match drive_link(
            &self.inner,
            peer_id,
            "gatt.write-descriptor",
            &ctl.ticket,
            window,
            _gatt_admission.dispatch(self.inner.boundary.write_descriptor(
                peer_id,
                &key.1,
                key.2,
                &key.3,
                key.4,
                &descriptor,
                descriptor_occurrence,
                value,
            )),
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
        let result = result.map_err(classify_dispatched_write);
        self.name_link_end(
            &peer_key,
            result.map_err(|error| self.annotate_observation_failure(peer_id, error)),
        )
        .await
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
        self.validate_gatt_prerequisite(peer_id, selector, &ctl, "gatt.subscribe", false)
            .await?;
        let _gatt_admission = self
            .wait_gatt_admission(peer_id, &ctl, "gatt.subscribe", window)
            .await?;
        let peer_key = self.known_peer_key(peer_id).await?;
        // Resolve the instance first (pure read, no side effects) so the
        // scoped retirement gate covers admission through routing insertion,
        // but never a physical enable wait (the event loop must stay live).
        let coordinator = discovery_coordinator(&self.inner, peer_id);
        let admission = match drive_link(
            &self.inner,
            peer_id,
            "gatt.subscribe",
            &ctl.ticket,
            window,
            async { Ok(coordinator.snapshot.lock().await) },
        )
        .await
        {
            Wait::Done(Ok(guard)) => guard,
            Wait::Done(Err(error)) => return Err(error),
            Wait::Expired => {
                return Err(classify(
                    timed_out("gatt.subscribe", window),
                    OpKind::Subscribe,
                    false,
                ));
            }
            Wait::Cancelled => {
                return Err(classify(
                    ctl.ticket.interruption("gatt.subscribe"),
                    OpKind::Subscribe,
                    false,
                ));
            }
        };
        self.refuse_before_admission(&ctl, "gatt.subscribe")?;
        let key = {
            let core = self.inner.core.lock().await;
            let (index, key, _) = self.resolve_instance(
                &core,
                &peer_key,
                peer_id,
                selector,
                "gatt.subscribe",
                false,
            )?;
            core.validate_gatt_admission(ctl.gatt_path(index), "gatt.subscribe")
                .map_err(DesktopError::from)?;
            self.inner
                .acquired
                .assert_available(&key, crate::acquired_gatt::AcquisitionKind::Notify)?;
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
            let (index, resolved, _) = self.resolve_instance(
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
                    ctl.gatt_path(index),
                    policy.as_str(),
                    item_capacity,
                    byte_capacity,
                    consumer,
                    window.core_timeout_ms(),
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            // Consumer IDs may be reused after a terminal generation. The
            // new admission owns its own cleanup; old link-loss evidence
            // must never waive a later generation's native disable.
            lock_std(&self.inner.retired_consumers)
                .remove(&(peer_key.clone(), consumer.to_owned()));
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
        drop(admission);
        if !drive_enable {
            _gatt_admission.mark_dispatched();
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
        let result = match drive_link(
            &self.inner,
            peer_id,
            "gatt.subscribe",
            &ctl.ticket,
            window,
            _gatt_admission.dispatch(self.inner.boundary.set_notifications_with_preference(
                peer_id,
                &key.1,
                key.2,
                &key.3,
                key.4,
                true,
                epoch,
                delivery,
                ctl.delivery_preference(),
            )),
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
        let result = result.map_err(|error| classify(error, OpKind::Subscribe, true));
        self.name_link_end(
            &peer_key,
            result.map_err(|error| self.annotate_observation_failure(peer_id, error)),
        )
        .await
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
        self.unsubscribe_draining(peer_id, selector, consumer, ctl, None)
            .await
    }

    /// Internal native handoff: drain accepted values under the same core
    /// lock that retires the consumer, so a final value cannot disappear
    /// between the collector's last poll and physical disable completion.
    pub(crate) async fn unsubscribe_draining(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
        ctl: OpControl,
        drain: Option<&(dyn Fn(NotificationPoll) + Send + Sync)>,
    ) -> Result<bool, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        if self.inner.shutdown_release_confirmed.load(Ordering::SeqCst) {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let retired = lock_std(&self.inner.retired_consumers)
                .contains(&(peer_key.clone(), consumer.to_owned()));
            if (core.connection_state(&peer_key) == Some(ConnectionState::Disconnected) || retired)
                && !lock_std(&self.inner.half_open_cleanup).contains_key(peer_id)
            {
                if core.consumer_path(&peer_key, selector, consumer).is_none() && retired {
                    lock_std(&self.inner.retired_consumers)
                        .remove(&(peer_key, consumer.to_owned()));
                    return Ok(false);
                }
                let index = core
                    .consumer_path(&peer_key, selector, consumer)
                    .ok_or_else(|| {
                        contract_error(
                            BleErrorCode::OwnershipDenied,
                            BleErrorDomain::Gatt,
                            "gatt.unsubscribe",
                        )
                    })?;
                drain_consumer_before_retirement(&mut core, index, consumer, drain);
                core.unsubscribe(index, consumer, now_ms(), &mut batch())
                    .map_err(DesktopError::from)?;
                lock_std(&self.inner.retired_consumers).remove(&(peer_key, consumer.to_owned()));
                recycle_observations(&mut core);
                return Ok(false);
            }
        }
        self.precheck(&ctl, "gatt.unsubscribe")?;
        let window = ctl.budget.window(LIVENESS_CLEANUP);
        let _gatt_admission = self
            .wait_gatt_admission(peer_id, &ctl, "gatt.unsubscribe", window)
            .await?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let (disable_physical, path_index, key) = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            // Cleanup addresses the exact admitted consumer, not whichever
            // generation happens to resolve the same selector now.
            let resolved = match core.consumer_path(&peer_key, selector, consumer) {
                Some(index) => core
                    .stored_path(index)
                    .and_then(|stored| {
                        stored
                            .characteristic_uuid()
                            .map(|uuid| (index, instance_key(peer_id, stored, uuid), None))
                    })
                    .ok_or_else(|| {
                        contract_error(
                            BleErrorCode::GattNotFound,
                            BleErrorDomain::Gatt,
                            "gatt.unsubscribe",
                        )
                    }),
                None => self.resolve_instance(
                    &core,
                    &peer_key,
                    peer_id,
                    selector,
                    "gatt.unsubscribe",
                    false,
                ),
            };
            let (index, key, _) = match resolved {
                Ok(resolved) => resolved,
                Err(error) => {
                    // A confirmed link end/reset already ended this exact
                    // consumer's physical obligation, independently of whether
                    // a later reconnect could rediscover its former path.
                    if lock_std(&self.inner.retired_consumers)
                        .remove(&(peer_key.clone(), consumer.to_owned()))
                    {
                        return Ok(false);
                    }
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
            // Invalid consumers may be removed immediately by unsubscribe.
            // Hand their already accepted FIFO to the owner before retirement.
            drain_consumer_before_retirement(&mut core, index, consumer, drain);
            let disable = core
                .unsubscribe(index, consumer, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            lock_std(&self.inner.retired_consumers)
                .remove(&(peer_key.clone(), consumer.to_owned()));
            drain_consumer_before_retirement(&mut core, index, consumer, drain);
            (disable, index, key)
        };
        if !disable_physical && lock_std(&self.inner.retained_enablements).contains(&key) {
            return self
                .release_retained(peer_id, &key, &ctl.ticket, window)
                .await;
        }
        if !disable_physical && !self.inner.failed_disables.lock().await.contains(&key) {
            // No radio work: still recycle any terminal shares (e.g. an
            // immediate-success join that released elsewhere) so the
            // ledger never grows across unsubscribe-only cycles.
            let mut core = self.inner.core.lock().await;
            recycle_observations(&mut core);
            return Ok(false);
        }
        _gatt_admission
            .dispatch(self.drive_disable(
                peer_id,
                &key,
                path_index,
                &ctl.ticket,
                window,
                consumer,
                drain,
            ))
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

    #[allow(clippy::too_many_arguments)]
    async fn drive_disable(
        &self,
        peer_id: &str,
        key: &InstanceKey,
        path_index: usize,
        ticket: &OpTicket,
        window: Window,
        consumer: &str,
        drain: Option<&(dyn Fn(NotificationPoll) + Send + Sync)>,
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
                drain_consumer_before_retirement(&mut core, path_index, consumer, drain);
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
        // Reading an already admitted consumer's FIFO acquires no radio work.
        // It must remain drainable even while physical shutdown is pending.
        if !self.inner.shut_down.load(Ordering::SeqCst) {
            self.admit("gatt.take-notification")?;
        }
        let peer_key = self.known_peer_key(peer_id).await?;
        let mut core = self.inner.core.lock().await;
        if self.inner.shut_down.load(Ordering::SeqCst)
            && core.consumer_path(&peer_key, selector, consumer).is_none()
        {
            return Err(contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Gatt,
                "gatt.take-notification",
            ));
        }
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
                // a cancel against the still-present terminal would. An
                // expired local acknowledgement and a foreign central's
                // opaque id are distinct lifecycle/ownership outcomes;
                // only a structurally unknown id remains `argument.invalid`.
                let ticket = resolved_scan_ticket(&core, &self.inner.completed_scans, operation);
                if matches!(ticket, CompletedScanTicket::Settled(_))
                    || core.shutdown_terminal_kind(operation).is_some()
                {
                    let suppressed = core.suppressed_count(operation).unwrap_or(0);
                    ubm_core::central::CompletionOutcome::DuplicateSuppressed { suppressed }
                } else {
                    return Err(match ticket {
                        CompletedScanTicket::Expired => expired_scan_ticket_error(),
                        CompletedScanTicket::Foreign => foreign_scan_ticket_error(),
                        CompletedScanTicket::Settled(_)
                        | CompletedScanTicket::Local
                        | CompletedScanTicket::Unknown => DesktopError::from(error),
                    });
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
        self.inner
            .shutdown_release_confirmed
            .store(false, Ordering::SeqCst);
        // F14: admission closes before any cleanup starts, so a racing
        // starter cannot slip work in behind the scan stop.
        self.inner.shut_down.store(true, Ordering::SeqCst);
        self.inner.gatt_admission.seal();
        self.inner.acquired.retire(
            None,
            None,
            contract_error(
                BleErrorCode::OperationCancelledByDestroy,
                BleErrorDomain::Core,
                "gatt.acquired",
            ),
        );
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
        let half_open: Vec<_> = lock_std(&self.inner.half_open_cleanup)
            .iter()
            .map(|(peer, debt)| (peer.clone(), debt.clone()))
            .collect();
        for (peer, debt) in half_open {
            let _ = self.retry_half_open_cleanup(&peer, &debt).await;
        }
        // F14: release owned OS links with per-link receipts before the
        // owner is destroyed.
        let mut link_cleanup_failures = self.release_owned_links().await;
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
        // Settle admitted native work before accounting for its temporary
        // event streams. No operation may acquire a new transport behind this.
        let (record, destroy_steps) = self.drive_destroy().await;
        // The logical scan operation can be retired while its exact physical
        // identity remains in scan_slot. Report current physical debt here,
        // not as irreversible kernel cleanup history that poisons a retry.
        let record = record.and_then(|record| {
            let mut failures = record.failures().to_vec();
            if let Some(failure) = scan_stop_failure.as_ref() {
                failures.push(
                    CleanupFailure::new("scan".to_owned(), failure.code())
                        .map_err(DesktopError::from)?,
                );
            }
            if failures.is_empty() {
                return Ok(record);
            }
            CleanupRecord::new(
                record.operation_id().cloned(),
                CleanupState::ReleaseFailed,
                failures,
            )
            .map_err(DesktopError::from)
        });
        let half_open_close_failures = lock_std(&self.inner.half_open_cleanup)
            .values()
            .map(|debt| {
                lock_std(&debt.failure).clone().unwrap_or_else(|| {
                    contract_error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Cleanup,
                        "connection.compensate",
                    )
                    .with_detail("half-open connection cleanup remains owned")
                })
            })
            .collect();
        let mut transport_close_failures =
            match tokio::time::timeout(Duration::from_secs(5), self.inner.boundary.finish_close())
                .await
            {
                // The native owner has retried and accounted for every remaining
                // obligation. Do not append an earlier refusal it just retired.
                Ok(result) => result,
                Err(_) => {
                    // Without final accounting, retain the provisional causes
                    // as well as the timeout; cleanup remains independently owned.
                    link_cleanup_failures.push(
                        DesktopError::new(
                            BleErrorCode::OperationTimedOut,
                            BleErrorDomain::Cleanup,
                            "radio.close.transport",
                        )
                        .with_detail(
                            "Transport cleanup remains owned after the five-second close bound",
                        ),
                    );
                    link_cleanup_failures
                }
            };
        match tokio::time::timeout(
            Duration::from_secs(5),
            self.inner.acquired.release_scope(None, None),
        )
        .await
        {
            Ok(failures) => transport_close_failures.extend(failures),
            Err(_) => transport_close_failures.push(
                contract_error(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Cleanup,
                    "gatt.acquired.close",
                )
                .with_detail("acquired child cleanup remains owned"),
            ),
        }
        let radio_close_failures = self.inner.boundary.take_close_failures();
        // F15: the final record is taken only after every destroy pass
        // executed, every dispatched remainder was answered, and every
        // terminal release was acknowledged — never from the legacy
        // unacknowledged `destroy()`.
        let report = ShutdownReport {
            record,
            radio_close_failures,
            transport_close_failures,
            half_open_close_failures,
            destroy_steps,
            scan_stop_failure,
        };
        self.inner
            .shutdown_release_confirmed
            .store(report.is_released(), Ordering::SeqCst);
        report
    }

    /// Shutdown's final scan stop (PR210-09): one bounded attempt. When it
    /// fails, the retained scan op settles as failed with a release
    /// failure in the current shutdown report. The physical marker remains
    /// owned for a later shutdown retry; no new scan admission is permitted.
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
            recycle_observations(&mut core);
        }
        // stop_scan_with retains either its failed leader or an in-flight
        // leader followed by this bounded attempt. Never erase that identity
        // or replace its single-flight receiver merely because shutdown waited.
        Some(error)
    }

    /// Release every owned OS link (F14): each peer whose core connection is
    /// still live gets one bounded radio disconnect. Success confirms link
    /// release (`Disconnected`, published as `Released { requested: true }`);
    /// a radio failure or deadline marks the link `Disconnecting` and
    /// records a disconnect failure, so the final destroy record names it
    /// (receipt) instead of claiming a clean release. Skips peers that
    /// already released, so repeat shutdowns stay quiet and idempotent.
    async fn release_owned_links(&self) -> Vec<DesktopError> {
        let mut cleanup_failures = Vec::new();
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
                self.inner.boundary.disconnect_with_observation(&peer_id),
            )
            .await;
            if matches!(&outcome, Ok(Ok(_))) {
                note_confirmed_release(&self.inner, &peer_id);
            }
            // Late radio completions must not resurrect the link: drop local
            // subscription routing for this peer now.
            self.drop_peer_subscriptions(&peer_id).await;
            let event = {
                let mut core = self.inner.core.lock().await;
                match outcome {
                    Ok(Ok(observation)) => {
                        cleanup_failures.extend(observation.cleanup_failure.clone());
                        let generation = Generations::of(&core, &peer_key);
                        core.shutdown_release_link(&peer_key).ok().map(|()| {
                            self.inner
                                .boundary
                                .consume_disconnect_observation(&peer_id, &observation);
                            self.inner.stage_lifecycle_with_platform(
                                &peer_id,
                                &peer_key,
                                generation,
                                LifecycleKind::Released { requested: true },
                                observation.platform,
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
        cleanup_failures
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
            core.current_shutdown_record().map_err(DesktopError::from)
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

    /// One word per event (owner decision, 5.0): a link operation that
    /// failed because the link is gone is `connection.lost`, unless the
    /// app's own release was underway, which is `operation.disconnected`.
    async fn name_link_end<T>(
        &self,
        peer_key: &str,
        result: Result<T, DesktopError>,
    ) -> Result<T, DesktopError> {
        match result {
            Err(error) if error.code() == BleErrorCode::ConnectionLost => {
                let requested = matches!(
                    self.inner.core.lock().await.connection_state(peer_key),
                    Some(ConnectionState::Disconnecting | ConnectionState::Disconnected)
                );
                Err(error.named_for_requested_release(requested))
            }
            other => other,
        }
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
    pub async fn open_btleplug(profile: CentralProfile) -> Result<Self, DesktopError> {
        Self::open_btleplug_with_policy(profile, None).await
    }

    /// The normal production open path with a trusted BlueZ LE attestation.
    /// Adapter initialization and compensation are identical to `open_btleplug`.
    pub async fn open_btleplug_with_policy(
        mut profile: CentralProfile,
        connection_policy: Option<crate::boundary::BluezConnectionPolicy>,
    ) -> Result<Self, DesktopError> {
        let started = tokio::time::Instant::now();
        let open = crate::btleplug_backend::BtleplugRadio::open_on_with_policy(
            crate::executor::desktop_runtime(),
            profile.adapter_id.clone(),
            profile.bluez_bus,
            connection_policy,
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
                if !report.is_released() {
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
    clear_peer_routing_scoped(inner, peer_id, None).await;
}
async fn clear_peer_routing_scoped<B>(
    inner: &Arc<Inner<B>>,
    peer_id: &str,
    expected: Option<(&str, Option<&str>)>,
) {
    let mut subscriptions = inner.subscriptions.lock().await;
    let mut failed = inner.failed_disables.lock().await;
    let mut epochs = inner.epochs.lock().await;
    let _core = if let Some((peer_key, generation)) = expected {
        let core = inner.core.lock().await;
        if Generations::of(&core, peer_key).connection.as_deref() != generation {
            return;
        }
        Some(core)
    } else {
        None
    };
    lock_std(&inner.retained_enablements).retain(|key| key.0 != peer_id);
    lock_std(&inner.service_access).retain(|(peer, _, _), _| peer != peer_id);
    subscriptions.retain(|key, _| key.0 != peer_id);
    failed.retain(|key| key.0 != peer_id);
    lock_std(&inner.deliveries).retain(|key, _| key.0 != peer_id);
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
                    Some(RadioEvent::ConnectionParameters {
                        peer_id,
                        interval_us,
                        latency,
                        supervision_timeout_us,
                    }) => {
                        parity::publish_connection_parameters(
                            &inner,
                            &peer_id,
                            interval_us,
                            latency,
                            supervision_timeout_us,
                        )
                        .await;
                    }
                    Some(RadioEvent::Advertisement(snapshot)) => {
                        ingest_advertisement(&inner, snapshot).await;
                    }
                    Some(RadioEvent::ConnectionParameterSourceFailed { peer_id, error }) => {
                        parity::publish_connection_parameter_source(&inner, &peer_id, Some(error), 0).await;
                    }
                    Some(RadioEvent::ConnectionParameterGap { peer_id, missed }) => {
                        parity::publish_connection_parameter_source(&inner, &peer_id, None, missed).await;
                    }
                    Some(RadioEvent::Connected(peer_id)) => {
                        reconcile_connected(&inner, &peer_id).await;
                    }
                    Some(RadioEvent::Disconnected(peer_id)) => {
                        reconcile_disconnected(&inner, &peer_id, false).await;
                    }
                    Some(RadioEvent::Lost(peer_id)) => {
                        reconcile_disconnected(&inner, &peer_id, true).await;
                    }
                    #[cfg(target_os = "linux")]
                    Some(RadioEvent::LinuxPhysicalLost { peer_id, physical_generation, reason }) => {
                        reconcile_disconnected_scoped(&inner, &peer_id, false, Some((physical_generation, reason))).await;
                    }
                    Some(RadioEvent::ServicesChanged(peer_id)) => {
                        services_changed_invalidated(&inner, &peer_id).await;
                    }
                    Some(RadioEvent::ServicesChangedScoped { peer_id, identity }) => {
                        services_changed_scoped_invalidated(&inner, &peer_id, &identity).await;
                    }
                    Some(RadioEvent::GattInvalidationHint(peer_id)) => {
                        inner.note_compensation_failure();
                        eprintln!("{}: unresolved GATT invalidation hint for {peer_id}", inner.identity.log_tag());
                    }
                    Some(RadioEvent::GattWatchFailed(detail)) => {
                        gatt_watch_failed(&inner, &detail).await;
                    }
                    Some(RadioEvent::GattObservationFailed { peer_id, identity, error }) => {
                        gatt_observation_failed(&inner, &peer_id, &identity, &error).await;
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
                            adapter_reset(&inner, cause, Some(state), Some(sequence)).await;
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
                        let event = wake_state_waiters(&inner);
                        if loss {
                            adapter_reset(
                                &inner,
                                AdapterLossCause::Unauthorized,
                                Some(event.state),
                                Some(event.sequence),
                            )
                            .await;
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
                        let event = wake_state_waiters(&inner);
                        if inner.teardown_on_loss {
                            adapter_reset(&inner, cause, Some(event.state), Some(event.sequence)).await;
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
                            "{}: {skipped} adapter events were lost: the OS event \
                             broadcast outran the radio",
                            inner.identity.log_tag()
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
fn wake_state_waiters<B>(inner: &Inner<B>) -> AdapterEvent {
    let state = lock_std(&inner.adapter_facts)
        .power
        .unwrap_or(AdapterPowerState::Unknown);
    let sequence = inner.adapter_sequence.fetch_add(1, Ordering::SeqCst) + 1;
    let event = AdapterEvent { sequence, state };
    let _ = inner.adapter.send(event);
    inner.signal(CentralSignal::Adapter(event));
    event
}

/// The adapter facts at open, read from the radio when it has an adapter
/// gate (a gate-less radio reads nothing). A fact the radio cannot report
/// stays unreported; a failed read is logged and stays unreported, so
/// admission never refuses on it.
fn reported_fact<T>(
    log_tag: &str,
    what: &str,
    outcome: Result<Result<T, DesktopError>, tokio::time::error::Elapsed>,
) -> Option<T> {
    match outcome {
        Ok(Ok(value)) => Some(value),
        Ok(Err(error)) => {
            if error.code() != BleErrorCode::CapabilityUnsupported {
                eprintln!(
                    "{log_tag}: adapter {what} unread at open: {}",
                    error.detail().unwrap_or(error.code_str())
                );
            }
            None
        }
        Err(_) => {
            eprintln!("{log_tag}: adapter {what} read at open timed out");
            None
        }
    }
}

async fn seed_adapter_facts<B: RadioBoundary>(
    boundary: &B,
    admission: AdmissionPolicy,
    log_tag: &str,
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
        log_tag,
        "state",
        tokio::time::timeout(LIVENESS_CLEANUP, boundary.adapter_state()).await,
    );
    facts.authorization = reported_fact(
        log_tag,
        "authorization",
        tokio::time::timeout(LIVENESS_CLEANUP, boundary.adapter_authorization()).await,
    );
    facts.lost = facts.power.is_some_and(AdapterPowerState::is_loss);
    facts
}

/// The attachment and kernel generation after reset `index` of the central
/// opened as `ordinal`, as the owning host names them. The backend instance
/// and the adapter must stay those of `previous`: a host whose names move
/// them fails the reset (reported), never re-attaches the central elsewhere.
fn next_scope(
    identity: &dyn HostIdentity,
    previous: &AttachmentTuple,
    ordinal: u64,
    index: u64,
) -> Result<(AttachmentTuple, Generation), DesktopError> {
    let epoch = AttachmentEpoch {
        ordinal,
        resets: index,
        adapter: previous.adapter_id().as_str(),
    };
    let current = identity.attachment(epoch).map_err(DesktopError::from)?;
    if current.backend_instance_id() != previous.backend_instance_id()
        || current.adapter_id() != previous.adapter_id()
    {
        return Err(contract_error(
            BleErrorCode::LifecycleInvariantViolation,
            BleErrorDomain::Core,
            &format!("{}.reset.identity", identity.namespace()),
        )
        .with_detail("the host renamed the backend instance or adapter across a reset"));
    }
    let generation = identity
        .kernel_generation(epoch)
        .map_err(DesktopError::from)?;
    Ok((current, generation))
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
async fn adapter_reset<B: RadioBoundary>(
    inner: &Arc<Inner<B>>,
    cause: AdapterLossCause,
    power: Option<AdapterPowerState>,
    adapter_sequence: Option<u64>,
) {
    inner.acquired.retire(
        None,
        None,
        contract_error(
            BleErrorCode::OperationReset,
            BleErrorDomain::Core,
            "gatt.acquired",
        ),
    );
    let scan = inner.scan_slot().take();
    let peers: Vec<(String, String)> = inner
        .peers
        .lock()
        .await
        .iter()
        .map(|(peer_id, peer_key)| (peer_id.clone(), peer_key.clone()))
        .collect();
    let ended_subscriptions = inner.subscriptions.lock().await.len();
    let index;
    let mut release_failures = Vec::new();
    let previous = lock_std(&inner.attachment).clone();
    let (current, cancelled_operations, links, events) = {
        let mut core = inner.core.lock().await;
        // Publish the adapter epoch together with its generation transition.
        // A failed connect retaining cleanup under this lock must see either
        // the old scope and epoch or the new pair, never a mixture.
        index = inner.resets.fetch_add(1, Ordering::SeqCst) + 1;
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
        lock_std(&inner.retired_leases).extend(core.held_leases().into_iter().map(|key| {
            let generation = Generations::of(&core, &key.0).connection;
            (key, generation)
        }));
        lock_std(&inner.retired_consumers).extend(core.held_consumers());
        if let Some(active) = &scan {
            retain_completed_scan(
                &inner.completed_scans,
                &active.id,
                OperationTerminalKind::Reset,
            );
        }
        let reset = next_scope(inner.identity.as_ref(), &previous, inner.ordinal, index).and_then(
            |(current, generation)| {
                let mut out = batch();
                let settled = core
                    .handle_adapter_reset(current.clone(), generation, now_ms(), &mut out)
                    .map_err(DesktopError::from)?;
                let _ = out.drain();
                Ok((current, settled))
            },
        );
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
            Ok(Ok(())) => {
                note_confirmed_release(inner, peer_id);
            }
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
        power,
        adapter_sequence,
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
    reobserve_known_peers(inner).await;
}

/// Findings 120 and 205: re-read every known peripheral and report each as
/// an observation of the OS's device state (finding 120: Tauri 4.x re-read
/// known peripherals every 2 s, so a peer that does not advertise again, or
/// whose repeats the OS filters, stays visible; finding 205: CoreBluetooth
/// withholds repeats for known peers, so a fresh scan re-reads them once at
/// start). A re-read the OS cannot answer is counted and logged, never
/// silent.
async fn reobserve_known_peers<B: RadioBoundary>(inner: &Arc<Inner<B>>) {
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
                "{}: known peripherals could not be re-read during a scan: {}",
                inner.identity.log_tag(),
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
/// (`Released { requested: true }`, `Disconnected`) unless the OS reported
/// the disconnect with an error (`errored`, [`RadioEvent::Lost`]); a live
/// link, or an errored one, is lost (`LinkLost`, `Lost`). A link already
/// terminal is a stale event for an older generation and publishes nothing.
async fn reconcile_disconnected<B: RadioBoundary>(
    inner: &Arc<Inner<B>>,
    peer_id: &str,
    errored: bool,
) {
    reconcile_disconnected_scoped(inner, peer_id, errored, None).await;
}

async fn reconcile_disconnected_scoped<B: RadioBoundary>(
    inner: &Arc<Inner<B>>,
    peer_id: &str,
    errored: bool,
    physical_generation: Option<(u64, u8)>,
) {
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    let Some(peer_key) = peer_key else {
        return;
    };
    // Acquire local routing ownership before core admission. No synchronous
    // guard crosses an await; native cleanup is only enqueued by the boundary.
    let mut subscriptions = inner.subscriptions.lock().await;
    let mut failed = inner.failed_disables.lock().await;
    let mut epochs = inner.epochs.lock().await;
    let event = {
        let mut core = inner.core.lock().await;
        #[cfg(target_os = "linux")]
        if let Some((generation, reason)) = physical_generation
            && !inner
                .boundary
                .accept_physical_loss(peer_id, generation, reason)
                .await
        {
            // An authenticated observation can still belong to an older
            // generation. It neither invalidates GATT nor publishes loss.
            return;
        }
        #[cfg(not(target_os = "linux"))]
        debug_assert!(physical_generation.is_none());
        lock_std(&inner.retained_enablements).retain(|key| key.0 != peer_id);
        subscriptions.retain(|key, _| key.0 != peer_id);
        failed.retain(|key| key.0 != peer_id);
        lock_std(&inner.deliveries).retain(|key, _| key.0 != peer_id);
        let epoch = epochs.entry(peer_id.to_owned()).or_insert(0);
        *epoch = epoch.saturating_add(1);
        let generation = Generations::of(&core, &peer_key);
        note_confirmed_release(inner, peer_id);
        let consumers: Vec<_> = core
            .held_consumers()
            .into_iter()
            .filter(|(owner, _)| owner == &peer_key)
            .collect();
        let kind = match core.connection_state(&peer_key) {
            Some(ConnectionState::Disconnecting) if !errored => core
                .note_link_released(&peer_key)
                .ok()
                .map(|()| LifecycleKind::Released { requested: true }),
            // The OS said the link ended with an error: it was lost, not
            // released, whether or not a release was pending.
            Some(ConnectionState::Disconnecting)
            | Some(ConnectionState::Connected | ConnectionState::Connecting) => {
                let mut out = batch();
                core.note_peer_loss(&peer_key, now_ms(), &mut out)
                    .ok()
                    .map(|_| LifecycleKind::LinkLost)
            }
            _ => None,
        };
        kind.map(|kind| {
            lock_std(&inner.retired_consumers).extend(consumers);
            lock_std(&inner.retired_leases).extend(
                core.held_leases()
                    .into_iter()
                    .filter(|(peer, _)| peer == &peer_key)
                    .map(|key| {
                        let generation = Generations::of(&core, &key.0).connection;
                        (key, generation)
                    }),
            );
            let platform = physical_generation
                .map(|(_, reason)| crate::boundary::bluez_disconnect_observation(reason));
            inner.stage_lifecycle_with_platform(peer_id, &peer_key, generation, kind, platform)
        })
    };
    drop(epochs);
    drop(failed);
    drop(subscriptions);
    if let Some(event) = event {
        lock_std(&inner.parameter_source_failures).remove(peer_id);
        // Only a transition of a live link ends its operations; a stale
        // event for an older generation publishes nothing and ends nothing.
        inner
            .gatt_admission
            .invalidate_peer(peer_id, BleErrorCode::ConnectionLost);
        inner.acquired.retire(
            Some(peer_id),
            None,
            contract_error(
                BleErrorCode::ConnectionLost,
                BleErrorDomain::Connection,
                "gatt.acquired",
            ),
        );
        note_link_end(inner, peer_id);
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
    if delivered {
        let _ = inner.native_wake.send(());
        if inner.observer.is_some() {
            inner.signal(CentralSignal::Value { scope, value });
        }
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
            "{}: {lost} lost notifications could not be accounted on {scope:?}: {error}",
            inner.identity.log_tag()
        );
    }
}

/// Invalidate generations when the OS reports a changed GATT database
/// (L6): stale paths must fail closed and require rediscovery instead of
/// serving re-reads through dead handles. Routing drops alongside the
/// core hubs so late values cannot reach invalidated consumers. A live
/// connection publishes `ServicesChanged` (PR210-11).
fn observation_refusal(cause: &DesktopError, operation: &'static str) -> DesktopError {
    let mut error = contract_error(
        BleErrorCode::GattDiscoveryRequired,
        BleErrorDomain::Gatt,
        operation,
    );
    if let Some(detail) = cause.detail() {
        error = error.with_detail(detail);
    }
    if let Some(platform) = cause.platform() {
        error = error.with_platform(platform.clone());
    }
    error
}

async fn gatt_observation_failed<B: RadioBoundary>(
    inner: &Arc<Inner<B>>,
    peer_id: &str,
    identity: &GattSnapshotIdentity,
    error: &DesktopError,
) {
    let coordinator = discovery_coordinator(inner, peer_id);
    let mut snapshot = coordinator.snapshot.lock().await;
    if snapshot.identity.as_ref() != Some(identity) {
        return;
    }
    // The backend evicts the failed graph before sending this event. Any
    // accepted graph now, even the same daemon token, is a newer successful
    // re-verification and must survive the delayed observation failure.
    if matches!(inner.boundary.gatt_snapshot_identity(peer_id), Ok(Some(_))) {
        return;
    }
    let Some(peer_key) = inner.peers.lock().await.get(peer_id).cloned() else {
        return;
    };
    {
        let core = inner.core.lock().await;
        if core
            .connection_generation(&peer_key)
            .zip(core.database_generation(&peer_key))
            != snapshot.generations
            || core.database_state(&peer_key) != Some(DatabaseState::Current)
        {
            return;
        }
    }
    lock_std(&inner.gatt_observation_failures)
        .entry(peer_id.to_owned())
        .or_insert_with(|| error.clone());
    inner.acquired.retire(Some(peer_id), None, error.clone());
    inner.note_compensation_failure();
    eprintln!(
        "{}: peer GATT observation failed: {error}; {:?}",
        inner.identity.log_tag(),
        error.detail()
    );
    retire_database_routing(inner, peer_id).await;
    let mut core = inner.core.lock().await;
    if let Err(error) = core
        .services_changed(&peer_key)
        .and_then(|()| core.require_rediscovery(&peer_key))
    {
        inner.note_compensation_failure();
        eprintln!(
            "{}: peer GATT retirement failed: {error}",
            inner.identity.log_tag()
        );
    }
    *snapshot = DiscoverySnapshot::default();
    let _ = inner.native_wake.send(());
}

async fn gatt_watch_failed<B: RadioBoundary>(inner: &Arc<Inner<B>>, detail: &DesktopError) {
    inner.acquired.retire(None, None, detail.clone());
    {
        let mut retained = lock_std(&inner.gatt_watch_failure);
        if retained.is_none() {
            *retained = Some(detail.clone());
        }
    }
    inner.note_compensation_failure();
    eprintln!(
        "{}: GATT control observation failed: {detail}; {:?}",
        inner.identity.log_tag(),
        detail.detail()
    );
    let peers: Vec<(String, String)> = inner
        .peers
        .lock()
        .await
        .iter()
        .map(|(id, key)| (id.clone(), key.clone()))
        .collect();
    for (peer_id, peer_key) in peers {
        let coordinator = discovery_coordinator(inner, &peer_id);
        let mut snapshot = coordinator.snapshot.lock().await;
        retire_database_routing(inner, &peer_id).await;
        let mut core = inner.core.lock().await;
        if matches!(
            core.database_state(&peer_key),
            Some(DatabaseState::Current | DatabaseState::Discovering)
        ) && let Err(error) = core
            .services_changed(&peer_key)
            .and_then(|()| core.require_rediscovery(&peer_key))
        {
            inner.note_compensation_failure();
            eprintln!(
                "{}: local GATT retirement failed: {error}",
                inner.identity.log_tag()
            );
        }
        *snapshot = DiscoverySnapshot::default();
    }
    let _ = inner.native_wake.send(());
}

async fn services_changed_scoped_invalidated<B: RadioBoundary>(
    inner: &Arc<Inner<B>>,
    peer_id: &str,
    identity: &GattSnapshotIdentity,
) {
    let coordinator = discovery_coordinator(inner, peer_id);
    let mut snapshot = coordinator.snapshot.lock().await;
    if snapshot.identity.as_ref() != Some(identity) {
        return;
    }
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    let Some(peer_key) = peer_key else {
        return;
    };
    {
        let core = inner.core.lock().await;
        if core
            .connection_generation(&peer_key)
            .zip(core.database_generation(&peer_key))
            != snapshot.generations
            || core.database_state(&peer_key) != Some(DatabaseState::Current)
        {
            return;
        }
    }
    // Discovery publication and new routing admission hold this same gate.
    // The existing cleanup performs only local bookkeeping, never radio I/O.
    services_changed_invalidated(inner, peer_id).await;
    snapshot.identity = None;
    snapshot.generations = None;
    snapshot.report = None;
    snapshot.leases.clear();
}

async fn services_changed_invalidated<B: RadioBoundary>(inner: &Arc<Inner<B>>, peer_id: &str) {
    inner
        .gatt_admission
        .invalidate_peer(peer_id, BleErrorCode::GattStaleHandle);
    inner.acquired.retire(
        Some(peer_id),
        None,
        contract_error(
            BleErrorCode::GattStaleHandle,
            BleErrorDomain::Gatt,
            "gatt.acquired",
        ),
    );
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    let Some(peer_key) = peer_key else {
        return;
    };
    retire_database_routing(inner, peer_id).await;
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

/// Retire local routing without asserting a physical cause. The caller owns
/// the per-peer admission gate when retirement can race new publication.
async fn retire_database_routing<B>(inner: &Arc<Inner<B>>, peer_id: &str) {
    // Finding 40: the routing goes, but every enabled instance's OS CCCD
    // may still be live — keep each one releasable by its instance.
    let mut subscriptions = inner.subscriptions.lock().await;
    let mut failed = inner.failed_disables.lock().await;
    let mut epochs = inner.epochs.lock().await;
    lock_std(&inner.service_access).retain(|(peer, _, _), _| peer != peer_id);
    // Acquire every asynchronous guard before changing ownership: deadline
    // or cancellation while waiting cannot discard a partially copied debt.
    let mut retained = lock_std(&inner.retained_enablements);
    let enabled: Vec<InstanceKey> = subscriptions
        .keys()
        .chain(failed.iter())
        .chain(retained.iter())
        .filter(|key| key.0 == peer_id)
        .cloned()
        .collect();
    if enabled.is_empty() {
        return;
    }
    retained.extend(enabled);
    subscriptions.retain(|key, _| key.0 != peer_id);
    failed.retain(|key| key.0 != peer_id);
    lock_std(&inner.deliveries).retain(|key, _| key.0 != peer_id);
    let epoch = epochs.entry(peer_id.to_owned()).or_insert(0);
    *epoch = epoch.saturating_add(1);
}

/// Adapter behavior over the mocked boundary: scan ownership and cleanup,
/// peer/connection/GATT mapping with contract identities, partial
/// discovery failures, descriptor paths, subscription sharing, and
/// cancellation. No radio is touched; the fake boundary is the only
/// evidence source on this host.
#[cfg(test)]
mod adapter_tests {
    #[tokio::test]
    async fn peer_directory_scripted_gate_holds_only_after_allowed_reads() {
        use std::future::Future;
        let central = open().await;
        central.boundary().set_directory_peers(Vec::new());
        central.boundary().block_op(FaultOp::PeerDirectory);
        central.boundary().set_directory_unblocked_reads(1);
        assert!(
            central
                .resolve_peer("first", OpControl::unbounded())
                .await
                .unwrap()
                .is_none()
        );
        let second = central.resolve_peer("second", OpControl::unbounded());
        tokio::pin!(second);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(second.as_mut().poll(&mut context).is_pending());
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| call.as_str() == "resolve_peer")
                .count(),
            2
        );
        central.boundary().unblock_all(FaultOp::PeerDirectory);
        assert!(second.await.unwrap().is_none());
        assert!(central.shutdown().await.is_released());
    }

    #[tokio::test]
    async fn peer_directory_capabilities_snapshot_is_instantiated_core_truth() {
        let central = open().await;
        assert_eq!(
            central.capability_states().await,
            central
                .with_core(|core| core.registered_capability_states())
                .await
        );
        assert!(central.shutdown().await.is_released());
    }
    #[test]
    fn peer_directory_uses_normal_radio_admission() {
        use crate::AdapterPowerState;
        for operation in ["peers.connected", "peers.resolve"] {
            for (power, code) in [
                (
                    AdapterPowerState::Unauthorized,
                    ubm_core::contracts::BleErrorCode::PermissionDenied,
                ),
                (
                    AdapterPowerState::PoweredOff,
                    ubm_core::contracts::BleErrorCode::AdapterPoweredOff,
                ),
            ] {
                let status = super::AdapterStatus {
                    power: Some(power),
                    authorization: None,
                    availability: super::AdapterAvailability::Available,
                    lost: false,
                };
                assert!(super::gated(
                    crate::boundary::AdmissionPolicy::CoreBluetooth,
                    operation
                ));
                assert_eq!(
                    super::admission_refusal(
                        crate::boundary::AdmissionPolicy::CoreBluetooth,
                        status,
                        operation
                    )
                    .unwrap()
                    .code(),
                    code
                );
            }
        }
    }
    #[tokio::test]
    async fn peer_directory_default_refuses_without_acquiring_a_link() {
        let central = open().await;
        let error = central
            .connected_peers(&["180d".to_owned()], OpControl::unbounded())
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            ubm_core::contracts::BleErrorCode::CapabilityUnsupported
        );
        let error = central
            .resolve_peer("unknown", OpControl::unbounded())
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            ubm_core::contracts::BleErrorCode::CapabilityUnsupported
        );
        assert!(central.peer_records().await.is_empty());
        assert!(central.shutdown().await.is_released());
    }

    #[tokio::test]
    async fn peer_directory_read_preserves_identity_without_owned_connection() {
        let central = open().await;
        let peer = crate::boundary::DirectoryPeer {
            peer_id: "00e2ce71-3ba4-6569-e3de-3081ce0c95fb".to_owned(),
            name: Some("SIM".to_owned()),
            connection: "connected",
        };
        central.boundary().set_directory_peers(vec![peer.clone()]);
        assert_eq!(
            central
                .connected_peers(
                    &["0000180d-0000-1000-8000-00805f9b34fb".to_owned()],
                    OpControl::unbounded()
                )
                .await
                .unwrap(),
            vec![peer.clone()]
        );
        let resolved = central
            .resolve_peer(&peer.peer_id, OpControl::unbounded())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.peer_id, peer.peer_id);
        assert_eq!(resolved.connection, "unknown");
        assert_eq!(
            central
                .resolve_peer("missing", OpControl::unbounded())
                .await
                .unwrap(),
            None
        );
        assert!(central.peer_records().await.is_empty());
        assert!(
            !central
                .boundary()
                .calls()
                .iter()
                .any(|call| call == "connect" || call == "disconnect")
        );
        assert!(central.shutdown().await.is_released());
    }

    #[tokio::test(start_paused = true)]
    async fn peer_directory_timeout_and_preabort_preserve_no_link_ownership() {
        use crate::{Budget, OpTicket};
        let central = open().await;
        central.boundary().set_directory_peers(Vec::new());
        central.boundary().block_op(FaultOp::PeerDirectory);
        let error = central
            .connected_peers(&[], OpControl::budget_ms(10))
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            ubm_core::contracts::BleErrorCode::OperationTimedOut
        );
        let before = central.boundary().calls();
        let ticket = OpTicket::new();
        ticket.request_cancel();
        let error = central
            .resolve_peer("peer", OpControl::new(Budget::unbounded(), ticket))
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            ubm_core::contracts::BleErrorCode::OperationAborted
        );
        assert_eq!(before, central.boundary().calls());
        central.boundary().unblock_all(FaultOp::PeerDirectory);
        assert!(central.peer_records().await.is_empty());
        assert!(central.shutdown().await.is_released());
    }

    #[tokio::test(start_paused = true)]
    async fn bonded_directory_reads_native_facts_without_link_ownership_and_honors_budget() {
        let central = open().await;
        let peer = crate::boundary::DirectoryPeer {
            peer_id: "bonded-peer".into(),
            name: Some("saved sensor".into()),
            connection: "disconnected",
        };
        central
            .boundary()
            .set_bonded_directory_peers(vec![peer.clone()]);
        assert_eq!(
            central.bonded_peers(OpControl::unbounded()).await.unwrap(),
            vec![peer]
        );
        assert!(central.peer_records().await.is_empty());
        assert!(
            !central
                .boundary()
                .calls()
                .iter()
                .any(|call| call == "connect" || call == "disconnect")
        );
        central.boundary().block_op(FaultOp::PeerDirectory);
        assert_eq!(
            central
                .bonded_peers(OpControl::budget_ms(10))
                .await
                .unwrap_err()
                .code(),
            ubm_core::contracts::BleErrorCode::OperationTimedOut
        );
        central.boundary().unblock_all(FaultOp::PeerDirectory);
        assert!(central.shutdown().await.is_released());
    }

    #[tokio::test]
    async fn peer_directory_held_lookup_is_retired_by_adapter_reset() {
        let central = open().await;
        central.boundary().set_directory_peers(Vec::new());
        central.boundary().block_op(FaultOp::PeerDirectory);
        let lookup = central.connected_peers(&[], OpControl::unbounded());
        let reset = async {
            central
                .boundary()
                .wait_for_calls("connected_peers", 1)
                .await;
            super::adapter_reset(
                &central.inner,
                crate::boundary::AdapterLossCause::DaemonRestarted,
                None,
                None,
            )
            .await;
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(lookup, reset)
        })
        .await
        .expect("reset must retire lookup");
        assert_eq!(
            result.unwrap_err().code(),
            ubm_core::contracts::BleErrorCode::OperationReset
        );
        central.boundary().unblock_all(FaultOp::PeerDirectory);
        assert!(central.peer_records().await.is_empty());
        assert!(central.shutdown().await.is_released());
    }

    #[tokio::test]
    async fn peer_directory_held_lookup_is_retired_by_shutdown() {
        let central = open().await;
        central.boundary().set_directory_peers(Vec::new());
        central.boundary().block_op(FaultOp::PeerDirectory);
        let lookup = central.resolve_peer("peer", OpControl::unbounded());
        let close = async {
            central.boundary().wait_for_calls("resolve_peer", 1).await;
            assert!(central.shutdown().await.is_released());
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(lookup, close)
        })
        .await
        .expect("shutdown must retire lookup");
        assert!(result.is_err());
        central.boundary().unblock_all(FaultOp::PeerDirectory);
        assert!(central.peer_records().await.is_empty());
    }
    use std::time::Duration;

    use ubm_core::central::{ConnectionState, ConsumerState, ScanSessionState};

    use crate::boundary::{
        CharacteristicSnapshot, DescriptorSnapshot, FakeRadio, FaultOp, PeerSnapshot,
        PropertyFlags, RadioEvent, ServiceSnapshot,
    };

    use super::{CentralProfile, DesktopCentral, services_changed_invalidated};
    use crate::errors::DesktopError;
    use crate::op_control::OpControl;
    use ubm_core::central::Central;
    use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

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
            primary: None,
            included_services: None,
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
            access: std::default::Default::default(),
        }
    }

    fn second_hrm_service() -> ServiceSnapshot {
        ServiceSnapshot {
            occurrence: 1,
            ..hrm_service()
        }
    }

    /// One service carrying two same-UUID notify characteristics (wrist +
    /// chest strap): occurrence is the only instance identity.
    fn duplicate_hrm_service() -> ServiceSnapshot {
        ServiceSnapshot {
            primary: None,
            included_services: None,
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
            access: std::default::Default::default(),
        }
    }

    fn battery_service() -> ServiceSnapshot {
        ServiceSnapshot {
            primary: None,
            included_services: None,
            uuid: BATTERY_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![CharacteristicSnapshot {
                uuid: BATTERY_LEVEL.to_owned(),
                occurrence: 0,
                properties: rw_props(),
                descriptors: Vec::new(),
            }],
            access: std::default::Default::default(),
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
        let first = central
            .start_scan("owner-a", &[], OpControl::budget_ms(5000))
            .await
            .expect("first");
        let error = central
            .start_scan("owner-b", &[], OpControl::budget_ms(5000))
            .await
            .expect_err("second owner rejected");
        assert_eq!(error.code_str(), "scan.already-active");
        // Finding 209: the refusal names the occupying scan.
        let detail = error.detail().expect("refusal names the occupant");
        assert!(
            detail.contains(&first.operation_id().to_string()),
            "detail names the occupying scan, got: {detail}"
        );
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
    async fn deferred_connect_routes_native_and_preserves_timeout_compensation() {
        let central = open().await;
        central
            .boundary()
            .push_event(advertisement("deferred-peer"));
        central.boundary().block_op(FaultOp::Connect);
        let error = central
            .connect_when_available("deferred-peer", "lease", OpControl::budget_ms(10))
            .await
            .unwrap_err();
        assert_eq!(error.code_str(), "connection.failed");
        assert_eq!(
            error.platform().expect("deadline cause").code,
            "deadline-expired"
        );
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| call.as_str() == "connect_when_available")
                .count(),
            1
        );
        central.boundary().unblock_op(FaultOp::Connect);
        central.shutdown().await;
    }

    #[tokio::test]
    async fn connect_shares_live_link_by_default() {
        // FX1B: sharing is the default — a second connect leases the live
        // link instead of failing `connection.already-owned`.
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
        let joined = central
            .connect("peer-1", "lease-b", OpControl::budget_ms(5000))
            .await
            .expect("second lease joins the live link");
        assert_eq!(
            joined.connection_generation, handle.connection_generation,
            "one link, one link generation, two leases"
        );
        let leases = central
            .with_core(|core| core.connection_lease_count(&handle.peer_key))
            .await;
        assert_eq!(leases, 2, "two leases on the shared link");
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

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_timeout_is_retried_by_shutdown() {
        use ubm_core::ownership::CleanupState;
        let central = open().await;
        central.boundary().block_op(FaultOp::Disconnect);
        central
            .boundary()
            .fail_next(FaultOp::Connect, "connect refused");
        assert!(
            central
                .connect("half-open", "a", OpControl::unbounded())
                .await
                .is_err()
        );
        let attempts = central
            .boundary()
            .calls()
            .iter()
            .filter(|call| call.as_str() == "disconnect")
            .count();
        assert_eq!(attempts, 1);
        let failed = central.shutdown().await;
        assert!(!failed.is_released());
        assert_eq!(failed.half_open_close_failures.len(), 1);
        assert_eq!(
            failed.half_open_close_failures[0].code_str(),
            "operation.timed-out"
        );
        assert_eq!(failed.record.unwrap().state(), CleanupState::Released);
        central.boundary().unblock_op(FaultOp::Disconnect);
        let released = central.shutdown().await;
        assert!(released.is_released());
        assert!(released.half_open_close_failures.is_empty());
        assert_eq!(released.record.unwrap().state(), CleanupState::Released);
        assert!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| call.as_str() == "disconnect")
                .count()
                > attempts
        );
        let disconnects = || {
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| call.as_str() == "disconnect")
                .count()
        };
        let settled_calls = disconnects();
        assert_eq!(
            central.shutdown().await.record.unwrap().state(),
            CleanupState::Released
        );
        assert_eq!(disconnects(), settled_calls);
        assert!(central.resource_counters().await.compensation_failures > 0);
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_refusal_retains_actual_cause_and_retries() {
        use ubm_core::ownership::CleanupState;
        let central = open().await;
        central
            .boundary()
            .fail_next(FaultOp::Connect, "original connect refusal");
        central
            .boundary()
            .fail_next(FaultOp::Disconnect, "actual cleanup refusal");
        assert_eq!(
            central
                .connect("half-open", "a", OpControl::unbounded())
                .await
                .unwrap_err()
                .code_str(),
            "connection.failed"
        );
        let debt = super::lock_std(&central.inner.half_open_cleanup)["half-open"].clone();
        let failure = super::lock_std(&debt.failure).clone().unwrap();
        assert_eq!(failure.code_str(), "connection.lost");
        assert_eq!(failure.detail(), Some("actual cleanup refusal"));
        central
            .boundary()
            .fail_next(FaultOp::Disconnect, "retry refused");
        let failed = central.shutdown().await;
        assert_eq!(failed.record.unwrap().state(), CleanupState::Released);
        assert_eq!(failed.half_open_close_failures.len(), 1);
        let reported = &failed.half_open_close_failures[0];
        assert_eq!(reported.code_str(), "connection.lost");
        assert_eq!(reported.detail(), Some("retry refused"));
        assert_eq!(reported.operation(), "connection.disconnect");
        assert_eq!(
            central.shutdown().await.record.unwrap().state(),
            CleanupState::Released
        );
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| call.as_str() == "disconnect")
                .count(),
            3
        );
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_retry_preserves_unrelated_cached_failure() {
        use super::BleErrorCode;
        let central = open().await;
        let other = central
            .connect("other", "owner", OpControl::unbounded())
            .await
            .unwrap();
        {
            let mut core = central.inner.core.lock().await;
            core.note_peer_loss(&other.peer_key, super::now_ms(), &mut super::batch())
                .unwrap();
            core.report_disconnect_failure(&other.peer_key, BleErrorCode::PlatformFailure)
                .unwrap();
        }
        central.boundary().block_op(FaultOp::Disconnect);
        central
            .boundary()
            .fail_next(FaultOp::Connect, "connect refused");
        assert!(
            central
                .connect("half-open", "a", OpControl::unbounded())
                .await
                .is_err()
        );
        let failed = central.shutdown().await;
        assert_eq!(
            failed.half_open_close_failures[0].code(),
            BleErrorCode::OperationTimedOut
        );
        central.boundary().unblock_op(FaultOp::Disconnect);
        let retried = central.shutdown().await.record.unwrap();
        assert_eq!(retried.failures().len(), 1);
        assert_eq!(retried.failures()[0].code(), BleErrorCode::PlatformFailure);
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_reset_epoch_is_published_with_generation() {
        let central = open().await;
        let link = central
            .connect("half-open", "a", OpControl::unbounded())
            .await
            .unwrap();
        let mut core = central.inner.core.lock().await;
        let epoch = central
            .inner
            .resets
            .load(std::sync::atomic::Ordering::SeqCst);
        let reset = super::adapter_reset(
            &central.inner,
            crate::boundary::AdapterLossCause::DaemonRestarted,
            None,
            None,
        );
        tokio::pin!(reset);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(reset.as_mut().poll(cx).is_pending()))
                .await
        );
        assert!(central.retain_half_open_cleanup(
            &core,
            "half-open",
            &link.peer_key,
            &link.connection_generation,
            epoch,
            super::confirmed_release_count(&central.inner, "half-open"),
        ));
        let debt = super::lock_std(&central.inner.half_open_cleanup)["half-open"].clone();
        assert_eq!(
            debt.adapter_epoch, epoch,
            "a pending reset cannot publish its epoch before its generation transition"
        );
        core.note_peer_loss(&link.peer_key, super::now_ms(), &mut super::batch())
            .unwrap();
        drop(core);
        reset.await;
        let before = central.boundary().calls();
        central
            .retry_half_open_cleanup("half-open", &debt)
            .await
            .unwrap();
        assert_eq!(central.boundary().calls(), before);
        central.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_confirmed_loss_and_reset_retire_only_old_debt() {
        for reset in [false, true] {
            let central = open().await;
            central.boundary().block_op(FaultOp::Disconnect);
            central
                .boundary()
                .fail_next(FaultOp::Connect, "connect refused");
            assert!(
                central
                    .connect("half-open", "a", OpControl::unbounded())
                    .await
                    .is_err()
            );
            let old = super::lock_std(&central.inner.half_open_cleanup)["half-open"].clone();
            if reset {
                super::adapter_reset(
                    &central.inner,
                    crate::boundary::AdapterLossCause::DaemonRestarted,
                    None,
                    None,
                )
                .await;
            } else {
                super::reconcile_disconnected(&central.inner, "half-open", true).await;
            }
            central.boundary().unblock_op(FaultOp::Disconnect);
            let before = central
                .boundary()
                .calls()
                .iter()
                .filter(|call| call.as_str() == "disconnect")
                .count();
            let new = central
                .connect("half-open", "b", OpControl::unbounded())
                .await
                .unwrap();
            assert_ne!(new.connection_generation, old.generation);
            central
                .retry_half_open_cleanup("half-open", &old)
                .await
                .unwrap();
            assert_eq!(
                central
                    .boundary()
                    .calls()
                    .iter()
                    .filter(|call| call.as_str() == "disconnect")
                    .count(),
                before
            );
            assert!(central.boundary().link_connected("half-open"));
            central.shutdown().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_concurrent_acquisition_cannot_publish_lost_generation() {
        for explicit_cancel in [false, true] {
            let central = open().await;
            central.boundary().block_op(FaultOp::Connect);
            central.boundary().block_op(FaultOp::Disconnect);
            let ctl = if explicit_cancel {
                OpControl::unbounded()
            } else {
                OpControl::budget_ms(10)
            };
            let ticket = ctl.ticket.clone();
            let mut first = Box::pin(central.connect("peer", "a", ctl));
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(
                    first.as_mut().poll(cx).is_pending()
                ))
                .await
            );
            let generation = central
                .with_core(|core| core.connection_generation("platform-guid:peer"))
                .await;
            let mut second = Box::pin(central.connect("peer", "b", OpControl::unbounded()));
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(
                    second.as_mut().poll(cx).is_pending()
                ))
                .await
            );
            assert_eq!(
                central
                    .boundary()
                    .calls()
                    .iter()
                    .filter(|call| call.as_str() == "connect")
                    .count(),
                2,
                "both native acquisitions admitted"
            );
            assert_eq!(
                central
                    .with_core(|core| core.connection_generation("platform-guid:peer"))
                    .await,
                generation
            );
            if explicit_cancel {
                central.cancel(&ticket).await.unwrap();
            } else {
                tokio::time::advance(Duration::from_millis(10)).await;
            }
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(
                    first.as_mut().poll(cx).is_pending()
                ))
                .await
            );
            central.boundary().unblock_all(FaultOp::Connect);
            let second_state =
                std::future::poll_fn(|cx| std::task::Poll::Ready(second.as_mut().poll(cx))).await;
            assert!(
                !matches!(&second_state, std::task::Poll::Ready(Ok(_))),
                "B cannot return an unusable handle: {second_state:?}"
            );
            central.boundary().unblock_op(FaultOp::Disconnect);
            let a = first.await;
            let b = match second_state {
                std::task::Poll::Ready(result) => result,
                std::task::Poll::Pending => second.await,
            };
            assert!(a.is_err());
            assert_eq!(b.unwrap_err().code_str(), "connection.stale");
            assert!(!central.boundary().link_connected("peer"));
            assert!(central.shutdown().await.is_released());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_refused_overlap_keeps_other_peer_usable() {
        let central = open().await;
        ready_peer(&central, "other", vec![hrm_service()]).await;
        central.boundary().block_op(FaultOp::Connect);
        central.boundary().block_op(FaultOp::Disconnect);
        let mut b = Box::pin(central.connect("peer", "b", OpControl::unbounded()));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(b.as_mut().poll(cx).is_pending()))
                .await
        );
        central
            .boundary()
            .fail_next(FaultOp::Connect, "ordinary A refusal");
        let mut a = Box::pin(central.connect("peer", "a", OpControl::unbounded()));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(a.as_mut().poll(cx).is_pending()))
                .await
        );
        assert_eq!(
            central
                .with_core(|core| core.connection_state("platform-guid:peer"))
                .await,
            Some(super::ConnectionState::Lost)
        );
        central.boundary().unblock_all(FaultOp::Connect);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(b.as_mut().poll(cx).is_pending()))
                .await
        );
        central.boundary().unblock_op(FaultOp::Disconnect);
        let (a, b) = tokio::join!(a, b);
        assert_eq!(a.unwrap_err().code_str(), "connection.failed");
        assert_eq!(b.unwrap_err().code_str(), "connection.stale");
        assert!(!central.boundary().link_connected("peer"));
        assert!(central.boundary().link_connected("other"));
        assert_eq!(
            central
                .read("other", &hrm_selector(0), OpControl::unbounded())
                .await
                .unwrap()
                .value,
            vec![0x42]
        );
        assert!(central.shutdown().await.is_released());
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_later_acquisition_refreshes_retained_same_generation_debt() {
        for held_cleanup in [false, true] {
            let central = open().await;
            let link = central
                .connect("peer", "a", OpControl::unbounded())
                .await
                .unwrap();
            {
                let mut core = central.inner.core.lock().await;
                assert!(central.retain_half_open_cleanup(
                    &core,
                    "peer",
                    &link.peer_key,
                    &link.connection_generation,
                    0,
                    0
                ));
                core.note_peer_loss(&link.peer_key, super::now_ms(), &mut super::batch())
                    .unwrap();
            }
            let original_debt = super::lock_std(&central.inner.half_open_cleanup)["peer"].clone();
            let cleanup = central.retry_half_open_cleanup("peer", &original_debt);
            tokio::pin!(cleanup);
            if held_cleanup {
                central.boundary().block_op(FaultOp::Disconnect);
                assert!(
                    std::future::poll_fn(|cx| std::task::Poll::Ready(
                        cleanup.as_mut().poll(cx).is_pending()
                    ))
                    .await
                );
            }
            super::reconcile_disconnected(&central.inner, "peer", true).await;
            crate::boundary::RadioBoundary::connect(central.boundary(), "peer")
                .await
                .unwrap();
            let serial = super::confirmed_release_count(&central.inner, "peer");
            {
                let core = central.inner.core.lock().await;
                assert!(central.retain_half_open_cleanup(
                    &core,
                    "peer",
                    &link.peer_key,
                    &link.connection_generation,
                    0,
                    serial
                ));
            }
            assert!(std::sync::Arc::ptr_eq(
                &original_debt,
                &super::lock_std(&central.inner.half_open_cleanup)["peer"]
            ));
            if held_cleanup {
                central.boundary().unblock_op(FaultOp::Disconnect);
                cleanup.await.unwrap();
            }
            central
                .compensate_half_open("peer", &link.connection_generation)
                .await;
            assert!(
                !central.boundary().link_connected("peer"),
                "newly acquired physical link cannot inherit the old retired receipt"
            );
            assert!(super::lock_std(&central.inner.half_open_cleanup).is_empty());
            central.shutdown().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_drop_after_late_native_success_still_compensates() {
        let central = open().await;
        central.boundary().block_op(FaultOp::Connect);
        let mut pending = Box::pin(central.connect("late", "a", OpControl::unbounded()));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                pending.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        central.shutdown().await;
        let core = central.inner.core.lock().await;
        central.boundary().unblock_op(FaultOp::Connect);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                pending.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        assert!(
            central.boundary().link_connected("late"),
            "native success precedes blocked core settlement"
        );
        drop(pending);
        drop(core);
        tokio::time::timeout(
            Duration::from_secs(2),
            central.boundary().wait_for_calls("disconnect", 2),
        )
        .await
        .expect("late acquisition cleanup is dispatched");
        assert!(
            !central.boundary().link_connected("late"),
            "dropped late acquisition must remain cleanup-owned"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_delayed_drop_does_not_reopen_confirmed_shutdown_release() {
        for os_loss in [false, true] {
            let central = open().await;
            let old = central
                .connect("peer", "a", OpControl::unbounded())
                .await
                .unwrap();
            let operation = {
                let mut core = central.inner.core.lock().await;
                core.connect(
                    &old.peer_key,
                    "pending",
                    1_000,
                    super::now_ms(),
                    &mut super::batch(),
                )
                .unwrap()
            };
            if os_loss {
                crate::boundary::RadioBoundary::disconnect(central.boundary(), "peer")
                    .await
                    .unwrap();
                super::reconcile_disconnected(&central.inner, "peer", true).await;
            } else {
                let report = central.shutdown().await;
                assert!(report.half_open_close_failures.is_empty());
            }
            let before = central.boundary().calls();
            central
                .boundary()
                .fail_next(FaultOp::Disconnect, "duplicate release must not run");
            super::run_drop_cleanup(
                central.clone(),
                operation.clone(),
                super::DropCleanup::Connect {
                    peer_id: "peer".to_owned(),
                    peer_key: old.peer_key.clone(),
                    generation: old.connection_generation.clone(),
                    adapter_epoch: 0,
                    release_serial: 0,
                },
            )
            .await;
            assert_eq!(central.boundary().calls(), before);
            assert!(super::lock_std(&central.inner.half_open_cleanup).is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_old_drop_guard_cannot_retain_or_disconnect_new_generation() {
        let central = open().await;
        let old = central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        let operation = central
            .inner
            .core
            .lock()
            .await
            .connect(
                &old.peer_key,
                "old-pending",
                1000,
                super::now_ms(),
                &mut super::batch(),
            )
            .unwrap();
        super::reconcile_disconnected(&central.inner, "peer", true).await;
        let new = central
            .connect("peer", "b", OpControl::unbounded())
            .await
            .unwrap();
        assert_ne!(old.connection_generation, new.connection_generation);
        let before = central.boundary().calls();
        super::run_drop_cleanup(
            central.clone(),
            operation,
            super::DropCleanup::Connect {
                peer_id: "peer".to_owned(),
                peer_key: old.peer_key,
                generation: old.connection_generation,
                adapter_epoch: 0,
                release_serial: 0,
            },
        )
        .await;
        assert_eq!(central.boundary().calls(), before);
        assert!(central.boundary().link_connected("peer"));
        assert!(super::lock_std(&central.inner.half_open_cleanup).is_empty());
        central.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_wait_is_abort_aware_without_new_connect() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        central.boundary().block_op(FaultOp::Disconnect);
        central
            .boundary()
            .fail_next(FaultOp::Connect, "connect refused");
        assert!(
            central
                .connect("peer", "a", OpControl::unbounded())
                .await
                .is_err()
        );
        let ctl = OpControl::unbounded();
        let ticket = ctl.ticket.clone();
        let next = central.connect("peer", "b", ctl);
        tokio::pin!(next);
        assert!(std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx).is_pending())).await);
        central.cancel(&ticket).await.unwrap();
        assert_eq!(next.await.unwrap_err().code_str(), "operation.aborted");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| call.as_str() == "connect")
                .count(),
            1
        );
        assert!(!super::lock_std(&central.inner.half_open_cleanup).is_empty());
        central.boundary().unblock_op(FaultOp::Disconnect);
        central.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_cleanup_wait_honors_new_connect_budget_and_preserves_other_peer() {
        let central = open().await;
        ready_peer(&central, "other", vec![hrm_service()]).await;
        central.boundary().block_op(FaultOp::Disconnect);
        central
            .boundary()
            .fail_next(FaultOp::Connect, "connect refused");
        assert!(
            central
                .connect("half-open", "a", OpControl::unbounded())
                .await
                .is_err()
        );
        let count = central
            .boundary()
            .calls()
            .iter()
            .filter(|call| call.as_str() == "connect")
            .count();
        let before = tokio::time::Instant::now();
        let error = central
            .connect("half-open", "b", OpControl::budget_ms(5))
            .await
            .unwrap_err();
        assert_eq!(error.code_str(), "operation.timed-out");
        assert!(before.elapsed() < Duration::from_millis(100));
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| call.as_str() == "connect")
                .count(),
            count
        );
        assert_eq!(
            central
                .read("other", &hrm_selector(0), OpControl::unbounded())
                .await
                .unwrap()
                .value,
            vec![0x42]
        );
        central.boundary().unblock_op(FaultOp::Disconnect);
        let new = central
            .connect("half-open", "b", OpControl::unbounded())
            .await
            .unwrap();
        assert!(new.connection_generation.is_some());
        assert!(super::lock_std(&central.inner.half_open_cleanup).is_empty());
        assert!(central.boundary().link_connected("half-open"));
        central.shutdown().await;
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
    /// Finding 161: a connect whose deadline expires before any link came
    /// up is `connection.failed` (caller-decides) on every host — the same
    /// physical event as the controller giving up (Android GATT 133/147),
    /// which CoreBluetooth, btleplug and Web never report on their own.
    /// The deadline fact rides in `platform`. A caller-supplied AbortSignal
    /// abort stays `operation.aborted`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_connect_deadline_without_a_link_is_connection_failed() {
        let central = open().await;
        central.boundary().block_op(FaultOp::Connect);
        let error = central
            .connect("peer-161", "lease-a", OpControl::budget_ms(50))
            .await
            .expect_err("no link came up before the deadline");
        assert_eq!(error.code_str(), "connection.failed");
        assert_eq!(
            error.retryability(),
            crate::errors::Retryability::CallerDecides
        );
        assert_eq!(error.operation(), "connection.connect");
        let platform = error.platform().expect("the deadline fact rides the error");
        assert_eq!(platform.domain, "core");
        assert_eq!(platform.code, "deadline-expired");
        assert!(
            platform
                .message
                .as_deref()
                .is_some_and(|message| message.contains("deadline")),
            "the deadline fact is kept"
        );
        assert!(
            matches!(
                platform.metadata.get("deadlineMs"),
                Some(crate::errors::PlatformValue::Int(ms)) if *ms > 0
            ),
            "the bound rides the error"
        );
        central.boundary().unblock_op(FaultOp::Connect);
    }

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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_old_physical_loss_does_not_invalidate_new_connection() {
        let central = open().await;
        central
            .boundary()
            .push_event(advertisement("lease-generation"));
        wait_peer(&central, "lease-generation").await;
        let handle = central
            .connect("lease-generation", "lease", OpControl::budget_ms(5000))
            .await
            .unwrap();
        central
            .boundary()
            .set_services("lease-generation", vec![hrm_service()]);
        central
            .discover("lease-generation", "lease", OpControl::budget_ms(5000))
            .await
            .unwrap();
        central
            .boundary()
            .set_physical_generation("lease-generation", 74);
        super::reconcile_disconnected_scoped(
            &central.inner,
            "lease-generation",
            false,
            Some((73, 2)),
        )
        .await;
        assert_eq!(
            central
                .with_core(|core| core.connection_state(&handle.peer_key))
                .await,
            Some(ConnectionState::Connected)
        );
        assert!(
            central
                .with_core(|core| core.holds_lease(&handle.peer_key, "lease"))
                .await
        );
        assert_eq!(
            central
                .with_core(|core| core.database_state(&handle.peer_key))
                .await,
            Some(ubm_core::central::DatabaseState::Current),
            "old physical loss preserves current GATT"
        );
        let mut events = central.lifecycle_events();
        super::reconcile_disconnected_scoped(
            &central.inner,
            "lease-generation",
            false,
            Some((74, 2)),
        )
        .await;
        let event = events.recv().await.unwrap();
        assert_eq!(event.kind, super::LifecycleKind::LinkLost);
        let platform = event
            .platform
            .as_ref()
            .expect("native reason survives admitted observation");
        assert_eq!(platform.domain, "bluez-mgmt");
        assert_eq!(platform.code, "2");
        assert_eq!(
            platform.metadata.get("disconnectReason"),
            Some(&crate::errors::PlatformValue::Int(2))
        );
        assert_ne!(
            central
                .with_core(|core| core.database_state(&handle.peer_key))
                .await,
            Some(ubm_core::central::DatabaseState::Current),
            "current physical loss invalidates GATT"
        );
        assert_eq!(
            central
                .disconnect("lease-generation", "lease", OpControl::budget_ms(5000))
                .await
                .unwrap(),
            super::LinkRelease::AlreadyReleased,
            "the public lease is retired by observed loss, not by deleting core history"
        );
        super::reconcile_disconnected_scoped(
            &central.inner,
            "lease-generation",
            false,
            Some((74, 2)),
        )
        .await;
        assert!(
            events.try_recv().is_err(),
            "duplicate loss publishes no second transition"
        );
    }

    #[tokio::test]
    async fn old_release_answer_cannot_clear_new_connection_routing_or_gatt() {
        let central = open().await;
        central
            .boundary()
            .push_event(advertisement("release-reconnect"));
        central
            .connect("release-reconnect", "old", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().block_op(FaultOp::Disconnect);
        let owner = central.clone();
        let old = tokio::spawn(async move {
            owner
                .disconnect("release-reconnect", "old", OpControl::unbounded())
                .await
        });
        central.boundary().wait_for_calls("disconnect", 1).await;
        super::reconcile_disconnected(&central.inner, "release-reconnect", false).await;
        let handle = central
            .connect("release-reconnect", "new", OpControl::unbounded())
            .await
            .unwrap();
        central
            .boundary()
            .set_services("release-reconnect", vec![hrm_service()]);
        central
            .discover("release-reconnect", "new", OpControl::unbounded())
            .await
            .unwrap();
        let epoch = central
            .inner
            .epochs
            .lock()
            .await
            .get("release-reconnect")
            .copied();
        let mut events = central.lifecycle_events();
        central.boundary().unblock_op(FaultOp::Disconnect);
        old.await.unwrap().unwrap();
        assert_eq!(
            central
                .inner
                .epochs
                .lock()
                .await
                .get("release-reconnect")
                .copied(),
            epoch,
            "old answer cannot advance newer routing epoch"
        );
        assert_eq!(
            central
                .with_core(|core| core.connection_state(&handle.peer_key))
                .await,
            Some(ConnectionState::Connected)
        );
        assert_eq!(
            central
                .with_core(|core| core.database_state(&handle.peer_key))
                .await,
            Some(ubm_core::central::DatabaseState::Current)
        );
        assert!(
            events.try_recv().is_err(),
            "old answer cannot emit a newer-generation release"
        );
    }

    #[tokio::test]
    async fn old_release_answer_cannot_settle_newer_disconnecting_generation() {
        let central = open().await;
        central
            .boundary()
            .push_event(advertisement("release-newer-pending"));
        central
            .connect("release-newer-pending", "old", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().block_op(FaultOp::Disconnect);
        let owner = central.clone();
        let old = tokio::spawn(async move {
            owner
                .disconnect("release-newer-pending", "old", OpControl::unbounded())
                .await
        });
        central.boundary().wait_for_calls("disconnect", 1).await;
        super::reconcile_disconnected(&central.inner, "release-newer-pending", false).await;
        let handle = central
            .connect("release-newer-pending", "new", OpControl::unbounded())
            .await
            .unwrap();
        {
            let mut core = central.inner.core.lock().await;
            core.disconnect(
                &handle.peer_key,
                "new",
                super::now_ms(),
                &mut super::batch(),
            )
            .unwrap();
        }
        let mut events = central.lifecycle_events();
        central.boundary().unblock_op(FaultOp::Disconnect);
        old.await.unwrap().unwrap();
        assert_eq!(
            central
                .with_core(|core| core.connection_state(&handle.peer_key))
                .await,
            Some(ConnectionState::Disconnecting),
            "old native answer cannot settle a newer disconnect request"
        );
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancelled_local_release_stage_retry_preserves_original_native_answer() {
        let central = open().await;
        central
            .boundary()
            .push_event(advertisement("release-stage"));
        central
            .connect("release-stage", "lease", OpControl::unbounded())
            .await
            .unwrap();
        let platform = crate::boundary::bluez_disconnect_observation(2);
        central
            .boundary()
            .set_disconnect_observation("release-stage", platform.clone());
        let held = central.inner.subscriptions.lock().await;
        let mut events = central.lifecycle_events();
        let first_owner = central.clone();
        let first = tokio::spawn(async move {
            first_owner
                .disconnect("release-stage", "lease", OpControl::unbounded())
                .await
        });
        central.boundary().wait_for_calls("disconnect", 1).await;
        tokio::task::yield_now().await;
        first.abort();
        let _ = first.await;
        let retry_owner = central.clone();
        let retry = tokio::spawn(async move {
            retry_owner
                .disconnect("release-stage", "lease", OpControl::unbounded())
                .await
        });
        central.boundary().wait_for_calls("disconnect", 2).await;
        drop(held);
        retry.await.unwrap().unwrap();
        assert_eq!(events.recv().await.unwrap().platform, Some(platform));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn retired_release_report_preserves_original_generation_across_reset_and_replacement() {
        let central = open().await;
        central
            .boundary()
            .push_event(advertisement("retired-report"));
        let original = central
            .connect("retired-report", "old", OpControl::unbounded())
            .await
            .unwrap();
        let _held_events = central.lifecycle_events();
        super::adapter_reset(
            &central.inner,
            crate::boundary::AdapterLossCause::PoweredOff,
            None,
            None,
        )
        .await;
        assert_eq!(
            central
                .with_core(|core| core.connection_state(&original.peer_key))
                .await,
            None
        );
        let replacement = central
            .connect("retired-report", "new", OpControl::unbounded())
            .await
            .unwrap();
        assert_ne!(
            replacement.connection_generation,
            original.connection_generation
        );
        let calls = central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "disconnect")
            .count();
        let report = central
            .release_connection_lease_report("retired-report", "old", OpControl::unbounded())
            .await
            .unwrap();
        assert_eq!(report.connection_generation, original.connection_generation);
        assert_eq!(report.platform, None);
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "disconnect")
                .count(),
            calls
        );
        assert_eq!(
            central
                .with_core(|core| core.connection_state(&replacement.peer_key))
                .await,
            Some(ConnectionState::Connected)
        );
    }

    #[tokio::test]
    async fn own_lease_release_report_does_not_depend_on_lifecycle_consumer_delivery() {
        let central = open().await;
        central
            .boundary()
            .push_event(advertisement("release-report"));
        let connected = central
            .connect("release-report", "lease", OpControl::unbounded())
            .await
            .unwrap();
        let platform = crate::boundary::bluez_disconnect_observation(2);
        central
            .boundary()
            .set_disconnect_observation("release-report", platform.clone());
        let _held_events = central.lifecycle_events();
        let report = central
            .release_connection_lease_report("release-report", "lease", OpControl::unbounded())
            .await
            .unwrap();
        assert!(report.physical);
        assert_eq!(
            report.connection_generation,
            connected.connection_generation
        );
        assert_eq!(report.platform, Some(platform));
    }

    #[tokio::test]
    async fn requested_release_publishes_its_own_native_observation_once() {
        let central = open().await;
        central
            .boundary()
            .push_event(advertisement("release-observation"));
        central
            .connect("release-observation", "lease", OpControl::unbounded())
            .await
            .unwrap();
        let platform = crate::errors::PlatformDetail::new("bluez-mgmt", "2")
            .with_metadata("disconnectReason", crate::errors::PlatformValue::Int(2));
        central
            .boundary()
            .set_disconnect_observation("release-observation", platform.clone());
        let mut events = central.lifecycle_events();
        central
            .disconnect("release-observation", "lease", OpControl::unbounded())
            .await
            .unwrap();
        let event = events.recv().await.unwrap();
        assert_eq!(
            event.kind,
            super::LifecycleKind::Released { requested: true }
        );
        assert_eq!(event.platform, Some(platform));
        super::reconcile_disconnected(&central.inner, "release-observation", false).await;
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn requested_release_event_first_preserves_same_observation_once() {
        let central = open().await;
        central
            .boundary()
            .push_event(advertisement("release-event-first"));
        central
            .connect("release-event-first", "lease", OpControl::unbounded())
            .await
            .unwrap();
        let platform = crate::boundary::bluez_disconnect_observation(2);
        central
            .boundary()
            .set_disconnect_observation("release-event-first", platform.clone());
        central
            .boundary()
            .set_physical_generation("release-event-first", 73);
        central.boundary().block_op(FaultOp::Disconnect);
        let mut events = central.lifecycle_events();
        let owner = central.clone();
        let release = tokio::spawn(async move {
            owner
                .disconnect("release-event-first", "lease", OpControl::unbounded())
                .await
        });
        central.boundary().wait_for_calls("disconnect", 1).await;
        super::reconcile_disconnected_scoped(
            &central.inner,
            "release-event-first",
            false,
            Some((73, 2)),
        )
        .await;
        let event = events.recv().await.unwrap();
        assert_eq!(
            event.kind,
            super::LifecycleKind::Released { requested: true }
        );
        assert_eq!(event.platform, Some(platform));
        central.boundary().unblock_op(FaultOp::Disconnect);
        assert_eq!(
            release.await.unwrap().unwrap(),
            super::LinkRelease::Released
        );
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn lifecycle_platform_detail_is_published_with_the_original_transition() {
        let central = open().await;
        let mut events = central.lifecycle_events();
        let platform = crate::errors::PlatformDetail::new("bluez-mgmt", "1")
            .with_metadata("disconnectReason", crate::errors::PlatformValue::Int(1));
        let staged = central.inner.stage_lifecycle_with_platform(
            "peer",
            "peer-key",
            super::Generations {
                connection: None,
                database: None,
            },
            super::LifecycleKind::LinkLost,
            Some(platform.clone()),
        );
        assert_eq!(staged.platform, Some(platform.clone()));
        assert_eq!(events.try_recv().unwrap().platform, Some(platform));
        let unavailable = central.inner.stage_lifecycle(
            "peer",
            "peer-key",
            super::Generations {
                connection: None,
                database: None,
            },
            super::LifecycleKind::LinkLost,
        );
        assert!(unavailable.platform.is_none());
        assert!(events.try_recv().unwrap().platform.is_none());
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
    async fn shared_discovery_joins_without_replacing_the_first_owners_paths() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        for lease in ["public", "native"] {
            central
                .connect("peer-shared", lease, OpControl::unbounded())
                .await
                .unwrap();
        }
        central
            .boundary()
            .set_services("peer-shared", vec![hrm_service()]);
        central.boundary().block_op(FaultOp::Discover);
        let first = central.discover("peer-shared", "public", OpControl::unbounded());
        tokio::pin!(first);
        assert!(std::future::poll_fn(|cx| Poll::Ready(first.as_mut().poll(cx).is_pending())).await);
        let second = central.discover("peer-shared", "native", OpControl::unbounded());
        tokio::pin!(second);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx).is_pending())).await,
            "a concurrent live lease joins discovery instead of failing discovery.state"
        );
        central.boundary().unblock_op(FaultOp::Discover);
        let (a, b) = tokio::join!(first, second);
        assert_eq!(a.unwrap().paths_registered, b.unwrap().paths_registered);
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "discover")
                .count(),
            1
        );
        let key = central.peer_key_for("peer-shared").await.unwrap();
        let generation = central
            .with_core(|core| core.database_generation(&key))
            .await;
        central
            .connect("peer-shared", "third", OpControl::unbounded())
            .await
            .unwrap();
        central
            .discover("peer-shared", "third", OpControl::unbounded())
            .await
            .unwrap();
        assert_eq!(
            central
                .with_core(|core| core.database_generation(&key))
                .await,
            generation,
            "a new lease attaches to the immutable current physical snapshot"
        );
        assert!(
            !central
                .release_connection_lease("peer-shared", "public", OpControl::unbounded())
                .await
                .unwrap()
        );
        central
            .with_core(|core| {
                let path = core.resolve_path(&key, &hrm_selector(0)).unwrap();
                assert!(
                    core.holds_lease(&key, core.stored_path(path).unwrap().owner_lease()),
                    "physical path operation ownership must remain attached to a live lease"
                );
            })
            .await;
        assert_eq!(
            central
                .read("peer-shared", &hrm_selector(0), OpControl::unbounded())
                .await
                .unwrap()
                .value,
            vec![0x42]
        );
        central.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn shared_discovery_cancellation_and_deadline_are_per_caller() {
        use std::{future::Future, task::Poll};
        for interrupt_leader in [false, true] {
            for deadline in [false, true] {
                let central = open().await;
                for lease in ["a", "b"] {
                    central
                        .connect("peer", lease, OpControl::unbounded())
                        .await
                        .unwrap();
                }
                central.boundary().set_services("peer", vec![hrm_service()]);
                central.boundary().block_op(FaultOp::Discover);
                let a = OpControl::budget_ms(if interrupt_leader { 100 } else { 1000 });
                let b = OpControl::budget_ms(if interrupt_leader { 1000 } else { 100 });
                let ticket = if interrupt_leader {
                    a.ticket.clone()
                } else {
                    b.ticket.clone()
                };
                let first = central.discover("peer", "a", a);
                let second = central.discover("peer", "b", b);
                tokio::pin!(first, second);
                assert!(
                    std::future::poll_fn(|cx| Poll::Ready(first.as_mut().poll(cx).is_pending()))
                        .await
                );
                assert!(
                    std::future::poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx).is_pending()))
                        .await
                );
                if deadline {
                    tokio::time::advance(Duration::from_millis(101)).await;
                } else {
                    central.cancel(&ticket).await.unwrap();
                }
                let error = if interrupt_leader {
                    first.as_mut().await.unwrap_err()
                } else {
                    second.as_mut().await.unwrap_err()
                };
                assert_eq!(
                    error.code_str(),
                    if deadline {
                        "operation.timed-out"
                    } else {
                        "operation.aborted"
                    }
                );
                assert_eq!(error.retryability(), crate::Retryability::CallerDecides);
                if !interrupt_leader {
                    assert_eq!(
                        error.commit(),
                        Some(ubm_core::contracts::CommitState::NotDispatched)
                    );
                }
                central.boundary().unblock_op(FaultOp::Discover);
                if interrupt_leader {
                    second.await.unwrap();
                } else {
                    first.await.unwrap();
                }
                assert_eq!(
                    central
                        .read("peer", &hrm_selector(0), OpControl::unbounded())
                        .await
                        .unwrap()
                        .value,
                    vec![0x42]
                );
                central.shutdown().await;
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shared_discovery_released_leader_cannot_publish_over_surviving_owner() {
        use std::{future::Future, task::Poll};
        for queued in [false, true] {
            let central = open().await;
            for lease in ["a", "b"] {
                central
                    .connect("peer", lease, OpControl::unbounded())
                    .await
                    .unwrap();
            }
            central.boundary().set_services("peer", vec![hrm_service()]);
            central.boundary().block_op(FaultOp::Discover);
            let leader = central.discover("peer", "a", OpControl::unbounded());
            tokio::pin!(leader);
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(leader.as_mut().poll(cx).is_pending())).await
            );
            let follower = central.discover("peer", "b", OpControl::unbounded());
            tokio::pin!(follower);
            if queued {
                assert!(
                    std::future::poll_fn(|cx| Poll::Ready(follower.as_mut().poll(cx).is_pending()))
                        .await
                );
            }
            assert!(
                !central
                    .release_connection_lease("peer", "a", OpControl::unbounded())
                    .await
                    .unwrap()
            );
            central.boundary().unblock_op(FaultOp::Discover);
            assert_eq!(leader.await.unwrap_err().code_str(), "ownership.denied");
            follower.await.unwrap();
            assert_eq!(
                central
                    .read(
                        "peer",
                        &hrm_selector(0),
                        OpControl::unbounded().with_connection_lease("b".to_owned())
                    )
                    .await
                    .unwrap()
                    .value,
                vec![0x42]
            );
            central.shutdown().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shared_lease_release_retires_exact_old_generation_consumer_without_disabling_replacement()
     {
        use std::{future::Future, task::Poll};
        for drain_first in [false, true] {
            let central = open().await;
            for lease in ["a", "b"] {
                central
                    .connect("peer", lease, OpControl::unbounded())
                    .await
                    .unwrap();
            }
            central.boundary().set_services("peer", vec![hrm_service()]);
            central
                .discover("peer", "a", OpControl::unbounded())
                .await
                .unwrap();
            central
                .subscribe(
                    "peer",
                    &hrm_selector(0),
                    "old-a",
                    None,
                    OpControl::unbounded().with_connection_lease("a".to_owned()),
                )
                .await
                .unwrap();
            let peer_key = central.peer_key_for("peer").await.unwrap();
            {
                let mut core = central.inner.core.lock().await;
                let path = core.resolve_path(&peer_key, &hrm_selector(0)).unwrap();
                core.deliver_notification_value(path, &[0x73]).unwrap();
            }
            central.boundary().block_op(FaultOp::Read);
            let held_selector = hrm_selector(0);
            let held_read = central.read(
                "peer",
                &held_selector,
                OpControl::unbounded().with_connection_lease("a".to_owned()),
            );
            tokio::pin!(held_read);
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(held_read.as_mut().poll(cx).is_pending()))
                    .await
            );
            let mut lifecycle = central.lifecycle_events();
            central
                .boundary()
                .push_event(RadioEvent::ServicesChanged("peer".into()));
            lifecycle.recv().await.unwrap();
            central
                .discover("peer", "b", OpControl::unbounded())
                .await
                .unwrap();
            central
                .subscribe(
                    "peer",
                    &hrm_selector(0),
                    "new-b",
                    None,
                    OpControl::unbounded().with_connection_lease("b".to_owned()),
                )
                .await
                .unwrap();
            let before = central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count();
            if drain_first {
                let observed = std::sync::Mutex::new(Vec::new());
                let drain = |item| {
                    if let super::NotificationPoll::Value(bytes) = item {
                        super::lock_std(&observed).push(bytes);
                    }
                };
                central
                    .unsubscribe_draining(
                        "peer",
                        &hrm_selector(0),
                        "old-a",
                        OpControl::unbounded(),
                        Some(&drain),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    *super::lock_std(&observed),
                    vec![vec![0x73]],
                    "old accepted FIFO belongs to its handoff owner"
                );
            }
            assert!(
                !central
                    .release_connection_lease("peer", "a", OpControl::unbounded())
                    .await
                    .unwrap()
            );
            let key = central.peer_key_for("peer").await.unwrap();
            assert!(
                central
                    .with_core(|core| core.consumers_for_lease(&key, "a"))
                    .await
                    .is_empty(),
                "old consumer cannot disappear behind current-path resolution"
            );
            assert_eq!(
                central
                    .boundary()
                    .calls()
                    .iter()
                    .filter(|call| *call == "set_notifications")
                    .count(),
                before,
                "a new generation's enablement belongs to B"
            );
            central.boundary().unblock_op(FaultOp::Read);
            assert!(held_read.await.is_err());
            central.shutdown().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shared_lease_release_keeps_overflowed_child_cleanup_after_consumer_record_retirement()
    {
        let central = open().await;
        for lease in ["a", "b"] {
            central
                .connect("peer", lease, OpControl::unbounded())
                .await
                .unwrap();
        }
        central.boundary().set_services("peer", vec![hrm_service()]);
        central
            .discover("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central
            .subscribe_buffered(
                "peer",
                &hrm_selector(0),
                "consumer-a",
                None,
                ubm_core::streams::OverflowPolicy::Error,
                (64, 8192),
                OpControl::unbounded().with_connection_lease("a".to_owned()),
            )
            .await
            .unwrap();
        let key = central.peer_key_for("peer").await.unwrap();
        {
            let mut core = central.inner.core.lock().await;
            let path = core.resolve_path(&key, &hrm_selector(0)).unwrap();
            for _ in 0..65 {
                core.deliver_notification_value(path, &[1]).unwrap();
            }
        }
        for _ in 0..65 {
            central
                .poll_notification("peer", &hrm_selector(0), "consumer-a")
                .await
                .unwrap();
        }
        central
            .boundary()
            .fail_next(FaultOp::Unsubscribe, "orphan disable refused");
        assert!(
            central
                .release_connection_lease("peer", "a", OpControl::unbounded())
                .await
                .is_err()
        );
        assert_eq!(central.resource_counters().await.pending_disables, 1);
        assert!(
            !central
                .release_connection_lease("peer", "a", OpControl::unbounded())
                .await
                .unwrap()
        );
        assert_eq!(
            central.resource_counters().await.pending_disables,
            0,
            "physical retry cannot depend on a retired terminal consumer record"
        );
        central.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn shared_lease_release_deadline_retains_child_and_fences_only_its_owner() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        for lease in ["a", "b"] {
            central
                .connect("peer", lease, OpControl::unbounded())
                .await
                .unwrap();
        }
        central
            .boundary()
            .set_services("peer", vec![hrm_service(), second_hrm_service()]);
        central
            .discover("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central
            .subscribe(
                "peer",
                &hrm_selector(0),
                "consumer-a",
                None,
                OpControl::unbounded().with_connection_lease("a".to_owned()),
            )
            .await
            .unwrap();
        central.boundary().block_op(FaultOp::Unsubscribe);
        let release = central.release_connection_lease("peer", "a", OpControl::budget_ms(100));
        tokio::pin!(release);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(release.as_mut().poll(cx).is_pending())).await
        );
        assert_eq!(
            central
                .read(
                    "peer",
                    &hrm_selector(1),
                    OpControl::unbounded().with_connection_lease("a".to_owned())
                )
                .await
                .unwrap_err()
                .code_str(),
            "ownership.denied"
        );
        assert_eq!(
            central
                .read(
                    "peer",
                    &hrm_selector(1),
                    OpControl::unbounded().with_connection_lease("b".to_owned())
                )
                .await
                .unwrap()
                .value,
            vec![0x42]
        );
        tokio::time::advance(Duration::from_millis(101)).await;
        assert_eq!(release.await.unwrap_err().code_str(), "operation.timed-out");
        assert_eq!(central.resource_counters().await.pending_disables, 1);
        central.boundary().unblock_op(FaultOp::Unsubscribe);
        assert!(
            !central
                .release_connection_lease("peer", "a", OpControl::unbounded())
                .await
                .unwrap()
        );
        assert_eq!(central.resource_counters().await.pending_disables, 0);
        central.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn shared_lease_release_retains_failed_child_and_preserves_other_owners_subscription() {
        let central = open().await;
        for lease in ["a", "b"] {
            central
                .connect("peer", lease, OpControl::unbounded())
                .await
                .unwrap();
        }
        central
            .boundary()
            .set_services("peer", vec![hrm_service(), second_hrm_service()]);
        central
            .discover("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central
            .discover("peer", "b", OpControl::unbounded())
            .await
            .unwrap();
        central
            .subscribe(
                "peer",
                &hrm_selector(0),
                "consumer-a",
                None,
                OpControl::unbounded().with_connection_lease("a".to_owned()),
            )
            .await
            .unwrap();
        central
            .subscribe(
                "peer",
                &hrm_selector(1),
                "consumer-b",
                None,
                OpControl::unbounded().with_connection_lease("b".to_owned()),
            )
            .await
            .unwrap();
        central
            .boundary()
            .fail_next(FaultOp::Unsubscribe, "retained child refusal");
        assert!(
            central
                .release_connection_lease("peer", "a", OpControl::unbounded())
                .await
                .is_err()
        );
        let key = central.peer_key_for("peer").await.unwrap();
        assert!(central.with_core(|core| core.holds_lease(&key, "a")).await);
        assert_eq!(
            central
                .read(
                    "peer",
                    &hrm_selector(1),
                    OpControl::unbounded().with_connection_lease("b".to_owned())
                )
                .await
                .unwrap()
                .value,
            vec![0x42]
        );
        assert!(
            !central
                .release_connection_lease("peer", "a", OpControl::unbounded())
                .await
                .unwrap()
        );
        assert_eq!(central.resource_counters().await.core.live_consumers, 1);
        assert!(
            central
                .release_connection_lease("peer", "b", OpControl::unbounded())
                .await
                .unwrap()
        );
        central.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn shared_discovery_original_path_owner_release_preserves_other_lease_inflight_read() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        for lease in ["a", "b"] {
            central
                .connect("peer", lease, OpControl::unbounded())
                .await
                .unwrap();
        }
        central.boundary().set_services("peer", vec![hrm_service()]);
        central
            .discover("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central
            .discover("peer", "b", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().block_op(FaultOp::Read);
        let selector = hrm_selector(0);
        let pending = central.read(
            "peer",
            &selector,
            OpControl::unbounded().with_connection_lease("b".to_owned()),
        );
        tokio::pin!(pending);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx).is_pending())).await
        );
        assert!(
            !central
                .release_connection_lease("peer", "a", OpControl::unbounded())
                .await
                .unwrap()
        );
        central.boundary().unblock_op(FaultOp::Read);
        assert_eq!(pending.await.unwrap().value, vec![0x42]);
        assert_eq!(
            central
                .read(
                    "peer",
                    &selector,
                    OpControl::unbounded().with_connection_lease("a".to_owned())
                )
                .await
                .unwrap_err()
                .code_str(),
            "ownership.denied"
        );
        central.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn shared_discovery_ready_cache_cannot_renew_an_expired_waiter_budget() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        for lease in ["a", "b"] {
            central
                .connect("peer", lease, OpControl::unbounded())
                .await
                .unwrap();
        }
        central.boundary().set_services("peer", vec![hrm_service()]);
        central.boundary().block_op(FaultOp::Discover);
        let first = central.discover("peer", "a", OpControl::unbounded());
        let second = central.discover("peer", "b", OpControl::budget_ms(100));
        tokio::pin!(first, second);
        assert!(std::future::poll_fn(|cx| Poll::Ready(first.as_mut().poll(cx).is_pending())).await);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx).is_pending())).await
        );
        tokio::time::advance(Duration::from_millis(101)).await;
        central.boundary().unblock_op(FaultOp::Discover);
        first.await.unwrap();
        assert_eq!(second.await.unwrap_err().code_str(), "operation.timed-out");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "discover")
                .count(),
            1
        );
        central.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn shared_discovery_same_lease_coalesces_but_sequential_refresh_is_explicit() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        central.boundary().block_op(FaultOp::Discover);
        let first = central.discover("peer", "a", OpControl::unbounded());
        let second = central.discover("peer", "a", OpControl::unbounded());
        tokio::pin!(first, second);
        assert!(std::future::poll_fn(|cx| Poll::Ready(first.as_mut().poll(cx).is_pending())).await);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx).is_pending())).await
        );
        central.boundary().unblock_op(FaultOp::Discover);
        let (a, b) = tokio::join!(first, second);
        a.unwrap();
        b.unwrap();
        let key = central.peer_key_for("peer").await.unwrap();
        let before = central
            .with_core(|core| core.database_generation(&key))
            .await;
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "discover")
                .count(),
            1
        );
        central
            .discover("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        assert_ne!(
            central
                .with_core(|core| core.database_generation(&key))
                .await,
            before
        );
        central.shutdown().await;
    }

    fn snapshot_identity(revision: u64) -> crate::boundary::GattSnapshotIdentity {
        crate::boundary::GattSnapshotIdentity {
            owner: ":1.42".into(),
            attachment: 1,
            revision,
        }
    }

    async fn stage_routed_value(central: &DesktopCentral<FakeRadio>, value: u8) {
        super::deliver(
            &central.inner,
            (
                "peer".into(),
                HRM_SERVICE.into(),
                0,
                HRM_MEASUREMENT.into(),
                0,
            ),
            central.routing_epoch("peer").await,
            vec![value],
        )
        .await;
    }

    fn gatt_observation_cause() -> crate::errors::DesktopError {
        crate::errors::DesktopError::new(
            ubm_core::contracts::BleErrorCode::GattDiscoveryRequired,
            ubm_core::contracts::BleErrorDomain::Gatt,
            "gatt.snapshot",
        )
        .with_detail("peer snapshot reply timed out")
        .with_platform(
            crate::errors::PlatformDetail::new("bluez-dbus", "org.freedesktop.DBus.Error.NoReply")
                .with_metadata(
                    "method",
                    crate::errors::PlatformValue::Text("GetSnapshot".into()),
                ),
        )
    }

    async fn await_positive_notification(
        central: &DesktopCentral<FakeRadio>,
        peer_id: &str,
        selector: &ubm_core::central::PathSelector,
        consumer: &str,
    ) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match central
                    .poll_notification(peer_id, selector, consumer)
                    .await
                    .unwrap()
                {
                    super::NotificationPoll::Value(value) => break value,
                    super::NotificationPoll::Empty => tokio::task::yield_now().await,
                    outcome => panic!("expected live positive delivery, got {outcome:?}"),
                }
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn gatt_observation_failure_retires_only_matching_peer_and_can_reverify() {
        let central = open().await;
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            None,
            None,
        )
        .unwrap();
        for peer in ["peer", "other"] {
            central
                .connect(peer, "a", OpControl::unbounded())
                .await
                .unwrap();
            central.boundary().set_services(peer, vec![hrm_service()]);
            central
                .boundary()
                .set_gatt_snapshot_identity(peer, snapshot_identity(1));
            central
                .discover(peer, "a", OpControl::unbounded())
                .await
                .unwrap();
            central
                .subscribe(peer, &selector, peer, None, OpControl::unbounded())
                .await
                .unwrap();
        }
        stage_routed_value(&central, 1).await;
        let mut lifecycle = central.lifecycle_events();
        let cause = gatt_observation_cause();
        central
            .boundary()
            .fail_gatt_snapshot_identity("peer", cause.clone());
        central
            .boundary()
            .push_event(RadioEvent::GattObservationFailed {
                peer_id: "peer".into(),
                identity: snapshot_identity(1),
                error: cause.clone(),
            });
        central
            .boundary()
            .push_event(notification("other", 0, vec![2]));
        assert_eq!(
            await_positive_notification(&central, "other", &selector, "other").await,
            vec![2]
        );
        assert_eq!(
            central
                .poll_notification("peer", &selector, "peer")
                .await
                .unwrap(),
            super::NotificationPoll::Value(vec![1])
        );
        assert_eq!(
            central
                .poll_notification("peer", &selector, "peer")
                .await
                .unwrap(),
            super::NotificationPoll::Invalidated(super::InvalidationCause::ServicesChanged)
        );
        let error = central
            .read("peer", &selector, OpControl::unbounded())
            .await
            .unwrap_err();
        assert_eq!(error.detail(), cause.detail());
        assert_eq!(error.platform(), cause.platform());
        let subscribe_error = central
            .subscribe("peer", &selector, "refused", None, OpControl::unbounded())
            .await
            .unwrap_err();
        assert_eq!(subscribe_error.platform(), cause.platform());
        assert_eq!(
            central
                .discovered_paths("peer")
                .await
                .unwrap_err()
                .platform(),
            cause.platform()
        );
        central
            .read("other", &selector, OpControl::unbounded())
            .await
            .unwrap();
        assert!(
            lifecycle.try_recv().is_err(),
            "observation failure cannot invent a physical service-change event"
        );
        central
            .boundary()
            .set_gatt_snapshot_identity("peer", snapshot_identity(1));
        central
            .discover("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central
            .read("peer", &selector, OpControl::unbounded())
            .await
            .unwrap();
        central.shutdown().await;
    }

    #[tokio::test]
    async fn gatt_observation_delayed_failure_preserves_new_or_reverified_same_token_graph() {
        for revision in [1, 2] {
            let central = open().await;
            central
                .connect("peer", "a", OpControl::unbounded())
                .await
                .unwrap();
            central.boundary().set_services("peer", vec![hrm_service()]);
            discover_with_identity(&central, 1).await;
            let cause = gatt_observation_cause();
            central
                .boundary()
                .fail_gatt_snapshot_identity("peer", cause.clone());
            // Failure event is held while the caller explicitly reverifies;
            // its fresh graph may legitimately retain the same daemon token.
            discover_with_identity(&central, revision).await;
            let selector = DesktopCentral::<FakeRadio>::selector(
                HRM_SERVICE,
                Some(0),
                Some(HRM_MEASUREMENT),
                Some(0),
                None,
                None,
            )
            .unwrap();
            central
                .subscribe("peer", &selector, "new", None, OpControl::unbounded())
                .await
                .unwrap();
            central
                .boundary()
                .push_event(RadioEvent::GattObservationFailed {
                    peer_id: "peer".into(),
                    identity: snapshot_identity(1),
                    error: cause.clone(),
                });
            central
                .boundary()
                .push_event(notification("peer", 0, vec![3]));
            assert_eq!(
                await_positive_notification(&central, "peer", &selector, "new").await,
                vec![3]
            );
            central
                .read("peer", &selector, OpControl::unbounded())
                .await
                .unwrap();
            central.shutdown().await;
        }
    }

    #[tokio::test]
    async fn gatt_observation_failure_preserves_cause_when_inflight_read_becomes_stale() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        discover_with_identity(&central, 1).await;
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            None,
            None,
        )
        .unwrap();
        central.boundary().block_op(FaultOp::Read);
        let read = central.read("peer", &selector, OpControl::unbounded());
        tokio::pin!(read);
        assert!(std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx).is_pending())).await);
        let cause = gatt_observation_cause();
        central
            .boundary()
            .fail_gatt_snapshot_identity("peer", cause.clone());
        super::gatt_observation_failed(&central.inner, "peer", &snapshot_identity(1), &cause).await;
        central.boundary().unblock_op(FaultOp::Read);
        let error = read.await.unwrap_err();
        assert_eq!(error.code_str(), "gatt.stale-handle");
        assert_eq!(error.platform(), cause.platform());
        let own_answer = crate::errors::DesktopError::new(
            ubm_core::contracts::BleErrorCode::GattStaleHandle,
            ubm_core::contracts::BleErrorDomain::Gatt,
            "gatt.read",
        )
        .with_platform(
            crate::errors::PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed")
                .with_metadata(
                    "method",
                    crate::errors::PlatformValue::Text("ReadValue".into()),
                ),
        );
        assert_eq!(
            central.annotate_observation_failure("peer", own_answer.clone()),
            own_answer,
            "a read's own platform answer is never replaced by a second observation"
        );
        central.shutdown().await;
    }

    #[tokio::test]
    async fn explicit_rediscovery_retires_old_fifo_and_routing_without_physical_event() {
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        discover_with_identity(&central, 1).await;
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            None,
            None,
        )
        .unwrap();
        central
            .subscribe("peer", &selector, "old", None, OpControl::unbounded())
            .await
            .unwrap();
        stage_routed_value(&central, 1).await;
        let mut lifecycle = central.lifecycle_events();
        discover_with_identity(&central, 2).await;
        assert!(
            central.inner.subscriptions.lock().await.is_empty(),
            "old physical routing cannot survive accepted replacement"
        );
        assert_eq!(
            central
                .poll_notification("peer", &selector, "old")
                .await
                .unwrap(),
            super::NotificationPoll::Value(vec![1])
        );
        assert_eq!(
            central
                .poll_notification("peer", &selector, "old")
                .await
                .unwrap(),
            super::NotificationPoll::Invalidated(super::InvalidationCause::ServicesChanged)
        );
        central
            .subscribe("peer", &selector, "new", None, OpControl::unbounded())
            .await
            .unwrap();
        super::services_changed_scoped_invalidated(&central.inner, "peer", &snapshot_identity(1))
            .await;
        stage_routed_value(&central, 2).await;
        assert_eq!(
            central
                .poll_notification("peer", &selector, "new")
                .await
                .unwrap(),
            super::NotificationPoll::Value(vec![2])
        );
        assert!(
            lifecycle.try_recv().is_err(),
            "requested replacement is not a physical ServicesChanged event"
        );
        central.shutdown().await;
    }

    #[tokio::test]
    async fn gatt_watch_failure_retires_fifo_preserves_diagnostic_and_scan_admission() {
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        discover_with_identity(&central, 1).await;
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            None,
            None,
        )
        .unwrap();
        central
            .subscribe("peer", &selector, "old", None, OpControl::unbounded())
            .await
            .unwrap();
        stage_routed_value(&central, 1).await;
        let mut lifecycle = central.lifecycle_events();
        let cause = crate::errors::DesktopError::new(
            ubm_core::contracts::BleErrorCode::GattDiscoveryRequired,
            ubm_core::contracts::BleErrorDomain::Gatt,
            "gatt.watch",
        )
        .with_detail("trusted control stream malformed")
        .with_platform(
            crate::errors::PlatformDetail::new(
                "bluez-dbus",
                "org.freedesktop.DBus.Error.InvalidSignature",
            )
            .with_metadata("expectedVersion", crate::errors::PlatformValue::Int(1)),
        );
        super::gatt_watch_failed(&central.inner, &cause).await;
        assert_eq!(
            central
                .poll_notification("peer", &selector, "old")
                .await
                .unwrap(),
            super::NotificationPoll::Value(vec![1])
        );
        assert_eq!(
            central
                .poll_notification("peer", &selector, "old")
                .await
                .unwrap(),
            super::NotificationPoll::Invalidated(super::InvalidationCause::ServicesChanged)
        );
        let error = central
            .discover("peer", "a", OpControl::unbounded())
            .await
            .unwrap_err();
        assert_eq!(error.code_str(), "gatt.discovery-required");
        assert_eq!(error.detail(), Some("trusted control stream malformed"));
        assert_eq!(error.platform(), cause.platform());
        let read_error = central
            .read("peer", &selector, OpControl::unbounded())
            .await
            .unwrap_err();
        assert_eq!(
            read_error.detail(),
            Some("trusted control stream malformed")
        );
        assert_eq!(read_error.platform(), cause.platform());
        assert!(
            lifecycle.try_recv().is_err(),
            "control failure cannot invent a physical event"
        );
        assert_eq!(
            super::lock_std(&central.inner.retained_enablements).len(),
            1
        );
        super::gatt_watch_failed(
            &central.inner,
            &crate::errors::DesktopError::adapter_unavailable("later-watch-failure"),
        )
        .await;
        assert_eq!(
            super::lock_std(&central.inner.retained_enablements).len(),
            1,
            "repeat retirement retains old CCCD obligation"
        );
        assert_eq!(
            central
                .discover("peer", "a", OpControl::unbounded())
                .await
                .unwrap_err()
                .platform(),
            cause.platform(),
            "first actual failure remains authoritative"
        );
        central
            .boundary()
            .fail_next(FaultOp::Unsubscribe, "actual native disable refused");
        assert!(
            central
                .unsubscribe("peer", &selector, "old", OpControl::unbounded())
                .await
                .is_err()
        );
        assert_eq!(
            super::lock_std(&central.inner.retained_enablements).len(),
            1
        );
        central
            .unsubscribe("peer", &selector, "old", OpControl::unbounded())
            .await
            .unwrap();
        assert_eq!(
            central.peer_records().await[0].connection_state,
            Some(ConnectionState::Connected)
        );
        central
            .start_scan("scanner", &[], OpControl::unbounded())
            .await
            .unwrap();
        central.shutdown().await;
    }

    #[tokio::test]
    async fn gatt_watch_failure_refuses_a_discovery_already_queued_before_failure() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        discover_with_identity(&central, 1).await;
        let coordinator = super::discovery_coordinator(&central.inner, "peer");
        let gate = coordinator.snapshot.lock().await;
        let discovery = central.discover("peer", "a", OpControl::unbounded());
        tokio::pin!(discovery);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(discovery.as_mut().poll(cx).is_pending())).await
        );
        let cause = crate::errors::DesktopError::adapter_unavailable("gatt.watch")
            .with_detail("observation ended while queued");
        let failure = super::gatt_watch_failed(&central.inner, &cause);
        tokio::pin!(failure);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(failure.as_mut().poll(cx).is_pending())).await
        );
        drop(gate);
        let (result, ()) = tokio::join!(discovery, failure);
        assert_eq!(result.unwrap_err().detail(), cause.detail());
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "discover")
                .count(),
            1,
            "queued discovery never reaches failed control source"
        );
        central.shutdown().await;
    }

    #[tokio::test]
    async fn gatt_watch_failure_refuses_inflight_discovery_publication() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        central.boundary().block_op(FaultOp::Discover);
        let discovery = central.discover("peer", "a", OpControl::unbounded());
        tokio::pin!(discovery);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(discovery.as_mut().poll(cx).is_pending())).await
        );
        let cause = crate::errors::DesktopError::adapter_unavailable("gatt.watch")
            .with_detail("observation ended during traversal");
        let failure = super::gatt_watch_failed(&central.inner, &cause);
        tokio::pin!(failure);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(failure.as_mut().poll(cx).is_pending())).await
        );
        central.boundary().unblock_op(FaultOp::Discover);
        let (result, ()) = tokio::join!(discovery, failure);
        assert_eq!(result.unwrap_err().detail(), cause.detail());
        let key = central.peer_key_for("peer").await.unwrap();
        assert_ne!(
            central.with_core(|core| core.database_state(&key)).await,
            Some(ubm_core::central::DatabaseState::Current)
        );
        central.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_rediscovery_retirement_counts_original_deadline_and_retry_keeps_debt() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        discover_with_identity(&central, 1).await;
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            None,
            None,
        )
        .unwrap();
        central
            .subscribe("peer", &selector, "old", None, OpControl::unbounded())
            .await
            .unwrap();
        let routing = central.inner.subscriptions.lock().await;
        let discovery = central.discover("peer", "a", OpControl::budget_ms(10));
        tokio::pin!(discovery);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(discovery.as_mut().poll(cx).is_pending())).await
        );
        tokio::time::advance(Duration::from_millis(10)).await;
        assert_eq!(
            discovery.await.unwrap_err().code_str(),
            "operation.timed-out"
        );
        drop(routing);
        discover_with_identity(&central, 2).await;
        assert!(central.inner.subscriptions.lock().await.is_empty());
        assert_eq!(
            super::lock_std(&central.inner.retained_enablements).len(),
            1
        );
        assert_eq!(
            central
                .poll_notification("peer", &selector, "old")
                .await
                .unwrap(),
            super::NotificationPoll::Invalidated(super::InvalidationCause::ServicesChanged)
        );
        central.shutdown().await;
    }

    async fn discover_with_identity(central: &DesktopCentral<FakeRadio>, revision: u64) {
        central
            .boundary()
            .set_gatt_snapshot_identity("peer", snapshot_identity(revision));
        central
            .discover("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn scoped_gatt_invalidation_deferred_r1_preserves_published_r2() {
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        discover_with_identity(&central, 1).await;
        discover_with_identity(&central, 2).await;
        let key = central.peer_key_for("peer").await.unwrap();
        let before = central
            .with_core(|core| core.database_generation(&key))
            .await;
        super::services_changed_scoped_invalidated(&central.inner, "peer", &snapshot_identity(1))
            .await;
        assert_eq!(
            central
                .with_core(|core| core.database_generation(&key))
                .await,
            before
        );
        assert_eq!(
            central.with_core(|core| core.database_state(&key)).await,
            Some(ubm_core::central::DatabaseState::Current)
        );
        super::services_changed_scoped_invalidated(&central.inner, "peer", &snapshot_identity(2))
            .await;
        assert_ne!(
            central
                .with_core(|core| core.database_generation(&key))
                .await,
            before
        );
        assert_eq!(
            central.with_core(|core| core.database_state(&key)).await,
            Some(ubm_core::central::DatabaseState::Undiscovered)
        );
        central.shutdown().await;
    }

    #[tokio::test]
    async fn scoped_gatt_invalidation_held_routing_fences_new_discovery() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        discover_with_identity(&central, 1).await;
        let routing = central.inner.subscriptions.lock().await;
        let identity = snapshot_identity(1);
        let invalidation =
            super::services_changed_scoped_invalidated(&central.inner, "peer", &identity);
        tokio::pin!(invalidation);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(invalidation.as_mut().poll(cx).is_pending()))
                .await
        );
        central
            .boundary()
            .set_gatt_snapshot_identity("peer", snapshot_identity(2));
        let discovery = central.discover("peer", "a", OpControl::unbounded());
        tokio::pin!(discovery);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(discovery.as_mut().poll(cx).is_pending())).await,
            "R2 admission must wait until scoped R1 routing retirement completes"
        );
        drop(routing);
        invalidation.await;
        discovery.await.unwrap();
        assert!(central.inner.subscriptions.lock().await.is_empty());
        let key = central.peer_key_for("peer").await.unwrap();
        assert_eq!(
            central.with_core(|core| core.database_state(&key)).await,
            Some(ubm_core::central::DatabaseState::Current)
        );
        central.shutdown().await;
    }

    #[tokio::test]
    async fn scoped_gatt_invalidation_held_routing_fences_subscription_admission() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        discover_with_identity(&central, 1).await;
        let routing = central.inner.subscriptions.lock().await;
        let identity = snapshot_identity(1);
        let invalidation =
            super::services_changed_scoped_invalidated(&central.inner, "peer", &identity);
        tokio::pin!(invalidation);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(invalidation.as_mut().poll(cx).is_pending()))
                .await
        );
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            None,
            None,
        )
        .unwrap();
        let subscription =
            central.subscribe("peer", &selector, "racing", None, OpControl::unbounded());
        tokio::pin!(subscription);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(subscription.as_mut().poll(cx).is_pending()))
                .await
        );
        assert!(
            !central
                .boundary()
                .calls()
                .iter()
                .any(|call| call == "subscribe")
        );
        drop(routing);
        invalidation.await;
        assert!(subscription.await.is_err());
        assert!(central.inner.subscriptions.lock().await.is_empty());
        central.shutdown().await;
    }

    #[tokio::test]
    async fn scoped_gatt_discovery_replacement_before_publication_is_refused() {
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        central.boundary().script_gatt_snapshot_identities(
            "peer",
            vec![Some(snapshot_identity(1)), Some(snapshot_identity(2))],
        );
        let error = central
            .discover("peer", "a", OpControl::unbounded())
            .await
            .unwrap_err();
        assert_eq!(error.code_str(), "gatt.stale-handle");
        let key = central.peer_key_for("peer").await.unwrap();
        assert_ne!(
            central.with_core(|core| core.database_state(&key)).await,
            Some(ubm_core::central::DatabaseState::Current)
        );
        central.shutdown().await;
    }

    #[tokio::test]
    async fn scoped_gatt_old_event_preserves_r2_notification_routing() {
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        discover_with_identity(&central, 1).await;
        discover_with_identity(&central, 2).await;
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            None,
            None,
        )
        .unwrap();
        central
            .subscribe("peer", &selector, "r2", None, OpControl::unbounded())
            .await
            .unwrap();
        central
            .boundary()
            .push_event(RadioEvent::ServicesChangedScoped {
                peer_id: "peer".into(),
                identity: snapshot_identity(1),
            });
        central
            .boundary()
            .push_event(notification("peer", 0, vec![0x42]));
        // The fake's FIFO delivers the value only after the scoped event;
        // a positive receipt proves the event did not retire R2 routing.
        let value = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(value) = central
                    .take_notification("peer", &selector, "r2")
                    .await
                    .unwrap()
                {
                    break value;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(value, vec![0x42]);
        central.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn scoped_gatt_subscription_gate_preserves_deadline_and_cancellation() {
        use std::{future::Future, task::Poll};
        for cancel in [false, true] {
            let central = open().await;
            central
                .connect("peer", "a", OpControl::unbounded())
                .await
                .unwrap();
            central.boundary().set_services("peer", vec![hrm_service()]);
            discover_with_identity(&central, 1).await;
            let coordinator = super::discovery_coordinator(&central.inner, "peer");
            let gate = coordinator.snapshot.lock().await;
            let selector = DesktopCentral::<FakeRadio>::selector(
                HRM_SERVICE,
                Some(0),
                Some(HRM_MEASUREMENT),
                Some(0),
                None,
                None,
            )
            .unwrap();
            let ctl = OpControl::budget_ms(10);
            let ticket = ctl.ticket.clone();
            let subscription = central.subscribe("peer", &selector, "bounded", None, ctl);
            tokio::pin!(subscription);
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(subscription.as_mut().poll(cx).is_pending()))
                    .await
            );
            if cancel {
                ticket.request_cancel();
            } else {
                tokio::time::advance(Duration::from_millis(10)).await;
            }
            let error = subscription.await.unwrap_err();
            assert_eq!(
                error.code_str(),
                if cancel {
                    "operation.aborted"
                } else {
                    "operation.timed-out"
                }
            );
            assert!(ticket.operation_id().is_none());
            assert!(
                !central
                    .boundary()
                    .calls()
                    .iter()
                    .any(|call| call == "subscribe")
            );
            drop(gate);
            central.shutdown().await;
        }
    }

    #[tokio::test]
    async fn scoped_gatt_invalidation_never_waits_for_physical_subscribe_completion() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        discover_with_identity(&central, 1).await;
        central.boundary().block_op(FaultOp::Subscribe);
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            None,
            None,
        )
        .unwrap();
        let subscription =
            central.subscribe("peer", &selector, "held", None, OpControl::unbounded());
        tokio::pin!(subscription);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(subscription.as_mut().poll(cx).is_pending()))
                .await
        );
        tokio::time::timeout(
            Duration::from_secs(2),
            super::services_changed_scoped_invalidated(
                &central.inner,
                "peer",
                &snapshot_identity(1),
            ),
        )
        .await
        .unwrap();
        assert!(central.inner.subscriptions.lock().await.is_empty());
        central.boundary().unblock_op(FaultOp::Subscribe);
        assert!(subscription.await.is_err());
        central.shutdown().await;
    }

    #[tokio::test]
    async fn shared_discovery_service_change_during_traversal_refuses_stale_publication() {
        use std::{future::Future, task::Poll};
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        central.boundary().block_op(FaultOp::Discover);
        let pending = central.discover("peer", "a", OpControl::unbounded());
        tokio::pin!(pending);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx).is_pending())).await
        );
        let mut lifecycle = central.lifecycle_events();
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("peer".into()));
        tokio::time::timeout(Duration::from_secs(2), lifecycle.recv())
            .await
            .unwrap()
            .unwrap();
        central.boundary().unblock_op(FaultOp::Discover);
        assert_eq!(pending.await.unwrap_err().code_str(), "gatt.stale-handle");
        central
            .discover("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.shutdown().await;
    }

    #[tokio::test]
    async fn shared_discovery_failed_attempt_and_service_change_never_reuse_stale_cache() {
        let central = open().await;
        central
            .connect("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_services("peer", vec![hrm_service()]);
        central
            .boundary()
            .fail_next(FaultOp::Discover, "injected discovery refusal");
        assert!(
            central
                .discover("peer", "a", OpControl::unbounded())
                .await
                .is_err()
        );
        central
            .discover("peer", "a", OpControl::unbounded())
            .await
            .unwrap();
        let key = central.peer_key_for("peer").await.unwrap();
        let original = central
            .with_core(|core| core.database_generation(&key))
            .await
            .unwrap();
        let mut lifecycle = central.lifecycle_events();
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("peer".into()));
        let changed = tokio::time::timeout(Duration::from_secs(2), lifecycle.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(changed.kind, crate::LifecycleKind::ServicesChanged);
        central
            .connect("peer", "b", OpControl::unbounded())
            .await
            .unwrap();
        central
            .discover("peer", "b", OpControl::unbounded())
            .await
            .unwrap();
        assert_ne!(
            central
                .with_core(|core| core.database_generation(&key))
                .await
                .unwrap(),
            original
        );
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "discover")
                .count(),
            3
        );
        central.shutdown().await;
    }

    #[tokio::test]
    async fn discovery_refuses_duplicate_native_service_identity() {
        let central = open().await;
        central
            .with_core(|core| {
                let error =
                    super::admit_snapshot(core, &[hrm_service(), hrm_service()]).unwrap_err();
                assert_eq!(error.code_str(), "protocol.violation");
                assert_eq!(error.operation(), "discovery.snapshot.service-identity");
                super::admit_snapshot(core, &[hrm_service(), second_hrm_service()]).unwrap();
            })
            .await;
        central.shutdown().await;
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
            .set_services("peer-3", vec![hrm_service(), second_hrm_service()]);
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
        assert_eq!(value.value, vec![0x42]);
        assert_eq!(
            value.provenance,
            crate::boundary::ReadProvenance::ReadResponse,
            "a radio that attributes the value reports the read response"
        );
        // A radio that fuses read responses and notifications (CoreBluetooth
        // while notifying) says so; the central carries its answer verbatim.
        central
            .boundary()
            .script_read_provenance(crate::boundary::ReadProvenance::ReadOrNotification);
        let fused = central
            .read("peer-3", &hrm_selector(1), OpControl::budget_ms(5000))
            .await
            .expect("read while notifying");
        assert_eq!(
            fused.provenance,
            crate::boundary::ReadProvenance::ReadOrNotification
        );
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
    async fn confirmed_loss_retirement_does_not_authorize_a_reused_lease() {
        let central = open().await;
        ready_peer(&central, "loss-owner", vec![hrm_service()]).await;
        let peer_key = central.peer_key_for("loss-owner").await.unwrap();
        central.remote_peer_loss("loss-owner").await.unwrap();
        assert!(
            super::lock_std(&central.inner.retired_leases)
                .contains_key(&(peer_key.clone(), "lease-a".into()))
        );
        central
            .connect("loss-owner", "lease-a", OpControl::unbounded())
            .await
            .unwrap();
        assert!(
            !super::lock_std(&central.inner.retired_leases)
                .contains_key(&(peer_key.clone(), "lease-a".into()))
        );
        central
            .boundary()
            .fail_next(FaultOp::Disconnect, "new generation remains owned");
        assert!(!central.shutdown().await.is_released());
        assert!(
            central
                .release_connection_lease("loss-owner", "lease-a", OpControl::unbounded())
                .await
                .is_err()
        );
        assert!(central.boundary().link_connected("loss-owner"));
        assert!(central.shutdown().await.is_released());
        assert!(
            central
                .release_connection_lease("loss-owner", "foreign", OpControl::unbounded())
                .await
                .is_err()
        );
        assert!(
            central
                .release_connection_lease("loss-owner", "lease-a", OpControl::unbounded())
                .await
                .unwrap()
        );
        assert!(
            central
                .release_connection_lease("loss-owner", "lease-a", OpControl::unbounded())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn retained_notification_fifo_remains_readable_during_and_after_shutdown() {
        let central = open().await;
        ready_peer(&central, "retained-fifo", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe(
                "retained-fifo",
                &selector,
                "retained-consumer",
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        let key = central.peer_key_for("retained-fifo").await.unwrap();
        central
            .with_core_mut(|core| {
                let index = core
                    .consumer_path(&key, &selector, "retained-consumer")
                    .unwrap();
                core.deliver_notification_value(index, &[0, 74]).unwrap();
                core.deliver_notification_value(index, &[0, 75]).unwrap();
            })
            .await;
        central.boundary().block_op(FaultOp::Disconnect);
        let owner = central.clone();
        let closing = tokio::spawn(async move { owner.shutdown().await });
        tokio::time::timeout(
            Duration::from_secs(2),
            central.boundary().wait_for_calls("disconnect", 1),
        )
        .await
        .unwrap();
        assert!(
            matches!(central.poll_notification("retained-fifo", &selector, "retained-consumer").await.unwrap(), super::NotificationPoll::Value(value) if value == [0, 74])
        );
        central.boundary().unblock_op(FaultOp::Disconnect);
        assert!(closing.await.unwrap().is_released());
        assert!(
            matches!(central.poll_notification("retained-fifo", &selector, "retained-consumer").await.unwrap(), super::NotificationPoll::Value(value) if value == [0, 75])
        );
        assert_eq!(
            central
                .poll_notification("retained-fifo", &selector, "foreign-consumer")
                .await
                .unwrap_err()
                .code_str(),
            "ownership.denied"
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
        assert_eq!(wrist.value, vec![0x77], "occurrence 0 reads instance 0");
        let chest = central
            .read(
                "peer-h1",
                &hrm_instance_selector(0, 1),
                OpControl::budget_ms(5000),
            )
            .await
            .expect("read chest");
        assert_eq!(
            chest.value,
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
    async fn instantiated_parameter_api_overrides_the_compiled_profile_and_preserves_probe_errors()
    {
        fn windows_capabilities(
            core: &mut ubm_core::central::Central,
        ) -> Result<(), ubm_core::contracts::CoreError> {
            crate::capabilities::register_desktop_capabilities_for(
                core,
                Some(crate::capabilities::DesktopOs::Windows),
                false,
            )
        }
        let radio = FakeRadio::new();
        radio.set_connection_parameters_capability_limitation(Ok(Some("runtime-api-absent")));
        let mut profile = super::CentralProfile::desktop("parameter-capability-test");
        profile.register_capabilities = windows_capabilities;
        let central = DesktopCentral::open_with(radio, profile).await.unwrap();
        let states = central
            .with_core(|core| core.registered_capability_states())
            .await;
        assert_eq!(
            states
                .iter()
                .find(|(id, _)| id == "connection:parameters")
                .unwrap()
                .1,
            ubm_core::central::CapabilityState::Unavailable
        );
        central.shutdown().await;

        let radio = FakeRadio::new();
        let failure = DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Platform,
            "test.api-information",
        );
        radio.set_connection_parameters_capability_limitation(Err(failure));
        let failure = match DesktopCentral::open(radio, "parameter-probe-error").await {
            Ok(_) => panic!("a failed runtime probe must not become an unavailable descriptor"),
            Err(error) => error,
        };
        assert_eq!(failure.code(), BleErrorCode::PlatformFailure);
        assert_eq!(failure.operation(), "test.api-information");
    }

    #[tokio::test]
    async fn runtime_priority_absence_and_probe_failure_are_not_advertised_as_supported() {
        let radio = FakeRadio::new();
        radio.set_priority_capability_limitation(Ok(Some("runtime-preferred-api-absent")));
        let central = DesktopCentral::open(radio, "priority-api-absent")
            .await
            .unwrap();
        let states = central
            .with_core(|core| core.registered_capability_states())
            .await;
        assert_eq!(
            states
                .iter()
                .find(|(id, _)| id == "connection:priority")
                .unwrap()
                .1,
            ubm_core::central::CapabilityState::Unavailable
        );
        central.shutdown().await;
        let radio = FakeRadio::new();
        radio.set_priority_capability_limitation(Err(DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Platform,
            "priority-api-probe",
        )));
        let failure = match DesktopCentral::open(radio, "priority-api-error").await {
            Ok(_) => panic!("probe error must propagate"),
            Err(error) => error,
        };
        assert_eq!(failure.operation(), "priority-api-probe");
    }

    #[tokio::test]
    async fn preferred_requests_are_lease_bound_and_cancel_deadline_cannot_publish_late_acceptance()
    {
        use crate::boundary::ConnectionPriority;
        use std::future::Future;
        fn windows_capabilities(
            core: &mut ubm_core::central::Central,
        ) -> Result<(), ubm_core::contracts::CoreError> {
            crate::capabilities::register_desktop_capabilities_for(
                core,
                Some(crate::capabilities::DesktopOs::Windows),
                false,
            )
        }
        let mut profile = super::CentralProfile::desktop("preferred-request-controls");
        profile.register_capabilities = windows_capabilities;
        let central = DesktopCentral::open_with(FakeRadio::new(), profile)
            .await
            .unwrap();
        central
            .connect("peer", "owner", OpControl::unbounded())
            .await
            .unwrap();
        let foreign = central
            .request_priority(
                "peer",
                "foreign",
                ConnectionPriority::Balanced,
                OpControl::unbounded(),
            )
            .await
            .unwrap_err();
        assert_eq!(foreign.code(), BleErrorCode::OwnershipDenied);
        assert!(central.boundary().priority_requests().is_empty());
        central.boundary().block_op(FaultOp::RequestPriority);
        let control = OpControl::unbounded();
        let ticket = control.ticket.clone();
        let pending =
            central.request_priority("peer", "owner", ConnectionPriority::HighThroughput, control);
        tokio::pin!(pending);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(pending.as_mut().poll(&mut context).is_pending());
        ticket.request_cancel();
        assert_eq!(
            pending.await.unwrap_err().code(),
            BleErrorCode::OperationAborted
        );
        assert!(central.boundary().priority_requests().is_empty());
        assert_eq!(
            central
                .request_priority(
                    "peer",
                    "owner",
                    ConnectionPriority::LowPower,
                    OpControl::budget_ms(10)
                )
                .await
                .unwrap_err()
                .code(),
            BleErrorCode::OperationTimedOut
        );
        central.boundary().unblock_all(FaultOp::RequestPriority);
        assert!(central.boundary().priority_requests().is_empty());
        assert!(
            central
                .request_priority(
                    "peer",
                    "owner",
                    ConnectionPriority::Balanced,
                    OpControl::unbounded()
                )
                .await
                .unwrap()
        );
        assert_eq!(
            central.boundary().priority_requests(),
            vec![("peer".to_owned(), ConnectionPriority::Balanced)]
        );
        central
            .disconnect("peer", "owner", OpControl::unbounded())
            .await
            .unwrap();
        assert!(
            central
                .request_priority(
                    "peer",
                    "owner",
                    ConnectionPriority::Balanced,
                    OpControl::unbounded()
                )
                .await
                .is_err()
        );
        assert_eq!(central.boundary().priority_requests().len(), 1);
        central.shutdown().await;
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
        // System directories now have native adapters on the three desktop OSes.
        let connected_directory = central
            .with_core(|core| core.check_capability("peer:system-connected", "desktop.probe"))
            .await;
        if cfg!(any(
            target_os = "macos",
            target_os = "linux",
            target_os = "windows"
        )) {
            assert!(matches!(
                connected_directory,
                Ok(CapabilityAdmission::ProceedWithLimitation)
            ));
        } else {
            assert_eq!(
                connected_directory
                    .expect_err("no OS directory adapter here")
                    .code(),
                BleErrorCode::CapabilityUnsupported
            );
        }
        // ...and address targeting opens only where a narrow native adapter
        // supplies it (BlueZ resolution or WinRT typed address lookup).
        let targeting = central
            .with_core(|core| core.check_capability("peer:address-targeting", "desktop.probe"))
            .await;
        if cfg!(any(target_os = "linux", target_os = "windows")) {
            assert!(
                matches!(targeting, Ok(CapabilityAdmission::ProceedWithLimitation)),
                "the native OS adapter provides limited address targeting: {targeting:?}"
            );
        } else {
            assert_eq!(
                targeting.expect_err("no OS adapter here").code(),
                BleErrorCode::CapabilityUnsupported
            );
        }
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
    async fn native_unsubscribe_drains_the_final_tail_before_retirement() {
        let central = open().await;
        ready_peer(&central, "peer-tail", vec![hrm_service()]).await;
        central
            .subscribe(
                "peer-tail",
                &hrm_selector(0),
                "native-tail",
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        central.boundary().block_op(FaultOp::Unsubscribe);
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let pending = tokio::spawn({
            let central = central.clone();
            let captured = captured.clone();
            async move {
                let drain = |value| {
                    if let crate::NotificationPoll::Value(bytes) = value {
                        captured.lock().unwrap().push(bytes);
                    }
                };
                central
                    .unsubscribe_draining(
                        "peer-tail",
                        &hrm_selector(0),
                        "native-tail",
                        OpControl::unbounded(),
                        Some(&drain),
                    )
                    .await
            }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "set_notifications")
            .count()
            < 2
        {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        let mut wake = central.native_wakes();
        central
            .boundary()
            .push_event(notification("peer-tail", 0, vec![0, 74]));
        tokio::time::timeout(Duration::from_secs(2), wake.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            captured.lock().unwrap().is_empty(),
            "no collector or pre-disable drain observes this tail"
        );
        central.boundary().unblock_op(FaultOp::Unsubscribe);
        assert!(pending.await.unwrap().unwrap());
        assert_eq!(*captured.lock().unwrap(), vec![vec![0, 74]]);
        central.shutdown().await;
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
            primary: None,
            included_services: None,
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![
                characteristic(HRM_MEASUREMENT, false, false),
                characteristic(BATTERY_LEVEL, true, false),
                characteristic(BODY_SENSOR_LOCATION, false, true),
                characteristic(HEART_RATE_CONTROL_POINT, true, true),
            ],
            access: std::default::Default::default(),
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

    async fn ready_write_central() -> DesktopCentral<FakeRadio> {
        fn apple_capabilities(
            core: &mut ubm_core::central::Central,
        ) -> Result<(), ubm_core::contracts::CoreError> {
            crate::capabilities::register_desktop_capabilities_for(
                core,
                Some(crate::capabilities::DesktopOs::MacOs),
                false,
            )
        }
        let mut profile = super::CentralProfile::desktop("ready-write-admission");
        profile.register_capabilities = apple_capabilities;
        let central = DesktopCentral::open_with(FakeRadio::new(), profile)
            .await
            .unwrap();
        ready_peer(&central, "ready-peer", vec![write_matrix_service()]).await;
        central.boundary().set_mtu("ready-peer", 23);
        central.boundary().set_write_limits(
            "ready-peer",
            crate::boundary::WriteLimits {
                with_response: 20,
                without_response: 20,
            },
        );
        central.boundary().set_write_readiness("ready-peer", false);
        central
    }

    #[tokio::test]
    async fn runtime_directory_subset_refuses_before_inventory_access_and_preserves_probe_failure()
    {
        fn windows_capabilities(core: &mut Central) -> Result<(), ubm_core::contracts::CoreError> {
            crate::capabilities::register_desktop_capabilities_for(
                core,
                Some(crate::capabilities::DesktopOs::Windows),
                false,
            )
        }
        let radio = FakeRadio::new();
        radio.set_directory_capability_limitations(Ok((
            Some("selected-adapter-unavailable"),
            Some("connected-selector-unavailable"),
        )));
        let mut profile = super::CentralProfile::desktop("runtime-directory-test");
        profile.register_capabilities = windows_capabilities;
        let central = DesktopCentral::open_with(radio, profile.clone())
            .await
            .unwrap();
        assert_eq!(
            central
                .known_directory_peers(OpControl::unbounded())
                .await
                .unwrap_err()
                .code(),
            BleErrorCode::CapabilityUnavailable
        );
        assert_eq!(
            central
                .connected_peers(&[], OpControl::unbounded())
                .await
                .unwrap_err()
                .code(),
            BleErrorCode::CapabilityUnavailable
        );
        assert!(
            !central
                .boundary()
                .calls()
                .iter()
                .any(|call| call == "known_directory_peers" || call == "connected_peers")
        );
        central.shutdown().await;
        let radio = FakeRadio::new();
        radio.set_directory_capability_limitations(Err(DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Platform,
            "directory.probe",
        )
        .with_detail("native getter failure")));
        let error = DesktopCentral::open_with(radio, profile)
            .await
            .err()
            .expect("original probe failure");
        assert_eq!(error.operation(), "directory.probe");
        assert_eq!(error.detail(), Some("native getter failure"));
    }

    #[tokio::test]
    async fn ready_write_keeps_native_admission_even_when_the_later_worker_runs_first() {
        use futures_util::FutureExt;
        let central = ready_write_central().await;
        let first = OpControl::budget_ms(5000)
            .with_connection_lease("lease-a".into())
            .with_gatt_admission(central.admit_gatt("ready-peer").unwrap());
        let second = OpControl::budget_ms(5000)
            .with_connection_lease("lease-a".into())
            .with_gatt_admission(central.admit_gatt("ready-peer").unwrap());
        let first_selector = write_matrix_selector(BODY_SENSOR_LOCATION);
        let second_selector = write_matrix_selector(HEART_RATE_CONTROL_POINT);
        let earlier = central.write_when_ready("ready-peer", &first_selector, vec![1], first);
        let later = central.write(
            "ready-peer",
            &second_selector,
            vec![2],
            "without-response",
            second,
        );
        tokio::pin!(earlier, later);
        assert!(
            later.as_mut().now_or_never().is_none(),
            "worker scheduling cannot change admission order"
        );
        assert!(central.boundary().writes().is_empty());
        assert!(earlier.as_mut().now_or_never().is_none());
        assert!(
            central.boundary().writes().is_empty(),
            "readiness has not admitted a native write"
        );
        central.boundary().set_write_readiness("ready-peer", true);
        central.boundary().push_event(RadioEvent::WriteReadiness {
            peer_id: "ready-peer".into(),
            ready: true,
        });
        earlier.await.unwrap();
        later.await.unwrap();
        let writes = central.boundary().writes();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].0.3, BODY_SENSOR_LOCATION);
        assert_eq!(writes[1].0.3, HEART_RATE_CONTROL_POINT);
        assert_eq!(central.inner.gatt_admission.active_slots(), 0);
        central.shutdown().await;
    }

    async fn acquired_test_central() -> DesktopCentral<FakeRadio> {
        fn capabilities(core: &mut Central) -> Result<(), ubm_core::contracts::CoreError> {
            crate::capabilities::register_desktop_capabilities_for(
                core,
                Some(crate::capabilities::DesktopOs::Linux),
                false,
            )
        }
        let mut profile = CentralProfile::desktop("acquired-native-owner");
        profile.register_capabilities = capabilities;
        let central = DesktopCentral::open_with(FakeRadio::new(), profile)
            .await
            .unwrap();
        ready_peer(&central, "fd-peer", vec![write_matrix_service()]).await;
        central
            .boundary()
            .acquired_gatt()
            .configure(
                "fd-peer",
                crate::acquired_gatt::synthetic::SyntheticAcquisition {
                    write: true,
                    notify: true,
                    mtu: 23,
                },
            )
            .unwrap();
        central
    }

    #[tokio::test]
    async fn acquired_write_owns_payload_and_cancelled_backpressure_has_no_effect() {
        use futures_util::FutureExt;
        let central = acquired_test_central().await;
        let selector = write_matrix_selector(BODY_SENSOR_LOCATION);
        let handle = central
            .acquire_gatt(
                "fd-peer",
                &selector,
                crate::acquired_gatt::AcquisitionKind::Write,
                OpControl::budget_ms(5000).with_connection_lease("lease-a".into()),
            )
            .await
            .unwrap();
        assert_eq!(handle.mtu, 23);
        assert_eq!(
            central.resource_counters().await.acquired_gatt_transports,
            1
        );
        central.boundary().acquired_gatt().block(true);
        let ctl = OpControl::budget_ms(5000).with_connection_lease("lease-a".into());
        let ticket = ctl.ticket.clone();
        let pending = central.acquired_write(&handle.handle, vec![42], ctl);
        tokio::pin!(pending);
        assert!(pending.as_mut().now_or_never().is_none());
        ticket.request_cancel();
        assert_eq!(
            pending.await.unwrap_err().code(),
            BleErrorCode::OperationAborted
        );
        assert!(central.boundary().acquired_gatt().writes().is_empty());
        central.boundary().acquired_gatt().block(false);
        central
            .acquired_write(
                &handle.handle,
                vec![2],
                OpControl::budget_ms(5000).with_connection_lease("lease-a".into()),
            )
            .await
            .unwrap();
        assert_eq!(central.boundary().acquired_gatt().writes(), vec![vec![2]]);
        central
            .close_acquired(
                &handle.handle,
                OpControl::unbounded().with_connection_lease("lease-a".into()),
            )
            .await
            .unwrap();
        central
            .close_acquired(
                &handle.handle,
                OpControl::unbounded().with_connection_lease("lease-a".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            central.resource_counters().await.acquired_gatt_transports,
            0
        );
        central.shutdown().await;
    }

    #[tokio::test]
    async fn explicit_rediscovery_ends_a_waiting_ready_write_without_a_readiness_event() {
        use futures_util::FutureExt;
        let central = ready_write_central().await;
        let selector = write_matrix_selector(BODY_SENSOR_LOCATION);
        let waiting = central.write_when_ready(
            "ready-peer",
            &selector,
            vec![1],
            OpControl::budget_ms(5000).with_connection_lease("lease-a".into()),
        );
        tokio::pin!(waiting);
        assert!(waiting.as_mut().now_or_never().is_none());
        let rediscovery = central.discover("ready-peer", "lease-a", OpControl::budget_ms(5000));
        tokio::pin!(rediscovery);
        assert!(rediscovery.as_mut().now_or_never().is_none());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap_err()
                .code(),
            BleErrorCode::GattStaleHandle
        );
        rediscovery.await.unwrap();
        assert!(central.boundary().writes().is_empty());
        central.boundary().set_write_readiness("ready-peer", true);
        central
            .write_when_ready(
                "ready-peer",
                &selector,
                vec![2],
                OpControl::budget_ms(5000).with_connection_lease("lease-a".into()),
            )
            .await
            .unwrap();
        assert_eq!(central.boundary().write_values(), vec![vec![2]]);
        central.shutdown().await;
    }

    #[tokio::test]
    async fn acquired_invalidation_settles_backpressure_and_parent_release_drops_the_fd() {
        use futures_util::FutureExt;
        let central = acquired_test_central().await;
        let selector = write_matrix_selector(BODY_SENSOR_LOCATION);
        let handle = central
            .acquire_gatt(
                "fd-peer",
                &selector,
                crate::acquired_gatt::AcquisitionKind::Write,
                OpControl::budget_ms(5000).with_connection_lease("lease-a".into()),
            )
            .await
            .unwrap();
        central.boundary().acquired_gatt().block(true);
        let pending = central.acquired_write(
            &handle.handle,
            vec![1],
            OpControl::budget_ms(5000).with_connection_lease("lease-a".into()),
        );
        tokio::pin!(pending);
        assert!(pending.as_mut().now_or_never().is_none());
        services_changed_invalidated(&central.inner, "fd-peer").await;
        assert_eq!(
            pending.await.unwrap_err().code(),
            BleErrorCode::GattStaleHandle
        );
        central
            .release_connection_lease_report("fd-peer", "lease-a", OpControl::budget_ms(5000))
            .await
            .unwrap();
        assert_eq!(
            central.resource_counters().await.acquired_gatt_transports,
            0
        );
        assert_eq!(central.boundary().acquired_gatt().active(), 0);
        assert!(central.boundary().acquired_gatt().writes().is_empty());
        central
            .close_acquired(
                &handle.handle,
                OpControl::unbounded().with_connection_lease("lease-a".into()),
            )
            .await
            .unwrap();
        central.shutdown().await;
    }

    #[tokio::test]
    async fn explicit_native_disconnect_closes_acquired_children_before_the_radio() {
        let central = acquired_test_central().await;
        let selector = write_matrix_selector(BODY_SENSOR_LOCATION);
        let handle = central
            .acquire_gatt(
                "fd-peer",
                &selector,
                crate::acquired_gatt::AcquisitionKind::Write,
                OpControl::budget_ms(5000).with_connection_lease("lease-a".into()),
            )
            .await
            .unwrap();
        central
            .disconnect("fd-peer", "lease-a", OpControl::budget_ms(5000))
            .await
            .unwrap();
        assert_eq!(central.boundary().acquired_gatt().active(), 0);
        assert_eq!(
            central.resource_counters().await.acquired_gatt_transports,
            0
        );
        central
            .close_acquired(
                &handle.handle,
                OpControl::unbounded().with_connection_lease("lease-a".into()),
            )
            .await
            .unwrap();
        central.shutdown().await;
    }

    #[tokio::test]
    async fn ready_write_queued_time_consumes_its_original_deadline_and_releases_its_position() {
        let central = ready_write_central().await;
        let earlier = central.admit_gatt("ready-peer").unwrap();
        let control = OpControl::budget_ms(10)
            .with_connection_lease("lease-a".into())
            .with_gatt_admission(central.admit_gatt("ready-peer").unwrap());
        let selector = write_matrix_selector(BODY_SENSOR_LOCATION);
        let failure = central
            .write_when_ready("ready-peer", &selector, vec![1], control)
            .await
            .unwrap_err();
        assert_eq!(failure.code(), BleErrorCode::OperationTimedOut);
        assert_eq!(
            failure.commit(),
            Some(ubm_core::contracts::CommitState::NotDispatched)
        );
        assert!(central.boundary().writes().is_empty());
        drop(earlier);
        central.boundary().set_write_readiness("ready-peer", true);
        central
            .write_when_ready(
                "ready-peer",
                &selector,
                vec![2],
                OpControl::budget_ms(5000).with_connection_lease("lease-a".into()),
            )
            .await
            .unwrap();
        assert_eq!(central.inner.gatt_admission.active_slots(), 0);
        central.shutdown().await;
    }

    #[tokio::test]
    async fn ready_write_invalidation_interrupts_a_held_probe_without_another_readiness_event() {
        use futures_util::FutureExt;
        let central = ready_write_central().await;
        central.boundary().block_op(FaultOp::WriteReadiness);
        let selector = write_matrix_selector(BODY_SENSOR_LOCATION);
        let control = OpControl::budget_ms(5000).with_connection_lease("lease-a".into());
        let waiting = central.write_when_ready("ready-peer", &selector, vec![1], control);
        tokio::pin!(waiting);
        assert!(waiting.as_mut().now_or_never().is_none());
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("ready-peer".into()));
        let failure = tokio::time::timeout(Duration::from_millis(1000), waiting)
            .await
            .expect("database invalidation must settle the held readiness probe")
            .unwrap_err();
        assert_eq!(failure.code(), BleErrorCode::GattStaleHandle);
        assert_eq!(
            failure.commit(),
            Some(ubm_core::contracts::CommitState::NotDispatched)
        );
        central.boundary().unblock_all(FaultOp::WriteReadiness);
        assert!(central.boundary().writes().is_empty());
        assert_eq!(central.inner.gatt_admission.active_slots(), 0);
        central.shutdown().await;
    }

    #[tokio::test]
    async fn a_delayed_worker_cannot_adopt_a_database_rediscovered_after_its_admission() {
        let central = ready_write_central().await;
        let stale = OpControl::budget_ms(5000)
            .with_connection_lease("lease-a".into())
            .with_gatt_admission(central.admit_gatt("ready-peer").unwrap());
        let mut events = central.lifecycle_events();
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("ready-peer".into()));
        tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        let selector = write_matrix_selector(BODY_SENSOR_LOCATION);
        // The old slot is invalidated immediately, even while it remains owned.
        let failure = central
            .write("ready-peer", &selector, vec![1], "without-response", stale)
            .await
            .unwrap_err();
        assert_eq!(failure.code(), BleErrorCode::GattStaleHandle);
        central
            .discover("ready-peer", "lease-a", OpControl::unbounded())
            .await
            .unwrap();
        central.boundary().set_write_readiness("ready-peer", true);
        central
            .write_when_ready(
                "ready-peer",
                &selector,
                vec![2],
                OpControl::budget_ms(5000).with_connection_lease("lease-a".into()),
            )
            .await
            .unwrap();
        assert_eq!(central.boundary().write_values(), vec![vec![2]]);
        assert_eq!(central.resource_counters().await.native_gatt_admissions, 0);
        central.shutdown().await;
    }

    #[tokio::test]
    async fn readiness_wait_ends_on_link_loss_and_original_gatt_source_failure() {
        use futures_util::FutureExt;
        for source_failure in [false, true] {
            let central = ready_write_central().await;
            let selector = write_matrix_selector(BODY_SENSOR_LOCATION);
            let waiting = central.write_when_ready(
                "ready-peer",
                &selector,
                vec![1],
                OpControl::budget_ms(5000).with_connection_lease("lease-a".into()),
            );
            tokio::pin!(waiting);
            assert!(waiting.as_mut().now_or_never().is_none());
            if source_failure {
                central.boundary().push_event(RadioEvent::GattWatchFailed(
                    DesktopError::new(
                        BleErrorCode::AdapterPoweredOff,
                        BleErrorDomain::Adapter,
                        "test.readiness.source",
                    )
                    .with_detail("original source fault"),
                ));
            } else {
                central
                    .boundary()
                    .push_event(RadioEvent::Disconnected("ready-peer".into()));
            }
            let failure = tokio::time::timeout(Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(
                failure.code(),
                if source_failure {
                    BleErrorCode::AdapterPoweredOff
                } else {
                    BleErrorCode::ConnectionLost
                }
            );
            if source_failure {
                assert_eq!(failure.operation(), "test.readiness.source");
            }
            assert!(central.boundary().write_values().is_empty());
            assert_eq!(central.resource_counters().await.native_gatt_admissions, 0);
            central.shutdown().await;
        }
    }

    #[tokio::test]
    async fn ready_write_cancel_and_probe_failure_keep_zero_write_effects_and_retire_admission() {
        use futures_util::FutureExt;
        let central = ready_write_central().await;
        let selector = write_matrix_selector(BODY_SENSOR_LOCATION);
        let control = OpControl::budget_ms(5000).with_connection_lease("lease-a".into());
        let ticket = control.ticket.clone();
        let waiting = central.write_when_ready("ready-peer", &selector, vec![1], control);
        tokio::pin!(waiting);
        assert!(waiting.as_mut().now_or_never().is_none());
        ticket.request_cancel();
        let failure = waiting.await.unwrap_err();
        assert_eq!(failure.code(), BleErrorCode::OperationAborted);
        assert_eq!(
            failure.commit(),
            Some(ubm_core::contracts::CommitState::NotDispatched)
        );
        central
            .boundary()
            .fail_next(FaultOp::WriteReadiness, "readiness getter refused");
        let failure = central
            .write_when_ready(
                "ready-peer",
                &selector,
                vec![2],
                OpControl::budget_ms(5000).with_connection_lease("lease-a".into()),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code(), BleErrorCode::PlatformFailure);
        assert_eq!(failure.operation(), "gatt.write-readiness");
        assert_eq!(failure.detail(), Some("readiness getter refused"));
        assert!(central.boundary().writes().is_empty());
        assert_eq!(central.inner.gatt_admission.active_slots(), 0);
        central.shutdown().await;
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
                primary: None,
                included_services: None,
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

                access: std::default::Default::default(),
            },
            ServiceSnapshot {
                primary: None,
                included_services: None,
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

                access: std::default::Default::default(),
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
            value.value,
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
        assert_eq!(value.value, vec![0x42]);
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
        assert_eq!(value.value, vec![0x42]);
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
        // Owner decision (5.0): the link was lost, so the read reports the
        // same word as every other host.
        assert!(
            error.code_str() == "connection.lost" || error.code_str() == "gatt.stale-handle",
            "lost or stale, got {}",
            error.code_str()
        );
    }

    /// Owner decision (5.0): the OS reports the link gone while an
    /// operation waits on a radio that never answers (CoreBluetooth does not
    /// call back a pending read at a disconnect). The central ends it at
    /// once with `connection.lost`, as Android's stack does, instead of
    /// leaving it to its deadline.
    #[tokio::test]
    async fn a_link_loss_ends_a_pending_operation_the_radio_never_answers() {
        for (op, fault) in [("read", FaultOp::Read), ("discover", FaultOp::Discover)] {
            let central = open().await;
            let peer = format!("peer-hang-{op}");
            ready_peer(&central, &peer, vec![hrm_service()]).await;
            central.boundary().block_op(fault);
            let worker = central.clone();
            let target = peer.clone();
            let pending = tokio::spawn(async move {
                match op {
                    "read" => worker
                        .read(&target, &hrm_selector(0), OpControl::budget_ms(60_000))
                        .await
                        .map(|_| ()),
                    _ => worker
                        .discover(&target, "lease-a", OpControl::budget_ms(60_000))
                        .await
                        .map(|_| ()),
                }
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
            central
                .boundary()
                .push_event(RadioEvent::Lost(peer.clone()));
            let error = tokio::time::timeout(Duration::from_secs(5), pending)
                .await
                .expect("ended by the loss, not the deadline")
                .expect("task")
                .expect_err("the link is gone");
            assert_eq!(error.code_str(), "connection.lost", "{op}");
            central.boundary().unblock_op(fault);
        }
    }

    /// The app's own release ends a read waiting on a radio that never
    /// answers: `operation.disconnected`, at once.
    #[tokio::test]
    async fn the_apps_release_ends_a_pending_operation_the_radio_never_answers() {
        let central = open().await;
        ready_peer(&central, "peer-hang-release", vec![hrm_service()]).await;
        central.boundary().block_op(FaultOp::Read);
        let worker = central.clone();
        let pending = tokio::spawn(async move {
            worker
                .read(
                    "peer-hang-release",
                    &hrm_selector(0),
                    OpControl::budget_ms(60_000),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        central
            .disconnect("peer-hang-release", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("release");
        let error = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .expect("ended by the release, not the deadline")
            .expect("task")
            .expect_err("released");
        assert_eq!(error.code_str(), "operation.disconnected");
        central.boundary().unblock_op(FaultOp::Read);
    }

    /// The app's own release cut the read off: the link did not drop, so
    /// the read reports `operation.disconnected`, not a loss.
    #[tokio::test]
    async fn f03_requested_disconnect_during_read_is_operation_disconnected() {
        let central = open().await;
        ready_peer(&central, "peer-f03r", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central.boundary().block_op(FaultOp::Read);
        let reader = central.clone();
        let pending = tokio::spawn(async move {
            reader
                .read("peer-f03r", &selector, OpControl::budget_ms(5000))
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
            .disconnect("peer-f03r", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("release");
        central.boundary().unblock_op(FaultOp::Read);
        let error = pending
            .await
            .expect("read task")
            .expect_err("the release wins over late radio success");
        assert!(
            error.code_str() == "operation.disconnected" || error.code_str() == "gatt.stale-handle",
            "released or stale, got {}",
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
    async fn reused_consumer_does_not_inherit_prior_link_retirement() {
        let central = open().await;
        let selector = hrm_selector(0);
        ready_peer(&central, "reuse-peer", vec![hrm_service()]).await;
        central
            .subscribe(
                "reuse-peer",
                &selector,
                "reused",
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        super::reconcile_disconnected(&central.inner, "reuse-peer", true).await;
        ready_peer(&central, "reuse-peer", vec![hrm_service()]).await;
        central
            .subscribe(
                "reuse-peer",
                &selector,
                "reused",
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        super::services_changed_invalidated(&central.inner, "reuse-peer").await;
        assert!(
            central
                .unsubscribe("reuse-peer", &selector, "reused", OpControl::unbounded())
                .await
                .unwrap(),
            "current-generation retained native enablement must actually be disabled"
        );
        assert!(super::lock_std(&central.inner.retained_enablements).is_empty());
        central.shutdown().await;
    }

    #[tokio::test]
    async fn host_reported_loss_retires_exact_owned_consumer() {
        let central = open().await;
        let selector = hrm_selector(0);
        ready_peer(&central, "host-lost-peer", vec![hrm_service()]).await;
        central
            .subscribe(
                "host-lost-peer",
                &selector,
                "owned",
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        central.remote_peer_loss("host-lost-peer").await.unwrap();
        central
            .boundary()
            .fail_next(FaultOp::Connect, "reconnect refused");
        assert!(
            central
                .connect("host-lost-peer", "retry", OpControl::unbounded())
                .await
                .is_err()
        );
        assert!(
            !central
                .unsubscribe("host-lost-peer", &selector, "owned", OpControl::unbounded())
                .await
                .unwrap()
        );
        assert!(
            central
                .unsubscribe(
                    "host-lost-peer",
                    &selector,
                    "foreign",
                    OpControl::unbounded()
                )
                .await
                .is_err()
        );
        central.shutdown().await;
    }

    #[tokio::test]
    async fn confirmed_link_loss_retires_only_the_old_subscription_consumer() {
        for reconnect in [false, true] {
            let central = open().await;
            ready_peer(&central, "retired-peer", vec![hrm_service()]).await;
            ready_peer(&central, "unrelated-peer", vec![hrm_service()]).await;
            let selector = hrm_selector(0);
            for (peer, consumer) in [
                ("retired-peer", "old-consumer"),
                ("unrelated-peer", "other-consumer"),
            ] {
                central
                    .subscribe(peer, &selector, consumer, None, OpControl::unbounded())
                    .await
                    .unwrap();
            }
            super::reconcile_disconnected(&central.inner, "retired-peer", true).await;
            if reconnect {
                ready_peer(&central, "retired-peer", vec![hrm_service()]).await;
                central
                    .subscribe(
                        "retired-peer",
                        &selector,
                        "new-consumer",
                        None,
                        OpControl::unbounded(),
                    )
                    .await
                    .unwrap();
            } else {
                central
                    .boundary()
                    .fail_next(FaultOp::Connect, "reconnect refused");
                assert!(
                    central
                        .connect("retired-peer", "retry-lease", OpControl::unbounded())
                        .await
                        .is_err()
                );
            }
            assert!(
                !central
                    .unsubscribe(
                        "retired-peer",
                        &selector,
                        "old-consumer",
                        OpControl::unbounded()
                    )
                    .await
                    .unwrap()
            );
            if !reconnect {
                assert!(
                    central
                        .unsubscribe(
                            "retired-peer",
                            &selector,
                            "foreign-consumer",
                            OpControl::unbounded()
                        )
                        .await
                        .is_err()
                );
            }
            if reconnect {
                assert!(
                    central
                        .unsubscribe(
                            "retired-peer",
                            &selector,
                            "new-consumer",
                            OpControl::unbounded()
                        )
                        .await
                        .unwrap()
                );
            }
            assert!(
                central
                    .unsubscribe(
                        "unrelated-peer",
                        &selector,
                        "other-consumer",
                        OpControl::unbounded()
                    )
                    .await
                    .unwrap()
            );
            assert!(super::lock_std(&central.inner.retired_consumers).is_empty());
            central.shutdown().await;
        }
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
        assert_eq!(value.value, vec![0x42]);
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
        assert_eq!(value.value, vec![0x42]);
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
        assert!(
            !central.boundary().link_connected("peer-f03x"),
            "late native success must be physically compensated after shutdown"
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
        assert_eq!(value.value, vec![0x42]);
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
            assert_eq!(value.value, vec![0x42]);
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
        assert_eq!(value.value, vec![0x42]);
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
    async fn held_transport_close_is_bounded_and_retryable() {
        tokio::time::pause();
        let central = open().await;
        central.boundary().block_op(FaultOp::FinishClose);
        let first = central.shutdown().await;
        assert_eq!(
            first
                .transport_close_failures
                .first()
                .expect("bounded cleanup")
                .code(),
            ubm_core::contracts::BleErrorCode::OperationTimedOut
        );
        central.boundary().unblock_op(FaultOp::FinishClose);
        assert!(central.shutdown().await.transport_close_failures.is_empty());
    }

    #[tokio::test]
    async fn shutdown_retains_transport_failure_and_retries_after_loop_join() {
        let central = open().await;
        central
            .boundary()
            .fail_next(FaultOp::FinishClose, "match removal refused");
        let first = central.shutdown().await;
        assert_eq!(
            first
                .transport_close_failures
                .first()
                .expect("owned transport refusal")
                .detail(),
            Some("match removal refused")
        );
        assert!(first.radio_close_failures.is_empty());
        assert!(central.inner.loop_done.lock().await.is_none());
        let second = central.shutdown().await;
        assert!(second.transport_close_failures.is_empty());
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "finish_close")
                .count(),
            2
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
