// Synthetic radio tests: these exercise recorder ownership, not physical BLE.
async fn reclaim_authority_fixture() -> (
    std::sync::Arc<DesktopCentral<FakeRadio>>,
    std::sync::Arc<crate::ordered_recording::OrderedRecording<FakeRadio>>,
    String,
    std::path::PathBuf,
) {
    use crate::continuation_journal::{JournalQuota, JournalRegistry};
    use crate::ordered_recording::{
        OrderedRecording, OrderedRecordingDeclaration, OrderedRecordingMember,
    };
    let central = std::sync::Arc::new(open().await);
    let peer = "reclaim-regression";
    central.boundary().set_gatt_snapshot_identity(
        peer,
        crate::boundary::GattSnapshotIdentity {
            owner: "graph".into(),
            attachment: 1,
            revision: 1,
        },
    );
    ready_peer(&central, peer, vec![hrm_service()]).await;
    let key = central.peer_key_for(peer).await.unwrap();
    let (connection, database) = {
        let core = central.inner.core.lock().await;
        (
            core.connection_generation(&key).unwrap(),
            core.database_generation(&key).unwrap(),
        )
    };
    let path = std::env::temp_dir().join(format!(
        "ubm-reclaim-authority-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    let registry = std::sync::Arc::new(JournalRegistry::default());
    registry.configure_directory(&path).unwrap();
    let declaration = OrderedRecordingDeclaration {
        peer_id: peer.into(),
        connection_generation: connection,
        database_generation: database,
        members: vec![OrderedRecordingMember {
            member_id: 1,
            public_member_id: "heart".into(),
            selector: hrm_selector(0),
            delivery: None,
            preference: None,
        }],
        max_items: 8,
        max_bytes: 128,
        max_controls: 8,
    };
    let recorder = OrderedRecording::prepare(
        central.clone(),
        registry,
        peer.into(),
        JournalQuota {
            max_bytes: 1 << 20,
            max_records: 16,
        },
        declaration,
    )
    .await
    .unwrap();
    let capability = recorder.issue_recovery_capability().unwrap();
    recorder
        .start_receipt(OpControl::budget_ms(5000).with_connection_lease("lease-a".into()))
        .await
        .unwrap();
    (central, recorder, capability, path)
}

#[tokio::test]
async fn reclaim_commit_automatically_releases_lost_release_request() {
    use crate::reclaim_reservation::ReclaimReservationStatus as S;
    let (central, recorder, capability, path) = reclaim_authority_fixture().await;
    let reservation = recorder
        .reclaim_reservation_owner(&capability)
        .await
        .unwrap();
    reservation
        .reserve(1, &capability, "lease-a", "caller")
        .unwrap();
    reservation
        .start(1, &capability, "lease-a", "caller")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !matches!(
            reservation
                .status(1, &capability, "lease-a", "caller")
                .unwrap(),
            S::Ready
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    recorder.retire_start_receipt().await.unwrap();
    central.boundary().lose_reclaim_replies(false, true);
    reservation
        .commit(1, &capability, "lease-a", "caller", || Ok(()))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !matches!(
            reservation
                .status(1, &capability, "lease-a", "caller")
                .unwrap(),
            S::ReleaseFailed {
                mapping_committed: true,
                ..
            }
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("lost native release is retained as cleanup debt");
    tokio::time::timeout(Duration::from_secs(2), async {
        while !matches!(
            reservation
                .status(1, &capability, "lease-a", "caller")
                .unwrap(),
            S::Released {
                mapping_committed: true
            }
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native owns release even when the caller disappears");
    let _ = recorder.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !matches!(
            recorder.driver_status(),
            crate::ordered_recording::RecordingDriverStatus::Finished
        ) {
            recorder.retry_driver();
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retained receipt must not prevent recorder stop after physical release");
    assert!(matches!(
        reservation.status(1, &capability, "lease-a", "caller"),
        Ok(S::Released {
            mapping_committed: true
        })
    ));
    reservation
        .commit(1, &capability, "lease-a", "caller", || {
            panic!("terminal replay must not republish")
        })
        .unwrap();
    reservation
        .retire(1, &capability, "lease-a", "caller")
        .unwrap();
    reservation
        .acknowledge_retirement(1, &capability, "lease-a", "caller")
        .unwrap();
    reservation
        .forget_retirement(1, &capability, "lease-a", "caller")
        .unwrap();
    drop(reservation);
    drop(recorder);
    drop(central);
    let _ = std::fs::remove_dir_all(path);
}

#[tokio::test]
async fn reclaim_receipt_survives_terminal_authority_revocation() {
    use crate::reclaim_reservation::ReclaimReservationStatus as S;
    let (central, recorder, capability, path) = reclaim_authority_fixture().await;
    let reservation = recorder
        .reclaim_reservation_owner(&capability)
        .await
        .unwrap();
    reservation
        .reserve(1, &capability, "lease-a", "caller")
        .unwrap();
    reservation
        .cancel(1, &capability, "lease-a", "caller")
        .unwrap();
    reservation
        .retire(1, &capability, "lease-a", "caller")
        .unwrap();
    let owner = central
        .inner
        .ordered_batches
        .get("reclaim-regression", recorder.collection_id())
        .unwrap();
    {
        let owner = owner.lock().await;
        super::lock_std(owner.reclaim_reservation.as_ref().unwrap()).revoke();
    }
    assert!(matches!(
        reservation.status(1, &capability, "lease-a", "caller"),
        Ok(S::Retired {
            mapping_committed: false
        })
    ));
    assert!(
        reservation.next_request_id(&capability).is_err(),
        "terminal authority must not admit new effects"
    );
    drop(reservation);
    drop(recorder);
    drop(central);
    let _ = std::fs::remove_dir_all(path);
}

#[tokio::test]
async fn reclaim_retirement_does_not_release_a_later_marker_gate() {
    use crate::reclaim_reservation::ReclaimReservationStatus as S;
    use std::sync::atomic::Ordering;
    let (central, recorder, capability, path) = reclaim_authority_fixture().await;
    let reservation = recorder
        .reclaim_reservation_owner(&capability)
        .await
        .unwrap();
    reservation
        .reserve(1, &capability, "lease-a", "caller")
        .unwrap();
    reservation
        .start(1, &capability, "lease-a", "caller")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !matches!(
            reservation
                .status(1, &capability, "lease-a", "caller")
                .unwrap(),
            S::Ready
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    reservation
        .cancel(1, &capability, "lease-a", "caller")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !matches!(
            reservation
                .status(1, &capability, "lease-a", "caller")
                .unwrap(),
            S::Released {
                mapping_committed: false
            }
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let owner = central
        .inner
        .ordered_batches
        .get("reclaim-regression", recorder.collection_id())
        .unwrap();
    let gate = {
        let owner = owner.lock().await;
        std::sync::Arc::clone(&owner.marker_reservation.as_ref().unwrap().2)
    };
    gate.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .unwrap();
    reservation
        .retire(1, &capability, "lease-a", "caller")
        .unwrap();
    reservation
        .acknowledge_retirement(1, &capability, "lease-a", "caller")
        .unwrap();
    reservation
        .forget_retirement(1, &capability, "lease-a", "caller")
        .unwrap();
    assert_eq!(
        gate.load(Ordering::Acquire),
        1,
        "old receipt cannot release a newer admission"
    );
    gate.store(0, Ordering::Release);
    drop(reservation);
    drop(recorder);
    drop(central);
    let _ = std::fs::remove_dir_all(path);
}

#[tokio::test]
async fn cancelled_reserved_reclaim_releases_gate_without_receipt_retirement() {
    use std::sync::atomic::Ordering;
    let (central, recorder, capability, path) = reclaim_authority_fixture().await;
    let reservation = recorder
        .reclaim_reservation_owner(&capability)
        .await
        .unwrap();
    reservation
        .reserve(1, &capability, "lease-a", "caller")
        .unwrap();
    reservation
        .cancel(1, &capability, "lease-a", "caller")
        .unwrap();
    let owner = central
        .inner
        .ordered_batches
        .get("reclaim-regression", recorder.collection_id())
        .unwrap();
    let gate = {
        let owner = owner.lock().await;
        std::sync::Arc::clone(&owner.marker_reservation.as_ref().unwrap().2)
    };
    assert_eq!(
        gate.load(Ordering::Acquire),
        0,
        "no-effect fence must release admission without losing its receipt"
    );
    assert!(
        reservation
            .commit(1, &capability, "lease-a", "caller", || panic!(
                "fenced request published"
            ))
            .is_err()
    );
    drop(reservation);
    drop(recorder);
    drop(central);
    let _ = std::fs::remove_dir_all(path);
}

#[tokio::test]
async fn recorder_stop_fences_ready_reclaim_and_supervises_release() {
    use crate::reclaim_reservation::ReclaimReservationStatus as S;
    let (central, recorder, capability, path) = reclaim_authority_fixture().await;
    let reservation = recorder.reclaim_reservation_owner(&capability).await.unwrap();
    reservation.reserve(1, &capability, "lease-a", "stop-race").unwrap();
    reservation.start(1, &capability, "lease-a", "stop-race").unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !matches!(reservation.status(1, &capability, "lease-a", "stop-race").unwrap(), S::Ready) {
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), recorder.shutdown()).await.unwrap().unwrap();
    assert!(reservation.commit(1, &capability, "lease-a", "stop-race", || panic!("stop must fence publication")).is_err());
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(reservation.status(1, &capability, "lease-a", "stop-race").unwrap(), S::Released { mapping_committed: false })
                && matches!(recorder.driver_status(), crate::ordered_recording::RecordingDriverStatus::Finished) { break; }
            recorder.retry_driver();
            tokio::task::yield_now().await;
        }
    }).await.expect("stop must supervise release without the caller");
    drop(reservation); drop(recorder); drop(central);
    let _ = std::fs::remove_dir_all(path);
}

#[tokio::test]
async fn recorder_stop_keeps_pending_reclaim_until_native_acquire_settles() {
    use crate::reclaim_reservation::ReclaimReservationStatus as S;
    let (central, recorder, capability, path) = reclaim_authority_fixture().await;
    let reservation = recorder.reclaim_reservation_owner(&capability).await.unwrap();
    central.boundary().block_op(FaultOp::ReclaimAcquire);
    reservation.reserve(1, &capability, "lease-a", "pending-stop").unwrap();
    reservation.start(1, &capability, "lease-a", "pending-stop").unwrap();
    tokio::time::timeout(Duration::from_secs(3), central.boundary().wait_for_calls("reclaim_acquire", 1)).await.unwrap();
    recorder.request_driver_close();
    assert!(matches!(reservation.status(1, &capability, "lease-a", "pending-stop").unwrap(), S::Pending));
    assert!(reservation.commit(1, &capability, "lease-a", "pending-stop", || panic!("stopped pending reclaim published")).is_err());
    central.boundary().unblock_op(FaultOp::ReclaimAcquire);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(reservation.status(1, &capability, "lease-a", "pending-stop").unwrap(), S::Released { mapping_committed: false }) { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("late acquisition must be released by its native owner");
    let _ = recorder.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !matches!(recorder.driver_status(), crate::ordered_recording::RecordingDriverStatus::Finished) {
            recorder.retry_driver();
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    drop(reservation); drop(recorder); drop(central);
    let _ = std::fs::remove_dir_all(path);
}
