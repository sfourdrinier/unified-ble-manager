//! Narrow per-OS adapters behind [`crate::RadioBoundary`] for what btleplug
//! 0.12 does not expose (PR210 decision 7). Each module uses only its
//! platform's own API and is compiled only for its target with the
//! production `btleplug` radio. The translation rules (`*_model`) are pure
//! data and compile everywhere, so they are tested on every host.

pub mod bluez_model;
pub mod winrt_model;

#[cfg(all(feature = "btleplug", target_os = "linux"))]
pub(crate) mod linux;
#[cfg(all(feature = "btleplug", target_os = "macos"))]
pub(crate) mod macos;
#[cfg(all(feature = "btleplug", target_os = "windows"))]
pub(crate) mod windows;
