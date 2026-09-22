//! Lifecycle events acceptance (PR210-11): idle link loss, requested
//! release and service changes are published with the generation they
//! applied to, stale events publish nothing, a slow receiver sees `Lagged`
//! rather than a silent gap, unobserved events are counted, and the
//! profile observer receives the same events plus advertisements and
//! values. Also covers the delivery-mode carriage to the radio.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;
use ubm_desktop::{
    CentralProfile, CentralSignal, CharacteristicSnapshot, ConnectionState, DatabaseState,
    DeliveryMode, DesktopCentral, FakeRadio, InvalidationCause, LifecycleEvent, LifecycleKind,
    NotificationPoll, ObservedDelivery, OpControl, PeerSnapshot, PropertyFlags, RadioEvent,
    ServiceSnapshot,
};

const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";

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

fn hrm_service() -> ServiceSnapshot {
    ServiceSnapshot {
        uuid: HRM_SERVICE.to_owned(),
        occurrence: 0,
        characteristics: vec![CharacteristicSnapshot {
            uuid: HRM_MEASUREMENT.to_owned(),
            occurrence: 0,
            properties: PropertyFlags {
                read: true,
                write: false,
                write_without_response: false,
                notify: true,
                indicate: true,
            },
            descriptors: Vec::new(),
        }],
    }
}

fn selector() -> ubm_desktop::PathSelector {
    DesktopCentral::<FakeRadio>::selector(
        HRM_SERVICE,
        Some(0),
        Some(HRM_MEASUREMENT),
        Some(0),
        None,
        None,
    )
    .expect("selector")
}

async fn wait_peer(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
    for _ in 0..2000 {
        if central.peer_key_for(peer_id).await.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("peer {peer_id} never resolved");
}

async fn ready_peer(central: &DesktopCentral<FakeRadio>, peer_id: &str) -> Option<String> {
    central.boundary().push_event(advertisement(peer_id));
    wait_peer(central, peer_id).await;
    central
        .boundary()
        .set_services(peer_id, vec![hrm_service()]);
    let handle = central
        .connect(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .discover(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("discover");
    handle.connection_generation
}

async fn next_event(
    events: &mut tokio::sync::broadcast::Receiver<LifecycleEvent>,
) -> LifecycleEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("lifecycle event arrives")
        .expect("event")
}

async fn record_state(
    central: &DesktopCentral<FakeRadio>,
    peer_id: &str,
) -> Option<ConnectionState> {
    central
        .peer_records()
        .await
        .into_iter()
        .find(|record| record.peer_id == peer_id)
        .and_then(|record| record.connection_state)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_link_loss_publishes_link_lost_and_ends_the_stream() {
    let central = DesktopCentral::open(FakeRadio::new(), "lifecycle-host")
        .await
        .expect("open");
    let generation = ready_peer(&central, "peer-1").await;
    central
        .subscribe(
            "peer-1",
            &selector(),
            "consumer",
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("subscribe");
    let mut events = central.lifecycle_events();
    // No operation follows: the OS alone reports the loss.
    central
        .boundary()
        .push_event(RadioEvent::Disconnected("peer-1".to_owned()));
    let event = next_event(&mut events).await;
    assert_eq!(event.peer_id, "peer-1");
    assert_eq!(event.kind, LifecycleKind::LinkLost);
    assert_eq!(
        event.connection_generation, generation,
        "generation read before the loss"
    );
    assert!(
        event.database_generation.is_some(),
        "the discovered database generation is read before the loss"
    );
    assert_eq!(
        record_state(&central, "peer-1").await,
        Some(ConnectionState::Lost)
    );
    let records = central.peer_records().await;
    assert_eq!(
        records[0].database_state,
        Some(DatabaseState::Invalid),
        "the database invalidates with the link"
    );
    assert_eq!(
        central
            .poll_notification("peer-1", &selector(), "consumer")
            .await
            .expect("poll"),
        NotificationPoll::Invalidated(InvalidationCause::LinkEnded),
        "the notification stream ends as connection-lost"
    );
    // A second report for the ended link is stale: nothing is published.
    central
        .boundary()
        .push_event(RadioEvent::Disconnected("peer-1".to_owned()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(events.try_recv().is_err(), "stale loss publishes nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requested_release_publishes_released_exactly_once() {
    let central = DesktopCentral::open(FakeRadio::new(), "lifecycle-host")
        .await
        .expect("open");
    let generation = ready_peer(&central, "peer-2").await;
    let mut events = central.lifecycle_events();
    central
        .disconnect("peer-2", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("disconnect");
    let event = next_event(&mut events).await;
    assert_eq!(event.kind, LifecycleKind::Released { requested: true });
    assert_eq!(event.connection_generation, generation);
    // The OS event that follows the release is for an ended link.
    central
        .boundary()
        .push_event(RadioEvent::Disconnected("peer-2".to_owned()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(events.try_recv().is_err(), "the release is published once");
    assert_eq!(
        record_state(&central, "peer-2").await,
        Some(ConnectionState::Disconnected)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_change_publishes_and_invalidates_with_its_cause() {
    let central = DesktopCentral::open(FakeRadio::new(), "lifecycle-host")
        .await
        .expect("open");
    let generation = ready_peer(&central, "peer-3").await;
    let before_database = central
        .peer_records()
        .await
        .into_iter()
        .find(|record| record.peer_id == "peer-3")
        .expect("record")
        .database_generation;
    assert!(
        before_database.is_some(),
        "peer records carry the database generation"
    );
    central
        .subscribe(
            "peer-3",
            &selector(),
            "consumer",
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("subscribe");
    let mut events = central.lifecycle_events();
    central
        .boundary()
        .push_event(RadioEvent::ServicesChanged("peer-3".to_owned()));
    let event = next_event(&mut events).await;
    assert_eq!(event.kind, LifecycleKind::ServicesChanged);
    assert_eq!(event.connection_generation, generation);
    assert_eq!(
        event.database_generation, before_database,
        "the service change names the database generation it invalidated"
    );
    let after = central.peer_records().await;
    let after = after
        .iter()
        .find(|record| record.peer_id == "peer-3")
        .expect("record");
    assert_ne!(
        after.database_generation, before_database,
        "that generation is gone"
    );
    assert_eq!(
        record_state(&central, "peer-3").await,
        Some(ConnectionState::Connected)
    );
    assert_eq!(
        central
            .poll_notification("peer-3", &selector(), "consumer")
            .await
            .expect("poll"),
        NotificationPoll::Invalidated(InvalidationCause::ServicesChanged)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_receiver_sees_lagged_never_a_silent_gap() {
    let central = DesktopCentral::open(FakeRadio::new(), "lifecycle-host")
        .await
        .expect("open");
    ready_peer(&central, "peer-4").await;
    let mut slow = central.lifecycle_events();
    let mut pacer = central.lifecycle_events();
    let total = ubm_desktop::LIFECYCLE_EVENT_CAPACITY + 10;
    for _ in 0..total {
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("peer-4".to_owned()));
        // Pace the scripted source (its control queue is bounded) on a
        // receiver that keeps up.
        next_event(&mut pacer).await;
    }
    match slow.recv().await {
        Err(RecvError::Lagged(missed)) => assert!(missed >= 10, "missed {missed}"),
        other => panic!("slow receiver must observe the lag, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_nobody_observes_are_counted() {
    let central = DesktopCentral::open(FakeRadio::new(), "lifecycle-host")
        .await
        .expect("open");
    ready_peer(&central, "peer-5").await;
    assert_eq!(central.lifecycle_unobserved_count(), 0);
    central
        .boundary()
        .push_event(RadioEvent::Disconnected("peer-5".to_owned()));
    for _ in 0..2000 {
        if central.lifecycle_unobserved_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(central.lifecycle_unobserved_count(), 1);
    assert_eq!(central.resource_counters().await.lifecycle_unobserved, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_observer_sees_advertisements_values_and_the_same_lifecycle_events() {
    let seen: Arc<Mutex<Vec<CentralSignal>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut profile = CentralProfile::desktop("observed-host");
    profile.identity = std::sync::Arc::new(ubm_desktop::DesktopIdentity::new(
        "scripted",
        "observed-host",
    ));
    profile.observer = Some(Arc::new(move |signal| {
        sink.lock().expect("sink").push(signal);
    }));
    let central = DesktopCentral::open_with(FakeRadio::new(), profile)
        .await
        .expect("open");
    assert_eq!(
        central.attachment().backend_instance_id().as_str(),
        "ubm-desktop-scripted-observed-host",
        "the backend label reaches the attachment identity"
    );
    let mut events = central.lifecycle_events();
    // Finding 121: sightings are observations while a scan runs.
    central
        .start_scan("scanner", &[], OpControl::budget_ms(5000))
        .await
        .expect("scan");
    ready_peer(&central, "peer-6").await;
    central
        .subscribe(
            "peer-6",
            &selector(),
            "consumer",
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("subscribe");
    let epoch = central.boundary().enable_epochs()[0].1;
    central.boundary().push_event(RadioEvent::Notification {
        peer_id: "peer-6".to_owned(),
        service_uuid: HRM_SERVICE.to_owned(),
        service_occurrence: 0,
        characteristic_uuid: HRM_MEASUREMENT.to_owned(),
        characteristic_occurrence: 0,
        epoch,
        value: vec![0xAB],
    });
    central
        .boundary()
        .push_event(RadioEvent::Disconnected("peer-6".to_owned()));
    let broadcast = next_event(&mut events).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let signals = seen.lock().expect("sink").clone();
    assert!(
        signals
            .iter()
            .any(|signal| matches!(signal, CentralSignal::Advertisement(snapshot) if snapshot.id == "peer-6")),
        "advertisement signalled"
    );
    assert!(
        signals.iter().any(|signal| matches!(
            signal,
            CentralSignal::Value { scope, value } if scope.0 == "peer-6" && value == &vec![0xAB]
        )),
        "admitted value signalled"
    );
    let lifecycle: Vec<&LifecycleEvent> = signals
        .iter()
        .filter_map(|signal| match signal {
            CentralSignal::Lifecycle(event) => Some(event),
            _ => None,
        })
        .collect();
    assert_eq!(
        lifecycle,
        vec![&broadcast],
        "one emission point, two consumers"
    );
    assert_eq!(central.lifecycle_unobserved_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_requirements_reach_the_radio_and_joins_are_checked() {
    let central = DesktopCentral::open(FakeRadio::new(), "delivery-host")
        .await
        .expect("open");
    ready_peer(&central, "peer-7").await;
    let observed = central
        .subscribe(
            "peer-7",
            &selector(),
            "enabler",
            Some(DeliveryMode::Indication),
            OpControl::budget_ms(5000),
        )
        .await
        .expect("a radio that writes the mode honours it");
    assert_eq!(observed, ObservedDelivery::Indication);
    assert_eq!(
        central.boundary().delivery_requests(),
        vec![Some(DeliveryMode::Indication)],
        "the requirement reached the radio"
    );
    let calls = central.boundary().calls().len();
    let refused = central
        .subscribe(
            "peer-7",
            &selector(),
            "wants-notification",
            Some(DeliveryMode::Notification),
            OpControl::budget_ms(5000),
        )
        .await
        .expect_err("a join cannot rewrite the live CCCD");
    assert_eq!(refused.code_str(), "capability.limited");
    assert_eq!(
        central.boundary().calls().len(),
        calls,
        "refused before any effect"
    );
    let joined = central
        .subscribe(
            "peer-7",
            &selector(),
            "wants-indication",
            Some(DeliveryMode::Indication),
            OpControl::budget_ms(5000),
        )
        .await
        .expect("a matching join shares the CCCD");
    assert_eq!(joined, ObservedDelivery::Indication);
    let plain = central
        .subscribe(
            "peer-7",
            &selector(),
            "any",
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("no requirement joins");
    assert_eq!(
        plain,
        ObservedDelivery::Indication,
        "joins report what was observed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unproven_delivery_requirement_is_undone_not_accepted() {
    let central = DesktopCentral::open(FakeRadio::new(), "delivery-host")
        .await
        .expect("open");
    ready_peer(&central, "peer-8").await;
    // The radio enables but reports a different mode than required.
    central
        .boundary()
        .script_observed_delivery(ObservedDelivery::Unknown);
    let error = central
        .subscribe(
            "peer-8",
            &selector(),
            "consumer",
            Some(DeliveryMode::Notification),
            OpControl::budget_ms(5000),
        )
        .await
        .expect_err("an unenforced requirement is refused");
    assert_eq!(error.code_str(), "capability.limited");
    assert_eq!(
        central.boundary().live_subscription_count(),
        0,
        "the enable was undone, not left live"
    );
    assert_eq!(central.resource_counters().await.routed_subscriptions, 0);
}

/// Finding 118: the observer (the napi event waker) is signalled for every
/// report the host pumps, as the legacy backends called back for each:
/// security changes, write readiness and a scan the OS ended. None of them
/// waits for a polling interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_observer_is_signalled_for_security_readiness_and_scan_end() {
    let seen: Arc<Mutex<Vec<CentralSignal>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut profile = CentralProfile::desktop("woken-host");
    profile.observer = Some(Arc::new(move |signal| {
        sink.lock().expect("sink").push(signal);
    }));
    let central = DesktopCentral::open_with(FakeRadio::new(), profile)
        .await
        .expect("open");
    ready_peer(&central, "peer-w").await;
    let state = ubm_desktop::SecurityState {
        bond: ubm_desktop::BondState::Bonded,
        pairing_possible: Some(true),
    };
    central.boundary().push_event(RadioEvent::SecurityChanged {
        peer_id: "peer-w".to_owned(),
        state,
    });
    central.boundary().push_event(RadioEvent::WriteReadiness {
        peer_id: "peer-w".to_owned(),
        ready: true,
    });
    let session = central
        .start_scan("scanner", &[], OpControl::budget_ms(1000))
        .await
        .expect("scan");
    central.boundary().push_event(RadioEvent::ScanTerminated {
        aborted: true,
        detail: "the OS ended the scan".to_owned(),
    });
    let signalled =
        |matches: &dyn Fn(&CentralSignal) -> bool| seen.lock().expect("sink").iter().any(matches);
    for _ in 0..2000 {
        if signalled(&|signal| matches!(signal, CentralSignal::ScanTerminal(_))) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(signalled(&|signal| matches!(
        signal,
        CentralSignal::Security(event) if event.peer_id == "peer-w" && event.state == state
    )));
    assert!(signalled(&|signal| matches!(
        signal,
        CentralSignal::WriteReadiness(event) if event.peer_id == "peer-w" && event.ready
    )));
    assert!(signalled(&|signal| matches!(
        signal,
        CentralSignal::ScanTerminal(event)
            if event.aborted && &event.operation_id == session.operation_id()
    )));
}
