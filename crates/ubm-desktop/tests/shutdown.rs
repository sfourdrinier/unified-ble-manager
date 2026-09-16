//! Explicit-shutdown receipt for the desktop central (HOST-DESKTOP).
//!
//! Lives in its own test binary (separate process) because
//! [`DesktopCentral::shutdown`] records the process-global executor
//! shutdown latch: running it inside the lib test process would refuse
//! admission for every other test there. No radio is touched.

use ubm_desktop::{DesktopCentral, FakeRadio};

#[tokio::test]
async fn shutdown_stops_scan_and_refuses_new_work() {
    let central = DesktopCentral::open(FakeRadio::new(), "test-host")
        .await
        .expect("open");
    central
        .start_scan("owner-a", &[], 5000)
        .await
        .expect("start scan");
    assert!(central.has_active_scan().await);

    central.shutdown().await;

    assert!(central.is_shut_down(), "shutdown recorded");
    assert!(
        !central.has_active_scan().await,
        "owned scan cleaned up by shutdown"
    );
    assert!(
        !central.boundary().scan_active(),
        "OS scan stopped by shutdown"
    );
    let error = central
        .start_scan("owner-a", &[], 5000)
        .await
        .expect_err("no admission after shutdown");
    assert_eq!(error.code_str(), "adapter.unavailable");
    let error = central
        .connect("peer-x", "lease-a", 5000)
        .await
        .expect_err("no admission after shutdown");
    assert_eq!(error.code_str(), "adapter.unavailable");

    // Idempotent: a second shutdown is safe cleanup, not a second record.
    central.shutdown().await;
    assert!(central.is_shut_down());
}
