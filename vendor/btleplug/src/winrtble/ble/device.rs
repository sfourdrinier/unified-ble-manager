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
    api::BDAddr,
    winrtble::{
        gatt_model::{self, require_gatt_success},
        utils,
    },
};
use log::{debug, trace};
use windows::{
    Devices::Bluetooth::{
        BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice,
        BluetoothLEPreferredConnectionParameters,
        GenericAttributeProfile::{
            GattCharacteristic, GattCommunicationStatus, GattDescriptor, GattDeviceService,
            GattDeviceServicesResult, GattSession,
        },
    },
    Foundation::TypedEventHandler,
};

/// UBM patch (`winrt-uncached-discovery`): a non-success status of one
/// discovery query, named, as the discovery's error.
fn discovery_status(
    stage: &str,
    status: windows::core::Result<GattCommunicationStatus>,
) -> Result<()> {
    let status = status?;
    // UBM patch (UBM_PATCHES.md #15): the status as the platform's answer.
    // Discovery queries return no result object, so no ATT error byte is
    // available here.
    require_gatt_success(stage, status.0)
        .map_err(|_| crate::winrtble::utils::gatt_status_error(stage, status, None))
}

pub type ConnectedEventHandler = Box<dyn Fn(bool) + Send>;
pub type MaxPduSizeChangedEventHandler = Box<dyn Fn(u16) + Send>;

pub struct BLEDevice {
    device: BluetoothLEDevice,
    gatt_session: GattSession,
    connection_token: i64,
    pdu_change_token: i64,
    services: Vec<GattDeviceService>,
}

/// Outcome of one service's characteristic query.
pub enum CharacteristicList {
    /// The service listed its characteristics.
    Ready(Vec<GattCharacteristic>),
    /// Windows refused the service and did not ask the peer. The service
    /// stays out of the discovered table.
    OperatingSystemDenied,
}

impl BLEDevice {
    pub async fn new(
        address: BDAddr,
        address_type: Option<crate::api::AddressType>,
        connection_status_changed: ConnectedEventHandler,
        max_pdu_size_changed: MaxPduSizeChangedEventHandler,
    ) -> Result<Self> {
        let async_op = match address_type {
            Some(kind) => BluetoothLEDevice::FromBluetoothAddressWithBluetoothAddressTypeAsync(
                address.into(),
                match kind {
                    crate::api::AddressType::Public => {
                        windows::Devices::Bluetooth::BluetoothAddressType::Public
                    }
                    crate::api::AddressType::Random => {
                        windows::Devices::Bluetooth::BluetoothAddressType::Random
                    }
                },
            ),
            None => BluetoothLEDevice::FromBluetoothAddressAsync(address.into()),
        }
        .map_err(Error::from)?;
        let device = async_op.await.map_err(|_| Error::DeviceNotFound)?;

        let async_op = GattSession::FromDeviceIdAsync(&device.BluetoothDeviceId()?)
            .map_err(|_| Error::DeviceNotFound)?;
        let gatt_session = async_op.await.map_err(|_| Error::DeviceNotFound)?;

        let connection_status_handler =
            TypedEventHandler::<BluetoothLEDevice, _>::new(move |sender, _| {
                if let Some(sender) = sender.as_ref() {
                    let is_connected = sender
                        .ConnectionStatus()
                        .ok()
                        .map_or(false, |v| v == BluetoothConnectionStatus::Connected);
                    connection_status_changed(is_connected);
                    trace!("state {:?}", sender.ConnectionStatus());
                }
                Ok(())
            });
        let connection_token = device
            .ConnectionStatusChanged(&connection_status_handler)
            .map_err(|_| Error::Other("Could not add connection status handler".into()))?;

        max_pdu_size_changed(gatt_session.MaxPduSize().unwrap());
        let max_pdu_size_changed_handler =
            TypedEventHandler::<GattSession, _>::new(move |sender, _| {
                if let Some(sender) = sender.as_ref() {
                    max_pdu_size_changed(sender.MaxPduSize().unwrap());
                }
                Ok(())
            });
        let pdu_change_token = gatt_session
            .MaxPduSizeChanged(&max_pdu_size_changed_handler)
            .map_err(|_| Error::Other("Could not add max pdu size changed handler".into()))?;

        Ok(BLEDevice {
            device,
            gatt_session,
            connection_token,
            pdu_change_token,
            services: vec![],
        })
    }

    async fn get_gatt_services(
        &self,
        cache_mode: BluetoothCacheMode,
    ) -> Result<GattDeviceServicesResult> {
        let winrt_error = Error::from;
        let async_op = self
            .device
            .GetGattServicesWithCacheModeAsync(cache_mode)
            .map_err(winrt_error)?;
        let service_result = async_op.await.map_err(winrt_error)?;
        Ok(service_result)
    }

    pub fn name(&self) -> windows::core::Result<windows::core::HSTRING> {
        self.device.Name()
    }

    pub async fn connect(&self) -> Result<()> {
        if self.is_connected().await? {
            return Ok(());
        }

        // WinRT tears down a link that no GattSession has asked to keep.
        // GetGattServicesAsync is what brings the link up, and without a
        // hold it answers Unreachable while the peripheral link flaps.
        // BlueZ and CoreBluetooth keep the connection their connect call
        // established. Hold this session first. Drop clears the hold, so a
        // failed connect does not leave the radio connected.
        self.gatt_session
            .SetMaintainConnection(true)
            .map_err(Error::from)?;
        let mut service_result = self.get_gatt_services(BluetoothCacheMode::Uncached).await?;
        let mut status = service_result.Status().map_err(|_| Error::DeviceNotFound)?;
        // The first query can answer Unreachable while the link is already
        // up (the peripheral accepted the connection and a notification).
        // One more uncached query on the held session sees that link. A
        // second query while the link is still down can sit there until the
        // caller gives up, so it runs only when the link is already up.
        if status == GattCommunicationStatus::Unreachable && self.is_connected().await? {
            service_result = self.get_gatt_services(BluetoothCacheMode::Uncached).await?;
            status = service_result.Status().map_err(|_| Error::DeviceNotFound)?;
        }
        // UBM patch (UBM_PATCHES.md #15): a device the connect could not
        // reach is the platform's answer (`gatt-status` `unreachable`), so
        // the host can tell a link that was not established from a refusal.
        if status == GattCommunicationStatus::Unreachable {
            return Err(utils::gatt_status_error("connect", status, None));
        }
        utils::to_error(status)
    }

    async fn is_connected(&self) -> Result<bool> {
        let winrt_error = Error::from;
        let status = self.device.ConnectionStatus().map_err(winrt_error)?;

        Ok(status == BluetoothConnectionStatus::Connected)
    }

    /// UBM patch (`winrt-uncached-discovery`): always
    /// `BluetoothCacheMode::Uncached`, as the legacy addon
    /// (`winrt-boundary.inc` `Discover`). No fallback to the OS cache and no
    /// timeout: a slow query is cancelled by its caller. A peer error is
    /// named and fails discovery. `AccessDenied` with no ATT byte is Windows
    /// keeping the service (Microphone Control, HID, and the same class);
    /// that service is left out instead of failing the services around it.
    pub async fn get_characteristics(
        service: &GattDeviceService,
    ) -> Result<CharacteristicList> {
        let result = service
            .GetCharacteristicsWithCacheModeAsync(BluetoothCacheMode::Uncached)?
            .await?;
        let status = result.Status()?;
        let att_error = utils::protocol_att_error(result.ProtocolError());
        match gatt_model::characteristic_discovery(status.0, att_error) {
            gatt_model::CharacteristicDiscovery::Continue => {
                let characteristics = result.Characteristics()?;
                debug!("characteristics {:?}", characteristics.Size());
                Ok(CharacteristicList::Ready(
                    characteristics.into_iter().collect(),
                ))
            }
            gatt_model::CharacteristicDiscovery::OperatingSystemDenied => {
                Ok(CharacteristicList::OperatingSystemDenied)
            }
            gatt_model::CharacteristicDiscovery::Failed => Err(utils::gatt_status_error(
                "characteristic discovery",
                status,
                att_error,
            )),
        }
    }

    /// UBM patch (`winrt-uncached-discovery`): no WinRT descriptor query.
    /// `GetDescriptors` and `GetDescriptorsForUuid` both read
    /// Characteristic User Description when the characteristic has one.
    /// The first read returns ATT Insufficient Encryption at once. Windows
    /// remembers that and, on the next connection, waits to pair before it
    /// sends the read. A host with no pairing UI waits there until the
    /// process exits, so the services after that characteristic are never
    /// discovered. Subscribe writes the Client Characteristic Configuration
    /// descriptor through `GattCharacteristic`, which does not read the
    /// user description. The descriptor list stays empty.
    pub async fn get_characteristic_descriptors(
        _characteristic: &GattCharacteristic,
    ) -> Result<Vec<GattDescriptor>> {
        Ok(Vec::new())
    }

    pub fn get_connection_parameters(&self) -> Result<crate::api::ConnectionParameters> {
        let winrt_error = Error::from;
        let params = self.device.GetConnectionParameters().map_err(winrt_error)?;
        // ConnectionInterval is in units of 1.25ms, convert to microseconds
        let interval_us = (params.ConnectionInterval().map_err(winrt_error)? as u32) * 1250;
        let latency = params.ConnectionLatency().map_err(winrt_error)? as u16;
        // LinkTimeout is in units of 10ms, convert to microseconds
        let supervision_timeout_us = (params.LinkTimeout().map_err(winrt_error)? as u32) * 10_000;
        Ok(crate::api::ConnectionParameters {
            interval_us,
            latency,
            supervision_timeout_us,
        })
    }

    pub fn request_connection_parameters(
        &self,
        preset: crate::api::ConnectionParameterPreset,
    ) -> Result<()> {
        let winrt_error = Error::from;
        let params = match preset {
            crate::api::ConnectionParameterPreset::Balanced => {
                BluetoothLEPreferredConnectionParameters::Balanced()
            }
            crate::api::ConnectionParameterPreset::ThroughputOptimized => {
                BluetoothLEPreferredConnectionParameters::ThroughputOptimized()
            }
            crate::api::ConnectionParameterPreset::PowerOptimized => {
                BluetoothLEPreferredConnectionParameters::PowerOptimized()
            }
        }
        .map_err(winrt_error)?;
        let result = self
            .device
            .RequestPreferredConnectionParameters(&params)
            .map_err(winrt_error)?;
        let status = result.Status().map_err(winrt_error)?;
        // BluetoothLEPreferredConnectionParametersRequestStatus:
        //   Unspecified = 0, Success = 1, DeviceNotAvailable = 2, AccessDenied = 3
        match status.0 {
            1 => Ok(()),
            2 | 3 => Err(Error::NotSupported(format!(
                "request_connection_parameters not supported (status {:?})",
                status
            ))),
            _ => Err(Error::Other(
                format!(
                    "RequestPreferredConnectionParameters failed with status {:?}",
                    status
                )
                .into(),
            )),
        }
    }

    /// UBM patch (`winrt-uncached-discovery`): the device's primary
    /// services, queried `Uncached` every time (the legacy addon's
    /// `Discover`), so a discovery after `GattServicesChanged` sees the
    /// changed database. A failed query is an error naming the status.
    pub async fn discover_services(&mut self) -> Result<Vec<GattDeviceService>> {
        let service_result = self.get_gatt_services(BluetoothCacheMode::Uncached).await?;
        discovery_status("service discovery", service_result.Status())?;
        // The IVectorView is not Send, so it is collected before any await.
        let services: Vec<_> = service_result.Services()?.into_iter().collect();
        debug!("services {:?}", services.len());
        self.services = services.clone();
        Ok(services)
    }
}

impl Drop for BLEDevice {
    fn drop(&mut self) {
        // Release the hold taken in `connect` before the device is closed.
        // The desktop session releases its own hold as well; this one must
        // not keep the link up after disconnect.
        if let Err(err) = self.gatt_session.SetMaintainConnection(false) {
            debug!("Drop: clear maintain connection {:?}", err);
        }

        let result = self
            .gatt_session
            .RemoveMaxPduSizeChanged(self.pdu_change_token);
        if let Err(err) = result {
            debug!("Drop: remove_max_pdu_size_changed {:?}", err);
        }

        let result = self
            .device
            .RemoveConnectionStatusChanged(self.connection_token);
        if let Err(err) = result {
            debug!("Drop:remove_connection_status_changed {:?}", err);
        }

        self.services.iter().for_each(|service| {
            if let Err(err) = service.Close() {
                debug!("Drop:remove_gatt_Service {:?}", err);
            }
        });

        let result = self.device.Close();
        if let Err(err) = result {
            debug!("Drop:close {:?}", err);
        }
    }
}
