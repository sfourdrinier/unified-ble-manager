use super::*;

#[test]
fn failed_sibling_does_not_release_graph_until_stalled_query_finishes() {
    use std::sync::{Arc, Mutex};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let gate = Arc::new(tokio::sync::RwLock::new(()));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let seen = Arc::new(Mutex::new(false));
        let seen_query = seen.clone();
        let owner = gate.clone();
        let task = tokio::spawn(async move {
            let _lease = owner.write().await;
            let queries: Vec<
                std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>,
            > = vec![
                Box::pin(async { Err(Error::NotConnected) }),
                Box::pin(async move {
                    *seen_query.lock().unwrap() = true;
                    rx.await.unwrap();
                    Ok(())
                }),
            ];
            settle_discovery_queries(queries).await
        });
        tokio::task::yield_now().await;
        assert!(*seen.lock().unwrap());
        assert!(!task.is_finished());
        assert!(gate.try_read().is_err());
        assert!(super::super::marker_executor::try_graph_exclusive(&gate).is_err());
        tx.send(()).unwrap();
        assert!(task.await.unwrap().is_err());
        assert!(gate.try_write().is_ok());
    });
}

#[test]
fn discovery_retirement_only_accepts_completed_or_error() {
    use super::super::ble::device::discovery_status_is_pending;
    use windows_future::AsyncStatus;
    assert!(discovery_status_is_pending(AsyncStatus::Started));
    assert!(discovery_status_is_pending(AsyncStatus::Canceled));
    assert!(!discovery_status_is_pending(AsyncStatus::Completed));
    assert!(!discovery_status_is_pending(AsyncStatus::Error));
}

#[test]
fn failed_candidates_and_old_services_remain_owned_through_retry() {
    use super::super::ble::device::ServiceOwners;
    let mut owners = ServiceOwners::default();
    owners.begin_candidate();
    owners.retain_candidate(1);
    owners.publish_candidate();
    owners.begin_candidate();
    owners.retain_candidate(2); // Later query or handoff fails.
    owners.begin_candidate();
    owners.retain_candidate(3);
    owners.publish_candidate();
    let mut attempts = Vec::new();
    assert!(
        owners
            .close_retired(|service| {
                attempts.push(*service);
                if *service == 1 {
                    Err("close refused")
                } else {
                    Ok(())
                }
            })
            .is_err()
    );
    assert!(attempts.contains(&1));
    let mut retried = Vec::new();
    owners
        .close_retired(|service| {
            retried.push(*service);
            Ok::<_, &str>(())
        })
        .unwrap();
    assert!(retried.contains(&1));
    assert!(!retried.contains(&3));
    owners.close_all(|_| Ok::<_, &str>(())).unwrap();
    assert!(owners.is_empty());
}

#[test]
fn service_alias_is_not_closed_while_owned_by_published_graph() {
    use super::super::ble::device::ServiceOwners;
    let mut owners = ServiceOwners::default();
    owners.begin_candidate();
    owners.retain_candidate(7);
    owners.publish_candidate();
    owners.begin_candidate();
    owners.retain_candidate(7);
    owners.publish_candidate();
    owners
        .close_retired(|_| -> Result<()> { panic!("published service closed") })
        .unwrap();
    let mut closed = Vec::new();
    owners
        .close_all(|value| {
            closed.push(*value);
            Ok::<_, &str>(())
        })
        .unwrap();
    assert_eq!(closed, [7]);
}

#[test]
fn invalidated_cccd_recovery_requires_disconnect_instead_of_stale_write() {
    assert!(
        cccd_recovery_error(false)
            .to_string()
            .contains("disconnect")
    );
    assert!(
        cccd_recovery_error(true)
            .to_string()
            .contains("unsubscribe")
    );
    assert_eq!(
        cccd_recovery_action(false, true),
        CccdRecoveryAction::RemoveRegistration
    );
    assert_eq!(
        cccd_recovery_action(false, false),
        CccdRecoveryAction::Disconnect
    );
    assert_eq!(
        cccd_recovery_action(true, false),
        CccdRecoveryAction::Unsubscribe
    );
}

fn characteristic() -> Characteristic {
    Characteristic {
        uuid: Uuid::nil(),
        instance: 2,
        service_uuid: Uuid::nil(),
        service_instance: 1,
        properties: api::CharPropFlags::empty(),
        descriptors: Default::default(),
    }
}

#[test]
fn uncertain_retry_cannot_be_mistaken_for_local_only_cleanup() {
    let peer = Peripheral::new_with_id(
        Weak::new(),
        PeripheralId::from(BDAddr::from([1, 2, 3, 4, 5, 6])),
        Arc::new(Mutex::new(Default::default())),
    );
    let key = ordinary_cccd_key(&characteristic());
    peer.shared
        .ordinary_cccd_uncertain
        .lock()
        .unwrap()
        .insert(key);
    peer.shared
        .ordinary_cccd_local_cleanup
        .lock()
        .unwrap()
        .insert(key);
    peer.note_remote_cccd_uncertainty(key);
    assert!(
        peer.shared
            .ordinary_cccd_uncertain
            .lock()
            .unwrap()
            .contains(&key)
    );
    assert!(
        !peer
            .shared
            .ordinary_cccd_local_cleanup
            .lock()
            .unwrap()
            .contains(&key)
    );
}

#[tokio::test]
async fn service_change_exposes_disconnect_recovery_at_public_entrypoints() {
    let peer = Peripheral::new_with_id(
        Weak::new(),
        PeripheralId::from(BDAddr::from([1, 2, 3, 4, 5, 6])),
        Arc::new(Mutex::new(Default::default())),
    );
    let route = characteristic();
    peer.shared.graph.lock().unwrap().ready = true;
    peer.shared
        .ordinary_cccd_uncertain
        .lock()
        .unwrap()
        .insert(ordinary_cccd_key(&route));
    peer.shared.graph.lock().unwrap().invalidate();
    assert!(
        peer.discover_services()
            .await
            .unwrap_err()
            .to_string()
            .contains("disconnect")
    );
    assert!(
        peer.unsubscribe(&route)
            .await
            .unwrap_err()
            .to_string()
            .contains("disconnect")
    );
    assert!(
        peer.shared
            .ordinary_cccd_uncertain
            .lock()
            .unwrap()
            .contains(&ordinary_cccd_key(&route))
    );
    peer.shared
        .ordinary_cccd_local_cleanup
        .lock()
        .unwrap()
        .insert(ordinary_cccd_key(&route));
    assert!(
        peer.unsubscribe(&route)
            .await
            .unwrap_err()
            .to_string()
            .contains("discovery")
    );
}

#[tokio::test]
async fn stalled_discovery_fences_public_operations_and_reclaim() {
    use crate::ubm_reclaim_authority::{GraphKey, GraphTarget, PermitStatus};
    let authorities = Arc::new(Mutex::new(Default::default()));
    let peer = Peripheral::new_with_id(
        Weak::new(),
        PeripheralId::from(BDAddr::from([1, 2, 3, 4, 5, 6])),
        authorities.clone(),
    );
    let identity = {
        let mut graph = peer.shared.graph.lock().unwrap();
        graph.ready = true;
        let id = graph.identity().unwrap().unwrap();
        graph.reclaim_binding.attach();
        assert!(graph.reclaim_binding.publish(id.clone()));
        id
    };
    peer.shared.source_connection.lock().unwrap().connected = true;
    let binding = peer
        .capture_reclaim_graph_binding("peer", "connection", &identity)
        .unwrap();
    let target = GraphTarget {
        peer_id: "peer".into(),
        connection_generation: "connection".into(),
        graph: identity.clone(),
    };
    {
        let mut owner = authorities.lock().unwrap();
        owner.reserve_open(1).unwrap();
        owner.resolve_open(1, true).unwrap();
        owner
            .reserve_permit_token_bound(1, 1, target.clone(), &binding)
            .unwrap();
        owner.resolve_permit_token(1, 1, &target, true).unwrap();
    }
    let key = GraphKey {
        authority_id: 1,
        peer_id: "peer".into(),
        connection_generation: "connection".into(),
        graph: identity,
        permit_id: 1,
    };
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let operation_peer = peer.clone();
    let lease_peer = peer.clone();
    let discovery = tokio::spawn(async move {
        operation_peer
            .own_reclaim_operation(false, async move {
                let _lease = super::super::marker_executor::try_graph_exclusive(
                    &lease_peer.shared.notification_lifecycle,
                )
                .unwrap();
                let queries: Vec<
                    std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>,
                > = vec![
                    Box::pin(async { Err(Error::NotConnected) }),
                    Box::pin(async move {
                        let _ = started_tx.send(());
                        let _ = finish_rx.await;
                        Ok(())
                    }),
                ];
                settle_discovery_queries(queries).await.map(|_| ())
            })
            .await
    });
    started_rx.await.unwrap();
    assert_eq!(
        peer.acquire_reclaim(&binding, key.clone()),
        Ok(PermitStatus::Busy)
    );
    assert!(
        peer.discover_services()
            .await
            .unwrap_err()
            .to_string()
            .contains("graph operation is pending")
    );
    assert!(
        peer.disconnect()
            .await
            .unwrap_err()
            .to_string()
            .contains("graph operation is pending")
    );
    let read_peer = peer.clone();
    let read = tokio::spawn(async move { read_peer.read(&characteristic()).await });
    tokio::task::yield_now().await;
    assert!(!read.is_finished());
    assert!(!discovery.is_finished());
    finish_tx.send(()).unwrap();
    assert!(discovery.await.unwrap().is_err());
    assert!(read.await.unwrap().is_err());
    assert_eq!(
        peer.acquire_reclaim(&binding, key.clone()),
        Ok(PermitStatus::Acquired)
    );
    peer.release_reclaim(&key).unwrap();
    assert!(matches!(
        peer.discover_services().await,
        Err(Error::NotConnected)
    ));
    assert!(peer.disconnect().await.is_ok());
}

#[tokio::test]
async fn failed_retirement_keeps_graph_unpublished_until_cleanup_retry() {
    use super::super::ble::device::ServiceOwners;
    use crate::ubm_reclaim_authority::{GraphKey, GraphTarget, PermitStatus};
    let authorities = Arc::new(Mutex::new(Default::default()));
    let peer = Peripheral::new_with_id(
        Weak::new(),
        PeripheralId::from(BDAddr::from([1, 2, 3, 4, 5, 6])),
        authorities.clone(),
    );
    peer.shared.connected.store(true, Ordering::Relaxed);
    peer.shared.source_connection.lock().unwrap().connected = true;
    let old = {
        let mut graph = peer.shared.graph.lock().unwrap();
        graph.ready = true;
        let old = graph.identity().unwrap().unwrap();
        graph.publish_discovery(old.1, old.2);
        old
    };
    let binding = peer
        .capture_reclaim_graph_binding("peer", "connection", &old)
        .unwrap();
    let target = GraphTarget {
        peer_id: "peer".into(),
        connection_generation: "connection".into(),
        graph: old.clone(),
    };
    {
        let mut authority = authorities.lock().unwrap();
        authority.reserve_open(1).unwrap();
        authority.resolve_open(1, true).unwrap();
        authority
            .reserve_permit_token_bound(1, 1, target.clone(), &binding)
            .unwrap();
        authority.resolve_permit_token(1, 1, &target, true).unwrap();
    }
    let key = GraphKey {
        authority_id: 1,
        peer_id: "peer".into(),
        connection_generation: "connection".into(),
        graph: old.clone(),
        permit_id: 1,
    };
    let mut owners = ServiceOwners::default();
    owners.retain_candidate(1);
    owners.publish_candidate();
    owners.retain_candidate(2);
    peer.begin_graph_replacement(old.2, Some(old.clone()))
        .unwrap();
    let service_key = (Uuid::nil(), 123);
    let failed = install_and_retire_services(
        &mut owners,
        || {
            peer.shared.ble_services.insert(
                service_key,
                BLEService {
                    uuid: service_key.0,
                    instance: service_key.1,
                    characteristics: Default::default(),
                    access: gatt_model::ServiceRestriction::Open,
                    included_services: None,
                },
            );
            Ok(())
        },
        |owner| owner.publish_candidate(),
        || Ok(()),
        |owner| owner.close_retired(|_| Err(Error::Other("close failed".into()))),
    );
    assert!(failed.unwrap_err().to_string().contains("close failed"));
    assert!(peer.gatt_graph_snapshot().is_err());
    assert!(peer.services().is_empty());
    assert!(
        peer.read(&characteristic())
            .await
            .unwrap_err()
            .to_string()
            .contains("not ready")
    );
    assert_eq!(peer.acquire_reclaim(&binding, key), Ok(PermitStatus::Stale));
    let mut closed = Vec::new();
    owners
        .prepare_candidate(
            || Ok::<_, Error>(()),
            |value| {
                closed.push(*value);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(closed, [1]);
    owners.retain_candidate(3);
    install_and_retire_services(
        &mut owners,
        || Ok(()),
        |owner| owner.publish_candidate(),
        || Ok(()),
        |owner| {
            owner.close_retired(|value| {
                closed.push(*value);
                Ok(())
            })
        },
    )
    .unwrap();
    peer.finish_graph_replacement(old.2).unwrap();
    assert_eq!(closed, [1, 2]);
    let snapshot = peer.gatt_graph_snapshot().unwrap();
    assert_eq!(snapshot.2.2, old.2 + 1);
    assert_eq!(snapshot.0.len(), 1);
}
