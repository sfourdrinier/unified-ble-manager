//! Manager-lifetime separation (F14): closing one desktop central must not
//! close admission process-wide. Other managers keep working and new managers
//! can open. Process-executor shutdown stays an explicit process-owner step.
//!
//! Lives in its own test binary (separate process) so the explicit
//! process-shutdown latch at the end cannot refuse admission for other tests.

use ubm_desktop::{DesktopCentral, FakeRadio};

#[tokio::test]
async fn f14_close_one_central_leaves_others_operational() {
    let central_a = DesktopCentral::open(FakeRadio::new(), "host-a")
        .await
        .expect("open A");
    let central_b = DesktopCentral::open(FakeRadio::new(), "host-b")
        .await
        .expect("open B");
    central_a
        .start_scan("owner-a", &[], 5000)
        .await
        .expect("A scans");
    central_b
        .start_scan("owner-b", &[], 5000)
        .await
        .expect("B scans");

    central_a.shutdown().await;
    assert!(central_a.is_shut_down(), "A records its own shutdown");
    assert!(!central_b.is_shut_down(), "B untouched by A's shutdown");

    // B continues: stop and restart its own scan.
    central_b.stop_scan().await.expect("B stops");
    central_b
        .start_scan("owner-b", &[], 5000)
        .await
        .expect("B restarts after A closed");
    central_b.stop_scan().await.expect("B stops again");

    // C opens after A closed.
    let central_c = DesktopCentral::open(FakeRadio::new(), "host-c")
        .await
        .expect("C opens after A closed");
    central_c
        .start_scan("owner-c", &[], 5000)
        .await
        .expect("C scans");
    central_c.stop_scan().await.expect("C stops");

    // A stays closed: its own admission refuses.
    let error = central_a
        .start_scan("owner-a", &[], 5000)
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
