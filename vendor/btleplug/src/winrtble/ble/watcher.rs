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

use crate::{
    Error, Result,
    api::{ScanFilter, WindowsScanOptions, WindowsScanningMode},
};
use windows::{Devices::Bluetooth::Advertisement::*, Foundation::TypedEventHandler, core::Ref};

pub type AdvertisementEventHandler =
    Box<dyn Fn(&BluetoothLEAdvertisementReceivedEventArgs) -> windows::core::Result<()> + Send>;

/// UBM patch (UBM_PATCHES.md #5): the watcher stopped. `error` is the raw
/// `BluetoothError` (0 = `Success`, e.g. an explicit stop).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanStopped {
    pub error: i32,
    pub error_name: String,
}

#[derive(Debug)]
pub struct BLEWatcher {
    watcher: BluetoothLEAdvertisementWatcher,
    // UBM patch (UBM_PATCHES.md #5): the `Received` registration of the
    // current scan, removed on stop and before the next start so handlers
    // never accumulate across scans (upstream re-registers on every start,
    // duplicating every advertisement after the first scan).
    received: std::sync::Mutex<Option<i64>>,
    stopped: tokio::sync::broadcast::Sender<ScanStopped>,
}

impl From<windows::core::Error> for Error {
    /// UBM patch (UBM_PATCHES.md #15): the HRESULT as the platform's
    /// answer, the legacy addon's `{code:"hresult", hresult}` identity.
    fn from(err: windows::core::Error) -> Error {
        Error::Platform(
            crate::PlatformError::new("winrt", "hresult", err.message().to_string()).with(
                "hresult",
                crate::winrtble::gatt_model::hresult_code(err.code().0),
            ),
        )
    }
}

impl BLEWatcher {
    pub fn new() -> Result<Self> {
        let ad = BluetoothLEAdvertisementFilter::new()?;
        let watcher = BluetoothLEAdvertisementWatcher::Create(&ad)?;
        let (stopped, _) = tokio::sync::broadcast::channel(16);
        let sender = stopped.clone();
        let handler: TypedEventHandler<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementWatcherStoppedEventArgs,
        > = TypedEventHandler::new(
            move |_sender, args: Ref<BluetoothLEAdvertisementWatcherStoppedEventArgs>| {
                if let Ok(args) = args.ok() {
                    let error = args.Error()?;
                    // No receiver is not a failure.
                    let _ = sender.send(ScanStopped {
                        error: error.0,
                        error_name: format!("{error:?}"),
                    });
                }
                Ok(())
            },
        );
        watcher.Stopped(&handler)?;
        Ok(BLEWatcher {
            watcher,
            received: std::sync::Mutex::new(None),
            stopped,
        })
    }

    /// UBM patch (UBM_PATCHES.md #5): every time the watcher stops.
    pub fn stopped_events(&self) -> tokio::sync::broadcast::Receiver<ScanStopped> {
        self.stopped.subscribe()
    }

    fn remove_received(&self) -> Result<()> {
        let mut token = self
            .received
            .lock()
            .map_err(|_| Error::Other("watcher registration lock poisoned".into()))?;
        if let Some(value) = *token {
            self.watcher.RemoveReceived(value)?;
            *token = None;
        }
        Ok(())
    }

    /// Prepare the watcher for one scan.
    ///
    /// UBM patch (UBM_PATCHES.md #11): default active scanning, without extended
    /// advertisements. BlueZ and CoreBluetooth both return the complete
    /// local name from the scan response. A passive WinRT watcher never
    /// sends `SCAN_REQ`, so that name is absent and a name-selected peer
    /// cannot be chosen. Explicit Windows options select active, passive or
    /// versioned None reception and opt into extended advertisements; absent
    /// options restore the active/nonextended defaults for every scan.
    ///
    /// UBM patch (UBM_PATCHES.md #16): the OS service-UUID filter stays
    /// empty. A scan response carries the complete local name and does not
    /// repeat the service UUIDs. Windows delivers that packet to `Received`
    /// only when `ServiceUuids` is empty; a non-empty filter keeps the
    /// advertising packet (empty name) and drops the scan response. BlueZ
    /// and CoreBluetooth both surface the name, so the OS filter cannot be
    /// used here. The caller's services are matched in software instead.
    fn configure_for_scan(&self, options: WindowsScanOptions) -> Result<()> {
        use windows::{Foundation::Metadata::ApiInformation, core::HSTRING};
        let none_present = options.mode != WindowsScanningMode::None
            || ApiInformation::IsEnumNamedValuePresent(
                &HSTRING::from("Windows.Devices.Bluetooth.Advertisement.BluetoothLEScanningMode"),
                &HSTRING::from("None"),
            )?;
        let extended_present = ApiInformation::IsPropertyPresent(
            &HSTRING::from(
                "Windows.Devices.Bluetooth.Advertisement.BluetoothLEAdvertisementWatcher",
            ),
            &HSTRING::from("AllowExtendedAdvertisements"),
        )?;
        // Adapter support was admitted by Adapter::start_scan before this
        // synchronous watcher configuration; both APIs still precede effects.
        options.validate_runtime(none_present, extended_present, true)?;
        let ad = self.watcher.AdvertisementFilter()?.Advertisement()?;
        ad.ServiceUuids()?.Clear()?;
        self.watcher.SetScanningMode(match options.mode {
            WindowsScanningMode::Active => BluetoothLEScanningMode::Active,
            WindowsScanningMode::Passive => BluetoothLEScanningMode::Passive,
            WindowsScanningMode::None => BluetoothLEScanningMode::None,
        })?;
        if extended_present {
            self.watcher
                .SetAllowExtendedAdvertisements(options.allow_extended_advertisements)?;
        }
        Ok(())
    }

    pub fn start(&self, filter: ScanFilter, on_received: AdvertisementEventHandler) -> Result<()> {
        // Service/name conjunctions may span several packets in either order.
        // Native ingress cannot decide which partial packet will be needed by
        // the bounded, generation-scoped shared evidence matcher. Keep all
        // packets here and leave that decision to the canonical matcher.
        self.configure_for_scan(filter.windows.unwrap_or_default())?;

        let handler: TypedEventHandler<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementReceivedEventArgs,
        > = TypedEventHandler::new(
            move |_sender, args: Ref<BluetoothLEAdvertisementReceivedEventArgs>| {
                if let Ok(args) = args.ok() {
                    on_received(args)?;
                }
                Ok(())
            },
        );

        self.remove_received()?;
        let token = self.watcher.Received(&handler)?;
        *self
            .received
            .lock()
            .map_err(|_| Error::Other("watcher registration lock poisoned".into()))? = Some(token);
        self.watcher.Start()?;
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        self.watcher.Stop()?;
        self.remove_received()
    }
}

#[cfg(test)]
mod ubm_scan_mode_tests {
    use super::*;
    use crate::winrtble::utils;

    /// UBM patch #11: every scan requests scan responses, and extended
    /// advertisements stay off. The complete local name of a legacy
    /// advertiser arrives in the scan response.
    #[test]
    fn a_scan_is_active_without_extended_advertisements() {
        let watcher = BLEWatcher::new().expect("watcher");
        watcher
            .configure_for_scan(WindowsScanOptions::default())
            .expect("configure");
        assert_eq!(
            watcher.watcher.ScanningMode().expect("mode"),
            BluetoothLEScanningMode::Active
        );
        assert!(
            !watcher
                .watcher
                .AllowExtendedAdvertisements()
                .expect("extended advertisements")
        );
    }

    /// UBM patch #16: requested service UUIDs stay off the OS filter. A
    /// non-empty OS filter drops scan responses, and that is the packet
    /// that carries the local name.
    #[test]
    fn the_service_filter_stays_off_the_os_watcher() {
        let heart_rate = utils::to_guid(&uuid::Uuid::from_u128(
            0x0000180d_0000_1000_8000_00805f9b34fb,
        ));
        let battery = utils::to_guid(&uuid::Uuid::from_u128(
            0x0000180f_0000_1000_8000_00805f9b34fb,
        ));
        let watcher = BLEWatcher::new().expect("watcher");
        let read_back = |watcher: &BLEWatcher| -> Vec<windows::core::GUID> {
            let uuids = watcher
                .watcher
                .AdvertisementFilter()
                .and_then(|filter| filter.Advertisement())
                .and_then(|ad| ad.ServiceUuids())
                .expect("filter");
            (0..uuids.Size().expect("size"))
                .map(|index| uuids.GetAt(index).expect("uuid"))
                .collect()
        };
        // Seed a stale native filter to prove configuration clears it.
        let uuids = watcher
            .watcher
            .AdvertisementFilter()
            .unwrap()
            .Advertisement()
            .unwrap()
            .ServiceUuids()
            .unwrap();
        uuids.Append(heart_rate).unwrap();
        uuids.Append(battery).unwrap();
        watcher
            .configure_for_scan(WindowsScanOptions::default())
            .expect("configure");
        assert!(
            read_back(&watcher).is_empty(),
            "a service UUID filter drops scan responses, so it stays off the OS watcher"
        );
        watcher
            .configure_for_scan(WindowsScanOptions::default())
            .expect("configure");
        assert!(
            read_back(&watcher).is_empty(),
            "a later scan leaves the OS filter empty"
        );
    }
}
