//! libFuzzer target: staged synthetic-radio step lines
//! (`StagedDriver::run_step`, `crates/ubm-fake-radio`).
//!
//! Untrusted input: TCK corpus / JSON step programs crossing from hosts
//! (napi `staged_step`, wasm `staged_step_js`) as text. `run_step` must
//! answer every line with a loud staged observation — never panic, hang,
//! or skip silently. Lines after the first run through accumulated driver
//! state, so multi-line inputs also shake down stateful dispatch.

#![no_main]

use libfuzzer_sys::fuzz_target;
use ubm_fake_radio::StagedDriver;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(mut driver) = StagedDriver::open() else {
        return;
    };
    for line in text.split('\n').take(32) {
        let _ = driver.run_step(line);
    }
    let _ = driver.drain_log();
});
