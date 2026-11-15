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
    Error, Result,
    api::BDAddr,
    winrtble::{gatt_model, utils},
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

#[path = "device_event_ownership.rs"]
mod device_event_ownership;
use device_event_ownership::{Event, EventTokens, close_once, retain_failed_drop};
#[cfg(test)]
#[path = "device_event_ownership_tests.rs"]
mod device_event_ownership_tests;
#[cfg(test)]
#[path = "discovery_retirement_tests.rs"]
mod discovery_retirement_tests;
#[cfg(test)]
#[path = "service_ownership_retry_tests.rs"]
mod service_ownership_retry_tests;

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
pub type ServicesChangedHandler = Box<dyn Fn() + Send>;

// Keep the typed async operation, which WinRT declares Send + Sync, rather
// than retaining its non-agile IAsyncInfo projection across executor threads.
trait DiscoveryControl: Send + Sync {
    fn pending(&self) -> windows::core::Result<bool>;
    fn cancel(&self) -> windows::core::Result<()>;
}
impl<T: RuntimeType + 'static> DiscoveryControl for IAsyncOperation<T> {
    fn pending(&self) -> windows::core::Result<bool> {
        self.Status().map(discovery_status_is_pending)
    }
    fn cancel(&self) -> windows::core::Result<()> {
        self.Cancel()
    }
}

pub(crate) fn discovery_status_is_pending(status: AsyncStatus) -> bool {
    !matches!(status, AsyncStatus::Completed | AsyncStatus::Error)
}

// The published graph, an in-progress graph, and failed/replaced graphs may
// all own native services. A WinRT service can be returned by multiple queries;
// close each exact object once, only after all graph registrations are retired.
pub(crate) struct ServiceOwners<T> {
    active: Vec<T>,
    candidate: Vec<T>,
    retired: Vec<T>,
}

impl<T> Default for ServiceOwners<T> {
    fn default() -> Self { Self { active: Vec::new(), candidate: Vec::new(), retired: Vec::new() } }
}

impl<T: PartialEq> ServiceOwners<T> {
    pub(crate) fn prepare_candidate<E>(
        &mut self,
        quiesce: impl FnOnce() -> std::result::Result<(), E>,
        close: impl FnMut(&T) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        quiesce()?;
        self.begin_candidate();
        self.close_retired(close)
    }
    pub(crate) fn begin_candidate(&mut self) {
        self.retired.append(&mut self.candidate);
    }
    pub(crate) fn retain_candidate(&mut self, service: T) {
        if !self.candidate.contains(&service) { self.candidate.push(service); }
    }
    pub(crate) fn publish_candidate(&mut self) {
        self.retired.append(&mut self.active);
        self.active = std::mem::take(&mut self.candidate);
    }
    pub(crate) fn close_retired<E>(&mut self, mut close: impl FnMut(&T) -> std::result::Result<(), E>) -> std::result::Result<(), E> {
        while let Some(service) = self.retired.last() {
            if self.active.contains(service) || self.candidate.contains(service) {
                self.retired.pop();
                continue;
            }
            close(service)?;
            let service = self.retired.pop().unwrap();
            self.retired.retain(|other| other != &service);
        }
        Ok(())
    }
    pub(crate) fn close_all<E>(&mut self, mut close: impl FnMut(&T) -> std::result::Result<(), E>) -> std::result::Result<(), E> {
        while let Some(service) = self.retired.last().or_else(|| self.candidate.last()).or_else(|| self.active.last()) {
            close(service)?;
            // Remove all aliases after the one actual close succeeds.
            let service = if !self.retired.is_empty() { self.retired.pop().unwrap() }
                else if !self.candidate.is_empty() { self.candidate.pop().unwrap() }
                else { self.active.pop().unwrap() };
            self.retired.retain(|other| other != &service);
            self.candidate.retain(|other| other != &service);
            self.active.retain(|other| other != &service);
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool { self.active.is_empty() && self.candidate.is_empty() && self.retired.is_empty() }
}
fn retire_discovery_operations(operations: &OperationPool<DiscoveryOperation>) -> Result<()> {
    operations.retire().map_err(|failures| {
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
    address: BDAddr,
    address_type: Option<crate::api::AddressType>,
    device: BluetoothLEDevice,
    gatt_session: GattSession,
    event_tokens: EventTokens,
    session_released: bool,
    session_closed: bool,
    device_closed: bool,
    drop_retained: bool,
    services: ServiceOwners<GattDeviceService>,
    discovery_operations: OperationPool<DiscoveryOperation>,
    preferred_request:
        crate::request_lifetime::RequestLifetime<BluetoothLEPreferredConnectionParametersRequest>,
}

/// A failed constructor owns any resources whose rollback did not complete.
pub struct BLEDeviceConstructionError {
    pub error: Error,
    pub cleanup_error: Option<Error>,
    pub owner: Option<PartialBLEDevice>,
}

impl From<Error> for BLEDeviceConstructionError {
    fn from(error: Error) -> Self {
        Self {
            error,
            cleanup_error: None,
            owner: None,
        }
    }
}

struct PartialResources {
    address: BDAddr,
    address_type: Option<crate::api::AddressType>,
    device: BluetoothLEDevice,
    session: Option<GattSession>,
    events: EventTokens,
    closed: bool,
}

/// A partially constructed device. Keep it until `close` succeeds.
pub struct PartialBLEDevice(Option<PartialResources>);

impl PartialBLEDevice {
    pub fn close(&mut self) -> Result<()> {
        let Some(resources) = self.0.as_mut() else {
            return Ok(());
        };
        resources.events.remove_all(|event, token| {
            match event {
                Event::Connection => resources.device.RemoveConnectionStatusChanged(token),
                Event::Pdu => resources
                    .session
                    .as_ref()
                    .expect("PDU token requires a session")
                    .RemoveMaxPduSizeChanged(token),
                Event::Parameters => resources.device.RemoveConnectionParametersChanged(token),
                Event::Services => resources.device.RemoveGattServicesChanged(token),
            }
            .map_err(Error::from)
        })?;
        if let Some(session) = resources.session.as_ref() {
            session.Close().map_err(Error::from)?;
            resources.session = None;
        }
        close_once(&mut resources.closed, || {
            resources.device.Close().map_err(Error::from)
        })?;
        Ok(())
    }

    fn failed(mut self, error: Error) -> BLEDeviceConstructionError {
        match self.close() {
            Ok(()) => BLEDeviceConstructionError {
                error,
                cleanup_error: None,
                owner: None,
            },
            Err(cleanup_error) => BLEDeviceConstructionError {
                error,
                cleanup_error: Some(cleanup_error),
                owner: Some(self),
            },
        }
    }
}

/// Ownership recovered from a dropped constructor or device after cleanup failed.
pub enum RetainedBLEDevice {
    Partial(PartialBLEDevice),
    Complete(BLEDevice),
}

impl RetainedBLEDevice {
    /// The originally requested identity, including an unresolved address type.
    pub fn identity(&self) -> (BDAddr, Option<crate::api::AddressType>) {
        match self {
            Self::Partial(owner) => {
                let resources = owner.0.as_ref().expect("retained partial owner exists");
                (resources.address, resources.address_type)
            }
            Self::Complete(owner) => (owner.address, owner.address_type),
        }
    }

    pub fn close(&mut self) -> Result<()> {
        match self {
            Self::Partial(owner) => owner.close(),
            Self::Complete(owner) => owner.close(),
        }
    }
}

static RETAINED_DEVICES: std::sync::Mutex<Vec<RetainedBLEDevice>> =
    std::sync::Mutex::new(Vec::new());

type RecoveryIdentity = (BDAddr, Option<crate::api::AddressType>);
static RECOVERY_IN_PROGRESS: std::sync::Mutex<Vec<RecoveryIdentity>> =
    std::sync::Mutex::new(Vec::new());

fn identities_overlap(a: RecoveryIdentity, b: RecoveryIdentity) -> bool {
    a.0 == b.0 && (a.1.is_none() || b.1.is_none() || a.1 == b.1)
}

/// Serializes cleanup and replacement for identities that may denote the same peer.
pub(crate) struct RecoveryLease(RecoveryIdentity);

impl RecoveryLease {
    pub(crate) fn acquire(
        address: BDAddr,
        kind: Option<crate::api::AddressType>,
    ) -> Result<Self> {
        let identity = (address, kind);
        let mut active = RECOVERY_IN_PROGRESS
            .lock()
            .map_err(|_| Error::Other("WinRT recovery lock poisoned".into()))?;
        // An unresolved queued owner can overlap either address type, so both
        // typed peers with the same address must serialize while checking it.
        if active.iter().any(|other| identity.0 == other.0) {
            return Err(Error::Other("WinRT peer recovery is pending; retry".into()));
        }
        active.push(identity);
        Ok(Self(identity))
    }

    pub(crate) fn recover(&self) -> Result<()> {
        let mut owners = {
            let mut queue = RETAINED_DEVICES
                .lock()
                .map_err(|_| Error::Other("WinRT recovery queue lock poisoned".into()))?;
            let mut owners = Vec::new();
            let mut index = 0;
            while index < queue.len() {
                if identities_overlap(self.0, queue[index].identity()) {
                    owners.push(queue.remove(index));
                } else {
                    index += 1;
                }
            }
            owners
        };
        let result = recover_matching(&mut owners, |_| true, RetainedBLEDevice::close);
        RETAINED_DEVICES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .append(&mut owners);
        result
    }
}

impl Drop for RecoveryLease {
    fn drop(&mut self) {
        let mut active = RECOVERY_IN_PROGRESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = active.iter().position(|identity| *identity == self.0) {
            active.remove(index);
        }
    }
}

pub(crate) use super::super::callback_retirement::recover_matching;

#[cfg(test)]
#[path = "lifecycle_recovery_portable_tests.rs"]
mod lifecycle_recovery_portable_tests;
#[cfg(test)]
#[path = "lifecycle_claim_portable_tests.rs"]
mod lifecycle_claim_portable_tests;

fn retain_device(owner: RetainedBLEDevice) {
    RETAINED_DEVICES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(owner);
}

impl Drop for PartialBLEDevice {
    fn drop(&mut self) {
        retain_failed_drop(self, Self::close, |owner, error| {
            log::error!("WinRT partial device cleanup retained for retry: {error}");
            retain_device(RetainedBLEDevice::Partial(PartialBLEDevice(owner.0.take())));
        });
    }
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
    use windows::Foundation::Metadata::ApiInformation;
    use windows::core::HSTRING;
    ApiInformation::IsMethodPresent(
        &HSTRING::from("Windows.Devices.Bluetooth.BluetoothLEDevice"),
        &HSTRING::from("GetConnectionPhy"),
    )
    .map_err(Error::from)
}

pub fn preferred_parameters_api_present() -> Result<bool> {
    use windows::Foundation::Metadata::ApiInformation;
    use windows::core::HSTRING;
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
        let result = super::terminal_result(operation).await.map_err(Error::from);
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
        retire_discovery_operations(&self.discovery_operations)
    }
    pub async fn new(
        address: BDAddr,
        address_type: Option<crate::api::AddressType>,
        connection_status_changed: ConnectedEventHandler,
        max_pdu_size_changed: MaxPduSizeChangedEventHandler,
        connection_parameters_changed: ConnectionParametersHandler,
        services_changed: ServicesChangedHandler,
    ) -> Result<Self> {
        match Self::new_retained(
            address,
            address_type,
            connection_status_changed,
            max_pdu_size_changed,
            connection_parameters_changed,
            services_changed,
        )
        .await
        {
            Ok(device) => Ok(device),
            Err(failure) => {
                let BLEDeviceConstructionError {
                    error,
                    cleanup_error,
                    owner,
                } = failure;
                // A failed owner's Drop retains it for explicit recovery if cleanup still fails.
                drop(owner);
                Err(match cleanup_error {
                    Some(cleanup) => Error::WithCleanup {
                        primary: Box::new(error),
                        cleanup: Box::new(cleanup),
                    },
                    None => error,
                })
            }
        }
    }

    /// Construct with explicit ownership of any incomplete rollback. The caller
    /// must retain `owner` and retry its close before admitting a replacement.
    pub async fn new_retained(
        address: BDAddr,
        address_type: Option<crate::api::AddressType>,
        connection_status_changed: ConnectedEventHandler,
        max_pdu_size_changed: MaxPduSizeChangedEventHandler,
        connection_parameters_changed: ConnectionParametersHandler,
        services_changed: ServicesChangedHandler,
    ) -> std::result::Result<Self, BLEDeviceConstructionError> {
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
        let device = super::terminal_result(async_op)
            .await
            .map_err(|_| Error::DeviceNotFound)?;

        let mut partial = PartialBLEDevice(Some(PartialResources {
            address,
            address_type,
            device,
            session: None,
            events: EventTokens::default(),
            closed: false,
        }));
        let result = async {
            let resources = partial.0.as_mut().expect("partial owner exists");
            let async_op = GattSession::FromDeviceIdAsync(&resources.device.BluetoothDeviceId()?)
                .map_err(|_| Error::DeviceNotFound)?;
            resources.session = Some(
                super::terminal_result(async_op)
                    .await
                    .map_err(|_| Error::DeviceNotFound)?,
            );
            Ok::<_, Error>(())
        }
        .await;
        if let Err(error) = result {
            return Err(partial.failed(error));
        }
        let resources = partial.0.as_mut().expect("partial owner exists");
        let device = &resources.device;
        let gatt_session = resources.session.as_ref().expect("session established");
        let result = (|| -> Result<()> {
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
                .map_err(Error::from)?;
            resources.events.record(Event::Connection, connection_token);

            max_pdu_size_changed(gatt_session.MaxPduSize().map_err(Error::from)?);
            let max_pdu_size_changed_handler =
                TypedEventHandler::<GattSession, _>::new(move |sender, _| {
                    if let Some(sender) = sender.as_ref() {
                        max_pdu_size_changed(sender.MaxPduSize()?);
                    }
                    Ok(())
                });
            let pdu_change_token = gatt_session
                .MaxPduSizeChanged(&max_pdu_size_changed_handler)
                .map_err(Error::from)?;
            resources.events.record(Event::Pdu, pdu_change_token);

            if parameter_api_present {
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
                let token = device
                    .ConnectionParametersChanged(&parameters_handler)
                    .map_err(Error::from)?;
                resources.events.record(Event::Parameters, token);
            }

            let services_handler = TypedEventHandler::<BluetoothLEDevice, _>::new(move |_, _| {
                services_changed();
                Ok(())
            });
            let services_changed_token = device
                .GattServicesChanged(&services_handler)
                .map_err(Error::from)?;
            resources
                .events
                .record(Event::Services, services_changed_token);
            Ok(())
        })();
        if let Err(error) = result {
            return Err(partial.failed(error));
        }
        let resources = partial.0.take().expect("partial owner exists");
        Ok(BLEDevice {
            address: resources.address,
            address_type: resources.address_type,
            device: resources.device,
            gatt_session: resources.session.expect("session established"),
            event_tokens: resources.events,
            session_released: false,
            session_closed: false,
            device_closed: false,
            drop_retained: false,
            services: ServiceOwners::default(),
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
            return Err(Error::Platform(crate::PlatformError::new(
                "winrt",
                "winrt-connection-phy-requires-windows-11-22000",
                "GetConnectionPhy is absent; Windows 11 build 22000 or newer is required",
            )));
        }
        let phy = self.device.GetConnectionPhy().map_err(Error::from)?;
        let read = |info: windows::Devices::Bluetooth::BluetoothLEConnectionPhyInfo| {
            crate::connection_phy_source::direction(
                info.IsUncoded1MPhy().map_err(Error::from)?,
                info.IsUncoded2MPhy().map_err(Error::from)?,
                info.IsCodedPhy().map_err(Error::from)?,
            )
        };
        Ok(crate::connection_phy_source::ConnectionPhy {
            tx: read(phy.TransmitInfo().map_err(Error::from)?)?,
            rx: read(phy.ReceiveInfo().map_err(Error::from)?)?,
        })
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
        for service in &services { self.services.retain_candidate(service.clone()); }
        Ok(services)
    }

    pub fn retain_included_service(&mut self, service: &GattDeviceService) {
        self.services.retain_candidate(service.clone());
    }

    pub fn publish_services(&mut self) {
        self.services.publish_candidate();
    }

    // The caller first retires displaced registrations. A failed handoff's
    // installed table remains active, while an uninstalled candidate is safe
    // to retire only once all exact discovery operations are quiescent.
    pub fn prepare_service_candidate(&mut self) -> Result<()> {
        let operations = &self.discovery_operations;
        self.services.prepare_candidate(
            || retire_discovery_operations(operations),
            |service| service.Close().map_err(Error::from),
        )
    }

    pub fn close_retired_services(&mut self) -> Result<()> {
        self.retire_discovery_operations()?;
        self.services.close_retired(|service| service.Close().map_err(Error::from))
    }
}

#[cfg(test)]
fn remove_token(token: &mut Option<i64>, remove: impl FnOnce(i64) -> Result<()>) -> Result<()> {
    if let Some(value) = *token {
        remove(value)?;
        *token = None;
    }
    Ok(())
}

impl BLEDevice {
    /// Retire all native ownership explicitly. Each completed step is recorded
    /// so a failed close can be retried without repeating successful operations.
    pub fn close(&mut self) -> Result<()> {
        self.retire_discovery_operations()?;
        self.close_preferred_request()?;
        close_once(&mut self.session_released, || {
            self.gatt_session
                .SetMaintainConnection(false)
                .map_err(Error::from)
        })?;
        self.event_tokens.remove(Event::Pdu, |token| {
            self.gatt_session
                .RemoveMaxPduSizeChanged(token)
                .map_err(Error::from)
        })?;
        self.event_tokens.remove(Event::Parameters, |token| {
            self.device
                .RemoveConnectionParametersChanged(token)
                .map_err(Error::from)
        })?;
        self.event_tokens.remove(Event::Services, |token| {
            self.device
                .RemoveGattServicesChanged(token)
                .map_err(Error::from)
        })?;
        self.services.close_all(|service| service.Close().map_err(Error::from))?;
        self.event_tokens.remove(Event::Connection, |token| {
            self.device
                .RemoveConnectionStatusChanged(token)
                .map_err(Error::from)
        })?;
        close_once(&mut self.session_closed, || {
            self.gatt_session.Close().map_err(Error::from)
        })?;
        close_once(&mut self.device_closed, || {
            self.device.Close().map_err(Error::from)
        })?;
        Ok(())
    }
}

impl Drop for BLEDevice {
    fn drop(&mut self) {
        if self.drop_retained {
            return;
        }
        retain_failed_drop(self, Self::close, |owner, error| {
            log::error!("WinRT device cleanup retained for retry: {error}");
            owner.drop_retained = true;
            retain_device(RetainedBLEDevice::Complete(BLEDevice {
                address: owner.address,
                address_type: owner.address_type,
                device: owner.device.clone(),
                gatt_session: owner.gatt_session.clone(),
                event_tokens: std::mem::take(&mut owner.event_tokens),
                session_released: owner.session_released,
                session_closed: owner.session_closed,
                device_closed: owner.device_closed,
                drop_retained: false,
                services: std::mem::take(&mut owner.services),
                discovery_operations: std::mem::take(&mut owner.discovery_operations),
                preferred_request: std::mem::take(&mut owner.preferred_request),
            }));
        });
    }
}

#[cfg(test)]
mod close_tests {
    use super::{Error, close_once, remove_token};

    #[test]
    fn failed_close_is_retryable_and_completed_steps_are_not_repeated() {
        let mut closed = false;
        let mut token = Some(17);
        assert!(remove_token(&mut token, |_| Err(Error::NotConnected)).is_err());
        assert_eq!(token, Some(17));
        remove_token(&mut token, |value| {
            assert_eq!(value, 17);
            Ok(())
        })
        .unwrap();
        remove_token(&mut token, |_| panic!("completed removal repeated")).unwrap();
        assert!(close_once(&mut closed, || Err(Error::NotConnected)).is_err());
        assert!(!closed);
        close_once(&mut closed, || Ok::<(), Error>(())).unwrap();
        close_once(&mut closed, || -> Result<(), Error> {
            panic!("completed close repeated")
        })
        .unwrap();
        assert!(closed);
    }
}
