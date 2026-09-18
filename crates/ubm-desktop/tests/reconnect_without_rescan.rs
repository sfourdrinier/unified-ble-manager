//! Reconnect without rescan (finding 127) against the production radio.
//!
//! The legacy backends reconnected without a new scan: CoreBluetooth via
//! `retrievePeripheralsWithIdentifiers`, WinRT by address
//! (`BluetoothLEDevice::FromBluetoothAddressAsync`), BlueZ against its own
//! object tree. The Rust path keeps that shape in
//! `BtleplugRadio::peripheral_by_id` (vendored patch #19): a peer the
//! adapter no longer lists is re-resolved by its OS identity, and only a
//! listing without the peer is `peer.not-found`.
//!
//! Evidence level: hardware. Opening the production radio needs a usable
//! Bluetooth adapter, so these run only on demand:
//! `cargo test -p ubm-desktop --test reconnect_without_rescan -- --ignored`.
//! The resolve identity itself (no radio) is pinned without hardware in
//! `btleplug_backend::tests::f127_a_listed_identity_resolves_without_a_scan`,
//! `os::winrt_model::tests::a_listed_address_reopens_without_a_scan`, and
//! `cargo test -p btleplug --lib ubm_instance`.

use ubm_core::contracts::BleErrorCode;
use ubm_desktop::{BtleplugRadio, RadioBoundary};

const HARDWARE: &str =
    "hardware: cargo test -p ubm-desktop --test reconnect_without_rescan -- --ignored";

/// A peer id no adapter has ever listed, in the shape each OS resolves by
/// (a CoreBluetooth identifier on Apple, a WinRT address on Windows; BlueZ
/// resolves peers itself, so any fixed id exercises its miss path).
#[cfg(target_vendor = "apple")]
const NEVER_SEEN_PEER: &str = "5e0b1c9a-6c0f-4f60-a1c1-3b5f2a0e7d11";
#[cfg(target_os = "windows")]
const NEVER_SEEN_PEER: &str = "AA:BB:CC:DD:EE:FF";
#[cfg(not(any(target_vendor = "apple", target_os = "windows")))]
const NEVER_SEEN_PEER: &str = "AA:BB:CC:DD:EE:FF";

/// Open the production radio. A missing or unusable adapter fails the test.
async fn open_radio() -> BtleplugRadio {
    let handle = tokio::runtime::Handle::current();
    match BtleplugRadio::open(handle, None).await {
        Ok(radio) => radio,
        Err(error) => panic!(
            "reconnect_without_rescan needs a usable Bluetooth adapter ({HARDWARE}); open failed: {} ({})",
            error.code_str(),
            error.detail().unwrap_or("no detail")
        ),
    }
}

/// A connect to a never-observed peer answers `peer.not-found` from the
/// resolve path — it never starts a scan waiting for the peer to appear.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "hardware: cargo test -p ubm-desktop --test reconnect_without_rescan -- --ignored"]
async fn a_never_observed_peer_is_not_found_without_a_scan() {
    let radio = open_radio().await;
    let error = radio
        .connect(NEVER_SEEN_PEER)
        .await
        .expect_err("a never-observed peer must not connect");
    assert_eq!(
        error.code(),
        BleErrorCode::PeerNotFound,
        "resolve miss answers peer.not-found (got {}: {})",
        error.code_str(),
        error.detail().unwrap_or(error.operation())
    );
    radio.close().await;
}
