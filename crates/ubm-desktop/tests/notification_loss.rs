//! Notification and adapter-event loss on the OS broadcast (finding 78,
//! vendored btleplug patch 10). The lag is forced through btleplug's real
//! bounded broadcast and its real notification stream, then through the
//! real production forwarder; the central accounts it on the subscription
//! by its overflow policy instead of dropping it.

use std::time::Duration;

use ubm_desktop::{
    CharacteristicSnapshot, DesktopCentral, FakeRadio, NotificationPoll, OpControl, PeerSnapshot,
    PropertyFlags, RadioEvent, ServiceSnapshot,
};

const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";

/// CoreBluetooth and WinRT only: BlueZ has no notification broadcast.
#[cfg(not(target_os = "linux"))]
mod broadcast {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    use btleplug::api::ValueNotification;
    use tokio::sync::mpsc;
    use ubm_desktop::RadioEvent;
    use ubm_desktop::btleplug_backend::{
        ForwardTarget, NOTIFICATION_CAP, NotificationRoute, spawn_notification_forwarder,
    };

    use super::{HRM_MEASUREMENT, HRM_SERVICE};

    fn uuid(text: &str) -> uuid::Uuid {
        uuid::Uuid::parse_str(text).expect("uuid")
    }

    fn note(byte: u8) -> ValueNotification {
        ValueNotification {
            uuid: uuid(HRM_MEASUREMENT),
            instance: 0x12,
            service_uuid: uuid(HRM_SERVICE),
            service_instance: 0x10,
            value: vec![byte],
            lost_before: 0,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lagging_os_broadcast_reaches_the_subscription_as_loss() {
        // The platform peripheral's bounded broadcast (btleplug uses 16 slots;
        // two here), outrun before the subscription's receiver reads.
        let (air, receiver) = tokio::sync::broadcast::channel(2);
        for byte in 1..=5 {
            air.send(note(byte)).expect("receiver alive");
        }
        drop(air);
        let stream = btleplug::ubm::notifications_stream_from_broadcast_receiver(receiver);
        let (sender, mut ingress) = mpsc::channel(NOTIFICATION_CAP);
        let forwarder = spawn_notification_forwarder(
            &tokio::runtime::Handle::current(),
            stream,
            NotificationRoute::new(uuid(HRM_SERVICE), 0x10, uuid(HRM_MEASUREMENT), 0x12),
            ForwardTarget {
                peer_id: "peer-1".to_owned(),
                service_uuid: HRM_SERVICE.to_owned(),
                service_occurrence: 0,
                characteristic_uuid: HRM_MEASUREMENT.to_owned(),
                characteristic_occurrence: 0,
                epoch: 4,
            },
            sender,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );
        tokio::time::timeout(Duration::from_secs(5), forwarder)
            .await
            .expect("finite broadcast drains")
            .expect("forwarder never panics");
        let mut events = Vec::new();
        while let Ok(event) = ingress.try_recv() {
            events.push(event);
        }
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(
            matches!(
                &events[0],
                RadioEvent::NotificationsLost { lost: 3, epoch: 4, peer_id, .. } if peer_id == "peer-1"
            ),
            "the three overwritten values are reported before the next one: {events:?}"
        );
        assert!(matches!(&events[1], RadioEvent::Notification { value, .. } if value == &vec![4]));
        assert!(matches!(&events[2], RadioEvent::Notification { value, .. } if value == &vec![5]));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_central_ends_an_error_policy_stream_with_the_counted_loss() {
    let central = DesktopCentral::open(FakeRadio::new(), "loss-host")
        .await
        .expect("open");
    central.boundary().push_event(advertisement("peer-1"));
    for _ in 0..2000 {
        if central.peer_key_for("peer-1").await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    central.boundary().set_services(
        "peer-1",
        vec![ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![CharacteristicSnapshot {
                uuid: HRM_MEASUREMENT.to_owned(),
                occurrence: 0,
                properties: PropertyFlags {
                    read: false,
                    write: false,
                    write_without_response: false,
                    notify: true,
                    indicate: false,
                },
                descriptors: Vec::new(),
            }],
        }],
    );
    central
        .connect("peer-1", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .discover("peer-1", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("discover");
    central
        .subscribe(
            "peer-1",
            &selector(),
            "consumer-a",
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("subscribe");
    let (scope, epoch) = central
        .boundary()
        .enable_epochs()
        .pop()
        .expect("enabled once");
    let stale = RadioEvent::NotificationsLost {
        peer_id: scope.0.clone(),
        service_uuid: scope.1.clone(),
        service_occurrence: scope.2,
        characteristic_uuid: scope.3.clone(),
        characteristic_occurrence: scope.4,
        epoch: epoch + 1,
        lost: 9,
    };
    central.boundary().push_event(stale);
    central
        .boundary()
        .push_event(RadioEvent::NotificationsLost {
            peer_id: scope.0,
            service_uuid: scope.1,
            service_occurrence: scope.2,
            characteristic_uuid: scope.3,
            characteristic_occurrence: scope.4,
            epoch,
            lost: 3,
        });
    let mut polled = NotificationPoll::Empty;
    for _ in 0..2000 {
        polled = central
            .poll_notification("peer-1", &selector(), "consumer-a")
            .await
            .expect("poll");
        if polled != NotificationPoll::Empty {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    match polled {
        NotificationPoll::Terminal(terminal) => {
            assert_eq!(terminal.reason(), "overflow");
            assert_eq!(
                terminal.dropped_items(),
                3,
                "only the live epoch's loss counts, never the stale one"
            );
        }
        other => panic!("the lossy upstream ends the stream, saw {other:?}"),
    }
    assert_eq!(
        central
            .notification_loss("peer-1", &selector(), "consumer-a")
            .await
            .expect("loss"),
        Some(3)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_adapter_events_are_counted() {
    let central = DesktopCentral::open(FakeRadio::new(), "loss-host")
        .await
        .expect("open");
    central
        .boundary()
        .push_event(RadioEvent::EventsLost { skipped: 4 });
    for _ in 0..2000 {
        if central.resource_counters().await.radio_events_lost == 4 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("lost adapter events were not counted");
}

/// Finding 107 (audit): the central's per-subscription buffer is never a
/// loss point below the caller's own stream. Legacy CoreBluetooth delivered
/// every value straight into the public stream (unbounded thread-safe
/// function queue), so the only bound was the caller's policy, up to the
/// public maximum. 300 values that arrive before the host polls are all
/// retained, in order, with no overflow terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_before_the_host_polls_is_retained_up_to_the_public_maximum() {
    let central = DesktopCentral::open(FakeRadio::new(), "burst-host")
        .await
        .expect("open");
    central.boundary().push_event(advertisement("peer-1"));
    for _ in 0..2000 {
        if central.peer_key_for("peer-1").await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    central.boundary().set_services(
        "peer-1",
        vec![ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![CharacteristicSnapshot {
                uuid: HRM_MEASUREMENT.to_owned(),
                occurrence: 0,
                properties: PropertyFlags {
                    read: false,
                    write: false,
                    write_without_response: false,
                    notify: true,
                    indicate: false,
                },
                descriptors: Vec::new(),
            }],
        }],
    );
    central
        .connect("peer-1", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .discover("peer-1", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("discover");
    central
        .subscribe(
            "peer-1",
            &selector(),
            "consumer-a",
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("subscribe");
    let (scope, epoch) = central
        .boundary()
        .enable_epochs()
        .pop()
        .expect("enabled once");
    // 300 values of 244 bytes (73 KB): past the old 64-item / 8 KiB buffer.
    for index in 0..300u32 {
        central.boundary().push_event(RadioEvent::Notification {
            peer_id: scope.0.clone(),
            service_uuid: scope.1.clone(),
            service_occurrence: scope.2,
            characteristic_uuid: scope.3.clone(),
            characteristic_occurrence: scope.4,
            epoch,
            value: [index.to_le_bytes().to_vec(), vec![0; 240]].concat(),
        });
        // Paced below the scripted boundary's own 256-deep data queue.
        if index % 128 == 127 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut values = Vec::new();
    loop {
        match central
            .poll_notification("peer-1", &selector(), "consumer-a")
            .await
            .expect("poll")
        {
            NotificationPoll::Value(value) => values.push(value),
            NotificationPoll::Empty => break,
            other => panic!("the stream stays live: {other:?} after {}", values.len()),
        }
    }
    assert_eq!(central.boundary().dropped_notification_count(), 0);
    assert_eq!(values.len(), 300);
    for (index, value) in values.iter().enumerate() {
        assert_eq!(value[..4], (index as u32).to_le_bytes(), "arrival order");
    }
}

/// Finding 111: values the central already holds when a subscription is
/// invalidated (link loss, service change) reach the host, in order,
/// before the invalidation, as the legacy native callbacks delivered them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn values_held_at_invalidation_drain_before_it() {
    for (event, cause) in [
        (
            RadioEvent::Disconnected("peer-1".to_owned()),
            ubm_desktop::InvalidationCause::LinkEnded,
        ),
        (
            RadioEvent::ServicesChanged("peer-1".to_owned()),
            ubm_desktop::InvalidationCause::ServicesChanged,
        ),
    ] {
        let central = DesktopCentral::open(FakeRadio::new(), "drain-host")
            .await
            .expect("open");
        central.boundary().push_event(advertisement("peer-1"));
        for _ in 0..2000 {
            if central.peer_key_for("peer-1").await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        central.boundary().set_services(
            "peer-1",
            vec![ServiceSnapshot {
                uuid: HRM_SERVICE.to_owned(),
                occurrence: 0,
                characteristics: vec![CharacteristicSnapshot {
                    uuid: HRM_MEASUREMENT.to_owned(),
                    occurrence: 0,
                    properties: PropertyFlags {
                        read: false,
                        write: false,
                        write_without_response: false,
                        notify: true,
                        indicate: false,
                    },
                    descriptors: Vec::new(),
                }],
            }],
        );
        central
            .connect("peer-1", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("connect");
        central
            .discover("peer-1", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("discover");
        central
            .subscribe(
                "peer-1",
                &selector(),
                "consumer-a",
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("subscribe");
        let (scope, epoch) = central.boundary().enable_epochs().pop().expect("enabled");
        for byte in 1..=3u8 {
            central.boundary().push_event(RadioEvent::Notification {
                peer_id: scope.0.clone(),
                service_uuid: scope.1.clone(),
                service_occurrence: scope.2,
                characteristic_uuid: scope.3.clone(),
                characteristic_occurrence: scope.4,
                epoch,
                value: vec![byte],
            });
        }
        central.boundary().push_event(event);
        let mut seen = Vec::new();
        for _ in 0..2000 {
            match central
                .poll_notification("peer-1", &selector(), "consumer-a")
                .await
                .expect("poll")
            {
                NotificationPoll::Value(value) => seen.push(value[0]),
                NotificationPoll::Invalidated(observed) => {
                    assert_eq!(observed, cause);
                    break;
                }
                NotificationPoll::Empty => tokio::time::sleep(Duration::from_millis(1)).await,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(seen, vec![1, 2, 3], "every held value, in order, first");
    }
}

/// Finding 131: a subscriber's own overflow policy decides what a loss does.
/// Under `drop-oldest` the loss is counted and the stream stays live (it
/// used to end, because the central always subscribed with `error`); the
/// consumer's counters are readable on every poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lossy_subscriber_counts_a_loss_and_stays_live() {
    let central = DesktopCentral::open(FakeRadio::new(), "lossy-host")
        .await
        .expect("open");
    central.boundary().push_event(advertisement("peer-1"));
    for _ in 0..2000 {
        if central.peer_key_for("peer-1").await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    central.boundary().set_services(
        "peer-1",
        vec![ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![CharacteristicSnapshot {
                uuid: HRM_MEASUREMENT.to_owned(),
                occurrence: 0,
                properties: PropertyFlags {
                    read: false,
                    write: false,
                    write_without_response: false,
                    notify: true,
                    indicate: false,
                },
                descriptors: Vec::new(),
            }],
        }],
    );
    central
        .connect("peer-1", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .discover("peer-1", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("discover");
    central
        .subscribe_with_policy(
            "peer-1",
            &selector(),
            "consumer-a",
            None,
            ubm_core::streams::OverflowPolicy::DropOldest,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("subscribe");
    let (scope, epoch) = central.boundary().enable_epochs().pop().expect("enabled");
    central
        .boundary()
        .push_event(RadioEvent::NotificationsLost {
            peer_id: scope.0.clone(),
            service_uuid: scope.1.clone(),
            service_occurrence: scope.2,
            characteristic_uuid: scope.3.clone(),
            characteristic_occurrence: scope.4,
            epoch,
            lost: 3,
        });
    central.boundary().push_event(RadioEvent::Notification {
        peer_id: scope.0,
        service_uuid: scope.1,
        service_occurrence: scope.2,
        characteristic_uuid: scope.3,
        characteristic_occurrence: scope.4,
        epoch,
        value: vec![9],
    });
    let mut value = None;
    for _ in 0..2000 {
        match central
            .poll_notification("peer-1", &selector(), "consumer-a")
            .await
            .expect("poll")
        {
            NotificationPoll::Value(bytes) => {
                value = Some(bytes);
                break;
            }
            NotificationPoll::Empty => tokio::time::sleep(Duration::from_millis(1)).await,
            other => panic!("the stream stays live: {other:?}"),
        }
    }
    assert_eq!(value, Some(vec![9]), "later values still arrive");
    let counters = central
        .consumer_counters("peer-1", &selector(), "consumer-a")
        .await
        .expect("counters")
        .expect("consumer");
    assert_eq!(counters.upstream_lost(), 3, "the loss is counted");
    assert!(!counters.terminated());
}
