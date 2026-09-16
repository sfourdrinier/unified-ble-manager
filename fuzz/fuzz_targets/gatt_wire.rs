//! libFuzzer target: JNI GATT wire enqueue/drain parsing
//! (`bindings/jni` `gatt_queue` + session drain).
//!
//! Untrusted input: positional pipe-delimited `kind|arg|...` wire lines
//! from the Android/Kotlin host. The JNI crate's modules are private to
//! the cdylib (they need a JVM only at the outer JNI boundary), and both
//! sources depend solely on `ubm-core` / `ubm-fake-radio` / `std`, so this
//! target compiles the REAL decoder sources by path — no copies, no
//! visibility changes to production code. Each input enqueues up to 64
//! lines into a fresh session queue, then drains them through the real
//! session-owned central: enqueue validation, per-kind arity checks, hex
//! decoding, and core transitions are all exercised, and every outcome
//! must be a `Result` — never a panic.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../bindings/jni/src/core_backend.rs"]
mod core_backend;
#[path = "../../bindings/jni/src/gatt_queue.rs"]
mod gatt_queue;

use core_backend::{CoreSession, CONTRACT_REVISION};
use gatt_queue::enqueue_event;

fuzz_target!(|data: &[u8]| {
    if data.len() > 8192 {
        return;
    }
    let text = String::from_utf8_lossy(data);
    let Ok(mut session) = CoreSession::open(CONTRACT_REVISION) else {
        return;
    };
    for line in text.split('\n').take(64) {
        let _ = enqueue_event(&mut session.gatt_queue, line);
    }
    let _ = session.drain_gatt_events("fuzz-gatt-drain");
});
