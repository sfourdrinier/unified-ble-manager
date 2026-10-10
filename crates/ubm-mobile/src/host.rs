//! [`MobileHost`]: the process-owned mobile radio owner.
//!
//! One platform radio and one [`DesktopCentral<ForeignRadio>`] per process,
//! on the shared executor; every RN manager is a [`MobileSession`] lease on
//! it (decision 4). One owner is what state restoration needs (the OS
//! hands restored peripherals to the process's one central, and the core
//! adopts them through the ordinary connect path), what several managers
//! sharing the platform radio need (one CoreBluetooth delegate, one
//! Android GATT owner), and what background operation needs (the owner
//! outlives any JS manager).
//!
//! Signals from the central (advertisements, values, lifecycle) and
//! host-level platform facts (adapter, security, restoration, scan
//! failure, ingress drops) enter one ordered queue; a single pump task
//! routes them to the sessions that hold the resource, so every session's
//! drain order matches the order the owner observed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use ubm_core::central::{Central, ConnectionState, PathSelector, canonical_uuid};
use ubm_core::contracts::{AttachmentTuple, BleErrorCode, BleErrorDomain, CoreError, OperationId};
use ubm_desktop::{
    Budget, CentralProfile, CentralSignal, DesktopCentral, DesktopError, InstanceKey,
    LifecycleEvent, LifecycleKind, NotificationPoll, OpControl, OpTicket, PeerRecord, PeerSnapshot,
    RadioEvent,
};

use crate::drain::Outbox;
use crate::foreign::{ForeignRadio, central_adapter_state, lock};
use crate::identity::MobileIdentity;
use crate::radio::{
    AdapterPower, AdapterSnapshot, Advertisement, AndroidScanOptions, BondState, IngressClass,
    IngressStatus, MobilePlatform, PlatformRadio, RadioCompletion, RadioIngress, RequestId,
    RestoredPeer, ScanRequest, SecurityState, WakeSink,
};
use crate::session::{MobileSession, SessionState};
use crate::wire::{self, object, opt_text};

/// Host identity at open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostOptions {
    pub platform: MobilePlatform,
    /// The installing host's label (e.g. the app bundle id). Non-empty
    /// (`argument.invalid`, `ubm-mobile.host.owner`). It names no scope:
    /// the attachment identity is [`crate::MobileIdentity`]'s, in the legacy
    /// React Native formats, which carry no owner.
    pub owner: String,
    /// Adapter identity the platform reports (Android adapter address,
    /// `"corebluetooth"` on Apple). Non-empty.
    pub adapter_label: String,
}

/// One route from a characteristic instance to one session consumer.
#[derive(Debug, Clone)]
pub(crate) struct Route {
    pub session_id: u64,
    pub consumer: String,
    pub core_consumer: String,
    pub peer_id: String,
    pub selector: PathSelector,
    pub delivery: &'static str,
    /// The terminal that ended this consumer's stream, kept until the
    /// session unsubscribes so `session.reconcile` can answer it after its
    /// `stream-end` record was lost.
    pub terminal: Option<StreamEnd>,
}

/// The last link end the owner reported for one peer: what its `link`
/// record said, kept for `session.reconcile`.
#[derive(Debug, Clone)]
pub(crate) struct LinkEnd {
    pub connection_generation: String,
    pub database_generation: Option<String>,
    pub reason: &'static str,
}

/// One session's scan membership.
#[derive(Debug, Clone)]
pub(crate) struct ScanMember {
    pub membership: String,
    pub start_operation_id: String,
    pub service_uuids: Vec<String>,
    pub device_addresses: Vec<String>,
    pub deadline: Option<tokio::time::Instant>,
    /// Independent of the admission operation, which settles after start.
    pub expiry_cancel: OpTicket,
}

#[derive(Debug)]
pub(crate) struct PhysicalScan {
    pub operation: OperationId,
    pub request: ScanRequest,
}

/// The shared physical scan and who is on it.
#[derive(Debug, Default)]
pub(crate) struct ScanShare {
    pub physical: Option<PhysicalScan>,
    orphan_retry_scheduled: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PeerInfo {
    pub name: Option<String>,
    pub rssi: Option<i16>,
    pub last_seen_ms: Option<u64>,
    pub source: &'static str,
}

/// Why a consumer's stream ended: (reason, dropped items, dropped bytes).
pub(crate) type StreamEnd = (&'static str, u64, u64);

/// Result of draining one notification route for a bounded pump turn.
/// `committed` means at least one polled group reached the session's outbox
/// (admitted or refused), which is the cost a scope turn must not repeat.
enum RouteDrain {
    /// The route is still installed. `pending` means the budget stopped the
    /// drain while the core still holds values.
    Live { committed: bool, pending: bool },
    /// The route reached a terminal answer.
    Ended {
        committed: bool,
        terminal: StreamEnd,
    },
}

impl RouteDrain {
    fn committed(&self) -> bool {
        match self {
            Self::Live { committed, .. } | Self::Ended { committed, .. } => *committed,
        }
    }
}

/// What ended one poll run of a value group, applied only after the values
/// polled before it are committed.
#[derive(Clone, Copy)]
enum PollEnd {
    /// The group is full; the core may still hold more.
    Full,
    Empty,
    Ended(StreamEnd),
}

/// The terminal a refused value group ends its route with. Every polled value
/// the group could not admit is counted once, together with the core's own
/// terminal loss when it answered in the same poll run. `None` means only the
/// cutoff value of a journal that stopped under the group was refused: it is
/// counted in the handoff cutoff, not in a stream terminal, exactly as the
/// next turn's poll would have refused later values.
fn refusal_terminal(
    rejected: &ubm_desktop::continuation_outbox::DataBatchRejection,
    core: Option<StreamEnd>,
) -> Option<StreamEnd> {
    use ubm_desktop::continuation_outbox::DataIngressFailure as Failure;
    let (core_items, core_bytes) = core.map_or((0, 0), |(_, items, bytes)| (items, bytes));
    match rejected.failure {
        // A full queue, or sealed before this group (the cutoff counted every
        // value): the terminal reports them as overflow.
        Failure::Overflow { .. } | Failure::Sealed { .. } => Some((
            "overflow",
            rejected.items.saturating_add(core_items),
            rejected.bytes.saturating_add(core_bytes),
        )),
        // The session's journal failure retains the precise storage cause;
        // closed is the frozen wire lifecycle name, never a fabricated queue
        // overflow.
        Failure::Storage { .. } => Some((
            "closed",
            rejected.items.saturating_add(core_items),
            rejected.bytes.saturating_add(core_bytes),
        )),
        Failure::Stopped { bytes: cutoff } => {
            let later = rejected.items.saturating_sub(1);
            (later > 0).then(|| {
                (
                    "overflow",
                    later.saturating_add(core_items),
                    rejected
                        .bytes
                        .saturating_sub(cutoff as u64)
                        .saturating_add(core_bytes),
                )
            })
        }
    }
}

enum HostSignal {
    Advertisements,
    /// At least one value scope is dirty; the scopes themselves wait in the
    /// dirty set, so any number of them costs this one queue slot.
    Values,
    Lifecycle(LifecycleEvent),
    /// A platform adapter change, with the attachment it happened under.
    Adapter(AdapterSnapshot, u64, AttachmentTuple),
    /// The core reset the attachment after an adapter loss: the new
    /// attachment, and whether the reset ended the owned scan.
    AdapterReset(AttachmentTuple, bool),
    ScanFailed(String),
    ScanDeadlines,
    Security(String, SecurityState),
    SecurityFailed(Option<String>, DesktopError),
    /// Latest write-without-response readiness for one peer. The generation
    /// is the connection generation current when the report arrived.
    WriteReadiness(String, Option<String>, bool),
    Restored(Vec<RestoredPeer>),
    IngressDrop(IngressClass, u64),
}

/// Bound for queued host signals (X-R6). Value scopes share one queued
/// marker, the advertisement signal one, and every other current-state fact
/// merges per scope below, so a stalled pump plus a burst retains a bounded
/// prefix: at most `SIGNALS_CAP + 3` entries whatever the number of scopes.
/// What the bound refuses is counted in `signal_lost` / `overflow_drops`
/// and broadcast by the pump — never silently discarded.
const SIGNALS_CAP: usize = 1024;

/// Dirty value scopes one marker handling flushes. Bounded so one marker
/// cannot starve lifecycle and current-state signals; leftovers requeue
/// the marker for another turn.
const VALUE_SCOPE_BATCH: usize = 32;

/// Journaled records one scope turn may commit, which is also the most one
/// synced SQLite commit carries. The pump groups only values the core already
/// holds, never waiting to fill a group, so a commit costs one sync chain
/// instead of one per record. A scope turn commits at most one such group
/// across all of its routes, then yields: an unbounded drain, or one group per
/// route, would hold every already-queued security and lifecycle signal until
/// the whole backlog had been written.
const VALUE_RECORD_BATCH: usize = ubm_desktop::continuation_journal::APPEND_BATCH_MAX;

/// One advertisement turn must yield to queued lifecycle/deadline markers.
const ADVERTISEMENT_BATCH: usize = 32;

async fn pump_advertisement_batch<T, F: std::future::Future<Output = Option<T>>>(
    mut take: impl FnMut() -> F,
    mut deliver: impl FnMut(T),
) -> bool {
    for _ in 0..ADVERTISEMENT_BATCH {
        let Some(record) = take().await else {
            return false;
        };
        deliver(record);
    }
    // Do not probe one extra record: that would consume an undelivered
    // observation. An empty follow-up turn is safe and constantly bounded.
    true
}

const INGRESS_CLASSES: [IngressClass; 3] = [
    IngressClass::Advertisement,
    IngressClass::Notification,
    IngressClass::Control,
];

const fn ingress_index(class: IngressClass) -> usize {
    match class {
        IngressClass::Advertisement => 0,
        IngressClass::Notification => 1,
        IngressClass::Control => 2,
    }
}

#[derive(Default)]
struct SignalState {
    queue: VecDeque<HostSignal>,
    /// Dirty value scopes in round-robin order. The queue holds at most
    /// one marker for all of them, so the queue — not this list — is the
    /// bounded channel. `dirty_members` keeps `push_value` idempotent.
    dirty: VecDeque<InstanceKey>,
    dirty_members: HashSet<InstanceKey>,
    /// A `Values` marker already waits in the queue.
    value_marker_queued: bool,
    advertisements_pending: bool,
    scan_deadlines_pending: bool,
    closed: bool,
    /// Non-coalescible signals refused past the bound (lifecycle
    /// transitions, and current-state facts with no queued marker to merge
    /// into). The pump broadcasts one control ingress-drop per session for
    /// these, which drives `session.reconcile` from retained owner truth.
    signal_lost: u64,
    /// Ingress-drop counts that arrived while the queue was full, per
    /// class. The pump broadcasts them class-accurately to every session.
    overflow_drops: [u64; 3],
}

/// Ordered signal queue between the central (and ingress) and the pump.
/// Value scopes share one queued marker and advertisement signals one: the
/// pump polls the core's bounded queues, so one pending marker is enough
/// and the queue stays constantly bounded whatever the number of scopes.
/// Current-state facts (adapter, security, restored set, scan outcome,
/// reset, ingress-drop counts) merge per scope too: only the latest is
/// ever queued. Lifecycle transitions never coalesce — past the bound
/// they are counted and reconciled.
#[derive(Default)]
struct Signals {
    state: Mutex<SignalState>,
    notify: Notify,
}

impl Signals {
    /// Queue one signal, unless it merges into a queued one. A closed queue
    /// refuses everything (host teardown); a full queue refuses only what
    /// cannot merge, counting it for the pump's overflow broadcast.
    fn push(&self, signal: HostSignal) {
        {
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            match signal {
                HostSignal::Values => Self::ensure_value_marker(&mut state),
                HostSignal::Advertisements => {
                    if state.advertisements_pending {
                        return;
                    }
                    state.advertisements_pending = true;
                    state.queue.push_back(HostSignal::Advertisements);
                }
                HostSignal::ScanDeadlines => {
                    if state.scan_deadlines_pending {
                        return;
                    }
                    // One reserved marker drives all expired memberships. It
                    // cannot be dropped behind ordinary control queue pressure.
                    state.scan_deadlines_pending = true;
                    state.queue.push_back(HostSignal::ScanDeadlines);
                }
                HostSignal::Adapter(snapshot, updated_at, attachment) => {
                    let signal = HostSignal::Adapter(snapshot, updated_at, attachment);
                    if let Some(slot) = state
                        .queue
                        .iter_mut()
                        .find(|queued| matches!(queued, HostSignal::Adapter(..)))
                    {
                        // The latest platform snapshot wins: an adapter
                        // record carries current state, not an event.
                        *slot = signal;
                    } else {
                        Self::push_bounded(&mut state, signal);
                    }
                }
                HostSignal::AdapterReset(attachment, ended_scan) => {
                    let mut merged = false;
                    for queued in state.queue.iter_mut() {
                        if let HostSignal::AdapterReset(current, ended) = queued {
                            *current = attachment.clone();
                            *ended |= ended_scan;
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        Self::push_bounded(
                            &mut state,
                            HostSignal::AdapterReset(attachment, ended_scan),
                        );
                    }
                }
                HostSignal::ScanFailed(detail) => {
                    let mut merged = false;
                    for queued in state.queue.iter_mut() {
                        if let HostSignal::ScanFailed(current) = queued {
                            *current = detail.clone();
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        Self::push_bounded(&mut state, HostSignal::ScanFailed(detail));
                    }
                }
                HostSignal::Security(peer_id, observed) => {
                    let mut merged = false;
                    for queued in state.queue.iter_mut() {
                        if let HostSignal::Security(existing, current) = queued
                            && *existing == peer_id
                        {
                            *current = observed.clone();
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        Self::push_bounded(&mut state, HostSignal::Security(peer_id, observed));
                    }
                }
                HostSignal::WriteReadiness(peer_id, generation, ready) => {
                    let mut merged = false;
                    for queued in state.queue.iter_mut() {
                        if let HostSignal::WriteReadiness(
                            existing,
                            current_generation,
                            current_ready,
                        ) = queued
                            && *existing == peer_id
                        {
                            *current_generation = generation.clone();
                            *current_ready = ready;
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        Self::push_bounded(
                            &mut state,
                            HostSignal::WriteReadiness(peer_id, generation, ready),
                        );
                    }
                }
                HostSignal::Restored(peers) => {
                    let mut merged = false;
                    for queued in state.queue.iter_mut() {
                        if let HostSignal::Restored(current) = queued {
                            *current = peers.clone();
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        Self::push_bounded(&mut state, HostSignal::Restored(peers));
                    }
                }
                HostSignal::IngressDrop(class, count) => {
                    let mut merged = false;
                    for queued in state.queue.iter_mut() {
                        if let HostSignal::IngressDrop(existing, total) = queued
                            && *existing == class
                        {
                            *total += count;
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        Self::push_countable(&mut state, class, count);
                    }
                }
                HostSignal::SecurityFailed(peer, error) => {
                    Self::push_bounded(&mut state, HostSignal::SecurityFailed(peer, error));
                }
                HostSignal::Lifecycle(event) => {
                    if state.queue.len() >= SIGNALS_CAP {
                        state.signal_lost += 1;
                    } else {
                        state.queue.push_back(HostSignal::Lifecycle(event));
                    }
                }
            }
        }
        self.notify.notify_one();
    }

    /// Queue a current-state fact, or count it when the queue is full. A
    /// counted fact is re-readable: the pump's overflow broadcast drives
    /// `session.reconcile`, which answers every such fact from retained
    /// owner truth (adapter snapshot, security table, restored set, scan
    /// memberships).
    fn push_bounded(state: &mut SignalState, signal: HostSignal) {
        if state.queue.len() >= SIGNALS_CAP {
            state.signal_lost += 1;
        } else {
            state.queue.push_back(signal);
        }
    }

    /// Queue an ingress-drop marker, merging per class. A marker that finds
    /// no room keeps its count in the per-class overflow tally, which the
    /// pump broadcasts class-accurately.
    fn push_countable(state: &mut SignalState, class: IngressClass, count: u64) {
        if state.queue.len() >= SIGNALS_CAP {
            state.overflow_drops[ingress_index(class)] += count;
        } else {
            state.queue.push_back(HostSignal::IngressDrop(class, count));
        }
    }

    /// Queue one dirty value scope. The scope joins the dirty set; at most
    /// one marker waits in the queue for all of them, so any number of
    /// scopes costs one queue slot. The marker may pass the cap by one
    /// slot: dropping it would strand dirty scopes with no marker, and one
    /// slot keeps the queue's constant bound.
    fn push_value(&self, scope: InstanceKey) {
        {
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            if state.dirty_members.insert(scope.clone()) {
                state.dirty.push_back(scope);
            }
            Self::ensure_value_marker(&mut state);
        }
        self.notify.notify_one();
    }

    /// Queue the value marker unless one already waits. Callers hold the
    /// signal lock.
    fn ensure_value_marker(state: &mut SignalState) {
        if !state.value_marker_queued {
            state.value_marker_queued = true;
            state.queue.push_back(HostSignal::Values);
        }
    }

    /// Take up to `max` dirty value scopes from the front of the FIFO.
    /// A caller that still has values pushes that scope back, behind the
    /// scopes that have not had this cycle's turn. Values within a scope
    /// stay ordered by the core queue the pump polls.
    fn take_value_batch(&self, max: usize) -> Vec<InstanceKey> {
        let mut state = lock(&self.state);
        let mut batch = Vec::with_capacity(max.min(state.dirty.len()));
        while batch.len() < max {
            let Some(scope) = state.dirty.pop_front() else {
                break;
            };
            state.dirty_members.remove(&scope);
            batch.push(scope);
        }
        batch
    }

    /// Requeue the value marker while dirty scopes remain (the consumed
    /// marker drained one bounded batch). May pass the cap by one slot,
    /// like the initial queue, so no scope is ever stranded.
    fn requeue_values_if_dirty(&self) {
        let queued = {
            let mut state = lock(&self.state);
            if state.dirty.is_empty() || state.value_marker_queued {
                false
            } else {
                state.value_marker_queued = true;
                state.queue.push_back(HostSignal::Values);
                true
            }
        };
        if queued {
            self.notify.notify_one();
        }
    }

    /// Take accumulated overflow for the pump to broadcast. Counts reset:
    /// every lost signal is reported exactly once, to every session.
    fn take_overflow(&self) -> (u64, [u64; 3]) {
        let mut state = lock(&self.state);
        (
            std::mem::take(&mut state.signal_lost),
            std::mem::take(&mut state.overflow_drops),
        )
    }

    #[cfg(test)]
    pub(crate) fn queue_len(&self) -> usize {
        lock(&self.state).queue.len()
    }

    #[cfg(test)]
    pub(crate) fn dirty_len(&self) -> usize {
        lock(&self.state).dirty.len()
    }

    fn pop(&self) -> Option<HostSignal> {
        let mut state = lock(&self.state);
        let signal = state.queue.pop_front()?;
        match &signal {
            HostSignal::Values => state.value_marker_queued = false,
            HostSignal::Advertisements => state.advertisements_pending = false,
            HostSignal::ScanDeadlines => state.scan_deadlines_pending = false,
            _ => {}
        }
        Some(signal)
    }

    fn close(&self) {
        lock(&self.state).closed = true;
        self.notify.notify_one();
    }

    fn is_closed(&self) -> bool {
        lock(&self.state).closed
    }
}

/// Who a background (foreground-service) lease belongs to (87/N8).
///
/// A shared scope is one React Native module instance: its leases outlive
/// the manager that acquired them (legacy held them on the native module
/// until `invalidate()`), any session of that scope may update or release
/// them, and [`MobileHost::release_background_scope`] ends them. A session
/// opened without a scope is its own scope, so `session.dispose` ends it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BackgroundScope {
    Session(u64),
    Shared(String),
}

pub(crate) struct HostInner {
    pub platform: MobilePlatform,
    pub runtime: Handle,
    pub radio: ForeignRadio,
    pub central: DesktopCentral<ForeignRadio>,
    pub wake: Arc<dyn WakeSink>,
    signals: Arc<Signals>,
    pub sessions: Mutex<BTreeMap<u64, Arc<SessionState>>>,
    next_session: AtomicU64,
    #[cfg(test)]
    before_session_admission: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Consumers `drain_route` polled, in order. Debug tests read it.
    /// Release builds omit it.
    #[cfg(debug_assertions)]
    route_turns: Mutex<Vec<String>>,
    /// The consumers whose group each value scope turn committed, in order.
    /// Debug tests read it. Release builds omit it.
    #[cfg(debug_assertions)]
    scope_turns: Mutex<Vec<Vec<String>>>,
    pub scan: tokio::sync::Mutex<ScanShare>,
    pub scan_members: Mutex<BTreeMap<u64, ScanMember>>,
    pub routes: Mutex<HashMap<InstanceKey, Vec<Route>>>,
    pub directory: Mutex<BTreeMap<String, PeerInfo>>,
    pub restored: Mutex<BTreeMap<String, RestoredPeer>>,
    /// Restored peers already handed to an adopter, and the session that
    /// claimed each. Claims last for the process (legacy consumed the OS
    /// restoration identifiers once per process): a later manager never
    /// adopts the same peer again. Lock after `restored`.
    pub restoration_claims: Mutex<BTreeMap<String, u64>>,
    pub security: Mutex<HashMap<String, SecurityState>>,
    pub security_failures: Mutex<BTreeMap<Option<String>, (u64, DesktopError)>>,
    pub security_failure_revision: AtomicU64,
    pub adapter: Mutex<Option<(AdapterSnapshot, u64)>>,
    /// Live background leases per scope. A lease leaves only when the
    /// platform confirmed its release; a failed release stays for a retry.
    pub background: Mutex<BTreeMap<BackgroundScope, BTreeSet<String>>>,
    /// The latest link end per peer (one entry per peer).
    pub link_ends: Mutex<BTreeMap<String, LinkEnd>>,
    /// The latest database change per peer: (connection generation, the
    /// database generation the change invalidated).
    pub database_changes: Mutex<BTreeMap<String, (String, String)>>,
    shut_down: AtomicBool,
    ingress_failure: Mutex<Option<Value>>,
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    clock: Instant,
    pub(crate) continuation: std::sync::OnceLock<ubm_desktop::continuation::NativeContinuation>,
    pub(crate) recording_registry: Arc<ubm_desktop::continuation_journal::JournalRegistry>,
    pub(crate) continuation_closed: AtomicBool,
    pub(crate) continuation_admission: Mutex<()>,
}

/// The process-owned mobile owner. Cloning shares it.
#[derive(Clone)]
pub struct MobileHost {
    pub(crate) inner: Arc<HostInner>,
}

/// Mobile capability truth is reported by the platform through the
/// provider; the core registers no desktop rows for a mobile radio.
fn register_mobile_capabilities(_core: &mut Central) -> Result<(), CoreError> {
    Ok(())
}

fn canonical_advertisement(advertisement: Advertisement) -> Option<PeerSnapshot> {
    let in_i8 = |value: Option<i16>| value.is_none_or(|v| (-128..=127).contains(&v));
    if advertisement.peer_id.is_empty()
        || !in_i8(advertisement.rssi)
        || !in_i8(advertisement.tx_power_level)
    {
        return None;
    }
    let service_uuids = advertisement
        .service_uuids
        .iter()
        .map(|uuid| canonical_uuid(uuid).ok())
        .collect::<Option<Vec<_>>>()?;
    let mut service_data = advertisement.service_data;
    for entry in &mut service_data {
        entry.uuid = canonical_uuid(&entry.uuid).ok()?;
    }
    let canonical_list = |list: Option<Vec<String>>| -> Option<Option<Vec<String>>> {
        match list {
            None => Some(None),
            Some(uuids) => uuids
                .iter()
                .map(|uuid| canonical_uuid(uuid).ok())
                .collect::<Option<Vec<_>>>()
                .map(Some),
        }
    };
    let extras = ubm_desktop::AdvertisementExtras {
        capture_timestamp_ms: advertisement.capture_timestamp_ms,
        cached_name: advertisement.cached_name,
        address_type: None,
        solicited_service_uuids: canonical_list(advertisement.solicited_service_uuids)?,
        overflow_service_uuids: canonical_list(advertisement.overflow_service_uuids)?,
        connectable: advertisement.connectable,
        appearance: advertisement.appearance,
        raw_record: advertisement.raw_record,
        // Android `ScanResult` and CoreBluetooth discovery callbacks carry
        // this one advertisement's data (finding 122).
        source: ubm_desktop::ObservationSource::Advertisement,
    };
    Some(PeerSnapshot {
        id: advertisement.peer_id,
        address: advertisement.address,
        service_uuids,
        rssi: advertisement.rssi,
        local_name: advertisement.local_name,
        manufacturer_data: advertisement.manufacturer_data,
        service_data,
        tx_power_level: advertisement.tx_power_level,
        extras,
    })
}

fn non_empty_or_null(items: Vec<Value>) -> Value {
    if items.is_empty() {
        Value::Null
    } else {
        Value::Array(items)
    }
}

fn uuid_list(uuids: Option<&[String]>) -> Value {
    non_empty_or_null(
        uuids
            .unwrap_or_default()
            .iter()
            .map(|uuid| Value::from(uuid.as_str()))
            .collect(),
    )
}

pub(crate) fn advertisement_record(
    snapshot: &PeerSnapshot,
    observed_at_ms: u64,
    membership: &str,
    start_operation_id: &str,
) -> Value {
    object(vec![
        ("t", Value::from("adv")),
        ("operationId", Value::from(membership)),
        ("startOperationId", Value::from(start_operation_id)),
        ("peerId", Value::from(snapshot.id.as_str())),
        ("localName", opt_text(snapshot.local_name.as_deref())),
        ("rssi", snapshot.rssi.map_or(Value::Null, Value::from)),
        (
            "txPower",
            snapshot.tx_power_level.map_or(Value::Null, Value::from),
        ),
        (
            "serviceUuids",
            non_empty_or_null(
                snapshot
                    .service_uuids
                    .iter()
                    .map(|uuid| Value::from(uuid.as_str()))
                    .collect(),
            ),
        ),
        (
            "manufacturerData",
            non_empty_or_null(
                snapshot
                    .manufacturer_data
                    .iter()
                    .map(|entry| {
                        object(vec![
                            ("companyId", Value::from(entry.company_id)),
                            (
                                "payloadB64",
                                Value::from(wire::encode_base64(&entry.payload)),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "serviceData",
            non_empty_or_null(
                snapshot
                    .service_data
                    .iter()
                    .map(|entry| {
                        object(vec![
                            ("uuid", Value::from(entry.uuid.as_str())),
                            (
                                "payloadB64",
                                Value::from(wire::encode_base64(&entry.payload)),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "connectable",
            snapshot.extras.connectable.map_or(Value::Null, Value::from),
        ),
        (
            "solicitedServiceUuids",
            uuid_list(snapshot.extras.solicited_service_uuids.as_deref()),
        ),
        (
            "overflowServiceUuids",
            uuid_list(snapshot.extras.overflow_service_uuids.as_deref()),
        ),
        (
            "appearance",
            snapshot.extras.appearance.map_or(Value::Null, Value::from),
        ),
        (
            "rawRecordB64",
            snapshot
                .extras
                .raw_record
                .as_deref()
                .map_or(Value::Null, |bytes| Value::from(wire::encode_base64(bytes))),
        ),
        ("observedAtMs", Value::from(observed_at_ms)),
        (
            "sourceTimestampMs",
            snapshot
                .extras
                .capture_timestamp_ms
                .map_or(Value::Null, Value::from),
        ),
    ])
}

pub(crate) fn adapter_value(
    snapshot: &AdapterSnapshot,
    updated_at: u64,
    attachment: &AttachmentTuple,
) -> Value {
    object(vec![
        ("availability", Value::from(snapshot.availability.as_str())),
        (
            "authorization",
            Value::from(snapshot.authorization.as_str()),
        ),
        ("power", Value::from(snapshot.power.as_str())),
        ("safeReason", opt_text(snapshot.safe_reason.as_deref())),
        ("updatedAt", Value::from(updated_at)),
        (
            "backendGeneration",
            Value::from(attachment.backend_generation().as_str()),
        ),
        (
            "adapterGeneration",
            Value::from(attachment.adapter_generation().as_str()),
        ),
    ])
}

pub(crate) fn security_value(state: &SecurityState) -> Value {
    object(vec![
        ("bond", Value::from(state.bond.as_str())),
        ("encryption", Value::from(state.encryption.as_str())),
        ("authentication", Value::from(state.authentication.as_str())),
        (
            "secureConnections",
            Value::from(state.secure_connections.as_str()),
        ),
        (
            "pairingPossible",
            state.pairing_possible.map_or(Value::Null, Value::from),
        ),
    ])
}

fn peer_connection(record: Option<&PeerRecord>) -> &'static str {
    match record.and_then(|record| record.connection_state) {
        Some(ConnectionState::Connected) => "connected",
        Some(ConnectionState::Disconnected | ConnectionState::Lost | ConnectionState::Invalid) => {
            "disconnected"
        }
        Some(ConnectionState::Connecting | ConnectionState::Disconnecting) | None => "unknown",
    }
}

fn matches_member(member: &ScanMember, snapshot: &PeerSnapshot) -> bool {
    if member
        .deadline
        .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
    {
        return false;
    }
    let service_match = member.service_uuids.is_empty()
        || snapshot
            .service_uuids
            .iter()
            .any(|uuid| member.service_uuids.contains(uuid));
    let address_match = member.device_addresses.is_empty()
        || member.device_addresses.iter().any(|address| {
            address.eq_ignore_ascii_case(&snapshot.id)
                || snapshot
                    .address
                    .as_deref()
                    .is_some_and(|own| own.eq_ignore_ascii_case(address))
        });
    service_match && address_match
}

impl HostInner {
    async fn fail_pump(self: &Arc<Self>, error: Value) {
        // Close admission before waiting for disk or native cleanup. A dead
        // pump is not a usable owner, even when disposal must be retried.
        {
            let _sessions = lock(&self.sessions);
            self.shut_down.store(true, Ordering::SeqCst);
        }
        self.continuation_closed.store(true, Ordering::SeqCst);
        if let Some(executor) = self.continuation.get() {
            executor.stop_recovery();
        }
        *lock(&self.ingress_failure) = Some(serde_json::json!({"error":error}));
        // This is the pump itself: shutdown must not await its own join handle.
        lock(&self.pump).take();
        let owned = Arc::clone(self);
        let runtime = self.runtime.clone();
        let result = ubm_desktop::continuation_journal::run_blocking_result(move || {
            for session in owned.session_list() {
                session.outbox.fail_collection_worker();
                session.outbox.push_ingress_drop(IngressClass::Control);
            }
            let routes = lock(&owned.routes).clone();
            for (scope, entries) in routes {
                for route in entries {
                    if route.terminal.is_none() {
                        owned.mark_ended(&scope, &route, ("closed", 0, 0));
                        owned.end_route(&route, ("closed", 0, 0));
                    }
                }
            }
            // Keep membership ownership until authoritative native cleanup.
            let scans = lock(&owned.scan_members).clone();
            for (id, member) in scans {
                if let Some(session) = owned.session(id) {
                    session.outbox.push_control(serde_json::json!({"t":"scan-end","operationId":member.membership,"reason":"source-failed"}));
                }
            }
            let receipt = runtime.block_on(MobileHost { inner: owned }.shutdown());
            serde_json::from_str(&receipt).map_err(|_| serde_json::json!({"code":"protocol.malformed","domain":"core","operation":"ubm-mobile.ingress.cleanup","detail":"invalid cleanup receipt"}))
        }).await;
        if let Some(record) = lock(&self.ingress_failure).as_mut() {
            match result {
                Ok(cleanup) => record["cleanup"] = cleanup,
                Err(error) => record["cleanupFailure"] = error,
            }
        }
    }
    pub(crate) fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) fn is_shut_down(&self) -> bool {
        self.shut_down.load(Ordering::SeqCst)
    }

    pub(crate) fn session_list(&self) -> Vec<Arc<SessionState>> {
        lock(&self.sessions).values().cloned().collect()
    }

    fn session(&self, id: u64) -> Option<Arc<SessionState>> {
        lock(&self.sessions).get(&id).cloned()
    }

    pub(crate) fn broadcast(&self, record: &Value) {
        for session in self.session_list() {
            session.outbox.push_control(record.clone());
        }
    }

    /// One peer record for the wire (`peers.*`).
    pub(crate) fn peer_value(
        &self,
        peer_id: &str,
        core: Option<&PeerRecord>,
        source_override: Option<&'static str>,
    ) -> Value {
        let info = lock(&self.directory).get(peer_id).cloned();
        let restored = lock(&self.restored).get(peer_id).cloned();
        let bond = lock(&self.security)
            .get(peer_id)
            .map(|state| state.bond)
            .map_or("unknown", |bond| match bond {
                BondState::Bonded => "bonded",
                BondState::NotBonded => "not-bonded",
                BondState::Unsupported => "unsupported",
                BondState::Bonding | BondState::Unknown => "unknown",
            });
        let connection = peer_connection(core);
        let source = source_override
            .or(info.as_ref().map(|info| info.source))
            .or(restored.as_ref().map(|_| "restored"))
            .unwrap_or("app-reference");
        let name = info
            .as_ref()
            .and_then(|info| info.name.clone())
            .or(restored.and_then(|peer| peer.name));
        object(vec![
            ("peerId", Value::from(peer_id)),
            ("name", opt_text(name.as_deref())),
            (
                "rssi",
                info.as_ref()
                    .and_then(|info| info.rssi)
                    .map_or(Value::Null, Value::from),
            ),
            ("source", Value::from(source)),
            (
                "reachability",
                Value::from(if connection == "connected" {
                    "reachable"
                } else {
                    "unknown"
                }),
            ),
            ("connection", Value::from(connection)),
            ("bond", Value::from(bond)),
            (
                "lastSeenAtMonotonicMs",
                info.and_then(|info| info.last_seen_ms)
                    .map_or(Value::Null, Value::from),
            ),
        ])
    }

    /// Record a peer the host learned about outside a scan.
    pub(crate) fn note_peer(&self, peer_id: &str, name: Option<String>, source: &'static str) {
        let mut directory = lock(&self.directory);
        let entry = directory.entry(peer_id.to_owned()).or_insert(PeerInfo {
            name: None,
            rssi: None,
            last_seen_ms: None,
            source,
        });
        if entry.name.is_none() {
            entry.name = name;
        }
    }

    async fn handle(self: &Arc<Self>, signal: HostSignal) {
        match signal {
            HostSignal::ScanDeadlines => self.expire_scan_members(),
            HostSignal::Advertisements => {
                if pump_advertisement_batch(
                    || self.central.take_advertisement(),
                    |snapshot| self.route_advertisement(&snapshot),
                )
                .await
                {
                    self.signals.push(HostSignal::Advertisements);
                }
            }
            HostSignal::Values => {
                // One bounded batch per marker turn, so lifecycle and
                // current-state signals waiting behind it are not starved;
                // leftovers requeue the marker for another turn.
                let batch = self.signals.take_value_batch(VALUE_SCOPE_BATCH);
                for scope in &batch {
                    if self.flush_scope(scope).await {
                        self.signals.push_value(scope.clone());
                    }
                }
                self.signals.requeue_values_if_dirty();
            }
            HostSignal::Lifecycle(event) => self.route_lifecycle(event).await,
            HostSignal::Adapter(snapshot, updated_at, attachment) => {
                let record = object(vec![
                    ("t", Value::from("adapter")),
                    ("state", adapter_value(&snapshot, updated_at, &attachment)),
                ]);
                self.broadcast(&record);
            }
            HostSignal::AdapterReset(attachment, ended_scan) => {
                self.adapter_reset(&attachment, ended_scan).await;
            }
            HostSignal::ScanFailed(detail) => self.scan_failed(detail).await,
            HostSignal::Security(peer_id, state) => {
                let record = object(vec![
                    ("t", Value::from("security")),
                    ("peerId", Value::from(peer_id.as_str())),
                    ("state", security_value(&state)),
                ]);
                self.broadcast(&record);
            }
            HostSignal::SecurityFailed(peer_id, error) => {
                self.broadcast(&object(vec![
                    ("t", Value::from("security-failed")),
                    ("peerId", opt_text(peer_id.as_deref())),
                    ("error", Value::Object(wire::error_object(&error))),
                ]));
            }
            HostSignal::WriteReadiness(peer_id, generation, ready) => {
                let record = object(vec![
                    ("t", Value::from("readiness")),
                    ("peerId", Value::from(peer_id.as_str())),
                    ("connectionGeneration", opt_text(generation.as_deref())),
                    ("ready", Value::from(ready)),
                ]);
                self.broadcast(&record);
            }
            HostSignal::Restored(peers) => {
                let record = object(vec![
                    ("t", Value::from("restored")),
                    (
                        "peers",
                        Value::Array(
                            peers
                                .iter()
                                .map(|peer| {
                                    object(vec![
                                        ("peerId", Value::from(peer.peer_id.as_str())),
                                        ("name", opt_text(peer.name.as_deref())),
                                        ("connected", Value::Bool(peer.connected)),
                                    ])
                                })
                                .collect(),
                        ),
                    ),
                ]);
                self.broadcast(&record);
            }
            HostSignal::IngressDrop(class, count) => {
                for session in self.session_list() {
                    session.outbox.push_ingress_drop_count(class, count);
                }
            }
        }
    }

    fn route_advertisement(&self, snapshot: &PeerSnapshot) {
        let now = self.now_ms();
        {
            let mut directory = lock(&self.directory);
            let entry = directory.entry(snapshot.id.clone()).or_insert(PeerInfo {
                name: None,
                rssi: None,
                last_seen_ms: None,
                source: "scan-observed",
            });
            if snapshot.local_name.is_some() {
                entry.name.clone_from(&snapshot.local_name);
            } else if snapshot.extras.cached_name.is_some() {
                entry.name.clone_from(&snapshot.extras.cached_name);
            }
            entry.rssi = snapshot.rssi.or(entry.rssi);
            entry.last_seen_ms = Some(now);
        }
        let members: Vec<(u64, ScanMember)> = lock(&self.scan_members)
            .iter()
            .map(|(id, member)| (*id, member.clone()))
            .collect();
        for (session_id, member) in members {
            if !matches_member(&member, snapshot) {
                continue;
            }
            if let Some(session) = self.session(session_id)
                && session
                    .outbox
                    .push_data(advertisement_record(
                        snapshot,
                        now,
                        &member.membership,
                        &member.start_operation_id,
                    ))
                    .is_err()
            {
                session
                    .outbox
                    .push_ingress_drop(IngressClass::Advertisement);
            }
        }
    }

    fn end_route(&self, route: &Route, (reason, dropped_items, dropped_bytes): StreamEnd) {
        if let Some(session) = self.session(route.session_id) {
            session.outbox.push_control(object(vec![
                ("t", Value::from("stream-end")),
                ("consumer", Value::from(route.consumer.as_str())),
                ("reason", Value::from(reason)),
                ("droppedItems", Value::from(dropped_items)),
                ("droppedBytes", Value::from(dropped_bytes)),
            ]));
        }
    }

    fn mark_ended(&self, scope: &InstanceKey, route: &Route, terminal: StreamEnd) {
        if let Some(routes) = lock(&self.routes).get_mut(scope) {
            for entry in routes.iter_mut() {
                if entry.session_id == route.session_id && entry.consumer == route.consumer {
                    entry.terminal = Some(terminal);
                }
            }
        }
    }

    /// One consumer's stream state: `Some(None)` live, `Some(Some(end))`
    /// ended, `None` when no route is installed.
    pub(crate) fn route_terminal(
        &self,
        scope: &InstanceKey,
        session_id: u64,
        consumer: &str,
    ) -> Option<Option<StreamEnd>> {
        lock(&self.routes).get(scope).and_then(|routes| {
            routes
                .iter()
                .find(|route| route.session_id == session_id && route.consumer == consumer)
                .map(|route| route.terminal)
        })
    }

    /// One value turn for `scope`: commit at most one already-polled group of
    /// [`VALUE_RECORD_BATCH`] values, from the first route that holds any, and
    /// end streams on terminal answers. Routes without a value are searched
    /// without committing anything.
    ///
    /// Returns whether more work remains for the scope: the committing route
    /// may hold more values, or later routes were not visited. The caller
    /// requeues the scope so a queued security or lifecycle signal runs
    /// before the next commit.
    async fn flush_scope(&self, scope: &InstanceKey) -> bool {
        #[cfg(debug_assertions)]
        let counted = {
            let mut turns = lock(&self.scope_turns);
            let room = turns.len() < 1024;
            if room {
                turns.push(Vec::new());
            }
            room
        };
        let routes = self.live_routes(scope);
        for (index, route) in routes.iter().enumerate() {
            let unvisited = index + 1 < routes.len();
            let drain = self.drain_route(route, VALUE_RECORD_BATCH).await;
            let committed = drain.committed();
            #[cfg(debug_assertions)]
            if committed
                && counted
                && let Some(turn) = lock(&self.scope_turns).last_mut()
            {
                turn.push(route.consumer.clone());
            }
            match drain {
                // Nothing to commit: look at the next consumer.
                RouteDrain::Live { pending, .. } if !committed && !pending => {}
                // The committed route goes to the back so the next turn
                // starts at a consumer this one did not serve. Otherwise a
                // route with a standing backlog would be the first one
                // polled every turn, and later consumers of the scope would
                // never be polled while their core queues overflow.
                RouteDrain::Live { pending, .. } => {
                    self.rotate_route_to_end(scope, route);
                    return pending || unvisited;
                }
                RouteDrain::Ended { terminal, .. } => {
                    self.mark_ended(scope, route, terminal);
                    self.end_route(route, terminal);
                    if committed {
                        return unvisited;
                    }
                }
            }
        }
        false
    }

    /// Drain every value the core holds for the live routes of `scope`, in
    /// bounded commit groups, and return the routes that reached a terminal
    /// answer. The streams are marked ended and not emitted: the caller
    /// orders their terminals against lifecycle records.
    async fn drain_scope(&self, scope: &InstanceKey) -> Vec<(Route, StreamEnd)> {
        let mut ended = Vec::new();
        for route in self.live_routes(scope) {
            if let RouteDrain::Ended { terminal, .. } = self.drain_route(&route, usize::MAX).await {
                self.mark_ended(scope, &route, terminal);
                ended.push((route, terminal));
            }
        }
        ended
    }

    /// Move `route` behind the other routes of `scope`. The next bounded
    /// flush then starts at a consumer this turn did not finish.
    fn rotate_route_to_end(&self, scope: &InstanceKey, route: &Route) {
        let mut routes = lock(&self.routes);
        let Some(entries) = routes.get_mut(scope) else {
            return;
        };
        let Some(index) = entries.iter().position(|entry| {
            entry.session_id == route.session_id && entry.consumer == route.consumer
        }) else {
            return;
        };
        let deferred = entries.remove(index);
        entries.push(deferred);
    }

    fn live_routes(&self, scope: &InstanceKey) -> Vec<Route> {
        lock(&self.routes)
            .get(scope)
            .map(|routes| {
                routes
                    .iter()
                    .filter(|r| r.terminal.is_none())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Move up to `limit` values the core holds for one consumer into its
    /// session's outbox, committing at most [`VALUE_RECORD_BATCH`] already
    /// polled values per group (so an unlimited lifecycle drain is a series
    /// of bounded commits). A full budget returns with work still pending and
    /// does not poll another value. A terminal answer is returned and not
    /// emitted; the caller orders it against lifecycle records. Lifecycle
    /// passes `usize::MAX` so values that arrived before the transition
    /// all land first.
    ///
    /// Every value polled before an empty/terminal/invalidated answer is
    /// committed first, in order. A refused group's values are counted once in
    /// the returned terminal: a rolled-back group is never reported accepted.
    async fn drain_route(&self, route: &Route, limit: usize) -> RouteDrain {
        #[cfg(debug_assertions)]
        {
            let mut turns = lock(&self.route_turns);
            if turns.len() < 1024 {
                turns.push(route.consumer.clone());
            }
        }
        let mut taken = 0usize;
        let mut committed = false;
        loop {
            if taken == limit {
                return RouteDrain::Live {
                    committed,
                    pending: true,
                };
            }
            let group = (limit - taken).min(VALUE_RECORD_BATCH);
            let mut records = Vec::with_capacity(group);
            let mut session = None;
            let mut ending = PollEnd::Full;
            while records.len() < group {
                let poll = self
                    .central
                    .poll_notification(&route.peer_id, &route.selector, &route.core_consumer)
                    .await;
                match poll {
                    Ok(NotificationPoll::Value(bytes)) => {
                        let Some(owner) = self.session(route.session_id) else {
                            continue;
                        };
                        session = Some(owner);
                        records.push(object(vec![
                            ("t", Value::from("value")),
                            ("consumer", Value::from(route.consumer.as_str())),
                            ("valueB64", Value::from(wire::encode_base64(&bytes))),
                            ("delivery", Value::from(route.delivery)),
                        ]));
                    }
                    Ok(NotificationPoll::Empty) => {
                        ending = PollEnd::Empty;
                        break;
                    }
                    Ok(NotificationPoll::Terminal(terminal)) => {
                        ending = PollEnd::Ended((
                            "overflow",
                            terminal.dropped_items(),
                            terminal.dropped_bytes(),
                        ));
                        break;
                    }
                    Ok(NotificationPoll::Invalidated(_)) => {
                        ending = PollEnd::Ended(("invalidated", 0, 0));
                        break;
                    }
                    Ok(NotificationPoll::Closed) | Err(_) => {
                        ending = PollEnd::Ended(("closed", 0, 0));
                        break;
                    }
                }
            }
            if let Some(session) = session {
                committed = true;
                let outcome = session.outbox.push_data_batch(records);
                taken += outcome.accepted;
                if let Some(rejected) = outcome.rejected {
                    let core = match ending {
                        PollEnd::Ended(terminal) => Some(terminal),
                        PollEnd::Full | PollEnd::Empty => None,
                    };
                    match refusal_terminal(&rejected, core) {
                        Some(terminal) => {
                            return RouteDrain::Ended {
                                committed,
                                terminal,
                            };
                        }
                        // Only the stopped journal's cutoff value: nothing
                        // more is polled this turn unless the core already
                        // answered.
                        None if matches!(ending, PollEnd::Full) => {
                            return RouteDrain::Live {
                                committed,
                                pending: false,
                            };
                        }
                        None => {}
                    }
                }
            }
            match ending {
                PollEnd::Full => {}
                PollEnd::Empty => {
                    return RouteDrain::Live {
                        committed,
                        pending: false,
                    };
                }
                PollEnd::Ended(terminal) => {
                    return RouteDrain::Ended {
                        committed,
                        terminal,
                    };
                }
            }
        }
    }

    fn lifecycle_record(&self, event: &LifecycleEvent) -> Option<Value> {
        // A transition without a connection record cannot be matched to
        // any stream; the caller surfaces it instead of inventing one.
        let connection_generation = event.connection_generation.clone()?;
        let database_generation = crate::compat::lifecycle_database_generation(event);
        match event.kind {
            LifecycleKind::ServicesChanged => {
                let database_generation = database_generation?;
                // Kept for `session.reconcile` in case this record is lost.
                lock(&self.database_changes).insert(
                    event.peer_id.clone(),
                    (connection_generation.clone(), database_generation.clone()),
                );
                Some(object(vec![
                    ("t", Value::from("db-changed")),
                    ("peerId", Value::from(event.peer_id.as_str())),
                    ("connectionGeneration", Value::from(connection_generation)),
                    ("databaseGeneration", Value::from(database_generation)),
                ]))
            }
            LifecycleKind::LinkLost
            | LifecycleKind::AdapterLost
            | LifecycleKind::Released { .. } => {
                let reason = match event.kind {
                    LifecycleKind::Released { requested: true } => "local",
                    // The core ended the link because the adapter went away.
                    LifecycleKind::AdapterLost => "adapter",
                    _ => {
                        let adapter_down = lock(&self.adapter)
                            .as_ref()
                            .is_some_and(|(snapshot, _)| snapshot.power != AdapterPower::On);
                        if adapter_down { "adapter" } else { "peer" }
                    }
                };
                // Kept for `session.reconcile` in case this record is lost.
                lock(&self.link_ends).insert(
                    event.peer_id.clone(),
                    LinkEnd {
                        connection_generation: connection_generation.clone(),
                        database_generation: database_generation.clone(),
                        reason,
                    },
                );
                Some(object(vec![
                    ("t", Value::from("link")),
                    ("peerId", Value::from(event.peer_id.as_str())),
                    ("connectionGeneration", Value::from(connection_generation)),
                    (
                        "databaseGeneration",
                        opt_text(database_generation.as_deref()),
                    ),
                    ("reason", Value::from(reason)),
                ]))
            }
        }
    }

    /// Order per peer: values that arrived before the transition, then the
    /// transition record, then the stream ends it caused.
    async fn route_lifecycle(&self, event: LifecycleEvent) {
        let scopes: Vec<InstanceKey> = lock(&self.routes)
            .keys()
            .filter(|scope| scope.0 == event.peer_id)
            .cloned()
            .collect();
        let mut ended = Vec::new();
        for scope in &scopes {
            ended.extend(self.drain_scope(scope).await);
        }
        match self.lifecycle_record(&event) {
            Some(record) => self.broadcast(&record),
            None => {
                for session in self.session_list() {
                    session.outbox.push_ingress_drop(IngressClass::Control);
                }
            }
        }
        for (route, terminal) in ended {
            self.end_route(&route, terminal);
        }
        // Hubs the core invalidated after the first pass end here.
        for scope in &scopes {
            for (route, terminal) in self.drain_scope(scope).await {
                self.end_route(&route, terminal);
            }
        }
    }

    /// Legacy order after an adapter loss (`releaseCoreBluetoothAdapterLossResources`,
    /// then `advanceGeneration`): the scan members end `source-failed` (the
    /// core already stopped and ended the owned scan), then the new
    /// generations are published as an `adapter` record over the last
    /// platform snapshot. Links and streams ended through their own
    /// lifecycle records before this signal.
    async fn adapter_reset(&self, attachment: &AttachmentTuple, ended_scan: bool) {
        for session in self.session_list() {
            for (scope, consumer) in session.end_by_reset() {
                self.remove_route(&scope, session.id, &consumer);
            }
        }
        if ended_scan {
            let members = self.take_scan_members();
            self.scan.lock().await.physical = None;
            self.end_scan_members(members, "source-failed");
        }
        let last = lock(&self.adapter).clone();
        match last {
            Some((snapshot, updated_at)) => {
                let record = object(vec![
                    ("t", Value::from("adapter")),
                    ("state", adapter_value(&snapshot, updated_at, attachment)),
                ]);
                self.broadcast(&record);
            }
            // A reset needs a lost adapter, which only a platform snapshot
            // reports; without one the advance cannot be published.
            None => {
                for session in self.session_list() {
                    session.outbox.push_ingress_drop(IngressClass::Control);
                }
            }
        }
    }

    fn take_scan_members(&self) -> Vec<(u64, ScanMember)> {
        std::mem::take(&mut *lock(&self.scan_members))
            .into_iter()
            .collect()
    }

    fn end_scan_members(&self, members: Vec<(u64, ScanMember)>, reason: &'static str) {
        for (session_id, member) in members {
            member.expiry_cancel.request_cancel();
            if let Some(session) = self.session(session_id) {
                session.clear_scan(&member.membership);
                session.outbox.push_control(object(vec![
                    ("t", Value::from("scan-end")),
                    ("operationId", Value::from(member.membership.as_str())),
                    ("reason", Value::from(reason)),
                ]));
            }
        }
    }

    /// Arm a native membership clock. No JavaScript timer, event drain or
    /// runtime activity is required to stop delivering after its deadline.
    pub(crate) fn arm_scan_deadline(self: &Arc<Self>, session_id: u64, membership: &str) {
        let timer = lock(&self.scan_members)
            .get(&session_id)
            .and_then(|member| {
                (member.membership == membership)
                    .then_some(member)
                    .and_then(|member| {
                        member
                            .deadline
                            .map(|deadline| (deadline, member.expiry_cancel.clone()))
                    })
            });
        let Some((deadline, cancel)) = timer else {
            return;
        };
        let owner = Arc::downgrade(self);
        self.runtime.spawn(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {},
                () = tokio::time::sleep_until(deadline) => {
                    if let Some(owner) = owner.upgrade() {
                        owner.signals.push(HostSignal::ScanDeadlines);
                    }
                }
            }
        });
    }

    /// Runs on the same ordered pump as advertisement delivery: a cloned
    /// advertisement membership cannot publish behind its terminal. Native
    /// cleanup is separate so a slow scan stop cannot stall connected data.
    fn expire_scan_members(self: &Arc<Self>) {
        let now = tokio::time::Instant::now();
        let expired = {
            let mut members = lock(&self.scan_members);
            let ids: Vec<_> = members
                .iter()
                .filter_map(|(id, member)| {
                    member
                        .deadline
                        .is_some_and(|deadline| now >= deadline)
                        .then_some(*id)
                })
                .collect();
            ids.into_iter()
                .filter_map(|id| members.remove(&id).map(|member| (id, member)))
                .collect::<Vec<_>>()
        };
        if expired.is_empty() {
            return;
        }
        self.end_scan_members(expired, "operation-timed-out");
        let owner = Arc::clone(self);
        self.runtime.spawn(async move {
            let mut share = owner.scan.lock().await;
            // Another live member, including one admitted while we waited,
            // owns the physical scan. Never stop it for a retired generation.
            if !lock(&owner.scan_members).is_empty() {
                return;
            }
            let Some(operation) = share.physical.as_ref().map(|scan| scan.operation.clone()) else {
                return;
            };
            match owner
                .central
                .stop_scan(&operation, OpControl::unbounded())
                .await
            {
                Ok(_) => share.physical = None,
                Err(error) => {
                    eprintln!(
                        "ubm-mobile: expired scan cleanup retained by process owner: {error}"
                    );
                    if !share.orphan_retry_scheduled {
                        share.orphan_retry_scheduled = true;
                        owner.runtime.spawn(Arc::clone(&owner).retry_orphan_scan());
                    }
                }
            }
        });
    }

    async fn scan_failed(&self, detail: String) {
        let members = self.take_scan_members();
        let physical = self.scan.lock().await.physical.take();
        if let Some(physical) = physical {
            // The OS already stopped; release the core's scan ownership.
            // A failed release is retained by the central and counted.
            let _ = self
                .central
                .stop_scan(&physical.operation, OpControl::unbounded())
                .await;
        }
        let _ = detail;
        self.end_scan_members(members, "source-failed");
    }

    /// The op is dead: cancelled, or past its caller budget. Cancellation
    /// reports `operation.aborted`; an expired budget reports
    /// `operation.timed-out` with no backstop detail (the caller set it).
    fn scan_not_alive(ctl: &OpControl) -> Option<DesktopError> {
        Self::scan_not_alive_parts(&ctl.ticket, &ctl.budget)
    }

    fn scan_not_alive_parts(ticket: &OpTicket, budget: &Budget) -> Option<DesktopError> {
        if ticket.is_cancel_requested() {
            return Some(DesktopError::new(
                BleErrorCode::OperationAborted,
                BleErrorDomain::Scan,
                "scan.start",
            ));
        }
        if budget.is_expired() {
            return Some(DesktopError::new(
                BleErrorCode::OperationTimedOut,
                BleErrorDomain::Scan,
                "scan.start",
            ));
        }
        None
    }

    /// Acquire the shared-scan admission section in a cancellation- and
    /// deadline-aware way (X-R1): an op cancelled or expired while queued on
    /// the scan mutex never takes a membership. The checks run again inside
    /// the section, so the no-radio fast path cannot admit a dead op
    /// either. This is deliberately not a timeout around the whole
    /// mutation: abandoning the join after the widen restart stopped the
    /// previous physical scan would orphan every member.
    async fn lock_scan_share(
        &self,
        ctl: &OpControl,
    ) -> Result<tokio::sync::MutexGuard<'_, ScanShare>, DesktopError> {
        if let Some(error) = Self::scan_not_alive(ctl) {
            return Err(error);
        }
        let guard = match ctl.budget.remaining() {
            Some(wait) => {
                tokio::select! {
                    biased;
                    () = ctl.ticket.cancelled() => return Err(Self::scan_not_alive(ctl)
                        .unwrap_or_else(|| DesktopError::new(
                            BleErrorCode::OperationAborted,
                            BleErrorDomain::Scan,
                            "scan.start",
                        ))),
                    () = tokio::time::sleep(wait) => return Err(DesktopError::new(
                        BleErrorCode::OperationTimedOut,
                        BleErrorDomain::Scan,
                        "scan.start",
                    )),
                    guard = self.scan.lock() => guard,
                }
            }
            None => {
                tokio::select! {
                    biased;
                    () = ctl.ticket.cancelled() => return Err(Self::scan_not_alive(ctl)
                        .unwrap_or_else(|| DesktopError::new(
                            BleErrorCode::OperationAborted,
                            BleErrorDomain::Scan,
                            "scan.start",
                        ))),
                    guard = self.scan.lock() => guard,
                }
            }
        };
        if let Some(error) = Self::scan_not_alive(ctl) {
            return Err(error);
        }
        Ok(guard)
    }

    /// Start, join or widen the shared physical scan for one member.
    pub(crate) async fn join_scan(
        self: &Arc<Self>,
        session_id: u64,
        member: ScanMember,
        android: Option<AndroidScanOptions>,
        ctl: OpControl,
        orphan_cleanup: tokio::sync::oneshot::Sender<()>,
    ) -> Result<(), DesktopError> {
        let mut share = self.lock_scan_share(&ctl).await?;
        let mut wanted = {
            let members = lock(&self.scan_members);
            let everyone: Vec<&ScanMember> = members
                .iter()
                .filter(|(id, _)| **id != session_id)
                .map(|(_, member)| member)
                .chain(std::iter::once(&member))
                .collect();
            ScanRequest {
                service_uuids: union_or_broad(everyone.iter().map(|m| &m.service_uuids)),
                device_addresses: union_or_broad(everyone.iter().map(|m| &m.device_addresses)),
                android,
            }
        };
        // A retained physical record with no members is cleanup debt, not
        // a shareable scan. A new manager must stop that exact generation
        // before it can acquire a fresh radio scan. The orphan worker uses
        // this same lock, so it cannot race a replacement into stopping the
        // new generation. A refused cleanup leaves the old record and retry
        // driver intact.
        if lock(&self.scan_members).is_empty()
            && let Some(operation) = share.physical.as_ref().map(|scan| scan.operation.clone())
        {
            // Notify the admission waiter only after the locked state check.
            // A preflight snapshot would race another caller creating this
            // orphan before we acquire the scan section.
            let _ = orphan_cleanup.send(());
            self.central
                .stop_scan(
                    &operation,
                    OpControl::new(ctl.budget, ubm_desktop::OpTicket::new()),
                )
                .await?;
            share.physical = None;
            if let Some(error) = Self::scan_not_alive(&ctl) {
                return Err(error);
            }
        }
        if let Some(physical) = &share.physical {
            if physical.request.android != android {
                return Err(DesktopError::new(
                    BleErrorCode::ScanAlreadyActive,
                    BleErrorDomain::Scan,
                    "scan.start",
                )
                .with_detail("the shared scan runs with different platform options"));
            }
            if covers(&physical.request.service_uuids, &member.service_uuids)
                && covers(&physical.request.device_addresses, &member.device_addresses)
            {
                // No radio call below would notice a dead op: re-check the
                // ticket and the deadline before taking the membership.
                if let Some(error) = Self::scan_not_alive(&ctl) {
                    return Err(error);
                }
                lock(&self.scan_members).insert(session_id, member);
                return Ok(());
            }
            // Widen: restart the physical scan with the union filter.
            let operation = physical.operation.clone();
            // The restart's stop runs under the caller's budget with its own
            // ticket: one ticket settles once, and the start below is the
            // operation the caller's cancel targets.
            self.central
                .stop_scan(
                    &operation,
                    OpControl::new(ctl.budget, ubm_desktop::OpTicket::new()),
                )
                .await?;
            share.physical = None;
        }
        sort_dedup(&mut wanted.service_uuids);
        sort_dedup(&mut wanted.device_addresses);
        self.radio.stage_scan(wanted.clone());
        let filter: Vec<&str> = wanted.service_uuids.iter().map(String::as_str).collect();
        // `ctl` moves into the start call; keep the liveness witnesses for
        // the post-call membership check below.
        let ticket = ctl.ticket.clone();
        let budget = ctl.budget;
        let started = self
            .central
            .start_scan(&format!("ubm-mobile-scan-{session_id}"), &filter, ctl)
            .await;
        self.radio.clear_staging(None, None, true);
        match started {
            Ok(session) => {
                let operation = session.operation_id().clone();
                share.physical = Some(PhysicalScan {
                    operation: operation.clone(),
                    request: wanted,
                });
                // The radio call took a while: a queued cancel or an
                // expired budget must not take a membership for a dead op.
                if let Some(error) = Self::scan_not_alive_parts(&ticket, &budget) {
                    let cleanup = self
                        .central
                        .stop_scan(&operation, OpControl::unbounded())
                        .await;
                    if cleanup.is_ok() {
                        // Widening already stopped the scan that served the
                        // existing members. The replacement is gone too, so
                        // those members must be told that their source ended.
                        share.physical = None;
                        let orphans = self.take_scan_members();
                        drop(share);
                        self.end_scan_members(orphans, "source-failed");
                        return Err(error);
                    }
                    // A refused cleanup stays in both owners. Existing
                    // members can retry their ordinary stop; a first scanner
                    // has no member, so the process owner drives that debt.
                    if lock(&self.scan_members).is_empty() && !share.orphan_retry_scheduled {
                        share.orphan_retry_scheduled = true;
                        self.runtime.spawn(Arc::clone(self).retry_orphan_scan());
                    }
                    return Err(error);
                }
                lock(&self.scan_members).insert(session_id, member);
                Ok(())
            }
            Err(error) => {
                // A failed restart leaves the previous members without a
                // radio scan: end their streams instead of pretending. The
                // central can nevertheless retain this generation after a
                // failed compensating stop. There is no public membership
                // left to retry it, so transfer that exact identity to the
                // process-owned orphan driver before returning the error.
                let orphans = self.take_scan_members();
                if let Some(operation) = self.central.active_scan_id() {
                    share.physical = Some(PhysicalScan {
                        operation,
                        request: wanted,
                    });
                    if !share.orphan_retry_scheduled {
                        share.orphan_retry_scheduled = true;
                        self.runtime.spawn(Arc::clone(self).retry_orphan_scan());
                    }
                }
                drop(share);
                self.end_scan_members(orphans, "source-failed");
                Err(error)
            }
        }
    }

    async fn retry_orphan_scan(self: Arc<Self>) {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if self.shut_down.load(Ordering::SeqCst) {
                return;
            }
            let mut share = self.scan.lock().await;
            if !lock(&self.scan_members).is_empty() || share.physical.is_none() {
                share.orphan_retry_scheduled = false;
                return;
            }
            let operation = share.physical.as_ref().unwrap().operation.clone();
            match self
                .central
                .stop_scan(&operation, OpControl::unbounded())
                .await
            {
                Ok(_) => {
                    share.physical = None;
                    share.orphan_retry_scheduled = false;
                    return;
                }
                Err(error) => {
                    eprintln!("ubm-mobile: retained orphan scan cleanup failed: {error}");
                }
            }
        }
    }

    /// Leave the shared scan; the last member stops the physical scan. A
    /// failed stop keeps the membership so the caller can retry.
    pub(crate) async fn leave_scan(
        &self,
        session_id: u64,
        membership: &str,
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        let mut share = self.scan.lock().await;
        let last = {
            let members = lock(&self.scan_members);
            if !members
                .get(&session_id)
                .is_some_and(|member| member.membership == membership)
            {
                return Ok(());
            }
            members.len() == 1
        };
        if last && let Some(physical) = &share.physical {
            let operation = physical.operation.clone();
            self.central.stop_scan(&operation, ctl).await?;
            share.physical = None;
        }
        let removed = {
            let mut members = lock(&self.scan_members);
            if members
                .get(&session_id)
                .is_some_and(|member| member.membership == membership)
            {
                members.remove(&session_id)
            } else {
                None
            }
        };
        if let Some(member) = removed {
            member.expiry_cancel.request_cancel();
        }
        Ok(())
    }

    pub(crate) fn add_route(&self, scope: InstanceKey, route: Route) {
        let mut routes = lock(&self.routes);
        let entries = routes.entry(scope).or_default();
        entries.retain(|entry| {
            !(entry.session_id == route.session_id && entry.consumer == route.consumer)
        });
        entries.push(route);
    }

    pub(crate) fn remove_route(&self, scope: &InstanceKey, session_id: u64, consumer: &str) {
        let mut routes = lock(&self.routes);
        if let Some(entries) = routes.get_mut(scope) {
            entries.retain(|entry| !(entry.session_id == session_id && entry.consumer == consumer));
            if entries.is_empty() {
                routes.remove(scope);
            }
        }
    }

    /// Queue a flush of `scope` (values admitted before a route existed).
    pub(crate) fn kick(&self, scope: InstanceKey) {
        self.signals.push_value(scope);
    }

    pub(crate) fn remove_session(&self, session_id: u64) {
        lock(&self.sessions).remove(&session_id);
    }

    pub(crate) fn note_background(&self, scope: &BackgroundScope, lease_id: String) {
        lock(&self.background)
            .entry(scope.clone())
            .or_default()
            .insert(lease_id);
    }

    pub(crate) fn holds_background(&self, scope: &BackgroundScope, lease_id: &str) -> bool {
        lock(&self.background)
            .get(scope)
            .is_some_and(|leases| leases.contains(lease_id))
    }

    /// Release one lease through the platform; forgotten only once the
    /// platform confirmed it.
    pub(crate) async fn release_background(
        &self,
        scope: &BackgroundScope,
        lease_id: &str,
        ctl: &OpControl,
    ) -> Result<(), DesktopError> {
        let lease = lease_id.to_owned();
        match crate::session::bounded(
            ctl,
            "background.release",
            self.radio
                .call(|id| crate::radio::RadioRequest::ReleaseBackground {
                    id,
                    lease_id: lease,
                }),
        )
        .await?
        {
            RadioCompletion::Unit => {
                let mut background = lock(&self.background);
                if let Some(leases) = background.get_mut(scope) {
                    leases.remove(lease_id);
                    if leases.is_empty() {
                        background.remove(scope);
                    }
                }
                Ok(())
            }
            _ => Err(crate::session::protocol("background.release")),
        }
    }

    /// Release every lease of `scope`; each failure is reported and the
    /// lease kept for a retry.
    pub(crate) async fn release_background_scope(&self, scope: &BackgroundScope) -> Vec<Value> {
        let leases: Vec<String> = lock(&self.background)
            .get(scope)
            .map(|leases| leases.iter().cloned().collect())
            .unwrap_or_default();
        let mut failures = Vec::new();
        for lease_id in leases {
            if let Err(error) = self
                .release_background(scope, &lease_id, &OpControl::unbounded())
                .await
            {
                failures.push(crate::session::cleanup_failure("background", &error));
            }
        }
        failures
    }
}

fn union_or_broad<'a>(sets: impl Iterator<Item = &'a Vec<String>>) -> Vec<String> {
    let mut union = Vec::new();
    for set in sets {
        if set.is_empty() {
            return Vec::new();
        }
        union.extend(set.iter().cloned());
    }
    union
}

fn covers(physical: &[String], wanted: &[String]) -> bool {
    physical.is_empty()
        || (!wanted.is_empty()
            && wanted
                .iter()
                .all(|item| physical.iter().any(|p| p.eq_ignore_ascii_case(item))))
}

fn sort_dedup(items: &mut Vec<String>) {
    items.sort();
    items.dedup();
}

async fn pump(host: Weak<HostInner>, signals: Arc<Signals>) {
    loop {
        while let Some(signal) = signals.pop() {
            let Some(host) = host.upgrade() else {
                return;
            };
            let continuation_changed = match &signal {
                HostSignal::Lifecycle(event) => {
                    !matches!(event.kind, LifecycleKind::Released { .. })
                }
                HostSignal::AdapterReset(..) | HostSignal::Adapter(..) => true,
                _ => false,
            };
            {
                // One awaited signal pass owns order. The existing bounded
                // signal/native queues retain pressure while disk is busy.
                // A journal may attach while handle awaits; never choose the
                // execution thread from a pre-await attachment snapshot.
                let owned = host.clone();
                let runtime = host.runtime.clone();
                if let Err(error) =
                    ubm_desktop::continuation_journal::run_blocking_result(move || {
                        runtime.block_on(owned.handle(signal));
                        Ok(())
                    })
                    .await
                {
                    host.fail_pump(error).await;
                    return;
                }
            }
            if continuation_changed
                && !host.shut_down.load(Ordering::SeqCst)
                && let Some(executor) = host.continuation.get()
            {
                executor.request_recovery(&host.runtime);
            }
        }
        // Overflow is never silently discarded (X-R6): a lost lifecycle or
        // current-state fact becomes one control ingress-drop per session,
        // which drives `session.reconcile` from retained owner truth; lost
        // ingress drops keep their class so drop accounting stays exact.
        let (lost, drops) = signals.take_overflow();
        if lost > 0 || drops != [0, 0, 0] {
            let Some(host) = host.upgrade() else {
                return;
            };
            let sessions = host.session_list();
            let report = move || {
                if lost > 0 {
                    for session in &sessions {
                        session.outbox.push_ingress_drop(IngressClass::Control);
                    }
                }
                for (index, class) in INGRESS_CLASSES.iter().enumerate() {
                    let count = drops[index];
                    if count > 0 {
                        for session in &sessions {
                            session.outbox.push_ingress_drop_count(*class, count);
                        }
                    }
                }
            };
            {
                if let Err(error) =
                    ubm_desktop::continuation_journal::run_blocking_result(move || {
                        report();
                        Ok(())
                    })
                    .await
                {
                    host.fail_pump(error).await;
                    return;
                }
            }
        }
        if signals.is_closed() {
            return;
        }
        signals.notify.notified().await;
    }
}

impl MobileHost {
    /// Retained process-level source failure, including its attempted cleanup.
    /// Reading this does not clear the diagnostic or acknowledge any recording.
    #[must_use]
    pub fn ingress_failure(&self) -> Option<Value> {
        lock(&self.inner.ingress_failure).clone()
    }
    /// Open the process owner: the central over `platform` on `runtime`
    /// (the shared desktop executor in production). Open issues no radio
    /// request, so the platform can answer requests only after it holds
    /// the returned host.
    pub async fn open(
        platform: Arc<dyn PlatformRadio>,
        wake: Arc<dyn WakeSink>,
        options: HostOptions,
        runtime: Handle,
    ) -> Result<Self, DesktopError> {
        Self::open_with_recording_registry(platform, wake, options, runtime, Arc::default()).await
    }

    /// Install the radio using the same authority as pre-existing offline data access.
    pub async fn open_with_recording_registry(
        platform: Arc<dyn PlatformRadio>,
        wake: Arc<dyn WakeSink>,
        options: HostOptions,
        runtime: Handle,
        recording_registry: Arc<ubm_desktop::continuation_journal::JournalRegistry>,
    ) -> Result<Self, DesktopError> {
        if options.owner.is_empty() {
            return Err(DesktopError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "ubm-mobile.host.owner",
            ));
        }
        let radio = ForeignRadio::new(platform, options.platform, options.adapter_label.clone());
        let signals = Arc::new(Signals::default());
        let observer_signals = Arc::clone(&signals);
        let observer: ubm_desktop::CentralObserver = Arc::new(move |signal| match signal {
            CentralSignal::Advertisement(_) => observer_signals.push(HostSignal::Advertisements),
            CentralSignal::Value { scope, .. } => observer_signals.push_value(scope),
            CentralSignal::Lifecycle(event) => {
                observer_signals.push(HostSignal::Lifecycle(event));
            }
            // Adapter records come from the platform's full snapshot
            // (authorization, resetting), not the central's projection.
            CentralSignal::Adapter(_) => {}
            // The mobile radio opts into the core's adapter-loss teardown
            // (legacy `startAdapterLossCleanup`/`advanceGeneration`).
            CentralSignal::AdapterReset(event) => {
                observer_signals.push(HostSignal::AdapterReset(
                    event.current,
                    event.ended_scan.is_some(),
                ));
            }
            // Security, scan-terminal, and connection-parameter facts are
            // not a mobile readiness stream. Apple write readiness is:
            // the radio probes `canSendWriteWithoutResponse` and ingests
            // `peripheralIsReady(toSendWriteWithoutResponse:)`.
            CentralSignal::WriteReadiness(event) => {
                observer_signals.push(HostSignal::WriteReadiness(
                    event.peer_id,
                    event.connection_generation,
                    event.ready,
                ));
            }
            CentralSignal::Security(_)
            | CentralSignal::ScanTerminal(_)
            | CentralSignal::ConnectionParameters(_) => {}
        });
        let drop_signals = Arc::clone(&signals);
        radio.set_drop_hook(Arc::new(move |class| {
            drop_signals.push(HostSignal::IngressDrop(class, 1));
        }));
        let profile = CentralProfile {
            directory_os: ubm_desktop::DesktopOs::MacOs,
            identity: Arc::new(MobileIdentity::new(options.platform)),
            register_capabilities: register_mobile_capabilities,
            observer: Some(observer),
            adapter_id: None,
            bluez_bus: ubm_desktop::BluezBus::System,
        };
        let central = DesktopCentral::open_with(radio.clone(), profile).await?;
        let inner = Arc::new(HostInner {
            platform: options.platform,
            runtime: runtime.clone(),
            radio,
            central,
            wake,
            signals: Arc::clone(&signals),
            sessions: Mutex::new(BTreeMap::new()),
            next_session: AtomicU64::new(1),
            #[cfg(test)]
            before_session_admission: Mutex::new(None),
            #[cfg(debug_assertions)]
            route_turns: Mutex::new(Vec::new()),
            #[cfg(debug_assertions)]
            scope_turns: Mutex::new(Vec::new()),
            scan: tokio::sync::Mutex::new(ScanShare::default()),
            scan_members: Mutex::new(BTreeMap::new()),
            routes: Mutex::new(HashMap::new()),
            directory: Mutex::new(BTreeMap::new()),
            restored: Mutex::new(BTreeMap::new()),
            restoration_claims: Mutex::new(BTreeMap::new()),
            security: Mutex::new(HashMap::new()),
            security_failures: Mutex::new(BTreeMap::new()),
            security_failure_revision: AtomicU64::new(0),
            adapter: Mutex::new(None),
            background: Mutex::new(BTreeMap::new()),
            link_ends: Mutex::new(BTreeMap::new()),
            database_changes: Mutex::new(BTreeMap::new()),
            shut_down: AtomicBool::new(false),
            ingress_failure: Mutex::new(None),
            pump: Mutex::new(None),
            clock: Instant::now(),
            continuation: std::sync::OnceLock::new(),
            recording_registry,
            continuation_closed: AtomicBool::new(false),
            continuation_admission: Mutex::new(()),
        });
        let worker = runtime.spawn(pump(Arc::downgrade(&inner), signals));
        *lock(&inner.pump) = Some(worker);
        Ok(Self { inner })
    }

    /// Open from a non-runtime thread (JNI/UniFFI entry): runs [`Self::open`]
    /// on `runtime` and waits for it.
    pub fn open_blocking(
        platform: Arc<dyn PlatformRadio>,
        wake: Arc<dyn WakeSink>,
        options: HostOptions,
        runtime: Handle,
    ) -> Result<Self, DesktopError> {
        Self::open_blocking_with_recording_registry(
            platform,
            wake,
            options,
            runtime,
            Arc::default(),
        )
    }

    pub fn open_blocking_with_recording_registry(
        platform: Arc<dyn PlatformRadio>,
        wake: Arc<dyn WakeSink>,
        options: HostOptions,
        runtime: Handle,
        recording_registry: Arc<ubm_desktop::continuation_journal::JournalRegistry>,
    ) -> Result<Self, DesktopError> {
        let (tx, rx) = std::sync::mpsc::channel();
        runtime.spawn(
            Self::open_with_recording_registry(
                platform,
                wake,
                options,
                runtime.clone(),
                recording_registry,
            )
            .then_send(tx),
        );
        rx.recv().unwrap_or_else(|_| {
            Err(DesktopError::new(
                BleErrorCode::LifecycleInvariantViolation,
                BleErrorDomain::Core,
                "ubm-mobile.host.open",
            )
            .with_detail("open task ended without an answer"))
        })
    }

    #[must_use]
    pub fn platform(&self) -> MobilePlatform {
        self.inner.platform
    }

    /// Deliver the platform's answer to one request.
    pub fn complete(
        &self,
        request_id: RequestId,
        completion: RadioCompletion,
    ) -> crate::foreign::CompletionStatus {
        self.inner.radio.complete(request_id, completion)
    }

    /// Deliver one unsolicited platform fact. Never blocks.
    pub fn ingest(&self, ingress: RadioIngress) -> IngressStatus {
        let inner = &*self.inner;
        if inner.is_shut_down() {
            return IngressStatus::Closed;
        }
        let pushed = match ingress {
            RadioIngress::Advertisement(advertisement) => {
                match canonical_advertisement(advertisement) {
                    Some(snapshot) => inner.radio.push_event(RadioEvent::Advertisement(snapshot)),
                    None => {
                        inner.radio.note_drop(IngressClass::Advertisement);
                        Err(IngressClass::Advertisement)
                    }
                }
            }
            RadioIngress::Connection {
                peer_id,
                connected,
                status,
            } => {
                if !connected {
                    let mut security = lock(&inner.security);
                    if let Some(state) = security.get_mut(&peer_id) {
                        if state.encryption != crate::radio::EncryptionState::Unsupported {
                            state.encryption = crate::radio::EncryptionState::Unknown;
                        }
                        if state.authentication != crate::radio::AuthenticationState::Unsupported {
                            state.authentication = crate::radio::AuthenticationState::Unknown;
                        }
                        if state.secure_connections
                            != crate::radio::SecureConnectionsState::Unsupported
                        {
                            state.secure_connections =
                                crate::radio::SecureConnectionsState::Unknown;
                        }
                        inner
                            .signals
                            .push(HostSignal::Security(peer_id.clone(), state.clone()));
                    }
                }
                inner.radio.push_event(if connected {
                    RadioEvent::Connected(peer_id)
                } else if status.is_some_and(|status| status != 0) {
                    // Android reports a non-zero GATT status, CoreBluetooth an
                    // `NSError`, when the link ended for a reason other than
                    // this app's release: a loss even if a release was pending.
                    RadioEvent::Lost(peer_id)
                } else {
                    RadioEvent::Disconnected(peer_id)
                })
            }
            RadioIngress::ServicesChanged { peer_id } => {
                inner.radio.push_event(RadioEvent::ServicesChanged(peer_id))
            }
            RadioIngress::Notification {
                instance,
                epoch,
                value,
            } => match (
                canonical_uuid(&instance.service_uuid),
                canonical_uuid(&instance.characteristic_uuid),
            ) {
                (Ok(service_uuid), Ok(characteristic_uuid)) => {
                    inner.radio.push_event(RadioEvent::Notification {
                        peer_id: instance.peer_id,
                        service_uuid,
                        service_occurrence: instance.service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence: instance.characteristic_occurrence,
                        epoch,
                        value,
                    })
                }
                _ => {
                    inner.radio.note_drop(IngressClass::Notification);
                    Err(IngressClass::Notification)
                }
            },
            RadioIngress::AdapterState(snapshot) => {
                let updated_at = inner.now_ms();
                *lock(&inner.adapter) = Some((snapshot.clone(), updated_at));
                let state = central_adapter_state(&snapshot);
                // The change is published under the attachment it happened
                // in, queued before the central can reset it (legacy emitted
                // the state, then advanced the generation after cleanup).
                inner.signals.push(HostSignal::Adapter(
                    snapshot,
                    updated_at,
                    inner.central.attachment(),
                ));
                inner.radio.push_event(RadioEvent::AdapterState(state))
            }
            RadioIngress::ScanFailed { detail } => {
                inner.signals.push(HostSignal::ScanFailed(detail));
                Ok(())
            }
            RadioIngress::SecurityChanged { peer_id, state } => {
                lock(&inner.security_failures).remove(&Some(peer_id.clone()));
                lock(&inner.security_failures).remove(&None);
                lock(&inner.security).insert(peer_id.clone(), state.clone());
                inner.signals.push(HostSignal::Security(peer_id, state));
                Ok(())
            }
            RadioIngress::SecurityFailed { peer_id, failure } => {
                let error =
                    failure.to_error(crate::radio::RequestKind::SecurityState, inner.platform);
                if let Some(peer) = &peer_id {
                    lock(&inner.security).remove(peer);
                } else {
                    lock(&inner.security).clear();
                }
                let revision = inner
                    .security_failure_revision
                    .fetch_add(1, Ordering::SeqCst);
                lock(&inner.security_failures).insert(peer_id.clone(), (revision, error.clone()));
                inner
                    .signals
                    .push(HostSignal::SecurityFailed(peer_id, error));
                Ok(())
            }
            RadioIngress::WriteReadiness { peer_id, ready } => inner
                .radio
                .push_event(RadioEvent::WriteReadiness { peer_id, ready }),
            RadioIngress::Restored { peers } => {
                {
                    let mut restored = lock(&inner.restored);
                    for peer in &peers {
                        restored.insert(peer.peer_id.clone(), peer.clone());
                    }
                }
                inner.signals.push(HostSignal::Restored(peers));
                Ok(())
            }
            RadioIngress::Dropped { class, .. } => {
                inner.radio.note_drop(class);
                Err(class)
            }
        };
        match pushed {
            Ok(()) => IngressStatus::Accepted,
            Err(class) => IngressStatus::Dropped(class),
        }
    }

    /// Consumers polled by the pump, in order. Debug tests use it.
    #[cfg(debug_assertions)]
    pub fn route_turns(&self) -> Vec<String> {
        lock(&self.inner.route_turns).clone()
    }

    /// The consumers whose group each value scope turn committed, in order.
    /// Debug tests use it.
    #[cfg(debug_assertions)]
    pub fn scope_turns(&self) -> Vec<Vec<String>> {
        lock(&self.inner.scope_turns).clone()
    }

    /// Open one session lease (one RN manager) that is its own background
    /// scope: `session.dispose` releases its background leases.
    pub fn open_session(&self, owner: &str) -> Result<MobileSession, DesktopError> {
        self.open_session_in(owner, None, Arc::clone(&self.inner.wake))
    }

    /// Native intake is consumed through an explicit claim, not a JavaScript
    /// wake route. Sharing the public wake sink would create orphan early wakes.
    pub(crate) fn open_native_session(&self, owner: &str) -> Result<MobileSession, DesktopError> {
        struct ClaimWake;
        impl WakeSink for ClaimWake {
            fn wake(&self, _: u64) {}
        }
        self.open_session_in(owner, None, Arc::new(ClaimWake))
    }

    /// Open one session lease whose background leases belong to the shared
    /// `scope` (one React Native module instance, 87/N8): they outlive the
    /// session and end with [`Self::release_background_scope`] or host
    /// shutdown.
    pub fn open_scoped_session(
        &self,
        owner: &str,
        scope: &str,
    ) -> Result<MobileSession, DesktopError> {
        if scope.is_empty() {
            return Err(wire::invalid("session.scope"));
        }
        self.open_session_in(owner, Some(scope), Arc::clone(&self.inner.wake))
    }

    fn open_session_in(
        &self,
        owner: &str,
        scope: Option<&str>,
        wake: Arc<dyn WakeSink>,
    ) -> Result<MobileSession, DesktopError> {
        if owner.is_empty() {
            return Err(wire::invalid("session.owner"));
        }
        if self.inner.is_shut_down() {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                "ubm-mobile.session.open",
            ));
        }
        #[cfg(test)]
        if let Some(hook) = lock(&self.inner.before_session_admission).clone() {
            hook();
        }
        // The same guard fences the fatal source transition and publication.
        // An open already parsing options cannot publish after its snapshot.
        let mut sessions = lock(&self.inner.sessions);
        if self.inner.is_shut_down() {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                "ubm-mobile.session.open",
            ));
        }
        let id = self.inner.next_session.fetch_add(1, Ordering::Relaxed);
        let background_scope = scope.map_or(BackgroundScope::Session(id), |scope| {
            BackgroundScope::Shared(scope.to_owned())
        });
        let state = Arc::new(SessionState::new(
            id,
            Outbox::new(id, wake),
            background_scope,
        ));
        sessions.insert(id, Arc::clone(&state));
        Ok(MobileSession::new(Arc::clone(&self.inner), state))
    }

    /// Open a session after checking the caller speaks this wire revision
    /// (`protocol.incompatible` before any session exists otherwise), and
    /// answer the admission record `{sessionId, contractRevision,
    /// wireRevision, buildIdentity}` in one step. `build_identity_json` is
    /// the binding's `ubm-native-build-identity/1` record.
    pub fn admit_session(
        &self,
        owner: &str,
        expected_wire_revision: &str,
        build_identity_json: &str,
    ) -> Result<(MobileSession, String), DesktopError> {
        self.admit(owner, None, expected_wire_revision, build_identity_json)
    }

    /// [`Self::admit_session`] for a session of the shared background
    /// `scope` (see [`Self::open_scoped_session`]).
    pub fn admit_scoped_session(
        &self,
        owner: &str,
        scope: &str,
        expected_wire_revision: &str,
        build_identity_json: &str,
    ) -> Result<(MobileSession, String), DesktopError> {
        if scope.is_empty() {
            return Err(wire::invalid("session.scope"));
        }
        self.admit(
            owner,
            Some(scope),
            expected_wire_revision,
            build_identity_json,
        )
    }

    fn admit(
        &self,
        owner: &str,
        scope: Option<&str>,
        expected_wire_revision: &str,
        build_identity_json: &str,
    ) -> Result<(MobileSession, String), DesktopError> {
        if expected_wire_revision != wire::WIRE_REVISION {
            return Err(DesktopError::new(
                BleErrorCode::ProtocolIncompatible,
                BleErrorDomain::Core,
                "ubm-mobile.session.open",
            )
            .with_detail(format!(
                "caller speaks {expected_wire_revision}, owner speaks {}",
                wire::WIRE_REVISION
            )));
        }
        let identity: Value = serde_json::from_str(build_identity_json).map_err(|_| {
            DesktopError::new(
                BleErrorCode::ProtocolMalformed,
                BleErrorDomain::Core,
                "ubm-mobile.session.build-identity",
            )
        })?;
        let session = self.open_session_in(owner, scope, Arc::clone(&self.inner.wake))?;
        let record = object(vec![
            ("sessionId", Value::from(session.id())),
            (
                "contractRevision",
                Value::from(ubm_core::contracts::CONTRACT_REVISION),
            ),
            ("wireRevision", Value::from(wire::WIRE_REVISION)),
            ("buildIdentity", identity),
        ]);
        Ok((session, record.to_string()))
    }

    /// Look up a live session by id (binding handle tables).
    #[must_use]
    pub fn session(&self, id: u64) -> Option<MobileSession> {
        self.inner
            .session(id)
            .map(|state| MobileSession::new(Arc::clone(&self.inner), state))
    }

    /// Explicit process-owner shutdown: dispose every session, shut the
    /// central down, fail every waiting request. Returns the cleanup
    /// record JSON (`released` / `release-failed` with every failure).
    pub async fn shutdown(&self) -> String {
        let inner = &*self.inner;
        {
            let _admission = lock(&inner.continuation_admission);
            inner.continuation_closed.store(true, Ordering::SeqCst);
        }
        if let Some(executor) = inner.continuation.get() {
            executor.stop_recovery();
        }
        let mut failures = Vec::new();
        for state in inner.session_list() {
            let session = MobileSession::new(Arc::clone(&self.inner), state);
            failures.extend(session.dispose_failures().await);
        }
        let scopes: Vec<BackgroundScope> = lock(&inner.background).keys().cloned().collect();
        for scope in scopes {
            failures.extend(inner.release_background_scope(&scope).await);
        }
        inner.shut_down.store(true, Ordering::SeqCst);
        let report = inner.central.shutdown().await;
        let central_released = report.is_released();
        failures.extend(shutdown_failures(report));
        if let Some(error) = inner.radio.close_error() {
            failures.push(crate::session::cleanup_failure("radio", &error));
        }
        inner.radio.close_events();
        inner.radio.abandon_pending();
        inner.signals.close();
        let worker = lock(&inner.pump).take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }
        let mut record = crate::session::cleanup_record(failures);
        if !central_released {
            record["state"] = Value::from("release-failed");
        }
        record.to_string()
    }

    /// End a shared background scope (React Native module invalidation):
    /// release every foreground-service lease its sessions acquired.
    /// Returns the cleanup record JSON; a lease whose release failed stays
    /// held and a second call retries it.
    pub async fn release_background_scope(&self, scope: &str) -> String {
        let failures = self
            .inner
            .release_background_scope(&BackgroundScope::Shared(scope.to_owned()))
            .await;
        crate::session::cleanup_record(failures).to_string()
    }

    /// Blocking form of [`Self::release_background_scope`] for native
    /// threads.
    #[must_use]
    pub fn release_background_scope_blocking(&self, scope: &str) -> String {
        let (tx, rx) = std::sync::mpsc::channel();
        let host = self.clone();
        let scope = scope.to_owned();
        self.inner
            .runtime
            .spawn(async move { host.release_background_scope(&scope).await }.then_send(tx));
        rx.recv().unwrap_or_else(|_| {
            crate::session::cleanup_record(vec![crate::session::cleanup_failure(
                "background",
                &DesktopError::new(
                    BleErrorCode::LifecycleInvariantViolation,
                    BleErrorDomain::Cleanup,
                    "ubm-mobile.host.release-background-scope",
                ),
            )])
            .to_string()
        })
    }

    /// Blocking form of [`Self::shutdown`] for native threads.
    #[must_use]
    pub fn shutdown_blocking(&self) -> String {
        let (tx, rx) = std::sync::mpsc::channel();
        let host = self.clone();
        self.inner
            .runtime
            .spawn(async move { host.shutdown().await }.then_send(tx));
        rx.recv().unwrap_or_else(|_| {
            crate::session::cleanup_record(vec![crate::session::cleanup_failure(
                "central",
                &DesktopError::new(
                    BleErrorCode::LifecycleInvariantViolation,
                    BleErrorDomain::Cleanup,
                    "ubm-mobile.host.shutdown",
                ),
            )])
            .to_string()
        })
    }

    /// Radio counters (tests and diagnostics).
    #[must_use]
    pub fn radio_counters(&self) -> crate::foreign::RadioCounters {
        self.inner.radio.counters()
    }
}

/// Send a future's output over a std channel when it completes.
trait ThenSend: std::future::Future + Sized {
    fn then_send(
        self,
        tx: std::sync::mpsc::Sender<Self::Output>,
    ) -> impl std::future::Future<Output = ()> + Send
    where
        Self: Send,
        Self::Output: Send,
    {
        async move {
            let _ = tx.send(self.await);
        }
    }
}

impl<F: std::future::Future> ThenSend for F {}

fn shutdown_failures(report: ubm_desktop::ShutdownReport) -> Vec<Value> {
    let mut failures = Vec::new();
    for failure in report.radio_close_failures {
        failures.push(crate::session::cleanup_failure(
            "subscription",
            &DesktopError::new(
                BleErrorCode::GattSubscribeFailed,
                BleErrorDomain::Cleanup,
                "radio.close",
            )
            .with_detail(failure.detail),
        ));
    }
    for error in report.half_open_close_failures {
        failures.push(crate::session::cleanup_failure("connection", &error));
    }
    for error in report.transport_close_failures {
        failures.push(crate::session::cleanup_failure("backend", &error));
    }
    if let Some(error) = report.scan_stop_failure {
        failures.push(crate::session::cleanup_failure("scan", &error));
    }
    match report.record {
        Ok(record) => {
            for failure in record.failures() {
                failures.push(crate::session::cleanup_failure(
                    failure.resource_kind(),
                    &DesktopError::new(failure.code(), BleErrorDomain::Cleanup, "central.destroy"),
                ));
            }
        }
        Err(error) => failures.push(crate::session::cleanup_failure("central", &error)),
    }
    failures
}

#[cfg(test)]
mod signal_tests {
    #[test]
    fn scan_deadline_marker_is_reserved_and_coalesced_under_control_pressure() {
        let signals = super::Signals::default();
        for sequence in 0..super::SIGNALS_CAP as u64 {
            signals.push(lifecycle(sequence));
        }
        for _ in 0..1000 {
            signals.push(super::HostSignal::ScanDeadlines);
        }
        assert_eq!(signals.queue_len(), super::SIGNALS_CAP + 1);
        let mut deadlines = 0;
        while let Some(signal) = signals.pop() {
            if matches!(signal, super::HostSignal::ScanDeadlines) {
                deadlines += 1;
            }
        }
        assert_eq!(deadlines, 1);
        assert_eq!(signals.take_overflow(), (0, [0, 0, 0]));
        signals.push(super::HostSignal::ScanDeadlines);
        assert!(matches!(
            signals.pop(),
            Some(super::HostSignal::ScanDeadlines)
        ));
    }

    #[tokio::test]
    async fn advertisement_batch_yields_to_deadline_without_losing_remaining_records() {
        let signals = super::Signals::default();
        signals.push(super::HostSignal::Advertisements);
        signals.push(super::HostSignal::ScanDeadlines);
        assert!(matches!(
            signals.pop(),
            Some(super::HostSignal::Advertisements)
        ));
        let mut source = 0..(super::ADVERTISEMENT_BATCH * 2 + 1);
        let mut delivered = Vec::new();
        let filled = super::pump_advertisement_batch(
            || std::future::ready(source.next()),
            |value| delivered.push(value),
        )
        .await;
        assert_eq!(delivered.len(), super::ADVERTISEMENT_BATCH);
        assert!(filled);
        signals.push(super::HostSignal::Advertisements);
        assert!(matches!(
            signals.pop(),
            Some(super::HostSignal::ScanDeadlines)
        ));
        while let Some(signal) = signals.pop() {
            assert!(matches!(signal, super::HostSignal::Advertisements));
            if super::pump_advertisement_batch(
                || std::future::ready(source.next()),
                |value| delivered.push(value),
            )
            .await
            {
                // A producer can concurrently queue the same marker. The
                // continuation must coalesce, not create duplicate turns.
                signals.push(super::HostSignal::Advertisements);
                signals.push(super::HostSignal::Advertisements);
                assert_eq!(signals.queue_len(), 1);
            }
        }
        assert_eq!(
            delivered,
            (0..(super::ADVERTISEMENT_BATCH * 2 + 1)).collect::<Vec<_>>()
        );
        assert_eq!(signals.take_overflow(), (0, [0, 0, 0]));
    }

    #[test]
    fn half_open_shutdown_receipt_preserves_native_cause_and_unrelated_failures() {
        let error = ubm_desktop::DesktopError::new(
            ubm_core::contracts::BleErrorCode::PlatformFailure,
            ubm_core::contracts::BleErrorDomain::Connection,
            "radio.disconnect.compensation",
        )
        .with_platform(
            ubm_desktop::PlatformDetail::new("android", "133").with_message("disconnect refused"),
        );
        let report = ubm_desktop::ShutdownReport {
            record: Ok(ubm_core::ownership::CleanupRecord::new(
                None,
                ubm_core::ownership::CleanupState::Released,
                vec![],
            )
            .unwrap()),
            radio_close_failures: vec![],
            transport_close_failures: vec![],
            half_open_close_failures: vec![error.clone()],
            destroy_steps: 1,
            scan_stop_failure: None,
        };
        let failures = super::shutdown_failures(report);
        assert_eq!(
            failures,
            vec![crate::session::cleanup_failure("connection", &error)]
        );
        let unrelated = ubm_core::ownership::CleanupFailure::new(
            "subscription".into(),
            ubm_core::contracts::BleErrorCode::GattSubscribeFailed,
        )
        .unwrap();
        let retry = ubm_desktop::ShutdownReport {
            record: Ok(ubm_core::ownership::CleanupRecord::new(
                None,
                ubm_core::ownership::CleanupState::ReleaseFailed,
                vec![unrelated],
            )
            .unwrap()),
            radio_close_failures: vec![],
            transport_close_failures: vec![],
            half_open_close_failures: vec![],
            destroy_steps: 1,
            scan_stop_failure: None,
        };
        let failures = super::shutdown_failures(retry);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["resourceKind"], "subscription");
        assert_eq!(failures[0]["code"], "gatt.subscribe-failed");
    }
    struct NoRadio;
    impl super::PlatformRadio for NoRadio {
        fn submit(&self, _: crate::RadioRequest) {
            panic!("admission test must not use radio");
        }
        fn cancel(&self, _: crate::RequestId) {}
    }
    struct NoWake;
    impl super::WakeSink for NoWake {
        fn wake(&self, _: u64) {}
    }
    #[tokio::test]
    async fn accepted_open_cannot_publish_after_host_closes_admission() {
        let host = super::MobileHost::open(
            std::sync::Arc::new(NoRadio),
            std::sync::Arc::new(NoWake),
            super::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "test".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let entered = std::sync::Mutex::new(Some(entered_tx));
        let release = std::sync::Arc::new(std::sync::Barrier::new(2));
        let waiting = release.clone();
        *super::lock(&host.inner.before_session_admission) = Some(std::sync::Arc::new(move || {
            entered.lock().unwrap().take().unwrap().send(()).unwrap();
            waiting.wait();
        }));
        let opening = host.clone();
        let result = tokio::task::spawn_blocking(move || opening.open_session("racing"));
        entered_rx.await.unwrap();
        host.inner
            .shut_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        release.wait();
        assert!(
            result.await.unwrap().is_err(),
            "accepted open published after source failure closed the owner"
        );
        assert!(super::lock(&host.inner.sessions).is_empty());
    }
    use super::*;
    use crate::radio::{AuthenticationState, EncryptionState, SecureConnectionsState};

    fn lifecycle(sequence: u64) -> HostSignal {
        HostSignal::Lifecycle(LifecycleEvent {
            sequence,
            peer_id: "AA:BB:CC:DD:EE:FF".to_owned(),
            peer_key: "key".to_owned(),
            connection_generation: None,
            database_generation: None,
            kind: LifecycleKind::LinkLost,
            platform: None,
        })
    }

    fn security(peer: &str, bonded: BondState) -> HostSignal {
        HostSignal::Security(
            peer.to_owned(),
            SecurityState {
                bond: bonded,
                encryption: EncryptionState::Unknown,
                authentication: AuthenticationState::Unknown,
                secure_connections: SecureConnectionsState::Unknown,
                pairing_possible: None,
            },
        )
    }

    fn scope(peer: &str) -> InstanceKey {
        (peer.to_owned(), "180d".to_owned(), 0, "2a37".to_owned(), 0)
    }

    fn refused(
        failure: ubm_desktop::continuation_outbox::DataIngressFailure,
        items: u64,
        bytes: u64,
    ) -> ubm_desktop::continuation_outbox::DataBatchRejection {
        ubm_desktop::continuation_outbox::DataBatchRejection {
            failure,
            items,
            bytes,
        }
    }

    /// Every value a refused group polled lands in its route terminal exactly
    /// once, under the frozen lifecycle names, with the core's own loss.
    #[test]
    fn refused_group_terminals_count_each_polled_value_once() {
        use ubm_desktop::continuation_outbox::DataIngressFailure as Failure;
        let storage = Failure::Storage {
            bytes: 10,
            error: ubm_desktop::continuation_journal::JournalError {
                kind: "storage.full",
                detail: "capacity",
                operation: "append",
                sqlite_extended_code: None,
                sqlite_code: None,
            },
        };
        assert_eq!(
            refusal_terminal(&refused(storage, 5, 70), None),
            Some(("closed", 5, 70))
        );
        assert_eq!(
            refusal_terminal(
                &refused(Failure::Overflow { bytes: 10 }, 3, 30),
                Some(("overflow", 4, 400))
            ),
            Some(("overflow", 7, 430))
        );
        // Already sealed: the cutoff counted the group and the terminal still
        // reports it as overflow, exactly as the queue-full cause does.
        assert_eq!(
            refusal_terminal(&refused(Failure::Sealed { bytes: 10 }, 3, 30), None),
            Some(("overflow", 3, 30))
        );
        // A stopped journal: the first value is the handoff cutoff itself.
        let stopped = Failure::Stopped { bytes: 10 };
        assert_eq!(
            refusal_terminal(&refused(stopped.clone(), 1, 10), None),
            None
        );
        assert_eq!(
            refusal_terminal(&refused(stopped.clone(), 4, 45), None),
            Some(("overflow", 3, 35))
        );
        assert_eq!(
            refusal_terminal(&refused(stopped, 2, 25), Some(("overflow", 1, 8))),
            Some(("overflow", 2, 23))
        );
    }

    /// X-R6: a stalled pump (no pops) plus a burst stays bounded, and every
    /// refused signal is counted exactly once.
    #[test]
    fn signal_burst_is_bounded() {
        let signals = Signals::default();
        for sequence in 0..5000 {
            signals.push(lifecycle(sequence));
        }
        let mut drained = 0u64;
        while signals.pop().is_some() {
            drained += 1;
        }
        let (lost, drops) = signals.take_overflow();
        assert!(
            drained <= SIGNALS_CAP as u64,
            "bounded retention, got {drained}"
        );
        assert_eq!(drained + lost, 5000, "exact accounting");
        assert_eq!(drops, [0, 0, 0]);
    }

    /// Current-state facts merge per scope: the latest wins, queued once.
    #[test]
    fn countable_classes_coalesce_per_scope() {
        let signals = Signals::default();
        signals.push(security("peer-a", BondState::NotBonded));
        signals.push(security("peer-a", BondState::Bonded));
        signals.push(security("peer-b", BondState::Bonded));
        signals.push(HostSignal::ScanFailed("one".to_owned()));
        signals.push(HostSignal::ScanFailed("two".to_owned()));
        signals.push(HostSignal::IngressDrop(IngressClass::Control, 2));
        signals.push(HostSignal::IngressDrop(IngressClass::Control, 3));
        signals.push(HostSignal::IngressDrop(IngressClass::Advertisement, 1));
        signals.push_value(scope("peer-a"));
        signals.push_value(scope("peer-a"));
        signals.push_value(scope("peer-b"));

        let mut seen = Vec::new();
        while let Some(signal) = signals.pop() {
            seen.push(signal);
        }
        // Two peers' security (latest per peer), one scan failure
        // (latest), two ingress-drop markers (merged per class), one value
        // marker for both dirty scopes.
        assert_eq!(seen.len(), 6);
        let security_a = seen
            .iter()
            .find(|signal| matches!(signal, HostSignal::Security(peer, _) if peer == "peer-a"))
            .expect("peer-a security");
        assert!(matches!(
            security_a,
            HostSignal::Security(_, state) if state.bond == BondState::Bonded
        ));
        let control = seen
            .iter()
            .find(|signal| matches!(signal, HostSignal::IngressDrop(class, _) if *class == IngressClass::Control))
            .expect("control marker");
        assert!(matches!(control, HostSignal::IngressDrop(_, 5)));
        let (lost, drops) = signals.take_overflow();
        assert_eq!((lost, drops), (0, [0, 0, 0]));
    }

    /// X-R6 follow-up: a stalled pump (no pops) plus far more distinct
    /// value scopes than the cap keeps the queue constantly bounded, and
    /// every dirty scope is still delivered — one shared marker, bounded
    /// batches, requeued while scopes remain. Nothing is stranded and
    /// nothing is silently dropped.
    #[test]
    fn distinct_value_scopes_stay_bounded_and_all_delivered() {
        let signals = Signals::default();
        let scopes = SIGNALS_CAP + 5000;
        for index in 0..scopes {
            signals.push_value(scope(&format!("peer-{index:05}")));
        }
        // Stalled: the whole burst costs one queue slot.
        assert_eq!(signals.queue_len(), 1);
        assert_eq!(signals.dirty_len(), scopes);
        // Pump simulation: each marker drains one bounded batch and
        // requeues while scopes remain; the queue never grows back.
        let mut delivered = HashSet::new();
        let mut markers = 0usize;
        while let Some(signal) = signals.pop() {
            assert!(
                matches!(signal, HostSignal::Values),
                "only the value marker waits while value scopes drain"
            );
            markers += 1;
            let batch = signals.take_value_batch(VALUE_SCOPE_BATCH);
            assert!(
                !batch.is_empty() && batch.len() <= VALUE_SCOPE_BATCH,
                "bounded non-empty batch per marker turn"
            );
            for scope in batch {
                assert!(delivered.insert(scope), "a scope drained twice");
            }
            signals.requeue_values_if_dirty();
            assert!(
                signals.queue_len() <= 1,
                "queue grew back mid-drain: {}",
                signals.queue_len()
            );
        }
        assert_eq!(delivered.len(), scopes, "every dirty scope drained");
        assert_eq!(signals.dirty_len(), 0, "no scope stranded dirty");
        assert!(
            markers * VALUE_SCOPE_BATCH >= scopes,
            "batching covered every scope in {markers} marker turns"
        );
        let (lost, drops) = signals.take_overflow();
        assert_eq!((lost, drops), (0, [0, 0, 0]));
    }

    /// A batch that stays busy is pushed behind the scopes still waiting.
    /// Hash-set iteration used to poll the same 32 and never reach the rest.
    #[test]
    fn busy_scopes_past_one_batch_each_get_a_turn() {
        let signals = Signals::default();
        let count = VALUE_SCOPE_BATCH + 8;
        let scopes: Vec<_> = (0..count)
            .map(|index| scope(&format!("peer-{index:05}")))
            .collect();
        for scope_key in &scopes {
            signals.push_value(scope_key.clone());
        }
        // Deadline and security sit behind the first value batch. They must
        // run before the requeued marker, while some scopes are still unseen.
        signals.push(HostSignal::ScanDeadlines);
        signals.push(security("peer-security", BondState::Bonded));
        let mut seen = HashSet::new();
        let mut saw_deadline = false;
        let mut saw_security = false;
        for _ in 0..(count + 2) {
            let signal = signals.pop().expect("queued signal");
            match signal {
                HostSignal::Values => {
                    let batch = signals.take_value_batch(VALUE_SCOPE_BATCH);
                    assert!(!batch.is_empty() && batch.len() <= VALUE_SCOPE_BATCH);
                    for scope_key in batch {
                        seen.insert(scope_key.clone());
                        signals.push_value(scope_key);
                    }
                    signals.requeue_values_if_dirty();
                }
                HostSignal::ScanDeadlines => {
                    assert!(
                        seen.len() < count,
                        "the deadline waited until every scope had a turn"
                    );
                    saw_deadline = true;
                }
                HostSignal::Security(_, state) => {
                    assert_eq!(state.bond, BondState::Bonded);
                    assert!(
                        seen.len() < count,
                        "security waited until every scope had a turn"
                    );
                    saw_security = true;
                }
                _ => panic!("unexpected signal while rotating busy scopes"),
            }
            if seen.len() == count && saw_deadline && saw_security {
                break;
            }
        }
        assert!(saw_deadline, "scan deadline never ran");
        assert!(saw_security, "security never ran");
        assert_eq!(
            seen.len(),
            count,
            "a busy first batch kept {count} scopes from all being polled; saw {}",
            seen.len()
        );
    }
}
