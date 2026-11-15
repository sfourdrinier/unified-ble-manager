//! `ubm-mobile`: the process-owned mobile Rust owner (PR210-01/14/17).
//!
//! One [`MobileHost`] per process drives the real `ubm-core` central
//! (through [`ubm_desktop::DesktopCentral`]) over the platform radio
//! ([`ForeignRadio`] → [`PlatformRadio`]); every React Native manager is a
//! [`MobileSession`] lease on it. JS talks to a session through the
//! `ubm-mobile-wire/3` op table ([`wire`], `docs/MOBILE_RUST_WIRE.md`):
//! `invoke` for operations, wake-driven `drain` for records.
//!
//! This crate has no path to the staged/fake radio: its production
//! surface cannot reach `ubm-fake-radio` (guarded by the `cargo tree`
//! test in `tests/no_fake_radio.rs`).

mod companion_filters;
#[cfg(test)]
extern crate self as ubm_mobile;
#[cfg(all(feature = "synthetic-marker-harness", not(debug_assertions)))]
compile_error!("synthetic-marker-harness cannot be used in release builds");

pub mod compat;
pub mod continuation;
pub mod drain;
pub mod foreign;
pub mod host;
pub mod identity;
mod ordered_encoding;
mod ordered_open_slots;
mod ordered_owners;
mod ordered_public;
pub mod radio;
mod reclaim_v3_receipts;
#[cfg(test)]
#[path = "../tests/reclaim_v3/receipts.rs"]
mod reclaim_v3_receipts_tests;
pub mod session;
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod test_common;
pub mod wire;

pub use foreign::{CompletionStatus, ForeignRadio, RadioCounters};
pub use host::{
    HostOptions, MobileHost, ScanFailureRecovery, TerminalEvidenceRecovery,
    TerminalEvidenceRecoveryEntry, TerminalEvidenceRecoveryListPage, TerminalEvidenceRecoveryPage,
    TerminalEvidenceRecoveryState,
};
pub use identity::MobileIdentity;
pub use radio::*;
pub use session::{Completion, MobileSession, OPS};
pub use wire::WIRE_REVISION;
