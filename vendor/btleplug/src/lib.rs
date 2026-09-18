// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.
//
// Some portions of this file are taken and/or modified from Rumble
// (https://github.com/mwylde/rumble), using a dual MIT/Apache License under the
// following copyright:
//
// Copyright (c) 2014 The Rust Project Developers

//! btleplug is a Bluetooth Low Energy (BLE) central module library for Rust.
//! It currently supports Windows 10, macOS (and possibly iOS) and Linux
//! (BlueZ). Android support is planned for the future.
//!
//! ## Usage
//!
//! An example of how to use the library to control some BLE smart lights:
//!
//! ```rust,no_run
//! use btleplug::api::{bleuuid::uuid_from_u16, Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
//! use btleplug::platform::{Adapter, Manager, Peripheral};
//! use rand::{RngExt, rng};
//! use std::error::Error;
//! use std::thread;
//! use std::time::Duration;
//! use tokio::time;
//! use uuid::Uuid;
//!
//! const LIGHT_CHARACTERISTIC_UUID: Uuid = uuid_from_u16(0xFFE9);
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn Error>> {
//!     let manager = Manager::new().await.unwrap();
//!
//!     // get the first bluetooth adapter
//!     let adapters = manager.adapters().await?;
//!     let central = adapters.into_iter().nth(0).unwrap();
//!
//!     // start scanning for devices
//!     central.start_scan(ScanFilter::default()).await?;
//!     // instead of waiting, you can use central.events() to get a stream which will
//!     // notify you of new devices, for an example of that see examples/event_driven_discovery.rs
//!     time::sleep(Duration::from_secs(2)).await;
//!
//!     // find the device we're interested in
//!     let light = find_light(&central).await.unwrap();
//!
//!     // connect to the device
//!     light.connect().await?;
//!
//!     // discover services and characteristics
//!     light.discover_services().await?;
//!
//!     // find the characteristic we want
//!     let chars = light.characteristics();
//!     let cmd_char = chars.iter().find(|c| c.uuid == LIGHT_CHARACTERISTIC_UUID).unwrap();
//!
//!     // dance party
//!     let mut rng = rng();
//!     for _ in 0..20 {
//!         let color_cmd = vec![0x56, rng.random(), rng.random(), rng.random(), 0x00, 0xF0, 0xAA];
//!         light.write(&cmd_char, &color_cmd, WriteType::WithoutResponse).await?;
//!         time::sleep(Duration::from_millis(200)).await;
//!     }
//!     Ok(())
//! }
//!
//! async fn find_light(central: &Adapter) -> Option<Peripheral> {
//!     for p in central.peripherals().await.unwrap() {
//!         if p.properties()
//!             .await
//!             .unwrap()
//!             .unwrap()
//!             .local_name
//!             .iter()
//!             .any(|name| name.contains("LEDBlue"))
//!         {
//!             return Some(p);
//!         }
//!     }
//!     None
//! }
//! ```

use crate::api::ParseBDAddrError;
use std::result;
use std::time::Duration;

pub mod api;
#[cfg(target_os = "linux")]
mod bluez;
#[cfg(not(target_os = "linux"))]
mod common;

/// UBM patch (UBM_PATCHES.md #10): the notification stream every platform
/// peripheral hands out, exported so the desktop core's tests drive the
/// real lag-reporting path. Linux (BlueZ) has no broadcast: its
/// notifications arrive over bluez-async's unbounded D-Bus stream.
#[cfg(not(target_os = "linux"))]
pub mod ubm {
    pub use crate::common::util::notifications_stream_from_broadcast_receiver;

    /// UBM patch (UBM_PATCHES.md #13): slots of the adapter event broadcast
    /// and of each peripheral's notification broadcast (upstream: 16). The
    /// legacy backends queued far more before losing anything (CoreBluetooth
    /// thread-safe functions without a bound, WinRT 128 notifications and
    /// 256 advertisements), so 16 was a loss point below legacy. A receiver
    /// that still falls this far behind is told what it lost (patch 10).
    pub const EVENT_CAPACITY: usize = 4096;

    /// UBM patch (UBM_PATCHES.md #15): the legacy WinRT addon's HRESULT
    /// code (`0x` and eight upper-case hex digits), for hosts that meet a
    /// WinRT error outside btleplug.
    #[cfg(target_os = "windows")]
    pub use crate::winrtble::gatt_model::hresult_code;
}
#[cfg(target_vendor = "apple")]
mod corebluetooth;
#[cfg(target_os = "android")]
mod droidplug;
#[cfg(all(not(target_os = "android"), feature = "jni-host-tests"))]
mod droidplug {
    mod jni_utils;
}
pub mod platform;
#[cfg(feature = "serde")]
pub mod serde;
#[cfg(target_os = "windows")]
mod winrtble;

/// UBM patch (UBM_PATCHES.md #15): the platform's own answer behind a
/// failure, as typed fields rather than text, so a host can restore the
/// legacy error identity:
///
/// - CoreBluetooth: `domain` `"corebluetooth"`, `code` the `NSError` code
///   (metadata `nsErrorDomain`); the read/notify refusals of patch 14 use
///   the legacy addon codes 413, 414 and 415;
/// - WinRT: `domain` `"winrt"`, `code` `"gatt-status"` (metadata
///   `gattStatus`: `protocol-error`, `access-denied`, `unreachable`) or
///   `"hresult"` (metadata `hresult` as `0xXXXXXXXX`), as the legacy addon;
/// - BlueZ: `domain` `"bluez-dbus"`, `code` the D-Bus error name
///   (`org.bluez.Error.Failed` when BlueZ gave none), as the legacy backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformError {
    pub domain: &'static str,
    pub code: String,
    pub message: String,
    pub metadata: Vec<(&'static str, String)>,
}

impl PlatformError {
    /// A platform error without metadata.
    pub fn new(domain: &'static str, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            domain,
            code: code.into(),
            message: message.into(),
            metadata: Vec::new(),
        }
    }

    /// A BlueZ D-Bus failure: the error name as the code
    /// (`org.bluez.Error.Failed` when D-Bus gave none), as the legacy BlueZ
    /// backend reported it.
    pub fn bluez_dbus(name: Option<&str>, message: Option<&str>) -> Self {
        Self::new(
            "bluez-dbus",
            name.unwrap_or("org.bluez.Error.Failed"),
            message.unwrap_or_default(),
        )
    }

    /// This error with one metadata entry.
    #[must_use]
    pub fn with(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.metadata.push((key, value.into()));
        self
    }
}

#[cfg(test)]
mod ubm_platform_error_tests {
    use super::PlatformError;

    #[test]
    fn a_dbus_failure_keeps_its_error_name() {
        let named = PlatformError::bluez_dbus(
            Some("org.bluez.Error.NotPermitted"),
            Some("Read not permitted"),
        );
        assert_eq!(
            (named.domain, named.code.as_str(), named.message.as_str()),
            (
                "bluez-dbus",
                "org.bluez.Error.NotPermitted",
                "Read not permitted"
            )
        );
        let unnamed = PlatformError::bluez_dbus(None, None);
        assert_eq!(unnamed.code, "org.bluez.Error.Failed");
    }
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({} {})", self.message, self.domain, self.code)
    }
}

/// The main error type returned by most methods in btleplug.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// UBM patch (UBM_PATCHES.md #15): the platform's own answer.
    #[error("{}", _0)]
    Platform(PlatformError),

    #[error("Permission denied")]
    PermissionDenied,

    #[error("Device not found")]
    DeviceNotFound,

    #[error("Not connected")]
    NotConnected,

    #[error("Unexpected callback")]
    UnexpectedCallback,

    #[error("Unexpected characteristic")]
    UnexpectedCharacteristic,

    #[error("No such characteristic")]
    NoSuchCharacteristic,

    #[error("No Bluetooth adapter available")]
    NoAdapterAvailable,

    #[error("The operation is not supported: {}", _0)]
    NotSupported(String),

    #[error("Timed out after {:?}", _0)]
    TimedOut(Duration),

    #[error("Error parsing UUID: {0}")]
    Uuid(#[from] uuid::Error),

    #[error("Invalid Bluetooth address: {0}")]
    InvalidBDAddr(#[from] ParseBDAddrError),

    #[error("Runtime Error: {}", _0)]
    RuntimeError(String),

    #[error("{}", _0)]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

/// Convert [`PoisonError`] to [`Error`] for replace `unwrap` to `map_err`
impl<T: std::fmt::Debug> From<std::sync::PoisonError<T>> for Error {
    fn from(e: std::sync::PoisonError<T>) -> Self {
        Self::Other(format!("{:?}", e).into())
    }
}

/// Convenience type for a result using the btleplug [`Error`] type.
pub type Result<T> = result::Result<T, Error>;
