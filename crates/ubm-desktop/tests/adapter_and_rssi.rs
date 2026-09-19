//! Parity addendum (FIX-PLAN decisions 10–13): connected RSSI, adapter
//! power state with its lifecycle signal, and adapter selection reaching the
//! central. Scripted radio only; physical proof stays queued.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ubm_desktop::{
    AdapterPowerState, CentralProfile, CentralSignal, DesktopCentral, FakeRadio, FaultOp,
    OpControl, PeerSnapshot, RadioEvent, Retryability,
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
async fn connected_rssi_reads_the_radio_for_the_lease_holder() {
    let central = connected("peer-1").await;
    central.boundary().set_rssi("peer-1", -47);
    let rssi = central
        .read_rssi("peer-1", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("rssi");
    assert_eq!(rssi, -47);
    let foreign = central
        .read_rssi("peer-1", "lease-z", OpControl::budget_ms(5000))
        .await
        .expect_err("only the lease holder reads the link");
    assert_eq!(foreign.code_str(), "ownership.denied");
    assert_eq!(
        central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "read_rssi")
            .count(),
        1,
        "the refused read never reached the radio"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rssi_is_refused_on_a_link_that_is_not_connected() {
    let central = connected("peer-2").await;
    central
        .disconnect("peer-2", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("disconnect");
    let error = central
        .read_rssi("peer-2", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect_err("no live link");
    assert_eq!(error.code_str(), "connection.stale");
}

#[tokio::test(start_paused = true)]
async fn rssi_is_bounded_by_the_budget_and_unmeasured_is_unsupported() {
    let central = connected("peer-3").await;
    central.boundary().block_op(FaultOp::Rssi);
    let error = central
        .read_rssi("peer-3", "lease-a", OpControl::budget_ms(50))
        .await
        .expect_err("held read is bounded");
    assert_eq!(error.code_str(), "operation.timed-out");
    assert_eq!(error.retryability(), Retryability::CallerDecides);
    central.boundary().unblock_op(FaultOp::Rssi);
    // Nothing scripted: the radio cannot measure it, and says so.
    let unmeasured = central
        .read_rssi("peer-3", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect_err("unmeasured");
    assert_eq!(unmeasured.code_str(), "capability.unsupported");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adapter_state_reads_the_radio_and_changes_are_published() {
    let seen: Arc<Mutex<Vec<CentralSignal>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut profile = CentralProfile::desktop("adapter-host");
    profile.observer = Some(Arc::new(move |signal| {
        sink.lock().expect("sink").push(signal);
    }));
    let central = DesktopCentral::open_with(FakeRadio::new(), profile)
        .await
        .expect("open");
    central
        .boundary()
        .set_adapter_state(AdapterPowerState::PoweredOn);
    assert_eq!(
        central
            .adapter_state(OpControl::budget_ms(5000))
            .await
            .expect("state"),
        AdapterPowerState::PoweredOn
    );
    let mut events = central.adapter_events();
    central
        .boundary()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOff));
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("adapter event arrives")
        .expect("event");
    assert_eq!(event.state, AdapterPowerState::PoweredOff);
    assert_eq!(event.sequence, 1);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        seen.lock()
            .expect("sink")
            .iter()
            .any(|signal| matches!(signal, CentralSignal::Adapter(observed) if observed == &event)),
        "the observer sees the same adapter event"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_requested_adapter_is_the_one_the_central_opens_on() {
    let mut profile = CentralProfile::desktop("select-host");
    profile.adapter_id = Some("fake-desktop-adapter".to_owned());
    let central = DesktopCentral::open_with(FakeRadio::new(), profile)
        .await
        .expect("matching adapter opens");
    assert_eq!(
        central.attachment().adapter_id().as_str(),
        "fake-desktop-adapter"
    );
    let mut wrong = CentralProfile::desktop("select-host");
    wrong.adapter_id = Some("hci9".to_owned());
    match DesktopCentral::open_with(FakeRadio::new(), wrong).await {
        Ok(_) => panic!("a boundary on another adapter must never open silently"),
        Err(error) => {
            assert_eq!(error.code_str(), "adapter.unavailable");
            assert_eq!(error.operation(), "adapter.select");
        }
    }
}
