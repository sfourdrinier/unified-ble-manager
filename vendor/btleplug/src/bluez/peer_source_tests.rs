// vendor/btleplug/src/bluez/peer_source_tests.rs
use super::*;
use crate::ubm_peer_notification_ingress::{IngressRecord, Poll, TerminalReason};

#[test]
fn lifecycle_delivery_allows_reentrant_publisher_and_preserves_order() {
    use futures::Stream;
    use std::pin::Pin;
    use std::task::{Context, Poll as TaskPoll, Wake, Waker};
    struct BlockingWake {
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl Wake for BlockingWake {
        fn wake(self: Arc<Self>) { self.wake_by_ref(); }
        fn wake_by_ref(self: &Arc<Self>) {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
    }
    let peer = Arc::new(PeerSource::default());
    let mut events = tokio_stream::wrappers::BroadcastStream::new(peer.lifecycle.subscribe());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let waker = Waker::from(Arc::new(BlockingWake { entered: entered_tx, release: Mutex::new(release_rx) }));
    let mut context = Context::from_waker(&waker);
    assert!(matches!(Pin::new(&mut events).poll_next(&mut context), TaskPoll::Pending));
    let sender = peer.clone();
    let first = std::thread::spawn(move || {
        let start = { let mut state = sender.state.lock().unwrap(); PeerSource::queue_lifecycle(&mut state, PeerLifecycleEvent::EventsLost(1)) };
        if start { sender.drain_lifecycle(); }
    });
    entered_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    // A second thread must admit while the outward wake is still blocked.
    let (second_tx, second_rx) = std::sync::mpsc::channel();
    let sender = peer.clone();
    let second = std::thread::spawn(move || {
        let start = { let mut state = sender.state.lock().unwrap(); PeerSource::queue_lifecycle(&mut state, PeerLifecycleEvent::EventsLost(2)) };
        second_tx.send(start).unwrap();
    });
    let admitted = second_rx.recv_timeout(std::time::Duration::from_secs(2));
    release_tx.send(()).unwrap();
    first.join().unwrap();
    second.join().unwrap();
    assert_eq!(admitted.unwrap(), false);
    let noop = futures::task::noop_waker();
    let mut context = Context::from_waker(&noop);
    assert!(matches!(Pin::new(&mut events).poll_next(&mut context), TaskPoll::Ready(Some(Ok(PeerLifecycleEvent::EventsLost(1))))));
    assert!(matches!(Pin::new(&mut events).poll_next(&mut context), TaskPoll::Ready(Some(Ok(PeerLifecycleEvent::EventsLost(2))))));
}

#[test]
fn cancelled_graph_waiter_does_not_release_source_operation() {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
    let peer = Arc::new(PeerSource::default());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let owner = peer.clone();
    let waiter = tokio::spawn(async move {
        owner.own_graph_operation(GraphOperationScope::Effect(None), async move {
            let _ = started_tx.send(());
            finish_rx.await.map_err(|_| crate::Error::Other("test operation interrupted".into()))?;
            Ok(())
        }).await
    });
    started_rx.await.unwrap();
    waiter.abort();
    assert_eq!(peer.state.lock().unwrap().graph_operations, 1);
    finish_tx.send(()).unwrap();
    for _ in 0..100 {
        if peer.state.lock().unwrap().graph_operations == 0 { break; }
        tokio::task::yield_now().await;
    }
    let state = peer.state.lock().unwrap();
    assert_eq!(state.graph_operations, 0);
    assert!(!state.uncertain_unknown_owner);
    });
}

#[test]
fn cancelled_observation_waiter_does_not_continue_after_its_caller() {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
        let peer = Arc::new(PeerSource::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let owner = peer.clone();
        let waiter = tokio::spawn(async move {
            owner
                .own_graph_operation(GraphOperationScope::Observation, async move {
                    let _ = started_tx.send(());
                    finish_rx
                        .await
                        .map_err(|_| crate::Error::Other("test observation interrupted".into()))?;
                    Ok(())
                })
                .await
        });
        started_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(peer.state.lock().unwrap().graph_operations, 0);
        assert!(finish_tx.send(()).is_err());
        let state = peer.state.lock().unwrap();
        assert!(!state.uncertain_unknown_owner);
        assert!(state.uncertain_owners.is_empty());
    });
}

#[test]
fn graph_operation_recovery_distinguishes_refusal_timeout_and_owner_retirement() {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
        let peer = Arc::new(PeerSource::default());
        peer.state.lock().unwrap().observe_daemon(":1.10".into());
        let refused = peer.own_graph_operation(GraphOperationScope::SingleReplyEffect(Some(":1.10".into())), async {
            Err::<(), _>(crate::Error::Platform(crate::PlatformError::new("bluez-dbus", "org.bluez.Error.NotAuthorized", "refused")))
        }).await;
        assert!(refused.is_err());
        assert!(peer.state.lock().unwrap().uncertain_owners.is_empty());
        let late_refusal = Err::<(), _>(crate::Error::Platform(crate::PlatformError::new("bluez-dbus", "org.bluez.Error.NotAuthorized", "property read refused")));
        assert_eq!(graph_operation_settlement(&GraphOperationScope::Effect(Some(":1.10".into())), &late_refusal), GraphOperationSettlement::Indeterminate);
        let observation = peer.own_graph_operation(GraphOperationScope::Observation, async {
            Err::<(), _>(crate::Error::TimedOut(std::time::Duration::from_secs(1)))
        }).await;
        assert!(observation.is_err());
        assert!(peer.state.lock().unwrap().uncertain_owners.is_empty());
        let uncertain = peer.own_graph_operation(GraphOperationScope::Effect(Some(":1.10".into())), async {
            Err::<(), _>(crate::Error::TimedOut(std::time::Duration::from_secs(1)))
        }).await;
        assert!(uncertain.is_err());
        {
            let mut state = peer.state.lock().unwrap();
            assert!(state.uncertain_owners.contains(":1.10"));
            state.observe_daemon(":1.10".into());
            assert!(state.uncertain_owners.contains(":1.10"));
            state.observe_daemon(":1.11".into());
            assert!(state.uncertain_owners.is_empty());
        }
        let unknown = peer.own_graph_operation(GraphOperationScope::Effect(None), async {
            Err::<(), _>(crate::Error::TimedOut(std::time::Duration::from_secs(1)))
        }).await;
        assert!(unknown.is_err());
        let mut state = peer.state.lock().unwrap();
        state.observe_daemon(":1.12".into());
        assert!(state.uncertain_unknown_owner);
    });
}

#[test]
fn indeterminate_lease_release_retains_source_uncertainty_despite_ok_reply() {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
        let peer = Arc::new(PeerSource::default());
        peer.state.lock().unwrap().observe_daemon(":1.10".into());
        let result = peer.own_classified_graph_operation(
            GraphOperationScope::SingleReplyEffect(Some(":1.10".into())),
            async { Ok(bluez_async::LeLeaseReleaseReceipt {
                token: 1,
                physical_generation: 1,
                scope: bluez_async::LeLeaseReleaseScope::LeaseReleasedIndeterminate,
                disconnect_reason: None,
            }) },
            lease_release_settlement,
        ).await;
        assert!(result.is_ok());
        assert!(peer.state.lock().unwrap().uncertain_owners.contains(":1.10"));
    });
}

#[test]
fn reclaim_gate_rejects_inflight_uncertain_and_stale_graphs() {
    let peer = PeerSource::default();
    let graph = ("daemon".to_owned(), 1, 2);
    let binding = {
        let mut state = peer.state.lock().unwrap();
        state.watching = true;
        state.connected = true;
        state.resolved = true;
        state.graph_revision = Some(state.revision);
        state.reclaim_binding.attach();
        state.reclaim_binding.publish(graph.clone());
        state.reclaim_binding.capture("peer".into(), "connection".into(), &graph).unwrap()
    };
    let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph, permit_id: 1 };
    peer.state.lock().unwrap().graph_operations = 1;
    assert_eq!(peer.acquire_reclaim_gate(&binding, key.clone()), Err(GateError::Busy));
    {
        let mut state = peer.state.lock().unwrap();
        state.graph_operations = 0;
        state.uncertain_unknown_owner = true;
    }
    assert_eq!(peer.acquire_reclaim_gate(&binding, key.clone()), Err(GateError::Busy));
    peer.state.lock().unwrap().uncertain_unknown_owner = false;
    assert_eq!(peer.acquire_reclaim_gate(&binding, key.clone()), Ok(()));
    assert_eq!(peer.commit_reclaim_gate(&key), Ok(()));
    assert_eq!(peer.release_reclaim_gate(&key), Ok(()));
    peer.state.lock().unwrap().reclaim_binding.disconnect();
    assert_eq!(peer.acquire_reclaim_gate(&binding, key), Err(GateError::Invalidated));
}

#[test]
fn overflow_retires_old_generation_and_bracketed_graph_recovers_only_callback_uncertainty() {
    let peer = PeerSource::default();
    let old_graph = (":1.10".to_owned(), 1, 1);
    {
        let mut state = peer.state.lock().unwrap();
        state.watch_id = 1; state.watching = true; state.connected = true; state.resolved = true;
        state.graph_revision = Some(state.revision); state.generation = Some(1);
        state.reclaim_binding.attach(); state.reclaim_binding.publish(old_graph.clone());
    }
    peer.faults.start_source_generation(1).unwrap();
    let (_, consumer) = peer.faults.open_source_group(1, 8, 128).unwrap();
    peer.fail_uncertain(1);
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { .. }, .. })));
    assert!(peer.state.lock().unwrap().callback_uncertain);
    let fresh = bluez_async::LeGattReadyToken { daemon_owner: ":1.10".into(), attachment: 1, revision: 2 };
    peer.commit_graph(1, PublishedGatt { services: Vec::new(), accepted_token: Some(fresh.clone()) }).unwrap();
    let graph = (fresh.daemon_owner, fresh.attachment, fresh.revision);
    let binding = {
        let state = peer.state.lock().unwrap();
        assert!(!state.callback_uncertain);
        assert!(state.watching && state.connected);
        state.reclaim_binding.capture("peer".into(), "connection".into(), &graph).unwrap()
    };
    let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph, permit_id: 1 };
    assert_eq!(peer.acquire_reclaim_gate(&binding, key.clone()), Ok(()));
    peer.release_reclaim_gate(&key).unwrap();
    peer.state.lock().unwrap().uncertain_unknown_owner = true;
    assert_eq!(peer.acquire_reclaim_gate(&binding, key), Err(GateError::Busy));
}

#[test]
fn reclaim_gate_defers_close_and_rejects_precommit_invalidation() {
    let peer = PeerSource::default();
    let graph = ("daemon".to_owned(), 1, 2);
    let binding = {
        let mut state = peer.state.lock().unwrap();
        state.watching = true;
        state.connected = true;
        state.resolved = true;
        state.graph_revision = Some(state.revision);
        state.reclaim_binding.attach();
        state.reclaim_binding.publish(graph.clone());
        state.reclaim_binding.capture("peer".into(), "connection".into(), &graph).unwrap()
    };
    let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph, permit_id: 1 };
    peer.acquire_reclaim_gate(&binding, key.clone()).unwrap();
    peer.close_current();
    assert!(peer.state.lock().unwrap().watching);
    assert_eq!(peer.commit_reclaim_gate(&key), Err(GateError::Invalidated));
    peer.release_reclaim_gate(&key).unwrap();
    assert!(!peer.state.lock().unwrap().watching);
}

#[test]
fn registry_preserves_indeterminate_effect_after_all_peer_handles_drop() {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
        let registry = PeerRegistry::default();
        let peer = registry.get_by_peer("hci0/dev_fixture").unwrap();
        peer.state.lock().unwrap().observe_daemon(":1.10".into());
        let result = peer.own_graph_operation(GraphOperationScope::Effect(Some(":1.10".into())), async {
            Err::<(), _>(crate::Error::TimedOut(std::time::Duration::from_secs(1)))
        }).await;
        assert!(result.is_err());
        drop(peer);
        let reacquired = registry.get_by_peer("hci0/dev_fixture").unwrap();
        let mut state = reacquired.state.lock().unwrap();
        state.observe_daemon(":1.10".into());
        assert!(state.uncertain_owners.contains(":1.10"));
        state.observe_daemon(":1.11".into());
        assert!(state.uncertain_owners.is_empty());
    });
}

#[test]
fn registry_releases_fenced_uncertainty_and_idle_peers() {
    let registry = PeerRegistry::default();
    let peer = registry.get_by_peer("hci0/dev_fixture").unwrap();
    let weak = Arc::downgrade(&peer);
    {
        let mut state = peer.state.lock().unwrap();
        state.observe_daemon(":1.10".into());
        state.graph_operations = 1;
    }
    drop(GraphOperationGuard { peer: peer.clone(), scope: GraphOperationScope::Effect(Some(":1.10".into())), settlement: GraphOperationSettlement::Indeterminate });
    drop(peer);
    let peer = registry.get_by_peer("hci0/dev_fixture").unwrap();
    peer.state.lock().unwrap().observe_daemon(":1.11".into());
    drop(peer);
    drop(registry.get_by_peer("hci0/dev_other").unwrap());
    assert!(weak.upgrade().is_none());
}

#[test]
fn peer_registry_fences_adapter_effects_against_the_same_peer_gate() {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
        let registry = PeerRegistry::default();
        let peer = registry.get_by_peer("hci0/dev_fixture").unwrap();
        let adapter_peer = registry.get_by_peer("hci0/dev_fixture").unwrap();
        assert!(Arc::ptr_eq(&peer, &adapter_peer));
        let graph = ("daemon".to_owned(), 1, 2);
        let binding = {
            let mut state = peer.state.lock().unwrap();
            state.watching = true; state.connected = true; state.resolved = true;
            state.graph_revision = Some(state.revision);
            state.reclaim_binding.attach(); state.reclaim_binding.publish(graph.clone());
            state.reclaim_binding.capture("peer".into(), "connection".into(), &graph).unwrap()
        };
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph, permit_id: 1 };
        peer.acquire_reclaim_gate(&binding, key.clone()).unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        let result = adapter_peer.own_graph_operation(GraphOperationScope::Effect(None), async move {
            let _ = sent.send(());
            Ok(())
        }).await;
        assert!(result.is_err());
        assert!(received.await.is_err());
        peer.release_reclaim_gate(&key).unwrap();
    });
}

#[test]
fn reclaim_receipt_and_gate_serialize_invalidation_and_release() {
    use crate::ubm_reclaim_authority::{GraphTarget, OpenStatus, PermitTokenStatus};
    let peer = PeerSource::default();
    let graph = ("daemon".to_owned(), 1, 2);
    let binding = {
        let mut state = peer.state.lock().unwrap();
        state.watching = true;
        state.connected = true;
        state.resolved = true;
        state.graph_revision = Some(state.revision);
        state.reclaim_binding.attach();
        state.reclaim_binding.publish(graph.clone());
        state.reclaim_binding.capture("peer".into(), "connection".into(), &graph).unwrap()
    };
    let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: graph.clone(), permit_id: 1 };
    let target = GraphTarget { peer_id: key.peer_id.clone(), connection_generation: key.connection_generation.clone(), graph };
    {
        let mut authorities = peer.authorities.lock().unwrap();
        authorities.reserve_open(1).unwrap();
        assert_eq!(authorities.resolve_open(1, true), Ok(OpenStatus::Opened(1)));
        authorities.reserve_permit_token_bound(1, 10, target.clone(), &binding).unwrap();
        assert_eq!(authorities.resolve_permit_token(1, 10, &target, true), Ok(PermitTokenStatus::Issued(1)));
    }
    assert_eq!(peer.acquire_reclaim(&binding, key.clone()), Ok(PermitStatus::Acquired));
    assert_eq!(peer.acquire_reclaim(&binding, key.clone()), Ok(PermitStatus::Acquired));
    let foreign = PeerSource::with_authorities(peer.authorities.clone());
    let foreign_binding = {
        let mut state = foreign.state.lock().unwrap();
        state.watching = true; state.connected = true; state.resolved = true;
        state.graph_revision = Some(state.revision);
        state.reclaim_binding.attach(); state.reclaim_binding.publish(key.graph.clone());
        state.reclaim_binding.capture(key.peer_id.clone(), key.connection_generation.clone(), &key.graph).unwrap()
    };
    assert_eq!(foreign.acquire_reclaim(&binding, key.clone()), Err(AuthorityError::Conflict));
    assert_eq!(foreign.acquire_reclaim(&foreign_binding, key.clone()), Err(AuthorityError::Conflict));
    assert_eq!(foreign.commit_reclaim(&key), Err(AuthorityError::Conflict));
    assert_eq!(foreign.release_reclaim(&key), Err(AuthorityError::Conflict));
    assert_eq!(foreign.forget_reclaim(&key), Err(AuthorityError::Conflict));
    peer.state.lock().unwrap().reclaim_binding.disconnect();
    assert_eq!(peer.acquire_reclaim(&binding, key.clone()), Ok(PermitStatus::Acquired));
    peer.close_current();
    assert_eq!(peer.commit_reclaim(&key), Ok(PermitStatus::Invalidated));
    assert_eq!(peer.release_reclaim(&key), Ok(PermitStatus::Released));
    assert_eq!(peer.release_reclaim(&key), Ok(PermitStatus::Released));
    assert!(!peer.state.lock().unwrap().watching);
    peer.forget_reclaim(&key).unwrap();
}

#[test]
fn graph_invalidation_fences_group_and_stale_bootstrap() {
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
    peer.invalidate_graph(1, false, false);
    assert!(peer.state.lock().unwrap().resolved);
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
    assert!(peer.commit_graph(0, PublishedGatt::default()).is_err());
    assert!(peer.commit_graph(1, PublishedGatt::default()).is_ok());
}

#[test]
fn disconnect_between_groups_invalidates_the_graph() {
    let peer = PeerSource::default();
    {
        let mut state = peer.state.lock().unwrap();
        state.watch_id = 1;
        state.watching = true;
        state.connected = true;
        state.resolved = true;
        state.graph_revision = Some(0);
    }
    peer.invalidate(1);
    let state = peer.state.lock().unwrap();
    assert!(!state.watching);
    assert!(!state.connected);
    assert!(state.graph_revision.is_none());
    assert_ne!(state.revision, 0);
}
#[test]
fn unresolved_bootstrap_and_resolution_change_fence_discovery() {
    let peer = PeerSource::default();
    {
        let mut state = peer.state.lock().unwrap();
        state.watch_id = 1;
        state.watching = true;
    }
    assert!(peer.apply_bootstrap(1, 0, true, false).is_err());
    peer.resolved(1);
    assert!(peer.apply_bootstrap(1, 0, true, true).is_err());
}

#[test]
fn stale_disconnect_completion_does_not_close_new_watch() {
    let peer = PeerSource::default();
    {
        let mut state = peer.state.lock().unwrap();
        state.watch_id = 2;
        state.watching = true;
        state.connected = true;
    }
    peer.close_watch(1);
    let state = peer.state.lock().unwrap();
    assert_eq!(state.watch_id, 2);
    assert!(state.watching && state.connected);
}

#[test]
fn sender_failure_terminates_only_matching_route() {
    use crate::api::{CharPropFlags, Characteristic};
    use crate::ubm_peer_notification_ingress::NotificationRoute;
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
    let characteristic = Characteristic {
        uuid: uuid::Uuid::from_u128(2),
        instance: 2,
        service_uuid: uuid::Uuid::from_u128(1),
        service_instance: 1,
        properties: CharPropFlags::NOTIFY,
        descriptors: Default::default(),
    };
    let route = NotificationRoute {
        service_uuid: characteristic.service_uuid,
        service_instance: characteristic.service_instance,
        characteristic_uuid: characteristic.uuid,
        characteristic_instance: characteristic.instance,
    };
    let other_route = NotificationRoute {
        characteristic_instance: 3,
        ..route
    };
    peer.faults.start_source_generation(1).unwrap();
    let (group, consumer) = peer.faults.open_source_group(1, 8, 100).unwrap();
    let (other, other_consumer) = peer.faults.open_source_group(1, 8, 100).unwrap();
    let (replacement, replacement_consumer) = peer.faults.open_source_group(1, 8, 100).unwrap();
    peer.faults.start_source_activation(1, route, 7).unwrap();
    peer.faults.start_source_activation(1, route, 8).unwrap();
    peer.faults
        .register_source_route_for_activation(&replacement, route, 3, 8)
        .unwrap();
    peer.faults
        .register_source_route_for_activation(&group, route, 1, 7)
        .unwrap();
    peer.faults
        .register_source_route(&other, other_route, 2)
        .unwrap();
    let graph = ("daemon".to_owned(), 1, 1);
    let binding = {
        let mut state = peer.state.lock().unwrap();
        state.reclaim_binding.attach();
        state.reclaim_binding.publish(graph.clone());
        state.reclaim_binding.capture("peer".into(), "connection".into(), &graph).unwrap()
    };
    let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph, permit_id: 1 };
    peer.acquire_reclaim_gate(&binding, key.clone()).unwrap();
    peer.commit_reclaim_gate(&key).unwrap();
    peer.activation_failed(
        1,
        0,
        1,
        7,
        &characteristic,
        crate::PlatformError::new("bluez-notification-owner", "sender-lost", "fixture"),
    );
    assert_eq!(consumer.poll(), Poll::Empty);
    peer.release_reclaim_gate(&key).unwrap();
    assert!(matches!(
        consumer.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::SourceFailure { .. },
            ..
        })
    ));
    assert_eq!(other_consumer.poll(), Poll::Empty);
    assert_eq!(replacement_consumer.poll(), Poll::Empty);
    peer.retire_activation(1, &characteristic, 7).unwrap();
    peer.activation_failed(
        1,
        0,
        1,
        8,
        &characteristic,
        crate::PlatformError::new("bluez-notification-owner", "new-sender-lost", "fixture"),
    );
    assert!(matches!(
        replacement_consumer.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::SourceFailure { .. },
            ..
        })
    ));
}
