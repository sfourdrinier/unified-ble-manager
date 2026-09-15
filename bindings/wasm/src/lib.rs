//! Portable WASM echo boundary: a raw integer ABI (always) plus an optional
//! ergonomic JS mapping (`js-glue` feature).
//!
//! Default build: zero dependencies (besides the portable `ubm-core`), zero
//! imports — the module instantiates with an empty import object. `js-glue`
//! adds `wasm-bindgen` ergonomic wrappers over the same core state for
//! bundler-based hosts.
//!
//! The surface calls the core ONLY through `CoreBackend`, implemented for
//! the ubm-core-backed [`core_backend::CoreSession`] (one implementation;
//! contract truth is single-owned by `ubm-core`).

mod core_backend;
mod raw_abi;

#[cfg(feature = "js-glue")]
mod js_glue;

pub use core_backend::{
    u64_max_decimal, CoreBackend, CoreSession, EchoCode, EchoError, CONTRACT_REVISION,
    MAX_OPERATION_BYTES,
};
pub use raw_abi::{
    ubm_echo_alloc, ubm_echo_counter, ubm_echo_describe_json, ubm_echo_free, ubm_echo_init,
    ubm_echo_last_error, ubm_echo_last_error_text, ubm_echo_run, ubm_echo_stream_begin,
    ubm_echo_stream_cancel, ubm_echo_stream_finish, ubm_echo_stream_push,
};

#[cfg(feature = "js-glue")]
pub use js_glue::{describe_json_js, echo_bytes_js, echo_counter_js, init_contract};
