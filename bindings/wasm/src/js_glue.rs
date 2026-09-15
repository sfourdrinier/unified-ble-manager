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
//! `echo_core` and is unit-tested there; this module adds types only.

use crate::echo_core::{CoreBackend, EchoError};
use crate::raw_abi::with_core;
use wasm_bindgen::prelude::*;

fn err_to_js(err: EchoError) -> JsValue {
    JsValue::from_str(&err.wire_message())
}

/// Initialises the binding (PKG-02 / WEB init contract). Foreign revisions
/// fail closed with `protocol.incompatible`.
#[wasm_bindgen(js_name = initContract)]
pub fn init_contract(revision: String) -> Result<(), JsValue> {
    with_core(|core| core.init(&revision).map_err(err_to_js))
}

/// Owned byte-batch echo as `Uint8Array` round-trip.
#[wasm_bindgen(js_name = echoBytes)]
pub fn echo_bytes_js(input: &[u8]) -> Result<Vec<u8>, JsValue> {
    with_core(|core| CoreBackend::echo_bytes(core, input, "echo-bytes").map_err(err_to_js))
}

/// Lossless u64 echo over decimal strings (`BigInt(n).toString()` in,
/// `BigInt(text)` out).
#[wasm_bindgen(js_name = echoCounterU64)]
pub fn echo_counter_js(decimal: String) -> Result<String, JsValue> {
    with_core(|core| CoreBackend::echo_counter(core, &decimal, "echo-counter").map_err(err_to_js))
}

/// JSON bridge document for `JSON.parse` (static metadata, no init needed).
#[wasm_bindgen(js_name = describeJson)]
pub fn describe_json_js() -> String {
    format!(
        "{{\"revision\":\"{}\",\"maxBytes\":{},\"u64max\":\"{}\"}}",
        crate::echo_core::CONTRACT_REVISION,
        crate::echo_core::MAX_OPERATION_BYTES,
        crate::echo_core::U64_MAX_DECIMAL
    )
}
