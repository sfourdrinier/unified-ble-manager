//! libFuzzer target: host-string scalar decoders (`ubm_core::contracts` +
//! `ubm_core::central::canonical_uuid`).
//!
//! Untrusted input: decimal-string counters (`echo_counter`,
//! `drive_expire_sweep` `now_ms` across napi/wasm/JNI) governed by the
//! frozen TCK counter-vector grammar, and UUID strings resolved from
//! hosts. All three must fail closed — never panic on adversarial shapes
//! (empty, over-long digit runs, signs, prefixes, truncated UUIDs).

#![no_main]

use libfuzzer_sys::fuzz_target;
use ubm_core::{central, contracts};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if text.len() > 256 {
        return;
    }
    let _ = contracts::parse_u64_decimal(text);
    let _ = contracts::parse_i64_decimal(text);
    let _ = central::canonical_uuid(text);
});
