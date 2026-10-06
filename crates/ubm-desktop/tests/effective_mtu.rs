//! Desktop effective ATT MTU. Scripted radio only; physical proof stays queued.
//!
//! A scripted value is that observation. An unset script is `Ok(None)`:
//! unobserved, not a link failure and not `capability.unsupported`.

use ubm_desktop::{
    DesktopCentral, FakeRadio, FaultOp, OpControl, PeerSnapshot, PlatformDetail, RadioEvent,
};

fn advertisement(peer_id: &str) -> RadioEvent {
    RadioEvent::Advertisement(PeerSnapshot {
        id: peer_id.to_owned(),
        address: None,
        service_uuids: Vec::new(),
        rssi: Some(-60),
        local_name: None,
        manufacturer_data: Vec::new(),
        service_data: Vec::new(),
        tx_power_level: None,
        extras: ubm_desktop::AdvertisementExtras::default(),
    })
}

async fn connected(peer_id: &str) -> DesktopCentral<FakeRadio> {
    let central = DesktopCentral::open(FakeRadio::new(), "parity-host")
        .await
        .expect("open");
    central.boundary().push_event(advertisement(peer_id));
    central
        .connect(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connected_effective_mtu_reads_the_radio_for_the_lease_holder() {
    let central = connected("peer-1").await;
    central.boundary().set_effective_mtu("peer-1", 515);
    let mtu = central
        .read_effective_mtu("peer-1", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("effective mtu");
    assert_eq!(mtu, Some(515));
    let foreign = central
        .read_effective_mtu("peer-1", "lease-z", OpControl::budget_ms(5000))
        .await
        .expect_err("only the lease holder reads the link");
    assert_eq!(foreign.code_str(), "ownership.denied");
    assert_eq!(
        central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "read_effective_mtu")
            .count(),
        1,
        "the refused read never reached the radio"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn effective_mtu_is_refused_on_a_link_that_is_not_connected() {
    let central = connected("peer-2").await;
    central
        .disconnect("peer-2", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("disconnect");
    let error = central
        .read_effective_mtu("peer-2", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect_err("no live link");
    assert_eq!(error.code_str(), "connection.stale");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmeasured_effective_mtu_is_unobserved() {
    let central = connected("peer-3").await;
    let mtu = central
        .read_effective_mtu("peer-3", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("an unobserved MTU is not a link failure");
    assert_eq!(mtu, None);
}

/// RV3 finding 1: a link drop mid-MTU-read is one physical event with the
/// characteristic-read name (`connection.lost`), never the raw radio code,
/// with the platform's answer kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn effective_mtu_link_loss_is_connection_lost() {
    let central = connected("peer-mtu-lost").await;
    central.boundary().fail_next_with_platform(
        FaultOp::EffectiveMtu,
        "gone",
        PlatformDetail::new("btleplug", "not-connected").with_message("Not connected"),
    );
    let error = central
        .read_effective_mtu("peer-mtu-lost", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect_err("MTU read on a lost link");
    assert_eq!(error.code_str(), "connection.lost");
    assert_eq!(error.domain().as_str(), "connection");
    assert!(error.platform().is_some(), "the platform's answer is kept");
}
