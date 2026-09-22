//! Ergonomic JS mapping over the raw ABI (`js-glue` feature, `wasm-bindgen`).
//!
//! Type mapping (FFI-WASM card):
//! - bytes: `Uint8Array` in/out. wasm-bindgen copies across the boundary, so
//!   ownership matches the raw ABI: owned in, owned out, no retained views.
//! - u64 counters: decimal strings. The host passes `BigInt(n).toString()`
//!   and converts back with `BigInt(text)`: lossless to `2^64-1`.
//! - failures: rejected `JsValue` strings in the shared
//!   `code|domain|operation|detail` wire form (same parser as the raw ABI's
//!   `ubm_echo_last_error_text`).
//! - JSON: [`describe_json_js`] returns the bridge document as text for
//!   `JSON.parse`.
//!
//! Both surfaces share one init state via [`crate::raw_abi`] so the init
//! contract cannot disagree with itself.
//!
//! Proof strategy (why there are no in-process unit tests here):
//! `#[wasm_bindgen]` shims call `__wbindgen_placeholder__` imports and abort
//! outside a real module instance, so they are provable only at the wasm32
//! boundary. The runner asserts `cargo check --target
//! wasm32-unknown-unknown --features js-glue` plus the presence of the
//! mapped export names in the built module. All conversion logic lives in
//! `core_backend` and is unit-tested there; this module adds types only.

use crate::core_backend::{CoreBackend, EchoError};
use crate::raw_abi::with_core;
use wasm_bindgen::prelude::*;

fn err_to_js(err: EchoError) -> JsValue {
    JsValue::from_str(&err.wire_message())
}

/// A poisoned core lock (a prior panic while held) fails the JS call loudly,
/// never panics across the boundary.
fn lock_poisoned(operation: &'static str) -> JsValue {
    // Same wire shape as the raw ABI's `lock_failed`: the semantic code
    // travels in the name slot, the numeric code stays `InvalidState`.
    JsValue::from_str(
        &EchoError::new(
            crate::core_backend::EchoCode::InvalidState,
            "lifecycle.invariant-violation",
            "core",
            operation,
            "lock-poisoned",
        )
        .wire_message(),
    )
}

/// Initialises the binding (PKG-02 / WEB init contract). Foreign revisions
/// fail closed with `protocol.incompatible`.
#[wasm_bindgen(js_name = initContract)]
pub fn init_contract(revision: String) -> Result<(), JsValue> {
    match with_core(|core| core.init(&revision).map_err(err_to_js)) {
        Some(result) => result,
        None => Err(lock_poisoned("echo-init")),
    }
}

/// Owned byte-batch echo as `Uint8Array` round-trip.
#[wasm_bindgen(js_name = echoBytes)]
pub fn echo_bytes_js(input: &[u8]) -> Result<Vec<u8>, JsValue> {
    match with_core(|core| CoreBackend::echo_bytes(core, input, "echo-bytes").map_err(err_to_js)) {
        Some(result) => result,
        None => Err(lock_poisoned("echo-bytes")),
    }
}

/// Lossless u64 echo over decimal strings (`BigInt(n).toString()` in,
/// `BigInt(text)` out).
#[wasm_bindgen(js_name = echoCounterU64)]
pub fn echo_counter_js(decimal: String) -> Result<String, JsValue> {
    match with_core(|core| {
        CoreBackend::echo_counter(core, &decimal, "echo-counter").map_err(err_to_js)
    }) {
        Some(result) => result,
        None => Err(lock_poisoned("echo-counter")),
    }
}

/// JSON bridge document for `JSON.parse` (static metadata, no init needed).
#[wasm_bindgen(js_name = describeJson)]
pub fn describe_json_js() -> String {
    format!(
        "{{\"revision\":\"{}\",\"maxBytes\":{},\"u64max\":\"{}\"}}",
        crate::core_backend::CONTRACT_REVISION,
        crate::core_backend::MAX_OPERATION_BYTES,
        crate::core_backend::u64_max_decimal()
    )
}

/// Observes the session-owned transition core (U7): frozen revision plus
/// live kernel counters as a JSON document (same shape as the sibling
/// bindings). Fails closed before init.
#[wasm_bindgen(js_name = centralStatus)]
pub fn central_status_js() -> Result<String, JsValue> {
    match with_core(|core| core.central_status("central-status").map_err(err_to_js)) {
        Some(result) => result,
        None => Err(lock_poisoned("central-status")),
    }
}

/// Drives a REAL kernel expiry sweep (U7) at host-supplied monotonic time
/// (decimal string, DATA-02 mapping); returns the settled-operation count
/// as decimal.
#[wasm_bindgen(js_name = driveExpireSweep)]
pub fn drive_expire_sweep_js(now_ms_decimal: String) -> Result<String, JsValue> {
    match with_core(|core| {
        core.drive_expire_sweep(&now_ms_decimal, "central-expire-sweep")
            .map(|settled| settled.to_string())
            .map_err(err_to_js)
    }) {
        Some(result) => result,
        None => Err(lock_poisoned("central-expire-sweep")),
    }
}

/// Drives the REAL shutdown transition (U7); returns `released` on a clean
/// release, `release-failed` otherwise. Idempotent.
#[wasm_bindgen(js_name = driveDestroy)]
pub fn drive_destroy_js() -> Result<String, JsValue> {
    match with_core(|core| core.drive_destroy("central-destroy").map_err(err_to_js)) {
        Some(result) => result.map(|state| state.to_string()),
        None => Err(lock_poisoned("central-destroy")),
    }
}

/// Loud rejection for BLE transitions beyond the driven slice (U7): every
/// named transition fails closed with `capability.unsupported|capability`.
#[wasm_bindgen(js_name = requestBleTransition)]
pub fn request_ble_transition_js(transition: String) -> Result<(), JsValue> {
    match with_core(|core| {
        core.request_ble_transition(&transition, "request-ble-transition")
            .map_err(err_to_js)
    }) {
        Some(result) => result,
        None => Err(lock_poisoned("request-ble-transition")),
    }
}

/// U7 staged-transition slice: runs one scripted synthetic-radio step (a
/// JSON object line) and returns one JSON observation object. Step-level
/// core rejections come back as data; only the session lifetime rejects.
#[wasm_bindgen(js_name = stagedStep)]
pub fn staged_step_js(line: String) -> Result<String, JsValue> {
    match with_core(|core| core.staged_step(&line, "staged-step").map_err(err_to_js)) {
        Some(result) => result,
        None => Err(lock_poisoned("staged-step")),
    }
}

/// U7 staged-transition slice: drains the observation log (FIFO,
/// newline-joined JSON lines).
#[wasm_bindgen(js_name = stagedDrainLog)]
pub fn staged_drain_log_js() -> Result<String, JsValue> {
    match with_core(|core| core.staged_drain("staged-drain-log").map_err(err_to_js)) {
        Some(result) => result,
        None => Err(lock_poisoned("staged-drain-log")),
    }
}

/// U7 staged-transition slice: observes the batch accounting as JSON.
#[wasm_bindgen(js_name = stagedCounters)]
pub fn staged_counters_js() -> Result<String, JsValue> {
    match with_core(|core| core.staged_counters("staged-counters").map_err(err_to_js)) {
        Some(result) => result,
        None => Err(lock_poisoned("staged-counters")),
    }
}
