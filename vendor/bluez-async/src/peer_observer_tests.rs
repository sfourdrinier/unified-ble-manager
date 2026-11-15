// vendor/bluez-async/src/peer_observer_tests.rs
use super::*;
use crate::{BluetoothEvent, DeviceEvent};

#[test]
fn registration_dispatch_disconnect_and_stale_lease_are_serialized() {
    let registry = PeerObservers::default();
    let peer = DeviceId::new("/org/bluez/hci0/dev_01");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let first = registry
        .insert(
            peer.clone(),
            Box::new(move |event| {
                sink.lock().unwrap().push(event);
            }),
        )
        .unwrap();
    assert!(matches!(
        registry.insert(peer.clone(), Box::new(|_| {})),
        Err(PeerObserverError::AlreadyRegistered)
    ));
    let disconnected = BluetoothEvent::Device {
        id: peer.clone(),
        event: DeviceEvent::Connected { connected: false },
    };
    assert!(!registry.dispatch(&peer, first, vec![disconnected.clone()]));
    assert_eq!(
        *observed.lock().unwrap(),
        vec![
            PeerObserverEvent::Event(disconnected),
            PeerObserverEvent::Closed(PeerObserverClose::Disconnected)
        ]
    );
    let second = registry.insert(peer.clone(), Box::new(|_| {})).unwrap();
    registry.remove(&peer, first);
    assert!(registry.dispatch(&peer, second, vec![]));
}

#[test]
fn connection_loss_closes_each_peer_once_and_rejects_new_registration() {
    let registry = PeerObservers::default();
    let peer = DeviceId::new("/org/bluez/hci0/dev_01");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let id = registry
        .insert(
            peer.clone(),
            Box::new(move |event| sink.lock().unwrap().push(event)),
        )
        .unwrap();
    registry.connection_lost();
    registry.remove(&peer, id);
    registry.connection_lost();
    assert_eq!(
        *observed.lock().unwrap(),
        vec![PeerObserverEvent::Closed(PeerObserverClose::ConnectionLost)]
    );
    assert!(matches!(
        registry.insert(peer, Box::new(|_| {})),
        Err(PeerObserverError::ConnectionLost)
    ));
}
#[test]
fn callback_can_unregister_self_and_another_peer() {
    let registry = Arc::new(PeerObservers::default());
    let first_peer = DeviceId::new("/org/bluez/hci0/dev_01");
    let second_peer = DeviceId::new("/org/bluez/hci0/dev_02");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let second_sink = observed.clone();
    let second = registry
        .insert(
            second_peer.clone(),
            Box::new(move |event| {
                second_sink.lock().unwrap().push((2, event));
            }),
        )
        .unwrap();
    let first_id = Arc::new(Mutex::new(None));
    let own_id = first_id.clone();
    let registry_callback = registry.clone();
    let peer_callback = first_peer.clone();
    let first_sink = observed.clone();
    let first = registry
        .insert(
            first_peer.clone(),
            Box::new(move |event| {
                first_sink.lock().unwrap().push((1, event.clone()));
                if matches!(event, PeerObserverEvent::Event(_)) {
                    registry_callback.remove(&second_peer, second);
                    registry_callback.remove(&peer_callback, own_id.lock().unwrap().unwrap());
                }
            }),
        )
        .unwrap();
    *first_id.lock().unwrap() = Some(first);
    let event = BluetoothEvent::Device {
        id: first_peer.clone(),
        event: DeviceEvent::Connected { connected: true },
    };
    registry.dispatch(&first_peer, first, vec![event.clone()]);
    assert_eq!(
        *observed.lock().unwrap(),
        vec![
            (1, PeerObserverEvent::Event(event)),
            (
                2,
                PeerObserverEvent::Closed(PeerObserverClose::Unregistered)
            ),
            (
                1,
                PeerObserverEvent::Closed(PeerObserverClose::Unregistered)
            ),
        ]
    );
}
#[test]
fn queue_limits_reserve_overflow_terminal() {
    for limits in [
        PeerObserverLimits {
            max_records: 0,
            max_bytes: usize::MAX,
        },
        PeerObserverLimits {
            max_records: 4,
            max_bytes: 0,
        },
    ] {
        let registry = PeerObservers::default();
        let peer = DeviceId::new("/org/bluez/hci0/dev_01");
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let id = registry
            .insert_with_limits(
                peer.clone(),
                Box::new(move |event| sink.lock().unwrap().push(event)),
                limits,
                false,
            )
            .unwrap();
        let event = BluetoothEvent::Device {
            id: peer.clone(),
            event: DeviceEvent::Connected { connected: true },
        };
        assert!(!registry.dispatch(&peer, id, vec![event]));
        assert_eq!(
            *events.lock().unwrap(),
            vec![PeerObserverEvent::Closed(PeerObserverClose::Overflow)]
        );
        assert!(!registry.dispatch(&peer, id, vec![]));
    }
}
#[test]
fn one_message_cannot_cross_a_disconnect_and_re_registration() {
    let registry = Arc::new(PeerObservers::default());
    registry.owner_changed(Some(":1.42".into()));
    let peer = DeviceId::new("/org/bluez/hci0/dev_01");
    let new_events = Arc::new(Mutex::new(Vec::new()));
    let callback_registry = registry.clone();
    let callback_peer = peer.clone();
    let sink = new_events.clone();
    registry
        .insert(
            peer.clone(),
            Box::new(move |event| {
                if matches!(
                    event,
                    PeerObserverEvent::Closed(PeerObserverClose::Disconnected)
                ) {
                    let sink = sink.clone();
                    callback_registry
                        .insert(
                            callback_peer.clone(),
                            Box::new(move |event| sink.lock().unwrap().push(event)),
                        )
                        .unwrap();
                }
            }),
        )
        .unwrap();
    registry.dispatch_message(
        ":1.42",
        vec![
            BluetoothEvent::Device {
                id: peer.clone(),
                event: DeviceEvent::Connected { connected: false },
            },
            BluetoothEvent::Device {
                id: peer.clone(),
                event: DeviceEvent::Rssi { rssi: -50 },
            },
        ],
    );
    assert!(new_events.lock().unwrap().is_empty());
    registry.dispatch_message(
        ":1.42",
        vec![BluetoothEvent::Device {
            id: peer,
            event: DeviceEvent::Rssi { rssi: -40 },
        }],
    );
    assert_eq!(new_events.lock().unwrap().len(), 1);
}
#[test]
fn owner_lookup_race_and_sender_identity_are_fenced() {
    let registry = PeerObservers::default();
    let revision = registry.0.lock().unwrap().owner_revision;
    registry.owner_changed(Some(":1.2".into()));
    registry
        .apply_owner_lookup(revision, ":1.1".into())
        .unwrap();
    let peer = DeviceId::new("/org/bluez/hci0/dev_01");
    let output = Arc::new(Mutex::new(Vec::new()));
    let sink = output.clone();
    registry
        .insert(
            peer.clone(),
            Box::new(move |event| sink.lock().unwrap().push(event)),
        )
        .unwrap();
    let event = BluetoothEvent::Device {
        id: peer.clone(),
        event: DeviceEvent::Rssi { rssi: -40 },
    };
    registry.dispatch_message(":1.1", vec![event.clone()]);
    assert!(output.lock().unwrap().is_empty());
    registry.dispatch_message(":1.2", vec![event]);
    registry.owner_changed(Some(":1.3".into()));
    assert!(matches!(
        output.lock().unwrap().last(),
        Some(PeerObserverEvent::Closed(PeerObserverClose::OwnerChanged))
    ));
    assert!(registry.0.lock().unwrap().peers.is_empty());
}
#[tokio::test]
async fn dropping_final_owner_releases_registry_with_live_watch() {
    use crate::match_cleanup::MatchRegistry;
    let registry = PeerObservers::default();
    let observers = Arc::new(registry);
    let weak = Arc::downgrade(&observers);
    let add = Arc::new(
        |_: String| -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), crate::match_cleanup::MatchFailure>>
                    + Send,
            >,
        > { Box::pin(async { Ok(()) }) },
    );
    let remove = add.clone();
    let matches = MatchRegistry::new(add, remove);
    let lease = matches.acquire("type='signal'".into(), 0).await.unwrap();
    observers.0.lock().unwrap().owner_watch = Some(lease);
    // Connection filters and the resource task capture this weak reference.
    let filter_owner = weak.clone();
    drop(observers);
    assert!(filter_owner.upgrade().is_none());
}

#[test]
fn removed_gatt_object_targets_exact_peer_and_sender() {
    let registry = PeerObservers::default();
    registry.owner_changed(Some(":1.42".into()));
    let peer = DeviceId::new("/org/bluez/hci0/dev_01");
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    registry
        .insert(
            peer.clone(),
            Box::new(move |event| sink.lock().unwrap().push(event)),
        )
        .unwrap();
    let interfaces = vec!["org.bluez.GattCharacteristic1".to_string()];
    registry.removed_graph(
        ":1.99",
        "/org/bluez/hci0/dev_01/service0010/char0020",
        &interfaces,
    );
    registry.removed_graph(
        ":1.42",
        "/org/bluez/hci0/dev_02/service0010/char0020",
        &interfaces,
    );
    assert!(events.lock().unwrap().is_empty());
    registry.removed_graph(
        ":1.42",
        "/org/bluez/hci0/dev_01/service0010/char0020",
        &interfaces,
    );
    assert_eq!(
        *events.lock().unwrap(),
        vec![PeerObserverEvent::GattGraphChanged]
    );
}

#[test]
fn removed_device_closes_observer() {
    let registry = PeerObservers::default();
    registry.owner_changed(Some(":1.42".into()));
    let peer = DeviceId::new("/org/bluez/hci0/dev_01");
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    registry
        .insert(
            peer.clone(),
            Box::new(move |event| sink.lock().unwrap().push(event)),
        )
        .unwrap();
    registry.removed_graph(
        ":1.42",
        "/org/bluez/hci0/dev_01",
        &["org.bluez.Device1".into()],
    );
    assert_eq!(
        *events.lock().unwrap(),
        vec![PeerObserverEvent::Closed(PeerObserverClose::Disconnected)]
    );
    assert!(registry.0.lock().unwrap().peers.is_empty());
}

#[test]
fn added_gatt_object_fences_only_its_peer() {
    let registry = PeerObservers::default();
    registry.owner_changed(Some(":1.42".into()));
    let peer = DeviceId::new("/org/bluez/hci0/dev_01");
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    registry
        .insert(
            peer,
            Box::new(move |event| sink.lock().unwrap().push(event)),
        )
        .unwrap();
    registry.added_graph(
        ":1.42",
        "/org/bluez/hci0/dev_02/service0010",
        &["org.bluez.GattService1".into()],
    );
    registry.added_graph(
        ":1.42",
        "/org/bluez/hci0/dev_01",
        &["org.bluez.Device1".into()],
    );
    assert!(events.lock().unwrap().is_empty());
    registry.added_graph(
        ":1.42",
        "/org/bluez/hci0/dev_01/service0010",
        &["org.bluez.GattService1".into()],
    );
    assert_eq!(
        *events.lock().unwrap(),
        vec![PeerObserverEvent::GattGraphChanged]
    );
}

#[test]
fn observer_owner_snapshot_cannot_cross_daemon_replacement() {
    let registry = PeerObservers::default();
    let peer = DeviceId::new("/org/bluez/hci0/dev_01");
    registry.owner_changed(Some(":1.10".into()));
    let old = registry.insert(peer.clone(), Box::new(|_| {})).unwrap();
    assert_eq!(
        registry.registered_owner(&peer, old).as_deref(),
        Some(":1.10")
    );
    registry.owner_changed(Some(":1.11".into()));
    assert!(registry.registered_owner(&peer, old).is_none());
    let new = registry.insert(peer.clone(), Box::new(|_| {})).unwrap();
    assert!(registry.registered_owner(&peer, old).is_none());
    assert_eq!(
        registry.registered_owner(&peer, new).as_deref(),
        Some(":1.11")
    );
}

#[tokio::test]
#[ignore = "private session bus only; no radio"]
async fn private_bus_drains_peer_and_connection_watches_before_executor_shutdown() {
    assert_eq!(
        std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
        Ok("1")
    );
    let (resource, publisher) = dbus_tokio::connection::new_session_sync().unwrap();
    let publisher_task = tokio::spawn(resource);
    publisher
        .request_name("org.bluez", false, false, false)
        .await
        .unwrap();
    let (resource, session) = crate::BluetoothSession::new_session_bus().await.unwrap();
    let peer = DeviceId::new("/org/bluez/hci0/dev_01");
    let observer = session.observe_peer(&peer, |_| {}).await.unwrap();
    let matches = session.matches.clone();
    let scope = session.cleanup_scope;
    drop(observer);
    tokio::time::timeout(std::time::Duration::from_secs(2), matches.drain(scope))
        .await
        .unwrap()
        .unwrap();
    drop(session);
    tokio::time::timeout(std::time::Duration::from_secs(2), matches.drain(0))
        .await
        .unwrap()
        .unwrap();
    drop(matches);
    drop(resource);
    drop(publisher);
    publisher_task.abort();
}
