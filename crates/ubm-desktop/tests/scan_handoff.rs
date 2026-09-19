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
use ubm_desktop::{CompletionOutcome, DesktopCentral, FakeRadio, FaultOp};

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
