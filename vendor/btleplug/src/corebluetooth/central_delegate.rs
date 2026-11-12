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
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    fmt::{self, Debug, Formatter},
    ops::Deref,
};
use uuid::Uuid;

fn bounded_discovery_array<T>(values: Retained<NSArray<T>>, error: Option<crate::PlatformError>) -> (Retained<NSArray<T>>, Option<crate::PlatformError>)
where T: ClassType, T: objc2::mutability::IsRetainable {
    if values.len() > 4096 {
        let error = error.or_else(|| Some(crate::PlatformError::new("corebluetooth", "discovery-bound", "The discovery callback exceeds the owned snapshot bound")));
        return (NSArray::from_vec(Vec::new()), error);
    }
    (NSArray::from_vec(values.iter().map(|value| value.retain()).collect()), error)
}

/// UBM patch (UBM_PATCHES.md #6): one GATT attribute instance. CoreBluetooth
/// reports no ATT handles, so `instance` is the attribute's position in its
/// parent's CoreBluetooth array (`peripheral.services`,
/// `service.characteristics`, `characteristic.descriptors`): discovery
/// order. Upstream keyed attributes by UUID alone, so same-UUID siblings
/// collapsed into one.
#[derive(Clone, Debug)]
pub(super) enum PeerLifecycle { Disconnected, DisconnectedWithError(crate::PlatformError), ServicesModified }

#[derive(Debug)]
pub(super) enum PeerActorCallback {
    CharacteristicWritten { route: (AttrKey, AttrKey) },
    DescriptorNotified { route: (AttrKey, AttrKey), descriptor: AttrKey, data: Vec<u8> },
    DescriptorWritten { route: (AttrKey, AttrKey), descriptor: AttrKey },
    AttributeFailed { route: (AttrKey, AttrKey), descriptor: Option<AttrKey>, stage: AttributeStage, error: crate::PlatformError },
}

impl PeerActorCallback {
    pub(super) fn into_event(self, peer: Uuid) -> CentralDelegateEvent {
        match self {
            Self::CharacteristicWritten { route } => CentralDelegateEvent::CharacteristicWritten { peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1 },
            Self::DescriptorNotified { route, descriptor, data } => CentralDelegateEvent::DescriptorNotified { peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, descriptor_uuid: descriptor, data },
            Self::DescriptorWritten { route, descriptor } => CentralDelegateEvent::DescriptorWritten { peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, descriptor_uuid: descriptor },
            Self::AttributeFailed { route, descriptor, stage, error } => CentralDelegateEvent::AttributeFailed { peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, descriptor_uuid: descriptor, stage, error },
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum PeerDelegateCallback {
    ValueError { route: (AttrKey, AttrKey), notifying: bool, error: crate::PlatformError },
    CapturedValueError { route: (AttrKey, AttrKey), error: crate::PlatformError, read_failed: bool },
    NotificationState { route: (AttrKey, AttrKey), notifying: bool },
    CapturedNotificationState { route: (AttrKey, AttrKey), notifying: bool, cleanup: Option<u64> },
    NotificationError { route: (AttrKey, AttrKey), error: crate::PlatformError },
}

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

/// Stable position of one service object in this discovery's graph. Included
/// objects append once; duplicate UUIDs and cycles do not change earlier keys.
fn object_identity<T: objc2::Message>(object: &T) -> usize {
    std::ptr::from_ref(object).addr()
}

fn service_key(
    delegate: &CentralDelegate,
    peripheral: &CBPeripheral,
    service: &CBService,
) -> Option<AttrKey> {
    let id = nsuuid_to_uuid(&*unsafe { peripheral.identifier() });
    let tables = delegate.ivars().services.borrow();
    let services = tables.get(&id)?;
    Some(AttrKey {
        uuid: cbuuid_to_uuid(&*unsafe { service.UUID() }),
        instance: services
            .iter()
            .position(|candidate| std::ptr::eq(&**candidate, service))?
            .try_into()
            .ok()?,
    })
}

/// The service and characteristic keys of one characteristic.
fn characteristic_key(
    delegate: &CentralDelegate,
    peripheral: &CBPeripheral,
    characteristic: &CBCharacteristic,
) -> Option<(AttrKey, AttrKey)> {
    let service = unsafe { characteristic.service() }?;
    let service_key = service_key(delegate, peripheral, &service)?;
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
    delegate: &CentralDelegate,
    peripheral: &CBPeripheral,
    descriptor: &CBDescriptor,
) -> Option<(AttrKey, AttrKey, AttrKey)> {
    let characteristic = unsafe { descriptor.characteristic() }?;
    let (service_key, characteristic_key) =
        characteristic_key(delegate, peripheral, &characteristic)?;
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

/// Parse CoreBluetooth's NSData at the callback boundary, including its
/// little-endian company identifier. The returned bytes own their storage.
fn advertisement_manufacturer_data(
    adv_data: &NSDictionary<NSString, AnyObject>,
) -> Option<(u16, Vec<u8>)> {
    let value = adv_data.get(unsafe { CBAdvertisementDataManufacturerDataKey })?;
    // SAFETY: CoreBluetooth specifies NSData for this key.
    let pointer: *const AnyObject = value;
    let data = unsafe { &*pointer.cast::<NSData>() };
    let bytes = data.bytes();
    let company = u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]);
    Some((company, bytes[2..].to_vec()))
}

#[cfg(test)]
mod ubm_advertisement_boundary_tests {
    use super::*;
    use objc2_foundation::NSMutableData;

    fn dictionary(
        keys: &[&NSString],
        values: &[&NSObject],
    ) -> Retained<NSDictionary<NSString, AnyObject>> {
        // SAFETY: every borrowed Foundation object is alive for construction;
        // the dictionary owns these retains and is dropped before mutation.
        let retained = values
            .iter()
            .map(|value| unsafe {
                Retained::retain(std::ptr::from_ref(*value).cast_mut()).expect("Foundation object")
            })
            .collect();
        let dictionary = NSDictionary::<NSString, NSObject>::from_vec(keys, retained);
        // SAFETY: NSObject values are Objective-C objects. Only the generic
        // view changes; NSDictionary retains the real Foundation objects.
        unsafe { Retained::cast(dictionary) }
    }

    #[test]
    fn advertisement_extras_preserve_absent_and_present_empty_fields() {
        let absent = dictionary(&[], &[]);
        let extras = advertisement_extras(&absent);
        assert_eq!(extras.solicited_service_uuids, None);
        assert_eq!(extras.overflow_service_uuids, None);
        assert_eq!(extras.connectable, None);
        let empty = NSArray::<CBUUID>::from_vec(vec![]);
        let connectable = NSNumber::new_bool(false);
        let values: [&NSObject; 3] = [&empty, &empty, &connectable];
        let keys = unsafe {
            [
                CBAdvertisementDataSolicitedServiceUUIDsKey,
                CBAdvertisementDataOverflowServiceUUIDsKey,
                CBAdvertisementDataIsConnectable,
            ]
        };
        let present = dictionary(&keys, &values);
        let extras = advertisement_extras(&present);
        assert_eq!(extras.solicited_service_uuids, Some(vec![]));
        assert_eq!(extras.overflow_service_uuids, Some(vec![]));
        assert_eq!(extras.connectable, Some(false));
    }

    #[test]
    fn manufacturer_data_requires_a_company_id_and_copies_the_payload() {
        let key = unsafe { CBAdvertisementDataManufacturerDataKey };
        let absent = dictionary(&[], &[]);
        assert_eq!(advertisement_manufacturer_data(&absent), None);
        for bytes in [vec![], vec![0x6b]] {
            let data = NSData::with_bytes(&bytes);
            let values: [&NSObject; 1] = [&data];
            let dictionary = dictionary(&[key], &values);
            assert_eq!(advertisement_manufacturer_data(&dictionary), None);
        }
        let mut data = NSMutableData::with_bytes(&[0x6b, 0x00, 0x11, 0x22]);
        let parsed = {
            let values: [&NSObject; 1] = [&data];
            let dictionary = dictionary(&[key], &values);
            advertisement_manufacturer_data(&dictionary).expect("complete company id")
        };
        data.bytes_mut().fill(0xff);
        assert_eq!(parsed, (0x006b, vec![0x11, 0x22]));
        let data = NSData::with_bytes(&[0x34, 0x12]);
        let values: [&NSObject; 1] = [&data];
        let dictionary = dictionary(&[key], &values);
        assert_eq!(
            advertisement_manufacturer_data(&dictionary),
            Some((0x1234, vec![]))
        );
    }
}

pub enum CentralDelegateEvent {
    Ticketed { peer: Uuid, ticket: super::internal::PeerCallbackTicket, event: Box<CentralDelegateEvent> },
    AdapterTicketed { ticket: super::internal::AdapterEventTicket, event: Box<CentralDelegateEvent> },
    DidUpdateState {
        state: CBManagerState,
    },
    AdapterBarrierFailed { error: crate::PlatformError },
    /// A serial-delegate-queue marker. All discovery callbacks accepted
    /// before it already precede it in the actor channel.
    ScanIngressFence {
        epoch: u64,
        fence_id: u64,
    },
    DiscoveredPeripheral {
        cbperipheral: Retained<CBPeripheral>,
        advertisement_name: Option<String>,
        native_token: Option<super::native_registry::NativeObjectToken>,
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
    DiscoveryFailed {
        peripheral_uuid: Uuid,
        error: crate::PlatformError,
    },
    DiscoveryDrained {
        peripheral_uuid: Uuid,
    },
    DiscoveredIncludedServices {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        added: HashMap<AttrKey, Retained<CBService>>,
        result: Result<Vec<AttrKey>, crate::PlatformError>,
    },
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
    // UBM patch (UBM_PATCHES.md #15): the `NSError` CoreBluetooth failed
    // the connect with, as the platform's answer (`None` when it gave none).
    ConnectionFailed {
        peripheral_uuid: Uuid,
        error: Option<crate::PlatformError>,
    },
    /// A native connection callback which did not match an admitted exact
    /// manager/peripheral attempt.  It is deliberately handed to the actor as
    /// an observation, rather than being attributed to whichever UUID owner
    /// happens to be current.
    UnassociatedConnectionObservation {
        peripheral_uuid: Uuid,
        error: Option<crate::PlatformError>,
    },
    DisconnectedDevice {
        peripheral_uuid: Uuid,
        error: Option<crate::PlatformError>,
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
        cleanup: u64,
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
    // `advertisement_name` is this packet's advertised name, or nothing
    // when the packet carries none (finding 205: the report itself wears
    // the GAP fallback then, and the internal loop replaces it with the
    // merged advertised name when one is known).
    Advertised {
        peripheral_uuid: Uuid,
        report: crate::api::AdvertisementReport,
        advertisement_name: Option<String>,
        /// Captured on CoreBluetooth's serial delegate queue when the native
        /// callback arrives. It is deliberately not looked up again by the
        /// asynchronous actor, which may already be serving another scan.
        ingress_epoch: Option<u64>,
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
            CentralDelegateEvent::Ticketed { event, .. } | CentralDelegateEvent::AdapterTicketed { event, .. } => f.debug_tuple("Ticketed").field(event).finish(),
            CentralDelegateEvent::AdapterBarrierFailed { error } => f.debug_tuple("AdapterBarrierFailed").field(error).finish(),
            CentralDelegateEvent::ScanIngressFence { epoch, fence_id } => f
                .debug_struct("ScanIngressFence")
                .field("epoch", epoch)
                .field("fence_id", fence_id)
                .finish(),
            CentralDelegateEvent::DidUpdateState { state } => f
                .debug_struct("CentralDelegateEvent")
                .field("state", state)
                .finish(),
            CentralDelegateEvent::DiscoveredPeripheral {
                cbperipheral,
                advertisement_name,
                native_token,
            } => f
                .debug_struct("CentralDelegateEvent")
                .field("cbperipheral", cbperipheral.deref())
                .field("advertisement_name", advertisement_name)
                .field("native_token", native_token)
                .finish(),
            CentralDelegateEvent::DiscoveredServices {
                peripheral_uuid,
                services,
            } => f
                .debug_struct("DiscoveredServices")
                .field("peripheral_uuid", peripheral_uuid)
                .field("services", &services.keys().collect::<Vec<_>>())
                .finish(),
            CentralDelegateEvent::DiscoveryDrained { peripheral_uuid } => f
                .debug_struct("DiscoveryDrained")
                .field("peripheral_uuid", peripheral_uuid)
                .finish(),
            CentralDelegateEvent::DiscoveryFailed {
                peripheral_uuid,
                error,
            } => f
                .debug_struct("DiscoveryFailed")
                .field("peripheral_uuid", peripheral_uuid)
                .field("error", error)
                .finish(),
            CentralDelegateEvent::DiscoveredIncludedServices {
                peripheral_uuid,
                service_uuid,
                added,
                result,
            } => f
                .debug_struct("DiscoveredIncludedServices")
                .field("peripheral_uuid", peripheral_uuid)
                .field("service_uuid", service_uuid)
                .field("added", &added.keys().collect::<Vec<_>>())
                .field("result", result)
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
                error,
            } => f
                .debug_struct("ConnectionFailed")
                .field("peripheral_uuid", peripheral_uuid)
                .field("error", error)
                .finish(),
            CentralDelegateEvent::UnassociatedConnectionObservation {
                peripheral_uuid,
                error,
            } => f
                .debug_struct("UnassociatedConnectionObservation")
                .field("peripheral_uuid", peripheral_uuid)
                .field("error", error)
                .finish(),
            CentralDelegateEvent::DisconnectedDevice { peripheral_uuid, error } => f
                .debug_struct("DisconnectedDevice")
                .field("peripheral_uuid", peripheral_uuid)
                .field("error", error)
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
                cleanup: _,
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
                advertisement_name,
                ingress_epoch,
            } => f
                .debug_struct("Advertised")
                .field("peripheral_uuid", peripheral_uuid)
                .field("report", report)
                .field("advertisement_name", advertisement_name)
                .field("ingress_epoch", ingress_epoch)
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

// This key addresses storage owned by the specific serial CoreBluetooth queue.
// The context never crosses queues; jobs retrieve it only while on that queue.
static DELEGATE_CONTEXT_KEY: u8 = 0;

/// A single atomic delegate-queue admission generation.  Native state
/// callbacks run on the serial CoreBluetooth queue while actor requests are
/// enqueued from another executor, so the request must carry the generation
/// it observed.  A request from before a reset must not become eligible again
/// merely because a later `PoweredOn` callback ran before its queued job.
#[derive(Debug)]
pub(super) struct AdapterDispatchFence(std::sync::atomic::AtomicU64);

/// A command whose ordering is defined by CoreBluetooth's serial delegate
/// queue. CoreBluetooth offers no physical scan identifier, so this is the
/// narrowest possible scan-ingress ownership boundary.
#[derive(Clone, Copy, Debug)]
pub(super) enum ScanIngressAction {
    Arm,
    Retire,
    Fence,
}

impl AdapterDispatchFence {
    fn new() -> Self {
        // Generation one, initially available. A production manager publishes
        // its first state before discovery can create a native request; this
        // default preserves the radio-free queue fixtures that have no native
        // state callback.
        Self(std::sync::atomic::AtomicU64::new(3))
    }

    fn observe(&self, state: CBManagerState) {
        use std::sync::atomic::Ordering;
        let available = u64::from(matches!(state, CBManagerState::PoweredOn));
        let mut previous = self.0.load(Ordering::Acquire);
        loop {
            let next = previous.saturating_add(2) & !1 | available;
            match self.0.compare_exchange_weak(previous, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(observed) => previous = observed,
            }
        }
    }

    fn snapshot(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }

    fn admits(&self, snapshot: u64) -> bool {
        snapshot & 1 == 1 && self.0.load(std::sync::atomic::Ordering::Acquire) == snapshot
    }
}

struct DelegateQueueContext {
    delegate: Retained<CentralDelegate>,
    queue: super::ffi::dispatch_queue_t,
    native_lane: Option<super::native_lane::NativeLane<
        Retained<CBCentralManager>,
        Retained<CBPeripheral>,
        std::sync::Arc<super::internal::PeerSourceOwner>,
        super::internal::CoreBluetoothReplyStateShared,
        Option<crate::PlatformError>,
    >>,
    #[cfg(test)] test_events: Option<futures::channel::mpsc::Receiver<CentralDelegateEvent>>,
    #[cfg(test)] adapter_invalidation_observer: Option<Box<dyn FnMut(CBManagerState)>>,
}

struct DelegateScanIngressJob {
    action: ScanIngressAction,
    epoch: u64,
    fence_id: Option<u64>,
    reply: super::internal::CoreBluetoothReplyStateShared,
}

fn scan_ingress_error(code: &'static str, message: impl Into<String>) -> crate::PlatformError {
    crate::PlatformError::new("corebluetooth", code, message.into())
}

unsafe extern "C" fn run_delegate_scan_ingress(context: *mut std::ffi::c_void) {
    let job = unsafe { Box::from_raw(context.cast::<DelegateScanIngressJob>()) };
    let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
    let local = unsafe { super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(context) = (!local.is_null()).then(|| unsafe { &mut *local }) else {
            return Err(scan_ingress_error(
                "scan-ingress-delegate-unavailable",
                "The CoreBluetooth delegate queue is unavailable",
            ));
        };
        let delegate = &context.delegate;
        match job.action {
            ScanIngressAction::Arm => delegate.arm_scan_ingress(job.epoch),
            ScanIngressAction::Retire => delegate.retire_scan_ingress(job.epoch),
            ScanIngressAction::Fence => delegate.enqueue_scan_ingress_fence(
                job.epoch,
                job.fence_id.ok_or_else(|| scan_ingress_error(
                    "scan-ingress-fence-id-missing",
                    "The CoreBluetooth scan ingress fence had no adapter acknowledgment identifier",
                ))?,
            ),
        }
    }))
    .unwrap_or_else(|_| {
        Err(scan_ingress_error(
            "scan-ingress-delegate-failed",
            "The CoreBluetooth delegate queue rejected a scan ingress command",
        ))
    });
    match result {
        Ok(()) if !matches!(job.action, ScanIngressAction::Fence) => {
            job.reply.lock().unwrap().set_reply(super::internal::CoreBluetoothReply::Ok);
        }
        Ok(()) => {}
        Err(error) => {
            job.reply
                .lock()
                .unwrap()
                .set_reply(super::internal::CoreBluetoothReply::Failed(error));
        }
    }
}

/// Queue a command behind every delegate callback that CoreBluetooth had
/// already accepted. `Fence` completes only when the actor has consumed this
/// ordered marker, so its caller also knows the accepted callback prefix has
/// crossed the delegate-to-actor handoff.
pub(super) unsafe fn schedule_scan_ingress(
    queue: super::ffi::dispatch_queue_t,
    registry: std::sync::Arc<super::internal::PeerSourceRegistry>,
    action: ScanIngressAction,
    epoch: u64,
    fence_id: Option<u64>,
    reply: super::internal::CoreBluetoothReplyStateShared,
) -> Result<(), ()> {
    if registry.queue_closed.load(std::sync::atomic::Ordering::Acquire) {
        return Err(());
    }
    let job = Box::new(DelegateScanIngressJob { action, epoch, fence_id, reply });
    fn require_send<T: Send>(_: &T) {}
    require_send(&job);
    unsafe { super::ffi::dispatch_async_f(queue, Box::into_raw(job).cast(), run_delegate_scan_ingress) };
    Ok(())
}

unsafe extern "C" fn drop_delegate_queue_context(_: *mut std::ffi::c_void) {
    let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
    let context_ptr = unsafe { super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
    if !context_ptr.is_null() {
        let queue = unsafe { (*context_ptr).queue };
        unsafe { super::ffi::dispatch_queue_set_specific(queue, key, std::ptr::null_mut(), None) };
        // This is the final delegate-queue turn. Settle every lane-owned
        // future before dropping native retains; no CoreBluetooth callback can
        // arrive after the context is removed.
        let context = unsafe { &mut *context_ptr };
        if let Some(lane) = context.native_lane.as_mut() {
            let error = crate::PlatformError::new(
                "corebluetooth",
                "delegate-queue-closed",
                "The CoreBluetooth delegate queue closed before the native connection cycle settled",
            );
            context.delegate.fail_control(error.clone(), None);
            for action in lane.abort_all(Some(error)) {
                settle_aborted_lane_action(action);
            }
        }
        // Context ownership was transferred to this queue by ensure_queue_context.
        drop(unsafe { Box::from_raw(context_ptr) });
    }
}

/// The manager remains alive across an adapter reset, but every native
/// peripheral and pending CoreBluetooth callback from before that reset is no
/// longer a usable completion.  Settle the queue-owned callers before we
/// invalidate the actor graph.  This must run on the delegate queue: the lane
/// owns retained Objective-C objects and no cross-thread handle may retire
/// them.
fn abort_lane_for_adapter_invalidation(state: CBManagerState) {
    debug_assert!(invalidating_adapter_state(state));
    let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
    let local = unsafe { super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
    // Unit tests exercise adapter graph ordering without installing a native
    // lane.  Production `centralManagerDidUpdateState:` always installs the
    // queue context before queuing the state transition.
    let Some(context) = (!local.is_null()).then(|| unsafe { &mut *local }) else {
        return;
    };
    #[cfg(test)]
    if let Some(observer) = context.adapter_invalidation_observer.as_mut() {
        observer(state);
    }
    let Some(lane) = context.native_lane.as_mut() else {
        return;
    };
    let error = crate::PlatformError::new(
        "corebluetooth",
        "adapter-unavailable",
        "CoreBluetooth invalidated pending native connection work",
    )
    .with("state", format!("{state:?}"));
    for action in lane.abort_all(Some(error)) {
        settle_aborted_lane_action(action);
    }
    // Do not call `fail_control` here.  Reset and power restoration are
    // recoverable adapter transitions; the per-operation replies above are
    // their concrete outcome, while a later discovery may populate a new
    // lane cycle.
}

pub(super) unsafe fn schedule_delegate_queue_cleanup(queue: super::ffi::dispatch_queue_t) {
    unsafe { super::ffi::dispatch_async_f(queue, std::ptr::null_mut(), drop_delegate_queue_context) };
}

struct DelegateReplayJob {
    owner: std::sync::Arc<super::internal::PeerSourceOwner>,
    registry: std::sync::Arc<super::internal::PeerSourceRegistry>,
    peer: Uuid,
    key: crate::ubm_reclaim_authority::GraphKey,
    reply: tokio::sync::oneshot::Sender<Result<crate::ubm_reclaim_authority::PermitStatus, crate::ubm_reclaim_authority::AuthorityError>>,
}

/// Send-safe connect admission from the actor to the serial CoreBluetooth
/// queue.  The queue owns the retained `CBPeripheral`; the actor carries only
/// its logical owner and reply destination.
struct DelegateConnectJob {
    identity: super::internal::NativeRequestIdentity,
    reply: super::internal::CoreBluetoothReplyStateShared,
    registry: std::sync::Arc<super::internal::PeerSourceRegistry>,
    adapter_fence: std::sync::Arc<AdapterDispatchFence>,
    adapter_generation: u64,
}

fn settle_connect_reply(
    state: super::internal::CoreBluetoothReplyStateShared,
    result: Result<(), Option<crate::PlatformError>>,
) {
    let reply = match result {
        Ok(()) => super::internal::CoreBluetoothReply::Connected,
        Err(Some(error)) => super::internal::CoreBluetoothReply::Failed(error),
        Err(None) => super::internal::CoreBluetoothReply::Err("Connection failed".to_owned()),
    };
    let (_, waker) = state.lock().unwrap().try_set_reply_deferred(reply);
    if let Some(waker) = waker { waker.wake(); }
}

/// Complete only the disconnect requests carried by the terminal native
/// cycle.  The actor no longer owns these reply states: it cannot safely
/// recover their identity after a peer owner is replaced.
fn settle_disconnect_replies(
    replies: Vec<super::internal::CoreBluetoothReplyStateShared>,
    error: Option<crate::PlatformError>,
) {
    let reply = match error {
        Some(error) => super::internal::CoreBluetoothReply::Failed(error),
        None => super::internal::CoreBluetoothReply::Ok,
    };
    let mut wakers = Vec::with_capacity(replies.len());
    for state in replies {
        let (accepted, waker) = state.lock().unwrap().try_set_reply_deferred(reply.clone());
        if !accepted { log::warn!("CoreBluetooth disconnect reply arrived after terminal completion"); }
        if let Some(waker) = waker { wakers.push(waker); }
    }
    for waker in wakers { waker.wake(); }
}

/// Queue shutdown has no actor callback to finish a native cycle. Settle its
/// retained caller replies directly after the registry has recorded the
/// terminal failure. This is intentionally exhaustive so a queued request
/// cannot be lost merely because its predecessor never produced a callback.
fn settle_aborted_lane_action(
    action: super::native_attempt::Action<
        super::native_registry::NativeObjectToken,
        std::sync::Arc<super::internal::PeerSourceOwner>,
        super::internal::CoreBluetoothReplyStateShared,
        Option<crate::PlatformError>,
    >,
) {
    match action {
        super::native_attempt::Action::DisconnectedWhileConnecting { reply, error, disconnect_replies, .. } => {
            let error = error.flatten();
            settle_connect_reply(reply, Err(error.clone()));
            settle_disconnect_replies(disconnect_replies, error);
        }
        super::native_attempt::Action::Disconnected { error, disconnect_replies, .. } => {
            settle_disconnect_replies(disconnect_replies, error.flatten());
        }
        super::native_attempt::Action::CancelledQueued { reply, .. } => {
            settle_connect_reply(reply, Err(Some(crate::PlatformError::new(
                "corebluetooth",
                "delegate-queue-closed",
                "The queued CoreBluetooth connection was never dispatched because the delegate queue closed",
            ))));
        }
        super::native_attempt::Action::Connect { .. }
        | super::native_attempt::Action::Cancel { .. }
        | super::native_attempt::Action::ConnectResult { .. }
        | super::native_attempt::Action::Unassociated { .. } => {
            unreachable!("only retained requests may be aborted during queue shutdown")
        }
    }
}

fn refuse_connect_before_dispatch(
    identity: &super::internal::NativeRequestIdentity,
    reply: super::internal::CoreBluetoothReplyStateShared,
    code: &'static str,
    message: &'static str,
) {
    identity.admission().refuse_before_dispatch();
    let error = crate::PlatformError::new("corebluetooth", code, message);
    let (_, waker) = reply.lock().unwrap().try_set_reply_deferred(
        super::internal::CoreBluetoothReply::Failed(error),
    );
    if let Some(waker) = waker { waker.wake(); }
}

unsafe extern "C" fn run_delegate_connect(context: *mut std::ffi::c_void) {
    let job = unsafe { Box::from_raw(context.cast::<DelegateConnectJob>()) };
    let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
    let local = unsafe { super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if local.is_null() {
            refuse_connect_before_dispatch(&job.identity, job.reply.clone(), "connect-delegate-unavailable", "The CoreBluetooth delegate queue is unavailable");
            return;
        }
        if !job.adapter_fence.admits(job.adapter_generation) {
            refuse_connect_before_dispatch(&job.identity, job.reply.clone(), "connect-adapter-unavailable", "The CoreBluetooth adapter changed state before the connect request reached its delegate queue");
            return;
        }
        if !job.registry.get(job.identity.uuid()).is_some_and(|owner| std::sync::Arc::ptr_eq(&owner, job.identity.owner())) {
            refuse_connect_before_dispatch(&job.identity, job.reply.clone(), "connect-owner-retired", "The connect request's peer owner was retired before dispatch");
            return;
        }
        let context = unsafe { &mut *local };
        let Some(lane) = context.native_lane.as_mut() else {
            refuse_connect_before_dispatch(&job.identity, job.reply.clone(), "connect-native-identity-unavailable", "No delegate-queue native identity is available for this peer");
            return;
        };
        let peer = job.identity.uuid();
        let owner = job.identity.owner().clone();
        let action = lane.connect_remembered(peer, owner, job.reply.clone(), std::sync::Arc::ptr_eq);
        let Ok((_id, action)) = action else {
            refuse_connect_before_dispatch(&job.identity, job.reply.clone(), "connect-native-identity-unavailable", "The requested owner has no remembered CoreBluetooth peripheral");
            return;
        };
        if let Some(super::native_attempt::Action::Connect { object, .. }) = action {
            let peripheral = lane.resolve(peer, object).expect("lane connect action must retain its native peripheral");
            unsafe { lane.manager().connectPeripheral_options(peripheral, None) };
        }
    }));
    if result.is_err() {
        refuse_connect_before_dispatch(&job.identity, job.reply, "connect-delegate-failed", "The CoreBluetooth delegate queue rejected the connect request");
    }
}

/// Called by the actor after it wins caller cancellation admission.  No
/// Objective-C object crosses this boundary; dispatch happens only after the
/// queue has selected the discovery-retained exact peripheral for this owner.
pub(super) unsafe fn schedule_delegate_connect(
    queue: super::ffi::dispatch_queue_t,
    registry: std::sync::Arc<super::internal::PeerSourceRegistry>,
    identity: super::internal::NativeRequestIdentity,
    reply: super::internal::CoreBluetoothReplyStateShared,
    adapter_fence: std::sync::Arc<AdapterDispatchFence>,
) -> Result<(), ()> {
    if registry.queue_closed.load(std::sync::atomic::Ordering::Acquire) { return Err(()); }
    let adapter_generation = adapter_fence.snapshot();
    let job = Box::new(DelegateConnectJob { identity, reply, registry, adapter_fence, adapter_generation });
    fn require_send<T: Send>(_: &T) {}
    require_send(&job);
    unsafe { super::ffi::dispatch_async_f(queue, Box::into_raw(job).cast(), run_delegate_connect) };
    Ok(())
}

struct DelegateDisconnectJob {
    identity: super::internal::NativeRequestIdentity,
    reply: super::internal::CoreBluetoothReplyStateShared,
    adapter_fence: std::sync::Arc<AdapterDispatchFence>,
    adapter_generation: u64,
}

unsafe extern "C" fn run_delegate_disconnect(context: *mut std::ffi::c_void) {
    let job = unsafe { Box::from_raw(context.cast::<DelegateDisconnectJob>()) };
    let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
    let local = unsafe { super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(context) = (!local.is_null()).then(|| unsafe { &mut *local }) else {
            refuse_connect_before_dispatch(&job.identity, job.reply.clone(), "disconnect-delegate-unavailable", "The CoreBluetooth delegate queue is unavailable");
            return;
        };
        if !job.adapter_fence.admits(job.adapter_generation) {
            refuse_connect_before_dispatch(&job.identity, job.reply.clone(), "disconnect-adapter-unavailable", "The CoreBluetooth adapter changed state before the disconnect request reached its delegate queue");
            return;
        }
        let Some(lane) = context.native_lane.as_mut() else {
            refuse_connect_before_dispatch(&job.identity, job.reply.clone(), "disconnect-native-identity-unavailable", "No delegate-queue native connection cycle is available for this peer");
            return;
        };
        let peer = job.identity.uuid();
        match lane.disconnect_for_owner(peer, job.identity.owner(), job.reply.clone(), std::sync::Arc::ptr_eq) {
            Ok(Some(super::native_attempt::Action::Cancel { object, .. })) => {
                let peripheral = lane.resolve(peer, object).expect("lane disconnect action must retain its native peripheral");
                unsafe { lane.manager().cancelPeripheralConnection(peripheral) };
            }
            Ok(None) => {}
            Ok(Some(_)) => unreachable!("disconnect can only issue native cancellation"),
            Err(_) => refuse_connect_before_dispatch(&job.identity, job.reply.clone(), "disconnect-owner-retired", "The disconnect request has no active exact native connection cycle"),
        }
    }));
    if result.is_err() {
        refuse_connect_before_dispatch(&job.identity, job.reply, "disconnect-delegate-failed", "The CoreBluetooth delegate queue rejected the disconnect request");
    }
}

pub(super) unsafe fn schedule_delegate_disconnect(
    queue: super::ffi::dispatch_queue_t,
    registry: std::sync::Arc<super::internal::PeerSourceRegistry>,
    identity: super::internal::NativeRequestIdentity,
    reply: super::internal::CoreBluetoothReplyStateShared,
    adapter_fence: std::sync::Arc<AdapterDispatchFence>,
) -> Result<(), ()> {
    if registry.queue_closed.load(std::sync::atomic::Ordering::Acquire) { return Err(()); }
    let adapter_generation = adapter_fence.snapshot();
    let job = Box::new(DelegateDisconnectJob { identity, reply, adapter_fence, adapter_generation });
    fn require_send<T: Send>(_: &T) {}
    require_send(&job);
    unsafe { super::ffi::dispatch_async_f(queue, Box::into_raw(job).cast(), run_delegate_disconnect) };
    Ok(())
}

unsafe extern "C" fn run_delegate_replay(context: *mut std::ffi::c_void) {
    let job = unsafe { Box::from_raw(context.cast::<DelegateReplayJob>()) };
    let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
    let local = unsafe { super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if local.is_null() {
            Err(crate::ubm_reclaim_authority::AuthorityError::Unknown)
        } else {
            // The queue-specific context is installed and accessed only on CBqueue.
            let delegate = unsafe { &(*local).delegate };
            let result = job.owner.release_reclaim(&job.key, |route, data| delegate.handoff_retained(job.peer, &job.owner,
                CentralDelegateEvent::CharacteristicNotified { peripheral_uuid: job.peer, service_uuid: route.0, characteristic_uuid: route.1, data }, Some((job.peer, route))),
                |id| delegate.replay_root_discovery(id), |event| delegate.apply_peer_lifecycle(job.peer, event, true, Some(&job.owner)), |event| delegate.apply_peer_callback(job.peer, event, true, Some(&job.owner)), |event| delegate.handoff_retained(job.peer, &job.owner, event.into_event(job.peer), None), |id| delegate.adapter_barrier_arrived(job.peer, id), |error| delegate.send_event(CentralDelegateEvent::AdapterBarrierFailed { error }));
            if delegate.ivars().failed.load(std::sync::atomic::Ordering::Acquire) {
                job.owner.waiters.lock().unwrap().reclaim_uncertain = true;
                Err(crate::ubm_reclaim_authority::AuthorityError::Conflict)
            } else { result }
        }
    })).unwrap_or_else(|_| {
        let mut state = job.owner.waiters.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reclaim_uncertain = true;
        Err(crate::ubm_reclaim_authority::AuthorityError::Conflict)
    });
    job.registry.replay_jobs.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    let _ = job.reply.send(result);
}

/// Called only by the actor, which serializes submission against queue cleanup.
pub(super) unsafe fn schedule_delegate_replay(
    queue: super::ffi::dispatch_queue_t,
    registry: std::sync::Arc<super::internal::PeerSourceRegistry>,
    owner: std::sync::Arc<super::internal::PeerSourceOwner>,
    peer: Uuid,
    key: crate::ubm_reclaim_authority::GraphKey,
) -> Result<tokio::sync::oneshot::Receiver<Result<crate::ubm_reclaim_authority::PermitStatus, crate::ubm_reclaim_authority::AuthorityError>>, crate::ubm_reclaim_authority::AuthorityError> {
    use std::sync::atomic::Ordering;
    use crate::ubm_reclaim_authority::AuthorityError;
    if registry.queue_closed.load(Ordering::Acquire) { return Err(AuthorityError::Closed); }
    registry.replay_jobs.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| (count < 256).then_some(count + 1)).map_err(|_| AuthorityError::Capacity)?;
    let (reply, receiver) = tokio::sync::oneshot::channel();
    let job = Box::new(DelegateReplayJob { owner, registry, peer, key, reply });
    // Only Send-safe receipt/identity data crosses into this queue job.
    fn require_send<T: Send>(_: &T) {}
    require_send(&job);
    unsafe { super::ffi::dispatch_async_f(queue, Box::into_raw(job).cast(), run_delegate_replay) };
    Ok(receiver)
}

struct AdapterDrainJob {
    registry: std::sync::Arc<super::internal::PeerSourceRegistry>,
    peers: Vec<(Uuid, std::sync::Arc<super::internal::PeerSourceOwner>)>,
}

unsafe extern "C" fn run_adapter_drain(context: *mut std::ffi::c_void) {
    let job = unsafe { Box::from_raw(context.cast::<AdapterDrainJob>()) };
    let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
    let local = unsafe { super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
    if !local.is_null() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let delegate = unsafe { &(*local).delegate };
            for (peer, owner) in &job.peers { delegate.drain_adapter_peer(*peer, owner); }
        }));
        if result.is_err() {
            let error = crate::PlatformError::new("corebluetooth", "adapter-drain-failed", "Adapter callback replay failed");
            job.registry.fail_all(error, None);
        }
    } else {
        job.registry.fail_all(crate::PlatformError::new("corebluetooth", "adapter-drain-failed", "Delegate queue context is unavailable"), None);
    }
    job.registry.replay_jobs.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
}

/// Queue-confined slots for retained native callback snapshots. IDs may cross
/// threads; the retained objects never leave the CoreBluetooth delegate queue.
#[derive(Debug)]
struct CallbackSlots<T> {
    next: Option<u64>,
    values: HashMap<u64, T>,
    capacity: usize,
}

impl<T> CallbackSlots<T> {
    fn new(capacity: usize) -> Self { Self { next: Some(1), values: HashMap::new(), capacity } }
    fn insert(&mut self, value: T) -> Result<u64, T> {
        if self.values.len() >= self.capacity { return Err(value); }
        let Some(id) = self.next else { return Err(value); };
        self.next = id.checked_add(1);
        self.values.insert(id, value);
        Ok(id)
    }
    fn take(&mut self, id: u64) -> Option<T> { self.values.remove(&id) }
}

#[derive(Debug)]
struct DiscoverySlot {
    peer: Uuid,
    owner: std::sync::Arc<super::internal::PeerSourceOwner>,
    snapshot: DiscoverySnapshot,
}

#[derive(Debug)]
enum DiscoverySnapshot {
    #[cfg(test)] Synthetic { peer: Uuid },
    Root { peripheral: Retained<CBPeripheral>, services: Retained<NSArray<CBService>>, error: Option<crate::PlatformError> },
    Included { peripheral: Retained<CBPeripheral>, service: Retained<CBService>, includes: Retained<NSArray<CBService>>, error: Option<crate::PlatformError> },
    Characteristics { peripheral: Retained<CBPeripheral>, service: Retained<CBService>, values: Retained<NSArray<CBCharacteristic>>, error: Option<crate::PlatformError> },
    Descriptors { peripheral: Retained<CBPeripheral>, characteristic: Retained<CBCharacteristic>, values: Retained<NSArray<CBDescriptor>>, error: Option<crate::PlatformError> },
}

struct AdapterTransition {
    id: u64,
    state: CBManagerState,
    reset: Option<CBManagerState>,
    latest: CBManagerState,
    omitted: usize,
    overflow: bool,
    waiting: HashSet<Uuid>,
    peers: Vec<(Uuid, std::sync::Arc<super::internal::PeerSourceOwner>, bool, (u64, Option<u64>, bool), RefCell<Option<super::internal::AdapterInvalidation>>)>,
    _ticket: super::internal::AdapterEventTicket,
}

struct AdapterTransitions { next: Option<u64>, queue: VecDeque<AdapterTransition>, cut: Option<u64> }
impl Default for AdapterTransitions {
    fn default() -> Self { Self { next: Some(1), queue: VecDeque::new(), cut: None } }
}

impl std::fmt::Debug for AdapterTransitions {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result { f.debug_struct("AdapterTransitions").finish_non_exhaustive() }
}

fn overflow_adapter_states(first: CBManagerState, reset: Option<CBManagerState>, latest: CBManagerState) -> Vec<CBManagerState> {
    let mut states = vec![first];
    if let Some(reset) = reset.filter(|state| *state != first) { states.push(reset); }
    if states.last().is_some_and(|state| *state != latest) { states.push(latest); }
    states
}

fn invalidating_adapter_state(state: CBManagerState) -> bool {
    matches!(state, CBManagerState::PoweredOff | CBManagerState::Resetting | CBManagerState::Unsupported | CBManagerState::Unauthorized)
}

#[derive(Debug)]
pub struct CentralDelegateIvars {
    sender: std::sync::Mutex<Sender<CentralDelegateEvent>>,
    delegate_queue: Option<super::ffi::dispatch_queue_t>,
    queue_context_installed: std::cell::Cell<bool>,
    failed: std::sync::atomic::AtomicBool,
    failure_signal:
        std::sync::Mutex<Option<futures::channel::oneshot::Sender<crate::PlatformError>>>,
    source_registry: std::sync::Arc<super::internal::PeerSourceRegistry>,
    adapter_dispatch_fence: std::sync::Arc<AdapterDispatchFence>,
    /// CoreBluetooth exposes one lifetime delegate rather than a distinct
    /// callback object for each scan. This value therefore lives on that
    /// delegate's serial queue: a discovery callback snapshots it at ingress
    /// and carries the immutable epoch through the actor pipeline.
    scan_ingress_epoch: std::cell::Cell<Option<u64>>,
    /// The epoch removed from native callback ownership but whose actor
    /// handoff marker has not yet been enqueued.
    retiring_scan_ingress_epoch: std::cell::Cell<Option<u64>>,
    /// A retiring epoch whose marker has been emitted but has not yet been
    /// acknowledged after traversing the adapter event stream. A retry uses
    /// this same marker identity; it must not repeat the physical stop.
    fenced_scan_ingress: std::cell::Cell<Option<(u64, u64)>>,
    services: RefCell<HashMap<Uuid, Vec<Retained<CBService>>>>,
    root_snapshots: RefCell<CallbackSlots<DiscoverySlot>>,
    adapter_transitions: RefCell<AdapterTransitions>,
    discoveries:
        std::sync::Mutex<HashMap<Uuid, crate::discovery_reservations::DiscoveryReservations>>,
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
        type Ivars = CentralDelegateIvars;
    }

    unsafe impl NSObjectProtocol for CentralDelegate {}

    unsafe impl CBCentralManagerDelegate for CentralDelegate {
        #[method(centralManagerDidUpdateState:)]
        fn delegate_centralmanagerdidupdatestate(&self, central: &CBCentralManager) {
            trace!("delegate_centralmanagerdidupdatestate");
            self.ensure_queue_context();
            self.register_native_manager(central);
            let state = unsafe { central.state() };
            self.enqueue_adapter_state(state);
        }

        // #[method(centralManager:willRestoreState:)]
        // fn delegate_centralmanager_willrestorestate(&self, _central: &CBCentralManager, _dict: &NSDictionary<NSString, AnyObject>) {
        //     trace!("delegate_centralmanager_willrestorestate");
        // }

        #[method(centralManager:didConnectPeripheral:)]
        fn delegate_centralmanager_didconnectperipheral(
            &self,
            central: &CBCentralManager,
            peripheral: &CBPeripheral,
        ) {
            trace!(
                "delegate_centralmanager_didconnectperipheral {}",
                peripheral_debug(peripheral)
            );
            unsafe { peripheral.setDelegate(Some(ProtocolObject::from_ref(self))) };
            self.ensure_queue_context();
            self.register_native_manager(central);
            let id = unsafe { peripheral.identifier() };
            let peripheral_uuid = nsuuid_to_uuid(&id);
            self.apply_native_connect_result(central, peripheral, peripheral_uuid, None);
        }

        #[method(centralManager:didDisconnectPeripheral:error:)]
        fn delegate_centralmanager_diddisconnectperipheral_error(
            &self,
            central: &CBCentralManager,
            peripheral: &CBPeripheral,
            error: Option<&NSError>,
        ) {
            trace!(
                "delegate_centralmanager_diddisconnectperipheral_error {} (error={:?})",
                peripheral_debug(peripheral),
                error
            );
            self.ensure_queue_context();
            self.register_native_manager(central);
            let id = unsafe { peripheral.identifier() };
            let peripheral_uuid = nsuuid_to_uuid(&id);
            let error = error.map(nserror_platform);
            if let Some(error) = &error {
                log::warn!("CoreBluetooth disconnected {peripheral_uuid} with native error: {error:?}");
            }
            self.apply_native_disconnect_result(central, peripheral, peripheral_uuid, error);
        }

        #[method(centralManager:didFailToConnectPeripheral:error:)]
        fn delegate_centralmanager_didfailtoconnectperipheral_error(
            &self,
            central: &CBCentralManager,
            peripheral: &CBPeripheral,
            error: Option<&NSError>,
        ) {
            trace!("delegate_centralmanager_didfailtoconnectperipheral_error");
            let id = unsafe { peripheral.identifier() };
            let peripheral_uuid = nsuuid_to_uuid(&id);
            let error = error.map(nserror_platform);
            self.ensure_queue_context();
            self.register_native_manager(central);
            self.apply_native_connect_result(central, peripheral, peripheral_uuid, error);
        }

        #[method(centralManager:didDiscoverPeripheral:advertisementData:RSSI:)]
        fn delegate_centralmanager_diddiscoverperipheral_advertisementdata_rssi(
            &self,
            central: &CBCentralManager,
            peripheral: &CBPeripheral,
            adv_data: &NSDictionary<NSString, AnyObject>,
            rssi: &NSNumber,
        ) {
            trace!(
                "delegate_centralmanager_diddiscoverperipheral_advertisementdata_rssi {}",
                peripheral_debug(peripheral)
            );

            self.ensure_queue_context();
            self.register_native_manager(central);
            // CBCentralManager provides a single lifetime delegate and no
            // scan-generation argument. This queue-local snapshot is the
            // only honest ownership boundary: the actor must never infer an
            // epoch from mutable state after this callback has been queued.
            let ingress_epoch = self.ivars().scan_ingress_epoch.get();

            let advertisement_name = adv_data
                .get(unsafe { CBAdvertisementDataLocalNameKey })
                .map(|name| name as *const AnyObject as *const NSString)
                .and_then(|name| unsafe { nsstring_to_string(name) });
            // UBM patch (UBM_PATCHES.md #17): this advertisement's own data,
            // named as the legacy addon named it (advertised name, else the
            // peripheral's GAP name). A nameless packet therefore wears the
            // GAP name here; finding 205 overwrites it with the merged
            // advertised name when one is known (internal.rs), so the GAP
            // alias never stands in for the advertised identity.
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

            let id = unsafe { peripheral.identifier() };
            let peripheral_uuid = nsuuid_to_uuid(&id);
            let owner = self.ivars().source_registry.get_or_create(peripheral_uuid);
            self.remember_native_discovery(central, peripheral_uuid, peripheral, owner);
            self.send_event(CentralDelegateEvent::DiscoveredPeripheral {
                cbperipheral: peripheral.retain(),
                advertisement_name: advertisement_name.clone(),
                // The attempt lane does not yet own or retire registry leases.
                // Do not retain unconsumed diagnostic identities indefinitely.
                native_token: None,
            });

            let rssi_value = rssi.as_i16();

            // UBM patch (UBM_PATCHES.md #2): sent before the events that
            // raise `DeviceUpdated`, on the same ordered channel, so a
            // listener reading properties on that event sees this
            // advertisement's extras.
            self.send_event(CentralDelegateEvent::AdvertisementExtras {
                peripheral_uuid,
                extras: advertisement_extras(adv_data),
            });

            if let Some((manufacturer_id, manufacturer_data)) = advertisement_manufacturer_data(adv_data) {
                    report
                        .manufacturer_data
                        .insert(manufacturer_id, manufacturer_data.clone());
                    self.send_event(CentralDelegateEvent::ManufacturerData {
                        peripheral_uuid,
                        manufacturer_id,
                        data: manufacturer_data,
                        rssi: rssi_value,
                    });
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
                advertisement_name,
                ingress_epoch,
            });
        }
    }

    unsafe impl CBPeripheralDelegate for CentralDelegate {
        #[method(peripheral:didDiscoverServices:)]
        fn delegate_peripheral_diddiscoverservices(&self, peripheral: &CBPeripheral, error: Option<&NSError>) {
            let peer = nsuuid_to_uuid(&*unsafe { peripheral.identifier() });
            let (services, error) = bounded_discovery_array(unsafe { peripheral.services() }.unwrap_or_default(), error.map(nserror_platform));
            let bytes = services.len().saturating_mul(64).saturating_add(error.as_ref().map(super::internal::retained_error_bytes).unwrap_or(0));
            self.admit_discovery_snapshot(peer, DiscoverySnapshot::Root { peripheral: peripheral.retain(), services, error }, bytes);
        }

        #[method(peripheral:didDiscoverIncludedServicesForService:error:)]
        fn delegate_peripheral_diddiscoverincludedservicesforservice_error(&self, peripheral: &CBPeripheral, service: &CBService, error: Option<&NSError>) {
            let peer = nsuuid_to_uuid(&*unsafe { peripheral.identifier() });
            let (values, error) = bounded_discovery_array(unsafe { service.includedServices() }.unwrap_or_default(), error.map(nserror_platform));
            let bytes = values.len().saturating_mul(64).saturating_add(error.as_ref().map(super::internal::retained_error_bytes).unwrap_or(0));
            self.admit_discovery_snapshot(peer, DiscoverySnapshot::Included { peripheral: peripheral.retain(), service: service.retain(), includes: values, error }, bytes);
        }

        #[method(peripheral:didDiscoverCharacteristicsForService:error:)]
        fn delegate_peripheral_diddiscovercharacteristicsforservice_error(&self, peripheral: &CBPeripheral, service: &CBService, error: Option<&NSError>) {
            let peer = nsuuid_to_uuid(&*unsafe { peripheral.identifier() });
            let (values, error) = bounded_discovery_array(unsafe { service.characteristics() }.unwrap_or_default(), error.map(nserror_platform));
            let bytes = values.len().saturating_mul(64).saturating_add(error.as_ref().map(super::internal::retained_error_bytes).unwrap_or(0));
            self.admit_discovery_snapshot(peer, DiscoverySnapshot::Characteristics { peripheral: peripheral.retain(), service: service.retain(), values: values, error }, bytes);
        }

        #[method(peripheral:didDiscoverDescriptorsForCharacteristic:error:)]
        fn delegate_peripheral_diddiscoverdescriptorsforcharacteristic_error(&self, peripheral: &CBPeripheral, characteristic: &CBCharacteristic, error: Option<&NSError>) {
            let peer = nsuuid_to_uuid(&*unsafe { peripheral.identifier() });
            let (values, error) = bounded_discovery_array(unsafe { characteristic.descriptors() }.unwrap_or_default(), error.map(nserror_platform));
            let bytes = values.len().saturating_mul(64).saturating_add(error.as_ref().map(super::internal::retained_error_bytes).unwrap_or(0));
            self.admit_discovery_snapshot(peer, DiscoverySnapshot::Descriptors { peripheral: peripheral.retain(), characteristic: characteristic.retain(), values: values, error }, bytes);
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
                    characteristic_key(self, peripheral, characteristic)
                {
                    self.apply_peer_callback(peripheral_uuid, PeerDelegateCallback::ValueError {
                        route: (service_uuid, characteristic_uuid), notifying: unsafe { characteristic.isNotifying() }, error: nserror_platform(error),
                    }, false, None);
                }
                return;
            }
            if error.is_none() {
                let id = unsafe { peripheral.identifier() };
                let peripheral_uuid = nsuuid_to_uuid(&id);
                let Some((service_uuid, characteristic_uuid)) =
                    characteristic_key(self, peripheral, characteristic)
                else {
                    trace!("event for an untracked characteristic ignored");
                    return;
                };
                let data = get_characteristic_value(characteristic);
                let Some(owner) = self.peer_source_owner(peripheral_uuid) else {
                    return;
                };
                let Some(_callback) = owner.begin_callback() else { self.fail_ticket_capacity(); return; };
                if owner.value((service_uuid, characteristic_uuid), unsafe { characteristic.isNotifying() }, &data) {
                    self.send_peer_event(peripheral_uuid, CentralDelegateEvent::CharacteristicNotified {
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                        data,
                    });
                }
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
                    characteristic_key(self, peripheral, characteristic)
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
                    characteristic_key(self, peripheral, characteristic)
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
            let id = unsafe { peripheral.identifier() };
            let peer = nsuuid_to_uuid(&id);
            let Some(route) = characteristic_key(self, peripheral, characteristic) else { return; };
            let callback = match error {
                Some(error) => PeerDelegateCallback::NotificationError { route, error: nserror_platform(error) },
                None => PeerDelegateCallback::NotificationState { route, notifying: unsafe { characteristic.isNotifying() } },
            };
            self.apply_peer_callback(peer, callback, false, None);
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
                    descriptor_key(self, peripheral, descriptor)
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
                    descriptor_key(self, peripheral, descriptor)
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
                    descriptor_key(self, peripheral, descriptor)
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
                    descriptor_key(self, peripheral, descriptor)
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
            self.apply_peer_lifecycle(peripheral_uuid, PeerLifecycle::ServicesModified, false, None);
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
    pub(super) fn new(
        sender: Sender<CentralDelegateEvent>,
        source_registry: std::sync::Arc<super::internal::PeerSourceRegistry>,
        failure_signal: Option<futures::channel::oneshot::Sender<crate::PlatformError>>,
    ) -> Retained<Self> {
        Self::new_with_queue(sender, source_registry, failure_signal, None)
    }

    pub(super) fn new_with_queue(
        sender: Sender<CentralDelegateEvent>,
        source_registry: std::sync::Arc<super::internal::PeerSourceRegistry>,
        failure_signal: Option<futures::channel::oneshot::Sender<crate::PlatformError>>,
        delegate_queue: Option<super::ffi::dispatch_queue_t>,
    ) -> Retained<Self> {
        let this = CentralDelegate::alloc().set_ivars(CentralDelegateIvars {
            sender: std::sync::Mutex::new(sender),
            delegate_queue,
            queue_context_installed: std::cell::Cell::new(false),
            failed: std::sync::atomic::AtomicBool::new(false),
            failure_signal: std::sync::Mutex::new(failure_signal),
            source_registry,
            adapter_dispatch_fence: std::sync::Arc::new(AdapterDispatchFence::new()),
            scan_ingress_epoch: std::cell::Cell::new(None),
            retiring_scan_ingress_epoch: std::cell::Cell::new(None),
            fenced_scan_ingress: std::cell::Cell::new(None),
            services: RefCell::new(HashMap::new()),
            root_snapshots: RefCell::new(CallbackSlots::new(256)),
            adapter_transitions: RefCell::new(AdapterTransitions::default()),
            discoveries: std::sync::Mutex::new(HashMap::new()),
        });
        unsafe { msg_send_id![super(this), init] }
    }

    pub(super) fn adapter_dispatch_fence(&self) -> std::sync::Arc<AdapterDispatchFence> {
        self.ivars().adapter_dispatch_fence.clone()
    }

    // These methods run only through `run_delegate_scan_ingress`, on the
    // same serial queue CoreBluetooth uses for delegate callbacks.
    fn arm_scan_ingress(&self, epoch: u64) -> Result<(), crate::PlatformError> {
        if let Some(active) = self
            .ivars()
            .scan_ingress_epoch
            .get()
            .or(self.ivars().retiring_scan_ingress_epoch.get())
        {
            return Err(scan_ingress_error(
                "scan-ingress-already-armed",
                format!("scan ingress epoch {active} is still active"),
            ));
        }
        // The caller can only issue a replacement arm after its fence future
        // resolved from the downstream event-stream acknowledgement. This is
        // therefore the safe point to forget the completed marker.
        self.ivars().fenced_scan_ingress.set(None);
        self.ivars().scan_ingress_epoch.set(Some(epoch));
        Ok(())
    }

    fn retire_scan_ingress(&self, epoch: u64) -> Result<(), crate::PlatformError> {
        match self.ivars().scan_ingress_epoch.get() {
            Some(active) if active == epoch => {
                self.ivars().scan_ingress_epoch.set(None);
                self.ivars().retiring_scan_ingress_epoch.set(Some(epoch));
                Ok(())
            }
            Some(active) => Err(scan_ingress_error(
                "scan-ingress-epoch-mismatch",
                format!("cannot retire scan ingress epoch {epoch}; epoch {active} is active"),
            )),
            None => Err(scan_ingress_error(
                "scan-ingress-not-armed",
                format!("cannot retire scan ingress epoch {epoch}; no epoch is active"),
            )),
        }
    }

    fn enqueue_scan_ingress_fence(
        &self,
        epoch: u64,
        fence_id: u64,
    ) -> Result<(), crate::PlatformError> {
        // `retire_scan_ingress` ran before this barrier in the same queue.
        // While it is retired every later native callback is unowned, never
        // relabelled as a future scan. Sending the marker on this queue also
        // places it after every accepted callback handoff.
        if self.ivars().scan_ingress_epoch.get().is_some() {
            return Err(scan_ingress_error(
                "scan-ingress-still-armed",
                format!("cannot fence retired scan ingress epoch {epoch}; another epoch is active"),
            ));
        }
        if let Some((fenced_epoch, fenced_id)) = self.ivars().fenced_scan_ingress.get() {
            if fenced_epoch != epoch || fenced_id != fence_id {
                return Err(scan_ingress_error(
                    "scan-ingress-fence-mismatch",
                    format!("cannot retry scan ingress epoch {epoch} with a different fence identity"),
                ));
            }
            // The original marker may still be waiting downstream, but a
            // timed-out caller needs an exact retry. Re-emitting the same
            // opaque marker is safe: the adapter acknowledges it at most once.
            return self
                .ivars()
                .sender
                .lock()
                .unwrap()
                .try_send(CentralDelegateEvent::ScanIngressFence { epoch, fence_id })
                .map_err(|failure| {
                    let detail = if failure.is_full() { "full" } else { "closed" };
                    scan_ingress_error(
                        "scan-ingress-fence-handoff-failed",
                        format!("The delegate event channel was {detail}"),
                    )
                });
        }
        if self.ivars().retiring_scan_ingress_epoch.get() != Some(epoch) {
            return Err(scan_ingress_error(
                "scan-ingress-not-retiring",
                format!("cannot fence scan ingress epoch {epoch}; it was not retired"),
            ));
        }
        let event = CentralDelegateEvent::ScanIngressFence { epoch, fence_id };
        if let Err(failure) = self.ivars().sender.lock().unwrap().try_send(event) {
            let detail = if failure.is_full() { "full" } else { "closed" };
            let error = scan_ingress_error(
                "scan-ingress-fence-handoff-failed",
                format!("The delegate event channel was {detail}"),
            );
            self.fail_control(error.clone(), None);
            return Err(error);
        }
        self.ivars().retiring_scan_ingress_epoch.set(None);
        self.ivars().fenced_scan_ingress.set(Some((epoch, fence_id)));
        Ok(())
    }

    fn ensure_queue_context(&self) {
        let Some(queue) = self.ivars().delegate_queue else { return; };
        if self.ivars().source_registry.queue_closed.load(std::sync::atomic::Ordering::Acquire) { return; }
        if self.ivars().queue_context_installed.replace(true) { return; }
        let context = Box::new(DelegateQueueContext {
            delegate: self.retain(),
            queue,
            native_lane: None,
            #[cfg(test)]
            test_events: None,
            #[cfg(test)]
            adapter_invalidation_observer: None,
        });
        let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
        // This is called by CoreBluetooth on the configured serial queue.
        // Teardown is enqueued on that same queue after all actor jobs.
        unsafe { super::ffi::dispatch_queue_set_specific(queue, key, Box::into_raw(context).cast(), None) };
    }

    // Called only from native callbacks on the configured delegate queue.
    // The queue context owns the retain and destroys it on that same queue.
    fn register_native_manager(&self, manager: &CBCentralManager) {
        let Some(queue) = self.ivars().delegate_queue else { return; };
        let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
        let local = unsafe { super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
        if local.is_null() || unsafe { (*local).queue != queue } {
            self.fail_control(crate::PlatformError::new("corebluetooth", "native-registry-queue", "Native manager registration was not on its delegate queue"), None);
            return;
        }
        let mismatch = {
            let context = unsafe { &mut *local };
            match &context.native_lane {
                None => {
                    context.native_lane = Some(super::native_lane::NativeLane::new(manager.retain()));
                    false
                }
                Some(lane) => !std::ptr::eq(&**lane.manager(), manager),
            }
        };
        if mismatch {
            self.fail_control(crate::PlatformError::new("corebluetooth", "native-registry-manager", "A different native manager used the delegate queue"), None);
        }
    }

    /// Discovery is the sole place where the delegate has both the real
    /// peripheral object and the owner the actor will later use.  Retain that
    /// exact pairing on the serial delegate queue before publishing discovery.
    fn remember_native_discovery(
        &self,
        manager: &CBCentralManager,
        peer: Uuid,
        peripheral: &CBPeripheral,
        owner: std::sync::Arc<super::internal::PeerSourceOwner>,
    ) {
        let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
        let local = unsafe { super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
        let Some(context) = (!local.is_null()).then(|| unsafe { &mut *local }) else {
            self.fail_control(crate::PlatformError::new("corebluetooth", "native-registry-queue", "Native discovery was not on its delegate queue"), None);
            return;
        };
        let Some(lane) = context.native_lane.as_mut() else {
            self.fail_control(crate::PlatformError::new("corebluetooth", "native-registry-manager", "Native discovery arrived before manager registration"), None);
            return;
        };
        if !std::ptr::eq(&**lane.manager(), manager) {
            self.fail_control(crate::PlatformError::new("corebluetooth", "native-registry-manager", "A different native manager discovered the peripheral"), None);
            return;
        }
        if lane.remember(
            peer,
            peripheral.retain(),
            owner,
            |left, right| std::ptr::eq(&**left, &**right),
            std::sync::Arc::ptr_eq,
        ).is_none() {
            // Do not silently publish a peripheral whose native identity was
            // not retained. A later connect would otherwise look ordinary
            // and fail without the exact queue-local cause.
            self.send_event(CentralDelegateEvent::AdapterBarrierFailed {
                error: crate::PlatformError::new(
                    "corebluetooth",
                    "native-discovery-retain-capacity",
                    "The bounded native discovery registry is full; rediscover the peripheral after capacity is released",
                ),
            });
        }
    }

    /// Consume an exact CoreBluetooth connect/failure callback through the
    /// lane.  An unknown object remains an actor-visible observation; it is
    /// never reassigned to a replacement UUID owner.
    fn apply_native_connect_result(
        &self,
        manager: &CBCentralManager,
        peripheral: &CBPeripheral,
        peer: Uuid,
        error: Option<crate::PlatformError>,
    ) {
        let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
        let local = unsafe { super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
        let Some(context) = (!local.is_null()).then(|| unsafe { &mut *local }) else {
            self.handoff_raw(CentralDelegateEvent::UnassociatedConnectionObservation { peripheral_uuid: peer, error }, None);
            return;
        };
        let Some(lane) = context.native_lane.as_mut() else {
            self.handoff_raw(CentralDelegateEvent::UnassociatedConnectionObservation { peripheral_uuid: peer, error }, None);
            return;
        };
        let manager = manager.retain();
        let peripheral = peripheral.retain();
        let actions = match error {
            Some(error) => lane.failed(&manager, &peripheral, peer, |left, right| std::ptr::eq(&**left, &**right), |left, right| std::ptr::eq(&**left, &**right), Some(error)),
            None => lane.connected(&manager, &peripheral, peer, |left, right| std::ptr::eq(&**left, &**right), |left, right| std::ptr::eq(&**left, &**right)),
        };
        for action in actions {
            match action {
                super::native_attempt::Action::ConnectResult { id, reply, result, .. } => {
                    settle_connect_reply(reply, result);
                    assert!(lane.complete_connect_result(peer, id), "lane connect result lost its receipt");
                    match lane.complete_terminal(peer, id) {
                        Ok(Some(super::native_attempt::Action::Connect { object, .. })) => {
                            let next = lane.resolve(peer, object).expect("queued lane cycle lost its native peripheral");
                            unsafe { lane.manager().connectPeripheral_options(next, None) };
                        }
                        Ok(None) => {}
                        Err(super::native_lane::TerminalReceiptError::NotTerminal) => {
                            // Successful connections remain active until a
                            // later disconnect.
                        }
                        Err(super::native_lane::TerminalReceiptError::ConnectResultPending) => {
                            unreachable!("the connect result receipt was acknowledged before terminal completion")
                        }
                        Ok(Some(_)) => unreachable!("a completed connect cycle can only start another connect"),
                    }
                }
                super::native_attempt::Action::Unassociated { error } => {
                    self.handoff_raw(CentralDelegateEvent::UnassociatedConnectionObservation { peripheral_uuid: peer, error: error.flatten() }, None);
                }
                _ => unreachable!("connect callback can only resolve a connect result"),
            }
        }
    }

    /// Consume a CoreBluetooth disconnect against the exact manager and
    /// peripheral retained by the queue-local lane.  A UUID is only display
    /// data here: it never selects an owner.  This is intentionally separate
    /// from `apply_native_connect_result`, since disconnect must settle every
    /// request retained by the dispatched cycle and retain its lifecycle on
    /// that cycle's original peer owner before a queued cycle may begin.
    fn apply_native_disconnect_result(
        &self,
        manager: &CBCentralManager,
        peripheral: &CBPeripheral,
        peer: Uuid,
        error: Option<crate::PlatformError>,
    ) {
        let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
        let local = unsafe { super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
        let Some(context) = (!local.is_null()).then(|| unsafe { &mut *local }) else {
            self.handoff_raw(CentralDelegateEvent::UnassociatedConnectionObservation { peripheral_uuid: peer, error }, None);
            return;
        };
        let Some(lane) = context.native_lane.as_mut() else {
            self.handoff_raw(CentralDelegateEvent::UnassociatedConnectionObservation { peripheral_uuid: peer, error }, None);
            return;
        };
        let manager = manager.retain();
        let peripheral = peripheral.retain();
        // The generic executor is intentionally shared with its identity-safe
        // Rust test adapter.  CoreBluetooth objects are retained only here on
        // the delegate queue; tests never construct framework objects.
        super::native_lane::execute_disconnect_callback(
            lane,
            &manager,
            &peripheral,
            peer,
            |left, right| std::ptr::eq(&**left, &**right),
            |left, right| std::ptr::eq(&**left, &**right),
            Some(error),
            |reply, error| settle_connect_reply(reply, Err(error.flatten())),
            |replies, error| settle_disconnect_replies(replies, error.flatten()),
            |owner, error| {
                let lifecycle = error.flatten().map(PeerLifecycle::DisconnectedWithError)
                    .unwrap_or(PeerLifecycle::Disconnected);
                self.apply_peer_lifecycle(peer, lifecycle, false, Some(owner));
                // `apply_peer_lifecycle` either hands off to this owner or
                // captures it behind its graph gate.  A control failure is an
                // explicit refusal; keeping the terminal receipt is safer
                // than allowing a successor to overtake that observation.
                !self.ivars().failed.load(std::sync::atomic::Ordering::Acquire)
            },
            |manager, next| unsafe { manager.connectPeripheral_options(next, None) },
            |error| self.handoff_raw(
                CentralDelegateEvent::UnassociatedConnectionObservation {
                    peripheral_uuid: peer,
                    error: error.flatten(),
                },
                None,
            ),
        );
    }

    pub(super) fn peer_source_owner(
        &self,
        peer: Uuid,
    ) -> Option<std::sync::Arc<super::internal::PeerSourceOwner>> {
        self.ivars().source_registry.get(peer)
    }

    fn enqueue_adapter_state(&self, state: CBManagerState) {
        use crate::ubm_reclaim_graph_gate::BarrierAdmission;
        // Advance the delegate-native admission epoch before settling the
        // old lane or publishing the actor transition. A queued job must
        // observe this fence even while actor delivery is still pending.
        self.ivars().adapter_dispatch_fence.observe(state);
        // The graph barrier may deliberately wait behind accepted callbacks.
        // A native attempt cannot: CoreBluetooth has already declared every
        // pre-reset object unusable, so settle queue-owned callers before the
        // barrier is allowed to defer actor replay.
        if invalidating_adapter_state(state) {
            abort_lane_for_adapter_invalidation(state);
        }
        let registry = &self.ivars().source_registry;
        let mut starts = Vec::new();
        {
            let _admission = registry.adapter_admission.lock().unwrap();
            let mut transitions = self.ivars().adapter_transitions.borrow_mut();
            if let Some(cut) = transitions.cut {
                if let Some(pending) = transitions.queue.iter_mut().find(|item| item.id == cut) {
                    pending.latest = state;
                    pending.omitted = pending.omitted.saturating_add(1);
                    if pending.reset.is_none() && invalidating_adapter_state(state) {
                        for (_, owner, _, _, captured) in &pending.peers {
                            match owner.capture_adapter_invalidation() {
                                Ok(invalidation) => *captured.borrow_mut() = Some(invalidation),
                                Err(reason) => {
                                    drop(transitions); drop(_admission);
                                    self.fail_control(crate::PlatformError::new("corebluetooth", "adapter-invalidation-reservation-failed", "Could not retain the adapter invalidation")
                                        .with("sourceError", format!("{reason:?}")), None);
                                    return;
                                }
                            }
                        }
                        pending.reset = Some(state);
                    }
                }
                return;
            }
            let Some(id) = transitions.next else { drop(transitions); drop(_admission); self.fail_ticket_capacity(); return; };
            transitions.next = id.checked_add(1);
            let Some(ticket) = registry.begin_adapter_event() else { drop(transitions); drop(_admission); self.fail_ticket_capacity(); return; };
            let owners = registry.barrier_owners();
            if owners.len() > 4096 { drop(transitions); drop(_admission); drop(ticket); self.fail_ticket_capacity(); return; }
            if owners.iter().any(|(_, owner)| owner.adapter_barrier_id_seen(id)) {
                drop(transitions); drop(_admission); drop(ticket);
                self.fail_control(crate::PlatformError::new("corebluetooth", "adapter-barrier-duplicate", "Adapter barrier identity was already used"), None);
                return;
            }
            let mut overflow = transitions.queue.len() >= 64;
            let mut duplicate = false;
            let mut peers = Vec::with_capacity(owners.len());
            let mut waiting = HashSet::with_capacity(owners.len());
            for (peer, owner) in owners {
                waiting.insert(peer);
                let snapshot = owner.graph_snapshot();
                let overflow_marker = match owner.admit_adapter_barrier(id) {
                    BarrierAdmission::Drain => { starts.push((peer, owner.clone())); false }
                    BarrierAdmission::Queued => false,
                    BarrierAdmission::Overflow { start_drain, .. } => {
                        overflow = true;
                        if start_drain { starts.push((peer, owner.clone())); }
                        true
                    }
                    BarrierAdmission::Duplicate(_) => { duplicate = true; true }
                };
                let cut = if invalidating_adapter_state(state) {
                    match owner.capture_adapter_invalidation() {
                        Ok(cut) => Some(cut),
                        Err(reason) => {
                            drop(transitions); drop(_admission); drop(ticket); drop(peers);
                            self.fail_control(crate::PlatformError::new("corebluetooth", "adapter-invalidation-reservation-failed", "Could not retain the adapter invalidation")
                                .with("sourceError", format!("{reason:?}")), None);
                            return;
                        }
                    }
                } else { None };
                peers.push((peer, owner, overflow_marker, snapshot, RefCell::new(cut)));
            }
            if duplicate {
                drop(transitions);
                drop(_admission);
                drop(ticket);
                self.fail_control(crate::PlatformError::new("corebluetooth", "adapter-barrier-duplicate", "Adapter barrier identity was already used"), None);
                return;
            }
            if overflow {
                for (_, owner, _, _, _) in &peers { owner.fail_adapter_tail(); }
                transitions.cut = Some(id);
            }
            transitions.queue.push_back(AdapterTransition { id, state, reset: invalidating_adapter_state(state).then_some(state), latest: state, omitted: 0, overflow, waiting, peers, _ticket: ticket });
        }
        self.complete_ready_adapter_transitions();
        for (peer, owner) in starts { self.drain_adapter_peer(peer, &owner); }
    }

    fn adapter_barrier_arrived(&self, peer: Uuid, id: Option<u64>) {
        {
            let mut transitions = self.ivars().adapter_transitions.borrow_mut();
            let selected = id.or(transitions.cut);
            let Some(selected) = selected else { drop(transitions); self.fail_ticket_capacity(); return; };
            let Some(transition) = transitions.queue.iter_mut().find(|item| item.id == selected) else { drop(transitions); self.fail_ticket_capacity(); return; };
            if !transition.waiting.remove(&peer) { drop(transitions); self.fail_ticket_capacity(); return; }
        }
        self.complete_ready_adapter_transitions();
    }

    fn complete_ready_adapter_transitions(&self) {
        loop {
            let transition = {
                let mut transitions = self.ivars().adapter_transitions.borrow_mut();
                if !transitions.queue.front().is_some_and(|item| item.waiting.is_empty()) { return; }
                let transition = transitions.queue.pop_front().unwrap();
                if transitions.cut == Some(transition.id) { transitions.cut = None; }
                transition
            };
            let replay = if transition.overflow {
                overflow_adapter_states(transition.state, transition.reset, transition.latest)
            } else { vec![transition.state] };
            if transition.overflow {
                let omitted = transition.omitted.saturating_sub(replay.len().saturating_sub(1));
                let error = crate::PlatformError::new("corebluetooth", "adapter-barrier-overflow", "Adapter callback history is uncertain").with("omittedTransitions", omitted.to_string());
                self.ivars().source_registry.fail_all(error.clone(), None);
                self.send_event(CentralDelegateEvent::AdapterBarrierFailed { error });
                for (_, owner, overflow_marker, _, _) in &transition.peers {
                    if !overflow_marker { owner.mark_adapter_tail_reported(); }
                }
            }
            for state in replay { self.apply_adapter_transition(state, &transition.peers); }
            let mut peers = Vec::with_capacity(transition.peers.len());
            for (peer, owner, overflow_marker, _, _) in transition.peers {
                let resumed = if overflow_marker { owner.resume_adapter_overflow() } else { owner.resume_adapter_barrier(transition.id) };
                if resumed.is_err() { self.fail_ticket_capacity(); return; }
                peers.push((peer, owner));
            }
            if !peers.is_empty() { self.schedule_adapter_drains(peers); }
        }
    }

    fn apply_adapter_transition(&self, state: CBManagerState, peers: &[(Uuid, std::sync::Arc<super::internal::PeerSourceOwner>, bool, (u64, Option<u64>, bool), RefCell<Option<super::internal::AdapterInvalidation>>)]) {
        if invalidating_adapter_state(state) {
            for (_, owner, _, snapshot, captured) in peers {
                let cut = captured.borrow_mut().take();
                if let Some(cut) = cut {
                    cut.complete();
                } else {
                    owner.apply_invalidation_matching(crate::PlatformError::new("corebluetooth", "adapter-unavailable", "CoreBluetooth adapter became unavailable"), true, Some(*snapshot));
                }
            }
            self.ivars().discoveries.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
            self.ivars().services.borrow_mut().clear();
        }
        self.send_event(CentralDelegateEvent::DidUpdateState { state });
    }

    fn drain_adapter_peer(&self, peer: Uuid, owner: &std::sync::Arc<super::internal::PeerSourceOwner>) {
        match owner.drain_reclaim_callbacks(
            |route, data| self.handoff_retained(peer, owner, CentralDelegateEvent::CharacteristicNotified { peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, data }, Some((peer, route))),
            |id| self.replay_root_discovery(id), |event| self.apply_peer_lifecycle(peer, event, true, Some(owner)),
            |event| self.apply_peer_callback(peer, event, true, Some(owner)), |event| self.handoff_retained(peer, owner, event.into_event(peer), None), |id| self.adapter_barrier_arrived(peer, id), |error| self.send_event(CentralDelegateEvent::AdapterBarrierFailed { error }),
        ) {
            Ok(()) => owner.finish_adapter_drain(),
            Err(crate::ubm_reclaim_graph_gate::GateError::Paused) => {},
            Err(_) => self.fail_ticket_capacity(),
        }
    }

    fn schedule_adapter_drains(&self, peers: Vec<(Uuid, std::sync::Arc<super::internal::PeerSourceOwner>)>) {
        use std::sync::atomic::Ordering;
        let registry = self.ivars().source_registry.clone();
        let Some(queue) = self.ivars().delegate_queue else { self.fail_ticket_capacity(); return; };
        if registry.queue_closed.load(Ordering::Acquire) || registry.replay_jobs.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| (count < 256).then_some(count + 1)).is_err() {
            self.fail_ticket_capacity(); return;
        }
        let job = Box::new(AdapterDrainJob { registry, peers });
        fn require_send<T: Send>(_: &T) {}
        require_send(&job);
        unsafe { super::ffi::dispatch_async_f(queue, Box::into_raw(job).cast(), run_adapter_drain) };
    }

    fn send_owned_event(&self, peer: Uuid, owner: Option<&std::sync::Arc<super::internal::PeerSourceOwner>>, event: CentralDelegateEvent) {
        if let Some(owner) = owner {
            let route = match &event {
                CentralDelegateEvent::CharacteristicNotified { service_uuid, characteristic_uuid, .. }
                | CentralDelegateEvent::CharacteristicUnsubscribed { service_uuid, characteristic_uuid, .. }
                | CentralDelegateEvent::AttributeFailed { service_uuid, characteristic_uuid, .. } => Some((peer, (*service_uuid, *characteristic_uuid))),
                _ => None,
            };
            self.handoff_retained(peer, owner, event, route);
        } else { self.send_event(event); }
    }

    fn apply_peer_callback(&self, peer: Uuid, callback: PeerDelegateCallback, replay: bool, retained_owner: Option<&std::sync::Arc<super::internal::PeerSourceOwner>>) {
        let Some(owner) = retained_owner.cloned().or_else(|| self.peer_source_owner(peer)) else {
            match callback {
                PeerDelegateCallback::ValueError { route, error, .. } | PeerDelegateCallback::CapturedValueError { route, error, read_failed: false } => self.send_peer_event(peer, CentralDelegateEvent::AttributeFailed {
                    peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, descriptor_uuid: None, stage: AttributeStage::Value, error,
                }),
                PeerDelegateCallback::NotificationState { route, notifying: true } => self.send_event(CentralDelegateEvent::CharacteristicSubscribed {
                    peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1,
                }),
                _ => {},
            }
            return;
        };
        // A reclaim replay is bound to `retained_owner`, even when the UUID
        // has acquired a replacement owner. Re-selecting it by UUID would
        // drop the old graph's deferred callback or mutate the replacement.
        if !replay {
            let Some(_ticket) = owner.begin_callback() else { self.fail_ticket_capacity(); return; };
            match owner.prepare_delegate_callback(callback.clone()) {
                Ok(None) => return,
                Ok(Some(effect)) => {
                    if let Some(captured) = owner.execute_delegate_capture(effect) {
                        self.apply_peer_callback(peer, captured, true, Some(&owner));
                    }
                    return;
                }
                Err(reason) => {
                    self.fail_control(crate::PlatformError::new("corebluetooth", "callback-reservation-failed", "Could not reserve the delegate callback")
                        .with("sourceError", format!("{reason:?}")), None);
                    return;
                }
            }
        }
        match callback {
            PeerDelegateCallback::CapturedValueError { route, error, read_failed } => {
                if !read_failed {
                    self.send_owned_event(peer, Some(&owner), CentralDelegateEvent::AttributeFailed { peripheral_uuid: peer,
                        service_uuid: route.0, characteristic_uuid: route.1, descriptor_uuid: None,
                        stage: AttributeStage::Value, error });
                }
            }
            PeerDelegateCallback::ValueError { route, notifying, error } => {
                let _wake_scope = owner.faults.defer_wakes();
                let mut state = owner.waiters.lock().unwrap();
                let possible = state.notification_possible(route, notifying);
                let read_failed = state.value_error(route, error.clone());
                if let Some(generation) = state.generation.filter(|_| !read_failed || possible) {
                    let value = crate::api::ValueNotification { uuid: route.1.uuid, instance: route.1.instance,
                        service_uuid: route.0.uuid, service_instance: route.0.instance, value: Vec::new(),
                        source_failure: Some(error.clone()), lost_before: 0 };
                    let _ = owner.faults.publish_source_event_with_provenance(generation,
                        crate::api::NotificationEvent::Value(value), crate::ubm_peer_notification_ingress::CallbackProvenance::Unknown);
                }
                let wakers = state.take_wakers();
                drop(state);
                for waker in wakers { waker.wake(); }
                if !read_failed {
                    self.send_owned_event(peer, Some(&owner), CentralDelegateEvent::AttributeFailed { peripheral_uuid: peer,
                        service_uuid: route.0, characteristic_uuid: route.1, descriptor_uuid: None,
                        stage: AttributeStage::Value, error });
                }
            }
            PeerDelegateCallback::NotificationError { route, error } => {
                owner.with_waiters(|state| state.notification_error(route, error));
            }
            PeerDelegateCallback::CapturedNotificationState { route, notifying, cleanup } => {
                if notifying {
                    self.send_owned_event(peer, Some(&owner), CentralDelegateEvent::CharacteristicSubscribed { peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1 });
                } else if let Some(cleanup) = cleanup {
                    self.send_owned_event(peer, Some(&owner), CentralDelegateEvent::CharacteristicUnsubscribed { peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, cleanup });
                }
            }
            PeerDelegateCallback::NotificationState { route, notifying } => {
                let cleanup = owner.with_waiters(|state| state.notification_state(route, notifying));
                if notifying {
                    self.send_owned_event(peer, Some(&owner), CentralDelegateEvent::CharacteristicSubscribed { peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1 });
                } else {
                    match cleanup {
                        Ok(Some(cleanup)) => self.send_owned_event(peer, Some(&owner), CentralDelegateEvent::CharacteristicUnsubscribed { peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, cleanup }),
                        Ok(None) => {},
                        Err(_) => owner.with_waiters(|state| state.invalidate(crate::PlatformError::new("corebluetooth", "notification-cleanup-failed", "Notification cleanup identity exhausted"))),
                    }
                }
            }
        }
    }

    fn apply_peer_lifecycle(&self, peer: Uuid, lifecycle: PeerLifecycle, replay: bool, retained_owner: Option<&std::sync::Arc<super::internal::PeerSourceOwner>>) {
        let lifecycle_owner = retained_owner.cloned().or_else(|| self.peer_source_owner(peer));
        let _callback = if let Some(owner) = lifecycle_owner.as_ref() {
            // A retained replay has already selected its exact old owner at
            // graph-gate admission; UUID-currentness is not an ownership test.
            let callback = if !replay {
                let Some(ticket) = owner.begin_callback() else { self.fail_ticket_capacity(); return; };
                match owner.prepare_lifecycle(lifecycle.clone()) {
                    Ok(None) => return,
                    Ok(Some(super::internal::DeferredPeerCallback::Lifecycle { replies, source, .. })) => {
                        if let Some(source) = source { source.release(); }
                        replies.complete();
                    }
                    Ok(Some(_)) => unreachable!("lifecycle preparation returns a lifecycle effect"),
                    Err(reason) => {
                        self.fail_control(crate::PlatformError::new("corebluetooth", "lifecycle-reservation-failed", "Could not reserve the lifecycle callback")
                            .with("sourceError", format!("{reason:?}")), None);
                        return;
                    }
                }
                Some(ticket)
            } else { None };
            callback
        } else { None };
        match lifecycle {
            PeerLifecycle::Disconnected | PeerLifecycle::DisconnectedWithError(_) => {
                let error = match lifecycle { PeerLifecycle::DisconnectedWithError(error) => Some(error), _ => None };
                // A terminal callback may be retained for an old native owner
                // while a new owner for the same UUID is already current. The
                // lifecycle event still belongs to the old owner, but UUID-keyed
                // delegate caches belong to the current owner and must survive.
                let owns_current_cache = lifecycle_owner.as_ref().is_none_or(|owner| {
                    self.peer_source_owner(peer)
                        .is_some_and(|current| std::sync::Arc::ptr_eq(&current, owner))
                });
                if owns_current_cache {
                    self.ivars().services.borrow_mut().remove(&peer);
                    self.ivars().discoveries.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&peer);
                }
                self.send_owned_event(peer, lifecycle_owner.as_ref(), CentralDelegateEvent::DisconnectedDevice { peripheral_uuid: peer, error });
            }
            PeerLifecycle::ServicesModified => {
                self.fail_discovery_owned(peer, crate::PlatformError::new("corebluetooth", "services-modified", "CoreBluetooth invalidated the service graph during discovery"), lifecycle_owner.as_ref());
                self.ivars().services.borrow_mut().remove(&peer);
                self.send_owned_event(peer, lifecycle_owner.as_ref(), CentralDelegateEvent::ServicesModified { peripheral_uuid: peer });
            }
        }
    }

    fn refuse_discovery_snapshot(&self, peer: Uuid) {
        if let Some(owner) = self.peer_source_owner(peer) {
            if matches!(owner.admit_root_discovery(0, usize::MAX), crate::ubm_reclaim_graph_gate::Admission::Overflow(_)) { return; }
        }
        let error = crate::PlatformError::new("corebluetooth", "discovery-bound", "The discovery snapshot capacity is full");
        self.ivars().source_registry.fail_all(error.clone(), None);
        self.send_event(CentralDelegateEvent::AdapterBarrierFailed { error });
    }

    #[cfg(test)]
    pub(super) fn simulate_discovery_slot_exhaustion(&self, peer: Uuid) {
        let owner = self.peer_source_owner(peer).unwrap();
        {
            let mut slots = self.ivars().root_snapshots.borrow_mut();
            for _ in 0..256 {
                assert!(slots.insert(DiscoverySlot { peer, owner: owner.clone(), snapshot: DiscoverySnapshot::Synthetic { peer } }).is_ok());
            }
        }
        self.admit_discovery_snapshot(peer, DiscoverySnapshot::Synthetic { peer }, 0);
    }

    fn admit_discovery_snapshot(&self, peer: Uuid, snapshot: DiscoverySnapshot, bytes: usize) {
        let Some(owner) = self.peer_source_owner(peer) else {
            self.fail_control(crate::PlatformError::new("corebluetooth", "discovery-owner-missing", "A discovery callback has no native peer owner"), None);
            return;
        };
        let id = match self.ivars().root_snapshots.borrow_mut().insert(DiscoverySlot { peer, owner: owner.clone(), snapshot }) {
            Ok(id) => id,
            Err(_) => { self.refuse_discovery_snapshot(peer); return; }
        };
        let admission = owner.admit_root_discovery(id, bytes);
        match admission {
            crate::ubm_reclaim_graph_gate::Admission::Apply(id) => self.replay_root_discovery(id),
            crate::ubm_reclaim_graph_gate::Admission::Deferred => {},
            crate::ubm_reclaim_graph_gate::Admission::Overflow(id) => { self.ivars().root_snapshots.borrow_mut().take(id); },
        }
    }

    fn replay_root_discovery(&self, id: u64) {
        let Some(slot) = self.ivars().root_snapshots.borrow_mut().take(id) else { return; };
        if !self.peer_source_owner(slot.peer).is_some_and(|current| std::sync::Arc::ptr_eq(&current, &slot.owner)) {
            slot.owner.fail_source(crate::PlatformError::new("corebluetooth", "callback-owner-retired", "A discovery callback belonged to a retired native peer owner"), None);
            return;
        }
        let source = slot.owner;
        match slot.snapshot {
            #[cfg(test)]
            DiscoverySnapshot::Synthetic { peer } => self.send_owned_event(peer, Some(&source), CentralDelegateEvent::DiscoveryDrained { peripheral_uuid: peer }),
            DiscoverySnapshot::Root { peripheral, services, error } => {
                let peer = nsuuid_to_uuid(&*unsafe { peripheral.identifier() });
                let Some(halted) = self.consume_discovery(peer, |scope| scope.take_root()) else { return; };
                if halted { self.finish_discovery_owned(peer, Some(&source)); return; }
                if let Some(error) = error { self.fail_discovery_owned(peer, error, Some(&source)); return; }
                if !self.register_discovery(peer, |scope| scope.register_services(services.iter().map(|service| object_identity(&*service)))) {
                    self.fail_discovery_owned(peer, crate::PlatformError::new("corebluetooth", "discovery-bound", "The service graph exceeds the owned callback bound"), Some(&source)); return;
                }
                self.ivars().services.borrow_mut().insert(peer, services.iter().map(|service| service.retain()).collect());
                let service_map = keyed(services, |service| cbuuid_to_uuid(&*unsafe { service.UUID() }));
                self.send_owned_event(peer, Some(&source), CentralDelegateEvent::DiscoveredServices { peripheral_uuid: peer, services: service_map.clone() });
                for service in service_map.values() { unsafe { peripheral.discoverCharacteristics_forService(None, service); peripheral.discoverIncludedServices_forService(None, service); } }
                self.finish_discovery_owned(peer, Some(&source));
            }
            DiscoverySnapshot::Included { peripheral, service, includes, error } => {
                let peer = nsuuid_to_uuid(&*unsafe { peripheral.identifier() });
                let Some(halted) = self.consume_discovery(peer, |scope| scope.take_includes(object_identity(&*service))) else { return; };
                if halted { self.finish_discovery_owned(peer, Some(&source)); return; }
                if let Some(error) = error { self.fail_discovery_owned(peer, error, Some(&source)); return; }
                let Some(service_uuid) = service_key(self, &peripheral, &service) else { self.fail_discovery_owned(peer, crate::PlatformError::new("corebluetooth", "discovery-identity", "An admitted service callback lost its object identity"), Some(&source)); return; };
                let mut added = HashMap::new(); let mut keys = Vec::new();
                {
                    let mut tables = self.ivars().services.borrow_mut();
                    let Some(table) = tables.get_mut(&peer) else { return; };
                    for target in includes.iter() {
                        let mut inserted = false;
                        let index = match table.iter().position(|candidate| std::ptr::eq(&**candidate, &*target)) {
                            Some(index) => index, None => { let index = table.len(); table.push(target.retain()); inserted = true; index }
                        };
                        let key = AttrKey { uuid: cbuuid_to_uuid(&*unsafe { target.UUID() }), instance: index as u64 };
                        if inserted { added.insert(key, target.retain()); } keys.push(key);
                    }
                }
                if !self.register_discovery(peer, |scope| scope.register_services(added.values().map(|service| object_identity(&**service)))) {
                    self.fail_discovery_owned(peer, crate::PlatformError::new("corebluetooth", "discovery-bound", "The included service graph exceeds the owned callback bound"), Some(&source)); return;
                }
                self.send_owned_event(peer, Some(&source), CentralDelegateEvent::DiscoveredIncludedServices { peripheral_uuid: peer, service_uuid, added: added.clone(), result: Ok(keys) });
                for target in added.values() { unsafe { peripheral.discoverCharacteristics_forService(None, target); peripheral.discoverIncludedServices_forService(None, target); } }
                self.finish_discovery_owned(peer, Some(&source));
            }
            DiscoverySnapshot::Characteristics { peripheral, service, values, error } => {
                let peer = nsuuid_to_uuid(&*unsafe { peripheral.identifier() });
                let Some(halted) = self.consume_discovery(peer, |scope| scope.take_characteristics(object_identity(&*service))) else { return; };
                if halted { self.finish_discovery_owned(peer, Some(&source)); return; }
                if let Some(error) = error { self.fail_discovery_owned(peer, error, Some(&source)); return; }
                let Some(service_uuid) = service_key(self, &peripheral, &service) else { self.fail_discovery_owned(peer, crate::PlatformError::new("corebluetooth", "discovery-identity", "An admitted characteristic callback lost its service identity"), Some(&source)); return; };
                if !self.register_discovery(peer, |scope| scope.register_descriptors(values.iter().map(|characteristic| object_identity(&*characteristic)))) {
                    self.fail_discovery_owned(peer, crate::PlatformError::new("corebluetooth", "discovery-bound", "The descriptor callback graph exceeds its bound"), Some(&source)); return;
                }
                let characteristics = keyed(values, |characteristic| cbuuid_to_uuid(&*unsafe { characteristic.UUID() }));
                self.send_owned_event(peer, Some(&source), CentralDelegateEvent::DiscoveredCharacteristics { peripheral_uuid: peer, service_uuid, characteristics: characteristics.clone() });
                for characteristic in characteristics.values() { unsafe { peripheral.discoverDescriptorsForCharacteristic(characteristic) }; }
                self.finish_discovery_owned(peer, Some(&source));
            }
            DiscoverySnapshot::Descriptors { peripheral, characteristic, values, error } => {
                let peer = nsuuid_to_uuid(&*unsafe { peripheral.identifier() });
                let Some(halted) = self.consume_discovery(peer, |scope| scope.take_descriptor(object_identity(&*characteristic))) else { return; };
                if halted { self.finish_discovery_owned(peer, Some(&source)); return; }
                if let Some(error) = error { self.fail_discovery_owned(peer, error, Some(&source)); return; }
                let Some((service_uuid, characteristic_uuid)) = characteristic_key(self, &peripheral, &characteristic) else { self.fail_discovery_owned(peer, crate::PlatformError::new("corebluetooth", "discovery-identity", "An admitted descriptor callback lost its characteristic identity"), Some(&source)); return; };
                let descriptors = keyed(values, |descriptor| cbuuid_to_uuid(&*unsafe { descriptor.UUID() }));
                self.send_owned_event(peer, Some(&source), CentralDelegateEvent::DiscoveredCharacteristicDescriptors { peripheral_uuid: peer, service_uuid, characteristic_uuid, descriptors });
                self.finish_discovery_owned(peer, Some(&source));
            }
        }
    }

    pub(crate) fn begin_discovery(&self, peer: Uuid) -> Result<(), crate::PlatformError> {
        let mut scopes = self
            .ivars()
            .discoveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if scopes.contains_key(&peer) {
            return Err(crate::PlatformError::new(
                "corebluetooth",
                "discovery-in-progress",
                "The previous discovery still owns native callbacks",
            ));
        }
        if scopes.len() >= 4096 {
            return Err(crate::PlatformError::new(
                "corebluetooth",
                "discovery-bound",
                "The live discovery callback owner capacity is full",
            ));
        }
        scopes.insert(
            peer,
            crate::discovery_reservations::DiscoveryReservations::default(),
        );
        Ok(())
    }

    fn consume_discovery(
        &self,
        peer: Uuid,
        take: impl FnOnce(&mut crate::discovery_reservations::DiscoveryReservations) -> bool,
    ) -> Option<bool> {
        let mut scopes = self
            .ivars()
            .discoveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let scope = scopes.get_mut(&peer)?;
        take(scope).then(|| scope.halted())
    }

    fn register_discovery(
        &self,
        peer: Uuid,
        register: impl FnOnce(&mut crate::discovery_reservations::DiscoveryReservations) -> bool,
    ) -> bool {
        self.ivars()
            .discoveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&peer)
            .is_some_and(register)
    }

    fn fail_discovery_owned(&self, peer: Uuid, error: crate::PlatformError, owner: Option<&std::sync::Arc<super::internal::PeerSourceOwner>>) {
        let failed = {
            let mut scopes = self
                .ivars()
                .discoveries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match scopes.get_mut(&peer) {
                Some(scope) if !scope.halted() => {
                    scope.fail();
                    true
                }
                _ => false,
            }
        };
        if failed {
            self.send_owned_event(peer, owner, CentralDelegateEvent::DiscoveryFailed { peripheral_uuid: peer, error });
        }
        self.finish_discovery_owned(peer, owner);
    }

    fn finish_discovery_owned(&self, peer: Uuid, owner: Option<&std::sync::Arc<super::internal::PeerSourceOwner>>) {
        let ended = {
            let mut scopes = self
                .ivars()
                .discoveries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if scopes.get(&peer).is_some_and(|scope| scope.is_drained()) {
                scopes.remove(&peer)
            } else {
                None
            }
        };
        if let Some(scope) = ended {
            if scope.halted() {
                self.ivars().services.borrow_mut().remove(&peer);
            }
            self.send_owned_event(peer, owner, CentralDelegateEvent::DiscoveryDrained { peripheral_uuid: peer });
        }
    }

    pub(super) fn send_peer_event(&self, peer: Uuid, event: CentralDelegateEvent) {
        let route = match &event {
            CentralDelegateEvent::CharacteristicNotified {
                service_uuid,
                characteristic_uuid,
                ..
            }
            | CentralDelegateEvent::CharacteristicUnsubscribed {
                service_uuid,
                characteristic_uuid,
                ..
            }
            | CentralDelegateEvent::AttributeFailed {
                service_uuid,
                characteristic_uuid,
                ..
            } => Some((peer, (*service_uuid, *characteristic_uuid))),
            _ => None,
        };
        self.handoff(event, Some(peer), route);
    }

    pub(super) fn send_event(&self, event: CentralDelegateEvent) {
        let event = match event {
            CentralDelegateEvent::CharacteristicWritten { peripheral_uuid, service_uuid, characteristic_uuid } => {
                self.submit_peer_actor_callback(peripheral_uuid, PeerActorCallback::CharacteristicWritten { route: (service_uuid, characteristic_uuid) }); return;
            }
            CentralDelegateEvent::DescriptorNotified { peripheral_uuid, service_uuid, characteristic_uuid, descriptor_uuid, data } => {
                self.submit_peer_actor_callback(peripheral_uuid, PeerActorCallback::DescriptorNotified { route: (service_uuid, characteristic_uuid), descriptor: descriptor_uuid, data }); return;
            }
            CentralDelegateEvent::DescriptorWritten { peripheral_uuid, service_uuid, characteristic_uuid, descriptor_uuid } => {
                self.submit_peer_actor_callback(peripheral_uuid, PeerActorCallback::DescriptorWritten { route: (service_uuid, characteristic_uuid), descriptor: descriptor_uuid }); return;
            }
            CentralDelegateEvent::AttributeFailed { peripheral_uuid, service_uuid, characteristic_uuid, descriptor_uuid, stage, error } => {
                self.submit_peer_actor_callback(peripheral_uuid, PeerActorCallback::AttributeFailed { route: (service_uuid, characteristic_uuid), descriptor: descriptor_uuid, stage, error }); return;
            }
            event => event,
        };
        let event = if matches!(event, CentralDelegateEvent::DidUpdateState { .. } | CentralDelegateEvent::AdapterBarrierFailed { .. }) {
            let Some(ticket) = self.ivars().source_registry.begin_adapter_event() else { self.fail_ticket_capacity(); return; };
            CentralDelegateEvent::AdapterTicketed { ticket, event: Box::new(event) }
        } else { event };
        let peer = match &event {
            CentralDelegateEvent::ServicesModified { peripheral_uuid }
            | CentralDelegateEvent::DisconnectedDevice { peripheral_uuid, .. }
            | CentralDelegateEvent::ConnectedDevice { peripheral_uuid }
            | CentralDelegateEvent::ConnectionFailed { peripheral_uuid, .. }
            | CentralDelegateEvent::DiscoveredServices { peripheral_uuid, .. }
            | CentralDelegateEvent::DiscoveredIncludedServices { peripheral_uuid, .. }
            | CentralDelegateEvent::DiscoveredCharacteristics { peripheral_uuid, .. }
            | CentralDelegateEvent::DiscoveredCharacteristicDescriptors { peripheral_uuid, .. }
            | CentralDelegateEvent::DiscoveryFailed { peripheral_uuid, .. }
            | CentralDelegateEvent::DiscoveryDrained { peripheral_uuid }
            | CentralDelegateEvent::CharacteristicSubscribed { peripheral_uuid, .. }
            | CentralDelegateEvent::CharacteristicWritten { peripheral_uuid, .. }
            | CentralDelegateEvent::AttributeFailed { peripheral_uuid, .. } => Some(*peripheral_uuid),
            _ => None,
        };
        self.handoff(event, peer, None);
    }

    fn submit_peer_actor_callback(&self, peer: Uuid, callback: PeerActorCallback) {
        use crate::ubm_reclaim_graph_gate::Admission;
        let Some(owner) = self.peer_source_owner(peer) else { self.handoff(callback.into_event(peer), None, None); return; };
        let Some(_callback) = owner.begin_callback() else { self.fail_ticket_capacity(); return; };
        match owner.admit_actor_callback(callback) {
            Admission::Apply(callback) => self.handoff(callback.into_event(peer), Some(peer), None),
            Admission::Deferred | Admission::Overflow(_) => {},
        }
    }

    fn fail_ticket_capacity(&self) {
        self.fail_control(crate::PlatformError::new("corebluetooth", "callback-ticket-capacity", "CoreBluetooth callback ownership capacity is full"), None);
    }

    pub(super) fn fail_control(&self, error: crate::PlatformError, peer: Option<(Uuid, (AttrKey, AttrKey))>) {
        use std::sync::atomic::Ordering;
        if self.ivars().failed.swap(true, Ordering::AcqRel) { return; }
        self.ivars().source_registry.fail_all(error.clone(), peer);
        if let Some(signal) = self.ivars().failure_signal.lock().unwrap().take() { let _ = signal.send(error); }
    }

    fn handoff(&self, event: CentralDelegateEvent, peer_id: Option<Uuid>, route: Option<(Uuid, (AttrKey, AttrKey))>) {
        use std::sync::atomic::Ordering;
        if self.ivars().failed.load(Ordering::Acquire) {
            return;
        }
        if let Some((peer, owner)) = peer_id.and_then(|peer| self.peer_source_owner(peer).map(|owner| (peer, owner))) {
            self.handoff_retained(peer, &owner, event, route);
            return;
        }
        self.handoff_raw(event, route);
    }

    pub(super) fn handoff_retained(&self, peer: Uuid, owner: &std::sync::Arc<super::internal::PeerSourceOwner>, event: CentralDelegateEvent, route: Option<(Uuid, (AttrKey, AttrKey))>) {
        let Some(ticket) = owner.begin_callback() else { self.fail_ticket_capacity(); return; };
        self.handoff_raw(CentralDelegateEvent::Ticketed { peer, ticket, event: Box::new(event) }, route);
    }

    fn handoff_raw(&self, event: CentralDelegateEvent, peer: Option<(Uuid, (AttrKey, AttrKey))>) {
        let result = self.ivars().sender.lock().unwrap().try_send(event);
        if let Err(failure) = result {
            let detail = if failure.is_full() { "full" } else { "closed" };
            self.fail_control(crate::PlatformError::new("corebluetooth", "source-handoff-failed",
                format!("The delegate event channel was {detail}")), peer);
        }
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

fn characteristic_debug(characteristic: &CBCharacteristic) -> String {
    let uuid = unsafe { characteristic.UUID().UUIDString() };
    format!("CBCharacteristic({})", uuid)
}

fn descriptor_debug(descriptor: &CBDescriptor) -> String {
    let uuid = unsafe { descriptor.UUID().UUIDString() };
    format!("CBDescriptor({})", uuid)
}

#[cfg(test)]
mod delegate_executor_lifetime_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn scan_ingress_epoch_is_snapshotted_then_retired_before_the_actor_fence() {
        let (sender, mut receiver) = futures::channel::mpsc::channel(4);
        let registry = Arc::new(super::super::internal::PeerSourceRegistry::default());
        let delegate = CentralDelegate::new(sender, registry, None);

        delegate.arm_scan_ingress(41).unwrap();
        assert_eq!(delegate.ivars().scan_ingress_epoch.get(), Some(41));
        assert!(matches!(
            delegate.arm_scan_ingress(42),
            Err(error) if error.code == "scan-ingress-already-armed"
        ));

        // A native callback would capture this exact value immediately on
        // CBqueue, before any actor scheduling can observe another scan.
        let captured_at_ingress = delegate.ivars().scan_ingress_epoch.get();
        // Model a discovery callback accepted before retirement. The actor
        // must see this immutable old epoch before the fence marker, even if
        // it does not consume either item until after the stop request.
        delegate.send_event(CentralDelegateEvent::Advertised {
            peripheral_uuid: Uuid::nil(),
            report: crate::api::AdvertisementReport::default(),
            advertisement_name: None,
            ingress_epoch: captured_at_ingress,
        });
        delegate.retire_scan_ingress(41).unwrap();
        assert_eq!(captured_at_ingress, Some(41));
        assert_eq!(delegate.ivars().scan_ingress_epoch.get(), None);

        // A callback CoreBluetooth delivers after retirement but before a
        // replacement arm is deliberately unowned. There is no API for us to
        // decide whether its RF packet belonged to the old physical scan.
        delegate.send_event(CentralDelegateEvent::Advertised {
            peripheral_uuid: Uuid::nil(),
            report: crate::api::AdvertisementReport::default(),
            advertisement_name: None,
            ingress_epoch: delegate.ivars().scan_ingress_epoch.get(),
        });

        delegate
            .enqueue_scan_ingress_fence(41, 7)
            .unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(CentralDelegateEvent::Advertised {
                ingress_epoch: Some(41),
                ..
            })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(CentralDelegateEvent::Advertised {
                ingress_epoch: None,
                ..
            })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(CentralDelegateEvent::ScanIngressFence { epoch: 41, fence_id: 7 })
        ));

        // A lifecycle deadline can cancel its first wait after the marker was
        // enqueued but before the downstream Btleplug stream acknowledges it.
        // Retrying exactly this retired epoch must re-publish the same marker,
        // not require a second retire/physical stop or leave the delegate
        // permanently unable to fence the scan.
        delegate.enqueue_scan_ingress_fence(41, 7).unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(CentralDelegateEvent::ScanIngressFence { epoch: 41, fence_id: 7 })
        ));
        assert!(matches!(
            delegate.enqueue_scan_ingress_fence(41, 8),
            Err(error) if error.code == "scan-ingress-fence-mismatch"
        ));

        // The marker has been placed after the accepted callback prefix, so
        // the next epoch may arm only after the caller receives its reply.
        delegate.arm_scan_ingress(42).unwrap();
        let callback_after_rearm = delegate.ivars().scan_ingress_epoch.get();
        assert_eq!(callback_after_rearm, Some(42));
        // Once re-armed, CoreBluetooth provides no physical scan identity.
        // A callback delivered now is correctly stamped as callback-observed
        // epoch 42, while its RF origin remains intentionally unknowable.
        assert_eq!(delegate.ivars().scan_ingress_epoch.get(), Some(42));
    }

    #[test]
    fn adapter_dispatch_generation_rejects_queued_connect_and_disconnect_across_reset_before_actor_receipt() {
        use std::{rc::Rc, sync::Arc};

        let fence = AdapterDispatchFence::new();
        let before_reset = fence.snapshot();
        assert!(fence.admits(before_reset));

        // A lane is deliberately built from inert Rust identities rather than
        // framework objects.  It represents the queue-owned active cycle that
        // must be settled during Resetting while the actor has not received
        // either adapter callback yet.
        let registry = Arc::new(super::super::internal::PeerSourceRegistry::default());
        let peer = Uuid::from_u128(0xCB_51);
        let owner = registry.get_or_create(peer);
        let mut lane = super::super::native_lane::NativeLane::<
            Rc<u8>,
            Rc<u8>,
            Arc<super::super::internal::PeerSourceOwner>,
            &'static str,
            Option<crate::PlatformError>,
        >::new(Rc::new(1));
        let (cycle, started) = lane
            .connect(peer, Rc::new(2), owner, "queued-connect", Rc::ptr_eq)
            .expect("safe injected native identity must start a lane");
        assert!(matches!(started, Some(super::super::native_attempt::Action::Connect { .. })));
        assert!(matches!(
            lane.disconnect(peer, cycle, "queued-disconnect"),
            Ok(None),
        ));

        // `centralManagerDidUpdateState:` runs before the actor consumes its
        // state event. Both jobs carrying this fence must remain refused even
        // after a later PoweredOn callback is queued before actor receipt.
        fence.observe(CBManagerState::Resetting);
        let invalidation = lane.abort_all(Some(crate::PlatformError::new(
            "corebluetooth",
            "adapter-unavailable",
            "test reset before actor receipt",
        )));
        assert!(matches!(
            invalidation.as_slice(),
            [super::super::native_attempt::Action::DisconnectedWhileConnecting {
                disconnect_replies,
                ..
            }] if disconnect_replies.len() == 1
        ));
        fence.observe(CBManagerState::PoweredOn);
        assert!(!fence.admits(before_reset));

        let after_powered_on = fence.snapshot();
        assert!(fence.admits(after_powered_on));
        assert!(
            !fence.admits(before_reset),
            "a pre-reset connect or disconnect job cannot be revived by the post-reset PoweredOn callback"
        );
        fence.observe(CBManagerState::PoweredOff);
        assert!(!fence.admits(after_powered_on));
    }

    fn release_retained_lifecycle(
        delegate: &CentralDelegate,
        peer: Uuid,
        owner: &std::sync::Arc<super::super::internal::PeerSourceOwner>,
        key: &crate::ubm_reclaim_authority::GraphKey,
    ) {
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(key).unwrap();
        owner.drain_reclaim_callbacks(
            |route, data| delegate.handoff_retained(peer, owner, CentralDelegateEvent::CharacteristicNotified {
                peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, data,
            }, Some((peer, route))),
            |id| delegate.replay_root_discovery(id),
            |event| delegate.apply_peer_lifecycle(peer, event, true, Some(owner)),
            |event| delegate.apply_peer_callback(peer, event, true, Some(owner)),
            |event| delegate.handoff_retained(peer, owner, event.into_event(peer), None),
            |id| delegate.adapter_barrier_arrived(peer, id),
            |error| delegate.send_event(CentralDelegateEvent::AdapterBarrierFailed { error }),
        ).unwrap();
    }

    // The synthetic callback deliberately follows today's callback boundary:
    // CoreBluetooth supplies an object, but the delegate forwards only its UUID.
    // Public connect requests are real; the test does not dispatch a radio or
    // pretend that the current actor already implements a native attempt lane.
    #[tokio::test]
    async fn public_connect_requests_preserve_exact_source_owner() {
        use crate::api::Peripheral as _;
        use futures::StreamExt;
        use std::sync::{Arc, Weak};
        use super::super::internal::{CoreBluetoothMessage, CoreBluetoothReply, PeerSourceRegistry};
        use super::super::peripheral::Peripheral;

        let registry = Arc::new(PeerSourceRegistry::default());
        let peer = Uuid::from_u128(42);
        let old = registry.get_or_create(peer);
        let (messages, mut received) = futures::channel::mpsc::channel(8);
        let (_events, events) = futures::channel::mpsc::channel(8);
        let handle = Peripheral::new_with_source_owner(
            peer, None, None, Weak::new(), events, messages.clone(), old.clone(),
        );
        let old_connect = tokio::spawn(async move { handle.connect().await });
        let CoreBluetoothMessage::ConnectDevice { identity: old_identity, future: old_reply } =
            received.next().await.unwrap() else { panic!("expected connect request") };
        registry.remove_if_current(peer, &old);
        let replacement = registry.get_or_create(peer);
        let (_events, events) = futures::channel::mpsc::channel(8);
        let handle = Peripheral::new_with_source_owner(
            peer, None, None, Weak::new(), events, messages, replacement.clone(),
        );
        let new_connect = tokio::spawn(async move { handle.connect().await });
        let CoreBluetoothMessage::ConnectDevice { identity: new_identity, future: new_reply } =
            received.next().await.unwrap() else { panic!("expected connect request") };

        assert_eq!(old_identity.uuid(), peer);
        assert_eq!(new_identity.uuid(), peer);
        assert!(old_identity.matches_owner(&old));
        assert!(!old_identity.matches_owner(&replacement));
        assert!(new_identity.matches_owner(&replacement));
        assert!(!new_identity.matches_owner(&old));
        old_reply.lock().unwrap().set_reply(CoreBluetoothReply::Err("fixture shutdown".into()));
        new_reply.lock().unwrap().set_reply(CoreBluetoothReply::Err("fixture shutdown".into()));
        let _ = old_connect.await;
        let _ = new_connect.await;
    }

    #[tokio::test]
    async fn public_connect_unassociated_callback_must_not_claim_replacement() {
        use crate::api::Peripheral as _;
        use futures::StreamExt;
        use std::sync::{Arc, Weak};
        use super::super::internal::{CoreBluetoothMessage, CoreBluetoothReply, PeerSourceRegistry};
        use super::super::peripheral::Peripheral;

        let registry = Arc::new(PeerSourceRegistry::default());
        let peer = Uuid::from_u128(42);
        let old = registry.get_or_create(peer);
        let (messages, mut received) = futures::channel::mpsc::channel(8);
        let (_old_events, old_events) = futures::channel::mpsc::channel(8);
        let old_handle = Peripheral::new_with_source_owner(
            peer, None, None, Weak::new(), old_events, messages.clone(), old.clone(),
        );
        let old_connect = tokio::spawn(async move { old_handle.connect().await });
        let old_request = received.next().await.expect("old public connect submitted");
        let CoreBluetoothMessage::ConnectDevice { identity, future: old_reply } = old_request else { panic!("expected connect request") };
        assert_eq!(identity.uuid(), peer);

        registry.remove_if_current(peer, &old);
        let replacement = registry.get_or_create(peer);
        let (_new_events, new_events) = futures::channel::mpsc::channel(8);
        let new_handle = Peripheral::new_with_source_owner(
            peer, None, None, Weak::new(), new_events, messages, replacement.clone(),
        );
        let new_connect = tokio::spawn(async move { new_handle.connect().await });
        let new_request = received.next().await.expect("replacement public connect submitted");
        let CoreBluetoothMessage::ConnectDevice { identity, future: new_reply } = new_request else { panic!("expected connect request") };
        assert_eq!(identity.uuid(), peer);

        let (sender, mut receiver) = futures::channel::mpsc::channel(8);
        let delegate = CentralDelegate::new(sender, registry, None);
        // The queue has no exact native attempt for this callback, so it is
        // observable but must not acquire the replacement UUID owner.
        delegate.handoff_raw(CentralDelegateEvent::UnassociatedConnectionObservation { peripheral_uuid: peer, error: None }, None);
        let replacement_tickets = replacement.waiters.lock().unwrap().callback_tickets;
        let routed = receiver.try_recv().ok();
        old_reply.lock().unwrap().set_reply(CoreBluetoothReply::Err("fixture shutdown".into()));
        new_reply.lock().unwrap().set_reply(CoreBluetoothReply::Err("fixture shutdown".into()));
        let _ = old_connect.await;
        let _ = new_connect.await;
        assert_eq!(replacement_tickets, 0,
            "the old native callback must not reserve the replacement owner");
        assert!(matches!(routed, Some(CentralDelegateEvent::UnassociatedConnectionObservation { peripheral_uuid, error: None }) if peripheral_uuid == peer));
    }

    #[test]
    fn unassociated_lifecycle_handoff_remains_observable_without_a_lane_attempt() {
        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, mut receiver) = futures::channel::mpsc::channel(8);
        let delegate = CentralDelegate::new(sender, registry, None);
        let peer = Uuid::from_u128(940);
        delegate.send_event(CentralDelegateEvent::ConnectedDevice { peripheral_uuid: peer });
        let error = crate::PlatformError::new("corebluetooth", "native", "connection failed");
        delegate.send_event(CentralDelegateEvent::ConnectionFailed { peripheral_uuid: peer, error: Some(error.clone()) });
        delegate.apply_peer_lifecycle(peer, PeerLifecycle::DisconnectedWithError(error.clone()), false, None);
        assert!(matches!(receiver.try_recv(), Ok(CentralDelegateEvent::ConnectedDevice { peripheral_uuid }) if peripheral_uuid == peer));
        assert!(matches!(receiver.try_recv(), Ok(CentralDelegateEvent::ConnectionFailed { peripheral_uuid, error: Some(observed) }) if peripheral_uuid == peer && observed == error));
        assert!(matches!(receiver.try_recv(), Ok(CentralDelegateEvent::DisconnectedDevice { peripheral_uuid, error: Some(observed) }) if peripheral_uuid == peer && observed == error));
    }

    #[test]
    fn unassociated_connect_callback_must_not_claim_replacement_owner() {
        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, mut receiver) = futures::channel::mpsc::channel(8);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::from_u128(42);
        let old = registry.get_or_create(peer);
        registry.remove_if_current(peer, &old);
        let replacement = registry.get_or_create(peer);

        // No exact native attempt has been admitted: the callback stays an
        // observation rather than inventing a UUID owner.
        delegate.handoff_raw(CentralDelegateEvent::UnassociatedConnectionObservation { peripheral_uuid: peer, error: None }, None);

        assert_eq!(replacement.waiters.lock().unwrap().callback_tickets, 0,
            "an unassociated callback must not reserve the replacement owner");
        assert!(matches!(receiver.try_recv(), Ok(CentralDelegateEvent::UnassociatedConnectionObservation { peripheral_uuid, error: None }) if peripheral_uuid == peer));
    }

    #[test]
    fn unassociated_connect_failure_must_not_claim_replacement_owner() {
        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, mut receiver) = futures::channel::mpsc::channel(8);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::from_u128(43);
        let old = registry.get_or_create(peer);
        registry.remove_if_current(peer, &old);
        let replacement = registry.get_or_create(peer);

        // The UUID-only path cannot establish an owner. Preserve the native
        // failure as an observation instead of claiming the replacement.
        let native = crate::PlatformError::new("corebluetooth", "fixture-failure", "native failure");
        delegate.handoff_raw(CentralDelegateEvent::UnassociatedConnectionObservation {
            peripheral_uuid: peer, error: Some(native.clone()),
        }, None);

        assert_eq!(replacement.waiters.lock().unwrap().callback_tickets, 0,
            "an unassociated failure must not reserve the replacement owner");
        assert!(matches!(receiver.try_recv(), Ok(CentralDelegateEvent::UnassociatedConnectionObservation { peripheral_uuid, error: Some(error) }) if peripheral_uuid == peer && error == native));
    }

    #[test]
    fn retained_old_disconnect_keeps_replacement_delegate_cache() {
        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, _receiver) = futures::channel::mpsc::channel(8);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::from_u128(944);
        let old = registry.get_or_create(peer);
        // This cache is populated by the normal discovery callbacks. The
        // empty service vector is sufficient to prove UUID-keyed ownership;
        // no fabricated CBService crosses this test boundary.
        delegate.ivars().services.borrow_mut().insert(peer, Vec::new());
        registry.remove_if_current(peer, &old);
        let replacement = registry.get_or_create(peer);

        delegate.apply_peer_lifecycle(peer, PeerLifecycle::Disconnected, false, Some(&old));

        assert!(delegate.ivars().services.borrow().contains_key(&peer),
            "a terminal callback retained for the old owner must not clear replacement cache state");
        assert!(registry.get(peer).is_some_and(|current| std::sync::Arc::ptr_eq(&current, &replacement)));
    }

    #[test]
    fn identity_safe_disconnect_executor_routes_old_owner_through_actor_before_replacement_starts() {
        use std::{cell::RefCell, rc::Rc, sync::Arc};

        // This is the same executor called from didDisconnect.  It uses Rust
        // identities because CoreBluetooth framework objects are not valid
        // test fixtures outside the framework.  The lifecycle closure is the
        // real CentralDelegate actor handoff, not a mock event sink.
        let registry = Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, mut receiver) = futures::channel::mpsc::channel(8);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::from_u128(9_451);
        let old_owner = registry.get_or_create(peer);
        old_owner.waiters.lock().unwrap().ready = true;

        // Populate the UUID-keyed cache, retire the old owner, then install a
        // replacement.  The executor must retain its old destination while
        // `apply_peer_lifecycle` leaves this cache owned by the replacement.
        delegate.ivars().services.borrow_mut().insert(peer, Vec::new());
        registry.remove_if_current(peer, &old_owner);
        let replacement_owner = registry.get_or_create(peer);
        replacement_owner.waiters.lock().unwrap().ready = true;

        let manager = Rc::new(1_u8);
        let old_native = Rc::new(2_u8);
        let replacement_native = Rc::new(3_u8);
        let old_weak = Rc::downgrade(&old_native);
        let mut lane = super::super::native_lane::NativeLane::<
            Rc<u8>,
            Rc<u8>,
            Arc<super::super::internal::PeerSourceOwner>,
            &'static str,
            Option<crate::PlatformError>,
        >::new(manager.clone());
        let (old_id, _) = lane.connect(
            peer,
            old_native.clone(),
            old_owner.clone(),
            "old-connect",
            Rc::ptr_eq,
        ).unwrap();
        let (replacement_id, _) = lane.connect(
            peer,
            replacement_native.clone(),
            replacement_owner.clone(),
            "replacement-connect",
            Rc::ptr_eq,
        ).unwrap();
        let cancellation = match lane.disconnect(peer, old_id, "old-disconnect") {
            Ok(action) => action,
            Err(_) => panic!("old cycle must accept its disconnect"),
        };
        let callback_native = lane.resolve(peer, match cancellation {
            Some(super::super::native_attempt::Action::Cancel { object, .. }) => object,
            _ => panic!("active cycle must return its exact native identity"),
        }).unwrap().clone();
        drop(old_native);
        // The lane is now the sole retained destination for the old logical
        // owner.  UUID lookup cannot recover it because a replacement is
        // current in the registry.
        drop(old_owner);

        let starts = RefCell::new(Vec::new());
        let native = crate::PlatformError::new("corebluetooth", "test-link-lost", "test link lost");
        super::super::native_lane::execute_disconnect_callback(
            &mut lane,
            &manager,
            &callback_native,
            peer,
            Rc::ptr_eq,
            Rc::ptr_eq,
            Some(Some(native.clone())),
            |_reply, _error| {},
            |_replies, _error| {},
            |owner, error| {
                let lifecycle = error.flatten()
                    .map(PeerLifecycle::DisconnectedWithError)
                    .unwrap_or(PeerLifecycle::Disconnected);
                delegate.apply_peer_lifecycle(peer, lifecycle, false, Some(owner));
                !delegate.ivars().failed.load(std::sync::atomic::Ordering::Acquire)
            },
            |_, next| starts.borrow_mut().push(**next),
            |error| delegate.handoff_raw(
                CentralDelegateEvent::UnassociatedConnectionObservation {
                    peripheral_uuid: peer,
                    error: error.flatten(),
                },
                None,
            ),
        );
        drop(callback_native);

        let event = receiver.try_recv().expect("old lifecycle must reach the actor");
        let event = match event { CentralDelegateEvent::Ticketed { event, .. } => *event, event => event };
        assert!(matches!(event, CentralDelegateEvent::DisconnectedDevice { peripheral_uuid, error: Some(error) }
            if peripheral_uuid == peer && error == native));
        assert!(delegate.ivars().services.borrow().contains_key(&peer),
            "old-owner lifecycle must not clear replacement cache state");
        assert!(registry.get(peer).is_some_and(|current| Arc::ptr_eq(&current, &replacement_owner)));
        assert_eq!(&*starts.borrow(), &[3], "successor dispatch follows accepted old-owner actor handoff");
        assert_eq!(lane.active_cycle_for_owner(peer, &replacement_owner, Arc::ptr_eq), Some(replacement_id));
        assert!(old_weak.upgrade().is_none(), "the old identity retires after its actor handoff and successor dispatch");
    }

    #[test]
    fn adapter_reset_does_not_invalidate_a_replacement_registered_after_admission() {
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, _receiver) = futures::channel::mpsc::channel(8);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::from_u128(1);
        let old = registry.get_or_create(peer);
        let key = GraphKey { authority_id: 1, peer_id: peer.to_string(), connection_generation: "old".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = old.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new_with_barriers(8, 8192, 8, 8192);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        delegate.enqueue_adapter_state(CBManagerState::Resetting);
        registry.remove_if_current(peer, &old);
        let replacement = registry.get_or_create(peer);
        replacement.waiters.lock().unwrap().ready = true;
        let transition = delegate.ivars().adapter_transitions.borrow_mut().queue.pop_front().unwrap();
        delegate.apply_adapter_transition(transition.state, &transition.peers);
        assert!(!old.waiters.lock().unwrap().ready);
        assert!(replacement.waiters.lock().unwrap().ready, "replay must not select a replacement owner");
    }

    #[test]
    fn adapter_reset_preserves_a_later_generation_on_the_same_owner() {
        use crate::ubm_peer_notification_ingress::Poll as SourcePoll;
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, _receiver) = futures::channel::mpsc::channel(8);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::from_u128(4);
        let owner = registry.get_or_create(peer);
        let key = GraphKey { authority_id: 1, peer_id: peer.to_string(), connection_generation: "old".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new_with_barriers(8, 8192, 8, 8192);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        delegate.enqueue_adapter_state(CBManagerState::Resetting);
        owner.faults.start_source_generation(2).unwrap();
        let (_, replacement) = owner.faults.open_source_group(2, 8, 64).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.generation = Some(2);
        }
        let transition = delegate.ivars().adapter_transitions.borrow_mut().queue.pop_front().unwrap();
        delegate.apply_adapter_transition(transition.state, &transition.peers);
        let state = owner.waiters.lock().unwrap();
        assert!(state.ready, "replay must not invalidate the replacement generation");
        assert_eq!(state.generation, Some(2));
        assert_eq!(replacement.poll(), SourcePoll::Empty);
    }

    #[test]
    fn adapter_reset_retains_old_waiters_when_a_new_generation_arrives() {
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        use super::super::internal::{CoreBluetoothReply, CoreBluetoothReplyFuture};
        use futures::FutureExt;
        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, _receiver) = futures::channel::mpsc::channel(8);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::from_u128(5);
        let owner = registry.get_or_create(peer);
        let route = (AttrKey { uuid: peer, instance: 1 }, AttrKey { uuid: peer, instance: 2 });
        let waiter = CoreBluetoothReplyFuture::default();
        let key = GraphKey { authority_id: 1, peer_id: peer.to_string(), connection_generation: "old".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.read(route, waiter.get_state_clone());
            let mut gate = GraphGate::new_with_barriers(8, 8192, 8, 8192);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        delegate.enqueue_adapter_state(CBManagerState::Resetting);
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.generation = Some(2);
        }
        let transition = delegate.ivars().adapter_transitions.borrow_mut().queue.pop_front().unwrap();
        delegate.apply_adapter_transition(transition.state, &transition.peers);
        assert!(matches!(waiter.now_or_never(), Some(CoreBluetoothReply::Failed(error)) if error.code == "adapter-unavailable"));
        assert_eq!(owner.waiters.lock().unwrap().generation, Some(2));
    }

    #[test]
    fn coalesced_reset_captures_waiter_at_reset_admission() {
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        use super::super::internal::{CoreBluetoothReply, CoreBluetoothReplyFuture};
        use futures::FutureExt;
        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, _receiver) = futures::channel::mpsc::channel(128);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::from_u128(6);
        let owner = registry.get_or_create(peer);
        let route = (AttrKey { uuid: peer, instance: 1 }, AttrKey { uuid: peer, instance: 2 });
        let key = GraphKey { authority_id: 1, peer_id: peer.to_string(), connection_generation: "old".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new_with_barriers(256, 1_000_000, 256, 1_000_000);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        for _ in 0..65 { delegate.enqueue_adapter_state(CBManagerState::PoweredOn); }
        let waiter = CoreBluetoothReplyFuture::default();
        owner.waiters.lock().unwrap().read(route, waiter.get_state_clone());
        delegate.enqueue_adapter_state(CBManagerState::Resetting);
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.generation = Some(2);
        }
        let transition = delegate.ivars().adapter_transitions.borrow_mut().queue.pop_back().unwrap();
        delegate.apply_adapter_transition(CBManagerState::Resetting, &transition.peers);
        assert!(matches!(waiter.now_or_never(), Some(CoreBluetoothReply::Failed(error)) if error.code == "adapter-unavailable"));
        assert_eq!(owner.waiters.lock().unwrap().generation, Some(2));
    }

    #[test]
    fn disconnect_lifecycle_handoff_retains_native_error() {
        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, mut receiver) = futures::channel::mpsc::channel(8);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::from_u128(27);
        registry.get_or_create(peer);
        let native = crate::PlatformError::new("corebluetooth", "7", "link lost")
            .with("nsErrorDomain", "CBErrorDomain");
        delegate.apply_peer_lifecycle(peer, PeerLifecycle::DisconnectedWithError(native.clone()), false, None);
        let event = receiver.try_recv().unwrap();
        let event = match event { CentralDelegateEvent::Ticketed { event, .. } => *event, event => event };
        assert!(matches!(event, CentralDelegateEvent::DisconnectedDevice { peripheral_uuid, error: Some(error) }
            if peripheral_uuid == peer && error == native));
    }

    #[test]
    fn retired_admitted_disconnect_still_hands_off_its_lifecycle_event() {
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, mut receiver) = futures::channel::mpsc::channel(8);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::from_u128(2);
        let old = registry.get_or_create(peer);
        let key = GraphKey { authority_id: 1, peer_id: peer.to_string(), connection_generation: "old".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = old.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new(8, 8192);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        delegate.apply_peer_lifecycle(peer, PeerLifecycle::Disconnected, false, None);
        registry.remove_if_current(peer, &old);
        release_retained_lifecycle(&delegate, peer, &old, &key);
        drop(old);
        let event = receiver.try_recv().expect("retiring the admitted cut must retain its lifecycle event");
        let event = match event { CentralDelegateEvent::Ticketed { event, .. } => *event, event => event };
        assert!(matches!(event, CentralDelegateEvent::DisconnectedDevice { peripheral_uuid, .. } if peripheral_uuid == peer));
    }

    #[test]
    fn admitted_disconnect_error_survives_owner_replacement() {
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;

        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, mut receiver) = futures::channel::mpsc::channel(8);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::from_u128(44);
        let old = registry.get_or_create(peer);
        let key = GraphKey { authority_id: 1, peer_id: peer.to_string(), connection_generation: "old".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = old.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new(8, 8192);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        let native = crate::PlatformError::new("corebluetooth", "7", "link lost")
            .with("nsErrorDomain", "CBErrorDomain");
        delegate.apply_peer_lifecycle(peer, PeerLifecycle::DisconnectedWithError(native.clone()), false, None);
        registry.remove_if_current(peer, &old);
        let replacement = registry.get_or_create(peer);
        replacement.waiters.lock().unwrap().ready = true;
        release_retained_lifecycle(&delegate, peer, &old, &key);
        drop(old);

        let event = receiver.try_recv().expect("the admitted native disconnect must survive retirement");
        let event = match event { CentralDelegateEvent::Ticketed { event, .. } => *event, event => event };
        assert!(matches!(event, CentralDelegateEvent::DisconnectedDevice { peripheral_uuid, error: Some(error) }
            if peripheral_uuid == peer && error == native));
        assert!(replacement.waiters.lock().unwrap().ready, "the old disconnect must not cut the replacement");
    }

    #[tokio::test]
    async fn repeated_failed_handoffs_produce_one_explicit_route_failure() {
        use crate::common::util::{retained_notifications, NotificationFaults};
        use futures::{FutureExt, StreamExt};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::{DrainItem, GraphGate};
        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (sender, receiver) = futures::channel::mpsc::channel(1);
        drop(receiver);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::from_u128(3);
        let owner = registry.get_or_create(peer);
        let route = (AttrKey { uuid: peer, instance: 1 }, AttrKey { uuid: peer, instance: 2 });
        let (legacy_sender, legacy_receiver) = tokio::sync::broadcast::channel(8);
        let faults = std::sync::Arc::new(NotificationFaults::new(8));
        let mut legacy = retained_notifications(legacy_receiver, faults.clone());
        *owner.legacy.lock().unwrap() = Some((faults, legacy_sender));
        let key = GraphKey { authority_id: 1, peer_id: peer.to_string(), connection_generation: "old".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new(0, 0);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        for _ in 0..1000 {
            delegate.handoff_retained(peer, &owner, CentralDelegateEvent::CharacteristicNotified {
                peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, data: vec![1],
            }, Some((peer, route)));
        }
        let effect = {
            let mut state = owner.waiters.lock().unwrap();
            let gate = state.reclaim_gate.as_mut().unwrap();
            gate.release(&key).unwrap();
            let effect = match gate.next() { Ok(DrainItem::Event(effect)) => effect, _ => panic!("first failure must be retained") };
            assert!(matches!(gate.next(), Ok(DrainItem::Finished)), "failed handoff producer must not enqueue repeated cuts");
            effect
        };
        drop(effect);
        let failure = legacy.next().now_or_never().expect("the first failure remains observable").unwrap();
        assert_eq!(failure.source_failure.as_ref().unwrap().code, "source-handoff-failed");
        assert!(legacy.next().now_or_never().is_none());
    }

    #[tokio::test]
    async fn adapter_reset_waits_for_both_peer_prefixes_on_delegate_queue() {
        use crate::ubm_reclaim_authority::{GraphKey, GraphTarget, PermitStatus, AuthorityError};
        use std::sync::Arc;
        struct Setup { queue: super::super::ffi::dispatch_queue_t, registry: Arc<super::super::internal::PeerSourceRegistry>, ready: std::sync::mpsc::Sender<()> }
        unsafe extern "C" fn setup(context: *mut std::ffi::c_void) {
            let setup = unsafe { Box::from_raw(context.cast::<Setup>()) };
            let (sender, receiver) = futures::channel::mpsc::channel(32);
            let delegate = CentralDelegate::new_with_queue(sender, setup.registry.clone(), None, Some(setup.queue));
            delegate.ensure_queue_context();
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
            unsafe { (*local).test_events = Some(receiver); }
            setup.ready.send(()).unwrap();
        }
        unsafe extern "C" fn reset(context: *mut std::ffi::c_void) {
            let done = unsafe { Box::from_raw(context.cast::<std::sync::mpsc::Sender<()>>()) };
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
            unsafe { (*local).delegate.enqueue_adapter_state(CBManagerState::Resetting); }
            done.send(()).unwrap();
        }
        struct WriteAfter { peer: Uuid, done: std::sync::mpsc::Sender<()> }
        unsafe extern "C" fn write_after(context: *mut std::ffi::c_void) {
            let write = unsafe { Box::from_raw(context.cast::<WriteAfter>()) };
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
            unsafe { (*local).delegate.send_event(CentralDelegateEvent::CharacteristicWritten {
                peripheral_uuid: write.peer, service_uuid: AttrKey { uuid: write.peer, instance: 1 },
                characteristic_uuid: AttrKey { uuid: write.peer, instance: 2 },
            }); }
            write.done.send(()).unwrap();
        }
        unsafe extern "C" fn inspect(context: *mut std::ffi::c_void) {
            let done = unsafe { Box::from_raw(context.cast::<std::sync::mpsc::Sender<Vec<&'static str>>>()) };
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
            let receiver = unsafe { (*local).test_events.as_mut().unwrap() };
            let mut seen = Vec::new();
            while let Ok(event) = receiver.try_recv() {
                let event = match event {
                    CentralDelegateEvent::Ticketed { event, .. } | CentralDelegateEvent::AdapterTicketed { event, .. } => *event,
                    event => event,
                };
                match event {
                    CentralDelegateEvent::CharacteristicNotified { .. } => seen.push("value"),
                    CentralDelegateEvent::CharacteristicWritten { .. } => seen.push("write"),
                    CentralDelegateEvent::DidUpdateState { state: CBManagerState::Resetting } => seen.push("reset"),
                    _ => {},
                }
            }
            done.send(seen).unwrap();
        }
        let label = std::ffi::CString::new("ubm.reclaim.barrier-test").unwrap();
        let queue = unsafe { super::super::ffi::dispatch_queue_create(label.as_ptr(), super::super::ffi::DISPATCH_QUEUE_SERIAL) };
        let registry = Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (ready, started) = std::sync::mpsc::channel();
        let job = Box::new(Setup { queue, registry: registry.clone(), ready });
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(job).cast(), setup) };
        started.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let mut keys = Vec::new();
        for (index, peer) in [Uuid::from_u128(1), Uuid::from_u128(2)].into_iter().enumerate() {
            let owner = registry.get_or_create(peer);
            let graph = ("corebluetooth".to_owned(), owner.graph_attachment.unwrap(), 1);
            let key = GraphKey { authority_id: 1, peer_id: peer.to_string(), connection_generation: "connection".into(), graph: graph.clone(), permit_id: (index + 1) as u64 };
            let target = GraphTarget { peer_id: key.peer_id.clone(), connection_generation: key.connection_generation.clone(), graph: graph.clone() };
            let binding = { let mut state = owner.waiters.lock().unwrap(); state.ready = true; state.revision = 1; state.reclaim_binding.attach(); state.reclaim_binding.publish(graph.clone()); state.reclaim_binding.capture(key.peer_id.clone(), key.connection_generation.clone(), &graph).unwrap() };
            {
                let mut authority = owner.authorities.lock().unwrap();
                if index == 0 { authority.reserve_open(1).unwrap(); authority.resolve_open(1, true).unwrap(); }
                authority.reserve_permit_token_bound(1, (index + 1) as u64, target.clone(), &binding).unwrap();
                authority.resolve_permit_token(1, (index + 1) as u64, &target, true).unwrap();
            }
            assert_eq!(owner.acquire_reclaim(&binding, key.clone()), Ok(PermitStatus::Acquired));
            assert_eq!(owner.commit_reclaim(&key), Ok(PermitStatus::Committed));
            let route = (AttrKey { uuid: peer, instance: 1 }, AttrKey { uuid: peer, instance: 2 });
            assert!(!owner.value(route, true, &[1]));
            keys.push((peer, owner, key));
        }
        let (done, observed) = std::sync::mpsc::channel::<()>();
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(Box::new(done)).cast(), reset) };
        observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let (done, observed) = std::sync::mpsc::channel::<()>();
        let write = Box::new(WriteAfter { peer: keys[1].0, done });
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(write).cast(), write_after) };
        observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let second = unsafe { schedule_delegate_replay(queue, registry.clone(), keys[1].1.clone(), keys[1].0, keys[1].2.clone()) }.unwrap();
        assert_eq!(second.await.unwrap(), Err(AuthorityError::Pending));
        let (done, observed) = std::sync::mpsc::channel::<Vec<&'static str>>();
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(Box::new(done)).cast(), inspect) };
        assert_eq!(observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap(), vec!["value"]);
        let first = unsafe { schedule_delegate_replay(queue, registry.clone(), keys[0].1.clone(), keys[0].0, keys[0].2.clone()) }.unwrap();
        assert_eq!(first.await.unwrap(), Err(AuthorityError::Pending));
        let (done, observed) = std::sync::mpsc::channel::<Vec<&'static str>>();
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(Box::new(done)).cast(), inspect) };
        assert_eq!(observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap(), vec!["value", "reset", "write"]);
        registry.queue_closed.store(true, std::sync::atomic::Ordering::Release);
        unsafe { schedule_delegate_queue_cleanup(queue); super::super::ffi::dispatch_release(queue); }
    }

    #[test]
    fn invalidating_adapter_state_settles_the_queue_lane_before_graph_replay() {
        use std::sync::Arc;

        struct Setup {
            queue: super::super::ffi::dispatch_queue_t,
            registry: Arc<super::super::internal::PeerSourceRegistry>,
            ready: std::sync::mpsc::Sender<()>,
            invalidated: std::sync::mpsc::Sender<CBManagerState>,
        }
        unsafe extern "C" fn setup(context: *mut std::ffi::c_void) {
            let Setup {
                queue,
                registry,
                ready,
                invalidated,
            } = *unsafe { Box::from_raw(context.cast::<Setup>()) };
            let (sender, _receiver) = futures::channel::mpsc::channel(8);
            let delegate = CentralDelegate::new_with_queue(
                sender,
                registry,
                None,
                Some(queue),
            );
            delegate.ensure_queue_context();
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }
                .cast::<DelegateQueueContext>();
            unsafe {
                (*local).adapter_invalidation_observer = Some(Box::new(move |state| {
                    invalidated.send(state).unwrap();
                }));
            }
            ready.send(()).unwrap();
        }
        unsafe extern "C" fn invalidate(context: *mut std::ffi::c_void) {
            let done = unsafe {
                Box::from_raw(context.cast::<std::sync::mpsc::Sender<()>>())
            };
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }
                .cast::<DelegateQueueContext>();
            unsafe {
                (*local)
                    .delegate
                    .enqueue_adapter_state(CBManagerState::PoweredOff);
            }
            done.send(()).unwrap();
        }

        let label = std::ffi::CString::new("ubm.adapter-lane-invalidation-test").unwrap();
        let queue = unsafe {
            super::super::ffi::dispatch_queue_create(
                label.as_ptr(),
                super::super::ffi::DISPATCH_QUEUE_SERIAL,
            )
        };
        let registry = Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (ready, started) = std::sync::mpsc::channel();
        let (invalidated, observed) = std::sync::mpsc::channel();
        let job = Box::new(Setup {
            queue,
            registry: registry.clone(),
            ready,
            invalidated,
        });
        unsafe {
            super::super::ffi::dispatch_async_f(
                queue,
                Box::into_raw(job).cast(),
                setup,
            );
        }
        started
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let (done, completed) = std::sync::mpsc::channel::<()>();
        unsafe {
            super::super::ffi::dispatch_async_f(
                queue,
                Box::into_raw(Box::new(done)).cast(),
                invalidate,
            );
        }
        assert_eq!(
            observed
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            CBManagerState::PoweredOff,
        );
        completed
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        registry
            .queue_closed
            .store(true, std::sync::atomic::Ordering::Release);
        unsafe {
            schedule_delegate_queue_cleanup(queue);
            super::super::ffi::dispatch_release(queue);
        }
    }

    #[tokio::test]
    async fn adapter_reset_blocks_other_peers_inflight_discovery_until_release() {
        use crate::ubm_reclaim_authority::{GraphKey, GraphTarget, PermitStatus, AuthorityError};
        use std::sync::Arc;
        struct Setup { queue: super::super::ffi::dispatch_queue_t, registry: Arc<super::super::internal::PeerSourceRegistry>, ready: std::sync::mpsc::Sender<()> }
        unsafe extern "C" fn setup(context: *mut std::ffi::c_void) {
            let setup = unsafe { Box::from_raw(context.cast::<Setup>()) };
            let (sender, receiver) = futures::channel::mpsc::channel(32);
            let delegate = CentralDelegate::new_with_queue(sender, setup.registry.clone(), None, Some(setup.queue));
            delegate.ensure_queue_context();
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
            unsafe { (*local).test_events = Some(receiver); }
            setup.ready.send(()).unwrap();
        }
        unsafe extern "C" fn reset(context: *mut std::ffi::c_void) {
            let done = unsafe { Box::from_raw(context.cast::<std::sync::mpsc::Sender<()>>()) };
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
            unsafe { (*local).delegate.enqueue_adapter_state(CBManagerState::Resetting); }
            done.send(()).unwrap();
        }
        struct WriteAfter { peer: Uuid, done: std::sync::mpsc::Sender<()> }
        unsafe extern "C" fn write_after(context: *mut std::ffi::c_void) {
            let write = unsafe { Box::from_raw(context.cast::<WriteAfter>()) };
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
            unsafe { (*local).delegate.admit_discovery_snapshot(write.peer, DiscoverySnapshot::Synthetic { peer: write.peer }, 0); }
            write.done.send(()).unwrap();
        }
        unsafe extern "C" fn inspect(context: *mut std::ffi::c_void) {
            let done = unsafe { Box::from_raw(context.cast::<std::sync::mpsc::Sender<Vec<&'static str>>>()) };
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
            let receiver = unsafe { (*local).test_events.as_mut().unwrap() };
            let mut seen = Vec::new();
            while let Ok(event) = receiver.try_recv() {
                let event = match event {
                    CentralDelegateEvent::Ticketed { event, .. } | CentralDelegateEvent::AdapterTicketed { event, .. } => *event,
                    event => event,
                };
                match event {
                    CentralDelegateEvent::CharacteristicNotified { .. } => seen.push("value"),
                    CentralDelegateEvent::DiscoveryDrained { .. } => seen.push("discovery"),
                    CentralDelegateEvent::DidUpdateState { state: CBManagerState::Resetting } => seen.push("reset"),
                    _ => {},
                }
            }
            done.send(seen).unwrap();
        }
        let label = std::ffi::CString::new("ubm.reclaim.barrier-test").unwrap();
        let queue = unsafe { super::super::ffi::dispatch_queue_create(label.as_ptr(), super::super::ffi::DISPATCH_QUEUE_SERIAL) };
        let registry = Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (ready, started) = std::sync::mpsc::channel();
        let job = Box::new(Setup { queue, registry: registry.clone(), ready });
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(job).cast(), setup) };
        started.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let mut keys = Vec::new();
        for (index, peer) in [Uuid::from_u128(1), Uuid::from_u128(2)].into_iter().enumerate() {
            let owner = registry.get_or_create(peer);
            let graph = ("corebluetooth".to_owned(), owner.graph_attachment.unwrap(), 1);
            let key = GraphKey { authority_id: 1, peer_id: peer.to_string(), connection_generation: "connection".into(), graph: graph.clone(), permit_id: (index + 1) as u64 };
            let target = GraphTarget { peer_id: key.peer_id.clone(), connection_generation: key.connection_generation.clone(), graph: graph.clone() };
            let binding = { let mut state = owner.waiters.lock().unwrap(); state.ready = true; state.revision = 1; state.reclaim_binding.attach(); state.reclaim_binding.publish(graph.clone()); state.reclaim_binding.capture(key.peer_id.clone(), key.connection_generation.clone(), &graph).unwrap() };
            {
                let mut authority = owner.authorities.lock().unwrap();
                if index == 0 { authority.reserve_open(1).unwrap(); authority.resolve_open(1, true).unwrap(); }
                authority.reserve_permit_token_bound(1, (index + 1) as u64, target.clone(), &binding).unwrap();
                authority.resolve_permit_token(1, (index + 1) as u64, &target, true).unwrap();
            }
            if index == 0 {
                assert_eq!(owner.acquire_reclaim(&binding, key.clone()), Ok(PermitStatus::Acquired));
                assert_eq!(owner.commit_reclaim(&key), Ok(PermitStatus::Committed));
                let route = (AttrKey { uuid: peer, instance: 1 }, AttrKey { uuid: peer, instance: 2 });
                assert!(!owner.value(route, true, &[1]));
            } else { owner.waiters.lock().unwrap().discovery_actor_pending = true; }
            keys.push((peer, owner, key));
        }
        let (done, observed) = std::sync::mpsc::channel::<()>();
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(Box::new(done)).cast(), reset) };
        observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let (done, observed) = std::sync::mpsc::channel::<()>();
        let write = Box::new(WriteAfter { peer: keys[1].0, done });
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(write).cast(), write_after) };
        observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let (done, observed) = std::sync::mpsc::channel::<Vec<&'static str>>();
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(Box::new(done)).cast(), inspect) };
        assert!(observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap().is_empty());
        let first = unsafe { schedule_delegate_replay(queue, registry.clone(), keys[0].1.clone(), keys[0].0, keys[0].2.clone()) }.unwrap();
        assert_eq!(first.await.unwrap(), Err(AuthorityError::Pending));
        let (done, observed) = std::sync::mpsc::channel::<Vec<&'static str>>();
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(Box::new(done)).cast(), inspect) };
        assert_eq!(observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap(), vec!["value", "reset", "discovery"]);
        registry.queue_closed.store(true, std::sync::atomic::Ordering::Release);
        unsafe { schedule_delegate_queue_cleanup(queue); super::super::ffi::dispatch_release(queue); }
    }

    #[tokio::test]
    async fn adapter_overflow_retains_reset_and_latest_after_both_peers() {
        use crate::ubm_reclaim_authority::{GraphKey, GraphTarget, PermitStatus, AuthorityError};
        use std::sync::Arc;
        struct Setup { queue: super::super::ffi::dispatch_queue_t, registry: Arc<super::super::internal::PeerSourceRegistry>, ready: std::sync::mpsc::Sender<()> }
        unsafe extern "C" fn setup(context: *mut std::ffi::c_void) {
            let setup = unsafe { Box::from_raw(context.cast::<Setup>()) };
            let (sender, receiver) = futures::channel::mpsc::channel(256);
            let delegate = CentralDelegate::new_with_queue(sender, setup.registry.clone(), None, Some(setup.queue));
            delegate.ensure_queue_context();
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
            unsafe { (*local).test_events = Some(receiver); }
            setup.ready.send(()).unwrap();
        }
        unsafe extern "C" fn reset(context: *mut std::ffi::c_void) {
            let done = unsafe { Box::from_raw(context.cast::<std::sync::mpsc::Sender<()>>()) };
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
            unsafe { for _ in 0..70 { (*local).delegate.enqueue_adapter_state(CBManagerState::PoweredOn); }
                (*local).delegate.enqueue_adapter_state(CBManagerState::Resetting);
                (*local).delegate.enqueue_adapter_state(CBManagerState::PoweredOn); }
            done.send(()).unwrap();
        }
        unsafe extern "C" fn inspect(context: *mut std::ffi::c_void) {
            let done = unsafe { Box::from_raw(context.cast::<std::sync::mpsc::Sender<Vec<&'static str>>>()) };
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            let local = unsafe { super::super::ffi::dispatch_get_specific(key) }.cast::<DelegateQueueContext>();
            let receiver = unsafe { (*local).test_events.as_mut().unwrap() };
            let mut seen = Vec::new();
            while let Ok(event) = receiver.try_recv() {
                let event = match event {
                    CentralDelegateEvent::Ticketed { event, .. } | CentralDelegateEvent::AdapterTicketed { event, .. } => *event,
                    event => event,
                };
                match event {
                    CentralDelegateEvent::CharacteristicNotified { .. } => seen.push("value"),
                    CentralDelegateEvent::DidUpdateState { state: CBManagerState::Resetting } => seen.push("reset"),
                    CentralDelegateEvent::DidUpdateState { state: CBManagerState::PoweredOn } => seen.push("on"),
                    _ => {},
                }
            }
            done.send(seen).unwrap();
        }
        let label = std::ffi::CString::new("ubm.reclaim.barrier-test").unwrap();
        let queue = unsafe { super::super::ffi::dispatch_queue_create(label.as_ptr(), super::super::ffi::DISPATCH_QUEUE_SERIAL) };
        let registry = Arc::new(super::super::internal::PeerSourceRegistry::default());
        let (ready, started) = std::sync::mpsc::channel();
        let job = Box::new(Setup { queue, registry: registry.clone(), ready });
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(job).cast(), setup) };
        started.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let mut keys = Vec::new();
        for (index, peer) in [Uuid::from_u128(1), Uuid::from_u128(2)].into_iter().enumerate() {
            let owner = registry.get_or_create(peer);
            let graph = ("corebluetooth".to_owned(), owner.graph_attachment.unwrap(), 1);
            let key = GraphKey { authority_id: 1, peer_id: peer.to_string(), connection_generation: "connection".into(), graph: graph.clone(), permit_id: (index + 1) as u64 };
            let target = GraphTarget { peer_id: key.peer_id.clone(), connection_generation: key.connection_generation.clone(), graph: graph.clone() };
            let binding = { let mut state = owner.waiters.lock().unwrap(); state.ready = true; state.revision = 1; state.reclaim_binding.attach(); state.reclaim_binding.publish(graph.clone()); state.reclaim_binding.capture(key.peer_id.clone(), key.connection_generation.clone(), &graph).unwrap() };
            {
                let mut authority = owner.authorities.lock().unwrap();
                if index == 0 { authority.reserve_open(1).unwrap(); authority.resolve_open(1, true).unwrap(); }
                authority.reserve_permit_token_bound(1, (index + 1) as u64, target.clone(), &binding).unwrap();
                authority.resolve_permit_token(1, (index + 1) as u64, &target, true).unwrap();
            }
            assert_eq!(owner.acquire_reclaim(&binding, key.clone()), Ok(PermitStatus::Acquired));
            assert_eq!(owner.commit_reclaim(&key), Ok(PermitStatus::Committed));
            if index == 0 {
                let mut gate = crate::ubm_reclaim_graph_gate::GraphGate::new_with_barriers(256, 524_288, 1, 4096);
                gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
                owner.waiters.lock().unwrap().reclaim_gate = Some(gate);
            }
            let route = (AttrKey { uuid: peer, instance: 1 }, AttrKey { uuid: peer, instance: 2 });
            assert!(!owner.value(route, true, &[1]));
            keys.push((peer, owner, key));
        }
        let (done, observed) = std::sync::mpsc::channel::<()>();
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(Box::new(done)).cast(), reset) };
        observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let second = unsafe { schedule_delegate_replay(queue, registry.clone(), keys[1].1.clone(), keys[1].0, keys[1].2.clone()) }.unwrap();
        assert_eq!(second.await.unwrap(), Err(AuthorityError::Pending));
        let first = unsafe { schedule_delegate_replay(queue, registry.clone(), keys[0].1.clone(), keys[0].0, keys[0].2.clone()) }.unwrap();
        assert_eq!(first.await.unwrap(), Err(AuthorityError::Pending));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if keys.iter().all(|(_, owner, _)| !owner.waiters.lock().unwrap().reclaim_gate.as_ref().unwrap().is_busy()) { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        let (done, observed) = std::sync::mpsc::channel::<Vec<&'static str>>();
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(Box::new(done)).cast(), inspect) };
        let seen = observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(&seen[..2], &["value", "value"]);
        assert_eq!(&seen[seen.len() - 3..], &["on", "reset", "on"]);
        registry.queue_closed.store(true, std::sync::atomic::Ordering::Release);
        unsafe { schedule_delegate_queue_cleanup(queue); super::super::ffi::dispatch_release(queue); }
    }

    #[tokio::test]
    async fn queue_replay_survives_waiter_drop_and_releases_permit() {
        use crate::ubm_reclaim_authority::{GraphKey, GraphTarget, PermitStatus};
        use std::sync::{Arc, atomic::Ordering};
        struct Setup { queue: super::super::ffi::dispatch_queue_t, registry: Arc<super::super::internal::PeerSourceRegistry>, ready: std::sync::mpsc::Sender<()> }
        unsafe extern "C" fn setup(context: *mut std::ffi::c_void) {
            let setup = unsafe { Box::from_raw(context.cast::<Setup>()) };
            let (sender, _receiver) = futures::channel::mpsc::channel(4);
            let delegate = CentralDelegate::new_with_queue(sender, setup.registry.clone(), None, Some(setup.queue));
            delegate.ensure_queue_context();
            setup.ready.send(()).unwrap();
        }
        unsafe extern "C" fn fence(context: *mut std::ffi::c_void) {
            let done = unsafe { Box::from_raw(context.cast::<std::sync::mpsc::Sender<()>>()) };
            done.send(()).unwrap();
        }
        unsafe extern "C" fn after_cleanup(context: *mut std::ffi::c_void) {
            let done = unsafe { Box::from_raw(context.cast::<std::sync::mpsc::Sender<bool>>()) };
            let key = std::ptr::addr_of!(DELEGATE_CONTEXT_KEY).cast();
            done.send(unsafe { super::super::ffi::dispatch_get_specific(key) }.is_null()).unwrap();
        }
        let label = std::ffi::CString::new("ubm.reclaim.replay-test").unwrap();
        let queue = unsafe { super::super::ffi::dispatch_queue_create(label.as_ptr(), super::super::ffi::DISPATCH_QUEUE_SERIAL) };
        assert!(!queue.is_null());
        let registry = Arc::new(super::super::internal::PeerSourceRegistry::default());
        let peer = Uuid::nil();
        let owner = registry.get_or_create(peer);
        let graph = ("corebluetooth".to_owned(), owner.graph_attachment.unwrap(), 1);
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: graph.clone(), permit_id: 1 };
        let target = GraphTarget { peer_id: key.peer_id.clone(), connection_generation: key.connection_generation.clone(), graph: graph.clone() };
        let binding = {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true; state.revision = 1; state.reclaim_binding.attach(); state.reclaim_binding.publish(graph.clone());
            state.reclaim_binding.capture(key.peer_id.clone(), key.connection_generation.clone(), &graph).unwrap()
        };
        {
            let mut authority = owner.authorities.lock().unwrap();
            authority.reserve_open(1).unwrap(); authority.resolve_open(1, true).unwrap();
            authority.reserve_permit_token_bound(1, 1, target.clone(), &binding).unwrap();
            authority.resolve_permit_token(1, 1, &target, true).unwrap();
        }
        assert_eq!(owner.acquire_reclaim(&binding, key.clone()), Ok(PermitStatus::Acquired));
        let (ready, started) = std::sync::mpsc::channel();
        let setup_job = Box::new(Setup { queue, registry: registry.clone(), ready });
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(setup_job).cast(), setup) };
        started.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let reply = unsafe { schedule_delegate_replay(queue, registry.clone(), owner.clone(), peer, key.clone()) }.unwrap();
        drop(reply);
        let (done, completed) = std::sync::mpsc::channel::<()>();
        unsafe { super::super::ffi::dispatch_async_f(queue, Box::into_raw(Box::new(done)).cast(), fence) };
        completed.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(registry.replay_jobs.load(Ordering::Acquire), 0);
        assert_eq!(owner.authorities.lock().unwrap().permit_status(&key), Ok(PermitStatus::Released));
        registry.queue_closed.store(true, Ordering::Release);
        let (cleaned, observed) = std::sync::mpsc::channel::<bool>();
        unsafe {
            schedule_delegate_queue_cleanup(queue);
            super::super::ffi::dispatch_async_f(queue, Box::into_raw(Box::new(cleaned)).cast(), after_cleanup);
        }
        assert!(observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap());
        unsafe { super::super::ffi::dispatch_release(queue); }
    }

    #[tokio::test]
    async fn duplicate_barrier_identity_fails_actor_and_source_instead_of_waiting() {
        let registry = std::sync::Arc::new(super::super::internal::PeerSourceRegistry::default());
        let peer = Uuid::nil();
        let owner = registry.get_or_create(peer);
        let other = registry.get_or_create(Uuid::from_u128(1));
        owner.waiters.lock().unwrap().ready = true;
        assert!(matches!(owner.admit_adapter_barrier(1), crate::ubm_reclaim_graph_gate::BarrierAdmission::Drain));
        let (sender, _receiver) = futures::channel::mpsc::channel(4);
        let (signal, failed) = futures::channel::oneshot::channel();
        let delegate = CentralDelegate::new(sender, registry, Some(signal));
        delegate.enqueue_adapter_state(CBManagerState::Resetting);
        assert_eq!(failed.await.unwrap().code, "adapter-barrier-duplicate");
        assert!(!owner.waiters.lock().unwrap().ready);
        assert!(other.waiters.lock().unwrap().reclaim_gate.is_none());
    }

    #[test]
    fn overflow_retains_mandatory_reset_before_latest_powered_on() {
        assert_eq!(overflow_adapter_states(CBManagerState::PoweredOn, Some(CBManagerState::Resetting), CBManagerState::PoweredOn),
            vec![CBManagerState::PoweredOn, CBManagerState::Resetting, CBManagerState::PoweredOn]);
    }

    #[test]
    fn callback_slots_are_bounded_and_ids_are_not_reused() {
        let mut slots = CallbackSlots::new(1);
        assert_eq!(slots.insert(10), Ok(1));
        assert_eq!(slots.insert(20), Err(20));
        assert_eq!(slots.take(1), Some(10));
        assert_eq!(slots.insert(30), Ok(2));
        assert_eq!(slots.take(1), None);
        assert_eq!(slots.take(2), Some(30));
    }

 }
