//! Release retention acceptance (PR210-09): a failed, hung or cancelled
//! release keeps the resource and its identity, the next attempt really
//! calls the radio again, concurrent releases share one radio call, and a
//! release addressed to another resource never touches the radio.
//!
//! For each of scan stop, unsubscribe, disconnect and shutdown: the first
//! native attempt fails, the second succeeds — two native calls, the same
//! identity, a truthful first error, and nothing deleted before the
//! acknowledgement.

use std::time::Duration;

use ubm_desktop::{
    CharacteristicSnapshot, ConnectionState, DesktopCentral, FakeRadio, FaultOp, LinkRelease,
    NotificationPoll, OpControl, PeerSnapshot, PropertyFlags, RadioEvent, Retryability, ScanStop,
    ServiceSnapshot,
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

async fn open() -> DesktopCentral<FakeRadio> {
    DesktopCentral::open(FakeRadio::new(), "retention-host")
        .await
        .expect("open")
}

async fn wait_peer(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
    for _ in 0..2000 {
        if central.peer_key_for(peer_id).await.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("peer {peer_id} never resolved");
}

async fn until(mut probe: impl FnMut() -> bool, what: &str) {
    for _ in 0..2000 {
        if probe() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn ready_peer(central: &DesktopCentral<FakeRadio>, peer_id: &str) -> String {
    central.boundary().push_event(advertisement(peer_id));
    wait_peer(central, peer_id).await;
    central
        .boundary()
        .set_services(peer_id, vec![hrm_service()]);
    let handle = central
        .connect(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .discover(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("discover");
    handle.peer_key
}

fn count(central: &DesktopCentral<FakeRadio>, call: &str) -> usize {
    central
        .boundary()
        .calls()
        .iter()
        .filter(|recorded| *recorded == call)
        .count()
}

fn disables(central: &DesktopCentral<FakeRadio>) -> usize {
    // Every enable records an epoch; the rest of the set_notifications
    // calls are disables.
    count(central, "set_notifications") - central.boundary().enable_epochs().len()
}

fn state_of(records: &[ubm_desktop::PeerRecord], peer_id: &str) -> Option<ConnectionState> {
    records
        .iter()
        .find(|record| record.peer_id == peer_id)
        .and_then(|record| record.connection_state)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_scan_stop_keeps_the_scan_and_the_retry_reaches_the_radio() {
    let central = open().await;
    let session = central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("start");
    central
        .boundary()
        .fail_next(FaultOp::StopScan, "os refused");
    let first = central
        .stop_scan(session.operation_id(), OpControl::budget_ms(5000))
        .await
        .expect_err("first stop fails");
    assert_eq!(
        first.code_str(),
        "scan.stop-failed",
        "truthful first failure"
    );
    assert_eq!(
        central.active_scan_id().as_ref(),
        Some(session.operation_id())
    );
    assert!(central.resource_counters().await.scan_owned);
    assert!(central.boundary().scan_active(), "OS scan still running");
    let second = central
        .stop_scan(session.operation_id(), OpControl::budget_ms(5000))
        .await
        .expect("retry succeeds");
    assert_eq!(second, ScanStop::Stopped);
    assert_eq!(count(&central, "stop_scan"), 2, "two native stops");
    assert!(!central.has_active_scan().await);
    assert!(!central.boundary().scan_active());
}

#[tokio::test(start_paused = true)]
async fn hung_scan_stop_is_bounded_and_retained() {
    let central = open().await;
    let session = central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("start");
    central.boundary().block_op(FaultOp::StopScan);
    let error = central
        .stop_scan(session.operation_id(), OpControl::budget_ms(100))
        .await
        .expect_err("hung stop is bounded");
    assert_eq!(error.code_str(), "operation.timed-out");
    assert_eq!(error.retryability(), Retryability::CallerDecides);
    assert_eq!(
        central.active_scan_id().as_ref(),
        Some(session.operation_id())
    );
    central.boundary().unblock_op(FaultOp::StopScan);
    let stopped = central
        .stop_scan(session.operation_id(), OpControl::budget_ms(5000))
        .await
        .expect("retry");
    assert_eq!(stopped, ScanStop::Stopped);
    assert!(!central.has_active_scan().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_scan_stops_share_one_radio_call() {
    let central = open().await;
    let session = central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("start");
    central.boundary().block_op(FaultOp::StopScan);
    let id = session.operation_id().clone();
    let first = tokio::spawn({
        let central = central.clone();
        let id = id.clone();
        async move { central.stop_scan(&id, OpControl::budget_ms(5000)).await }
    });
    until(
        || count(&central, "stop_scan") == 1,
        "leader reached the radio",
    )
    .await;
    let second = tokio::spawn({
        let central = central.clone();
        let id = id.clone();
        async move { central.stop_scan(&id, OpControl::budget_ms(5000)).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    central.boundary().unblock_op(FaultOp::StopScan);
    assert_eq!(
        first.await.expect("join").expect("leader"),
        ScanStop::Stopped
    );
    assert_eq!(
        second.await.expect("join").expect("follower"),
        ScanStop::Stopped
    );
    assert_eq!(
        count(&central, "stop_scan"),
        1,
        "one OS stop for both callers"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_for_a_foreign_scan_id_never_touches_the_radio() {
    let central = open().await;
    let first = central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("start");
    central
        .stop_scan(first.operation_id(), OpControl::budget_ms(5000))
        .await
        .expect("stop first");
    let second = central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("restart");
    let stops = count(&central, "stop_scan");
    let stale = central
        .stop_scan(first.operation_id(), OpControl::budget_ms(5000))
        .await
        .expect("stale stop");
    assert_eq!(
        stale,
        ScanStop::NotActive,
        "the old id no longer owns the radio"
    );
    assert_eq!(count(&central, "stop_scan"), stops, "no radio call");
    assert_eq!(
        central.active_scan_id().as_ref(),
        Some(second.operation_id())
    );
    assert!(
        central.boundary().scan_active(),
        "the newer scan keeps running"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_unsubscribe_keeps_delivery_and_the_retry_reaches_the_radio() {
    let central = open().await;
    ready_peer(&central, "peer-u").await;
    central
        .subscribe(
            "peer-u",
            &selector(),
            "consumer",
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("subscribe");
    central
        .boundary()
        .fail_next(FaultOp::Unsubscribe, "os refused");
    let error = central
        .unsubscribe(
            "peer-u",
            &selector(),
            "consumer",
            OpControl::budget_ms(5000),
        )
        .await
        .expect_err("first disable fails");
    assert_eq!(
        error.code_str(),
        "gatt.subscribe-failed",
        "truthful first failure"
    );
    assert_eq!(
        central.boundary().live_subscription_count(),
        1,
        "CCCD still live"
    );
    assert_eq!(central.resource_counters().await.pending_disables, 1);
    assert_eq!(
        central.resource_counters().await.routed_subscriptions,
        1,
        "routing stays until a confirmed release: the live CCCD keeps its route"
    );
    let disabled = central
        .unsubscribe(
            "peer-u",
            &selector(),
            "consumer",
            OpControl::budget_ms(5000),
        )
        .await
        .expect("retry disables");
    assert!(disabled, "the retry performs the physical disable");
    assert_eq!(disables(&central), 2, "two native disables");
    assert_eq!(central.boundary().live_subscription_count(), 0);
    let counters = central.resource_counters().await;
    assert_eq!(counters.pending_disables, 0);
    assert_eq!(
        counters.routed_subscriptions, 0,
        "route removed after the release"
    );
}

#[tokio::test(start_paused = true)]
async fn hung_unsubscribe_is_bounded_and_blocks_a_racing_enable() {
    let central = open().await;
    ready_peer(&central, "peer-h").await;
    central
        .subscribe(
            "peer-h",
            &selector(),
            "consumer",
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("subscribe");
    central.boundary().block_op(FaultOp::Unsubscribe);
    let error = central
        .unsubscribe("peer-h", &selector(), "consumer", OpControl::budget_ms(100))
        .await
        .expect_err("hung disable is bounded");
    assert_eq!(error.code_str(), "operation.timed-out");
    let resubscribe = central
        .subscribe(
            "peer-h",
            &selector(),
            "consumer-2",
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect_err("pending disable fails a new enable closed");
    assert_eq!(resubscribe.code_str(), "lifecycle.invalid-state");
    central.boundary().unblock_op(FaultOp::Unsubscribe);
    assert!(
        central
            .unsubscribe(
                "peer-h",
                &selector(),
                "consumer",
                OpControl::budget_ms(5000)
            )
            .await
            .expect("retry"),
        "retry completes the disable"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_disconnect_keeps_the_link_and_the_retry_reaches_the_radio() {
    let central = open().await;
    ready_peer(&central, "peer-d").await;
    let mut events = central.lifecycle_events();
    central
        .boundary()
        .fail_next(FaultOp::Disconnect, "os refused");
    let error = central
        .disconnect("peer-d", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect_err("first release fails");
    assert_eq!(
        error.code_str(),
        "connection.lost",
        "truthful first failure"
    );
    assert_eq!(
        state_of(&central.peer_records().await, "peer-d"),
        Some(ConnectionState::Disconnecting),
        "ownership retained"
    );
    assert!(central.boundary().link_connected("peer-d"), "link still up");
    // Another lease cannot drive the retained release.
    let foreign = central
        .disconnect("peer-d", "lease-z", OpControl::budget_ms(5000))
        .await
        .expect_err("foreign lease refused");
    assert_eq!(foreign.code_str(), "ownership.denied");
    assert_eq!(
        count(&central, "disconnect"),
        1,
        "no radio call for the foreign lease"
    );
    let released = central
        .disconnect("peer-d", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("retry releases");
    assert_eq!(released, LinkRelease::Released);
    assert_eq!(count(&central, "disconnect"), 2, "two native releases");
    assert!(!central.boundary().link_connected("peer-d"));
    assert_eq!(
        state_of(&central.peer_records().await, "peer-d"),
        Some(ConnectionState::Disconnected)
    );
    let event = events.try_recv().expect("one Released event");
    assert_eq!(
        event.kind,
        ubm_desktop::LifecycleKind::Released { requested: true }
    );
    assert!(events.try_recv().is_err(), "exactly one lifecycle event");
    let again = central
        .disconnect("peer-d", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("idempotent");
    assert_eq!(again, LinkRelease::AlreadyReleased);
    assert_eq!(count(&central, "disconnect"), 2, "no third radio call");
}

/// Finding 38: the destroy record reports the final truth. A disconnect
/// that failed and was then retried successfully leaves nothing
/// outstanding, so shutdown must report `Released` — not a stale
/// `ReleaseFailed` from the attempt the retry superseded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_record_is_released_after_a_successful_disconnect_retry() {
    let central = open().await;
    ready_peer(&central, "peer-r").await;
    central
        .boundary()
        .fail_next(FaultOp::Disconnect, "os refused");
    central
        .disconnect("peer-r", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect_err("first release fails");
    assert_eq!(
        central.resource_counters().await.core.disconnect_failures,
        1,
        "the failed attempt is retained while the link is unreleased"
    );
    central
        .disconnect("peer-r", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("retry releases");
    let report = central.shutdown().await;
    let record = report.record.expect("destroy drive succeeds");
    assert_eq!(
        record.state(),
        ubm_desktop::CleanupState::Released,
        "a won retry leaves a clean record"
    );
    assert!(record.failures().is_empty(), "no stale release failure");
}

/// Finding 38 counterpart: a release the retry never won still fails the
/// record, with its failure preserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_record_keeps_a_release_that_was_never_won() {
    let central = open().await;
    ready_peer(&central, "peer-f").await;
    central
        .boundary()
        .fail_next(FaultOp::Disconnect, "os refused");
    central
        .disconnect("peer-f", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect_err("release fails");
    // Shutdown's own release attempt fails too: nothing ever confirmed it.
    central
        .boundary()
        .fail_next(FaultOp::Disconnect, "os refused again");
    let report = central.shutdown().await;
    let record = report.record.expect("destroy drive succeeds");
    assert_eq!(
        record.state(),
        ubm_desktop::CleanupState::ReleaseFailed,
        "an unreleased link fails the record"
    );
    assert!(!record.failures().is_empty(), "the failure is preserved");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn core_lock_is_free_while_a_release_is_held() {
    let central = open().await;
    ready_peer(&central, "peer-l").await;
    ready_peer(&central, "peer-m").await;
    central.boundary().block_op(FaultOp::Disconnect);
    let pending = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .disconnect("peer-l", "lease-a", OpControl::budget_ms(10_000))
                .await
        }
    });
    until(|| count(&central, "disconnect") == 1, "release held").await;
    // Unrelated core work proceeds while the release is held (PR210-34).
    let read = tokio::time::timeout(
        Duration::from_secs(2),
        central.poll_notification("peer-m", &selector(), "nobody"),
    )
    .await
    .expect("core lock free during a held release")
    .expect("poll");
    assert_eq!(read, NotificationPoll::Closed);
    central.boundary().unblock_op(FaultOp::Disconnect);
    assert_eq!(
        pending.await.expect("join").expect("released"),
        LinkRelease::Released
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_reports_a_scan_stop_that_never_succeeded() {
    let central = open().await;
    central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("start");
    central
        .boundary()
        .fail_next(FaultOp::StopScan, "os refused");
    let report = central.shutdown().await;
    let failure = report
        .scan_stop_failure
        .expect("the failed final stop is reported");
    assert_eq!(failure.code_str(), "scan.stop-failed");
    let record = report.record.expect("destroy drive");
    assert_eq!(
        record.state(),
        ubm_core::ownership::CleanupState::ReleaseFailed,
        "the record names the unreleased scan instead of claiming a clean release"
    );
}
