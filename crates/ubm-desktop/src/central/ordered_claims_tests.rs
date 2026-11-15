// crates/ubm-desktop/src/central/ordered_claims_tests.rs
use super::*;

#[tokio::test]
async fn batch_prepares_two_exact_claims_without_enabling_and_refuses_stale_generation() {
    use crate::boundary::{
        AdvertisementExtras, CharacteristicSnapshot, FakeRadio, PeerSnapshot, PropertyFlags,
        RadioEvent, ServiceSnapshot,
    };
    const SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
    const CHARACTERISTIC: &str = "00002a37-0000-1000-8000-00805f9b34fb";
    let central = DesktopCentral::open(FakeRadio::new(), "ordered-test")
        .await
        .unwrap();
    central
        .boundary()
        .push_event(RadioEvent::Advertisement(PeerSnapshot {
            id: "peer".into(),
            address: None,
            service_uuids: vec![SERVICE.into()],
            rssi: Some(-50),
            local_name: None,
            manufacturer_data: vec![],
            service_data: vec![],
            tx_power_level: None,
            extras: AdvertisementExtras::default(),
        }))
        .expect("synthetic event admission failed");
    central
        .connect("peer", "lease", OpControl::budget_ms(5000))
        .await
        .unwrap();
    let properties = PropertyFlags {
        read: true,
        write: false,
        write_without_response: false,
        notify: true,
        indicate: false,
    };
    central.boundary().set_services(
        "peer",
        vec![ServiceSnapshot {
            uuid: SERVICE.into(),
            occurrence: 0,
            primary: None,
            included_services: None,
            access: Default::default(),
            characteristics: vec![0, 1]
                .into_iter()
                .map(|occurrence| CharacteristicSnapshot {
                    uuid: CHARACTERISTIC.into(),
                    occurrence,
                    properties,
                    descriptors: vec![],
                })
                .collect(),
        }],
    );
    central
        .discover("peer", "lease", OpControl::unbounded())
        .await
        .unwrap();
    let peer_key = central.known_peer_key("peer").await.unwrap();
    let (connection, database) = {
        let core = central.inner.core.lock().await;
        (
            core.connection_generation(&peer_key).unwrap(),
            core.database_generation(&peer_key).unwrap(),
        )
    };
    let requests: Vec<OrderedClaimRequest> = (0..2)
        .map(|id| OrderedClaimRequest {
            member_id: id,
            selector: DesktopCentral::<FakeRadio>::selector(
                SERVICE,
                Some(0),
                Some(CHARACTERISTIC),
                Some(id),
                None,
                None,
            )
            .unwrap(),
            consumer: format!("ordered-{id}"),
            delivery: None,
            preference: None,
        })
        .collect();
    let before = central
        .boundary()
        .calls()
        .iter()
        .filter(|call| *call == "set_notifications")
        .count();
    let (first_id, _) = central
        .inner
        .ordered_batches
        .reserve("peer".into())
        .unwrap();
    central
        .prepare_ordered_claims(
            "peer",
            first_id,
            &connection,
            &database,
            &requests,
            OpControl::budget_ms(5000),
        )
        .await
        .unwrap();
    let owner = central.inner.ordered_batches.get("peer", first_id).unwrap();
    assert_eq!(owner.lock().await.claims.len(), 2);
    assert_eq!(
        central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "set_notifications")
            .count(),
        before
    );
    central
        .retry_retained_ordered_batch("peer", first_id, OpControl::budget_ms(5000))
        .await
        .unwrap();
    let (second_id, _) = central
        .inner
        .ordered_batches
        .reserve("peer".into())
        .unwrap();
    let error = central
        .prepare_ordered_claims(
            "peer",
            second_id,
            "stale",
            &database,
            &requests,
            OpControl::budget_ms(5000),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), BleErrorCode::GattStaleHandle);
    assert_eq!(
        central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "set_notifications")
            .count(),
        before
    );
    central
        .retry_retained_ordered_batch("peer", second_id, OpControl::budget_ms(5000))
        .await
        .unwrap();
    // Cancellation after target preparation but before source Open's first
    // poll must leave an authoritative no-effect recovery boundary.
    let (cancel_id, cancel_owner) = central
        .inner
        .ordered_batches
        .reserve("peer".into())
        .unwrap();
    central
        .prepare_ordered_claims(
            "peer",
            cancel_id,
            &connection,
            &database,
            &requests,
            OpControl::budget_ms(5000),
        )
        .await
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    *lock_std(&central.inner.ordered_before_open) =
        Some((Arc::clone(&entered), Arc::clone(&release)));
    let control = OpControl::budget_ms(5000);
    let ticket = control.ticket.clone();
    let driver = {
        let central = central.clone();
        tokio::spawn(async move {
            central
                .install_ordered_batch_source("peer", cancel_id, 8, 1024, 4, control)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    ticket.request_cancel();
    assert!(driver.await.unwrap().is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while cancel_owner.lock().await.source_open_driver_active {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!cancel_owner.lock().await.source_open_first_polled);
    assert!(
        !central
            .boundary()
            .calls()
            .iter()
            .any(|call| call == "ordered_open")
    );
    central
        .retry_retained_ordered_batch("peer", cancel_id, OpControl::budget_ms(5000))
        .await
        .unwrap();
    // A delayed Open has installed its source but has not returned its
    // lease. A service change must retire that exact old owner.
    let (late_id, late_owner) = central
        .inner
        .ordered_batches
        .reserve("peer".into())
        .unwrap();
    central
        .prepare_ordered_claims(
            "peer",
            late_id,
            &connection,
            &database,
            &requests,
            OpControl::budget_ms(5000),
        )
        .await
        .unwrap();
    central
        .boundary()
        .block_op(crate::boundary::FaultOp::OrderedOpen);
    let open = {
        let central = central.clone();
        tokio::spawn(async move {
            central
                .install_ordered_batch_source(
                    "peer",
                    late_id,
                    8,
                    1024,
                    4,
                    OpControl::budget_ms(5000),
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while !central
            .boundary()
            .calls()
            .iter()
            .any(|call| call == "ordered_open")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let epoch = {
        let state = late_owner.lock().await;
        assert!(state.source_open_first_polled);
        state.claims[0].epoch
    };
    central
        .boundary()
        .push_event(RadioEvent::Notification {
            peer_id: "peer".into(),
            service_uuid: SERVICE.into(),
            service_occurrence: 0,
            characteristic_uuid: CHARACTERISTIC.into(),
            characteristic_occurrence: 0,
            epoch,
            value: vec![42],
        })
        .expect("synthetic event admission failed");
    central
        .boundary()
        .stage_ordered_source_loss("peer", Some(3))
        .unwrap();
    central
        .boundary()
        .push_event(RadioEvent::ServicesChanged("peer".into()))
        .expect("synthetic event admission failed");
    tokio::time::timeout(Duration::from_secs(2), async {
        while late_owner.lock().await.setup_error.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    central
        .boundary()
        .unblock_op(crate::boundary::FaultOp::OrderedOpen);
    assert!(open.await.unwrap().is_err());
    let records = tokio::time::timeout(Duration::from_secs(2), async {
        let mut records = Vec::new();
        loop {
            match central.poll_retained_ordered_batch("peer", late_id).await {
                Ok(ubm_core::ordered_notifications::Poll::Record(record)) => records.push(record),
                Ok(ubm_core::ordered_notifications::Poll::Terminal(_)) => break records,
                _ => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .unwrap();
    assert!(matches!(&records[..], [
        ubm_core::ordered_notifications::Record::Value { value, .. },
        ubm_core::ordered_notifications::Record::Loss { count: Some(3), .. },
    ] if value == &[42]));
    central
        .retry_retained_ordered_batch("peer", late_id, OpControl::budget_ms(5000))
        .await
        .unwrap();
    central
        .discover("peer", "lease", OpControl::budget_ms(5000))
        .await
        .unwrap();
    assert_ne!(
        central
            .inner
            .core
            .lock()
            .await
            .database_generation(&peer_key)
            .unwrap(),
        database
    );
    central.shutdown().await;
}

#[test]
fn reservation_is_bounded_and_never_reuses_ids() {
    let registry = OrderedBatchRegistry::default();
    let (first_id, first) = registry.reserve("peer".into()).unwrap();
    assert_eq!(first_id, 0);
    for expected in 1..OrderedBatchRegistry::CAPACITY as u64 {
        let (id, _) = registry.reserve("peer".into()).unwrap();
        assert_eq!(id, expected);
    }
    assert!(registry.reserve("peer".into()).is_err());
    assert!(registry.remove_if_same("peer", first_id, &first));
    let (other_id, _) = registry.reserve("other-peer".into()).unwrap();
    assert_eq!(other_id, OrderedBatchRegistry::CAPACITY as u64);
    assert!(registry.get("other-peer", first_id).is_none());
    let mut ledger = lock_std(&registry.state);
    ledger.entries.clear();
    ledger.next_id = Some(u64::MAX);
    drop(ledger);
    assert_eq!(registry.reserve("peer".into()).unwrap().0, u64::MAX);
    assert!(registry.reserve("other-peer".into()).is_err());
}
