//! Per-central shutdown receipt for the desktop central (HOST-DESKTOP).
//!
//! Lives in its own test binary (separate process) because the final step
//! records the explicit process-global executor shutdown latch: running it
//! inside the lib test process would refuse admission for every other test
//! there. No radio is touched. Per-central shutdown itself (F14) never sets
//! that latch — other managers keep working and new managers can open.

use ubm_desktop::{
    CharacteristicSnapshot, DescriptorSnapshot, DesktopCentral, FakeRadio, PeerSnapshot,
    PropertyFlags, RadioEvent, ServiceSnapshot,
};

const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";

#[tokio::test]
async fn shutdown_stops_scan_and_refuses_new_work() {
    let central = DesktopCentral::open(FakeRadio::new(), "test-host")
        .await
        .expect("open");
    central
        .start_scan("owner-a", &[], 5000)
        .await
        .expect("start scan");
    assert!(central.has_active_scan().await);

    // M3 setup: one live subscription before teardown.
    central
        .boundary()
        .push_event(RadioEvent::Advertisement(PeerSnapshot {
            id: "peer-1".to_owned(),
            address: None,
            service_uuids: vec![HRM_SERVICE.to_owned()],
            rssi: Some(-60),
            local_name: None,
            manufacturer_data: Vec::new(),
            service_data: Vec::new(),
            tx_power_level: None,
        }));
    central
        .connect("peer-1", "lease-a", 5000)
        .await
        .expect("connect");
    central.boundary().set_services(
        "peer-1",
        vec![ServiceSnapshot {
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
                    indicate: false,
                },
                descriptors: vec![DescriptorSnapshot {
                    uuid: "00002901-0000-1000-8000-00805f9b34fb".to_owned(),
                    occurrence: 0,
                }],
            }],
        }],
    );
    central
        .discover("peer-1", "lease-a")
        .await
        .expect("discover");
    let selector = DesktopCentral::<FakeRadio>::selector(
        HRM_SERVICE,
        Some(0),
        Some(HRM_MEASUREMENT),
        Some(0),
        None,
        None,
    )
    .expect("selector");
    central
        .subscribe("peer-1", &selector, "consumer-a", 5000)
        .await
        .expect("subscribe");
    assert_eq!(
        central.boundary().live_subscription_count(),
        1,
        "one live CCCD before shutdown"
    );

    central.shutdown().await;

    assert!(central.is_shut_down(), "shutdown recorded");
    assert!(
        !central.has_active_scan().await,
        "owned scan cleaned up by shutdown"
    );
    assert!(
        !central.boundary().scan_active(),
        "OS scan stopped by shutdown"
    );
    // M3: no live OS subscription outlives the central.
    assert_eq!(
        central.boundary().live_subscription_count(),
        0,
        "zero live CCCDs after shutdown"
    );
    assert!(
        central.boundary().calls().contains(&"close".to_owned()),
        "shutdown drives the boundary teardown hook"
    );
    let error = central
        .start_scan("owner-a", &[], 5000)
        .await
        .expect_err("no admission after shutdown");
    assert_eq!(error.code_str(), "adapter.unavailable");
    let error = central
        .connect("peer-x", "lease-a", 5000)
        .await
        .expect_err("no admission after shutdown");
    assert_eq!(error.code_str(), "adapter.unavailable");

    // Idempotent: a second shutdown is safe cleanup, not a second record.
    central.shutdown().await;
    assert!(central.is_shut_down());

    // F14: per-central shutdown never latches the process executor — a new
    // central opens fine afterwards. Only the explicit process-owner step
    // closes admission globally.
    DesktopCentral::open(FakeRadio::new(), "test-host")
        .await
        .expect("new central opens after per-central shutdown");
    ubm_desktop::executor::shutdown_desktop_runtime();
    match DesktopCentral::open(FakeRadio::new(), "test-host").await {
        Ok(_) => panic!("no new centrals after process shutdown"),
        Err(error) => assert_eq!(error.code_str(), "adapter.unavailable"),
    }
}
