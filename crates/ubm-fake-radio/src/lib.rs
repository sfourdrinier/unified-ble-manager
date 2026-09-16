//! `ubm-fake-radio`: deterministic synthetic-radio staged driver for the
//! UBM 5.0 U7 slice (trackourhealth/bun-mono#1188).
//!
//! NO BLE hardware exists on this host, so transitions are driven from
//! SYNTHETIC host events only: deterministic scripted discovery, IO,
//! notification, and service-change event programs plus fault injection
//! (timeouts, disconnects, errors, out-of-order delivery). The state
//! machine underneath is never synthetic: [`StagedDriver`] runs every
//! transition through the REAL `ubm-core` central (read-only dependency),
//! stages kernel effects into bounded batches of at most 64 slots, and
//! preserves dropped-not-staged accounting.
//!
//! Why a new crate instead of reusing `ubm-desktop`'s `FakeRadio`: that
//! boundary is `async` over `tokio::sync` primitives and models the
//! btleplug OS-radio seam. Bindings are synchronous FFI contexts with
//! minimal dependency closures (they also check for `wasm32`); pulling
//! Tokio and btleplug into them is disproportionate and platform-wrong.
//! This crate has exactly one dependency (`ubm-core`), no async runtime,
//! no OS services, no I/O.

pub mod driver;
pub mod json;

pub use driver::{CONTRACT_REVISION, STAGED_BATCH_MAX, StagedDriver, StagedError};
pub use json::{JsonError, JsonValue};
