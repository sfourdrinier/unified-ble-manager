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

use crate::active_operations::{NativeOperation, OperationPool, RetirementError};
use crate::{
    api::BDAddr,
    winrtble::{gatt_model, utils},
    Error, Result,
};
use log::{debug, trace};
use windows::core::RuntimeType;
use windows::{
    Devices::Bluetooth::{
        BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice,
        BluetoothLEPreferredConnectionParameters, BluetoothLEPreferredConnectionParametersRequest,
        GenericAttributeProfile::{
            GattCharacteristic, GattCommunicationStatus, GattDescriptor, GattDeviceService,
            GattDeviceServicesResult, GattSession,
        },
    },
    Foundation::TypedEventHandler,
};
use windows_future::{AsyncStatus, IAsyncOperation};

/// UBM patch (`winrt-uncached-discovery`): a non-success status of one
/// service-discovery query, named, as the discovery's error. The result's
/// protocol byte is kept when Windows reported one.
fn discovery_status(stage: &str, service_result: &GattDeviceServicesResult) -> Result<()> {
    let status = service_result.Status()?;
    if status == GattCommunicationStatus::Success {
        return Ok(());
    }
    let att_error = utils::protocol_att_error(service_result.ProtocolError());
    Err(utils::gatt_status_error(stage, status, att_error))
}

pub type ConnectedEventHandler = Box<dyn Fn(bool) + Send>;
pub type MaxPduSizeChangedEventHandler = Box<dyn Fn(u16) + Send>;
pub type ConnectionParametersHandler = Box<dyn Fn(crate::api::ConnectionParametersReport) + Send>;

// Keep the typed async operation, which WinRT declares Send + Sync, rather
// than retaining its non-agile IAsyncInfo projection across executor threads.
trait DiscoveryControl: Send + Sync {
    fn pending(&self) -> windows::core::Result<bool>;
    fn cancel(&self) -> windows::core::Result<()>;
}
impl<T: RuntimeType + 'static> DiscoveryControl for IAsyncOperation<T> {
    fn pending(&self) -> windows::core::Result<bool> {
        self.Status().map(|status| status == AsyncStatus::Started)
    }
    fn cancel(&self) -> windows::core::Result<()> {
        self.Cancel()
    }
}
#[derive(Clone)]
struct DiscoveryOperation(std::sync::Arc<dyn DiscoveryControl>);
impl NativeOperation for DiscoveryOperation {
    type Error = windows::core::Error;
    fn pending(&self) -> windows::core::Result<bool> {
        self.0.pending()
    }
    fn cancel(&self) -> windows::core::Result<()> {
        self.0.cancel()
    }
}

pub struct BLEDevice {
    device: BluetoothLEDevice,
    gatt_session: GattSession,
    connection_token: i64,
    pdu_change_token: i64,
    /// Present only when this OS exposes `ConnectionParametersChanged`.
    connection_parameters_token: Option<i64>,
    services: Vec<GattDeviceService>,
    discovery_operations: OperationPool<DiscoveryOperation>,
    preferred_request:
        crate::request_lifetime::RequestLifetime<BluetoothLEPreferredConnectionParametersRequest>,
}

/// `GetConnectionParameters` exists from Windows 11 build 22000. Older
/// Windows is a real limitation, not an empty success.
pub fn connection_parameters_api_present() -> Result<bool> {
    let device = windows::core::HSTRING::from("Windows.Devices.Bluetooth.BluetoothLEDevice");
    let getter = windows::Foundation::Metadata::ApiInformation::IsMethodPresent(
        &device,
        &windows::core::HSTRING::from("GetConnectionParameters"),
    )
    .map_err(Error::from)?;
    let events = windows::Foundation::Metadata::ApiInformation::IsEventPresent(
        &device,
        &windows::core::HSTRING::from("ConnectionParametersChanged"),
    )
    .map_err(Error::from)?;
    Ok(getter && events)
}

pub fn connection_phy_api_present() -> Result<bool> {
    use windows::core::HSTRING;
    use windows::Foundation::Metadata::ApiInformation;
    ApiInformation::IsMethodPresent(&HSTRING::from("Windows.Devices.Bluetooth.BluetoothLEDevice"), &HSTRING::from("GetConnectionPhy")).map_err(Error::from)
}

pub fn preferred_parameters_api_present() -> Result<bool> {
    use windows::core::HSTRING;
    use windows::Foundation::Metadata::ApiInformation;
    let device = HSTRING::from("Windows.Devices.Bluetooth.BluetoothLEDevice");
    let presets =
        HSTRING::from("Windows.Devices.Bluetooth.BluetoothLEPreferredConnectionParameters");
    if !ApiInformation::IsMethodPresent(
        &device,
        &HSTRING::from("RequestPreferredConnectionParameters"),
    )? {
        return Ok(false);
    }
    for property in ["Balanced", "ThroughputOptimized", "PowerOptimized"] {
        if !ApiInformation::IsPropertyPresent(&presets, &HSTRING::from(property))? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn connection_parameters_unavailable() -> Error {
    Error::Platform(crate::PlatformError::new(
        "winrt",
        "winrt-connection-parameters-requires-windows-11-22000",
        "GetConnectionParameters is absent; Windows 11 build 22000 or newer is required",
    ))
}

fn read_connection_parameters(
    device: &BluetoothLEDevice,
) -> Result<crate::api::ConnectionParameters> {
    if !connection_parameters_api_present()? {
        return Err(connection_parameters_unavailable());
    }
    let winrt_error = Error::from;
    let params = device.GetConnectionParameters().map_err(winrt_error)?;
    crate::connection_parameters_source::parameter_answer(
        params.ConnectionInterval().map_err(winrt_error)?,
        params.ConnectionLatency().map_err(winrt_error)?,
        params.LinkTimeout().map_err(winrt_error)?,
    )
}

/// Outcome of one service's characteristic query.
pub enum CharacteristicList {
    /// The service listed its characteristics.
    Ready(Vec<GattCharacteristic>),
    /// AccessDenied with no ATT byte. The service stays in the table
    /// with no characteristics. This is not an OS-reserved omission.
    AccessDenied,
}

impl BLEDevice {
    async fn await_discovery<T: RuntimeType + 'static>(
        &self,
        operation: IAsyncOperation<T>,
        stage: &'static str,
    ) -> Result<T> {
        let control = DiscoveryOperation(std::sync::Arc::new(operation.clone()));
        // IAsyncOperation::Cancel can report Canceled while WinRT still
        // initializes GATT characteristics (including its descriptor reads).
        // Closing the service then blocks on that initialization's lock. The
        // caller may stop waiting, but the exact hot query remains owned until
        // its natural terminal state; cleanup refuses promptly while pending.
        let guard = self
            .discovery_operations
            .admit(control.clone(), false, stage);
        let result = operation.await.map_err(Error::from);
        match &result {
            Ok(_) => guard.complete(),
            Err(_) => match control.pending() {
                Ok(false) => guard.complete(),
                Ok(true) => drop(guard),
                Err(error) => {
                    drop(guard);
                    return match result {
                        Err(primary) => Err(Error::WithCleanup {
                            primary: Box::new(primary),
                            cleanup: Box::new(Error::from(error)),
                        }),
                        Ok(value) => Ok(value),
                    };
                }
            },
        }
        result
    }

    /// Do not close GATT services while an interrupted native query still owns
    /// them. Public cancellation stops waiting without calling WinRT Cancel,
    /// whose Canceled status does not establish internal GATT quiescence.
    /// Cleanup refuses with retained ownership until the original query finishes,
    /// permitting the same owner to retry.
    pub fn retire_discovery_operations(&self) -> Result<()> {
        self.discovery_operations.retire().map_err(|failures| {
            let mut errors = failures.into_iter().map(|(stage, failure)| match failure {
                RetirementError::Native(error) => Error::Platform(
                    crate::PlatformError::new("winrt", "hresult", error.message().to_string())
                        .with("hresult", gatt_model::hresult_code(error.code().0))
                        .with("operation", stage),
                ),
                RetirementError::Pending => Error::Platform(
                    crate::PlatformError::new("winrt", "discovery-retirement-pending",
                        "The native discovery query is still active; cleanup remains owned and retryable")
                        .with("operation", stage),
                ),
            });
            let Some(mut primary) = errors.next() else {
                return Error::Other("Native discovery retirement failed without an error".into());
            };
            for cleanup in errors {
                primary = Error::WithCleanup { primary: Box::new(primary), cleanup: Box::new(cleanup) };
            }
            primary
        })
    }
    pub async fn new(
        address: BDAddr,
        address_type: Option<crate::api::AddressType>,
        connection_status_changed: ConnectedEventHandler,
        max_pdu_size_changed: MaxPduSizeChangedEventHandler,
        connection_parameters_changed: ConnectionParametersHandler,
    ) -> Result<Self> {
        // Probe before any event registration acquires a native token.
        let parameter_api_present = connection_parameters_api_present()?;
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

        let connection_parameters_token = if parameter_api_present {
            let parameters_handler =
                TypedEventHandler::<BluetoothLEDevice, _>::new(move |sender, _| {
                    let report = crate::connection_parameters_source::callback_answer(|| {
                        match sender.as_ref() {
                            Some(sender) => read_connection_parameters(sender),
                            None => Err(Error::Platform(crate::PlatformError::new(
                                "winrt",
                                "connection-parameter-source-missing",
                                "ConnectionParametersChanged supplied no device",
                            ))),
                        }
                    });
                    connection_parameters_changed(report);
                    Ok(())
                });
            Some(
                device
                    .ConnectionParametersChanged(&parameters_handler)
                    .map_err(|_| {
                        Error::Other("Could not add connection parameters handler".into())
                    })?,
            )
        } else {
            None
        };

        Ok(BLEDevice {
            device,
            gatt_session,
            connection_token,
            pdu_change_token,
            connection_parameters_token,
            services: vec![],
            discovery_operations: Default::default(),
            preferred_request: Default::default(),
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
        let service_result = self.await_discovery(async_op, "service discovery").await?;
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
        let status = service_result.Status().map_err(Error::from)?;
        // The first query can answer Unreachable while the link is already
        // up (the peripheral accepted the connection and a notification).
        // One more uncached query on the held session sees that link. A
        // second query while the link is still down can sit there until the
        // caller gives up, so it runs only when the link is already up.
        if status == GattCommunicationStatus::Unreachable && self.is_connected().await? {
            service_result = self.get_gatt_services(BluetoothCacheMode::Uncached).await?;
        }
        // UBM patch (UBM_PATCHES.md #15): a device the connect could not
        // reach is the platform's answer (`gatt-status` `unreachable`), so
        // the host can tell a link that was not established from a refusal.
        discovery_status("connect", &service_result)
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
    /// named and fails discovery. `AccessDenied` with no ATT byte keeps the
    /// service identity and reports the denial. Known OS-reserved UUIDs are
    /// not queried.
    pub async fn get_characteristics(
        &self,
        service: &GattDeviceService,
    ) -> Result<CharacteristicList> {
        let operation =
            service.GetCharacteristicsWithCacheModeAsync(BluetoothCacheMode::Uncached)?;
        let result = self
            .await_discovery(operation, "characteristic discovery")
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
            gatt_model::CharacteristicDiscovery::AccessDenied => {
                Ok(CharacteristicList::AccessDenied)
            }
            gatt_model::CharacteristicDiscovery::Failed => Err(utils::gatt_status_error(
                "characteristic discovery",
                status,
                att_error,
            )),
        }
    }

    pub async fn get_included_services(
        &self,
        service: &GattDeviceService,
    ) -> Result<Option<Vec<GattDeviceService>>> {
        let operation =
            service.GetIncludedServicesWithCacheModeAsync(BluetoothCacheMode::Uncached)?;
        let result = self
            .await_discovery(operation, "included service discovery")
            .await?;
        let status = result.Status()?;
        let att_error = utils::protocol_att_error(result.ProtocolError());
        match gatt_model::characteristic_discovery(status.0, att_error) {
            gatt_model::CharacteristicDiscovery::Continue => {
                Ok(Some(result.Services()?.into_iter().collect()))
            }
            gatt_model::CharacteristicDiscovery::AccessDenied => Ok(None),
            gatt_model::CharacteristicDiscovery::Failed => Err(utils::gatt_status_error(
                "included service discovery",
                status,
                att_error,
            )),
        }
    }

    /// Enumerate this characteristic's descriptors with one owned uncached
    /// `GetDescriptors` operation. Success returns the list Windows
    /// returned, which is empty only when the peer listed none. Any other
    /// status is a platform error carrying the ATT byte when the result
    /// had one. A discovery deadline stops waiting while retaining the exact
    /// native query. Pending work prevents service close or a replacement
    /// discovery until natural completion. The library does not initiate pairing
    /// or explicitly read descriptor values; Windows may read descriptor metadata
    /// while materializing characteristics. Subscribe still writes the CCCD
    /// through `GattCharacteristic`.
    pub async fn get_characteristic_descriptors(
        &self,
        characteristic: &GattCharacteristic,
    ) -> Result<Vec<GattDescriptor>> {
        let operation = characteristic
            .GetDescriptorsWithCacheModeAsync(BluetoothCacheMode::Uncached)
            .map_err(Error::from)?;
        let result = self
            .await_discovery(operation, "descriptor discovery")
            .await?;
        let status = result.Status().map_err(Error::from)?;
        let att_error = utils::protocol_att_error(result.ProtocolError());
        if gatt_model::descriptor_enumeration(status.0) == gatt_model::DescriptorEnumeration::Listed
        {
            let descriptors = result.Descriptors().map_err(Error::from)?;
            debug!("descriptors {:?}", descriptors.Size());
            return Ok(descriptors.into_iter().collect());
        }
        Err(utils::gatt_status_error(
            "descriptor discovery",
            status,
            att_error,
        ))
    }

    pub fn get_connection_phy(&self) -> Result<crate::connection_phy_source::ConnectionPhy> {
        if !connection_phy_api_present()? {
            return Err(Error::Platform(crate::PlatformError::new("winrt", "winrt-connection-phy-requires-windows-11-22000", "GetConnectionPhy is absent; Windows 11 build 22000 or newer is required")));
        }
        let phy = self.device.GetConnectionPhy().map_err(Error::from)?;
        let read = |info: windows::Devices::Bluetooth::BluetoothLEConnectionPhyInfo| {
            crate::connection_phy_source::direction(info.IsUncoded1MPhy().map_err(Error::from)?, info.IsUncoded2MPhy().map_err(Error::from)?, info.IsCodedPhy().map_err(Error::from)?)
        };
        Ok(crate::connection_phy_source::ConnectionPhy { tx: read(phy.TransmitInfo().map_err(Error::from)?)?, rx: read(phy.ReceiveInfo().map_err(Error::from)?)? })
    }

    pub fn get_connection_parameters(&self) -> Result<crate::api::ConnectionParameters> {
        read_connection_parameters(&self.device)
    }

    pub fn request_connection_parameters(
        &mut self,
        preset: crate::api::ConnectionParameterPreset,
    ) -> Result<()> {
        if !preferred_parameters_api_present()? {
            return Err(Error::Platform(crate::PlatformError::new(
                "winrt",
                "winrt-preferred-parameters-requires-windows-11-22000",
                "WinRT preferred connection parameters are absent on this runtime",
            )));
        }
        self.close_preferred_request()?;
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
        self.preferred_request.retain(result.clone());
        let status = match result.Status() {
            Ok(status) => status,
            Err(error) => return Err(self.retire_failed_preference(Error::from(error))),
        };
        // BluetoothLEPreferredConnectionParametersRequestStatus:
        //   Unspecified = 0, Success = 1, DeviceNotAvailable = 2, AccessDenied = 3
        let code = match status.0 {
            1 => return Ok(()),
            2 => "device-not-available",
            3 => "access-denied",
            _ => "unspecified",
        };
        let error = Error::Platform(
            crate::PlatformError::new(
                "winrt",
                code,
                format!("RequestPreferredConnectionParameters status {}", status.0),
            )
            .with("requestStatus", status.0.to_string()),
        );
        Err(self.retire_failed_preference(error))
    }

    pub fn close_preferred_request(&mut self) -> Result<()> {
        self.preferred_request
            .close(|request| request.Close().map_err(Error::from))
    }

    fn retire_failed_preference(&mut self, primary: Error) -> Error {
        match self.close_preferred_request() {
            Ok(()) => primary,
            Err(cleanup) => Error::WithCleanup {
                primary: Box::new(primary),
                cleanup: Box::new(cleanup),
            },
        }
    }

    /// UBM patch (`winrt-uncached-discovery`): the device's primary
    /// services, queried `Uncached` every time (the legacy addon's
    /// `Discover`), so a discovery after `GattServicesChanged` sees the
    /// changed database. A failed query is an error naming the status.
    pub async fn discover_services(&mut self) -> Result<Vec<GattDeviceService>> {
        let service_result = self.get_gatt_services(BluetoothCacheMode::Uncached).await?;
        discovery_status("service discovery", &service_result)?;
        // The IVectorView is not Send, so it is collected before any await.
        let services: Vec<_> = service_result.Services()?.into_iter().collect();
        debug!("services {:?}", services.len());
        self.services = services.clone();
        Ok(services)
    }

    pub fn retain_included_service(&mut self, service: &GattDeviceService) {
        if !self.services.contains(service) {
            self.services.push(service.clone());
        }
    }
}

impl Drop for BLEDevice {
    fn drop(&mut self) {
        if let Err(error) = self.retire_discovery_operations() {
            log::error!("Drop: active discovery prevents GATT service close: {error}");
            return;
        }
        if let Err(error) = self.close_preferred_request() {
            log::error!("Drop: preferred-parameter request cleanup failed: {error}");
        }
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

        if let Some(token) = self.connection_parameters_token {
            if let Err(err) = self.device.RemoveConnectionParametersChanged(token) {
                debug!("Drop: remove_connection_parameters_changed {:?}", err);
            }
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
