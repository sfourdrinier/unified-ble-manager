//! Production-path notification-ingress harness (F07 + F09 + F13 proof).
//!
//! The external re-verification ruled F07/F13 FIX-INCOMPLETE purely for
//! proof: the flood/ambiguity/setup-failure tests exercise only fakes, so
//! reverting production code stays green. This harness closes that gap as
//! far as hardware allows by driving the REAL production functions from
//! `ubm_desktop::btleplug_backend` — the same items
//! [`BtleplugRadio::set_notifications`] calls — with scripted streams and a
//! scripted [`NotificationTransport`] leaf:
//!
//! * flood: the real [`spawn_notification_forwarder`] over a scripted
//!   [`ValueNotification`] stream into a real bounded channel, with exact
//!   drop/byte expectations derived from the real [`NOTIFICATION_CAP`] /
//!   [`NOTIFICATION_BYTES`];
//! * control under flood: the production `select!` shape (default unbiased
//!   mode, no `biased;` — tokio picks the first branch randomly) between the
//!   real flooded ingress receiver and a scripted control source, mirroring
//!   `BtleplugRadio::next_event`;
//! * ambiguity: the real [`NotificationRoute::matches`] filter inside two
//!   live real forwarders, the real [`route_is_ambiguous`] gate over the
//!   real [`live_scopes`], and the real [`ambiguous_routing_error`];
//! * setup failure: the real [`subscribe_and_stream`] sequencing
//!   (subscribe-ok-then-notifications-err split with compensating rollback)
//!   folded through the real [`apply_enable_stream_failure`];
//! * teardown failure: the real [`unsubscribe_and_fold`] (native-first,
//!   forwarder retained on failure) with values proven to keep flowing
//!   through the real retained forwarder, then a succeeding retry;
//! * byte accounting: the real [`ingress_try_reserve`] /
//!   [`ingress_release`] under concurrency (load-then-add overshoot fix) and
//!   on dequeue.
//!
//! HARDWARE-GATED (deliberately not claimed here): constructing the concrete
//! `btleplug::platform::{Adapter, Peripheral}` and calling the real
//! `BtleplugRadio::set_notifications` / `next_event` adapter arm. btleplug
//! 0.12 exposes no public constructor for its platform peripheral or
//! adapter (`Peripheral::new`, `Adapter::new`, and `DeviceId::new` are all
//! `pub(crate)`; `PeripheralId` wraps a `pub(crate)` `DeviceId` with no
//! parsing constructor), `Manager::new` requires a live BlueZ D-Bus session
//! (`BluetoothSession::new`), and `BtleplugRadio::open` is the only
//! constructor. The `btleplug::api::Peripheral` trait itself is public and
//! mockable; the production struct is monomorphized over the platform
//! types, so the seam stops at [`NotificationTransport`]. Everything below
//! that trait — D-Bus session, adapter enumeration, peripheral lookup, the
//! adapter event stream — needs a radio and stays queued in PARITY_GAPS.md.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::pin::Pin;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicU64, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;

use btleplug::api::{CharPropFlags, Characteristic, ValueNotification};
use futures_util::Stream;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use ubm_desktop::boundary::{FakeRadio, InstanceKey, RadioBoundary, RadioEvent};
use ubm_desktop::btleplug_backend::{
    EnableStreamError, ForwardTarget, ForwarderEntry, NOTIFICATION_BYTES, NOTIFICATION_CAP,
    NotificationRoute, NotificationStream, NotificationTransport, ambiguous_routing_error,
    apply_enable_stream_failure, forwarder_key, ingress_release, ingress_try_reserve, live_scopes,
    route_is_ambiguous, spawn_notification_forwarder, subscribe_and_stream, unsubscribe_and_fold,
};

const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";
const BATTERY_SERVICE: &str = "0000180f-0000-1000-8000-00805f9b34fb";
const PEER: &str = "peer-1";

fn uuid(text: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(text).expect("fixture uuid")
}

fn note(service: &str, characteristic: &str, value: Vec<u8>) -> ValueNotification {
    ValueNotification {
        uuid: uuid(characteristic),
        service_uuid: uuid(service),
        value,
    }
}

fn characteristic(service: &str, characteristic: &str) -> Characteristic {
    Characteristic {
        uuid: uuid(characteristic),
        service_uuid: uuid(service),
        properties: CharPropFlags::NOTIFY,
        descriptors: BTreeSet::new(),
    }
}

fn scope(
    peer: &str,
    service: &str,
    service_occurrence: u64,
    characteristic: &str,
    characteristic_occurrence: u64,
) -> InstanceKey {
    (
        peer.to_owned(),
        service.to_owned(),
        service_occurrence,
        characteristic.to_owned(),
        characteristic_occurrence,
    )
}

fn target_for(scope: &InstanceKey, epoch: u64) -> ForwardTarget {
    ForwardTarget {
        peer_id: scope.0.clone(),
        service_uuid: scope.1.clone(),
        service_occurrence: scope.2,
        characteristic_uuid: scope.3.clone(),
        characteristic_occurrence: scope.4,
        epoch,
    }
}

/// One real bounded ingress endpoint: the genuine production caps, the
/// genuine counters, one channel.
struct Ingress {
    sender: mpsc::Sender<RadioEvent>,
    receiver: mpsc::Receiver<RadioEvent>,
    queued: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

fn ingress() -> Ingress {
    let (sender, receiver) = mpsc::channel(NOTIFICATION_CAP);
    Ingress {
        sender,
        receiver,
        queued: Arc::new(AtomicU64::new(0)),
        dropped: Arc::new(AtomicU64::new(0)),
    }
}

/// Controllable scripted notification stream: values are pushed after the
/// real forwarder installed, modelling continued CCCD emission. Built on a
/// channel endpoint (whose waker registration the channel owns across
/// polls), never on a `Notify::notified()` future minted per poll — such a
/// future unregisters its waiter when dropped on `Pending`, silently losing
/// the wakeup whenever the forwarder parks before the push lands.
/// [`script_pair`] returns the test-side push handle plus the stream the
/// forwarder owns; dropping the push handle closes the stream.
#[derive(Debug)]
struct ScriptedStream {
    receiver: UnboundedReceiver<ValueNotification>,
}

impl Stream for ScriptedStream {
    type Item = ValueNotification;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<ValueNotification>> {
        Pin::new(&mut self.receiver).poll_recv(cx)
    }
}

fn script_pair() -> (UnboundedSender<ValueNotification>, ScriptedStream) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (sender, ScriptedStream { receiver })
}

/// Scripted [`NotificationTransport`] leaf: programmed subscribe / stream /
/// unsubscribe outcomes with exact call counts. Only the programmed calls
/// may happen — anything else is a test-design bug and panics.
struct StubTransport {
    subscribe: StdMutex<VecDeque<Result<(), btleplug::Error>>>,
    notifications: StdMutex<VecDeque<Result<NotificationStream, btleplug::Error>>>,
    unsubscribe: StdMutex<VecDeque<Result<(), btleplug::Error>>>,
    subscribe_calls: AtomicU64,
    notifications_calls: AtomicU64,
    unsubscribe_calls: AtomicU64,
}

impl StubTransport {
    fn new() -> Self {
        Self {
            subscribe: StdMutex::new(VecDeque::new()),
            notifications: StdMutex::new(VecDeque::new()),
            unsubscribe: StdMutex::new(VecDeque::new()),
            subscribe_calls: AtomicU64::new(0),
            notifications_calls: AtomicU64::new(0),
            unsubscribe_calls: AtomicU64::new(0),
        }
    }

    fn with_subscribe(self, outcome: Result<(), btleplug::Error>) -> Self {
        self.subscribe.lock().expect("stub").push_back(outcome);
        self
    }

    fn with_notifications(self, outcome: Result<NotificationStream, btleplug::Error>) -> Self {
        self.notifications.lock().expect("stub").push_back(outcome);
        self
    }

    fn with_unsubscribe(self, outcome: Result<(), btleplug::Error>) -> Self {
        self.unsubscribe.lock().expect("stub").push_back(outcome);
        self
    }

    fn script_error(detail: &str) -> btleplug::Error {
        btleplug::Error::NotSupported(detail.to_owned())
    }

    fn calls(&self) -> (u64, u64, u64) {
        (
            self.subscribe_calls.load(Ordering::Relaxed),
            self.notifications_calls.load(Ordering::Relaxed),
            self.unsubscribe_calls.load(Ordering::Relaxed),
        )
    }
}

impl NotificationTransport for StubTransport {
    async fn transport_subscribe(
        &self,
        _characteristic: &Characteristic,
    ) -> Result<(), btleplug::Error> {
        self.subscribe_calls.fetch_add(1, Ordering::Relaxed);
        self.subscribe
            .lock()
            .expect("stub")
            .pop_front()
            .expect("unstubbed transport_subscribe call")
    }

    async fn transport_notifications(&self) -> Result<NotificationStream, btleplug::Error> {
        self.notifications_calls.fetch_add(1, Ordering::Relaxed);
        self.notifications
            .lock()
            .expect("stub")
            .pop_front()
            .expect("unstubbed transport_notifications call")
    }

    async fn transport_unsubscribe(
        &self,
        _characteristic: &Characteristic,
    ) -> Result<(), btleplug::Error> {
        self.unsubscribe_calls.fetch_add(1, Ordering::Relaxed);
        self.unsubscribe
            .lock()
            .expect("stub")
            .pop_front()
            .expect("unstubbed transport_unsubscribe call")
    }
}

async fn recv_notification(
    receiver: &mut mpsc::Receiver<RadioEvent>,
    queued: &AtomicU64,
) -> RadioEvent {
    let event = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await
        .expect("notification arrives")
        .expect("ingress open");
    if let RadioEvent::Notification { ref value, .. } = event {
        ingress_release(queued, value.len() as u64);
    }
    event
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f07_flood_real_forwarder_exact_drops_and_byte_bounds() {
    // Flood the REAL production forwarder while nobody drains: every
    // expectation is derived from the REAL caps, never restated. Each value
    // is 2048 bytes so the BYTE bound (128 admits) binds strictly before
    // the item bound (256) — this test is sensitive to byte accounting, not
    // just channel capacity.
    const NOTES: usize = 300;
    const VALUE_LEN: usize = 2048;
    let byte_admit = NOTIFICATION_BYTES as usize / VALUE_LEN;
    let admitted = NOTES.min(NOTIFICATION_CAP).min(byte_admit);
    let dropped_expected = (NOTES - admitted) as u64;
    assert_eq!(byte_admit, 128, "fixture divides the byte cap exactly");
    assert_eq!(admitted, 128, "the byte bound binds before the item bound");
    assert_eq!(dropped_expected, 172, "exact expected overload drops");

    let handle = tokio::runtime::Handle::current();
    let scope = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let notes: Vec<ValueNotification> = (0..NOTES)
        .map(|i| {
            note(
                HRM_SERVICE,
                HRM_MEASUREMENT,
                vec![(i % 251) as u8; VALUE_LEN],
            )
        })
        .collect();
    let mut ingress = ingress();
    let route = NotificationRoute::new(uuid(HRM_SERVICE), uuid(HRM_MEASUREMENT));
    let join = spawn_notification_forwarder(
        &handle,
        futures_util::stream::iter(notes),
        route,
        target_for(&scope, 3),
        ingress.sender.clone(),
        Arc::clone(&ingress.queued),
        Arc::clone(&ingress.dropped),
    );
    tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("finite script drains")
        .expect("forwarder never panics");
    assert_eq!(
        ingress.dropped.load(Ordering::Relaxed),
        dropped_expected,
        "exact overload drops through the real ingress"
    );
    assert_eq!(
        ingress.queued.load(Ordering::Relaxed),
        (admitted * VALUE_LEN) as u64,
        "queued bytes sit exactly at the byte cap"
    );
    assert_eq!(
        ingress.receiver.len(),
        admitted,
        "channel holds every admit"
    );

    // Drain through the REAL release path: bytes return to exactly zero and
    // the queue empties — no leak, no negative accounting.
    for _ in 0..admitted {
        let event = recv_notification(&mut ingress.receiver, &ingress.queued).await;
        assert!(
            matches!(event, RadioEvent::Notification { .. }),
            "flood carries only notifications"
        );
    }
    assert_eq!(
        ingress.queued.load(Ordering::Relaxed),
        0,
        "bytes fully released"
    );
    assert!(
        ingress.receiver.try_recv().is_err(),
        "nothing lingers past the drain"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f07_control_disconnect_delivered_under_flood() {
    // Fill the REAL ingress completely, then run the production `select!`
    // shape (default unbiased mode, exactly as `BtleplugRadio::next_event`:
    // no `biased;`) between the flooded data receiver and a scripted
    // control source. The exact disconnect must arrive despite the flood.
    const VALUE_LEN: usize = 1024;
    let handle = tokio::runtime::Handle::current();
    let scope = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let notes: Vec<ValueNotification> = (0..NOTIFICATION_CAP)
        .map(|i| {
            note(
                HRM_SERVICE,
                HRM_MEASUREMENT,
                vec![(i % 251) as u8; VALUE_LEN],
            )
        })
        .collect();
    let mut ingress = ingress();
    let route = NotificationRoute::new(uuid(HRM_SERVICE), uuid(HRM_MEASUREMENT));
    let join = spawn_notification_forwarder(
        &handle,
        futures_util::stream::iter(notes),
        route,
        target_for(&scope, 3),
        ingress.sender.clone(),
        Arc::clone(&ingress.queued),
        Arc::clone(&ingress.dropped),
    );
    tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("finite script drains")
        .expect("forwarder never panics");
    assert_eq!(ingress.receiver.len(), NOTIFICATION_CAP, "ingress flooded");
    assert_eq!(ingress.dropped.load(Ordering::Relaxed), 0, "no drops yet");

    let (control_tx, mut control_rx) = mpsc::channel::<RadioEvent>(8);
    control_tx
        .send(RadioEvent::Disconnected(PEER.to_owned()))
        .await
        .expect("control queued");
    let mut data_seen = 0usize;
    let mut control_seen: Option<RadioEvent> = None;
    for _ in 0..100_000 {
        tokio::select! {
            notified = ingress.receiver.recv() => {
                let event = notified.expect("ingress open");
                if let RadioEvent::Notification { ref value, .. } = event {
                    ingress_release(&ingress.queued, value.len() as u64);
                }
                data_seen += 1;
            }
            control = control_rx.recv() => {
                control_seen = control;
                break;
            }
        }
    }
    assert_eq!(
        control_seen,
        Some(RadioEvent::Disconnected(PEER.to_owned())),
        "the exact disconnect arrives despite the data flood"
    );
    // Drain the rest through the real release path: exact accounting holds.
    while !ingress.receiver.is_empty() {
        recv_notification(&mut ingress.receiver, &ingress.queued).await;
        data_seen += 1;
    }
    assert_eq!(data_seen, NOTIFICATION_CAP, "every flooded item accounted");
    assert_eq!(
        ingress.queued.load(Ordering::Relaxed),
        0,
        "bytes fully released"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f09_same_characteristic_uuid_two_services_routes_only_to_owner() {
    // Two REAL forwarders share one ingress (as in production) and observe
    // the same peripheral-wide scripted content (as two
    // `peripheral.notifications()` streams over the same air). The same
    // characteristic UUID appears under two services with distinguishable
    // values: every byte must reach only its owner.
    let handle = tokio::runtime::Handle::current();
    let scope_hrm = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let scope_battery = scope(PEER, BATTERY_SERVICE, 0, HRM_MEASUREMENT, 0);
    let air = vec![
        note(HRM_SERVICE, HRM_MEASUREMENT, vec![0xA1]),
        note(BATTERY_SERVICE, HRM_MEASUREMENT, vec![0xB2]),
        note(HRM_SERVICE, HRM_MEASUREMENT, vec![0xA3]),
        note(BATTERY_SERVICE, HRM_MEASUREMENT, vec![0xB4]),
    ];
    let mut ingress = ingress();
    let hrm = spawn_notification_forwarder(
        &handle,
        futures_util::stream::iter(air.clone()),
        NotificationRoute::new(uuid(HRM_SERVICE), uuid(HRM_MEASUREMENT)),
        target_for(&scope_hrm, 7),
        ingress.sender.clone(),
        Arc::clone(&ingress.queued),
        Arc::clone(&ingress.dropped),
    );
    let battery = spawn_notification_forwarder(
        &handle,
        futures_util::stream::iter(air.clone()),
        NotificationRoute::new(uuid(BATTERY_SERVICE), uuid(HRM_MEASUREMENT)),
        target_for(&scope_battery, 9),
        ingress.sender.clone(),
        Arc::clone(&ingress.queued),
        Arc::clone(&ingress.dropped),
    );
    for join in [hrm, battery] {
        tokio::time::timeout(Duration::from_secs(10), join)
            .await
            .expect("finite script drains")
            .expect("forwarder never panics");
    }
    let mut events = Vec::new();
    for _ in 0..4 {
        events.push(recv_notification(&mut ingress.receiver, &ingress.queued).await);
    }
    assert!(
        ingress.receiver.try_recv().is_err(),
        "no cross-routed duplicates: exactly four events"
    );
    assert_eq!(ingress.dropped.load(Ordering::Relaxed), 0, "no drops");
    let mut hrm_values = Vec::new();
    let mut battery_values = Vec::new();
    for event in &events {
        match event {
            RadioEvent::Notification {
                service_uuid,
                characteristic_uuid,
                epoch,
                value,
                ..
            } => {
                assert_eq!(characteristic_uuid, HRM_MEASUREMENT);
                if service_uuid == HRM_SERVICE {
                    assert_eq!(*epoch, 7, "owner epoch stamps HRM bytes");
                    hrm_values.push(value.clone());
                } else if service_uuid == BATTERY_SERVICE {
                    assert_eq!(*epoch, 9, "owner epoch stamps battery bytes");
                    battery_values.push(value.clone());
                } else {
                    panic!("misrouted service label {service_uuid}");
                }
            }
            other => panic!("flood carries only notifications, saw {other:?}"),
        }
    }
    hrm_values.sort();
    battery_values.sort();
    assert_eq!(hrm_values, vec![vec![0xA1], vec![0xA3]], "HRM owner only");
    assert_eq!(
        battery_values,
        vec![vec![0xB2], vec![0xB4]],
        "battery owner only"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f09_duplicate_occurrence_enable_rejected_explicitly() {
    // One REAL forwarder task owns (peer, HRM service 0, char 0). The native
    // stream carries no occurrence identity, so enabling char occurrence 1
    // of the same scope must be rejected explicitly — never fanned out.
    let handle = tokio::runtime::Handle::current();
    let live_scope = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    // The push handle stays alive: dropping it would close the stream and
    // end the installed forwarder.
    let (_push, pending) = script_pair();
    let task = spawn_notification_forwarder(
        &handle,
        pending,
        NotificationRoute::new(uuid(HRM_SERVICE), uuid(HRM_MEASUREMENT)),
        target_for(&live_scope, 11),
        ingress().sender,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
    );
    let key = forwarder_key(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let mut forwarders = HashMap::new();
    forwarders.insert(
        key.clone(),
        ForwarderEntry {
            task,
            peer_id: live_scope.0.clone(),
            service_uuid: live_scope.1.clone(),
            service_occurrence: live_scope.2,
            characteristic_uuid: live_scope.3.clone(),
            characteristic_occurrence: live_scope.4,
        },
    );
    let debt = HashSet::new();
    let live = live_scopes(&forwarders, &debt);
    assert_eq!(
        live,
        vec![live_scope.clone()],
        "real table reports one live scope"
    );
    assert!(
        route_is_ambiguous(&live, PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 1),
        "second characteristic occurrence of a live scope is ambiguous"
    );
    assert!(
        route_is_ambiguous(&live, PEER, HRM_SERVICE, 1, HRM_MEASUREMENT, 0),
        "same characteristic under a duplicate service occurrence is ambiguous"
    );
    assert!(
        !route_is_ambiguous(&live, PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0),
        "re-enabling the exact same instance stays idempotent, not ambiguous"
    );
    // The rejection production raises is explicit and contract-attributed.
    let error = ambiguous_routing_error(HRM_SERVICE, HRM_MEASUREMENT);
    assert_eq!(
        error.code_str(),
        "gatt.subscribe-failed",
        "exact contract code"
    );
    assert_eq!(error.operation(), "gatt.subscribe", "exact operation path");
    assert!(
        error
            .detail()
            .is_some_and(|detail| detail.contains("ambiguous")),
        "rejection names the ambiguity, never a silent fan-out"
    );
    // Cleanup: the installed forwarder aborts like a successful teardown.
    let entry = forwarders.remove(&key).expect("installed forwarder");
    entry.task.abort();
    let outcome = tokio::time::timeout(Duration::from_secs(10), entry.task).await;
    assert!(
        outcome.is_err() || matches!(outcome, Ok(Err(_))),
        "task stops"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f13_subscribe_ok_then_notifications_err_rolls_back_or_parks_debt() {
    let ch = characteristic(HRM_SERVICE, HRM_MEASUREMENT);
    let scope = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);

    // Rollback succeeds: no debt, error carries the stream detail.
    let ok_rollback = StubTransport::new()
        .with_subscribe(Ok(()))
        .with_notifications(Err(StubTransport::script_error("scripted stream refused")))
        .with_unsubscribe(Ok(()));
    let error = subscribe_and_stream(&ok_rollback, &ch)
        .await
        .err()
        .expect("stream failure must surface");
    match &error {
        EnableStreamError::Stream {
            detail,
            rollback_ok,
        } => {
            assert!(*rollback_ok, "rollback succeeded");
            assert!(
                detail.contains("scripted stream refused"),
                "detail preserved"
            );
        }
        other => panic!("expected the stream split, saw {other:?}"),
    }
    let mut debt = HashSet::new();
    apply_enable_stream_failure(&mut debt, &scope, true);
    assert!(
        debt.is_empty(),
        "successful rollback leaves no cleanup debt"
    );
    assert_eq!(
        ok_rollback.calls(),
        (1, 1, 1),
        "subscribe, stream, rollback — once each"
    );

    // Rollback fails: the orphaned CCCD parks as debt with its exact scope.
    let failed_rollback = StubTransport::new()
        .with_subscribe(Ok(()))
        .with_notifications(Err(StubTransport::script_error("scripted stream refused")))
        .with_unsubscribe(Err(StubTransport::script_error(
            "scripted rollback refused",
        )));
    let error = subscribe_and_stream(&failed_rollback, &ch)
        .await
        .err()
        .expect("stream failure must surface");
    assert!(
        matches!(
            error,
            EnableStreamError::Stream {
                rollback_ok: false,
                ..
            }
        ),
        "failed rollback reported, saw {error:?}"
    );
    let mut debt = HashSet::new();
    apply_enable_stream_failure(&mut debt, &scope, false);
    assert_eq!(
        debt,
        HashSet::from([scope.clone()]),
        "failed rollback parks the orphaned enablement as debt"
    );
    // Debt counts as live: a sibling occurrence enablement stays ambiguous.
    let live = live_scopes(&HashMap::new(), &debt);
    assert!(
        route_is_ambiguous(&live, PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 1),
        "debt CCCD blocks the ambiguous sibling"
    );
    assert_eq!(
        failed_rollback.calls(),
        (1, 1, 1),
        "exact native call counts"
    );

    // Subscribe refused: no CCCD enabled, so no rollback is even attempted.
    let refused = StubTransport::new().with_subscribe(Err(StubTransport::script_error(
        "scripted subscribe refused",
    )));
    let error = subscribe_and_stream(&refused, &ch)
        .await
        .err()
        .expect("subscribe failure must surface");
    match &error {
        EnableStreamError::Subscribe(detail) => {
            assert!(
                detail.contains("scripted subscribe refused"),
                "detail preserved"
            );
        }
        other => panic!("expected the subscribe refusal, saw {other:?}"),
    }
    assert_eq!(
        refused.calls(),
        (1, 0, 0),
        "refused subscribe attempts no rollback"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f13_disable_failure_keeps_values_flowing_until_retry_succeeds() {
    // Install a REAL forwarder over a controllable scripted stream, then run
    // the REAL disable sequencing against a failing native leaf: the
    // forwarder must stay (values keep flowing) until a retry succeeds.
    let handle = tokio::runtime::Handle::current();
    let scope = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let key = forwarder_key(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let ch = characteristic(HRM_SERVICE, HRM_MEASUREMENT);
    let (scripted, stream) = script_pair();
    let mut ingress = ingress();
    let task = spawn_notification_forwarder(
        &handle,
        stream,
        NotificationRoute::new(uuid(HRM_SERVICE), uuid(HRM_MEASUREMENT)),
        target_for(&scope, 5),
        ingress.sender.clone(),
        Arc::clone(&ingress.queued),
        Arc::clone(&ingress.dropped),
    );
    let forwarders = StdMutex::new(HashMap::from([(
        key.clone(),
        ForwarderEntry {
            task,
            peer_id: scope.0.clone(),
            service_uuid: scope.1.clone(),
            service_occurrence: scope.2,
            characteristic_uuid: scope.3.clone(),
            characteristic_occurrence: scope.4,
        },
    )]));
    let debt = StdMutex::new(HashSet::new());
    let transport = StubTransport::new()
        .with_unsubscribe(Err(StubTransport::script_error(
            "scripted teardown refused",
        )))
        .with_unsubscribe(Ok(()));

    // First disable attempt fails: error propagates, forwarder retained, no
    // separate debt while the retained consumer owns cleanup.
    let error = unsubscribe_and_fold(&transport, &ch, &forwarders, &debt, &key, &scope)
        .await
        .expect_err("failed native disable must surface");
    assert!(
        error.contains("scripted teardown refused"),
        "detail preserved"
    );
    assert!(
        forwarders.lock().expect("table").contains_key(&key),
        "failed disable keeps the forwarder: the CCCD is still live"
    );
    assert!(
        debt.lock().expect("debt").is_empty(),
        "no debt beside the consumer"
    );
    assert_eq!(transport.calls(), (0, 0, 1), "one native disable attempted");

    // Values keep flowing through the REAL retained forwarder.
    scripted
        .send(note(HRM_SERVICE, HRM_MEASUREMENT, vec![0xC3]))
        .expect("consumer alive: the failed disable kept the forwarder");
    let event = recv_notification(&mut ingress.receiver, &ingress.queued).await;
    match &event {
        RadioEvent::Notification {
            peer_id,
            service_uuid,
            characteristic_uuid,
            epoch,
            value,
            ..
        } => {
            assert_eq!(peer_id, PEER);
            assert_eq!(service_uuid, HRM_SERVICE);
            assert_eq!(characteristic_uuid, HRM_MEASUREMENT);
            assert_eq!(*epoch, 5, "install-time epoch survives the failed teardown");
            assert_eq!(*value, vec![0xC3], "post-failure bytes still delivered");
        }
        other => panic!("expected the live value, saw {other:?}"),
    }

    // Retry succeeds: the forwarder is removed (task aborted) with no debt.
    unsubscribe_and_fold(&transport, &ch, &forwarders, &debt, &key, &scope)
        .await
        .expect("retry disables");
    assert!(
        !forwarders.lock().expect("table").contains_key(&key),
        "retry success removes the forwarder"
    );
    assert!(
        debt.lock().expect("debt").is_empty(),
        "retry success holds no debt"
    );
    assert_eq!(transport.calls(), (0, 0, 2), "exactly two native disables");

    // The removed consumer is deterministically gone: the abort the retry
    // issued ends the task, which drops its stream receiver. `closed()`
    // resolves exactly when that receiver is gone — after that no forwarder
    // code can ever run again, so a later CCCD emission fails closed with
    // nothing landing in ingress. No wall-clock absence window needed.
    tokio::time::timeout(Duration::from_secs(10), scripted.closed())
        .await
        .expect("torn-down consumer drops its receiver");
    assert!(
        scripted
            .send(note(HRM_SERVICE, HRM_MEASUREMENT, vec![0xC4]))
            .is_err(),
        "pushing into the torn-down consumer fails: its receiver is gone"
    );
    assert!(
        ingress.receiver.try_recv().is_err(),
        "no value flows after the successful teardown"
    );
}

#[test]
fn f07_byte_reservation_never_overshoots_under_concurrency() {
    // Four OS threads, barrier-released onto one reservation of exactly
    // half the production cap each. Under load-then-add the four near-
    // simultaneous loads all admit and the total lands at 3–4 halves; the
    // CAS reservation admits exactly two halves in every interleaving, so
    // every round must total exactly the cap. (Cooperative tokio tasks
    // without yield points serialize here and cannot open the race window —
    // real forwarders race across OS threads, hence `std::thread`.)
    use std::sync::Barrier;

    const ROUNDS: usize = 50;
    const HALF: u64 = NOTIFICATION_BYTES / 2;
    assert_eq!(NOTIFICATION_BYTES % 2, 0, "fixture halves the cap exactly");
    let mut bad_rounds = 0usize;
    for _ in 0..ROUNDS {
        let queued = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(Barrier::new(4));
        let admitted = std::thread::scope(|s| {
            let mut handles = Vec::new();
            for _ in 0..4 {
                let (counter, start) = (Arc::clone(&queued), Arc::clone(&gate));
                handles.push(s.spawn(move || {
                    start.wait();
                    u64::from(ingress_try_reserve(&counter, HALF))
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("racer never panics"))
                .sum::<u64>()
        });
        if admitted != 2 || queued.load(Ordering::Relaxed) != NOTIFICATION_BYTES {
            bad_rounds += 1;
        }
    }
    assert_eq!(
        bad_rounds, 0,
        "every round admits exactly the cap, never more"
    );
    // An oversized single item is refused up front, even against an empty
    // counter — single-item admissibility first.
    let queued = Arc::new(AtomicU64::new(0));
    assert!(
        !ingress_try_reserve(&queued, NOTIFICATION_BYTES + 1),
        "oversized single item refused"
    );
    // Every reservation pairs with one release: back to exactly zero.
    assert!(ingress_try_reserve(&queued, HALF));
    assert!(ingress_try_reserve(&queued, HALF));
    ingress_release(&queued, NOTIFICATION_BYTES);
    assert_eq!(queued.load(Ordering::Relaxed), 0, "bytes fully released");
}

#[tokio::test]
async fn fake_control_overload_is_counted_not_silent() {
    // Boundary minor: control drops past 64 are counted, never silent.
    let radio = FakeRadio::new();
    for index in 0..70 {
        radio.push_event(RadioEvent::Connected(format!("peer-{index}")));
    }
    assert_eq!(
        radio.dropped_control_count(),
        6,
        "control overload past 64 is counted exactly"
    );
    for index in 0..64 {
        assert_eq!(
            radio.next_event().await,
            Some(RadioEvent::Connected(format!("peer-{index}"))),
            "admitted control drains in push order"
        );
    }
    radio.close_events();
    assert_eq!(
        radio.next_event().await,
        None,
        "closed source ends the stream"
    );
    assert_eq!(
        radio.dropped_control_count(),
        6,
        "the count is stable after the drain"
    );
}
