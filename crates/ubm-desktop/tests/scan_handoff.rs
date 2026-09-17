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

use ubm_desktop::{CompletionOutcome, DesktopCentral, FakeRadio, FaultOp};

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
        .start_scan("owner-a", &[], 5000)
        .await
        .expect("start scan");
    assert!(central.has_active_scan().await);
    central.stop_scan().await.expect("stop scan");
    assert!(!central.has_active_scan().await, "no owned scan after stop");
    assert!(
        !central.boundary().scan_active(),
        "radio scan off after stop"
    );
    central
        .start_scan("owner-a", &[], 5000)
        .await
        .expect("core released the scan: restart admitted");
    central.stop_scan().await.expect("final stop");
}

// R14b: stop issued while start is in flight wins deterministically.
#[tokio::test]
async fn r14b_stop_wins_over_inflight_start() {
    let central = open().await;
    central.boundary().block_op(FaultOp::StartScan);
    let racing = tokio::spawn({
        let central = central.clone();
        async move { central.start_scan("owner-a", &[], 5000).await }
    });
    wait_for_call(&central, "start_scan").await;

    central.stop_scan().await.expect("stop wins");
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
        .start_scan("owner-a", &[], 5000)
        .await
        .expect("scan owner free after stop wins");
    central.stop_scan().await.expect("final stop");
}

// R14c: shutdown racing an in-flight start leaves no observable scan.
#[tokio::test]
async fn r14c_shutdown_during_start_leaves_no_scan() {
    let central = open().await;
    central.boundary().block_op(FaultOp::StartScan);
    let racing = tokio::spawn({
        let central = central.clone();
        async move { central.start_scan("owner-a", &[], 5000).await }
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
        .start_scan("owner-a", &[], 5000)
        .await
        .expect("start scan");
    central.stop_scan().await.expect("stop scan");
    match central.cancel_operation(session.operation_id()).await {
        Ok(CompletionOutcome::DuplicateSuppressed { .. }) => {}
        other => panic!("late duplicate must suppress onto the settled terminal, got {other:?}"),
    }
}

// R15 error path: the same retention holds when the stop itself failed.
#[tokio::test]
async fn r15_late_cancel_after_failed_stop_maps_to_settled_terminal() {
    let central = open().await;
    let session = central
        .start_scan("owner-a", &[], 5000)
        .await
        .expect("start scan");
    central
        .boundary()
        .fail_next(FaultOp::StopScan, "os refused");
    let error = central.stop_scan().await.expect_err("stop fails");
    assert_eq!(error.code_str(), "scan.stop-failed");
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
        .start_scan("owner-a", &[], 5000)
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
    central.stop_scan().await.expect("late stop stays safe");
}
