//! Manager-lifetime separation (F14): closing one desktop central must not
//! close admission process-wide. Other managers keep working and new managers
//! can open. Process-executor shutdown stays an explicit process-owner step.
//!
//! Lives in its own test binary (separate process) so the explicit
//! process-shutdown latch at the end cannot refuse admission for other tests.

use ubm_desktop::OpControl;
use ubm_desktop::{DesktopCentral, FakeRadio};

/// Stop whatever scan the central owns (`NotActive` when none is owned).
async fn stop_owned_scan<B: ubm_desktop::RadioBoundary>(
    central: &DesktopCentral<B>,
) -> Result<ubm_desktop::ScanStop, ubm_desktop::DesktopError> {
    match central.active_scan_id() {
        Some(id) => central.stop_scan(&id, OpControl::unbounded()).await,
        None => Ok(ubm_desktop::ScanStop::NotActive),
    }
}

#[tokio::test]
async fn f14_close_one_central_leaves_others_operational() {
    let central_a = DesktopCentral::open(FakeRadio::new(), "host-a")
        .await
        .expect("open A");
    let central_b = DesktopCentral::open(FakeRadio::new(), "host-b")
        .await
        .expect("open B");
    central_a
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect("A scans");
    central_b
        .start_scan("owner-b", &[], OpControl::budget_ms(5000))
        .await
        .expect("B scans");

    central_a.shutdown().await;
    assert!(central_a.is_shut_down(), "A records its own shutdown");
    assert!(!central_b.is_shut_down(), "B untouched by A's shutdown");

    // B continues: stop and restart its own scan.
    stop_owned_scan(&central_b).await.expect("B stops");
    central_b
        .start_scan("owner-b", &[], OpControl::budget_ms(5000))
        .await
        .expect("B restarts after A closed");
    stop_owned_scan(&central_b).await.expect("B stops again");

    // C opens after A closed.
    let central_c = DesktopCentral::open(FakeRadio::new(), "host-c")
        .await
        .expect("C opens after A closed");
    central_c
        .start_scan("owner-c", &[], OpControl::budget_ms(5000))
        .await
        .expect("C scans");
    stop_owned_scan(&central_c).await.expect("C stops");

    // A stays closed: its own admission refuses.
    let error = central_a
        .start_scan("owner-a", &[], OpControl::budget_ms(5000))
        .await
        .expect_err("A admits nothing after its own shutdown");
    assert_eq!(error.code_str(), "adapter.unavailable");

    // Explicit process shutdown (process-owner step) still closes admission
    // globally afterwards.
    ubm_desktop::executor::shutdown_desktop_runtime();
    match DesktopCentral::open(FakeRadio::new(), "host-d").await {
        Ok(_) => panic!("no opens after process shutdown"),
        Err(error) => assert_eq!(error.code_str(), "adapter.unavailable"),
    }
}
