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
use ubm_core::contracts::{BleErrorCode, BleErrorDomain, CoreError, OperationId};
use ubm_desktop::{
    CentralProfile, CentralSignal, DesktopCentral, DesktopError, InstanceKey, LifecycleEvent,
    LifecycleKind, NotificationPoll, OpControl, PeerRecord, PeerSnapshot, RadioEvent,
};

use crate::drain::Outbox;
use crate::foreign::{ForeignRadio, lock, power_state};
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
    /// Attachment owner label (e.g. the app bundle id). Non-empty.
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
    pub service_uuids: Vec<String>,
    pub device_addresses: Vec<String>,
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

enum HostSignal {
    Advertisements,
    Value(InstanceKey),
    Lifecycle(LifecycleEvent),
    Adapter(AdapterSnapshot, u64),
    ScanFailed(String),
    Security(String, SecurityState),
    Restored(Vec<RestoredPeer>),
    IngressDrop(IngressClass),
}

#[derive(Default)]
struct SignalState {
    queue: VecDeque<HostSignal>,
    dirty: HashSet<InstanceKey>,
    advertisements_pending: bool,
    closed: bool,
}

/// Ordered signal queue between the central (and ingress) and the pump.
/// Value and advertisement signals coalesce per scope: the pump polls the
/// core's bounded queues, so one pending marker per scope is enough.
#[derive(Default)]
struct Signals {
    state: Mutex<SignalState>,
    notify: Notify,
}

impl Signals {
    fn push(&self, signal: HostSignal) {
        {
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            match &signal {
                HostSignal::Value(scope) => {
                    if !state.dirty.insert(scope.clone()) {
                        return;
                    }
                }
                HostSignal::Advertisements => {
                    if state.advertisements_pending {
                        return;
                    }
                    state.advertisements_pending = true;
                }
                _ => {}
            }
            state.queue.push_back(signal);
        }
        self.notify.notify_one();
    }

    fn pop(&self) -> Option<HostSignal> {
        let mut state = lock(&self.state);
        let signal = state.queue.pop_front()?;
        match &signal {
            HostSignal::Value(scope) => {
                state.dirty.remove(scope);
            }
            HostSignal::Advertisements => state.advertisements_pending = false,
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
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    clock: Instant,
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

pub(crate) fn advertisement_record(snapshot: &PeerSnapshot, observed_at_ms: u64) -> Value {
    object(vec![
        ("t", Value::from("adv")),
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
    ])
}

pub(crate) fn adapter_value(
    snapshot: &AdapterSnapshot,
    updated_at: u64,
    central: &DesktopCentral<ForeignRadio>,
) -> Value {
    let attachment = central.attachment();
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

    async fn handle(&self, signal: HostSignal) {
        match signal {
            HostSignal::Advertisements => {
                while let Some(snapshot) = self.central.take_advertisement().await {
                    self.route_advertisement(&snapshot);
                }
            }
            HostSignal::Value(scope) => self.flush_scope(&scope).await,
            HostSignal::Lifecycle(event) => self.route_lifecycle(event).await,
            HostSignal::Adapter(snapshot, updated_at) => {
                let record = object(vec![
                    ("t", Value::from("adapter")),
                    ("state", adapter_value(&snapshot, updated_at, &self.central)),
                ]);
                self.broadcast(&record);
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
            HostSignal::IngressDrop(class) => {
                for session in self.session_list() {
                    session.outbox.push_ingress_drop(class);
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
            }
            entry.rssi = snapshot.rssi.or(entry.rssi);
            entry.last_seen_ms = Some(now);
        }
        let members: Vec<(u64, ScanMember)> = lock(&self.scan_members)
            .iter()
            .map(|(id, member)| (*id, member.clone()))
            .collect();
        let record = advertisement_record(snapshot, now);
        for (session_id, member) in members {
            if !matches_member(&member, snapshot) {
                continue;
            }
            if let Some(session) = self.session(session_id)
                && session.outbox.push_data(record.clone()).is_err()
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

    /// Move every value the core holds for `scope` into the owning
    /// sessions' outboxes, ending streams on terminal answers.
    async fn flush_scope(&self, scope: &InstanceKey) {
        for route in self.live_routes(scope) {
            if let Some(terminal) = self.drain_route(&route).await {
                self.mark_ended(scope, &route, terminal);
                self.end_route(&route, terminal);
            }
        }
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

    /// Move every value the core holds for one consumer into its session's
    /// outbox; answer the terminal that ended the stream, if any, without
    /// emitting it (the caller orders it against lifecycle records).
    async fn drain_route(&self, route: &Route) -> Option<StreamEnd> {
        loop {
            let poll = self
                .central
                .poll_notification(&route.peer_id, &route.selector, &route.core_consumer)
                .await;
            return match poll {
                Ok(NotificationPoll::Value(bytes)) => {
                    let Some(session) = self.session(route.session_id) else {
                        continue;
                    };
                    let record = object(vec![
                        ("t", Value::from("value")),
                        ("consumer", Value::from(route.consumer.as_str())),
                        ("valueB64", Value::from(wire::encode_base64(&bytes))),
                        ("delivery", Value::from(route.delivery)),
                    ]);
                    match session.outbox.push_data(record) {
                        Ok(()) => continue,
                        Err(overflow) => Some(("overflow", 1, overflow.bytes as u64)),
                    }
                }
                Ok(NotificationPoll::Empty) => None,
                Ok(NotificationPoll::Terminal(terminal)) => Some((
                    "overflow",
                    terminal.dropped_items(),
                    terminal.dropped_bytes(),
                )),
                Ok(NotificationPoll::Invalidated(_)) => Some(("invalidated", 0, 0)),
                Ok(NotificationPoll::Closed) | Err(_) => Some(("closed", 0, 0)),
            };
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
            for route in self.live_routes(scope) {
                if let Some(terminal) = self.drain_route(&route).await {
                    self.mark_ended(scope, &route, terminal);
                    ended.push((route, terminal));
                }
            }
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
            self.flush_scope(scope).await;
        }
    }

    async fn scan_failed(&self, detail: String) {
        let members: Vec<(u64, ScanMember)> = std::mem::take(&mut *lock(&self.scan_members))
            .into_iter()
            .collect();
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
        for (session_id, member) in members {
            if let Some(session) = self.session(session_id) {
                session.clear_scan(&member.membership);
                session.outbox.push_control(object(vec![
                    ("t", Value::from("scan-end")),
                    ("operationId", Value::from(member.membership.as_str())),
                    ("reason", Value::from("source-failed")),
                ]));
            }
        }
    }

    /// Start, join or widen the shared physical scan for one member.
    pub(crate) async fn join_scan(
        &self,
        session_id: u64,
        member: ScanMember,
        android: Option<AndroidScanOptions>,
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        let mut share = self.scan.lock().await;
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
        let started = self
            .central
            .start_scan(&format!("ubm-mobile-scan-{session_id}"), &filter, ctl)
            .await;
        self.radio.clear_staging(None, None, true);
        match started {
            Ok(session) => {
                share.physical = Some(PhysicalScan {
                    operation: session.operation_id().clone(),
                    request: wanted,
                });
                lock(&self.scan_members).insert(session_id, member);
                Ok(())
            }
            Err(error) => {
                // A failed restart leaves the previous members without a
                // radio scan: end their streams instead of pretending.
                let orphans: Vec<(u64, ScanMember)> =
                    std::mem::take(&mut *lock(&self.scan_members))
                        .into_iter()
                        .collect();
                for (orphan, member) in orphans {
                    if let Some(session) = self.session(orphan) {
                        session.clear_scan(&member.membership);
                        session.outbox.push_control(object(vec![
                            ("t", Value::from("scan-end")),
                            ("operationId", Value::from(member.membership.as_str())),
                            ("reason", Value::from("source-failed")),
                        ]));
                    }
                }
                Err(error)
            }
        }
    }

    /// Leave the shared scan; the last member stops the physical scan. A
    /// failed stop keeps the membership so the caller can retry.
    pub(crate) async fn leave_scan(
        &self,
        session_id: u64,
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        let mut share = self.scan.lock().await;
        let last = {
            let members = lock(&self.scan_members);
            members.len() == 1 && members.contains_key(&session_id)
        };
        if last && let Some(physical) = &share.physical {
            let operation = physical.operation.clone();
            self.central.stop_scan(&operation, ctl).await?;
            share.physical = None;
        }
        lock(&self.scan_members).remove(&session_id);
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
        self.signals.push(HostSignal::Value(scope));
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
            host.handle(signal).await;
        }
        if signals.is_closed() {
            return;
        }
        signals.notify.notified().await;
    }
}

impl MobileHost {
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
        let radio = ForeignRadio::new(platform, options.platform, options.adapter_label.clone());
        let signals = Arc::new(Signals::default());
        let observer_signals = Arc::clone(&signals);
        let observer: ubm_desktop::CentralObserver = Arc::new(move |signal| match signal {
            CentralSignal::Advertisement(_) => observer_signals.push(HostSignal::Advertisements),
            CentralSignal::Value { scope, .. } => observer_signals.push(HostSignal::Value(scope)),
            CentralSignal::Lifecycle(event) => {
                observer_signals.push(HostSignal::Lifecycle(event));
            }
            // Adapter records come from the platform's full snapshot
            // (authorization, resetting), not the power-only projection.
            // The mobile radio does not opt into the core's adapter-loss
            // teardown (`tears_down_on_adapter_loss` keeps its default), so
            // the core never emits a reset for it: the platform's own
            // adapter facts drive the `adapter` records.
            CentralSignal::Adapter(_) | CentralSignal::AdapterReset(_) => {}
            // The platform delivers these facts to the host directly as
            // ingress (`SecurityChanged` → `security` record, `ScanFailed` →
            // `scan-end`) and never hands them to the central as radio
            // events, so the central has none to signal here; the mobile
            // radio reports no write readiness (Apple readiness is read per
            // write through `ReadWriteLimits`/the adapter).
            CentralSignal::Security(_)
            | CentralSignal::WriteReadiness(_)
            | CentralSignal::ScanTerminal(_) => {}
        });
        let drop_signals = Arc::clone(&signals);
        radio.set_drop_hook(Arc::new(move |class| {
            drop_signals.push(HostSignal::IngressDrop(class));
        }));
        let profile = CentralProfile {
            owner: options.owner.clone(),
            backend_label: format!("mobile-{}", options.platform.as_str()),
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
            scan: tokio::sync::Mutex::new(ScanShare::default()),
            scan_members: Mutex::new(BTreeMap::new()),
            routes: Mutex::new(HashMap::new()),
            directory: Mutex::new(BTreeMap::new()),
            restored: Mutex::new(BTreeMap::new()),
            restoration_claims: Mutex::new(BTreeMap::new()),
            security: Mutex::new(HashMap::new()),
            adapter: Mutex::new(None),
            background: Mutex::new(BTreeMap::new()),
            link_ends: Mutex::new(BTreeMap::new()),
            database_changes: Mutex::new(BTreeMap::new()),
            shut_down: AtomicBool::new(false),
            pump: Mutex::new(None),
            clock: Instant::now(),
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
        let (tx, rx) = std::sync::mpsc::channel();
        runtime.spawn(Self::open(platform, wake, options, runtime.clone()).then_send(tx));
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
                peer_id, connected, ..
            } => inner.radio.push_event(if connected {
                RadioEvent::Connected(peer_id)
            } else {
                RadioEvent::Disconnected(peer_id)
            }),
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
                let pushed = inner
                    .radio
                    .push_event(RadioEvent::AdapterState(power_state(&snapshot)));
                inner
                    .signals
                    .push(HostSignal::Adapter(snapshot, updated_at));
                pushed
            }
            RadioIngress::ScanFailed { detail } => {
                inner.signals.push(HostSignal::ScanFailed(detail));
                Ok(())
            }
            RadioIngress::SecurityChanged { peer_id, state } => {
                lock(&inner.security).insert(peer_id.clone(), state.clone());
                inner.signals.push(HostSignal::Security(peer_id, state));
                Ok(())
            }
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

    /// Open one session lease (one RN manager) that is its own background
    /// scope: `session.dispose` releases its background leases.
    pub fn open_session(&self, owner: &str) -> Result<MobileSession, DesktopError> {
        self.open_session_in(owner, None)
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
        self.open_session_in(owner, Some(scope))
    }

    fn open_session_in(
        &self,
        owner: &str,
        scope: Option<&str>,
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
        let id = self.inner.next_session.fetch_add(1, Ordering::Relaxed);
        let background_scope = scope.map_or(BackgroundScope::Session(id), |scope| {
            BackgroundScope::Shared(scope.to_owned())
        });
        let state = Arc::new(SessionState::new(
            id,
            Outbox::new(id, Arc::clone(&self.inner.wake)),
            background_scope,
        ));
        lock(&self.inner.sessions).insert(id, Arc::clone(&state));
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
        let session = self.open_session_in(owner, scope)?;
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
        if let Some(error) = inner.radio.close_error() {
            failures.push(crate::session::cleanup_failure("radio", &error));
        }
        if let Some(error) = report.scan_stop_failure {
            failures.push(crate::session::cleanup_failure("scan", &error));
        }
        match report.record {
            Ok(record) if matches!(record.state(), ubm_core::ownership::CleanupState::Released) => {
            }
            Ok(_) => failures.push(crate::session::cleanup_failure(
                "central",
                &DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "central.destroy",
                )
                .with_detail("the core destroy record reports release-failed"),
            )),
            Err(error) => failures.push(crate::session::cleanup_failure("central", &error)),
        }
        inner.radio.close_events();
        inner.radio.abandon_pending();
        inner.signals.close();
        let worker = lock(&inner.pump).take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }
        crate::session::cleanup_record(failures).to_string()
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
