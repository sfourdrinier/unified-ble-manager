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

use super::{
    advertisement_data_type,
    ble::characteristic::{BLECharacteristic, NotifyEventHandler},
    ble::descriptor::{AttributeKey, BLEDescriptor},
    ble::device::BLEDevice,
    ble::service::BLEService,
    gatt_model::{self, index_unique},
    utils,
};
use crate::{
    Error, Result,
    api::{
        self, AddressType, BDAddr, CentralEvent, Characteristic, ConnectionParameterPreset,
        ConnectionParameters, Descriptor, Peripheral as ApiPeripheral, PeripheralProperties,
        Service, ValueNotification, WriteType,
    },
    common::{adapter_manager::AdapterManager, util::notifications_stream_from_broadcast_receiver},
};
use async_trait::async_trait;
use dashmap::DashMap;
use futures::stream::Stream;
use log::{trace, warn};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "serde")]
use serde_cr as serde;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt::{self, Debug, Display, Formatter},
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU16, Ordering},
    sync::{Arc, RwLock},
};
use tokio::sync::broadcast;
use uuid::Uuid;

use std::sync::Weak;
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattClientCharacteristicConfigurationDescriptorValue, GattDescriptor,
    GattDeviceService,
};
use windows::Devices::Bluetooth::{Advertisement::*, BluetoothAddressType};

#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_cr")
)]
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PeripheralId(BDAddr);

impl PeripheralId {
    /// UBM patch (UBM_PATCHES.md #19): the peripheral's Bluetooth address.
    pub(crate) fn address(&self) -> BDAddr {
        self.0
    }
}

impl Display for PeripheralId {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

/// Implementation of [api::Peripheral](crate::api::Peripheral).
#[derive(Clone)]
pub struct Peripheral {
    shared: Arc<Shared>,
}

struct Shared {
    device: tokio::sync::Mutex<Option<BLEDevice>>,
    adapter: Weak<AdapterManager<Peripheral>>,
    address: BDAddr,
    mtu: AtomicU16,
    connected: AtomicBool,
    /// UBM patch (`winrt-attribute-instances`): keyed by (UUID,
    /// `AttributeHandle`), so repeated service UUIDs stay distinct.
    ble_services: DashMap<AttributeKey, BLEService>,
    notifications_channel: broadcast::Sender<ValueNotification>,

    // Mutable, advertised, state...
    address_type: RwLock<Option<AddressType>>,
    local_name: RwLock<Option<String>>,
    advertisement_name: RwLock<Option<String>>,
    last_tx_power_level: RwLock<Option<i16>>, // XXX: would be nice to avoid lock here!
    last_rssi: RwLock<Option<i16>>,           // XXX: would be nice to avoid lock here!
    latest_manufacturer_data: RwLock<HashMap<u16, Vec<u8>>>,
    latest_service_data: RwLock<HashMap<Uuid, Vec<u8>>>,
    services: RwLock<HashSet<Uuid>>,
    class: RwLock<Option<u32>>,
}

impl Peripheral {
    pub(crate) fn new(adapter: Weak<AdapterManager<Self>>, address: BDAddr) -> Self {
        let (broadcast_sender, _) = broadcast::channel(crate::ubm::EVENT_CAPACITY);
        Peripheral {
            shared: Arc::new(Shared {
                adapter,
                device: tokio::sync::Mutex::new(None),
                address,
                mtu: AtomicU16::new(api::DEFAULT_MTU_SIZE),
                connected: AtomicBool::new(false),
                ble_services: DashMap::new(),
                notifications_channel: broadcast_sender,
                address_type: RwLock::new(None),
                local_name: RwLock::new(None),
                advertisement_name: RwLock::new(None),
                last_tx_power_level: RwLock::new(None),
                last_rssi: RwLock::new(None),
                latest_manufacturer_data: RwLock::new(HashMap::new()),
                latest_service_data: RwLock::new(HashMap::new()),
                services: RwLock::new(HashSet::new()),
                class: RwLock::new(None),
            }),
        }
    }

    // TODO: see if the other backends can also be similarly decoupled from PeripheralProperties
    // so it can potentially be replaced by individial state getters
    fn derive_properties(&self) -> PeripheralProperties {
        PeripheralProperties {
            address: self.address(),
            address_type: *self.shared.address_type.read().unwrap(),
            local_name: self.shared.local_name.read().unwrap().clone(),
            advertisement_name: self.shared.advertisement_name.read().unwrap().clone(),
            tx_power_level: *self.shared.last_tx_power_level.read().unwrap(),
            rssi: *self.shared.last_rssi.read().unwrap(),
            manufacturer_data: self.shared.latest_manufacturer_data.read().unwrap().clone(),
            service_data: self.shared.latest_service_data.read().unwrap().clone(),
            services: self
                .shared
                .services
                .read()
                .unwrap()
                .iter()
                .copied()
                .collect(),
            class: *self.shared.class.read().unwrap(),
        }
    }

    /// UBM patch (UBM_PATCHES.md #17): this advertisement's own data, read
    /// from the received event itself rather than the peripheral's merged
    /// properties.
    pub(crate) fn advertisement_report(
        args: &BluetoothLEAdvertisementReceivedEventArgs,
    ) -> windows::core::Result<api::AdvertisementReport> {
        let advertisement = args.Advertisement()?;
        let local_name = advertisement
            .LocalName()
            .ok()
            .map(|name| name.to_string())
            .filter(|name| !name.is_empty());
        let mut manufacturer_data = HashMap::new();
        for section in advertisement.ManufacturerData()? {
            manufacturer_data.insert(section.CompanyId()?, utils::to_vec(&section.Data()?));
        }
        let mut service_data = HashMap::new();
        for section in advertisement.DataSections()? {
            let data = utils::to_vec(&section.Data()?);
            if let Some((uuid, payload)) =
                gatt_model::service_data_section(section.DataType()?, &data)
            {
                service_data.insert(Uuid::from_u128(uuid), payload);
            }
        }
        let services = advertisement
            .ServiceUuids()?
            .into_iter()
            .map(|uuid| utils::to_uuid(&uuid))
            .collect();
        let tx_power_level = args
            .TransmitPowerLevelInDBm()
            .ok()
            .and_then(|reference| reference.Value().ok());
        Ok(api::AdvertisementReport {
            source: api::ReportSource::Advertisement,
            local_name,
            rssi: Some(args.RawSignalStrengthInDBm()?),
            tx_power_level,
            manufacturer_data,
            service_data,
            services,
            solicited_services: None,
            overflow_services: None,
            connectable: None,
        })
    }

    pub(crate) fn update_properties(&self, args: &BluetoothLEAdvertisementReceivedEventArgs) {
        let advertisement = args.Advertisement().unwrap();

        // Advertisements are cumulative: set/replace data only if it's set
        if let Ok(name) = advertisement.LocalName() {
            if !name.is_empty() {
                let name_str = name.to_string();
                let mut adv_name_guard = self.shared.advertisement_name.write().unwrap();
                *adv_name_guard = Some(name_str.clone());
                drop(adv_name_guard);
                // Also use as local_name fallback if we don't have one yet
                let local_name_guard = self.shared.local_name.read().unwrap();
                if local_name_guard.is_none() {
                    drop(local_name_guard);
                    let mut local_name_guard = self.shared.local_name.write().unwrap();
                    *local_name_guard = Some(name_str);
                }
            }
        }
        if let Ok(manufacturer_data) = advertisement.ManufacturerData() {
            if manufacturer_data.Size().unwrap() > 0 {
                let mut manufacturer_data_guard =
                    self.shared.latest_manufacturer_data.write().unwrap();
                *manufacturer_data_guard = manufacturer_data
                    .into_iter()
                    .map(|d| {
                        let manufacturer_id = d.CompanyId().unwrap();
                        let data = utils::to_vec(&d.Data().unwrap());

                        (manufacturer_id, data)
                    })
                    .collect();

                // Emit event of newly received advertisement
                self.emit_event(CentralEvent::ManufacturerDataAdvertisement {
                    id: self.shared.address.into(),
                    manufacturer_data: manufacturer_data_guard.clone(),
                });
            }
        }

        // The Windows Runtime API (as of 19041) does not directly expose Service Data as a friendly API (like Manufacturer Data above)
        // Instead they provide data sections for access to raw advertising data. That is processed here.
        if let Ok(data_sections) = advertisement.DataSections() {
            // See if we have any advertised service data before taking a lock to update...
            let mut found_service_data = false;
            for section in &data_sections {
                match section.DataType().unwrap() {
                    advertisement_data_type::SERVICE_DATA_16_BIT_UUID
                    | advertisement_data_type::SERVICE_DATA_32_BIT_UUID
                    | advertisement_data_type::SERVICE_DATA_128_BIT_UUID => {
                        found_service_data = true;
                        break;
                    }
                    _ => {}
                }
            }
            if found_service_data {
                let mut service_data_guard = self.shared.latest_service_data.write().unwrap();

                // UBM patch (UBM_PATCHES.md #17): a short section is
                // skipped, never a panic (upstream `split_at` panicked).
                *service_data_guard = data_sections
                    .into_iter()
                    .filter_map(|d| {
                        let data = utils::to_vec(&d.Data().ok()?);
                        gatt_model::service_data_section(d.DataType().ok()?, &data)
                            .map(|(uuid, payload)| (Uuid::from_u128(uuid), payload))
                    })
                    .collect();

                // Emit event of newly received advertisement
                self.emit_event(CentralEvent::ServiceDataAdvertisement {
                    id: self.shared.address.into(),
                    service_data: service_data_guard.clone(),
                });
            }
        }

        if let Ok(services) = advertisement.ServiceUuids() {
            let mut found_new_service = false;

            // Limited scope for read-only lock...
            {
                let services_guard_ro = self.shared.services.read().unwrap();

                // In all likelihood we've already seen all the advertised services before so lets
                // check to see if we can avoid taking the write lock and emitting an event...
                for uuid in &services {
                    if !services_guard_ro.contains(&utils::to_uuid(&uuid)) {
                        found_new_service = true;
                        break;
                    }
                }
            }

            if found_new_service {
                let mut services_guard = self.shared.services.write().unwrap();

                // ServicesUuids combines all the 16, 32 and 128 bit, 'complete' and 'incomplete'
                // service IDs that may be part of this advertisement into one single list with
                // a consistent (128bit) format. Considering that we don't practically know
                // whether the aggregate list is ever complete we always union the IDs with the
                // IDs already tracked.
                for uuid in services {
                    services_guard.insert(utils::to_uuid(&uuid));
                }

                self.emit_event(CentralEvent::ServicesAdvertisement {
                    id: self.shared.address.into(),
                    services: services_guard.iter().copied().collect(),
                });
            }
        }

        if let Ok(address_type) = args.BluetoothAddressType() {
            let mut address_type_guard = self.shared.address_type.write().unwrap();
            *address_type_guard = match address_type {
                BluetoothAddressType::Public => Some(AddressType::Public),
                BluetoothAddressType::Random => Some(AddressType::Random),
                _ => None,
            };
        }

        if let Ok(tx_reference) = args.TransmitPowerLevelInDBm() {
            // IReference is (ironically) a crazy foot gun in Rust since it very easily
            // panics if you look at it wrong. Calling GetInt16(), IsNumericScalar() or Type()
            // all panic here without returning a Result as documented.
            // Value() is apparently the _right_ way to extract something from an IReference<T>...
            if let Ok(tx) = tx_reference.Value() {
                let mut tx_power_level_guard = self.shared.last_tx_power_level.write().unwrap();
                *tx_power_level_guard = Some(tx);
            }
        }
        if let Ok(rssi) = args.RawSignalStrengthInDBm() {
            let mut rssi_guard = self.shared.last_rssi.write().unwrap();
            let old_rssi = *rssi_guard;
            *rssi_guard = Some(rssi);
            drop(rssi_guard);
            // Emit RssiUpdate event when RSSI changes
            if old_rssi != Some(rssi) {
                self.emit_event(CentralEvent::RssiUpdate {
                    id: self.shared.address.into(),
                    rssi,
                });
            }
        }
    }

    /// UBM patch (`winrt-cccd-mode`): write `value` to the CCCD of exactly
    /// this characteristic instance (service and characteristic addressed by
    /// UUID and handle). `subscribe` writes `Indicate` whenever a
    /// characteristic can indicate; the desktop core rewrites the mode it
    /// needs through this, on the same GATT object the subscription uses.
    pub async fn write_client_configuration(
        &self,
        characteristic: &Characteristic,
        value: GattClientCharacteristicConfigurationDescriptorValue,
    ) -> Result<()> {
        let gatt = self.gatt_characteristic(characteristic, "CCCD write")?;
        BLECharacteristic::write_client_configuration(&gatt, value, "CCCD write").await
    }

    fn gatt_characteristic(
        &self,
        characteristic: &Characteristic,
        operation: &str,
    ) -> Result<GattCharacteristic> {
        let service = self
            .shared
            .ble_services
            .get(&(characteristic.service_uuid, characteristic.service_instance))
            .ok_or_else(|| {
                not_found(
                    "Service",
                    characteristic.service_uuid,
                    characteristic.service_instance,
                    operation,
                )
            })?;
        let ble_characteristic = service
            .characteristics
            .get(&(characteristic.uuid, characteristic.instance))
            .ok_or_else(|| {
                not_found(
                    "Characteristic",
                    characteristic.uuid,
                    characteristic.instance,
                    operation,
                )
            })?;
        Ok(ble_characteristic.gatt().clone())
    }

    /// Run `f` on the characteristic instance under a short map lock that
    /// is never held across an await.
    fn with_characteristic_mut<T>(
        &self,
        characteristic: &Characteristic,
        operation: &str,
        f: impl FnOnce(&mut BLECharacteristic) -> Result<T>,
    ) -> Result<T> {
        let mut service = self
            .shared
            .ble_services
            .get_mut(&(characteristic.service_uuid, characteristic.service_instance))
            .ok_or_else(|| {
                not_found(
                    "Service",
                    characteristic.service_uuid,
                    characteristic.service_instance,
                    operation,
                )
            })?;
        let ble_characteristic = service
            .characteristics
            .get_mut(&(characteristic.uuid, characteristic.instance))
            .ok_or_else(|| {
                not_found(
                    "Characteristic",
                    characteristic.uuid,
                    characteristic.instance,
                    operation,
                )
            })?;
        f(ble_characteristic)
    }

    fn gatt_descriptor(&self, descriptor: &Descriptor, operation: &str) -> Result<GattDescriptor> {
        let service = self
            .shared
            .ble_services
            .get(&(descriptor.service_uuid, descriptor.service_instance))
            .ok_or_else(|| {
                not_found(
                    "Service",
                    descriptor.service_uuid,
                    descriptor.service_instance,
                    operation,
                )
            })?;
        let ble_characteristic = service
            .characteristics
            .get(&(
                descriptor.characteristic_uuid,
                descriptor.characteristic_instance,
            ))
            .ok_or_else(|| {
                not_found(
                    "Characteristic",
                    descriptor.characteristic_uuid,
                    descriptor.characteristic_instance,
                    operation,
                )
            })?;
        let ble_descriptor = ble_characteristic
            .descriptors
            .get(&(descriptor.uuid, descriptor.instance))
            .ok_or_else(|| {
                not_found(
                    "Descriptor",
                    descriptor.uuid,
                    descriptor.instance,
                    operation,
                )
            })?;
        Ok(ble_descriptor.gatt().clone())
    }

    /// Install a freshly discovered service table in place of the previous
    /// one. A live subscription whose attribute (same service and
    /// characteristic UUID and handle) is still present moves to the new
    /// GATT object; one whose attribute is gone ends with it, which the
    /// desktop core learns from `GattServicesChanged`. A subscription that
    /// could not move is an error, never a silent loss.
    fn replace_services(&self, table: HashMap<AttributeKey, BLEService>) -> Result<()> {
        let stale: Vec<AttributeKey> = self
            .shared
            .ble_services
            .iter()
            .map(|entry| *entry.key())
            .filter(|key| !table.contains_key(key))
            .collect();
        let mut failures = Vec::new();
        for (key, service) in table {
            let Some(mut previous) = self.shared.ble_services.insert(key, service) else {
                continue;
            };
            let Some(mut current) = self.shared.ble_services.get_mut(&key) else {
                continue;
            };
            for (characteristic_key, old) in previous
                .characteristics
                .iter_mut()
                .filter(|(_, characteristic)| characteristic.is_subscribed())
            {
                match current.characteristics.get_mut(characteristic_key) {
                    Some(new) => {
                        if let Err(error) = new.adopt_subscription(old) {
                            failures.push(format!(
                                "characteristic {} (handle {}) of service {} (handle {}): {error}",
                                characteristic_key.0, characteristic_key.1, key.0, key.1
                            ));
                        }
                    }
                    None => warn!(
                        "rediscovery removed subscribed characteristic {} (handle {}) of service {} (handle {})",
                        characteristic_key.0, characteristic_key.1, key.0, key.1
                    ),
                }
            }
        }
        for key in stale {
            if let Some((_, previous)) = self.shared.ble_services.remove(&key) {
                for characteristic in previous
                    .characteristics
                    .values()
                    .filter(|characteristic| characteristic.is_subscribed())
                {
                    let (uuid, handle) = characteristic.key();
                    warn!(
                        "rediscovery removed service {} (handle {}) with subscribed characteristic {uuid} (handle {handle})",
                        key.0, key.1
                    );
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Other(
                format!(
                    "rediscovery could not keep live subscriptions: {}",
                    failures.join("; ")
                )
                .into(),
            ))
        }
    }

    fn emit_event(&self, event: CentralEvent) {
        if let Some(manager) = self.shared.adapter.upgrade() {
            manager.emit(event);
        } else {
            trace!("Could not emit an event. AdapterManager has been dropped");
        }
    }
}

impl Display for Peripheral {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let connected = if self.shared.connected.load(Ordering::Relaxed) {
            " connected"
        } else {
            ""
        };
        write!(
            f,
            "{} {}{}",
            self.shared.address,
            self.shared
                .local_name
                .read()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "(unknown)".to_string()),
            connected
        )
    }
}

impl Debug for Peripheral {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let connected = if self.shared.connected.load(Ordering::Relaxed) {
            " connected"
        } else {
            ""
        };
        let properties = self.derive_properties();
        write!(
            f,
            "{} properties: {:?}, services: {:?} {}",
            self.shared.address, properties, self.shared.ble_services, connected
        )
    }
}

#[async_trait]
impl ApiPeripheral for Peripheral {
    fn id(&self) -> PeripheralId {
        PeripheralId(self.shared.address)
    }

    /// Returns the address of the peripheral.
    fn address(&self) -> BDAddr {
        self.shared.address
    }

    /// Returns the currently negotiated mtu size
    fn mtu(&self) -> u16 {
        self.shared.mtu.load(Ordering::Relaxed)
    }

    /// Returns the set of properties associated with the peripheral. These may be updated over time
    /// as additional advertising reports are received.
    async fn properties(&self) -> Result<Option<PeripheralProperties>> {
        Ok(Some(self.derive_properties()))
    }

    fn services(&self) -> BTreeSet<Service> {
        self.shared
            .ble_services
            .iter()
            .map(|item| item.value().to_service())
            .collect()
    }

    /// Returns true iff we are currently connected to the device.
    async fn is_connected(&self) -> Result<bool> {
        Ok(self.shared.connected.load(Ordering::Relaxed))
    }

    /// Creates a connection to the device. This is a synchronous operation; if this method returns
    /// Ok there has been successful connection. Note that peripherals allow only one connection at
    /// a time. Operations that attempt to communicate with a device will fail until it is connected.
    async fn connect(&self) -> Result<()> {
        let adapter_clone = self.shared.adapter.clone();
        let address = self.shared.address;

        let connection_status_changed = Box::new({
            let shared_clone = Arc::downgrade(&self.shared);
            move |is_connected| {
                if let Some(shared) = shared_clone.upgrade() {
                    shared.connected.store(is_connected, Ordering::Relaxed);
                }

                if !is_connected {
                    if let Some(adapter) = adapter_clone.upgrade() {
                        adapter.emit(CentralEvent::DeviceDisconnected(address.into()));
                    }
                }
            }
        });

        let max_pdu_size_changed = Box::new({
            let shared_clone = Arc::downgrade(&self.shared);
            move |mtu| {
                if let Some(shared) = shared_clone.upgrade() {
                    shared.mtu.store(mtu, Ordering::Relaxed);
                }
            }
        });

        let device = BLEDevice::new(
            self.shared.address,
            connection_status_changed,
            max_pdu_size_changed,
        )
        .await?;

        device.connect().await?;
        // Query the system-cached device name (GAP name) and update local_name
        if let Ok(name) = device.name() {
            let name_str = name.to_string();
            if !name_str.is_empty() {
                let mut local_name_guard = self.shared.local_name.write().unwrap();
                *local_name_guard = Some(name_str);
            }
        }
        let mut d = self.shared.device.lock().await;
        *d = Some(device);
        self.shared.connected.store(true, Ordering::Relaxed);
        self.emit_event(CentralEvent::DeviceConnected(self.shared.address.into()));
        Ok(())
    }

    /// Terminates a connection to the device. This is a synchronous operation.
    async fn disconnect(&self) -> Result<()> {
        // We need to clear the services because if this device is re-connected,
        // the cached service objects will no longer be valid (they must be refreshed).
        self.shared.ble_services.clear();
        let mut device = self.shared.device.lock().await;
        *device = None;
        self.shared.connected.store(false, Ordering::Relaxed);
        self.emit_event(CentralEvent::DeviceDisconnected(self.shared.address.into()));
        Ok(())
    }

    /// Discovers all characteristics for the device. This is a synchronous operation.
    ///
    /// UBM patch (`winrt-attribute-instances`, `winrt-uncached-discovery`):
    /// every service, characteristic and descriptor is queried `Uncached`
    /// and kept as its own instance keyed by (UUID, `AttributeHandle`). The
    /// discovered database REPLACES the previous one, so a discovery after
    /// `GattServicesChanged` drops removed and changed attributes. Any
    /// failed query fails the discovery (naming the attribute and the
    /// status) and leaves the previous table in place.
    async fn discover_services(&self) -> Result<()> {
        let mut device = self.shared.device.lock().await;
        let Some(device) = device.as_mut() else {
            return Err(Error::NotConnected);
        };
        let gatt_services = device.discover_services().await?;
        let mut discovered = Vec::with_capacity(gatt_services.len());
        for service in gatt_services {
            discovered.push(discover_service(service).await?);
        }
        let table = index_unique(
            discovered
                .into_iter()
                .map(|service| (service.key(), service)),
        )
        .map_err(|(uuid, handle)| {
            Error::Other(
                format!("service discovery listed service {uuid} at handle {handle} twice").into(),
            )
        })?;
        self.replace_services(table)
    }

    /// Write some data to the characteristic. Returns an error if the write couldn't be send or (in
    /// the case of a write-with-response) if the device returns an error.
    async fn write(
        &self,
        characteristic: &Characteristic,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<()> {
        let gatt = self.gatt_characteristic(characteristic, "write")?;
        BLECharacteristic::write_value(&gatt, data, write_type).await
    }

    /// Enables either notify or indicate (depending on support) for the specified characteristic.
    /// This is a synchronous call.
    async fn subscribe(&self, characteristic: &Characteristic) -> Result<()> {
        let notifications_sender = self.shared.notifications_channel.clone();
        let uuid = characteristic.uuid;
        let instance = characteristic.instance;
        let service_uuid = characteristic.service_uuid;
        let service_instance = characteristic.service_instance;
        let handler: NotifyEventHandler = std::sync::Arc::new(move |value| {
            let notification = ValueNotification {
                uuid,
                instance,
                service_uuid,
                service_instance,
                value,
                lost_before: 0,
            };
            // Note: we ignore send errors here which may happen while there are no
            // receivers...
            let _ = notifications_sender.send(notification);
        });
        let (gatt, config, token) =
            self.with_characteristic_mut(characteristic, "subscribe", |ble_characteristic| {
                let (config, token) = ble_characteristic.register(handler)?;
                Ok((ble_characteristic.gatt().clone(), config, token))
            })?;
        let written =
            BLECharacteristic::write_client_configuration(&gatt, config, "subscribe").await;
        if let Err(error) = written {
            // Roll back only this subscribe's handler; a rollback failure is
            // reported with the write failure, never dropped.
            if let Err(rollback) =
                self.with_characteristic_mut(characteristic, "subscribe", |ble_characteristic| {
                    ble_characteristic.deregister(token)
                })
            {
                return Err(Error::Other(
                    format!("{error}; removing the ValueChanged handler also failed: {rollback}")
                        .into(),
                ));
            }
            return Err(error);
        }
        Ok(())
    }

    /// Disables either notify or indicate (depending on support) for the specified characteristic.
    /// This is a synchronous call.
    async fn unsubscribe(&self, characteristic: &Characteristic) -> Result<()> {
        let gatt =
            self.with_characteristic_mut(characteristic, "unsubscribe", |ble_characteristic| {
                ble_characteristic.take_registration()?;
                Ok(ble_characteristic.gatt().clone())
            })?;
        BLECharacteristic::write_client_configuration(
            &gatt,
            GattClientCharacteristicConfigurationDescriptorValue::None,
            "unsubscribe",
        )
        .await
    }

    async fn read(&self, characteristic: &Characteristic) -> Result<Vec<u8>> {
        let gatt = self.gatt_characteristic(characteristic, "read")?;
        BLECharacteristic::read_value(&gatt).await
    }

    async fn notifications(&self) -> Result<Pin<Box<dyn Stream<Item = ValueNotification> + Send>>> {
        let receiver = self.shared.notifications_channel.subscribe();
        Ok(notifications_stream_from_broadcast_receiver(receiver))
    }

    async fn write_descriptor(&self, descriptor: &Descriptor, data: &[u8]) -> Result<()> {
        let gatt = self.gatt_descriptor(descriptor, "write")?;
        BLEDescriptor::write_value(&gatt, data).await
    }

    async fn read_descriptor(&self, descriptor: &Descriptor) -> Result<Vec<u8>> {
        let gatt = self.gatt_descriptor(descriptor, "read")?;
        BLEDescriptor::read_value(&gatt).await
    }

    async fn read_rssi(&self) -> Result<i16> {
        self.shared
            .last_rssi
            .read()
            .unwrap()
            .ok_or(Error::NotConnected)
    }

    async fn connection_parameters(&self) -> Result<Option<ConnectionParameters>> {
        let device = self.shared.device.lock().await;
        match &*device {
            Some(device) => Ok(Some(device.get_connection_parameters()?)),
            None => Err(Error::NotConnected),
        }
    }

    async fn request_connection_parameters(&self, preset: ConnectionParameterPreset) -> Result<()> {
        let device = self.shared.device.lock().await;
        match &*device {
            Some(device) => device.request_connection_parameters(preset),
            None => Err(Error::NotConnected),
        }
    }
}

impl From<BDAddr> for PeripheralId {
    fn from(address: BDAddr) -> Self {
        PeripheralId(address)
    }
}

fn not_found(kind: &str, uuid: Uuid, instance: u64, operation: &str) -> Error {
    Error::NotSupported(format!(
        "{kind} {uuid} (instance {instance}) not found for {operation}"
    ))
}

/// UBM patch (`winrt-attribute-instances`, `winrt-uncached-discovery`): one
/// service with every characteristic and descriptor it lists, each kept as
/// its own instance. A failed query names the service it belongs to.
async fn discover_service(service: GattDeviceService) -> Result<BLEService> {
    let uuid = utils::to_uuid(&service.Uuid()?);
    let instance = u64::from(service.AttributeHandle()?);
    let context =
        |error: Error| Error::Other(format!("service {uuid} (handle {instance}): {error}").into());
    let characteristics = BLEDevice::get_characteristics(&service)
        .await
        .map_err(context)?;
    let characteristics =
        futures::future::try_join_all(characteristics.into_iter().map(discover_characteristic))
            .await
            .map_err(context)?;
    let characteristics = index_unique(
        characteristics
            .into_iter()
            .map(|characteristic| (characteristic.key(), characteristic)),
    )
    .map_err(|(characteristic, handle)| {
        context(Error::Other(
            format!("characteristic {characteristic} at handle {handle} was listed twice").into(),
        ))
    })?;
    Ok(BLEService {
        uuid,
        instance,
        characteristics,
    })
}

async fn discover_characteristic(characteristic: GattCharacteristic) -> Result<BLECharacteristic> {
    let uuid = utils::to_uuid(&characteristic.Uuid()?);
    let handle = characteristic.AttributeHandle()?;
    let context = |error: Error| {
        Error::Other(format!("characteristic {uuid} (handle {handle}): {error}").into())
    };
    let descriptors = BLEDevice::get_characteristic_descriptors(&characteristic)
        .await
        .map_err(context)?
        .into_iter()
        .map(|descriptor| {
            BLEDescriptor::new(descriptor).map(|descriptor| (descriptor.key(), descriptor))
        })
        .collect::<Result<Vec<_>>>()
        .map_err(context)?;
    let descriptors = index_unique(descriptors).map_err(|(descriptor, handle)| {
        context(Error::Other(
            format!("descriptor {descriptor} at handle {handle} was listed twice").into(),
        ))
    })?;
    BLECharacteristic::new(characteristic, descriptors)
}
