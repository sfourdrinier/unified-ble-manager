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
use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::Notify;
use tokio::time::Instant;
use ubm_core::central::{ConnectionState, PathSelector, canonical_uuid};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain, CommitState, MAX_TIMEOUT_MS};
use ubm_desktop::{
    Budget, CancelAck, DeliveryMode, DesktopCentral, DesktopError, DiscoveredPath, InstanceKey,
    LIVENESS_BACKSTOP_DETAIL, LIVENESS_OP, LinkRelease, ObservedDelivery, OpControl, OpTicket,
    PeerRecord,
};

use crate::drain::Outbox;
use crate::foreign::{ConnectStaging, DISPATCHED, OP_RADIO, OpRadio, lock};
use crate::host::{BackgroundScope, HostInner, Route, ScanMember, adapter_value, security_value};
use crate::radio::BackgroundKind;
use crate::radio::{
    AndroidScanOptions, BondState, ConnectionPriority, Instance, MobilePlatform, PairTransport,
    Phy, RadioCompletion, RadioRequest, ScanCallbackType, ScanMode,
};
use crate::wire::{self, Args, object, opt_text};

/// Completion callback: called exactly once with the envelope text.
pub type Completion = Box<dyn FnOnce(String) + Send + 'static>;

/// Every op the wire accepts, in documentation order.
pub const OPS: &[&str] = &[
    "adapter.state",
    "counters.describe",
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
    "connection.read-phy",
    "connection.request-phy",
    "connection.maximum-write-length",
    "security.state",
    "security.pair",
    "security.cancel-pairing",
    "gatt.discover",
    "gatt.read",
    "gatt.read-descriptor",
    "gatt.write",
    "gatt.write-descriptor",
    "gatt.subscribe",
    "gatt.unsubscribe",
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
    "session.dispose",
];

const WRITE_OPS: &[&str] = &["gatt.write", "gatt.write-descriptor"];
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

pub(crate) struct SessionState {
    pub id: u64,
    /// Process-unique instance identity, shared by clones, never reused.
    pub instance: usize,
    pub outbox: Outbox,
    ops: Mutex<OpTable>,
    idle: Notify,
    live_ops: AtomicU64,
    /// Radio progress of every live op except `counters.describe` (which
    /// never counts itself), keyed by a per-session sequence.
    op_radio: Mutex<HashMap<u64, Arc<OpRadio>>>,
    op_sequence: AtomicU64,
    leases: Mutex<HashMap<String, String>>,
    subscriptions: Mutex<HashMap<String, Subscription>>,
    /// Leases and subscriptions an adapter reset ended: the core forgot
    /// them, and their release answers `released`, as the legacy backends'
    /// adapter-loss cleanup left terminalized handles.
    reset_leases: Mutex<HashMap<String, String>>,
    reset_subscriptions: Mutex<HashMap<String, Subscription>>,
    scan: Mutex<ScanSlot>,
    scan_ordinal: AtomicU64,
    pub background_scope: BackgroundScope,
    closing: AtomicBool,
}

impl SessionState {
    pub(crate) fn new(id: u64, outbox: Outbox, background_scope: BackgroundScope) -> Self {
        Self {
            id,
            instance: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
            outbox,
            ops: Mutex::new(OpTable {
                live: HashMap::new(),
                highest_admission: 0,
                cancelled_ahead: BTreeSet::new(),
            }),
            idle: Notify::new(),
            live_ops: AtomicU64::new(0),
            op_radio: Mutex::new(HashMap::new()),
            op_sequence: AtomicU64::new(0),
            leases: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            reset_leases: Mutex::new(HashMap::new()),
            reset_subscriptions: Mutex::new(HashMap::new()),
            scan: Mutex::new(ScanSlot::Idle),
            scan_ordinal: AtomicU64::new(0),
            background_scope,
            closing: AtomicBool::new(false),
        }
    }

    /// The host ended this session's scan membership.
    pub(crate) fn clear_scan(&self, membership: &str) {
        let mut scan = lock(&self.scan);
        if scan.membership() == Some(membership) {
            *scan = ScanSlot::Idle;
        }
    }

    /// An adapter reset ended every link: the core cleared its connection
    /// records, including links already lost before the reset. Move all of
    /// this session's leases and subscriptions to the reset tables. Answers
    /// the routes (scope, consumer) the host drops.
    pub(crate) fn end_by_reset(&self) -> Vec<(InstanceKey, String)> {
        lock(&self.reset_leases).extend(lock(&self.leases).drain());
        let mut ended = lock(&self.reset_subscriptions);
        lock(&self.subscriptions)
            .drain()
            .map(|(consumer, subscription)| {
                let route = (subscription.scope.clone(), consumer.clone());
                ended.insert(consumer, subscription);
                route
            })
            .collect()
    }

    fn core_name(&self, name: &str) -> String {
        format!("s{}:{name}", self.id)
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

fn error(code: BleErrorCode, domain: BleErrorDomain, operation: &str) -> DesktopError {
    DesktopError::new(code, domain, operation)
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
                object(vec![
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
    pub(crate) fn new(host: Arc<HostInner>, state: Arc<SessionState>) -> Self {
        Self { host, state }
    }

    #[must_use]
    pub fn id(&self) -> u64 {
        self.state.id
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
        let received = Instant::now();
        let is_write = WRITE_OPS.contains(&op);
        let reject = |error: DesktopError, completion: Completion| {
            completion(wire::error_envelope(
                &error,
                is_write.then_some("not-dispatched"),
            ));
        };
        let Some(op) = OPS.iter().copied().find(|known| *known == op) else {
            return reject(wire::invalid("op"), completion);
        };
        // A disposed session still answers `counters.describe` (a read): it
        // reports what the session still holds — nothing after a clean
        // dispose — so a manager can confirm its own return to baseline.
        if self.state.closing.load(Ordering::SeqCst)
            && op != "session.dispose"
            && op != "counters.describe"
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
        let command = match self.parse(op, &args, admission, received) {
            Ok(command) => command,
            Err(error) => return reject(error, completion),
        };
        if op != "op.cancel" && command.operation_id.is_some() != admission.is_some() {
            return reject(wire::invalid("args.admission"), completion);
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
        self.state.live_ops.fetch_add(1, Ordering::SeqCst);
        let progress = Arc::new(OpRadio::default());
        let tracked = (!matches!(command.body, Body::Counters)).then(|| {
            let key = self.state.op_sequence.fetch_add(1, Ordering::Relaxed);
            lock(&self.state.op_radio).insert(key, Arc::clone(&progress));
            key
        });
        let session = self.clone();
        let ctl = OpControl::new(command.budget, ticket);
        self.host.runtime.spawn(async move {
            let operation_id = command.operation_id.clone();
            let (outcome, dispatched) = OP_RADIO
                .scope(
                    progress,
                    DISPATCHED.scope(Cell::new(false), async {
                        let outcome = session.execute(command.body, ctl).await;
                        (outcome, DISPATCHED.with(Cell::get))
                    }),
                )
                .await;
            if let Some(key) = tracked {
                lock(&session.state.op_radio).remove(&key);
            }
            let text = match outcome {
                Ok(value) => wire::ok_envelope(value),
                Err(error) => {
                    wire::error_envelope(&error, is_write.then(|| commit_of(&error, dispatched)))
                }
            };
            if let Some(id) = operation_id {
                lock(&session.state.ops).live.remove(&id);
            }
            completion(text);
            if session.state.live_ops.fetch_sub(1, Ordering::SeqCst) == 1 {
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
    ) -> Result<Command, DesktopError> {
        let platform = self.host.platform;
        let apple = platform == MobilePlatform::Apple;
        let id_required = |args: &Args| -> Result<Option<String>, DesktopError> {
            Ok(Some(args.string("operationId")?))
        };
        let (body, operation_id, budget) = match op {
            "adapter.state" | "counters.describe" | "peers.known" | "peers.connected"
            | "peers.restored" | "session.reconcile" | "session.dispose" => {
                args.exact(&[], &[])?;
                let body = match op {
                    "adapter.state" => Body::AdapterState,
                    "session.reconcile" => Body::Reconcile,
                    "counters.describe" => Body::Counters,
                    "peers.known" => Body::PeersKnown,
                    "peers.connected" => Body::PeersConnected,
                    "peers.restored" => Body::PeersRestored,
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
            "peers.bonded" => {
                args.exact(&["operationId"], &["budgetMs"])?;
                (
                    Body::PeersBonded,
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "peers.resolve" => {
                args.exact(&["reference"], &[])?;
                let reference = args.object("reference")?;
                reference.exact(&["opaqueId"], &["version", "backendId", "scope"])?;
                (
                    Body::PeersResolve(reference.string("opaqueId")?),
                    None,
                    Budget::unbounded(),
                )
            }
            "scan.start" => {
                args.exact(
                    &["serviceUuids", "duplicatePolicy", "operationId"],
                    &["deviceAddresses", "platform", "budgetMs"],
                )?;
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
                        // Legacy refused an Android scan PHY or batched
                        // report delay as unsupported (139, AN-1).
                        if options.get_raw("phy").is_some()
                            || options.get_raw("reportDelayMs").is_some()
                        {
                            return Err(error(
                                BleErrorCode::CapabilityUnsupported,
                                BleErrorDomain::Scan,
                                "scan.start.platform-options",
                            ));
                        }
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
                        service_uuids,
                        device_addresses,
                        android,
                    },
                    id_required(args)?,
                    budget(args, received)?,
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
                if intent == "when-available" && apple {
                    return Err(unsupported(
                        "connection.connect.when-available",
                        "CoreBluetooth has no autoConnect",
                    ));
                }
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
                            auto_connect: intent == "when-available",
                            preferred_phy,
                        },
                    },
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "connection.disconnect" => {
                args.exact(&["peerId", "lease"], &["budgetMs", "operationId"])?;
                (
                    Body::Disconnect {
                        peer_id: args.string("peerId")?,
                        lease: args.string("lease")?,
                    },
                    Self::operation_id(args)?,
                    budget(args, received)?,
                )
            }
            "connection.rssi"
            | "connection.effective-mtu"
            | "connection.request-mtu"
            | "connection.request-priority"
            | "connection.read-phy"
            | "connection.request-phy"
            | "connection.maximum-write-length" => {
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
                        "connection.effective-mtu"
                            | "connection.request-mtu"
                            | "connection.request-priority"
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
                    "connection.maximum-write-length" => Control::MaximumWriteLength(
                        args.one_of("mode", &["with-response", "without-response"])?
                            == "with-response",
                    ),
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
            "gatt.write" | "gatt.write-descriptor" => {
                args.exact(
                    &["peerId", "selector", "valueB64", "mode", "operationId"],
                    &["budgetMs"],
                )?;
                let descriptor = op == "gatt.write-descriptor";
                let mode = args.one_of("mode", &["with-response", "without-response"])?;
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
                    },
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "gatt.subscribe" => {
                args.exact(
                    &["peerId", "selector", "consumer", "operationId"],
                    &["deliveryMode", "budgetMs"],
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
                    },
                    id_required(args)?,
                    budget(args, received)?,
                )
            }
            "gatt.unsubscribe" => {
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
                args.exact(&[], &["name", "serviceUuid", "budgetMs", "operationId"])?;
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
                (
                    Body::CompanionAssociate {
                        name: args.opt_string("name")?,
                        service_uuid,
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
        match lock(&self.state.leases).get(peer_id) {
            Some(held) if *held == core_lease => Ok(core_lease),
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

    async fn execute(&self, body: Body, ctl: OpControl) -> Result<Value, DesktopError> {
        let host = &*self.host;
        let central = &host.central;
        match body {
            Body::AdapterState => {
                let snapshot =
                    bounded(&ctl, "adapter.state", host.radio.adapter_snapshot()).await?;
                let updated_at = host.now_ms();
                *lock(&host.adapter) = Some((snapshot.clone(), updated_at));
                Ok(adapter_value(&snapshot, updated_at, &central.attachment()))
            }
            Body::Counters => self.counters().await,
            Body::ScanStart {
                service_uuids,
                device_addresses,
                android,
            } => {
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
                    service_uuids,
                    device_addresses,
                };
                if let Err(error) = host.join_scan(self.state.id, member, android, ctl).await {
                    let mut slot = lock(&self.state.scan);
                    if slot.membership() == Some(membership.as_str()) {
                        *slot = ScanSlot::Idle;
                    }
                    return Err(error);
                }
                {
                    // Bind the comparison first: the borrow of the slot must
                    // not live across the `leave_scan` await below.
                    let ours = lock(&self.state.scan).membership() == Some(membership.as_str());
                    if ours {
                        *lock(&self.state.scan) = ScanSlot::Active {
                            membership: membership.clone(),
                        };
                    } else {
                        // The host ended our membership while we were
                        // joining (adapter loss, shutdown): release what the
                        // join admitted instead of reporting a live scan.
                        let _ = host.leave_scan(self.state.id, OpControl::unbounded()).await;
                        return Err(error(
                            BleErrorCode::ScanStartFailed,
                            BleErrorDomain::Scan,
                            "scan.start",
                        )
                        .with_detail("the host ended the scan while it was starting"));
                    }
                }
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
                host.leave_scan(self.state.id, ctl).await?;
                self.state.clear_scan(&membership);
                Ok(cleanup_record(Vec::new()))
            }
            Body::PeersResolve(peer_id) => {
                let known = lock(&host.directory).contains_key(&peer_id)
                    || lock(&host.restored).contains_key(&peer_id);
                let record = self.peer_record(&peer_id).await;
                if !known && record.is_none() {
                    return Ok(Value::Null);
                }
                Ok(host.peer_value(&peer_id, record.as_ref(), None))
            }
            Body::PeersKnown | Body::PeersConnected | Body::PeersRestored => {
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
                    .filter(|(record, _)| {
                        !matches!(body, Body::PeersConnected)
                            || record.and_then(|r| r.connection_state)
                                == Some(ConnectionState::Connected)
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
                            .and_modify(|state| state.bond = BondState::Bonded)
                            .or_insert(crate::radio::SecurityState {
                                bond: BondState::Bonded,
                                encryption: crate::radio::EncryptionState::Unknown,
                                authentication: crate::radio::AuthenticationState::Unknown,
                                secure_connections: crate::radio::SecureConnectionsState::Unknown,
                                pairing_possible: None,
                            });
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
                // The peer's connect section (X-R3): stage, dispatch and
                // cleanup run under mutual exclusion, so a concurrent
                // same-peer connect can neither overwrite this staging nor
                // wipe it in its own cleanup. A dead op never stages: the
                // wait is cancellation- and deadline-aware.
                let section = host.radio.lock_connect_section(&peer_id, &ctl).await?;
                host.radio.stage_connect(&peer_id, staging);
                let connected = central.connect(&peer_id, &core_lease, ctl).await;
                host.radio.clear_staging(Some(&peer_id), None, false);
                drop(section);
                let handle = connected?;
                let generation = handle.connection_generation.ok_or_else(|| {
                    error(
                        BleErrorCode::LifecycleInvariantViolation,
                        BleErrorDomain::Core,
                        "connection.connect",
                    )
                    .with_detail("connected without a connection generation")
                })?;
                lock(&self.state.reset_leases).remove(&peer_id);
                lock(&self.state.leases).insert(peer_id.clone(), core_lease);
                host.note_peer(&peer_id, None, "app-reference");
                Ok(object(vec![
                    ("peerKey", Value::from(handle.peer_key)),
                    ("connectionGeneration", Value::from(generation.as_str())),
                ]))
            }
            Body::Disconnect { peer_id, lease } => {
                let core_lease = self.state.core_name(&lease);
                {
                    let mut ended = lock(&self.state.reset_leases);
                    if ended.get(&peer_id) == Some(&core_lease) {
                        ended.remove(&peer_id);
                        return Ok(cleanup_record(Vec::new()));
                    }
                }
                match central.disconnect(&peer_id, &core_lease, ctl).await {
                    Ok(LinkRelease::Released | LinkRelease::AlreadyReleased) => {
                        lock(&self.state.leases).remove(&peer_id);
                        Ok(cleanup_record(Vec::new()))
                    }
                    Err(error) => Ok(cleanup_record(vec![cleanup_failure("connection", &error)])),
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
                lock(&host.security).insert(peer_id, state.clone());
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
                    central.write(&peer_id, &selector, value, mode, ctl).await?;
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
            } => self.subscribe(peer_id, selector, consumer, mode, ctl).await,
            Body::Unsubscribe {
                peer_id,
                selector,
                consumer,
            } => {
                let reset = lock(&self.state.reset_subscriptions)
                    .get(&consumer)
                    .is_some_and(|s| {
                        s.peer_id == peer_id && s.scope == scope_of(&peer_id, &selector)
                    });
                if reset {
                    lock(&self.state.reset_subscriptions).remove(&consumer);
                    return Ok(object(vec![
                        ("state", Value::from("released")),
                        // The reset's cleanup disabled it, not this call.
                        ("physicalDisabled", Value::Bool(false)),
                    ]));
                }
                let subscription = lock(&self.state.subscriptions).get(&consumer).cloned();
                let Some(subscription) = subscription
                    .filter(|s| s.peer_id == peer_id && s.scope == scope_of(&peer_id, &selector))
                else {
                    return Err(error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Gatt,
                        "gatt.unsubscribe",
                    )
                    .with_detail("no such consumer in this session"));
                };
                let physical_disabled = central
                    .unsubscribe(
                        &peer_id,
                        &subscription.selector,
                        &subscription.core_consumer,
                        ctl,
                    )
                    .await?;
                host.remove_route(&subscription.scope, self.state.id, &consumer);
                lock(&self.state.subscriptions).remove(&consumer);
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
            Body::CompanionAssociate { name, service_uuid } => {
                match awaited(
                    &ctl,
                    "companion.associate",
                    host.radio.call(|id| RadioRequest::AssociateCompanion {
                        id,
                        name,
                        service_uuid,
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
            Body::Dispose => Ok(cleanup_record(self.release(1).await)),
        }
    }

    async fn security_state(
        &self,
        peer_id: &str,
        ctl: &OpControl,
    ) -> Result<crate::radio::SecurityState, DesktopError> {
        let peer = peer_id.to_owned();
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
                lock(&self.host.security).insert(peer_id.to_owned(), state.clone());
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
        self.require_lease(peer_id, lease, operation)?;
        self.require_connected(peer_id, operation).await?;
        let peer = peer_id.to_owned();
        let completion = bounded(
            &ctl,
            operation,
            host.radio.call(move |id| match control {
                // Rssi and MaximumWriteLength returned above through the core.
                Control::EffectiveMtu | Control::Rssi | Control::MaximumWriteLength(_) => {
                    RadioRequest::ReadMtu { id, peer_id: peer }
                }
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
            (Control::Priority(_), RadioCompletion::Accepted(accepted)) => {
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
        ctl: OpControl,
    ) -> Result<Value, DesktopError> {
        let host = &*self.host;
        if lock(&self.state.subscriptions).contains_key(&consumer) {
            return Err(wire::invalid("args.consumer").with_detail("consumer already subscribed"));
        }
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
        if host.platform == MobilePlatform::Android
            && let Some(preferred) = preferred
        {
            host.radio
                .stage_preference(instance_of(&peer_id, &selector), preferred);
        }
        let core_consumer = self.state.core_name(&consumer);
        let subscribed = host
            .central
            .subscribe(&peer_id, &selector, &core_consumer, radio_requirement, ctl)
            .await;
        host.radio
            .clear_staging(None, Some(&instance_of(&peer_id, &selector)), false);
        let observed = subscribed?;
        let delivery = match host.platform {
            MobilePlatform::Android => observed.as_str(),
            MobilePlatform::Apple => ObservedDelivery::Unknown.as_str(),
        };
        lock(&self.state.subscriptions).insert(
            consumer.clone(),
            Subscription {
                peer_id: peer_id.clone(),
                selector: selector.clone(),
                core_consumer: core_consumer.clone(),
                scope: scope.clone(),
            },
        );
        host.add_route(
            scope.clone(),
            Route {
                session_id: self.state.id,
                consumer: consumer.clone(),
                core_consumer,
                peer_id,
                selector,
                delivery,
                terminal: None,
            },
        );
        // Values the hub admitted while the enable was in flight are
        // flushed now instead of waiting for the next notification.
        host.kick(scope);
        Ok(object(vec![
            ("consumer", Value::from(consumer)),
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
                        let (reason, items, bytes) = ended.flatten().unwrap_or(("closed", 0, 0));
                        object(vec![
                            ("consumer", Value::from(consumer)),
                            ("state", Value::from("ended")),
                            ("reason", Value::from(reason)),
                            ("droppedItems", Value::from(items)),
                            ("droppedBytes", Value::from(bytes)),
                        ])
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
                        ("state", security_value(&known[peer_id])),
                    ])
                })
                .collect()
        };
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
            ("restored", Value::Array(restored)),
            ("scan", opt_text(scan.as_deref())),
        ]))
    }

    /// Release everything this session holds on the shared owner. Failed
    /// releases stay held (a second dispose retries them).
    pub(crate) async fn dispose_failures(&self) -> Vec<Value> {
        self.release(0).await
    }

    async fn release(&self, own_ops: u64) -> Vec<Value> {
        let host = &*self.host;
        self.state.closing.store(true, Ordering::SeqCst);
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
            if self.state.live_ops.load(Ordering::SeqCst) <= own_ops {
                break;
            }
            idle.await;
        }
        let mut failures = Vec::new();
        let membership = lock(&self.state.scan).membership().map(str::to_owned);
        if let Some(membership) = membership {
            match host.leave_scan(self.state.id, OpControl::unbounded()).await {
                Ok(()) => self.state.clear_scan(&membership),
                Err(error) => failures.push(cleanup_failure("scan", &error)),
            }
        }
        let subscriptions: Vec<(String, Subscription)> = lock(&self.state.subscriptions)
            .iter()
            .map(|(consumer, subscription)| (consumer.clone(), subscription.clone()))
            .collect();
        for (consumer, subscription) in subscriptions {
            match host
                .central
                .unsubscribe(
                    &subscription.peer_id,
                    &subscription.selector,
                    &subscription.core_consumer,
                    OpControl::unbounded(),
                )
                .await
            {
                Ok(_) => {
                    host.remove_route(&subscription.scope, self.state.id, &consumer);
                    lock(&self.state.subscriptions).remove(&consumer);
                }
                Err(error) => failures.push(cleanup_failure("subscription", &error)),
            }
        }
        let leases: Vec<(String, String)> = lock(&self.state.leases)
            .iter()
            .map(|(peer, lease)| (peer.clone(), lease.clone()))
            .collect();
        for (peer_id, lease) in leases {
            match host
                .central
                .disconnect(&peer_id, &lease, OpControl::unbounded())
                .await
            {
                Ok(_) => {
                    lock(&self.state.leases).remove(&peer_id);
                }
                Err(error) => failures.push(cleanup_failure("connection", &error)),
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
        if failures.is_empty() {
            host.remove_session(self.state.id);
        }
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
    ReadPhy,
    RequestPhy(Option<Phy>, Option<Phy>),
    /// `true` = with response.
    MaximumWriteLength(bool),
}

impl Control {
    const fn operation(self) -> &'static str {
        match self {
            Self::Rssi => "connection.rssi",
            Self::EffectiveMtu => "connection.effective-mtu",
            Self::RequestMtu(_) => "connection.request-mtu",
            Self::Priority(_) => "connection.request-priority",
            Self::ReadPhy => "connection.read-phy",
            Self::RequestPhy(..) => "connection.request-phy",
            Self::MaximumWriteLength(_) => "connection.maximum-write-length",
        }
    }
}

enum Body {
    AdapterState,
    Counters,
    ScanStart {
        service_uuids: Vec<String>,
        device_addresses: Vec<String>,
        android: Option<AndroidScanOptions>,
    },
    ScanStop(String),
    PeersResolve(String),
    PeersKnown,
    PeersConnected,
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
    },
    Subscribe {
        peer_id: String,
        selector: PathSelector,
        consumer: String,
        mode: Option<&'static str>,
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

struct Command {
    body: Body,
    operation_id: Option<String>,
    budget: Budget,
}
