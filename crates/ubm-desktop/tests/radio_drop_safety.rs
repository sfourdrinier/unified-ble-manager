//! Off-context drop safety for the production radio (HOST-DESKTOP).
//!
//! HARDWARE-GATED (PR210-26): these tests are `#[ignore]`d, so a runner
//! without Bluetooth reports them as ignored, never as passed. Run them on
//! a host with a usable adapter (on macOS, from a terminal with Bluetooth
//! authorization):
//!
//! ```sh
//! cargo test -p ubm-desktop --test radio_drop_safety -- --ignored
//! ```
//!
//! Under `--ignored` a missing adapter FAILS the test: an executed run is
//! physical evidence, a skipped one is not. With hardware, dropping a
//! `BtleplugRadio` outside any Tokio runtime — the V8-finalizer shape that
//! aborted the Node host process — must neither panic nor abort, with or
//! without a preceding `close`, and must not take the contended drop path.

use ubm_desktop::btleplug_backend::contended_radio_drops;
use ubm_desktop::{BtleplugRadio, RadioBoundary};

const HARDWARE: &str = "hardware: cargo test -p ubm-desktop --test radio_drop_safety -- --ignored";

/// Open the production radio. A missing or unusable adapter fails the test.
async fn open_radio() -> BtleplugRadio {
    let handle = tokio::runtime::Handle::current();
    match BtleplugRadio::open(handle, None).await {
        Ok(radio) => radio,
        Err(error) => panic!(
            "radio_drop_safety needs a usable Bluetooth adapter ({HARDWARE}); open failed: {} ({})",
            error.code_str(),
            error.detail().unwrap_or("no detail")
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "hardware: cargo test -p ubm-desktop --test radio_drop_safety -- --ignored"]
async fn closed_radio_drops_off_context() {
    let radio = open_radio().await;
    radio.close().await;
    let contended_before = contended_radio_drops();
    let dropped = std::thread::spawn(move || drop(radio)).join();
    assert!(
        dropped.is_ok(),
        "dropping a closed radio off-context must not panic"
    );
    assert_eq!(
        contended_radio_drops(),
        contended_before,
        "no contended drop"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "hardware: cargo test -p ubm-desktop --test radio_drop_safety -- --ignored"]
async fn unclosed_radio_drops_off_context() {
    let radio = open_radio().await;
    let contended_before = contended_radio_drops();
    let dropped = std::thread::spawn(move || drop(radio)).join();
    assert!(
        dropped.is_ok(),
        "dropping an unclosed radio off-context must not panic"
    );
    assert_eq!(
        contended_radio_drops(),
        contended_before,
        "no contended drop"
    );
}
