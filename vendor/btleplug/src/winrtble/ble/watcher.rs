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

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

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
    /// UBM patch (UBM_PATCHES.md #11): active scanning, without extended
    /// advertisements. BlueZ and CoreBluetooth both return the complete
    /// local name from the scan response. A passive WinRT watcher never
    /// sends `SCAN_REQ`, so that name is absent and a name-selected peer
    /// cannot be chosen. Extended advertisements stay at the watcher
    /// default (off); nothing sets them.
    ///
    /// UBM patch (UBM_PATCHES.md #16): the OS service-UUID filter stays
    /// empty. A scan response carries the complete local name and does not
    /// repeat the service UUIDs. Windows delivers that packet to `Received`
    /// only when `ServiceUuids` is empty; a non-empty filter keeps the
    /// advertising packet (empty name) and drops the scan response. BlueZ
    /// and CoreBluetooth both surface the name, so the OS filter cannot be
    /// used here. The caller's services are matched in software instead.
    fn configure_for_scan(&self, _services: &[windows::core::GUID]) -> Result<()> {
        let ad = self.watcher.AdvertisementFilter()?.Advertisement()?;
        ad.ServiceUuids()?.Clear()?;
        self.watcher
            .SetScanningMode(BluetoothLEScanningMode::Active)?;
        Ok(())
    }

    pub fn start(&self, filter: ScanFilter, on_received: AdvertisementEventHandler) -> Result<()> {
        let ScanFilter { services, .. } = filter;
        // Pre-convert the filter UUIDs once so the handler closure is cheap.
        let filter_guids: Vec<windows::core::GUID> = services.iter().map(utils::to_guid).collect();
        self.configure_for_scan(&filter_guids)?;
        // Addresses whose advertising packet carried the service filter.
        // A scan response does not repeat those UUIDs; it still belongs to
        // that peer and is where the complete local name usually lives.
        let admitted: Arc<Mutex<HashSet<u64>>> = Arc::new(Mutex::new(HashSet::new()));

        let handler: TypedEventHandler<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementReceivedEventArgs,
        > = TypedEventHandler::new(
            move |_sender, args: Ref<BluetoothLEAdvertisementReceivedEventArgs>| {
                if let Ok(args) = args.ok() {
                    if !filter_guids.is_empty()
                        && !service_filter_keeps(&args, &filter_guids, &admitted)
                    {
                        return Ok(());
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

/// Software service filter for one received event.
///
/// An advertising packet that carries every requested service admits its
/// address. A later scan response from that address is kept even though it
/// has no service UUIDs, because that is the packet that carries the local
/// name. Anything else that lacks the services is dropped. An event whose
/// service list cannot be read is kept, as the previous predicate did.
fn service_filter_keeps(
    args: &BluetoothLEAdvertisementReceivedEventArgs,
    required: &[windows::core::GUID],
    admitted: &Mutex<HashSet<u64>>,
) -> bool {
    match advertised_services(args, required) {
        ServiceMatch::Present => {
            if let Ok(address) = args.BluetoothAddress()
                && let Ok(mut admitted) = admitted.lock()
            {
                admitted.insert(address);
            }
            true
        }
        ServiceMatch::Unreadable => true,
        ServiceMatch::Absent => {
            let scan_response =
                args.AdvertisementType().ok() == Some(BluetoothLEAdvertisementType::ScanResponse);
            if !scan_response {
                return false;
            }
            let Ok(address) = args.BluetoothAddress() else {
                return false;
            };
            admitted
                .lock()
                .map(|admitted| admitted.contains(&address))
                .unwrap_or(false)
        }
    }
}

enum ServiceMatch {
    Present,
    Absent,
    Unreadable,
}

fn advertised_services(
    args: &BluetoothLEAdvertisementReceivedEventArgs,
    required: &[windows::core::GUID],
) -> ServiceMatch {
    let Ok(advertisement) = args.Advertisement() else {
        return ServiceMatch::Unreadable;
    };
    let Ok(uuids) = advertisement.ServiceUuids() else {
        return ServiceMatch::Unreadable;
    };
    let count = uuids.Size().unwrap_or(0);
    let advertised: Vec<windows::core::GUID> = (0..count)
        .filter_map(|index| uuids.GetAt(index).ok())
        .collect();
    if required.iter().all(|service| advertised.contains(service)) {
        ServiceMatch::Present
    } else {
        ServiceMatch::Absent
    }
}

#[cfg(test)]
mod ubm_scan_mode_tests {
    use super::*;

    /// UBM patch #11: every scan requests scan responses, and extended
    /// advertisements stay off. The complete local name of a legacy
    /// advertiser arrives in the scan response.
    #[test]
    fn a_scan_is_active_without_extended_advertisements() {
        let watcher = BLEWatcher::new().expect("watcher");
        watcher.configure_for_scan(&[]).expect("configure");
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
        watcher
            .configure_for_scan(&[heart_rate, battery])
            .expect("configure");
        assert!(
            read_back(&watcher).is_empty(),
            "a service UUID filter drops scan responses, so it stays off the OS watcher"
        );
        watcher.configure_for_scan(&[]).expect("configure");
        assert!(
            read_back(&watcher).is_empty(),
            "a later scan leaves the OS filter empty"
        );
    }
}
