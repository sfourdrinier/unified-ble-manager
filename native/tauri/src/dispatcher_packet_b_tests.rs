//! PR #210 packet B acceptance: the Tauri dispatcher over one shared
//! `DesktopCentral` on a scripted `FakeRadio` (no hardware).
//!
//! Every test runs the real dispatcher against the real core, holds radio
//! calls open with deterministic `FakeRadio` gates, and asserts delivered
//! data — the events the renderer's channel actually received, the radio
//! calls actually made, the mappings actually kept — never just a handle.
//! Evidence level: deterministic core + dispatcher; not physical-radio proof.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::Value;
use tauri::ipc::{Channel, InvokeResponseBody};
use tokio::sync::Notify;
use ubm_core::contracts::{BleErrorCode, CommitState};
use ubm_desktop::{
    AdapterAuthorization, AdapterPowerState, AdapterResetEvent, AdmissionPolicy,
    CharacteristicSnapshot, DeliveryMode, DescriptorSnapshot, DesktopCentral, FakeRadio, FaultOp,
    LifecycleEvent, LifecycleKind, ObservedDelivery, OpControl, OpTicket, OperationId,
    PeerSnapshot, PlatformDetail, PlatformValue, PropertyFlags, RadioEvent, Retryability,
    ServiceSnapshot, WriteLimits,
};

use super::{
    attachment_identity_matches, attachment_record, caller_key, into_object, object,
    required_value, string, Attachment, AuthorityOpener, BtleplugDispatcher, CallerState,
    DispatchError, IpcEventSink, IpcValue, OrphanResource, ReleasePhase, StreamEnd,
};
use crate::desktop_core::CoreAuthority;
use crate::AuthenticatedCaller;

const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
/// Notify-only (the Polar H10 heart-rate measurement shape).
const NOTIFY_ONLY: &str = "00002a37-0000-1000-8000-00805f9b34fb";
/// Indicate-only.
const INDICATE_ONLY: &str = "00002a38-0000-1000-8000-00805f9b34fb";
/// Readable and writable with response.
const CONTROL_POINT: &str = "00002a39-0000-1000-8000-00805f9b34fb";
const LEASE_ID: &str = "lease-1";
const LEASE_GENERATION: &str = "generation-1";
const WAIT: Duration = Duration::from_secs(5);

fn flags(read: bool, write: bool, notify: bool, indicate: bool) -> PropertyFlags {
    PropertyFlags {
        read,
        write,
        write_without_response: false,
        notify,
        indicate,
    }
}

fn characteristic(uuid: &str, properties: PropertyFlags) -> CharacteristicSnapshot {
    CharacteristicSnapshot {
        uuid: uuid.to_owned(),
        occurrence: 0,
        properties,
        descriptors: Vec::new(),
    }
}

fn hrm_service() -> ServiceSnapshot {
    ServiceSnapshot {
        uuid: HRM_SERVICE.to_owned(),
        occurrence: 0,
        characteristics: vec![
            characteristic(NOTIFY_ONLY, flags(false, false, true, false)),
            characteristic(INDICATE_ONLY, flags(false, false, false, true)),
            characteristic(CONTROL_POINT, flags(true, true, false, false)),
        ],
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
        extras: ubm_desktop::AdvertisementExtras::default(),
    })
}

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

fn number(value: i64) -> IpcValue {
    IpcValue::Number(serde_json::Number::from(value))
}

fn field<'a>(value: &'a IpcValue, key: &str) -> &'a IpcValue {
    match value {
        IpcValue::Object(fields) => fields
            .get(key)
            .unwrap_or_else(|| panic!("missing field {key} in {value:?}")),
        _ => panic!("expected an object, got {value:?}"),
    }
}

fn text(value: &IpcValue, key: &str) -> String {
    match field(value, key) {
        IpcValue::String(text) => text.clone(),
        other => panic!("field {key} is not a string: {other:?}"),
    }
}

fn count(calls: &[String], name: &str) -> usize {
    calls.iter().filter(|call| call.as_str() == name).count()
}

/// One connected peer as the renderer holds it.
#[derive(Clone)]
struct Link {
    handle: String,
    peer_id: String,
    connection_id: String,
    owner_lease_id: String,
    generation: String,
}

/// One discovered database as the renderer holds it.
#[derive(Clone)]
struct Database {
    handle: String,
    id: String,
    generation: String,
    characteristics: HashMap<String, String>,
}

struct Harness {
    dispatcher: BtleplugDispatcher,
    central: DesktopCentral<FakeRadio>,
    caller: AuthenticatedCaller,
    events: Arc<StdMutex<Vec<Value>>>,
    /// The attachment the caller was bound to at attach.
    attachment: Attachment,
}

impl Harness {
    /// Central opened on the shared desktop executor, as production opens
    /// it; one attached caller whose event channel records every delivery.
    async fn new() -> Self {
        Self::with_radio(FakeRadio::new()).await
    }

    /// Central over a radio scripted before the open (adapter facts and
    /// OS policy are read at open).
    async fn with_radio(radio: FakeRadio) -> Self {
        let central = ubm_desktop::executor::desktop_runtime()
            .spawn(DesktopCentral::open(radio, "tauri-test"))
            .await
            .expect("open task joins")
            .expect("fake radio opens");
        Self::over(central).await
    }

    /// Central opened on the current runtime (paused-clock tests).
    async fn on_current_runtime() -> Self {
        let central = DesktopCentral::open(FakeRadio::new(), "tauri-test")
            .await
            .expect("fake radio opens");
        Self::over(central).await
    }

    async fn over(central: DesktopCentral<FakeRadio>) -> Self {
        let authority: Arc<dyn CoreAuthority> = Arc::new(central.clone());
        let dispatcher = BtleplugDispatcher::with_core_authority(authority);
        let caller = AuthenticatedCaller::new("test-app".to_owned(), "main".to_owned());
        let events = Arc::new(StdMutex::new(Vec::new()));
        let log = Arc::clone(&events);
        let sink = IpcEventSink::new(Channel::new(move |body| {
            if let InvokeResponseBody::Json(json) = body {
                log.lock()
                    .expect("event log")
                    .push(serde_json::from_str(&json).expect("event json"));
            }
            Ok(())
        }));
        // The attachment is the shared central's own (finding 43), read the
        // way bootstrap reads it.
        let attachment = dispatcher
            .ensure_adapter()
            .await
            .expect("the central's attachment");
        {
            let mut state = dispatcher.inner.lock().await;
            state.callers.insert(
                caller_key(&caller),
                CallerState {
                    lease_id: LEASE_ID.to_owned(),
                    lease_generation: LEASE_GENERATION.to_owned(),
                    versions: object([]),
                    attachment: attachment.clone(),
                    event_sink: sink,
                    retired: false,
                    scan: None,
                    connections: HashMap::new(),
                    databases: HashMap::new(),
                    subscriptions: HashMap::new(),
                    connection_events: HashMap::new(),
                    operations: HashMap::new(),
                    completed_correlations: HashMap::new(),
                    pending_events: std::collections::HashSet::new(),
                },
            );
        }
        Self {
            dispatcher,
            central,
            caller,
            events,
            attachment,
        }
    }

    fn radio(&self) -> &FakeRadio {
        self.central.boundary()
    }

    fn key(&self) -> String {
        caller_key(&self.caller)
    }

    fn lease_payload(entries: Vec<(&str, IpcValue)>) -> BTreeMap<String, IpcValue> {
        let mut payload: BTreeMap<String, IpcValue> = entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
        payload.insert("__expectedLeaseId".to_owned(), string(LEASE_ID));
        payload.insert(
            "__expectedLeaseGeneration".to_owned(),
            string(LEASE_GENERATION),
        );
        payload
    }

    /// Run one command on the current runtime, as `route` would after
    /// admission, with an explicit control.
    async fn execute(
        &self,
        command: &str,
        entries: Vec<(&str, IpcValue)>,
        bytes: Option<Vec<u8>>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        self.dispatcher
            .execute(
                &self.caller,
                command,
                Self::lease_payload(entries),
                bytes,
                ctl,
            )
            .await
    }

    /// The full IPC route request the webview sends.
    fn route_request(
        &self,
        command: &str,
        correlation: &str,
        entries: Vec<(&str, IpcValue)>,
        bytes: Option<Vec<u8>>,
    ) -> BTreeMap<String, IpcValue> {
        let payload = IpcValue::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        );
        let envelope = object([
            ("command", string(command)),
            ("correlation", string(correlation)),
            ("payload", payload),
            (
                "rendererLease",
                object([
                    ("leaseId", string(LEASE_ID)),
                    ("generation", string(LEASE_GENERATION)),
                ]),
            ),
            (
                "attachmentId",
                string(self.attachment.attachment_id.clone()),
            ),
            ("attachment", attachment_record(&self.attachment)),
            (
                "renderer",
                object([
                    ("clientId", string("test-app:main")),
                    ("windowScope", string("main")),
                    ("sessionScope", string(LEASE_GENERATION)),
                ]),
            ),
            ("versions", object([])),
            (
                "binaryPayload",
                bytes.map_or(IpcValue::Null, IpcValue::Bytes),
            ),
        ]);
        BTreeMap::from([
            ("kind".to_owned(), string("route")),
            ("envelope".to_owned(), envelope),
        ])
    }

    async fn route(
        &self,
        command: &str,
        correlation: &str,
        entries: Vec<(&str, IpcValue)>,
        bytes: Option<Vec<u8>>,
    ) -> Result<IpcValue, DispatchError> {
        self.dispatcher
            .route(
                self.caller.clone(),
                self.route_request(command, correlation, entries, bytes),
            )
            .await
            .map(|response| field(&response, "payload").clone())
    }

    fn spawn_route(
        &self,
        command: &str,
        correlation: &str,
        entries: Vec<(&str, IpcValue)>,
        bytes: Option<Vec<u8>>,
    ) -> tokio::task::JoinHandle<Result<IpcValue, DispatchError>> {
        let dispatcher = self.dispatcher.clone();
        let caller = self.caller.clone();
        let request = self.route_request(command, correlation, entries, bytes);
        tokio::spawn(async move {
            dispatcher
                .route(caller, request)
                .await
                .map(|response| field(&response, "payload").clone())
        })
    }

    async fn cancel(&self, target: &str) -> String {
        let answer = self
            .route(
                "operation.cancel",
                &format!("cancel-{target}"),
                vec![("targetCorrelation", string(target))],
                None,
            )
            .await
            .expect("cancel answers");
        text(&answer, "state")
    }

    /// The radio has heard `peer_id` and resolves it; services scripted.
    async fn advertise(&self, peer_id: &str) {
        self.radio().set_services(peer_id, vec![hrm_service()]);
        self.radio().set_mtu(peer_id, 185);
        self.radio().push_event(advertisement(peer_id));
        for _ in 0..5000 {
            if self.central.peer_key_for(peer_id).await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("peer {peer_id} never resolved");
    }

    async fn connect(&self, peer_id: &str) -> Link {
        self.advertise(peer_id).await;
        let response = self
            .execute(
                "connection.connect",
                vec![("peerId", string(peer_id))],
                None,
                OpControl::unbounded(),
            )
            .await
            .expect("connect");
        Link {
            handle: text(&response, "handle"),
            peer_id: peer_id.to_owned(),
            connection_id: text(&response, "connectionId"),
            owner_lease_id: text(&response, "ownerLeaseId"),
            generation: text(&response, "connectionGeneration"),
        }
    }

    fn link_entries(link: &Link) -> Vec<(&'static str, IpcValue)> {
        vec![
            ("connectionHandle", string(link.handle.clone())),
            ("peerId", string(link.peer_id.clone())),
            ("connectionId", string(link.connection_id.clone())),
            ("ownerLeaseId", string(link.owner_lease_id.clone())),
            ("connectionGeneration", string(link.generation.clone())),
        ]
    }

    async fn discover(&self, link: &Link) -> Database {
        let response = self
            .execute(
                "gatt.discover",
                Self::link_entries(link),
                None,
                OpControl::unbounded(),
            )
            .await
            .expect("discover");
        let IpcValue::Array(records) = field(&response, "characteristics") else {
            panic!("characteristics must be an array");
        };
        let characteristics = records
            .iter()
            .map(|record| (text(record, "characteristicUuid"), text(record, "handle")))
            .collect();
        Database {
            handle: text(&response, "handle"),
            id: text(&response, "databaseId"),
            generation: text(&response, "databaseGeneration"),
            characteristics,
        }
    }

    fn gatt_entries(link: &Link, database: &Database, uuid: &str) -> Vec<(&'static str, IpcValue)> {
        let mut entries = Self::link_entries(link);
        entries.extend([
            ("databaseHandle", string(database.handle.clone())),
            ("databaseId", string(database.id.clone())),
            ("databaseGeneration", string(database.generation.clone())),
            (
                "characteristicHandle",
                string(database.characteristics[uuid].clone()),
            ),
        ]);
        entries
    }

    async fn subscribe(
        &self,
        link: &Link,
        database: &Database,
        uuid: &str,
        mode: Option<&str>,
    ) -> Result<IpcValue, DispatchError> {
        let mut entries = Self::gatt_entries(link, database, uuid);
        if let Some(mode) = mode {
            entries.push(("deliveryMode", string(mode)));
        }
        self.execute("gatt.subscribe", entries, None, OpControl::unbounded())
            .await
    }

    async fn connection_events(&self, link: &Link, stream: &str) {
        let mut entries = Self::link_entries(link);
        entries.push(("connectionEventsHandle", string(stream)));
        self.execute(
            "connection.events.subscribe",
            entries,
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("connection events subscribe");
    }

    async fn ready(&self, stream: &str) {
        self.execute(
            "connection.events.ready",
            vec![("connectionEventsHandle", string(stream))],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("connection events ready");
    }

    /// Items delivered on one stream, in delivery order.
    fn items(&self, stream: &str) -> Vec<Value> {
        self.events
            .lock()
            .expect("event log")
            .iter()
            .filter(|event| event["streamId"] == stream)
            .map(|event| event["item"].clone())
            .collect()
    }

    async fn wait_items(&self, stream: &str, at_least: usize) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let items = self.items(stream);
            if items.len() >= at_least {
                return items;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "stream {stream} delivered {} of {at_least} items: {items:?}",
                items.len()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn wait_calls(&self, name: &str, at_least: usize) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while count(&self.radio().calls(), name) < at_least {
            assert!(
                tokio::time::Instant::now() < deadline,
                "radio never saw {at_least} {name} calls: {:?}",
                self.radio().calls()
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    async fn wait_tracked(&self, correlation: &str) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let tracked = self
                .dispatcher
                .inner
                .lock()
                .await
                .callers
                .get(&self.key())
                .is_some_and(|caller| caller.operations.contains_key(correlation));
            if tracked {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "correlation {correlation} never tracked"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Push one notification for the live enablement of `uuid` on `peer_id`.
    fn notify(&self, peer_id: &str, uuid: &str, value: Vec<u8>) {
        let epoch = self
            .radio()
            .enable_epochs()
            .iter()
            .rev()
            .find(|(key, _)| key.0 == peer_id && key.3 == uuid)
            .map(|(_, epoch)| *epoch)
            .expect("an enablement for this characteristic");
        self.radio().push_event(RadioEvent::Notification {
            peer_id: peer_id.to_owned(),
            service_uuid: HRM_SERVICE.to_owned(),
            service_occurrence: 0,
            characteristic_uuid: uuid.to_owned(),
            characteristic_occurrence: 0,
            epoch,
            value,
        });
    }

    async fn with_caller<T>(&self, read: impl FnOnce(&CallerState) -> T) -> T {
        let state = self.dispatcher.inner.lock().await;
        read(state.callers.get(&self.key()).expect("caller mapped"))
    }

    async fn replace_lease(&self) {
        let mut state = self.dispatcher.inner.lock().await;
        let caller = state.callers.get_mut(&self.key()).expect("caller mapped");
        caller.lease_id = "lease-2".to_owned();
        caller.lease_generation = "generation-2".to_owned();
    }
}

fn value_bytes(item: &Value) -> Vec<u8> {
    item["value"]["value"]["$__unifiedBleBytesV2"]
        .as_array()
        .expect("byte value")
        .iter()
        .map(|byte| u8::try_from(byte.as_u64().expect("byte")).expect("u8"))
        .collect()
}

fn empty_scan_query() -> IpcValue {
    let canonical = "{\"anyOf\":null,\"exclude\":null}";
    let mut hash = 0xcbf29ce484222325_u64;
    for code_unit in canonical.encode_utf16() {
        hash ^= u64::from(code_unit);
        hash = hash.wrapping_mul(0x100000001b3_u64);
    }
    object([
        ("anyOf", IpcValue::Null),
        ("exclude", IpcValue::Null),
        ("digest", string(format!("scan-query-v1:{hash:016x}"))),
    ])
}

fn wire_error(error: &DispatchError) -> Value {
    error.normalized_error().into_wire()
}

// PR210-04 — one peer's hung discovery must not stall another peer's
// notifications, a cancel, or shutdown: the dispatcher shares the central
// and never holds a lock across radio work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_04_a_hung_discovery_never_stalls_another_peer_cancel_or_shutdown() {
    let harness = Harness::new().await;
    let a = harness.connect("peer-a").await;
    let b = harness.connect("peer-b").await;
    let b_database = harness.discover(&b).await;
    let subscription = harness
        .subscribe(&b, &b_database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe b");
    let b_stream = text(&subscription, "handle");

    harness.radio().block_op(FaultOp::Discover);
    let discovers = count(&harness.radio().calls(), "discover");
    let hung = harness.spawn_route(
        "gatt.discover",
        "discover-a",
        Harness::link_entries(&a),
        None,
    );
    harness.wait_calls("discover", discovers + 1).await;

    for index in 0..50_u8 {
        harness.notify("peer-b", NOTIFY_ONLY, vec![index]);
    }
    let items = harness.wait_items(&b_stream, 50).await;
    let delivered: Vec<u8> = items.iter().flat_map(value_bytes).collect();
    assert_eq!(delivered, (0..50).collect::<Vec<u8>>(), "all 50, in order");

    assert_eq!(harness.cancel("discover-a").await, "cancellation-requested");
    let error = tokio::time::timeout(WAIT, hung)
        .await
        .expect("the cancel settles while the radio call is still held")
        .expect("route joins")
        .expect_err("cancelled discovery fails");
    assert_eq!(error.code, BleErrorCode::OperationAborted);

    let held_again = harness.spawn_route(
        "gatt.discover",
        "discover-a-2",
        Harness::link_entries(&a),
        None,
    );
    harness.wait_calls("discover", discovers + 2).await;
    let shutdown = tokio::time::timeout(WAIT, harness.dispatcher.authority_shutdown())
        .await
        .expect("shutdown finishes while a discovery is held");
    assert!(shutdown.core.is_some());
    harness.radio().unblock_op(FaultOp::Discover);
    let _ = tokio::time::timeout(WAIT, held_again).await;
}

// PR210-05 — a cancel targets exactly its own core operation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_05_a_cancel_targets_exactly_one_of_two_dispatched_writes() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let entries = || {
        let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
        entries.push(("mode", string("with-response")));
        entries
    };
    harness.radio().block_op(FaultOp::Write);
    let first = harness.spawn_route("gatt.write", "write-1", entries(), Some(vec![1]));
    let second = harness.spawn_route("gatt.write", "write-2", entries(), Some(vec![2]));
    harness.wait_calls("write_characteristic", 2).await;

    assert_eq!(harness.cancel("write-1").await, "cancellation-requested");
    let aborted = tokio::time::timeout(WAIT, first)
        .await
        .expect("the cancelled write settles")
        .expect("route joins")
        .expect_err("the cancelled write fails");
    assert_eq!(aborted.code, BleErrorCode::OperationAborted);
    assert_eq!(
        aborted.retryability,
        Retryability::Never,
        "a dispatched write may have committed: never caller-retryable (PR210-22)"
    );
    assert_eq!(aborted.commit, Some(CommitState::Unknown));
    let wire = wire_error(&aborted);
    assert_eq!(wire["retryability"], "never");
    assert_eq!(wire["commit"], "uncertain");

    harness.radio().unblock_op(FaultOp::Write);
    tokio::time::timeout(WAIT, second)
        .await
        .expect("the other write completes")
        .expect("route joins")
        .expect("the other write is unaffected");
    assert_eq!(
        count(&harness.radio().calls(), "write_characteristic"),
        2,
        "one radio write per operation, none repeated"
    );
    assert_eq!(
        harness.radio().writes().len(),
        1,
        "only the unaffected write completed at the radio"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_05_a_cancel_before_admission_never_reaches_the_radio() {
    let harness = Harness::new().await;
    harness.advertise("peer-a").await;
    // Hold the authority slot: the connect is tracked but cannot reach the
    // core, so the cancel arrives before any core id exists.
    let slot = harness.dispatcher.authority.lock().await;
    let connect = harness.spawn_route(
        "connection.connect",
        "connect-1",
        vec![("peerId", string("peer-a"))],
        None,
    );
    harness.wait_tracked("connect-1").await;
    assert_eq!(harness.cancel("connect-1").await, "cancellation-requested");
    drop(slot);

    let error = tokio::time::timeout(WAIT, connect)
        .await
        .expect("connect settles")
        .expect("route joins")
        .expect_err("cancelled connect fails");
    assert_eq!(error.code, BleErrorCode::OperationAborted);
    assert_eq!(error.retryability, Retryability::CallerDecides);
    assert_eq!(error.commit, Some(CommitState::NotDispatched));
    assert_eq!(wire_error(&error)["commit"], "not-dispatched");
    assert_eq!(
        count(&harness.radio().calls(), "connect"),
        0,
        "no radio call"
    );
    assert!(
        harness
            .with_caller(|caller| caller.connections.is_empty())
            .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_05_a_success_that_beat_the_cancel_is_returned_and_owned() {
    let harness = Harness::new().await;
    harness.advertise("peer-a").await;
    harness.radio().block_op(FaultOp::Connect);
    let connect = harness.spawn_route(
        "connection.connect",
        "connect-1",
        vec![("peerId", string("peer-a"))],
        None,
    );
    harness.wait_calls("connect", 1).await;
    harness.radio().unblock_op(FaultOp::Connect);
    let connected = tokio::time::timeout(WAIT, connect)
        .await
        .expect("connect settles")
        .expect("route joins")
        .expect("connect succeeds");
    assert_eq!(harness.cancel("connect-1").await, "already-terminal");
    let handle = text(&connected, "handle");
    assert!(
        harness
            .with_caller(|caller| caller.connections.contains_key(&handle))
            .await,
        "the returned handle is owned: no lease without an owner"
    );
}

// PR210-06 — the caller budget crosses as relative `budgetMs` and is
// admitted on the plugin clock when the route arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_06_budget_ms_is_validated_before_any_effect() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let reads = || count(&harness.radio().calls(), "read_characteristic");
    let before = reads();
    let malformed: Vec<IpcValue> = vec![
        number(-1),
        IpcValue::Number(serde_json::Number::from_f64(1.5).expect("finite")),
        string("50"),
        IpcValue::Number(serde_json::Number::from(9_007_199_254_740_992_u64)),
    ];
    for (index, budget) in malformed.into_iter().enumerate() {
        let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
        entries.push(("budgetMs", budget));
        let error = harness
            .route("gatt.read", &format!("bad-{index}"), entries, None)
            .await
            .expect_err("a malformed budget is refused");
        assert_eq!(
            error.identity(),
            ("protocol.malformed", "ipc", "tauri.route-budget".to_owned())
        );
    }
    let mut spent = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    spent.push(("budgetMs", number(0)));
    let error = harness
        .route("gatt.read", "spent", spent, None)
        .await
        .expect_err("a spent budget times out");
    assert_eq!(error.code, BleErrorCode::OperationTimedOut);
    assert_eq!(error.retryability, Retryability::CallerDecides);
    assert_eq!(reads(), before, "no radio call for a spent budget");

    let mut absent = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    absent.push(("budgetMs", IpcValue::Null));
    harness
        .route("gatt.read", "unbounded", absent, None)
        .await
        .expect("null means no caller budget");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_06_a_short_budget_bounds_the_radio_call() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    harness.radio().block_op(FaultOp::Read);
    let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    // Room to reach the radio under parallel test load; the held read then
    // ends on the budget, far inside every liveness backstop.
    entries.push(("budgetMs", number(500)));
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(WAIT, harness.route("gatt.read", "short", entries, None))
        .await
        .expect("a 500 ms budget ends the read")
        .expect_err("the read times out");
    eprintln!(
        "DEBUG elapsed {:?} calls {:?} error {:?}",
        started.elapsed(),
        harness.radio().calls(),
        error
    );
    assert_eq!(error.code, BleErrorCode::OperationTimedOut);
    assert_eq!(
        error.platform, None,
        "a caller-budget expiry is not a liveness backstop"
    );
    assert_eq!(count(&harness.radio().calls(), "read_characteristic"), 1);
    harness.radio().unblock_op(FaultOp::Read);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_06_queueing_behind_the_dispatcher_spends_the_budget() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let before = count(&harness.radio().calls(), "read_characteristic");
    let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    entries.push(("budgetMs", number(50)));
    let state = harness.dispatcher.inner.lock().await;
    let queued = harness.spawn_route("gatt.read", "queued", entries, None);
    tokio::time::sleep(Duration::from_millis(150)).await;
    drop(state);
    let error = tokio::time::timeout(WAIT, queued)
        .await
        .expect("the queued read settles")
        .expect("route joins")
        .expect_err("the budget ran out while queued");
    assert_eq!(error.code, BleErrorCode::OperationTimedOut);
    assert_eq!(error.commit, Some(CommitState::NotDispatched));
    assert_eq!(
        count(&harness.radio().calls(), "read_characteristic"),
        before,
        "the expired read never reached the radio"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_06_a_write_that_times_out_after_dispatch_is_never_retryable() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    harness.radio().block_op(FaultOp::Write);
    let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    entries.push(("mode", string("with-response")));
    entries.push(("budgetMs", number(500)));
    let error = tokio::time::timeout(
        WAIT,
        harness.route("gatt.write", "write", entries, Some(vec![7])),
    )
    .await
    .expect("the budget ends the write")
    .expect_err("the write times out");
    assert_eq!(error.code, BleErrorCode::OperationTimedOut);
    assert_eq!(error.retryability, Retryability::Never);
    assert_eq!(wire_error(&error)["commit"], "uncertain");
    assert_eq!(count(&harness.radio().calls(), "write_characteristic"), 1);
    harness.radio().unblock_op(FaultOp::Write);
}

/// A 45 s budget outlives the old fixed 30 s backstop: the read that
/// answers at 40 s succeeds.
#[tokio::test(start_paused = true)]
async fn pr210_06_a_budget_longer_than_the_old_backstop_is_honoured() {
    let harness = Harness::on_current_runtime().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    harness.radio().block_op(FaultOp::Read);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(Harness::gatt_entries(&link, &database, CONTROL_POINT));
    let read = tokio::spawn(async move {
        dispatcher
            .execute(
                &caller,
                "gatt.read",
                payload,
                None,
                OpControl::new(ubm_desktop::Budget::from_ms(45_000), OpTicket::new()),
            )
            .await
    });
    while count(&harness.radio().calls(), "read_characteristic") == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(40)).await;
    harness.radio().unblock_op(FaultOp::Read);
    let value = read
        .await
        .expect("read joins")
        .expect("answered at 40 s of 45");
    assert_eq!(field(&value, "value"), &IpcValue::Bytes(vec![0x42]));
}

// A read answer carries the radio's own provenance verbatim: CoreBluetooth
// answering a read on a notifying characteristic says the value may be a
// notification, and the host never rewrites it into a read response.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_carries_the_radio_provenance() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    for (provenance, wire) in [
        (ubm_desktop::ReadProvenance::ReadResponse, "read-response"),
        (
            ubm_desktop::ReadProvenance::ReadOrNotification,
            "read-or-notification",
        ),
    ] {
        harness.radio().script_read_provenance(provenance);
        let value = harness
            .execute(
                "gatt.read",
                Harness::gatt_entries(&link, &database, CONTROL_POINT),
                None,
                OpControl::unbounded(),
            )
            .await
            .expect("read");
        assert_eq!(field(&value, "value"), &IpcValue::Bytes(vec![0x42]));
        assert_eq!(field(&value, "provenance"), &string(wire));
    }
}

// PR210-07 — forwarders spawn only after their entry is published, so the
// first observation and the first notification are delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_07_scan_and_notification_forwarders_deliver_from_the_first_item() {
    let harness = Harness::new().await;
    // A sighting the OS reports while its scan start is still in flight
    // belongs to that scan (finding 121: before any scan it is no
    // observation at all). It is queued before the scan is published, so
    // the forwarder's very first poll finds it — which is when a forwarder
    // that raced its publication lost data.
    harness.radio().block_op(FaultOp::StartScan);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(vec![("query", empty_scan_query())]);
    let start = tokio::spawn(async move {
        dispatcher
            .start_scan(&caller, payload, OpControl::unbounded())
            .await
    });
    harness.wait_calls("start_scan", 1).await;
    harness.radio().push_event(advertisement("peer-early"));
    let deadline = tokio::time::Instant::now() + WAIT;
    while harness
        .central
        .resource_counters()
        .await
        .queued_advertisements
        < 1
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the in-flight scan never queued its sighting"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    harness.radio().unblock_op(FaultOp::StartScan);
    let scan = tokio::time::timeout(WAIT, start)
        .await
        .expect("start settles")
        .expect("joins")
        .expect("scan starts");
    let scan_stream = text(&scan, "handle");
    assert!(
        harness
            .with_caller(|caller| caller
                .scan
                .as_ref()
                .is_some_and(|scan| scan.handle == scan_stream && scan.task.is_some()))
            .await,
        "published with its forwarder"
    );
    let first = harness.wait_items(&scan_stream, 1).await;
    assert_eq!(first[0]["value"]["peerId"], "peer-early");
    harness.radio().push_event(advertisement("peer-late"));
    let later = harness.wait_items(&scan_stream, 2).await;
    assert_eq!(later[1]["value"]["peerId"], "peer-late");

    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let stream = text(&subscription, "handle");
    harness.notify("peer-a", NOTIFY_ONLY, vec![9]);
    let items = harness.wait_items(&stream, 1).await;
    assert_eq!(value_bytes(&items[0]), vec![9]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_07_a_lease_replaced_during_admission_stops_the_scan_it_admitted() {
    let harness = Harness::new().await;
    harness.radio().block_op(FaultOp::StartScan);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(vec![("query", empty_scan_query())]);
    let start = tokio::spawn(async move {
        dispatcher
            .start_scan(&caller, payload, OpControl::unbounded())
            .await
    });
    harness.wait_calls("start_scan", 1).await;
    harness.replace_lease().await;
    harness.radio().unblock_op(FaultOp::StartScan);
    let error = tokio::time::timeout(WAIT, start)
        .await
        .expect("start settles")
        .expect("joins")
        .expect_err("a stale lease cannot own the scan");
    assert_eq!(error.identity().2, "tauri.scan-stale-lease");
    assert_eq!(count(&harness.radio().calls(), "stop_scan"), 1);
    assert_eq!(
        harness.central.active_scan_id(),
        None,
        "the admitted scan stopped"
    );
    assert!(harness.with_caller(|caller| caller.scan.is_none()).await);
    assert!(harness.dispatcher.inner.lock().await.orphan_debt.is_empty());
}

// PR210-08 — a caller that disappears during admission gets a real,
// identity-scoped compensation; a failed compensation is retained debt.
// Paused clock: the automatic retries (finding 114) run only when this test
// lets time pass, so every radio call below is the one the test names.
#[tokio::test(start_paused = true)]
async fn pr210_08_a_caller_released_during_scan_admission_gets_a_real_stop() {
    let harness = Harness::on_current_runtime().await;
    let admitted = tokio::time::Instant::now();
    harness.radio().block_op(FaultOp::StartScan);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(vec![("query", empty_scan_query())]);
    let start = tokio::spawn(async move {
        dispatcher
            .start_scan(&caller, payload, OpControl::unbounded())
            .await
    });
    harness.wait_calls("start_scan", 1).await;
    harness
        .dispatcher
        .inner
        .lock()
        .await
        .callers
        .remove(&harness.key());
    harness.radio().fail_next(FaultOp::StopScan, "stop refused");
    harness
        .radio()
        .fail_next(FaultOp::StopScan, "stop refused again");
    harness.radio().unblock_op(FaultOp::StartScan);
    let error = tokio::time::timeout(WAIT, start)
        .await
        .expect("start settles")
        .expect("joins")
        .expect_err("nobody owns the scan");
    assert_eq!(error.code, BleErrorCode::OwnershipDenied);
    let scan_id = harness
        .central
        .active_scan_id()
        .expect("the stop failed: still owned");
    {
        let state = harness.dispatcher.inner.lock().await;
        assert_eq!(state.orphan_debt.len(), 1, "failed compensation is debt");
        assert!(
            matches!(&state.orphan_debt[0].resource, OrphanResource::Scan(id) if *id == scan_id)
        );
    }

    assert_no_automatic_retry_yet(admitted);
    let first = harness.dispatcher.release(&harness.key()).await;
    assert_eq!(first.clone().into_wire()["state"], "release-failed");
    assert_eq!(first.into_wire()["failures"][0]["resourceKind"], "scan");
    let second = harness.dispatcher.release(&harness.key()).await;
    assert_eq!(second.into_wire()["state"], "released");
    assert_eq!(count(&harness.radio().calls(), "stop_scan"), 3);
    assert_eq!(harness.central.active_scan_id(), None);
    assert!(harness.dispatcher.inner.lock().await.orphan_debt.is_empty());
    elapse(60_000).await;
    assert_eq!(
        count(&harness.radio().calls(), "stop_scan"),
        3,
        "a settled debt is never retried"
    );
}

/// The releases a test drives run before the first automatic retry is due,
/// so the radio calls it counts are exactly its own.
fn assert_no_automatic_retry_yet(since: tokio::time::Instant) {
    assert!(
        since.elapsed() < super::ORPHAN_RETRY_FIRST_DELAY,
        "{:?} of paused time passed; the first automatic retry is due at {:?}",
        since.elapsed(),
        super::ORPHAN_RETRY_FIRST_DELAY
    );
}

#[tokio::test(start_paused = true)]
async fn pr210_08_connect_and_subscribe_compensation_is_recorded_not_discarded() {
    let harness = Harness::on_current_runtime().await;
    harness.advertise("peer-a").await;
    let admitted = tokio::time::Instant::now();
    harness.radio().block_op(FaultOp::Connect);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(vec![("peerId", string("peer-a"))]);
    let connect = tokio::spawn(async move {
        dispatcher
            .connect(&caller, payload, OpControl::unbounded())
            .await
    });
    harness.wait_calls("connect", 1).await;
    harness.replace_lease().await;
    harness
        .radio()
        .fail_next(FaultOp::Disconnect, "release refused");
    harness.radio().unblock_op(FaultOp::Connect);
    let error = tokio::time::timeout(WAIT, connect)
        .await
        .expect("connect settles")
        .expect("joins")
        .expect_err("a stale lease cannot own the link");
    assert_eq!(error.identity().2, "tauri.connect-stale-lease");
    {
        let state = harness.dispatcher.inner.lock().await;
        assert_eq!(state.orphan_debt.len(), 1);
        assert!(matches!(
            &state.orphan_debt[0].resource,
            OrphanResource::Link { peer_id, .. } if peer_id == "peer-a"
        ));
    }
    assert_no_automatic_retry_yet(admitted);
    let released = harness.dispatcher.release(&harness.key()).await;
    assert_eq!(
        released.into_wire()["state"],
        "released",
        "the retry releases the link"
    );
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 2);
    assert!(!harness.radio().link_connected("peer-a"));
    elapse(60_000).await;
    assert_eq!(
        count(&harness.radio().calls(), "disconnect"),
        2,
        "a settled debt is never retried"
    );
}

// PR210-09 — a failed release keeps the resource and its native identity;
// the retry calls native again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_09_a_failed_scan_stop_keeps_the_scan_for_a_real_retry() {
    let harness = Harness::new().await;
    let scan = harness
        .execute(
            "scan.start",
            vec![("query", empty_scan_query())],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("scan starts");
    let handle = text(&scan, "handle");
    let scan_id = harness.central.active_scan_id().expect("scan owned");
    harness.radio().fail_next(FaultOp::StopScan, "stop refused");
    let stop = || {
        harness.execute(
            "scan.stop",
            vec![("scanHandle", string(handle.clone()))],
            None,
            OpControl::unbounded(),
        )
    };
    let error = stop().await.expect_err("the first stop fails truthfully");
    assert_eq!(error.code, BleErrorCode::ScanStopFailed);
    assert!(
        harness
            .with_caller(|caller| caller.scan.as_ref().is_some_and(|scan| {
                scan.core_operation_id == scan_id
                    && matches!(scan.phase, ReleasePhase::ReleaseFailed)
            }))
            .await,
        "the mapping and its core id survive the failure"
    );
    stop().await.expect("the retry stops");
    assert_eq!(count(&harness.radio().calls(), "stop_scan"), 2);
    assert!(harness.with_caller(|caller| caller.scan.is_none()).await);
    assert_eq!(harness.central.active_scan_id(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_09_a_failed_unsubscribe_keeps_the_consumer_for_a_real_retry() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let handle = text(&subscription, "handle");
    harness
        .radio()
        .fail_next(FaultOp::Unsubscribe, "disable refused");
    let unsubscribe = || {
        harness.execute(
            "gatt.unsubscribe",
            vec![("subscriptionHandle", string(handle.clone()))],
            None,
            OpControl::unbounded(),
        )
    };
    unsubscribe()
        .await
        .expect_err("the first disable fails truthfully");
    assert!(
        harness
            .with_caller(|caller| caller
                .subscriptions
                .get(&handle)
                .is_some_and(|subscription| {
                    matches!(subscription.phase, ReleasePhase::ReleaseFailed)
                }))
            .await,
        "the consumer mapping survives the failure"
    );
    assert_eq!(
        harness.radio().live_subscription_count(),
        1,
        "CCCD still live"
    );
    unsubscribe().await.expect("the retry disables");
    assert_eq!(harness.radio().live_subscription_count(), 0);
    assert_eq!(
        count(&harness.radio().calls(), "set_notifications"),
        3,
        "one enable, two disable attempts"
    );
    assert!(
        harness
            .with_caller(|caller| caller.subscriptions.is_empty())
            .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_09_a_failed_disconnect_keeps_the_link_for_a_real_retry() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness
        .radio()
        .fail_next(FaultOp::Disconnect, "release refused");
    let disconnect = || {
        harness.execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
    };
    disconnect()
        .await
        .expect_err("the first release fails truthfully");
    assert!(
        harness
            .with_caller(|caller| caller
                .connections
                .get(&link.handle)
                .is_some_and(|connection| {
                    matches!(connection.phase, ReleasePhase::ReleaseFailed)
                }))
            .await,
        "the link mapping survives the failure"
    );
    disconnect().await.expect("the retry releases");
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 2);
    assert!(!harness.radio().link_connected("peer-a"));
    assert!(
        harness
            .with_caller(|caller| caller.connections.is_empty())
            .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_09_a_failed_destroy_keeps_the_caller_for_a_real_retry() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness
        .radio()
        .fail_next(FaultOp::Disconnect, "release refused");
    let first = harness.dispatcher.release(&harness.key()).await.into_wire();
    assert_eq!(first["state"], "release-failed");
    assert_eq!(first["failures"][0]["resourceKind"], "connection");
    assert_eq!(
        first["failures"][0]["error"]["retryability"], "never",
        "the wire vocabulary a TypeScript transport accepts"
    );
    assert!(
        harness
            .with_caller(|caller| caller.retired && caller.connections.contains_key(&link.handle))
            .await,
        "the retired caller keeps the link it could not release"
    );
    let second = harness.dispatcher.release(&harness.key()).await.into_wire();
    assert_eq!(second["state"], "released");
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 2);
    assert!(harness.dispatcher.inner.lock().await.callers.is_empty());
}

// PR210-10 — a rejected disconnect changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_10_a_wrong_generation_disconnect_changes_nothing() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    let shape = |caller: &CallerState| {
        (
            caller.connections.len(),
            caller.databases.len(),
            caller.subscriptions.len(),
            caller.connection_events.len(),
            caller
                .connections
                .get(&link.handle)
                .is_some_and(|connection| connection.phase.is_active()),
        )
    };
    let before = harness.with_caller(shape).await;
    let mut wrong = Harness::link_entries(&link);
    wrong.retain(|(key, _)| *key != "connectionGeneration");
    wrong.push(("connectionGeneration", string("generation-wrong")));
    let error = harness
        .execute("connection.disconnect", wrong, None, OpControl::unbounded())
        .await
        .expect_err("a wrong generation is refused");
    assert_eq!(error.code, BleErrorCode::ProtocolViolation);
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 0);
    assert_eq!(harness.with_caller(shape).await, before, "nothing changed");
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("the valid disconnect succeeds");
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 1);
}

// PR210-11 — typed core lifecycle events reach the connection-event
// stream by its own handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_11_idle_link_loss_reaches_the_stream_and_invalidates() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    harness.ready("connection-events-ipc-1").await;

    // No operation follows: the OS alone reports the loss.
    harness
        .radio()
        .push_event(RadioEvent::Disconnected("peer-a".to_owned()));
    let items = harness.wait_items("connection-events-ipc-1", 3).await;
    assert_eq!(items[0]["value"]["current"], "connected");
    assert_eq!(items[1]["value"]["previous"], "connected");
    assert_eq!(items[1]["value"]["current"], "lost");
    assert_eq!(items[1]["value"]["cause"], "peer-link-loss");
    assert_eq!(
        items[1]["value"]["connectionGeneration"],
        link.generation.as_str()
    );
    assert_eq!(items[2]["kind"], "terminal");
    assert_eq!(items[2]["reason"], "connection-lost");
    let ended = harness.wait_items(&notifications, 1).await;
    assert_eq!(ended[0]["kind"], "terminal");
    assert_eq!(ended[0]["reason"], "connection-lost");
    assert!(
        harness
            .with_caller(|caller| !caller.databases[&database.handle].valid)
            .await,
        "the database of the lost link is stale"
    );
    // The lost link releases without a radio call.
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("a lost link releases");
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_11_requested_disconnect_reports_requested_disconnect() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    harness.ready("connection-events-ipc-1").await;
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("disconnect");
    let items = harness.wait_items("connection-events-ipc-1", 3).await;
    assert_eq!(items[1]["value"]["previous"], "disconnecting");
    assert_eq!(items[1]["value"]["current"], "disconnected");
    assert_eq!(items[1]["value"]["cause"], "requested-disconnect");
    assert_eq!(items[2]["reason"], "owner-released");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        harness.items("connection-events-ipc-1").len(),
        3,
        "exactly once"
    );
}

// Finding 190a — after an app-requested `connection.disconnect`, a live
// notification subscription must end `owner-released` (the vocabulary's
// requested-disconnect word, as on RN iOS/Android/tvOS), never
// `stream.closed`: the supervisor then backs off and reconnects instead of
// stopping.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_190a_a_requested_disconnect_ends_notifications_owner_released() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("disconnect");
    let ended = harness.wait_items(&notifications, 1).await;
    assert_eq!(ended[0]["kind"], "terminal");
    assert_eq!(
        ended[0]["reason"], "owner-released",
        "a requested disconnect ends subscriptions owner-released on every host"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        harness.items(&notifications).len(),
        1,
        "exactly one terminal"
    );
}

// The core can invalidate a notification poll before the requested native
// disconnect has answered. The in-flight poll must not win the transport
// terminal race against the owner-release path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requested_disconnect_pauses_an_in_flight_notification_terminal() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");
    harness.radio().block_op(FaultOp::Disconnect);
    let disconnect = harness.spawn_route(
        "connection.disconnect",
        "requested-disconnect-race",
        Harness::link_entries(&link),
        None,
    );
    harness.wait_calls("disconnect", 1).await;
    assert!(
        harness
            .with_caller(|caller| caller
                .connections
                .get(&link.handle)
                .is_some_and(|connection| matches!(connection.phase, ReleasePhase::Releasing(_))))
            .await
    );

    // Model the already-started core poll returning LinkEnded while the
    // disconnect owns the link. Its terminal send must be rejected atomically
    // with the release phase, not by a separate check before sending.
    let delivered = harness
        .dispatcher
        .notification_terminal(
            &harness.key(),
            (LEASE_ID, LEASE_GENERATION),
            &notifications,
            "connection-lost",
            None,
        )
        .await
        .expect("terminal admission check");
    assert!(!delivered);
    assert!(
        harness.items(&notifications).is_empty(),
        "the pending owner release must reserve the notification terminal"
    );

    harness.radio().unblock_op(FaultOp::Disconnect);
    disconnect
        .await
        .expect("disconnect task")
        .expect("disconnect");
    let ended = harness.wait_items(&notifications, 1).await;
    assert_eq!(ended[0]["reason"], "owner-released");
    assert_eq!(harness.items(&notifications).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_notification_terminal_sent_before_disconnect_is_not_sent_twice() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");

    assert!(harness
        .dispatcher
        .notification_terminal(
            &harness.key(),
            (LEASE_ID, LEASE_GENERATION),
            &notifications,
            "connection-lost",
            None,
        )
        .await
        .expect("first terminal is sent"));
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("disconnect");
    assert_eq!(
        harness.items(&notifications).len(),
        1,
        "a terminal physically sent before owner release is not duplicated"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_owner_terminal_send_keeps_the_disconnect_retryable() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");
    let attempts = Arc::new(AtomicUsize::new(0));
    let send_attempts = Arc::clone(&attempts);
    let events = Arc::clone(&harness.events);
    let sink = IpcEventSink::new(Channel::new(move |body| {
        if send_attempts.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            return Err(tauri::Error::Io(std::io::Error::other(
                "scripted terminal send refusal",
            )));
        }
        if let InvokeResponseBody::Json(json) = body {
            events
                .lock()
                .expect("event log")
                .push(serde_json::from_str(&json).expect("event json"));
        }
        Ok(())
    }));
    {
        let mut state = harness.dispatcher.inner.lock().await;
        state
            .callers
            .get_mut(&harness.key())
            .expect("caller")
            .event_sink = sink;
    }

    let first = harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("a refused terminal send cannot report complete cleanup");
    assert_eq!(first.code, BleErrorCode::PlatformTransport);
    assert!(
        harness
            .with_caller(|caller| caller.connections.contains_key(&link.handle)
                && caller.subscriptions.contains_key(&notifications))
            .await
    );
    assert!(harness.items(&notifications).is_empty());

    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("retry delivers the terminal and releases the mapping");
    let ended = harness.wait_items(&notifications, 1).await;
    assert_eq!(ended[0]["reason"], "owner-released");
    assert_eq!(ended.len(), 1);
    assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_11_a_stale_generation_matches_nothing() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    harness.ready("connection-events-ipc-1").await;
    harness
        .dispatcher
        .apply_lifecycle_event(&LifecycleEvent {
            sequence: 99,
            peer_id: "peer-a".to_owned(),
            peer_key: "stale".to_owned(),
            connection_generation: Some("generation-from-an-older-link".to_owned()),
            database_generation: None,
            kind: LifecycleKind::LinkLost,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        harness.items("connection-events-ipc-1").len(),
        1,
        "only the initial event"
    );
    assert!(
        harness
            .with_caller(
                |caller| caller.connection_events["connection-events-ipc-1"].end == StreamEnd::Open
            )
            .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_11_a_lagged_pump_ends_every_stream_with_overflow() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    harness.ready("connection-events-ipc-1").await;
    harness.dispatcher.overflow_connection_event_streams().await;
    let items = harness.wait_items("connection-events-ipc-1", 2).await;
    assert_eq!(items[1]["kind"], "terminal");
    assert_eq!(items[1]["reason"], "overflow");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_11_a_service_change_invalidates_and_ends_notifications() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");
    harness
        .radio()
        .push_event(RadioEvent::ServicesChanged("peer-a".to_owned()));
    let ended = harness.wait_items(&notifications, 1).await;
    assert_eq!(ended[0]["reason"], "service-changed");
    let deadline = tokio::time::Instant::now() + WAIT;
    while harness
        .with_caller(|caller| caller.databases[&database.handle].valid)
        .await
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "database never invalidated"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_11_a_loss_before_ready_is_delivered_at_ready() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    harness
        .radio()
        .push_event(RadioEvent::Disconnected("peer-a".to_owned()));
    let deadline = tokio::time::Instant::now() + WAIT;
    while harness
        .with_caller(|caller| {
            caller.connection_events["connection-events-ipc-1"].end == StreamEnd::Open
        })
        .await
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "loss never recorded"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    harness.ready("connection-events-ipc-1").await;
    let items = harness.wait_items("connection-events-ipc-1", 3).await;
    assert_eq!(items[0]["value"]["current"], "connected");
    assert_eq!(items[1]["value"]["current"], "lost");
    assert_eq!(items[2]["reason"], "connection-lost");
}

// PR210-13T / FIX-PLAN decision 3 — the 4.x property check, then the
// requirement travels to the core.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_13t_a_requirement_is_refused_only_where_the_property_is_missing() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let enables = || count(&harness.radio().calls(), "set_notifications");

    let error = harness
        .subscribe(
            &link,
            &database,
            INDICATE_ONLY,
            Some("require-notification"),
        )
        .await
        .expect_err("an indicate-only characteristic cannot notify");
    assert_eq!(
        error.identity(),
        (
            "gatt.property-not-supported",
            "gatt",
            "tauri.subscribe.notification".to_owned()
        )
    );
    let error = harness
        .subscribe(&link, &database, NOTIFY_ONLY, Some("require-indication"))
        .await
        .expect_err("a notify-only characteristic cannot indicate");
    assert_eq!(error.identity().2, "tauri.subscribe.indication");
    let error = harness
        .subscribe(&link, &database, NOTIFY_ONLY, Some("require-telepathy"))
        .await
        .expect_err("an unknown mode is refused");
    assert_eq!(error.code, BleErrorCode::ArgumentInvalid);
    assert_eq!(enables(), 0, "no refusal reached the radio");

    // The 4.x-accepted case: require-notification on a notify-capable
    // characteristic succeeds end to end and reports what the radio saw.
    harness
        .radio()
        .script_observed_delivery(ObservedDelivery::Notification);
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, Some("require-notification"))
        .await
        .expect("a notify-capable characteristic accepts require-notification");
    assert_eq!(
        field(&subscription, "observedDelivery"),
        &string("notification")
    );
    assert_eq!(
        harness.radio().delivery_requests(),
        vec![Some(DeliveryMode::Notification)],
        "the requirement travels to the core and the radio"
    );
    let stream = text(&subscription, "handle");
    harness.notify("peer-a", NOTIFY_ONLY, vec![1, 2]);
    let items = harness.wait_items(&stream, 1).await;
    assert_eq!(items[0]["value"]["delivery"], "notification");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_13t_a_preference_reports_unknown_delivery_when_the_radio_says_nothing() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, Some("prefer-indication"))
        .await
        .expect("a preference rides through");
    assert_eq!(field(&subscription, "observedDelivery"), &string("unknown"));
    assert_eq!(harness.radio().delivery_requests(), vec![None]);
}

// PR210-22 — retryability is the core's answer, never the code's.
#[test]
fn pr210_22_a_dispatcher_error_is_never_retryable_by_its_code() {
    for code in [
        BleErrorCode::OperationAborted,
        BleErrorCode::OperationTimedOut,
    ] {
        let error = DispatchError::new(code, "ipc", "tauri.test");
        assert_eq!(error.retryability, Retryability::Never);
        let wire = error.normalized_error().into_wire();
        assert_eq!(wire["retryability"], "never");
        assert_eq!(wire["commit"], Value::Null);
    }
}

// PR210-32 — racing first calls open exactly one radio.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_32_racing_first_calls_open_exactly_one_authority() {
    let opens = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Notify::new());
    let opener: AuthorityOpener = {
        let opens = Arc::clone(&opens);
        let gate = Arc::clone(&gate);
        Arc::new(move || {
            let opens = Arc::clone(&opens);
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                opens.fetch_add(1, AtomicOrdering::SeqCst);
                gate.notified().await;
                let central = ubm_desktop::executor::desktop_runtime()
                    .spawn(DesktopCentral::open(FakeRadio::new(), "tauri-race"))
                    .await
                    .expect("open joins")
                    .map_err(|error| DispatchError::from_core(&error))?;
                let authority: Arc<dyn CoreAuthority> = Arc::new(central);
                Ok(authority)
            })
        })
    };
    let dispatcher = BtleplugDispatcher::with_authority_opener(opener);
    let first = tokio::spawn({
        let dispatcher = dispatcher.clone();
        async move { dispatcher.ensure_authority().await }
    });
    let second = tokio::spawn({
        let dispatcher = dispatcher.clone();
        async move { dispatcher.ensure_authority().await }
    });
    while opens.load(AtomicOrdering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.notify_waiters();
    let first = first
        .await
        .expect("joins")
        .map_err(|error| error.describe());
    let second = second
        .await
        .expect("joins")
        .map_err(|error| error.describe());
    let (first, second) = (first.expect("first opens"), second.expect("second shares"));
    assert_eq!(opens.load(AtomicOrdering::SeqCst), 1, "exactly one radio");
    assert!(Arc::ptr_eq(&first, &second), "one shared central");
}

// Finding 217 follow-up — the effective ATT MTU the OS reports crosses the
// dispatcher: macOS derives `maximumWriteValueLength(.withResponse) + 3`,
// Windows reads `GattSession.MaxPduSize`, Linux reads the BlueZ
// characteristic MTU. A withheld measurement is never synthesized.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connected_effective_mtu_reads_the_live_link_through_the_core() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let error = harness
        .execute(
            "connection.effective-mtu",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("an unmeasured MTU is not synthesized");
    assert_eq!(error.code, BleErrorCode::CapabilityUnsupported);
    harness.radio().set_effective_mtu("peer-a", 515);
    let mtu = harness
        .execute(
            "connection.effective-mtu",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("the OS measurement crosses");
    assert_eq!(field(&mtu, "mtu"), &number(515));
}

// PARITY §4 — connected RSSI works again (4.x read it through btleplug).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connected_rssi_reads_the_live_link_through_the_core() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let error = harness
        .execute(
            "connection.rssi",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("an unmeasured RSSI is not synthesized");
    assert_eq!(error.code, BleErrorCode::CapabilityUnsupported);
    harness.radio().set_rssi("peer-a", -55);
    let rssi = harness
        .execute(
            "connection.rssi",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("the OS measurement crosses");
    assert_eq!(field(&rssi, "rssi"), &number(-55));
}

/// A route's attachment identity is compared field by field against the
/// record the renderer was given — without building (and so without
/// sampling platform authorization for) a new record on every route.
#[test]
fn a_route_attachment_matches_its_record_by_identity_alone() {
    let attachment = test_attachment();
    let IpcValue::Object(record) = attachment_record(&attachment) else {
        panic!("the attachment record is an object");
    };
    assert!(attachment_identity_matches(&record, &attachment));
    for (key, adapter_level) in [
        ("attachmentId", false),
        ("backendInstanceId", false),
        ("backendGeneration", false),
        ("adapterId", true),
        ("adapterGeneration", true),
    ] {
        let mut forged = record.clone();
        let target = if adapter_level {
            match forged.get_mut("adapter") {
                Some(IpcValue::Object(adapter)) => adapter,
                _ => panic!("adapter record"),
            }
        } else {
            &mut forged
        };
        target.insert(key.to_owned(), string("forged"));
        assert!(
            !attachment_identity_matches(&forged, &attachment),
            "a forged {key} is refused"
        );
    }
}

/// A compensation that hangs is bounded by the cleanup liveness backstop
/// and becomes debt instead of stalling the refused start (PR210-08).
#[tokio::test(start_paused = true)]
async fn pr210_08_a_hung_compensation_is_bounded_and_becomes_debt() {
    let harness = Harness::on_current_runtime().await;
    harness.radio().block_op(FaultOp::StartScan);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(vec![("query", empty_scan_query())]);
    let start = tokio::spawn(async move {
        dispatcher
            .start_scan(&caller, payload, OpControl::unbounded())
            .await
    });
    while count(&harness.radio().calls(), "start_scan") == 0 {
        tokio::task::yield_now().await;
    }
    harness
        .dispatcher
        .inner
        .lock()
        .await
        .callers
        .remove(&harness.key());
    harness.radio().block_op(FaultOp::StopScan);
    harness.radio().unblock_op(FaultOp::StartScan);
    let started = tokio::time::Instant::now();
    let error = start
        .await
        .expect("joins")
        .expect_err("nobody owns the scan");
    assert_eq!(error.code, BleErrorCode::OwnershipDenied);
    assert!(
        started.elapsed() <= ubm_desktop::LIVENESS_CLEANUP + Duration::from_secs(1),
        "the hung stop is bounded by the cleanup backstop"
    );
    assert_eq!(harness.dispatcher.inner.lock().await.orphan_debt.len(), 1);
    assert!(harness.central.active_scan_id().is_some(), "still owned");
    harness.radio().unblock_op(FaultOp::StopScan);
}

/// A release that hangs is bounded and keeps the link for a real retry
/// (PR210-09).
#[tokio::test(start_paused = true)]
async fn pr210_09_a_hung_disconnect_is_bounded_and_keeps_the_link() {
    let harness = Harness::on_current_runtime().await;
    let link = harness.connect("peer-a").await;
    harness.radio().block_op(FaultOp::Disconnect);
    let started = tokio::time::Instant::now();
    let error = harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("the hung release times out");
    assert_eq!(error.code, BleErrorCode::OperationTimedOut);
    assert_eq!(
        error.platform.as_deref(),
        Some(ubm_desktop::LIVENESS_BACKSTOP_DETAIL),
        "no caller budget: the liveness backstop decided, and says so"
    );
    assert!(started.elapsed() <= ubm_desktop::LIVENESS_CLEANUP + Duration::from_secs(1));
    assert!(
        harness
            .with_caller(|caller| caller
                .connections
                .get(&link.handle)
                .is_some_and(|connection| {
                    matches!(connection.phase, ReleasePhase::ReleaseFailed)
                }))
            .await
    );
    harness.radio().unblock_op(FaultOp::Disconnect);
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("the retry releases");
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 2);
    assert!(
        harness
            .with_caller(|caller| caller.connections.is_empty())
            .await
    );
}

// ---------------------------------------------------------------------------
// PR #210 findings 43, 57, 58, 60 — the Tauri half.
// ---------------------------------------------------------------------------

/// A radio scripted as a desktop OS radio: `policy` gate, teardown on loss.
fn os_radio(policy: AdmissionPolicy) -> FakeRadio {
    let radio = FakeRadio::new();
    radio.set_os_policy(policy, true);
    radio
}

impl Harness {
    /// The terminal item of one stream, once delivered.
    async fn wait_terminal(&self, stream: &str) -> Value {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some(terminal) = self
                .items(stream)
                .into_iter()
                .find(|item| item["kind"] == "terminal")
            {
                return terminal;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "stream {stream} never ended: {:?}",
                self.items(stream)
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Drop the adapter out from under live work and wait for the core's
    /// reset report.
    async fn lose_adapter(&self) -> AdapterResetEvent {
        let mut resets = self.central.adapter_reset_events();
        self.radio()
            .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOff));
        tokio::time::timeout(WAIT, resets.recv())
            .await
            .expect("the core reports the reset")
            .expect("reset event")
    }

    async fn adapter_state(&self) -> Value {
        let response = self
            .execute("adapter.state", Vec::new(), None, OpControl::unbounded())
            .await
            .expect("adapter.state answers");
        field(&response, "state").clone().into_wire()
    }
}

fn version_offer() -> BTreeMap<String, IpcValue> {
    let range = |axis: &str, value: i64| {
        let version = object([("axis", string(axis)), ("value", number(value))]);
        object([
            ("axis", string(axis)),
            ("minimum", version.clone()),
            ("maximum", version),
        ])
    };
    let IpcValue::Object(offer) = object([
        ("backendContract", range("backend-contract", 1)),
        ("capabilitySchema", range("capability-schema", 1)),
        ("eventSchema", range("event-schema", 1)),
        ("traceFormat", range("trace-format", 1)),
        ("ipcProtocol", range("ipc-protocol", 4)),
    ]) else {
        panic!("the version offer is an object");
    };
    offer
}

// Finding 43 — the attachment identity is the shared central's own: no
// second btleplug Manager (a second CBCentralManager on macOS) is opened
// for it, and the adapter it names is the one the central runs on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_43_the_attachment_is_the_shared_central_s_own() {
    let harness = Harness::new().await;
    let core = harness.central.attachment();
    let attachment = &harness.attachment;
    assert_eq!(attachment.attachment_id, core.attachment_id().as_str());
    assert_eq!(
        attachment.backend_instance_id,
        core.backend_instance_id().as_str()
    );
    assert_eq!(
        attachment.backend_generation,
        core.backend_generation().as_str()
    );
    assert_eq!(attachment.adapter_id, core.adapter_id().as_str());
    assert_eq!(
        attachment.adapter_generation,
        core.adapter_generation().as_str()
    );
    let state = harness.adapter_state().await;
    assert_eq!(
        state["backendGeneration"],
        core.backend_generation().as_str()
    );
}

// Finding 57 — an adapter loss ends everything live on it in the legacy
// vocabulary: links `connected → lost` with cause `adapter-loss` (legacy
// `connection-state-changed` reason `adapter`), notification streams and
// the scan `source-failed`, databases stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_57_an_adapter_loss_ends_every_stream_in_the_legacy_vocabulary() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let scan = harness
        .execute(
            "scan.start",
            vec![("query", empty_scan_query())],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("scan starts");
    let scan_stream = text(&scan, "handle");
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    harness.ready("connection-events-ipc-1").await;

    let reset = harness.lose_adapter().await;
    assert_eq!(reset.released_links, vec!["peer-a".to_owned()]);

    let items = harness.wait_items("connection-events-ipc-1", 3).await;
    assert_eq!(items[1]["value"]["previous"], "connected");
    assert_eq!(items[1]["value"]["current"], "lost");
    assert_eq!(items[1]["value"]["cause"], "adapter-loss");
    assert_eq!(
        items[1]["value"]["attachmentId"],
        harness.attachment.attachment_id.as_str(),
        "the transition reports the attachment the link lived on"
    );
    assert_eq!(items[2]["kind"], "terminal");
    assert_eq!(items[2]["reason"], "connection-lost");

    let ended = harness.wait_terminal(&notifications).await;
    assert_eq!(ended["reason"], "source-failed");
    assert_eq!(ended["error"]["code"], "operation.reset");

    let scan_end = harness.wait_terminal(&scan_stream).await;
    assert_eq!(scan_end["reason"], "source-failed");
    assert!(
        scan_end["error"]["platform"]["safeMessage"]
            .as_str()
            .is_some_and(|detail| detail.contains("powered-off")),
        "the core's own words ride along: {scan_end:?}"
    );
    assert!(
        harness.with_caller(|caller| caller.scan.is_none()).await,
        "the core released the scan owner; nothing is left to stop"
    );
    assert!(
        harness
            .with_caller(|caller| !caller.databases[&database.handle].valid)
            .await,
        "the database of the lost link is stale"
    );
}

// Finding 57 — the new attachment generation reaches the renderer the way
// the IPC represents a backend restart: routes on the previous attachment
// fail `backend.reset` (recreate the manager) before any native I/O, the
// releases a renderer owes still run, and a fresh attach binds the new
// generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_57_a_route_validated_before_rebind_stays_on_its_original_attachment() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let request = harness.route_request(
        "gatt.read",
        "gatt.read-validated-before-rebind",
        Harness::gatt_entries(&link, &database, CONTROL_POINT),
        None,
    );
    let envelope = into_object(
        required_value(&request, "envelope", "test.route-envelope")
            .expect("route envelope")
            .clone(),
        "test.route-envelope",
    )
    .expect("route envelope object");
    let validated_attachment = harness
        .dispatcher
        .validate_envelope(&harness.caller, "gatt.read", &envelope)
        .await
        .expect("the original attachment validates before reset");

    let reset = harness.lose_adapter().await;
    harness
        .dispatcher
        .rebind_callers(reset.previous.attachment_id().as_str(), &reset.current)
        .await;
    assert_eq!(
        harness
            .dispatcher
            .bound_attachment(&harness.caller, "test.route-attachment")
            .await
            .expect("caller remains bound")
            .attachment_id,
        reset.current.attachment_id().as_str(),
        "the caller is rebound after the old envelope was validated"
    );
    let reads = count(&harness.radio().calls(), "read");
    let error = harness
        .dispatcher
        .execute_for_attachment(
            &harness.caller,
            "gatt.read",
            &validated_attachment,
            Harness::lease_payload(Harness::gatt_entries(&link, &database, CONTROL_POINT)),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("the route remains bound to the attachment it validated");

    assert_eq!(
        error.identity(),
        (
            "backend.reset",
            "adapter",
            "tauri.route-attachment".to_owned()
        )
    );
    assert_eq!(error.commit, Some(CommitState::NotDispatched));
    assert_eq!(
        count(&harness.radio().calls(), "read"),
        reads,
        "no native I/O"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_57_the_previous_attachment_fails_backend_reset_before_native_io() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let reset = harness.lose_adapter().await;
    assert_eq!(
        reset.previous.backend_generation().as_str(),
        harness.attachment.backend_generation
    );
    harness
        .radio()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOn));

    let reads = count(&harness.radio().calls(), "read");
    for (command, entries) in [
        (
            "gatt.read",
            Harness::gatt_entries(&link, &database, CONTROL_POINT),
        ),
        ("adapter.state", Vec::new()),
        ("scan.start", vec![("query", empty_scan_query())]),
    ] {
        let error = harness
            .route(command, &format!("{command}-stale"), entries, None)
            .await
            .expect_err("a route on the previous attachment is refused");
        assert_eq!(
            error.identity(),
            (
                "backend.reset",
                "adapter",
                "tauri.route-attachment".to_owned()
            ),
            "{command}"
        );
        assert_eq!(error.commit, Some(CommitState::NotDispatched));
    }
    assert_eq!(
        count(&harness.radio().calls(), "read"),
        reads,
        "no native I/O"
    );
    assert!(!harness.radio().scan_active());

    // The releases the renderer owes for the old attachment still run.
    harness
        .route(
            "connection.disconnect",
            "disconnect-after-reset",
            Harness::link_entries(&link),
            None,
        )
        .await
        .expect("a link the adapter took releases");
    assert!(
        harness
            .with_caller(|caller| caller.connections.is_empty())
            .await
    );
    let cleanup = harness.dispatcher.release(&harness.key()).await;
    assert!(super::is_released(&cleanup), "{cleanup:?}");

    // A fresh attach binds the central's new generation.
    let second = AuthenticatedCaller::new("test-app".to_owned(), "second".to_owned());
    let sink = IpcEventSink::new(Channel::new(|_| Ok(())));
    let response = harness
        .dispatcher
        .bootstrap(second, sink, version_offer())
        .await
        .expect("a fresh attach succeeds");
    let bootstrap = field(&response, "bootstrap");
    let attachment = field(bootstrap, "attachment");
    assert_eq!(
        text(attachment, "backendGeneration"),
        reset.current.backend_generation().as_str()
    );
    assert_eq!(
        text(attachment, "attachmentId"),
        reset.current.attachment_id().as_str()
    );
    assert_ne!(
        text(attachment, "backendGeneration"),
        harness.attachment.backend_generation
    );
}

// Finding 58 — admission errors from the core cross unchanged: code,
// domain, operation, detail, retryability and commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_58_admission_refusals_cross_verbatim() {
    for (power, authorization, expected) in [
        (
            AdapterPowerState::PoweredOff,
            AdapterAuthorization::Granted,
            "adapter.powered-off",
        ),
        (
            AdapterPowerState::Resetting,
            AdapterAuthorization::Granted,
            "adapter.resetting",
        ),
        (
            AdapterPowerState::PoweredOn,
            AdapterAuthorization::Denied,
            "permission.denied",
        ),
        (
            AdapterPowerState::PoweredOn,
            AdapterAuthorization::Restricted,
            "permission.restricted",
        ),
        (
            AdapterPowerState::PoweredOn,
            AdapterAuthorization::NotDetermined,
            "permission.not-determined",
        ),
    ] {
        let radio = FakeRadio::new();
        radio.set_os_policy(AdmissionPolicy::CoreBluetooth, false);
        radio.set_adapter_state(power);
        radio.set_adapter_authorization(authorization);
        let harness = Harness::with_radio(radio).await;
        harness.advertise("peer-a").await;
        let refused = harness
            .execute(
                "connection.connect",
                vec![("peerId", string("peer-a"))],
                None,
                OpControl::unbounded(),
            )
            .await
            .expect_err("admission refuses before any effect");
        let core = harness
            .central
            .connect("peer-a", "lease-direct", OpControl::unbounded())
            .await
            .expect_err("the core refuses the same way");
        assert_eq!(refused.code.as_str(), expected);
        assert_eq!(
            wire_error(&refused),
            wire_error(&DispatchError::from_core(&core)),
            "{expected} crosses unchanged"
        );
        assert_eq!(count(&harness.radio().calls(), "connect"), 0);
    }
}

// Finding 94 — an adapter.state read admitted before an adapter reset and
// still in flight when it lands answers the post-transition snapshot, as
// every legacy host did: never `operation.reset` or `backend.reset`. The
// radio read is held (a barrier, not timing) until the reset is published.
// A request that arrives after the reset still fails `backend.reset`
// (finding 57).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_94_an_adapter_state_read_racing_a_reset_answers_the_new_state() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    harness
        .radio()
        .set_adapter_authorization(AdapterAuthorization::Granted);
    harness
        .radio()
        .set_adapter_state(AdapterPowerState::PoweredOn);
    harness.radio().block_op(FaultOp::AdapterState);
    let read = harness.execute("adapter.state", Vec::new(), None, OpControl::unbounded());
    let reset = async {
        let deadline = tokio::time::Instant::now() + WAIT;
        while count(&harness.radio().calls(), "adapter_state") == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the read reaches the radio"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        harness
            .radio()
            .set_adapter_state(AdapterPowerState::PoweredOff);
        let reset = harness.lose_adapter().await;
        harness.radio().unblock_op(FaultOp::AdapterState);
        reset
    };
    let (read, reset) = tokio::join!(read, reset);
    let state = field(&read.expect("the racing read answers"), "state")
        .clone()
        .into_wire();
    assert_eq!(state["power"], "off");
    assert_eq!(
        state["backendGeneration"],
        reset.current.backend_generation().as_str(),
        "the snapshot carries the generation it was read under"
    );
}

// Finding 60 — adapter.state reports the central's facts in the IPC
// vocabulary: resetting / unsupported power, authorization as the OS
// reported it, and a stated reason where the platform cannot answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_60_adapter_state_reports_the_central_s_facts() {
    let harness = Harness::new().await;
    let radio = harness.radio();
    radio.set_adapter_authorization(AdapterAuthorization::Granted);
    for (power, availability, wire_power) in [
        (AdapterPowerState::PoweredOn, "available", "on"),
        (AdapterPowerState::PoweredOff, "available", "off"),
        (AdapterPowerState::Resetting, "available", "resetting"),
        (AdapterPowerState::Unknown, "available", "unknown"),
    ] {
        radio.set_adapter_state(power);
        let state = harness.adapter_state().await;
        assert_eq!(state["availability"], availability, "{power:?}");
        assert_eq!(state["power"], wire_power, "{power:?}");
        assert_eq!(state["authorization"], "granted", "{power:?}");
    }

    radio.set_adapter_state(AdapterPowerState::Unsupported);
    let state = harness.adapter_state().await;
    assert_eq!(state["availability"], "unsupported");
    assert_eq!(state["power"], "unsupported");
    assert_eq!(state["authorization"], "unavailable");
    assert!(state["safeReason"].is_string());

    // CoreBluetooth's Unauthorized is the legacy `authorization: denied`.
    radio.set_adapter_state(AdapterPowerState::Unauthorized);
    let state = harness.adapter_state().await;
    assert_eq!(state["power"], "unknown");
    assert_eq!(state["authorization"], "denied");

    radio.set_adapter_state(AdapterPowerState::PoweredOn);
    for (authorization, wire) in [
        (AdapterAuthorization::Denied, "denied"),
        (AdapterAuthorization::Restricted, "restricted"),
        (AdapterAuthorization::NotDetermined, "not-determined"),
    ] {
        radio.set_adapter_authorization(authorization);
        assert_eq!(harness.adapter_state().await["authorization"], wire);
    }

    // `heard` is the boundary's own peer list (the scripted radio lists
    // none), never a dispatcher count.
    harness.advertise("peer-a").await;
    assert_eq!(harness.adapter_state().await["heard"], 0);
    assert!(harness.radio().calls().iter().any(|call| call == "peers"));
}

// Finding 60 — a radio that cannot read authorization says so: `unknown`
// with the core's reason, never a denial and never silently absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_60_an_unreadable_authorization_is_unknown_with_a_reason() {
    let harness = Harness::new().await;
    harness
        .radio()
        .set_adapter_state(AdapterPowerState::PoweredOn);
    let state = harness.adapter_state().await;
    assert_eq!(state["authorization"], "unknown");
    assert!(
        state["safeReason"]
            .as_str()
            .is_some_and(|reason| reason.contains("authorization")),
        "{state:?}"
    );
    assert_eq!(state["power"], "on");
}

// Finding 60 — after a reset the attachment the renderer re-binds reports
// the new backend generation in adapter.state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_60_adapter_state_follows_the_reset_generation() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let reset = harness.lose_adapter().await;
    let rebound = harness
        .dispatcher
        .ensure_adapter()
        .await
        .expect("the new attachment");
    assert_eq!(
        rebound.backend_generation,
        reset.current.backend_generation().as_str()
    );
    assert_eq!(rebound.adapter_id, harness.attachment.adapter_id);
}

// Finding 90 — the maximum write length is the core's answer per write
// mode, and equals what a write of that mode actually accepts: with an OS
// long write (Windows/Linux) a with-response write takes up to 512 bytes
// while a write without response stays one ATT payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_90_the_maximum_write_length_is_what_a_write_accepts_per_mode() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    harness
        .radio()
        .set_write_limits("peer-a", WriteLimits::os_long_write(185).expect("limits"));
    let maximum = |mode: &'static str| {
        let mut entries = Harness::link_entries(&link);
        entries.push(("mode", string(mode)));
        harness.execute(
            "connection.maximum-write-length",
            entries,
            None,
            OpControl::unbounded(),
        )
    };
    let with_response = maximum("with-response").await.expect("with response");
    assert_eq!(field(&with_response, "bytes"), &number(512));
    let without_response = maximum("without-response").await.expect("without response");
    assert_eq!(field(&without_response, "bytes"), &number(182));
    let error = maximum("sometimes").await.expect_err("an unknown mode");
    assert_eq!(error.code, BleErrorCode::ArgumentInvalid);

    let write = |correlation: &'static str, length: usize| {
        let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
        entries.push(("mode", string("with-response")));
        harness.route("gatt.write", correlation, entries, Some(vec![0x5a; length]))
    };
    write("write-at-maximum", 512)
        .await
        .expect("the reported maximum is accepted");
    let refused = write("write-past-maximum", 513)
        .await
        .expect_err("one byte past the maximum is refused");
    assert_ne!(refused.code, BleErrorCode::OperationTimedOut);
}

// Finding 95 — a discovery the core refuses whole crosses the IPC with its
// typed identity unchanged (a malformed platform UUID is
// `protocol.malformed`, a database past the ATT handle space
// `capability.limited`); nothing is skipped, no database handle is minted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_95_a_refused_discovery_crosses_the_ipc_whole_and_typed() {
    let malformed = {
        let mut service = hrm_service();
        service.characteristics[1].uuid = "not-a-uuid".to_owned();
        vec![service]
    };
    let oversized = {
        let mut service = hrm_service();
        let template = service.characteristics[0].clone();
        service.characteristics = (0..65_535u32)
            .map(|index| CharacteristicSnapshot {
                uuid: format!("{:08x}-0000-1000-8000-00805f9b34fb", 0x10_0000 + index),
                ..template.clone()
            })
            .collect();
        vec![service]
    };
    for (services, code, operation) in [
        (malformed, "protocol.malformed", "discovery.snapshot.uuid"),
        (oversized, "capability.limited", "discovery.database-bound"),
    ] {
        let harness = Harness::new().await;
        let link = harness.connect("peer-r").await;
        harness.radio().set_services("peer-r", services);
        let refused = harness
            .execute(
                "gatt.discover",
                Harness::link_entries(&link),
                None,
                OpControl::unbounded(),
            )
            .await
            .expect_err("the discovery is refused whole");
        assert_eq!(
            refused.identity(),
            (code, "gatt", operation.to_owned()),
            "{code}"
        );
        assert_eq!(
            harness.with_caller(|caller| caller.databases.len()).await,
            0,
            "no database handle for a refused discovery"
        );
    }
}

// Finding 114 — the release of a resource nobody could own (a cancelled or
// refused scan, connect or subscribe that the core still admitted) is
// retried automatically on the Tauri 4.x schedule: 8 attempts in all, the
// retries 100 ms apart doubling to 5 s. The debt stays observable while it
// retries; exhaustion is reported as `tauri.quarantine.exhausted`; a window
// released meanwhile takes the debt over and the retries still end it.
impl Harness {
    /// A scan the core admitted for a lease that was replaced while it
    /// started: nobody can own it, so its compensation runs. `refusals`
    /// scan stops fail first (the compensation's own attempt included).
    async fn orphan_a_scan(&self, refusals: usize) -> OperationId {
        self.radio().block_op(FaultOp::StartScan);
        let dispatcher = self.dispatcher.clone();
        let caller = self.caller.clone();
        let payload = Self::lease_payload(vec![("query", empty_scan_query())]);
        let start = tokio::spawn(async move {
            dispatcher
                .start_scan(&caller, payload, OpControl::unbounded())
                .await
        });
        while count(&self.radio().calls(), "start_scan") == 0 {
            tokio::task::yield_now().await;
        }
        self.replace_lease().await;
        for attempt in 0..refusals {
            self.radio()
                .fail_next(FaultOp::StopScan, &format!("stop refused {attempt}"));
        }
        self.radio().unblock_op(FaultOp::StartScan);
        let error = start
            .await
            .expect("joins")
            .expect_err("a replaced lease cannot own the scan");
        assert_eq!(error.identity().2, "tauri.scan-stale-lease");
        self.central
            .active_scan_id()
            .expect("the compensation failed")
    }

    async fn debt(&self) -> Vec<(u32, bool)> {
        self.dispatcher
            .inner
            .lock()
            .await
            .orphan_debt
            .iter()
            .map(|debt| (debt.attempts, debt.exhausted))
            .collect()
    }

    fn stops(&self) -> usize {
        count(&self.radio().calls(), "stop_scan")
    }
}

/// Let the paused clock run `millis` forward and every task settle.
async fn elapse(millis: u64) {
    tokio::time::sleep(Duration::from_millis(millis)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn finding_114_a_failed_compensation_retries_until_the_release_lands() {
    let harness = Harness::on_current_runtime().await;
    // The compensation and the first two retries are refused; the third
    // retry (attempt 4) releases the scan.
    harness.orphan_a_scan(3).await;
    assert_eq!(harness.stops(), 1);
    assert_eq!(harness.debt().await, vec![(1, false)], "observable debt");

    elapse(100).await;
    assert_eq!(harness.stops(), 2, "first retry after 100 ms");
    assert_eq!(harness.debt().await, vec![(2, false)]);
    elapse(200).await;
    assert_eq!(harness.stops(), 3, "second retry 200 ms later");
    elapse(400).await;
    assert_eq!(harness.stops(), 4, "third retry 400 ms later");
    assert!(harness.debt().await.is_empty(), "the release landed");
    assert_eq!(harness.central.active_scan_id(), None);

    elapse(60_000).await;
    assert_eq!(harness.stops(), 4, "nothing retries a settled debt");
    let cleanup = harness.dispatcher.release(&harness.key()).await;
    assert_eq!(cleanup.into_wire()["state"], "released");
}

#[tokio::test(start_paused = true)]
async fn finding_114_eight_refused_attempts_exhaust_and_are_reported() {
    let harness = Harness::on_current_runtime().await;
    harness.orphan_a_scan(9).await;
    let mut expected_stops = 1;
    for delay in [100, 200, 400, 800, 1_600, 3_200, 5_000] {
        elapse(delay).await;
        expected_stops += 1;
        assert_eq!(harness.stops(), expected_stops, "retry after {delay} ms");
    }
    assert_eq!(
        harness.debt().await,
        vec![(8, true)],
        "exhausted, still owed"
    );
    elapse(60_000).await;
    assert_eq!(harness.stops(), 8, "no retry after exhaustion");
    assert!(harness.central.active_scan_id().is_some(), "still owned");

    // The window's release makes one more attempt (the ninth refusal) and
    // reports the exhausted debt.
    let cleanup = harness.dispatcher.release(&harness.key()).await.into_wire();
    assert_eq!(cleanup["state"], "release-failed");
    assert_eq!(cleanup["failures"][0]["resourceKind"], "scan");
    assert_eq!(
        cleanup["failures"][0]["error"]["operation"],
        "tauri.quarantine.exhausted"
    );
    assert_eq!(harness.stops(), 9);

    // Nothing refuses any more: the next release lands.
    let cleanup = harness.dispatcher.release(&harness.key()).await.into_wire();
    assert_eq!(cleanup["state"], "released");
    assert_eq!(harness.central.active_scan_id(), None);
}

#[tokio::test(start_paused = true)]
async fn finding_114_a_window_released_during_retries_still_ends_the_orphan() {
    let harness = Harness::on_current_runtime().await;
    // The compensation and the window's release are refused; the automatic
    // retry that follows releases the scan after the window is gone.
    harness.orphan_a_scan(2).await;
    elapse(50).await;
    let cleanup = harness.dispatcher.release(&harness.key()).await.into_wire();
    assert_eq!(cleanup["state"], "release-failed");
    assert_eq!(
        cleanup["failures"][0]["error"]["operation"],
        "tauri.release.orphan"
    );
    assert_eq!(harness.stops(), 2);
    assert_eq!(
        harness.debt().await.len(),
        1,
        "the debt outlives the release"
    );

    elapse(50).await;
    assert_eq!(harness.stops(), 3, "the retry schedule continues");
    assert!(harness.debt().await.is_empty());
    assert_eq!(harness.central.active_scan_id(), None, "no orphan stays up");
    elapse(60_000).await;
    assert_eq!(harness.stops(), 3);
}

#[tokio::test(start_paused = true)]
async fn finding_114_a_release_that_lands_first_ends_the_retries() {
    let harness = Harness::on_current_runtime().await;
    harness.orphan_a_scan(1).await;
    elapse(50).await;
    let cleanup = harness.dispatcher.release(&harness.key()).await.into_wire();
    assert_eq!(cleanup["state"], "released");
    assert_eq!(harness.stops(), 2);
    elapse(60_000).await;
    assert_eq!(harness.stops(), 2, "no retry of a released orphan");
    assert!(harness.debt().await.is_empty());
}

// Finding 116 — the OS's own answer behind a failure crosses as the same
// per-OS platform identity the Node desktop path reports; a failure without
// one keeps the Tauri 4.x `btleplug` / `native-error` shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_116_the_platform_identity_crosses_per_os() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let read = || {
        harness.execute(
            "gatt.read",
            Harness::gatt_entries(&link, &database, CONTROL_POINT),
            None,
            OpControl::unbounded(),
        )
    };
    for (platform, expected) in [
        (
            PlatformDetail::new("corebluetooth", "10")
                .with_message("The connection has timed out."),
            serde_json::json!({
                "domain": "corebluetooth",
                "code": "10",
                "safeMessage": "The connection has timed out.",
                "metadata": {}
            }),
        ),
        (
            PlatformDetail::new("winrt", "gatt-protocol-error")
                .with_message("The attribute requires authentication.")
                .with_metadata("hresult", PlatformValue::Int(-2_140_864_509))
                .with_metadata("gattStatus", PlatformValue::Int(5)),
            serde_json::json!({
                "domain": "winrt",
                "code": "gatt-protocol-error",
                "safeMessage": "The attribute requires authentication.",
                "metadata": { "hresult": -2_140_864_509_i64, "gattStatus": 5 }
            }),
        ),
        (
            // No platform message: the core's detail is the safe message,
            // as on the Node desktop path.
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.NotPermitted"),
            serde_json::json!({
                "domain": "bluez-dbus",
                "code": "org.bluez.Error.NotPermitted",
                "safeMessage": "read refused",
                "metadata": {}
            }),
        ),
        (
            // An integer JavaScript cannot hold exactly crosses as its text.
            PlatformDetail::new("winrt", "unreachable")
                .with_message("m")
                .with_metadata("handle", PlatformValue::Int(i64::MAX))
                .with_metadata("retried", PlatformValue::Bool(true))
                .with_metadata("device", PlatformValue::Text("BluetoothLE#1".to_owned())),
            serde_json::json!({
                "domain": "winrt",
                "code": "unreachable",
                "safeMessage": "m",
                "metadata": {
                    "handle": i64::MAX.to_string(),
                    "retried": true,
                    "device": "BluetoothLE#1"
                }
            }),
        ),
    ] {
        harness
            .radio()
            .fail_next_with_platform(FaultOp::Read, "read refused", platform);
        let error = read().await.expect_err("the scripted OS failure");
        assert_eq!(wire_error(&error)["platform"], expected);
    }

    harness.radio().fail_next(FaultOp::Read, "peer went away");
    let error = read()
        .await
        .expect_err("an OS failure without platform detail");
    assert_eq!(
        wire_error(&error)["platform"],
        serde_json::json!({
            "domain": "btleplug",
            "code": "native-error",
            "safeMessage": "peer went away",
            "metadata": {}
        }),
        "the Tauri 4.x shape"
    );
}

// Finding 182 — the Polar H10 database through the Tauri dispatcher: a
// characteristic that carries descriptors must register exactly once.
// Descriptor-level core paths share their characteristic's path, and the
// desktop N-API path renders one record per characteristic (its grouping
// merges descriptor rows into the characteristic node). The dispatcher
// fanned one characteristic record out per descriptor row, so every H10
// characteristic with a CCCD or user-description descriptor (heart-rate
// measurement, battery level, device-info strings, PMD ECG) rejected the
// snapshot downstream with public-gatt.duplicate-characteristic-path.
fn h10_characteristic(
    uuid: &str,
    properties: PropertyFlags,
    descriptors: Vec<DescriptorSnapshot>,
) -> CharacteristicSnapshot {
    CharacteristicSnapshot {
        uuid: uuid.to_owned(),
        occurrence: 0,
        properties,
        descriptors,
    }
}

fn cccd() -> DescriptorSnapshot {
    DescriptorSnapshot {
        uuid: "00002902-0000-1000-8000-00805f9b34fb".to_owned(),
        occurrence: 0,
    }
}

/// The Polar H10 E9B93D29 service set from the iOS/Android discovered
/// events: 1800, 1801, 180D, 180A, 180F, the Polar custom service, PMD,
/// and FEEE — with the descriptors a real strap reports.
fn h10_services() -> Vec<ServiceSnapshot> {
    vec![
        ServiceSnapshot {
            uuid: "00001800-0000-1000-8000-00805f9b34fb".to_owned(),
            occurrence: 0,
            characteristics: vec![
                h10_characteristic(
                    "00002a00-0000-1000-8000-00805f9b34fb",
                    flags(true, false, false, false),
                    Vec::new(),
                ),
                h10_characteristic(
                    "00002a01-0000-1000-8000-00805f9b34fb",
                    flags(true, false, false, false),
                    Vec::new(),
                ),
            ],
        },
        ServiceSnapshot {
            uuid: "00001801-0000-1000-8000-00805f9b34fb".to_owned(),
            occurrence: 0,
            characteristics: vec![h10_characteristic(
                "00002a05-0000-1000-8000-00805f9b34fb",
                flags(false, false, false, true),
                Vec::new(),
            )],
        },
        ServiceSnapshot {
            uuid: "0000180d-0000-1000-8000-00805f9b34fb".to_owned(),
            occurrence: 0,
            characteristics: vec![
                h10_characteristic(
                    "00002a37-0000-1000-8000-00805f9b34fb",
                    flags(false, false, true, false),
                    vec![cccd()],
                ),
                h10_characteristic(
                    "00002a38-0000-1000-8000-00805f9b34fb",
                    flags(true, false, false, false),
                    Vec::new(),
                ),
                h10_characteristic(
                    "00002a39-0000-1000-8000-00805f9b34fb",
                    flags(false, true, false, false),
                    Vec::new(),
                ),
            ],
        },
        ServiceSnapshot {
            uuid: "0000180a-0000-1000-8000-00805f9b34fb".to_owned(),
            occurrence: 0,
            characteristics: vec![
                h10_characteristic(
                    "00002a29-0000-1000-8000-00805f9b34fb",
                    flags(true, false, false, false),
                    vec![DescriptorSnapshot {
                        uuid: "00002901-0000-1000-8000-00805f9b34fb".to_owned(),
                        occurrence: 0,
                    }],
                ),
                h10_characteristic(
                    "00002a24-0000-1000-8000-00805f9b34fb",
                    flags(true, false, false, false),
                    Vec::new(),
                ),
            ],
        },
        ServiceSnapshot {
            uuid: "0000180f-0000-1000-8000-00805f9b34fb".to_owned(),
            occurrence: 0,
            characteristics: vec![h10_characteristic(
                "00002a19-0000-1000-8000-00805f9b34fb",
                flags(true, false, true, false),
                vec![cccd()],
            )],
        },
        ServiceSnapshot {
            uuid: "6217ff4b-fb31-1140-ad5a-a45545d7ecf3".to_owned(),
            occurrence: 0,
            characteristics: vec![h10_characteristic(
                "6217ff4c-fb31-1140-ad5a-a45545d7ecf3",
                flags(true, true, false, false),
                Vec::new(),
            )],
        },
        ServiceSnapshot {
            uuid: "fb005c80-02e7-f387-1cad-8acd2d8df0c8".to_owned(),
            occurrence: 0,
            characteristics: vec![h10_characteristic(
                "fb005c81-02e7-f387-1cad-8acd2d8df0c8",
                flags(false, false, true, false),
                vec![cccd()],
            )],
        },
        ServiceSnapshot {
            uuid: "0000feee-0000-1000-8000-00805f9b34fb".to_owned(),
            occurrence: 0,
            characteristics: vec![h10_characteristic(
                "0000feef-0000-1000-8000-00805f9b34fb",
                flags(true, false, false, false),
                Vec::new(),
            )],
        },
    ]
}

fn characteristic_keys(records: &[IpcValue]) -> Vec<String> {
    records
        .iter()
        .map(|record| {
            format!(
                "{}|{}|{}|{}",
                text(record, "serviceUuid"),
                text(record, "serviceOccurrence"),
                text(record, "characteristicUuid"),
                text(record, "characteristicOccurrence"),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_182_a_characteristic_with_descriptors_registers_once() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-h10").await;
    harness.radio().set_services("peer-h10", h10_services());
    let response = harness
        .execute(
            "gatt.discover",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("discover");
    let IpcValue::Array(characteristics) = field(&response, "characteristics") else {
        panic!("characteristics must be an array");
    };
    let IpcValue::Array(descriptors) = field(&response, "descriptors") else {
        panic!("descriptors must be an array");
    };
    // Twelve scripted characteristics, four scripted descriptors: one
    // record each, so the portable snapshot validates downstream.
    assert_eq!(
        characteristics.len(),
        12,
        "one characteristic record per characteristic: {characteristics:?}"
    );
    assert_eq!(
        descriptors.len(),
        4,
        "one descriptor record per descriptor: {descriptors:?}"
    );
    let mut keys = characteristic_keys(characteristics);
    keys.sort();
    keys.dedup();
    assert_eq!(
        keys.len(),
        12,
        "no duplicated characteristic path reaches validateTopology"
    );
    let descriptor_uuids: Vec<String> = descriptors
        .iter()
        .map(|record| text(record, "uuid"))
        .collect();
    for expected in [
        "00002902-0000-1000-8000-00805f9b34fb",
        "00002901-0000-1000-8000-00805f9b34fb",
    ] {
        assert!(
            descriptor_uuids.iter().any(|uuid| uuid == expected),
            "descriptor {expected} survives the render: {descriptor_uuids:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_182_a_second_discover_replaces_the_snapshot() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-h10").await;
    harness.radio().set_services("peer-h10", h10_services());
    let first = harness.discover(&link).await;
    let second_response = harness
        .execute(
            "gatt.discover",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("rediscover");
    let second_handle = text(&second_response, "handle");
    assert_ne!(
        first.handle, second_handle,
        "rediscovery mints a fresh database handle"
    );
    let IpcValue::Array(second_characteristics) = field(&second_response, "characteristics") else {
        panic!("characteristics must be an array");
    };
    assert_eq!(
        second_characteristics.len(),
        12,
        "the replaced snapshot still carries every characteristic once: {second_characteristics:?}"
    );
    // The first database is stale: a read through it fails with the
    // generation identity, never with the replaced tree's data.
    let mut entries = Harness::link_entries(&link);
    entries.extend([
        ("databaseHandle", string(first.handle.clone())),
        ("databaseId", string(first.id.clone())),
        ("databaseGeneration", string(first.generation.clone())),
        (
            "characteristicHandle",
            string(
                first
                    .characteristics
                    .get("00002a37-0000-1000-8000-00805f9b34fb")
                    .expect("heart-rate measurement handle")
                    .clone(),
            ),
        ),
    ]);
    let error = harness
        .execute("gatt.read", entries, None, OpControl::unbounded())
        .await
        .expect_err("a read on the replaced database fails");
    assert_eq!(
        error.identity(),
        (
            "gatt.stale-handle",
            "gatt",
            "tauri.characteristic-database-generation".to_owned()
        ),
        "stale generation, not appended state"
    );
}

// Findings F1/F2 — one vocabulary across hosts: a successful
// without-response write reports the contract word `unknown` (the same word
// Electron/Node report for the same physical event; the contract
// `WriteReceipt` allows only `confirmed`/`unknown`), and a with-response
// write reports `confirmed`. The shared renderer decoder rejects anything
// else as `protocol.malformed`, so a Tauri-only word would surface every
// successful without-response write as a protocol error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn findings_f1_f2_write_receipt_commit_state_matches_the_contract_on_both_modes() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let write = |correlation: &'static str, mode: &'static str| {
        let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
        entries.push(("mode", string(mode)));
        harness.route("gatt.write", correlation, entries, Some(vec![0x5a; 3]))
    };
    let without_response = write("write-without-response", "without-response")
        .await
        .expect("a without-response write succeeds");
    assert_eq!(text(&without_response, "mode"), "without-response");
    assert_eq!(
        text(&without_response, "commitState"),
        "unknown",
        "the contract word for an unconfirmed write, on every host"
    );
    assert_eq!(field(&without_response, "bytesSubmitted"), &number(3));
    let with_response = write("write-with-response", "with-response")
        .await
        .expect("a with-response write succeeds");
    assert_eq!(text(&with_response, "mode"), "with-response");
    assert_eq!(text(&with_response, "commitState"), "confirmed");
}
