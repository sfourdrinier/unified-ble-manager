//! Operation control acceptance (PR210-04/05/06/22/25) over the scripted
//! radio: exact cancellation by ticket, caller budgets as the only deadline
//! (liveness backstops only without one), retryability from the settled
//! outcome, and no global stall behind one peer's held radio call.
//!
//! Every assertion reads delivered data or core state, never handles alone.
//! Timing tests run on a paused clock; concurrency tests on a multi-thread
//! runtime.

use std::time::Duration;

use ubm_core::contracts::CommitState;
use ubm_desktop::{
    Budget, CancelAck, CharacteristicSnapshot, ConnectionState, DesktopCentral, FakeRadio, FaultOp,
    LIVENESS_BACKSTOP_DETAIL, LIVENESS_OP, NotificationPoll, OpControl, OpTicket, PeerSnapshot,
    PropertyFlags, RadioEvent, Retryability, ServiceSnapshot,
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
                write: true,
                write_without_response: true,
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

fn notification(peer_id: &str, epoch: u64, value: Vec<u8>) -> RadioEvent {
    RadioEvent::Notification {
        peer_id: peer_id.to_owned(),
        service_uuid: HRM_SERVICE.to_owned(),
        service_occurrence: 0,
        characteristic_uuid: HRM_MEASUREMENT.to_owned(),
        characteristic_occurrence: 0,
        epoch,
        value,
    }
}

async fn open() -> DesktopCentral<FakeRadio> {
    DesktopCentral::open(FakeRadio::new(), "op-control-host")
        .await
        .expect("open")
}

/// Yield until `probe` holds, bounded in real time so a wedge fails the test.
async fn until(mut probe: impl FnMut() -> bool, what: &str) {
    for _ in 0..2000 {
        if probe() {
            return;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for {what}");
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

/// Connected, discovered peer with a measured MTU.
async fn ready_peer(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
    central.boundary().push_event(advertisement(peer_id));
    wait_peer(central, peer_id).await;
    central
        .boundary()
        .set_services(peer_id, vec![hrm_service()]);
    central.boundary().set_mtu(peer_id, 185);
    central
        .connect(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .discover(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("discover");
}

fn count(central: &DesktopCentral<FakeRadio>, call: &str) -> usize {
    central
        .boundary()
        .calls()
        .iter()
        .filter(|recorded| *recorded == call)
        .count()
}

// PR210-05: cancel one of two outstanding writes — only its exact core op
// aborts, the other completes, and no second write is ever issued.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_targets_exactly_one_of_two_dispatched_writes() {
    let central = open().await;
    ready_peer(&central, "peer-w").await;
    let live_before = central.resource_counters().await.core.live_operations;
    central.boundary().block_op(FaultOp::Write);
    let first = OpControl::budget_ms(5000);
    let second = OpControl::budget_ms(5000);
    let first_ticket = first.ticket.clone();
    let second_ticket = second.ticket.clone();
    let a = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .write("peer-w", &selector(), vec![1], "with-response", first)
                .await
        }
    });
    let b = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .write("peer-w", &selector(), vec![2], "with-response", second)
                .await
        }
    });
    until(
        || count(&central, "write_characteristic") == 2,
        "both writes dispatched",
    )
    .await;
    let first_id = first_ticket.operation_id().expect("first published");
    let second_id = second_ticket.operation_id().expect("second published");
    assert_ne!(first_id, second_id, "each write has its own core op");

    let ack = central.cancel(&first_ticket).await.expect("cancel");
    match ack {
        CancelAck::Forwarded { operation, .. } => assert_eq!(operation, first_id),
        other => panic!("cancel must reach the published op, got {other:?}"),
    }
    let cancelled = a.await.expect("join a").expect_err("first aborted");
    assert_eq!(cancelled.code_str(), "operation.aborted");
    assert_eq!(
        cancelled.retryability(),
        Retryability::Never,
        "a dispatched write may have committed: never retryable"
    );
    assert_eq!(cancelled.commit(), Some(CommitState::Unknown));
    assert!(
        !b.is_finished(),
        "the other write is untouched by the cancel"
    );

    central.boundary().unblock_op(FaultOp::Write);
    b.await.expect("join b").expect("second write completes");
    assert_eq!(
        central.boundary().writes().len(),
        1,
        "exactly one write reached the peer; the cancelled one was never replayed"
    );
    assert_eq!(
        count(&central, "write_characteristic"),
        2,
        "no second write"
    );
    let live_after = central.resource_counters().await.core.live_operations;
    assert_eq!(live_after, live_before, "both ops settled and released");
    assert_eq!(
        central.cancel(&second_ticket).await.expect("late cancel"),
        CancelAck::AlreadySettled
    );
}

// PR210-05: a cancel recorded before admission ends the op without any
// radio call and leaves no core op behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_before_admission_never_reaches_the_radio() {
    let central = open().await;
    central.boundary().push_event(advertisement("peer-c"));
    wait_peer(&central, "peer-c").await;
    let ctl = OpControl::budget_ms(5000);
    assert_eq!(
        central.cancel(&ctl.ticket).await.expect("cancel"),
        CancelAck::RecordedBeforeAdmission
    );
    let error = central
        .connect("peer-c", "lease-a", ctl)
        .await
        .expect_err("cancelled before admission");
    assert_eq!(error.code_str(), "operation.aborted");
    assert_eq!(error.commit(), Some(CommitState::NotDispatched));
    assert_eq!(error.retryability(), Retryability::CallerDecides);
    assert_eq!(count(&central, "connect"), 0, "no radio call");
    assert_eq!(central.resource_counters().await.core.live_operations, 0);
    let peer_key = central.peer_key_for("peer-c").await.expect("peer");
    // The peer is still free: a fresh connect succeeds.
    central
        .connect("peer-c", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("fresh connect after the refused one");
    let records = central.peer_records().await;
    let record = records
        .iter()
        .find(|record| record.peer_key == peer_key)
        .expect("record");
    assert_eq!(record.connection_state, Some(ConnectionState::Connected));
}

// PR210-05: a connect that succeeds just before the cancel returns its
// handle; the cancel finds the op settled and nothing is left without an
// owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_success_before_cancel_keeps_the_handle_and_the_lease() {
    let central = open().await;
    central.boundary().push_event(advertisement("peer-s"));
    wait_peer(&central, "peer-s").await;
    let ctl = OpControl::budget_ms(5000);
    let ticket = ctl.ticket.clone();
    let handle = central
        .connect("peer-s", "lease-a", ctl)
        .await
        .expect("connect succeeds");
    assert_eq!(
        central.cancel(&ticket).await.expect("late cancel"),
        CancelAck::AlreadySettled
    );
    assert!(central.boundary().link_connected("peer-s"), "link kept");
    let records = central.peer_records().await;
    let record = records
        .iter()
        .find(|record| record.peer_id == "peer-s")
        .expect("record");
    assert_eq!(record.connection_state, Some(ConnectionState::Connected));
    assert_eq!(record.connection_generation, handle.connection_generation);
    assert_eq!(
        count(&central, "disconnect"),
        0,
        "no compensation for a won connect"
    );
}

// PR210-05: cancelling a connect in flight compensates the half-open link
// and reports a caller-retryable abort.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_during_connect_releases_the_half_open_link() {
    let central = open().await;
    central.boundary().push_event(advertisement("peer-h"));
    wait_peer(&central, "peer-h").await;
    central.boundary().block_op(FaultOp::Connect);
    let ctl = OpControl::budget_ms(5000);
    let ticket = ctl.ticket.clone();
    let pending = tokio::spawn({
        let central = central.clone();
        async move { central.connect("peer-h", "lease-a", ctl).await }
    });
    until(|| count(&central, "connect") == 1, "connect dispatched").await;
    central.cancel(&ticket).await.expect("cancel");
    let error = pending.await.expect("join").expect_err("aborted");
    assert_eq!(error.code_str(), "operation.aborted");
    assert_eq!(error.retryability(), Retryability::CallerDecides);
    assert_eq!(count(&central, "disconnect"), 1, "half-open link released");
    let records = central.peer_records().await;
    let record = records
        .iter()
        .find(|record| record.peer_id == "peer-h")
        .expect("record");
    assert_eq!(
        record.connection_state, None,
        "a cancelled pending connect leaves no connection record or lease behind"
    );
    assert_eq!(central.resource_counters().await.core.live_operations, 0);
    central.boundary().unblock_op(FaultOp::Connect);
    central
        .connect("peer-h", "lease-b", OpControl::budget_ms(5000))
        .await
        .expect("the peer is free for a new owner");
}

// PR210-06: the caller budget is the deadline — a short one times out a
// held read, reported as the caller's (no backstop detail).
#[tokio::test(start_paused = true)]
async fn short_budget_times_out_a_held_read() {
    let central = open().await;
    ready_peer(&central, "peer-r").await;
    central.boundary().block_op(FaultOp::Read);
    let error = central
        .read("peer-r", &selector(), OpControl::budget_ms(50))
        .await
        .expect_err("budget expires");
    assert_eq!(error.code_str(), "operation.timed-out");
    assert_eq!(error.detail(), None, "caller budget, not a backstop");
    assert_eq!(error.retryability(), Retryability::CallerDecides);
    central.boundary().unblock_op(FaultOp::Read);
}

// PR210-06: a caller budget above the old fixed 30 s backstop is honoured.
#[tokio::test(start_paused = true)]
async fn long_budget_above_thirty_seconds_is_honoured() {
    let central = open().await;
    ready_peer(&central, "peer-l").await;
    central.boundary().block_op(FaultOp::Read);
    let pending = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .read("peer-l", &selector(), OpControl::budget_ms(45_000))
                .await
        }
    });
    tokio::time::sleep(Duration::from_secs(40)).await;
    assert!(
        !pending.is_finished(),
        "still inside its 45 s budget at 40 s"
    );
    central.boundary().unblock_op(FaultOp::Read);
    let value = pending.await.expect("join").expect("read succeeds at 40 s");
    assert_eq!(value.value, vec![0x42]);
}

// PR210-06: without a caller budget the liveness backstop decides, and says
// so in the detail.
#[tokio::test(start_paused = true)]
async fn backstop_applies_only_without_a_budget() {
    let central = open().await;
    ready_peer(&central, "peer-b").await;
    central.boundary().block_op(FaultOp::Read);
    let started = tokio::time::Instant::now();
    let error = central
        .read("peer-b", &selector(), OpControl::unbounded())
        .await
        .expect_err("backstop fires");
    assert_eq!(error.code_str(), "operation.timed-out");
    assert_eq!(error.detail(), Some(LIVENESS_BACKSTOP_DETAIL));
    assert!(
        started.elapsed() >= LIVENESS_OP,
        "fires at the backstop, not earlier"
    );
    central.boundary().unblock_op(FaultOp::Read);
}

// PR210-06: a spent budget makes no radio call and admits nothing.
#[tokio::test(start_paused = true)]
async fn zero_budget_makes_no_radio_call() {
    let central = open().await;
    ready_peer(&central, "peer-z").await;
    let reads_before = count(&central, "read_characteristic");
    let live_before = central.resource_counters().await.core.live_operations;
    let error = central
        .read("peer-z", &selector(), OpControl::budget_ms(0))
        .await
        .expect_err("spent budget");
    assert_eq!(error.code_str(), "operation.timed-out");
    assert_eq!(error.commit(), Some(CommitState::NotDispatched));
    assert_eq!(error.retryability(), Retryability::CallerDecides);
    assert_eq!(count(&central, "read_characteristic"), reads_before);
    assert_eq!(
        central.resource_counters().await.core.live_operations,
        live_before
    );
}

// PR210-06: queueing time before the central sees the op counts against the
// budget (the budget starts when the host received the call).
#[tokio::test(start_paused = true)]
async fn queued_time_counts_against_the_budget() {
    let central = open().await;
    ready_peer(&central, "peer-q").await;
    let received = tokio::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let ctl = OpControl::new(Budget::from_ms_at(received, 50), OpTicket::new());
    let error = central
        .write("peer-q", &selector(), vec![1], "with-response", ctl)
        .await
        .expect_err("spent while queued");
    assert_eq!(error.code_str(), "operation.timed-out");
    assert_eq!(error.commit(), Some(CommitState::NotDispatched));
    assert_eq!(count(&central, "mtu"), 0, "not even the MTU lookup ran");
    assert!(central.boundary().writes().is_empty());
}

// PR210-06/22: a write that expires after dispatch is commit-unknown and
// never retryable; nothing replays it.
#[tokio::test(start_paused = true)]
async fn dispatched_write_expiry_is_never_retryable() {
    let central = open().await;
    ready_peer(&central, "peer-x").await;
    central.boundary().block_op(FaultOp::Write);
    let error = central
        .write(
            "peer-x",
            &selector(),
            vec![9],
            "with-response",
            OpControl::budget_ms(50),
        )
        .await
        .expect_err("expires after dispatch");
    assert_eq!(error.code_str(), "operation.timed-out");
    assert_eq!(error.commit(), Some(CommitState::Unknown));
    assert_eq!(error.retryability(), Retryability::Never);
    assert_eq!(
        count(&central, "write_characteristic"),
        1,
        "one dispatch, no replay"
    );
    central.boundary().unblock_op(FaultOp::Write);
}

// PR210-04: discovery is bounded by the budget; an expiry fails the
// discovery in the core so a later discovery can run.
#[tokio::test(start_paused = true)]
async fn discovery_is_bounded_and_retryable() {
    let central = open().await;
    central.boundary().push_event(advertisement("peer-d"));
    wait_peer(&central, "peer-d").await;
    central
        .boundary()
        .set_services("peer-d", vec![hrm_service()]);
    central
        .connect("peer-d", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central.boundary().block_op(FaultOp::Discover);
    let error = central
        .discover("peer-d", "lease-a", OpControl::budget_ms(100))
        .await
        .expect_err("held discovery expires");
    assert_eq!(error.code_str(), "operation.timed-out");
    assert_eq!(error.retryability(), Retryability::CallerDecides);
    central.boundary().unblock_op(FaultOp::Discover);
    let report = central
        .discover("peer-d", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("rediscovery after the expiry");
    assert!(report.paths_registered > 0);
}

// PR210-04/25: one peer's held discovery neither stalls another peer's
// notification delivery nor blocks a cancel or the central's shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_discovery_on_one_peer_never_stalls_another() {
    let central = open().await;
    ready_peer(&central, "peer-b").await;
    central
        .subscribe(
            "peer-b",
            &selector(),
            "consumer",
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("subscribe b");
    let epoch = central
        .boundary()
        .enable_epochs()
        .last()
        .map(|(_, epoch)| *epoch)
        .expect("enable epoch");

    central.boundary().push_event(advertisement("peer-a"));
    wait_peer(&central, "peer-a").await;
    central
        .connect("peer-a", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect a");
    central.boundary().block_op(FaultOp::Discover);
    let discover_ctl = OpControl::unbounded();
    let discover_ticket = discover_ctl.ticket.clone();
    let held = tokio::spawn({
        let central = central.clone();
        async move { central.discover("peer-a", "lease-a", discover_ctl).await }
    });
    until(|| count(&central, "discover") >= 2, "peer-a discovery held").await;

    for value in 0..50u8 {
        central
            .boundary()
            .push_event(notification("peer-b", epoch, vec![value]));
    }
    let started = std::time::Instant::now();
    let mut delivered = Vec::new();
    while delivered.len() < 50 {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "peer-b stalled behind peer-a: {} of 50 delivered",
            delivered.len()
        );
        match central
            .poll_notification("peer-b", &selector(), "consumer")
            .await
            .expect("poll b")
        {
            NotificationPoll::Value(value) => delivered.push(value[0]),
            NotificationPoll::Empty => tokio::task::yield_now().await,
            other => panic!("unexpected poll outcome {other:?}"),
        }
    }
    assert_eq!(
        delivered,
        (0..50u8).collect::<Vec<_>>(),
        "all 50 values, in order"
    );
    assert!(!held.is_finished(), "peer-a is still held");

    central.cancel(&discover_ticket).await.expect("cancel a");
    let error = tokio::time::timeout(Duration::from_secs(5), held)
        .await
        .expect("cancel ends the held discovery before the radio answers")
        .expect("join")
        .expect_err("aborted");
    assert_eq!(error.code_str(), "operation.aborted");

    let second = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .discover("peer-a", "lease-a", OpControl::unbounded())
                .await
        }
    });
    until(|| count(&central, "discover") >= 3, "second discovery held").await;
    let report = tokio::time::timeout(Duration::from_secs(10), central.shutdown())
        .await
        .expect("shutdown completes while a discovery is held");
    assert!(report.record.is_ok());
    central.boundary().unblock_op(FaultOp::Discover);
    let _ = second.await;
}

// Finding 112: a connect without a caller budget waits as long as the OS
// does, as the legacy backends did (a pending CoreBluetooth connect, an
// Android autoConnect; docs/PEERS.md). No liveness backstop ends it; the
// caller's cancel still does, and a later OS answer still lands.
#[tokio::test(start_paused = true)]
async fn a_connect_without_a_budget_waits_until_the_os_answers_or_the_caller_cancels() {
    let central = open().await;
    for peer in ["peer-w", "peer-c"] {
        central.boundary().push_event(advertisement(peer));
        wait_peer(&central, peer).await;
    }
    central.boundary().block_op(FaultOp::Connect);
    let answered = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .connect("peer-w", "lease-a", OpControl::unbounded())
                .await
        }
    });
    tokio::time::sleep(LIVENESS_OP * 10).await;
    assert!(!answered.is_finished(), "no backstop ends the wait");
    central.boundary().unblock_op(FaultOp::Connect);
    answered
        .await
        .expect("join")
        .expect("the OS answer lands however late");

    central.boundary().block_op(FaultOp::Connect);
    let ctl = OpControl::unbounded();
    let ticket = ctl.ticket.clone();
    let cancelled = tokio::spawn({
        let central = central.clone();
        async move { central.connect("peer-c", "lease-a", ctl).await }
    });
    tokio::time::sleep(LIVENESS_OP * 10).await;
    assert!(!cancelled.is_finished(), "still waiting");
    central.cancel(&ticket).await.expect("cancel");
    assert_eq!(
        cancelled
            .await
            .expect("join")
            .expect_err("cancelled")
            .code_str(),
        "operation.aborted"
    );
    central.boundary().unblock_op(FaultOp::Connect);
}

// Finding 123: pairing without a caller budget waits as long as the OS does,
// as every legacy host did (the user may sit in a passkey dialog). No
// liveness backstop ends it; a cancel of the pairing does.
#[tokio::test(start_paused = true)]
async fn pairing_without_a_budget_waits_for_the_os_and_its_cancel() {
    let central = open().await;
    ready_peer(&central, "peer-s").await;
    central.boundary().block_op(FaultOp::Pair);
    central.boundary().block_op(FaultOp::CancelPairing);
    let pairing = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .pair(
                    "peer-s",
                    ubm_desktop::PairRequest::default(),
                    OpControl::unbounded(),
                )
                .await
        }
    });
    tokio::time::sleep(LIVENESS_OP * 10).await;
    assert!(!pairing.is_finished(), "no backstop ends the ceremony");
    let cancelling = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .cancel_pairing("peer-s", OpControl::unbounded())
                .await
        }
    });
    tokio::time::sleep(LIVENESS_OP * 10).await;
    assert!(
        !cancelling.is_finished(),
        "the OS cancel has no backstop either"
    );
    central.boundary().unblock_op(FaultOp::CancelPairing);
    assert_eq!(
        pairing.await.expect("join").expect("answered"),
        ubm_desktop::PairOutcome::Cancelled
    );
    assert_eq!(
        cancelling.await.expect("join").expect("answered"),
        ubm_desktop::CancelPairingOutcome::Cancelled
    );
}
