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

use super::{ble::watcher::BLEWatcher, peripheral::Peripheral, peripheral::PeripheralId};
use crate::{
    Error, Result,
    api::{BDAddr, Central, CentralEvent, CentralState, ScanFilter},
    common::adapter_manager::AdapterManager,
};
use async_trait::async_trait;
use futures::stream::Stream;
use log::warn;
use std::convert::TryInto;
use std::fmt::{self, Debug, Formatter};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use windows::{
    Devices::Radios::{Radio, RadioState},
    Foundation::TypedEventHandler,
};

/// Implementation of [api::Central](crate::api::Central).
#[derive(Clone)]
pub struct Adapter {
    watcher: Arc<Mutex<BLEWatcher>>,
    manager: Arc<AdapterManager<Peripheral>>,
    /// UBM patch (`winrt-adapter-by-id`): the radio this adapter reports
    /// state from, rebindable when the adapter device comes back after a
    /// removal. `None` when the OS withholds the radio (adapter access not
    /// granted), answered by `withheld`.
    radio: Arc<Mutex<Option<RadioBinding>>>,
    withheld: CentralState,
}

/// One radio and its `StateChanged` registration, removed on drop so the
/// handler never outlives the adapter.
#[derive(Debug)]
struct RadioBinding {
    radio: Radio,
    token: i64,
}

impl Drop for RadioBinding {
    fn drop(&mut self) {
        if let Err(error) = self.radio.RemoveStateChanged(self.token) {
            warn!("Drop: removing the radio StateChanged handler failed: {error:?}");
        }
    }
}

// https://github.com/microsoft/windows-rs/blob/master/crates/libs/windows/src/Windows/Devices/Radios/mod.rs
/// UBM patch (UBM_PATCHES.md #7): `Disabled` is off, as the legacy addon's
/// `RadioPower` (`addon.cpp`); every other state is `Unknown`.
fn central_state(state: RadioState) -> CentralState {
    match state {
        RadioState::On => CentralState::PoweredOn,
        RadioState::Off | RadioState::Disabled => CentralState::PoweredOff,
        _ => CentralState::Unknown,
    }
}

fn get_central_state(radio: &Radio) -> Result<CentralState> {
    Ok(central_state(radio.State()?))
}

fn bind_radio(radio: Radio, manager: &Arc<AdapterManager<Peripheral>>) -> Result<RadioBinding> {
    let manager = Arc::clone(manager);
    let handler = TypedEventHandler::new(move |sender: windows::core::Ref<Radio>, _args| {
        // A state the OS cannot read is reported as `Unknown`, and logged.
        let state = sender
            .ok()
            .map_err(Error::from)
            .and_then(get_central_state)
            .unwrap_or_else(|error| {
                warn!("radio StateChanged: reading the state failed: {error:?}");
                CentralState::Unknown
            });
        manager.emit(CentralEvent::StateUpdate(state));
        Ok(())
    });
    let token = radio.StateChanged(&handler)?;
    Ok(RadioBinding { radio, token })
}

impl Adapter {
    pub(crate) fn new(radio: Radio) -> Result<Self> {
        Self::from_radio(radio)
    }

    /// UBM patch (`winrt-adapter-by-id`): the adapter for one chosen radio
    /// (`BluetoothAdapter::FromIdAsync(id).GetRadioAsync()`), so a caller
    /// selects the Bluetooth adapter by its device id instead of by radio
    /// order. Scanning and connections still run through the Windows
    /// Bluetooth LE stack (the advertisement watcher and
    /// `BluetoothLEDevice::FromBluetoothAddressAsync` take no adapter), as
    /// they did in the legacy addon; the radio decides the reported state.
    pub fn from_radio(radio: Radio) -> Result<Self> {
        let manager = Arc::new(AdapterManager::default());
        let binding = bind_radio(radio, &manager)?;
        Ok(Adapter {
            watcher: Arc::new(Mutex::new(BLEWatcher::new()?)),
            manager,
            radio: Arc::new(Mutex::new(Some(binding))),
            withheld: CentralState::Unknown,
        })
    }

    /// UBM patch (`winrt-adapter-by-id`): an adapter whose radio the OS
    /// withholds (the legacy addon selected such an adapter without a
    /// radio when access was not granted). `adapter_state` answers `state`
    /// until [`Adapter::rebind_radio`] supplies a radio.
    pub fn without_radio(state: CentralState) -> Result<Self> {
        Ok(Adapter {
            watcher: Arc::new(Mutex::new(BLEWatcher::new()?)),
            manager: Arc::new(AdapterManager::default()),
            radio: Arc::new(Mutex::new(None)),
            withheld: state,
        })
    }

    /// UBM patch (`winrt-adapter-by-id`): report state from `radio` from now
    /// on (the adapter device came back after a removal, so the previous
    /// `Radio` object is stale), and emit its current state.
    pub fn rebind_radio(&self, radio: Radio) -> Result<()> {
        let state = get_central_state(&radio)?;
        let binding = bind_radio(radio, &self.manager)?;
        let previous = self
            .radio
            .lock()
            .map_err(Into::<Error>::into)?
            .replace(binding);
        drop(previous);
        self.manager.emit(CentralEvent::StateUpdate(state));
        Ok(())
    }
}

impl Adapter {
    /// UBM patch (UBM_PATCHES.md #5): every time this adapter's
    /// advertisement watcher stops, with the OS's `BluetoothError`.
    pub fn scan_stopped_events(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<super::ble::watcher::ScanStopped>> {
        let watcher = self.watcher.lock().map_err(Into::<Error>::into)?;
        Ok(watcher.stopped_events())
    }
}

impl Debug for Adapter {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("Adapter")
            .field("manager", &self.manager)
            .finish()
    }
}

#[async_trait]
impl Central for Adapter {
    type Peripheral = Peripheral;

    async fn events(&self) -> Result<Pin<Box<dyn Stream<Item = CentralEvent> + Send>>> {
        Ok(self.manager.event_stream())
    }

    async fn start_scan(&self, filter: ScanFilter) -> Result<()> {
        let watcher = self.watcher.lock().map_err(Into::<Error>::into)?;
        let manager = self.manager.clone();
        watcher.start(
            filter,
            Box::new(move |args| {
                let bluetooth_address = args.BluetoothAddress()?;
                let address: BDAddr = bluetooth_address.try_into().unwrap();
                if let Some(mut entry) = manager.peripheral_mut(&address.into()) {
                    entry.value_mut().update_properties(args);
                    manager.emit(CentralEvent::DeviceUpdated(address.into()));
                } else {
                    let peripheral = Peripheral::new(Arc::downgrade(&manager), address);
                    peripheral.update_properties(args);
                    manager.add_peripheral(peripheral);
                    manager.emit(CentralEvent::DeviceDiscovered(address.into()));
                }
                // UBM patch (UBM_PATCHES.md #17): every received
                // advertisement, with its own data; an unreadable one is
                // reported, never dropped.
                manager.emit(match Peripheral::advertisement_report(args) {
                    Ok(report) => CentralEvent::Advertisement {
                        id: address.into(),
                        report,
                    },
                    Err(error) => CentralEvent::AdvertisementUnread {
                        id: address.into(),
                        detail: format!("{error:?}"),
                    },
                });
                Ok(())
            }),
        )
    }

    async fn stop_scan(&self) -> Result<()> {
        let watcher = self.watcher.lock().map_err(Into::<Error>::into)?;
        watcher.stop()?;
        Ok(())
    }

    async fn peripherals(&self) -> Result<Vec<Peripheral>> {
        Ok(self.manager.peripherals())
    }

    async fn peripheral(&self, id: &PeripheralId) -> Result<Peripheral> {
        self.manager.peripheral(id).ok_or(Error::DeviceNotFound)
    }

    /// UBM patch (UBM_PATCHES.md #19, finding 127): the peripheral at this
    /// address, known or registered now. The legacy WinRT addon connected by
    /// address (`BluetoothLEDevice::FromBluetoothAddressAsync`,
    /// `winrt-boundary.inc:868`), which `connect` does here too, so no scan
    /// is needed first.
    async fn add_peripheral(&self, address: &PeripheralId) -> Result<Peripheral> {
        if let Some(peripheral) = self.manager.peripheral(address) {
            return Ok(peripheral);
        }
        let peripheral = Peripheral::new(Arc::downgrade(&self.manager), address.address());
        self.manager.replace_peripheral(peripheral.clone());
        Ok(peripheral)
    }

    async fn clear_peripherals(&self) -> Result<()> {
        self.manager.clear_peripherals();
        Ok(())
    }

    async fn adapter_info(&self) -> Result<String> {
        // TODO: Get information about the adapter.
        Ok("WinRT".to_string())
    }

    async fn adapter_state(&self) -> Result<CentralState> {
        let radio = self.radio.lock().map_err(Into::<Error>::into)?;
        match radio.as_ref() {
            Some(binding) => get_central_state(&binding.radio),
            None => Ok(self.withheld.clone()),
        }
    }
}
