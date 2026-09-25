//! Scan handoff races (R14) and completed-ticket retention (R15).
//!
//! * R14a: `stop_scan` success must mean the core no longer believes a scan
//!   is live (regression pin for the stop ordering).
//! * R14b: a stop issued while a start is in flight must win
//!   deterministically: after both futures resolve, no scan session is
//!   observable and the radio is off.
//! * R14c: a shutdown racing an in-flight start must leave no observable
//!   scan session behind once the racing starter resolves.
//! * R15: scan completion/error paths release (reap) the kernel op; a late
//!   duplicate completion for that scan must still map to the genuine
//!   settled terminal instead of `argument.invalid`.

use std::time::Duration;

use ubm_desktop::OpControl;
use ubm_desktop::{
    AdapterPowerState, AdmissionPolicy, COMPLETED_SCAN_TICKET_CAPACITY, CompletionOutcome,
    DesktopCentral, FakeRadio, FaultOp, RadioEvent,
};

/// Stop whatever scan the central owns (`NotActive` when none is owned).
async fn stop_owned_scan<B: ubm_desktop::RadioBoundary>(
    central: &DesktopCentral<B>,
) -> Result<ubm_desktop::ScanStop, ubm_desktop::DesktopError> {
    match central.active_scan_id() {
        Some(id) => central.stop_scan(&id, OpControl::unbounded()).await,
        None => Ok(ubm_desktop::ScanStop::NotActive),
    }
}

async fn open() -> DesktopCentral<FakeRadio> {
    DesktopCentral::open(FakeRadio::new(), "test-host")
        .await
        .expect("open")
}

/// Wait until the fake radio records `call` (the racing driver reached the
/// radio), bounded so a wedged driver fails the test instead of hanging it.
async fn wait_for_call(central: &DesktopCentral<FakeRadio>, call: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if central.boundary().calls().iter().any(|c| c == call) {
            return;
        }
        if tokio::time::Instant::now() > deadline {
            panic!(
                "radio never reached {call}: {:?}",
                central.boundary().calls()
            );
        }
        tokio::task::yield_now().await;
    }
}

// R14a pin: stop success means the core released the scan — a new start is
// admitted (the core does not still believe scanning).
#[tokio::test]
async fn r14a_stop_success_releases_core_scan() {
    let central = open().await;
    central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("start scan");
    assert!(central.has_active_scan().await);
    stop_owned_scan(&central).await.expect("stop scan");
    assert!(!central.has_active_scan().await, "no owned scan after stop");
    assert!(
        !central.boundary().scan_active(),
        "radio scan off after stop"
    );
    central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("core released the scan: restart admitted");
    stop_owned_scan(&central).await.expect("final stop");
}

// R14b: stop issued while start is in flight wins deterministically.
#[tokio::test]
async fn r14b_stop_wins_over_inflight_start() {
    let central = open().await;
    central.boundary().block_op(FaultOp::StartScan);
    let racing = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .start_scan("owner-a", &[], OpControl::budget_ms(5000))
                .await
        }
    });
    wait_for_call(&central, "start_scan").await;

    stop_owned_scan(&central).await.expect("stop wins");
    assert_eq!(
        central
            .start_scan(
                "replacement-before-old-start-settles",
                &[],
                OpControl::budget_ms(5000)
            )
            .await
            .expect_err("the late starter still owns compensation")
            .code_str(),
        "scan.already-active"
    );
    central.boundary().unblock_op(FaultOp::StartScan);

    let racing = tokio::time::timeout(Duration::from_secs(10), racing)
        .await
        .expect("starter resolves")
        .expect("starter joins");
    assert_eq!(
        racing
            .expect_err("losing starter reports cancellation")
            .code_str(),
        "operation.aborted",
        "in-flight start loses to stop"
    );
    assert!(
        !central.has_active_scan().await,
        "no scan session observable after stop wins"
    );
    assert!(
        !central.boundary().scan_active(),
        "no orphan radio scan after stop wins"
    );
    central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("scan owner free after stop wins");
    stop_owned_scan(&central).await.expect("final stop");
}

// RC7-01: once the original start has resolved and its compensating OS stop
// has succeeded, the cancelled-start marker must not block this central.
#[tokio::test]
async fn successful_lost_start_compensation_allows_same_central_restart() {
    let central = open().await;
    central.boundary().block_op(FaultOp::StartScan);
    let starting = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .start_scan("first", &[], OpControl::budget_ms(50))
                .await
        }
    });
    wait_for_call(&central, "start_scan").await;
    let error = starting.await.unwrap().expect_err("first start expires");
    assert_eq!(error.code_str(), "operation.timed-out");
    assert!(!central.boundary().scan_active());
    central.boundary().unblock_op(FaultOp::StartScan);
    central
        .start_scan("replacement", &[], OpControl::budget_ms(5000))
        .await
        .expect("successful compensation releases the old identity");
    stop_owned_scan(&central).await.expect("replacement stop");
}

#[tokio::test]
async fn cancelled_start_with_successful_compensation_allows_same_central_restart() {
    let central = open().await;
    central.boundary().block_op(FaultOp::StartScan);
    let control = OpControl::budget_ms(5000);
    let ticket = control.ticket.clone();
    let starting = tokio::spawn({
        let central = central.clone();
        async move { central.start_scan("first", &[], control).await }
    });
    wait_for_call(&central, "start_scan").await;
    central
        .cancel(&ticket)
        .await
        .expect("cancel admitted start");
    let error = starting.await.unwrap().expect_err("start cancelled");
    assert_eq!(error.code_str(), "operation.aborted");
    central.boundary().unblock_op(FaultOp::StartScan);
    central
        .start_scan("replacement", &[], OpControl::budget_ms(5000))
        .await
        .expect("cancelled start compensation releases the old identity");
    stop_owned_scan(&central).await.expect("replacement stop");
}

// An explicit stop may finish while native start is still unresolved. Its
// marker protects against a late success, but a native refusal settles that
// obligation and must release the marker without another stop or shutdown.
#[tokio::test]
async fn early_stop_then_native_start_refusal_allows_same_central_restart() {
    let central = open().await;
    central.boundary().block_op(FaultOp::StartScan);
    central
        .boundary()
        .fail_next(FaultOp::StartScan, "native refused");
    let starting = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .start_scan("first", &[], OpControl::budget_ms(5000))
                .await
        }
    });
    wait_for_call(&central, "start_scan").await;
    stop_owned_scan(&central).await.expect("early stop");
    central.boundary().unblock_op(FaultOp::StartScan);
    let error = starting.await.unwrap().expect_err("native refusal");
    assert_eq!(error.code_str(), "scan.start-failed");
    central
        .start_scan("replacement", &[], OpControl::budget_ms(5000))
        .await
        .expect("native refusal retires cancelled-start identity");
    stop_owned_scan(&central).await.expect("replacement stop");
}

#[tokio::test]
async fn refused_early_stop_then_native_start_refusal_allows_restart() {
    let central = open().await;
    central.boundary().block_op(FaultOp::StartScan);
    central
        .boundary()
        .fail_next(FaultOp::StartScan, "native refused");
    let starting = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .start_scan("first", &[], OpControl::budget_ms(5000))
                .await
        }
    });
    wait_for_call(&central, "start_scan").await;
    central
        .boundary()
        .fail_next(FaultOp::StopScan, "early stop refused");
    stop_owned_scan(&central)
        .await
        .expect_err("early stop refusal");
    central.boundary().unblock_op(FaultOp::StartScan);
    let error = starting.await.unwrap().expect_err("native refusal");
    assert_eq!(error.code_str(), "scan.start-failed");
    central
        .start_scan("replacement", &[], OpControl::budget_ms(5000))
        .await
        .expect("native refusal retires an earlier failed stop");
    stop_owned_scan(&central).await.expect("replacement stop");
}

async fn native_start_refusal_while_early_stop_is_inflight(stop_fails: bool) {
    let central = open().await;
    central.boundary().block_op(FaultOp::StartScan);
    central.boundary().block_op(FaultOp::StopScan);
    central
        .boundary()
        .fail_next(FaultOp::StartScan, "native refused");
    let starting = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .start_scan("first", &[], OpControl::budget_ms(5000))
                .await
        }
    });
    wait_for_call(&central, "start_scan").await;
    if stop_fails {
        central
            .boundary()
            .fail_next(FaultOp::StopScan, "early stop refused");
    }
    let stopping = tokio::spawn({
        let central = central.clone();
        async move { stop_owned_scan(&central).await }
    });
    wait_for_call(&central, "stop_scan").await;
    central.boundary().unblock_op(FaultOp::StartScan);
    let error = starting.await.unwrap().expect_err("native refusal");
    assert_eq!(error.code_str(), "scan.start-failed");
    central.boundary().unblock_op(FaultOp::StopScan);
    let stop_answer = stopping.await.unwrap();
    if stop_fails {
        assert_eq!(
            stop_answer.expect_err("early stop refused").code_str(),
            "scan.stop-failed"
        );
    } else {
        stop_answer.expect("early stop completes");
    }
    central
        .start_scan("replacement", &[], OpControl::budget_ms(5000))
        .await
        .expect("resolved start and stop retire the identity");
    stop_owned_scan(&central).await.expect("replacement stop");
}

#[tokio::test]
async fn native_start_refusal_while_early_stop_is_inflight_allows_restart() {
    native_start_refusal_while_early_stop_is_inflight(false).await;
}

#[tokio::test]
async fn native_start_refusal_while_early_stop_fails_allows_restart() {
    native_start_refusal_while_early_stop_is_inflight(true).await;
}

// R14c: shutdown racing an in-flight start leaves no observable scan.
#[tokio::test]
async fn r14c_shutdown_during_start_leaves_no_scan() {
    let central = open().await;
    central.boundary().block_op(FaultOp::StartScan);
    let racing = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .start_scan("owner-a", &[], OpControl::budget_ms(5000))
                .await
        }
    });
    wait_for_call(&central, "start_scan").await;

    central.shutdown().await;
    assert!(central.is_shut_down(), "shutdown recorded");
    central.boundary().unblock_op(FaultOp::StartScan);

    let racing = tokio::time::timeout(Duration::from_secs(10), racing)
        .await
        .expect("starter resolves")
        .expect("starter joins");
    assert!(
        racing.is_err(),
        "racing starter never activates after shutdown"
    );
    assert!(
        !central.has_active_scan().await,
        "no scan session observable after shutdown"
    );
    assert!(
        !central.boundary().scan_active(),
        "no orphan radio scan after shutdown"
    );
}

// R15: late duplicate completion after a successful stop maps to the genuine
// settled terminal, not `argument.invalid`.
#[tokio::test]
async fn r15_late_cancel_after_stop_maps_to_settled_terminal() {
    let central = open().await;
    let session = central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("start scan");
    stop_owned_scan(&central).await.expect("stop scan");
    match central.cancel_operation(session.operation_id()).await {
        Ok(CompletionOutcome::DuplicateSuppressed { .. }) => {}
        other => panic!("late duplicate must suppress onto the settled terminal, got {other:?}"),
    }
}

// PR210-09 + R15 error path: a failed OS stop keeps the scan (the kernel op
// stays live and the marker stays), so the retry calls the OS again with the
// same id; once it succeeds, a late duplicate maps to the settled terminal.
#[tokio::test]
async fn r15_failed_stop_retains_scan_then_retry_settles_terminal() {
    let central = open().await;
    let session = central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("start scan");
    central
        .boundary()
        .fail_next(FaultOp::StopScan, "os refused");
    let error = central
        .stop_scan(session.operation_id(), OpControl::unbounded())
        .await
        .expect_err("stop fails");
    assert_eq!(error.code_str(), "scan.stop-failed");
    assert!(
        central.has_active_scan().await,
        "failed stop retains the scan"
    );
    assert_eq!(
        central.active_scan_id().as_ref(),
        Some(session.operation_id()),
        "same scan identity retained"
    );
    assert!(central.boundary().scan_active(), "the OS scan is still on");
    let stopped = central
        .stop_scan(session.operation_id(), OpControl::unbounded())
        .await
        .expect("retry reaches the OS again");
    assert_eq!(stopped, ubm_desktop::ScanStop::Stopped);
    assert_eq!(
        central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "stop_scan")
            .count(),
        2,
        "two native stops: the failure and the real retry"
    );
    assert!(!central.has_active_scan().await);
    assert!(!central.boundary().scan_active());
    match central.cancel_operation(session.operation_id()).await {
        Ok(CompletionOutcome::DuplicateSuppressed { .. }) => {}
        other => panic!("late duplicate must suppress onto the settled terminal, got {other:?}"),
    }
}

// R15: cancelling an already-aborted scan twice keeps reporting the genuine
// (aborted) terminal instead of `argument.invalid`.
#[tokio::test]
async fn r15_double_cancel_after_abort_maps_to_settled_terminal() {
    use ubm_core::contracts::OperationTerminalKind;

    let central = open().await;
    let session = central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("start scan");
    match central.cancel_operation(session.operation_id()).await {
        Ok(CompletionOutcome::Settled { kind, .. }) => {
            assert_eq!(kind, OperationTerminalKind::Aborted, "first cancel aborts");
        }
        other => panic!("first cancel must settle the live scan, got {other:?}"),
    }
    match central.cancel_operation(session.operation_id()).await {
        Ok(CompletionOutcome::DuplicateSuppressed { .. }) => {}
        other => panic!("second cancel must suppress onto aborted, got {other:?}"),
    }
    stop_owned_scan(&central)
        .await
        .expect("late stop stays safe");
}

// R5: completed scan tickets are only a duplicate-cleanup acknowledgement
// window, not central-lifetime history. The newest ticket must still suppress
// a duplicate cancel, while a ticket outside the documented window expires
// rather than retaining an unbounded record.
#[tokio::test]
async fn r5_completed_scan_tickets_are_bounded_without_losing_recent_duplicate_semantics() {
    let central = open().await;
    let mut first = None;
    let mut latest = None;
    for _ in 0..=COMPLETED_SCAN_TICKET_CAPACITY {
        let session = central
            .start_scan("owner-a", &[], OpControl::budget_ms(5000))
            .await
            .expect("start scan");
        first.get_or_insert_with(|| session.operation_id().clone());
        latest = Some(session.operation_id().clone());
        central
            .stop_scan(session.operation_id(), OpControl::budget_ms(5000))
            .await
            .expect("stop scan");
    }

    assert_eq!(
        central.resource_counters().await.retained_scan_tickets,
        COMPLETED_SCAN_TICKET_CAPACITY,
        "a long-lived central retains only the documented duplicate window"
    );
    match central
        .cancel_operation(latest.as_ref().expect("latest scan"))
        .await
    {
        Ok(CompletionOutcome::DuplicateSuppressed { .. }) => {}
        other => panic!("recent duplicate must preserve its terminal answer, got {other:?}"),
    }
    let expired = central
        .cancel_operation(first.as_ref().expect("first scan"))
        .await
        .expect_err("expired ticket reports its lifecycle, not an unknown argument");
    assert_eq!(expired.code_str(), "lifecycle.invalid-state");
    assert_eq!(
        expired.detail(),
        Some("scan duplicate acknowledgement window expired")
    );
}

// R5: a ticket from another central must never borrow this central's
// acknowledgement window merely because both kernels start their counters at
// zero. Foreign cleanup is rejected before it can affect this central's own
// completed scan or radio.
#[tokio::test]
async fn r5_foreign_scan_ticket_is_rejected_without_consuming_local_terminal() {
    let foreign = open().await;
    let local = open().await;
    let foreign_scan = foreign
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("foreign start");
    foreign
        .stop_scan(foreign_scan.operation_id(), OpControl::budget_ms(5000))
        .await
        .expect("foreign stop");
    let local_scan = local
        .start_scan("owner-b", &[], OpControl::budget_ms(5000))
        .await
        .expect("local start");
    let before_foreign_stop = local.boundary().calls();
    assert_eq!(
        local
            .stop_scan(foreign_scan.operation_id(), OpControl::budget_ms(5000))
            .await
            .expect("a non-owned scan id is idempotently inactive"),
        ubm_desktop::ScanStop::NotActive
    );
    assert_eq!(
        local.boundary().calls(),
        before_foreign_stop,
        "a foreign stop never reaches the local radio"
    );
    local
        .stop_scan(local_scan.operation_id(), OpControl::budget_ms(5000))
        .await
        .expect("local stop");

    let before = local.boundary().calls();
    let denied = local
        .cancel_operation(foreign_scan.operation_id())
        .await
        .expect_err("foreign operation id is not this central's cleanup handle");
    assert_eq!(denied.code_str(), "ownership.denied");
    assert_eq!(
        local.boundary().calls(),
        before,
        "foreign cancel reaches no radio"
    );
    match local.cancel_operation(local_scan.operation_id()).await {
        Ok(CompletionOutcome::DuplicateSuppressed { .. }) => {}
        other => panic!("local terminal remains intact, got {other:?}"),
    }
}

// R5: a namespace match is not proof that this central issued a handle. A
// forged future ordinal remains an unknown argument, never an expired cleanup
// ticket.
#[tokio::test]
async fn r5_future_local_namespace_ticket_is_not_misreported_as_expired() {
    use ubm_core::contracts::OperationId;

    let central = open().await;
    let session = central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("start");
    let issued = session.operation_id().as_str();
    let (prefix, _) = issued.rsplit_once('/').expect("opaque operation ordinal");
    let forged = OperationId::new(format!("{prefix}/999999")).expect("forged opaque shape");
    let error = central
        .cancel_operation(&forged)
        .await
        .expect_err("future ordinal was never issued");
    assert_eq!(error.code_str(), "argument.invalid");
    central
        .stop_scan(session.operation_id(), OpControl::budget_ms(5000))
        .await
        .expect("stop");
}

// R5: the bounded acknowledgement window belongs only to scans the core
// actually minted. Rewriting an ordinary operation's class to `scan` cannot
// manufacture an expired scan acknowledgement.
#[tokio::test]
async fn r5_forged_non_scan_operation_cannot_become_an_expired_scan_ticket() {
    use ubm_core::contracts::OperationId;

    let central = open().await;
    let session = central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("start");
    let (without_last_segment, _) = session
        .operation_id()
        .as_str()
        .rsplit_once('/')
        .expect("scan operation suffix");
    let (scan_prefix, ordinal) = without_last_segment
        .rsplit_once('/')
        .expect("scan operation ordinal");
    let non_scan = OperationId::new(format!(
        "{}/{}",
        scan_prefix.replacen("/scan/", "/op/", 1),
        ordinal
    ))
    .expect("ordinary-operation opaque shape");
    let forged_scan = OperationId::new(non_scan.as_str().replacen("/op/", "/scan/", 1))
        .expect("forged scan opaque shape");
    let error = central
        .cancel_operation(&forged_scan)
        .await
        .expect_err("a forged non-scan handle cannot use scan ticket retention");
    assert_eq!(error.code_str(), "argument.invalid");
    central
        .stop_scan(session.operation_id(), OpControl::budget_ms(5000))
        .await
        .expect("stop");
}

// R5: an adapter reset replaces the attachment generation but not this
// central's immutable operation namespace. The old ticket therefore remains
// an acknowledged local terminal and cannot stop the replacement scan.
#[tokio::test]
async fn r5_old_generation_scan_ticket_cannot_stop_a_replacement_scan() {
    let radio = FakeRadio::new();
    radio.set_os_policy(AdmissionPolicy::LifecycleOnly, true);
    let central = DesktopCentral::open(radio, "reset-host")
        .await
        .expect("open");
    let mut resets = central.adapter_reset_events();
    let old = central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("old scan");
    central
        .boundary()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOff));
    tokio::time::timeout(std::time::Duration::from_secs(5), resets.recv())
        .await
        .expect("reset arrives")
        .expect("reset event");
    central
        .boundary()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOn));
    let replacement = central
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("replacement scan");
    let stops = central
        .boundary()
        .calls()
        .iter()
        .filter(|call| *call == "stop_scan")
        .count();
    assert_eq!(
        central
            .stop_scan(old.operation_id(), OpControl::budget_ms(5000))
            .await
            .expect("old generation stop is harmless"),
        ubm_desktop::ScanStop::NotActive
    );
    assert_eq!(
        central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "stop_scan")
            .count(),
        stops,
        "the old generation never stops the replacement radio scan"
    );
    match central.cancel_operation(old.operation_id()).await {
        Ok(CompletionOutcome::DuplicateSuppressed { .. }) => {}
        other => panic!("old generation terminal stays local, got {other:?}"),
    }
    central
        .stop_scan(replacement.operation_id(), OpControl::budget_ms(5000))
        .await
        .expect("replacement stop");
}
