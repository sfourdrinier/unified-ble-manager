// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.
//
// For more info on handling CoreBluetooth Managers (and possibly having
// multiple), see https://forums.developer.apple.com/thread/20810

use super::{
    central_delegate::{AttrKey, AttributeStage, CentralDelegate, CentralDelegateEvent, keyed},
    ffi,
    future::{BtlePlugFuture, BtlePlugFutureStateShared},
    utils::{
        core_bluetooth::{cbuuid_to_uuid, uuid_to_cbuuid},
        nsuuid_to_uuid,
    },
};
use crate::Error;
use crate::api::{CharPropFlags, Characteristic, Descriptor, ScanFilter, Service, WriteType};
use futures::channel::mpsc::{self, Receiver, Sender};
use futures::select;
use futures::sink::SinkExt;
use futures::stream::{Fuse, StreamExt};
use log::{error, trace, warn};
use objc2::{ClassType, msg_send_id};
use objc2::{rc::Retained, runtime::AnyObject};
use objc2_core_bluetooth::{
    CBCentralManager, CBCentralManagerScanOptionAllowDuplicatesKey, CBCharacteristic,
    CBCharacteristicProperties, CBCharacteristicWriteType, CBDescriptor, CBManager,
    CBManagerAuthorization, CBManagerState, CBPeripheral, CBPeripheralState, CBService, CBUUID,
};
use objc2_foundation::{NSArray, NSData, NSMutableDictionary, NSNumber};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    ffi::CString,
    fmt::{self, Debug, Formatter},
    ops::Deref,
    thread,
};
use tokio::runtime;
use uuid::Uuid;

struct DescriptorInternal {
    pub descriptor: Retained<CBDescriptor>,
    pub read_future_state: VecDeque<CoreBluetoothReplyStateShared>,
    pub write_future_state: VecDeque<CoreBluetoothReplyStateShared>,
}

impl DescriptorInternal {
    pub fn new(descriptor: Retained<CBDescriptor>) -> Self {
        Self {
            descriptor,
            read_future_state: VecDeque::with_capacity(10),
            write_future_state: VecDeque::with_capacity(10),
        }
    }
}

struct CharacteristicInternal {
    pub characteristic: Retained<CBCharacteristic>,
    pub uuid: Uuid,
    pub properties: CharPropFlags,
    pub descriptors: HashMap<AttrKey, DescriptorInternal>,
    pub read_future_state: VecDeque<CoreBluetoothReplyStateShared>,
    pub write_future_state: VecDeque<CoreBluetoothReplyStateShared>,
    pub subscribe_future_state: VecDeque<CoreBluetoothReplyStateShared>,
    pub unsubscribe_future_state: VecDeque<CoreBluetoothReplyStateShared>,
    pub discovered: bool,
}

impl CharacteristicInternal {
    /// UBM patch (UBM_PATCHES.md #14): what this characteristic has in
    /// flight, for the legacy read/notify provenance decisions.
    fn read_notify_state(&self) -> super::read_notify::ReadNotifyState {
        super::read_notify::ReadNotifyState {
            is_notifying: unsafe { self.characteristic.isNotifying() },
            pending_reads: !self.read_future_state.is_empty(),
            pending_subscribe: !self.subscribe_future_state.is_empty(),
            pending_unsubscribe: !self.unsubscribe_future_state.is_empty(),
        }
    }
}

/// UBM patch (UBM_PATCHES.md #14): answer the oldest pending read with one
/// successful value update; whether the value must also reach the
/// notification stream.
fn answer_value_update(characteristic: &mut CharacteristicInternal, data: &[u8]) -> bool {
    let route = characteristic.read_notify_state().route_value();
    if let Some(provenance) = route.read {
        if let Some(state) = characteristic.read_future_state.pop_back() {
            state
                .lock()
                .unwrap()
                .set_reply(CoreBluetoothReply::CharacteristicRead(data.to_vec(), provenance));
        }
    }
    route.notification
}

fn fail(state: CoreBluetoothReplyStateShared, error: crate::PlatformError) {
    state
        .lock()
        .unwrap()
        .set_reply(CoreBluetoothReply::Failed(error));
}

impl Debug for CharacteristicInternal {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("CBCharacteristic")
            .field("characteristic", self.characteristic.deref())
            .field("uuid", &self.uuid)
            .field("properties", &self.properties)
            .field("read_future_state", &self.read_future_state)
            .field("write_future_state", &self.write_future_state)
            .field("subscribe_future_state", &self.subscribe_future_state)
            .field("unsubscribe_future_state", &self.unsubscribe_future_state)
            .finish()
    }
}

impl CharacteristicInternal {
    pub fn new(characteristic: Retained<CBCharacteristic>) -> Self {
        let properties = CharacteristicInternal::form_flags(&*characteristic);
        let raw_uuid = unsafe { characteristic.UUID() };
        let uuid = cbuuid_to_uuid(&raw_uuid);
        let descriptors = unsafe { characteristic.descriptors() }
            .map(|descriptors| {
                keyed(descriptors, |descriptor| {
                    cbuuid_to_uuid(&*unsafe { descriptor.UUID() })
                })
                .into_iter()
                .map(|(key, descriptor)| (key, DescriptorInternal::new(descriptor)))
                .collect()
            })
            .unwrap_or_default();
        Self {
            characteristic,
            uuid,
            properties,
            descriptors,
            read_future_state: VecDeque::with_capacity(10),
            write_future_state: VecDeque::with_capacity(10),
            subscribe_future_state: VecDeque::with_capacity(10),
            unsubscribe_future_state: VecDeque::with_capacity(10),
            discovered: false,
        }
    }

    fn form_flags(characteristic: &CBCharacteristic) -> CharPropFlags {
        let flags = unsafe { characteristic.properties() };
        let mut v = CharPropFlags::default();
        if flags.contains(CBCharacteristicProperties::CBCharacteristicPropertyBroadcast) {
            v |= CharPropFlags::BROADCAST;
        }
        if flags.contains(CBCharacteristicProperties::CBCharacteristicPropertyRead) {
            v |= CharPropFlags::READ;
        }
        if flags.contains(CBCharacteristicProperties::CBCharacteristicPropertyWriteWithoutResponse)
        {
            v |= CharPropFlags::WRITE_WITHOUT_RESPONSE;
        }
        if flags.contains(CBCharacteristicProperties::CBCharacteristicPropertyWrite) {
            v |= CharPropFlags::WRITE;
        }
        if flags.contains(CBCharacteristicProperties::CBCharacteristicPropertyNotify) {
            v |= CharPropFlags::NOTIFY;
        }
        if flags.contains(CBCharacteristicProperties::CBCharacteristicPropertyIndicate) {
            v |= CharPropFlags::INDICATE;
        }
        if flags
            .contains(CBCharacteristicProperties::CBCharacteristicPropertyAuthenticatedSignedWrites)
        {
            v |= CharPropFlags::AUTHENTICATED_SIGNED_WRITES;
        }
        trace!("Flags: {:?}", v);
        v
    }
}

struct PendingWriteWithoutResponse {
    service_uuid: AttrKey,
    characteristic_uuid: AttrKey,
    data: Vec<u8>,
    fut: CoreBluetoothReplyStateShared,
}

#[derive(Clone, Debug)]
pub enum CoreBluetoothReply {
    AdapterState(CBManagerState),
    ReadResult(Vec<u8>),
    // UBM patch (UBM_PATCHES.md #14): a characteristic read's value and what
    // CoreBluetooth can say it is.
    CharacteristicRead(Vec<u8>, crate::api::ReadProvenance),
    ReadRssi(i16),
    // UBM patch (UBM_PATCHES.md #4): `canSendWriteWithoutResponse`.
    WriteReadiness(bool),
    // UBM patch (UBM_PATCHES.md #1): per-write-type maximum write length.
    WriteLengths {
        with_response: usize,
        without_response: usize,
    },
    Connected,
    ServicesDiscovered(BTreeSet<Service>),
    State(CBPeripheralState),
    Ok,
    Err(String),
    // UBM patch (UBM_PATCHES.md #15): the platform's own answer (an
    // `NSError`, or a patch-14 read/notify refusal).
    Failed(crate::PlatformError),
}

#[derive(Debug)]
pub enum PeripheralEventInternal {
    Disconnected,
    Notification(AttrKey, AttrKey, Vec<u8>),
    ManufacturerData(u16, Vec<u8>, i16),
    ServiceData(HashMap<Uuid, Vec<u8>>, i16),
    Services(Vec<Uuid>, i16),
    ServicesModified,
    TxPowerLevel(i16),
    // UBM patch (UBM_PATCHES.md #2).
    AdvertisementExtras(super::peripheral::AdvertisementExtras),
    // UBM patch (UBM_PATCHES.md #4): readiness after
    // `peripheralIsReadyToSendWriteWithoutResponse:` and the queue drain.
    WriteReadiness(bool),
    RssiRead(i16),
}

pub type CoreBluetoothReplyStateShared = BtlePlugFutureStateShared<CoreBluetoothReply>;
pub type CoreBluetoothReplyFuture = BtlePlugFuture<CoreBluetoothReply>;

struct ServiceInternal {
    cbservice: Retained<CBService>,
    characteristics: HashMap<AttrKey, CharacteristicInternal>,
    pub discovered: bool,
}

impl ServiceInternal {
    /// Record one discovery answer's characteristics, each under its own
    /// instance key (UBM patch #6).
    fn merge_characteristics(
        &mut self,
        characteristics: HashMap<AttrKey, Retained<CBCharacteristic>>,
    ) {
        for (characteristic_uuid, cb_characteristic) in characteristics {
            if let Some(existing) = self.characteristics.get_mut(&characteristic_uuid) {
                // Update the CB object reference and properties, but preserve
                // in-flight future state and already-discovered descriptors to
                // avoid dropping pending operations during late re-discovery
                // events (see issue #167).
                existing.properties = CharacteristicInternal::form_flags(&*cb_characteristic);
                existing.characteristic = cb_characteristic;
            } else {
                self.characteristics.insert(
                    characteristic_uuid,
                    CharacteristicInternal::new(cb_characteristic),
                );
            }
        }
    }
}

/// The API view of a discovered service table: every instance is its own
/// entry (UBM patch #6).
fn api_services(services: &HashMap<AttrKey, ServiceInternal>) -> BTreeSet<Service> {
    services
        .iter()
        .map(|(&service_key, service)| Service {
            uuid: service_key.uuid,
            instance: service_key.instance,
            primary: unsafe { service.cbservice.isPrimary() },
            characteristics: service
                .characteristics
                .iter()
                .map(|(&characteristic_key, characteristic)| {
                    let descriptors = characteristic
                        .descriptors
                        .keys()
                        .map(|descriptor_key| Descriptor {
                            uuid: descriptor_key.uuid,
                            instance: descriptor_key.instance,
                            service_uuid: service_key.uuid,
                            service_instance: service_key.instance,
                            characteristic_uuid: characteristic_key.uuid,
                            characteristic_instance: characteristic_key.instance,
                        })
                        .collect();
                    Characteristic {
                        uuid: characteristic_key.uuid,
                        instance: characteristic_key.instance,
                        service_uuid: service_key.uuid,
                        service_instance: service_key.instance,
                        descriptors,
                        properties: characteristic.properties,
                    }
                })
                .collect(),
        })
        .collect()
}

struct PeripheralInternal {
    pub peripheral: Retained<CBPeripheral>,
    services: HashMap<AttrKey, ServiceInternal>,
    pub event_sender: Sender<PeripheralEventInternal>,
    pub disconnected_future_state: Option<CoreBluetoothReplyStateShared>,
    pub connected_future_state: Option<CoreBluetoothReplyStateShared>,
    pub services_discovered_future_state: Option<CoreBluetoothReplyStateShared>,
    pub read_rssi_future_state: VecDeque<CoreBluetoothReplyStateShared>,
    pub write_without_response_queue: VecDeque<PendingWriteWithoutResponse>,
}

impl Debug for PeripheralInternal {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("CBPeripheral")
            .field("peripheral", self.peripheral.deref())
            .field(
                "services",
                &self
                    .services
                    .iter()
                    .map(|(service_uuid, service)| (service_uuid, service.characteristics.len()))
                    .collect::<HashMap<_, _>>(),
            )
            .field("event_sender", &self.event_sender)
            .field("connected_future_state", &self.connected_future_state)
            .field(
                "services_discovered_future_state",
                &self.services_discovered_future_state,
            )
            .finish()
    }
}

impl PeripheralInternal {
    pub fn new(
        peripheral: Retained<CBPeripheral>,
        event_sender: Sender<PeripheralEventInternal>,
    ) -> Self {
        Self {
            peripheral,
            services: HashMap::new(),
            event_sender,
            connected_future_state: None,
            disconnected_future_state: None,
            services_discovered_future_state: None,
            read_rssi_future_state: VecDeque::with_capacity(4),
            write_without_response_queue: VecDeque::new(),
        }
    }

    pub fn set_characteristics(
        &mut self,
        service_uuid: AttrKey,
        characteristics: HashMap<AttrKey, Retained<CBCharacteristic>>,
    ) {
        let service = self
            .services
            .get_mut(&service_uuid)
            .expect("Got characteristics for a service we don't know about");
        service.merge_characteristics(characteristics);
        if service.characteristics.is_empty() {
            service.discovered = true;
            self.check_discovered();
        }
    }

    pub fn set_characteristic_descriptors(
        &mut self,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptors: HashMap<AttrKey, Retained<CBDescriptor>>,
    ) {
        let service = self
            .services
            .get_mut(&service_uuid)
            .expect("Got descriptors for a service we don't know about");
        let characteristic = service
            .characteristics
            .get_mut(&characteristic_uuid)
            .expect("Got descriptors for a characteristic we don't know about");
        for (descriptor_uuid, cb_descriptor) in descriptors {
            if let Some(existing) = characteristic.descriptors.get_mut(&descriptor_uuid) {
                // Update the CB object reference but preserve in-flight future
                // state to avoid dropping pending operations during late
                // re-discovery events (see issue #167).
                existing.descriptor = cb_descriptor;
            } else {
                characteristic
                    .descriptors
                    .insert(descriptor_uuid, DescriptorInternal::new(cb_descriptor));
            }
        }
        characteristic.discovered = true;

        if !service
            .characteristics
            .values()
            .any(|characteristic| !characteristic.discovered)
        {
            service.discovered = true;
            self.check_discovered()
        }
    }

    fn check_discovered(&mut self) {
        // It's time for QUESTIONABLE ASSUMPTIONS.
        //
        // For sake of being lazy, we don't want to fire device connection until
        // we have all of our services and characteristics. We assume that
        // set_characteristics should be called once for every entry in the
        // service map. Once that's done, we're filled out enough and can send
        // back a ServicesDiscovered reply to the waiting future with all of
        // the characteristic info in it.
        if !self.services.values().any(|service| !service.discovered) {
            if self.services_discovered_future_state.is_none() {
                panic!("We should still have a future at this point!");
            }
            let services = api_services(&self.services);
            self.services_discovered_future_state
                .take()
                .unwrap()
                .lock()
                .unwrap()
                .set_reply(CoreBluetoothReply::ServicesDiscovered(services));
        }
    }

    pub fn confirm_disconnect(&mut self) {
        // Fulfill the disconnected future, if there is one.
        // There might not be a future if the device disconnects unexpectedly.
        if let Some(future) = self.disconnected_future_state.take() {
            future.lock().unwrap().set_reply(CoreBluetoothReply::Ok)
        }

        // Fulfill pending RSSI futures
        let error = CoreBluetoothReply::Err(String::from("Device disconnected"));
        for state in self.read_rssi_future_state.drain(..) {
            state.lock().unwrap().set_reply(error.clone());
        }

        // Fulfill pending write-without-response futures
        for pending in self.write_without_response_queue.drain(..) {
            pending.fut.lock().unwrap().set_reply(error.clone());
        }

        // UBM patch (UBM_PATCHES.md #19, finding 130): a connect, a
        // discovery and every attribute waiter, descriptor reads and writes
        // included, are answered now, as the legacy addon's
        // `failPendingForDevice` did; upstream left the discovery and the
        // descriptor waiters to hang. The database is gone with the link:
        // a reconnect discovers it again.
        if let Some(future) = self.connected_future_state.take() {
            future.lock().unwrap().set_reply(error.clone());
        }
        if let Some(future) = self.services_discovered_future_state.take() {
            future.lock().unwrap().set_reply(error.clone());
        }
        fail_attribute_waiters(&mut self.services, &error);
        self.services.clear();
    }
}

/// UBM patch (UBM_PATCHES.md #19): answer every waiter of every attribute
/// with `error`: characteristic reads, writes and notification changes, and
/// descriptor reads and writes.
fn fail_attribute_waiters(
    services: &mut HashMap<AttrKey, ServiceInternal>,
    error: &CoreBluetoothReply,
) {
    for service in services.values_mut() {
        for characteristic in service.characteristics.values_mut() {
            let waiters = characteristic
                .read_future_state
                .drain(..)
                .chain(characteristic.write_future_state.drain(..))
                .chain(characteristic.subscribe_future_state.drain(..))
                .chain(characteristic.unsubscribe_future_state.drain(..))
                .collect::<Vec<_>>();
            for descriptor in characteristic.descriptors.values_mut() {
                for state in descriptor
                    .read_future_state
                    .drain(..)
                    .chain(descriptor.write_future_state.drain(..))
                {
                    state.lock().unwrap().set_reply(error.clone());
                }
            }
            for state in waiters {
                state.lock().unwrap().set_reply(error.clone());
            }
        }
    }
}

// All of CoreBluetooth is basically async. It's all just waiting on delegate
// events/callbacks. Therefore, we should be able to round up all of our wacky
// ass mut *Object values, keep them in a single struct, in a single thread, and
// call it good. Right?
struct CoreBluetoothInternal {
    manager: Retained<CBCentralManager>,
    delegate: Retained<CentralDelegate>,
    // Map of identifiers to object pointers
    peripherals: HashMap<Uuid, PeripheralInternal>,
    delegate_receiver: Fuse<Receiver<CentralDelegateEvent>>,
    // Out in the world beyond CoreBluetooth, we'll be async, so just
    // task::block this when sending even though it'll never actually block.
    event_sender: Sender<CoreBluetoothEvent>,
    message_receiver: Fuse<Receiver<CoreBluetoothMessage>>,
}

impl Debug for CoreBluetoothInternal {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("CoreBluetoothInternal")
            .field("manager", self.manager.deref())
            .field("delegate", self.delegate.deref())
            .field("peripherals", &self.peripherals)
            .field("delegate_receiver", &self.delegate_receiver)
            .field("event_sender", &self.event_sender)
            .field("message_receiver", &self.message_receiver)
            .finish()
    }
}

#[derive(Debug)]
pub enum CoreBluetoothMessage {
    GetAdapterState {
        future: CoreBluetoothReplyStateShared,
    },
    StartScanning {
        filter: ScanFilter,
        // UBM patch (UBM_PATCHES.md #8): answered once the scan started or
        // was refused.
        future: CoreBluetoothReplyStateShared,
    },
    StopScanning,
    ConnectDevice {
        peripheral_uuid: Uuid,
        future: CoreBluetoothReplyStateShared,
    },
    // UBM patch (UBM_PATCHES.md #19): known, or retrieved by identifier.
    ResolvePeripheral {
        peripheral_uuid: Uuid,
        future: CoreBluetoothReplyStateShared,
    },
    DisconnectDevice {
        peripheral_uuid: Uuid,
        future: CoreBluetoothReplyStateShared,
    },
    ReadValue {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        future: CoreBluetoothReplyStateShared,
    },
    WriteValue {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        data: Vec<u8>,
        write_type: WriteType,
        future: CoreBluetoothReplyStateShared,
    },
    Subscribe {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        future: CoreBluetoothReplyStateShared,
    },
    Unsubscribe {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        future: CoreBluetoothReplyStateShared,
    },
    IsConnected {
        peripheral_uuid: Uuid,
        future: CoreBluetoothReplyStateShared,
    },
    ReadDescriptorValue {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptor_uuid: AttrKey,
        future: CoreBluetoothReplyStateShared,
    },
    WriteDescriptorValue {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptor_uuid: AttrKey,
        data: Vec<u8>,
        future: CoreBluetoothReplyStateShared,
    },
    DiscoverServices {
        peripheral_uuid: Uuid,
        future: CoreBluetoothReplyStateShared,
    },
    ReadRssi {
        peripheral_uuid: Uuid,
        future: CoreBluetoothReplyStateShared,
    },
    // UBM patch (UBM_PATCHES.md #1).
    GetWriteLengths {
        peripheral_uuid: Uuid,
        future: CoreBluetoothReplyStateShared,
    },
    // UBM patch (UBM_PATCHES.md #4).
    GetWriteReadiness {
        peripheral_uuid: Uuid,
        future: CoreBluetoothReplyStateShared,
    },
}

#[derive(Debug)]
pub enum CoreBluetoothEvent {
    DidUpdateState {
        state: CBManagerState,
    },
    DeviceDiscovered {
        uuid: Uuid,
        local_name: Option<String>,
        advertisement_name: Option<String>,
        event_receiver: Receiver<PeripheralEventInternal>,
    },
    DeviceUpdated {
        uuid: Uuid,
        local_name: Option<String>,
        advertisement_name: Option<String>,
    },
    DeviceDisconnected {
        uuid: Uuid,
    },
    // UBM patch (UBM_PATCHES.md #17): one advertisement with its own data.
    Advertised {
        uuid: Uuid,
        report: crate::api::AdvertisementReport,
    },
}

impl CoreBluetoothInternal {
    pub fn new(
        message_receiver: Receiver<CoreBluetoothMessage>,
        event_sender: Sender<CoreBluetoothEvent>,
    ) -> Self {
        // Pretty sure these come preallocated?
        let (sender, receiver) = mpsc::channel::<CentralDelegateEvent>(256);
        let delegate = CentralDelegate::new(sender);

        let label = CString::new("CBqueue").unwrap();
        let queue =
            unsafe { ffi::dispatch_queue_create(label.as_ptr(), ffi::DISPATCH_QUEUE_SERIAL) };
        let queue: *mut AnyObject = queue.cast();

        let manager = unsafe {
            msg_send_id![CBCentralManager::alloc(), initWithDelegate: &*delegate, queue: queue]
        };

        Self {
            manager,
            peripherals: HashMap::new(),
            delegate_receiver: receiver.fuse(),
            event_sender,
            message_receiver: message_receiver.fuse(),
            delegate,
        }
    }

    async fn dispatch_event(&self, event: CoreBluetoothEvent) {
        let mut s = self.event_sender.clone();
        if let Err(e) = s.send(event).await {
            error!("Error dispatching event: {:?}", e);
        }
    }

    async fn on_manufacturer_data(
        &mut self,
        peripheral_uuid: Uuid,
        manufacturer_id: u16,
        manufacturer_data: Vec<u8>,
        rssi: i16,
    ) {
        trace!(
            "Got manufacturer data advertisement! {}: {:?}",
            manufacturer_id, manufacturer_data
        );
        if let Some(p) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Err(e) = p
                .event_sender
                .send(PeripheralEventInternal::ManufacturerData(
                    manufacturer_id,
                    manufacturer_data,
                    rssi,
                ))
                .await
            {
                error!("Error sending notification event: {}", e);
            }
        }
    }

    async fn on_service_data(
        &mut self,
        peripheral_uuid: Uuid,
        service_data: HashMap<Uuid, Vec<u8>>,
        rssi: i16,
    ) {
        trace!("Got service data advertisement! {:?}", service_data);
        if let Some(p) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Err(e) = p
                .event_sender
                .send(PeripheralEventInternal::ServiceData(service_data, rssi))
                .await
            {
                error!("Error sending notification event: {}", e);
            }
        }
    }

    async fn on_services(&mut self, peripheral_uuid: Uuid, services: Vec<Uuid>, rssi: i16) {
        trace!("Got service advertisement! {:?}", services);
        if let Some(p) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Err(e) = p
                .event_sender
                .send(PeripheralEventInternal::Services(services, rssi))
                .await
            {
                error!("Error sending notification event: {}", e);
            }
        }
    }

    async fn on_services_modified(&mut self, peripheral_uuid: Uuid) {
        trace!(
            "Peripheral modified services and must be rediscovered! {:?}",
            peripheral_uuid
        );
        if let Some(p) = self.peripherals.get_mut(&peripheral_uuid) {
            p.services.clear();
            if let Err(e) = p
                .event_sender
                .send(PeripheralEventInternal::ServicesModified)
                .await
            {
                error!("Error sending notification event: {}", e);
            }
        }
    }

    async fn on_discovered_peripheral(
        &mut self,
        peripheral: Retained<CBPeripheral>,
        advertisement_name: Option<String>,
    ) {
        let id = unsafe { peripheral.identifier() };
        let uuid = nsuuid_to_uuid(&id);
        let peripheral_name = unsafe { peripheral.name() };
        let local_name = peripheral_name
            .map(|n| n.to_string())
            .or(advertisement_name.clone());

        if self.peripherals.contains_key(&uuid) {
            // UBM patch (UBM_PATCHES.md #17): every rediscovery is an
            // update, named or not (upstream skipped peripherals without a
            // name, so an unnamed one was reported once and never again).
            self.dispatch_event(CoreBluetoothEvent::DeviceUpdated {
                uuid,
                local_name,
                advertisement_name,
            })
            .await;
        } else {
            // Create our channels
            let (event_sender, event_receiver) = mpsc::channel(256);
            self.peripherals
                .insert(uuid, PeripheralInternal::new(peripheral, event_sender));
            self.dispatch_event(CoreBluetoothEvent::DeviceDiscovered {
                uuid,
                local_name,
                advertisement_name,
                event_receiver,
            })
            .await;
        }
    }

    fn on_discovered_services(
        &mut self,
        peripheral_uuid: Uuid,
        service_map: HashMap<AttrKey, Retained<CBService>>,
    ) {
        trace!("Found services!");
        for id in service_map.keys() {
            trace!("{}", id);
        }
        if let Some(p) = self.peripherals.get_mut(&peripheral_uuid) {
            let services = service_map
                .into_iter()
                .map(|(service_uuid, cbservice)| {
                    (
                        service_uuid,
                        ServiceInternal {
                            cbservice,
                            characteristics: HashMap::new(),
                            discovered: false,
                        },
                    )
                })
                .collect();
            p.services = services;
        }
    }

    fn on_discovered_characteristics(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristics: HashMap<AttrKey, Retained<CBCharacteristic>>,
    ) {
        trace!(
            "Found characteristics for peripheral {} service {}:",
            peripheral_uuid, service_uuid
        );
        for id in characteristics.keys() {
            trace!("{}", id);
        }
        if let Some(p) = self.peripherals.get_mut(&peripheral_uuid) {
            p.set_characteristics(service_uuid, characteristics);
        }
    }

    fn on_discovered_characteristic_descriptors(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptors: HashMap<AttrKey, Retained<CBDescriptor>>,
    ) {
        trace!(
            "Found descriptors for peripheral {} service {} characteristic {}:",
            peripheral_uuid, service_uuid, characteristic_uuid,
        );
        for id in descriptors.keys() {
            trace!("{}", id);
        }
        if let Some(p) = self.peripherals.get_mut(&peripheral_uuid) {
            p.set_characteristic_descriptors(service_uuid, characteristic_uuid, descriptors);
        }
    }

    fn on_peripheral_connect(&mut self, peripheral_uuid: Uuid) {
        if self.peripherals.contains_key(&peripheral_uuid) {
            let peripheral = self
                .peripherals
                .get_mut(&peripheral_uuid)
                .expect("If we're here we should have an ID");
            peripheral
                .connected_future_state
                .take()
                .unwrap()
                .lock()
                .unwrap()
                .set_reply(CoreBluetoothReply::Connected);
        }
    }

    fn on_peripheral_connection_failed(
        &mut self,
        peripheral_uuid: Uuid,
        error: Option<crate::PlatformError>,
    ) {
        trace!("Got connection fail event!");
        // UBM patch (UBM_PATCHES.md #15): the platform's own answer.
        let reply = match error {
            Some(error) => CoreBluetoothReply::Failed(error),
            None => CoreBluetoothReply::Err(String::from("Connection failed")),
        };
        if self.peripherals.contains_key(&peripheral_uuid) {
            let peripheral = self
                .peripherals
                .get_mut(&peripheral_uuid)
                .expect("If we're here we should have an ID");
            peripheral
                .connected_future_state
                .take()
                .unwrap()
                .lock()
                .unwrap()
                .set_reply(reply);
        }
    }

    async fn on_adapter_powered_off(&mut self) {
        warn!("Adapter powered off, canceling all pending operations");
        let peripheral_uuids: Vec<Uuid> = self.peripherals.keys().cloned().collect();
        for uuid in peripheral_uuids {
            if let Err(e) = self
                .peripherals
                .get_mut(&uuid)
                .unwrap()
                .event_sender
                .send(PeripheralEventInternal::Disconnected)
                .await
            {
                error!("Error sending disconnect event for {}: {}", uuid, e);
            }
            self.peripherals
                .get_mut(&uuid)
                .unwrap()
                .confirm_disconnect();
            self.dispatch_event(CoreBluetoothEvent::DeviceDisconnected { uuid })
                .await;
        }
        self.peripherals.clear();
    }

    async fn on_peripheral_disconnect(&mut self, peripheral_uuid: Uuid) {
        trace!("Got disconnect event!");
        if self.peripherals.contains_key(&peripheral_uuid) {
            if let Err(e) = self
                .peripherals
                .get_mut(&peripheral_uuid)
                .expect("If we're here we should have an ID")
                .event_sender
                .send(PeripheralEventInternal::Disconnected)
                .await
            {
                error!("Error sending notification event: {}", e);
            }
            // Unlike connect, we'll want to fulfill our disconnect future here, which means grabbing
            // our peripheral and having it fire, then dropping it and dispatching our event.
            // UBM patch (UBM_PATCHES.md #19, finding 127): the peripheral
            // stays known after a disconnect, as the legacy addon kept every
            // peripheral it had seen, so a reconnect needs no new scan.
            self.peripherals
                .get_mut(&peripheral_uuid)
                .expect("If we're here we should have an ID")
                .confirm_disconnect();
            self.dispatch_event(CoreBluetoothEvent::DeviceDisconnected {
                uuid: peripheral_uuid,
            })
            .await;
        }
    }

    /// UBM patch (UBM_PATCHES.md #14/#15): an attribute callback carried an
    /// `NSError`: the oldest waiter of that stage gets it as the platform's
    /// answer. A value-update error with no pending read is a failed
    /// notification, which the legacy addon ignored too.
    fn on_attribute_failed(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptor_uuid: Option<AttrKey>,
        stage: AttributeStage,
        error: crate::PlatformError,
    ) {
        let Some(characteristic) =
            self.get_characteristic(peripheral_uuid, service_uuid, characteristic_uuid)
        else {
            return;
        };
        let waiter = match descriptor_uuid {
            Some(descriptor_uuid) => {
                let Some(descriptor) = characteristic.descriptors.get_mut(&descriptor_uuid) else {
                    return;
                };
                match stage {
                    AttributeStage::Value => descriptor.read_future_state.pop_back(),
                    AttributeStage::Write => descriptor.write_future_state.pop_back(),
                    AttributeStage::NotifyState => None,
                }
            }
            None => match stage {
                AttributeStage::Value => characteristic.read_future_state.pop_back(),
                AttributeStage::Write => characteristic.write_future_state.pop_back(),
                AttributeStage::NotifyState => characteristic
                    .subscribe_future_state
                    .pop_back()
                    .or_else(|| characteristic.unsubscribe_future_state.pop_back()),
            },
        };
        if let Some(waiter) = waiter {
            fail(waiter, error);
        }
    }

    /// Get the CBCharacteristic for the given characteristic of the given peripheral, if it exists.
    fn get_characteristic(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
    ) -> Option<&mut CharacteristicInternal> {
        self.peripherals
            .get_mut(&peripheral_uuid)?
            .services
            .get_mut(&service_uuid)?
            .characteristics
            .get_mut(&characteristic_uuid)
    }

    /// Get the CBDescriptor for the given descriptor of the given peripheral, if it exists.
    fn get_descriptor(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptor_uuid: AttrKey,
    ) -> Option<&mut DescriptorInternal> {
        self.get_characteristic(peripheral_uuid, service_uuid, characteristic_uuid)?
            .descriptors
            .get_mut(&descriptor_uuid)
    }

    fn on_characteristic_subscribed(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
    ) {
        if let Some(characteristic) =
            self.get_characteristic(peripheral_uuid, service_uuid, characteristic_uuid)
        {
            trace!("Got subscribed event!");
            if let Some(state) = characteristic.subscribe_future_state.pop_back() {
                state.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
            }
        }
    }

    fn on_characteristic_unsubscribed(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
    ) {
        if let Some(characteristic) =
            self.get_characteristic(peripheral_uuid, service_uuid, characteristic_uuid)
        {
            trace!("Got unsubscribed event!");
            if let Some(state) = characteristic.unsubscribe_future_state.pop_back() {
                state.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
            } else if let Some(state) = characteristic.subscribe_future_state.pop_back() {
                // UBM patch (UBM_PATCHES.md #14): an enable that left the
                // characteristic not notifying fails (legacy 411) instead
                // of waiting forever.
                fail(state, super::read_notify::enable_not_notifying_error());
            }
        }
    }

    async fn on_characteristic_read(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        data: Vec<u8>,
    ) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Some(service) = peripheral.services.get_mut(&service_uuid) {
                if let Some(characteristic) = service.characteristics.get_mut(&characteristic_uuid)
                {
                    trace!("Got read event!");
                    // Reads and notifications both return the same callback.
                    // UBM patch (UBM_PATCHES.md #14): the oldest pending read
                    // completes with the provenance CoreBluetooth can give
                    // it, and a value that may be a notification still
                    // reaches the notification stream.
                    if answer_value_update(characteristic, &data) {
                        if let Err(e) = peripheral
                            .event_sender
                            .send(PeripheralEventInternal::Notification(
                                characteristic_uuid,
                                service_uuid,
                                data,
                            ))
                            .await
                        {
                            error!("Error sending notification event: {}", e);
                        }
                    }
                }
            }
        }
    }

    fn on_characteristic_written(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
    ) {
        if let Some(characteristic) =
            self.get_characteristic(peripheral_uuid, service_uuid, characteristic_uuid)
        {
            trace!("Got written event!");
            if let Some(state) = characteristic.write_future_state.pop_back() {
                state.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
            }
        }
    }

    async fn connect_peripheral(
        &mut self,
        peripheral_uuid: Uuid,
        fut: CoreBluetoothReplyStateShared,
    ) {
        trace!("Trying to connect peripheral!");
        // UBM patch (UBM_PATCHES.md #19, finding 127): a peripheral this
        // central no longer holds (CoreBluetooth invalidated it at a
        // power-off or reset) is retrieved by identifier, as the legacy addon
        // did; upstream never answered the connect.
        if !self.peripherals.contains_key(&peripheral_uuid)
            && !self.retrieve_peripheral(peripheral_uuid).await
        {
            fut.lock()
                .unwrap()
                .set_reply(CoreBluetoothReply::Err(String::from(
                    "Peripheral not found",
                )));
            return;
        }
        if let Some(p) = self.peripherals.get_mut(&peripheral_uuid) {
            trace!("Connecting peripheral!");
            p.connected_future_state = Some(fut);
            unsafe { self.manager.connectPeripheral_options(&p.peripheral, None) };
        }
    }

    /// UBM patch (UBM_PATCHES.md #19): `retrievePeripheralsWithIdentifiers`
    /// for one identifier (legacy addon `addon.mm:796-846`). A retrieved
    /// peripheral is registered again; the adapter replaces its entry.
    /// `false` when CoreBluetooth does not know the identifier.
    async fn retrieve_peripheral(&mut self, peripheral_uuid: Uuid) -> bool {
        let identifier = objc2_foundation::NSUUID::from_bytes(*peripheral_uuid.as_bytes());
        let identifiers = NSArray::from_vec(vec![identifier]);
        let retrieved = unsafe {
            self.manager
                .retrievePeripheralsWithIdentifiers(&identifiers)
        };
        let Some(peripheral) = retrieved.iter().find(|candidate| {
            nsuuid_to_uuid(&*unsafe { candidate.identifier() }) == peripheral_uuid
        }) else {
            return false;
        };
        let local_name = unsafe { peripheral.name() }.map(|name| name.to_string());
        let (event_sender, event_receiver) = mpsc::channel(256);
        self.peripherals.insert(
            peripheral_uuid,
            PeripheralInternal::new(peripheral.retain(), event_sender),
        );
        self.dispatch_event(CoreBluetoothEvent::DeviceDiscovered {
            uuid: peripheral_uuid,
            local_name,
            advertisement_name: None,
            event_receiver,
        })
        .await;
        true
    }

    /// UBM patch (UBM_PATCHES.md #19): resolve a peripheral by identifier
    /// for `Central::add_peripheral`: known, or retrieved from CoreBluetooth.
    async fn resolve_peripheral(
        &mut self,
        peripheral_uuid: Uuid,
        fut: CoreBluetoothReplyStateShared,
    ) {
        let known = self.peripherals.contains_key(&peripheral_uuid)
            || self.retrieve_peripheral(peripheral_uuid).await;
        fut.lock().unwrap().set_reply(if known {
            CoreBluetoothReply::Ok
        } else {
            CoreBluetoothReply::Err(String::from("Peripheral not found"))
        });
    }

    fn disconnect_peripheral(&mut self, peripheral_uuid: Uuid, fut: CoreBluetoothReplyStateShared) {
        trace!("Trying to disconnect peripheral!");
        if let Some(p) = self.peripherals.get_mut(&peripheral_uuid) {
            trace!("Disconnecting peripheral!");
            p.disconnected_future_state = Some(fut);
            unsafe { self.manager.cancelPeripheralConnection(&p.peripheral) };
        }
    }

    fn is_connected(&mut self, peripheral_uuid: Uuid, fut: CoreBluetoothReplyStateShared) {
        if let Some(p) = self.peripherals.get_mut(&peripheral_uuid) {
            let state = unsafe { p.peripheral.state() };
            trace!("Connected state {:?} ", state);
            fut.lock()
                .unwrap()
                .set_reply(CoreBluetoothReply::State(state));
        } else {
            // Peripheral was removed after disconnect — report as disconnected
            // rather than hanging the future forever.
            fut.lock()
                .unwrap()
                .set_reply(CoreBluetoothReply::State(CBPeripheralState::Disconnected));
        }
    }

    fn write_value(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        data: Vec<u8>,
        kind: WriteType,
        fut: CoreBluetoothReplyStateShared,
    ) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Some(service) = peripheral.services.get_mut(&service_uuid) {
                if let Some(characteristic) = service.characteristics.get_mut(&characteristic_uuid)
                {
                    trace!("Writing value! With kind {:?}", kind);
                    match kind {
                        WriteType::WithoutResponse => {
                            if unsafe { peripheral.peripheral.canSendWriteWithoutResponse() } {
                                unsafe {
                                    peripheral.peripheral.writeValue_forCharacteristic_type(
                                        &NSData::from_vec(data),
                                        &characteristic.characteristic,
                                        CBCharacteristicWriteType::CBCharacteristicWriteWithoutResponse,
                                    );
                                }
                                fut.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
                            } else {
                                trace!("Queueing write-without-response (peripheral not ready)");
                                peripheral.write_without_response_queue.push_back(
                                    PendingWriteWithoutResponse {
                                        service_uuid,
                                        characteristic_uuid,
                                        data,
                                        fut,
                                    },
                                );
                            }
                        }
                        WriteType::WithResponse => {
                            unsafe {
                                peripheral.peripheral.writeValue_forCharacteristic_type(
                                    &NSData::from_vec(data),
                                    &characteristic.characteristic,
                                    CBCharacteristicWriteType::CBCharacteristicWriteWithResponse,
                                );
                            }
                            characteristic.write_future_state.push_front(fut);
                        }
                    }
                }
            }
        }
    }

    fn drain_write_without_response_queue(&mut self, peripheral_uuid: Uuid) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            while let Some(pending) = peripheral.write_without_response_queue.pop_front() {
                if !unsafe { peripheral.peripheral.canSendWriteWithoutResponse() } {
                    peripheral.write_without_response_queue.push_front(pending);
                    break;
                }
                if let Some(service) = peripheral.services.get(&pending.service_uuid) {
                    if let Some(characteristic) =
                        service.characteristics.get(&pending.characteristic_uuid)
                    {
                        unsafe {
                            peripheral.peripheral.writeValue_forCharacteristic_type(
                                &NSData::from_vec(pending.data),
                                &characteristic.characteristic,
                                CBCharacteristicWriteType::CBCharacteristicWriteWithoutResponse,
                            );
                        }
                        pending
                            .fut
                            .lock()
                            .unwrap()
                            .set_reply(CoreBluetoothReply::Ok);
                    } else {
                        pending
                            .fut
                            .lock()
                            .unwrap()
                            .set_reply(CoreBluetoothReply::Err(
                                "Characteristic no longer available".into(),
                            ));
                    }
                } else {
                    pending
                        .fut
                        .lock()
                        .unwrap()
                        .set_reply(CoreBluetoothReply::Err(
                            "Service no longer available".into(),
                        ));
                }
            }
        }
    }

    fn read_value(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        fut: CoreBluetoothReplyStateShared,
    ) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Some(service) = peripheral.services.get_mut(&service_uuid) {
                if let Some(characteristic) = service.characteristics.get_mut(&characteristic_uuid)
                {
                    // UBM patch (UBM_PATCHES.md #14): every read runs, also
                    // while the characteristic notifies; waiters complete in
                    // request order with their provenance.
                    trace!("Reading value!");
                    unsafe {
                        peripheral
                            .peripheral
                            .readValueForCharacteristic(&characteristic.characteristic);
                    }
                    characteristic.read_future_state.push_front(fut);
                }
            }
        }
    }

    fn subscribe(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        fut: CoreBluetoothReplyStateShared,
    ) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Some(service) = peripheral.services.get_mut(&service_uuid) {
                if let Some(characteristic) = service.characteristics.get_mut(&characteristic_uuid)
                {
                    trace!("Setting subscribe!");
                    unsafe {
                        peripheral
                            .peripheral
                            .setNotifyValue_forCharacteristic(true, &characteristic.characteristic);
                    }
                    characteristic.subscribe_future_state.push_front(fut);
                }
            }
        }
    }

    fn unsubscribe(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        fut: CoreBluetoothReplyStateShared,
    ) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Some(service) = peripheral.services.get_mut(&service_uuid) {
                if let Some(characteristic) = service.characteristics.get_mut(&characteristic_uuid)
                {
                    trace!("Setting subscribe!");
                    unsafe {
                        peripheral.peripheral.setNotifyValue_forCharacteristic(
                            false,
                            &characteristic.characteristic,
                        );
                    }
                    characteristic.unsubscribe_future_state.push_front(fut);
                }
            }
        }
    }

    fn write_descriptor_value(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptor_uuid: AttrKey,
        data: Vec<u8>,
        fut: CoreBluetoothReplyStateShared,
    ) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Some(service) = peripheral.services.get_mut(&service_uuid) {
                if let Some(characteristic) = service.characteristics.get_mut(&characteristic_uuid)
                {
                    if let Some(descriptor) = characteristic.descriptors.get_mut(&descriptor_uuid) {
                        trace!("Writing descriptor value!");
                        unsafe {
                            peripheral.peripheral.writeValue_forDescriptor(
                                &NSData::from_vec(data),
                                &descriptor.descriptor,
                            );
                        }
                        descriptor.write_future_state.push_front(fut);
                    }
                }
            }
        }
    }

    fn read_descriptor_value(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptor_uuid: AttrKey,
        fut: CoreBluetoothReplyStateShared,
    ) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Some(service) = peripheral.services.get_mut(&service_uuid) {
                if let Some(characteristic) = service.characteristics.get_mut(&characteristic_uuid)
                {
                    if let Some(descriptor) = characteristic.descriptors.get_mut(&descriptor_uuid) {
                        trace!("Reading descriptor value!");
                        unsafe {
                            peripheral
                                .peripheral
                                .readValueForDescriptor(&descriptor.descriptor);
                        }
                        descriptor.read_future_state.push_front(fut);
                    }
                }
            }
        }
    }

    fn read_rssi(&mut self, peripheral_uuid: Uuid, fut: CoreBluetoothReplyStateShared) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            trace!("Reading RSSI!");
            unsafe {
                peripheral.peripheral.readRSSI();
            }
            peripheral.read_rssi_future_state.push_front(fut);
        }
    }

    // UBM patch (UBM_PATCHES.md #4): the current
    // `canSendWriteWithoutResponse` of one peripheral.
    fn get_write_readiness(&mut self, peripheral_uuid: Uuid, fut: CoreBluetoothReplyStateShared) {
        let reply = match self.peripherals.get(&peripheral_uuid) {
            Some(peripheral) => CoreBluetoothReply::WriteReadiness(unsafe {
                peripheral.peripheral.canSendWriteWithoutResponse()
            }),
            None => CoreBluetoothReply::Err(String::from("Peripheral not found")),
        };
        fut.lock().unwrap().set_reply(reply);
    }

    // UBM patch (UBM_PATCHES.md #4): after CoreBluetooth reports readiness
    // and the queued writes drained, report what readiness is left.
    async fn on_write_readiness(&mut self, peripheral_uuid: Uuid) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            let ready = unsafe { peripheral.peripheral.canSendWriteWithoutResponse() };
            if let Err(e) = peripheral
                .event_sender
                .send(PeripheralEventInternal::WriteReadiness(ready))
                .await
            {
                error!("Error sending write readiness event: {}", e);
            }
        }
    }

    // UBM patch (UBM_PATCHES.md #1): `-[CBPeripheral
    // maximumWriteValueLengthForType:]` for both write types, read on the
    // CoreBluetooth thread that owns the peripheral.
    fn get_write_lengths(&mut self, peripheral_uuid: Uuid, fut: CoreBluetoothReplyStateShared) {
        let reply = match self.peripherals.get(&peripheral_uuid) {
            Some(peripheral) => unsafe {
                CoreBluetoothReply::WriteLengths {
                    with_response: peripheral.peripheral.maximumWriteValueLengthForType(
                        CBCharacteristicWriteType::CBCharacteristicWriteWithResponse,
                    ),
                    without_response: peripheral.peripheral.maximumWriteValueLengthForType(
                        CBCharacteristicWriteType::CBCharacteristicWriteWithoutResponse,
                    ),
                }
            },
            None => CoreBluetoothReply::Err(String::from("Peripheral not found")),
        };
        fut.lock().unwrap().set_reply(reply);
    }

    async fn on_read_rssi(&mut self, peripheral_uuid: Uuid, rssi: i16) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            trace!("Got RSSI read event: {}", rssi);
            if let Some(state) = peripheral.read_rssi_future_state.pop_back() {
                state
                    .lock()
                    .unwrap()
                    .set_reply(CoreBluetoothReply::ReadRssi(rssi));
            }
            // Also send as a peripheral event for CentralEvent emission
            if let Err(e) = peripheral
                .event_sender
                .send(PeripheralEventInternal::RssiRead(rssi))
                .await
            {
                error!("Error sending RSSI event: {}", e);
            }
        }
    }

    // UBM patch (UBM_PATCHES.md #2).
    async fn on_advertisement_extras(
        &mut self,
        peripheral_uuid: Uuid,
        extras: super::peripheral::AdvertisementExtras,
    ) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Err(e) = peripheral
                .event_sender
                .send(PeripheralEventInternal::AdvertisementExtras(extras))
                .await
            {
                error!("Error sending advertisement extras event: {}", e);
            }
        }
    }

    async fn on_tx_power_level(&mut self, peripheral_uuid: Uuid, tx_power_level: i16) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Err(e) = peripheral
                .event_sender
                .send(PeripheralEventInternal::TxPowerLevel(tx_power_level))
                .await
            {
                error!("Error sending tx_power_level event: {}", e);
            }
        }
    }

    async fn on_descriptor_read(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptor_uuid: AttrKey,
        data: Vec<u8>,
    ) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            if let Some(service) = peripheral.services.get_mut(&service_uuid) {
                if let Some(characteristic) = service.characteristics.get_mut(&characteristic_uuid)
                {
                    if let Some(descriptor) = characteristic.descriptors.get_mut(&descriptor_uuid) {
                        trace!("Got read event!");

                        let mut data_clone = Vec::new();
                        for byte in data.iter() {
                            data_clone.push(*byte);
                        }
                        if let Some(state) = descriptor.read_future_state.pop_back() {
                            state
                                .lock()
                                .unwrap()
                                .set_reply(CoreBluetoothReply::ReadResult(data_clone));
                        }
                    }
                }
            }
        }
    }

    fn on_descriptor_written(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        descriptor_uuid: AttrKey,
    ) {
        if let Some(descriptor) = self.get_descriptor(
            peripheral_uuid,
            service_uuid,
            characteristic_uuid,
            descriptor_uuid,
        ) {
            trace!("Got written event!");
            if let Some(state) = descriptor.write_future_state.pop_back() {
                state.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
            }
        }
    }

    fn discover_services(&mut self, peripheral_uuid: Uuid, fut: CoreBluetoothReplyStateShared) {
        if let Some(p) = self.peripherals.get_mut(&peripheral_uuid) {
            trace!("Discovering services!");
            p.services_discovered_future_state = Some(fut);
            // This will trigger the delegate_peripheral_diddiscoverservices in central_delegate.rs
            unsafe { p.peripheral.discoverServices(None) };
        }
    }

    async fn wait_for_message(&mut self) {
        select! {
            delegate_msg = self.delegate_receiver.select_next_some() => {
                match delegate_msg {
                    // TODO We should probably also register some sort of
                    // "ready" variable in our adapter that will cause scans/etc
                    // to fail if this hasn't updated.
                    CentralDelegateEvent::DidUpdateState{state} => {
                        // UBM patch (UBM_PATCHES.md #7): CoreBluetooth
                        // invalidates every peripheral when it resets or
                        // loses authorization too, not only on power-off.
                        if matches!(
                            state,
                            CBManagerState::PoweredOff
                                | CBManagerState::Resetting
                                | CBManagerState::Unsupported
                                | CBManagerState::Unauthorized
                        ) {
                            self.on_adapter_powered_off().await;
                        }
                        self.dispatch_event(CoreBluetoothEvent::DidUpdateState{state}).await
                    }
                    CentralDelegateEvent::DiscoveredPeripheral{cbperipheral, advertisement_name} => {
                        self.on_discovered_peripheral(cbperipheral, advertisement_name).await
                    }
                    CentralDelegateEvent::DiscoveredServices{peripheral_uuid, services} => {
                        self.on_discovered_services(peripheral_uuid, services)
                    }
                    CentralDelegateEvent::DiscoveredCharacteristics{peripheral_uuid, service_uuid, characteristics} => {
                        self.on_discovered_characteristics(peripheral_uuid, service_uuid, characteristics)
                    }
                    CentralDelegateEvent::DiscoveredCharacteristicDescriptors{peripheral_uuid, service_uuid, characteristic_uuid, descriptors} => {
                        self.on_discovered_characteristic_descriptors(peripheral_uuid, service_uuid, characteristic_uuid, descriptors)
                    }
                    CentralDelegateEvent::ConnectedDevice{peripheral_uuid} => {
                        self.on_peripheral_connect(peripheral_uuid)
                    },
                    CentralDelegateEvent::ConnectionFailed{peripheral_uuid, error} => {
                        self.on_peripheral_connection_failed(peripheral_uuid, error)
                    },
                    CentralDelegateEvent::DisconnectedDevice{peripheral_uuid} => {
                        self.on_peripheral_disconnect(peripheral_uuid).await
                    }
                    CentralDelegateEvent::CharacteristicSubscribed{
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                     } => self.on_characteristic_subscribed(peripheral_uuid, service_uuid, characteristic_uuid),
                    CentralDelegateEvent::CharacteristicUnsubscribed{
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                     } => self.on_characteristic_unsubscribed(peripheral_uuid, service_uuid,characteristic_uuid),
                    CentralDelegateEvent::CharacteristicNotified{
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                        data,
                     } => self.on_characteristic_read(peripheral_uuid, service_uuid,characteristic_uuid, data).await,
                    CentralDelegateEvent::CharacteristicWritten{
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                    } => self.on_characteristic_written(peripheral_uuid, service_uuid, characteristic_uuid),
                    CentralDelegateEvent::ManufacturerData{peripheral_uuid, manufacturer_id, data, rssi} => {
                        self.on_manufacturer_data(peripheral_uuid, manufacturer_id, data, rssi).await
                    },
                    CentralDelegateEvent::ServiceData{peripheral_uuid, service_data, rssi} => {
                        self.on_service_data(peripheral_uuid, service_data, rssi).await
                    },
                    CentralDelegateEvent::Services{peripheral_uuid, service_uuids, rssi} => {
                        self.on_services(peripheral_uuid, service_uuids, rssi).await
                    },
                    CentralDelegateEvent::ServicesModified{peripheral_uuid} => {
                        self.on_services_modified(peripheral_uuid).await
                    },
                    CentralDelegateEvent::AdvertisementExtras{peripheral_uuid, extras} => {
                        self.on_advertisement_extras(peripheral_uuid, extras).await
                    },
                    CentralDelegateEvent::DescriptorNotified{
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                        descriptor_uuid,
                        data,
                     } => self.on_descriptor_read(peripheral_uuid, service_uuid, characteristic_uuid, descriptor_uuid, data).await,
                    CentralDelegateEvent::DescriptorWritten{
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                        descriptor_uuid,
                    } => self.on_descriptor_written(peripheral_uuid, service_uuid, characteristic_uuid, descriptor_uuid),
                    CentralDelegateEvent::TxPowerLevel{peripheral_uuid, tx_power_level} => {
                        self.on_tx_power_level(peripheral_uuid, tx_power_level).await
                    },
                    CentralDelegateEvent::DidReadRssi{peripheral_uuid, rssi} => {
                        self.on_read_rssi(peripheral_uuid, rssi).await
                    },
                    CentralDelegateEvent::ReadyToSendWriteWithoutResponse{peripheral_uuid} => {
                        self.drain_write_without_response_queue(peripheral_uuid);
                        self.on_write_readiness(peripheral_uuid).await
                    },
                    CentralDelegateEvent::Advertised{peripheral_uuid, report} => {
                        self.dispatch_event(CoreBluetoothEvent::Advertised {
                            uuid: peripheral_uuid,
                            report,
                        })
                        .await
                    },
                    CentralDelegateEvent::AttributeFailed{
                        peripheral_uuid,
                        service_uuid,
                        characteristic_uuid,
                        descriptor_uuid,
                        stage,
                        error,
                    } => self.on_attribute_failed(peripheral_uuid, service_uuid, characteristic_uuid, descriptor_uuid, stage, error),
                };
            }
            adapter_msg = self.message_receiver.select_next_some() => {
                trace!("Adapter message!");
                match adapter_msg {
                    CoreBluetoothMessage::GetAdapterState { future } => {
                        self.get_adapter_state(future);
                    },
                    CoreBluetoothMessage::StartScanning{filter, future} => self.start_discovery(filter, future),
                    CoreBluetoothMessage::StopScanning => self.stop_discovery(),
                    CoreBluetoothMessage::ConnectDevice{peripheral_uuid, future} => {
                        trace!("got connectdevice msg!");
                        self.connect_peripheral(peripheral_uuid, future).await;
                    }
                    CoreBluetoothMessage::ResolvePeripheral{peripheral_uuid, future} => {
                        self.resolve_peripheral(peripheral_uuid, future).await;
                    }
                    CoreBluetoothMessage::DisconnectDevice{peripheral_uuid, future} => {
                        self.disconnect_peripheral(peripheral_uuid, future);
                    }
                    CoreBluetoothMessage::ReadValue{peripheral_uuid, service_uuid,characteristic_uuid, future} => {
                        self.read_value(peripheral_uuid, service_uuid,characteristic_uuid, future)
                    }
                    CoreBluetoothMessage::WriteValue{
                        peripheral_uuid,service_uuid,
                        characteristic_uuid,
                        data,
                        write_type,
                        future,
                    } => self.write_value(peripheral_uuid, service_uuid,characteristic_uuid, data, write_type, future),
                    CoreBluetoothMessage::Subscribe{peripheral_uuid, service_uuid,characteristic_uuid, future} => {
                        self.subscribe(peripheral_uuid, service_uuid,characteristic_uuid, future)
                    }
                    CoreBluetoothMessage::Unsubscribe{peripheral_uuid, service_uuid,characteristic_uuid, future} => {
                        self.unsubscribe(peripheral_uuid, service_uuid,characteristic_uuid, future)
                    }
                    CoreBluetoothMessage::IsConnected{peripheral_uuid, future} => {
                        self.is_connected(peripheral_uuid, future);
                    },
                    CoreBluetoothMessage::ReadDescriptorValue{peripheral_uuid, service_uuid, characteristic_uuid, descriptor_uuid, future} => {
                        self.read_descriptor_value(peripheral_uuid, service_uuid, characteristic_uuid, descriptor_uuid, future)
                    }
                    CoreBluetoothMessage::WriteDescriptorValue{
                        peripheral_uuid,service_uuid,
                        characteristic_uuid,
                        descriptor_uuid,
                        data,
                        future,
                    } => self.write_descriptor_value(peripheral_uuid, service_uuid, characteristic_uuid, descriptor_uuid, data, future),
                    CoreBluetoothMessage::DiscoverServices{peripheral_uuid, future} => {
                        self.discover_services(peripheral_uuid, future);
                    }
                    CoreBluetoothMessage::ReadRssi{peripheral_uuid, future} => {
                        self.read_rssi(peripheral_uuid, future)
                    }
                    CoreBluetoothMessage::GetWriteLengths{peripheral_uuid, future} => {
                        self.get_write_lengths(peripheral_uuid, future)
                    }
                    CoreBluetoothMessage::GetWriteReadiness{peripheral_uuid, future} => {
                        self.get_write_readiness(peripheral_uuid, future)
                    }
                };
            }
        }
    }

    fn get_adapter_state(&mut self, fut: CoreBluetoothReplyStateShared) {
        let state = unsafe { self.manager.state() };
        fut.lock()
            .unwrap()
            .set_reply(CoreBluetoothReply::AdapterState(state))
    }

    fn start_discovery(&mut self, filter: ScanFilter, fut: CoreBluetoothReplyStateShared) {
        trace!("BluetoothAdapter::start_discovery");
        // UBM patch (UBM_PATCHES.md #8): CoreBluetooth ignores a scan
        // requested while the manager is not powered on; refuse it instead
        // of reporting a scan that never runs.
        let state = unsafe { self.manager.state() };
        if state != CBManagerState::PoweredOn {
            fut.lock()
                .unwrap()
                .set_reply(CoreBluetoothReply::Err(format!(
                    "CoreBluetooth is not powered on ({state:?}); the scan was not started"
                )));
            return;
        }
        // UBM patch (UBM_PATCHES.md #8): `AllowDuplicatesKey` follows the
        // caller's duplicate policy; `None` keeps upstream's `YES`.
        let allow_duplicates = filter.allow_duplicates.unwrap_or(true);
        let service_uuids = scan_filter_to_service_uuids(filter);
        let mut options = NSMutableDictionary::new();
        // NOTE: If duplicates are not allowed then a peripheral will not show
        // up again once connected and then disconnected.
        options.insert_id(
            unsafe { CBCentralManagerScanOptionAllowDuplicatesKey },
            Retained::into_super(Retained::into_super(Retained::into_super(
                NSNumber::new_bool(allow_duplicates),
            ))),
        );
        unsafe {
            self.manager
                .scanForPeripheralsWithServices_options(service_uuids.as_deref(), Some(&options))
        };
        fut.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
    }

    fn stop_discovery(&mut self) {
        trace!("BluetoothAdapter::stop_discovery");
        unsafe { self.manager.stopScan() };
    }
}

/// Convert a `ScanFilter` to the appropriate `NSArray<CBUUID *> *` to use for discovery. If the
/// filter has an empty list of services then this will return `nil`, to discover all devices.
fn scan_filter_to_service_uuids(filter: ScanFilter) -> Option<Retained<NSArray<CBUUID>>> {
    if filter.services.is_empty() {
        None
    } else {
        let service_uuids = filter
            .services
            .into_iter()
            .map(uuid_to_cbuuid)
            .collect::<Vec<_>>();
        Some(NSArray::from_vec(service_uuids))
    }
}

impl Drop for CoreBluetoothInternal {
    fn drop(&mut self) {
        trace!("BluetoothAdapter::drop");
        // NOTE: stop discovery only here instead of in BluetoothDiscoverySession
        self.stop_discovery();
    }
}

pub fn run_corebluetooth_thread(
    event_sender: Sender<CoreBluetoothEvent>,
) -> Result<Sender<CoreBluetoothMessage>, Error> {
    let authorization = unsafe { CBManager::authorization_class() };
    if authorization != CBManagerAuthorization::AllowedAlways
        && authorization != CBManagerAuthorization::NotDetermined
    {
        warn!("Authorization status {:?}", authorization);
        return Err(Error::PermissionDenied);
    } else {
        trace!("Authorization status {:?}", authorization);
    }
    let (sender, receiver) = mpsc::channel::<CoreBluetoothMessage>(256);
    // CoreBluetoothInternal is !Send, so we need to keep it on a single thread.
    thread::spawn(move || {
        let runtime = runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async move {
            let mut cbi = CoreBluetoothInternal::new(receiver, event_sender);
            loop {
                cbi.wait_for_message().await;
            }
        })
    });
    Ok(sender)
}

/// UBM patch #6 tests through btleplug's own CoreBluetooth bookkeeping,
/// with real CoreBluetooth attribute objects (`CBMutableService` /
/// `CBMutableCharacteristic` / `CBMutableDescriptor`, which need no radio):
/// the keying the delegate applies to every discovery answer, the table
/// merge `set_characteristics` runs, and the API view discovery replies
/// with.
#[cfg(test)]
mod ubm_instance_tests {
    use super::{AttrKey, ServiceInternal, api_services, keyed};
    use crate::api::CharPropFlags;
    use objc2::ClassType;
    use objc2::rc::Retained;
    use objc2_core_bluetooth::{
        CBAttributePermissions, CBCharacteristic, CBCharacteristicProperties, CBDescriptor,
        CBMutableCharacteristic, CBMutableDescriptor, CBMutableService, CBService, CBUUID,
    };
    use objc2_foundation::{NSArray, NSString};
    use std::collections::HashMap;

    const HRM_SERVICE: &str = "180D";
    const HRM_MEASUREMENT: &str = "2A37";
    const USER_DESCRIPTION: &str = "2901";

    use crate::api::ReadProvenance;

    /// A device identity CoreBluetooth never knows (fixed, so the run is
    /// deterministic with or without hardware nearby).
    const UNKNOWN_PEER: &str = "5e0b1c9a-6c0f-4f60-a1c1-3b5f2a0e7d11";

    fn headless_internal() -> (
        super::CoreBluetoothInternal,
        futures::channel::mpsc::Receiver<super::CoreBluetoothEvent>,
    ) {
        let (message_sender, message_receiver) =
            futures::channel::mpsc::channel::<super::CoreBluetoothMessage>(8);
        let _ = message_sender;
        let (event_sender, event_receiver) =
            futures::channel::mpsc::channel::<super::CoreBluetoothEvent>(8);
        let internal = super::CoreBluetoothInternal::new(message_receiver, event_sender);
        (internal, event_receiver)
    }

    /// Finding 127 (CoreBluetooth): an identifier CoreBluetooth does not
    /// know reports not-found through the resolve path
    /// (`retrievePeripheralsWithIdentifiers`, as the legacy addon
    /// reconnected without a scan) — never a scan, never a hang.
    #[test]
    fn an_unknown_identifier_resolves_without_a_scan() {
        use super::{CoreBluetoothReply, CoreBluetoothReplyFuture};
        let (mut internal, _events) = headless_internal();
        let uuid = uuid::Uuid::parse_str(UNKNOWN_PEER).expect("fixture uuid");
        let future = CoreBluetoothReplyFuture::default();
        let state = future.get_state_clone();
        futures::executor::block_on(internal.resolve_peripheral(uuid, state));
        assert!(
            matches!(
                futures::executor::block_on(future),
                CoreBluetoothReply::Err(detail) if detail == "Peripheral not found"
            ),
            "an unknown identifier is not-found, not a scan"
        );
        assert!(
            internal.peripherals.is_empty(),
            "nothing unknown is registered"
        );
    }

    /// Finding 127 (CoreBluetooth): a disconnect for a peripheral the
    /// adapter does not hold sends no event and registers nothing — and,
    /// symmetrically, a plain disconnect never clears the table (patch
    /// #19): `on_peripheral_disconnect` keeps the entry so a reconnect
    /// needs no new scan.
    #[test]
    fn a_disconnect_of_an_unknown_peripheral_sends_no_event() {
        let (mut internal, mut events) = headless_internal();
        let uuid = uuid::Uuid::parse_str(UNKNOWN_PEER).expect("fixture uuid");
        futures::executor::block_on(internal.on_peripheral_disconnect(uuid));
        assert!(internal.peripherals.is_empty());
        // Dropping the adapter closes its event sender: an empty, closed
        // channel proves the unknown disconnect published nothing.
        drop(internal);
        assert!(
            matches!(events.try_next(), Ok(None)),
            "an unknown disconnect publishes nothing"
        );
    }

    fn cbuuid(short: &str) -> Retained<CBUUID> {
        unsafe { CBUUID::UUIDWithString(&NSString::from_str(short)) }
    }

    fn uuid_of_service(service: &CBService) -> uuid::Uuid {
        super::cbuuid_to_uuid(&*unsafe { service.UUID() })
    }

    fn uuid_of_characteristic(characteristic: &CBCharacteristic) -> uuid::Uuid {
        super::cbuuid_to_uuid(&*unsafe { characteristic.UUID() })
    }

    fn description() -> Retained<CBDescriptor> {
        Retained::into_super(unsafe {
            CBMutableDescriptor::initWithType_value(
                CBMutableDescriptor::alloc(),
                &cbuuid(USER_DESCRIPTION),
                Some(&NSString::from_str("strap")),
            )
        })
    }

    fn characteristic(properties: CBCharacteristicProperties) -> Retained<CBCharacteristic> {
        let characteristic = unsafe {
            CBMutableCharacteristic::initWithType_properties_value_permissions(
                CBMutableCharacteristic::alloc(),
                &cbuuid(HRM_MEASUREMENT),
                properties,
                None,
                CBAttributePermissions::Readable,
            )
        };
        let descriptors = NSArray::from_vec(vec![description()]);
        unsafe { characteristic.setDescriptors(Some(&descriptors)) };
        Retained::into_super(characteristic)
    }

    fn service() -> Retained<CBService> {
        let service = unsafe {
            CBMutableService::initWithType_primary(
                CBMutableService::alloc(),
                &cbuuid(HRM_SERVICE),
                true,
            )
        };
        let characteristics = NSArray::from_vec(vec![
            characteristic(CBCharacteristicProperties::CBCharacteristicPropertyNotify),
            characteristic(CBCharacteristicProperties::CBCharacteristicPropertyNotify),
        ]);
        unsafe { service.setCharacteristics(Some(&characteristics)) };
        Retained::into_super(service)
    }

    /// The table one peripheral builds from `peripheral.services` and each
    /// service's characteristics, as the delegate and
    /// `set_characteristics` build it.
    fn discovered() -> HashMap<AttrKey, ServiceInternal> {
        let services = NSArray::from_vec(vec![service(), service()]);
        keyed(services, uuid_of_service)
            .into_iter()
            .map(|(key, cbservice)| {
                let characteristics = unsafe { cbservice.characteristics() }
                    .map(|characteristics| keyed(characteristics, uuid_of_characteristic))
                    .unwrap_or_default();
                let mut service = ServiceInternal {
                    cbservice,
                    characteristics: HashMap::new(),
                    discovered: true,
                };
                service.merge_characteristics(characteristics);
                (key, service)
            })
            .collect()
    }

    /// UBM patch #19 (finding 130): at a disconnect every attribute waiter
    /// is answered, descriptor reads and writes included (upstream left
    /// them, and the discovery, waiting forever), as the legacy addon's
    /// `failPendingForDevice` did.
    #[test]
    fn a_disconnect_answers_every_attribute_waiter() {
        use super::{
            CoreBluetoothReply, CoreBluetoothReplyFuture, DescriptorInternal,
            fail_attribute_waiters,
        };
        let mut services = discovered();
        let mut waiters: Vec<CoreBluetoothReplyFuture> = Vec::new();
        let mut waiter = || {
            let future = CoreBluetoothReplyFuture::default();
            let state = future.get_state_clone();
            waiters.push(future);
            state
        };
        let service = services.values_mut().next().expect("service");
        let characteristic = service
            .characteristics
            .values_mut()
            .next()
            .expect("characteristic");
        characteristic.read_future_state.push_front(waiter());
        characteristic.subscribe_future_state.push_front(waiter());
        let mut descriptor = DescriptorInternal::new(description());
        descriptor.read_future_state.push_front(waiter());
        descriptor.write_future_state.push_front(waiter());
        characteristic.descriptors.insert(
            AttrKey {
                uuid: super::cbuuid_to_uuid(&cbuuid(USER_DESCRIPTION)),
                instance: 0,
            },
            descriptor,
        );
        fail_attribute_waiters(&mut services, &CoreBluetoothReply::Err("gone".into()));
        assert_eq!(waiters.len(), 4);
        for future in waiters {
            assert!(matches!(
                futures::executor::block_on(future),
                CoreBluetoothReply::Err(detail) if detail == "gone"
            ));
        }
    }

    #[test]
    fn same_uuid_services_characteristics_and_descriptors_stay_distinct() {
        let services = api_services(&discovered());
        assert_eq!(services.len(), 2, "two same-UUID services survive");
        for (instance, service) in (0u64..).zip(services.iter()) {
            assert_eq!(service.instance, instance, "discovery order");
            assert_eq!(
                service.characteristics.len(),
                2,
                "two identical same-UUID characteristics survive"
            );
            for (position, characteristic) in (0u64..).zip(service.characteristics.iter()) {
                assert_eq!(characteristic.instance, position);
                assert_eq!(characteristic.service_instance, service.instance);
                assert_eq!(characteristic.properties, CharPropFlags::NOTIFY);
                // CoreBluetooth refuses two same-UUID descriptors on one
                // local characteristic; the descriptor keying is proven on
                // its own below.
                assert_eq!(characteristic.descriptors.len(), 1);
                for descriptor in &characteristic.descriptors {
                    assert_eq!(descriptor.characteristic_instance, characteristic.instance);
                    assert_eq!(descriptor.service_instance, service.instance);
                }
            }
        }
    }

    #[test]
    fn same_uuid_descriptors_are_keyed_apart() {
        let descriptors = NSArray::from_vec(vec![description(), description()]);
        let keyed = keyed(descriptors, |descriptor| {
            super::cbuuid_to_uuid(&*unsafe { descriptor.UUID() })
        });
        let mut instances: Vec<u64> = keyed.keys().map(|key| key.instance).collect();
        instances.sort_unstable();
        assert_eq!(instances, vec![0, 1], "both same-UUID descriptors kept");
        assert_eq!(
            keyed
                .keys()
                .map(|key| key.uuid)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            1
        );
    }

    #[test]
    fn a_repeated_discovery_answer_keeps_the_same_instances() {
        let mut table = discovered();
        let (key, service) = table.iter_mut().next().expect("a service");
        let again = unsafe { service.cbservice.characteristics() }
            .map(|characteristics| keyed(characteristics, uuid_of_characteristic))
            .unwrap_or_default();
        service.merge_characteristics(again);
        assert_eq!(service.characteristics.len(), 2, "merged, not duplicated");
        assert!(key.instance < 2);
    }

    fn read_waiter(
        characteristic: &mut super::CharacteristicInternal,
    ) -> super::CoreBluetoothReplyFuture {
        let future = super::CoreBluetoothReplyFuture::default();
        characteristic
            .read_future_state
            .push_front(future.get_state_clone());
        future
    }

    fn read_answer(future: super::CoreBluetoothReplyFuture) -> (Vec<u8>, ReadProvenance) {
        match futures::executor::block_on(future) {
            super::CoreBluetoothReply::CharacteristicRead(value, provenance) => (value, provenance),
            reply => panic!("expected a characteristic read, got {reply:?}"),
        }
    }

    /// UBM patch #14: reads on a characteristic that cannot notify complete
    /// in request order as read responses, and no response leaks into the
    /// notification stream.
    #[test]
    fn queued_reads_complete_in_order_as_read_responses() {
        let mut table = discovered();
        let service = table.values_mut().next().expect("service");
        let characteristic = service
            .characteristics
            .values_mut()
            .next()
            .expect("characteristic");
        let first = read_waiter(characteristic);
        let second = read_waiter(characteristic);
        assert!(!super::answer_value_update(characteristic, &[1]));
        assert!(!super::answer_value_update(characteristic, &[2]));
        assert_eq!(read_answer(first), (vec![1], ReadProvenance::ReadResponse));
        assert_eq!(read_answer(second), (vec![2], ReadProvenance::ReadResponse));
    }

    /// UBM patch #14, the Polar PMD race: while a notification can arrive,
    /// a notification that lands just before the read reply completes the
    /// read as `ReadOrNotification` and still reaches subscribers; the reply
    /// that follows is delivered to subscribers, never dropped.
    #[test]
    fn a_notification_before_the_read_reply_is_reported_ambiguous_and_still_notified() {
        let mut table = discovered();
        let service = table.values_mut().next().expect("service");
        let characteristic = service
            .characteristics
            .values_mut()
            .next()
            .expect("characteristic");
        let subscribe = super::CoreBluetoothReplyFuture::default();
        characteristic
            .subscribe_future_state
            .push_front(subscribe.get_state_clone());
        let read = read_waiter(characteristic);
        assert!(
            super::answer_value_update(characteristic, &[0xAA]),
            "the value that may be a notification reaches subscribers"
        );
        assert_eq!(
            read_answer(read),
            (vec![0xAA], ReadProvenance::ReadOrNotification)
        );
        assert!(
            super::answer_value_update(characteristic, &[0xBB]),
            "the late read reply is delivered as a value, not dropped"
        );
        assert!(characteristic.read_future_state.is_empty());
    }
}
