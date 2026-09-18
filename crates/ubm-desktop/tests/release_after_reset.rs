//! Releasing what an adapter reset ended. The reset clears every connection
//! record (live links and links already lost before it); a later release of a
//! lease the reset ended answers `AlreadyReleased`, and an unsubscribe of a
//! consumer it ended answers released, as the legacy backends' adapter-loss
//! cleanup left terminalized handles. A lease the central never held is
//! still `connection.not-found`.

use std::time::Duration;

use ubm_desktop::{
    AdapterPowerState, AdmissionPolicy, CharacteristicSnapshot, DesktopCentral, FakeRadio,
    LinkRelease, OpControl, PeerSnapshot, PropertyFlags, RadioEvent, ServiceSnapshot,
};

const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";

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

async fn open() -> DesktopCentral<FakeRadio> {
    let radio = FakeRadio::new();
    radio.set_os_policy(AdmissionPolicy::LifecycleOnly, true);
    DesktopCentral::open(radio, "release-after-reset")
        .await
        .expect("open")
}

async fn live_link(central: &DesktopCentral<FakeRadio>, peer_id: &str, lease: &str) {
    central
        .boundary()
        .push_event(RadioEvent::Advertisement(PeerSnapshot {
            id: peer_id.to_owned(),
            address: None,
            service_uuids: vec![HRM_SERVICE.to_owned()],
            rssi: Some(-60),
            local_name: None,
            manufacturer_data: Vec::new(),
            service_data: Vec::new(),
            tx_power_level: None,
            extras: ubm_desktop::AdvertisementExtras::default(),
        }));
    for _ in 0..2000 {
        if central.peer_key_for(peer_id).await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    central.boundary().set_services(
        peer_id,
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
                descriptors: Vec::new(),
            }],
        }],
    );
    central
        .connect(peer_id, lease, OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .discover(peer_id, lease, OpControl::budget_ms(5000))
        .await
        .expect("discover");
    central
        .subscribe(
            peer_id,
            &selector(),
            "consumer-a",
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("subscribe");
}

async fn reset(central: &DesktopCentral<FakeRadio>) {
    let mut resets = central.adapter_reset_events();
    central
        .boundary()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOff));
    tokio::time::timeout(Duration::from_secs(5), resets.recv())
        .await
        .expect("a reset is published")
        .expect("reset event");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lease_and_consumer_a_reset_ended_release_as_already_released() {
    let central = open().await;
    live_link(&central, "peer-1", "lease-a").await;
    reset(&central).await;
    assert!(
        central
            .unsubscribe(
                "peer-1",
                &selector(),
                "consumer-a",
                OpControl::budget_ms(5000)
            )
            .await
            .is_ok(),
        "the reset ended the consumer"
    );
    assert_eq!(
        central
            .disconnect("peer-1", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("the reset ended the link"),
        LinkRelease::AlreadyReleased
    );
    // Once: the ended lease is forgotten after its release.
    let again = central
        .disconnect("peer-1", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect_err("released once");
    assert_eq!(again.code_str(), "connection.not-found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_lost_before_the_reset_also_releases_as_already_released() {
    let central = open().await;
    live_link(&central, "peer-1", "lease-a").await;
    central
        .boundary()
        .push_event(RadioEvent::Disconnected("peer-1".to_owned()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    reset(&central).await;
    assert_eq!(
        central
            .disconnect("peer-1", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("the reset ended the lost link"),
        LinkRelease::AlreadyReleased
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lease_never_held_is_still_not_found_after_a_reset() {
    let central = open().await;
    live_link(&central, "peer-1", "lease-a").await;
    reset(&central).await;
    let error = central
        .disconnect("peer-1", "lease-other", OpControl::budget_ms(5000))
        .await
        .expect_err("never held");
    assert_eq!(error.code_str(), "connection.not-found");
}
