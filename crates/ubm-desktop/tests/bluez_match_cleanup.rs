//! Run the exact vendored server-match lease state machine on every host,
//! without a D-Bus daemon or radio. The Linux build checks its D-Bus adapter.
#[path = "../../../vendor/bluez-async/src/match_cleanup.rs"]
mod match_cleanup;

#[test]
fn owned_match_cleanup_is_a_required_production_vendor_patch() {
    assert!(include_str!("../build.rs").contains("\"bluez-match-cleanup\""));
    assert!(include_str!("../../../vendor/btleplug/build.rs").contains("bluez-match-cleanup"));
}
