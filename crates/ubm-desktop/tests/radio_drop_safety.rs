//! Off-context drop safety for the production radio (HOST-DESKTOP).
//!
//! HARDWARE-GATED: without a usable adapter the open fails loudly and each
//! test passes as skipped. With hardware, dropping a `BtleplugRadio`
//! outside any Tokio runtime — the V8-finalizer shape that aborted the
//! Node host process — must neither panic nor abort, with or without a
//! preceding `close`.

use ubm_desktop::{BtleplugRadio, RadioBoundary};

/// Open the production radio, or `None` (skip) without usable hardware.
/// Any other open failure is a loud test failure, never a silent skip.
async fn try_open_radio() -> Option<BtleplugRadio> {
    let handle = tokio::runtime::Handle::current();
    match BtleplugRadio::open(handle, None).await {
        Ok(radio) => Some(radio),
        Err(error) => {
            assert_eq!(
                error.code_str(),
                "adapter.unavailable",
                "open without hardware must fail loud as unavailable"
            );
            eprintln!("SKIP radio_drop_safety: no usable adapter");
            None
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_radio_drops_off_context() {
    let Some(radio) = try_open_radio().await else {
        return;
    };
    radio.close().await;
    let dropped = std::thread::spawn(move || drop(radio)).join();
    assert!(
        dropped.is_ok(),
        "dropping a closed radio off-context must not panic"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unclosed_radio_drops_off_context() {
    let Some(radio) = try_open_radio().await else {
        return;
    };
    let dropped = std::thread::spawn(move || drop(radio)).join();
    assert!(
        dropped.is_ok(),
        "dropping an unclosed radio off-context must not panic"
    );
}
