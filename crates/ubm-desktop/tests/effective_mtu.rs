//! Desktop effective ATT MTU (finding 217 follow-up): the OS-measured MTU
//! exposed as a host read. Scripted radio only; physical proof stays queued.
//!
//! macOS answers `maximumWriteValueLength(.withResponse) + 3` (the same
//! derivation as the Apple React Native route), Windows answers
//! `GattSession.MaxPduSize`, Linux answers the `org.bluez.GattCharacteristic1`
//! MTU. A radio that withholds the measurement answers
//! `capability.unsupported` with the reason, never a guessed 23.

use ubm_desktop::{DesktopCentral, FakeRadio, OpControl, PeerSnapshot, RadioEvent};

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
    assert_eq!(mtu, 515);
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
async fn unmeasured_effective_mtu_is_unsupported_with_a_reason() {
    let central = connected("peer-3").await;
    let error = central
        .read_effective_mtu("peer-3", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect_err("no measurement scripted");
    assert_eq!(error.code_str(), "capability.unsupported");
    let detail = error.detail().expect("unsupported names its reason");
    assert!(
        detail.contains("no effective ATT MTU measured"),
        "unexpected reason: {detail}"
    );
}
