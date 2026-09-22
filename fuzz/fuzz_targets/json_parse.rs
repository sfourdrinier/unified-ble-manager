//! libFuzzer target: minimal deterministic JSON seam
//! (`ubm_fake_radio::json::parse` + `parse_hex`).
//!
//! Untrusted input: staged-drive step text and scripted hex payloads from
//! hosts. The parser must fail closed with `JsonError` (never panic);
//! `parse_hex` must reject odd-length / non-hex input with `None`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use ubm_fake_radio::json;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = json::parse(text);
    let _ = json::parse_hex(text);
});
