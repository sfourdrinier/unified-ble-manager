//! Adapter loss, admission and first-state wait (PR210 findings 57-60, 63,
//! 65) over the scriptable radio. The OS sources behind these events (btleplug
//! `StateUpdate`, the BlueZ name-owner and object watchers, the WinRT radio
//! watcher) are type-checked on their hosts; here the central's own rules are
//! proven: what a loss ends, the generation it advances, what admission
//! refuses before any effect, and how long an open waits for a usable
//! adapter.

use std::time::Duration;

use ubm_desktop::{
    AdapterAuthorization, AdapterAvailability, AdapterLossCause, AdapterPowerState,
    AdmissionPolicy, CharacteristicSnapshot, DesktopCentral, FakeRadio, FaultOp, InvalidationCause,
    LifecycleKind, NotificationPoll, OpControl, PeerSnapshot, PropertyFlags, RadioEvent,
    ScanDuplicatePolicy, ServiceSnapshot, WriteLimits,
};

const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";

fn advertisement(peer_id: &str) -> RadioEvent {
    RadioEvent::Advertisement(PeerSnapshot {
        id: peer_id.to_owned(),
        address: None,
        service_uuids: vec![HRM_SERVICE.to_owned()],
        rssi: Some(-60),
        local_name: None,
        manufacturer_data: Vec::new(),
        service_data: Vec::new(),
        tx_power_level: None,
        extras: ubm_desktop::AdvertisementExtras::default(),
    })
}

fn hrm_service() -> ServiceSnapshot {
    ServiceSnapshot {
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
    }
}

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

/// A radio scripted as a desktop OS radio: `policy` gate, teardown on loss.
fn os_radio(policy: AdmissionPolicy) -> FakeRadio {
    let radio = FakeRadio::new();
    radio.set_os_policy(policy, true);
    radio
}

async fn open(radio: FakeRadio) -> DesktopCentral<FakeRadio> {
    DesktopCentral::open(radio, "adapter-loss-host")
        .await
        .expect("open")
}

async fn wait_until<F: Fn() -> bool>(what: &str, done: F) {
    for _ in 0..2000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn known_peer(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
    central.boundary().push_event(advertisement(peer_id));
    for _ in 0..2000 {
        if central.peer_key_for(peer_id).await.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("peer {peer_id} never became known");
}

/// Connected, discovered and subscribed.
async fn live_link(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
    known_peer(central, peer_id).await;
    central
        .boundary()
        .set_services(peer_id, vec![hrm_service()]);
    central
        .connect(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .discover(peer_id, "lease-a", OpControl::budget_ms(5000))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_power_loss_tears_down_everything_live_and_advances_the_generation() {
    let central = open(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let mut resets = central.adapter_reset_events();
    let mut lifecycle = central.lifecycle_events();
    let mut scan_ends = central.scan_terminal_events();
    live_link(&central, "peer-1").await;
    let scan = central
        .start_scan("scanner", &[], OpControl::budget_ms(5000))
        .await
        .expect("scan");
    // A read held in flight at the radio when the adapter goes away.
    central.boundary().block_op(FaultOp::Read);
    let reader = {
        let central = central.clone();
        tokio::spawn(async move {
            central
                .read("peer-1", &selector(), OpControl::budget_ms(30_000))
                .await
        })
    };
    wait_until("the read to reach the radio", || {
        central
            .boundary()
            .calls()
            .iter()
            .any(|call| call == "read_characteristic")
    })
    .await;
    let before = central.attachment();

    central
        .boundary()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOff));

    let reset = tokio::time::timeout(Duration::from_secs(5), resets.recv())
        .await
        .expect("a reset is published")
        .expect("reset event");
    assert_eq!(reset.cause, AdapterLossCause::PoweredOff);
    assert_eq!(reset.previous, before);
    assert_eq!(reset.current, central.attachment());
    assert_ne!(
        reset.current.backend_generation(),
        before.backend_generation(),
        "the backend generation advances"
    );
    assert_ne!(
        reset.current.adapter_generation(),
        before.adapter_generation(),
        "the adapter generation advances"
    );
    assert_eq!(reset.current.adapter_id(), before.adapter_id());
    assert!(
        reset.cancelled_operations >= 1,
        "the read and the scan settle"
    );
    assert_eq!(reset.ended_scan.as_ref(), Some(scan.operation_id()));
    assert_eq!(reset.released_links, vec!["peer-1".to_owned()]);
    assert_eq!(reset.ended_subscriptions, 1);
    assert!(
        reset.release_failures.is_empty(),
        "{:?}",
        reset.release_failures
    );

    let read = tokio::time::timeout(Duration::from_secs(5), reader)
        .await
        .expect("the in-flight read wakes")
        .expect("join");
    let error = read.expect_err("the read ends with the adapter");
    assert_eq!(error.code_str(), "operation.reset");

    let mut kinds = Vec::new();
    while let Ok(event) = lifecycle.try_recv() {
        kinds.push(event.kind);
    }
    assert!(
        kinds.contains(&LifecycleKind::AdapterLost),
        "the link ends with cause adapter: {kinds:?}"
    );
    let ended = scan_ends.try_recv().expect("the scan end is reported");
    assert!(ended.aborted, "a scan the adapter took away is aborted");
    assert!(ended.detail.contains("powered-off"), "{}", ended.detail);
    assert!(
        !central.has_active_scan().await,
        "no scan survives the loss"
    );

    assert_eq!(
        central
            .poll_notification("peer-1", &selector(), "consumer-a")
            .await
            .expect("poll"),
        NotificationPoll::Invalidated(InvalidationCause::AdapterReset)
    );
    let calls = central.boundary().calls();
    assert!(calls.iter().any(|call| call == "stop_scan"), "{calls:?}");
    assert!(calls.iter().any(|call| call == "disconnect"), "{calls:?}");
    assert!(!central.boundary().link_connected("peer-1"));
    let status = central.adapter_status();
    assert!(status.lost);
    assert_eq!(status.power, Some(AdapterPowerState::PoweredOff));
    central.boundary().unblock_op(FaultOp::Read);
}

/// Finding 94: an adapter-state or authorization read in flight when the
/// adapter resets is never torn down with `operation.reset`; it answers
/// the OS's current (post-transition) state, as every legacy host did. The
/// reads are held at the radio (a barrier, not timing) until the reset has
/// been published.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adapter_reads_racing_a_reset_answer_the_post_transition_state() {
    let radio = os_radio(AdmissionPolicy::CoreBluetooth);
    radio.set_adapter_state(AdapterPowerState::PoweredOn);
    radio.set_adapter_authorization(AdapterAuthorization::Granted);
    let central = std::sync::Arc::new(open(radio).await);
    let mut resets = central.adapter_reset_events();
    central.boundary().block_op(FaultOp::AdapterState);
    central.boundary().block_op(FaultOp::AdapterAuthorization);
    let power = tokio::spawn({
        let central = central.clone();
        async move { central.adapter_state(OpControl::budget_ms(5000)).await }
    });
    let authorization = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .adapter_authorization(OpControl::budget_ms(5000))
                .await
        }
    });
    wait_until("both reads to reach the radio", || {
        let calls = central.boundary().calls();
        calls.iter().any(|call| call == "adapter_state")
            && calls.iter().any(|call| call == "adapter_authorization")
    })
    .await;
    central
        .boundary()
        .set_adapter_state(AdapterPowerState::Unsupported);
    central
        .boundary()
        .set_adapter_authorization(AdapterAuthorization::Denied);
    central
        .boundary()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::Unsupported));
    tokio::time::timeout(Duration::from_secs(5), resets.recv())
        .await
        .expect("reset")
        .expect("event");
    central.boundary().unblock_op(FaultOp::AdapterState);
    central.boundary().unblock_op(FaultOp::AdapterAuthorization);
    assert_eq!(
        power.await.expect("join"),
        Ok(AdapterPowerState::Unsupported)
    );
    assert_eq!(
        authorization.await.expect("join"),
        Ok(AdapterAuthorization::Denied)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_loss_resets_once_and_power_on_ends_the_loss() {
    let central = open(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let mut resets = central.adapter_reset_events();
    let radio = central.boundary();
    radio.push_event(RadioEvent::AdapterState(AdapterPowerState::Resetting));
    radio.push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOff));
    let first = tokio::time::timeout(Duration::from_secs(5), resets.recv())
        .await
        .expect("reset")
        .expect("event");
    assert_eq!(first.cause, AdapterLossCause::Resetting);
    radio.push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOn));
    wait_until("power on", || !central.adapter_status().lost).await;
    assert!(
        resets.try_recv().is_err(),
        "a second loss state while lost is not a second reset"
    );
    radio.push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOff));
    let second = tokio::time::timeout(Duration::from_secs(5), resets.recv())
        .await
        .expect("reset")
        .expect("event");
    assert_eq!(second.cause, AdapterLossCause::PoweredOff);
    assert_eq!(second.previous, first.current, "generations chain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_adapter_or_restarted_daemon_resets_and_reads_unavailable() {
    let central = open(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let mut resets = central.adapter_reset_events();
    live_link(&central, "peer-2").await;
    central
        .boundary()
        .push_event(RadioEvent::AdapterLost(AdapterLossCause::DaemonRestarted));
    let reset = tokio::time::timeout(Duration::from_secs(5), resets.recv())
        .await
        .expect("reset")
        .expect("event");
    assert_eq!(reset.cause, AdapterLossCause::DaemonRestarted);
    assert_eq!(reset.released_links, vec!["peer-2".to_owned()]);
    assert_eq!(
        central.adapter_status().availability,
        AdapterAvailability::Unavailable
    );
    central.boundary().push_event(RadioEvent::AdapterRestored);
    wait_until("the adapter to come back", || {
        central.adapter_status().availability != AdapterAvailability::Unavailable
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_radio_without_teardown_reports_the_state_only() {
    let central = open(FakeRadio::new()).await;
    let mut resets = central.adapter_reset_events();
    let mut adapter = central.adapter_events();
    known_peer(&central, "peer-3").await;
    central
        .connect("peer-3", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .boundary()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOff));
    let event = tokio::time::timeout(Duration::from_secs(5), adapter.recv())
        .await
        .expect("state")
        .expect("event");
    assert_eq!(event.state, AdapterPowerState::PoweredOff);
    assert!(resets.try_recv().is_err(), "no teardown was asked for");
    assert!(central.boundary().link_connected("peer-3"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corebluetooth_admission_refuses_before_any_effect_in_legacy_order() {
    let radio = os_radio(AdmissionPolicy::CoreBluetooth);
    radio.set_adapter_state(AdapterPowerState::PoweredOff);
    radio.set_adapter_authorization(AdapterAuthorization::Denied);
    let central = open(radio).await;
    known_peer(&central, "peer-4").await;
    let error = central
        .connect("peer-4", "lease-a", OpControl::budget_ms(1000))
        .await
        .expect_err("authorization first");
    assert_eq!(error.code_str(), "permission.denied");
    assert_eq!(error.domain().as_str(), "adapter");
    assert_eq!(
        error.commit(),
        Some(ubm_core::contracts::CommitState::NotDispatched),
        "nothing reached the radio"
    );
    assert_eq!(
        error.retryability(),
        ubm_desktop::Retryability::CallerDecides
    );
    assert_eq!(radio_calls(&central, "connect"), 0, "no radio call");

    central
        .boundary()
        .push_event(RadioEvent::AdapterAuthorization(
            AdapterAuthorization::NotDetermined,
        ));
    wait_until("authorization", || {
        central.adapter_status().authorization == Some(AdapterAuthorization::NotDetermined)
    })
    .await;
    let error = central
        .start_scan("scanner", &[], OpControl::budget_ms(1000))
        .await
        .expect_err("a pending decision is refused on CoreBluetooth");
    assert_eq!(error.code_str(), "permission.not-determined");

    central
        .boundary()
        .push_event(RadioEvent::AdapterAuthorization(
            AdapterAuthorization::Granted,
        ));
    wait_until("granted", || {
        central.adapter_status().authorization == Some(AdapterAuthorization::Granted)
    })
    .await;
    let error = central
        .start_scan("scanner", &[], OpControl::budget_ms(1000))
        .await
        .expect_err("power next");
    assert_eq!(error.code_str(), "adapter.powered-off");

    central
        .boundary()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::Resetting));
    wait_until("resetting", || {
        central.adapter_status().power == Some(AdapterPowerState::Resetting)
    })
    .await;
    let error = central
        .connect("peer-4", "lease-a", OpControl::budget_ms(1000))
        .await
        .expect_err("resetting");
    assert_eq!(error.code_str(), "adapter.resetting");

    central
        .boundary()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::Unsupported));
    wait_until("unsupported", || {
        central.adapter_status().availability == AdapterAvailability::Unsupported
    })
    .await;
    let error = central
        .connect("peer-4", "lease-a", OpControl::budget_ms(1000))
        .await
        .expect_err("unsupported");
    assert_eq!(error.code_str(), "adapter.unavailable");

    central
        .boundary()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOn));
    wait_until("powered on", || usable(&central)).await;
    central
        .connect("peer-4", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("admitted once usable");
    assert_eq!(radio_calls(&central, "connect"), 1);
}

fn radio_calls(central: &DesktopCentral<FakeRadio>, operation: &str) -> usize {
    central
        .boundary()
        .calls()
        .iter()
        .filter(|call| *call == operation)
        .count()
}

fn usable(central: &DesktopCentral<FakeRadio>) -> bool {
    central.adapter_status().power == Some(AdapterPowerState::PoweredOn)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corebluetooth_cleanup_stays_admitted_while_winrt_gates_unsubscribe() {
    for (policy, gated) in [
        (AdmissionPolicy::CoreBluetooth, false),
        (AdmissionPolicy::WinRt, true),
    ] {
        let radio = FakeRadio::new();
        // Admission only: this test keeps the links to probe cleanup.
        radio.set_os_policy(policy, false);
        let central = open(radio).await;
        live_link(&central, "peer-5").await;
        central
            .boundary()
            .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOff));
        wait_until("powered off", || {
            central.adapter_status().power == Some(AdapterPowerState::PoweredOff)
        })
        .await;
        let outcome = central
            .unsubscribe(
                "peer-5",
                &selector(),
                "consumer-a",
                OpControl::budget_ms(5000),
            )
            .await;
        if gated {
            assert_eq!(
                outcome.expect_err("WinRT gated unsubscribe").code_str(),
                "adapter.powered-off"
            );
        } else {
            outcome.expect("CoreBluetooth never gated unsubscribe");
        }
        central
            .disconnect("peer-5", "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("disconnect is never gated");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn winrt_admission_reads_availability_before_authorization() {
    let radio = os_radio(AdmissionPolicy::WinRt);
    radio.set_adapter_authorization(AdapterAuthorization::Denied);
    let central = open(radio).await;
    known_peer(&central, "peer-6").await;
    let error = central
        .connect("peer-6", "lease-a", OpControl::budget_ms(1000))
        .await
        .expect_err("denied");
    assert_eq!(error.code_str(), "permission.denied");
    central
        .boundary()
        .push_event(RadioEvent::AdapterLost(AdapterLossCause::Removed));
    wait_until("removed", || {
        central.adapter_status().availability == AdapterAvailability::Unavailable
    })
    .await;
    let error = central
        .connect("peer-6", "lease-a", OpControl::budget_ms(1000))
        .await
        .expect_err("removed");
    assert_eq!(
        error.code_str(),
        "adapter.unavailable",
        "availability wins over authorization on WinRT"
    );
    // A pending decision reads as an unusable adapter on WinRT.
    central.boundary().push_event(RadioEvent::AdapterRestored);
    central
        .boundary()
        .push_event(RadioEvent::AdapterAuthorization(
            AdapterAuthorization::NotDetermined,
        ));
    wait_until("not determined", || {
        central.adapter_status().authorization == Some(AdapterAuthorization::NotDetermined)
            && central.adapter_status().availability != AdapterAvailability::Unavailable
    })
    .await;
    let error = central
        .connect("peer-6", "lease-a", OpControl::budget_ms(1000))
        .await
        .expect_err("pending");
    assert_eq!(error.code_str(), "adapter.unavailable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gateless_radio_and_an_unreported_fact_admit() {
    let radio = FakeRadio::new();
    radio.set_adapter_state(AdapterPowerState::PoweredOff);
    let central = open(radio).await;
    assert!(
        !central
            .boundary()
            .calls()
            .iter()
            .any(|call| call == "adapter_state"),
        "a gate-less radio is not asked for facts at open"
    );
    known_peer(&central, "peer-7").await;
    central
        .connect("peer-7", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("BlueZ legacy had no adapter gate");

    // A gated radio that cannot report a fact admits on it.
    let central = open(os_radio(AdmissionPolicy::CoreBluetooth)).await;
    assert_eq!(central.adapter_status().power, None);
    assert_eq!(
        central.adapter_status().availability,
        AdapterAvailability::Unknown
    );
    known_peer(&central, "peer-8").await;
    central
        .connect("peer-8", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("no fact, no refusal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_usable_state_is_awaited_within_the_bound() {
    let radio = os_radio(AdmissionPolicy::CoreBluetooth);
    radio.set_adapter_state(AdapterPowerState::Unknown);
    let central = open(radio).await;
    let error = central
        .await_usable_adapter(Duration::from_millis(50))
        .await
        .expect_err("never usable");
    assert_eq!(error.code_str(), "capability.unavailable");
    assert_eq!(error.operation(), "adapter.initialize");
    assert!(
        error
            .detail()
            .is_some_and(|detail| detail.starts_with(ubm_desktop::ADAPTER_INITIALIZATION_TIMED_OUT)),
        "{error:?}"
    );
    let waiter = {
        let central = central.clone();
        tokio::spawn(async move { central.await_usable_adapter(Duration::from_secs(5)).await })
    };
    central
        .boundary()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOn));
    assert_eq!(
        waiter.await.expect("join").expect("usable"),
        AdapterPowerState::PoweredOn
    );
    // A refusal keeps the adapter unusable even when powered on.
    central
        .boundary()
        .push_event(RadioEvent::AdapterAuthorization(
            AdapterAuthorization::Denied,
        ));
    wait_until("denied", || {
        central.adapter_status().authorization == Some(AdapterAuthorization::Denied)
    })
    .await;
    central
        .await_usable_adapter(Duration::from_millis(20))
        .await
        .expect_err("denied is not usable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_duplicate_policy_reaches_the_radio() {
    let central = open(FakeRadio::new()).await;
    for policy in [
        ScanDuplicatePolicy::First,
        ScanDuplicatePolicy::Merged,
        ScanDuplicatePolicy::All,
    ] {
        let scan = central
            .start_scan_with(
                "scanner",
                &[HRM_SERVICE],
                policy,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("scan");
        central
            .stop_scan(scan.operation_id(), OpControl::budget_ms(5000))
            .await
            .expect("stop");
    }
    let applied: Vec<(ScanDuplicatePolicy, bool)> = central
        .boundary()
        .scan_filters()
        .iter()
        .map(|filter| (filter.duplicates, filter.allow_duplicates()))
        .collect();
    assert_eq!(
        applied,
        vec![
            (ScanDuplicatePolicy::First, false),
            (ScanDuplicatePolicy::Merged, false),
            (ScanDuplicatePolicy::All, true),
        ]
    );
}

/// Finding 89 (N10): the caller's name prefix reaches the radio's scan
/// filter (BlueZ `Pattern`, as the legacy backend sent); no prefix sets
/// none, and an empty prefix is refused before any radio effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_name_prefix_reaches_the_radio() {
    let central = open(FakeRadio::new()).await;
    for prefix in [Some("Polar H10"), None] {
        let scan = central
            .start_scan_matching(
                "scanner",
                &[HRM_SERVICE],
                ScanDuplicatePolicy::First,
                prefix,
                OpControl::budget_ms(5000),
            )
            .await
            .expect("scan");
        central
            .stop_scan(scan.operation_id(), OpControl::budget_ms(5000))
            .await
            .expect("stop");
    }
    let refused = central
        .start_scan_matching(
            "scanner",
            &[],
            ScanDuplicatePolicy::All,
            Some(""),
            OpControl::budget_ms(5000),
        )
        .await
        .expect_err("an empty prefix");
    assert_eq!(refused.code_str(), "argument.invalid");
    let prefixes: Vec<Option<String>> = central
        .boundary()
        .scan_filters()
        .into_iter()
        .map(|filter| filter.name_prefix)
        .collect();
    assert_eq!(prefixes, vec![Some("Polar H10".to_owned()), None]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_connection_write_limit_needs_no_discovery() {
    let central = open(FakeRadio::new()).await;
    known_peer(&central, "peer-9").await;
    central
        .connect("peer-9", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central.boundary().set_write_limits(
        "peer-9",
        WriteLimits {
            with_response: 512,
            without_response: 182,
        },
    );
    let with_response = central
        .connection_maximum_write_length("peer-9", "lease-a", true, OpControl::budget_ms(5000))
        .await
        .expect("with response");
    let without_response = central
        .connection_maximum_write_length("peer-9", "lease-a", false, OpControl::budget_ms(5000))
        .await
        .expect("without response");
    assert_eq!((with_response, without_response), (512, 182));
    assert!(
        !central
            .boundary()
            .calls()
            .iter()
            .any(|call| call == "discover"),
        "no discovery was needed"
    );
    let foreign = central
        .connection_maximum_write_length("peer-9", "lease-z", true, OpControl::budget_ms(5000))
        .await
        .expect_err("lease holder only");
    assert_eq!(foreign.code_str(), "ownership.denied");
}
