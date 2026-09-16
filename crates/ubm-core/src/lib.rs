//! `ubm-core`: portable deterministic UBM 5.0 transition kernel.
//!
//! Mirrors C-UBM.0.1.1-DRAFT (U1 accepted); derivation notes live on
//! each module. Shape follows the BLE Rust convergence plan execution model:
//! the core receives validated inputs plus a monotonic clock reading, advances
//! bounded state, and appends concrete effects to a caller-provided batch. The
//! host executes those effects outside the core's mutable borrow and feeds
//! results back with operation/resource generations.
//!
//! No async runtime (no Tokio), no OS services, no I/O: `core`/`alloc`-level
//! `std` collections only (`Vec`, `String`), so the crate also checks for
//! `wasm32-unknown-unknown`.

pub mod central;
pub mod codec;
pub mod contracts;
pub mod ownership;
pub mod profiles;
pub mod streams;

pub use codec::{MAX_WIRE_BYTES, WireCleanupFailure, WireCleanupRecord, decode, encode};

/// Test-only failure marker. A literal false assertion trips
/// `clippy::assertions_on_constants`, and halting macros stay off fallible
/// paths, so failing test arms report through this runtime condition instead.
#[cfg(test)]
pub(crate) fn check(condition: bool, message: &str) {
    assert!(condition, "{message}");
}
