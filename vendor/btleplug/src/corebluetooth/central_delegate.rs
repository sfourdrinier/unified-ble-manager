// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.
//
// Some portions of this file are taken and/or modified from blurmac
// (https://github.com/servo/devices), using a BSD 3-Clause license under the
// following copyright:
//
// Copyright (c) 2017 Akos Kiss.
//
// Licensed under the BSD 3-Clause License
// <LICENSE.md or https://opensource.org/licenses/BSD-3-Clause>.
// This file may not be copied, modified, or distributed except
// according to those terms.

use super::utils::nsstring_to_string;
use super::utils::{core_bluetooth::cbuuid_to_uuid, nsuuid_to_uuid};
use futures::channel::mpsc::Sender;
use futures::sink::SinkExt;
use log::{error, trace};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{ClassType, DeclaredClass, declare_class, msg_send_id, mutability, rc::Retained};
use objc2_core_bluetooth::{
    CBAdvertisementDataIsConnectable, CBAdvertisementDataLocalNameKey,
    CBAdvertisementDataManufacturerDataKey, CBAdvertisementDataOverflowServiceUUIDsKey,
    CBAdvertisementDataServiceDataKey, CBAdvertisementDataServiceUUIDsKey,
    CBAdvertisementDataSolicitedServiceUUIDsKey, CBAdvertisementDataTxPowerLevelKey,
    CBCentralManager, CBCentralManagerDelegate, CBCharacteristic, CBDescriptor, CBManagerState,
    CBPeripheral, CBPeripheralDelegate, CBService, CBUUID,
};
use objc2_foundation::{
    NSArray, NSData, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use std::convert::TryInto;
use std::{
    collections::HashMap,
    fmt::{self, Debug, Formatter},
    ops::Deref,
};
use uuid::Uuid;

/// UBM patch (UBM_PATCHES.md #6): one GATT attribute instance. CoreBluetooth
/// reports no ATT handles, so `instance` is the attribute's position in its
/// parent's CoreBluetooth array (`peripheral.services`,
/// `service.characteristics`, `characteristic.descriptors`): discovery
/// order. Upstream keyed attributes by UUID alone, so same-UUID siblings
/// collapsed into one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AttrKey {
    pub uuid: Uuid,
    pub instance: u64,
}

impl fmt::Display for AttrKey {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}#{}", self.uuid, self.instance)
    }
}

/// Key every attribute of one CoreBluetooth array by UUID and position
/// (UBM patch #6): the table every discovery answer is stored in.
pub(crate) fn keyed<T>(
    items: Retained<NSArray<T>>,
    uuid_of: impl Fn(&T) -> Uuid,
) -> HashMap<AttrKey, Retained<T>>
where
    T: ClassType,
    T: objc2::mutability::IsRetainable,
{
    (0u64..)
        .zip(items.iter())
        .map(|(instance, item)| {
            (
                AttrKey {
                    uuid: uuid_of(item),
                    instance,
                },
                item.retain(),
            )
        })
        .collect()
}

/// Position of `item` (by object identity) in a CoreBluetooth array.
fn position<T: objc2::Message>(items: &NSArray<T>, item: &T) -> Option<u64> {
    items
        .iter()
        .position(|candidate| std::ptr::eq(&*candidate, item))
        .and_then(|index| u64::try_from(index).ok())
}

/// The key of one service of `peripheral`. `None` for a service that is not
/// one of the peripheral's own (an included service), which upstream never
/// exposed either.
fn service_key(peripheral: &CBPeripheral, service: &CBService) -> Option<AttrKey> {
    let services = unsafe { peripheral.services() }?;
    Some(AttrKey {
        uuid: cbuuid_to_uuid(&*unsafe { service.UUID() }),
        instance: position(&services, service)?,
    })
}

/// The service and characteristic keys of one characteristic.
fn characteristic_key(
    peripheral: &CBPeripheral,
    characteristic: &CBCharacteristic,
) -> Option<(AttrKey, AttrKey)> {
    let service = unsafe { characteristic.service() }?;
    let service_key = service_key(peripheral, &service)?;
    let characteristics = unsafe { service.characteristics() }?;
    Some((
        service_key,
        AttrKey {
            uuid: cbuuid_to_uuid(&*unsafe { characteristic.UUID() }),
            instance: position(&characteristics, characteristic)?,
        },
    ))
}

/// The service, characteristic and descriptor keys of one descriptor.
fn descriptor_key(
    peripheral: &CBPeripheral,
    descriptor: &CBDescriptor,
) -> Option<(AttrKey, AttrKey, AttrKey)> {
    let characteristic = unsafe { descriptor.characteristic() }?;
    let (service_key, characteristic_key) = characteristic_key(peripheral, &characteristic)?;
    let descriptors = unsafe { characteristic.descriptors() }?;
    Some((
        service_key,
        characteristic_key,
        AttrKey {
            uuid: cbuuid_to_uuid(&*unsafe { descriptor.UUID() }),
            instance: position(&descriptors, descriptor)?,
        },
    ))
}

/// UBM patch (UBM_PATCHES.md #2): the advertisement fields upstream drops.
/// A key the advertisement does not carry stays `None`; a carried key with
/// no entries is `Some(vec![])`.
pub fn advertisement_extras(
    adv_data: &NSDictionary<NSString, AnyObject>,
) -> super::peripheral::AdvertisementExtras {
    let uuids = |key: &NSString| {
        adv_data.get(key).map(|value| {
            // SAFETY: CoreBluetooth documents both keys as `NSArray<CBUUID>`.
            let value: *const AnyObject = value;
            let value: *const NSArray<CBUUID> = value.cast();
            unsafe { &*value }
                .iter()
                .map(cbuuid_to_uuid)
                .collect::<Vec<_>>()
        })
    };
    super::peripheral::AdvertisementExtras {
        solicited_service_uuids: uuids(unsafe { CBAdvertisementDataSolicitedServiceUUIDsKey }),
        overflow_service_uuids: uuids(unsafe { CBAdvertisementDataOverflowServiceUUIDsKey }),
        connectable: adv_data
            .get(unsafe { CBAdvertisementDataIsConnectable })
            .map(|value| {
                // SAFETY: CoreBluetooth documents the key as an `NSNumber`
                // holding a boolean.
                let value: *const AnyObject = value;
                let value: *const NSNumber = value.cast();
                unsafe { &*value }.as_bool()
            }),
    }
}

pub enum CentralDelegateEvent {
    DidUpdateState {
        state: CBManagerState,
    },
    DiscoveredPeripheral {
        cbperipheral: Retained<CBPeripheral>,
        advertisement_name: Option<String>,
    },
    DiscoveredServices {
        peripheral_uuid: Uuid,
        services: HashMap<AttrKey, Retained<CBService>>,
    },
    // UBM patch (UBM_PATCHES.md #2): advertisement fields upstream drops.
    AdvertisementExtras {
        peripheral_uuid: Uuid,
        extras: super::peripheral::AdvertisementExtras,
    },
    ManufacturerData {
        peripheral_uuid: Uuid,
        manufacturer_id: u16,
        data: Vec<u8>,
        rssi: i16,
    },
    ServiceData {
        peripheral_uuid: Uuid,
        service_data: HashMap<Uuid, Vec<u8>>,
        rssi: i16,
    },
    Services {
        peripheral_uuid: Uuid,
        service_uuids: Vec<Uuid>,
        rssi: i16,
    },
    ServicesModified {
        peripheral_uuid: Uuid,
    },
    // DiscoveredIncludedServices(Uuid, HashMap<AttrKey, Retained<CBService>>),
    DiscoveredCharacteristics {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        /// Characteristic UUID to CBCharacteristic
        characteristics: HashMap<AttrKey, Retained<CBCharacteristic>>,
    },
    DiscoveredCharacteristicDescriptors {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptors: HashMap<AttrKey, Retained<CBDescriptor>>,
    },
    ConnectedDevice {
        peripheral_uuid: Uuid,
    },
    ConnectionFailed {
        peripheral_uuid: Uuid,
        error_description: Option<String>,
    },
    DisconnectedDevice {
        peripheral_uuid: Uuid,
    },
    CharacteristicSubscribed {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
    },
    CharacteristicUnsubscribed {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
    },
    CharacteristicNotified {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        data: Vec<u8>,
    },
    CharacteristicWritten {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
    },
    DescriptorNotified {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptor_uuid: AttrKey,
        data: Vec<u8>,
    },
    DescriptorWritten {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptor_uuid: AttrKey,
    },
    TxPowerLevel {
        peripheral_uuid: Uuid,
        tx_power_level: i16,
    },
    DidReadRssi {
        peripheral_uuid: Uuid,
        rssi: i16,
    },
    ReadyToSendWriteWithoutResponse {
        peripheral_uuid: Uuid,
    },
    // UBM patch (UBM_PATCHES.md #17): one advertisement with its own data,
    // sent after the events that update the peripheral's merged state.
    Advertised {
        peripheral_uuid: Uuid,
        report: crate::api::AdvertisementReport,
    },
    // UBM patch (UBM_PATCHES.md #14/#15): an attribute callback carried an
    // `NSError`. Upstream dropped it, leaving the waiting read, write or
    // notification change without an answer.
    AttributeFailed {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptor_uuid: Option<AttrKey>,
        stage: AttributeStage,
        error: crate::PlatformError,
    },
}

/// UBM patch (UBM_PATCHES.md #14/#15): which attribute callback failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributeStage {
    /// `didUpdateValueFor…` (a read response or a notification).
    Value,
    /// `didWriteValueFor…`.
    Write,
    /// `didUpdateNotificationStateFor…`.
    NotifyState,
}

/// UBM patch (UBM_PATCHES.md #15): an `NSError` as the platform's answer:
/// the legacy CoreBluetooth identity `{domain:"corebluetooth", code}`.
fn nserror_platform(error: &NSError) -> crate::PlatformError {
    crate::PlatformError::new(
        "corebluetooth",
        error.code().to_string(),
        error.localizedDescription().to_string(),
    )
    .with("nsErrorDomain", error.domain().to_string())
}

impl Debug for CentralDelegateEvent {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            CentralDelegateEvent::DidUpdateState { state } => f
                .debug_struct("CentralDelegateEvent")
                .field("state", state)
                .finish(),
            CentralDelegateEvent::DiscoveredPeripheral {
                cbperipheral,
                advertisement_name,
            } => f
                .debug_struct("CentralDelegateEvent")
                .field("cbperipheral", cbperipheral.deref())
                .field("advertisement_name", advertisement_name)
                .finish(),
            CentralDelegateEvent::DiscoveredServices {
                peripheral_uuid,
                services,
            } => f
                .debug_struct("DiscoveredServices")
                .field("peripheral_uuid", peripheral_uuid)
                .field("services", &services.keys().collect::<Vec<_>>())
                .finish(),
            CentralDelegateEvent::DiscoveredCharacteristics {
                peripheral_uuid,
                service_uuid,
                characteristics,
            } => f
                .debug_struct("DiscoveredCharacteristics")
                .field("peripheral_uuid", peripheral_uuid)
                .field("service_uuid", service_uuid)
                .field(
                    "characteristics",
                    &characteristics.keys().collect::<Vec<_>>(),
                )
                .finish(),
            CentralDelegateEvent::DiscoveredCharacteristicDescriptors {
                peripheral_uuid,
                service_uuid,
                characteristic_uuid,
                descriptors,
            } => f
                .debug_struct("DiscoveredCharacteristicDescriptors")
                .field("peripheral_uuid", peripheral_uuid)
                .field("service_uuid", service_uuid)
                .field("characteristic_uuid", characteristic_uuid)
                .field("descriptors", &descriptors.keys().collect::<Vec<_>>())
                .finish(),
            CentralDelegateEvent::ConnectedDevice { peripheral_uuid } => f
                .debug_struct("ConnectedDevice")
                .field("peripheral_uuid", peripheral_uuid)
                .finish(),
            CentralDelegateEvent::ConnectionFailed {
                peripheral_uuid,
                error_description,
            } => f
                .debug_struct("ConnectionFailed")
                .field("peripheral_uuid", peripheral_uuid)
                .field("error_description", error_description)
                .finish(),
            CentralDelegateEvent::DisconnectedDevice { peripheral_uuid } => f
                .debug_struct("DisconnectedDevice")
                .field("peripheral_uuid", peripheral_uuid)
                .finish(),
            CentralDelegateEvent::CharacteristicSubscribed {
                peripheral_uuid,
                service_uuid,
                characteristic_uuid,
            } => f
                .debug_struct("CharacteristicSubscribed")
                .field("peripheral_uuid", peripheral_uuid)
                .field("service_uuid", service_uuid)
                .field("characteristic_uuid", characteristic_uuid)
                .finish(),
            CentralDelegateEvent::CharacteristicUnsubscribed {
                peripheral_uuid,
                service_uuid,
                characteristic_uuid,
            } => f
                .debug_struct("CharacteristicUnsubscribed")
                .field("peripheral_uuid", peripheral_uuid)
                .field("service_uuid", service_uuid)
                .field("characteristic_uuid", characteristic_uuid)
                .finish(),
            CentralDelegateEvent::CharacteristicNotified {
                peripheral_uuid,
                service_uuid,
                characteristic_uuid,
                data,
            } => f
                .debug_struct("CharacteristicNotified")
                .field("peripheral_uuid", peripheral_uuid)
                .field("service_uuid", service_uuid)
                .field("characteristic_uuid", characteristic_uuid)
                .field("data", data)
                .finish(),
            CentralDelegateEvent::CharacteristicWritten {
                peripheral_uuid,
                service_uuid,
                characteristic_uuid,
            } => f
                .debug_struct("CharacteristicWritten")
                .field("service_uuid", service_uuid)
                .field("peripheral_uuid", peripheral_uuid)
                .field("characteristic_uuid", characteristic_uuid)
                .finish(),
            CentralDelegateEvent::ManufacturerData {
                peripheral_uuid,
                manufacturer_id,
                data,
                rssi,
            } => f
                .debug_struct("ManufacturerData")
                .field("peripheral_uuid", peripheral_uuid)
                .field("manufacturer_id", manufacturer_id)
                .field("data", data)
                .field("rssi", rssi)
                .finish(),
            CentralDelegateEvent::ServiceData {
                peripheral_uuid,
                service_data,
                rssi,
            } => f
                .debug_struct("ServiceData")
                .field("peripheral_uuid", peripheral_uuid)
                .field("service_data", service_data)
                .field("rssi", rssi)
                .finish(),
            CentralDelegateEvent::Services {
                peripheral_uuid,
                service_uuids,
                rssi,
            } => f
                .debug_struct("Services")
                .field("peripheral_uuid", peripheral_uuid)
                .field("service_uuids", service_uuids)
                .field("rssi", rssi)
                .finish(),
            CentralDelegateEvent::ServicesModified { peripheral_uuid } => f
                .debug_struct("ServicesModified")
                .field("peripheral_uuid", peripheral_uuid)
                .finish(),
            CentralDelegateEvent::DescriptorNotified {
                peripheral_uuid,
                service_uuid,
                characteristic_uuid,
                descriptor_uuid,
                data,
            } => f
                .debug_struct("DescriptorNotified")
                .field("peripheral_uuid", peripheral_uuid)
                .field("service_uuid", service_uuid)
                .field("characteristic_uuid", characteristic_uuid)
                .field("descriptor_uuid", descriptor_uuid)
                .field("data", data)
                .finish(),
            CentralDelegateEvent::DescriptorWritten {
                peripheral_uuid,
                service_uuid,
                characteristic_uuid,
                descriptor_uuid,
            } => f
                .debug_struct("DescriptorWritten")
                .field("service_uuid", service_uuid)
                .field("peripheral_uuid", peripheral_uuid)
                .field("characteristic_uuid", characteristic_uuid)
                .field("descriptor_uuid", descriptor_uuid)
                .finish(),
            CentralDelegateEvent::AdvertisementExtras {
                peripheral_uuid,
                extras,
            } => f
                .debug_struct("AdvertisementExtras")
                .field("peripheral_uuid", peripheral_uuid)
                .field("extras", extras)
                .finish(),
            CentralDelegateEvent::Advertised {
                peripheral_uuid,
                report,
            } => f
                .debug_struct("Advertised")
                .field("peripheral_uuid", peripheral_uuid)
                .field("report", report)
                .finish(),
            CentralDelegateEvent::AttributeFailed {
                peripheral_uuid,
                service_uuid,
                characteristic_uuid,
                descriptor_uuid,
                stage,
                error,
            } => f
                .debug_struct("AttributeFailed")
                .field("peripheral_uuid", peripheral_uuid)
                .field("service_uuid", service_uuid)
                .field("characteristic_uuid", characteristic_uuid)
                .field("descriptor_uuid", descriptor_uuid)
                .field("stage", stage)
                .field("error", error)
                .finish(),
            CentralDelegateEvent::TxPowerLevel {
                peripheral_uuid,
                tx_power_level,
            } => f
                .debug_struct("TxPowerLevel")
                .field("peripheral_uuid", peripheral_uuid)
                .field("tx_power_level", tx_power_level)
                .finish(),
            CentralDelegateEvent::DidReadRssi {
                peripheral_uuid,
                rssi,
            } => f
                .debug_struct("DidReadRssi")
                .field("peripheral_uuid", peripheral_uuid)
                .field("rssi", rssi)
                .finish(),
            CentralDelegateEvent::ReadyToSendWriteWithoutResponse { peripheral_uuid } => f
                .debug_struct("ReadyToSendWriteWithoutResponse")
                .field("peripheral_uuid", peripheral_uuid)
                .finish(),
        }
    }
}

declare_class!(
    #[derive(Debug)]
    pub struct CentralDelegate;

    unsafe impl ClassType for CentralDelegate {
        type Super = NSObject;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "BtlePlugCentralManagerDelegate";
    }

    impl DeclaredClass for CentralDelegate {
        type Ivars = Sender<CentralDelegateEvent>;
    }

    unsafe impl NSObjectProtocol for CentralDelegate {}

    unsafe impl CBCentralManagerDelegate for CentralDelegate {
        #[method(centralManagerDidUpdateState:)]
        fn delegate_centralmanagerdidupdatestate(&self, central: &CBCentralManager) {
            trace!("delegate_centralmanagerdidupdatestate");
            let state = unsafe { central.state() };
            self.send_event(CentralDelegateEvent::DidUpdateState { state });
        }

        // #[method(centralManager:willRestoreState:)]
        // fn delegate_centralmanager_willrestorestate(&self, _central: &CBCentralManager, _dict: &NSDictionary<NSString, AnyObject>) {
        //     trace!("delegate_centralmanager_willrestorestate");
        // }

        #[method(centralManager:didConnectPeripheral:)]
        fn delegate_centralmanager_didconnectperipheral(
            &self,
            _central: &CBCentralManager,
            peripheral: &CBPeripheral,
        ) {
            trace!(
                "delegate_centralmanager_didconnectperipheral {}",
                peripheral_debug(peripheral)
            );
            unsafe { peripheral.setDelegate(Some(ProtocolObject::from_ref(self))) };
            let id = unsafe { peripheral.identifier() };
            let peripheral_uuid = nsuuid_to_uuid(&id);
            self.send_event(CentralDelegateEvent::ConnectedDevice { peripheral_uuid });
        }

        #[method(centralManager:didDisconnectPeripheral:error:)]
        fn delegate_centralmanager_diddisconnectperipheral_error(
            &self,
            _central: &CBCentralManager,
            peripheral: &CBPeripheral,
            error: Option<&NSError>,
        ) {
            trace!(
                "delegate_centralmanager_diddisconnectperipheral_error {} (error={:?})",
                peripheral_debug(peripheral),
                error
            );
            let id = unsafe { peripheral.identifier() };
            let peripheral_uuid = nsuuid_to_uuid(&id);
            self.send_event(CentralDelegateEvent::DisconnectedDevice { peripheral_uuid });
        }

        #[method(centralManager:didFailToConnectPeripheral:error:)]
        fn delegate_centralmanager_didfailtoconnectperipheral_error(
            &self,
            _central: &CBCentralManager,
            peripheral: &CBPeripheral,
            error: Option<&NSError>,
        ) {
            trace!("delegate_centralmanager_didfailtoconnectperipheral_error");
            let id = unsafe { peripheral.identifier() };
            let peripheral_uuid = nsuuid_to_uuid(&id);
            let error_description = error.map(|error| error.localizedDescription().to_string());
            self.send_event(CentralDelegateEvent::ConnectionFailed {
                peripheral_uuid,
                error_description,
            });
        }

        #[method(centralManager:didDiscoverPeripheral:advertisementData:RSSI:)]
        fn delegate_centralmanager_diddiscoverperipheral_advertisementdata_rssi(
            &self,
            _central: &CBCentralManager,
            peripheral: &CBPeripheral,
            adv_data: &NSDictionary<NSString, AnyObject>,
            rssi: &NSNumber,
        ) {
            trace!(
                "delegate_centralmanager_diddiscoverperipheral_advertisementdata_rssi {}",
                peripheral_debug(peripheral)
            );

            let advertisement_name = adv_data
                .get(unsafe { CBAdvertisementDataLocalNameKey })
                .map(|name| name as *const AnyObject as *const NSString)
                .and_then(|name| unsafe { nsstring_to_string(name) });
            // UBM patch (UBM_PATCHES.md #17): this advertisement's own data,
            // named as the legacy addon named it (advertised name, else the
            // peripheral's GAP name).
            let mut report = crate::api::AdvertisementReport {
                source: crate::api::ReportSource::Advertisement,
                local_name: advertisement_name
                    .clone()
                    .or_else(|| unsafe { peripheral.name() }.map(|name| name.to_string())),
                rssi: Some(rssi.as_i16()),
                ..Default::default()
            };
            {
                let extras = advertisement_extras(adv_data);
                report.solicited_services = extras.solicited_service_uuids;
                report.overflow_services = extras.overflow_service_uuids;
                report.connectable = extras.connectable;
            }

            self.send_event(CentralDelegateEvent::DiscoveredPeripheral {
                cbperipheral: peripheral.retain(),
                advertisement_name,
            });

            let rssi_value = rssi.as_i16();

            let id = unsafe { peripheral.identifier() };
            let peripheral_uuid = nsuuid_to_uuid(&id);

            // UBM patch (UBM_PATCHES.md #2): sent before the events that
            // raise `DeviceUpdated`, on the same ordered channel, so a
            // listener reading properties on that event sees this
            // advertisement's extras.
            self.send_event(CentralDelegateEvent::AdvertisementExtras {
                peripheral_uuid,
                extras: advertisement_extras(adv_data),
            });

            let manufacturer_data = adv_data.get(unsafe { CBAdvertisementDataManufacturerDataKey });
            if let Some(manufacturer_data) = manufacturer_data {
                // SAFETY: manufacturer_data is `NSData`
                let manufacturer_data: *const AnyObject = manufacturer_data;
                let manufacturer_data: *const NSData = manufacturer_data.cast();
                let manufacturer_data = unsafe { &*manufacturer_data };

                if manufacturer_data.len() >= 2 {
                    let (manufacturer_id, manufacturer_data) =
                        manufacturer_data.bytes().split_at(2);

                    let manufacturer_id = u16::from_le_bytes(manufacturer_id.try_into().unwrap());
                    report
                        .manufacturer_data
                        .insert(manufacturer_id, Vec::from(manufacturer_data));
                    self.send_event(CentralDelegateEvent::ManufacturerData {
                        peripheral_uuid,
                        manufacturer_id,
                        data: Vec::from(manufacturer_data),
                        rssi: rssi_value,
                    });
                }
            }

            let service_data = adv_data.get(unsafe { CBAdvertisementDataServiceDataKey });
            if let Some(service_data) = service_data {
                // SAFETY: service_data is `NSDictionary<CBUUID, NSData>`
                let service_data: *const AnyObject = service_data;
                let service_data: *const NSDictionary<CBUUID, NSData> = service_data.cast();
                let service_data = unsafe { &*service_data };

                let mut result = HashMap::new();
                for uuid in service_data.keys() {
                    let data = &service_data[uuid];
                    result.insert(cbuuid_to_uuid(uuid), data.bytes().to_vec());
                }

                report.service_data = result.clone();
                self.send_event(CentralDelegateEvent::ServiceData {
                    peripheral_uuid,
                    service_data: result,
                    rssi: rssi_value,
                });
            }

            let services = adv_data.get(unsafe { CBAdvertisementDataServiceUUIDsKey });
            if let Some(services) = services {
                // SAFETY: services is `NSArray<CBUUID>`
                let services: *const AnyObject = services;
                let services: *const NSArray<CBUUID> = services.cast();
                let services = unsafe { &*services };

                let mut service_uuids = Vec::new();
                for uuid in services {
                    service_uuids.push(cbuuid_to_uuid(uuid));
                }

                report.services = service_uuids.clone();
                self.send_event(CentralDelegateEvent::Services {
                    peripheral_uuid,
                    service_uuids,
                    rssi: rssi_value,
                });
            }

            let tx_power_level = adv_data
                .get(unsafe { CBAdvertisementDataTxPowerLevelKey })
                .map(|val| {
                    let val: *const AnyObject = val;
                    let val: *const NSNumber = val.cast();
                    unsafe { &*val }.as_i16()
                });

            if let Some(tx_power_level) = tx_power_level {
                self.send_event(CentralDelegateEvent::TxPowerLevel {
                    peripheral_uuid,
                    tx_power_level,
                });
            }
            report.tx_power_level = tx_power_level;
            self.send_event(CentralDelegateEvent::Advertised {
                peripheral_uuid,
                report,
            });
        }
    }

    unsafe impl CBPeripheralDelegate for CentralDelegate {
        #[method(peripheral:didDiscoverServices:)]
        fn delegate_peripheral_diddiscoverservices(
            &self,
            peripheral: &CBPeripheral,
            error: Option<&NSError>,
        ) {
            trace!(
                "delegate_peripheral_diddiscoverservices {} {}",
                peripheral_debug(peripheral),
                localized_description(error)
            );
            if error.is_none() {
                let services = unsafe { peripheral.services() }.unwrap_or_default();
                let service_map = keyed(services, |s| cbuuid_to_uuid(&*unsafe { s.UUID() }));
                for s in service_map.values() {
                    // go ahead and ask for characteristics and other services
                    unsafe {
                        peripheral.discoverCharacteristics_forService(None, s);
                        peripheral.discoverIncludedServices_forService(None, s);
                    }
                }
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                self.send_event(CentralDelegateEvent::DiscoveredServices {
                    peripheral_uuid,
                    services: service_map,
                });
            }
        }

        #[method(peripheral:didDiscoverIncludedServicesForService:error:)]
        fn delegate_peripheral_diddiscoverincludedservicesforservice_error(
            &self,
            peripheral: &CBPeripheral,
            service: &CBService,
            error: Option<&NSError>,
        ) {
            trace!(
                "delegate_peripheral_diddiscoverincludedservicesforservice_error {} {} {}",
                peripheral_debug(peripheral),
                service_debug(service),
                localized_description(error)
            );
            if error.is_none() {
                let includes = unsafe { service.includedServices() }.unwrap_or_default();
                for s in includes {
                    unsafe { peripheral.discoverCharacteristics_forService(None, &s) };
                }
            }
        }

        #[method(peripheral:didDiscoverCharacteristicsForService:error:)]
        fn delegate_peripheral_diddiscovercharacteristicsforservice_error(
            &self,
            peripheral: &CBPeripheral,
            service: &CBService,
            error: Option<&NSError>,
        ) {
            trace!(
                "delegate_peripheral_diddiscovercharacteristicsforservice_error {} {} {}",
                peripheral_debug(peripheral),
                service_debug(service),
                localized_description(error)
            );
            if error.is_none() {
                // UBM patch (UBM_PATCHES.md #6): an included service is not
                // one of the peripheral's own; upstream never exposed its
                // characteristics either.
                let Some(service_uuid) = service_key(peripheral, service) else {
                    trace!("characteristics of an included service are not tracked");
                    return;
                };
                let chars = unsafe { service.characteristics() }.unwrap_or_default();
                let characteristics = keyed(chars, |c| cbuuid_to_uuid(&*unsafe { c.UUID() }));
                for c in characteristics.values() {
                    unsafe { peripheral.discoverDescriptorsForCharacteristic(c) };
                }
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                self.send_event(CentralDelegateEvent::DiscoveredCharacteristics {
                    peripheral_uuid,
                    service_uuid,
                    characteristics,
                });
            }
        }

        #[method(peripheral:didDiscoverDescriptorsForCharacteristic:error:)]
        fn delegate_peripheral_diddiscoverdescriptorsforcharacteristic_error(
            &self,
            peripheral: &CBPeripheral,
            characteristic: &CBCharacteristic,
            error: Option<&NSError>,
        ) {
            trace!(
                "delegate_peripheral_diddiscoverdescriptorsforcharacteristic_error {} {} {}",
                peripheral_debug(peripheral),
                characteristic_debug(characteristic),
                localized_description(error)
            );
            if error.is_none() {
                let Some((service_uuid, characteristic_uuid)) =
                    characteristic_key(peripheral, characteristic)
                else {
                    trace!("descriptors of an untracked characteristic are ignored");
                    return;
                };
                let descs = unsafe { characteristic.descriptors() }.unwrap_or_default();
                let descriptors = keyed(descs, |d| cbuuid_to_uuid(&*unsafe { d.UUID() }));
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                self.send_event(CentralDelegateEvent::DiscoveredCharacteristicDescriptors {
                    peripheral_uuid,
                    service_uuid,
                    characteristic_uuid,
                    descriptors,
                });
            }
        }

        #[method(peripheral:didUpdateValueForCharacteristic:error:)]
        fn delegate_peripheral_didupdatevalueforcharacteristic_error(
            &self,
            peripheral: &CBPeripheral,
            characteristic: &CBCharacteristic,
            error: Option<&NSError>,
        ) {
            trace!(
                "delegate_peripheral_didupdatevalueforcharacteristic_error {} {} {}",
                peripheral_debug(peripheral),
                characteristic_debug(characteristic),
                localized_description(error)
            );
            if let Some(error) = error {
                // UBM patch (UBM_PATCHES.md #14/#15): answer the waiter.
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                if let Some((service_uuid, characteristic_uuid)) =
                    characteristic_key(peripheral, characteristic)
                {
                    self.send_event(CentralDelegateEvent::AttributeFailed {
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                        descriptor_uuid: None,
                        stage: AttributeStage::Value,
                        error: nserror_platform(error),
                    });
                }
                return;
            }
            if error.is_none() {
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                let Some((service_uuid, characteristic_uuid)) =
                    characteristic_key(peripheral, characteristic)
                else {
                    trace!("event for an untracked characteristic ignored");
                    return;
                };
                self.send_event(CentralDelegateEvent::CharacteristicNotified {
                    peripheral_uuid,
                    service_uuid,
                    characteristic_uuid,
                    data: get_characteristic_value(characteristic),
                });
                // Notify BluetoothGATTCharacteristic::read_value that read was successful.
            }
        }

        #[method(peripheral:didWriteValueForCharacteristic:error:)]
        fn delegate_peripheral_didwritevalueforcharacteristic_error(
            &self,
            peripheral: &CBPeripheral,
            characteristic: &CBCharacteristic,
            error: Option<&NSError>,
        ) {
            trace!(
                "delegate_peripheral_didwritevalueforcharacteristic_error {} {} {}",
                peripheral_debug(peripheral),
                characteristic_debug(characteristic),
                localized_description(error)
            );
            if let Some(error) = error {
                // UBM patch (UBM_PATCHES.md #14/#15): answer the waiter.
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                if let Some((service_uuid, characteristic_uuid)) =
                    characteristic_key(peripheral, characteristic)
                {
                    self.send_event(CentralDelegateEvent::AttributeFailed {
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                        descriptor_uuid: None,
                        stage: AttributeStage::Write,
                        error: nserror_platform(error),
                    });
                }
                return;
            }
            if error.is_none() {
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                let Some((service_uuid, characteristic_uuid)) =
                    characteristic_key(peripheral, characteristic)
                else {
                    trace!("event for an untracked characteristic ignored");
                    return;
                };
                self.send_event(CentralDelegateEvent::CharacteristicWritten {
                    peripheral_uuid,
                    service_uuid,
                    characteristic_uuid,
                });
            }
        }

        #[method(peripheral:didUpdateNotificationStateForCharacteristic:error:)]
        fn delegate_peripheral_didupdatenotificationstateforcharacteristic_error(
            &self,
            peripheral: &CBPeripheral,
            characteristic: &CBCharacteristic,
            error: Option<&NSError>,
        ) {
            trace!("delegate_peripheral_didupdatenotificationstateforcharacteristic_error");
            if let Some(error) = error {
                // UBM patch (UBM_PATCHES.md #14/#15): answer the waiter.
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                if let Some((service_uuid, characteristic_uuid)) =
                    characteristic_key(peripheral, characteristic)
                {
                    self.send_event(CentralDelegateEvent::AttributeFailed {
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                        descriptor_uuid: None,
                        stage: AttributeStage::NotifyState,
                        error: nserror_platform(error),
                    });
                }
                return;
            }
            let id = unsafe { peripheral.identifier() };
            let peripheral_uuid = nsuuid_to_uuid(&id);
            let Some((service_uuid, characteristic_uuid)) =
                characteristic_key(peripheral, characteristic)
            else {
                trace!("notification state of an untracked characteristic ignored");
                return;
            };
            if unsafe { characteristic.isNotifying() } {
                self.send_event(CentralDelegateEvent::CharacteristicSubscribed {
                    peripheral_uuid,
                    service_uuid,
                    characteristic_uuid,
                });
            } else {
                self.send_event(CentralDelegateEvent::CharacteristicUnsubscribed {
                    peripheral_uuid,
                    service_uuid,
                    characteristic_uuid,
                });
            }
        }

        #[method(peripheral:didReadRSSI:error:)]
        fn delegate_peripheral_didreadrssi_error(
            &self,
            peripheral: &CBPeripheral,
            rssi: &NSNumber,
            error: Option<&NSError>,
        ) {
            trace!(
                "delegate_peripheral_didreadrssi_error {}",
                peripheral_debug(peripheral)
            );
            if error.is_none() {
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                let rssi_value = rssi.as_i16();
                self.send_event(CentralDelegateEvent::DidReadRssi {
                    peripheral_uuid,
                    rssi: rssi_value,
                });
            }
        }

        #[method(peripheral:didUpdateValueForDescriptor:error:)]
        fn delegate_peripheral_didupdatevaluefordescriptor_error(
            &self,
            peripheral: &CBPeripheral,
            descriptor: &CBDescriptor,
            error: Option<&NSError>,
        ) {
            trace!(
                "delegate_peripheral_didupdatevaluefordescriptor_error {} {} {}",
                peripheral_debug(peripheral),
                descriptor_debug(descriptor),
                localized_description(error)
            );
            if let Some(error) = error {
                // UBM patch (UBM_PATCHES.md #14/#15): answer the waiter.
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                if let Some((service_uuid, characteristic_uuid, descriptor_uuid)) =
                    descriptor_key(peripheral, descriptor)
                {
                    self.send_event(CentralDelegateEvent::AttributeFailed {
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                        descriptor_uuid: Some(descriptor_uuid),
                        stage: AttributeStage::Value,
                        error: nserror_platform(error),
                    });
                }
                return;
            }
            if error.is_none() {
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                let Some((service_uuid, characteristic_uuid, descriptor_uuid)) =
                    descriptor_key(peripheral, descriptor)
                else {
                    trace!("event for an untracked descriptor ignored");
                    return;
                };
                self.send_event(CentralDelegateEvent::DescriptorNotified {
                    peripheral_uuid,
                    service_uuid,
                    characteristic_uuid,
                    descriptor_uuid,
                    data: get_descriptor_value(&descriptor),
                });
                // Notify BluetoothGATTCharacteristic::read_value that read was successful.
            }
        }

        #[method(peripheral:didWriteValueForDescriptor:error:)]
        fn delegate_peripheral_didwritevaluefordescriptor_error(
            &self,
            peripheral: &CBPeripheral,
            descriptor: &CBDescriptor,
            error: Option<&NSError>,
        ) {
            trace!(
                "delegate_peripheral_didwritevaluefordescriptor_error {} {} {}",
                peripheral_debug(peripheral),
                descriptor_debug(descriptor),
                localized_description(error)
            );
            if let Some(error) = error {
                // UBM patch (UBM_PATCHES.md #14/#15): answer the waiter.
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                if let Some((service_uuid, characteristic_uuid, descriptor_uuid)) =
                    descriptor_key(peripheral, descriptor)
                {
                    self.send_event(CentralDelegateEvent::AttributeFailed {
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                        descriptor_uuid: Some(descriptor_uuid),
                        stage: AttributeStage::Write,
                        error: nserror_platform(error),
                    });
                }
                return;
            }
            if error.is_none() {
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                let Some((service_uuid, characteristic_uuid, descriptor_uuid)) =
                    descriptor_key(peripheral, descriptor)
                else {
                    trace!("event for an untracked descriptor ignored");
                    return;
                };
                self.send_event(CentralDelegateEvent::DescriptorWritten {
                    peripheral_uuid,
                    service_uuid,
                    characteristic_uuid,
                    descriptor_uuid,
                });
            }
        }

        #[method(peripheral:didModifyServices:)]
        fn delegate_peripheral_didmodifyservices(
            &self,
            peripheral: &CBPeripheral,
            _invalidated_services: &NSArray<CBService>,
        ) {
            trace!(
                "delegate_peripheral_didmodifyservices {}",
                peripheral_debug(peripheral),
            );
            // This is a corebluetooth-only event that makes peripheral services unusable until discovery has been performed again.
            // https://developer.apple.com/documentation/corebluetooth/cbperipheraldelegate/peripheral(_:didmodifyservices:)?language=objc
            // Trigger the removal of internal corebluetooth peripheral discovered services. It is also expected that
            // discover_services() will be performed again on the peripheral at the API level as soon as is practical.
            // NOTE: the list of modified services does not appear to be particularly useful; a full service rediscovery is needed.
            let id = unsafe { peripheral.identifier() };
            let peripheral_uuid = nsuuid_to_uuid(&id);
            self.send_event(CentralDelegateEvent::ServicesModified {
                peripheral_uuid,
            });
        }

        #[method(peripheralIsReadyToSendWriteWithoutResponse:)]
        fn delegate_peripheral_is_ready_to_send_write_without_response(
            &self,
            peripheral: &CBPeripheral,
        ) {
            trace!(
                "delegate_peripheral_is_ready_to_send_write_without_response {}",
                peripheral_debug(peripheral)
            );
            let id = unsafe { peripheral.identifier() };
            let peripheral_uuid = nsuuid_to_uuid(&id);
            self.send_event(CentralDelegateEvent::ReadyToSendWriteWithoutResponse {
                peripheral_uuid,
            });
        }
    }
);

impl CentralDelegate {
    pub fn new(sender: Sender<CentralDelegateEvent>) -> Retained<Self> {
        let this = CentralDelegate::alloc().set_ivars(sender);
        unsafe { msg_send_id![super(this), init] }
    }

    fn send_event(&self, event: CentralDelegateEvent) {
        let mut sender = self.ivars().clone();
        futures::executor::block_on(async {
            if let Err(e) = sender.send(event).await {
                error!("Error sending delegate event: {}", e);
            }
        });
    }
}

fn localized_description(error: Option<&NSError>) -> String {
    if let Some(error) = error {
        error.localizedDescription().to_string()
    } else {
        "".to_string()
    }
}

fn get_characteristic_value(characteristic: &CBCharacteristic) -> Vec<u8> {
    trace!("Getting data!");
    let v = unsafe { characteristic.value() }.map(|value| value.bytes().into());
    trace!("BluetoothGATTCharacteristic::get_value -> {:?}", v);
    v.unwrap_or_default()
}

fn get_descriptor_value(descriptor: &CBDescriptor) -> Vec<u8> {
    trace!("Getting data!");
    let v = unsafe { descriptor.value() }.map(|value| unsafe {
        let mut clazz = value.class();
        // Find the root class until we reach NSObject
        while let Some(superclass) = clazz.superclass() {
            if superclass == NSObject::class() {
                break;
            }
            clazz = superclass;
        }

        match clazz.name() {
            "NSString" => {
                let d: Retained<NSString> = Retained::cast(value);
                d.to_string().into_bytes()
            }
            "NSData" => {
                let d: Retained<NSData> = Retained::cast(value);
                d.bytes().into()
            }
            "NSNumber" => {
                let d: Retained<NSNumber> = Retained::cast(value);
                d.stringValue().to_string().into_bytes()
            }
            _ => {
                error!("Unknown descriptor value class: {:?}", clazz);
                Vec::new()
            }
        }
    });
    trace!("BluetoothGATTDescriptor::get_value -> {:?}", v);
    v.unwrap_or_default()
}

fn peripheral_debug(peripheral: &CBPeripheral) -> String {
    let uuid = unsafe { peripheral.identifier() }.UUIDString();
    match unsafe { peripheral.name() } {
        Some(name) => {
            format!("CBPeripheral({}, {})", name, uuid)
        }
        _ => {
            format!("CBPeripheral({})", uuid)
        }
    }
}

fn service_debug(service: &CBService) -> String {
    let uuid = unsafe { service.UUID().UUIDString() };
    format!("CBService({})", uuid)
}

fn characteristic_debug(characteristic: &CBCharacteristic) -> String {
    let uuid = unsafe { characteristic.UUID().UUIDString() };
    format!("CBCharacteristic({})", uuid)
}

fn descriptor_debug(descriptor: &CBDescriptor) -> String {
    let uuid = unsafe { descriptor.UUID().UUIDString() };
    format!("CBDescriptor({})", uuid)
}
