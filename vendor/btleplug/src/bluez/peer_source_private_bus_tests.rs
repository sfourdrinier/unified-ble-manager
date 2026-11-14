// vendor/btleplug/src/bluez/peer_source_private_bus_tests.rs
use super::super::adapter::Adapter;
use super::super::peripheral::{le_gatt_tests::Fixture, PeripheralId};
use super::*;
use crate::api::{Central, Peripheral as _};
use crate::ubm_peer_notification_ingress::{IngressRecord, Poll, TerminalReason};

#[test]
#[ignore = "private session bus only; no radio"]
fn reclaim_callback_overflow_emits_source_uncertainty() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let _fixture = Fixture::new();
        let (_, session) = BluetoothSession::new_session_bus().await.unwrap();
        let device = session.get_devices().await.unwrap().remove(0);
        let registry = Arc::new(PeerRegistry::default());
        let mut events = registry.lifecycle_events();
        let peer = registry.get(&device.id).unwrap();
        let key = GraphKey { authority_id: 1, peer_id: device.id.to_string(), connection_generation: "connection".into(), graph: ("daemon".into(), 1, 1), permit_id: 1 };
        {
            let mut state = peer.state.lock().unwrap();
            state.watching = true;
            state.connected = true;
            let gate = state.reclaim_gate.get_or_insert_with(|| GraphGate::new(1, 100));
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_graph = Some(key.graph.clone());
        }
        let characteristic = crate::api::Characteristic {
            uuid: uuid::Uuid::from_u128(2), instance: 2,
            service_uuid: uuid::Uuid::from_u128(1), service_instance: 1,
            properties: crate::api::CharPropFlags::NOTIFY, descriptors: Default::default(),
        };
        peer.activation_failed(0, 0, 1, 1, &characteristic, crate::PlatformError::new("fixture", "first", "first"));
        peer.activation_failed(0, 0, 1, 1, &characteristic, crate::PlatformError::new("fixture", "second", "second"));
        peer.release_reclaim_gate(&key).unwrap();
        let PeerLifecycleEvent::Uncertain(actual, graph) = events.try_recv().unwrap() else { panic!("expected source uncertainty"); };
        assert_eq!(actual, device.id);
        assert_eq!(graph, Some(key.graph));
        let state = peer.state.lock().unwrap();
        assert!(state.watching && state.connected);
        assert!(state.callback_uncertain);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn discovery_replacement_retires_only_exact_changed_graph() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let _fixture = Fixture::new();
        let (_, session) = BluetoothSession::new_session_bus().await.unwrap();
        let device = session.get_devices().await.unwrap().remove(0);
        let registry = Arc::new(PeerRegistry::default());
        let mut events = registry.lifecycle_events();
        let peer = registry.get(&device.id).unwrap();
        let old = bluez_async::LeGattReadyToken { daemon_owner: ":1.42".into(), attachment: 1, revision: 1 };
        {
            let mut state = peer.state.lock().unwrap();
            state.watching = true; state.connected = true; state.resolved = true;
            state.graph_revision = Some(0); state.reclaim_binding.attach();
        }
        peer.services.lock().unwrap().accepted_token = Some(old.clone());
        peer.commit_graph(0, PublishedGatt { services: Vec::new(), accepted_token: Some(old.clone()) }).unwrap();
        assert!(events.try_recv().is_err());
        let new = bluez_async::LeGattReadyToken { revision: 2, ..old.clone() };
        peer.commit_graph(1, PublishedGatt { services: Vec::new(), accepted_token: Some(new.clone()) }).unwrap();
        let newest = bluez_async::LeGattReadyToken { revision: 3, ..old.clone() };
        peer.commit_graph(2, PublishedGatt { services: Vec::new(), accepted_token: Some(newest) }).unwrap();
        let PeerLifecycleEvent::GraphRetired(actual, graph) = events.try_recv().unwrap() else { panic!("expected graph retirement"); };
        assert_eq!(actual, device.id);
        assert_eq!(graph, (old.daemon_owner, old.attachment, old.revision));
        let PeerLifecycleEvent::GraphRetired(_, graph) = events.try_recv().unwrap() else { panic!("expected second graph retirement"); };
        assert_eq!(graph, (new.daemon_owner, new.attachment, new.revision));
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn shared_fetched_peer_owners() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let _fixture = Fixture::new();
            let (_, session) = BluetoothSession::new_session_bus().await.unwrap();
            let device = session.get_devices().await.unwrap().remove(0);
            let registry = Arc::new(PeerRegistry::default());
            let adapter = Adapter::new(session, device.id.adapter(), registry);
            let id = PeripheralId(device.id);
            let first = adapter.peripheral(&id).await.unwrap();
            let second = adapter.peripheral(&id).await.unwrap();
            assert!(Arc::ptr_eq(&first.peer, &second.peer));
            first.discover_services().await.unwrap();
            assert!(!first.services().is_empty());
            assert_eq!(first.services(), second.services());
            first.start_notification_source_generation(1).await.unwrap();
            let (_, consumer) = second.open_notification_source_group(1, 8, 128).unwrap();
            drop(first);
            assert_eq!(consumer.poll(), Poll::Empty);
            drop(second);
            assert!(matches!(
                consumer.poll(),
                Poll::Record(IngressRecord::Terminal {
                    reason: TerminalReason::Closed {
                        tail_uncertain: true
                    },
                    ..
                })
            ));
        });
}
#[test]
fn unknown_connection_fences_group_and_allows_revision_checked_recovery() {
    let peer = PeerSource::default();
    {
        let mut state = peer.state.lock().unwrap();
        state.watch_id = 1;
        state.watching = true;
        state.connected = true;
        state.resolved = true;
        state.graph_revision = Some(0);
        state.generation = Some(1);
    }
    peer.faults.start_source_generation(1).unwrap();
    let (_, consumer) = peer.faults.open_source_group(1, 8, 100).unwrap();
    peer.invalidate_graph(1, false, true);
    assert!(matches!(
        consumer.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Closed {
                tail_uncertain: true
            },
            ..
        })
    ));
    assert!(peer.apply_bootstrap(1, 0, true, true).is_err());
    assert!(peer.apply_bootstrap(1, 1, true, true).is_ok());
    assert!(peer.commit_graph(1, PublishedGatt::default()).is_ok());
}
#[tokio::test]
async fn pending_open_observes_terminal_link() {
    let peer = Arc::new(PeerSource::default());
    {
        let mut state = peer.state.lock().unwrap();
        state.watch_id = 1;
        state.watching = true;
    }
    let target = peer.clone();
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = std::future::pending::<()>() => false,
            _ = target.wait_link_end(1) => true,
        }
    });
    peer.invalidate(1);
    assert!(task.await.unwrap());
}
#[test]
#[ignore = "private session bus only; no radio"]
fn private_sender_cleanup_does_not_stop_replacement() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let fixture = Fixture::new();
            let peripheral = fixture.peripheral().await;
            peripheral.discover_services().await.unwrap();
            let first = peripheral.characteristics().into_iter().next().unwrap();
            let old = peripheral
                .notification_activation_owner(&first)
                .await
                .unwrap();
            old.subscribe_selecting(None).await.unwrap();
            fixture.refresh_same_path();
            peripheral.discover_services().await.unwrap();
            let second = peripheral.characteristics().into_iter().next().unwrap();
            let replacement = peripheral
                .notification_activation_owner(&second)
                .await
                .unwrap();
            assert_ne!(old.identity(), replacement.identity());
            replacement.subscribe_selecting(None).await.unwrap();
            let (started, _) = fixture.notify_calls();
            assert_eq!(started.len(), 2);
            assert_eq!(started[0].0, started[1].0);
            assert_ne!(started[0].1, started[1].1);
            old.unsubscribe().await.unwrap();
            assert!(!fixture.sender_present(&started[0].1));
            assert!(fixture.retired_senders().contains(&started[0].1));
            assert!(fixture.sender_present(&started[1].1));
            assert!(fixture.notify_calls().1.is_empty());
            replacement.unsubscribe().await.unwrap();
        });
}
