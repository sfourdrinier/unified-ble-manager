//! libFuzzer target: SIG profile characteristic decoders (`ubm_core::profiles`).
//!
//! Untrusted input: raw GATT characteristic bytes delivered by the radio or
//! replayed by a host. Every decoder must fail closed with
//! `ProfileCodecError` — never panic, wrap, or read out of bounds. Offset
//! variants cover the positional IEEE-11073 / date-time readers at their
//! boundary offsets.

#![no_main]

use libfuzzer_sys::fuzz_target;
use ubm_core::profiles;

fuzz_target!(|data: &[u8]| {
    let _ = profiles::parse_battery_level(data);
    let _ = profiles::parse_heart_rate_measurement(data);
    let _ = profiles::parse_body_sensor_location(data);
    let _ = profiles::parse_temperature_measurement(data);
    let _ = profiles::parse_blood_pressure_measurement(data);
    let _ = profiles::decode_device_information_string(data);
    let _ = profiles::parse_system_id(data);
    let _ = profiles::parse_pnp_id(data);
    for offset in [
        0usize,
        data.len() / 2,
        data.len(),
        data.len().saturating_add(1),
    ] {
        let _ = profiles::decode_ieee11073_float(data, offset);
        let _ = profiles::decode_ieee11073_sfloat(data, offset);
        let _ = profiles::decode_bluetooth_date_time(data, offset, "fuzz-date-time");
    }
});
