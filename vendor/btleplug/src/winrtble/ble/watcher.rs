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

use crate::{Error, Result, api::ScanFilter, winrtble::utils};
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
        let token = self
            .received
            .lock()
            .map_err(|_| Error::Other("watcher registration lock poisoned".into()))?
            .take();
        if let Some(token) = token {
            self.watcher.RemoveReceived(token)?;
        }
        Ok(())
    }

    /// Prepare the watcher for one scan.
    ///
    /// UBM patch (UBM_PATCHES.md #11): passive scanning without extended
    /// advertisements, the watcher defaults the legacy WinRT addon scanned
    /// with (`winrt-boundary.inc`). Upstream forced `Active` (a scan request
    /// to every advertiser: more radio traffic and power) and
    /// `AllowExtendedAdvertisements(true)` (its failure ignored). Extended
    /// advertisements stay at the watcher default (off); nothing sets them.
    ///
    /// UBM patch (UBM_PATCHES.md #16): the caller's service UUIDs go on the
    /// OS advertisement filter, as the legacy addon appended them
    /// (`winrt-boundary.inc`: `AdvertisementFilter().Advertisement()
    /// .ServiceUuids().Append`), so Windows filters in the controller path.
    /// Upstream cleared the OS filter and filtered only in software; the
    /// software predicate in the handler stays the final gate.
    fn configure_for_scan(&self, services: &[windows::core::GUID]) -> Result<()> {
        let ad = self.watcher.AdvertisementFilter()?.Advertisement()?;
        let uuids = ad.ServiceUuids()?;
        uuids.Clear()?;
        for service in services {
            uuids.Append(*service)?;
        }
        self.watcher
            .SetScanningMode(BluetoothLEScanningMode::Passive)?;
        Ok(())
    }

    pub fn start(&self, filter: ScanFilter, on_received: AdvertisementEventHandler) -> Result<()> {
        let ScanFilter { services, .. } = filter;
        // Pre-convert the filter UUIDs once so the handler closure is cheap.
        let filter_guids: Vec<windows::core::GUID> = services.iter().map(utils::to_guid).collect();
        self.configure_for_scan(&filter_guids)?;

        let handler: TypedEventHandler<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementReceivedEventArgs,
        > = TypedEventHandler::new(
            move |_sender, args: Ref<BluetoothLEAdvertisementReceivedEventArgs>| {
                if let Ok(args) = args.ok() {
                    // Software service-UUID filter.
                    if !filter_guids.is_empty() {
                        if let Ok(ad) = args.Advertisement() {
                            if let Ok(ad_uuids) = ad.ServiceUuids() {
                                let count = ad_uuids.Size().unwrap_or(0);
                                let advertised: Vec<windows::core::GUID> =
                                    (0..count).filter_map(|i| ad_uuids.GetAt(i).ok()).collect();
                                let all_present =
                                    filter_guids.iter().all(|g| advertised.contains(g));
                                if !all_present {
                                    return Ok(());
                                }
                            }
                        }
                    }
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

    /// UBM patch #11: every scan runs passive without extended
    /// advertisements, as the legacy WinRT addon did.
    #[test]
    fn a_scan_is_passive_without_extended_advertisements() {
        let watcher = BLEWatcher::new().expect("watcher");
        watcher.configure_for_scan(&[]).expect("configure");
        assert_eq!(
            watcher.watcher.ScanningMode().expect("mode"),
            BluetoothLEScanningMode::Passive
        );
        assert!(
            !watcher
                .watcher
                .AllowExtendedAdvertisements()
                .expect("extended advertisements")
        );
    }

    /// UBM patch #16: the caller's service UUIDs reach the OS filter, as the
    /// legacy addon set them, and a later scan replaces them.
    #[test]
    fn the_service_filter_reaches_the_os_watcher() {
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
        watcher
            .configure_for_scan(&[heart_rate, battery])
            .expect("configure");
        assert_eq!(read_back(&watcher), vec![heart_rate, battery]);
        watcher.configure_for_scan(&[]).expect("configure");
        assert!(
            read_back(&watcher).is_empty(),
            "a later scan replaces the filter"
        );
    }
}
