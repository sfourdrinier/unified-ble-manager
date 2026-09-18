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
//! * instance routing: the real [`NotificationRoute::matches`] filter inside
//!   live real forwarders, for same-UUID characteristics under two services
//!   and same-UUID instances under one service (UBM_PATCHES.md #6);
//! * setup: the real [`subscribe_and_stream`] sequencing (the stream opens
//!   before the enable, finding 128), including a value sent the moment the
//!   enable lands;
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
    NotificationRoute, NotificationStream, NotificationTransport, forwarder_key, ingress_release,
    ingress_try_reserve, spawn_notification_forwarder, subscribe_and_stream, unsubscribe_and_fold,
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
        instance: 0,
        service_uuid: uuid(service),
        service_instance: 0,
        value,
        lost_before: 0,
    }
}

fn characteristic(service: &str, characteristic: &str) -> Characteristic {
    Characteristic {
        uuid: uuid(characteristic),
        instance: 0,
        service_uuid: uuid(service),
        service_instance: 0,
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
    // is 2048 bytes so the BYTE bound binds strictly before the item bound
    // — this test is sensitive to byte accounting, not just channel
    // capacity. 172 values more than the byte bound admits are sent.
    const VALUE_LEN: usize = 2048;
    const OVERLOAD: usize = 172;
    let byte_admit = NOTIFICATION_BYTES as usize / VALUE_LEN;
    assert_eq!(
        byte_admit * VALUE_LEN,
        NOTIFICATION_BYTES as usize,
        "fixture divides the byte cap exactly"
    );
    assert!(
        byte_admit < NOTIFICATION_CAP,
        "the byte bound binds before the item bound"
    );
    let notes_sent = byte_admit + OVERLOAD;
    let admitted = byte_admit;
    let dropped_expected = OVERLOAD as u64;

    let handle = tokio::runtime::Handle::current();
    let scope = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let notes: Vec<ValueNotification> = (0..notes_sent)
        .map(|i| {
            note(
                HRM_SERVICE,
                HRM_MEASUREMENT,
                vec![(i % 251) as u8; VALUE_LEN],
            )
        })
        .collect();
    let mut ingress = ingress();
    let route = NotificationRoute::new(uuid(HRM_SERVICE), 0, uuid(HRM_MEASUREMENT), 0);
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
    // Drain through the REAL release path: bytes return to exactly zero and
    // the queue empties — no leak, no negative accounting. Every refused
    // value is also reported as this subscription's loss (finding 131).
    let mut notifications = 0usize;
    let mut reported_loss = 0u64;
    while !ingress.receiver.is_empty() {
        match recv_notification(&mut ingress.receiver, &ingress.queued).await {
            RadioEvent::Notification { .. } => notifications += 1,
            RadioEvent::NotificationsLost { lost, epoch, .. } => {
                assert_eq!(epoch, 3, "on the subscription's own epoch");
                reported_loss += lost;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(notifications, admitted, "channel holds every admit");
    assert_eq!(
        reported_loss, dropped_expected,
        "every drop reported as loss"
    );
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
    // Values fill the item and byte bounds together.
    let value_len = NOTIFICATION_BYTES as usize / NOTIFICATION_CAP;
    let handle = tokio::runtime::Handle::current();
    let scope = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let notes: Vec<ValueNotification> = (0..NOTIFICATION_CAP)
        .map(|i| {
            note(
                HRM_SERVICE,
                HRM_MEASUREMENT,
                vec![(i % 251) as u8; value_len],
            )
        })
        .collect();
    let mut ingress = ingress();
    let route = NotificationRoute::new(uuid(HRM_SERVICE), 0, uuid(HRM_MEASUREMENT), 0);
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
        NotificationRoute::new(uuid(HRM_SERVICE), 0, uuid(HRM_MEASUREMENT), 0),
        target_for(&scope_hrm, 7),
        ingress.sender.clone(),
        Arc::clone(&ingress.queued),
        Arc::clone(&ingress.dropped),
    );
    let battery = spawn_notification_forwarder(
        &handle,
        futures_util::stream::iter(air.clone()),
        NotificationRoute::new(uuid(BATTERY_SERVICE), 0, uuid(HRM_MEASUREMENT), 0),
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
async fn f61_same_uuid_instances_of_one_scope_route_to_their_own_forwarders() {
    // Two REAL forwarders own two same-UUID characteristics of one service
    // (handles 0x12 and 0x15, occurrences 0 and 1). The air carries both
    // instances; every value reaches only the forwarder of the instance
    // that fired (UBM_PATCHES.md #6), so the second enablement is served,
    // not refused.
    let handle = tokio::runtime::Handle::current();
    let first = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let second = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 1);
    let at = |instance: u64, value: u8| ValueNotification {
        uuid: uuid(HRM_MEASUREMENT),
        instance,
        service_uuid: uuid(HRM_SERVICE),
        service_instance: 0x10,
        value: vec![value],
        lost_before: 0,
    };
    let air = vec![
        at(0x12, 0x01),
        at(0x15, 0x02),
        at(0x12, 0x03),
        at(0x15, 0x04),
    ];
    let mut ingress = ingress();
    let tasks = [(0x12u64, &first, 21u64), (0x15, &second, 22)].map(|(instance, scope, epoch)| {
        spawn_notification_forwarder(
            &handle,
            futures_util::stream::iter(air.clone()),
            NotificationRoute::new(uuid(HRM_SERVICE), 0x10, uuid(HRM_MEASUREMENT), instance),
            target_for(scope, epoch),
            ingress.sender.clone(),
            Arc::clone(&ingress.queued),
            Arc::clone(&ingress.dropped),
        )
    });
    for task in tasks {
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("finite script drains")
            .expect("forwarder never panics");
    }
    let mut by_occurrence: HashMap<u64, Vec<Vec<u8>>> = HashMap::new();
    for _ in 0..4 {
        match recv_notification(&mut ingress.receiver, &ingress.queued).await {
            RadioEvent::Notification {
                characteristic_occurrence,
                epoch,
                value,
                ..
            } => {
                assert_eq!(epoch, 21 + characteristic_occurrence, "owner epoch");
                by_occurrence
                    .entry(characteristic_occurrence)
                    .or_default()
                    .push(value);
            }
            other => panic!("only notifications, saw {other:?}"),
        }
    }
    assert!(
        ingress.receiver.try_recv().is_err(),
        "no fan-out: exactly four events"
    );
    for values in by_occurrence.values_mut() {
        values.sort();
    }
    assert_eq!(by_occurrence[&0], vec![vec![0x01], vec![0x03]], "0x12 only");
    assert_eq!(by_occurrence[&1], vec![vec![0x02], vec![0x04]], "0x15 only");
}

/// Finding 128: the value stream opens before the native enable, so a
/// stream that cannot open enables nothing (no rollback owed, no debt), and
/// a refused enable drops the opened stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f13_the_stream_opens_before_the_enable() {
    let ch = characteristic(HRM_SERVICE, HRM_MEASUREMENT);

    // The stream cannot open: nothing was enabled, nothing to roll back.
    let no_stream = StubTransport::new()
        .with_notifications(Err(StubTransport::script_error("scripted stream refused")));
    let error = subscribe_and_stream(&no_stream, &ch)
        .await
        .err()
        .expect("stream failure must surface");
    match &error {
        EnableStreamError::Stream(refusal) => {
            assert_eq!(refusal.code_str(), "gatt.subscribe-failed");
            assert!(
                refusal
                    .detail()
                    .is_some_and(|detail| detail.contains("scripted stream refused")),
                "detail preserved"
            );
        }
        other => panic!("expected the stream refusal, saw {other:?}"),
    }
    assert_eq!(no_stream.calls(), (0, 1, 0), "no enable, no rollback");

    // The enable is refused after the stream opened: the refusal surfaces.
    let (_values, stream) = script_pair();
    let refused = StubTransport::new()
        .with_notifications(Ok(Box::pin(stream)))
        .with_subscribe(Err(StubTransport::script_error(
            "scripted subscribe refused",
        )));
    let error = subscribe_and_stream(&refused, &ch)
        .await
        .err()
        .expect("subscribe failure must surface");
    match &error {
        EnableStreamError::Subscribe(refusal) => {
            assert_eq!(refusal.code_str(), "gatt.subscribe-failed");
            assert!(
                refusal
                    .detail()
                    .is_some_and(|detail| detail.contains("scripted subscribe refused")),
                "detail preserved"
            );
        }
        other => panic!("expected the subscribe refusal, saw {other:?}"),
    }
    assert_eq!(refused.calls(), (1, 1, 0), "stream first, then the enable");
}

/// Finding 128: a value the peer sends the moment the enable lands reaches
/// the subscription. The stream is the real btleplug broadcast stream
/// (vendored patch 10), which only sees values sent after it opens; the
/// scripted enable sends a value as it succeeds. BlueZ has no broadcast
/// (its values arrive over an unbounded D-Bus stream), so this is the
/// CoreBluetooth and WinRT path; `f13_the_stream_opens_before_the_enable`
/// pins the order on every OS.
#[cfg(not(target_os = "linux"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_value_sent_right_after_the_enable_is_not_lost() {
    use futures_util::StreamExt;
    struct EagerPeer {
        sender: tokio::sync::broadcast::Sender<ValueNotification>,
    }
    impl NotificationTransport for EagerPeer {
        async fn transport_subscribe(
            &self,
            _characteristic: &Characteristic,
        ) -> Result<(), btleplug::Error> {
            // The peer notifies as soon as its CCCD is written; a
            // broadcast with no receiver drops the value.
            let _ = self
                .sender
                .send(note(HRM_SERVICE, HRM_MEASUREMENT, vec![0x5a]));
            Ok(())
        }

        async fn transport_notifications(&self) -> Result<NotificationStream, btleplug::Error> {
            Ok(btleplug::ubm::notifications_stream_from_broadcast_receiver(
                self.sender.subscribe(),
            ))
        }

        async fn transport_unsubscribe(
            &self,
            _characteristic: &Characteristic,
        ) -> Result<(), btleplug::Error> {
            Ok(())
        }
    }
    let (sender, _keep) = tokio::sync::broadcast::channel(16);
    let peer = EagerPeer { sender };
    let mut stream = subscribe_and_stream(&peer, &characteristic(HRM_SERVICE, HRM_MEASUREMENT))
        .await
        .expect("enabled");
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the first value arrives")
        .expect("stream open");
    assert_eq!(first.value, vec![0x5a]);
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
        NotificationRoute::new(uuid(HRM_SERVICE), 0, uuid(HRM_MEASUREMENT), 0),
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
            epoch: 5,
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
    assert_eq!(error.code_str(), "gatt.subscribe-failed");
    assert!(
        error
            .detail()
            .is_some_and(|detail| detail.contains("scripted teardown refused")),
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

/// Finding 124: a refused native disable keeps the platform's answer (the
/// legacy WinRT and BlueZ unsubscribe failures carried it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_disable_keeps_the_platform_answer() {
    let scope = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let key = forwarder_key(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let ch = characteristic(HRM_SERVICE, HRM_MEASUREMENT);
    let forwarders = StdMutex::new(HashMap::new());
    let debt = StdMutex::new(HashSet::new());
    let transport = StubTransport::new().with_unsubscribe(Err(btleplug::Error::Platform(
        btleplug::PlatformError::new("winrt", "gatt-status", "CCCD write refused")
            .with("gattStatus", "access-denied"),
    )));
    let error = unsubscribe_and_fold(&transport, &ch, &forwarders, &debt, &key, &scope)
        .await
        .expect_err("refused");
    assert_eq!(error.code_str(), "gatt.subscribe-failed");
    assert_eq!(
        error.platform(),
        Some(
            &ubm_desktop::PlatformDetail::new("winrt", "gatt-status")
                .with_message("CCCD write refused")
                .with_metadata(
                    "gattStatus",
                    ubm_desktop::PlatformValue::Text("access-denied".into())
                )
        )
    );
}

/// Finding 129: at a disconnect the forwarder is drained, not aborted:
/// every value btleplug already buffered for it reaches the ingress, and
/// what does not fit is returned as loss for the subscription, never
/// dropped silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f129_a_drained_forwarder_delivers_its_buffered_values_or_their_loss() {
    let handle = tokio::runtime::Handle::current();
    let scope = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let (scripted, stream) = script_pair();
    // A two-slot ingress: the drain must deliver two and report one lost.
    let (sender, mut receiver) = mpsc::channel(2);
    let queued = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let task = spawn_notification_forwarder(
        &handle,
        stream,
        NotificationRoute::new(uuid(HRM_SERVICE), 0, uuid(HRM_MEASUREMENT), 0),
        target_for(&scope, 7),
        sender,
        Arc::clone(&queued),
        Arc::clone(&dropped),
    );
    // Buffered in the OS stream when the disconnect arrives.
    for byte in 1..=3u8 {
        scripted
            .send(note(HRM_SERVICE, HRM_MEASUREMENT, vec![byte]))
            .expect("stream open");
    }
    let lost = task.drain(Duration::from_secs(5)).await;
    let mut delivered = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let RadioEvent::Notification { value, epoch, .. } = event {
            assert_eq!(epoch, 7, "install-time epoch");
            delivered.push(value[0]);
        }
    }
    assert_eq!(
        delivered.len() as u64 + lost,
        3,
        "every buffered value is delivered or returned as loss: {delivered:?} + {lost}"
    );
    assert_eq!(delivered, vec![1, 2], "delivered in order");
    assert_eq!(lost, 1);
}

/// Finding 131: a value the full ingress refuses is attributed to its
/// subscription as upstream loss (a `NotificationsLost` on its scope and
/// epoch, reported as soon as the ingress has room), so the consumer's
/// overflow policy applies, not only the global counter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f131_an_ingress_drop_is_reported_on_its_subscription() {
    let handle = tokio::runtime::Handle::current();
    let scope = scope(PEER, HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
    let (scripted, stream) = script_pair();
    let (sender, mut receiver) = mpsc::channel(1);
    let queued = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let _task = spawn_notification_forwarder(
        &handle,
        stream,
        NotificationRoute::new(uuid(HRM_SERVICE), 0, uuid(HRM_MEASUREMENT), 0),
        target_for(&scope, 4),
        sender,
        Arc::clone(&queued),
        Arc::clone(&dropped),
    );
    scripted
        .send(note(HRM_SERVICE, HRM_MEASUREMENT, vec![1]))
        .expect("open");
    scripted
        .send(note(HRM_SERVICE, HRM_MEASUREMENT, vec![2]))
        .expect("open");
    // The one-slot ingress holds the first value; the second is refused.
    for _ in 0..500 {
        if dropped.load(Ordering::Relaxed) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(dropped.load(Ordering::Relaxed), 1, "counted globally");
    let first = recv_notification(&mut receiver, &queued).await;
    assert!(matches!(first, RadioEvent::Notification { ref value, .. } if value == &vec![1]));
    let loss = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("the loss is reported once the ingress has room")
        .expect("open");
    match loss {
        RadioEvent::NotificationsLost { epoch, lost, .. } => {
            assert_eq!((epoch, lost), (4, 1), "on its own subscription");
        }
        other => panic!("expected the loss report, saw {other:?}"),
    }
}
