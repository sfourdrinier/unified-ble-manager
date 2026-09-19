//! UBM vendored-patch marker: publishes the applied patch set to direct
//! dependents as `DEP_BTLEPLUG_UBM_PATCHES` (see UBM_PATCHES.md), so they
//! can compile against the patched API only when this copy is in use.
fn main() {
    println!("cargo:patches=corebluetooth-write-length,corebluetooth-advertisement-extras,bluez-session-bus,corebluetooth-write-readiness,winrt-scan-stopped,attribute-instances,central-state-detail,scan-policy,winrt-uncached-discovery,winrt-cccd-mode,winrt-adapter-by-id,stream-lag-reported,winrt-passive-scan,bluez-name-pattern,event-capacity,corebluetooth-read-notify,platform-errors,winrt-service-filter,advertisement-reports,bluez-device-changes,disconnect-lifecycle,winrt-att-error");
}
