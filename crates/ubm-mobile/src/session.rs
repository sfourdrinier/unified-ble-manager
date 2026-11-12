//! [`MobileSession`]: one RN manager's lease on the process owner.
//!
//! `invoke` parses and validates synchronously (every rejection happens
//! before any effect), registers the `operationId` before the first await,
//! runs the op on the host runtime and calls the completion exactly once
//! with the envelope text. `op.cancel` targets exactly one operation
//! and is classified exactly by the operation's `admission` (finding 109):
//! admitted and ended → `already-terminal`; not arrived yet → the later
//! invoke ends `operation.aborted` with zero effects. `drain` hands out queued
//! records in ordinal order and re-arms the wake.
//!
//! Resources a session holds on the shared central (scan membership,
//! connection leases, subscription consumers) are namespaced by session id
//! and released by `session.dispose`, which reports every failure and
//! keeps what failed so a second dispose retries it. Background leases
//! belong to the session's background scope: a shared scope (one React
//! Native module) keeps them past dispose, as legacy did (87/N8).

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde_json::Value;
use tokio::sync::Notify;
use tokio::time::Instant;
use ubm_core::central::{ConnectionState, OperationAdmission, PathSelector, canonical_uuid};
use ubm_core::contracts::{
    AttachmentTuple, BleErrorCode, BleErrorDomain, CommitState, MAX_TIMEOUT_MS,
};
use ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope;
use ubm_desktop::{
    Budget, CancelAck, DeliveryMode, DesktopCentral, DesktopError, DiscoveredPath, InstanceKey,
    LIVENESS_BACKSTOP_DETAIL, LIVENESS_OP, ObservedDelivery, OpControl, OpTicket, PeerRecord,
};

use crate::drain::Outbox;
use crate::foreign::{ConnectStaging, DISPATCHED, OP_RADIO, OpRadio, lock};
use crate::host::{
    BackgroundScope, HostInner, Route, ScanMember, ScanTerminalEvidenceScope, SecurityEntry,
    adapter_value, security_value,
};
use crate::radio::BackgroundKind;
use crate::radio::{
    AndroidScanOptions, BondState, ConnectionPriority, Instance, MobilePlatform, PairTransport,
    Phy, RadioCompletion, RadioRequest, ScanCallbackType, ScanMode, ScanPhy, SubrateMode,
};
use crate::wire::{self, Args, object, opt_text};

/// Completion callback: called exactly once with the envelope text.
pub type Completion = Box<dyn FnOnce(String) + Send + 'static>;

/// Every op the wire accepts, in documentation order.
pub const OPS: &[&str] = &[
    "adapter.state",
    "counters.describe",
    // Host-owned durable terminal evidence.  These operations are deliberately
    // separate from the radio surface: the selected trusted host owns its
    // ledger and never accepts a storage path from JavaScript.
    "terminal-evidence.reserve",
    "terminal-evidence.commit",
    "terminal-evidence.read",
    "terminal-evidence.release",
    "scan.start",
    "scan.stop",
    "peers.resolve",
    "peers.known",
    "peers.connected",
    "peers.bonded",
    "peers.restored",
    "peers.claim-restored",
    "connection.connect",
    "connection.disconnect",
    "connection.rssi",
    "connection.effective-mtu",
    "connection.request-mtu",
    "connection.request-priority",
    "connection.control-capabilities",
    "connection.request-subrate",
    "connection.read-phy",
    "connection.request-phy",
    "connection.maximum-write-length",
    "connection.write-readiness",
    "security.state",
    "security.pair",
    "security.cancel-pairing",
    "gatt.discover",
    "gatt.read",
    "gatt.read-descriptor",
    "gatt.write",
    "gatt.write-when-ready",
    "gatt.write-descriptor",
    "gatt.subscribe",
    "gatt.subscribe-recorded",
    "gatt.unsubscribe",
    "gatt.ordered.version",
    "gatt.ordered.failure-metadata.version",
    "gatt.ordered.capacity.version",
    "gatt.ordered.reserve",
    "gatt.ordered.reclaim",
    "gatt.ordered.open",
    "gatt.ordered.status",
    "gatt.ordered.increase-capacity",
    "gatt.ordered.poll",
    "gatt.ordered.ack-delivery",
    "gatt.ordered.discard",
    "gatt.ordered.poll-discard",
    "gatt.ordered.ack-discard",
    "gatt.ordered.close",
    "gatt.ordered.ack-group",
    "gatt.ordered.forget-group",
    "gatt.ordered.recording-reclaim.version",
    "gatt.ordered.recording-reclaim.next",
    "gatt.ordered.recording-reclaim.reserve",
    "gatt.ordered.recording-reclaim.start",
    "gatt.ordered.recording-reclaim.status",
    "gatt.ordered.recording-reclaim.commit",
    "gatt.ordered.recording-reclaim.cancel",
    "gatt.ordered.recording-reclaim.release",
    "gatt.ordered.recording-reclaim.retire",
    "gatt.ordered.recording-reclaim.ack-retire",
    "gatt.ordered.recording-reclaim.forget-retire",
    "gatt.ordered.recording-reclaim.ack-forget",
    "gatt.ordered.marker.version",
    "gatt.ordered.marker.reserve",
    "gatt.ordered.marker.start",
    "gatt.ordered.marker.status",
    "gatt.ordered.marker.cancel",
    "gatt.ordered.marker.forget",
    "background.acquire",
    "background.release",
    "background.update-notification",
    "companion.associate",
    "companion.list",
    "companion.disassociate",
    "presence.observe",
    "presence.unobserve",
    "op.cancel",
    "session.reconcile",
    // Internal native-continuation handoff operations. They never cross the
    // public React Native wire; Kotlin uses them to establish a cutoff and
    // report cleanup with post-cutoff accounting.
    "session.quiesce",
    "session.continuation-dispose",
    "session.dispose",
];

const EFFECTFUL_OPS: &[&str] = &[
    "gatt.write",
    "gatt.write-when-ready",
    "gatt.write-descriptor",
    "connection.request-mtu",
    "connection.request-priority",
    "connection.request-subrate",
    "connection.request-phy",
];
/// How far past the highest admitted operation a pre-admission cancel may
/// name an admission (finding 109). A client assigns admissions in the order
/// it sends invokes, so a cancel can only precede invokes it already sent;
/// naming one further ahead is a protocol violation (`argument.invalid`).
pub const ADMISSION_WINDOW: u64 = 65_536;

/// Operation identity per session (finding 109, legacy's dispatch epoch).
/// Every invoke that carries an `operationId` carries a strictly increasing
/// `admission`, so `op.cancel {operationId, admission}` is classified
/// exactly with no memory of finished operations: live → cancelled;
/// `admission <= highest_admission` → already terminal; above → the invoke
/// has not arrived yet and is refused when it does.
struct OpTable {
    live: HashMap<String, (OpTicket, u64)>,
    highest_admission: u64,
    /// Pre-admission cancels: admissions above `highest_admission` only
    /// (pruned as admission advances past them).
    cancelled_ahead: BTreeSet<u64>,
}

enum CancelTarget {
    Live(OpTicket),
    Terminal,
    Ahead,
}

#[derive(Debug, Clone)]
pub(crate) struct Subscription {
    peer_id: String,
    selector: PathSelector,
    core_consumer: String,
    scope: InstanceKey,
    attachment: AttachmentTuple,
    admission: OperationAdmission,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedLease {
    name: String,
    attachment: AttachmentTuple,
    admission: OperationAdmission,
}

// Live owners and failed cleanup are never evicted. Bound them separately
// from successful receipts, whose eviction only makes old identities stale.
const DEFAULT_EXACT_LIVE_LEASE_LIMIT: usize = 1024;
const DEFAULT_EXACT_RECEIPT_LIMIT: usize = 1024;

struct ExactLeaseRecord {
    peer: String,
    name: String,
    owner: Option<OwnedLease>,
    released: bool,
    attempt: Option<Arc<ExactReleaseAttempt>>,
}

struct ExactReleaseAttempt {
    result: Mutex<Option<Value>>,
    changed: Notify,
}

struct ExactReleaseLeader(Arc<ExactReleaseAttempt>);

impl Drop for ExactReleaseLeader {
    fn drop(&mut self) {
        let mut result = lock(&self.0.result);
        if result.is_none() {
            let failure = error(
                BleErrorCode::LifecycleInvariantViolation,
                BleErrorDomain::Cleanup,
                "connection.disconnect.attempt",
            )
            .with_detail("release task ended without a cleanup result");
            *result = Some(connection_cleanup_record(&failure));
            self.0.changed.notify_waiters();
        }
    }
}

struct ExactLeases {
    namespace: String,
    next: u64,
    records: HashMap<String, ExactLeaseRecord>,
    live_limit: usize,
    receipt_limit: usize,
    receipts: VecDeque<String>,
}

impl ExactLeases {
    fn trim_receipts(&mut self) {
        while self.receipts.len() > self.receipt_limit {
            let Some(identity) = self.receipts.pop_front() else {
                break;
            };
            if self
                .records
                .get(&identity)
                .is_some_and(|entry| entry.released)
            {
                self.records.remove(&identity);
            }
        }
    }
}

struct ExactLeaseReservation<'a> {
    table: &'a Mutex<Option<ExactLeases>>,
    identity: String,
}

impl Drop for ExactLeaseReservation<'_> {
    fn drop(&mut self) {
        if let Some(table) = lock(self.table).as_mut() {
            if table
                .records
                .get(&self.identity)
                .is_some_and(|entry| entry.owner.is_none())
            {
                table.records.remove(&self.identity);
            }
        }
    }
}

pub(crate) struct ResetResources {
    leases: Vec<(String, OwnedLease, Option<DesktopError>)>,
    subscriptions: Vec<(String, Subscription, Option<DesktopError>)>,
}

impl ResetResources {
    pub(crate) fn is_empty(&self) -> bool {
        self.leases.is_empty() && self.subscriptions.is_empty()
    }
}

#[cfg(test)]
struct PublicationTestGate {
    entered: Notify,
    resume: Notify,
}

#[cfg(test)]
impl SessionState {
    async fn pause_publication_for_test(&self, operation: &'static str) {
        let gate = {
            let mut slot = lock(&self.publication_test_gate);
            if slot.as_ref().is_some_and(|(kind, _)| *kind == operation) {
                slot.take().map(|(_, gate)| gate)
            } else {
                None
            }
        };
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.resume.notified().await;
        }
    }
}

/// Session state shared with the host (routing) and the op tasks.
/// This session's scan slot (X-R2): reserved before the first await so two
/// concurrent starts admit exactly one. `Starting` rolls back to `Idle`
/// when the join fails; only `Active` answers `scan.stop`. The host clears
/// either armed state by membership when it ends the scan underneath us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScanSlot {
    Idle,
    Starting { membership: String },
    Active { membership: String },
}

impl ScanSlot {
    fn membership(&self) -> Option<&str> {
        match self {
            ScanSlot::Idle => None,
            ScanSlot::Starting { membership } | ScanSlot::Active { membership } => Some(membership),
        }
    }
}

/// Process-monotonic session-instance mint: [`MobileSession::instance_key`]
/// must never repeat for a new session, which a pointer address cannot
/// promise once the allocation is freed (the allocator hands it back and a
/// new session inherits the dead one's client state).
static NEXT_INSTANCE: AtomicUsize = AtomicUsize::new(1);

fn reclaim_secret_matches(expected: &str, supplied: &str) -> bool {
    expected.len() == supplied.len()
        && expected
            .as_bytes()
            .iter()
            .zip(supplied.as_bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

#[derive(Clone)]
struct SessionReclaim {
    owner: Arc<
        ubm_desktop::reclaim_reservation::ReclaimReservationOwner<crate::foreign::ForeignRadio>,
    >,
    recording_owner:
        Arc<ubm_desktop::ordered_recording::OrderedRecording<crate::foreign::ForeignRadio>>,
    previous: u64,
    id: u64,
    peer: String,
    recording: String,
    capability: String,
    lease: String,
    caller_key: String,
}

struct ReclaimForgetReceipt {
    peer: String,
    recording: String,
    capability: String,
    caller_key: String,
}

const RECLAIM_RECEIPT_CAPACITY: usize = 128;

pub(crate) struct SessionState {
    #[cfg(test)]
    publication_test_gate: Mutex<Option<(&'static str, Arc<PublicationTestGate>)>>,
    #[cfg(test)]
    disconnect_failure_override: Mutex<Option<DesktopError>>,
    pub id: u64,
    /// Process-unique instance identity, shared by clones, never reused.
    pub instance: usize,
    pub outbox: Outbox,
    ops: Mutex<OpTable>,
    idle: Notify,
    admission: Mutex<AdmissionState>,
    release_guard: tokio::sync::Mutex<()>,
    release_version: AtomicU64,
    last_release: Mutex<Option<Vec<Value>>>,
    /// Radio progress of every live op except `counters.describe` (which
    /// never counts itself), keyed by a per-session sequence.
    op_radio: Mutex<HashMap<u64, Arc<OpRadio>>>,
    op_sequence: AtomicU64,
    recording_start_last: Mutex<Option<u64>>,
    reclaim_guards: Mutex<HashMap<u64, Weak<tokio::sync::Mutex<()>>>>,
    reclaim_reservations: Mutex<HashMap<u64, SessionReclaim>>,
    reclaim_forget_receipts: Mutex<HashMap<(u64, String, u64), ReclaimForgetReceipt>>,
    leases: Mutex<HashMap<String, OwnedLease>>,
    exact_leases: Mutex<Option<ExactLeases>>,
    exact_connection_gates: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    subscriptions: Mutex<HashMap<String, Subscription>>,
    pending_subscriptions: Mutex<HashSet<String>>,
    /// Reset outcomes retain the admitted attachment and operation, including
    /// failures, until the corresponding owner can observe them.
    reset_changed: Notify,
    reset_leases: Mutex<HashMap<OperationAdmission, (String, OwnedLease, Option<DesktopError>)>>,
    reset_subscriptions:
        Mutex<HashMap<OperationAdmission, (String, Subscription, Option<DesktopError>)>>,
    scan: Mutex<ScanSlot>,
    // None until negotiation or the first scan admission freezes the wire.
    pub(crate) scan_end_version: Mutex<Option<u8>>,
    scan_ordinal: AtomicU64,
    pub background_scope: BackgroundScope,
}

struct SubscriptionReservation<'a> {
    pending: &'a Mutex<HashSet<String>>,
    consumer: &'a str,
}
impl Drop for SubscriptionReservation<'_> {
    fn drop(&mut self) {
        lock(self.pending).remove(self.consumer);
    }
}

struct AdmissionState {
    closing: bool,
    ordinary_ops: u64,
}

impl SessionState {
    pub(crate) fn new(
        id: u64,
        outbox: Outbox,
        background_scope: BackgroundScope,
        exact_namespace: Option<String>,
    ) -> Self {
        Self {
            #[cfg(test)]
            publication_test_gate: Mutex::new(None),
            #[cfg(test)]
            disconnect_failure_override: Mutex::new(None),
            id,
            instance: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
            outbox,
            ops: Mutex::new(OpTable {
                live: HashMap::new(),
                highest_admission: 0,
                cancelled_ahead: BTreeSet::new(),
            }),
            idle: Notify::new(),
            admission: Mutex::new(AdmissionState {
                closing: false,
                ordinary_ops: 0,
            }),
            release_guard: tokio::sync::Mutex::new(()),
            release_version: AtomicU64::new(0),
            last_release: Mutex::new(None),
            op_radio: Mutex::new(HashMap::new()),
            op_sequence: AtomicU64::new(0),
            recording_start_last: Mutex::new(None),
            reclaim_guards: Mutex::new(HashMap::new()),
            reclaim_reservations: Mutex::new(HashMap::new()),
            reclaim_forget_receipts: Mutex::new(HashMap::new()),
            leases: Mutex::new(HashMap::new()),
            exact_leases: Mutex::new(exact_namespace.map(|namespace| ExactLeases {
                namespace,
                next: 1,
                records: HashMap::new(),
                live_limit: DEFAULT_EXACT_LIVE_LEASE_LIMIT,
                receipt_limit: DEFAULT_EXACT_RECEIPT_LIMIT,
                receipts: VecDeque::new(),
            })),
            exact_connection_gates: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            pending_subscriptions: Mutex::new(HashSet::new()),
            reset_changed: Notify::new(),
            reset_leases: Mutex::new(HashMap::new()),
            reset_subscriptions: Mutex::new(HashMap::new()),
            scan: Mutex::new(ScanSlot::Idle),
            scan_end_version: Mutex::new(None),
            scan_ordinal: AtomicU64::new(0),
            background_scope,
        }
    }

    /// The host ended this session's scan membership.
    pub(crate) fn clear_scan(&self, membership: &str) {
        let mut scan = lock(&self.scan);
        if scan.membership() == Some(membership) {
            *scan = ScanSlot::Idle;
        }
    }

    /// Snapshot only resources belonging to the attachment the core reset.
    pub(crate) fn capture_reset_resources(
        &self,
        previous: &AttachmentTuple,
        released: &[String],
        failures: &[(String, DesktopError)],
    ) -> ResetResources {
        let failure_for = |peer: &str| {
            failures
                .iter()
                .find(|(id, _)| id == peer)
                .map(|(_, error)| error.clone())
                .or_else(|| {
                    (!released.iter().any(|id| id == peer)).then(|| {
                        error(
                            BleErrorCode::LifecycleInvariantViolation,
                            BleErrorDomain::Cleanup,
                            "adapter.reset.release-observation",
                        )
                        .with_detail("reset did not report a release for the admitted peer")
                    })
                })
        };
        let leases = if let Some(table) = lock(&self.exact_leases).as_ref() {
            table
                .records
                .values()
                .filter_map(|entry| {
                    let lease = entry.owner.as_ref()?;
                    (!entry.released && &lease.attachment == previous)
                        .then(|| (entry.peer.clone(), lease.clone(), failure_for(&entry.peer)))
                })
                .collect()
        } else {
            lock(&self.leases)
                .iter()
                .filter(|(_, lease)| &lease.attachment == previous)
                .map(|(peer, lease)| (peer.clone(), lease.clone(), failure_for(peer)))
                .collect()
        };
        let subscriptions = lock(&self.subscriptions)
            .iter()
            .filter(|(_, subscription)| &subscription.attachment == previous)
            .map(|(consumer, subscription)| {
                (
                    consumer.clone(),
                    subscription.clone(),
                    failure_for(&subscription.peer_id),
                )
            })
            .collect();
        ResetResources {
            leases,
            subscriptions,
        }
    }

    /// Retire only the exact attachment-owned resources captured at reset.
    pub(crate) fn end_by_reset(&self, captured: ResetResources) -> Vec<(InstanceKey, String)> {
        let mut leases = lock(&self.leases);
        let mut ended_leases = lock(&self.reset_leases);
        for (peer, lease, failure) in captured.leases {
            if leases.get(&peer) == Some(&lease) {
                leases.remove(&peer);
            }
            ended_leases
                .entry(lease.admission.clone())
                .or_insert((peer, lease, failure));
        }
        drop(ended_leases);
        drop(leases);
        let mut subscriptions = lock(&self.subscriptions);
        let mut ended = lock(&self.reset_subscriptions);
        let mut routes = Vec::new();
        for (consumer, subscription, failure) in captured.subscriptions {
            if subscriptions
                .get(&consumer)
                .is_some_and(|current| current.admission == subscription.admission)
            {
                subscriptions.remove(&consumer);
                routes.push((subscription.scope.clone(), consumer.clone()));
            }
            ended.entry(subscription.admission.clone()).or_insert((
                consumer,
                subscription,
                failure,
            ));
        }
        drop(ended);
        drop(subscriptions);
        self.reset_changed.notify_waiters();
        routes
    }

    fn core_name(&self, name: &str) -> String {
        format!("s{}:{name}", self.id)
    }

    fn exact_connection_gate(&self, peer: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = lock(&self.exact_connection_gates);
        gates.retain(|_, gate| gate.strong_count() != 0);
        let slot = gates.entry(peer.to_owned()).or_default();
        if let Some(gate) = slot.upgrade() {
            gate
        } else {
            let gate = Arc::new(tokio::sync::Mutex::new(()));
            *slot = Arc::downgrade(&gate);
            gate
        }
    }
}

pub(crate) struct RecordingStartReceipt {
    recording_id: String,
    caller_key: String,
    owner_collection: Option<u64>,
    revoked: bool,
    identity: Option<(Value, ubm_desktop::continuation_journal::JournalQuota)>,
    reclaim_capability: Option<String>,
    result: Option<Value>,
    failed: bool,
}

struct RecordingStartGuard {
    host: Arc<HostInner>,
    key: (u64, u64),
    completed: bool,
}
impl Drop for RecordingStartGuard {
    fn drop(&mut self) {
        if !self.completed {
            if let Some(entry) = lock(&self.host.ordered_start_receipts).get_mut(&self.key) {
                entry.failed = true;
            }
        }
    }
}

/// One RN manager's session on the process owner.
#[derive(Clone)]
pub struct MobileSession {
    host: Arc<HostInner>,
    state: Arc<SessionState>,
}

pub(crate) fn cleanup_failure(resource_kind: &str, error: &DesktopError) -> Value {
    let mut map = wire::error_object(error);
    map.insert("resourceKind".to_owned(), Value::from(resource_kind));
    Value::Object(map)
}

// Desktop's cleanup aggregate stores the primary as element zero and every
// independent error in order. Unwrap only that explicit wrapper; a regular
// platform error can carry its own cleanup evidence and is not an aggregate.
fn connection_cleanup_failures(error: &DesktopError) -> Vec<Value> {
    let mut pending = vec![error];
    let mut failures = Vec::new();
    while let Some(error) = pending.pop() {
        match error.platform() {
            Some(platform)
                if platform.code == "cleanup-failures" && !platform.cleanup_failures.is_empty() =>
            {
                pending.extend(platform.cleanup_failures.iter().rev());
            }
            _ => failures.push(cleanup_failure("connection", error)),
        }
    }
    failures
}

fn connection_cleanup_record(error: &DesktopError) -> Value {
    cleanup_record(connection_cleanup_failures(error))
}

pub(crate) fn cleanup_record(failures: Vec<Value>) -> Value {
    object(vec![
        (
            "state",
            Value::from(if failures.is_empty() {
                "released"
            } else {
                "release-failed"
            }),
        ),
        ("failures", Value::Array(failures)),
    ])
}

/// The reset reports physical release only after its teardown completes.
/// Subscribe before admission so even a fast reset remains observable.
async fn reset_release_failure(
    events: &mut tokio::sync::broadcast::Receiver<ubm_desktop::AdapterResetEvent>,
    attachment: &AttachmentTuple,
    peer: &str,
) -> Option<DesktopError> {
    loop {
        match events.recv().await {
            Ok(event) if &event.previous == attachment => {
                if let Some((_, failure)) = event
                    .link_release_failures
                    .into_iter()
                    .find(|(id, _)| id == peer)
                {
                    return Some(failure);
                }
                return if event.released_links.iter().any(|id| id == peer) {
                    None
                } else {
                    Some(
                        error(
                            BleErrorCode::LifecycleInvariantViolation,
                            BleErrorDomain::Cleanup,
                            "adapter.reset.release-observation",
                        )
                        .with_detail("reset did not report a release for the admitted peer"),
                    )
                };
            }
            Ok(_) => {}
            Err(failure) => {
                return Some(
                    error(
                        BleErrorCode::PlatformFailure,
                        BleErrorDomain::Cleanup,
                        "adapter.reset.release-observation",
                    )
                    .with_detail(failure.to_string()),
                );
            }
        }
    }
}

fn error(code: BleErrorCode, domain: BleErrorDomain, operation: &str) -> DesktopError {
    let error = DesktopError::new(code, domain, operation);
    if matches!(
        code,
        BleErrorCode::OperationAborted | BleErrorCode::OperationTimedOut
    ) {
        error.with_outcome(None, ubm_desktop::Retryability::CallerDecides)
    } else {
        error
    }
}

fn ordered_encoding_error() -> DesktopError {
    error(
        BleErrorCode::ProtocolIncompatible,
        BleErrorDomain::Core,
        "gatt.ordered.failure-metadata-version",
    )
    .with_detail("ordered failure cannot be represented by the selected mobile wire revision")
}

fn ordered_discard_value(
    receipt: &ubm_desktop::central::OrderedDiscardReceipt,
) -> Result<Value, DesktopError> {
    let encoded = crate::ordered_encoding::discard(receipt).ok_or_else(ordered_encoding_error)?;
    Ok(
        serde_json::json!({"receiptId":receipt.receipt_id.to_string(),"discardedDeliveryId":receipt.discarded_delivery_id.to_string(),"evidence":encoded["evidence"].to_string()}),
    )
}

fn recording_storage_error(failure: &Value, operation: &str) -> DesktopError {
    let code = if failure["code"] == "argument.invalid" {
        BleErrorCode::ArgumentInvalid
    } else {
        BleErrorCode::PlatformFailure
    };
    let detail = failure["detail"]
        .as_str()
        .unwrap_or("recording storage operation failed");
    let platform_code = failure["platform"]["code"]
        .as_str()
        .unwrap_or("recording-worker-failed");
    error(code, BleErrorDomain::Platform, operation)
        .with_detail(detail)
        .with_platform(
            ubm_desktop::PlatformDetail::new("sqlite", platform_code).with_message(detail),
        )
}

fn unsupported(operation: &str, detail: &str) -> DesktopError {
    error(
        BleErrorCode::CapabilityUnsupported,
        BleErrorDomain::Capability,
        operation,
    )
    .with_detail(detail.to_owned())
}

fn budget(args: &Args, received: Instant) -> Result<Budget, DesktopError> {
    Ok(match args.opt_integer("budgetMs", MAX_TIMEOUT_MS)? {
        None => Budget::unbounded(),
        Some(0) => return Err(wire::invalid("args.budgetMs")),
        Some(ms) => Budget::from_ms_at(received, ms),
    })
}

fn characteristic_selector(args: &Args) -> Result<PathSelector, DesktopError> {
    let selector = args.object("selector")?;
    selector.exact(
        &[
            "serviceUuid",
            "serviceOccurrence",
            "characteristicUuid",
            "characteristicOccurrence",
        ],
        &[],
    )?;
    build_selector(&selector, false)
}

fn descriptor_selector(args: &Args) -> Result<PathSelector, DesktopError> {
    let selector = args.object("selector")?;
    selector.exact(
        &[
            "serviceUuid",
            "serviceOccurrence",
            "characteristicUuid",
            "characteristicOccurrence",
            "descriptorUuid",
            "descriptorOccurrence",
        ],
        &[],
    )?;
    build_selector(&selector, true)
}

fn build_selector(selector: &Args, descriptor: bool) -> Result<PathSelector, DesktopError> {
    let max = wire::MAX_SAFE_INTEGER;
    let descriptor_uuid = if descriptor {
        Some(selector.string("descriptorUuid")?)
    } else {
        None
    };
    let descriptor_occurrence = if descriptor {
        Some(selector.integer("descriptorOccurrence", max)?)
    } else {
        None
    };
    DesktopCentral::<crate::foreign::ForeignRadio>::selector(
        &selector.string("serviceUuid")?,
        Some(selector.integer("serviceOccurrence", max)?),
        Some(&selector.string("characteristicUuid")?),
        Some(selector.integer("characteristicOccurrence", max)?),
        descriptor_uuid.as_deref(),
        descriptor_occurrence,
    )
}

fn scope_of(peer_id: &str, selector: &PathSelector) -> InstanceKey {
    (
        peer_id.to_owned(),
        selector.service_uuid.clone(),
        selector.service_occurrence.unwrap_or(0),
        selector.characteristic_uuid.clone().unwrap_or_default(),
        selector.characteristic_occurrence.unwrap_or(0),
    )
}

fn instance_of(peer_id: &str, selector: &PathSelector) -> Instance {
    let scope = scope_of(peer_id, selector);
    Instance {
        peer_id: scope.0,
        service_uuid: scope.1,
        service_occurrence: scope.2,
        characteristic_uuid: scope.3,
        characteristic_occurrence: scope.4,
    }
}

fn canonical_address(value: &str) -> Result<String, DesktopError> {
    let upper = value.to_ascii_uppercase();
    let bytes: Vec<&str> = upper.split(':').collect();
    let valid = bytes.len() == 6
        && bytes
            .iter()
            .all(|byte| byte.len() == 2 && byte.bytes().all(|b| b.is_ascii_hexdigit()));
    if valid {
        Ok(upper)
    } else {
        Err(wire::invalid("args.deviceAddresses"))
    }
}

fn phy(text: &str) -> Option<Phy> {
    match text {
        "le-1m" => Some(Phy::Le1m),
        "le-2m" => Some(Phy::Le2m),
        "le-coded" => Some(Phy::LeCoded),
        _ => None,
    }
}

const PHYS: &[&str] = &["le-1m", "le-2m", "le-coded"];

fn phy_value(observation: crate::radio::PhyObservation) -> Value {
    object(vec![
        ("tx", Value::from(observation.tx.as_str())),
        ("rx", Value::from(observation.rx.as_str())),
    ])
}

/// (uuid, occurrence, property bits, descriptor records)
type CharacteristicEntry = (String, u64, u8, Vec<Value>);

fn discovery_tree(paths: &[DiscoveredPath]) -> Value {
    let mut services: Vec<(String, u64, Vec<CharacteristicEntry>)> = Vec::new();
    for path in paths {
        let service = match services.iter().position(|(uuid, occ, _)| {
            *uuid == path.service_uuid && *occ == path.service_occurrence
        }) {
            Some(index) => index,
            None => {
                services.push((
                    path.service_uuid.clone(),
                    path.service_occurrence,
                    Vec::new(),
                ));
                services.len() - 1
            }
        };
        let (Some(characteristic_uuid), Some(characteristic_occurrence)) =
            (&path.characteristic_uuid, path.characteristic_occurrence)
        else {
            continue;
        };
        let characteristics = &mut services[service].2;
        let characteristic = match characteristics.iter().position(|(uuid, occ, _, _)| {
            uuid == characteristic_uuid && *occ == characteristic_occurrence
        }) {
            Some(index) => index,
            None => {
                characteristics.push((
                    characteristic_uuid.clone(),
                    characteristic_occurrence,
                    0,
                    Vec::new(),
                ));
                characteristics.len() - 1
            }
        };
        let entry = &mut characteristics[characteristic];
        match (&path.descriptor_uuid, path.descriptor_occurrence) {
            (Some(uuid), Some(occurrence)) => entry.3.push(object(vec![
                ("uuid", Value::from(uuid.as_str())),
                ("occurrence", Value::from(occurrence)),
            ])),
            _ => entry.2 = path.properties & wire::GATT_PROPERTY_MASK,
        }
    }
    Value::Array(
        services
            .into_iter()
            .map(|(uuid, occurrence, characteristics)| {
                let metadata = paths.iter().find(|path| {
                    path.service_uuid == uuid
                        && path.service_occurrence == occurrence
                        && path.characteristic_uuid.is_none()
                });
                object(vec![
                    (
                        "primary",
                        metadata
                            .and_then(|path| path.service_primary)
                            .map_or(Value::Null, Value::from),
                    ),
                    (
                        "includedServices",
                        metadata
                            .and_then(|path| path.included_services.as_ref())
                            .map_or(Value::Null, |references| {
                                Value::Array(
                                    references
                                        .iter()
                                        .map(|reference| {
                                            object(vec![
                                                ("uuid", Value::from(reference.uuid.as_str())),
                                                ("occurrence", Value::from(reference.occurrence)),
                                            ])
                                        })
                                        .collect(),
                                )
                            }),
                    ),
                    ("uuid", Value::from(uuid)),
                    ("occurrence", Value::from(occurrence)),
                    (
                        "characteristics",
                        Value::Array(
                            characteristics
                                .into_iter()
                                .map(|(uuid, occurrence, properties, descriptors)| {
                                    object(vec![
                                        ("uuid", Value::from(uuid)),
                                        ("occurrence", Value::from(occurrence)),
                                        ("properties", Value::from(properties)),
                                        ("descriptors", Value::Array(descriptors)),
                                    ])
                                })
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect(),
    )
}

/// Bound one direct radio exchange by the caller's budget (the liveness
/// backstop without one) and the op's cancel ticket.
pub(crate) async fn bounded<T>(
    ctl: &OpControl,
    operation: &str,
    work: impl Future<Output = Result<T, DesktopError>>,
) -> Result<T, DesktopError> {
    let aborted = || {
        error(
            BleErrorCode::OperationAborted,
            BleErrorDomain::Connection,
            operation,
        )
    };
    if ctl.ticket.is_cancel_requested() {
        return Err(aborted());
    }
    let limit = ctl.budget.bound(LIVENESS_OP);
    let backstop = ctl.budget.deadline().is_none();
    tokio::select! {
        biased;
        () = ctl.ticket.cancelled() => Err(aborted()),
        outcome = tokio::time::timeout(limit, work) => outcome.unwrap_or_else(|_| {
            let timed_out = error(BleErrorCode::OperationTimedOut, BleErrorDomain::Connection, operation);
            Err(if backstop { timed_out.with_detail(LIVENESS_BACKSTOP_DETAIL) } else { timed_out })
        }),
    }
}

/// A wait on the user or the OS (pairing, its cancellation, the companion
/// chooser; finding 123): legacy React Native waited for these without a
/// deadline, so without a caller budget there is no liveness backstop. A
/// caller budget is still the deadline, and a cancel still ends it.
async fn awaited<T>(
    ctl: &OpControl,
    operation: &str,
    work: impl Future<Output = Result<T, DesktopError>>,
) -> Result<T, DesktopError> {
    if ctl.budget.deadline().is_some() {
        return bounded(ctl, operation, work).await;
    }
    if ctl.ticket.is_cancel_requested() {
        return Err(error(
            BleErrorCode::OperationAborted,
            BleErrorDomain::Connection,
            operation,
        ));
    }
    tokio::select! {
        biased;
        () = ctl.ticket.cancelled() => Err(error(
            BleErrorCode::OperationAborted,
            BleErrorDomain::Connection,
            operation,
        )),
        outcome = work => outcome,
    }
}

fn commit_of(error: &DesktopError, dispatched: bool) -> &'static str {
    match error.commit() {
        Some(CommitState::NotDispatched) => "not-dispatched",
        Some(_) => "uncertain",
        None if dispatched => "uncertain",
        None => "not-dispatched",
    }
}

impl MobileSession {
    /// Secret authority for recovering this session's scan evidence after its
    /// disposal. Persist it before admitting a scan and protect it as a secret.
    pub(crate) fn scan_failure_recovery_authority(&self) -> Result<[u8; 32], DesktopError> {
        let admission = lock(&self.state.admission);
        if self.host.is_shut_down() || admission.closing {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                "scan.failure-recovery-authority",
            ));
        }
        self.state
            .outbox
            .scan_failure_recovery_authority()
            .map_err(|_| {
                DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Core,
                    "scan.failure-recovery-authority",
                )
            })?
            .ok_or_else(|| {
                DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Core,
                    "scan.failure-recovery-authority",
                )
            })
    }

    /// Select the scan terminal format before this session admits a scan.
    pub fn negotiate_scan_end_version(&self, offered_json: &str) -> Result<String, DesktopError> {
        self.negotiate_scan_end_version_with_max(offered_json, 2)
    }

    /// Apply a host qualification ceiling without altering the peer's offer.
    pub fn negotiate_scan_end_version_with_max(
        &self,
        offered_json: &str,
        max_version: u8,
    ) -> Result<String, DesktopError> {
        let admission = lock(&self.state.admission);
        if self.host.is_shut_down() || admission.closing {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                "scan-end-version",
            ));
        }
        let offered: Vec<u8> =
            serde_json::from_str(offered_json).map_err(|_| wire::invalid("scan-end-version"))?;
        if offered.is_empty() || offered.len() > 16 {
            return Err(wire::invalid("scan-end-version"));
        }
        let mut selected = lock(&self.state.scan_end_version);
        let version = match *selected {
            Some(version) if version <= max_version && offered.contains(&version) => version,
            Some(_) => {
                return Err(DesktopError::new(
                    BleErrorCode::ProtocolIncompatible,
                    BleErrorDomain::Core,
                    "scan-end-version",
                ));
            }
            None if max_version >= 2 && offered.contains(&2) => 2,
            None if max_version >= 1 && offered.contains(&1) => 1,
            None => {
                return Err(DesktopError::new(
                    BleErrorCode::ProtocolIncompatible,
                    BleErrorDomain::Core,
                    "scan-end-version",
                ));
            }
        };
        *selected = Some(version);
        Ok(serde_json::json!({"version":version}).to_string())
    }
    pub(crate) fn seal_continuation_collection(&self) -> bool {
        self.host.seal_scan_ingress(&self.state).is_some()
    }
    pub(crate) fn continuation_collection_sealed(&self) -> bool {
        self.state.outbox.is_sealed()
    }
    pub(crate) fn attach_continuation_journal(
        &self,
        journal: Arc<ubm_desktop::continuation_journal::ContinuationJournal>,
        mut context: Value,
    ) -> ubm_desktop::continuation::Result<()> {
        context["sessionId"] = Value::from(self.id().to_string());
        context["backendInstanceId"] = Value::from(
            self.host
                .central
                .attachment()
                .backend_instance_id()
                .as_str(),
        );
        context["sessionStartedAtUnixNs"]=Value::from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_|serde_json::json!({"code":"platform.failure","domain":"restoration","operation":"continuation.recording","detail":"recording session clock unavailable"}))?.as_nanos().to_string());
        context["sessionEpoch"] = Value::from(format!(
            "{}:{}:{}",
            context["backendInstanceId"]
                .as_str()
                .expect("backend instance"),
            self.id(),
            context["sessionStartedAtUnixNs"]
                .as_str()
                .expect("session clock")
        ));
        self.state
            .outbox
            .attach_journal(journal, context)
            .map_err(ubm_desktop::continuation::recording_failure)
    }
    pub(crate) fn register_continuation_consumer(
        &self,
        consumer: &str,
        metadata: Value,
    ) -> ubm_desktop::continuation::Result<()> {
        self.state
            .outbox
            .register_journal_consumer(consumer, metadata)
            .map_err(ubm_desktop::continuation::recording_failure)
    }
    pub(crate) fn observe_continuation(
        &self,
        consumer: &str,
        matcher: ubm_desktop::continuation_outbox::RecordMatcher,
    ) -> Result<ubm_desktop::continuation_outbox::Observation, &'static str> {
        self.state.outbox.observe(consumer, matcher)
    }
    pub(crate) fn new(host: Arc<HostInner>, state: Arc<SessionState>) -> Self {
        Self { host, state }
    }

    #[must_use]
    pub fn id(&self) -> u64 {
        self.state.id
    }

    /// Configure the experimental /4 session's retained successful connection
    /// receipts (default 1,024). Evicted identities fail closed; live owners
    /// and failed cleanup are never evicted. The limit may be zero and is
    /// capped at 65,536. Admission separately bounds live and unresolved
    /// connection records using `set_exact_connection_live_limit`.
    pub fn set_exact_connection_receipt_limit(&self, limit: usize) -> Result<(), DesktopError> {
        if limit > 65_536 {
            return Err(wire::invalid("session.exact-connection-receipt-limit"));
        }
        let mut guard = lock(&self.state.exact_leases);
        let table = guard.as_mut().ok_or_else(|| {
            unsupported(
                "session.exact-connection-receipt-limit",
                "requires experimental wire revision 4",
            )
        })?;
        table.receipt_limit = limit;
        table.trim_receipts();
        Ok(())
    }

    /// Set the experimental /4 session's maximum admitted or unresolved
    /// connection owners. The default is 1,024; zero refuses new owners.
    /// Reducing the limit never evicts existing owners or failed cleanup.
    pub fn set_exact_connection_live_limit(&self, limit: usize) -> Result<(), DesktopError> {
        let mut guard = lock(&self.state.exact_leases);
        let table = guard.as_mut().ok_or_else(|| {
            unsupported(
                "session.exact-connection-live-limit",
                "requires experimental wire revision 4",
            )
        })?;
        table.live_limit = limit;
        Ok(())
    }

    /// Identity of this session object across hosts (session ids restart
    /// per host): clones share it, and it is never reused after the session
    /// dies, so clients keeping per-session state cannot inherit a dead
    /// session's state when the allocator recycles the address.
    #[must_use]
    pub fn instance_key(&self) -> usize {
        self.state.instance
    }

    /// Take queued records (see [`crate::drain`]).
    #[must_use]
    pub fn drain(&self, max_items: u32, max_bytes: u32) -> String {
        self.state
            .outbox
            .drain(max_items as usize, max_bytes as usize)
            .to_string()
    }

    /// Run one op; `completion` receives the envelope text exactly once.
    pub fn invoke(&self, op: &str, args_json: &str, completion: Completion) {
        if op == "scan.start" {
            lock(&self.state.scan_end_version).get_or_insert(1);
        }
        let received = Instant::now();
        let lifetime_received = tokio::time::Instant::now();
        let is_effectful = EFFECTFUL_OPS.contains(&op);
        let reject = |error: DesktopError, completion: Completion| {
            completion(wire::error_envelope(
                &error,
                is_effectful.then_some("not-dispatched"),
            ));
        };
        let Some(op) = OPS.iter().copied().find(|known| *known == op) else {
            return reject(wire::invalid("op"), completion);
        };
        // A disposed session still answers `counters.describe` (a read): it
        // reports what the session still holds — nothing after a clean
        // dispose — so a manager can confirm its own return to baseline.
        if (self.host.is_shut_down() || lock(&self.state.admission).closing)
            && op != "session.dispose"
            && op != "counters.describe"
            && op != "session.quiesce"
            && op != "session.continuation-dispose"
        {
            return reject(
                error(BleErrorCode::LifecycleDestroyed, BleErrorDomain::Core, op)
                    .with_detail("session disposed"),
                completion,
            );
        }
        let mut args = match wire::parse_args(op, args_json) {
            Ok(args) => args,
            Err(error) => return reject(error, completion),
        };
        let admission = match args.take_admission() {
            Ok(admission) => admission,
            Err(error) => return reject(error, completion),
        };
        // `op.cancel` names its target's admission; every other admission is
        // consumed on arrival, even when the invoke is then refused, so an
        // admission at or below the highest one never runs again.
        if op != "op.cancel"
            && let Some(admission) = admission
        {
            let mut ops = lock(&self.state.ops);
            if admission <= ops.highest_admission {
                drop(ops);
                return reject(wire::invalid("args.admission"), completion);
            }
            ops.highest_admission = admission;
            let cancelled = ops.cancelled_ahead.remove(&admission);
            ops.cancelled_ahead = ops.cancelled_ahead.split_off(&admission);
            if cancelled {
                drop(ops);
                return reject(
                    error(
                        BleErrorCode::OperationAborted,
                        BleErrorDomain::Connection,
                        op,
                    )
                    .with_detail("cancelled before admission"),
                    completion,
                );
            }
        }
        let command = match self.parse(op, &args, admission, received, lifetime_received) {
            Ok(command) => command,
            Err(error) => return reject(error, completion),
        };
        if op != "op.cancel" && command.operation_id.is_some() != admission.is_some() {
            return reject(wire::invalid("args.admission"), completion);
        }
        let gatt_admission = match &command.body {
            Body::Read { peer_id, .. }
            | Body::Write { peer_id, .. }
            | Body::Subscribe { peer_id, .. }
            | Body::Unsubscribe { peer_id, .. }
            | Body::Discover { peer_id, .. } => match self.host.central.admit_gatt(peer_id) {
                Ok(admission) => Some(admission),
                Err(error) => return reject(error, completion),
            },
            Body::Ordered(crate::ordered_public::OrderedCommand::Open { peer, .. }) => {
                match self.host.central.admit_gatt(peer) {
                    Ok(admission) => Some(admission),
                    Err(error) => return reject(error, completion),
                }
            }
            _ => None,
        };
        let is_disposal = matches!(command.body, Body::Dispose | Body::ContinuationDispose);
        let release_version = self.state.release_version.load(Ordering::SeqCst);
        // This is the definitive admission boundary. Teardown closes it under
        // the same lock, so an operation cannot appear after the drain check.
        let mut gate = lock(&self.state.admission);
        if (self.host.is_shut_down() || gate.closing)
            && op != "session.dispose"
            && op != "session.continuation-dispose"
            && op != "counters.describe"
            && op != "session.quiesce"
        {
            drop(gate);
            return reject(
                error(BleErrorCode::LifecycleDestroyed, BleErrorDomain::Core, op)
                    .with_detail("session disposed"),
                completion,
            );
        }
        let ticket = OpTicket::new();
        if let (Some(id), Some(admission)) = (&command.operation_id, admission) {
            let mut ops = lock(&self.state.ops);
            if ops.live.contains_key(id) {
                drop(ops);
                return reject(wire::invalid("args.operationId"), completion);
            }
            ops.live.insert(id.clone(), (ticket.clone(), admission));
        }
        if !is_disposal {
            gate.ordinary_ops += 1;
        }
        drop(gate);
        let progress = Arc::new(OpRadio::default());
        let tracked = (!matches!(command.body, Body::Counters)).then(|| {
            let key = self.state.op_sequence.fetch_add(1, Ordering::Relaxed);
            lock(&self.state.op_radio).insert(key, Arc::clone(&progress));
            key
        });
        let session = self.clone();
        let ctl = OpControl::new(command.budget, ticket);
        let ctl = match gatt_admission {
            Some(admission) => ctl.with_gatt_admission(admission),
            None => ctl,
        };
        self.host.runtime.spawn(async move {
            let operation_id = command.operation_id.clone();
            let executing = session.clone();
            let operation = async move {
                OP_RADIO
                    .scope(
                        progress,
                        DISPATCHED.scope(Cell::new(false), async {
                            let outcome =
                                executing.execute(command.body, ctl, release_version).await;
                            (outcome, DISPATCHED.with(Cell::get))
                        }),
                    )
                    .await
            };
            let (outcome, dispatched) = if session.state.outbox.has_journal() {
                let runtime = session.host.runtime.clone();
                match tokio::task::spawn_blocking(move || runtime.block_on(operation)).await {
                    Ok(result) => result,
                    Err(_) => (
                        Err(
                            error(BleErrorCode::PlatformFailure, BleErrorDomain::Platform, op)
                                .with_detail(
                                    "native continuation operation worker did not complete",
                                ),
                        ),
                        true,
                    ),
                }
            } else {
                operation.await
            };
            if let Some(key) = tracked {
                lock(&session.state.op_radio).remove(&key);
            }
            let text = match outcome {
                Ok(value) => wire::ok_envelope(value),
                Err(error) => wire::error_envelope(
                    &error,
                    is_effectful.then(|| commit_of(&error, dispatched)),
                ),
            };
            if let Some(id) = operation_id {
                lock(&session.state.ops).live.remove(&id);
            }
            completion(text);
            if !is_disposal {
                let mut gate = lock(&session.state.admission);
                gate.ordinary_ops -= 1;
                drop(gate);
                session.state.idle.notify_waiters();
            }
        });
    }

    /// Async convenience over [`Self::invoke`] (tests, Rust hosts).
    pub async fn call(&self, op: &str, args_json: &str) -> String {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.invoke(
            op,
            args_json,
            Box::new(move |text| {
                let _ = tx.send(text);
            }),
        );
        rx.await.unwrap_or_default()
    }

    fn operation_id(args: &Args) -> Result<Option<String>, DesktopError> {
        args.opt_string("operationId")
    }

    fn parse(
        &self,
        op: &'static str,
        args: &Args,
        admission: Option<u64>,
        received: Instant,
        lifetime_received: tokio::time::Instant,
    ) -> Result<Command, DesktopError> {
        let platform = self.host.platform;
        let apple = platform == MobilePlatform::Apple;
        let id_required = |args: &Args| -> Result<Option<String>, DesktopError> {
            Ok(Some(args.string("operationId")?))
        };
        let (body, operation_id, budget) = match op {
            "terminal-evidence.reserve" => {
                args.exact(
                    &["producer", "maximumBytes", "formatVersion", "scope"],
                    &["scopeUtf16leBase64"],
                )?;
                let producer = args.one_of("producer", &["scan", "subscription"])?;
                if args.integer("formatVersion", 1)? != 1 {
                    return Err(wire::invalid("args.formatVersion"));
                }
                let scope = args.object("scope")?;
                scope.exact(&["managerId", "operationId", "incarnation"], &[])?;
                (
                    Body::TerminalEvidenceReserve {
                        scope: TerminalEvidenceScope {
                            producer: producer.to_owned(),
                            manager_id: scope.string("managerId")?,
                            operation_id: scope.string("operationId")?,
                            incarnation: scope.string("incarnation")?,
                            scope_utf16le: args
                                .opt_string("scopeUtf16leBase64")?
                                .map(|text| wire::decode_base64(&text, "args.scopeUtf16leBase64"))
                                .transpose()?,
                        },
                        maximum_bytes: args.integer("maximumBytes", wire::MAX_SAFE_INTEGER)?,
                    },
                    None,
                    Budget::unbounded(),
                )
            }
            "terminal-evidence.commit" => {
                args.exact(&["reservationId", "canonicalBytesBase64", "digest"], &[])?;
                let reservation_id = args.string("reservationId")?;
                let ledger = self.host.terminal_evidence_ledger().ok_or_else(|| {
                    unsupported(
                        "terminal-evidence.commit",
                        "trusted terminal evidence storage was not configured",
                    )
                })?;
                let maximum_bytes = ledger
                    .reservation_maximum_bytes(&reservation_id)
                    .map_err(terminal_evidence_error)?;
                let maximum_bytes = usize::try_from(maximum_bytes).map_err(|_| {
                    error(
                        BleErrorCode::BytesTooLarge,
                        BleErrorDomain::Stream,
                        "terminal-evidence.commit",
                    )
                })?;
                let canonical_bytes = wire::decode_base64_bounded(
                    &args.string("canonicalBytesBase64")?,
                    "args.canonicalBytesBase64",
                    maximum_bytes,
                )?;
                (
                    Body::TerminalEvidenceCommit {
                        reservation_id,
                        canonical_bytes,
                        digest: terminal_digest(&args.string("digest")?)?,
                    },
                    None,
                    Budget::unbounded(),
                )
            }
            "terminal-evidence.read" => {
                args.exact(&["ref", "maximumBytes"], &[])?;
                (
                    Body::TerminalEvidenceRead {
                        reference: args.string("ref")?,
                        maximum_bytes: args.integer("maximumBytes", wire::MAX_SAFE_INTEGER)?,
                    },
                    None,
                    Budget::unbounded(),
                )
            }
            "terminal-evidence.release" => {
                args.exact(&["reservationId"], &[])?;
                (
                    Body::TerminalEvidenceRelease(args.string("reservationId")?),
                    None,
                    Budget::unbounded(),
                )
            }
            "adapter.state"
            | "connection.control-capabilities"
            | "counters.describe"
            | "peers.known"
            | "peers.restored"
            | "session.reconcile"
            | "session.quiesce"
            | "session.continuation-dispose"
            | "session.dispose" => {
                args.exact(&[], &[])?;
                let body = match op {
                    "adapter.state" => Body::AdapterState,
                    "connection.control-capabilities" => Body::ControlCapabilities,
                    "session.reconcile" => Body::Reconcile,
                    "counters.describe" => Body::Counters,
                    "peers.known" => Body::PeersKnown,
                    "peers.restored" => Body::PeersRestored,
                    "session.quiesce" => Body::Quiesce,
                    "session.continuation-dispose" => Body::ContinuationDispose,
                    _ => Body::Dispose,
                };
                (body, None, Budget::unbounded())
            }
            "peers.claim-restored" => {
                args.exact(&["maxPeers"], &[])?;
                let max_peers = args.integer("maxPeers", wire::MAX_SAFE_INTEGER)?;
                // Issue #212: Android claims the peers a Companion Device
                // Manager presence wake restored, with the same once-per-process
                // semantics as iOS state restoration.
                (Body::ClaimRestored(max_peers), None, Budget::unbounded())
            }
            "peers.connected" => {
                args.exact(&["operationId", "services"], &["budgetMs"])?;
                let services = args
                    .strings("services")?
                    .iter()
                    .map(|value| canonical_uuid(value).map_err(|_| wire::invalid("args.services")))
                    .collect::<Result<Vec<_>, _>>()?;
                if apple && services.is_empty() {
                    return Err(unsupported(
                        "peers.connected.services-required",
                        "CoreBluetooth requires a nonempty service query",
                    ));
                }
                if !apple && !services.is_empty() {
                    return Err(unsupported(
                        "peers.connected.services",
                        "Android's GATT inventory does not support a service filter",
                    ));
                }
                (
                    Body::PeersConnected(services),
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "peers.bonded" => {
                args.exact(&["operationId"], &["budgetMs"])?;
                (
                    Body::PeersBonded,
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "peers.resolve" => {
                args.exact(&["reference", "operationId"], &["budgetMs"])?;
                let reference = args.object("reference")?;
                reference.exact(&["opaqueId"], &["version", "backendId", "scope"])?;
                (
                    Body::PeersResolve(reference.string("opaqueId")?),
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "scan.start" => {
                args.exact(
                    &["serviceUuids", "duplicatePolicy", "operationId"],
                    &[
                        "deviceAddresses",
                        "platform",
                        "budgetMs",
                        "lifetimeMs",
                        "terminalEvidenceScope",
                    ],
                )?;
                let lifetime_ms = args.opt_integer("lifetimeMs", MAX_TIMEOUT_MS)?;
                if lifetime_ms == Some(0) {
                    return Err(wire::invalid("args.lifetimeMs"));
                }
                let requested_budget = budget(args, received)?;
                // Membership lifetime includes admission: a held start must
                // not acquire a resource after its entire lifetime expired.
                let admission_budget = lifetime_ms.map_or(requested_budget, |ms| {
                    let lifetime_budget = Budget::from_ms_at(received, ms);
                    match requested_budget.deadline() {
                        Some(deadline) if Some(deadline) <= lifetime_budget.deadline() => {
                            requested_budget
                        }
                        _ => lifetime_budget,
                    }
                });
                args.one_of("duplicatePolicy", &["all"]).map_err(|_| {
                    unsupported(
                        "scan.start.duplicate-policy",
                        "the radio reports every advertisement; apply first/merged above it",
                    )
                })?;
                let service_uuids = args
                    .strings("serviceUuids")?
                    .iter()
                    .map(|uuid| {
                        canonical_uuid(uuid).map_err(|_| wire::invalid("args.serviceUuids"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let device_addresses = args
                    .strings("deviceAddresses")?
                    .iter()
                    .map(|address| canonical_address(address))
                    .collect::<Result<Vec<_>, _>>()?;
                let android = match args.opt_object("platform")? {
                    None => None,
                    Some(options) => {
                        if apple {
                            return Err(unsupported(
                                "scan.start.platform-options",
                                "CoreBluetooth has no scan settings",
                            ));
                        }
                        options.exact(
                            &[],
                            &["mode", "callbackType", "legacy", "phy", "reportDelayMs"],
                        )?;
                        Some(AndroidScanOptions {
                            mode: options
                                .opt_one_of(
                                    "mode",
                                    &["low-power", "balanced", "low-latency", "opportunistic"],
                                )?
                                .map(|mode| match mode {
                                    "low-power" => ScanMode::LowPower,
                                    "balanced" => ScanMode::Balanced,
                                    "low-latency" => ScanMode::LowLatency,
                                    _ => ScanMode::Opportunistic,
                                }),
                            callback_type: match options.opt_one_of(
                                "callbackType",
                                &["all-matches", "first-match", "match-lost"],
                            )? {
                                None => None,
                                Some("all-matches") => Some(ScanCallbackType::AllMatches),
                                Some("first-match") => Some(ScanCallbackType::FirstMatch),
                                Some("match-lost") => {
                                    return Err(unsupported(
                                        "scan.start.callback-type-match-lost",
                                        "the radio has no advertisement-loss representation",
                                    ));
                                }
                                Some(_) => None,
                            },
                            legacy: options.opt_boolean("legacy")?,
                            report_delay_ms: options
                                .opt_integer("reportDelayMs", 2_147_483_647)?
                                .map(u32::try_from)
                                .transpose()
                                .map_err(|_| {
                                    error(
                                        BleErrorCode::ArgumentInvalid,
                                        BleErrorDomain::Scan,
                                        "scan.start.report-delay",
                                    )
                                })?,
                            phy: options
                                .opt_one_of("phy", &["all-supported", "1m", "coded"])?
                                .map(|phy| match phy {
                                    "1m" => ScanPhy::Le1m,
                                    "coded" => ScanPhy::LeCoded,
                                    _ => ScanPhy::AllSupported,
                                }),
                        })
                    }
                };
                if apple && !device_addresses.is_empty() {
                    return Err(unsupported(
                        "scan.start.device-addresses",
                        "CoreBluetooth does not expose device addresses",
                    ));
                }
                (
                    Body::ScanStart {
                        start_operation_id: args.string("operationId")?,
                        service_uuids,
                        device_addresses,
                        android,
                        terminal_evidence_scope: args
                            .opt_object("terminalEvidenceScope")?
                            .map(|scope| {
                                scope.exact(
                                    &["managerId", "operationId", "incarnation"],
                                    &["scopeUtf16leBase64"],
                                )?;
                                Ok::<ScanTerminalEvidenceScope, DesktopError>(
                                    ScanTerminalEvidenceScope {
                                        manager_id: scope.string("managerId")?,
                                        operation_id: scope.string("operationId")?,
                                        incarnation: scope.string("incarnation")?,
                                        scope_utf16le: scope
                                            .opt_string("scopeUtf16leBase64")?
                                            .map(|text| {
                                                wire::decode_base64(
                                                    &text,
                                                    "args.terminalEvidenceScope.scopeUtf16leBase64",
                                                )
                                            })
                                            .transpose()?,
                                    },
                                )
                            })
                            .transpose()?,
                        deadline: lifetime_ms
                            .map(|ms| lifetime_received + std::time::Duration::from_millis(ms)),
                    },
                    id_required(args)?,
                    admission_budget,
                )
            }
            "scan.stop" => {
                args.exact(&["operationId"], &["budgetMs"])?;
                (
                    Body::ScanStop(args.string("operationId")?),
                    None,
                    budget(args, received)?,
                )
            }
            "connection.connect" => {
                args.exact(
                    &["peerId", "lease", "operationId"],
                    &["budgetMs", "intent", "transport", "preferredPhy"],
                )?;
                let intent = args
                    .opt_one_of("intent", &["direct", "when-available"])?
                    .unwrap_or("direct");
                args.opt_one_of("transport", &["auto", "le"])?;
                let mut preferred_phy: Vec<Phy> = Vec::new();
                for text in args.strings("preferredPhy")? {
                    let parsed = phy(&text).ok_or_else(|| wire::invalid("args.preferredPhy"))?;
                    if !preferred_phy.contains(&parsed) {
                        preferred_phy.push(parsed);
                    }
                }
                if !preferred_phy.is_empty() {
                    if apple {
                        return Err(unsupported(
                            "connection.connect.preferred-phy",
                            "CoreBluetooth has no LE PHY control",
                        ));
                    }
                    if intent == "when-available" {
                        return Err(unsupported(
                            "connection.connect.preferred-phy",
                            "Android does not apply a connect PHY with autoConnect (when-available)",
                        ));
                    }
                }
                (
                    Body::Connect {
                        peer_id: args.string("peerId")?,
                        lease: args.string("lease")?,
                        staging: ConnectStaging {
                            // Apple `when-available` is the ordinary CoreBluetooth
                            // connect, which stays outstanding until that known
                            // peripheral is available. It is not Android `autoConnect`.
                            auto_connect: !apple && intent == "when-available",
                            preferred_phy,
                        },
                    },
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "connection.disconnect" => {
                let exact = lock(&self.state.exact_leases).is_some();
                if exact {
                    args.exact(
                        &["peerId", "lease", "releaseIdentity"],
                        &["budgetMs", "operationId"],
                    )?;
                } else {
                    args.exact(&["peerId", "lease"], &["budgetMs", "operationId"])?;
                }
                let release_identity = if exact {
                    let identity = args.string("releaseIdentity")?;
                    if identity.len() > 128 {
                        return Err(wire::invalid("args.connection.disconnect.releaseIdentity"));
                    }
                    Some(identity)
                } else {
                    None
                };
                (
                    Body::Disconnect {
                        peer_id: args.string("peerId")?,
                        lease: args.string("lease")?,
                        release_identity,
                    },
                    Self::operation_id(args)?,
                    budget(args, received)?,
                )
            }
            "connection.rssi"
            | "connection.effective-mtu"
            | "connection.request-mtu"
            | "connection.request-priority"
            | "connection.request-subrate"
            | "connection.read-phy"
            | "connection.request-phy"
            | "connection.maximum-write-length"
            | "connection.write-readiness" => {
                let (required, optional): (&[&str], &[&str]) = match op {
                    "connection.effective-mtu" => {
                        (&["peerId", "lease", "operationId"], &["budgetMs"])
                    }
                    "connection.request-mtu" => {
                        (&["peerId", "lease", "mtu", "operationId"], &["budgetMs"])
                    }
                    "connection.request-priority" => (
                        &["peerId", "lease", "priority", "operationId"],
                        &["budgetMs"],
                    ),
                    "connection.request-subrate" => {
                        (&["peerId", "lease", "mode", "operationId"], &["budgetMs"])
                    }
                    "connection.request-phy" => (
                        &["peerId", "lease", "operationId"],
                        &["tx", "rx", "budgetMs"],
                    ),
                    "connection.maximum-write-length" => {
                        (&["peerId", "lease", "mode", "operationId"], &["budgetMs"])
                    }
                    _ => (&["peerId", "lease", "operationId"], &["budgetMs"]),
                };
                args.exact(required, optional)?;
                if apple
                    && matches!(
                        op,
                        "connection.request-mtu"
                            | "connection.request-priority"
                            | "connection.request-subrate"
                            | "connection.read-phy"
                            | "connection.request-phy"
                    )
                {
                    return Err(unsupported(op, "CoreBluetooth has no such link control"));
                }
                let peer_id = args.string("peerId")?;
                let lease = args.string("lease")?;
                let control = match op {
                    "connection.rssi" => Control::Rssi,
                    "connection.effective-mtu" => Control::EffectiveMtu,
                    "connection.request-mtu" => {
                        // Legacy handed any requested MTU to the platform,
                        // which refuses one below 23 (139, AN-3).
                        let mtu = args.integer("mtu", 517)?;
                        Control::RequestMtu(
                            u16::try_from(mtu).map_err(|_| wire::invalid("args.mtu"))?,
                        )
                    }
                    "connection.request-priority" => Control::Priority(
                        match args
                            .one_of("priority", &["low-power", "balanced", "high-throughput"])?
                        {
                            "low-power" => ConnectionPriority::LowPower,
                            "balanced" => ConnectionPriority::Balanced,
                            _ => ConnectionPriority::HighThroughput,
                        },
                    ),
                    "connection.read-phy" => Control::ReadPhy,
                    "connection.request-subrate" => {
                        let mode = match args.one_of(
                            "mode",
                            &["default", "low-latency", "low-power", "high-throughput"],
                        )? {
                            "default" => SubrateMode::Default,
                            "low-latency" => SubrateMode::LowLatency,
                            "low-power" => SubrateMode::LowPower,
                            _ => SubrateMode::HighThroughput,
                        };
                        if !self.host.radio.connection_subrate_available() {
                            return Err(unsupported(
                                op,
                                "Android public subrate requests require an available SDK 36.1+ API",
                            ));
                        }
                        Control::Subrate(mode)
                    }
                    "connection.maximum-write-length" => Control::MaximumWriteLength(
                        args.one_of("mode", &["with-response", "without-response"])?
                            == "with-response",
                    ),
                    "connection.write-readiness" => Control::WriteReadiness,
                    _ => {
                        let tx = args.opt_one_of("tx", PHYS)?.and_then(phy);
                        let rx = args.opt_one_of("rx", PHYS)?.and_then(phy);
                        if tx.is_none() && rx.is_none() {
                            return Err(error(
                                BleErrorCode::ArgumentInvalid,
                                BleErrorDomain::Connection,
                                "connection.request-phy.preference",
                            ));
                        }
                        Control::RequestPhy(tx, rx)
                    }
                };
                (
                    Body::Control {
                        peer_id,
                        lease,
                        control,
                    },
                    Self::operation_id(args)?,
                    budget(args, received)?,
                )
            }
            "security.state" | "security.cancel-pairing" => {
                args.exact(&["peerId"], &["budgetMs", "operationId"])?;
                let peer_id = args.string("peerId")?;
                (
                    if op == "security.state" {
                        Body::SecurityState(peer_id)
                    } else {
                        Body::CancelPairing(peer_id)
                    },
                    Self::operation_id(args)?,
                    budget(args, received)?,
                )
            }
            "security.pair" => {
                args.exact(&["peerId", "transport", "operationId"], &["budgetMs"])?;
                let transport = match args.one_of("transport", &["auto", "le"])? {
                    "auto" => PairTransport::Auto,
                    _ => PairTransport::Le,
                };
                (
                    Body::Pair {
                        peer_id: args.string("peerId")?,
                        transport,
                    },
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "gatt.discover" => {
                args.exact(&["peerId", "lease", "operationId"], &["budgetMs"])?;
                (
                    Body::Discover {
                        peer_id: args.string("peerId")?,
                        lease: args.string("lease")?,
                    },
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "gatt.read" | "gatt.read-descriptor" => {
                args.exact(&["peerId", "selector", "operationId"], &["budgetMs"])?;
                let descriptor = op == "gatt.read-descriptor";
                let selector = if descriptor {
                    descriptor_selector(args)?
                } else {
                    characteristic_selector(args)?
                };
                (
                    Body::Read {
                        peer_id: args.string("peerId")?,
                        selector,
                        descriptor,
                    },
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "gatt.write" | "gatt.write-when-ready" | "gatt.write-descriptor" => {
                args.exact(
                    &["peerId", "selector", "valueB64", "mode", "operationId"],
                    &["budgetMs"],
                )?;
                let descriptor = op == "gatt.write-descriptor";
                let mode = args.one_of("mode", &["with-response", "without-response"])?;
                let wait_ready = op == "gatt.write-when-ready";
                if wait_ready && mode != "without-response" {
                    return Err(wire::invalid("args.mode"));
                }
                if descriptor && mode == "without-response" {
                    return Err(unsupported(
                        "gatt.write-descriptor.mode",
                        "descriptor writes are always acknowledged",
                    ));
                }
                let selector = if descriptor {
                    descriptor_selector(args)?
                } else {
                    characteristic_selector(args)?
                };
                (
                    Body::Write {
                        peer_id: args.string("peerId")?,
                        selector,
                        value: args.bytes("valueB64")?,
                        with_response: mode == "with-response",
                        descriptor,
                        wait_ready,
                    },
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "gatt.ordered.version" => {
                args.exact(&[], &[])?;
                (
                    Body::Ordered(crate::ordered_public::OrderedCommand::Version),
                    None,
                    Budget::unbounded(),
                )
            }
            "gatt.ordered.failure-metadata.version" => {
                args.exact(&[], &[])?;
                (
                    Body::Ordered(crate::ordered_public::OrderedCommand::FailureMetadataVersion),
                    None,
                    Budget::unbounded(),
                )
            }
            "gatt.ordered.capacity.version" => {
                args.exact(&[], &[])?;
                (
                    Body::Ordered(crate::ordered_public::OrderedCommand::CapacityVersion),
                    None,
                    Budget::unbounded(),
                )
            }
            "gatt.ordered.reserve" => {
                args.exact(&["peerId"], &[])?;
                (
                    Body::Ordered(crate::ordered_public::OrderedCommand::Reserve {
                        peer: args.string("peerId")?,
                    }),
                    None,
                    Budget::unbounded(),
                )
            }
            "gatt.ordered.reclaim" => {
                args.exact(
                    &["peerId", "collectionId", "reclaimCapability"],
                    &["recordingId"],
                )?;
                (
                    Body::Ordered(crate::ordered_public::OrderedCommand::Reclaim {
                        peer: args.string("peerId")?,
                        id: args.decimal_u64("collectionId")?,
                        capability: args.string("reclaimCapability")?,
                        recording_id: args.opt_string("recordingId")?,
                    }),
                    None,
                    Budget::unbounded(),
                )
            }
            "gatt.ordered.open" => {
                args.exact(
                    &[
                        "wireVersion",
                        "peerId",
                        "collectionId",
                        "connectionGeneration",
                        "databaseGeneration",
                        "publicConnectionGeneration",
                        "publicDatabaseGeneration",
                        "members",
                        "maxItems",
                        "maxBytes",
                        "maxControls",
                        "operationId",
                    ],
                    &["budgetMs", "failureMetadataVersion"],
                )?;
                if args.integer("wireVersion", 9_007_199_254_740_991)? != 2 {
                    return Err(error(
                        BleErrorCode::ProtocolIncompatible,
                        BleErrorDomain::Core,
                        "gatt.ordered.version",
                    ));
                }
                if args
                    .opt_integer("failureMetadataVersion", 9_007_199_254_740_991)?
                    .unwrap_or(1)
                    != 1
                {
                    return Err(error(
                        BleErrorCode::ProtocolIncompatible,
                        BleErrorDomain::Core,
                        "gatt.ordered.failure-metadata-version",
                    ));
                }
                let id = args.decimal_u64("collectionId")?;
                let mut members = Vec::new();
                let mut context_members = Vec::new();
                for member in args.objects("members", 128)? {
                    member.exact(
                        &[
                            "memberId",
                            "publicMemberId",
                            "selector",
                            "requestedDelivery",
                        ],
                        &["deliveryMode", "preferredDeliveryMode"],
                    )?;
                    let member_id = member.decimal_u64("memberId")?;
                    let mode = |name| -> Result<Option<DeliveryMode>, DesktopError> {
                        Ok(
                            match member.opt_one_of(name, &["notification", "indication"])? {
                                Some("notification") => Some(DeliveryMode::Notification),
                                Some("indication") => Some(DeliveryMode::Indication),
                                _ => None,
                            },
                        )
                    };
                    let selector = characteristic_selector(&member)?;
                    let public_member = member.string("publicMemberId")?;
                    let requested = member.one_of(
                        "requestedDelivery",
                        &[
                            "prefer-notification",
                            "prefer-indication",
                            "require-notification",
                            "require-indication",
                        ],
                    )?;
                    context_members.push(serde_json::json!({
                        "memberId": member_id.to_string(), "publicMemberId": public_member,
                        "selector": member.get_raw("selector").cloned().ok_or_else(|| wire::invalid("args.selector"))?,
                        "requestedDelivery": requested, "deliveryMode": member.opt_string("deliveryMode")?,
                        "preferredDeliveryMode": member.opt_string("preferredDeliveryMode")?
                    }));
                    members.push(ubm_desktop::central::OrderedClaimRequest {
                        member_id,
                        selector,
                        consumer: format!("ordered-{id}-{member_id}"),
                        delivery: mode("deliveryMode")?,
                        preference: mode("preferredDeliveryMode")?,
                    });
                }
                let context = serde_json::json!({
                    "connectionGeneration": args.string("connectionGeneration")?,
                    "databaseGeneration": args.string("databaseGeneration")?,
                    "publicConnectionGeneration": args.string("publicConnectionGeneration")?,
                    "publicDatabaseGeneration": args.string("publicDatabaseGeneration")?,
                    "maxItems": args.integer("maxItems", u32::MAX as u64)?,
                    "maxBytes": args.integer("maxBytes", u32::MAX as u64)?,
                    "maxControls": args.integer("maxControls", u32::MAX as u64)?,
                    "members": context_members,
                });
                (
                    Body::Ordered(crate::ordered_public::OrderedCommand::Open {
                        peer: args.string("peerId")?,
                        id,
                        connection: args.string("connectionGeneration")?,
                        database: args.string("databaseGeneration")?,
                        members,
                        items: args.integer("maxItems", u32::MAX as u64)? as usize,
                        bytes: args.integer("maxBytes", u32::MAX as u64)? as usize,
                        controls: args.integer("maxControls", u32::MAX as u64)? as usize,
                        context,
                    }),
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "gatt.ordered.recording-reclaim.next"
            | "gatt.ordered.recording-reclaim.reserve"
            | "gatt.ordered.recording-reclaim.start"
            | "gatt.ordered.recording-reclaim.status"
            | "gatt.ordered.recording-reclaim.commit"
            | "gatt.ordered.recording-reclaim.cancel"
            | "gatt.ordered.recording-reclaim.release"
            | "gatt.ordered.recording-reclaim.retire"
            | "gatt.ordered.recording-reclaim.ack-retire"
            | "gatt.ordered.recording-reclaim.forget-retire"
            | "gatt.ordered.recording-reclaim.ack-forget" => {
                let action = op.rsplit('.').next().expect("known operation").to_owned();
                let common = ["peerId", "collectionId", "recordingId", "reclaimCapability"];
                if action == "next" {
                    args.exact(&common, &["callerKey"])?;
                } else if action == "reserve" {
                    args.exact(
                        &[
                            "peerId",
                            "collectionId",
                            "recordingId",
                            "reclaimCapability",
                            "requestId",
                            "callerKey",
                        ],
                        &[],
                    )?;
                } else {
                    args.exact(
                        &[
                            "peerId",
                            "collectionId",
                            "recordingId",
                            "reclaimCapability",
                            "requestId",
                            "callerKey",
                        ],
                        &[],
                    )?;
                }
                (
                    Body::RecordingReclaim {
                        action: action.clone(),
                        peer: args.string("peerId")?,
                        collection: args.decimal_u64("collectionId")?,
                        recording: args.string("recordingId")?,
                        capability: args.string("reclaimCapability")?,
                        request: if action == "next" {
                            None
                        } else {
                            Some(args.decimal_u64("requestId")?)
                        },
                        caller_key: if action == "next" {
                            args.opt_string("callerKey")?
                        } else {
                            Some(args.string("callerKey")?)
                        },
                    },
                    None,
                    Budget::unbounded(),
                )
            }
            "gatt.ordered.recording-reclaim.version" => {
                args.exact(&[], &[])?;
                (Body::ReclaimVersion, None, Budget::unbounded())
            }
            "gatt.ordered.marker.version" => {
                args.exact(&[], &[])?;
                (Body::MarkerVersion, None, Budget::unbounded())
            }
            "gatt.ordered.marker.reserve"
            | "gatt.ordered.marker.start"
            | "gatt.ordered.marker.status"
            | "gatt.ordered.marker.cancel"
            | "gatt.ordered.marker.forget" => {
                let action = match op {
                    "gatt.ordered.marker.reserve" => {
                        args.exact(&["peerId", "collectionId", "reservationId"], &[])?;
                        MarkerAction::Reserve(args.string("reservationId")?)
                    }
                    "gatt.ordered.marker.start" => {
                        args.exact(
                            &[
                                "peerId",
                                "collectionId",
                                "handle",
                                "selector",
                                "valueB64",
                                "mode",
                            ],
                            &[],
                        )?;
                        MarkerAction::Start {
                            handle: args.decimal_u64("handle")?,
                            selector: characteristic_selector(args)?,
                            value: args.bytes("valueB64")?,
                            with_response: args
                                .one_of("mode", &["with-response", "without-response"])?
                                == "with-response",
                        }
                    }
                    "gatt.ordered.marker.status" => {
                        args.exact(&["peerId", "collectionId", "handle"], &[])?;
                        MarkerAction::Status(args.decimal_u64("handle")?)
                    }
                    "gatt.ordered.marker.cancel" => {
                        args.exact(&["peerId", "collectionId", "handle"], &[])?;
                        MarkerAction::Cancel(args.decimal_u64("handle")?)
                    }
                    _ => {
                        args.exact(&["peerId", "collectionId", "handle"], &[])?;
                        MarkerAction::Forget(args.decimal_u64("handle")?)
                    }
                };
                (
                    Body::Marker {
                        peer: args.string("peerId")?,
                        collection: args.decimal_u64("collectionId")?,
                        action,
                    },
                    None,
                    Budget::unbounded(),
                )
            }
            "gatt.ordered.increase-capacity" => {
                args.exact(
                    &[
                        "peerId",
                        "collectionId",
                        "sourceGeneration",
                        "maxBytes",
                        "wireVersion",
                    ],
                    &[],
                )?;
                if args.integer("wireVersion", 1)? != 1 {
                    return Err(wire::invalid("args.wireVersion"));
                }
                let generation = args.object("sourceGeneration")?;
                generation.exact(&["kind", "value"], &[])?;
                let generation = match generation.string("kind")?.as_str() {
                    "counter" => ubm_core::ordered_notifications::SourceGeneration::Counter(
                        generation.decimal_u64("value")?,
                    ),
                    "opaque" => ubm_core::ordered_notifications::SourceGeneration::Opaque(
                        generation.string("value")?,
                    ),
                    _ => return Err(wire::invalid("args.sourceGeneration.kind")),
                };
                let max_bytes = args
                    .integer("maxBytes", ubm_core::contracts::MAX_STREAM_BYTE_CAPACITY)?
                    as usize;
                if max_bytes == 0 {
                    return Err(wire::invalid("args.maxBytes"));
                }
                (
                    Body::Ordered(crate::ordered_public::OrderedCommand::IncreaseCapacity {
                        peer: args.string("peerId")?,
                        id: args.decimal_u64("collectionId")?,
                        generation,
                        max_bytes,
                    }),
                    None,
                    Budget::unbounded(),
                )
            }
            "gatt.ordered.status"
            | "gatt.ordered.poll"
            | "gatt.ordered.ack-delivery"
            | "gatt.ordered.discard"
            | "gatt.ordered.poll-discard"
            | "gatt.ordered.ack-discard"
            | "gatt.ordered.close"
            | "gatt.ordered.ack-group"
            | "gatt.ordered.forget-group" => {
                use crate::ordered_public::GroupAction;
                let action = match op {
                    "gatt.ordered.status" => GroupAction::Status,
                    "gatt.ordered.poll" => GroupAction::Poll,
                    "gatt.ordered.ack-delivery" => GroupAction::AckDelivery,
                    "gatt.ordered.discard" => GroupAction::Discard,
                    "gatt.ordered.poll-discard" => GroupAction::PollDiscard,
                    "gatt.ordered.ack-discard" => GroupAction::AckDiscard,
                    "gatt.ordered.close" => GroupAction::Close,
                    "gatt.ordered.ack-group" => GroupAction::AckGroup,
                    _ => GroupAction::ForgetGroup,
                };
                let identity = matches!(
                    action,
                    GroupAction::AckDelivery | GroupAction::Discard | GroupAction::AckDiscard
                );
                let close = matches!(action, GroupAction::Close);
                let forget = matches!(action, GroupAction::ForgetGroup);
                let required: &[&str] = if forget {
                    &["peerId", "collectionId", "reclaimCapability"]
                } else if identity {
                    &["peerId", "collectionId", "deliveryId"]
                } else if close {
                    &["peerId", "collectionId", "operationId"]
                } else {
                    &["peerId", "collectionId"]
                };
                args.exact(required, if close { &["budgetMs"] } else { &[] })?;
                (
                    Body::Ordered(crate::ordered_public::OrderedCommand::Group {
                        action,
                        peer: args.string("peerId")?,
                        id: args.decimal_u64("collectionId")?,
                        delivery: if identity {
                            Some(args.decimal_u64("deliveryId")?)
                        } else {
                            None
                        },
                        capability: if forget {
                            Some(args.string("reclaimCapability")?)
                        } else {
                            None
                        },
                    }),
                    if close { id_required(args)? } else { None },
                    if close {
                        budget(args, received)?
                    } else {
                        Budget::unbounded()
                    },
                )
            }
            "gatt.subscribe" | "gatt.subscribe-recorded" => {
                if lock(&self.state.exact_leases).is_some() {
                    return Err(unsupported(
                        "gatt.subscribe",
                        "exact subscription release is not implemented for wire revision 4",
                    ));
                }
                let recorded = op == "gatt.subscribe-recorded";
                let required: &[&str] = if recorded {
                    &["peerId", "selector", "consumer", "operationId", "recording"]
                } else {
                    &["peerId", "selector", "consumer", "operationId"]
                };
                args.exact(
                    required,
                    &["deliveryMode", "budgetMs", "terminalEvidenceScope"],
                )?;
                let mode = args.opt_one_of(
                    "deliveryMode",
                    &[
                        "prefer-notification",
                        "prefer-indication",
                        "require-notification",
                        "require-indication",
                    ],
                )?;
                (
                    Body::Subscribe {
                        peer_id: args.string("peerId")?,
                        selector: characteristic_selector(args)?,
                        consumer: args.string("consumer")?,
                        mode,
                        terminal_evidence_scope: args
                            .opt_object("terminalEvidenceScope")?
                            .map(|scope| {
                                scope.exact(
                                    &["managerId", "operationId", "incarnation"],
                                    &["scopeUtf16leBase64"],
                                )?;
                                Ok::<ScanTerminalEvidenceScope, DesktopError>(
                                    ScanTerminalEvidenceScope {
                                        manager_id: scope.string("managerId")?,
                                        operation_id: scope.string("operationId")?,
                                        incarnation: scope.string("incarnation")?,
                                        scope_utf16le: scope
                                            .opt_string("scopeUtf16leBase64")?
                                            .map(|text| {
                                                wire::decode_base64(
                                                    &text,
                                                    "args.terminalEvidenceScope.scopeUtf16leBase64",
                                                )
                                            })
                                            .transpose()?,
                                    },
                                )
                            })
                            .transpose()?,
                        recording: if recorded {
                            let recording = args.object("recording")?;
                            recording.exact(&["id", "maxBytes", "maxRecords"], &[])?;
                            Some(RecordingOptions {
                                id: recording.string("id")?,
                                max_bytes: recording.integer("maxBytes", 1 << 30)?,
                                max_records: recording.integer("maxRecords", 1_000_000)?,
                            })
                        } else {
                            None
                        },
                    },
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "gatt.unsubscribe" => {
                if lock(&self.state.exact_leases).is_some() {
                    return Err(unsupported(
                        "gatt.unsubscribe",
                        "exact subscription release is not implemented for wire revision 4",
                    ));
                }
                args.exact(
                    &["peerId", "selector", "consumer", "operationId"],
                    &["budgetMs"],
                )?;
                (
                    Body::Unsubscribe {
                        peer_id: args.string("peerId")?,
                        selector: characteristic_selector(args)?,
                        consumer: args.string("consumer")?,
                    },
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "background.acquire" => {
                args.exact(&["kind", "reason"], &["budgetMs", "operationId"])?;
                args.one_of("kind", &["connected-device"])?;
                (
                    Body::BackgroundAcquire {
                        kind: BackgroundKind::ConnectedDevice,
                        reason: args.string("reason")?,
                    },
                    Self::operation_id(args)?,
                    budget(args, received)?,
                )
            }
            "background.release" => {
                args.exact(&["leaseId"], &["budgetMs"])?;
                (
                    Body::BackgroundRelease(args.string("leaseId")?),
                    None,
                    budget(args, received)?,
                )
            }
            "background.update-notification" => {
                args.exact(&["leaseId", "title"], &["body", "budgetMs"])?;
                (
                    Body::BackgroundNotification {
                        lease_id: args.string("leaseId")?,
                        title: args.string("title")?,
                        body: args.opt_string("body")?,
                    },
                    None,
                    budget(args, received)?,
                )
            }
            "companion.associate" => {
                args.exact(
                    &[],
                    &[
                        "name",
                        "serviceUuid",
                        "filtersJson",
                        "budgetMs",
                        "operationId",
                    ],
                )?;
                if apple {
                    return Err(unsupported(
                        "companion.associate",
                        "companion-device association is an Android service",
                    ));
                }
                let service_uuid = args
                    .opt_string("serviceUuid")?
                    .map(|uuid| {
                        canonical_uuid(&uuid).map_err(|_| wire::invalid("args.serviceUuid"))
                    })
                    .transpose()?;
                let filters_json = args
                    .opt_string("filtersJson")?
                    .map(|text| crate::companion_filters::validate(&text))
                    .transpose()?;
                if filters_json.is_some()
                    && (args.opt_string("name")?.is_some() || service_uuid.is_some())
                {
                    return Err(wire::invalid("args.filtersJson.exclusive"));
                }
                (
                    Body::CompanionAssociate {
                        name: args.opt_string("name")?,
                        service_uuid,
                        filters_json,
                    },
                    Self::operation_id(args)?,
                    budget(args, received)?,
                )
            }
            "companion.list" => {
                args.exact(&[], &["budgetMs", "operationId"])?;
                if apple {
                    return Err(unsupported(
                        "companion.list",
                        "companion-device association is an Android service",
                    ));
                }
                (
                    Body::CompanionList,
                    Self::operation_id(args)?,
                    budget(args, received)?,
                )
            }
            "companion.disassociate" => {
                args.exact(&["associationId"], &["budgetMs", "operationId"])?;
                if apple {
                    return Err(unsupported(
                        "companion.disassociate",
                        "companion-device association is an Android service",
                    ));
                }
                let association_id = args.integer("associationId", wire::MAX_SAFE_INTEGER)?;
                if association_id == 0 {
                    return Err(wire::invalid("args.associationId"));
                }
                (
                    Body::CompanionDisassociate {
                        association_id: association_id as i64,
                    },
                    Self::operation_id(args)?,
                    budget(args, received)?,
                )
            }
            "presence.observe" | "presence.unobserve" => {
                args.exact(&["peerId"], &["budgetMs", "operationId"])?;
                if apple {
                    return Err(unsupported(
                        op,
                        "CoreBluetooth delivers restoration through willRestoreState; there is no presence observation to arm",
                    ));
                }
                let body = match op {
                    "presence.observe" => Body::PresenceObserve {
                        peer_id: args.string("peerId")?,
                    },
                    _ => Body::PresenceUnobserve {
                        peer_id: args.string("peerId")?,
                    },
                };
                (body, Self::operation_id(args)?, budget(args, received)?)
            }
            "op.cancel" => {
                args.exact(&["operationId"], &[])?;
                (
                    Body::Cancel {
                        operation_id: args.string("operationId")?,
                        admission: admission
                            .ok_or_else(|| wire::invalid("args.op.cancel.admission"))?,
                    },
                    None,
                    Budget::unbounded(),
                )
            }
            _ => return Err(wire::invalid("op")),
        };
        Ok(Command {
            body,
            operation_id,
            budget,
        })
    }

    fn require_lease(
        &self,
        peer_id: &str,
        lease: &str,
        operation: &str,
    ) -> Result<String, DesktopError> {
        let core_lease = self.state.core_name(lease);
        if let Some(table) = lock(&self.state.exact_leases).as_ref() {
            return if table.records.values().any(|entry| {
                entry.peer == peer_id
                    && entry.name == core_lease
                    && !entry.released
                    && entry.owner.is_some()
            }) {
                Ok(core_lease)
            } else {
                Err(error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Connection,
                    operation,
                )
                .with_detail("this session holds no such connection lease"))
            };
        }
        match lock(&self.state.leases).get(peer_id) {
            Some(held) if held.name == core_lease => Ok(core_lease),
            _ => Err(error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Connection,
                operation,
            )
            .with_detail("this session holds no such connection lease")),
        }
    }

    async fn peer_record(&self, peer_id: &str) -> Option<PeerRecord> {
        self.host
            .central
            .peer_records()
            .await
            .into_iter()
            .find(|record| record.peer_id == peer_id)
    }

    async fn require_connected(&self, peer_id: &str, operation: &str) -> Result<(), DesktopError> {
        match self
            .peer_record(peer_id)
            .await
            .and_then(|r| r.connection_state)
        {
            Some(ConnectionState::Connected) => Ok(()),
            None => Err(error(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                operation,
            )),
            Some(_) => Err(error(
                BleErrorCode::ConnectionStale,
                BleErrorDomain::Connection,
                operation,
            )),
        }
    }

    async fn marker_command(
        &self,
        peer: &str,
        collection: u64,
        action: MarkerAction,
    ) -> Result<Value, DesktopError> {
        if !ubm_desktop::RadioBoundary::ordered_marker_write_supported(&self.host.radio) {
            return Err(unsupported(
                "gatt.ordered.marker",
                "ordered marker writes are unavailable",
            ));
        }
        if !self
            .host
            .ordered_owners
            .owns(collection, peer, self.state.id)
        {
            return Err(error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Gatt,
                "gatt.ordered.marker",
            ));
        }
        let recorder =
            lock(&self.host.ordered_recordings)
                .iter()
                .find_map(|((session, _), owner)| {
                    (*session == self.state.id
                        && owner.peer_id() == peer
                        && owner.collection_id() == collection)
                        .then(|| Arc::clone(owner))
                });
        let owner = if let Some(recorder) = recorder {
            recorder.marker_reservation_owner().await.map_err(|cause| {
                error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker",
                )
                .with_detail(format!("recorder marker authority unavailable: {cause:?}"))
            })?
        } else {
            self.host
                .central
                .marker_reservation_owner(peer, collection, None)
                .await?
        };
        match action {
            MarkerAction::Reserve(reservation_id) => {
                Ok(serde_json::json!({"handle":owner.reserve(&reservation_id).await?.to_string()}))
            }
            MarkerAction::Start {
                handle,
                selector,
                value,
                with_response,
            } => ubm_desktop::ordered_wire::encode_marker_write_versioned(
                &owner.start(handle, selector, value, with_response).await?,
                1,
            )
            .ok_or_else(ordered_encoding_error),
            MarkerAction::Status(handle) => {
                ubm_desktop::ordered_wire::encode_marker_write_versioned(
                    &owner.status(handle).await?,
                    1,
                )
                .ok_or_else(ordered_encoding_error)
            }
            MarkerAction::Cancel(handle) => {
                ubm_desktop::ordered_wire::encode_marker_write_versioned(
                    &owner.cancel(handle).await?,
                    1,
                )
                .ok_or_else(ordered_encoding_error)
            }
            MarkerAction::Forget(handle) => {
                owner.forget(handle).await?;
                Ok(Value::Null)
            }
        }
    }

    async fn recording_reclaim(
        &self,
        action: &str,
        peer: &str,
        collection: u64,
        recording: &str,
        capability: &str,
        request: Option<u64>,
        caller_key: Option<&str>,
    ) -> Result<Value, DesktopError> {
        let denied = || {
            error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Gatt,
                "gatt.ordered.recording-reclaim",
            )
        };
        if !self.host.radio.ordered_reclaim_graph_supported() {
            return Err(error(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Gatt,
                "gatt.ordered.recording-reclaim",
            ));
        }
        // Only allocation and reservation need to serialize their lookup and
        // insertion. Receipt operations use the retained exact owner and must
        // remain callable while an unrelated acquisition is stalled.
        let allocation_gate = if action == "next" || action == "reserve" {
            let mut gates = lock(&self.state.reclaim_guards);
            gates.retain(|_, gate| gate.strong_count() != 0);
            let gate = gates.entry(collection).or_default();
            Some(match gate.upgrade() {
                Some(existing) => existing,
                None => {
                    let created = Arc::new(tokio::sync::Mutex::new(()));
                    *gate = Arc::downgrade(&created);
                    created
                }
            })
        } else {
            None
        };
        let _allocation = match &allocation_gate {
            Some(gate) => Some(gate.lock().await),
            None => None,
        };
        if action == "forget-retire" || action == "ack-forget" {
            let id = request.ok_or_else(denied)?;
            let caller_key = caller_key.ok_or_else(denied)?;
            let mut receipts = lock(&self.state.reclaim_forget_receipts);
            let receipt_key = (collection, recording.to_owned(), id);
            if let Some(receipt) = receipts.get(&receipt_key) {
                if receipt.peer != peer
                    || receipt.recording != recording
                    || !reclaim_secret_matches(&receipt.capability, capability)
                    || receipt.caller_key != caller_key
                {
                    return Err(denied());
                }
                if action == "ack-forget" {
                    receipts.remove(&receipt_key);
                    return Ok(serde_json::json!({"state":"acknowledged"}));
                }
                return Ok(serde_json::json!({"state":"forgotten"}));
            }
            // An acknowledgement may have succeeded while its response was
            // lost. Absence is not an authenticated receipt and removes nothing.
            if action == "ack-forget" {
                return Ok(serde_json::json!({"state":"absent"}));
            }
        }
        // `next` does not reserve. With a caller key it can replay that
        // caller's retained ID; a refusal requires a fenced, different
        // identity. A transport failure never proves absence.
        if action == "next" {
            if let Some(entry) = lock(&self.state.reclaim_reservations).get(&collection) {
                if entry.peer != peer
                    || entry.recording != recording
                    || !reclaim_secret_matches(&entry.capability, capability)
                {
                    return Err(denied());
                }
                if caller_key == Some(entry.caller_key.as_str()) {
                    return Ok(serde_json::json!({"requestId":entry.id.to_string()}));
                }
                if caller_key.is_none() {
                    return Err(denied());
                }
                let failure = error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "gatt.ordered.recording-reclaim",
                )
                .with_detail("another reclaim is retained");
                return Ok(
                    serde_json::json!({"state":"not-reserved", "error":crate::wire::error_object(&failure)}),
                );
            }
            let result = async {
                let owner = {
                    let entries = lock(&self.host.ordered_recordings);
                    entries
                        .iter()
                        .find_map(|((session, id), owner)| {
                            (id == recording
                                && owner.peer_id() == peer
                                && owner.collection_id() == collection
                                && (*session == 0 || *session == self.state.id))
                                .then(|| Arc::clone(owner))
                        })
                        .ok_or_else(denied)?
                };
                let reservation = owner
                    .reclaim_reservation_owner(capability)
                    .await
                    .map_err(|_| denied())?;
                reservation.next_request_id(capability)
            }
            .await;
            return Ok(match result {
                Ok(id) => serde_json::json!({"requestId": id.to_string()}),
                Err(failure) => {
                    serde_json::json!({"state": "not-reserved", "error": crate::wire::error_object(&failure)})
                }
            });
        }
        let id = request.ok_or_else(denied)?;
        let caller_key = caller_key.ok_or_else(denied)?;
        let retained = lock(&self.state.reclaim_reservations)
            .get(&collection)
            .cloned();
        let exact = retained.as_ref().filter(|entry| {
            entry.id == id
                && entry.peer == peer
                && entry.recording == recording
                && reclaim_secret_matches(&entry.capability, capability)
                && entry.caller_key == caller_key
        });
        // A mismatched capability with the same request and caller identity
        // cannot receive a statement that this caller owns nothing.
        if retained
            .as_ref()
            .is_some_and(|entry| entry.caller_key == caller_key && exact.is_none())
        {
            return Err(denied());
        }
        let (previous, owner, lease, reservation) = if let Some(entry) = exact {
            (
                entry.previous,
                Arc::clone(&entry.recording_owner),
                entry.lease.clone(),
                Arc::clone(&entry.owner),
            )
        } else {
            let candidate = {
                let entries = lock(&self.host.ordered_recordings);
                entries.iter().find_map(|((session, name), owner)| {
                    (name == recording
                        && owner.peer_id() == peer
                        && owner.collection_id() == collection
                        && (*session == 0 || *session == self.state.id))
                        .then(|| (*session, Arc::clone(owner)))
                })
            };
            let Some((previous, owner)) = candidate else {
                // The fence and missing session receipt prove that this exact
                // caller has no reservation, even if another session now owns
                // the recorder mapping.
                if action == "reserve" {
                    return Ok(
                        serde_json::json!({"state":"not-reserved", "error":crate::wire::error_object(&denied())}),
                    );
                }
                return Err(denied());
            };
            let Some(lease) = lock(&self.state.leases)
                .get(peer)
                .map(|lease| lease.name.clone())
            else {
                if action == "reserve" {
                    return Ok(
                        serde_json::json!({"state":"not-reserved", "error":crate::wire::error_object(&denied())}),
                    );
                }
                return Err(denied());
            };
            let reservation = owner
                .reclaim_reservation_owner(capability)
                .await
                .map_err(|_| denied())?;
            (previous, owner, lease, reservation)
        };
        match action {
            "reserve" => {
                let mut entries = lock(&self.state.reclaim_reservations);
                if exact.is_none() {
                    let receipts = lock(&self.state.reclaim_forget_receipts);
                    if entries.len() + receipts.len() >= RECLAIM_RECEIPT_CAPACITY
                        || receipts.contains_key(&(collection, recording.to_owned(), id))
                    {
                        let failure = error(
                            BleErrorCode::LifecycleInvalidState,
                            BleErrorDomain::Cleanup,
                            "gatt.ordered.recording-reclaim",
                        )
                        .with_detail("reclaim receipt capacity or identity unavailable");
                        return Ok(
                            serde_json::json!({"state":"not-reserved", "error":crate::wire::error_object(&failure)}),
                        );
                    }
                }
                if let Err(failure) = reservation.reserve(id, capability, &lease, caller_key) {
                    if exact.is_some() {
                        return Err(failure);
                    }
                    return Ok(
                        serde_json::json!({"state": "not-reserved", "error": crate::wire::error_object(&failure)}),
                    );
                }
                if exact.is_none() {
                    entries.insert(
                        collection,
                        SessionReclaim {
                            owner: Arc::clone(&reservation),
                            recording_owner: Arc::clone(&owner),
                            previous,
                            id,
                            peer: peer.to_owned(),
                            recording: recording.to_owned(),
                            capability: capability.to_owned(),
                            lease: lease.clone(),
                            caller_key: caller_key.to_owned(),
                        },
                    );
                }
            }
            "start" => reservation.start(id, capability, &lease, caller_key)?,
            "commit" => reservation.commit(id, capability, &lease, caller_key, || {
                let mut entries = lock(&self.host.ordered_recordings);
                if !entries
                    .get(&(previous, recording.to_owned()))
                    .is_some_and(|current| Arc::ptr_eq(current, &owner))
                    || !self
                        .host
                        .ordered_owners
                        .reclaim_recording(collection, peer, self.state.id)
                {
                    return Err(denied());
                }
                if previous != self.state.id {
                    entries.remove(&(previous, recording.to_owned()));
                    entries.insert((self.state.id, recording.to_owned()), Arc::clone(&owner));
                }
                Ok(())
            })?,
            "cancel" => reservation.cancel(id, capability, &lease, caller_key)?,
            "release" => reservation.release(id, capability, &lease, caller_key)?,
            "retire" => reservation.retire(id, capability, &lease, caller_key)?,
            "ack-retire" => {
                reservation.acknowledge_retirement(id, capability, &lease, caller_key)?;
                return Ok(serde_json::json!({"state":"acknowledged"}));
            }
            "forget-retire" => {
                let mut entries = lock(&self.state.reclaim_reservations);
                let entry = entries
                    .get(&collection)
                    .filter(|entry| {
                        entry.id == id
                            && entry.peer == peer
                            && entry.recording == recording
                            && reclaim_secret_matches(&entry.capability, capability)
                            && entry.lease == lease
                            && entry.caller_key == caller_key
                    })
                    .ok_or_else(denied)?;
                reservation.forget_retirement(id, capability, &lease, caller_key)?;
                let receipt = ReclaimForgetReceipt {
                    peer: entry.peer.clone(),
                    recording: entry.recording.clone(),
                    capability: entry.capability.clone(),
                    caller_key: entry.caller_key.clone(),
                };
                lock(&self.state.reclaim_forget_receipts)
                    .insert((collection, recording.to_owned(), id), receipt);
                entries.remove(&collection);
                return Ok(serde_json::json!({"state":"forgotten"}));
            }
            "status" => {}
            _ => return Err(denied()),
        }
        use ubm_desktop::reclaim_reservation::ReclaimReservationStatus as S;
        let (state, committed, failure) =
            match reservation.status(id, capability, &lease, caller_key)? {
                S::Reserved => ("reserved", false, None),
                S::Pending => ("pending", false, None),
                S::Ready => ("ready", false, None),
                S::Committed => ("committed", true, None),
                S::Releasing { mapping_committed } => ("releasing", mapping_committed, None),
                S::Released { mapping_committed } => ("released", mapping_committed, None),
                S::ReleaseFailed {
                    mapping_committed,
                    error,
                } => ("release-failed", mapping_committed, Some(error)),
                S::Failed(error) => ("failed", false, Some(error)),
                S::Retired { mapping_committed } => ("retired", mapping_committed, None),
            };
        Ok(
            serde_json::json!({"state":state, "mappingCommitted": committed, "error": failure.as_ref().map(crate::wire::error_object)}),
        )
    }

    async fn ordered_command(
        &self,
        command: crate::ordered_public::OrderedCommand,
        ctl: OpControl,
    ) -> Result<Value, DesktopError> {
        use crate::ordered_public::{GroupAction, OrderedCommand};
        let central = &self.host.central;
        let owners = &self.host.ordered_owners;
        match command {
            OrderedCommand::Version => Ok(serde_json::json!({"wireVersion": 2})),
            OrderedCommand::FailureMetadataVersion => Ok(serde_json::json!({"versions": [1]})),
            OrderedCommand::CapacityVersion => Ok(
                serde_json::json!({"wireVersion":1,"maximumMaxBytes":ubm_core::contracts::MAX_STREAM_BYTE_CAPACITY}),
            ),
            OrderedCommand::Reserve { peer } => {
                if !lock(&self.state.leases).contains_key(&peer) {
                    return Err(error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Connection,
                        "gatt.ordered.reserve",
                    ));
                }
                let (id, capability) = owners.reserve(&peer, self.state.id, || {
                    central.reserve_ordered_batch(&peer)
                })?;
                Ok(
                    serde_json::json!({"collectionId":id.to_string(), "reclaimCapability":capability}),
                )
            }
            OrderedCommand::Reclaim {
                peer,
                id,
                capability,
                recording_id: expected_recording_id,
            } => {
                if !owners.authenticate(id, &peer, &capability) {
                    return Err(error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.reclaim",
                    ));
                }
                let recorder = {
                    let entries = lock(&self.host.ordered_recordings);
                    entries.iter().find_map(|((session, recording_id), owner)| {
                        (owner.collection_id() == id
                            && owner.peer_id() == peer
                            && (*session == 0 || *session == self.state.id))
                            .then(|| (*session, recording_id.clone(), Arc::clone(owner)))
                    })
                };
                if recorder.is_some() {
                    // Recorder handoff requires the retained reservation API.
                    return Err(error(
                        BleErrorCode::CapabilityUnsupported,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.reclaim",
                    ));
                }
                // A recorder owned by another live session must never fall through
                // to ordinary group transfer.
                if lock(&self.host.ordered_recordings)
                    .values()
                    .any(|owner| owner.collection_id() == id && owner.peer_id() == peer)
                {
                    return Err(error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.reclaim",
                    ));
                }
                if expected_recording_id.is_some() {
                    return Err(error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.reclaim",
                    ));
                }
                if !owners.reclaim(id, &peer, self.state.id, &capability) {
                    return Err(error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.reclaim",
                    ));
                }
                owners
                    .context(id, &peer, self.state.id)
                    .ok_or_else(|| {
                        error(
                            BleErrorCode::OwnershipDenied,
                            BleErrorDomain::Gatt,
                            "gatt.ordered.reclaim",
                        )
                    })
                    .map(|context| context.unwrap_or(Value::Null))
            }
            OrderedCommand::Open {
                peer,
                id,
                connection,
                database,
                members,
                items,
                bytes,
                controls,
                context,
            } => {
                if !owners.owns(id, &peer, self.state.id)
                    || owners.completed(id, &peer, self.state.id)
                {
                    return Err(error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.open",
                    ));
                }
                let lease = lock(&self.state.leases)
                    .get(&peer)
                    .map(|lease| lease.name.clone())
                    .ok_or_else(|| {
                        error(
                            BleErrorCode::OwnershipDenied,
                            BleErrorDomain::Connection,
                            "gatt.ordered.open",
                        )
                    })?;
                owners.bind_context(id, &peer, self.state.id, context)?;
                central
                    .open_reserved_ordered_batch(
                        &peer,
                        id,
                        &connection,
                        &database,
                        &members,
                        items,
                        bytes,
                        controls,
                        ctl.with_connection_lease(lease),
                    )
                    .await?;
                Ok(Value::Null)
            }
            OrderedCommand::IncreaseCapacity {
                peer,
                id,
                generation,
                max_bytes,
            } => {
                if !owners.owns(id, &peer, self.state.id) {
                    return Err(error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.owner",
                    ));
                }
                let lease = central
                    .ordered_batch_source_lease(&peer, id)
                    .await?
                    .filter(|lease| lease.source_generation == generation)
                    .ok_or_else(|| {
                        error(
                            BleErrorCode::GattStaleHandle,
                            BleErrorDomain::Gatt,
                            "gatt.ordered.increase-capacity",
                        )
                    })?;
                let consumer = self
                    .host
                    .ordered_consumer_for_lease(&lease)
                    .ok_or_else(|| {
                        error(
                            BleErrorCode::GattStaleHandle,
                            BleErrorDomain::Gatt,
                            "gatt.ordered.increase-capacity",
                        )
                    })?;
                let previous = consumer
                    .increase_byte_capacity(self.state.id, max_bytes)
                    .await?;
                Ok(
                    serde_json::json!({"previousMaxBytes":previous,"maxBytes":max_bytes,"maximumMaxBytes":ubm_core::contracts::MAX_STREAM_BYTE_CAPACITY}),
                )
            }
            OrderedCommand::Group {
                action,
                peer,
                id,
                delivery,
                capability,
            } => {
                if matches!(action, GroupAction::ForgetGroup) {
                    let token = capability
                        .as_deref()
                        .ok_or_else(|| wire::invalid("args.reclaimCapability"))?;
                    if !owners.authenticate(id, &peer, token) {
                        return Err(error(
                            BleErrorCode::OwnershipDenied,
                            BleErrorDomain::Cleanup,
                            "gatt.ordered.forget-group",
                        ));
                    }
                    if owners.absent(id) {
                        central.forget_ordered_batch(&peer, id).await?;
                        return Ok(Value::Null);
                    }
                }
                if !owners.owns(id, &peer, self.state.id) {
                    return Err(error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.owner",
                    ));
                }
                let identity = || delivery.ok_or_else(|| wire::invalid("args.deliveryId"));
                match action {
                    GroupAction::Poll => {
                        let d = central.poll_ordered_delivery(&peer, id).await?;
                        let record = crate::ordered_encoding::encode(d.poll)
                            .ok_or_else(ordered_encoding_error)?;
                        Ok(
                            serde_json::json!({"deliveryId": d.delivery_id.map(|id| id.to_string()), "record": record.to_string()}),
                        )
                    }
                    GroupAction::AckDelivery => {
                        central
                            .acknowledge_ordered_delivery(&peer, id, identity()?)
                            .await?;
                        Ok(Value::Null)
                    }
                    GroupAction::Discard => {
                        let receipt = central
                            .discard_ordered_delivery(&peer, id, identity()?)
                            .await?;
                        ordered_discard_value(&receipt)
                    }
                    GroupAction::PollDiscard => central
                        .poll_ordered_discard_receipt(&peer, id)
                        .await?
                        .as_ref()
                        .map(ordered_discard_value)
                        .transpose()
                        .map(|receipt| receipt.unwrap_or(Value::Null)),
                    GroupAction::AckDiscard => {
                        if let Some(receipt) =
                            central.poll_ordered_discard_receipt(&peer, id).await?
                        {
                            if receipt.receipt_id == identity()? {
                                ordered_discard_value(&receipt)?;
                            }
                        }
                        central
                            .acknowledge_ordered_discard_receipt(&peer, id, identity()?)
                            .await?;
                        Ok(Value::Null)
                    }
                    GroupAction::Close => {
                        let status = central.retry_retained_ordered_batch(&peer, id, ctl).await?;
                        Ok(Value::from(match status {
                            ubm_desktop::central::OrderedCleanupOutcome::Released => "released",
                            ubm_desktop::central::OrderedCleanupOutcome::PendingSource => {
                                "pending-source"
                            }
                            ubm_desktop::central::OrderedCleanupOutcome::PendingDrain => {
                                "pending-drain"
                            }
                        }))
                    }
                    GroupAction::AckGroup => {
                        if !owners.completed(id, &peer, self.state.id) {
                            central.acknowledge_ordered_batch(&peer, id).await?;
                            if !owners.mark_completed(id, &peer, self.state.id) {
                                return Err(error(
                                    BleErrorCode::LifecycleInvariantViolation,
                                    BleErrorDomain::Cleanup,
                                    "gatt.ordered.ack-group",
                                ));
                            }
                        }
                        Ok(Value::Null)
                    }
                    GroupAction::ForgetGroup => {
                        if !owners.completed(id, &peer, self.state.id) {
                            return Err(error(
                                BleErrorCode::LifecycleInvalidState,
                                BleErrorDomain::Cleanup,
                                "gatt.ordered.forget-group",
                            ));
                        }
                        central.forget_ordered_batch(&peer, id).await?;
                        owners.remove(id, &peer, self.state.id);
                        Ok(Value::Null)
                    }
                    GroupAction::Status => {
                        let cleanup = central.ordered_batch_cleanup_complete(&peer, id).await?;
                        let lease = central.ordered_batch_source_lease(&peer, id).await?;
                        let (setup, fence) =
                            central.retained_ordered_batch_errors(&peer, id).await?;
                        let generation = lease.map(|lease| match lease.source_generation {
                            ubm_core::ordered_notifications::SourceGeneration::Counter(value) => {
                                serde_json::json!({"kind":"counter","value":value.to_string()})
                            }
                            ubm_core::ordered_notifications::SourceGeneration::Opaque(value) => {
                                serde_json::json!({"kind":"opaque","value":value})
                            }
                        });
                        let members: Vec<Value> = central.ordered_batch_member_status(&peer,id).await?.into_iter().map(|member| {
                            use ubm_desktop::central::OrderedMemberAcquisition;
                            let acquisition = match member.acquisition { OrderedMemberAcquisition::NotStarted => "not-started", OrderedMemberAcquisition::Acquired => "acquired", OrderedMemberAcquisition::Indeterminate => "indeterminate" };
                            let delivery = match member.effective_delivery { ObservedDelivery::Notification => "notification", ObservedDelivery::Indication => "indication", ObservedDelivery::Unknown => "unknown" };
                            serde_json::json!({"memberId":member.member_id.to_string(), "acquisition":acquisition, "effectiveDelivery":delivery, "cleanupReleased":member.cleanup_released, "cleanupError":member.cleanup_error.map(|e| Value::Object(wire::error_object(&e)))})
                        }).collect();
                        Ok(
                            serde_json::json!({"sourceGeneration":generation,"members":members,"cleanupComplete":cleanup,"setupError":setup.map(|e| Value::Object(wire::error_object(&e))),"fenceError":fence.map(|e| Value::Object(wire::error_object(&e)))}),
                        )
                    }
                }
            }
        }
    }

    async fn disconnect_exact_wire(
        &self,
        peer: &str,
        name: &str,
        identity: &str,
        ctl: OpControl,
    ) -> Result<Value, DesktopError> {
        let denied = || {
            cleanup_record(vec![cleanup_failure(
                "connection",
                &error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Connection,
                    "connection.disconnect.admission",
                ),
            )])
        };
        let (attempt, leader) = {
            let mut table = lock(&self.state.exact_leases);
            let Some(entry) = table
                .as_mut()
                .and_then(|table| table.records.get_mut(identity))
            else {
                return Ok(denied());
            };
            if entry.peer != peer || entry.name != name || entry.owner.is_none() {
                return Ok(denied());
            }
            if entry.released {
                return Ok(cleanup_record(Vec::new()));
            }
            if let Some(attempt) = entry
                .attempt
                .as_ref()
                .filter(|attempt| lock(&attempt.result).is_none())
            {
                (Arc::clone(attempt), false)
            } else {
                let attempt = Arc::new(ExactReleaseAttempt {
                    result: Mutex::new(None),
                    changed: Notify::new(),
                });
                entry.attempt = Some(Arc::clone(&attempt));
                (attempt, true)
            }
        };
        if !leader {
            let wait = async {
                loop {
                    let notified = attempt.changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if let Some(result) = lock(&attempt.result).clone() {
                        return Ok(result);
                    }
                    notified.await;
                }
            };
            return Ok(
                match bounded(&ctl, "connection.disconnect.join", wait).await {
                    Ok(result) => result,
                    Err(failure) => connection_cleanup_record(&failure),
                },
            );
        }
        let _leader = ExactReleaseLeader(Arc::clone(&attempt));
        let result = self
            .disconnect_exact_leader(peer, name, identity, ctl)
            .await;
        *lock(&attempt.result) = Some(result.clone());
        attempt.changed.notify_waiters();
        let mut table = lock(&self.state.exact_leases);
        if let Some(table) = table.as_mut() {
            if table
                .records
                .get(identity)
                .is_some_and(|entry| entry.released)
            {
                table.receipts.push_back(identity.to_owned());
                table.trim_receipts();
            }
        }
        Ok(result)
    }

    async fn disconnect_exact_leader(
        &self,
        peer: &str,
        name: &str,
        identity: &str,
        ctl: OpControl,
    ) -> Value {
        let gate = self.state.exact_connection_gate(peer);
        let _gate = match bounded(&ctl, "connection.disconnect.admission", async {
            Ok(gate.lock().await)
        })
        .await
        {
            Ok(gate) => gate,
            Err(failure) => return connection_cleanup_record(&failure),
        };
        let denied = || {
            cleanup_record(vec![cleanup_failure(
                "connection",
                &error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Connection,
                    "connection.disconnect.admission",
                ),
            )])
        };
        let owner = {
            let table = lock(&self.state.exact_leases);
            let Some(entry) = table.as_ref().and_then(|table| table.records.get(identity)) else {
                return denied();
            };
            if entry.peer != peer || entry.name != name {
                return denied();
            }
            if entry.released {
                return cleanup_record(Vec::new());
            }
            let Some(owner) = entry.owner.clone() else {
                return denied();
            };
            owner
        };
        let central = &self.host.central;
        if owner.attachment != central.attachment() {
            let wait = async {
                loop {
                    let notified = self.state.reset_changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if lock(&self.state.reset_leases).contains_key(&owner.admission) {
                        return Ok(());
                    }
                    notified.await;
                }
            };
            if let Err(failure) =
                bounded(&ctl, "connection.disconnect.reset-observation", wait).await
            {
                return connection_cleanup_record(&failure);
            }
            let failure = lock(&self.state.reset_leases)
                .get(&owner.admission)
                .and_then(|(_, _, failure)| failure.clone());
            if let Some(failure) = failure {
                return connection_cleanup_record(&failure);
            }
        } else if let Err(failure) = central
            .release_connection_lease_exact(peer, name, &owner.admission, ctl)
            .await
        {
            let reset = lock(&self.state.reset_leases)
                .get(&owner.admission)
                .map(|(_, _, failure)| failure.clone());
            match reset {
                Some(Some(failure)) => return connection_cleanup_record(&failure),
                Some(None) => {}
                None => return connection_cleanup_record(&failure),
            }
        }
        let replacement = {
            let mut guard = lock(&self.state.exact_leases);
            let table = guard.as_mut().expect("exact session");
            if let Some(entry) = table.records.get_mut(identity) {
                entry.released = true;
            }
            table
                .records
                .values()
                .filter(|entry| entry.peer == peer && !entry.released)
                .filter_map(|entry| entry.owner.as_ref())
                .find(|other| other.attachment == central.attachment())
                .cloned()
        };
        let mut leases = lock(&self.state.leases);
        if leases.get(peer) == Some(&owner) {
            if let Some(replacement) = replacement {
                leases.insert(peer.to_owned(), replacement);
            } else {
                leases.remove(peer);
            }
        }
        drop(leases);
        lock(&self.state.reset_leases).remove(&owner.admission);
        cleanup_record(Vec::new())
    }

    async fn execute(
        &self,
        body: Body,
        ctl: OpControl,
        release_version: u64,
    ) -> Result<Value, DesktopError> {
        let host = &*self.host;
        let central = &host.central;
        let ctl = match &body {
            Body::Read { peer_id, .. }
            | Body::Write { peer_id, .. }
            | Body::Subscribe { peer_id, .. } => {
                let lease = lock(&self.state.leases)
                    .get(peer_id)
                    .map(|lease| lease.name.clone())
                    .ok_or_else(|| {
                        error(
                            BleErrorCode::OwnershipDenied,
                            BleErrorDomain::Connection,
                            "gatt.admission",
                        )
                        .with_detail("this session holds no connection lease for GATT work")
                    })?;
                ctl.with_connection_lease(lease)
            }
            _ => ctl,
        };
        match body {
            Body::TerminalEvidenceReserve {
                scope,
                maximum_bytes,
            } => {
                let ledger = host.terminal_evidence_ledger().ok_or_else(|| {
                    unsupported(
                        "terminal-evidence.reserve",
                        "trusted terminal evidence storage was not configured",
                    )
                })?;
                let reservation = ledger
                    .reserve(&scope, maximum_bytes)
                    .map_err(terminal_evidence_error)?;
                Ok(
                    serde_json::json!({"maximumBytes": reservation.maximum_bytes, "reservationId": reservation.reservation_id}),
                )
            }
            Body::TerminalEvidenceCommit {
                reservation_id,
                canonical_bytes,
                digest,
            } => {
                let ledger = host.terminal_evidence_ledger().ok_or_else(|| {
                    unsupported(
                        "terminal-evidence.commit",
                        "trusted terminal evidence storage was not configured",
                    )
                })?;
                let committed = ledger
                    .commit(&reservation_id, &canonical_bytes, digest)
                    .map_err(terminal_evidence_error)?;
                Ok(
                    serde_json::json!({"kind":"committed","ref":committed.reference,"byteLength":committed.byte_length,"digest":terminal_digest_hex(committed.digest),"formatVersion":committed.format_version,"durability":"committed"}),
                )
            }
            Body::TerminalEvidenceRead {
                reference,
                maximum_bytes,
            } => {
                let ledger = host.terminal_evidence_ledger().ok_or_else(|| {
                    unsupported(
                        "terminal-evidence.read",
                        "trusted terminal evidence storage was not configured",
                    )
                })?;
                let (bytes, digest) = ledger
                    .read(&reference, maximum_bytes)
                    .map_err(terminal_evidence_error)?;
                Ok(
                    serde_json::json!({"canonicalBytesBase64": wire::encode_base64(&bytes), "digest": terminal_digest_hex(digest)}),
                )
            }
            Body::TerminalEvidenceRelease(reservation_id) => {
                let ledger = host.terminal_evidence_ledger().ok_or_else(|| {
                    unsupported(
                        "terminal-evidence.release",
                        "trusted terminal evidence storage was not configured",
                    )
                })?;
                let abandonment = ledger
                    .abandon(&reservation_id)
                    .map_err(terminal_evidence_error)?;
                host.release_subscription_terminal_evidence(&reservation_id);
                Ok(
                    serde_json::json!({"released":true,"retainedForRecovery":matches!(abandonment, ubm_desktop::terminal_evidence_ledger::TerminalEvidenceAbandonment::RetainedForRecovery)}),
                )
            }
            Body::RecordingReclaim {
                action,
                peer,
                collection,
                recording,
                capability,
                request,
                caller_key,
            } => {
                self.recording_reclaim(
                    &action,
                    &peer,
                    collection,
                    &recording,
                    &capability,
                    request,
                    caller_key.as_deref(),
                )
                .await
            }
            Body::ReclaimVersion => Ok(serde_json::json!({
                "wireVersion": if host.radio.ordered_reclaim_graph_supported() { 2 } else { 0 }
            })),
            Body::MarkerVersion => Ok(
                serde_json::json!({"wireVersion": if ubm_desktop::RadioBoundary::ordered_marker_write_supported(&host.radio) { 1 } else { 0 }}),
            ),
            Body::Marker {
                peer,
                collection,
                action,
            } => self.marker_command(&peer, collection, action).await,
            Body::Ordered(command) => self.ordered_command(command, ctl).await,
            Body::AdapterState => {
                let snapshot =
                    bounded(&ctl, "adapter.state", host.radio.adapter_snapshot()).await?;
                let updated_at = host.now_ms();
                *lock(&host.adapter) = Some((snapshot.clone(), updated_at));
                Ok(adapter_value(&snapshot, updated_at, &central.attachment()))
            }
            Body::Counters => self.counters().await,
            Body::ControlCapabilities => Ok(object(vec![(
                "subrate",
                Value::Bool(host.radio.connection_subrate_available()),
            )])),
            Body::ScanStart {
                start_operation_id,
                service_uuids,
                device_addresses,
                android,
                terminal_evidence_scope,
                deadline,
            } => {
                // The JS core must reserve this exact durable slot before
                // asking the native owner to activate the source. Validate it
                // before taking any local admission state or touching radio.
                host.verify_scan_terminal_evidence_scope(terminal_evidence_scope.as_ref())
                    .map_err(terminal_evidence_error)?;
                // Reserve the slot before the first await (X-R2): a second
                // concurrent start sees `Starting` and is refused, instead
                // of both joining and the loser orphaning the winner.
                let membership = {
                    let mut slot = lock(&self.state.scan);
                    if *slot != ScanSlot::Idle {
                        return Err(error(
                            BleErrorCode::ScanAlreadyActive,
                            BleErrorDomain::Scan,
                            "scan.start",
                        ));
                    }
                    let ordinal = self.state.scan_ordinal.fetch_add(1, Ordering::Relaxed) + 1;
                    let membership = format!("s{}-scan-{ordinal}", self.state.id);
                    *slot = ScanSlot::Starting {
                        membership: membership.clone(),
                    };
                    membership
                };
                let member = ScanMember {
                    membership: membership.clone(),
                    start_operation_id,
                    service_uuids,
                    device_addresses,
                    terminal_evidence_scope,
                    deadline,
                    expiry_cancel: OpTicket::new(),
                };
                // Reconciliation can be waiting on a different ticket's
                // cleanup-only scan stop. Cancellation of this admission
                // must answer promptly, but dropping join_scan would abandon
                // the old generation's cleanup authority and scan lock. A
                // normal scan keeps its original completion boundary, where
                // post-start compensation must finish before replying.
                let cancel = ctl.ticket.clone();
                let join_host = Arc::clone(&self.host);
                let progress = OP_RADIO.with(Arc::clone);
                let session_id = self.state.id;
                let (joined_tx, mut joined_rx) = tokio::sync::oneshot::channel();
                let (orphan_tx, mut orphan_rx) = tokio::sync::oneshot::channel();
                self.host.runtime.spawn(async move {
                    let result = OP_RADIO
                        .scope(
                            progress,
                            join_host.join_scan(session_id, member, android, ctl, orphan_tx),
                        )
                        .await;
                    let _ = joined_tx.send(result);
                });
                // The host publishes this signal from inside the scan lock,
                // at the exact cleanup-only branch. No read-then-join gap can
                // classify an orphan created by a competing operation as a
                // normal scan start.
                let before_orphan = tokio::select! {
                    biased;
                    joined = &mut joined_rx => Some(joined),
                    signalled = &mut orphan_rx => {
                        if signalled.is_ok() { None } else { Some((&mut joined_rx).await) }
                    }
                };
                let joined = if let Some(joined) = before_orphan {
                    joined
                } else {
                    tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        let host = Arc::clone(&self.host);
                        let state = Arc::clone(&self.state);
                        let abandoned_membership = membership.clone();
                        self.host.runtime.spawn(async move {
                            // A cancelled waiter still owns the outcome of
                            // the in-flight admission. If it somehow joined
                            // at the cancellation boundary, retire that
                            // membership; failed stop stays visible in the
                            // session slot for the ordinary disposal retry.
                            let joined = joined_rx.await;
                            if matches!(joined, Ok(Ok(()))) {
                                {
                                    let mut slot = lock(&state.scan);
                                    if slot.membership() == Some(abandoned_membership.as_str()) {
                                        *slot = ScanSlot::Active {
                                            membership: abandoned_membership.clone(),
                                        };
                                    }
                                }
                                if host.leave_scan(session_id, &abandoned_membership, OpControl::unbounded()).await.is_ok() {
                                    state.clear_scan(&abandoned_membership);
                                }
                            } else {
                                state.clear_scan(&abandoned_membership);
                            }
                        });
                        return Err(error(
                            BleErrorCode::OperationAborted,
                            BleErrorDomain::Scan,
                            "scan.start",
                        ));
                    }
                    joined = &mut joined_rx => joined,
                    }
                };
                let joined = joined.unwrap_or_else(|_| {
                    Err(error(
                        BleErrorCode::ScanStartFailed,
                        BleErrorDomain::Scan,
                        "scan.start",
                    )
                    .with_detail("scan admission task ended without an outcome"))
                });
                if let Err(error) = joined {
                    let mut slot = lock(&self.state.scan);
                    if slot.membership() == Some(membership.as_str()) {
                        *slot = ScanSlot::Idle;
                    }
                    return Err(error);
                }
                {
                    // Bind the comparison first: the borrow of the slot must
                    // not live across the `leave_scan` await below.
                    let ours = {
                        let mut slot = lock(&self.state.scan);
                        if slot.membership() == Some(membership.as_str()) {
                            *slot = ScanSlot::Active {
                                membership: membership.clone(),
                            };
                            true
                        } else {
                            false
                        }
                    };
                    if !ours {
                        // The host ended our membership while we were
                        // joining (adapter loss, shutdown): release what the
                        // join admitted instead of reporting a live scan.
                        let _ = host
                            .leave_scan(self.state.id, &membership, OpControl::unbounded())
                            .await;
                        return Err(error(
                            BleErrorCode::ScanStartFailed,
                            BleErrorDomain::Scan,
                            "scan.start",
                        )
                        .with_detail("the host ended the scan while it was starting"));
                    }
                }
                self.host.arm_scan_deadline(self.state.id, &membership);
                Ok(object(vec![("operationId", Value::from(membership))]))
            }
            Body::ScanStop(membership) => {
                // Only an `Active` membership stops: a `Starting` one is
                // still inside its start (stop after it completes), and any
                // other value was never ours.
                let ours = {
                    let slot = lock(&self.state.scan);
                    matches!(&*slot, ScanSlot::Active { membership: current } if current == &membership)
                };
                if !ours {
                    return Err(error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Scan,
                        "scan.stop",
                    )
                    .with_detail("scan-not-active: no such scan in this session"));
                }
                host.leave_scan(self.state.id, &membership, ctl).await?;
                self.state.clear_scan(&membership);
                Ok(cleanup_record(Vec::new()))
            }
            Body::PeersResolve(peer_id) => {
                let known = lock(&host.directory).contains_key(&peer_id)
                    || lock(&host.restored).contains_key(&peer_id);
                let record = self.peer_record(&peer_id).await;
                if !known && record.is_none() {
                    let resolved = match bounded(
                        &ctl,
                        "peers.resolve",
                        host.radio.call(|id| RadioRequest::ResolvePeer {
                            id,
                            peer_id: peer_id.clone(),
                        }),
                    )
                    .await?
                    {
                        RadioCompletion::ResolvedPeer(peer) => peer,
                        _ => return Err(protocol("peers.resolve")),
                    };
                    let Some(peer) = resolved else {
                        return Ok(Value::Null);
                    };
                    if peer.peer_id != peer_id {
                        return Err(protocol("peers.resolve.identity"));
                    }
                    let mut value = host.peer_value(&peer_id, None, Some("app-reference"));
                    value["name"] = peer.name.map_or(Value::Null, Value::from);
                    return Ok(value);
                }
                Ok(host.peer_value(&peer_id, record.as_ref(), None))
            }
            Body::PeersKnown | Body::PeersRestored => {
                let records = central.peer_records().await;
                let mut ids: Vec<String> = match body {
                    Body::PeersRestored => lock(&host.restored).keys().cloned().collect(),
                    _ => {
                        let mut ids: Vec<String> = lock(&host.directory).keys().cloned().collect();
                        ids.extend(lock(&host.restored).keys().cloned());
                        ids.extend(records.iter().map(|record| record.peer_id.clone()));
                        ids
                    }
                };
                ids.sort();
                ids.dedup();
                let values = ids
                    .iter()
                    .map(|peer_id| {
                        let record = records.iter().find(|record| record.peer_id == *peer_id);
                        (record, host.peer_value(peer_id, record, None))
                    })
                    .map(|(_, value)| value)
                    .collect();
                Ok(Value::Array(values))
            }
            Body::ClaimRestored(max_peers) => {
                let claimed: Vec<String> = {
                    let restored = lock(&host.restored);
                    let mut claims = lock(&host.restoration_claims);
                    let unclaimed: Vec<String> = restored
                        .keys()
                        .filter(|peer_id| !claims.contains_key(*peer_id))
                        .cloned()
                        .collect();
                    if unclaimed.len() as u64 > max_peers {
                        return Err(error(
                            BleErrorCode::BytesTooLarge,
                            BleErrorDomain::Restoration,
                            "peers.claim-restored",
                        )
                        .with_detail("more unclaimed restored peers than the journal can hold"));
                    }
                    for peer_id in &unclaimed {
                        claims.insert(peer_id.clone(), self.state.id);
                    }
                    unclaimed
                };
                let records = central.peer_records().await;
                let peers = claimed
                    .iter()
                    .map(|peer_id| {
                        let record = records.iter().find(|record| record.peer_id == *peer_id);
                        host.peer_value(peer_id, record, Some("restored"))
                    })
                    .collect();
                Ok(object(vec![("peers", Value::Array(peers))]))
            }
            Body::PeersConnected(services) => {
                let peers = match bounded(
                    &ctl,
                    "peers.connected",
                    host.radio.call(|id| RadioRequest::ConnectedPeers {
                        id,
                        services: services.clone(),
                    }),
                )
                .await?
                {
                    RadioCompletion::ConnectedPeers(peers) => peers,
                    _ => return Err(protocol("peers.connected")),
                };
                if peers.len() > 4096 {
                    return Err(error(
                        BleErrorCode::CapabilityLimited,
                        BleErrorDomain::Capability,
                        "peers.connected",
                    )
                    .with_detail("system inventory exceeds the bounded directory capacity"));
                }
                let mut seen = std::collections::HashSet::new();
                let mut values = Vec::new();
                for peer in peers {
                    if peer.peer_id.is_empty() || !seen.insert(peer.peer_id.clone()) {
                        return Err(protocol("peers.connected.identity"));
                    }
                    // This read creates neither cached ownership nor a central peer.
                    let mut value = host.peer_value(&peer.peer_id, None, Some("system-connected"));
                    value["name"] = peer.name.map_or(Value::Null, Value::from);
                    value["rssi"] = Value::Null;
                    value["lastSeenAtMonotonicMs"] = Value::Null;
                    value["connection"] = Value::from("connected");
                    value["reachability"] = Value::from("reachable");
                    values.push(value);
                }
                Ok(Value::Array(values))
            }
            Body::PeersBonded => {
                let peers = match bounded(
                    &ctl,
                    "peers.bonded",
                    host.radio.call(|id| RadioRequest::BondedPeers { id }),
                )
                .await?
                {
                    RadioCompletion::BondedPeers(peers) => peers,
                    _ => return Err(protocol("peers.bonded")),
                };
                let records = central.peer_records().await;
                let values = peers
                    .into_iter()
                    .map(|peer| {
                        host.note_peer(&peer.peer_id, peer.name.clone(), "system-bonded");
                        lock(&host.security)
                            .entry(peer.peer_id.clone())
                            .and_modify(|entry| entry.state.bond = BondState::Bonded)
                            .or_insert(SecurityEntry::new(crate::radio::SecurityState {
                                bond: BondState::Bonded,
                                encryption: crate::radio::EncryptionState::Unknown,
                                authentication: crate::radio::AuthenticationState::Unknown,
                                secure_connections: crate::radio::SecureConnectionsState::Unknown,
                                pairing_possible: None,
                            }));
                        let record = records.iter().find(|r| r.peer_id == peer.peer_id);
                        host.peer_value(&peer.peer_id, record, Some("system-bonded"))
                    })
                    .collect();
                Ok(Value::Array(values))
            }
            Body::Connect {
                peer_id,
                lease,
                staging,
            } => {
                let exact = lock(&self.state.exact_leases).is_some();
                let exact_gate = exact.then(|| self.state.exact_connection_gate(&peer_id));
                let _exact_gate = if exact {
                    Some(
                        bounded(&ctl, "connection.connect.admission", async {
                            Ok(exact_gate.as_ref().expect("exact gate").lock().await)
                        })
                        .await?,
                    )
                } else {
                    None
                };
                if !staging.preferred_phy.is_empty()
                    && matches!(
                        self.peer_record(&peer_id)
                            .await
                            .and_then(|record| record.connection_state),
                        Some(ConnectionState::Connecting | ConnectionState::Connected)
                    )
                {
                    // The PHYs were chosen when the live link was
                    // established; a lease on it could not honour them.
                    return Err(unsupported(
                        "connection.connect.preferred-phy",
                        "the link is already established; request a PHY change with connection.request-phy",
                    ));
                }
                let core_lease = self.state.core_name(&lease);
                let reservation = if exact {
                    let mut guard = lock(&self.state.exact_leases);
                    let table = guard.as_mut().expect("exact session");
                    if table.records.values().any(|entry| {
                        entry.peer == peer_id
                            && entry.name == core_lease
                            && !entry.released
                            && entry
                                .owner
                                .as_ref()
                                .is_some_and(|owner| owner.attachment == central.attachment())
                    }) {
                        return Err(error(
                            BleErrorCode::LifecycleInvalidState,
                            BleErrorDomain::Connection,
                            "connection.connect.admission",
                        )
                        .with_detail("this lease name is already admitted on the live link"));
                    }
                    if table
                        .records
                        .values()
                        .filter(|entry| !entry.released)
                        .count()
                        >= table.live_limit
                    {
                        return Err(error(
                            BleErrorCode::LifecycleInvalidState,
                            BleErrorDomain::Cleanup,
                            "connection.connect.admission",
                        )
                        .with_detail("exact connection live and cleanup capacity exhausted"));
                    }
                    let sequence = table.next;
                    table.next = sequence.checked_add(1).ok_or_else(|| {
                        error(
                            BleErrorCode::LifecycleInvariantViolation,
                            BleErrorDomain::Core,
                            "connection.connect.admission",
                        )
                        .with_detail("exact connection identity exhausted")
                    })?;
                    let identity = format!("{}:{sequence}", table.namespace);
                    table.records.insert(
                        identity.clone(),
                        ExactLeaseRecord {
                            peer: peer_id.clone(),
                            name: core_lease.clone(),
                            owner: None,
                            released: false,
                            attempt: None,
                        },
                    );
                    Some(ExactLeaseReservation {
                        table: &self.state.exact_leases,
                        identity,
                    })
                } else {
                    None
                };
                // The peer's connect section (X-R3): stage, dispatch and
                // cleanup run under mutual exclusion, so a concurrent
                // same-peer connect can neither overwrite this staging nor
                // wipe it in its own cleanup. A dead op never stages: the
                // wait is cancellation- and deadline-aware.
                #[cfg(test)]
                self.state
                    .pause_publication_for_test("connect-before-admission")
                    .await;
                let section = host.radio.lock_connect_section(&peer_id, &ctl).await?;
                host.radio.stage_connect(&peer_id, staging);
                let mut reset_events = central.adapter_reset_events();
                let connected = central
                    .connect_with_admission(&peer_id, &core_lease, ctl)
                    .await;
                host.radio.clear_staging(Some(&peer_id), None, false);
                drop(section);
                let (handle, admission) = connected?;
                let attachment = admission.attachment().clone();
                let owner = OwnedLease {
                    name: core_lease.clone(),
                    attachment: attachment.clone(),
                    admission,
                };
                if let Some(reservation) = &reservation {
                    let mut table = lock(&self.state.exact_leases);
                    table
                        .as_mut()
                        .expect("exact session")
                        .records
                        .get_mut(&reservation.identity)
                        .expect("reserved identity")
                        .owner = Some(owner.clone());
                }
                let generation = handle.connection_generation.ok_or_else(|| {
                    error(
                        BleErrorCode::LifecycleInvariantViolation,
                        BleErrorDomain::Core,
                        "connection.connect",
                    )
                    .with_detail("connected without a connection generation")
                })?;
                #[cfg(test)]
                self.state.pause_publication_for_test("connect").await;
                let published = central
                    .publish_if_attached(&attachment, || {
                        let mut leases = lock(&self.state.leases);
                        leases.insert(peer_id.clone(), owner.clone());
                        host.note_peer(&peer_id, None, "app-reference");
                    })
                    .await;
                if published.is_none() {
                    // The reset already retired the exact admitted core lease.
                    // Remember its terminal identity without disturbing a replacement.
                    let failure =
                        reset_release_failure(&mut reset_events, &attachment, &peer_id).await;
                    let mut ended = lock(&self.state.reset_leases);
                    ended.entry(owner.admission.clone()).or_insert((
                        peer_id.clone(),
                        owner,
                        failure,
                    ));
                }
                let mut result = vec![
                    ("peerKey", Value::from(handle.peer_key)),
                    ("connectionGeneration", Value::from(generation.as_str())),
                ];
                if let Some(reservation) = &reservation {
                    result.push(("releaseIdentity", Value::from(reservation.identity.clone())));
                }
                Ok(object(result))
            }
            Body::Disconnect {
                peer_id,
                lease,
                release_identity,
            } => {
                let core_lease = self.state.core_name(&lease);
                if let Some(identity) = release_identity {
                    return self
                        .disconnect_exact_wire(&peer_id, &core_lease, &identity, ctl)
                        .await;
                }
                let owned = lock(&self.state.leases)
                    .get(&peer_id)
                    .cloned()
                    .filter(|owned| owned.name == core_lease);
                #[cfg(test)]
                self.state
                    .pause_publication_for_test("disconnect-owner")
                    .await;
                if owned
                    .as_ref()
                    .is_some_and(|owner| owner.attachment == central.attachment())
                    && lock(&self.state.reset_leases)
                        .values()
                        .any(|(peer, old, failure)| {
                            peer == &peer_id && old.name == core_lease && failure.is_some()
                        })
                {
                    let failure = error(
                        BleErrorCode::LifecycleInvariantViolation,
                        BleErrorDomain::Cleanup,
                        "connection.disconnect.reset-observation",
                    )
                    .with_detail(
                        "lease name identifies both a live owner and an unresolved reset failure",
                    );
                    return Ok(connection_cleanup_record(&failure));
                }
                if let Some(owned) = owned
                    .as_ref()
                    .filter(|owned| owned.attachment != central.attachment())
                {
                    let wait = async {
                        loop {
                            let notified = self.state.reset_changed.notified();
                            tokio::pin!(notified);
                            notified.as_mut().enable();
                            {
                                let leases = lock(&self.state.leases);
                                let ended = lock(&self.state.reset_leases);
                                if ended.contains_key(&owned.admission) {
                                    return Ok(());
                                }
                                if leases.get(&peer_id) != Some(owned) {
                                    return Err(error(BleErrorCode::OperationReset, BleErrorDomain::Cleanup,
                                        "connection.disconnect.reset-observation")
                                        .with_detail("the original lease changed before reset cleanup was observed"));
                                }
                            }
                            notified.await;
                        }
                    };
                    if let Err(failure) =
                        bounded(&ctl, "connection.disconnect.reset-observation", wait).await
                    {
                        return Ok(connection_cleanup_record(&failure));
                    }
                }
                if owned
                    .as_ref()
                    .is_none_or(|owned| owned.attachment != central.attachment())
                {
                    let mut ended = lock(&self.state.reset_leases);
                    let matches: Vec<_> = ended
                        .iter()
                        .filter(|(_, (peer, lease, _))| {
                            peer == &peer_id
                                && lease.name == core_lease
                                && owned
                                    .as_ref()
                                    .is_none_or(|owner| owner.admission == lease.admission)
                        })
                        .map(|(id, _)| id.clone())
                        .collect();
                    if matches.len() > 1 {
                        let failure = error(
                            BleErrorCode::LifecycleInvariantViolation,
                            BleErrorDomain::Cleanup,
                            "connection.disconnect.reset-observation",
                        )
                        .with_detail("multiple retired operations share this lease name");
                        return Ok(connection_cleanup_record(&failure));
                    }
                    if let Some(id) = matches.first() {
                        let failure = &ended[id].2;
                        let failures = failure
                            .as_ref()
                            .map_or_else(Vec::new, connection_cleanup_failures);
                        if failure.is_none() {
                            ended.remove(id);
                        }
                        return Ok(cleanup_record(failures));
                    }
                }
                #[cfg(test)]
                self.state
                    .pause_publication_for_test("disconnect-before-central-release")
                    .await;
                let Some(owner) = owned.as_ref() else {
                    let failure = error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Connection,
                        "connection.disconnect.admission",
                    );
                    return Ok(connection_cleanup_record(&failure));
                };
                let release = central
                    .release_connection_lease_exact(&peer_id, &core_lease, &owner.admission, ctl)
                    .await;
                // Exercise a structured central error at the public-result boundary;
                // today's mobile native completion cannot yet carry one.
                #[cfg(test)]
                let release = release.map_err(|failure| {
                    lock(&self.state.disconnect_failure_override)
                        .take()
                        .unwrap_or(failure)
                });
                match release {
                    Ok(_) => {
                        let mut leases = lock(&self.state.leases);
                        if owned
                            .as_ref()
                            .is_some_and(|owner| leases.get(&peer_id) == Some(owner))
                        {
                            leases.remove(&peer_id);
                        }
                        Ok(cleanup_record(Vec::new()))
                    }
                    Err(error) => {
                        let mut ended = lock(&self.state.reset_leases);
                        match ended.get(&owner.admission) {
                            Some((_, _, Some(failure))) => Ok(connection_cleanup_record(failure)),
                            Some((_, _, None)) => {
                                ended.remove(&owner.admission);
                                Ok(cleanup_record(Vec::new()))
                            }
                            None => Ok(connection_cleanup_record(&error)),
                        }
                    }
                }
            }
            Body::Control {
                peer_id,
                lease,
                control,
            } => self.control(&peer_id, &lease, control, ctl).await,
            Body::SecurityState(peer_id) => {
                let state = self.security_state(&peer_id, &ctl).await?;
                Ok(security_value(&state))
            }
            Body::CancelPairing(peer_id) => {
                let peer = peer_id.clone();
                match awaited(
                    &ctl,
                    "security.cancel-pairing",
                    host.radio
                        .call(|id| RadioRequest::CancelBond { id, peer_id: peer }),
                )
                .await?
                {
                    RadioCompletion::Unit => Ok(object(vec![("state", Value::from("requested"))])),
                    _ => Err(protocol("security.cancel-pairing")),
                }
            }
            Body::Pair { peer_id, transport } => {
                let current = self.security_state(&peer_id, &ctl).await?;
                if transport == PairTransport::Auto && current.bond == BondState::Bonded {
                    return Ok(object(vec![
                        ("outcome", Value::from("already-paired")),
                        ("state", security_value(&current)),
                    ]));
                }
                let expected_owner = host.security_owner(&peer_id);
                let peer = peer_id.clone();
                let state = match awaited(
                    &ctl,
                    "security.pair",
                    host.radio.call(|id| RadioRequest::CreateBond {
                        id,
                        peer_id: peer,
                        transport,
                    }),
                )
                .await?
                {
                    RadioCompletion::Security(state) => state,
                    _ => return Err(protocol("security.pair")),
                };
                host.set_security_state_if_owner(
                    &peer_id,
                    expected_owner.as_deref(),
                    state.clone(),
                    "security.pair",
                )?;
                Ok(object(vec![
                    (
                        "outcome",
                        Value::from(if state.bond == BondState::Bonded {
                            "paired"
                        } else {
                            "rejected"
                        }),
                    ),
                    ("state", security_value(&state)),
                ]))
            }
            Body::Discover { peer_id, lease } => {
                let core_lease = self.state.core_name(&lease);
                central.discover(&peer_id, &core_lease, ctl).await?;
                let paths = central.discovered_paths(&peer_id).await?;
                let record = self.peer_record(&peer_id).await;
                let connection_generation = record
                    .as_ref()
                    .and_then(|record| record.connection_generation.clone())
                    .ok_or_else(|| {
                        error(
                            BleErrorCode::ConnectionStale,
                            BleErrorDomain::Connection,
                            "gatt.discover",
                        )
                    })?;
                let database_generation = record
                    .as_ref()
                    .and_then(crate::compat::peer_database_generation)
                    .ok_or_else(|| {
                        error(
                            BleErrorCode::CapabilityUnavailable,
                            BleErrorDomain::Capability,
                            "gatt.discover.database-generation",
                        )
                        .with_detail("the core does not expose the database generation yet")
                    })?;
                Ok(object(vec![
                    ("connectionGeneration", Value::from(connection_generation)),
                    ("databaseGeneration", Value::from(database_generation)),
                    ("services", discovery_tree(&paths)),
                ]))
            }
            Body::Read {
                peer_id,
                selector,
                descriptor,
            } => {
                if descriptor {
                    let bytes = central.read_descriptor(&peer_id, &selector, ctl).await?;
                    return Ok(object(vec![(
                        "valueB64",
                        Value::from(wire::encode_base64(&bytes)),
                    )]));
                }
                let read = central.read(&peer_id, &selector, ctl).await?;
                Ok(object(vec![
                    ("valueB64", Value::from(wire::encode_base64(&read.value))),
                    ("provenance", Value::from(read.provenance.as_str())),
                ]))
            }
            Body::Write {
                peer_id,
                selector,
                value,
                with_response,
                descriptor,
                wait_ready,
            } => {
                if descriptor {
                    central
                        .write_descriptor(&peer_id, &selector, value, ctl)
                        .await?;
                } else {
                    let mode = if with_response {
                        "with-response"
                    } else {
                        "without-response"
                    };
                    if wait_ready {
                        central
                            .write_when_ready(&peer_id, &selector, value, ctl)
                            .await?;
                    } else {
                        central.write(&peer_id, &selector, value, mode, ctl).await?;
                    }
                }
                Ok(object(vec![(
                    "commitState",
                    Value::from(if with_response {
                        "confirmed"
                    } else {
                        "unknown"
                    }),
                )]))
            }
            Body::Subscribe {
                peer_id,
                selector,
                consumer,
                mode,
                terminal_evidence_scope,
                recording,
            } => {
                self.subscribe(
                    peer_id,
                    selector,
                    consumer,
                    mode,
                    terminal_evidence_scope,
                    recording,
                    ctl,
                )
                .await
            }
            Body::Unsubscribe {
                peer_id,
                selector,
                consumer,
            } => {
                let subscription = lock(&self.state.subscriptions)
                    .get(&consumer)
                    .cloned()
                    .filter(|s| s.peer_id == peer_id && s.scope == scope_of(&peer_id, &selector));
                if subscription
                    .as_ref()
                    .is_some_and(|owner| owner.attachment == central.attachment())
                    && lock(&self.state.reset_subscriptions)
                        .values()
                        .any(|(name, old, failure)| {
                            name == &consumer
                                && old.peer_id == peer_id
                                && old.scope == scope_of(&peer_id, &selector)
                                && failure.is_some()
                        })
                {
                    return Err(error(BleErrorCode::LifecycleInvariantViolation, BleErrorDomain::Cleanup,
                        "gatt.unsubscribe.reset-observation").with_detail("consumer name identifies both a live owner and an unresolved reset failure"));
                }
                if let Some(owned) = subscription
                    .as_ref()
                    .filter(|owned| owned.attachment != central.attachment())
                {
                    let wait = async {
                        loop {
                            let notified = self.state.reset_changed.notified();
                            tokio::pin!(notified);
                            notified.as_mut().enable();
                            {
                                let subscriptions = lock(&self.state.subscriptions);
                                let ended = lock(&self.state.reset_subscriptions);
                                if ended.contains_key(&owned.admission) {
                                    return Ok(());
                                }
                                if !subscriptions
                                    .get(&consumer)
                                    .is_some_and(|current| current.admission == owned.admission)
                                {
                                    return Err(error(BleErrorCode::OperationReset, BleErrorDomain::Cleanup,
                                        "gatt.unsubscribe.reset-observation").with_detail("the original subscription changed before reset cleanup was observed"));
                                }
                            }
                            notified.await;
                        }
                    };
                    bounded(&ctl, "gatt.unsubscribe.reset-observation", wait).await?;
                }
                let reset = if subscription
                    .as_ref()
                    .is_none_or(|owned| owned.attachment != central.attachment())
                {
                    let mut ended = lock(&self.state.reset_subscriptions);
                    let matches: Vec<_> = ended
                        .iter()
                        .filter(|(_, (name, sub, _))| {
                            name == &consumer
                                && sub.peer_id == peer_id
                                && sub.scope == scope_of(&peer_id, &selector)
                                && subscription
                                    .as_ref()
                                    .is_none_or(|owner| owner.admission == sub.admission)
                        })
                        .map(|(id, _)| id.clone())
                        .collect();
                    if matches.len() > 1 {
                        return Err(error(
                            BleErrorCode::LifecycleInvariantViolation,
                            BleErrorDomain::Cleanup,
                            "gatt.unsubscribe.reset-observation",
                        )
                        .with_detail("multiple retired operations share this consumer name"));
                    }
                    matches.first().map(|id| {
                        let failure = ended[id].2.clone();
                        if failure.is_none() {
                            ended.remove(id);
                        }
                        failure
                    })
                } else {
                    None
                };
                if let Some(failure) = reset {
                    if let Some(failure) = failure {
                        return Err(failure);
                    }
                    return Ok(object(vec![
                        ("state", Value::from("released")),
                        // The reset's cleanup disabled it, not this call.
                        ("physicalDisabled", Value::Bool(false)),
                    ]));
                }
                let Some(subscription) = subscription else {
                    return Err(error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Gatt,
                        "gatt.unsubscribe",
                    )
                    .with_detail("no such consumer in this session"));
                };
                #[cfg(test)]
                self.state
                    .pause_publication_for_test("unsubscribe-before-central-release")
                    .await;
                if subscription.attachment != central.attachment() {
                    let mut ended = lock(&self.state.reset_subscriptions);
                    if let Some((_, _, failure)) = ended.get(&subscription.admission) {
                        if let Some(failure) = failure {
                            return Err(failure.clone());
                        }
                        ended.remove(&subscription.admission);
                        return Ok(object(vec![
                            ("state", Value::from("released")),
                            ("physicalDisabled", Value::Bool(false)),
                        ]));
                    }
                }
                let physical_disabled = if host
                    .recorded_route(&subscription.scope, self.state.id, &consumer)
                    .is_some()
                {
                    host.unsubscribe_recorded(
                        &subscription.scope,
                        self.state.id,
                        &consumer,
                        &subscription.core_consumer,
                        &subscription.selector,
                        ctl,
                    )
                    .await?
                } else {
                    central
                        .unsubscribe(
                            &peer_id,
                            &subscription.selector,
                            &subscription.core_consumer,
                            ctl,
                        )
                        .await?
                };
                let mut subscriptions = lock(&self.state.subscriptions);
                if subscriptions
                    .get(&consumer)
                    .is_some_and(|current| current.admission == subscription.admission)
                {
                    host.remove_route(&subscription.scope, self.state.id, &consumer);
                    subscriptions.remove(&consumer);
                }
                Ok(object(vec![
                    ("state", Value::from("released")),
                    ("physicalDisabled", Value::Bool(physical_disabled)),
                ]))
            }
            Body::Cancel {
                operation_id,
                admission,
            } => {
                let target = {
                    let mut ops = lock(&self.state.ops);
                    match ops.live.get(&operation_id) {
                        Some((ticket, live)) if *live == admission => {
                            CancelTarget::Live(ticket.clone())
                        }
                        Some(_) => return Err(wire::invalid("args.op.cancel.admission")),
                        None if admission <= ops.highest_admission => CancelTarget::Terminal,
                        None if admission - ops.highest_admission > ADMISSION_WINDOW => {
                            return Err(wire::invalid("args.op.cancel.admission"));
                        }
                        None => {
                            ops.cancelled_ahead.insert(admission);
                            CancelTarget::Ahead
                        }
                    }
                };
                let state = match target {
                    CancelTarget::Live(ticket) => match central.cancel(&ticket).await? {
                        CancelAck::RecordedBeforeAdmission | CancelAck::Forwarded { .. } => {
                            "cancellation-requested"
                        }
                        CancelAck::AlreadySettled => "already-terminal",
                    },
                    CancelTarget::Terminal => "already-terminal",
                    CancelTarget::Ahead => "cancellation-requested",
                };
                Ok(object(vec![("state", Value::from(state))]))
            }
            Body::BackgroundAcquire { kind, reason } => {
                let lease_id = match bounded(
                    &ctl,
                    "background.acquire",
                    host.radio
                        .call(|id| RadioRequest::AcquireBackground { id, kind, reason }),
                )
                .await?
                {
                    RadioCompletion::Lease(lease_id) if !lease_id.is_empty() => lease_id,
                    _ => return Err(protocol("background.acquire")),
                };
                host.note_background(&self.state.background_scope, lease_id.clone());
                Ok(object(vec![("leaseId", Value::from(lease_id))]))
            }
            Body::BackgroundRelease(lease_id) => {
                if !host.holds_background(&self.state.background_scope, &lease_id) {
                    return Err(error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Core,
                        "background.release",
                    )
                    .with_detail("this session's background scope holds no such lease"));
                }
                match host
                    .release_background(&self.state.background_scope, &lease_id, &ctl)
                    .await
                {
                    Ok(()) => Ok(cleanup_record(Vec::new())),
                    Err(error) => Ok(cleanup_record(vec![cleanup_failure("background", &error)])),
                }
            }
            Body::BackgroundNotification {
                lease_id,
                title,
                body,
            } => {
                if !host.holds_background(&self.state.background_scope, &lease_id) {
                    return Err(error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Core,
                        "background.update-notification",
                    ));
                }
                match bounded(
                    &ctl,
                    "background.update-notification",
                    host.radio
                        .call(|id| RadioRequest::UpdateBackgroundNotification {
                            id,
                            lease_id,
                            title,
                            body,
                        }),
                )
                .await?
                {
                    RadioCompletion::Unit => Ok(object(vec![("state", Value::from("updated"))])),
                    _ => Err(protocol("background.update-notification")),
                }
            }
            Body::CompanionAssociate {
                name,
                service_uuid,
                filters_json,
            } => {
                match awaited(
                    &ctl,
                    "companion.associate",
                    host.radio.call_owned_companion(self.state.id, |id| {
                        RadioRequest::AssociateCompanion {
                            id,
                            name,
                            service_uuid,
                            filters_json,
                        }
                    }),
                )
                .await?
                {
                    RadioCompletion::Companion {
                        association_id,
                        peer_id,
                        display_name,
                        already_associated,
                    } => {
                        if let Some(peer_id) = &peer_id {
                            host.note_peer(peer_id, display_name.clone(), "origin-authorized");
                        }
                        // Finding 236: the platform already held this
                        // association and created nothing new. The result
                        // reports what happened so the caller can tell.
                        let source = if already_associated {
                            "already-associated"
                        } else {
                            "associated"
                        };
                        Ok(object(vec![
                            ("source", Value::from(source)),
                            ("associationId", Value::from(association_id)),
                            ("peerId", opt_text(peer_id.as_deref())),
                            ("displayName", opt_text(display_name.as_deref())),
                        ]))
                    }
                    _ => Err(protocol("companion.associate")),
                }
            }
            Body::CompanionList => {
                match bounded(
                    &ctl,
                    "companion.list",
                    host.radio.call(|id| RadioRequest::ListCompanion { id }),
                )
                .await?
                {
                    RadioCompletion::CompanionList(records) => {
                        let values = records
                            .into_iter()
                            .map(|record| {
                                object(vec![
                                    ("associationId", Value::from(record.association_id)),
                                    ("peerId", opt_text(record.peer_id.as_deref())),
                                    ("displayName", opt_text(record.display_name.as_deref())),
                                ])
                            })
                            .collect();
                        Ok(object(vec![("associations", Value::Array(values))]))
                    }
                    _ => Err(protocol("companion.list")),
                }
            }
            Body::CompanionDisassociate { association_id } => {
                match bounded(
                    &ctl,
                    "companion.disassociate",
                    host.radio
                        .call(|id| RadioRequest::DisassociateCompanion { id, association_id }),
                )
                .await?
                {
                    RadioCompletion::Unit => Ok(object(vec![
                        ("state", Value::from("disassociated")),
                        ("associationId", Value::from(association_id)),
                    ])),
                    _ => Err(protocol("companion.disassociate")),
                }
            }
            Body::PresenceObserve { peer_id } => {
                match awaited(
                    &ctl,
                    "presence.observe",
                    host.radio
                        .call(|id| RadioRequest::ObservePresence { id, peer_id }),
                )
                .await?
                {
                    RadioCompletion::Unit => Ok(object(vec![("state", Value::from("observing"))])),
                    _ => Err(protocol("presence.observe")),
                }
            }
            Body::PresenceUnobserve { peer_id } => {
                match awaited(
                    &ctl,
                    "presence.unobserve",
                    host.radio
                        .call(|id| RadioRequest::StopPresence { id, peer_id }),
                )
                .await?
                {
                    RadioCompletion::Unit => Ok(object(vec![("state", Value::from("idle"))])),
                    _ => Err(protocol("presence.unobserve")),
                }
            }
            Body::Reconcile => self.reconcile(&ctl).await,
            Body::Quiesce => {
                let Some(loss) = self.host.seal_scan_ingress(&self.state) else {
                    return Ok(object(vec![("state", Value::from("pending"))]));
                };
                Ok(object(vec![
                    ("state", Value::from("sealed")),
                    ("afterCutoffItems", Value::from(loss.items)),
                    ("afterCutoffBytes", Value::from(loss.bytes)),
                ]))
            }
            Body::ContinuationDispose => {
                let failures = self.release(release_version).await;
                let loss = self.state.outbox.after_cutoff_loss();
                Ok(object(vec![
                    (
                        "state",
                        Value::from(if failures.is_empty() {
                            "released"
                        } else {
                            "release-failed"
                        }),
                    ),
                    ("failures", Value::Array(failures)),
                    ("afterCutoffItems", Value::from(loss.items)),
                    ("afterCutoffBytes", Value::from(loss.bytes)),
                ]))
            }
            Body::Dispose => Ok(cleanup_record(self.release(release_version).await)),
        }
    }

    async fn security_state(
        &self,
        peer_id: &str,
        ctl: &OpControl,
    ) -> Result<crate::radio::SecurityState, DesktopError> {
        let peer = peer_id.to_owned();
        let peer_key = Some(peer.clone());
        let expected_owner = self.host.security_owner(peer_id);
        let opening_failures = if expected_owner.is_none() {
            let failures = lock(&self.host.security_failures);
            (
                failures.get(&peer_key).map(|(revision, _)| *revision),
                failures.get(&None).map(|(revision, _)| *revision),
            )
        } else {
            (None, None)
        };
        match bounded(
            ctl,
            "security.state",
            self.host
                .radio
                .call(|id| RadioRequest::SecurityState { id, peer_id: peer }),
        )
        .await?
        {
            RadioCompletion::Security(state) => {
                if expected_owner.is_none() && self.host.security_owner(peer_id).is_some() {
                    return Err(DesktopError::new(
                        BleErrorCode::ConnectionStale,
                        BleErrorDomain::Connection,
                        "security.state",
                    ));
                }
                if expected_owner.is_none() {
                    let failures = lock(&self.host.security_failures);
                    for (key, opening_revision) in
                        [(&peer_key, opening_failures.0), (&None, opening_failures.1)]
                    {
                        if let Some((revision, error)) = failures.get(key)
                            && Some(*revision) != opening_revision
                        {
                            return Err(error.clone());
                        }
                    }
                }
                self.host.set_security_state_if_owner(
                    peer_id,
                    expected_owner.as_deref(),
                    state.clone(),
                    "security.state",
                )?;
                if expected_owner.is_none() {
                    let mut failures = lock(&self.host.security_failures);
                    for (key, opening_revision) in
                        [(&peer_key, opening_failures.0), (&None, opening_failures.1)]
                    {
                        if failures
                            .get(key)
                            .is_some_and(|(revision, _)| Some(*revision) == opening_revision)
                        {
                            failures.remove(key);
                        }
                    }
                }
                Ok(state)
            }
            _ => Err(protocol("security.state")),
        }
    }

    async fn control(
        &self,
        peer_id: &str,
        lease: &str,
        control: Control,
        ctl: OpControl,
    ) -> Result<Value, DesktopError> {
        let host = &*self.host;
        let operation = control.operation();
        let core_lease = self.state.core_name(lease);
        if let Control::MaximumWriteLength(with_response) = control {
            // The platform's own per-mode answer (`ReadWriteLimits`), bounded
            // by the ATT maximum attribute value, through the same core path
            // every write is admitted by.
            let maximum = host
                .central
                .connection_maximum_write_length(peer_id, &core_lease, with_response, ctl)
                .await?;
            return Ok(object(vec![("maximumWriteLength", Value::from(maximum))]));
        }
        if let Control::Rssi = control {
            let rssi = host.central.read_rssi(peer_id, &core_lease, ctl).await?;
            return Ok(object(vec![("rssi", Value::from(rssi))]));
        }
        if let Control::WriteReadiness = control {
            let ready = host
                .central
                .write_readiness(peer_id, &core_lease, ctl)
                .await?;
            return Ok(object(vec![("ready", Value::from(ready))]));
        }
        self.require_lease(peer_id, lease, operation)?;
        self.require_connected(peer_id, operation).await?;
        if matches!(control, Control::EffectiveMtu) && host.platform == MobilePlatform::Apple {
            // CoreBluetooth write length can include a long write. It is not
            // an observed ATT MTU, so the route stays and reports unobserved.
            return Ok(object(vec![("mtu", Value::Null)]));
        }
        let peer = peer_id.to_owned();
        let completion = bounded(
            &ctl,
            operation,
            host.radio.call(move |id| match control {
                // Rssi and MaximumWriteLength returned above through the core.
                Control::EffectiveMtu
                | Control::Rssi
                | Control::MaximumWriteLength(_)
                | Control::WriteReadiness => RadioRequest::ReadMtu { id, peer_id: peer },
                Control::RequestMtu(mtu) => RadioRequest::RequestMtu {
                    id,
                    peer_id: peer,
                    mtu,
                },
                Control::Priority(priority) => RadioRequest::RequestConnectionPriority {
                    id,
                    peer_id: peer,
                    priority,
                },
                Control::Subrate(mode) => RadioRequest::RequestSubrate {
                    id,
                    peer_id: peer,
                    mode,
                },
                Control::ReadPhy => RadioRequest::ReadPhy { id, peer_id: peer },
                Control::RequestPhy(tx, rx) => RadioRequest::RequestPhy {
                    id,
                    peer_id: peer,
                    tx,
                    rx,
                },
            }),
        )
        .await?;
        match (control, completion) {
            (Control::EffectiveMtu, RadioCompletion::Mtu(mtu)) => {
                Ok(object(vec![("mtu", mtu.map_or(Value::Null, Value::from))]))
            }
            (Control::RequestMtu(_), RadioCompletion::Mtu(Some(mtu))) => {
                Ok(object(vec![("mtu", Value::from(mtu))]))
            }
            (Control::Priority(_) | Control::Subrate(_), RadioCompletion::Accepted(accepted)) => {
                Ok(object(vec![("accepted", Value::Bool(accepted))]))
            }
            (Control::ReadPhy, RadioCompletion::Phy(observation)) => Ok(phy_value(observation)),
            (
                Control::RequestPhy(..),
                RadioCompletion::PhyRequest {
                    accepted,
                    observation,
                },
            ) => {
                if accepted != observation.is_some() {
                    return Err(protocol(operation));
                }
                Ok(object(vec![
                    ("accepted", Value::Bool(accepted)),
                    ("observation", observation.map_or(Value::Null, phy_value)),
                ]))
            }
            _ => Err(protocol(operation)),
        }
    }

    async fn subscribe(
        &self,
        peer_id: String,
        selector: PathSelector,
        consumer: String,
        mode: Option<&'static str>,
        terminal_evidence_scope: Option<ScanTerminalEvidenceScope>,
        recording: Option<RecordingOptions>,
        ctl: OpControl,
    ) -> Result<Value, DesktopError> {
        let host = &*self.host;
        {
            let mut pending = lock(&self.state.pending_subscriptions);
            if lock(&self.state.subscriptions).contains_key(&consumer)
                || !pending.insert(consumer.clone())
            {
                return Err(
                    wire::invalid("args.consumer").with_detail("consumer already subscribed")
                );
            }
        }
        let _reservation = SubscriptionReservation {
            pending: &self.state.pending_subscriptions,
            consumer: &consumer,
        };
        // The source owner verifies the caller's exact subscription reservation
        // before it enables CCCD; an unreserved or scan-scoped token must never
        // activate the radio.
        host.verify_subscription_terminal_evidence_scope(terminal_evidence_scope.as_ref())
            .map_err(terminal_evidence_error)?;
        let scope = scope_of(&peer_id, &selector);
        let required = match mode {
            Some("require-notification") => Some(DeliveryMode::Notification),
            Some("require-indication") => Some(DeliveryMode::Indication),
            _ => None,
        };
        let preferred = match mode {
            Some("prefer-notification") => Some(DeliveryMode::Notification),
            Some("prefer-indication") => Some(DeliveryMode::Indication),
            _ => None,
        };
        if let Some(required) = required {
            // Legacy rule on every platform: a requirement the
            // characteristic cannot meet is refused before any effect.
            let properties = host
                .central
                .discovered_paths(&peer_id)
                .await
                .ok()
                .and_then(|paths| {
                    paths
                        .into_iter()
                        .find(|path| {
                            path.service_uuid == scope.1
                                && path.service_occurrence == scope.2
                                && path.characteristic_uuid.as_deref() == Some(scope.3.as_str())
                                && path.characteristic_occurrence == Some(scope.4)
                                && path.descriptor_uuid.is_none()
                        })
                        .map(|path| path.properties)
                });
            let bit = match required {
                DeliveryMode::Notification => ubm_core::central::GATT_PROP_NOTIFY,
                DeliveryMode::Indication => ubm_core::central::GATT_PROP_INDICATE,
            };
            if properties.is_some_and(|properties| properties & bit == 0) {
                return Err(error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    "gatt.subscribe.delivery",
                )
                .with_detail(format!("characteristic lacks {}", required.as_str())));
            }
            if let Some(properties) = properties
                && let Some(refusal) =
                    unenforceable_requirement(host.platform, required, properties)
            {
                return Err(refusal);
            }
        }
        // Android writes the chosen CCCD mode and reports it. CoreBluetooth
        // selects the mode itself and never reports it: on Apple the
        // requirement is decided above (property check + decision C) and is
        // not sent to the radio, which could only refuse it; the delivery is
        // reported `unknown`.
        let radio_requirement = match host.platform {
            MobilePlatform::Android => required,
            MobilePlatform::Apple => None,
        };
        let recorded_route = if let Some(options) = recording {
            let peer = self.peer_record(&peer_id).await.ok_or_else(|| {
                error(
                    BleErrorCode::ConnectionNotFound,
                    BleErrorDomain::Connection,
                    "gatt.subscribe-recorded",
                )
            })?;
            let connection_generation = peer.connection_generation.ok_or_else(|| {
                error(
                    BleErrorCode::ConnectionStale,
                    BleErrorDomain::Connection,
                    "gatt.subscribe-recorded",
                )
            })?;
            let database_generation = peer.database_generation.ok_or_else(|| {
                error(
                    BleErrorCode::GattDiscoveryRequired,
                    BleErrorDomain::Gatt,
                    "gatt.subscribe-recorded",
                )
            })?;
            if peer.connection_state != Some(ConnectionState::Connected)
                || peer.database_state != Some(ubm_desktop::DatabaseState::Current)
            {
                return Err(error(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    "gatt.subscribe-recorded",
                ));
            }
            let clock = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| {
                    error(
                        BleErrorCode::PlatformFailure,
                        BleErrorDomain::Platform,
                        "gatt.subscribe-recorded.clock",
                    )
                })?
                .as_nanos()
                .to_string();
            let backend = host
                .central
                .attachment()
                .backend_instance_id()
                .as_str()
                .to_owned();
            let session = serde_json::json!({"peerId":peer_id,"sessionId":self.id().to_string(),
                "backendInstanceId":backend,"sessionStartedAtUnixNs":clock,
                "sessionEpoch":format!("{}:{}:{}", backend, self.id(), clock)});
            // The wire uses zero-based native occurrences; prepared recording
            // metadata uses the existing one-based continuation selector contract.
            let service_occurrence = selector
                .service_occurrence
                .and_then(|value| value.checked_add(1))
                .filter(|value| *value <= wire::MAX_SAFE_INTEGER)
                .ok_or_else(|| wire::invalid("args.selector.serviceOccurrence"))?;
            let characteristic_occurrence = selector
                .characteristic_occurrence
                .and_then(|value| value.checked_add(1))
                .filter(|value| *value <= wire::MAX_SAFE_INTEGER)
                .ok_or_else(|| wire::invalid("args.selector.characteristicOccurrence"))?;
            let metadata = serde_json::json!({"session":session,"consumer":{
            "consumer":consumer,"peerId":peer_id,"connectionGeneration":connection_generation,
            "databaseGeneration":database_generation,"selector":{
                "serviceUuid":selector.service_uuid,"serviceOccurrence":service_occurrence,
                "characteristicUuid":selector.characteristic_uuid,"characteristicOccurrence":characteristic_occurrence
            }}});
            let declaration = serde_json::json!({"kind":"ordinary-subscription","peerId":peer_id});
            let recording_id = options.id.clone();
            let registry = Arc::clone(&host.recording_registry);
            let registration = serde_json::json!({"t":"consumer-registration","consumer":consumer});
            let metadata_for_open = metadata.clone();
            let journal = ubm_desktop::continuation_journal::run_blocking_result(move || {
                let journal = registry
                    .open(
                        &options.id,
                        &declaration,
                        ubm_desktop::continuation_journal::JournalQuota {
                            max_bytes: options.max_bytes,
                            max_records: options.max_records,
                        },
                    )
                    .map_err(ubm_desktop::continuation::recording_failure)?;
                journal
                    .append(&metadata_for_open, &registration)
                    .map_err(ubm_desktop::continuation::recording_failure)?;
                Ok(journal)
            })
            .await
            .map_err(|failure| {
                recording_storage_error(&failure, "gatt.subscribe-recorded.storage")
            })?;
            Some(crate::host::RecordedRoute {
                recording_id,
                journal,
                metadata,
                order: Arc::new(tokio::sync::Mutex::new(())),
                radio_released: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })
        } else {
            None
        };
        let _pending_recording = recorded_route.as_ref().map(|recorded| {
            host.register_pending_recording(self.state.id, &consumer, recorded.clone())
        });
        if host.platform == MobilePlatform::Android
            && let Some(preferred) = preferred
        {
            host.radio
                .stage_preference(instance_of(&peer_id, &selector), preferred);
        }
        let core_consumer = self.state.core_name(&consumer);
        let mut reset_events = host.central.adapter_reset_events();
        let subscribed = host
            .central
            .subscribe_with_admission(&peer_id, &selector, &core_consumer, radio_requirement, ctl)
            .await;
        host.radio
            .clear_staging(None, Some(&instance_of(&peer_id, &selector)), false);
        let (observed, admission) = match subscribed {
            Ok(observed) => observed,
            Err(failure) => {
                if let Some(recorded) = recorded_route {
                    host.settle_recorded_ingress_drops().await?;
                    let terminal = serde_json::json!({"t":"stream-end","consumer":consumer,
                        "reason":"closed","droppedItems":0,"droppedBytes":0});
                    let metadata = recorded.metadata;
                    let journal = recorded.journal;
                    let result =
                        ubm_desktop::continuation_journal::run_blocking_result(move || {
                            journal
                                .append(&metadata, &terminal)
                                .map(|_| ())
                                .map_err(|error| {
                                    journal.mark_collection_failure(&error);
                                    ubm_desktop::continuation::recording_failure(error)
                                })
                        })
                        .await;
                    self.state
                        .outbox
                        .recording_changed(&consumer, &recorded.recording_id);
                    result.map_err(|diagnostic| {
                        recording_storage_error(
                            &diagnostic,
                            "gatt.subscribe-recorded.terminal-storage",
                        )
                    })?;
                }
                return Err(failure);
            }
        };
        let attachment = admission.attachment().clone();
        let delivery = match host.platform {
            MobilePlatform::Android => observed.as_str(),
            MobilePlatform::Apple => ObservedDelivery::Unknown.as_str(),
        };
        #[cfg(test)]
        self.state.pause_publication_for_test("subscribe").await;
        let subscription = Subscription {
            peer_id: peer_id.clone(),
            selector: selector.clone(),
            core_consumer: core_consumer.clone(),
            scope: scope.clone(),
            attachment: attachment.clone(),
            admission,
        };
        let published = host
            .central
            .publish_if_attached(&attachment, || {
                let mut subscriptions = lock(&self.state.subscriptions);
                subscriptions.insert(consumer.clone(), subscription.clone());
                host.add_route(
                    scope.clone(),
                    Route {
                        session_id: self.state.id,
                        consumer: consumer.clone(),
                        core_consumer,
                        peer_id,
                        selector,
                        delivery,
                        recorded: recorded_route.clone(),
                        terminal_evidence_scope: terminal_evidence_scope.clone(),
                        terminal: None,
                    },
                );
                // Flush values admitted while the enable was in flight.
                host.kick(scope);
            })
            .await;
        if published.is_none() {
            let failure =
                reset_release_failure(&mut reset_events, &attachment, &subscription.peer_id).await;
            {
                let mut ended = lock(&self.state.reset_subscriptions);
                ended.entry(subscription.admission.clone()).or_insert((
                    consumer.clone(),
                    subscription,
                    failure,
                ));
            }
            let terminal = serde_json::json!({"t":"stream-end","consumer":consumer,
                "reason":"invalidated","droppedItems":0,"droppedBytes":0});
            self.state.outbox.push_control(terminal.clone());
            if let Some(recorded) = recorded_route {
                host.settle_recorded_ingress_drops().await?;
                let journal = recorded.journal;
                let metadata = recorded.metadata;
                let result = ubm_desktop::continuation_journal::run_blocking_result(move || {
                    journal
                        .append(&metadata, &terminal)
                        .map(|_| ())
                        .map_err(|error| {
                            journal.mark_collection_failure(&error);
                            ubm_desktop::continuation::recording_failure(error)
                        })
                })
                .await;
                self.state
                    .outbox
                    .recording_changed(&consumer, &recorded.recording_id);
                result.map_err(|failure| {
                    recording_storage_error(&failure, "gatt.subscribe-recorded.terminal-storage")
                })?;
            }
        }
        Ok(object(vec![
            ("consumer", Value::from(consumer.clone())),
            ("delivery", Value::from(delivery)),
        ]))
    }

    /// `counters.describe`: the resources this session holds (its lease
    /// namespace) as `counters`/`native`, and the whole process owner, which
    /// every session shares, under the explicitly named `process`.
    async fn counters(&self) -> Result<Value, DesktopError> {
        let host = &*self.host;
        let counters = host.central.resource_counters().await;
        let (queued, dispatched) = crate::compat::operation_split(&counters).ok_or_else(|| {
            error(
                BleErrorCode::CapabilityUnavailable,
                BleErrorDomain::Capability,
                "counters.describe.operations",
            )
            .with_detail("the core does not split queued and dispatched operations yet")
        })?;
        let records = host.central.peer_records().await;
        let count = |value: usize| Value::from(value as u64);
        let sessions = host.session_list();
        let progress_of = |state: &SessionState| -> Vec<u64> {
            lock(&state.op_radio)
                .values()
                .map(|progress| progress.in_flight())
                .collect()
        };

        let own = &*self.state;
        let leased: Vec<String> = lock(&own.leases).keys().cloned().collect();
        let leased_records: Vec<&PeerRecord> = records
            .iter()
            .filter(|record| leased.contains(&record.peer_id))
            .collect();
        let scopes: HashSet<InstanceKey> = lock(&own.subscriptions)
            .values()
            .map(|subscription| subscription.scope.clone())
            .collect();
        let scanning = usize::from(lock(&own.scan).membership().is_some());
        let own_progress = progress_of(own);
        let claimed = lock(&host.restoration_claims)
            .values()
            .filter(|session_id| **session_id == own.id)
            .count();
        let session_counters = object(vec![
            ("activeScanControllers", count(scanning)),
            ("scanConsumers", count(scanning)),
            ("chooserSessions", count(0)),
            ("connectionLeases", count(leased.len())),
            (
                "physicalLinks",
                count(
                    leased_records
                        .iter()
                        .filter(|record| {
                            record.connection_state == Some(ConnectionState::Connected)
                        })
                        .count(),
                ),
            ),
            (
                "databaseSnapshots",
                count(
                    leased_records
                        .iter()
                        .filter(|record| {
                            record.database_state == Some(ubm_core::central::DatabaseState::Current)
                        })
                        .count(),
                ),
            ),
            ("physicalCccdEnablements", count(scopes.len())),
            (
                "subscriptionConsumers",
                count(lock(&own.subscriptions).len()),
            ),
            (
                "queuedOperations",
                count(
                    own_progress
                        .iter()
                        .filter(|in_flight| **in_flight == 0)
                        .count(),
                ),
            ),
            (
                "dispatchedOperations",
                count(
                    own_progress
                        .iter()
                        .filter(|in_flight| **in_flight > 0)
                        .count(),
                ),
            ),
            ("retainedByteBuffers", count(own.outbox.queued_data())),
            ("restorationRecords", count(claimed)),
            ("orphanedIpcOwners", count(0)),
        ]);
        let session_native = object(vec![
            (
                "pendingRadioRequests",
                Value::from(own_progress.iter().sum::<u64>()),
            ),
            ("liveOps", count(own_progress.len())),
        ]);

        let snapshots = records
            .iter()
            .filter(|record| {
                record.database_state == Some(ubm_core::central::DatabaseState::Current)
            })
            .count();
        let leases: usize = sessions.iter().map(|s| lock(&s.leases).len()).sum();
        let buffered: usize = sessions.iter().map(|s| s.outbox.queued_data()).sum();
        let live_ops: usize = sessions.iter().map(|s| lock(&s.op_radio).len()).sum();
        let radio = host.radio.counters();
        let process_counters = object(vec![
            (
                "activeScanControllers",
                count(usize::from(counters.scan_owned)),
            ),
            ("scanConsumers", count(lock(&host.scan_members).len())),
            ("chooserSessions", count(0)),
            ("connectionLeases", count(leases)),
            ("physicalLinks", count(counters.core.live_connections)),
            ("databaseSnapshots", count(snapshots)),
            (
                "physicalCccdEnablements",
                count(counters.routed_subscriptions),
            ),
            ("subscriptionConsumers", count(counters.core.live_consumers)),
            ("queuedOperations", Value::from(queued)),
            ("dispatchedOperations", Value::from(dispatched)),
            (
                "retainedByteBuffers",
                count(buffered + counters.queued_advertisements),
            ),
            ("restorationRecords", count(lock(&host.restored).len())),
            ("orphanedIpcOwners", count(0)),
        ]);
        let process_native = object(vec![
            (
                "nativeGattAdmissions",
                count(counters.native_gatt_admissions),
            ),
            (
                "acquiredGattTransports",
                count(counters.acquired_gatt_transports),
            ),
            (
                "pendingGattAcquisitions",
                count(counters.pending_gatt_acquisitions),
            ),
            ("pendingRadioRequests", Value::from(radio.pending_requests)),
            (
                "lateRadioCompletions",
                Value::from(radio.late_completions + radio.mismatched_completions),
            ),
            (
                "ingressDrops",
                object(vec![
                    ("advertisement", Value::from(radio.advertisement_drops)),
                    ("notification", Value::from(radio.notification_drops)),
                    ("control", Value::from(radio.control_drops)),
                ]),
            ),
            ("connectSections", Value::from(radio.connect_sections)),
            ("liveOps", count(live_ops)),
        ]);
        Ok(object(vec![
            ("counters", session_counters),
            ("native", session_native),
            (
                "process",
                object(vec![
                    ("counters", process_counters),
                    ("native", process_native),
                ]),
            ),
        ]))
    }

    /// `session.reconcile` (104/105): every fact a control record carries,
    /// re-read from the owner, so a record lost at the full control queue
    /// becomes the transition it would have caused. The adapter is re-read
    /// from the platform; links, databases, streams, security, restoration
    /// and scan membership are the owner's own state.
    async fn reconcile(&self, ctl: &OpControl) -> Result<Value, DesktopError> {
        let host = &*self.host;
        host.retry_scan_terminal_evidence_debts();
        host.retry_subscription_terminal_evidence_debts();
        let central = &host.central;
        let snapshot = bounded(ctl, "session.reconcile", host.radio.adapter_snapshot()).await?;
        let updated_at = host.now_ms();
        *lock(&host.adapter) = Some((snapshot.clone(), updated_at));
        let adapter = adapter_value(&snapshot, updated_at, &central.attachment());
        let mut links: Vec<Value> = central
            .peer_records()
            .await
            .iter()
            .filter(|record| record.connection_state == Some(ConnectionState::Connected))
            .filter_map(|record| {
                let generation = record.connection_generation.clone()?;
                Some(object(vec![
                    ("peerId", Value::from(record.peer_id.as_str())),
                    ("connectionGeneration", Value::from(generation.as_str())),
                    ("state", Value::from("connected")),
                    ("reason", Value::Null),
                    (
                        "databaseGeneration",
                        opt_text(crate::compat::peer_database_generation(record).as_deref()),
                    ),
                    (
                        "databaseChange",
                        lock(&host.database_changes)
                            .get(&record.peer_id)
                            .filter(|(connection, _)| *connection == generation)
                            .map_or(Value::Null, |(_, database)| Value::from(database.as_str())),
                    ),
                    (
                        "databaseState",
                        record
                            .database_state
                            .map_or(Value::Null, |state| Value::from(state.as_str())),
                    ),
                ]))
            })
            .collect();
        links.extend(lock(&host.link_ends).iter().map(|(peer_id, end)| {
            object(vec![
                ("peerId", Value::from(peer_id.as_str())),
                (
                    "connectionGeneration",
                    Value::from(end.connection_generation.as_str()),
                ),
                ("state", Value::from("ended")),
                ("reason", Value::from(end.reason)),
                (
                    "databaseGeneration",
                    opt_text(end.database_generation.as_deref()),
                ),
                ("databaseChange", Value::Null),
                ("databaseState", Value::Null),
            ])
        }));
        let held: Vec<(String, InstanceKey)> = {
            let subscriptions = lock(&self.state.subscriptions);
            let mut held: Vec<(String, InstanceKey)> = subscriptions
                .iter()
                .map(|(consumer, subscription)| (consumer.clone(), subscription.scope.clone()))
                .collect();
            held.sort_by(|a, b| a.0.cmp(&b.0));
            held
        };
        let subscriptions: Vec<Value> = held
            .into_iter()
            .map(|(consumer, scope)| {
                match host.route_terminal(&scope, self.state.id, &consumer) {
                    Some(None) => object(vec![
                        ("consumer", Value::from(consumer)),
                        ("state", Value::from("live")),
                    ]),
                    // A consumer without an installed route has no stream
                    // left to deliver: it reports closed, never live.
                    ended => {
                        let (reason, items, bytes, error) =
                            ended.flatten().unwrap_or(("closed", 0, 0, None));
                        let mut result = object(vec![
                            ("consumer", Value::from(consumer)),
                            ("state", Value::from("ended")),
                            ("reason", Value::from(reason)),
                            ("droppedItems", Value::from(items)),
                            ("droppedBytes", Value::from(bytes)),
                        ]);
                        if let Some(error) = error {
                            result["error"] = Value::Object(wire::error_object(&error));
                        }
                        result
                    }
                }
            })
            .collect();
        let security: Vec<Value> = {
            let known = lock(&host.security);
            let mut peers: Vec<&String> = known.keys().collect();
            peers.sort();
            peers
                .into_iter()
                .map(|peer_id| {
                    object(vec![
                        ("peerId", Value::from(peer_id.as_str())),
                        ("state", security_value(&known[peer_id].state)),
                    ])
                })
                .collect()
        };
        let security_failures: Vec<Value> = lock(&host.security_failures)
            .iter()
            .map(|(peer, (_, error))| {
                object(vec![
                    ("peerId", opt_text(peer.as_deref())),
                    ("error", Value::Object(wire::error_object(error))),
                ])
            })
            .collect();
        let restored: Vec<Value> = lock(&host.restored)
            .values()
            .map(|peer| {
                object(vec![
                    ("peerId", Value::from(peer.peer_id.as_str())),
                    ("name", opt_text(peer.name.as_deref())),
                    ("connected", Value::Bool(peer.connected)),
                ])
            })
            .collect();
        let scan = lock(&self.state.scan).membership().map(str::to_owned);
        Ok(object(vec![
            ("adapter", adapter),
            ("links", Value::Array(links)),
            ("subscriptions", Value::Array(subscriptions)),
            ("security", Value::Array(security)),
            ("securityFailures", Value::Array(security_failures)),
            ("restored", Value::Array(restored)),
            ("scan", opt_text(scan.as_deref())),
        ]))
    }

    fn recording_handle(&self, handle: &str) -> Result<u64, DesktopError> {
        let (owner, sequence) = handle
            .split_once(':')
            .ok_or_else(|| wire::invalid("handle"))?;
        let owner: usize = owner.parse().map_err(|_| wire::invalid("handle"))?;
        let sequence: u64 = sequence.parse().map_err(|_| wire::invalid("handle"))?;
        if owner != self.state.instance || format!("{owner}:{sequence}") != handle {
            return Err(error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Cleanup,
                "recording.handle",
            ));
        }
        Ok(sequence)
    }
    fn issued_recording_handle(&self, sequence: u64) -> bool {
        lock(&self.state.recording_start_last).is_some_and(|last| sequence <= last)
    }
    /// Reserve a bounded native handle before any source effect. Active retries
    /// of the same recording return the same handle after a lost response.
    pub fn reserve_ordered_recording_start(
        &self,
        reservation: &str,
    ) -> Result<Value, DesktopError> {
        let [id, caller_key]: [String; 2] =
            serde_json::from_str(reservation).map_err(|_| wire::invalid("reservation"))?;
        if id.is_empty() || id.len() > 256 || caller_key.is_empty() || caller_key.len() > 256 {
            return Err(wire::invalid("recordingId"));
        }
        let admission = lock(&self.state.admission);
        if admission.closing {
            return Err(error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "recording.reserve",
            ));
        }
        let mut receipts = lock(&self.host.ordered_start_receipts);
        if let Some(((.., sequence), _)) = receipts.iter().find(|((session, _), entry)| {
            *session == self.state.id && entry.recording_id == id && entry.caller_key == caller_key
        }) {
            return Ok(
                serde_json::json!({"version":2,"state":"reserved","handle":format!("{}:{sequence}", self.state.instance)}),
            );
        }
        if receipts.len() >= 128 {
            return Err(error(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "recording.reserve",
            ));
        }
        let mut last = lock(&self.state.recording_start_last);
        let sequence = match *last {
            None => 0,
            Some(value) => value.checked_add(1).ok_or_else(|| {
                error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "recording.reserve",
                )
                .with_detail("recording handles exhausted")
            })?,
        };
        receipts.insert(
            (self.state.id, sequence),
            RecordingStartReceipt {
                recording_id: id,
                caller_key,
                owner_collection: None,
                revoked: false,
                identity: None,
                reclaim_capability: None,
                result: None,
                failed: false,
            },
        );
        *last = Some(sequence);
        Ok(
            serde_json::json!({"version":2,"state":"reserved","handle":format!("{}:{sequence}", self.state.instance)}),
        )
    }
    fn revoke_ordered_recording_starts(&self, id: &str, collection: u64) {
        for entry in lock(&self.host.ordered_start_receipts).values_mut() {
            if entry.recording_id == id && entry.owner_collection == Some(collection) {
                entry.revoked = true;
            }
        }
    }

    pub fn ordered_recording_start_status(&self, handle: &str) -> Result<Value, DesktopError> {
        let sequence = self.recording_handle(handle)?;
        let receipts = lock(&self.host.ordered_start_receipts);
        let Some(entry) = receipts.get(&(self.state.id, sequence)) else {
            return if self.issued_recording_handle(sequence) {
                Ok(serde_json::json!({"version":1,"state":"retired","receipt":null}))
            } else {
                Err(error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Cleanup,
                    "recording.start-status",
                ))
            };
        };
        if entry.revoked {
            return Ok(serde_json::json!({"version":1,"state":"revoked","receipt":null}));
        }
        let (state, receipt) = if entry.failed {
            ("failed", Value::Null)
        } else if let Some(result) = &entry.result {
            ("ready", result.clone())
        } else if entry.identity.is_some() {
            ("pending", Value::Null)
        } else {
            ("reserved", Value::Null)
        };
        Ok(serde_json::json!({"version":1,"state":state,"receipt":receipt}))
    }
    pub async fn retire_ordered_recording_start(
        &self,
        handle: &str,
    ) -> Result<Value, DesktopError> {
        let sequence = self.recording_handle(handle)?;
        {
            let receipts = lock(&self.host.ordered_start_receipts);
            let Some(entry) = receipts.get(&(self.state.id, sequence)) else {
                return if self.issued_recording_handle(sequence) {
                    Ok(serde_json::json!({"version":1,"state":"retired"}))
                } else {
                    Err(error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Cleanup,
                        "recording.retire",
                    ))
                };
            };
            if entry.identity.is_some() && entry.result.is_none() && !entry.failed {
                return Err(error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "recording.retire",
                )
                .with_detail("start receipt remains pending"));
            }
        }
        let _leader = self.state.release_guard.lock().await;
        let id = {
            let receipts = lock(&self.host.ordered_start_receipts);
            let Some(entry) = receipts.get(&(self.state.id, sequence)) else {
                return if self.issued_recording_handle(sequence) {
                    Ok(serde_json::json!({"version":1,"state":"retired"}))
                } else {
                    Err(error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Cleanup,
                        "recording.retire",
                    ))
                };
            };
            if entry.identity.is_some() && entry.result.is_none() && !entry.failed {
                return Err(error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "recording.retire",
                ));
            }
            (entry.recording_id.clone(), entry.owner_collection)
        };
        let owner =
            lock(&self.host.ordered_recordings)
                .iter()
                .find_map(|((_, recording), owner)| {
                    (recording == &id.0 && Some(owner.collection_id()) == id.1)
                        .then(|| Arc::clone(owner))
                });
        if let Some(owner) = owner {
            if owner.start_status()["state"] == "not-started" {
                match owner.shutdown().await {
                    Ok(ubm_desktop::ordered_recording::RecordingDriverStatus::Finished) => {
                        lock(&self.host.ordered_recordings).retain(|(_, recording), current| {
                            recording != &id.0 || !Arc::ptr_eq(current, &owner)
                        });
                        self.host
                            .ordered_owners
                            .retire_host_owned(owner.collection_id(), owner.peer_id());
                    }
                    result => {
                        return Err(error(
                            BleErrorCode::LifecycleInvalidState,
                            BleErrorDomain::Cleanup,
                            "recording.retire",
                        )
                        .with_detail(format!(
                            "unopened owner cleanup remains pending: {result:?}"
                        )));
                    }
                }
            } else {
                owner.retire_start_receipt().await.map_err(|cause| {
                    error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Cleanup,
                        "recording.retire",
                    )
                    .with_detail(format!("start receipt retirement failed: {cause:?}"))
                })?;
            }
        }
        lock(&self.host.ordered_start_receipts).remove(&(self.state.id, sequence));
        Ok(serde_json::json!({"version":1,"state":"retired"}))
    }

    /// Reserve and start a supervised ordered recording for this session's
    /// existing connection lease. The detached task retains its reservation if
    /// the caller is cancelled during open.
    pub async fn start_ordered_recording(
        &self,
        handle: String,
        recording_id: String,
        quota: ubm_desktop::continuation_journal::JournalQuota,
        declaration: ubm_desktop::ordered_recording::OrderedRecordingDeclaration,
        control: OpControl,
    ) -> Result<Value, ubm_desktop::ordered_recording::RecordingError> {
        use ubm_desktop::ordered_recording::{OrderedRecording, RecordingError};
        let session = self.clone();
        tokio::spawn(async move {
            let identity = declaration.context()?;
            let sequence = session.recording_handle(&handle).map_err(|_| RecordingError::InvalidIdentity)?;
            let key = (session.state.id, sequence);
            {
                let mut receipts = lock(&session.host.ordered_start_receipts);
                let Some(entry) = receipts.get_mut(&key) else { return Err(if session.issued_recording_handle(sequence) { RecordingError::ReceiptRetired } else { RecordingError::InvalidIdentity }); };
                if entry.recording_id != recording_id { return Err(RecordingError::InvalidIdentity); }
                if entry.revoked { return Err(RecordingError::ReceiptRetired); }
                if entry.failed { return Err(RecordingError::WorkerFailed); }
                if let Some(existing) = &entry.identity {
                    if existing != &(identity.clone(), quota) {
                        return Err(RecordingError::InvalidIdentity);
                    }
                    if let Some(result) = &entry.result {
                        return Ok(result.clone());
                    }
                } else {
                    entry.identity = Some((identity, quota));
                }
            }
            let _leader = session.state.release_guard.lock().await;
            let mut start_guard = RecordingStartGuard { host: Arc::clone(&session.host), key: key.clone(), completed: false };
            let result = async {
                if let Some(result) = lock(&session.host.ordered_start_receipts).get(&key).and_then(|entry| entry.result.clone()) { return Ok(result); }
                if lock(&session.state.admission).closing {
                    return Err(RecordingError::InvalidIdentity);
                }
                let lease = lock(&session.state.leases)
                    .get(&declaration.peer_id)
                    .map(|lease| lease.name.clone())
                    .ok_or(RecordingError::InvalidIdentity)?;
                let _start = session.host.ordered_recording_start_guard.lock().await;
                let existing = {
                    let entries = lock(&session.host.ordered_recordings);
                    let existing = entries
                        .get(&(session.state.id, recording_id.clone()))
                        .cloned();
                    if existing.is_none() && entries.len() >= 128 {
                        return Err(RecordingError::InvalidIdentity);
                    }
                    existing
                };
                let peer = declaration.peer_id.clone();
                let mut reclaim_capability = lock(&session.host.ordered_start_receipts).get(&key).and_then(|entry| entry.reclaim_capability.clone());
                let owner = if let Some(owner) = existing {
                    if lock(&session.host.ordered_start_receipts).get(&key).and_then(|entry| entry.owner_collection) != Some(owner.collection_id()) {
                        return Err(RecordingError::InvalidIdentity);
                    }
                    if !owner.matches_start(&declaration, quota)? {
                        return Err(RecordingError::InvalidIdentity);
                    }
                    owner
                } else {
                    if session
                        .host
                        .recording_registry
                        .ordered_owner(&recording_id, &session.host.central)
                        .map_err(RecordingError::Storage)?
                        .is_some()
                    {
                        return Err(RecordingError::InvalidIdentity);
                    }
                    let owner = OrderedRecording::prepare(
                        Arc::new(session.host.central.clone()),
                        Arc::clone(&session.host.recording_registry),
                        recording_id.clone(),
                        quota,
                        declaration,
                    )
                    .await?;
                    lock(&session.host.ordered_recordings)
                        .insert((session.state.id, recording_id.clone()), Arc::clone(&owner));
                    if let Err(cause) = session.host.ordered_owners.reserve(&peer, session.state.id, || Ok(owner.collection_id())) {
                        match owner.shutdown().await {
                            Ok(ubm_desktop::ordered_recording::RecordingDriverStatus::Finished) => {
                                lock(&session.host.ordered_recordings).remove(&(session.state.id, recording_id.clone()));
                            }
                            cleanup => return Err(RecordingError::Source(error(BleErrorCode::LifecycleInvalidState, BleErrorDomain::Cleanup, "recording.prepare.cleanup")
                                .with_detail(format!("owner reservation failed: {cause:?}; unopened owner cleanup remains pending: {cleanup:?}")))),
                        }
                        return Err(RecordingError::Source(cause));
                    }
                    if let Some(entry) = lock(&session.host.ordered_start_receipts).get_mut(&key) { entry.owner_collection = Some(owner.collection_id()); }
                    let capability = owner.issue_recovery_capability()?;
                    if let Some(entry) = lock(&session.host.ordered_start_receipts).get_mut(&key) { entry.reclaim_capability = Some(capability.clone()); }
                    reclaim_capability = Some(capability);
                    owner
                };
                drop(_start);
                drop(_leader);
                match owner.start_receipt(control.with_connection_lease(lease)).await {
                    Ok(receipt) => Ok(serde_json::json!({"kind":"started", "receipt":receipt, "reclaimCapability":reclaim_capability})),
                    Err(cause) => Ok(ubm_desktop::ordered_recording::owner_failure_receipt(&recording_id, owner.collection_id(), &cause)),
                }
            }.await;
            let receipt = match result {
                Ok(receipt) => receipt,
                Err(cause) => serde_json::json!({"kind":"prepare-failed", "receipt":ubm_desktop::ordered_recording::prepare_failure_receipt(&recording_id, &cause)}),
            };
            let revoked = if let Some(entry) = lock(&session.host.ordered_start_receipts).get_mut(&key) {
                entry.result = Some(receipt.clone());
                entry.revoked
            } else { true };
            start_guard.completed = true;
            if revoked { Err(RecordingError::ReceiptRetired) } else { Ok(receipt) }
        })
        .await
        .map_err(|_| RecordingError::WorkerFailed)?
    }

    pub fn ordered_recording_receipt_control(
        &self,
        operation: String,
        id: String,
        completion: Completion,
    ) {
        let session = self.clone();
        self.host.runtime.spawn(async move {
            let result = match operation.as_str() {
                "reserve" => session.reserve_ordered_recording_start(&id),
                "status" => session.ordered_recording_start_status(&id),
                "retire" => session.retire_ordered_recording_start(&id).await,
                _ => Err(wire::invalid("operation")),
            }
            .map_err(|error| Value::Object(wire::error_object(&error)));
            completion(ubm_desktop::continuation::envelope(result));
        });
    }

    /// Native callback seam for the canonical shared declaration. Parsing and
    /// quota validation happen before reservation; the owned start survives the callback waiter.
    pub fn start_ordered_recording_wire(
        &self,
        handle: String,
        recording_id: String,
        max_bytes: String,
        max_records: String,
        declaration_json: String,
        budget_ms: u64,
        completion: Completion,
    ) {
        let session = self.clone();
        self.host.runtime.spawn(async move {
            let result = async {
                fn decimal(value: &str) -> Option<u64> {
                    let parsed: u64 = value.parse().ok()?;
                    (parsed.to_string() == value).then_some(parsed)
                }
                if declaration_json.len() > 65_536 || budget_ms > MAX_TIMEOUT_MS { return Err(ubm_desktop::ordered_recording::RecordingError::InvalidIdentity); }
                let quota = ubm_desktop::continuation_journal::JournalQuota {
                    max_bytes: decimal(&max_bytes).ok_or(ubm_desktop::ordered_recording::RecordingError::InvalidIdentity)?,
                    max_records: decimal(&max_records).ok_or(ubm_desktop::ordered_recording::RecordingError::InvalidIdentity)?,
                };
                let context: Value = serde_json::from_str(&declaration_json).map_err(|_| ubm_desktop::ordered_recording::RecordingError::InvalidIdentity)?;
                let declaration = ubm_desktop::ordered_recording::OrderedRecordingDeclaration::from_context(&context)?;
                session.start_ordered_recording(handle, recording_id, quota, declaration, OpControl::budget_ms(budget_ms)).await
            }.await;
            let answer = result.map_err(|_| serde_json::json!({"code":"lifecycle.invalid-state", "domain":"restoration", "operation":"continuation.recording.start-ordered", "detail":"ordered recording start is not authorized or valid"}));
            completion(ubm_desktop::continuation::envelope(answer));
        });
    }

    /// Authorize recording controls against this exact session-owned supervisor.
    pub fn ordered_recording_control_wire(
        &self,
        id: String,
        context: bool,
        completion: Completion,
    ) {
        let session = self.clone();
        self.host.runtime.spawn(async move {
            let _leader = session.state.release_guard.lock().await;
            let owner = lock(&session.host.ordered_recordings).get(&(session.state.id, id.clone())).cloned();
            let owner_collection = owner.as_ref().map(|owner| owner.collection_id());
            let result = match owner {
                None => Err(serde_json::json!({"code":"ownership.denied","domain":"restoration","operation":"continuation.recording.ordered","detail":"recording is not owned by this session"})),
                Some(_) if context => {
                    let registry = Arc::clone(&session.host.recording_registry);
                    let context_id = id.clone();
                    ubm_desktop::continuation_journal::run_blocking_result(move || registry.ordered_context(&context_id).map_err(ubm_desktop::continuation::recording_failure)).await
                }
                Some(owner) => {
                    if matches!(owner.driver_status(), ubm_desktop::ordered_recording::RecordingDriverStatus::Failed(_)) { owner.retry_driver(); }
                    session.host.recording_registry.stop_ordered_host_receipt(&id).await
                },
            };
            if !context && result.as_ref().ok().and_then(|value| value.get("recording")).and_then(|value| value.get("state")).and_then(Value::as_str).is_some_and(|state| state == "finished" || state == "interrupted") {
                if let Some(collection) = owner_collection {
                    session.revoke_ordered_recording_starts(&id, collection);
                }
            }
            completion(ubm_desktop::continuation::envelope(result));
        });
    }

    /// Read the retained supervisor status without starting a second poller.
    pub fn ordered_recording_status(
        &self,
        id: &str,
    ) -> Option<ubm_desktop::ordered_recording::RecordingDriverStatus> {
        lock(&self.host.ordered_recordings)
            .get(&(self.state.id, id.to_owned()))
            .map(|owner| owner.driver_status())
    }

    /// Wake a failed or blocked supervisor; ownership is retained until Finished.
    pub fn retry_ordered_recording(&self, id: &str) -> bool {
        let owner = lock(&self.host.ordered_recordings)
            .get(&(self.state.id, id.to_owned()))
            .cloned();
        if let Some(owner) = owner {
            owner.retry_driver();
            true
        } else {
            false
        }
    }

    /// Request the supervised fence, drain, durable commit and cleanup.
    pub async fn close_ordered_recording(
        &self,
        id: &str,
    ) -> Result<
        ubm_desktop::ordered_recording::RecordingDriverStatus,
        ubm_desktop::ordered_recording::RecordingError,
    > {
        let _leader = self.state.release_guard.lock().await;
        let owner = lock(&self.host.ordered_recordings)
            .get(&(self.state.id, id.to_owned()))
            .cloned()
            .ok_or(ubm_desktop::ordered_recording::RecordingError::InvalidIdentity)?;
        let result = owner.shutdown().await;
        if matches!(
            result,
            Ok(ubm_desktop::ordered_recording::RecordingDriverStatus::Finished)
        ) {
            self.revoke_ordered_recording_starts(id, owner.collection_id());
        }
        result
    }

    /// Release everything this session holds on the shared owner. Failed
    /// releases stay held (a second dispose retries them).
    pub(crate) async fn dispose_failures(&self) -> Vec<Value> {
        self.release(self.state.release_version.load(Ordering::SeqCst))
            .await
    }

    async fn release(&self, requested_version: u64) -> Vec<Value> {
        let _leader = self.state.release_guard.lock().await;
        if self.state.release_version.load(Ordering::SeqCst) > requested_version {
            return lock(&self.state.last_release).clone().unwrap_or_default();
        }
        let host = &*self.host;
        lock(&self.state.admission).closing = true;
        // Cancel every other live op and wait for them to settle (the
        // dispose op itself is not in the live table: it has no id).
        let tickets: Vec<OpTicket> = lock(&self.state.ops)
            .live
            .values()
            .map(|(ticket, _)| ticket.clone())
            .collect();
        for ticket in &tickets {
            let _ = host.central.cancel(ticket).await;
        }
        loop {
            let idle = self.state.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if lock(&self.state.admission).ordinary_ops == 0 {
                break;
            }
            idle.await;
        }
        let mut failures = Vec::new();
        if let Err(error) = host.radio.release_companion_choices(self.state.id).await {
            failures.push(cleanup_failure("companion-chooser", &error));
        }
        let membership = lock(&self.state.scan).membership().map(str::to_owned);
        if let Some(membership) = membership {
            match host
                .leave_scan(self.state.id, &membership, OpControl::unbounded())
                .await
            {
                Ok(()) => self.state.clear_scan(&membership),
                Err(error) => failures.push(cleanup_failure("scan", &error)),
            }
        }
        // Reclaim owns the replacement lease until native permit retirement.
        // Cancel unpublished reservations and release published ones first.
        let reclaims: Vec<_> = lock(&self.state.reclaim_reservations)
            .iter()
            .map(|(group, entry)| (*group, entry.clone()))
            .collect();
        let mut reclaim_pending = false;
        for (group, entry) in reclaims {
            use ubm_desktop::reclaim_reservation::ReclaimReservationStatus as S;
            let status =
                entry
                    .owner
                    .status(entry.id, &entry.capability, &entry.lease, &entry.caller_key);
            let result = match status {
                Ok(S::Reserved | S::Pending | S::Ready) => {
                    entry
                        .owner
                        .cancel(entry.id, &entry.capability, &entry.lease, &entry.caller_key)
                }
                Ok(S::Committed) => entry.owner.release(
                    entry.id,
                    &entry.capability,
                    &entry.lease,
                    &entry.caller_key,
                ),
                Ok(S::Released { .. } | S::Failed(_) | S::Retired { .. }) => {
                    match entry
                        .owner
                        .retire(entry.id, &entry.capability, &entry.lease, &entry.caller_key)
                        .and_then(|()| {
                            entry.owner.acknowledge_retirement(
                                entry.id,
                                &entry.capability,
                                &entry.lease,
                                &entry.caller_key,
                            )
                        })
                        .and_then(|()| {
                            entry.owner.forget_retirement(
                                entry.id,
                                &entry.capability,
                                &entry.lease,
                                &entry.caller_key,
                            )
                        }) {
                        Ok(()) => {
                            lock(&self.state.reclaim_reservations).remove(&group);
                            continue;
                        }
                        Err(error) => Err(error),
                    }
                }
                Ok(S::Releasing { .. } | S::ReleaseFailed { .. }) => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                failures.push(cleanup_failure("ordered-reclaim", &error));
                reclaim_pending = true;
                continue;
            }
            match entry
                .owner
                .status(entry.id, &entry.capability, &entry.lease, &entry.caller_key)
            {
                Err(error) => {
                    failures.push(cleanup_failure("ordered-reclaim", &error));
                    reclaim_pending = true;
                }
                Ok(S::Released { .. } | S::Failed(_) | S::Retired { .. }) => {
                    if let Err(error) = entry
                        .owner
                        .retire(entry.id, &entry.capability, &entry.lease, &entry.caller_key)
                        .and_then(|()| {
                            entry.owner.acknowledge_retirement(
                                entry.id,
                                &entry.capability,
                                &entry.lease,
                                &entry.caller_key,
                            )
                        })
                        .and_then(|()| {
                            entry.owner.forget_retirement(
                                entry.id,
                                &entry.capability,
                                &entry.lease,
                                &entry.caller_key,
                            )
                        })
                    {
                        failures.push(cleanup_failure("ordered-reclaim", &error));
                        reclaim_pending = true;
                    } else {
                        lock(&self.state.reclaim_reservations).remove(&group);
                    }
                }
                Ok(_) => {
                    reclaim_pending = true;
                }
            }
        }
        if reclaim_pending {
            let pending = error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "session.dispose.reclaim",
            )
            .with_detail("reclaim cleanup remains pending");
            failures.push(cleanup_failure("ordered-reclaim", &pending));
            *lock(&self.state.last_release) = Some(failures.clone());
            self.state.release_version.fetch_add(1, Ordering::SeqCst);
            return failures;
        }
        // The recording driver owns the only poller. Keep its session and
        // connection lease until the exact supervised owner finishes cleanup.
        let recordings: Vec<(
            String,
            Arc<ubm_desktop::ordered_recording::OrderedRecording<crate::foreign::ForeignRadio>>,
        )> = lock(&host.ordered_recordings)
            .iter()
            .filter_map(|((session, id), owner)| {
                (*session == self.state.id).then(|| (id.clone(), Arc::clone(owner)))
            })
            .collect();
        let mut recording_pending = false;
        for (id, owner) in recordings {
            let collection = owner.collection_id();
            let peer = owner.peer_id();
            let lease_status = owner.connection_lease_status();
            if matches!(
                lease_status,
                ubm_desktop::ordered_recording::RecordingConnectionLeaseStatus::Pending
            ) && owner.start_status()["state"] != "not-started"
            {
                recording_pending = true;
                let pending = error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "session.dispose.recording",
                )
                .with_detail("recorder lease handoff remains pending");
                failures.push(cleanup_failure("ordered-recording", &pending));
                continue;
            }
            if matches!(
                lease_status,
                ubm_desktop::ordered_recording::RecordingConnectionLeaseStatus::Retained { .. }
            ) && host
                .ordered_owners
                .detach_group(collection, peer, self.state.id)
            {
                let mut owners = lock(&host.ordered_recordings);
                if let Some(retained) = owners.remove(&(self.state.id, id.clone())) {
                    owners.insert((0, id), retained);
                }
                continue;
            }
            if matches!(
                owner.driver_status(),
                ubm_desktop::ordered_recording::RecordingDriverStatus::Failed(_)
            ) {
                owner.retry_driver();
            }
            let result = owner.shutdown().await;
            if matches!(
                result,
                Ok(ubm_desktop::ordered_recording::RecordingDriverStatus::Finished)
            ) {
                lock(&host.ordered_recordings).remove(&(self.state.id, id));
                host.ordered_owners.retire_host_owned(collection, peer);
            } else {
                recording_pending = true;
                let receipt = ubm_desktop::ordered_recording::stop_receipt(result);
                let pending = error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "session.dispose.recording",
                )
                .with_detail("ordered recording cleanup remains pending");
                let mut failure = cleanup_failure("ordered-recording", &pending);
                failure["recordingReceipt"] = receipt;
                failures.push(failure);
            }
        }
        if recording_pending {
            *lock(&self.state.last_release) = Some(failures.clone());
            self.state.release_version.fetch_add(1, Ordering::SeqCst);
            return failures;
        }
        // Ordered source values must be delivered or explicitly discarded before
        // the connection lease can be released. Keep the original session's
        // lease while the host-owned receipt is recoverable by capability.
        let ordered: Vec<_> = host
            .ordered_owners
            .groups(self.state.id)
            .into_iter()
            .filter(|(group, peer)| {
                !lock(&host.ordered_recordings)
                    .values()
                    .any(|owner| owner.collection_id() == *group && owner.peer_id() == peer)
            })
            .collect();
        if !ordered.is_empty() {
            for (id, peer) in &ordered {
                if host.ordered_owners.owns(*id, peer, self.state.id)
                    && !host.ordered_owners.completed(*id, peer, self.state.id)
                {
                    if let Err(error) = host
                        .central
                        .retry_retained_ordered_batch(peer, *id, OpControl::unbounded())
                        .await
                    {
                        failures.push(cleanup_failure("ordered-group", &error));
                    }
                }
            }
            host.ordered_owners.detach(self.state.id);
            let pending = error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "session.dispose.ordered",
            )
            .with_detail("ordered observations or cleanup receipt remain recoverable");
            failures.push(cleanup_failure("ordered-group", &pending));
            *lock(&self.state.last_release) = Some(failures.clone());
            self.state.release_version.fetch_add(1, Ordering::SeqCst);
            return failures;
        }
        let subscriptions: Vec<(String, Subscription)> = lock(&self.state.subscriptions)
            .iter()
            .map(|(consumer, subscription)| (consumer.clone(), subscription.clone()))
            .collect();
        for (consumer, subscription) in subscriptions {
            let released = if host
                .recorded_route(&subscription.scope, self.state.id, &consumer)
                .is_some()
            {
                host.unsubscribe_recorded(
                    &subscription.scope,
                    self.state.id,
                    &consumer,
                    &subscription.core_consumer,
                    &subscription.selector,
                    OpControl::unbounded(),
                )
                .await
            } else {
                host.central
                    .unsubscribe(
                        &subscription.peer_id,
                        &subscription.selector,
                        &subscription.core_consumer,
                        OpControl::unbounded(),
                    )
                    .await
            };
            match released {
                Ok(_) => {
                    host.remove_route(&subscription.scope, self.state.id, &consumer);
                    lock(&self.state.subscriptions).remove(&consumer);
                }
                Err(error) => failures.push(cleanup_failure("subscription", &error)),
            }
        }
        let exact_owners = lock(&self.state.exact_leases).as_ref().map(|table| {
            table
                .records
                .iter()
                .filter(|(_, entry)| !entry.released && entry.owner.is_some())
                .map(|(identity, entry)| (identity.clone(), entry.peer.clone(), entry.name.clone()))
                .collect::<Vec<_>>()
        });
        if let Some(exact_owners) = exact_owners {
            for (identity, peer, name) in exact_owners {
                match self
                    .disconnect_exact_wire(&peer, &name, &identity, OpControl::unbounded())
                    .await
                {
                    Ok(record) => {
                        if let Some(items) = record.get("failures").and_then(Value::as_array) {
                            failures.extend(items.iter().cloned());
                        }
                    }
                    Err(error) => failures.extend(connection_cleanup_failures(&error)),
                }
            }
        } else {
            let leases: Vec<(String, OwnedLease)> = lock(&self.state.leases)
                .iter()
                .map(|(peer, lease)| (peer.clone(), lease.clone()))
                .collect();
            for (peer_id, lease) in leases {
                match host
                    .central
                    .release_connection_lease_exact(
                        &peer_id,
                        &lease.name,
                        &lease.admission,
                        OpControl::unbounded(),
                    )
                    .await
                {
                    Ok(_) => {
                        let mut leases = lock(&self.state.leases);
                        if leases.get(&peer_id) == Some(&lease) {
                            leases.remove(&peer_id);
                        }
                    }
                    Err(error) => failures.extend(connection_cleanup_failures(&error)),
                }
            }
        }
        // A shared scope's leases outlive the session (87/N8); a session
        // that is its own scope ends them here.
        if let BackgroundScope::Session(_) = self.state.background_scope {
            failures.extend(
                host.release_background_scope(&self.state.background_scope)
                    .await,
            );
        }
        // A reset already retired these owners from the live maps. Preserve
        // failed physical cleanup in the disposal receipt and retain it for
        // another observation; a successful tombstone can be forgotten.
        lock(&self.state.reset_subscriptions).retain(|_, (_, _, failure)| {
            if let Some(failure) = failure {
                failures.push(cleanup_failure("subscription", failure));
                true
            } else {
                false
            }
        });
        lock(&self.state.reset_leases).retain(|_, (_, _, failure)| {
            if let Some(failure) = failure {
                failures.extend(connection_cleanup_failures(failure));
                true
            } else {
                false
            }
        });
        if failures.is_empty() {
            lock(&host.ordered_start_receipts).retain(|(session, _), _| *session != self.state.id);
            host.remove_session(self.state.id);
        }
        *lock(&self.state.last_release) = Some(failures.clone());
        self.state.release_version.fetch_add(1, Ordering::SeqCst);
        failures
    }
}

/// Owner decision C (pending) — the single policy point for a hard
/// delivery requirement the platform cannot honour on this characteristic.
///
/// CoreBluetooth picks the CCCD mode itself: when a characteristic can both
/// notify and indicate it enables notifications. A hard `require-indication`
/// there would be accepted but served as notifications, so it is refused
/// before any effect. On an indicate-only characteristic CoreBluetooth
/// enables indications and the requirement stands. Android writes the
/// requested CCCD mode itself, so nothing is refused here.
fn unenforceable_requirement(
    platform: MobilePlatform,
    required: DeliveryMode,
    properties: u8,
) -> Option<DesktopError> {
    let can_notify = properties & ubm_core::central::GATT_PROP_NOTIFY != 0;
    match (platform, required) {
        (MobilePlatform::Apple, DeliveryMode::Indication) if can_notify => Some(
            error(
                BleErrorCode::CapabilityLimited,
                BleErrorDomain::Capability,
                "gatt.subscribe.delivery",
            )
            .with_detail(
                "require-indication on a notify+indicate characteristic: CoreBluetooth enables notifications",
            ),
        ),
        _ => None,
    }
}

pub(crate) fn protocol(operation: &str) -> DesktopError {
    error(
        BleErrorCode::ProtocolMalformed,
        BleErrorDomain::Boundary,
        operation,
    )
    .with_detail("unexpected completion shape")
}

#[derive(Debug, Clone, Copy)]
enum Control {
    Rssi,
    EffectiveMtu,
    RequestMtu(u16),
    Priority(ConnectionPriority),
    Subrate(SubrateMode),
    ReadPhy,
    RequestPhy(Option<Phy>, Option<Phy>),
    /// `true` = with response.
    MaximumWriteLength(bool),
    WriteReadiness,
}

impl Control {
    const fn operation(self) -> &'static str {
        match self {
            Self::Rssi => "connection.rssi",
            Self::EffectiveMtu => "connection.effective-mtu",
            Self::RequestMtu(_) => "connection.request-mtu",
            Self::Priority(_) => "connection.request-priority",
            Self::Subrate(_) => "connection.request-subrate",
            Self::ReadPhy => "connection.read-phy",
            Self::RequestPhy(..) => "connection.request-phy",
            Self::MaximumWriteLength(_) => "connection.maximum-write-length",
            Self::WriteReadiness => "connection.write-readiness",
        }
    }
}

struct RecordingOptions {
    id: String,
    max_bytes: u64,
    max_records: u64,
}

enum MarkerAction {
    Reserve(String),
    Start {
        handle: u64,
        selector: PathSelector,
        value: Vec<u8>,
        with_response: bool,
    },
    Status(u64),
    Cancel(u64),
    Forget(u64),
}

enum Body {
    TerminalEvidenceReserve {
        scope: TerminalEvidenceScope,
        maximum_bytes: u64,
    },
    TerminalEvidenceCommit {
        reservation_id: String,
        canonical_bytes: Vec<u8>,
        digest: [u8; 32],
    },
    TerminalEvidenceRead {
        reference: String,
        maximum_bytes: u64,
    },
    TerminalEvidenceRelease(String),
    ReclaimVersion,
    RecordingReclaim {
        action: String,
        peer: String,
        collection: u64,
        recording: String,
        capability: String,
        request: Option<u64>,
        caller_key: Option<String>,
    },
    MarkerVersion,
    Marker {
        peer: String,
        collection: u64,
        action: MarkerAction,
    },
    Ordered(crate::ordered_public::OrderedCommand),
    AdapterState,
    ControlCapabilities,
    Counters,
    Quiesce,
    ContinuationDispose,
    ScanStart {
        start_operation_id: String,
        service_uuids: Vec<String>,
        device_addresses: Vec<String>,
        android: Option<AndroidScanOptions>,
        terminal_evidence_scope: Option<ScanTerminalEvidenceScope>,
        deadline: Option<tokio::time::Instant>,
    },
    ScanStop(String),
    PeersResolve(String),
    PeersKnown,
    PeersConnected(Vec<String>),
    PeersRestored,
    Reconcile,
    ClaimRestored(u64),
    PeersBonded,
    Connect {
        peer_id: String,
        lease: String,
        staging: ConnectStaging,
    },
    Disconnect {
        peer_id: String,
        lease: String,
        release_identity: Option<String>,
    },
    Control {
        peer_id: String,
        lease: String,
        control: Control,
    },
    SecurityState(String),
    CancelPairing(String),
    Pair {
        peer_id: String,
        transport: PairTransport,
    },
    Discover {
        peer_id: String,
        lease: String,
    },
    Read {
        peer_id: String,
        selector: PathSelector,
        descriptor: bool,
    },
    Write {
        peer_id: String,
        selector: PathSelector,
        value: Vec<u8>,
        with_response: bool,
        descriptor: bool,
        wait_ready: bool,
    },
    Subscribe {
        peer_id: String,
        selector: PathSelector,
        consumer: String,
        mode: Option<&'static str>,
        terminal_evidence_scope: Option<ScanTerminalEvidenceScope>,
        recording: Option<RecordingOptions>,
    },
    Unsubscribe {
        peer_id: String,
        selector: PathSelector,
        consumer: String,
    },
    BackgroundAcquire {
        kind: BackgroundKind,
        reason: String,
    },
    BackgroundRelease(String),
    BackgroundNotification {
        lease_id: String,
        title: String,
        body: Option<String>,
    },
    CompanionAssociate {
        name: Option<String>,
        service_uuid: Option<String>,
        filters_json: Option<String>,
    },
    CompanionList,
    CompanionDisassociate {
        association_id: i64,
    },
    PresenceObserve {
        peer_id: String,
    },
    PresenceUnobserve {
        peer_id: String,
    },
    Cancel {
        operation_id: String,
        admission: u64,
    },
    Dispose,
}

fn terminal_digest(value: &str) -> Result<[u8; 32], DesktopError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(wire::invalid("args.digest"));
    }
    let mut digest = [0_u8; 32];
    for (index, slot) in digest.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| wire::invalid("args.digest"))?;
    }
    Ok(digest)
}

fn terminal_digest_hex(digest: [u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn terminal_evidence_error(
    failure: ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError,
) -> DesktopError {
    use ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError;
    match failure {
        TerminalEvidenceError::Quota => error(
            BleErrorCode::StreamQuota,
            BleErrorDomain::Stream,
            "terminal-evidence",
        )
        .with_detail("terminal evidence quota exhausted"),
        TerminalEvidenceError::Busy => error(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Platform,
            "terminal-evidence",
        )
        .with_detail("terminal evidence storage is busy"),
        TerminalEvidenceError::Missing
        | TerminalEvidenceError::Unresolved
        | TerminalEvidenceError::NotStaged => error(
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Stream,
            "terminal-evidence",
        )
        .with_detail("terminal evidence reservation is unavailable"),
        TerminalEvidenceError::Invalid(_) | TerminalEvidenceError::Conflict => error(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Stream,
            "terminal-evidence",
        )
        .with_detail("terminal evidence record conflicts with its reservation"),
        TerminalEvidenceError::Corrupt | TerminalEvidenceError::Storage(_) => error(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Platform,
            "terminal-evidence",
        )
        .with_detail("terminal evidence storage failed"),
    }
}

struct Command {
    body: Body,
    operation_id: Option<String>,
    budget: Budget,
}

#[cfg(test)]
mod scan_stop_race_tests {
    use super::*;
    use crate::{
        HostOptions, MobileHost, MobilePlatform, PlatformRadio, RadioCompletion, RadioRequest,
        WakeSink,
    };
    use std::sync::{OnceLock, Weak};

    async fn reset_then_power_on(host: &MobileHost) {
        let mut resets = host.inner.central.adapter_reset_events();
        let mut off = crate::test_common::adapter_on();
        off.power = crate::AdapterPower::Off;
        host.ingest(crate::RadioIngress::AdapterState(off));
        tokio::time::timeout(std::time::Duration::from_secs(5), resets.recv())
            .await
            .unwrap()
            .unwrap();
        host.ingest(crate::RadioIngress::AdapterState(
            crate::test_common::adapter_on(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while host.inner.central.adapter_status().lost {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn hold_publication(session: &MobileSession, kind: &'static str) -> Arc<PublicationTestGate> {
        let gate = Arc::new(PublicationTestGate {
            entered: Notify::new(),
            resume: Notify::new(),
        });
        *lock(&session.state.publication_test_gate) = Some((kind, Arc::clone(&gate)));
        gate
    }

    #[tokio::test]
    async fn reset_observation_without_peer_release_is_not_success() {
        let radio = crate::test_common::Scripted::polar();
        let (host, _) = crate::test_common::open(&radio, MobilePlatform::Android).await;
        let attachment = host.inner.central.attachment();
        let (sender, mut receiver) = tokio::sync::broadcast::channel(1);
        sender
            .send(ubm_desktop::AdapterResetEvent {
                sequence: 1,
                power: None,
                adapter_sequence: None,
                cause: ubm_desktop::AdapterLossCause::PoweredOff,
                previous: attachment.clone(),
                current: attachment.clone(),
                cancelled_operations: 0,
                ended_scan: None,
                released_links: Vec::new(),
                ended_subscriptions: 0,
                release_failures: Vec::new(),
                link_release_failures: Vec::new(),
            })
            .unwrap();
        let failure = reset_release_failure(&mut receiver, &attachment, "unobserved-peer").await;
        assert!(
            failure.is_some(),
            "missing release observation must not report success"
        );
        host.shutdown().await;
    }

    #[tokio::test]
    async fn reset_release_observation_waits_for_actual_completion_beyond_two_minutes() {
        let radio = crate::test_common::Scripted::polar();
        let (host, _) = crate::test_common::open(&radio, MobilePlatform::Android).await;
        let attachment = host.inner.central.attachment();
        let (sender, mut receiver) = tokio::sync::broadcast::channel(1);
        tokio::time::pause();
        let expected = attachment.clone();
        let observation =
            tokio::spawn(
                async move { reset_release_failure(&mut receiver, &expected, "peer").await },
            );
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(121)).await;
        tokio::task::yield_now().await;
        assert!(
            !observation.is_finished(),
            "elapsed time is not a release result"
        );
        sender
            .send(ubm_desktop::AdapterResetEvent {
                sequence: 1,
                power: None,
                adapter_sequence: None,
                cause: ubm_desktop::AdapterLossCause::PoweredOff,
                previous: attachment.clone(),
                current: attachment,
                cancelled_operations: 0,
                ended_scan: None,
                released_links: vec!["peer".into()],
                ended_subscriptions: 0,
                release_failures: Vec::new(),
                link_release_failures: Vec::new(),
            })
            .unwrap();
        assert!(observation.await.unwrap().is_none());
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reset_fences_late_successful_connect_publication_after_replacement() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("late-connect").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let gate = hold_publication(&session, "connect");
        let old_session = session.clone();
        let old = tokio::spawn(async move {
            call(
                &old_session,
                "connection.connect",
                &json!({"peerId":POLAR,"lease":"old","operationId":"old"}).to_string(),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        reset_then_power_on(&host).await;
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"new","operationId":"new"}).to_string(),
        )
        .await);
        gate.resume.notify_one();
        ok(&old.await.unwrap());
        assert_eq!(
            lock(&session.state.leases)
                .get(POLAR)
                .map(|lease| lease.name.as_str()),
            Some("s1:new"),
            "late old success replaced the live lease"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn operation_entering_after_reset_uses_replacement_attachment() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("admit-after-reset").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let gate = hold_publication(&session, "connect-before-admission");
        let task_session = session.clone();
        let task = tokio::spawn(async move {
            call(
                &task_session,
                "connection.connect",
                &json!({"peerId":POLAR,"lease":"new","operationId":"new"}).to_string(),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        reset_then_power_on(&host).await;
        gate.resume.notify_one();
        ok(&task.await.unwrap());
        assert_eq!(
            lock(&session.state.leases)
                .get(POLAR)
                .map(|lease| lease.attachment.clone()),
            Some(host.inner.central.attachment())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn late_connect_retains_reset_disconnect_failure() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("late-reset-failure").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let gate = hold_publication(&session, "connect");
        let task_session = session.clone();
        let task = tokio::spawn(async move {
            call(
                &task_session,
                "connection.connect",
                &json!({"peerId":POLAR,"lease":"old","operationId":"connect"}).to_string(),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => {
                Reply::Now(crate::RadioCompletion::Failed(crate::PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "injected disconnect failure",
                )))
            }
            request => polar_responder(request),
        }));
        reset_then_power_on(&host).await;
        gate.resume.notify_one();
        ok(&task.await.unwrap());
        let cleanup = ok(&call(
            &session,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"disconnect"}).to_string(),
        )
        .await);
        assert_eq!(
            cleanup["state"], "release-failed",
            "late publication lost reset release failure: {cleanup}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn late_subscription_retains_reset_disconnect_failure() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("late-sub-reset-failure").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"connect"}).to_string(),
        )
        .await);
        ok(&call(
            &session,
            "gatt.discover",
            &json!({"peerId":POLAR,"lease":"old","operationId":"discover"}).to_string(),
        )
        .await);
        let gate = hold_publication(&session, "subscribe");
        let task_session = session.clone();
        let task = tokio::spawn(async move {
            call(&task_session, "gatt.subscribe", &json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"subscribe"}).to_string()).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => {
                Reply::Now(crate::RadioCompletion::Failed(crate::PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "injected disconnect failure",
                )))
            }
            request => polar_responder(request),
        }));
        reset_then_power_on(&host).await;
        gate.resume.notify_one();
        ok(&task.await.unwrap());
        let result = call(&session, "gatt.unsubscribe", &json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"unsubscribe"}).to_string()).await;
        let (failure, _) = failure(&result);
        assert_eq!(failure["operation"], "connection.disconnect");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn disconnect_waits_for_inflight_reset_release_result() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("reset-release-inflight").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"connect"}).to_string(),
        )
        .await);
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Hold,
            request => polar_responder(request),
        }));
        let mut off = adapter_on();
        off.power = crate::AdapterPower::Off;
        host.ingest(crate::RadioIngress::AdapterState(off));
        let held = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(id) = radio.held_of(crate::RequestKind::Disconnect).first() {
                    break *id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let task_session = session.clone();
        let mut task = tokio::spawn(async move {
            call(
                &task_session,
                "connection.disconnect",
                &json!({"peerId":POLAR,"lease":"old","operationId":"disconnect"}).to_string(),
            )
            .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut task)
                .await
                .is_err(),
            "disconnect answered before the reset release completed"
        );
        radio.answer(
            held,
            crate::RadioCompletion::Failed(crate::PlatformFailure::new(
                crate::FailureKind::Platform,
                "injected disconnect failure",
            )),
        );
        let cleanup = ok(
            &tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(cleanup["state"], "release-failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reset_disconnect_failure_is_not_reported_released() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("reset-release-failure").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"connect"}).to_string(),
        )
        .await);
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => {
                Reply::Now(crate::RadioCompletion::Failed(crate::PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "injected disconnect failure",
                )))
            }
            request => polar_responder(request),
        }));
        reset_then_power_on(&host).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !lock(&session.state.reset_leases)
                .values()
                .any(|(peer, _, _)| peer == POLAR)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let cleanup = ok(&call(
            &session,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"disconnect"}).to_string(),
        )
        .await);
        assert_eq!(
            cleanup["state"], "release-failed",
            "the reset failed to release the old physical connection: {cleanup}"
        );
        let retry = ok(&call(
            &session,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"disconnect-again"}).to_string(),
        )
        .await);
        assert_eq!(
            retry["state"], "release-failed",
            "unrepaired cleanup failure must not become success"
        );
        let disposed = session.dispose_failures().await;
        assert!(
            disposed
                .iter()
                .any(|failure| failure.to_string().contains("injected disconnect failure")),
            "dispose lost reset cleanup failure: {disposed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reset_disconnect_exposes_each_structured_cleanup_failure() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("reset-multiple-failures").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"connect"}).to_string(),
        )
        .await);
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Now(crate::RadioCompletion::Failed(
                crate::PlatformFailure::new(crate::FailureKind::Platform, "initial"),
            )),
            request => polar_responder(request),
        }));
        reset_then_power_on(&host).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !lock(&session.state.reset_leases)
                .values()
                .any(|(peer, _, _)| peer == POLAR)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let first = crate::PlatformFailure::new(crate::FailureKind::PermissionDenied, "first")
            .to_error(
                crate::radio::RequestKind::Disconnect,
                MobilePlatform::Android,
            );
        let second = crate::PlatformFailure::new(crate::FailureKind::Unsupported, "second")
            .to_error(
                crate::radio::RequestKind::Disconnect,
                MobilePlatform::Android,
            );
        let mut aggregate = ubm_desktop::PlatformDetail::new("android", "cleanup-failures");
        aggregate.cleanup_failures = vec![first.clone(), second];
        let combined = first.with_platform(aggregate);
        for (peer, _, failure) in lock(&session.state.reset_leases).values_mut() {
            if peer == POLAR {
                *failure = Some(combined.clone());
            }
        }
        let cleanup = ok(&call(
            &session,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"disconnect"}).to_string(),
        )
        .await);
        let failures = cleanup["failures"].as_array().unwrap();
        assert_eq!(
            failures.len(),
            2,
            "each native cause must be delivered separately: {cleanup}"
        );
        assert_ne!(failures[0]["code"], failures[1]["code"]);
        assert!(failures[0].to_string().contains("first"));
        assert!(failures[1].to_string().contains("second"));
        let retry = ok(&call(
            &session,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"disconnect-retry"}).to_string(),
        )
        .await);
        assert_eq!(
            retry, cleanup,
            "reset failures must remain retained for retry"
        );
        let disposed = session.dispose_failures().await;
        assert_eq!(
            disposed, *failures,
            "disposal must expose the same retained causes"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn normal_disconnect_exposes_each_structured_cleanup_failure() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("normal-multiple-failures").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"connect"}).to_string(),
        )
        .await);
        let first = crate::PlatformFailure::new(crate::FailureKind::PermissionDenied, "first")
            .to_error(
                crate::radio::RequestKind::Disconnect,
                MobilePlatform::Android,
            );
        let second = crate::PlatformFailure::new(crate::FailureKind::Unsupported, "second")
            .to_error(
                crate::radio::RequestKind::Disconnect,
                MobilePlatform::Android,
            );
        let mut aggregate = ubm_desktop::PlatformDetail::new("android", "cleanup-failures");
        aggregate.cleanup_failures = vec![first.clone(), second];
        *lock(&session.state.disconnect_failure_override) = Some(first.with_platform(aggregate));
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Now(crate::RadioCompletion::Failed(
                crate::PlatformFailure::new(crate::FailureKind::Platform, "boundary failure"),
            )),
            request => polar_responder(request),
        }));
        let cleanup = ok(&call(
            &session,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"disconnect"}).to_string(),
        )
        .await);
        let failures = cleanup["failures"].as_array().unwrap();
        assert_eq!(cleanup["state"], "release-failed");
        assert_eq!(failures.len(), 2, "{cleanup}");
        assert_eq!(failures[0]["code"], "permission.denied");
        assert_eq!(failures[1]["code"], "capability.unsupported");
        assert_eq!(failures[0]["resourceKind"], "connection");
        assert_eq!(failures[1]["resourceKind"], "connection");
        assert!(failures[0]["detail"].as_str().unwrap().contains("first"));
        assert!(failures[1]["detail"].as_str().unwrap().contains("second"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_reset_disconnect_exposes_each_structured_cleanup_failure() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("v4-reset-multiple", "ubm-mobile-wire/4", "{}")
            .unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let connected = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"connect"}).to_string(),
        )
        .await);
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Now(crate::RadioCompletion::Failed(
                crate::PlatformFailure::new(crate::FailureKind::Platform, "initial"),
            )),
            request => polar_responder(request),
        }));
        reset_then_power_on(&host).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !lock(&session.state.reset_leases)
                .values()
                .any(|(peer, _, _)| peer == POLAR)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let first = crate::PlatformFailure::new(crate::FailureKind::PermissionDenied, "first")
            .to_error(
                crate::radio::RequestKind::Disconnect,
                MobilePlatform::Android,
            );
        let second = crate::PlatformFailure::new(crate::FailureKind::Unsupported, "second")
            .to_error(
                crate::radio::RequestKind::Disconnect,
                MobilePlatform::Android,
            );
        let mut aggregate = ubm_desktop::PlatformDetail::new("android", "cleanup-failures");
        aggregate.cleanup_failures = vec![first.clone(), second];
        for (peer, _, failure) in lock(&session.state.reset_leases).values_mut() {
            if peer == POLAR {
                *failure = Some(first.clone().with_platform(aggregate.clone()));
            }
        }
        let args =
            json!({"peerId":POLAR,"lease":"old","releaseIdentity":connected["releaseIdentity"]})
                .to_string();
        let cleanup = ok(&call(&session, "connection.disconnect", &args).await);
        assert_eq!(cleanup["state"], "release-failed");
        assert_eq!(cleanup["failures"].as_array().unwrap().len(), 2);
        assert_eq!(cleanup["failures"][0]["code"], "permission.denied");
        assert_eq!(cleanup["failures"][1]["code"], "capability.unsupported");
        assert_eq!(
            ok(&call(&session, "connection.disconnect", &args).await),
            cleanup
        );
    }

    #[test]
    fn connection_cleanup_unwraps_nested_aggregates_but_keeps_single_errors() {
        let first = crate::PlatformFailure::new(crate::FailureKind::PermissionDenied, "first")
            .to_error(
                crate::radio::RequestKind::Disconnect,
                MobilePlatform::Android,
            );
        let second = crate::PlatformFailure::new(crate::FailureKind::Unsupported, "second")
            .to_error(
                crate::radio::RequestKind::Disconnect,
                MobilePlatform::Android,
            );
        let third = crate::PlatformFailure::new(crate::FailureKind::Busy, "third").to_error(
            crate::radio::RequestKind::Disconnect,
            MobilePlatform::Android,
        );
        let mut inner = ubm_desktop::PlatformDetail::new("android", "cleanup-failures");
        inner.cleanup_failures = vec![first.clone(), second.clone()];
        let mut outer = ubm_desktop::PlatformDetail::new("android", "cleanup-failures");
        outer.cleanup_failures = vec![first.clone().with_platform(inner), third];
        let flattened = connection_cleanup_failures(&first.clone().with_platform(outer));
        assert_eq!(flattened.len(), 3);
        assert_eq!(flattened[0]["code"], "permission.denied");
        assert_eq!(flattened[1]["code"], "capability.unsupported");
        assert!(flattened[2]["detail"].as_str().unwrap().contains("third"));
        assert_eq!(
            connection_cleanup_failures(&second),
            vec![cleanup_failure("connection", &second)]
        );
        let empty = first
            .clone()
            .with_platform(ubm_desktop::PlatformDetail::new(
                "android",
                "cleanup-failures",
            ));
        assert_eq!(
            connection_cleanup_failures(&empty),
            vec![cleanup_failure("connection", &empty)]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn replacement_connection_preserves_old_reset_failure() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("replacement-failure").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"connect-old"}).to_string(),
        )
        .await);
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => {
                Reply::Now(crate::RadioCompletion::Failed(crate::PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "injected disconnect failure",
                )))
            }
            request => polar_responder(request),
        }));
        reset_then_power_on(&host).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !lock(&session.state.reset_leases)
                .values()
                .any(|(peer, _, _)| peer == POLAR)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        radio.set_responder(Box::new(polar_responder));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"new","operationId":"connect-new"}).to_string(),
        )
        .await);
        let cleanup = ok(&call(
            &session,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"disconnect-old"}).to_string(),
        )
        .await);
        assert_eq!(
            cleanup["state"], "release-failed",
            "replacement erased old cleanup failure: {cleanup}"
        );
        assert_eq!(
            lock(&session.state.leases)
                .get(POLAR)
                .map(|lease| lease.name.clone()),
            Some(session.state.core_name("new"))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_resets_preserve_each_lease_release_failure() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("two-reset-failures").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        for (lease, message) in [
            ("old", "first reset failure"),
            ("new", "second reset failure"),
        ] {
            radio.set_responder(Box::new(polar_responder));
            ok(&call(
                &session,
                "connection.connect",
                &json!({"peerId":POLAR,"lease":lease,"operationId":format!("connect-{lease}")})
                    .to_string(),
            )
            .await);
            radio.set_responder(Box::new(move |request| match request {
                crate::RadioRequest::Disconnect { .. } => {
                    Reply::Now(crate::RadioCompletion::Failed(crate::PlatformFailure::new(
                        crate::FailureKind::Platform,
                        message,
                    )))
                }
                request => polar_responder(request),
            }));
            reset_then_power_on(&host).await;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while lock(&session.state.leases).contains_key(POLAR) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        for (lease, message) in [
            ("old", "first reset failure"),
            ("new", "second reset failure"),
        ] {
            let cleanup = ok(&call(
                &session,
                "connection.disconnect",
                &json!({"peerId":POLAR,"lease":lease,"operationId":format!("release-{lease}")})
                    .to_string(),
            )
            .await);
            assert_eq!(cleanup["state"], "release-failed", "{cleanup}");
            assert!(cleanup.to_string().contains(message), "{cleanup}");
        }
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_lease_name_with_unresolved_failure_refuses_ambiguous_disconnect() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("same-name-reset-failure").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"old-connect"}).to_string(),
        )
        .await);
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Now(crate::RadioCompletion::Failed(
                crate::PlatformFailure::new(crate::FailureKind::Platform, "old reset failure"),
            )),
            request => polar_responder(request),
        }));
        reset_then_power_on(&host).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while lock(&session.state.leases).contains_key(POLAR) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        radio.set_responder(Box::new(polar_responder));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"new-connect"}).to_string(),
        )
        .await);
        let cleanup = ok(&call(
            &session,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"new-disconnect"}).to_string(),
        )
        .await);
        assert_eq!(cleanup["state"], "release-failed", "{cleanup}");
        assert!(
            cleanup.to_string().contains("both a live owner"),
            "{cleanup}"
        );
        assert!(lock(&session.state.leases).contains_key(POLAR));
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn repeated_same_name_reset_failures_remain_distinct_and_fail_closed() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("same-name-repeated-resets").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        for (operation, message) in [("first", "first failure"), ("second", "second failure")] {
            radio.set_responder(Box::new(polar_responder));
            ok(&call(
                &session,
                "connection.connect",
                &json!({"peerId":POLAR,"lease":"same","operationId":operation}).to_string(),
            )
            .await);
            radio.set_responder(Box::new(move |request| match request {
                crate::RadioRequest::Disconnect { .. } => {
                    Reply::Now(crate::RadioCompletion::Failed(crate::PlatformFailure::new(
                        crate::FailureKind::Platform,
                        message,
                    )))
                }
                request => polar_responder(request),
            }));
            reset_then_power_on(&host).await;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while lock(&session.state.leases).contains_key(POLAR) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        assert_eq!(lock(&session.state.reset_leases).len(), 2);
        let cleanup = ok(&call(
            &session,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"release"}).to_string(),
        )
        .await);
        assert_eq!(cleanup["state"], "release-failed", "{cleanup}");
        assert!(
            cleanup.to_string().contains("multiple retired operations"),
            "{cleanup}"
        );
        let disposed = session.dispose_failures().await;
        assert!(
            disposed
                .iter()
                .any(|failure| failure.to_string().contains("first failure"))
        );
        assert!(
            disposed
                .iter()
                .any(|failure| failure.to_string().contains("second failure"))
        );
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_consumer_with_unresolved_failure_refuses_ambiguous_unsubscribe() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("same-consumer-reset-failure").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"connect-old"}).to_string(),
        )
        .await);
        ok(&call(
            &session,
            "gatt.discover",
            &json!({"peerId":POLAR,"lease":"old","operationId":"discover-old"}).to_string(),
        )
        .await);
        ok(&call(
            &session,
            "gatt.subscribe",
            &json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"sub-old"})
                .to_string(),
        )
        .await);
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Now(crate::RadioCompletion::Failed(
                crate::PlatformFailure::new(crate::FailureKind::Platform, "old reset failure"),
            )),
            request => polar_responder(request),
        }));
        reset_then_power_on(&host).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while lock(&session.state.subscriptions).contains_key("hr") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        radio.set_responder(Box::new(polar_responder));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"new","operationId":"connect-new"}).to_string(),
        )
        .await);
        ok(&call(
            &session,
            "gatt.discover",
            &json!({"peerId":POLAR,"lease":"new","operationId":"discover-new"}).to_string(),
        )
        .await);
        ok(&call(
            &session,
            "gatt.subscribe",
            &json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"sub-new"})
                .to_string(),
        )
        .await);
        let cleanup = call(&session, "gatt.unsubscribe", &json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"unsub-new"}).to_string()).await;
        let (failure, _) = failure(&cleanup);
        assert!(
            failure.to_string().contains("both a live owner"),
            "{failure}"
        );
        assert!(lock(&session.state.subscriptions).contains_key("hr"));
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reset_between_owner_check_and_release_cannot_release_replacement() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("release-admission-reset").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"old-connect"}).to_string(),
        )
        .await);
        let gate = hold_publication(&session, "disconnect-before-central-release");
        let old_session = session.clone();
        let old = tokio::spawn(async move {
            call(
                &old_session,
                "connection.disconnect",
                &json!({"peerId":POLAR,"lease":"same","operationId":"old-disconnect"}).to_string(),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Now(crate::RadioCompletion::Failed(
                crate::PlatformFailure::new(crate::FailureKind::Platform, "old reset failure"),
            )),
            request => polar_responder(request),
        }));
        reset_then_power_on(&host).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while lock(&session.state.leases).contains_key(POLAR) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        radio.set_responder(Box::new(polar_responder));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"new-connect"}).to_string(),
        )
        .await);
        gate.resume.notify_one();
        let cleanup = ok(&old.await.unwrap());
        let replacement = lock(&session.state.leases).get(POLAR).cloned().unwrap();
        assert_eq!(replacement.attachment, host.inner.central.attachment());
        let report = session.peer_record(POLAR).await.unwrap();
        assert_eq!(
            report.connection_state,
            Some(ConnectionState::Connected),
            "old release changed the replacement: {cleanup}"
        );
        assert_eq!(
            cleanup["state"], "release-failed",
            "old release must observe its reset failure: {cleanup}"
        );
        assert!(
            cleanup.to_string().contains("old reset failure"),
            "{cleanup}"
        );
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reset_during_recorded_unsubscribe_allows_later_same_name_replacement() {
        use crate::test_common::*;
        use serde_json::json;
        let Some(directory) = recording_test_directory(
            "session::scan_stop_race_tests::reset_during_recorded_unsubscribe_allows_later_same_name_replacement",
        ) else {
            return;
        };
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        host.continuation()
            .configure_recording_directory(&directory)
            .unwrap();
        let session = host.open_session("recorded-release-reset").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        for (op, id) in [
            ("connection.connect", "old-connect"),
            ("gatt.discover", "old-discover"),
        ] {
            ok(&call(
                &session,
                op,
                &json!({"peerId":POLAR,"lease":"same","operationId":id}).to_string(),
            )
            .await);
        }
        ok(&call(&session, "gatt.subscribe-recorded", &json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"old-sub","recording":{"id":"old","maxBytes":1048576,"maxRecords":1000}}).to_string()).await);
        let gate = hold_publication(&session, "unsubscribe-before-central-release");
        let old_session = session.clone();
        let old = tokio::spawn(async move {
            call(&old_session, "gatt.unsubscribe", &json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"old-unsub"}).to_string()).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        reset_then_power_on(&host).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while lock(&session.state.subscriptions).contains_key("hr") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // The old operation owns the FIFO slot. Let it observe reset before
        // asking a replacement discovery to enter behind that slot.
        gate.resume.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), old)
            .await
            .unwrap()
            .unwrap();
        ok(&result);
        let disable_count = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, crate::RadioRequest::DisableNotifications { .. }))
            .count();
        for (op, id) in [
            ("connection.connect", "new-connect"),
            ("gatt.discover", "new-discover"),
        ] {
            let args = json!({"peerId":POLAR,"lease":"same","operationId":id}).to_string();
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(5), call(&session, op, &args))
                    .await
                    .expect("replacement operation blocked behind stale recorded unsubscribe");
            ok(&result);
        }
        let args = json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"new-sub","recording":{"id":"new","maxBytes":1048576,"maxRecords":1000}}).to_string();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            call(&session, "gatt.subscribe-recorded", &args),
        )
        .await
        .expect("replacement subscription blocked behind stale recorded unsubscribe");
        ok(&result);
        assert_eq!(
            lock(&session.state.subscriptions)
                .get("hr")
                .map(|s| s.attachment.clone()),
            Some(host.inner.central.attachment())
        );
        assert_eq!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, crate::RadioRequest::DisableNotifications { .. }))
                .count(),
            disable_count
        );
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lost_disconnect_reply_cannot_release_same_name_replacement() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("lost-release-reply", "ubm-mobile-wire/4", "{}")
            .unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let old = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"old-connect"}).to_string(),
        )
        .await);
        let old_identity = old["releaseIdentity"]
            .as_str()
            .expect("owner-issued release identity");
        // Discard the successful first reply, as a transport loss would.
        let released = ok(&call(&session, "connection.disconnect", &json!({"peerId":POLAR,"lease":"same","releaseIdentity":old_identity,"operationId":"old-disconnect"}).to_string()).await);
        assert_eq!(released["state"], "released");
        let new = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"new-connect"}).to_string(),
        )
        .await);
        assert_ne!(new["releaseIdentity"], old_identity);
        let replacement = lock(&session.state.leases)
            .get(POLAR)
            .unwrap()
            .admission
            .clone();
        let disconnects = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
            .count();
        let _ = call(&session, "connection.disconnect", &json!({"peerId":POLAR,"lease":"same","releaseIdentity":old_identity,"operationId":"retry-disconnect"}).to_string()).await;
        assert_eq!(
            lock(&session.state.leases)
                .get(POLAR)
                .map(|owner| owner.admission.clone()),
            Some(replacement),
            "retry of lost old release removed the replacement"
        );
        assert_eq!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
                .count(),
            disconnects
        );
        assert_eq!(
            session.peer_record(POLAR).await.unwrap().connection_state,
            Some(ConnectionState::Connected)
        );
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_disconnect_requires_matching_session_peer_lease_and_identity() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("owner", "ubm-mobile-wire/4", "{}")
            .unwrap();
        let (other, _) = host
            .admit_session("other", "ubm-mobile-wire/4", "{}")
            .unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let connected = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"connect"}).to_string(),
        )
        .await);
        let identity = connected["releaseIdentity"].as_str().unwrap();
        let missing = call(
            &session,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"same"}).to_string(),
        )
        .await;
        assert_eq!(failure(&missing).0["code"], "argument.invalid");
        for malformed in [json!(""), json!(42), json!("x".repeat(129))] {
            let result = call(
                &session,
                "connection.disconnect",
                &json!({"peerId":POLAR,"lease":"same","releaseIdentity":malformed}).to_string(),
            )
            .await;
            assert_eq!(failure(&result).0["code"], "argument.invalid");
        }
        let before = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
            .count();
        for (target, peer, lease, value) in [
            (&session, POLAR, "wrong", identity),
            (&session, "wrong-peer", "same", identity),
            (&session, POLAR, "same", "unknown"),
            (&other, POLAR, "same", identity),
        ] {
            let result = ok(&call(
                target,
                "connection.disconnect",
                &json!({"peerId":peer,"lease":lease,"releaseIdentity":value}).to_string(),
            )
            .await);
            assert_eq!(result["state"], "release-failed", "{result}");
        }
        assert_eq!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
                .count(),
            before
        );
        assert!(lock(&session.state.leases).contains_key(POLAR));
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_failed_disconnect_retains_exact_owner_for_retry() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("retry", "ubm-mobile-wire/4", "{}")
            .unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let connected = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"connect"}).to_string(),
        )
        .await);
        let identity = connected["releaseIdentity"].as_str().unwrap();
        session.set_exact_connection_receipt_limit(0).unwrap();
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Now(crate::RadioCompletion::Failed(
                crate::PlatformFailure::new(crate::FailureKind::Platform, "injected"),
            )),
            request => polar_responder(request),
        }));
        let args = json!({"peerId":POLAR,"lease":"same","releaseIdentity":identity}).to_string();
        assert_eq!(
            ok(&call(&session, "connection.disconnect", &args).await)["state"],
            "release-failed"
        );
        assert!(lock(&session.state.leases).contains_key(POLAR));
        assert!(
            lock(&session.state.exact_leases)
                .as_ref()
                .unwrap()
                .records
                .contains_key(identity),
            "failed cleanup is not a terminal receipt"
        );
        session.set_exact_connection_live_limit(1).unwrap();
        let refused = call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"other","operationId":"capacity"}).to_string(),
        )
        .await;
        assert_eq!(
            failure(&refused).0["code"],
            "lifecycle.invalid-state",
            "unresolved cleanup still occupies capacity"
        );
        radio.set_responder(Box::new(polar_responder));
        assert_eq!(
            ok(&call(&session, "connection.disconnect", &args).await)["state"],
            "released"
        );
        assert!(
            !lock(&session.state.exact_leases)
                .as_ref()
                .unwrap()
                .records
                .contains_key(identity)
        );
        let admitted = call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"other","operationId":"after-retry"}).to_string(),
        )
        .await;
        ok(&admitted);
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_two_leases_on_one_link_release_independently() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("shared", "ubm-mobile-wire/4", "{}")
            .unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let first = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"one","operationId":"first"}).to_string(),
        )
        .await);
        let second = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"two","operationId":"second"}).to_string(),
        )
        .await);
        assert_ne!(first["releaseIdentity"], second["releaseIdentity"]);
        assert_eq!(
            first["connectionGeneration"],
            second["connectionGeneration"]
        );
        let count = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
            .count();
        let release =
            json!({"peerId":POLAR,"lease":"one","releaseIdentity":first["releaseIdentity"]})
                .to_string();
        assert_eq!(
            ok(&call(&session, "connection.disconnect", &release).await)["state"],
            "released"
        );
        assert_eq!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
                .count(),
            count
        );
        assert_eq!(
            ok(&call(&session, "connection.disconnect", &release).await)["state"],
            "released"
        );
        let release =
            json!({"peerId":POLAR,"lease":"two","releaseIdentity":second["releaseIdentity"]})
                .to_string();
        assert_eq!(
            ok(&call(&session, "connection.disconnect", &release).await)["state"],
            "released"
        );
        assert_eq!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
                .count(),
            count + 1
        );
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_concurrent_disconnects_share_the_terminal_receipt() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("concurrent", "ubm-mobile-wire/4", "{}")
            .unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let connected = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"one","operationId":"connect"}).to_string(),
        )
        .await);
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Hold,
            request => polar_responder(request),
        }));
        let args =
            json!({"peerId":POLAR,"lease":"one","releaseIdentity":connected["releaseIdentity"]})
                .to_string();
        let first_session = session.clone();
        let first_args = args.clone();
        let first = tokio::spawn(async move {
            call(&first_session, "connection.disconnect", &first_args).await
        });
        let held = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(id) = radio.held_of(crate::RequestKind::Disconnect).first() {
                    break *id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let second_session = session.clone();
        let second =
            tokio::spawn(
                async move { call(&second_session, "connection.disconnect", &args).await },
            );
        radio.answer(held, crate::RadioCompletion::Unit);
        assert_eq!(ok(&first.await.unwrap())["state"], "released");
        assert_eq!(ok(&second.await.unwrap())["state"], "released");
        assert_eq!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
                .count(),
            1
        );
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_cancelling_joiner_after_disconnect_dispatch_does_not_cancel_leader() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("cancel-joiner", "ubm-mobile-wire/4", "{}")
            .unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let connected = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"one","operationId":"connect"}).to_string(),
        )
        .await);
        let identity = connected["releaseIdentity"].as_str().unwrap();
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Hold,
            request => polar_responder(request),
        }));
        let first_session = session.clone();
        let first_args =
            json!({"peerId":POLAR,"lease":"one","releaseIdentity":identity,"operationId":"leader"})
                .to_string();
        let first = tokio::spawn(async move {
            call(&first_session, "connection.disconnect", &first_args).await
        });
        let held = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(id) = radio.held_of(crate::RequestKind::Disconnect).first() {
                    break *id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let second_session = session.clone();
        let second_args =
            json!({"peerId":POLAR,"lease":"one","releaseIdentity":identity,"operationId":"joiner"})
                .to_string();
        let second = tokio::spawn(async move {
            call(&second_session, "connection.disconnect", &second_args).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let joined = lock(&session.state.exact_leases)
                    .as_ref()
                    .and_then(|table| table.records.get(identity))
                    .and_then(|entry| entry.attempt.as_ref())
                    .is_some_and(|attempt| Arc::strong_count(attempt) >= 4);
                if joined {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        ok(&call(
            &session,
            "op.cancel",
            &json!({"operationId":"joiner"}).to_string(),
        )
        .await);
        let cancelled = ok(
            &tokio::time::timeout(std::time::Duration::from_secs(5), second)
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(cancelled["state"], "release-failed");
        assert!(lock(&session.state.leases).contains_key(POLAR));
        radio.answer(held, crate::RadioCompletion::Unit);
        assert_eq!(ok(&first.await.unwrap())["state"], "released");
        let retry = call(
            &session,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"one","releaseIdentity":identity,"operationId":"retry"})
                .to_string(),
        )
        .await;
        assert_eq!(ok(&retry)["state"], "released");
        assert_eq!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
                .count(),
            1
        );
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_concurrent_disconnects_share_a_failed_attempt() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("concurrent-failure", "ubm-mobile-wire/4", "{}")
            .unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let connected = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"one","operationId":"connect"}).to_string(),
        )
        .await);
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Hold,
            request => polar_responder(request),
        }));
        let args =
            json!({"peerId":POLAR,"lease":"one","releaseIdentity":connected["releaseIdentity"]})
                .to_string();
        let first_session = session.clone();
        let first_args = args.clone();
        let first = tokio::spawn(async move {
            call(&first_session, "connection.disconnect", &first_args).await
        });
        let held = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(id) = radio.held_of(crate::RequestKind::Disconnect).first() {
                    break *id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let second_session = session.clone();
        let second_args = args.clone();
        let second = tokio::spawn(async move {
            call(&second_session, "connection.disconnect", &second_args).await
        });
        let identity = connected["releaseIdentity"].as_str().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let joined = lock(&session.state.exact_leases)
                    .as_ref()
                    .and_then(|table| table.records.get(identity))
                    .and_then(|entry| entry.attempt.as_ref())
                    .is_some_and(|attempt| Arc::strong_count(attempt) >= 4);
                if joined {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        radio.set_responder(Box::new(polar_responder));
        radio.answer(
            held,
            crate::RadioCompletion::Failed(crate::PlatformFailure::new(
                crate::FailureKind::Platform,
                "shared failure",
            )),
        );
        assert_eq!(ok(&first.await.unwrap())["state"], "release-failed");
        assert_eq!(ok(&second.await.unwrap())["state"], "release-failed");
        assert_eq!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
                .count(),
            1
        );
        assert_eq!(
            ok(&call(&session, "connection.disconnect", &args).await)["state"],
            "released"
        );
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_reset_failure_remains_bound_to_old_identity_after_replacement() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("reset", "ubm-mobile-wire/4", "{}")
            .unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let old = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"old"}).to_string(),
        )
        .await);
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Now(crate::RadioCompletion::Failed(
                crate::PlatformFailure::new(crate::FailureKind::Platform, "reset debt"),
            )),
            request => polar_responder(request),
        }));
        reset_then_power_on(&host).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while lock(&session.state.leases).contains_key(POLAR) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        radio.set_responder(Box::new(polar_responder));
        let new = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"new"}).to_string(),
        )
        .await);
        let before = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
            .count();
        let args = json!({"peerId":POLAR,"lease":"same","releaseIdentity":old["releaseIdentity"]})
            .to_string();
        let failure = ok(&call(&session, "connection.disconnect", &args).await);
        assert_eq!(failure["state"], "release-failed");
        assert!(failure.to_string().contains("reset debt"));
        assert_ne!(old["releaseIdentity"], new["releaseIdentity"]);
        assert_eq!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
                .count(),
            before
        );
        assert!(lock(&session.state.leases).contains_key(POLAR));
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_dispose_releases_every_shared_lease() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, record) = host
            .admit_session("dispose-shared", "ubm-mobile-wire/4", "{}")
            .unwrap();
        assert_eq!(parse(&record)["wireRevision"], "ubm-mobile-wire/4");
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        for (lease, op) in [("one", "first"), ("two", "second")] {
            ok(&call(
                &session,
                "connection.connect",
                &json!({"peerId":POLAR,"lease":lease,"operationId":op}).to_string(),
            )
            .await);
        }
        let result = ok(&call(&session, "session.dispose", "{}").await);
        assert_eq!(result["state"], "released", "{result}");
        assert_eq!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
                .count(),
            1
        );
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_subscription_refuses_until_exact_release_is_available() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("subscription-gap", "ubm-mobile-wire/4", "{}")
            .unwrap();
        for (op, args) in [
            (
                "gatt.subscribe",
                json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"sub"}),
            ),
            (
                "gatt.subscribe-recorded",
                json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"recorded","recording":{"id":"r","maxBytes":4096,"maxRecords":10}}),
            ),
            (
                "gatt.unsubscribe",
                json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"unsub","releaseIdentity":"unknown"}),
            ),
        ] {
            let result = call(&session, op, &args.to_string()).await;
            assert_eq!(failure(&result).0["code"], "capability.unsupported", "{op}");
        }
        assert!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| !matches!(r, crate::RadioRequest::EnableNotifications { .. }))
        );
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_connection_live_capacity_is_configurable_without_evicting_owners() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("live-capacity", "ubm-mobile-wire/4", "{}")
            .unwrap();
        session.set_exact_connection_live_limit(1).unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let first = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"one","operationId":"one"}).to_string(),
        )
        .await);
        let refused = call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"two","operationId":"refused"}).to_string(),
        )
        .await;
        assert_eq!(failure(&refused).0["code"], "lifecycle.invalid-state");
        session.set_exact_connection_live_limit(2).unwrap();
        let second = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"two","operationId":"two"}).to_string(),
        )
        .await);
        session.set_exact_connection_live_limit(0).unwrap();
        for (lease, identity) in [
            ("one", &first["releaseIdentity"]),
            ("two", &second["releaseIdentity"]),
        ] {
            let result = call(
                &session,
                "connection.disconnect",
                &json!({"peerId":POLAR,"lease":lease,"releaseIdentity":identity}).to_string(),
            )
            .await;
            assert_eq!(ok(&result)["state"], "released");
        }
        let refused = call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"three","operationId":"three"}).to_string(),
        )
        .await;
        assert_eq!(failure(&refused).0["code"], "lifecycle.invalid-state");
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_terminal_receipts_do_not_permanently_block_admission() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("receipt-retention", "ubm-mobile-wire/4", "{}")
            .unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let first = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"first","operationId":"first"}).to_string(),
        )
        .await);
        let identity = first["releaseIdentity"].as_str().unwrap();
        assert_eq!(
            ok(&call(
                &session,
                "connection.disconnect",
                &json!({"peerId":POLAR,"lease":"first","releaseIdentity":identity}).to_string()
            )
            .await)["state"],
            "released"
        );
        // Fill the existing terminal table without issuing a thousand radio operations.
        {
            let mut guard = lock(&session.state.exact_leases);
            let table = guard.as_mut().unwrap();
            let owner = table.records[identity].owner.clone();
            for n in 0..1023 {
                let old = format!("old-{n}");
                table.records.insert(
                    old.clone(),
                    ExactLeaseRecord {
                        peer: POLAR.into(),
                        name: format!("old-{n}"),
                        owner: owner.clone(),
                        released: true,
                        attempt: None,
                    },
                );
                table.receipts.push_back(old);
            }
        }
        let replacement = call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"first","operationId":"replacement"}).to_string(),
        )
        .await;
        let replacement = ok(&replacement);
        assert_ne!(replacement["releaseIdentity"], identity);
        session.set_exact_connection_receipt_limit(1).unwrap();
        let before = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
            .count();
        let retry = ok(&call(
            &session,
            "connection.disconnect",
            &json!({"peerId":POLAR,"lease":"first","releaseIdentity":identity}).to_string(),
        )
        .await);
        assert_eq!(retry["state"], "release-failed");
        assert_eq!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
                .count(),
            before
        );
        assert!(lock(&session.state.leases).contains_key(POLAR));
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_receipt_limit_evicts_oldest_without_touching_replacement() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("receipt-order", "ubm-mobile-wire/4", "{}")
            .unwrap();
        assert!(session.set_exact_connection_receipt_limit(65_537).is_err());
        session.set_exact_connection_receipt_limit(1).unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let mut identities = Vec::new();
        for operation in ["first", "second"] {
            let result = ok(&call(
                &session,
                "connection.connect",
                &json!({"peerId":POLAR,"lease":"same","operationId":operation}).to_string(),
            )
            .await);
            let identity = result["releaseIdentity"].as_str().unwrap().to_owned();
            let result = call(
                &session,
                "connection.disconnect",
                &json!({"peerId":POLAR,"lease":"same","releaseIdentity":identity}).to_string(),
            )
            .await;
            assert_eq!(ok(&result)["state"], "released");
            identities.push(identity);
        }
        let replacement = ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"third"}).to_string(),
        )
        .await);
        let count = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
            .count();
        for (identity, state) in [
            (&identities[0], "release-failed"),
            (&identities[1], "released"),
        ] {
            let result = call(
                &session,
                "connection.disconnect",
                &json!({"peerId":POLAR,"lease":"same","releaseIdentity":identity}).to_string(),
            )
            .await;
            assert_eq!(ok(&result)["state"], state);
        }
        assert_eq!(
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, crate::RadioRequest::Disconnect { .. }))
                .count(),
            count
        );
        assert_ne!(replacement["releaseIdentity"], identities[1]);
        assert!(lock(&session.state.leases).contains_key(POLAR));
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v4_exhausted_identity_counter_refuses_before_radio() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let (session, _) = host
            .admit_session("identity-limit", "ubm-mobile-wire/4", "{}")
            .unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        lock(&session.state.exact_leases).as_mut().unwrap().next = u64::MAX;
        let before = radio.requests.lock().unwrap().len();
        let result = call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"one","operationId":"connect"}).to_string(),
        )
        .await;
        assert_eq!(failure(&result).0["code"], "lifecycle.invariant-violation");
        assert_eq!(radio.requests.lock().unwrap().len(), before);
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delayed_disconnect_retains_its_owner_after_same_name_replacement() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("delayed-disconnect-owner").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"connect-old"}).to_string(),
        )
        .await);
        let gate = hold_publication(&session, "disconnect-owner");
        let old_session = session.clone();
        let old = tokio::spawn(async move {
            call(
                &old_session,
                "connection.disconnect",
                &json!({"peerId":POLAR,"lease":"same","operationId":"disconnect-old"}).to_string(),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        radio.set_responder(Box::new(|request| match request {
            crate::RadioRequest::Disconnect { .. } => Reply::Now(crate::RadioCompletion::Failed(
                crate::PlatformFailure::new(crate::FailureKind::Platform, "old reset failure"),
            )),
            request => polar_responder(request),
        }));
        reset_then_power_on(&host).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while lock(&session.state.leases).contains_key(POLAR) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        radio.set_responder(Box::new(polar_responder));
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"connect-new"}).to_string(),
        )
        .await);
        gate.resume.notify_one();
        let cleanup = ok(&old.await.unwrap());
        assert_eq!(cleanup["state"], "release-failed", "{cleanup}");
        assert!(
            cleanup.to_string().contains("old reset failure"),
            "{cleanup}"
        );
        assert_eq!(
            lock(&session.state.leases)
                .get(POLAR)
                .map(|lease| lease.attachment.clone()),
            Some(host.inner.central.attachment())
        );
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reset_late_connect_does_not_shadow_reused_lease_name() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("same-lease").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        let gate = hold_publication(&session, "connect");
        let old_session = session.clone();
        let old = tokio::spawn(async move {
            call(
                &old_session,
                "connection.connect",
                &json!({"peerId":POLAR,"lease":"same","operationId":"old"}).to_string(),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        reset_then_power_on(&host).await;
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"same","operationId":"new"}).to_string(),
        )
        .await);
        gate.resume.notify_one();
        ok(&old.await.unwrap());
        assert_eq!(
            lock(&session.state.leases)
                .get(POLAR)
                .map(|lease| lease.attachment.clone()),
            Some(host.inner.central.attachment()),
            "old terminal shadows the replacement lease"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reset_fences_late_successful_subscription_publication_after_replacement() {
        use crate::test_common::*;
        use serde_json::json;
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let old_session = host.open_session("late-sub").unwrap();
        let replacement = host.open_session("replacement-sub").unwrap();
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: POLAR.into(),
            address: Some(POLAR.into()),
            ..Default::default()
        }));
        ok(&call(
            &old_session,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"old","operationId":"old-connect"}).to_string(),
        )
        .await);
        ok(&call(
            &old_session,
            "gatt.discover",
            &json!({"peerId":POLAR,"lease":"old","operationId":"old-discover"}).to_string(),
        )
        .await);
        let gate = hold_publication(&old_session, "subscribe");
        let task_session = old_session.clone();
        let old = tokio::spawn(async move {
            call(&task_session, "gatt.subscribe", &json!({"peerId":POLAR,"selector":selector(),"consumer":"old-hr","operationId":"old-sub"}).to_string()).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        reset_then_power_on(&host).await;
        ok(&call(
            &replacement,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"new","operationId":"new-connect"}).to_string(),
        )
        .await);
        ok(&call(
            &replacement,
            "gatt.discover",
            &json!({"peerId":POLAR,"lease":"new","operationId":"new-discover"}).to_string(),
        )
        .await);
        ok(&call(&replacement, "gatt.subscribe", &json!({"peerId":POLAR,"selector":selector(),"consumer":"new-hr","operationId":"new-sub"}).to_string()).await);
        gate.resume.notify_one();
        ok(&old.await.unwrap());
        assert!(
            !lock(&old_session.state.subscriptions).contains_key("old-hr"),
            "late old success published a stale subscription"
        );
        let records = parse(&old_session.drain(256, 65536));
        assert!(
            records["records"]
                .as_array()
                .unwrap()
                .iter()
                .any(|record| record["t"] == "stream-end"
                    && record["consumer"] == "old-hr"
                    && record["reason"] == "invalidated"),
            "late successful subscription must expose its reset terminal: {records}"
        );
    }

    #[derive(Default)]
    struct Radio {
        host: OnceLock<Weak<MobileHost>>,
        stops: AtomicU64,
    }
    impl PlatformRadio for Radio {
        fn submit(&self, request: RadioRequest) {
            let completion = match request {
                RadioRequest::AdapterState { .. } => {
                    RadioCompletion::Adapter(crate::AdapterSnapshot {
                        availability: crate::AdapterAvailability::Available,
                        authorization: crate::AdapterAuthorization::Granted,
                        power: crate::AdapterPower::On,
                        safe_reason: None,
                    })
                }
                RadioRequest::StopScan { .. } => {
                    self.stops.fetch_add(1, Ordering::SeqCst);
                    RadioCompletion::Unit
                }
                RadioRequest::StartScan { .. } => RadioCompletion::Unit,
                RadioRequest::Close { .. } => RadioCompletion::Closed(Vec::new()),
                RadioRequest::AssociateCompanion { .. } => return,
                _ => panic!("unexpected radio work in scan-stop race"),
            };
            self.host
                .get()
                .unwrap()
                .upgrade()
                .unwrap()
                .complete(request.id(), completion);
        }
        fn cancel(&self, _: crate::RequestId) {}
    }
    #[derive(Default)]
    struct Wake(tokio::sync::Notify);
    impl WakeSink for Wake {
        fn wake(&self, _: u64) {
            self.0.notify_one();
        }
    }
    async fn invoke(session: &MobileSession, op: &str, args: Value) -> Value {
        let (tx, rx) = tokio::sync::oneshot::channel();
        session.invoke(
            op,
            &args.to_string(),
            Box::new(move |reply| {
                tx.send(reply).unwrap();
            }),
        );
        serde_json::from_str(&rx.await.unwrap()).unwrap()
    }

    include!("session_reclaim_concurrency_tests.rs");

    #[tokio::test]
    async fn marker_reservation_follows_reclaimed_group_without_write() {
        use crate::test_common::{self, Scripted};
        let radio = Scripted::polar();
        let (host, _) = test_common::open(&radio, MobilePlatform::Android).await;
        let first = host.open_session("marker-first").unwrap();
        let second = host.open_session("marker-second").unwrap();
        let before = radio.requests.lock().unwrap().len();
        let reclaim_version = test_common::ok(
            &test_common::call(&first, "gatt.ordered.recording-reclaim.version", "{}").await,
        );
        assert_eq!(reclaim_version["wireVersion"], 0);
        assert_eq!(radio.requests.lock().unwrap().len(), before);
        let version =
            test_common::ok(&test_common::call(&first, "gatt.ordered.marker.version", "{}").await);
        assert_eq!(version["wireVersion"], 0);
        assert_eq!(radio.requests.lock().unwrap().len(), before);
        let (group, capability) = host
            .inner
            .ordered_owners
            .reserve("peer", first.id(), || {
                host.inner.central.reserve_ordered_batch("peer")
            })
            .unwrap();
        let base = serde_json::json!({"peerId":"peer","collectionId":group.to_string(),"reservationId":"marker-a"});
        let gated: Value = serde_json::from_str(
            &test_common::call(&first, "gatt.ordered.marker.reserve", &base.to_string()).await,
        )
        .unwrap();
        assert_eq!(gated["ok"], false);
        assert_eq!(gated["error"]["code"], "capability.unsupported");
        assert_eq!(radio.count(crate::RequestKind::OrderedMarkerWrite), 0);
        host.inner.radio.enable_marker_for_test();
        let before = radio.requests.lock().unwrap().len();
        let reclaim_version = test_common::ok(
            &test_common::call(&first, "gatt.ordered.recording-reclaim.version", "{}").await,
        );
        assert_eq!(reclaim_version["wireVersion"], 2);
        let denied_reclaim = serde_json::json!({"peerId":"peer","collectionId":group.to_string(),"recordingId":"missing","reclaimCapability":"invalid"});
        let refusal = test_common::ok(
            &test_common::call(
                &first,
                "gatt.ordered.recording-reclaim.next",
                &denied_reclaim.to_string(),
            )
            .await,
        );
        assert_eq!(refusal["state"], "not-reserved");
        assert_eq!(refusal["error"]["code"], "ownership.denied");
        let version =
            test_common::ok(&test_common::call(&first, "gatt.ordered.marker.version", "{}").await);
        assert_eq!(version["wireVersion"], 1);
        assert_eq!(radio.requests.lock().unwrap().len(), before);
        let first_reserve = test_common::ok(
            &test_common::call(&first, "gatt.ordered.marker.reserve", &base.to_string()).await,
        );
        let repeated = test_common::ok(
            &test_common::call(&first, "gatt.ordered.marker.reserve", &base.to_string()).await,
        );
        assert_eq!(first_reserve, repeated);
        let handle = first_reserve["handle"].as_str().unwrap();
        let args =
            serde_json::json!({"peerId":"peer","collectionId":group.to_string(),"handle":handle});
        let denied: Value = serde_json::from_str(
            &test_common::call(&second, "gatt.ordered.marker.status", &args.to_string()).await,
        )
        .unwrap();
        assert_eq!(denied["ok"], false);
        let cleanup =
            tokio::time::timeout(std::time::Duration::from_secs(2), first.dispose_failures())
                .await
                .unwrap();
        assert!(
            !cleanup.is_empty(),
            "pending marker retains group cleanup debt"
        );
        let reclaim = serde_json::json!({"peerId":"peer","collectionId":group.to_string(),"reclaimCapability":capability});
        test_common::ok(
            &test_common::call(&second, "gatt.ordered.reclaim", &reclaim.to_string()).await,
        );
        let status = test_common::ok(
            &test_common::call(&second, "gatt.ordered.marker.status", &args.to_string()).await,
        );
        assert_eq!(status["state"], "pending");
        let cancelled = test_common::ok(
            &test_common::call(&second, "gatt.ordered.marker.cancel", &args.to_string()).await,
        );
        assert_eq!(cancelled["state"], "not-admitted");
        assert_eq!(
            cancelled,
            test_common::ok(
                &test_common::call(&second, "gatt.ordered.marker.cancel", &args.to_string()).await
            )
        );
        test_common::ok(
            &test_common::call(&second, "gatt.ordered.marker.forget", &args.to_string()).await,
        );
        let next = serde_json::json!({"peerId":"peer","collectionId":group.to_string(),"reservationId":"marker-b"});
        let reserved = test_common::ok(
            &test_common::call(&second, "gatt.ordered.marker.reserve", &next.to_string()).await,
        );
        let handle = reserved["handle"].as_str().unwrap();
        let args =
            serde_json::json!({"peerId":"peer","collectionId":group.to_string(),"handle":handle});
        let start = serde_json::json!({"peerId":"peer","collectionId":group.to_string(),"handle":handle,"selector":test_common::selector(),"valueB64":"AQ==","mode":"with-response"});
        test_common::ok(
            &test_common::call(&second, "gatt.ordered.marker.start", &start.to_string()).await,
        );
        let failed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let status = test_common::ok(
                    &test_common::call(&second, "gatt.ordered.marker.status", &args.to_string())
                        .await,
                );
                if status["state"] != "pending" {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(failed["state"], "not-admitted");
        assert_eq!(
            failed,
            test_common::ok(
                &test_common::call(&second, "gatt.ordered.marker.start", &start.to_string()).await
            )
        );
        test_common::ok(
            &test_common::call(&second, "gatt.ordered.marker.forget", &args.to_string()).await,
        );
        assert_eq!(radio.count(crate::RequestKind::OrderedMarkerWrite), 0);
    }

    #[tokio::test]
    async fn session_marker_write_requires_fifo_ack_before_forget() {
        use crate::test_common::{self, Reply, Scripted};
        use std::sync::atomic::AtomicUsize;
        let records = Arc::new(Mutex::new(Vec::<crate::OrderedRecord>::new()));
        let writes = Arc::new(AtomicUsize::new(0));
        let source = Arc::clone(&records);
        let observed = Arc::clone(&writes);
        let member = crate::OrderedRegistration {
            member_id: 0,
            source_member: 11,
            epoch: 2,
        };
        let radio = Scripted::new(Box::new(move |request| match request {
            RadioRequest::Discover { .. } => {
                let mut services = test_common::polar_services();
                services[0].characteristics[0].properties.write = true;
                Reply::Now(RadioCompletion::DiscoveredScoped {
                    services,
                    identity: ubm_desktop::boundary::GattSnapshotIdentity {
                        owner: "graph".into(),
                        attachment: 1,
                        revision: 1,
                    },
                })
            }
            RadioRequest::OrderedOpen { .. } => Reply::Now(RadioCompletion::OrderedOpened {
                generation: crate::OrderedSourceGeneration::Counter(7),
                members: vec![member],
            }),
            RadioRequest::OrderedRetainClaim {
                generation, member, ..
            } => Reply::Now(RadioCompletion::OrderedClaim {
                generation: generation.clone(),
                member: *member,
            }),
            RadioRequest::OrderedEnableNotifications { .. } => Reply::Now(
                RadioCompletion::NotifyEnabled(ObservedDelivery::Notification),
            ),
            RadioRequest::OrderedMarkerWrite { marker_id, .. } => {
                observed.fetch_add(1, Ordering::SeqCst);
                lock(&source).push(crate::OrderedRecord::Marker {
                    marker_id: *marker_id,
                    ordinal: 1,
                });
                Reply::Now(RadioCompletion::OrderedMarkerWriteStatus(
                    crate::OrderedMarkerWriteStatus {
                        admission: crate::OrderedMarkerAdmission::Admitted { ordinal: 1 },
                        write: crate::OrderedMarkerWriteState::Settled { failure: None },
                    },
                ))
            }
            RadioRequest::OrderedCancelMarkerWrite { .. }
            | RadioRequest::OrderedMarkerWriteSettlement { .. } => Reply::Now(
                RadioCompletion::OrderedMarkerWriteStatus(crate::OrderedMarkerWriteStatus {
                    admission: crate::OrderedMarkerAdmission::Admitted { ordinal: 1 },
                    write: crate::OrderedMarkerWriteState::Settled { failure: None },
                }),
            ),
            RadioRequest::OrderedDrain { .. } => {
                let values = std::mem::take(&mut *lock(&source));
                Reply::Now(RadioCompletion::OrderedRecords {
                    batch_id: (!values.is_empty()).then_some(1),
                    records: values,
                })
            }
            _ => test_common::polar_responder(request),
        }));
        let (host, _) = test_common::open(&radio, MobilePlatform::Android).await;
        host.inner.radio.enable_marker_for_test();
        let session = host.open_session("marker-fifo").unwrap();
        test_common::ok(
            &test_common::call(
                &session,
                "scan.start",
                r#"{"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}"#,
            )
            .await,
        );
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: test_common::POLAR.into(),
            service_uuids: vec!["180D".into()],
            ..crate::Advertisement::default()
        }));
        test_common::drain_until(&session, |records| {
            !test_common::of_type(records, "adv").is_empty()
        })
        .await;
        let connected = test_common::ok(&test_common::call(&session, "connection.connect", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"connect"}).to_string()).await);
        let discovered = test_common::ok(&test_common::call(&session, "gatt.discover", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"discover"}).to_string()).await);
        let reserved = test_common::ok(
            &test_common::call(
                &session,
                "gatt.ordered.reserve",
                &serde_json::json!({"peerId":test_common::POLAR}).to_string(),
            )
            .await,
        );
        let group = reserved["collectionId"].as_str().unwrap();
        let open = serde_json::json!({"wireVersion":2,"peerId":test_common::POLAR,"collectionId":group,"connectionGeneration":connected["connectionGeneration"],"databaseGeneration":discovered["databaseGeneration"],"publicConnectionGeneration":"public-connection","publicDatabaseGeneration":"public-database","members":[{"memberId":"0","publicMemberId":"a","selector":test_common::selector(),"requestedDelivery":"prefer-notification"}],"maxItems":8,"maxBytes":128,"maxControls":8,"operationId":"open"});
        test_common::ok(&test_common::call(&session, "gatt.ordered.open", &open.to_string()).await);
        let reservation = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"reservationId":"exact"});
        let reserved = test_common::ok(
            &test_common::call(
                &session,
                "gatt.ordered.marker.reserve",
                &reservation.to_string(),
            )
            .await,
        );
        let handle = reserved["handle"].as_str().unwrap();
        let marker =
            serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"handle":handle});
        let start = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"handle":handle,"selector":test_common::selector(),"valueB64":"AQ==","mode":"with-response"});
        test_common::ok(
            &test_common::call(&session, "gatt.ordered.marker.start", &start.to_string()).await,
        );
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let status = test_common::ok(
                    &test_common::call(&session, "gatt.ordered.marker.status", &marker.to_string())
                        .await,
                );
                if status["state"] != "pending" {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(status["state"], "admitted");
        assert_eq!(
            status,
            test_common::ok(
                &test_common::call(&session, "gatt.ordered.marker.start", &start.to_string()).await
            )
        );
        assert_eq!(
            status,
            test_common::ok(
                &test_common::call(&session, "gatt.ordered.marker.cancel", &marker.to_string())
                    .await
            )
        );
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        let refused: Value = serde_json::from_str(
            &test_common::call(&session, "gatt.ordered.marker.forget", &marker.to_string()).await,
        )
        .unwrap();
        assert_eq!(refused["ok"], false);
        host.ingest(crate::RadioIngress::OrderedWake {
            generation: crate::OrderedSourceGeneration::Counter(7),
            group_id: group.parse().unwrap(),
        });
        let group_args = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group});
        let delivery = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let delivery = test_common::ok(
                    &test_common::call(&session, "gatt.ordered.poll", &group_args.to_string())
                        .await,
                );
                if !delivery["deliveryId"].is_null() {
                    break delivery;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let record: Value = serde_json::from_str(delivery["record"].as_str().unwrap()).unwrap();
        assert_eq!(record["kind"], "marker");
        test_common::ok(&test_common::call(&session, "gatt.ordered.ack-delivery", &serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"deliveryId":delivery["deliveryId"]}).to_string()).await);
        test_common::ok(
            &test_common::call(&session, "gatt.ordered.marker.forget", &marker.to_string()).await,
        );
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retired_recording_handles_free_capacity_and_reject_foreign_or_future() {
        let radio = crate::test_common::Scripted::polar();
        let (host, _) = crate::test_common::open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("receipt-owner").unwrap();
        let other = host.open_session("receipt-other").unwrap();
        let mut handles = Vec::new();
        for index in 0..128 {
            handles.push(
                session
                    .reserve_ordered_recording_start(
                        &serde_json::json!([
                            format!("recording-{index}"),
                            format!("recording-{index}")
                        ])
                        .to_string(),
                    )
                    .unwrap()["handle"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        assert!(
            session
                .reserve_ordered_recording_start(&serde_json::json!(["full", "full"]).to_string())
                .is_err()
        );
        assert!(other.ordered_recording_start_status(&handles[0]).is_err());
        assert!(
            other
                .retire_ordered_recording_start(&handles[0])
                .await
                .is_err()
        );
        for handle in handles.iter().rev() {
            session
                .retire_ordered_recording_start(handle)
                .await
                .unwrap();
        }
        for handle in &handles {
            assert_eq!(
                session
                    .retire_ordered_recording_start(handle)
                    .await
                    .unwrap()["state"],
                "retired"
            );
            assert_eq!(
                session.ordered_recording_start_status(handle).unwrap()["state"],
                "retired"
            );
        }
        for index in 0..150 {
            let handle = session
                .reserve_ordered_recording_start(
                    &serde_json::json!([format!("next-{index}"), format!("next-{index}")])
                        .to_string(),
                )
                .unwrap()["handle"]
                .as_str()
                .unwrap()
                .to_owned();
            session
                .retire_ordered_recording_start(&handle)
                .await
                .unwrap();
        }
        let future = format!("{}:{}", session.state.instance, 1000);
        assert!(session.ordered_recording_start_status(&future).is_err());
        assert!(
            session
                .retire_ordered_recording_start(&future)
                .await
                .is_err()
        );
        assert!(lock(&host.inner.ordered_start_receipts).is_empty());
        let first = session
            .reserve_ordered_recording_start(&serde_json::json!(["first", "first"]).to_string())
            .unwrap()["handle"]
            .as_str()
            .unwrap()
            .to_owned();
        let second = session
            .reserve_ordered_recording_start(&serde_json::json!(["second", "second"]).to_string())
            .unwrap()["handle"]
            .as_str()
            .unwrap()
            .to_owned();
        let declaration = ubm_desktop::ordered_recording::OrderedRecordingDeclaration {
            peer_id: "peer".into(),
            connection_generation: "connection".into(),
            database_generation: "database".into(),
            members: vec![ubm_desktop::ordered_recording::OrderedRecordingMember {
                member_id: 0,
                public_member_id: "member".into(),
                selector: DesktopCentral::<crate::foreign::ForeignRadio>::selector(
                    "180D",
                    Some(0),
                    Some("2A37"),
                    Some(0),
                    None,
                    None,
                )
                .unwrap(),
                delivery: None,
                preference: None,
            }],
            max_items: 8,
            max_bytes: 128,
            max_controls: 8,
        };
        let quota = ubm_desktop::continuation_journal::JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        for (handle, id) in [(&second, "second"), (&first, "first")] {
            let answer = session
                .start_ordered_recording(
                    handle.clone(),
                    id.into(),
                    quota,
                    declaration.clone(),
                    OpControl::budget_ms(5000),
                )
                .await
                .unwrap();
            assert_eq!(answer["kind"], "prepare-failed");
            assert_eq!(
                session.ordered_recording_start_status(handle).unwrap()["state"],
                "ready"
            );
            session
                .retire_ordered_recording_start(handle)
                .await
                .unwrap();
        }
        *lock(&session.state.recording_start_last) = Some(u64::MAX);
        assert!(
            session
                .reserve_ordered_recording_start(
                    &serde_json::json!(["overflow", "overflow"]).to_string()
                )
                .is_err()
        );
        assert!(lock(&host.inner.ordered_start_receipts).is_empty());
        assert!(session.dispose_failures().await.is_empty());
        assert!(other.dispose_failures().await.is_empty());
        let _ = host.shutdown().await;
    }

    #[tokio::test]
    async fn dispose_preserves_session_lease_while_recorder_open_is_pending() {
        use crate::test_common::{self, Reply, Scripted};
        use ubm_desktop::continuation_journal::JournalQuota;
        use ubm_desktop::ordered_recording::{OrderedRecordingDeclaration, OrderedRecordingMember};
        let radio = Scripted::new(Box::new(|request| match request {
            RadioRequest::Discover { .. } => Reply::Now(RadioCompletion::DiscoveredScoped {
                services: test_common::polar_services(),
                identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "graph".into(),
                    attachment: 1,
                    revision: 1,
                },
            }),
            RadioRequest::OrderedOpen { .. } => Reply::Hold,
            _ => test_common::polar_responder(request),
        }));
        let (host, _) = test_common::open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("recorder-pending-dispose").unwrap();
        test_common::ok(
            &test_common::call(
                &session,
                "scan.start",
                r#"{"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}"#,
            )
            .await,
        );
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: test_common::POLAR.into(),
            service_uuids: vec!["180D".into()],
            ..crate::Advertisement::default()
        }));
        test_common::drain_until(&session, |records| {
            !test_common::of_type(records, "adv").is_empty()
        })
        .await;
        let connected = test_common::ok(&test_common::call(&session, "connection.connect", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"connect"}).to_string()).await);
        let discovered = test_common::ok(&test_common::call(&session, "gatt.discover", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"discover"}).to_string()).await);
        let path = std::env::temp_dir().join(format!(
            "ubm-mobile-pending-dispose-{}-{}",
            std::process::id(),
            session.id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        host.inner
            .recording_registry
            .configure_directory(&path)
            .unwrap();
        let selector = DesktopCentral::<crate::foreign::ForeignRadio>::selector(
            "180D",
            Some(0),
            Some("2A37"),
            Some(0),
            None,
            None,
        )
        .unwrap();
        let declaration = OrderedRecordingDeclaration {
            peer_id: test_common::POLAR.into(),
            connection_generation: connected["connectionGeneration"].as_str().unwrap().into(),
            database_generation: discovered["databaseGeneration"].as_str().unwrap().into(),
            members: vec![OrderedRecordingMember {
                member_id: 0,
                public_member_id: "heart-rate".into(),
                selector,
                delivery: None,
                preference: None,
            }],
            max_items: 8,
            max_bytes: 128,
            max_controls: 8,
        };
        let reserved = session
            .reserve_ordered_recording_start(
                &serde_json::json!(["pending-dispose", "pending-dispose"]).to_string(),
            )
            .unwrap();
        let handle = reserved["handle"].as_str().unwrap().to_owned();
        assert_eq!(
            reserved,
            session
                .reserve_ordered_recording_start(
                    &serde_json::json!(["pending-dispose", "pending-dispose"]).to_string()
                )
                .unwrap()
        );
        let task = {
            let session = session.clone();
            let handle = handle.clone();
            tokio::spawn(async move {
                session
                    .start_ordered_recording(
                        handle,
                        "pending-dispose".into(),
                        JournalQuota {
                            max_bytes: 1 << 20,
                            max_records: 100,
                        },
                        declaration,
                        OpControl::budget_ms(5000),
                    )
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while radio.held_of(crate::RequestKind::OrderedOpen).is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let failures = session.dispose_failures().await;
        assert!(
            !failures.is_empty(),
            "pending recorder handoff must retain cleanup debt"
        );
        assert!(lock(&session.state.leases).contains_key(test_common::POLAR));
        let id = radio.held_of(crate::RequestKind::OrderedOpen)[0];
        radio.answer(
            id,
            RadioCompletion::OrderedOpenFailed {
                failure: crate::PlatformFailure::new(crate::FailureKind::GattStatus, "injected"),
                ownership: crate::OrderedOpenOwnership::NotCreated,
            },
        );
        let _ = task.await;
        for _ in 0..100 {
            if session.dispose_failures().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!lock(&session.state.leases).contains_key(test_common::POLAR));
        let _ = host.shutdown().await;
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn direct_terminal_close_revokes_start_receipt() {
        use crate::test_common::{self, Reply, Scripted};
        use ubm_desktop::continuation_journal::JournalQuota;
        use ubm_desktop::ordered_recording::{OrderedRecordingDeclaration, OrderedRecordingMember};
        let radio = Scripted::new(Box::new(|request| match request {
            RadioRequest::Discover { .. } => Reply::Now(RadioCompletion::DiscoveredScoped {
                services: test_common::polar_services(),
                identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "graph".into(),
                    attachment: 1,
                    revision: 1,
                },
            }),
            RadioRequest::OrderedOpen { .. } => Reply::Hold,
            _ => test_common::polar_responder(request),
        }));
        let (host, _) = test_common::open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("direct-close-revoke").unwrap();
        test_common::ok(
            &test_common::call(
                &session,
                "scan.start",
                r#"{"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}"#,
            )
            .await,
        );
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: test_common::POLAR.into(),
            service_uuids: vec!["180D".into()],
            ..crate::Advertisement::default()
        }));
        test_common::drain_until(&session, |records| {
            !test_common::of_type(records, "adv").is_empty()
        })
        .await;
        let connected = test_common::ok(&test_common::call(&session, "connection.connect", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"connect"}).to_string()).await);
        let discovered = test_common::ok(&test_common::call(&session, "gatt.discover", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"discover"}).to_string()).await);
        let path = std::env::temp_dir().join(format!(
            "ubm-mobile-recorder-direct-close-{}-{}",
            std::process::id(),
            session.id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        host.inner
            .recording_registry
            .configure_directory(&path)
            .unwrap();
        let selector = DesktopCentral::<crate::foreign::ForeignRadio>::selector(
            "180D",
            Some(0),
            Some("2A37"),
            Some(0),
            None,
            None,
        )
        .unwrap();
        let declaration = OrderedRecordingDeclaration {
            peer_id: test_common::POLAR.into(),
            connection_generation: connected["connectionGeneration"].as_str().unwrap().into(),
            database_generation: discovered["databaseGeneration"].as_str().unwrap().into(),
            members: vec![OrderedRecordingMember {
                member_id: 0,
                public_member_id: "heart-rate".into(),
                selector,
                delivery: None,
                preference: None,
            }],
            max_items: 8,
            max_bytes: 128,
            max_controls: 8,
        };
        let reservation = serde_json::json!(["direct-close", "caller"]).to_string();
        let handle = session
            .reserve_ordered_recording_start(&reservation)
            .unwrap()["handle"]
            .as_str()
            .unwrap()
            .to_owned();
        let owner = ubm_desktop::ordered_recording::OrderedRecording::prepare(
            Arc::new(host.inner.central.clone()),
            Arc::clone(&host.inner.recording_registry),
            "direct-close".into(),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 100,
            },
            declaration,
        )
        .await
        .unwrap();
        host.inner
            .ordered_owners
            .reserve(test_common::POLAR, session.state.id, || {
                Ok(owner.collection_id())
            })
            .unwrap();
        lock(&host.inner.ordered_recordings).insert(
            (session.state.id, "direct-close".into()),
            Arc::clone(&owner),
        );
        {
            let sequence = session.recording_handle(&handle).unwrap();
            let mut receipts = lock(&host.inner.ordered_start_receipts);
            let entry = receipts.get_mut(&(session.state.id, sequence)).unwrap();
            entry.owner_collection = Some(owner.collection_id());
            entry.result = Some(serde_json::json!({"kind":"started"}));
        }
        let previous = host.open_session("previous-owner").unwrap();
        let previous_handle = previous
            .reserve_ordered_recording_start(&reservation)
            .unwrap()["handle"]
            .as_str()
            .unwrap()
            .to_owned();
        {
            let sequence = previous.recording_handle(&previous_handle).unwrap();
            let mut receipts = lock(&host.inner.ordered_start_receipts);
            let entry = receipts.get_mut(&(previous.state.id, sequence)).unwrap();
            entry.owner_collection = Some(owner.collection_id());
            entry.result = Some(serde_json::json!({"kind":"started"}));
        }
        assert!(matches!(
            session.close_ordered_recording("direct-close").await,
            Ok(ubm_desktop::ordered_recording::RecordingDriverStatus::Finished)
        ));
        assert_eq!(
            session.ordered_recording_start_status(&handle).unwrap()["state"],
            "revoked"
        );
        assert_eq!(
            previous
                .ordered_recording_start_status(&previous_handle)
                .unwrap()["state"],
            "revoked"
        );
        previous
            .retire_ordered_recording_start(&previous_handle)
            .await
            .unwrap();
        assert!(
            !lock(&host.inner.ordered_recordings)
                .contains_key(&(session.state.id, "direct-close".into())),
            "retirement follows the exact owner after transfer"
        );
        let _ = session.dispose_failures().await;
        let _ = host.shutdown().await;
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn failed_prepared_owner_can_retire_and_dispose() {
        use crate::test_common::{self, Reply, Scripted};
        use ubm_desktop::continuation_journal::JournalQuota;
        use ubm_desktop::ordered_recording::{OrderedRecordingDeclaration, OrderedRecordingMember};
        let radio = Scripted::new(Box::new(|request| match request {
            RadioRequest::Discover { .. } => Reply::Now(RadioCompletion::DiscoveredScoped {
                services: test_common::polar_services(),
                identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "graph".into(),
                    attachment: 1,
                    revision: 1,
                },
            }),
            RadioRequest::OrderedOpen { .. } => Reply::Hold,
            _ => test_common::polar_responder(request),
        }));
        let (host, _) = test_common::open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("prepared-owner-failure").unwrap();
        test_common::ok(
            &test_common::call(
                &session,
                "scan.start",
                r#"{"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}"#,
            )
            .await,
        );
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: test_common::POLAR.into(),
            service_uuids: vec!["180D".into()],
            ..crate::Advertisement::default()
        }));
        test_common::drain_until(&session, |records| {
            !test_common::of_type(records, "adv").is_empty()
        })
        .await;
        let connected = test_common::ok(&test_common::call(&session, "connection.connect", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"connect"}).to_string()).await);
        let discovered = test_common::ok(&test_common::call(&session, "gatt.discover", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"discover"}).to_string()).await);
        let path = std::env::temp_dir().join(format!(
            "ubm-mobile-recorder-prepared-owner-{}-{}",
            std::process::id(),
            session.id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        host.inner
            .recording_registry
            .configure_directory(&path)
            .unwrap();
        let selector = DesktopCentral::<crate::foreign::ForeignRadio>::selector(
            "180D",
            Some(0),
            Some("2A37"),
            Some(0),
            None,
            None,
        )
        .unwrap();
        let declaration = OrderedRecordingDeclaration {
            peer_id: test_common::POLAR.into(),
            connection_generation: connected["connectionGeneration"].as_str().unwrap().into(),
            database_generation: discovered["databaseGeneration"].as_str().unwrap().into(),
            members: vec![OrderedRecordingMember {
                member_id: 0,
                public_member_id: "heart-rate".into(),
                selector,
                delivery: None,
                preference: None,
            }],
            max_items: 8,
            max_bytes: 128,
            max_controls: 8,
        };
        for index in 0..128 {
            host.inner
                .ordered_owners
                .reserve(test_common::POLAR, 999, || Ok(10000 + index))
                .unwrap();
        }
        let handle = session
            .reserve_ordered_recording_start(
                &serde_json::json!(["unopened", "unopened"]).to_string(),
            )
            .unwrap()["handle"]
            .as_str()
            .unwrap()
            .to_owned();
        let result = session
            .start_ordered_recording(
                handle.clone(),
                "unopened".into(),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 100,
                },
                declaration,
                OpControl::budget_ms(5000),
            )
            .await
            .unwrap();
        assert_ne!(result["kind"], "started");
        session
            .retire_ordered_recording_start(&handle)
            .await
            .unwrap();
        session
            .retire_ordered_recording_start(&handle)
            .await
            .unwrap();
        assert!(session.dispose_failures().await.is_empty());
        assert!(session.dispose_failures().await.is_empty());
        let _ = host.shutdown().await;
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn recording_reservations_distinguish_callers() {
        let radio = crate::test_common::Scripted::polar();
        let (host, _) = crate::test_common::open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("reservation-callers").unwrap();
        let first = r#"["capture","caller-1"]"#;
        let second = r#"["capture","caller-2"]"#;
        let one = session.reserve_ordered_recording_start(first).unwrap();
        assert_eq!(one, session.reserve_ordered_recording_start(first).unwrap());
        let two = session.reserve_ordered_recording_start(second).unwrap();
        assert_ne!(one["handle"], two["handle"]);
        session
            .retire_ordered_recording_start(one["handle"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(
            session
                .ordered_recording_start_status(two["handle"].as_str().unwrap())
                .unwrap()["state"],
            "reserved"
        );
    }

    #[tokio::test]
    async fn cancelled_recording_start_retains_supervised_owner() {
        use crate::test_common::{self, Reply, Scripted};
        use ubm_desktop::continuation_journal::JournalQuota;
        use ubm_desktop::ordered_recording::{OrderedRecordingDeclaration, OrderedRecordingMember};
        let radio = Scripted::new(Box::new(|request| match request {
            RadioRequest::Discover { .. } => Reply::Now(RadioCompletion::DiscoveredScoped {
                services: test_common::polar_services(),
                identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "graph".into(),
                    attachment: 1,
                    revision: 1,
                },
            }),
            RadioRequest::OrderedOpen { .. } => Reply::Hold,
            _ => test_common::polar_responder(request),
        }));
        let (host, _) = test_common::open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("recorder-cancel").unwrap();
        test_common::ok(
            &test_common::call(
                &session,
                "scan.start",
                r#"{"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}"#,
            )
            .await,
        );
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: test_common::POLAR.into(),
            service_uuids: vec!["180D".into()],
            ..crate::Advertisement::default()
        }));
        test_common::drain_until(&session, |records| {
            !test_common::of_type(records, "adv").is_empty()
        })
        .await;
        let connected = test_common::ok(&test_common::call(&session, "connection.connect", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"connect"}).to_string()).await);
        let discovered = test_common::ok(&test_common::call(&session, "gatt.discover", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"discover"}).to_string()).await);
        let path = std::env::temp_dir().join(format!(
            "ubm-mobile-recorder-cancelled-start-{}-{}",
            std::process::id(),
            session.id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        host.inner
            .recording_registry
            .configure_directory(&path)
            .unwrap();
        let selector = DesktopCentral::<crate::foreign::ForeignRadio>::selector(
            "180D",
            Some(0),
            Some("2A37"),
            Some(0),
            None,
            None,
        )
        .unwrap();
        let declaration = OrderedRecordingDeclaration {
            peer_id: test_common::POLAR.into(),
            connection_generation: connected["connectionGeneration"].as_str().unwrap().into(),
            database_generation: discovered["databaseGeneration"].as_str().unwrap().into(),
            members: vec![OrderedRecordingMember {
                member_id: 0,
                public_member_id: "heart-rate".into(),
                selector,
                delivery: None,
                preference: None,
            }],
            max_items: 8,
            max_bytes: 128,
            max_controls: 8,
        };
        let retry_declaration = declaration.clone();
        let reserved = session
            .reserve_ordered_recording_start(
                &serde_json::json!(["cancelled-start", "cancelled-start"]).to_string(),
            )
            .unwrap();
        let handle = reserved["handle"].as_str().unwrap().to_owned();
        assert_eq!(
            reserved,
            session
                .reserve_ordered_recording_start(
                    &serde_json::json!(["cancelled-start", "cancelled-start"]).to_string()
                )
                .unwrap()
        );
        let task = {
            let session = session.clone();
            let handle = handle.clone();
            tokio::spawn(async move {
                session
                    .start_ordered_recording(
                        handle,
                        "cancelled-start".into(),
                        JournalQuota {
                            max_bytes: 1 << 20,
                            max_records: 100,
                        },
                        declaration,
                        OpControl::budget_ms(5000),
                    )
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while radio.held_of(crate::RequestKind::OrderedOpen).is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert_eq!(
            session.ordered_recording_start_status(&handle).unwrap()["state"],
            "pending"
        );
        assert!(
            session
                .retire_ordered_recording_start(&handle)
                .await
                .is_err()
        );
        assert!(
            session
                .ordered_recording_status("cancelled-start")
                .is_some()
        );
        assert!(lock(&session.state.leases).contains_key(test_common::POLAR));
        let other = host.open_session("other-recorder").unwrap();
        assert!(other.ordered_recording_start_status(&handle).is_err());
        assert!(other.retire_ordered_recording_start(&handle).await.is_err());
        for context in [false, true] {
            let (tx, rx) = tokio::sync::oneshot::channel();
            other.ordered_recording_control_wire(
                "cancelled-start".into(),
                context,
                Box::new(move |answer| {
                    let _ = tx.send(answer);
                }),
            );
            let answer: Value = serde_json::from_str(&rx.await.unwrap()).unwrap();
            assert_eq!(answer["ok"], false);
            assert_eq!(answer["error"]["code"], "ownership.denied");
        }
        let id = radio.held_of(crate::RequestKind::OrderedOpen)[0];
        radio.answer(
            id,
            RadioCompletion::OrderedOpenFailed {
                failure: crate::PlatformFailure::new(crate::FailureKind::GattStatus, "injected"),
                ownership: crate::OrderedOpenOwnership::NotCreated,
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let _guard = session.state.release_guard.lock().await;
        })
        .await
        .unwrap();
        assert!(
            session
                .ordered_recording_status("cancelled-start")
                .is_some()
        );
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        let receipt = session
            .start_ordered_recording(
                handle.clone(),
                "cancelled-start".into(),
                quota,
                retry_declaration.clone(),
                OpControl::budget_ms(5000),
            )
            .await
            .unwrap();
        assert_eq!(receipt["kind"], "started");
        assert_eq!(receipt["receipt"]["state"], "setup-failed");
        assert_eq!(radio.count(crate::RequestKind::OrderedOpen), 1);
        let repeated = session
            .start_ordered_recording(
                handle.clone(),
                "cancelled-start".into(),
                quota,
                retry_declaration.clone(),
                OpControl::budget_ms(5000),
            )
            .await
            .unwrap();
        assert_eq!(receipt, repeated);
        let mut altered = retry_declaration.clone();
        altered.members[0].public_member_id = "other".into();
        assert!(
            session
                .start_ordered_recording(
                    handle.clone(),
                    "cancelled-start".into(),
                    quota,
                    altered,
                    OpControl::budget_ms(5000)
                )
                .await
                .is_err()
        );
        assert_eq!(radio.count(crate::RequestKind::OrderedOpen), 1);
        assert_eq!(
            session.ordered_recording_start_status(&handle).unwrap()["state"],
            "ready"
        );
        // The native start receipt belongs to this collection even if a
        // reclaim has moved the live supervisor to another session.
        let owner = {
            let mut entries = lock(&host.inner.ordered_recordings);
            let owner = entries
                .remove(&(session.state.id, "cancelled-start".into()))
                .unwrap();
            entries.insert(
                (other.state.id, "cancelled-start".into()),
                Arc::clone(&owner),
            );
            owner
        };
        session
            .retire_ordered_recording_start(&handle)
            .await
            .unwrap();
        assert_eq!(owner.start_status()["state"], "retired");
        assert!(
            lock(&host.inner.ordered_recordings)
                .contains_key(&(other.state.id, "cancelled-start".into()))
        );
        {
            let mut entries = lock(&host.inner.ordered_recordings);
            entries.remove(&(other.state.id, "cancelled-start".into()));
            entries.insert((session.state.id, "cancelled-start".into()), owner);
        }
        session
            .retire_ordered_recording_start(&handle)
            .await
            .unwrap();
        assert_eq!(
            session.ordered_recording_start_status(&handle).unwrap()["state"],
            "retired"
        );
        assert!(
            session
                .ordered_recording_status("cancelled-start")
                .is_some()
        );
        assert!(
            session
                .start_ordered_recording(
                    handle.clone(),
                    "cancelled-start".into(),
                    quota,
                    retry_declaration,
                    OpControl::budget_ms(5000)
                )
                .await
                .is_err()
        );
        assert_eq!(radio.count(crate::RequestKind::OrderedOpen), 1);
        let (tx, rx) = tokio::sync::oneshot::channel();
        session.ordered_recording_control_wire(
            "cancelled-start".into(),
            false,
            Box::new(move |answer| {
                let _ = tx.send(answer);
            }),
        );
        let stopped: Value = serde_json::from_str(&rx.await.unwrap()).unwrap();
        assert_eq!(stopped["ok"], true);
        assert!(stopped["value"]["hostCleanup"]["state"].is_string());
        assert!(lock(&session.state.leases).contains_key(test_common::POLAR));
        assert!(
            host.session(other.id()).is_some(),
            "stop must not dispose another session"
        );
        test_common::ok(&test_common::call(&other, "adapter.state", "{}").await);
        let mut failures = session.dispose_failures().await;
        for _ in 0..50 {
            if failures.is_empty() {
                break;
            }
            assert!(lock(&session.state.leases).contains_key(test_common::POLAR));
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            failures = session.dispose_failures().await;
        }
        assert!(
            failures.is_empty(),
            "failed source setup remains recoverable: {failures:?}"
        );
        assert!(
            session
                .ordered_recording_status("cancelled-start")
                .is_none()
        );
        assert_eq!(
            serde_json::from_str::<Value>(&host.shutdown().await).unwrap()["state"],
            "released"
        );
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test(start_paused = true)]
    async fn dispose_retries_unconfirmed_companion_ui_without_releasing_another_session() {
        let radio = Arc::new(Radio::default());
        let host = Arc::new(
            MobileHost::open(
                radio.clone(),
                Arc::new(Wake::default()),
                HostOptions {
                    platform: MobilePlatform::Android,
                    owner: "chooser-release".into(),
                    adapter_label: "test".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        radio.host.set(Arc::downgrade(&host)).unwrap();
        let session = host.open_session("chooser").unwrap();
        let other = host.open_session("unrelated").unwrap();
        let mut request_id = None;
        let mut choice = Box::pin(session.host.radio.call_owned_companion(session.id(), |id| {
            request_id = Some(id);
            RadioRequest::AssociateCompanion {
                id,
                name: None,
                service_uuid: None,
                filters_json: None,
            }
        }));
        std::future::poll_fn(|cx| {
            assert!(choice.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(choice);
        let failed = session.dispose_failures().await;
        assert_eq!(failed.len(), 1);
        assert!(other.dispose_failures().await.is_empty());
        assert_eq!(
            session.dispose_failures().await.len(),
            1,
            "unconfirmed UI remains retryable"
        );
        host.complete(
            request_id.expect("the companion request was dispatched"),
            RadioCompletion::Failed(crate::PlatformFailure::new(
                crate::FailureKind::Cancelled,
                "UI closed after held queue drained",
            )),
        );
        assert!(session.dispose_failures().await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn accepted_stop_reports_retired_membership_when_expiry_wins_its_lock_wait() {
        let radio = Arc::new(Radio::default());
        let wake = Arc::new(Wake::default());
        let host = Arc::new(
            MobileHost::open(
                radio.clone(),
                wake.clone(),
                HostOptions {
                    platform: MobilePlatform::Android,
                    owner: "stop-race".into(),
                    adapter_label: "test".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        radio.host.set(Arc::downgrade(&host)).unwrap();
        let session = host.open_session("race").unwrap();
        let started = invoke(&session, "scan.start", serde_json::json!({
            "operationId":"start", "admission":1, "serviceUuids":[], "duplicatePolicy":"all", "lifetimeMs":1000
        })).await;
        assert_eq!(started["ok"], true, "{started}");
        let membership = started["value"]["operationId"].as_str().unwrap().to_owned();
        let scan_lock = host.inner.scan.lock().await;
        let mut stop = Box::pin(session.execute(
            Body::ScanStop(membership.clone()),
            OpControl::unbounded(),
            0,
        ));
        std::future::poll_fn(|cx| {
            assert!(
                stop.as_mut().poll(cx).is_pending(),
                "stop must reach the held scan lock"
            );
            std::task::Poll::Ready(())
        })
        .await;
        tokio::time::advance(std::time::Duration::from_millis(1001)).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let notified = wake.0.notified();
                let drained: Value = serde_json::from_str(&session.drain(256, 65536)).unwrap();
                if drained["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|record| record["t"] == "scan-end" && record["operationId"] == membership)
                {
                    break;
                }
                notified.await;
            }
        })
        .await
        .unwrap();
        assert_eq!(*lock(&session.state.scan), ScanSlot::Idle);
        drop(scan_lock);
        assert_eq!(stop.await.unwrap()["state"], "released");
        let stale = invoke(
            &session,
            "scan.stop",
            serde_json::json!({"operationId":membership}),
        )
        .await;
        assert_eq!(stale["error"]["code"], "lifecycle.invalid-state");
        let replacement = invoke(
            &session,
            "scan.start",
            serde_json::json!({
                "operationId":"next", "admission":2, "serviceUuids":[], "duplicatePolicy":"all"
            }),
        )
        .await;
        assert_eq!(replacement["ok"], true, "{replacement}");
        assert_ne!(
            replacement["value"]["operationId"],
            started["value"]["operationId"]
        );
        assert_eq!(radio.stops.load(Ordering::SeqCst), 1);
        let ended: Value = serde_json::from_str(&host.shutdown().await).unwrap();
        assert_eq!(ended["state"], "released", "{ended}");
        assert_eq!(radio.stops.load(Ordering::SeqCst), 2);
    }
}
