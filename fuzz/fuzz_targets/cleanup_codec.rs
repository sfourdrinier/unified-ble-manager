//! libFuzzer target: `CleanupRecord` wire codec (`ubm_core::codec::decode`).
//!
//! Untrusted input: canonical cleanup wire JSON crossing any FFI/host
//! boundary as a string. Exercises fail-closed rejection of malformed
//! input plus the documented fixed point `decode(encode(x)) == encode(x)`:
//! a mismatch panics, so the fuzzer reports it as a real bug.

#![no_main]

use libfuzzer_sys::fuzz_target;
use ubm_core::codec;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(record) = codec::decode(text) else {
        return;
    };
    let wire = record.encode();
    let back = codec::decode(&wire).expect("re-encode of a decoded record must decode");
    assert_eq!(
        wire,
        back.encode(),
        "decode(encode(x)) must be a fixed point"
    );
});
