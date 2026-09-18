//! `ubm-mobile`: the process-owned mobile Rust owner (PR210-01/14/17).
//!
//! One [`MobileHost`] per process drives the real `ubm-core` central
//! (through [`ubm_desktop::DesktopCentral`]) over the platform radio
//! ([`ForeignRadio`] → [`PlatformRadio`]); every React Native manager is a
//! [`MobileSession`] lease on it. JS talks to a session through the
//! `ubm-mobile-wire/1` op table ([`wire`], `docs/MOBILE_RUST_WIRE.md`):
//! `invoke` for operations, wake-driven `drain` for records.
//!
//! This crate has no path to the staged/fake radio: its production
//! surface cannot reach `ubm-fake-radio` (guarded by the `cargo tree`
//! test in `tests/no_fake_radio.rs`).

pub mod compat;
pub mod drain;
pub mod foreign;
pub mod host;
pub mod identity;
pub mod radio;
pub mod session;
pub mod wire;

pub use foreign::{CompletionStatus, ForeignRadio, RadioCounters};
pub use host::{HostOptions, MobileHost};
pub use identity::MobileIdentity;
pub use radio::*;
pub use session::{Completion, MobileSession, OPS};
pub use wire::WIRE_REVISION;
