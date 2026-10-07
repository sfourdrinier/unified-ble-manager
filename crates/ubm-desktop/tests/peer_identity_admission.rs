//! Deterministic identity admission; no hardware qualification.
use std::time::Duration;
use ubm_desktop::{
    CharacteristicSnapshot, ConnectionState, DesktopCentral, FakeRadio, OpControl, PropertyFlags,
    RadioEvent, ServiceSnapshot,
};

const CANONICAL: &str = "00e2ce71-3ba4-6569-e3de-3081ce0c95fb";
const ALIAS: &str = "00E2CE71-3BA4-6569-E3DE-3081CE0C95FB";

#[tokio::test]
async fn aliases_are_refused_before_ownership_and_native_events_retire_the_canonical_owner() {
    let radio = FakeRadio::new();
    radio.set_canonical_peer_id(ALIAS, CANONICAL);
    let service = "0000180d-0000-1000-8000-00805f9b34fb";
    let characteristic = "00002a37-0000-1000-8000-00805f9b34fb";
    radio.set_services(
        CANONICAL,
        vec![ServiceSnapshot {
            primary: None,
            included_services: None,
            uuid: service.into(),
            occurrence: 0,
            characteristics: vec![CharacteristicSnapshot {
                uuid: characteristic.into(),
                occurrence: 0,
                properties: PropertyFlags {
                    notify: true,
                    indicate: false,
                    read: true,
                    write: false,
                    write_without_response: false,
                },
                descriptors: vec![],
            }],
            access: std::default::Default::default(),
        }],
    );
    let central = DesktopCentral::open(radio, "identity-admission")
        .await
        .unwrap();
    let error = central
        .connect(ALIAS, "alias-client", OpControl::unbounded())
        .await
        .unwrap_err();
    assert_eq!(error.code_str(), "argument.invalid");
    assert_eq!(error.operation(), "connection.connect");
    assert!(error.detail().unwrap().contains(CANONICAL));
    assert!(central.peer_records().await.is_empty());
    assert!(
        !central
            .boundary()
            .calls()
            .iter()
            .any(|call| call == "connect")
    );

    central
        .connect(CANONICAL, "owner", OpControl::unbounded())
        .await
        .unwrap();
    central
        .connect(ALIAS, "second-alias-client", OpControl::unbounded())
        .await
        .unwrap_err();
    assert_eq!(
        central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "connect")
            .count(),
        1
    );
    assert_eq!(central.peer_records().await.len(), 1);

    central
        .discover(CANONICAL, "owner", OpControl::unbounded())
        .await
        .unwrap();
    let selector = DesktopCentral::<FakeRadio>::selector(
        service,
        Some(0),
        Some(characteristic),
        Some(0),
        None,
        None,
    )
    .unwrap();
    central
        .subscribe(
            CANONICAL,
            &selector,
            "notifications",
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let mut wake = central.native_wakes();
    central.boundary().push_event(RadioEvent::Notification {
        peer_id: CANONICAL.into(),
        service_uuid: service.into(),
        service_occurrence: 0,
        characteristic_uuid: characteristic.into(),
        characteristic_occurrence: 0,
        value: vec![0, 72],
        epoch: central.routing_epoch(CANONICAL).await,
    });
    tokio::time::timeout(Duration::from_secs(2), wake.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        central
            .take_notification(CANONICAL, &selector, "notifications")
            .await
            .unwrap(),
        Some(vec![0, 72])
    );
    assert!(
        central
            .take_notification(ALIAS, &selector, "notifications")
            .await
            .is_err()
    );

    let mut lifecycle = central.lifecycle_events();
    central
        .boundary()
        .push_event(RadioEvent::Disconnected(CANONICAL.into()));
    tokio::time::timeout(Duration::from_secs(2), lifecycle.recv())
        .await
        .unwrap()
        .unwrap();
    let records = central.peer_records().await;
    assert_eq!(records[0].peer_id, CANONICAL);
    assert_eq!(records[0].connection_state, Some(ConnectionState::Lost));
    let report = central.shutdown().await;
    assert!(report.radio_close_failures.is_empty());
    assert_eq!(
        report.record.unwrap().state(),
        ubm_core::ownership::CleanupState::Released
    );
}

#[tokio::test]
async fn opaque_case_sensitive_radio_ids_are_not_case_folded() {
    let central = DesktopCentral::open(FakeRadio::new(), "opaque-identities")
        .await
        .unwrap();
    central
        .connect("opaque-A", "first", OpControl::unbounded())
        .await
        .unwrap();
    central
        .connect("opaque-a", "second", OpControl::unbounded())
        .await
        .unwrap();
    assert_eq!(central.peer_records().await.len(), 2);
    let report = central.shutdown().await;
    assert!(report.radio_close_failures.is_empty());
    assert_eq!(
        report.record.unwrap().state(),
        ubm_core::ownership::CleanupState::Released
    );
}
