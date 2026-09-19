//! Detect the vendored btleplug patch set (`vendor/btleplug/UBM_PATCHES.md`).
//!
//! The patched copy publishes its patch list through its `links` metadata
//! (`DEP_BTLEPLUG_UBM_PATCHES`). The attribute-instance, adapter-state,
//! scan-policy, WinRT, stream-lag, passive-scan, name-pattern,
//! event-capacity, read/notify, platform-error, WinRT service-filter,
//! advertisement-report, BlueZ device-change, disconnect-lifecycle and
//! WinRT ATT-error patches (#6-#20) are
//! required: without them same-UUID GATT attributes collapse,
//! adapter loss states are unreadable, scan duplicates ignore the caller,
//! WinRT scans actively, BlueZ discovery ignores the name prefix and
//! Windows security refusals are indistinguishable from other protocol
//! errors, so a build that links crates.io btleplug fails here with the
//! reason instead of compiling a degraded radio. The
//! earlier patches keep their cfg gates.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(btleplug_ubm_write_length)");
    println!("cargo::rustc-check-cfg=cfg(btleplug_ubm_advertisement_extras)");
    println!("cargo::rustc-check-cfg=cfg(btleplug_ubm_bluez_session)");
    println!("cargo::rustc-check-cfg=cfg(btleplug_ubm_write_readiness)");
    println!("cargo::rustc-check-cfg=cfg(btleplug_ubm_winrt_scan_stopped)");
    println!("cargo::rerun-if-env-changed=DEP_BTLEPLUG_UBM_PATCHES");
    let patches = std::env::var("DEP_BTLEPLUG_UBM_PATCHES").unwrap_or_default();
    // The applied list, as linked, for diagnostics (`vendored_btleplug_patches`).
    println!("cargo::rustc-env=UBM_BTLEPLUG_PATCHES={patches}");
    let applied = |wanted: &str| patches.split(',').any(|patch| patch.trim() == wanted);
    if applied("corebluetooth-write-length") {
        println!("cargo::rustc-cfg=btleplug_ubm_write_length");
    }
    if applied("corebluetooth-advertisement-extras") {
        println!("cargo::rustc-cfg=btleplug_ubm_advertisement_extras");
    }
    if applied("bluez-session-bus") {
        println!("cargo::rustc-cfg=btleplug_ubm_bluez_session");
    }
    if applied("corebluetooth-write-readiness") {
        println!("cargo::rustc-cfg=btleplug_ubm_write_readiness");
    }
    if applied("winrt-scan-stopped") {
        println!("cargo::rustc-cfg=btleplug_ubm_winrt_scan_stopped");
    }
    let radio = std::env::var_os("CARGO_FEATURE_BTLEPLUG").is_some();
    let missing: Vec<&str> = REQUIRED_PATCHES
        .iter()
        .copied()
        .filter(|patch| !applied(patch))
        .collect();
    if radio && !missing.is_empty() {
        panic!(
            "ubm-desktop's btleplug radio needs the vendored btleplug (vendor/btleplug, \
             UBM_PATCHES.md) through [patch.crates-io]; this build links a btleplug without \
             {missing:?}"
        );
    }
}

/// Vendored patches the production radio cannot run without.
const REQUIRED_PATCHES: &[&str] = &[
    "attribute-instances",
    "central-state-detail",
    "scan-policy",
    "winrt-uncached-discovery",
    "winrt-cccd-mode",
    "winrt-adapter-by-id",
    "stream-lag-reported",
    "winrt-passive-scan",
    "bluez-name-pattern",
    "event-capacity",
    "corebluetooth-read-notify",
    "platform-errors",
    "winrt-service-filter",
    "advertisement-reports",
    "bluez-device-changes",
    "disconnect-lifecycle",
    "winrt-att-error",
];
