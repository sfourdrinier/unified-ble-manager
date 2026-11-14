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
    central_delegate::{keyed, AttrKey, AttributeStage, CentralDelegate, CentralDelegateEvent},
    ffi,
    future::{BtlePlugFuture, BtlePlugFutureStateShared},
    utils::{
        core_bluetooth::{cbuuid_to_uuid, uuid_to_cbuuid},
        nsuuid_to_uuid,
    },
};
use crate::api::{CharPropFlags, Characteristic, Descriptor, ScanFilter, Service, WriteType};
use crate::Error;
use futures::channel::mpsc::{self, Receiver, Sender};
use futures::select;
use futures::sink::SinkExt;
use futures::stream::{Fuse, StreamExt};
use log::{error, trace, warn};
use objc2::{msg_send_id, ClassType};
use objc2::{rc::Retained, runtime::AnyObject};
use objc2_core_bluetooth::{
    CBCentralManager, CBCentralManagerScanOptionAllowDuplicatesKey, CBCharacteristic,
    CBCharacteristicProperties, CBCharacteristicWriteType, CBDescriptor, CBManager,
    CBManagerAuthorization, CBManagerState, CBPeripheral, CBPeripheralState, CBService, CBUUID,
};
use objc2_foundation::{NSArray, NSData, NSMutableDictionary, NSNumber};
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap, VecDeque},
    ffi::CString,
    fmt::{self, Debug, Formatter},
    ops::Deref,
    rc::Rc,
    sync::Arc,
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
    fn answer_notification_state(&mut self, notifying: bool) {
        if notifying {
            if let Some(state) = self.subscribe_future_state.pop_back() {
                state.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
            }
        } else if let Some(state) = self.unsubscribe_future_state.pop_back() {
            state.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
        } else if let Some(state) = self.subscribe_future_state.pop_back() {
            fail(state, super::read_notify::enable_not_notifying_error());
        }
    }

    fn fail_notification_state(&mut self, error: crate::PlatformError) {
        if let Some(state) = self
            .subscribe_future_state
            .pop_back()
            .or_else(|| self.unsubscribe_future_state.pop_back())
        {
            fail(state, error);
        }
    }

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
/// UBM patch (UBM_PATCHES.md #17, finding 205): fold one advertisement
/// callback's naming into the advertised-name stash and the sighting's
/// label. A named packet refreshes the stash and the sighting keeps its own
/// name; a nameless packet keeps the stash and the sighting is labelled
/// with it instead of the GAP alias the report wears. Without any
/// advertised name yet, the report keeps its GAP seed.
pub(crate) fn fold_sighting_name(
    stash: Option<String>,
    advertisement_name: Option<String>,
    report_name: Option<String>,
) -> (Option<String>, Option<String>) {
    match advertisement_name {
        Some(advertised) => (Some(advertised), report_name),
        None => (stash.clone(), stash.or(report_name)),
    }
}

#[cfg(test)]
mod ubm_fold_sighting_tests {
    use super::fold_sighting_name;

    fn owned(value: &str) -> Option<String> {
        Some(value.to_owned())
    }

    #[test]
    fn a_named_packet_refreshes_the_stash_and_keeps_its_name() {
        assert_eq!(
            fold_sighting_name(
                owned("Polar H10 SIM0001"),
                owned("Polar H10 SIM0001"),
                owned("Polar H10 SIM0001")
            ),
            (owned("Polar H10 SIM0001"), owned("Polar H10 SIM0001"))
        );
    }

    #[test]
    fn a_renamed_packet_replaces_the_stash() {
        assert_eq!(
            fold_sighting_name(
                owned("Polar H10 SIM0043"),
                owned("Polar H10 SIM0044"),
                owned("Polar H10 SIM0044"),
            ),
            (owned("Polar H10 SIM0044"), owned("Polar H10 SIM0044"))
        );
    }

    #[test]
    fn a_nameless_packet_is_labelled_with_the_stash_despite_the_gap_name() {
        // Finding 205: the report wears the GAP alias for a nameless
        // packet; the sighting must carry the advertised identity instead.
        assert_eq!(
            fold_sighting_name(owned("Polar H10 SIM0001"), None, owned("lx5090")),
            (owned("Polar H10 SIM0001"), owned("Polar H10 SIM0001"))
        );
    }

    #[test]
    fn a_nameless_packet_without_a_report_name_is_labelled_from_the_stash() {
        assert_eq!(
            fold_sighting_name(owned("Polar H10 SIM0001"), None, None),
            (owned("Polar H10 SIM0001"), owned("Polar H10 SIM0001"))
        );
    }

    #[test]
    fn without_any_advertised_name_the_gap_seed_stands() {
        assert_eq!(
            fold_sighting_name(None, None, owned("lx5090")),
            (None, owned("lx5090"))
        );
        assert_eq!(fold_sighting_name(None, None, None), (None, None));
    }
}

fn answer_value_update(characteristic: &mut CharacteristicInternal, data: &[u8]) -> bool {
    let route = characteristic.read_notify_state().route_value();
    if let Some(provenance) = route.read {
        if let Some(state) = characteristic.read_future_state.pop_back() {
            state
                .lock()
                .unwrap()
                .set_reply(CoreBluetoothReply::CharacteristicRead(
                    data.to_vec(),
                    provenance,
                ));
        }
    }
    route.notification
}

fn invalid_activation_error() -> crate::PlatformError {
    crate::PlatformError::new(
        "corebluetooth",
        "notification-owner-invalidated",
        "The native characteristic owner is no longer valid",
    )
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

#[derive(Debug)]
pub(super) struct NativeActivationIdentity {
    pub(super) id: u64,
    pub(super) ownership: super::notification_ownership::NotificationOwnership,
}

#[derive(Debug)]
pub(crate) struct NativeActivationToken {
    pub(super) identity: Arc<NativeActivationIdentity>,
    pub(super) revision: u64,
    pub(super) claim_id: u64,
}

struct NativeActivationEntry {
    identity: std::sync::Weak<NativeActivationIdentity>,
    characteristic: Retained<CBCharacteristic>,
}

static NEXT_NATIVE_CLAIM_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static NEXT_NATIVE_ACTIVATION_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn acquire_native_activation(
    entries: &mut HashMap<u64, NativeActivationEntry>,
    next_id: &std::sync::atomic::AtomicU64,
    object: Retained<CBCharacteristic>,
    revision: u64,
) -> Result<Arc<NativeActivationToken>, crate::PlatformError> {
    let claim_id = NEXT_NATIVE_CLAIM_ID
        .fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |id| id.checked_add(1),
        )
        .map_err(|_| {
            crate::PlatformError::new(
                "corebluetooth",
                "notification-owner-capacity",
                "Notification claim identity exhausted",
            )
        })?;
    entries.retain(|_, owner| owner.identity.strong_count() != 0);
    if let Some(identity) = entries.values().find_map(|owner| {
        std::ptr::eq(&*owner.characteristic, &*object)
            .then(|| owner.identity.upgrade())
            .flatten()
    }) {
        return Ok(Arc::new(NativeActivationToken {
            identity,
            revision,
            claim_id,
        }));
    }
    if entries.len() >= 4096 {
        return Err(crate::PlatformError::new(
            "corebluetooth",
            "notification-owner-capacity",
            "Notification owner capacity is full",
        ));
    }
    let id = next_id
        .fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |id| id.checked_add(1),
        )
        .map_err(|_| {
            crate::PlatformError::new(
                "corebluetooth",
                "notification-owner-capacity",
                "Notification owner identity exhausted",
            )
        })?;
    let identity = Arc::new(NativeActivationIdentity {
        id,
        ownership: Default::default(),
    });
    let token = Arc::new(NativeActivationToken {
        identity: identity.clone(),
        revision,
        claim_id,
    });
    entries.insert(
        id,
        NativeActivationEntry {
            identity: Arc::downgrade(&identity),
            characteristic: object,
        },
    );
    Ok(token)
}

fn resolve_native_activation(
    entries: &HashMap<u64, NativeActivationEntry>,
    services: &HashMap<AttrKey, ServiceInternal>,
    token: &NativeActivationToken,
    ready: bool,
    revision: u64,
) -> Result<((AttrKey, AttrKey), Retained<CBCharacteristic>), crate::PlatformError> {
    if !ready || revision != token.revision {
        return Err(invalid_activation_error());
    }
    let entry = entries
        .get(&token.identity.id)
        .ok_or_else(invalid_activation_error)?;
    if !entry
        .identity
        .upgrade()
        .is_some_and(|identity| Arc::ptr_eq(&identity, &token.identity))
    {
        return Err(invalid_activation_error());
    }
    let route = services
        .iter()
        .find_map(|(service_key, service)| {
            service
                .characteristics
                .iter()
                .find_map(|(characteristic_key, characteristic)| {
                    std::ptr::eq(&*characteristic.characteristic, &*entry.characteristic)
                        .then_some((*service_key, *characteristic_key))
                })
        })
        .ok_or_else(invalid_activation_error)?;
    Ok((route, entry.characteristic.clone()))
}

#[derive(Clone, Debug)]
pub enum CoreBluetoothReply {
    DirectoryPeers(Vec<(Uuid, Option<String>)>),
    NotificationOwner(Arc<NativeActivationToken>),
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
    NotificationFailed(AttrKey, AttrKey, crate::PlatformError),
    NotificationSourceRetired(AttrKey, AttrKey, tokio::sync::oneshot::Sender<()>),
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

pub(super) async fn enqueue_directory_event(
    sender: &mpsc::Sender<CoreBluetoothEvent>,
    event: CoreBluetoothEvent,
    reply: &CoreBluetoothReplyStateShared,
) -> Result<(), crate::PlatformError> {
    if sender.clone().send(event).await.is_err() {
        let error = crate::PlatformError::new(
            "corebluetooth",
            "directory-queue-closed",
            "directory adapter event receiver is closed",
        );
        reply
            .lock()
            .unwrap()
            .set_reply(CoreBluetoothReply::Failed(error.clone()));
        Err(error)
    } else {
        Ok(())
    }
}

struct ServiceInternal {
    cbservice: Retained<CBService>,
    characteristics: HashMap<AttrKey, CharacteristicInternal>,
    pub discovered: bool,
    included_services: Option<Vec<AttrKey>>,
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
            primary: Some(unsafe { service.cbservice.isPrimary() }),
            included_services: service.included_services.as_ref().map(|references| {
                references
                    .iter()
                    .map(|reference| crate::api::IncludedService {
                        uuid: reference.uuid,
                        instance: reference.instance,
                    })
                    .collect()
            }),
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

pub(super) fn retained_error_bytes(error: &crate::PlatformError) -> usize {
    let mut bytes = error.code.capacity().saturating_add(error.message.capacity())
        .saturating_add(error.metadata.capacity().saturating_mul(std::mem::size_of::<(&'static str, String)>()));
    for (_, value) in &error.metadata { bytes = bytes.saturating_add(value.capacity()); }
    bytes
}

/// Owned callback payloads retained while the graph is held. The actor handoff
/// must consume each replay result before requesting the next item.
pub(super) struct CutReplies(Vec<super::peer_waiters::PreparedReply>);

/// The graph and waiters selected at adapter-callback admission. Source
/// delivery precedes reply wakes, including when the transition is dropped.
pub(super) struct AdapterInvalidation {
    source: Option<crate::common::util::SourceValueReservation>,
    replies: CutReplies,
}

impl AdapterInvalidation {
    pub(super) fn complete(mut self) {
        if let Some(source) = self.source.take() { source.release(); }
        self.replies.complete();
    }
}

impl CutReplies {
    pub(super) fn complete(mut self) {
        for reply in self.0.drain(..) { super::peer_waiters::complete_reply(reply); }
    }
    fn is_empty(&self) -> bool { self.0.is_empty() }
}

impl Drop for CutReplies {
    fn drop(&mut self) {
        for reply in self.0.drain(..) { super::peer_waiters::complete_reply(reply); }
    }
}

/// An admitted read still owns its waiter if its queued callback is retired.
/// The source reservation precedes this field so its uncertain terminal is
/// recovered before this guard wakes the waiter on drop.
type RetiredReplies = Arc<std::sync::Mutex<Option<Vec<super::peer_waiters::PreparedReply>>>>;

fn recover_reply(recovery: &RetiredReplies, reply: super::peer_waiters::PreparedReply) {
    let mut pending = recovery.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(replies) = pending.as_mut() { replies.push(reply); return; }
    drop(pending);
    super::peer_waiters::complete_reply(reply);
}

pub(super) struct CapturedRead {
    read: Option<(CoreBluetoothReplyStateShared, crate::api::ReadProvenance)>,
    recovery: RetiredReplies,
}

impl Drop for CapturedRead {
    fn drop(&mut self) {
        if let Some((waiter, _)) = self.read.take() {
            recover_reply(&self.recovery, (waiter, CoreBluetoothReply::Failed(crate::PlatformError::new(
                "corebluetooth", "callback-owner-retired", "An admitted read callback was retired before delivery",
            ))));
        }
    }
}

pub(super) struct CapturedReply {
    reply: Option<super::peer_waiters::PreparedReply>,
    recovery: RetiredReplies,
}

impl CapturedReply {
    fn complete(mut self) {
        if let Some(reply) = self.reply.take() { super::peer_waiters::complete_reply(reply); }
    }
    fn is_some(&self) -> bool { self.reply.is_some() }
}

impl Drop for CapturedReply {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() { recover_reply(&self.recovery, reply); }
    }
}

pub(super) struct LegacyFailure {
    destination: Option<(Arc<crate::common::util::NotificationFaults>, tokio::sync::broadcast::Sender<crate::common::util::NotificationEnvelope>)>,
    routes: Vec<(AttrKey, AttrKey)>,
    error: crate::PlatformError,
}

impl LegacyFailure {
    fn publish(&mut self) {
        if let Some((faults, sender)) = self.destination.take() {
            for route in self.routes.drain(..) {
                faults.publish(&sender, crate::api::ValueNotification {
                    uuid: route.1.uuid, instance: route.1.instance,
                    service_uuid: route.0.uuid, service_instance: route.0.instance,
                    value: Vec::new(), source_failure: Some(self.error.clone()), lost_before: 0,
                });
            }
        }
    }
}

impl Drop for LegacyFailure {
    fn drop(&mut self) { self.publish(); }
}

pub(super) enum DeferredPeerCallback {
    CapturedNotification { route: (AttrKey, AttrKey), data: Vec<u8>, reservation: Option<crate::common::util::SourceValueReservation>, read: CapturedRead, notification: bool },
    CapturedValueError { route: (AttrKey, AttrKey), error: crate::PlatformError, reservation: Option<crate::common::util::SourceValueReservation>, read: CapturedReply },
    CapturedNotify { completion: CapturedReply, actor: Option<super::central_delegate::PeerDelegateCallback> },
    Invalidate { source: Option<crate::common::util::SourceValueReservation>, replies: CutReplies },
    SourceFailure { source: Option<crate::common::util::SourceValueReservation>, replies: CutReplies, legacy: LegacyFailure, error: crate::PlatformError, report_overflow: bool },
    RootDiscovery(u64),
    Lifecycle { event: super::central_delegate::PeerLifecycle, source: Option<crate::common::util::SourceValueReservation>, replies: CutReplies },
    Delegate(super::central_delegate::PeerDelegateCallback),
    Actor(super::central_delegate::PeerActorCallback),
    AdapterBarrier,
}

pub(crate) struct AdapterEventTicket { pending: Arc<std::sync::atomic::AtomicUsize> }
impl Debug for AdapterEventTicket {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result { f.debug_struct("AdapterEventTicket").finish_non_exhaustive() }
}
impl Drop for AdapterEventTicket {
    fn drop(&mut self) { self.pending.fetch_sub(1, std::sync::atomic::Ordering::AcqRel); }
}

struct MarkerOperationGuard { owner: Arc<PeerSourceOwner>, dispatched: bool, settled: bool }
impl Drop for MarkerOperationGuard {
    fn drop(&mut self) {
        let mut state = self.owner.waiters.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reclaim_operations -= 1;
        if self.dispatched && !self.settled { state.reclaim_uncertain = true; }
    }
}

pub(crate) struct PeerCallbackTicket { owner: Arc<PeerSourceOwner> }
impl Debug for PeerCallbackTicket {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result { f.debug_struct("PeerCallbackTicket").finish_non_exhaustive() }
}
impl Drop for PeerCallbackTicket {
    fn drop(&mut self) {
        let mut state = self.owner.waiters.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.callback_tickets -= 1;
    }
}

struct ReclaimDrainGuard<'a> { owner: &'a PeerSourceOwner }
impl Drop for ReclaimDrainGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.owner.waiters.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reclaim_drain_active = false;
        if std::thread::panicking() {
            state.reclaim_uncertain = true;
            if let Some(gate) = state.reclaim_gate.as_mut() { gate.fail_tail(); }
        }
    }
}

pub(crate) struct PeerSourceOwner {
    retired_replies: RetiredReplies,
    pub(super) adapter_admission: Arc<std::sync::Mutex<()>>,
    pub(super) adapter_pending: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) authorities: Arc<std::sync::Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
    pub(super) graph_attachment: Option<u64>,
    pub(super) faults: crate::common::util::NotificationFaults,
    pub(super) waiters: std::sync::Mutex<super::peer_waiters::PeerWaiters>,
    pub(super) legacy: std::sync::Mutex<
        Option<(
            Arc<crate::common::util::NotificationFaults>,
            tokio::sync::broadcast::Sender<crate::common::util::NotificationEnvelope>,
        )>,
    >,
}

impl Drop for PeerSourceOwner {
    fn drop(&mut self) {
        // Drop queued effects without holding the peer mutex: their recovery
        // can wake application tasks and release held ingress reservations.
        let state = self.waiters.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        let gate = state.reclaim_gate.take();
        let generation = state.generation.take();
        let remaining = state.prepare_invalidation(crate::PlatformError::new("corebluetooth", "callback-owner-retired", "The native peer owner was retired"));
        *self.retired_replies.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Vec::new());
        if let Some(generation) = generation {
            if let Err(error) = self.faults.close_source_generation(generation) {
                log::error!("CoreBluetooth could not close retired source generation {generation}: {error:?}");
            }
        }
        drop(gate);
        let replies = self.retired_replies.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take().unwrap_or_default();
        for reply in replies.into_iter().chain(remaining) { super::peer_waiters::complete_reply(reply); }
    }
}

impl PeerSourceOwner {
    pub(super) fn graph_snapshot(&self) -> (u64, Option<u64>, bool) {
        let state = self.waiters.lock().unwrap();
        (state.revision, state.generation, state.ready)
    }

    pub(super) fn capture_adapter_invalidation(&self) -> Result<AdapterInvalidation, crate::common::util::SourceGroupError> {
        let _wake_scope = self.faults.defer_wakes();
        let mut state = self.waiters.lock().unwrap();
        let error = crate::PlatformError::new("corebluetooth", "adapter-unavailable", "CoreBluetooth adapter became unavailable");
        let (replies, source) = self.prepare_invalidation_locked(&mut state, error, usize::MAX)?;
        Ok(AdapterInvalidation { source, replies })
    }

    fn capture_source_failure_locked(&self, state: &mut super::peer_waiters::PeerWaiters, error: crate::PlatformError, route: Option<(AttrKey, AttrKey)>, report_overflow: bool) -> DeferredPeerCallback {
        let legacy = self.legacy.lock().unwrap().clone();
        let mut routes: Vec<_> = state.routes().collect();
        if let Some(route) = route { if !routes.contains(&route) { routes.push(route); } }
        let source = match state.generation.take() {
            Some(generation) => match self.faults.reserve_failure_source_generation(generation, error.clone()) {
                Ok(source) => Some(source),
                Err(reason) => {
                    state.reclaim_uncertain = true;
                    log::error!("CoreBluetooth could not reserve source failure for generation {generation}: {reason:?}");
                    None
                }
            },
            None => None,
        };
        state.reclaim_binding.disconnect();
        state.ready = false;
        if let Some(next) = state.revision.checked_add(1) { state.revision = next; }
        else { state.revision_exhausted = true; }
        let replies = CutReplies(state.prepare_invalidation(error.clone()));
        let legacy = LegacyFailure { destination: legacy, routes, error: error.clone() };
        DeferredPeerCallback::SourceFailure { replies, source, legacy, error, report_overflow }
    }

    fn capture_overflow_locked(&self, state: &mut super::peer_waiters::PeerWaiters) {
        if state.overflow_cuts_queued != 0 && !state.ready && state.generation.is_none()
            && state.invalidation_waiter_count() == 0 && state.reclaim_reservation_failure.is_none() { return; }
        let error = if let Some(reason) = state.reclaim_reservation_failure.take() {
            crate::PlatformError::new("corebluetooth", "reclaim-callback-reservation-failed", "Could not reserve a reclaim callback; its tail is uncertain")
                .with("sourceError", format!("{reason:?}"))
        } else {
            crate::PlatformError::new("corebluetooth", "reclaim-callback-overflow", "Reclaim callback tail is uncertain")
        };
        let cut = self.capture_source_failure_locked(state, error, None, true);
        state.overflow_cuts_queued += 1;
        state.reclaim_gate.as_mut().expect("overflow requires a gate")
            .admit_mandatory_cut(crate::ubm_reclaim_graph_gate::MandatoryCut::new(cut));
    }

    fn execute_source_failure(effect: DeferredPeerCallback) -> Option<crate::PlatformError> {
        let DeferredPeerCallback::SourceFailure { replies, source, mut legacy, error, report_overflow } = effect else { unreachable!() };
        if let Some(source) = source { source.release(); }
        replies.complete();
        legacy.publish();
        report_overflow.then_some(error)
    }

    fn prepare_invalidation_locked(&self, state: &mut super::peer_waiters::PeerWaiters, error: crate::PlatformError, source_budget: usize) -> Result<(CutReplies, Option<crate::common::util::SourceValueReservation>), crate::common::util::SourceGroupError> {
        let source = state.generation.map(|generation| self.faults.reserve_close_source_generation_bounded(generation, source_budget)).transpose()?;
        state.reclaim_binding.disconnect();
        state.ready = false;
        if let Some(next) = state.revision.checked_add(1) { state.revision = next; }
        else { state.revision_exhausted = true; }
        state.generation = None;
        Ok((CutReplies(state.prepare_invalidation(error)), source))
    }

    pub(super) fn begin_callback(self: &Arc<Self>) -> Option<PeerCallbackTicket> {
        let mut state = self.waiters.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.callback_tickets >= 512 { state.reclaim_uncertain = true; return None; }
        state.callback_tickets += 1;
        Some(PeerCallbackTicket { owner: self.clone() })
    }

    fn reserve_marker_operation(self: &Arc<Self>) -> Result<MarkerOperationGuard, crate::common::util::SourceGroupError> {
        use crate::common::util::SourceGroupError;
        let _adapter = self.adapter_admission.lock().map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        let mut state = self.waiters.lock().map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        if self.adapter_pending.load(std::sync::atomic::Ordering::Acquire) != 0 || state.reclaim_held
            || state.reclaim_gate.as_ref().is_some_and(|gate| gate.is_busy()) || state.reclaim_operations >= 256 {
            return Err(SourceGroupError::SourceStateUnavailable);
        }
        state.reclaim_operations += 1;
        Ok(MarkerOperationGuard { owner: self.clone(), dispatched: false, settled: false })
    }

    pub(super) fn start_marker_transaction<T>(
        &self,
        group: &crate::common::util::SourceGroupHandle,
        expected: &(String, u64, u64),
        request: crate::common::util::SourceWriteRequest,
        initiate: impl FnOnce() -> std::result::Result<T, crate::PlatformError>,
    ) -> std::result::Result<
        crate::common::util::SourceWriteStart<T>,
        crate::common::util::SourceGroupError,
    > {
        use crate::common::util::{SourceGroupError, SourceWriteStart, SourceWriteStatus};
        let _wake_scope = self.faults.defer_wakes();
        if let Some(receipt) = self.faults.replay_source_marker_write(group, &request)? {
            if !matches!(receipt.status(), SourceWriteStatus::Pending) { return Ok(SourceWriteStart::Replay(receipt)); }
        }
        let _adapter = self.adapter_admission.lock().map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        let state = self
            .waiters
            .lock()
            .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        if self.adapter_pending.load(std::sync::atomic::Ordering::Acquire) != 0
            || state.reclaim_held || state.reclaim_gate.as_ref().is_some_and(|gate| gate.is_busy()) {
            return Err(SourceGroupError::SourceStateUnavailable);
        }
        if !state.ready
            || state.revision_exhausted
            || expected.0 != "corebluetooth"
            || self.graph_attachment != Some(expected.1)
            || state.revision != expected.2
            || request.graph != crate::common::util::marker_graph_identity(expected)
        {
            return Err(SourceGroupError::StaleGeneration);
        }
        self.faults
            .start_reserved_source_marker_write(group, request, initiate)
    }
    pub(super) fn with_waiters<R>(
        &self,
        f: impl FnOnce(&mut super::peer_waiters::PeerWaiters) -> R,
    ) -> R {
        let mut state = self.waiters.lock().unwrap();
        let result = f(&mut state);
        let wakers = state.take_wakers();
        drop(state);
        for waker in wakers {
            waker.wake();
        }
        result
    }
    pub(super) fn new() -> Arc<Self> {
        Self::with_authorities(Arc::default(), Arc::default(), Arc::default(), None)
    }

    fn with_authorities(
        authorities: Arc<std::sync::Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
        adapter_admission: Arc<std::sync::Mutex<()>>,
        adapter_pending: Arc<std::sync::atomic::AtomicUsize>,
        failure_evidence_store: Option<Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>>,
    ) -> Arc<Self> {
        static NEXT_GRAPH_ATTACHMENT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        let graph_attachment = NEXT_GRAPH_ATTACHMENT
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |value| value.checked_add(1),
            )
            .ok();
        Arc::new(Self {
            retired_replies: Arc::default(),
            adapter_admission,
            adapter_pending,
            authorities,
            graph_attachment,
            faults: crate::common::util::NotificationFaults::new_with_failure_evidence_store(
                crate::ubm::EVENT_CAPACITY,
                failure_evidence_store,
            ),
            waiters: std::sync::Mutex::new(super::peer_waiters::PeerWaiters::default()),
            legacy: std::sync::Mutex::new(None),
        })
    }

    /// Classify and reserve the value before the delegate hands it to the actor.
    pub(super) fn value(&self, route: (AttrKey, AttrKey), notifying: bool, data: &[u8]) -> bool {
        self.apply_value(route, notifying, data, false)
    }

    fn apply_value(&self, route: (AttrKey, AttrKey), notifying: bool, data: &[u8], replay: bool) -> bool {
        let _wake_scope = self.faults.defer_wakes();
        use crate::ubm_peer_notification_ingress::CallbackProvenance;
        let mut state = self.waiters.lock().unwrap();
        if !replay {
            // Queued effects already own their callback-time waiters and
            // memberships; later callbacks may reserve behind them.
            let bytes = std::mem::size_of::<DeferredPeerCallback>().saturating_add(data.len());
            let can_capture = state.reclaim_gate.as_ref().is_some_and(|gate|
                    gate.can_reserve_before_admit(bytes));
            if can_capture {
                let retained = data.to_vec();
                let retained_bytes = std::mem::size_of::<DeferredPeerCallback>().saturating_add(retained.capacity().max(data.len()));
                // Vec's allocation may exceed its requested length; recheck
                // before admission while the peer lock still fences the gate.
                if state.reclaim_gate.as_ref().is_some_and(|gate| gate.can_reserve_before_admit(retained_bytes)) {
                    let decision = state.classify_value(route, notifying);
                    let reservation = if let Some(generation) = state.generation.filter(|_| decision.notification) {
                        let value = crate::api::ValueNotification {
                            uuid: route.1.uuid, instance: route.1.instance,
                            service_uuid: route.0.uuid, service_instance: route.0.instance,
                            value: retained.clone(), source_failure: None, lost_before: 0,
                        };
                        let provenance = if decision.read.is_some() { CallbackProvenance::ReadOrNotification } else if notifying { CallbackProvenance::Notification } else { CallbackProvenance::Unknown };
                        let source_budget = state.reclaim_gate.as_ref().unwrap().remaining_bytes().saturating_sub(retained_bytes);
                        match self.faults.reserve_source_value_bounded(generation, &value, provenance, source_budget) {
                            Ok(reservation) => Some(reservation),
                            Err(error) => {
                                // The gate retains the ordered overflow marker;
                                // drain reports the source's uncertain tail.
                                state.reclaim_reservation_failure = Some(error);
                                state.reclaim_gate.as_mut().unwrap().fail_tail();
                                self.capture_overflow_locked(&mut state);
                                return false;
                            }
                        }
                    } else { None };
                    let retained_bytes = retained_bytes.saturating_add(reservation.as_ref().map_or(0, |reservation| reservation.retained_bytes()));
                    let (prepared_decision, read) = state.prepare_value(route, notifying);
                    debug_assert_eq!(decision, prepared_decision);
                    let exhausted = reservation.as_ref().is_some_and(|reservation| reservation.ordinal_exhausted());
                    let callback = DeferredPeerCallback::CapturedNotification { route, data: retained, reservation, read: CapturedRead { read, recovery: self.retired_replies.clone() }, notification: decision.notification };
                    match state.reclaim_gate.as_mut().unwrap().admit(callback, retained_bytes, false) {
                        crate::ubm_reclaim_graph_gate::Admission::Deferred => {
                            if exhausted { state.reclaim_reservation_failure = Some(crate::common::util::SourceGroupError::OrdinalExhausted); state.reclaim_gate.as_mut().unwrap().fail_tail(); self.capture_overflow_locked(&mut state); }
                            return false;
                        },
                        _ => unreachable!("gate preflight and admission share the peer lock"),
                    }
                }
            }
            if let Some(gate) = state.reclaim_gate.as_mut().filter(|gate| gate.is_busy()) {
                gate.fail_tail();
                self.capture_overflow_locked(&mut state);
                return false;
            }
        }
        let decision = state.value(route, notifying, data);
        if decision.notification {
            if let Some(generation) = state.generation {
                let provenance = if decision.read.is_some() {
                    CallbackProvenance::ReadOrNotification
                } else if notifying {
                    CallbackProvenance::Notification
                } else {
                    CallbackProvenance::Unknown
                };
                let value = crate::api::ValueNotification {
                    uuid: route.1.uuid,
                    instance: route.1.instance,
                    service_uuid: route.0.uuid,
                    service_instance: route.0.instance,
                    value: data.to_vec(),
                    source_failure: None,
                    lost_before: 0,
                };
                let _ = self.faults.publish_source_event_with_provenance(
                    generation,
                    crate::api::NotificationEvent::Value(value),
                    provenance,
                );
            }
        }
        let wakers = state.take_wakers();
        drop(state);
        for waker in wakers {
            waker.wake();
        }
        decision.notification
    }

    pub(super) fn invalidate(&self, error: crate::PlatformError) {
        self.apply_invalidation(error, false);
    }

    pub(super) fn apply_invalidation(&self, error: crate::PlatformError, replay: bool) {
        self.apply_invalidation_matching(error, replay, None);
    }

    pub(super) fn apply_invalidation_matching(
        &self,
        error: crate::PlatformError,
        replay: bool,
        snapshot: Option<(u64, Option<u64>, bool)>,
    ) {
        let _wake_scope = self.faults.defer_wakes();
        let mut state = self.waiters.lock().unwrap();
        if snapshot.is_some_and(|snapshot| snapshot != (state.revision, state.generation, state.ready)) {
            return;
        }
        if !replay {
            if state.reclaim_gate.as_ref().is_some_and(|gate| gate.is_busy()) {
                let count = state.invalidation_waiter_count();
                if state.empty_invalidation_admitted && !state.ready && state.generation.is_none() && count == 0 {
                    return;
                }
                let bytes = std::mem::size_of::<DeferredPeerCallback>().saturating_add(count.saturating_mul(retained_error_bytes(&error).saturating_add(std::mem::size_of::<super::peer_waiters::PreparedReply>())));
                let ordinary = state.reclaim_gate.as_ref().unwrap().can_reserve_before_admit(bytes);
                let (replies, source) = match self.prepare_invalidation_locked(&mut state, error, usize::MAX) {
                    Ok(effect) => effect,
                    Err(error) => {
                        state.reclaim_reservation_failure = Some(error);
                        state.reclaim_gate.as_mut().unwrap().fail_tail();
                        return;
                    }
                };
                let bytes = bytes.saturating_add(source.as_ref().map_or(0, |reservation| reservation.retained_bytes()));
                state.empty_invalidation_admitted = true;
                let gate = state.reclaim_gate.as_mut().unwrap();
                let effect = DeferredPeerCallback::Invalidate { replies, source };
                if ordinary && gate.can_reserve_before_admit(bytes) {
                    assert!(matches!(gate.admit(effect, bytes, true), crate::ubm_reclaim_graph_gate::Admission::Deferred));
                } else {
                    gate.admit_mandatory_cut(crate::ubm_reclaim_graph_gate::MandatoryCut::new(effect));
                }
                return;
            }
        }
        state.reclaim_binding.disconnect();
        state.ready = false;
        if let Some(next) = state.revision.checked_add(1) {
            state.revision = next;
        } else {
            state.revision_exhausted = true;
        }
        if let Some(generation) = state.generation.take() {
            let _ = self.faults.close_source_generation(generation);
        }
        state.invalidate(error);
        let wakers = state.take_wakers();
        drop(state);
        for waker in wakers {
            waker.wake();
        }
    }

    pub(super) fn fail_adapter_tail(&self) {
        let mut state = self.waiters.lock().unwrap();
        if let Some(gate) = state.reclaim_gate.as_mut() { gate.fail_tail(); }
    }

    pub(super) fn finish_adapter_drain(&self) {
        self.waiters.lock().unwrap().reclaim_held = false;
    }

    pub(super) fn adapter_barrier_id_seen(&self, id: u64) -> bool {
        self.waiters.lock().unwrap().reclaim_gate.as_ref().is_some_and(|gate| gate.barrier_id_seen(id))
    }

    pub(super) fn admit_adapter_barrier(&self, id: u64) -> crate::ubm_reclaim_graph_gate::BarrierAdmission<DeferredPeerCallback> {
        let mut state = self.waiters.lock().unwrap();
        let gate = state.reclaim_gate.get_or_insert_with(|| crate::ubm_reclaim_graph_gate::GraphGate::new_with_barriers(256, 524_288, 64, 4096));
        gate.admit_barrier(id, DeferredPeerCallback::AdapterBarrier, std::mem::size_of::<DeferredPeerCallback>())
    }

    pub(super) fn mark_adapter_tail_reported(&self) {
        self.waiters.lock().unwrap().adapter_tail_reported = true;
    }

    pub(super) fn resume_adapter_overflow(&self) -> Result<(), crate::ubm_reclaim_graph_gate::GateError> {
        let mut state = self.waiters.lock().unwrap();
        if !state.adapter_overflow_paused { return Err(crate::ubm_reclaim_graph_gate::GateError::NotCurrent); }
        state.adapter_overflow_paused = false;
        Ok(())
    }

    pub(super) fn resume_adapter_barrier(&self, id: u64) -> Result<(), crate::ubm_reclaim_graph_gate::GateError> {
        self.waiters.lock().unwrap().reclaim_gate.as_mut().ok_or(crate::ubm_reclaim_graph_gate::GateError::NotCurrent)?.resume_barrier(id)
    }

    pub(super) fn admit_actor_callback(&self, callback: super::central_delegate::PeerActorCallback) -> crate::ubm_reclaim_graph_gate::Admission<super::central_delegate::PeerActorCallback> {
        use crate::ubm_reclaim_graph_gate::Admission;
        use super::central_delegate::PeerActorCallback;
        let _wake_scope = self.faults.defer_wakes();
        let bytes = match &callback {
            PeerActorCallback::DescriptorNotified { data, .. } => data.capacity(),
            PeerActorCallback::AttributeFailed { error, .. } => retained_error_bytes(error),
            _ => 0,
        };
        let mut state = self.waiters.lock().unwrap();
        let Some(gate) = state.reclaim_gate.as_mut().filter(|gate| gate.is_busy()) else { return Admission::Apply(callback); };
        match gate.admit(DeferredPeerCallback::Actor(callback), bytes, false) {
            Admission::Apply(DeferredPeerCallback::Actor(callback)) => Admission::Apply(callback),
            Admission::Deferred => Admission::Deferred,
            Admission::Overflow(DeferredPeerCallback::Actor(callback)) => {
                self.capture_overflow_locked(&mut state);
                Admission::Overflow(callback)
            },
            _ => unreachable!("admission returns its input"),
        }
    }

    #[cfg(test)]
    pub(super) fn admit_delegate_callback(&self, callback: super::central_delegate::PeerDelegateCallback) -> bool {
        matches!(self.prepare_delegate_callback(callback), Ok(None))
    }

    /// Select callback-time ownership and decide whether to defer while the
    /// same peer lock fences reclaim admission and waiter registration.
    pub(super) fn prepare_delegate_callback(&self, callback: super::central_delegate::PeerDelegateCallback) -> Result<Option<DeferredPeerCallback>, crate::common::util::SourceGroupError> {
        use super::central_delegate::PeerDelegateCallback;
        let _wake_scope = self.faults.defer_wakes();
        let mut bytes = std::mem::size_of::<DeferredPeerCallback>().saturating_add(match &callback {
            PeerDelegateCallback::ValueError { error, .. }
            | PeerDelegateCallback::CapturedValueError { error, .. }
            | PeerDelegateCallback::NotificationError { error, .. } => retained_error_bytes(error),
            PeerDelegateCallback::NotificationState { .. } | PeerDelegateCallback::CapturedNotificationState { .. } => 0,
        });
        let mut state = self.waiters.lock().unwrap();
        // A value error is cloned only when it also answers a captured read.
        if let PeerDelegateCallback::ValueError { route, notifying, error } = &callback {
            if state.classify_value(*route, *notifying).read.is_some() {
                bytes = bytes.saturating_add(retained_error_bytes(error));
            }
        }
        let held = state.reclaim_gate.as_ref().is_some_and(|gate| gate.is_busy());
        if held && !state.reclaim_gate.as_ref().unwrap().can_reserve_before_admit(bytes) {
            state.reclaim_gate.as_mut().unwrap().fail_tail();
            self.capture_overflow_locked(&mut state);
            return Ok(None);
        }
        let captured = match callback {
            PeerDelegateCallback::ValueError { route, notifying, error } => {
                let possible = state.notification_possible(route, notifying);
                let has_read = state.classify_value(route, notifying).read.is_some();
                let reservation = if let Some(generation) = state.generation.filter(|_| !has_read || possible) {
                    let value = crate::api::ValueNotification { uuid: route.1.uuid, instance: route.1.instance,
                        service_uuid: route.0.uuid, service_instance: route.0.instance, value: Vec::new(),
                        source_failure: Some(error.clone()), lost_before: 0 };
                    let budget = if held { state.reclaim_gate.as_ref().unwrap().remaining_bytes().saturating_sub(bytes) } else { usize::MAX };
                    match self.faults.reserve_source_value_bounded(generation, &value, crate::ubm_peer_notification_ingress::CallbackProvenance::Unknown, budget) {
                        Ok(reservation) => {
                            bytes = bytes.saturating_add(reservation.retained_bytes());
                            Some(reservation)
                        },
                        Err(error) => {
                            if held {
                                state.reclaim_reservation_failure = Some(error);
                                state.reclaim_gate.as_mut().unwrap().fail_tail();
                                self.capture_overflow_locked(&mut state);
                                return Ok(None);
                            }
                            return Err(error);
                        }
                    }
                } else { None };
                let read = if has_read { state.prepare_value_error(route) } else { None };
                let read = CapturedReply { reply: read.map(|waiter| (waiter, CoreBluetoothReply::Failed(error.clone()))), recovery: self.retired_replies.clone() };
                DeferredPeerCallback::CapturedValueError { route, error, reservation, read }
            }
            PeerDelegateCallback::NotificationError { route, error } => DeferredPeerCallback::CapturedNotify {
                completion: CapturedReply { reply: state.prepare_notification_error(route, error), recovery: self.retired_replies.clone() }, actor: None,
            },
            PeerDelegateCallback::NotificationState { route, notifying } => {
                match state.prepare_notification_state(route, notifying) {
                    Ok(prepared) => DeferredPeerCallback::CapturedNotify {
                        completion: CapturedReply { reply: prepared.completion, recovery: self.retired_replies.clone() },
                        actor: Some(PeerDelegateCallback::CapturedNotificationState { route, notifying, cleanup: prepared.cleanup }),
                    },
                    Err(_) => {
                        if held {
                            state.reclaim_gate.as_mut().unwrap().fail_tail();
                            self.capture_overflow_locked(&mut state);
                            return Ok(None);
                        }
                        return Err(crate::common::util::SourceGroupError::SourceStateUnavailable);
                    }
                }
            }
            other => DeferredPeerCallback::Delegate(other),
        };
        let exhausted = matches!(&captured, DeferredPeerCallback::CapturedValueError { reservation: Some(reservation), .. } if reservation.ordinal_exhausted());
        if !held {
            if exhausted { return Err(crate::common::util::SourceGroupError::OrdinalExhausted); }
            return Ok(Some(captured));
        }
        match state.reclaim_gate.as_mut().unwrap().admit(captured, bytes, false) {
            crate::ubm_reclaim_graph_gate::Admission::Deferred => {
                if exhausted { state.reclaim_reservation_failure = Some(crate::common::util::SourceGroupError::OrdinalExhausted); state.reclaim_gate.as_mut().unwrap().fail_tail(); self.capture_overflow_locked(&mut state); }
                Ok(None)
            },
            _ => unreachable!("capacity preflight and admission share the peer lock"),
        }
    }

    pub(super) fn execute_delegate_capture(&self, effect: DeferredPeerCallback) -> Option<super::central_delegate::PeerDelegateCallback> {
        match effect {
            DeferredPeerCallback::CapturedValueError { route, error, reservation, read } => {
                if let Some(reservation) = reservation { reservation.release(); }
                let read_failed = read.is_some();
                read.complete();
                Some(super::central_delegate::PeerDelegateCallback::CapturedValueError { route, error, read_failed })
            }
            DeferredPeerCallback::CapturedNotify { completion, actor } => {
                completion.complete();
                actor
            }
            DeferredPeerCallback::Delegate(actor) => Some(actor),
            _ => unreachable!("delegate preparation produces only delegate effects"),
        }
    }

    #[cfg(test)]
    pub(super) fn admit_lifecycle(&self, lifecycle: super::central_delegate::PeerLifecycle) -> bool {
        matches!(self.prepare_lifecycle(lifecycle), Ok(None))
    }

    pub(super) fn prepare_lifecycle(&self, lifecycle: super::central_delegate::PeerLifecycle) -> Result<Option<DeferredPeerCallback>, crate::common::util::SourceGroupError> {
        let _wake_scope = self.faults.defer_wakes();
        let mut state = self.waiters.lock().unwrap();
        let held = state.reclaim_gate.as_ref().is_some_and(|gate| gate.is_busy());
        let error = match &lifecycle {
            super::central_delegate::PeerLifecycle::Disconnected => crate::PlatformError::new("corebluetooth", "disconnected", "Device disconnected"),
            super::central_delegate::PeerLifecycle::DisconnectedWithError(error) => error.clone(),
            super::central_delegate::PeerLifecycle::ServicesModified => crate::PlatformError::new("corebluetooth", "services-modified", "CoreBluetooth invalidated the service graph"),
        };
        let bytes = std::mem::size_of::<DeferredPeerCallback>().saturating_add(state.invalidation_waiter_count().saturating_mul(retained_error_bytes(&error).saturating_add(std::mem::size_of::<super::peer_waiters::PreparedReply>())));
        let cause = match &lifecycle {
            super::central_delegate::PeerLifecycle::Disconnected | super::central_delegate::PeerLifecycle::DisconnectedWithError(_) => 1,
            super::central_delegate::PeerLifecycle::ServicesModified => 2,
        };
        if state.ready || state.generation.is_some() { state.lifecycle_causes = 0; }
        let first = !held || state.lifecycle_causes == 0;
        let duplicate = held && state.lifecycle_causes & cause != 0;
        let (replies, source) = if first {
            self.prepare_invalidation_locked(&mut state, error, usize::MAX)?
        } else {
            (CutReplies(state.prepare_invalidation(error)), None)
        };
        if held { state.lifecycle_causes |= cause; }
        if !held { return Ok(Some(DeferredPeerCallback::Lifecycle { event: lifecycle, replies, source })); }
        let bytes = bytes.saturating_add(source.as_ref().map_or(0, |reservation| reservation.retained_bytes()))
            .saturating_add(match &lifecycle { super::central_delegate::PeerLifecycle::DisconnectedWithError(error) => retained_error_bytes(error), _ => 0 });
        // A repeated disconnect with a native cause is a distinct observation;
        // keep it ordered even when the graph cut itself was already admitted.
        let effect = if duplicate && !matches!(&lifecycle, super::central_delegate::PeerLifecycle::DisconnectedWithError(_)) {
            if replies.is_empty() && source.is_none() { return Ok(None); }
            DeferredPeerCallback::Invalidate { replies, source }
        } else {
            DeferredPeerCallback::Lifecycle { event: lifecycle, replies, source }
        };
        let gate = state.reclaim_gate.as_mut().unwrap();
        if gate.can_reserve_before_admit(bytes) {
            assert!(matches!(gate.admit(effect, bytes, true), crate::ubm_reclaim_graph_gate::Admission::Deferred));
        } else {
            gate.admit_mandatory_cut(crate::ubm_reclaim_graph_gate::MandatoryCut::new(effect));
        }
        Ok(None)
    }

    pub(super) fn admit_root_discovery(&self, id: u64, bytes: usize) -> crate::ubm_reclaim_graph_gate::Admission<u64> {
        use crate::ubm_reclaim_graph_gate::Admission;
        let _wake_scope = self.faults.defer_wakes();
        let mut state = self.waiters.lock().unwrap();
        let Some(gate) = state.reclaim_gate.as_mut().filter(|gate| gate.is_busy()) else { return Admission::Apply(id); };
        match gate.admit(DeferredPeerCallback::RootDiscovery(id), bytes, false) {
            Admission::Apply(DeferredPeerCallback::RootDiscovery(id)) => Admission::Apply(id),
            Admission::Deferred => Admission::Deferred,
            Admission::Overflow(DeferredPeerCallback::RootDiscovery(id)) => {
                self.capture_overflow_locked(&mut state);
                Admission::Overflow(id)
            },
            _ => unreachable!("admission returns its input"),
        }
    }

    pub(super) fn acquire_reclaim(&self, binding: &crate::ubm_reclaim_graph_binding::GraphBinding, key: crate::ubm_reclaim_authority::GraphKey) -> Result<crate::ubm_reclaim_authority::PermitStatus, crate::ubm_reclaim_authority::AuthorityError> {
        use crate::ubm_reclaim_authority::{AuthorityError, PermitStatus};
        let _adapter = self.adapter_admission.lock().map_err(|_| AuthorityError::Conflict)?;
        let mut authorities = self.authorities.lock().map_err(|_| AuthorityError::Conflict)?;
        let mut state = self.waiters.lock().map_err(|_| AuthorityError::Conflict)?;
        if !state.reclaim_binding.owns(binding) { return Err(AuthorityError::Conflict); }
        let busy = self.adapter_pending.load(std::sync::atomic::Ordering::Acquire) != 0 || state.reclaim_operations != 0 || state.callback_tickets != 0 || state.discovery_actor_pending || state.reclaim_uncertain || state.reclaim_held
            || state.reclaim_gate.as_ref().is_some_and(|gate| gate.is_busy());
        let valid = state.ready && !state.revision_exhausted
            && self.graph_attachment == Some(key.graph.1) && key.graph.0 == "corebluetooth"
            && state.revision == key.graph.2
            && state.reclaim_binding.validates(binding, &key.peer_id, &key.connection_generation, &key.graph);
        let status = authorities.acquire_bound(key.clone(), binding, valid, busy)?;
        if status == PermitStatus::Acquired {
            state.reclaim_gate.get_or_insert_with(|| crate::ubm_reclaim_graph_gate::GraphGate::new_with_barriers(256, 524_288, 64, 4096))
                .acquire(key).map_err(|_| AuthorityError::Conflict)?;
            state.reclaim_held = true;
        }
        Ok(status)
    }

    pub(super) fn commit_reclaim(&self, key: &crate::ubm_reclaim_authority::GraphKey) -> Result<crate::ubm_reclaim_authority::PermitStatus, crate::ubm_reclaim_authority::AuthorityError> {
        use crate::ubm_reclaim_authority::AuthorityError;
        let _adapter = self.adapter_admission.lock().map_err(|_| AuthorityError::Conflict)?;
        let mut authorities = self.authorities.lock().map_err(|_| AuthorityError::Conflict)?;
        let mut state = self.waiters.lock().map_err(|_| AuthorityError::Conflict)?;
        authorities.authenticate_permit_owner(key, &state.reclaim_binding)?;
        match state.reclaim_gate.as_mut().ok_or(AuthorityError::Unknown)?.commit(key) {
            Ok(()) => authorities.commit(key),
            Err(crate::ubm_reclaim_graph_gate::GateError::Invalidated) => authorities.invalidate_before_commit(key),
            Err(_) => Err(AuthorityError::Conflict),
        }
    }

    pub(super) fn release_reclaim(&self, key: &crate::ubm_reclaim_authority::GraphKey, handoff: impl FnMut((AttrKey, AttrKey), Vec<u8>), replay_root: impl FnMut(u64), replay_lifecycle: impl FnMut(super::central_delegate::PeerLifecycle), replay_delegate: impl FnMut(super::central_delegate::PeerDelegateCallback), replay_actor: impl FnMut(super::central_delegate::PeerActorCallback), barrier_arrived: impl FnMut(Option<u64>), on_overflow: impl FnMut(crate::PlatformError)) -> Result<crate::ubm_reclaim_authority::PermitStatus, crate::ubm_reclaim_authority::AuthorityError> {
        use crate::ubm_reclaim_authority::{AuthorityError, PermitStatus};
        let status = {
            let mut authorities = self.authorities.lock().map_err(|_| AuthorityError::Conflict)?;
            let mut state = self.waiters.lock().map_err(|_| AuthorityError::Conflict)?;
            authorities.authenticate_permit_owner(key, &state.reclaim_binding)?;
            let status = authorities.permit_status(key)?;
            if status == PermitStatus::Released {
                return if state.reclaim_gate.as_ref().is_some_and(|gate| gate.is_draining()) { Err(AuthorityError::Pending) } else { Ok(status) };
            }
            if !matches!(status, PermitStatus::Acquired | PermitStatus::Committed | PermitStatus::Invalidated) { return Err(AuthorityError::Conflict); }
            state.reclaim_gate.as_mut().ok_or(AuthorityError::Unknown)?.release(key).map_err(|_| AuthorityError::Conflict)?;
            authorities.release(key)?
        };
        match self.drain_reclaim_callbacks(handoff, replay_root, replay_lifecycle, replay_delegate, replay_actor, barrier_arrived, on_overflow) {
            Ok(()) => {},
            Err(crate::ubm_reclaim_graph_gate::GateError::Paused) => return Err(AuthorityError::Pending),
            Err(_) => return Err(AuthorityError::Conflict),
        }
        self.waiters.lock().map_err(|_| AuthorityError::Conflict)?.reclaim_held = false;
        Ok(status)
    }

    pub(super) fn forget_reclaim(&self, key: &crate::ubm_reclaim_authority::GraphKey) -> Result<(), crate::ubm_reclaim_authority::AuthorityError> {
        use crate::ubm_reclaim_authority::AuthorityError;
        let mut authorities = self.authorities.lock().map_err(|_| AuthorityError::Conflict)?;
        let state = self.waiters.lock().map_err(|_| AuthorityError::Conflict)?;
        match authorities.authenticate_permit_owner(key, &state.reclaim_binding) {
            Ok(()) | Err(AuthorityError::Unknown) => {},
            Err(error) => return Err(error),
        }
        if state.reclaim_gate.as_ref().is_some_and(|gate| gate.is_draining()) { return Err(AuthorityError::Pending); }
        authorities.forget_permit(key)
    }

    /// Drain one callback at a time outside the peer lock. The caller must
    /// synchronously hand each returned notification to the actor in order.
    pub(super) fn drain_reclaim_callbacks(&self, mut handoff: impl FnMut((AttrKey, AttrKey), Vec<u8>), mut replay_root: impl FnMut(u64), mut replay_lifecycle: impl FnMut(super::central_delegate::PeerLifecycle), mut replay_delegate: impl FnMut(super::central_delegate::PeerDelegateCallback), mut replay_actor: impl FnMut(super::central_delegate::PeerActorCallback), mut barrier_arrived: impl FnMut(Option<u64>), mut on_overflow: impl FnMut(crate::PlatformError)) -> Result<(), crate::ubm_reclaim_graph_gate::GateError> {
        use crate::ubm_reclaim_graph_gate::{DrainItem, GateError};
        {
            let mut state = self.waiters.lock().unwrap();
            if state.reclaim_drain_active { return Err(GateError::Busy); }
            state.reclaim_drain_active = true;
        }
        let _drain = ReclaimDrainGuard { owner: self };
        loop {
            let item = {
                let mut state = self.waiters.lock().unwrap();
                if state.adapter_overflow_paused { return Err(GateError::Paused); }
                let item = state.reclaim_gate.as_mut().ok_or(GateError::NotCurrent)?.next()?;
                if matches!(item, DrainItem::Finished) {
                    state.empty_invalidation_admitted = false;
                    state.empty_source_failure_admitted = false;
                    state.lifecycle_causes = 0;
                }
                item
            };
            match item {
                DrainItem::Event(DeferredPeerCallback::CapturedNotification { route, data, reservation, mut read, notification }) => {
                    if let Some(reservation) = reservation { reservation.release(); }
                    if let Some((waiter, provenance)) = read.read.take() {
                        let (accepted, waker) = waiter.lock().unwrap().try_set_reply_deferred(CoreBluetoothReply::CharacteristicRead(data.clone(), provenance));
                        if !accepted { log::warn!("CoreBluetooth reply arrived after terminal completion"); }
                        if let Some(waker) = waker { waker.wake(); }
                    }
                    if notification { handoff(route, data); }
                }
                DrainItem::Event(DeferredPeerCallback::CapturedValueError { route, error, reservation, read }) => {
                    if let Some(reservation) = reservation { reservation.release(); }
                    let read_failed = read.is_some();
                    read.complete();
                    replay_delegate(super::central_delegate::PeerDelegateCallback::CapturedValueError { route, error, read_failed });
                }
                DrainItem::Event(DeferredPeerCallback::CapturedNotify { completion, actor }) => {
                    completion.complete();
                    if let Some(actor) = actor { replay_delegate(actor); }
                }
                DrainItem::Event(DeferredPeerCallback::Invalidate { replies, source }) => {
                    if let Some(source) = source { source.release(); }
                    replies.complete();
                }
                DrainItem::Event(effect @ DeferredPeerCallback::SourceFailure { .. }) => {
                    if matches!(&effect, DeferredPeerCallback::SourceFailure { report_overflow: true, .. }) {
                        self.waiters.lock().unwrap().overflow_cuts_queued -= 1;
                    }
                    if let Some(error) = Self::execute_source_failure(effect) { on_overflow(error); }
                }
                DrainItem::Event(DeferredPeerCallback::RootDiscovery(id)) => replay_root(id),
                DrainItem::Event(DeferredPeerCallback::Lifecycle { event, replies, source }) => {
                    if let Some(source) = source { source.release(); }
                    replies.complete();
                    replay_lifecycle(event);
                }
                DrainItem::Event(DeferredPeerCallback::Delegate(event)) => replay_delegate(event),
                DrainItem::Event(DeferredPeerCallback::Actor(event)) => replay_actor(event),
                DrainItem::Event(DeferredPeerCallback::AdapterBarrier) => return Err(GateError::NotCurrent),
                DrainItem::Barrier { id, event: DeferredPeerCallback::AdapterBarrier } => {
                    barrier_arrived(Some(id));
                    return Err(GateError::Paused);
                }
                DrainItem::Barrier { .. } => return Err(GateError::NotCurrent),
                DrainItem::BarrierOverflow => {
                    self.waiters.lock().unwrap().adapter_overflow_paused = true;
                    barrier_arrived(None);
                    return Err(GateError::Paused);
                }
                DrainItem::Overflow => {
                    let (captured, already_reported, reservation_failure) = {
                        let mut state = self.waiters.lock().unwrap();
                        (state.overflow_cuts_queued != 0, std::mem::take(&mut state.adapter_tail_reported), state.reclaim_reservation_failure.take())
                    };
                    if !captured && (!already_reported || reservation_failure.is_some()) {
                        let error = if let Some(reason) = reservation_failure {
                            crate::PlatformError::new("corebluetooth", "reclaim-callback-reservation-failed", "Could not reserve a reclaim callback; its tail is uncertain")
                                .with("sourceError", format!("{reason:?}"))
                        } else {
                            crate::PlatformError::new("corebluetooth", "reclaim-callback-overflow", "Reclaim callback tail is uncertain")
                        };
                        self.fail_source(error.clone(), None);
                        on_overflow(error);
                    }
                }
                DrainItem::Finished => return Ok(()),
            }
        }
    }

    pub(super) fn fail_source(
        &self,
        error: crate::PlatformError,
        route: Option<(AttrKey, AttrKey)>,
    ) {
        let _wake_scope = self.faults.defer_wakes();
        let mut state = self.waiters.lock().unwrap();
        let held = state.reclaim_gate.as_ref().is_some_and(|gate| gate.is_busy());
        if held && state.empty_source_failure_admitted && !state.ready && state.generation.is_none()
            && state.invalidation_waiter_count() == 0 && route.is_none() { return; }
        let effect = self.capture_source_failure_locked(&mut state, error, route, false);
        if held { state.empty_source_failure_admitted = true; }
        if let Some(gate) = state.reclaim_gate.as_mut().filter(|gate| gate.is_busy()) {
            gate.admit_mandatory_cut(crate::ubm_reclaim_graph_gate::MandatoryCut::new(effect));
            return;
        }
        drop(state);
        Self::execute_source_failure(effect);
    }

    pub(super) fn fail_handoff(&self) {
        self.fail_source(
            crate::PlatformError::new(
                "corebluetooth",
                "source-handoff-failed",
                "The delegate event channel closed",
            ),
            None,
        );
    }
}

impl Debug for PeerSourceOwner {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("PeerSourceOwner").finish_non_exhaustive()
    }
}

#[derive(Default)]
pub(super) struct PeerSourceRegistry {
    pub(super) adapter_admission: Arc<std::sync::Mutex<()>>,
    adapter_pending: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) queue_closed: std::sync::atomic::AtomicBool,
    pub(super) replay_jobs: std::sync::atomic::AtomicUsize,
    pub(super) authorities: Arc<std::sync::Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
    failure_evidence_store: Option<Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>>,
    peers: std::sync::Mutex<HashMap<Uuid, Arc<PeerSourceOwner>>>,
}

impl Debug for PeerSourceRegistry {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        f.debug_struct("PeerSourceRegistry").finish_non_exhaustive()
    }
}

impl PeerSourceRegistry {
    pub(super) fn with_authorities(authorities: Arc<std::sync::Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>) -> Self {
        Self::with_authorities_and_failure_evidence_store(authorities, None)
    }
    pub(super) fn with_authorities_and_failure_evidence_store(
        authorities: Arc<std::sync::Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
        failure_evidence_store: Option<Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>>,
    ) -> Self {
        Self { authorities, failure_evidence_store, adapter_admission: Arc::default(), adapter_pending: Arc::default(), peers: std::sync::Mutex::new(HashMap::new()), queue_closed: std::sync::atomic::AtomicBool::new(false), replay_jobs: std::sync::atomic::AtomicUsize::new(0) }
    }
    pub(super) fn begin_adapter_event(&self) -> Option<AdapterEventTicket> {
        use std::sync::atomic::Ordering;
        self.adapter_pending.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| (count < 256).then_some(count + 1)).ok()?;
        Some(AdapterEventTicket { pending: self.adapter_pending.clone() })
    }

    pub(super) fn get_or_create(&self, peer: Uuid) -> Arc<PeerSourceOwner> {
        let _adapter = self.adapter_admission.lock().unwrap();
        self.peers
            .lock()
            .unwrap()
            .entry(peer)
            .or_insert_with(|| PeerSourceOwner::with_authorities(
                self.authorities.clone(),
                self.adapter_admission.clone(),
                self.adapter_pending.clone(),
                self.failure_evidence_store.clone(),
            ))
            .clone()
    }

    /// The caller holds adapter_admission so the peer set cannot change during installation.
    pub(super) fn barrier_owners(&self) -> Vec<(Uuid, Arc<PeerSourceOwner>)> {
        self.peers.lock().unwrap().iter().map(|(id, owner)| (*id, owner.clone())).collect()
    }

    pub(super) fn fail_all(
        &self,
        error: crate::PlatformError,
        peer: Option<(Uuid, (AttrKey, AttrKey))>,
    ) {
        let owners: Vec<_> = self
            .peers
            .lock()
            .unwrap()
            .iter()
            .map(|(id, owner)| (*id, owner.clone()))
            .collect();
        for (id, owner) in owners {
            let route = peer.and_then(|(selected, route)| (selected == id).then_some(route));
            owner.fail_source(error.clone(), route);
        }
    }

    pub(super) fn get(&self, peer: Uuid) -> Option<Arc<PeerSourceOwner>> {
        self.peers.lock().unwrap().get(&peer).cloned()
    }

    // Retirement and registration use this same admission lock. Keep it
    // through the native call so a request cannot dispatch against an owner
    // that was replaced after its final check. The closure must not wake a
    // waiter or re-enter the registry.
    fn dispatch_request_if_current(
        &self,
        identity: &NativeRequestIdentity,
        native_owner: &Arc<PeerSourceOwner>,
        dispatch: impl FnOnce(),
    ) -> bool {
        let _admission = self.adapter_admission.lock().unwrap();
        let current = self.peers.lock().unwrap().get(&identity.uuid)
            .is_some_and(|owner| Arc::ptr_eq(owner, &identity.owner));
        if !current || !Arc::ptr_eq(native_owner, &identity.owner) {
            return false;
        }
        dispatch();
        true
    }

    pub(super) fn remove_if_current(&self, peer: Uuid, expected: &Arc<PeerSourceOwner>) {
        let _adapter = self.adapter_admission.lock().unwrap();
        let mut peers = self.peers.lock().unwrap();
        if peers
            .get(&peer)
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            peers.remove(&peer);
        }
    }
}

/// Disconnect requests for one actor-owned native cycle. The first request
/// initiates cancellation; subsequent requests await the same observation.
#[derive(Default)]
struct DisconnectReplies(VecDeque<CoreBluetoothReplyStateShared>);

impl DisconnectReplies {
    fn admit(&mut self, reply: CoreBluetoothReplyStateShared) -> Result<bool, ()> {
        if self.0.len() >= 256 { return Err(()); }
        let initiate = self.0.is_empty();
        self.0.push_back(reply);
        Ok(initiate)
    }

    fn complete(&mut self, reply: CoreBluetoothReply) {
        let pending = std::mem::take(&mut self.0);
        let mut wakers = Vec::with_capacity(pending.len());
        for state in pending {
            let (accepted, waker) = state.lock().unwrap().try_set_reply_deferred(reply.clone());
            if !accepted { log::warn!("CoreBluetooth reply arrived after terminal completion"); }
            if let Some(waker) = waker { wakers.push(waker); }
        }
        for waker in wakers { waker.wake(); }
    }

    fn complete_observation(&mut self, platform_error: Option<crate::PlatformError>) {
        self.complete(match platform_error {
            Some(error) => CoreBluetoothReply::Failed(error),
            None => CoreBluetoothReply::Ok,
        });
    }
}

struct PeripheralInternal {
    source_owner: Arc<PeerSourceOwner>,
    pub peripheral: Retained<CBPeripheral>,
    // The exact queue-owned object seen at discovery. Retrieved peripherals
    // do not yet have a queue registration and therefore have no token.
    native_token: Option<super::native_registry::NativeObjectToken>,
    services: HashMap<AttrKey, ServiceInternal>,
    pub event_sender: Sender<PeripheralEventInternal>,
    disconnected_future_state: DisconnectReplies,
    pub connected_future_state: Option<CoreBluetoothReplyStateShared>,
    pub services_discovered_future_state: Option<CoreBluetoothReplyStateShared>,
    discovery_busy: bool,
    activation_owners: HashMap<u64, NativeActivationEntry>,
    pub read_rssi_future_state: VecDeque<CoreBluetoothReplyStateShared>,
    pub write_without_response_queue: VecDeque<PendingWriteWithoutResponse>,
    /// UBM patch (UBM_PATCHES.md #17, finding 205): the last advertised
    /// name this peripheral was seen with. A nameless sighting's report is
    /// labelled with it instead of the GAP alias; a peer no advertisement
    /// has ever named keeps the GAP seed.
    pub advertised_name: Option<String>,
}

impl Debug for PeripheralInternal {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("CBPeripheral")
            .field("peripheral", self.peripheral.deref())
            .field("native_token", &self.native_token)
            .field(
                "services",
                &self
                    .services
                    .iter()
                    .map(|(service_uuid, service)| (service_uuid, service.characteristics.len()))
                    .collect::<HashMap<_, _>>(),
            )
            .field("event_sender", &self.event_sender)
            .field("advertised_name", &self.advertised_name)
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
        source_owner: Arc<PeerSourceOwner>,
    ) -> Self {
        Self {
            source_owner,
            peripheral,
            native_token: None,
            services: HashMap::new(),
            event_sender,
            connected_future_state: None,
            disconnected_future_state: DisconnectReplies::default(),
            services_discovered_future_state: None,
            discovery_busy: false,
            activation_owners: HashMap::new(),
            read_rssi_future_state: VecDeque::with_capacity(4),
            write_without_response_queue: VecDeque::new(),
            advertised_name: None,
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
        if !self
            .services
            .values()
            .any(|service| !service.discovered || service.included_services.is_none())
        {
            if self.services_discovered_future_state.is_none() {
                return;
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

    pub fn confirm_disconnect(&mut self, platform_error: Option<crate::PlatformError>) {
        self.source_owner.with_waiters(|state| state.discovery_actor_pending = false);
        // Fulfill the disconnected future, if there is one.
        // There might not be a future if the device disconnects unexpectedly.
        self.disconnected_future_state.complete_observation(platform_error.clone());

        // Fulfill pending RSSI futures
        let error = platform_error.map(CoreBluetoothReply::Failed)
            .unwrap_or_else(|| CoreBluetoothReply::Err(String::from("Device disconnected")));
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
        self.discovery_busy = false;
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
// This owner stays on the CoreBluetooth thread and is never sent to the
// delegate queue. The existing event channel returns here when an actor is
// retired. Moving native dispatch and identities onto that queue is separate
// work; some existing delegate events still carry retained native objects.
struct CoreBluetoothManagerLifetime {
    manager: Option<Retained<CBCentralManager>>,
    delegate: Retained<CentralDelegate>,
    adapter_dispatch_fence: Arc<super::central_delegate::AdapterDispatchFence>,
    source_registry: Arc<PeerSourceRegistry>,
    delegate_queue: ffi::dispatch_queue_t,
    delegate_receiver: RefCell<Option<Fuse<Receiver<CentralDelegateEvent>>>>,
    failure_receiver: RefCell<Option<futures::channel::oneshot::Receiver<crate::PlatformError>>>,
    peripherals: RefCell<HashMap<Uuid, PeripheralInternal>>,
    directory_replies: RefCell<Vec<CoreBluetoothReplyStateShared>>,
    active_reply: RefCell<Option<CoreBluetoothReplyStateShared>>,
}

struct CoreBluetoothInternal {
    manager_lifetime: Rc<CoreBluetoothManagerLifetime>,
    source_registry: Arc<PeerSourceRegistry>,
    // Map of identifiers to object pointers
    peripherals: HashMap<Uuid, PeripheralInternal>,
    delegate_receiver: Fuse<Receiver<CentralDelegateEvent>>,
    active_reply: Option<CoreBluetoothReplyStateShared>,
    directory_replies: Vec<CoreBluetoothReplyStateShared>,
    failure_receiver: Option<futures::channel::oneshot::Receiver<crate::PlatformError>>,
    // Out in the world beyond CoreBluetooth, we'll be async, so just
    // task::block this when sending even though it'll never actually block.
    event_sender: Sender<CoreBluetoothEvent>,
    message_receiver: Fuse<Receiver<CoreBluetoothMessage>>,
}

impl Debug for CoreBluetoothInternal {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("CoreBluetoothInternal")
            .field("manager", &self.manager_lifetime.manager)
            .field("delegate", self.manager_lifetime.delegate.deref())
            .field("peripherals", &self.peripherals)
            .field("delegate_receiver", &self.delegate_receiver)
            .field("event_sender", &self.event_sender)
            .field("message_receiver", &self.message_receiver)
            .finish()
    }
}

/// Identity captured by a public native request. UUID alone is not enough:
/// a replacement handle can refer to the same peer with a different owner.
#[derive(Clone, Debug)]
pub struct NativeRequestIdentity {
    uuid: Uuid,
    owner: Arc<PeerSourceOwner>,
    admission: Arc<NativeRequestAdmission>,
}

/// The public caller and actor race exactly once over a queued request.
#[derive(Debug, Default)]
pub(super) struct NativeRequestAdmission {
    state: std::sync::atomic::AtomicU8,
    cancelled: tokio::sync::Notify,
}

impl NativeRequestAdmission {
    pub(super) fn cancel(&self) {
        use std::sync::atomic::Ordering;
        if self.state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.cancelled.notify_one();
        }
    }

    fn claim(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.state.compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.state.load(std::sync::atomic::Ordering::Acquire) == 1
    }

    /// The actor claimed this request but refused it before any native effect.
    pub(super) fn refuse_before_dispatch(&self) {
        use std::sync::atomic::Ordering;
        let previous = self.state.swap(3, Ordering::AcqRel);
        debug_assert_eq!(previous, 2);
    }

    pub(super) fn has_no_effect(&self) -> bool {
        matches!(self.state.load(std::sync::atomic::Ordering::Acquire), 1 | 3)
    }

    pub(super) async fn cancelled(&self) {
        // notify_one retains a permit if cancellation wins between this check
        // and registering the sole detached operation's waiter.
        if !self.is_cancelled() {
            self.cancelled.notified().await;
        }
    }
}

impl NativeRequestIdentity {
    pub(super) fn new(uuid: Uuid, owner: Arc<PeerSourceOwner>, admission: Arc<NativeRequestAdmission>) -> Self {
        Self { uuid, owner, admission }
    }

    pub(super) fn uuid(&self) -> Uuid {
        self.uuid
    }

    pub(super) fn owner(&self) -> &Arc<PeerSourceOwner> {
        &self.owner
    }

    pub(super) fn admission(&self) -> &Arc<NativeRequestAdmission> {
        &self.admission
    }

    #[cfg(test)]
    pub(super) fn matches_owner(&self, owner: &Arc<PeerSourceOwner>) -> bool {
        Arc::ptr_eq(&self.owner, owner)
    }
}

#[derive(Debug)]
pub enum CoreBluetoothMessage {
    DirectoryLookup {
        services: Option<Vec<Uuid>>,
        identifier: Option<Uuid>,
        future: CoreBluetoothReplyStateShared,
    },
    GetAdapterState {
        future: CoreBluetoothReplyStateShared,
    },
    StartScanning {
        filter: ScanFilter,
        // UBM patch (UBM_PATCHES.md #8): answered once the scan started or
        // was refused.
        future: CoreBluetoothReplyStateShared,
    },
    /// Arm/retire/fence one CoreBluetooth delegate-queue scan ingress epoch.
    /// The platform has no physical scan identity, so the serial delegate
    /// queue is the only boundary that can preserve callback ownership.
    ScanIngress {
        action: super::central_delegate::ScanIngressAction,
        epoch: u64,
        fence_id: Option<u64>,
        future: CoreBluetoothReplyStateShared,
    },
    StopScanning,
    ConnectDevice {
        identity: NativeRequestIdentity,
        future: CoreBluetoothReplyStateShared,
    },
    // UBM patch (UBM_PATCHES.md #19): known, or retrieved by identifier.
    DisconnectDevice {
        identity: NativeRequestIdentity,
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
    ReleaseReclaim {
        peripheral_uuid: Uuid,
        source_owner: Arc<PeerSourceOwner>,
        key: crate::ubm_reclaim_authority::GraphKey,
        result: tokio::sync::oneshot::Sender<Result<crate::ubm_reclaim_authority::PermitStatus, crate::ubm_reclaim_authority::AuthorityError>>,
    },
    MarkerWrite {
        peripheral_uuid: Uuid,
        expected: (String, u64, u64),
        group: crate::common::util::SourceGroupHandle,
        request: crate::common::util::SourceWriteRequest,
        result: tokio::sync::oneshot::Sender<
            std::result::Result<
                crate::common::util::SourceWriteReceipt,
                crate::common::util::SourceGroupError,
            >,
        >,
    },
    AcquireNotificationOwner {
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        future: CoreBluetoothReplyStateShared,
    },
    SetNotificationOwner {
        peripheral_uuid: Uuid,
        token: Arc<NativeActivationToken>,
        enable: bool,
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

impl CoreBluetoothMessage {
    fn fail(&self, error: &crate::PlatformError) {
        if let Some(reply) = self.reply() {
            reply
                .lock()
                .unwrap()
                .try_set_reply(CoreBluetoothReply::Failed(error.clone()));
        }
    }

    fn reply(&self) -> Option<CoreBluetoothReplyStateShared> {
        match self {
            Self::DirectoryLookup { future, .. }
            | Self::GetAdapterState { future }
            | Self::StartScanning { future, .. }
            | Self::ScanIngress { future, .. }
            | Self::ConnectDevice { future, .. }
            | Self::DisconnectDevice { future, .. }
            | Self::ReadValue { future, .. }
            | Self::WriteValue { future, .. }
            | Self::AcquireNotificationOwner { future, .. }
            | Self::SetNotificationOwner { future, .. }
            | Self::IsConnected { future, .. }
            | Self::ReadDescriptorValue { future, .. }
            | Self::WriteDescriptorValue { future, .. }
            | Self::DiscoverServices { future, .. }
            | Self::ReadRssi { future, .. }
            | Self::GetWriteLengths { future, .. }
            | Self::GetWriteReadiness { future, .. } => Some(future.clone()),
            Self::StopScanning | Self::MarkerWrite { .. } | Self::ReleaseReclaim { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum CoreBluetoothEvent {
    DirectoryReady {
        peers: Vec<(Uuid, Option<String>)>,
        future: CoreBluetoothReplyStateShared,
    },
    RetrievedPeripheral {
        source_owner: Arc<PeerSourceOwner>,
        uuid: Uuid,
        local_name: Option<String>,
        event_receiver: Receiver<PeripheralEventInternal>,
    },
    DidUpdateState {
        state: CBManagerState,
    },
    DeviceDiscovered {
        source_owner: Arc<PeerSourceOwner>,
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
    /// A native connection callback could not be associated with an admitted
    /// exact-object cycle.  This is deliberately a source-uncertainty event,
    /// not a disconnect for whichever UUID owner happens to be current.
    DeviceSourceUncertain {
        uuid: Uuid,
        error: Option<crate::PlatformError>,
    },
    // UBM patch (UBM_PATCHES.md #17): one advertisement with its own data.
    Advertised {
        uuid: Uuid,
        report: crate::api::AdvertisementReport,
        ingress_epoch: Option<u64>,
    },
    ScanIngressFence {
        epoch: u64,
        fence_id: u64,
    },
}

impl CoreBluetoothManagerLifetime {
    fn new(source_registry: Arc<PeerSourceRegistry>, radio: bool) -> Rc<Self> {
        let (sender, receiver) = mpsc::channel::<CentralDelegateEvent>(256);
        let (failure_sender, failure_receiver) = futures::channel::oneshot::channel();
        let label = CString::new("CBqueue").unwrap();
        let delegate_queue = unsafe { ffi::dispatch_queue_create(label.as_ptr(), ffi::DISPATCH_QUEUE_SERIAL) };
        assert!(!delegate_queue.is_null(), "CoreBluetooth queue creation failed");
        let delegate = CentralDelegate::new_with_queue(sender, source_registry.clone(), Some(failure_sender), Some(delegate_queue));
        let adapter_dispatch_fence = delegate.adapter_dispatch_fence();
        let manager = if radio {
            let queue: *mut AnyObject = delegate_queue.cast();
            Some(unsafe { msg_send_id![CBCentralManager::alloc(), initWithDelegate: &*delegate, queue: queue] })
        } else {
            None
        };
        Rc::new(Self {
            manager,
            delegate,
            adapter_dispatch_fence,
            source_registry,
            delegate_queue,
            delegate_receiver: RefCell::new(Some(receiver.fuse())),
            failure_receiver: RefCell::new(Some(failure_receiver)),
            peripherals: RefCell::new(HashMap::new()),
            directory_replies: RefCell::new(Vec::new()),
            active_reply: RefCell::new(None),
        })
    }
}

impl CoreBluetoothInternal {
    pub fn new(
        message_receiver: Receiver<CoreBluetoothMessage>,
        event_sender: Sender<CoreBluetoothEvent>,
    ) -> Self {
        Self::with_registry(message_receiver, event_sender, Arc::new(PeerSourceRegistry::default()))
    }

    fn with_registry(message_receiver: Receiver<CoreBluetoothMessage>, event_sender: Sender<CoreBluetoothEvent>, source_registry: Arc<PeerSourceRegistry>) -> Self {
        let lifetime = CoreBluetoothManagerLifetime::new(source_registry, true);
        Self::attach(lifetime, message_receiver, event_sender)
    }

    fn attach(lifetime: Rc<CoreBluetoothManagerLifetime>, message_receiver: Receiver<CoreBluetoothMessage>, event_sender: Sender<CoreBluetoothEvent>) -> Self {
        let receiver = lifetime.delegate_receiver.borrow_mut().take().expect("only one actor may attach to a CoreBluetooth manager");
        let failure_receiver = lifetime.failure_receiver.borrow_mut().take();
        let peripherals = std::mem::take(&mut *lifetime.peripherals.borrow_mut());
        let directory_replies = std::mem::take(&mut *lifetime.directory_replies.borrow_mut());
        let active_reply = lifetime.active_reply.borrow_mut().take();
        Self {
            source_registry: lifetime.source_registry.clone(),
            manager_lifetime: lifetime,
            peripherals,
            delegate_receiver: receiver,
            active_reply,
            directory_replies,
            failure_receiver,
            event_sender,
            message_receiver: message_receiver.fuse(),
        }
    }

    #[cfg(test)]
    fn new_without_radio(message_receiver: Receiver<CoreBluetoothMessage>, event_sender: Sender<CoreBluetoothEvent>) -> Self {
        let lifetime = CoreBluetoothManagerLifetime::new(Arc::new(PeerSourceRegistry::default()), false);
        Self::attach(lifetime, message_receiver, event_sender)
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
            manufacturer_id,
            manufacturer_data
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
        native_token: Option<super::native_registry::NativeObjectToken>,
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
            let source_owner = self.source_registry.get_or_create(uuid);
            let mut entry = PeripheralInternal::new(peripheral, event_sender, source_owner.clone());
            entry.native_token = native_token;
            self.peripherals.insert(uuid, entry);
            self.dispatch_event(CoreBluetoothEvent::DeviceDiscovered {
                source_owner,
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
                            included_services: None,
                        },
                    )
                })
                .collect();
            p.services = services;
            p.check_discovered();
        }
    }

    fn on_discovered_included_services(
        &mut self,
        peer: Uuid,
        service_key: AttrKey,
        added: HashMap<AttrKey, Retained<CBService>>,
        result: Result<Vec<AttrKey>, crate::PlatformError>,
    ) {
        let Some(peripheral) = self.peripherals.get_mut(&peer) else {
            return;
        };
        if peripheral.services_discovered_future_state.is_none() {
            return;
        }
        let references = match result {
            Ok(references) => references,
            Err(error) => {
                if let Some(future) = peripheral.services_discovered_future_state.take() {
                    fail(future, error);
                }
                return;
            }
        };
        for (key, cbservice) in added {
            peripheral.services.entry(key).or_insert(ServiceInternal {
                cbservice,
                characteristics: HashMap::new(),
                discovered: false,
                included_services: None,
            });
        }
        let Some(service) = peripheral.services.get_mut(&service_key) else {
            return;
        };
        service.included_services = Some(references);
        peripheral.check_discovered();
    }

    fn on_discovered_characteristics(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristics: HashMap<AttrKey, Retained<CBCharacteristic>>,
    ) {
        trace!(
            "Found characteristics for peripheral {} service {}:",
            peripheral_uuid,
            service_uuid
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
            peripheral_uuid,
            service_uuid,
            characteristic_uuid,
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
                .confirm_disconnect(None);
            self.dispatch_event(CoreBluetoothEvent::DeviceDisconnected { uuid })
                .await;
        }
        for (uuid, peripheral) in self.peripherals.drain() {
            self.source_registry
                .remove_if_current(uuid, &peripheral.source_owner);
        }
    }

    async fn on_peripheral_disconnect_owned(
        &mut self,
        peripheral_uuid: Uuid,
        error: Option<crate::PlatformError>,
        expected_owner: Option<&Arc<PeerSourceOwner>>,
    ) {
        trace!("Got disconnect event!");
        // A lifecycle callback can be retained by an owner which has since
        // been removed from the UUID registry.  It is still a real terminal
        // observation for that owner, but it must never disconnect a new
        // peripheral that now occupies the same UUID slot.
        if let Some(expected_owner) = expected_owner {
            if !self.peripherals.get(&peripheral_uuid).is_some_and(|peripheral|
                Arc::ptr_eq(&peripheral.source_owner, expected_owner)
            ) {
                trace!("Ignoring retired-owner disconnect for replacement peripheral {peripheral_uuid}");
                return;
            }
        }
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
                .confirm_disconnect(error);
            self.dispatch_event(CoreBluetoothEvent::DeviceDisconnected {
                uuid: peripheral_uuid,
            })
            .await;
        }
    }

    async fn on_peripheral_disconnect(&mut self, peripheral_uuid: Uuid, error: Option<crate::PlatformError>) {
        self.on_peripheral_disconnect_owned(peripheral_uuid, error, None).await;
    }

    /// UBM patch (UBM_PATCHES.md #14/#15): an attribute callback carried an
    /// `NSError`: the oldest waiter of that stage gets it as the platform's
    /// answer. A value-update error with no pending read reaches the exact
    /// notification attribute owner instead of disappearing.
    async fn on_attribute_failed(
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
                AttributeStage::NotifyState => {
                    characteristic.fail_notification_state(error);
                    return;
                }
            },
        };
        if let Some(waiter) = waiter {
            fail(waiter, error);
        } else if descriptor_uuid.is_none() && matches!(stage, AttributeStage::Value) {
            if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
                if let Err(failure) = peripheral
                    .event_sender
                    .send(PeripheralEventInternal::NotificationFailed(
                        characteristic_uuid,
                        service_uuid,
                        error,
                    ))
                    .await
                {
                    error!("Error sending notification source failure: {}", failure);
                }
            }
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
        _peripheral_uuid: Uuid,
        _service_uuid: AttrKey,
        _characteristic_uuid: AttrKey,
    ) {
        // The delegate completed the enable at callback ingress.
    }

    async fn on_characteristic_unsubscribed(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        id: u64,
    ) {
        let Some(peripheral) = self.peripherals.get(&peripheral_uuid) else {
            return;
        };
        let owner = peripheral.source_owner.clone();
        let (ack, completed) = tokio::sync::oneshot::channel();
        let sent = peripheral
            .event_sender
            .clone()
            .send(PeripheralEventInternal::NotificationSourceRetired(
                characteristic_uuid,
                service_uuid,
                ack,
            ))
            .await;
        let result = if sent.is_err() || completed.await.is_err() {
            Err(crate::PlatformError::new(
                "corebluetooth",
                "notification-retirement-failed",
                "Native notification terminal retirement was not acknowledged",
            ))
        } else {
            Ok(())
        };
        let failed = result.is_err();
        owner.with_waiters(|state| {
            if !failed {
                owner.faults.clear_key((
                    service_uuid.uuid,
                    service_uuid.instance,
                    characteristic_uuid.uuid,
                    characteristic_uuid.instance,
                ));
            }
            let _ = state.finish_cleanup((service_uuid, characteristic_uuid), id, result);
        });
        if failed {
            owner.fail_handoff();
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
                peripheral.source_owner.fail_handoff();
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
        identity: NativeRequestIdentity,
        fut: CoreBluetoothReplyStateShared,
    ) {
        if !identity.admission.claim() {
            let (_, waker) = fut.lock().unwrap().try_set_reply_deferred(CoreBluetoothReply::Err("Connect cancelled before dispatch".to_owned()));
            if let Some(waker) = waker { waker.wake(); }
            return;
        }
        let peripheral_uuid = identity.uuid();
        if !self.source_registry.get(peripheral_uuid).is_some_and(|owner| Arc::ptr_eq(&owner, &identity.owner)) {
            identity.admission.refuse_before_dispatch();
            let error = crate::PlatformError::new("corebluetooth", "connect-owner-retired", "The connect request's peer owner was retired before dispatch");
            let (_, waker) = fut.lock().unwrap().try_set_reply_deferred(CoreBluetoothReply::Failed(error));
            if let Some(waker) = waker { waker.wake(); }
            return;
        }
        trace!("Scheduling exact delegate-queue connect for peripheral");
        // The actor deliberately does not touch `CBPeripheral` here.  The
        // delegate queue selects the discovery-retained object associated with
        // this request's exact owner and invokes CoreBluetooth in that queue.
        if unsafe {
            super::central_delegate::schedule_delegate_connect(
                self.manager_lifetime.delegate_queue,
                self.source_registry.clone(),
                identity.clone(),
                fut.clone(),
                self.manager_lifetime.adapter_dispatch_fence.clone(),
            )
        }
        .is_err()
        {
            identity.admission.refuse_before_dispatch();
            let error = crate::PlatformError::new("corebluetooth", "connect-delegate-unavailable", "The CoreBluetooth delegate queue is unavailable");
            let (_, waker) = fut.lock().unwrap().try_set_reply_deferred(CoreBluetoothReply::Failed(error));
            if let Some(waker) = waker { waker.wake(); }
        }
    }

    /// UBM patch (UBM_PATCHES.md #19): `retrievePeripheralsWithIdentifiers`
    /// for one identifier (legacy addon `addon.mm:796-846`). A retrieved
    /// peripheral is registered without replacing an active callback owner.
    /// `false` when CoreBluetooth does not know the identifier.
    async fn retrieve_peripheral(
        &mut self,
        peripheral_uuid: Uuid,
    ) -> Result<bool, crate::PlatformError> {
        let identifier = objc2_foundation::NSUUID::from_bytes(*peripheral_uuid.as_bytes());
        let identifiers = NSArray::from_vec(vec![identifier]);
        let retrieved = unsafe {
            self.manager_lifetime.manager.as_ref().expect("radio manager")
                .retrievePeripheralsWithIdentifiers(&identifiers)
        };
        let Some(peripheral) = retrieved.iter().find(|candidate| {
            nsuuid_to_uuid(&*unsafe { candidate.identifier() }) == peripheral_uuid
        }) else {
            return Ok(false);
        };
        self.register_retrieved(peripheral.retain(), None).await?;
        Ok(true)
    }

    async fn register_retrieved(
        &mut self,
        peripheral: Retained<CBPeripheral>,
        reply: Option<&CoreBluetoothReplyStateShared>,
    ) -> Result<(), crate::PlatformError> {
        let peripheral_uuid = nsuuid_to_uuid(&*unsafe { peripheral.identifier() });
        // Retrieval must never replace an active peripheral's callback ownership.
        if self.peripherals.contains_key(&peripheral_uuid) {
            return Ok(());
        }
        let local_name = unsafe { peripheral.name() }.map(|name| name.to_string());
        let (event_sender, event_receiver) = mpsc::channel(256);
        let source_owner = self.source_registry.get_or_create(peripheral_uuid);
        self.peripherals.insert(
            peripheral_uuid,
            PeripheralInternal::new(peripheral.retain(), event_sender, source_owner.clone()),
        );
        let event = CoreBluetoothEvent::RetrievedPeripheral {
            source_owner,
            uuid: peripheral_uuid,
            local_name,
            event_receiver,
        };
        let outcome = if let Some(reply) = reply {
            enqueue_directory_event(&self.event_sender, event, reply).await
        } else {
            self.event_sender.clone().send(event).await.map_err(|_| {
                crate::PlatformError::new(
                    "corebluetooth",
                    "directory-queue-closed",
                    "directory adapter event receiver is closed",
                )
            })
        };
        if outcome.is_err() {
            if let Some(peripheral) = self.peripherals.remove(&peripheral_uuid) {
                self.source_registry
                    .remove_if_current(peripheral_uuid, &peripheral.source_owner);
            }
        }
        outcome
    }

    async fn directory_lookup(
        &mut self,
        services: Option<Vec<Uuid>>,
        identifier: Option<Uuid>,
        future: CoreBluetoothReplyStateShared,
    ) {
        let state = unsafe { self.manager_lifetime.manager.as_ref().expect("radio manager").state() };
        if state != CBManagerState::PoweredOn {
            future.lock().unwrap().set_reply(CoreBluetoothReply::Failed(
                crate::PlatformError::new(
                    "corebluetooth",
                    format!("manager-state-{}", state.0),
                    format!("directory query refused while manager state is {state:?}"),
                ),
            ));
            return;
        }
        let retrieved = if let Some(services) = services {
            let uuids = NSArray::from_vec(services.into_iter().map(uuid_to_cbuuid).collect());
            unsafe {
                self.manager_lifetime.manager.as_ref().expect("radio manager")
                    .retrieveConnectedPeripheralsWithServices(&uuids)
            }
        } else {
            let identifiers = NSArray::from_vec(
                identifier
                    .into_iter()
                    .map(|id| objc2_foundation::NSUUID::from_bytes(*id.as_bytes()))
                    .collect(),
            );
            unsafe {
                self.manager_lifetime.manager.as_ref().expect("radio manager")
                    .retrievePeripheralsWithIdentifiers(&identifiers)
            }
        };
        let mut peers = Vec::new();
        for peripheral in retrieved.iter() {
            peers.push((
                nsuuid_to_uuid(&*unsafe { peripheral.identifier() }),
                unsafe { peripheral.name() }.map(|name| name.to_string()),
            ));
            if self
                .register_retrieved(peripheral.retain(), Some(&future))
                .await
                .is_err()
            {
                return;
            }
        }
        // Same FIFO as registrations: reply only once the public adapter cache
        // has accepted every identity; no polling and no fabricated discovery.
        let _ = enqueue_directory_event(
            &self.event_sender,
            CoreBluetoothEvent::DirectoryReady {
                peers,
                future: future.clone(),
            },
            &future,
        )
        .await;
    }

    fn disconnect_peripheral(&mut self, identity: NativeRequestIdentity, fut: CoreBluetoothReplyStateShared) {
        if !identity.admission.claim() {
            let (_, waker) = fut.lock().unwrap().try_set_reply_deferred(CoreBluetoothReply::Err("Disconnect cancelled before dispatch".to_owned()));
            if let Some(waker) = waker { waker.wake(); }
            return;
        }
        // Reject before queue submission when this actor has no native peer
        // for the exact logical owner.  The delegate lane performs the same
        // exact-owner check at dispatch time; this early boundary keeps a
        // retired request from being made observable as a replacement's
        // cancellation and gives radio-free hosts a deterministic answer.
        if !self.source_registry.get(identity.uuid()).is_some_and(|owner| Arc::ptr_eq(&owner, identity.owner())) {
            identity.admission.refuse_before_dispatch();
            let (_, waker) = fut.lock().unwrap().try_set_reply_deferred(CoreBluetoothReply::Failed(
                crate::PlatformError::new("corebluetooth", "disconnect-owner-retired", "The disconnect request's peer owner was retired before dispatch"),
            ));
            if let Some(waker) = waker { waker.wake(); }
            return;
        }
        if !self.peripherals.get(&identity.uuid()).is_some_and(|peripheral| Arc::ptr_eq(&peripheral.source_owner, identity.owner())) {
            identity.admission.refuse_before_dispatch();
            let (_, waker) = fut.lock().unwrap().try_set_reply_deferred(CoreBluetoothReply::Failed(
                crate::PlatformError::new("corebluetooth", "disconnect-peer-unavailable", "No exact native peer is available for this disconnect request"),
            ));
            if let Some(waker) = waker { waker.wake(); }
            return;
        }
        trace!("Scheduling exact delegate-queue disconnect for peripheral");
        if unsafe { super::central_delegate::schedule_delegate_disconnect(self.manager_lifetime.delegate_queue, self.source_registry.clone(), identity.clone(), fut.clone(), self.manager_lifetime.adapter_dispatch_fence.clone()) }.is_err() {
            identity.admission.refuse_before_dispatch();
            let error = crate::PlatformError::new(
                "corebluetooth",
                "disconnect-delegate-unavailable",
                "The CoreBluetooth delegate queue is unavailable",
            );
            let (_, waker) = fut.lock().unwrap().try_set_reply_deferred(CoreBluetoothReply::Failed(error));
            if let Some(waker) = waker { waker.wake(); }
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

    fn marker_write(
        &mut self,
        peripheral_uuid: Uuid,
        expected: (String, u64, u64),
        group: crate::common::util::SourceGroupHandle,
        request: crate::common::util::SourceWriteRequest,
        result: tokio::sync::oneshot::Sender<
            std::result::Result<
                crate::common::util::SourceWriteReceipt,
                crate::common::util::SourceGroupError,
            >,
        >,
    ) {
        use crate::common::util::{SourceGroupError, SourceWriteStart};
        let retained_request = request.clone();
        let outcome = (|| {
            let peripheral = self
                .peripherals
                .get_mut(&peripheral_uuid)
                .ok_or(SourceGroupError::SourceStateUnavailable)?;
            let owner = peripheral.source_owner.clone();
            if let Some(receipt) = owner.faults.replay_source_marker_write(&group, &request)? {
                if !matches!(receipt.status(), crate::common::util::SourceWriteStatus::Pending) {
                    return Ok((receipt, None, Vec::new(), None));
                }
            }
            let mut guard = owner.reserve_marker_operation()?;
            let service = AttrKey {
                uuid: request.route.service_uuid,
                instance: request.route.service_instance,
            };
            let characteristic = AttrKey {
                uuid: request.route.characteristic_uuid,
                instance: request.route.characteristic_instance,
            };
            let selected = peripheral
                .services
                .get_mut(&service)
                .and_then(|service| service.characteristics.get_mut(&characteristic))
                .ok_or(SourceGroupError::SourceStateUnavailable)?;
            let value = request.value.clone();
            let mode = request.write_type;
            let start = owner.start_marker_transaction(&group, &expected, request, || {
                if mode == WriteType::WithoutResponse
                    && !unsafe { peripheral.peripheral.canSendWriteWithoutResponse() }
                {
                    return Err(crate::PlatformError::new(
                        "corebluetooth",
                        "write-not-ready",
                        "Write without response is not ready",
                    ));
                }
                let future = CoreBluetoothReplyFuture::default();
                let kind = match mode {
                    WriteType::WithResponse => {
                        CBCharacteristicWriteType::CBCharacteristicWriteWithResponse
                    }
                    WriteType::WithoutResponse => {
                        CBCharacteristicWriteType::CBCharacteristicWriteWithoutResponse
                    }
                };
                unsafe {
                    peripheral.peripheral.writeValue_forCharacteristic_type(
                        &NSData::from_vec(value),
                        &selected.characteristic,
                        kind,
                    );
                }
                if mode == WriteType::WithResponse {
                    selected
                        .write_future_state
                        .push_front(future.get_state_clone());
                    Ok(Some(future))
                } else {
                    Ok(None)
                }
            })?;
            Ok(match start {
                SourceWriteStart::Replay(receipt) => (receipt, None, Vec::new(), Some(guard)),
                SourceWriteStart::Rejected { receipt, wake }
                | SourceWriteStart::InitiationFailed { receipt, wake } => (receipt, None, wake, Some(guard)),
                SourceWriteStart::Initiated { receipt, operation, wake } => {
                    guard.dispatched = true;
                    (receipt, Some(operation), wake, Some(guard))
                },
            })
        })();
        match outcome {
            Ok((receipt, operation, wake, mut guard)) => {
                if let Some(operation) = operation {
                    match operation {
                        Some(future) => {
                            let retained = receipt.clone();
                            tokio::spawn(async move {
                                let mut operation_guard = guard;
                                let result = match future.await {
                                    CoreBluetoothReply::Ok => Ok(()),
                                    CoreBluetoothReply::Failed(error) => Err(error),
                                    CoreBluetoothReply::Err(error) => {
                                        Err(crate::PlatformError::new(
                                            "corebluetooth",
                                            "write-error",
                                            error,
                                        ))
                                    }
                                    _ => Err(crate::PlatformError::new(
                                        "corebluetooth",
                                        "unexpected-reply",
                                        "Unexpected write reply",
                                    )),
                                };
                                if let Some(guard) = operation_guard.as_mut() { guard.settled = result.is_ok(); }
                                retained.settle(result);
                            });
                        }
                        None => { if let Some(guard) = guard.as_mut() { guard.settled = true; } receipt.settle(Ok(())); },
                    }
                }
                for consumer in wake {
                    consumer.wake();
                }
                let _ = result.send(Ok(receipt));
            }
            Err(error) => {
                let retained = group.settle_pending_not_admitted(&retained_request, error);
                let _ = result.send(retained);
            }
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
                    peripheral
                        .source_owner
                        .waiters
                        .lock()
                        .unwrap()
                        .read((service_uuid, characteristic_uuid), fut);
                    unsafe {
                        peripheral
                            .peripheral
                            .readValueForCharacteristic(&characteristic.characteristic);
                    }
                }
            }
        }
    }

    fn acquire_notification_owner(
        &mut self,
        peripheral_uuid: Uuid,
        service_uuid: AttrKey,
        characteristic_uuid: AttrKey,
        fut: CoreBluetoothReplyStateShared,
    ) {
        let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) else {
            fail(fut, invalid_activation_error());
            return;
        };
        let state = peripheral.source_owner.waiters.lock().unwrap();
        if !state.ready {
            fail(fut, invalid_activation_error());
            return;
        }
        let Some(characteristic) = peripheral
            .services
            .get(&service_uuid)
            .and_then(|service| service.characteristics.get(&characteristic_uuid))
        else {
            fail(fut, invalid_activation_error());
            return;
        };
        let result = acquire_native_activation(
            &mut peripheral.activation_owners,
            &NEXT_NATIVE_ACTIVATION_ID,
            characteristic.characteristic.clone(),
            state.revision,
        );
        match result {
            Ok(token) => fut
                .lock()
                .unwrap()
                .set_reply(CoreBluetoothReply::NotificationOwner(token)),
            Err(error) => fail(fut, error),
        }
    }

    fn set_notification_owner(
        &mut self,
        peripheral_uuid: Uuid,
        token: Arc<NativeActivationToken>,
        enable: bool,
        fut: CoreBluetoothReplyStateShared,
    ) {
        let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) else {
            fail(fut, invalid_activation_error());
            return;
        };
        let mut state = peripheral.source_owner.waiters.lock().unwrap();
        let (route, object) = match resolve_native_activation(
            &peripheral.activation_owners,
            &peripheral.services,
            &token,
            state.ready,
            state.revision,
        ) {
            Ok(value) => value,
            Err(error) => {
                fail(fut, error);
                return;
            }
        };
        let result = if enable {
            state.enable(route, fut.clone())
        } else {
            state.disable(route, fut.clone())
        };
        if result.is_err() {
            fail(
                fut,
                crate::PlatformError::new(
                    "corebluetooth",
                    "notification-cleanup-pending",
                    "The previous notification cleanup is pending",
                ),
            );
            return;
        }
        unsafe {
            peripheral
                .peripheral
                .setNotifyValue_forCharacteristic(enable, &object);
        }
        drop(state);
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

    fn on_discovery_drained(&mut self, peripheral_uuid: Uuid) {
        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
            peripheral.discovery_busy = false;
        }
        if let Some(owner) = self.source_registry.get(peripheral_uuid) {
            owner.with_waiters(|state| state.discovery_actor_pending = false);
        }
    }

    fn discover_services(&mut self, peripheral_uuid: Uuid, fut: CoreBluetoothReplyStateShared) {
        if let Some(p) = self.peripherals.get_mut(&peripheral_uuid) {
            trace!("Discovering services!");
            if p.discovery_busy {
                fail(
                    fut,
                    crate::PlatformError::new(
                        "corebluetooth",
                        "discovery-in-progress",
                        "The previous discovery still owns native callbacks",
                    ),
                );
                return;
            }
            if let Err(error) = self.manager_lifetime.delegate.begin_discovery(peripheral_uuid) {
                fail(fut, error);
                return;
            }
            p.discovery_busy = true;
            p.source_owner.with_waiters(|state| state.discovery_actor_pending = true);
            p.services_discovered_future_state = Some(fut);
            // This will trigger the delegate_peripheral_diddiscoverservices in central_delegate.rs
            unsafe { p.peripheral.discoverServices(None) };
        }
    }

    async fn wait_for_message(&mut self) {
        select! {
            delegate_msg = self.delegate_receiver.select_next_some() => {
                let (_adapter_ticket, delegate_msg) = match delegate_msg {
                    CentralDelegateEvent::AdapterTicketed { ticket, event } => (Some(ticket), *event),
                    other => (None, other),
                };
                let (ticket_owner, delegate_msg) = match delegate_msg {
                    CentralDelegateEvent::Ticketed { peer, ticket, event } => {
                        let current = self.source_registry.get(peer).is_some_and(|owner| Arc::ptr_eq(&owner, &ticket.owner));
                        // A deferred disconnect lifecycle remains owned by
                        // the old graph after UUID replacement.  It is the
                        // one terminal callback which can safely cross this
                        // boundary with that old owner: the handler below
                        // checks the owner before touching the actor map.
                        if !current && !matches!(&*event, CentralDelegateEvent::DisconnectedDevice { .. }) {
                            ticket.owner.fail_source(crate::PlatformError::new("corebluetooth", "callback-owner-retired", "A callback belonged to a retired native peer owner"), None);
                            return;
                        }
                        (Some(ticket.owner.clone()), *event)
                    }
                    other => (None, other),
                };
                match delegate_msg {
                    CentralDelegateEvent::Ticketed { .. } | CentralDelegateEvent::AdapterTicketed { .. } => unreachable!("nested actor event ticket"),
                    // TODO We should probably also register some sort of
                    // "ready" variable in our adapter that will cause scans/etc
                    // to fail if this hasn't updated.
                    CentralDelegateEvent::AdapterBarrierFailed { error } => { self.fail_actor(&error); }
                    CentralDelegateEvent::ScanIngressFence { epoch, fence_id } => {
                        self.dispatch_event(CoreBluetoothEvent::ScanIngressFence { epoch, fence_id }).await;
                    }
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
                    CentralDelegateEvent::DiscoveredPeripheral{cbperipheral, advertisement_name, native_token} => {
                        self.on_discovered_peripheral(cbperipheral, advertisement_name, native_token).await
                    }
                    CentralDelegateEvent::DiscoveredServices{peripheral_uuid, services} => {
                        self.on_discovered_services(peripheral_uuid, services)
                    }
                    CentralDelegateEvent::DiscoveryDrained { peripheral_uuid } => {
                        self.on_discovery_drained(peripheral_uuid);
                    }
                    CentralDelegateEvent::DiscoveryFailed { peripheral_uuid, error } => {
                        if let Some(peripheral) = self.peripherals.get_mut(&peripheral_uuid) {
                            if let Some(future) = peripheral.services_discovered_future_state.take() { fail(future, error); }
                        }
                    }
                    CentralDelegateEvent::DiscoveredIncludedServices { peripheral_uuid, service_uuid, added, result } => {
                        self.on_discovered_included_services(peripheral_uuid, service_uuid, added, result)
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
                    CentralDelegateEvent::UnassociatedConnectionObservation { peripheral_uuid, error } => {
                        // Do not let a UUID-only callback settle a replacement
                        // connection.  It remains a typed, ordered source
                        // uncertainty event for the adapter instead of a log
                        // line that silently loses the observation.
                        self.dispatch_event(CoreBluetoothEvent::DeviceSourceUncertain {
                            uuid: peripheral_uuid,
                            error,
                        }).await;
                    }
                    CentralDelegateEvent::DisconnectedDevice{peripheral_uuid, error} => {
                        self.on_peripheral_disconnect_owned(peripheral_uuid, error, ticket_owner.as_ref()).await
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
                        cleanup,
                     } => self.on_characteristic_unsubscribed(peripheral_uuid, service_uuid,characteristic_uuid, cleanup).await,
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
                    CentralDelegateEvent::Advertised {
                        peripheral_uuid,
                        report,
                        advertisement_name,
                        ingress_epoch,
                    } => {
                        // UBM patch (UBM_PATCHES.md #17, finding 205): label
                        // a nameless sighting with the last advertised name
                        // instead of the GAP alias, so name-filtered scans
                        // keep reporting a peer the OS replays namelessly.
                        // A named packet refreshes the stash (a rename
                        // included); without any advertised name yet the
                        // report keeps its GAP seed.
                        let mut report = report;
                        if let Some(entry) = self.peripherals.get_mut(&peripheral_uuid) {
                            let (stash, name) = fold_sighting_name(
                                entry.advertised_name.clone(),
                                advertisement_name,
                                report.local_name.clone(),
                            );
                            entry.advertised_name = stash;
                            report.local_name = name;
                        }
                        self.dispatch_event(CoreBluetoothEvent::Advertised {
                            uuid: peripheral_uuid,
                            report,
                            ingress_epoch,
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
                    } => self.on_attribute_failed(peripheral_uuid, service_uuid, characteristic_uuid, descriptor_uuid, stage, error).await,
                };
            }
            adapter_msg = self.message_receiver.select_next_some() => {
                trace!("Adapter message!");
                self.directory_replies.retain(|reply| !reply.lock().unwrap().is_completed());
                self.active_reply = adapter_msg.reply();
                if matches!(&adapter_msg, CoreBluetoothMessage::DirectoryLookup { .. }) && self.directory_replies.len() >= 256 {
                    if let Some(reply) = self.active_reply.take() {
                        fail(reply, crate::PlatformError::new("corebluetooth", "directory-capacity", "The pending directory reply capacity is full"));
                    }
                    return;
                }
                if matches!(&adapter_msg, CoreBluetoothMessage::DirectoryLookup { .. }) {
                    if let Some(reply) = self.active_reply.clone() { self.directory_replies.push(reply); }
                }
                match adapter_msg {
                    CoreBluetoothMessage::GetAdapterState { future } => {
                        self.get_adapter_state(future);
                    },
                    CoreBluetoothMessage::StartScanning{filter, future} => self.start_discovery(filter, future),
                    CoreBluetoothMessage::ScanIngress { action, epoch, fence_id, future } => {
                        self.scan_ingress(action, epoch, fence_id, future);
                    }
                    CoreBluetoothMessage::DirectoryLookup {services, identifier, future} => self.directory_lookup(services, identifier, future).await,
                    CoreBluetoothMessage::StopScanning => self.stop_discovery(),
                    CoreBluetoothMessage::ConnectDevice{identity, future} => {
                        trace!("got connectdevice msg!");
                        self.connect_peripheral(identity, future).await;
                    }
                    CoreBluetoothMessage::DisconnectDevice{identity, future} => {
                        self.disconnect_peripheral(identity, future);
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
                    CoreBluetoothMessage::ReleaseReclaim { peripheral_uuid, source_owner, key, result } => {
                        match unsafe { super::central_delegate::schedule_delegate_replay(self.manager_lifetime.delegate_queue, self.source_registry.clone(), source_owner, peripheral_uuid, key) } {
                            Ok(reply) => { tokio::spawn(async move {
                                let answer = reply.await.unwrap_or(Err(crate::ubm_reclaim_authority::AuthorityError::Unknown));
                                let _ = result.send(answer);
                            }); },
                            Err(error) => { let _ = result.send(Err(error)); },
                        }
                    }
                    CoreBluetoothMessage::MarkerWrite { peripheral_uuid, expected, group, request, result } => {
                        self.marker_write(peripheral_uuid, expected, group, request, result)
                    }
                    CoreBluetoothMessage::AcquireNotificationOwner{peripheral_uuid, service_uuid,characteristic_uuid, future} => {
                        self.acquire_notification_owner(peripheral_uuid, service_uuid, characteristic_uuid, future)
                    }
                    CoreBluetoothMessage::SetNotificationOwner{peripheral_uuid, token, enable, future} => {
                        self.set_notification_owner(peripheral_uuid, token, enable, future)
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
                self.active_reply = None;
            }
        }
    }

    fn fail_actor(&mut self, error: &crate::PlatformError) {
        let reply = CoreBluetoothReply::Failed(error.clone());
        if let Some(active) = self.active_reply.take() {
            active.lock().unwrap().try_set_reply(reply.clone());
        }
        for pending in self.directory_replies.drain(..) {
            pending.lock().unwrap().try_set_reply(reply.clone());
        }
        for peripheral in self.peripherals.values_mut() {
            peripheral.source_owner.with_waiters(|state| {
                if state.discovery_actor_pending {
                    state.discovery_actor_pending = false;
                    state.reclaim_uncertain = true;
                }
            });
            peripheral.disconnected_future_state.complete(reply.clone());
            if let Some(pending) = peripheral.connected_future_state.take() {
                pending.lock().unwrap().try_set_reply(reply.clone());
            }
            if let Some(pending) = peripheral.services_discovered_future_state.take() {
                pending.lock().unwrap().try_set_reply(reply.clone());
            }
            for pending in peripheral.read_rssi_future_state.drain(..) {
                pending.lock().unwrap().try_set_reply(reply.clone());
            }
            for pending in peripheral.write_without_response_queue.drain(..) {
                pending.fut.lock().unwrap().try_set_reply(reply.clone());
            }
            fail_attribute_waiters(&mut peripheral.services, &reply);
            peripheral.services.clear();
        }
    }

    fn get_adapter_state(&mut self, fut: CoreBluetoothReplyStateShared) {
        let state = unsafe { self.manager_lifetime.manager.as_ref().expect("radio manager").state() };
        fut.lock()
            .unwrap()
            .set_reply(CoreBluetoothReply::AdapterState(state))
    }

    fn start_discovery(&mut self, filter: ScanFilter, fut: CoreBluetoothReplyStateShared) {
        trace!("BluetoothAdapter::start_discovery");
        // UBM patch (UBM_PATCHES.md #8): CoreBluetooth ignores a scan
        // requested while the manager is not powered on; refuse it instead
        // of reporting a scan that never runs.
        let state = unsafe { self.manager_lifetime.manager.as_ref().expect("radio manager").state() };
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
            self.manager_lifetime.manager.as_ref().expect("radio manager")
                .scanForPeripheralsWithServices_options(service_uuids.as_deref(), Some(&options))
        };
        fut.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
    }

    fn stop_discovery(&mut self) {
        trace!("BluetoothAdapter::stop_discovery");
        if let Some(manager) = &self.manager_lifetime.manager { unsafe { manager.stopScan() }; }
    }

    fn scan_ingress(
        &mut self,
        action: super::central_delegate::ScanIngressAction,
        epoch: u64,
        fence_id: Option<u64>,
        future: CoreBluetoothReplyStateShared,
    ) {
        let queued = unsafe {
            super::central_delegate::schedule_scan_ingress(
                self.manager_lifetime.delegate_queue,
                self.source_registry.clone(),
                action,
                epoch,
                fence_id,
                future.clone(),
            )
        };
        if queued.is_err() {
            future.lock().unwrap().set_reply(CoreBluetoothReply::Failed(
                crate::PlatformError::new(
                    "corebluetooth",
                    "scan-ingress-delegate-unavailable",
                    "The CoreBluetooth delegate queue is unavailable",
                ),
            ));
        }
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
        // Return the original receiver, including already admitted events, to
        // the manager before this actor's other state is destroyed.
        let (_, empty) = mpsc::channel(1);
        let receiver = std::mem::replace(&mut self.delegate_receiver, empty.fuse());
        *self.manager_lifetime.delegate_receiver.borrow_mut() = Some(receiver);
        *self.manager_lifetime.peripherals.borrow_mut() = std::mem::take(&mut self.peripherals);
        *self.manager_lifetime.directory_replies.borrow_mut() = std::mem::take(&mut self.directory_replies);
        *self.manager_lifetime.active_reply.borrow_mut() = self.active_reply.take();
        if let Some(failure) = self.failure_receiver.take() {
            *self.manager_lifetime.failure_receiver.borrow_mut() = Some(failure);
        }
    }
}

impl Drop for CoreBluetoothManagerLifetime {
    fn drop(&mut self) {
        trace!("CoreBluetoothManagerLifetime::drop");
        if let Some(manager) = &self.manager { unsafe { manager.stopScan() }; }
        self.source_registry.queue_closed.store(true, std::sync::atomic::Ordering::Release);
        unsafe {
            super::central_delegate::schedule_delegate_queue_cleanup(self.delegate_queue);
            ffi::dispatch_release(self.delegate_queue);
        }
    }
}

pub fn run_corebluetooth_thread(
    event_sender: Sender<CoreBluetoothEvent>,
    source_registry: Arc<PeerSourceRegistry>,
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
            let manager_lifetime = CoreBluetoothManagerLifetime::new(source_registry, true);
            let mut cbi = CoreBluetoothInternal::attach(manager_lifetime.clone(), receiver, event_sender);
            use futures::FutureExt;
            let Some(failure) = cbi.failure_receiver.take() else {
                return;
            };
            let mut failure = failure.fuse();
            loop {
                futures::select! {
                    terminal = failure => {
                        if let Ok(error) = terminal {
                            cbi.fail_actor(&error);
                            while let Some(message) = cbi.message_receiver.next().await {
                                message.fail(&error);
                            }
                        }
                        break;
                    }
                    _ = cbi.wait_for_message().fuse() => {}
                }
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
    use super::{api_services, keyed, AttrKey, ServiceInternal};
    use crate::api::CharPropFlags;
    use objc2::rc::Retained;
    use objc2::ClassType;
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
        let internal = super::CoreBluetoothInternal::new_without_radio(message_receiver, event_sender);
        (internal, event_receiver)
    }

    #[tokio::test]
    async fn unassociated_native_callback_reaches_the_typed_adapter_diagnostic_stream() {
        use futures::StreamExt;
        use super::super::central_delegate::CentralDelegateEvent;

        let (mut actor, mut events) = headless_internal();
        let peer = uuid::Uuid::from_u128(0xCB0B);
        let error = crate::PlatformError::new(
            "corebluetooth",
            "unassociated-fixture",
            "callback had no admitted native cycle",
        );
        actor.manager_lifetime.delegate.send_event(
            CentralDelegateEvent::UnassociatedConnectionObservation {
                peripheral_uuid: peer,
                error: Some(error.clone()),
            },
        );

        actor.wait_for_message().await;

        assert!(matches!(events.next().await,
            Some(super::CoreBluetoothEvent::DeviceSourceUncertain {
                uuid,
                error: Some(observed),
            }) if uuid == peer && observed == error
        ));
    }

    #[test]
    fn manager_lifetime_retains_admitted_delegate_events_across_actor_replacement() {
        use futures::StreamExt;
        use super::super::central_delegate::CentralDelegateEvent;

        let (messages, receiver) = futures::channel::mpsc::channel(8);
        let (events, _event_receiver) = futures::channel::mpsc::channel(8);
        let actor = super::CoreBluetoothInternal::new_without_radio(receiver, events.clone());
        let lifetime = actor.manager_lifetime.clone();
        let peer = uuid::Uuid::from_u128(31);
        let old_owner = lifetime.source_registry.get_or_create(peer);
        let native = crate::PlatformError::new("corebluetooth", "7", "link lost");
        lifetime.delegate.send_event(CentralDelegateEvent::DisconnectedDevice { peripheral_uuid: peer, error: Some(native.clone()) });
        lifetime.delegate.send_event(CentralDelegateEvent::DidUpdateState { state: objc2_core_bluetooth::CBManagerState::Resetting });
        lifetime.source_registry.remove_if_current(peer, &old_owner);
        let replacement_owner = lifetime.source_registry.get_or_create(peer);
        drop(actor);
        assert!(!lifetime.source_registry.queue_closed.load(std::sync::atomic::Ordering::Acquire));
        let (_replacement_messages, replacement_receiver) = futures::channel::mpsc::channel(8);
        let mut replacement = super::CoreBluetoothInternal::attach(lifetime.clone(), replacement_receiver, events);
        let received = futures::executor::block_on(replacement.delegate_receiver.next()).unwrap();
        let CentralDelegateEvent::Ticketed { ticket, event, .. } = received else { panic!("admitted event lost its owner") };
        assert!(std::sync::Arc::ptr_eq(&ticket.owner, &old_owner));
        assert!(!std::sync::Arc::ptr_eq(&ticket.owner, &replacement_owner));
        assert!(matches!(*event, CentralDelegateEvent::DisconnectedDevice { peripheral_uuid, error: Some(error) } if peripheral_uuid == peer && error == native));
        let reset = futures::executor::block_on(replacement.delegate_receiver.next()).unwrap();
        assert!(matches!(reset, CentralDelegateEvent::AdapterTicketed { event, .. }
            if matches!(*event, CentralDelegateEvent::DidUpdateState { state: objc2_core_bluetooth::CBManagerState::Resetting })));
        drop(replacement);
        drop(lifetime);
        drop(messages);
    }

    #[test]
    fn manager_lifetime_retains_actor_waiters_and_peripheral_table_across_replacement() {
        let (_messages, receiver) = futures::channel::mpsc::channel(8);
        let (events, _event_receiver) = futures::channel::mpsc::channel(8);
        let mut actor = super::CoreBluetoothInternal::new_without_radio(receiver, events.clone());
        let lifetime = actor.manager_lifetime.clone();
        let reply = super::CoreBluetoothReplyFuture::default().get_state_clone();
        actor.directory_replies.push(reply.clone());
        actor.peripherals.reserve(16);
        let capacity = actor.peripherals.capacity();
        drop(actor);
        let (_messages, receiver) = futures::channel::mpsc::channel(8);
        let replacement = super::CoreBluetoothInternal::attach(lifetime, receiver, events);
        assert!(std::sync::Arc::ptr_eq(&replacement.directory_replies[0], &reply));
        assert_eq!(replacement.peripherals.capacity(), capacity);
    }

    /// Finding 127 (CoreBluetooth): an identifier CoreBluetooth does not
    /// know reports not-found through the resolve path
    /// (`retrievePeripheralsWithIdentifiers`, as the legacy addon
    /// reconnected without a scan) — never a scan, never a hang.
    #[test]
    #[ignore = "requires a real CoreBluetooth manager and peripheral retrieval"]
    fn an_unknown_identifier_resolves_without_a_scan() {
        let (_messages, receiver) = futures::channel::mpsc::channel(8);
        let (events, _event_receiver) = futures::channel::mpsc::channel(8);
        let mut internal = super::CoreBluetoothInternal::new(receiver, events);
        let uuid = uuid::Uuid::parse_str(UNKNOWN_PEER).expect("fixture uuid");
        assert!(
            !futures::executor::block_on(internal.retrieve_peripheral(uuid)).unwrap(),
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
        futures::executor::block_on(internal.on_peripheral_disconnect(uuid, None));
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
                    included_services: Some(Vec::new()),
                };
                service.merge_characteristics(characteristics);
                (key, service)
            })
            .collect()
    }

    #[test]
    fn inclusion_metadata_preserves_same_uuid_target_and_unknowns() {
        let mut table = discovered();
        let mut keys: Vec<_> = table.keys().copied().collect();
        keys.sort();
        table.get_mut(&keys[0]).unwrap().included_services = Some(vec![keys[1]]);
        let graph: Vec<_> = api_services(&table).into_iter().collect();
        assert_eq!(
            graph[0].included_services.as_ref().unwrap()[0].instance,
            graph[1].instance
        );
        assert_eq!(graph[1].included_services, Some(Vec::new()));
        table.get_mut(&keys[0]).unwrap().included_services = None;
        assert_eq!(
            api_services(&table)
                .into_iter()
                .next()
                .unwrap()
                .included_services,
            None
        );
    }

    /// UBM patch #19 (finding 130): at a disconnect every attribute waiter
    /// is answered, descriptor reads and writes included (upstream left
    /// them, and the discovery, waiting forever), as the legacy addon's
    /// `failPendingForDevice` did.
    #[test]
    fn a_disconnect_answers_every_attribute_waiter() {
        use super::{
            fail_attribute_waiters, CoreBluetoothReply, CoreBluetoothReplyFuture,
            DescriptorInternal,
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
        characteristic.unsubscribe_future_state.push_front(waiter());
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
        assert_eq!(waiters.len(), 5);
        for future in waiters {
            assert!(matches!(
                futures::executor::block_on(future),
                CoreBluetoothReply::Err(detail) if detail == "gone"
            ));
        }
    }

    #[test]
    fn unsubscribe_is_owned_until_the_notification_state_answer() {
        use futures::FutureExt;
        let mut services = discovered();
        let characteristic = services
            .values_mut()
            .next()
            .unwrap()
            .characteristics
            .values_mut()
            .next()
            .unwrap();
        let mut answer = super::CoreBluetoothReplyFuture::default();
        characteristic
            .unsubscribe_future_state
            .push_front(answer.get_state_clone());
        assert!((&mut answer).now_or_never().is_none());
        assert_eq!(characteristic.unsubscribe_future_state.len(), 1);
        characteristic.answer_notification_state(true);
        assert!(
            (&mut answer).now_or_never().is_none(),
            "a still-notifying answer cannot confirm disable"
        );
        assert_eq!(characteristic.unsubscribe_future_state.len(), 1);
        characteristic.answer_notification_state(false);
        assert!(matches!(
            answer.now_or_never(),
            Some(super::CoreBluetoothReply::Ok)
        ));
        assert!(characteristic.unsubscribe_future_state.is_empty());
        // A late duplicate callback does not answer the already retired waiter again.
        characteristic.answer_notification_state(false);
    }

    #[test]
    fn rejected_notification_state_answers_unsubscribe_without_forgetting_it_early() {
        use futures::FutureExt;
        let mut services = discovered();
        let characteristic = services
            .values_mut()
            .next()
            .unwrap()
            .characteristics
            .values_mut()
            .next()
            .unwrap();
        let mut answer = super::CoreBluetoothReplyFuture::default();
        characteristic
            .unsubscribe_future_state
            .push_front(answer.get_state_clone());
        assert!((&mut answer).now_or_never().is_none());
        let refusal = crate::PlatformError::new("corebluetooth", "42", "native disable refused");
        characteristic.fail_notification_state(refusal.clone());
        assert!(
            matches!(answer.now_or_never(), Some(super::CoreBluetoothReply::Failed(error)) if error == refusal)
        );
        assert!(characteristic.unsubscribe_future_state.is_empty());
    }

    #[test]
    fn retained_activation_follows_native_object_across_duplicate_reordering() {
        use super::{acquire_native_activation, resolve_native_activation};
        let mut services = discovered();
        let service_key = *services.keys().min().unwrap();
        let service = services.get_mut(&service_key).unwrap();
        let mut keys: Vec<_> = service.characteristics.keys().copied().collect();
        keys.sort();
        let original = service.characteristics[&keys[0]].characteristic.clone();
        let replacement = service.characteristics[&keys[1]].characteristic.clone();
        let mut owners = HashMap::new();
        let next_id = std::sync::atomic::AtomicU64::new(0);
        let old = acquire_native_activation(&mut owners, &next_id, original.clone(), 3).unwrap();
        let other = acquire_native_activation(&mut owners, &next_id, replacement, 3).unwrap();
        assert_ne!(old.identity.id, other.identity.id);
        let first = service.characteristics.remove(&keys[0]).unwrap();
        let second = service.characteristics.remove(&keys[1]).unwrap();
        service.characteristics.insert(keys[0], second);
        service.characteristics.insert(keys[1], first);
        let (route, selected) =
            resolve_native_activation(&owners, &services, &old, true, 3).unwrap();
        assert_eq!(route, (service_key, keys[1]));
        assert!(std::ptr::eq(&*selected, &*original));
        assert!(resolve_native_activation(&owners, &services, &old, true, 4).is_err());
        let current =
            acquire_native_activation(&mut owners, &next_id, original.clone(), 4).unwrap();
        assert_eq!(current.identity.id, old.identity.id);
        assert!(resolve_native_activation(&owners, &services, &current, true, 4).is_ok());
        services
            .get_mut(&service_key)
            .unwrap()
            .characteristics
            .remove(&keys[1]);
        assert!(resolve_native_activation(&owners, &services, &current, true, 4).is_err());
        assert!(resolve_native_activation(&owners, &services, &other, true, 3).is_ok());
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

#[cfg(test)]
mod peer_source_registry_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_before_detached_poll_releases_capacity_without_actor() {
        use crate::api::Peripheral as _;
        use futures::{FutureExt, poll};
        use std::sync::Weak;

        let (messages, mut requests) = mpsc::channel(512);
        let (events_sender, events) = mpsc::channel(4);
        let owner = PeerSourceRegistry::default().get_or_create(Uuid::nil());
        let peripheral = super::super::peripheral::Peripheral::new_with_source_owner(
            Uuid::nil(), None, None, Weak::new(), events, messages, owner.clone(),
        );
        for _ in 0..256 {
            let mut connect = Box::pin(peripheral.connect());
            assert!(poll!(connect.as_mut()).is_pending());
            drop(connect);
        }
        let admitted = owner.waiters.lock().unwrap().reclaim_operations;
        for _ in 0..512 {
            if owner.waiters.lock().unwrap().reclaim_operations == 0 { break; }
            tokio::task::yield_now().await;
        }
        let (remaining, uncertain) = {
            let state = owner.waiters.lock().unwrap();
            (state.reclaim_operations, state.reclaim_uncertain)
        };
        let submitted = requests.next().now_or_never().is_some();
        drop(requests);
        while owner.waiters.lock().unwrap().reclaim_operations != 0 {
            tokio::task::yield_now().await;
        }
        let weak_owner = Arc::downgrade(&owner);
        drop(events_sender);
        drop(peripheral);
        drop(owner);
        for _ in 0..512 {
            if weak_owner.upgrade().is_none() { break; }
            tokio::task::yield_now().await;
        }
        assert_eq!(admitted, 256);
        assert_eq!(remaining, 0);
        assert!(!uncertain);
        assert!(!submitted, "cancelled tasks must not submit");
        assert!(weak_owner.upgrade().is_none(), "cancelled tasks must release their owner");
    }

    #[tokio::test]
    async fn cancelled_queued_connect_settles_certain_and_allows_reclaim() {
        use crate::api::Peripheral as _;
        use crate::ubm_reclaim_authority::{GraphKey, GraphTarget, PermitStatus};
        use std::sync::Weak;

        let (messages, mut requests) = mpsc::channel(8);
        let (_actor_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(4);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let peer = Uuid::from_u128(42);
        let owner = actor.source_registry.get_or_create(peer);
        let graph = ("corebluetooth".to_owned(), owner.graph_attachment.unwrap(), 1);
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: graph.clone(), permit_id: 1 };
        let target = GraphTarget { peer_id: key.peer_id.clone(), connection_generation: key.connection_generation.clone(), graph: graph.clone() };
        let binding = {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true; state.revision = 1;
            state.reclaim_binding.attach(); state.reclaim_binding.publish(graph.clone());
            state.reclaim_binding.capture(key.peer_id.clone(), key.connection_generation.clone(), &graph).unwrap()
        };
        {
            let mut authority = owner.authorities.lock().unwrap();
            authority.reserve_open(1).unwrap(); authority.resolve_open(1, true).unwrap();
            authority.reserve_permit_token_bound(1, 1, target.clone(), &binding).unwrap();
            authority.resolve_permit_token(1, 1, &target, true).unwrap();
        }
        let (_peripheral_events, peripheral_events) = mpsc::channel(4);
        let peripheral = super::super::peripheral::Peripheral::new_with_source_owner(
            peer, None, None, Weak::new(), peripheral_events, messages, owner.clone(),
        );
        let caller = tokio::spawn(async move { peripheral.connect().await });
        let CoreBluetoothMessage::ConnectDevice { identity, future } = requests.next().await.unwrap()
            else { panic!("expected connect request") };
        caller.abort();
        let _ = caller.await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while owner.waiters.lock().unwrap().reclaim_operations != 0 { tokio::task::yield_now().await; }
        }).await.expect("cancelled operation waited for stalled actor");
        let status = owner.acquire_reclaim(&binding, key);
        actor.connect_peripheral(identity, future).await;
        assert_eq!(status, Ok(PermitStatus::Acquired));
    }

    #[tokio::test]
    async fn retired_connect_request_does_not_retrieve_for_replacement_owner() {
        let (_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(4);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let peer = Uuid::from_u128(42);
        let old = actor.source_registry.get_or_create(peer);
        let identity = NativeRequestIdentity::new(peer, old.clone(), Arc::new(NativeRequestAdmission::default()));
        actor.source_registry.remove_if_current(peer, &old);
        let replacement = actor.source_registry.get_or_create(peer);
        let reply = CoreBluetoothReplyFuture::default();

        // A radio-free actor panics on retrieval: stale requests must be
        // resolved before looking up the replacement's native peripheral.
        actor.connect_peripheral(identity, reply.get_state_clone()).await;
        assert!(matches!(reply.await, CoreBluetoothReply::Failed(error) if error.code == "connect-owner-retired"));
        assert!(Arc::ptr_eq(&actor.source_registry.get(peer).unwrap(), &replacement));
        assert!(!actor.peripherals.contains_key(&peer));
    }

    #[test]
    fn native_connect_admission_requires_the_same_current_and_native_owner() {
        let registry = PeerSourceRegistry::default();
        let peer = Uuid::from_u128(42);
        let old = registry.get_or_create(peer);
        registry.remove_if_current(peer, &old);
        let replacement = registry.get_or_create(peer);
        let identity = NativeRequestIdentity::new(peer, replacement.clone(), Arc::new(NativeRequestAdmission::default()));
        let mut dispatched = false;

        assert!(!registry.dispatch_request_if_current(&identity, &old, || dispatched = true));
        assert!(!dispatched, "a stale native peripheral must not serve a replacement owner");
        assert!(registry.dispatch_request_if_current(&identity, &replacement, || {
            assert!(registry.adapter_admission.try_lock().is_err(), "retirement must remain excluded through dispatch");
            dispatched = true;
        }));
        assert!(dispatched);
        registry.remove_if_current(peer, &replacement);
        assert!(!registry.dispatch_request_if_current(&identity, &replacement, || panic!("retired request dispatched")));
    }

    #[test]
    fn disconnect_without_a_native_peer_settles_with_an_explicit_failure() {
        use futures::FutureExt;
        let (_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(4);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let reply = CoreBluetoothReplyFuture::default();

        let peer = Uuid::from_u128(42);
        let owner = actor.source_registry.get_or_create(peer);
        let identity = NativeRequestIdentity::new(peer, owner, Arc::new(NativeRequestAdmission::default()));
        actor.disconnect_peripheral(identity, reply.get_state_clone());

        assert!(matches!(reply.now_or_never(), Some(CoreBluetoothReply::Failed(error))
            if error.code == "disconnect-peer-unavailable"));
    }

    #[test]
    fn disconnect_observation_delivers_original_native_error_to_every_waiter() {
        use futures::FutureExt;
        let mut pending = DisconnectReplies::default();
        let first = CoreBluetoothReplyFuture::default();
        let second = CoreBluetoothReplyFuture::default();
        pending.admit(first.get_state_clone()).unwrap();
        pending.admit(second.get_state_clone()).unwrap();
        let native = crate::PlatformError::new("corebluetooth", "7", "link lost")
            .with("nsErrorDomain", "CBErrorDomain");

        pending.complete_observation(Some(native.clone()));

        assert!(matches!(first.now_or_never(), Some(CoreBluetoothReply::Failed(error)) if error == native));
        assert!(matches!(second.now_or_never(), Some(CoreBluetoothReply::Failed(error)) if error == native));
    }

    #[test]
    fn repeated_disconnect_requests_keep_each_reply_until_the_terminal_callback() {
        use futures::FutureExt;
        let mut pending = DisconnectReplies::default();
        let first = CoreBluetoothReplyFuture::default();
        let second = CoreBluetoothReplyFuture::default();
        assert_eq!(pending.admit(first.get_state_clone()), Ok(true));
        assert_eq!(pending.admit(second.get_state_clone()), Ok(false));
        assert!(!first.get_state_clone().lock().unwrap().is_completed());
        pending.complete(CoreBluetoothReply::Ok);
        assert!(matches!(first.now_or_never(), Some(CoreBluetoothReply::Ok)));
        assert!(matches!(second.now_or_never(), Some(CoreBluetoothReply::Ok)));
        let retry = CoreBluetoothReplyFuture::default();
        assert_eq!(pending.admit(retry.get_state_clone()), Ok(true));
        pending.complete(CoreBluetoothReply::Failed(crate::PlatformError::new("corebluetooth", "fixture", "failure")));
        assert!(matches!(retry.now_or_never(), Some(CoreBluetoothReply::Failed(error)) if error.code == "fixture"));
    }

    #[test]
    fn disconnect_waiters_are_bounded_and_wake_after_their_reply_lock_is_released() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Wake, Waker};

        struct CheckWake { state: CoreBluetoothReplyStateShared, unlocked: AtomicBool }
        impl Wake for CheckWake {
            fn wake(self: Arc<Self>) {
                self.unlocked.store(self.state.try_lock().is_ok(), Ordering::SeqCst);
            }
        }
        let mut pending = DisconnectReplies::default();
        let mut first = CoreBluetoothReplyFuture::default();
        let state = first.get_state_clone();
        let check = Arc::new(CheckWake { state: state.clone(), unlocked: AtomicBool::new(false) });
        let waker = Waker::from(check.clone());
        assert!(Pin::new(&mut first).poll(&mut Context::from_waker(&waker)).is_pending());
        assert_eq!(pending.admit(state), Ok(true));
        for _ in 1..256 {
            assert_eq!(pending.admit(CoreBluetoothReplyFuture::default().get_state_clone()), Ok(false));
        }
        let refused = CoreBluetoothReplyFuture::default();
        assert_eq!(pending.admit(refused.get_state_clone()), Err(()));
        assert!(!refused.get_state_clone().lock().unwrap().is_completed());
        pending.complete(CoreBluetoothReply::Ok);
        assert!(check.unlocked.load(Ordering::SeqCst));
    }

    #[test]
    fn disconnect_batch_installs_all_replies_before_waking_any_waiter() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Wake, Waker};

        struct CheckBatch { states: Vec<CoreBluetoothReplyStateShared>, ready: AtomicBool }
        impl Wake for CheckBatch {
            fn wake(self: Arc<Self>) {
                self.ready.store(self.states.iter().all(|state| {
                    state.try_lock().is_ok_and(|state| state.is_completed())
                }), Ordering::SeqCst);
            }
        }
        let mut pending = DisconnectReplies::default();
        let mut first = CoreBluetoothReplyFuture::default();
        let second = CoreBluetoothReplyFuture::default();
        let states = vec![first.get_state_clone(), second.get_state_clone()];
        let check = Arc::new(CheckBatch { states: states.clone(), ready: AtomicBool::new(false) });
        let waker = Waker::from(check.clone());
        assert!(Pin::new(&mut first).poll(&mut Context::from_waker(&waker)).is_pending());
        for state in states { pending.admit(state).unwrap(); }
        pending.complete(CoreBluetoothReply::Ok);
        assert!(check.ready.load(Ordering::SeqCst), "a waker observed an incomplete or locked batch member");
    }

    #[tokio::test]
    async fn cancelled_connect_reply_wakes_after_reply_unlock() {
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Wake, Waker};

        struct CheckWake { state: CoreBluetoothReplyStateShared, unlocked: AtomicBool }
        impl Wake for CheckWake {
            fn wake(self: Arc<Self>) {
                self.unlocked.store(self.state.try_lock().is_ok(), Ordering::SeqCst);
            }
        }
        let (_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(4);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let owner = actor.source_registry.get_or_create(Uuid::nil());
        let admission = Arc::new(NativeRequestAdmission::default());
        admission.cancel();
        let identity = NativeRequestIdentity::new(Uuid::nil(), owner, admission);
        let mut reply = Box::pin(CoreBluetoothReplyFuture::default());
        let state = reply.get_state_clone();
        let check = Arc::new(CheckWake { state: state.clone(), unlocked: AtomicBool::new(false) });
        let waker = Waker::from(check.clone());
        assert!(reply.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
        actor.connect_peripheral(identity, state).await;
        assert!(check.unlocked.load(Ordering::SeqCst), "reply mutex held during wake");
        assert!(matches!(reply.await, CoreBluetoothReply::Err(_)));
    }

    #[tokio::test]
    async fn cancelled_public_connect_is_not_dispatched_when_actor_receives_it() {
        use crate::api::Peripheral as _;
        use futures::FutureExt;
        use std::sync::Weak;

        let (messages, mut requests) = mpsc::channel(8);
        let (_actor_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(4);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let peer = Uuid::from_u128(42);
        let owner = actor.source_registry.get_or_create(peer);
        let (_peripheral_events, peripheral_events) = mpsc::channel(4);
        let peripheral = super::super::peripheral::Peripheral::new_with_source_owner(
            peer, None, None, Weak::new(), peripheral_events, messages, owner.clone(),
        );
        let caller = tokio::spawn(async move { peripheral.connect().await });
        let CoreBluetoothMessage::ConnectDevice { identity, future } = requests.next().await.unwrap()
            else { panic!("expected connect request") };
        caller.abort();
        let _ = caller.await;

        // With no radio in this actor, reaching native dispatch panics. A
        // cancelled queued request must be consumed before touching the radio.
        let dispatch = std::panic::AssertUnwindSafe(
            actor.connect_peripheral(identity, future.clone()),
        ).catch_unwind().await;
        future.lock().unwrap().try_set_reply(CoreBluetoothReply::Err("fixture shutdown".into()));
        assert!(dispatch.is_ok(), "cancelled queued connect reached native dispatch");
    }

    #[tokio::test]
    async fn cancelled_queued_public_disconnect_releases_its_operation_slot() {
        use crate::api::Peripheral as _;
        use std::sync::Weak;

        let owner = PeerSourceRegistry::default().get_or_create(Uuid::nil());
        let (messages, mut requests) = mpsc::channel(8);
        let (_events, events) = mpsc::channel(4);
        let peripheral = super::super::peripheral::Peripheral::new_with_source_owner(
            Uuid::nil(), None, None, Weak::new(), events, messages, owner.clone(),
        );
        let caller = tokio::spawn(async move { peripheral.disconnect().await });
        let CoreBluetoothMessage::DisconnectDevice { future, .. } = requests.next().await.unwrap()
            else { panic!("expected disconnect request") };
        caller.abort();
        let _ = caller.await;
        for _ in 0..32 { tokio::task::yield_now().await; }
        let retained = owner.waiters.lock().unwrap().reclaim_operations;
        future.lock().unwrap().try_set_reply(CoreBluetoothReply::Err("fixture shutdown".into()));
        assert_eq!(retained, 0, "a request cancelled before actor admission retained its slot");
    }

    #[tokio::test]
    async fn retired_public_disconnect_is_refused_before_peer_lookup() {
        use crate::api::Peripheral as _;
        use std::sync::Weak;

        let (messages, receiver) = mpsc::channel(8);
        let (events, _event_receiver) = mpsc::channel(4);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let peer = Uuid::from_u128(42);
        let old = actor.source_registry.get_or_create(peer);
        let (_peripheral_events, peripheral_events) = mpsc::channel(4);
        let peripheral = super::super::peripheral::Peripheral::new_with_source_owner(
            peer, None, None, Weak::new(), peripheral_events, messages, old.clone(),
        );
        actor.source_registry.remove_if_current(peer, &old);
        let replacement = actor.source_registry.get_or_create(peer);
        let caller = tokio::spawn(async move { peripheral.disconnect().await });
        actor.wait_for_message().await;
        let result = caller.await.unwrap();
        assert!(matches!(result, Err(crate::Error::Platform(error)) if error.code == "disconnect-owner-retired"));
        assert!(Arc::ptr_eq(&actor.source_registry.get(peer).unwrap(), &replacement));
        assert!(!old.waiters.lock().unwrap().reclaim_uncertain, "a refusal before dispatch has no native effect");
    }

    #[tokio::test]
    async fn retired_public_connect_refusal_does_not_poison_reclaim() {
        use crate::api::Peripheral as _;
        use std::sync::Weak;

        let (messages, receiver) = mpsc::channel(8);
        let (events, _event_receiver) = mpsc::channel(4);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let peer = Uuid::from_u128(42);
        let old = actor.source_registry.get_or_create(peer);
        let (_peripheral_events, peripheral_events) = mpsc::channel(4);
        let peripheral = super::super::peripheral::Peripheral::new_with_source_owner(
            peer, None, None, Weak::new(), peripheral_events, messages, old.clone(),
        );
        actor.source_registry.remove_if_current(peer, &old);
        let replacement = actor.source_registry.get_or_create(peer);
        let caller = tokio::spawn(async move { peripheral.connect().await });
        actor.wait_for_message().await;
        let result = caller.await.unwrap();
        assert!(matches!(result, Err(crate::Error::Platform(error)) if error.code == "connect-owner-retired"));
        assert!(Arc::ptr_eq(&actor.source_registry.get(peer).unwrap(), &replacement));
        assert!(!old.waiters.lock().unwrap().reclaim_uncertain, "a refusal before native dispatch has no effect");
    }

    #[tokio::test]
    async fn claimed_public_disconnect_keeps_its_slot_after_caller_cancellation() {
        use crate::api::Peripheral as _;
        use std::sync::Weak;

        let owner = PeerSourceRegistry::default().get_or_create(Uuid::nil());
        let (messages, mut requests) = mpsc::channel(8);
        let (_events, events) = mpsc::channel(4);
        let peripheral = super::super::peripheral::Peripheral::new_with_source_owner(
            Uuid::nil(), None, None, Weak::new(), events, messages, owner.clone(),
        );
        let caller = tokio::spawn(async move { peripheral.disconnect().await });
        let CoreBluetoothMessage::DisconnectDevice { identity, future } = requests.next().await.unwrap()
            else { panic!("expected disconnect request") };
        assert!(identity.admission.claim());
        caller.abort();
        let _ = caller.await;
        assert_eq!(owner.waiters.lock().unwrap().reclaim_operations, 1);
        assert!(!future.lock().unwrap().is_completed());
        future.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while owner.waiters.lock().unwrap().reclaim_operations != 0 { tokio::task::yield_now().await; }
        }).await.expect("claimed disconnect did not consume its reply");
    }

    #[test]
    fn connect_admission_race_has_one_winner() {
        use std::sync::{atomic::Ordering, Barrier};
        for _ in 0..200 {
            let admission = Arc::new(NativeRequestAdmission::default());
            let barrier = Arc::new(Barrier::new(2));
            let other = admission.clone();
            let other_barrier = barrier.clone();
            let cancel = std::thread::spawn(move || {
                other_barrier.wait();
                other.cancel();
            });
            barrier.wait();
            let claimed = admission.claim();
            cancel.join().unwrap();
            assert_eq!(admission.state.load(Ordering::Acquire), if claimed { 2 } else { 1 });
            assert!(!admission.claim(), "a request can be claimed only once");
        }
    }

    #[tokio::test]
    async fn cancelled_claimed_public_connect_keeps_its_waiter_until_reply() {
        use crate::api::Peripheral as _;
        use std::sync::Weak;

        let registry = Arc::new(PeerSourceRegistry::default());
        let peer = Uuid::from_u128(42);
        let owner = registry.get_or_create(peer);
        let (messages, mut requests) = mpsc::channel(8);
        let (_events, events) = mpsc::channel(4);
        let peripheral = super::super::peripheral::Peripheral::new_with_source_owner(
            peer, None, None, Weak::new(), events, messages, owner.clone(),
        );
        let caller = tokio::spawn(async move { peripheral.connect().await });
        let CoreBluetoothMessage::ConnectDevice { identity, future } = requests.next().await.unwrap()
            else { panic!("expected connect request") };
        assert!(identity.admission.claim());
        registry.remove_if_current(peer, &owner);
        let replacement = registry.get_or_create(peer);
        caller.abort();
        let _ = caller.await;
        assert_eq!(owner.waiters.lock().unwrap().reclaim_operations, 1);
        assert_eq!(replacement.waiters.lock().unwrap().reclaim_operations, 0);
        assert!(!future.lock().unwrap().is_completed());
        future.lock().unwrap().set_reply(CoreBluetoothReply::Connected);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while owner.waiters.lock().unwrap().reclaim_operations != 0 {
                tokio::task::yield_now().await;
            }
        }).await.expect("detached operation consumed the actual reply");
        assert_eq!(replacement.waiters.lock().unwrap().reclaim_operations, 0);
    }

    #[tokio::test]
    async fn claimed_connect_failure_remains_uncertain_after_caller_cancellation() {
        use crate::api::Peripheral as _;
        use std::sync::Weak;

        let owner = PeerSourceRegistry::default().get_or_create(Uuid::nil());
        let (messages, mut requests) = mpsc::channel(8);
        let (_events, events) = mpsc::channel(4);
        let peripheral = super::super::peripheral::Peripheral::new_with_source_owner(
            Uuid::nil(), None, None, Weak::new(), events, messages, owner.clone(),
        );
        let caller = tokio::spawn(async move { peripheral.connect().await });
        let CoreBluetoothMessage::ConnectDevice { identity, future } = requests.next().await.unwrap()
            else { panic!("expected connect request") };
        assert!(identity.admission.claim());
        caller.abort();
        let _ = caller.await;
        assert_eq!(owner.waiters.lock().unwrap().reclaim_operations, 1);
        future.lock().unwrap().set_reply(CoreBluetoothReply::Err("native failure".into()));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while owner.waiters.lock().unwrap().reclaim_operations != 0 { tokio::task::yield_now().await; }
        }).await.expect("claimed operation did not consume its reply");
        assert!(owner.waiters.lock().unwrap().reclaim_uncertain);
    }

    #[test]
    fn synthetic_actor_does_not_construct_a_central_manager() {
        let (_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(4);
        let actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        assert!(actor.manager_lifetime.manager.is_none());
    }

    #[tokio::test]
    async fn idle_snapshot_exhaustion_fails_only_after_accepted_actor_callback() {
        let (_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(4);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let mut active = Box::pin(CoreBluetoothReplyFuture::default()); actor.active_reply = Some(active.get_state_clone());
        let (sender, delegate_receiver) = mpsc::channel(8);
        actor.delegate_receiver = delegate_receiver.fuse();
        let (signal, mut failure) = futures::channel::oneshot::channel();
        let delegate = CentralDelegate::new(sender, actor.source_registry.clone(), Some(signal));
        let peer = Uuid::nil(); let owner = actor.source_registry.get_or_create(peer);
        let route = (AttrKey { uuid: peer, instance: 1 }, AttrKey { uuid: peer, instance: 2 });
        owner.waiters.lock().unwrap().ready = true;
        delegate.send_event(CentralDelegateEvent::CharacteristicWritten { peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1 });
        delegate.simulate_discovery_slot_exhaustion(peer);
        assert!(failure.try_recv().unwrap().is_none());
        actor.wait_for_message().await;
        assert!(futures::poll!(active.as_mut()).is_pending());
        actor.wait_for_message().await;
        assert!(matches!(active.await, CoreBluetoothReply::Failed(error) if error.code == "discovery-bound"));
        assert!(failure.try_recv().unwrap().is_none());
    }

    #[tokio::test]
    async fn callback_overflow_settles_actor_and_source_without_disconnect() {
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::{Admission, GraphGate};
        use super::super::central_delegate::PeerActorCallback;
        let (_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(4);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let mut active = Box::pin(CoreBluetoothReplyFuture::default()); actor.active_reply = Some(active.get_state_clone());
        let (sender, delegate_receiver) = mpsc::channel(8);
        actor.delegate_receiver = delegate_receiver.fuse();
        let (failure_signal, mut failure) = futures::channel::oneshot::channel();
        let delegate = CentralDelegate::new(sender, actor.source_registry.clone(), Some(failure_signal));
        let peer = Uuid::nil(); let owner = actor.source_registry.get_or_create(peer);
        let route = (AttrKey { uuid: peer, instance: 1 }, AttrKey { uuid: peer, instance: 2 });
        let read = CoreBluetoothReplyFuture::default();
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = owner.waiters.lock().unwrap();
            state.read(route, read.get_state_clone());
            let mut gate = GraphGate::new(1, 8); gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap(); state.reclaim_gate = Some(gate);
        }
        assert!(matches!(owner.admit_actor_callback(PeerActorCallback::CharacteristicWritten { route }), Admission::Deferred));
        assert!(matches!(owner.admit_actor_callback(PeerActorCallback::CharacteristicWritten { route }), Admission::Overflow(_)));
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |event| delegate.handoff_retained(peer, &owner, event.into_event(peer), None), |_| {}, |error| delegate.send_event(CentralDelegateEvent::AdapterBarrierFailed { error })).unwrap();
        assert!(failure.try_recv().unwrap().is_none(), "overflow must not race the actor FIFO through the failure channel");
        actor.wait_for_message().await;
        assert!(futures::poll!(active.as_mut()).is_pending(), "accepted completion must precede overflow failure");
        actor.wait_for_message().await;
        assert!(failure.try_recv().unwrap().is_none());
        assert!(matches!(read.await, CoreBluetoothReply::Failed(error) if error.code == "reclaim-callback-overflow"));
        assert!(matches!(active.await, CoreBluetoothReply::Failed(error) if error.code == "reclaim-callback-overflow"));
    }

    #[tokio::test]
    async fn callback_ticket_exhaustion_fails_source_and_actor_waiters() {
        let (_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(4);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let active = CoreBluetoothReplyFuture::default();
        actor.active_reply = Some(active.get_state_clone());
        let (sender, _receiver) = mpsc::channel(4);
        let (signal, failure) = futures::channel::oneshot::channel();
        let delegate = CentralDelegate::new(sender, actor.source_registry.clone(), Some(signal));
        let peer = Uuid::nil();
        let owner = actor.source_registry.get_or_create(peer);
        let route = (AttrKey { uuid: peer, instance: 1 }, AttrKey { uuid: peer, instance: 2 });
        let read = CoreBluetoothReplyFuture::default();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.callback_tickets = 512;
            state.read(route, read.get_state_clone());
        }
        delegate.send_peer_event(peer, CentralDelegateEvent::CharacteristicNotified {
            peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, data: vec![1],
        });
        let error = failure.await.unwrap();
        assert_eq!(error.code, "callback-ticket-capacity");
        actor.fail_actor(&error);
        assert!(matches!(read.await, CoreBluetoothReply::Failed(error) if error.code == "callback-ticket-capacity"));
        assert!(matches!(active.await, CoreBluetoothReply::Failed(error) if error.code == "callback-ticket-capacity"));
        assert!(owner.waiters.lock().unwrap().reclaim_uncertain);
    }

    #[tokio::test]
    async fn queued_callback_never_retargets_a_replacement_peer_owner() {
        let (_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(8);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let (sender, delegate_receiver) = mpsc::channel(8);
        actor.delegate_receiver = delegate_receiver.fuse();
        let delegate = CentralDelegate::new(sender, actor.source_registry.clone(), None);
        let peer = Uuid::nil();
        let old = actor.source_registry.get_or_create(peer);
        old.waiters.lock().unwrap().ready = true;
        let route = (AttrKey { uuid: peer, instance: 1 }, AttrKey { uuid: peer, instance: 2 });
        delegate.send_peer_event(peer, CentralDelegateEvent::CharacteristicNotified {
            peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, data: vec![1],
        });
        actor.source_registry.remove_if_current(peer, &old);
        let replacement = actor.source_registry.get_or_create(peer);
        replacement.waiters.lock().unwrap().ready = true;
        tokio::time::timeout(std::time::Duration::from_secs(3), actor.wait_for_message()).await.unwrap();
        assert!(!old.waiters.lock().unwrap().ready);
        assert!(replacement.waiters.lock().unwrap().ready);
    }

    #[tokio::test]
    async fn actor_shutdown_retires_queued_callback_ticket() {
        let (_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(4);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let (sender, delegate_receiver) = mpsc::channel(8);
        actor.delegate_receiver = delegate_receiver.fuse();
        let delegate = CentralDelegate::new(sender, actor.source_registry.clone(), None);
        let peer = Uuid::nil();
        let owner = actor.source_registry.get_or_create(peer);
        let route = (AttrKey { uuid: peer, instance: 1 }, AttrKey { uuid: peer, instance: 2 });
        delegate.send_peer_event(peer, CentralDelegateEvent::CharacteristicNotified {
            peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, data: vec![1],
        });
        assert_eq!(owner.waiters.lock().unwrap().callback_tickets, 1);
        drop(actor);
        assert_eq!(owner.waiters.lock().unwrap().callback_tickets, 0);
    }

    #[tokio::test]
    async fn adapter_event_ticket_blocks_all_peer_acquires_until_actor_consumes() {
        use crate::ubm_reclaim_authority::{GraphKey, GraphTarget, PermitStatus};
        let (_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(16);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let (delegate_sender, delegate_receiver) = mpsc::channel(8);
        actor.delegate_receiver = delegate_receiver.fuse();
        let delegate = CentralDelegate::new(delegate_sender, actor.source_registry.clone(), None);
        let owner = actor.source_registry.get_or_create(Uuid::nil());
        let graph = ("corebluetooth".to_owned(), owner.graph_attachment.unwrap(), 1);
        let first = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: graph.clone(), permit_id: 1 };
        let target = GraphTarget { peer_id: first.peer_id.clone(), connection_generation: first.connection_generation.clone(), graph: graph.clone() };
        let binding = {
            let mut state = owner.waiters.lock().unwrap(); state.ready = true; state.revision = 1;
            state.reclaim_binding.attach(); state.reclaim_binding.publish(graph.clone());
            state.reclaim_binding.capture(first.peer_id.clone(), first.connection_generation.clone(), &graph).unwrap()
        };
        {
            let mut authority = owner.authorities.lock().unwrap();
            authority.reserve_open(1).unwrap(); authority.resolve_open(1, true).unwrap();
            authority.reserve_permit_token_bound(1, 1, target.clone(), &binding).unwrap();
            authority.resolve_permit_token(1, 1, &target, true).unwrap();
        }
        delegate.send_event(CentralDelegateEvent::DidUpdateState { state: CBManagerState::PoweredOn });
        assert_eq!(owner.acquire_reclaim(&binding, first.clone()), Ok(PermitStatus::Busy));
        tokio::time::timeout(std::time::Duration::from_secs(3), actor.wait_for_message()).await.unwrap();
        let second = GraphKey { permit_id: 2, ..first };
        {
            let mut authority = owner.authorities.lock().unwrap();
            authority.reserve_permit_token_bound(1, 2, target.clone(), &binding).unwrap();
            authority.resolve_permit_token(1, 2, &target, true).unwrap();
        }
        assert_eq!(owner.acquire_reclaim(&binding, second), Ok(PermitStatus::Acquired));
    }

    #[tokio::test]
    async fn actor_event_ticket_blocks_acquire_until_event_consumed() {
        use crate::ubm_reclaim_authority::{GraphKey, GraphTarget, PermitStatus};
        let (_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(16);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let (delegate_sender, delegate_receiver) = mpsc::channel(8);
        actor.delegate_receiver = delegate_receiver.fuse();
        let delegate = CentralDelegate::new(delegate_sender, actor.source_registry.clone(), None);
        let peer = Uuid::nil();
        let owner = actor.source_registry.get_or_create(peer);
        let graph = ("corebluetooth".to_owned(), owner.graph_attachment.unwrap(), 1);
        let first = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: graph.clone(), permit_id: 1 };
        let target = GraphTarget { peer_id: first.peer_id.clone(), connection_generation: first.connection_generation.clone(), graph: graph.clone() };
        let binding = {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true; state.revision = 1;
            state.reclaim_binding.attach(); state.reclaim_binding.publish(graph.clone());
            state.reclaim_binding.capture(first.peer_id.clone(), first.connection_generation.clone(), &graph).unwrap()
        };
        {
            let mut authority = owner.authorities.lock().unwrap();
            authority.reserve_open(1).unwrap(); authority.resolve_open(1, true).unwrap();
            authority.reserve_permit_token_bound(1, 1, target.clone(), &binding).unwrap();
            authority.resolve_permit_token(1, 1, &target, true).unwrap();
        }
        let route = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        delegate.send_peer_event(peer, CentralDelegateEvent::CharacteristicNotified {
            peripheral_uuid: peer, service_uuid: route.0, characteristic_uuid: route.1, data: vec![1],
        });
        assert_eq!(owner.acquire_reclaim(&binding, first.clone()), Ok(PermitStatus::Busy));
        while owner.waiters.lock().unwrap().callback_tickets != 0 {
            tokio::time::timeout(std::time::Duration::from_secs(3), actor.wait_for_message()).await.unwrap();
        }
        let second = GraphKey { permit_id: 2, ..first };
        {
            let mut authority = owner.authorities.lock().unwrap();
            authority.reserve_permit_token_bound(1, 2, target.clone(), &binding).unwrap();
            authority.resolve_permit_token(1, 2, &target, true).unwrap();
        }
        assert_eq!(owner.acquire_reclaim(&binding, second), Ok(PermitStatus::Acquired));
    }

    #[tokio::test]
    async fn actor_drain_retires_discovery_ticket_after_queued_result() {
        use crate::ubm_reclaim_authority::{GraphKey, GraphTarget, PermitStatus};
        let (_messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(4);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let peer = Uuid::nil();
        let owner = actor.source_registry.get_or_create(peer);
        let graph = ("corebluetooth".to_owned(), owner.graph_attachment.unwrap(), 1);
        let first = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: graph.clone(), permit_id: 1 };
        let target = GraphTarget { peer_id: first.peer_id.clone(), connection_generation: first.connection_generation.clone(), graph: graph.clone() };
        let binding = {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true; state.revision = 1; state.discovery_actor_pending = true;
            state.reclaim_binding.attach(); state.reclaim_binding.publish(graph.clone());
            state.reclaim_binding.capture(first.peer_id.clone(), first.connection_generation.clone(), &graph).unwrap()
        };
        {
            let mut authority = owner.authorities.lock().unwrap();
            authority.reserve_open(1).unwrap(); authority.resolve_open(1, true).unwrap();
            authority.reserve_permit_token_bound(1, 1, target.clone(), &binding).unwrap();
            authority.resolve_permit_token(1, 1, &target, true).unwrap();
        }
        assert_eq!(owner.acquire_reclaim(&binding, first.clone()), Ok(PermitStatus::Busy));
        actor.on_discovery_drained(peer);
        let second = GraphKey { permit_id: 2, ..first };
        {
            let mut authority = owner.authorities.lock().unwrap();
            authority.reserve_permit_token_bound(1, 2, target.clone(), &binding).unwrap();
            authority.resolve_permit_token(1, 2, &target, true).unwrap();
        }
        assert_eq!(owner.acquire_reclaim(&binding, second), Ok(PermitStatus::Acquired));
    }

    #[tokio::test]
    async fn failure_interrupts_downstream_send_and_rejects_queued_commands() {
        use futures::FutureExt;
        use std::task::Poll;
        let (mut messages, receiver) = mpsc::channel(4);
        let (events, _event_receiver) = mpsc::channel(1);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let active = CoreBluetoothReplyFuture::default();
        actor.active_reply = Some(active.get_state_clone());
        let queued = CoreBluetoothReplyFuture::default();
        messages
            .try_send(CoreBluetoothMessage::GetAdapterState {
                future: queued.get_state_clone(),
            })
            .unwrap();
        let (mut downstream, _blocked_receiver) = mpsc::channel::<u8>(0);
        downstream.try_send(1).unwrap();
        let mut blocked = Box::pin(downstream.send(2).fuse());
        assert!(matches!(futures::poll!(&mut blocked), Poll::Pending));
        let (sender, _control_receiver) = mpsc::channel(0);
        let (signal, failure) = futures::channel::oneshot::channel();
        let delegate = CentralDelegate::new(sender, actor.source_registry.clone(), Some(signal));
        let mut failure = failure.fuse();
        for _ in 0..2 {
            delegate.send_event(CentralDelegateEvent::DidUpdateState {
                state: CBManagerState::PoweredOn,
            });
        }
        let error = futures::select! {
            terminal = failure => terminal.unwrap(),
            _ = blocked => panic!("downstream must remain blocked"),
        };
        actor.fail_actor(&error);
        actor.message_receiver.next().await.unwrap().fail(&error);
        assert!(
            matches!(active.await, CoreBluetoothReply::Failed(error) if error.code == "source-handoff-failed")
        );
        assert!(
            matches!(queued.await, CoreBluetoothReply::Failed(error) if error.code == "source-handoff-failed")
        );
    }

    #[tokio::test]
    async fn saturated_control_uses_reserved_failure_signal() {
        let registry = Arc::new(PeerSourceRegistry::default());
        let (sender, _receiver) = mpsc::channel(0);
        let (signal, completed) = futures::channel::oneshot::channel();
        let delegate = CentralDelegate::new(sender, registry, Some(signal));
        for _ in 0..2 {
            delegate.send_event(CentralDelegateEvent::DidUpdateState {
                state: CBManagerState::PoweredOn,
            });
        }
        let error = completed.await.unwrap();
        assert_eq!(error.code, "source-handoff-failed");
        assert!(error.message.contains("full"));
    }

    #[tokio::test]
    async fn actor_failure_completes_active_and_transferred_directory_once() {
        let (_messages, receiver) = mpsc::channel(1);
        let (events, _event_receiver) = mpsc::channel(1);
        let mut actor = CoreBluetoothInternal::new_without_radio(receiver, events);
        let active = CoreBluetoothReplyFuture::default();
        let transferred = CoreBluetoothReplyFuture::default();
        actor.active_reply = Some(active.get_state_clone());
        actor.directory_replies.push(active.get_state_clone());
        actor.directory_replies.push(transferred.get_state_clone());
        let error = crate::PlatformError::new("corebluetooth", "source-handoff-failed", "full");
        actor.fail_actor(&error);
        assert!(
            matches!(active.await, CoreBluetoothReply::Failed(error) if error.code == "source-handoff-failed")
        );
        assert!(
            matches!(transferred.await, CoreBluetoothReply::Failed(error) if error.code == "source-handoff-failed")
        );
    }

    #[tokio::test]
    async fn saturated_peer_handoff_fails_source_waiter_and_legacy_stream() {
        use crate::common::util::{retained_notifications, NotificationFaults};
        use crate::ubm_peer_notification_ingress::{IngressRecord, Poll, TerminalReason};
        let registry = Arc::new(PeerSourceRegistry::default());
        let (sender, _receiver) = mpsc::channel(0);
        let delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::nil();
        let owner = registry.get_or_create(peer);
        let service = AttrKey {
            uuid: Uuid::nil(),
            instance: 1,
        };
        let characteristic = AttrKey {
            uuid: Uuid::nil(),
            instance: 2,
        };
        let legacy_faults = Arc::new(NotificationFaults::new(8));
        let (broadcast, receiver) = tokio::sync::broadcast::channel(8);
        let mut legacy = retained_notifications(receiver, legacy_faults.clone());
        *owner.legacy.lock().unwrap() = Some((legacy_faults, broadcast));
        let read = CoreBluetoothReplyFuture::default();
        owner.faults.start_source_generation(1).unwrap();
        let (_, consumer) = owner.faults.open_source_group(1, 8, 64).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.generation = Some(1);
            state.read((service, characteristic), read.get_state_clone());
        }
        for _ in 0..2 {
            delegate.send_peer_event(
                peer,
                CentralDelegateEvent::CharacteristicNotified {
                    peripheral_uuid: peer,
                    service_uuid: service,
                    characteristic_uuid: characteristic,
                    data: vec![1],
                },
            );
        }
        assert!(matches!(read.await, CoreBluetoothReply::Failed(_)));
        assert!(matches!(
            consumer.poll(),
            Poll::Record(IngressRecord::Terminal {
                reason: TerminalReason::SourceFailure { .. },
                memberships,
            }) if memberships.is_empty()
        ));
        let failure = legacy.next().await.unwrap().source_failure.unwrap();
        assert_eq!(failure.code, "source-handoff-failed");
        assert!(failure.message.contains("full"));
    }

    #[test]
    fn owners_are_shared_and_stale_removal_cannot_remove_replacement() {
        let registry = Arc::new(PeerSourceRegistry::default());
        let (sender, _receiver) = mpsc::channel(4);
        let native_delegate = CentralDelegate::new(sender, registry.clone(), None);
        let peer = Uuid::nil();
        let discovered = registry.get_or_create(peer);
        let retrieved = registry.get_or_create(peer);
        let delegate = native_delegate.peer_source_owner(peer).unwrap();
        assert!(Arc::ptr_eq(&discovered, &retrieved));
        assert!(Arc::ptr_eq(&discovered, &delegate));
        registry.remove_if_current(peer, &discovered);
        let replacement = registry.get_or_create(peer);
        registry.remove_if_current(peer, &discovered);
        assert!(Arc::ptr_eq(&registry.get(peer).unwrap(), &replacement));
        assert!(!Arc::ptr_eq(&replacement, &discovered));
    }
}

#[cfg(test)]
mod reclaim_callback_fifo_tests {
    use super::*;
    use crate::ubm_reclaim_authority::GraphKey;
    use crate::ubm_reclaim_graph_gate::{GateError, GraphGate};

    fn key() -> GraphKey {
        GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 }
    }

    fn route() -> (AttrKey, AttrKey) {
        (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 })
    }

    #[test]
    fn independent_review_dropped_captures_complete_waiters_after_source_terminal() {
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Poll, Wake, Waker};
        use crate::ubm_peer_notification_ingress::{IngressRecord, Poll as SourcePoll, TerminalReason};
        struct Probe(crate::common::util::SourceGroupConsumer, AtomicBool);
        impl Wake for Probe {
            fn wake(self: Arc<Self>) {
                self.1.store(matches!(self.0.poll(), SourcePoll::Record(IngressRecord::Terminal { reason: TerminalReason::Closed { tail_uncertain: true }, .. })), Ordering::SeqCst);
            }
        }
        for is_error in [false, true] {
            let owner = PeerSourceOwner::new();
            let mut read = Box::pin(CoreBluetoothReplyFuture::default());
            owner.faults.start_source_generation(1).unwrap();
            let (_, consumer) = owner.faults.open_source_group(1, 8, 64).unwrap();
            let probe = Arc::new(Probe(consumer, AtomicBool::new(false)));
            let waker = Waker::from(probe.clone());
            assert!(read.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
            {
                let mut state = owner.waiters.lock().unwrap();
                state.ready = true;
                state.generation = Some(1);
                state.read(route(), read.get_state_clone());
                let mut gate = GraphGate::new(8, 8192);
                gate.acquire(key()).unwrap();
                state.reclaim_gate = Some(gate);
            }
            if is_error {
                assert!(owner.admit_delegate_callback(super::super::central_delegate::PeerDelegateCallback::ValueError {
                    route: route(), notifying: true, error: crate::PlatformError::new("corebluetooth", "exact-value-error", "original"),
                }));
            } else {
                assert!(!owner.value(route(), true, &[7]));
            }
            drop(owner);
            assert!(probe.1.load(Ordering::SeqCst), "source terminal must precede the captured waiter wake");
            let result = read.as_mut().poll(&mut Context::from_waker(&waker));
            if is_error {
                assert!(matches!(result, Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "exact-value-error"));
            } else {
                assert!(matches!(result, Poll::Ready(_)), "captured read must complete on retirement");
            }
        }
        for is_error in [false, true] {
            let owner = PeerSourceOwner::new();
            let mut enable = Box::pin(CoreBluetoothReplyFuture::default());
            {
                let mut state = owner.waiters.lock().unwrap();
                state.enable(route(), enable.get_state_clone()).unwrap();
                let mut gate = GraphGate::new(8, 8192);
                gate.acquire(key()).unwrap();
                state.reclaim_gate = Some(gate);
            }
            let callback = if is_error {
                super::super::central_delegate::PeerDelegateCallback::NotificationError { route: route(), error: crate::PlatformError::new("corebluetooth", "exact-notify-error", "original") }
            } else {
                super::super::central_delegate::PeerDelegateCallback::NotificationState { route: route(), notifying: true }
            };
            assert!(owner.admit_delegate_callback(callback));
            drop(owner);
            let waker = futures::task::noop_waker();
            let result = enable.as_mut().poll(&mut Context::from_waker(&waker));
            if is_error {
                assert!(matches!(result, Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "exact-notify-error"));
            } else {
                assert!(matches!(result, Poll::Ready(_)), "captured notification completion must not hang");
            }
        }
        // A disabled notification enters cleanup instead of carrying a
        // prepared reply in the callback. Retirement must not report success
        // when that actor-side cleanup never ran.
        let owner = PeerSourceOwner::new();
        let mut disable = Box::pin(CoreBluetoothReplyFuture::default());
        {
            let mut state = owner.waiters.lock().unwrap();
            state.disable(route(), disable.get_state_clone()).unwrap();
            let mut gate = GraphGate::new(8, 8192);
            gate.acquire(key()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(owner.admit_delegate_callback(super::super::central_delegate::PeerDelegateCallback::NotificationState { route: route(), notifying: false }));
        drop(owner);
        let waker = futures::task::noop_waker();
        assert!(matches!(disable.as_mut().poll(&mut Context::from_waker(&waker)), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "callback-owner-retired"));
    }

    #[test]
    fn independent_review_each_refusal_captures_its_replacement_generation() {
        use std::future::Future;
        use std::task::{Context, Poll};
        use crate::ubm_peer_notification_ingress::{IngressRecord, Poll as SourcePoll, TerminalReason};
        let owner = PeerSourceOwner::new();
        let key = key();
        let mut first = Box::pin(CoreBluetoothReplyFuture::default());
        let mut second = Box::pin(CoreBluetoothReplyFuture::default());
        owner.faults.start_source_generation(1).unwrap();
        let (_, old) = owner.faults.open_source_group(1, 8, 64).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true; state.generation = Some(1); state.read(route(), first.get_state_clone());
            let mut gate = GraphGate::new_with_barriers(0, 0, 1, 1024); gate.acquire(key.clone()).unwrap(); state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), true, &[1]));
        assert!(matches!(owner.admit_adapter_barrier(17), crate::ubm_reclaim_graph_gate::BarrierAdmission::Overflow { .. }));
        owner.faults.start_source_generation(2).unwrap();
        let (_, replacement) = owner.faults.open_source_group(2, 8, 64).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true; state.generation = Some(2); state.read(route(), second.get_state_clone());
        }
        assert!(!owner.value(route(), true, &[2]));
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut barrier = None;
        assert_eq!(owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |id| barrier = id, |_| {}), Err(GateError::Paused));
        assert_eq!(barrier, None);
        assert!(matches!(first.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "reclaim-callback-overflow"));
        assert!(second.as_mut().poll(&mut cx).is_pending());
        assert!(matches!(old.poll(), SourcePoll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, .. }, .. }) if error.code == "reclaim-callback-overflow"));
        assert_eq!(replacement.poll(), SourcePoll::Empty);
        owner.resume_adapter_overflow().unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(second.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "reclaim-callback-overflow"));
        assert!(matches!(replacement.poll(), SourcePoll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, .. }, .. }) if error.code == "reclaim-callback-overflow"));
    }

    #[test]
    fn independent_review_retirement_recovers_all_ingress_before_first_waiter() {
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Poll, Wake, Waker};
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll as SourcePoll, TerminalReason};
        struct Probe(crate::common::util::SourceGroupConsumer, AtomicBool);
        impl Wake for Probe {
            fn wake(self: Arc<Self>) {
                let first = self.0.poll();
                let second = self.0.poll();
                let terminal = self.0.poll();
                self.1.store(matches!(first, SourcePoll::Record(IngressRecord::Value { .. }))
                    && matches!(second, SourcePoll::Record(IngressRecord::Value { .. }))
                    && matches!(terminal, SourcePoll::Record(IngressRecord::Terminal { reason: TerminalReason::Closed { tail_uncertain: true }, .. })), Ordering::SeqCst);
            }
        }
        let owner = PeerSourceOwner::new();
        let mut first = Box::pin(CoreBluetoothReplyFuture::default());
        let mut second = Box::pin(CoreBluetoothReplyFuture::default());
        owner.faults.start_source_generation(1).unwrap();
        let (group, consumer) = owner.faults.open_source_group(1, 8, 64).unwrap();
        let (service, characteristic) = route();
        owner.faults.register_source_route(&group, NotificationRoute { service_uuid: service.uuid, service_instance: service.instance, characteristic_uuid: characteristic.uuid, characteristic_instance: characteristic.instance }, 1).unwrap();
        let probe = Arc::new(Probe(consumer, AtomicBool::new(false)));
        let waker = Waker::from(probe.clone());
        assert!(first.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true; state.generation = Some(1);
            state.read(route(), first.get_state_clone()); state.read(route(), second.get_state_clone());
            let mut gate = GraphGate::new(8, 8192); gate.acquire(key()).unwrap(); state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), true, &[1]));
        assert!(!owner.value(route(), true, &[2]));
        drop(owner);
        assert!(probe.1.load(Ordering::SeqCst), "all accepted ingress and its uncertain terminal must precede the first waiter wake");
        let waker = futures::task::noop_waker();
        assert!(matches!(second.as_mut().poll(&mut Context::from_waker(&waker)), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "callback-owner-retired"));
    }

    #[test]
    fn independent_review_repeated_empty_source_failures_are_bounded() {
        let owner = PeerSourceOwner::new();
        let key = key();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new(0, 0); gate.acquire(key.clone()).unwrap(); state.reclaim_gate = Some(gate);
        }
        for _ in 0..1000 { owner.fail_source(crate::PlatformError::new("corebluetooth", "failure", "original"), None); }
        let mut state = owner.waiters.lock().unwrap();
        let gate = state.reclaim_gate.as_mut().unwrap();
        gate.release(&key).unwrap();
        assert!(matches!(gate.next(), Ok(crate::ubm_reclaim_graph_gate::DrainItem::Event(_))));
        assert!(matches!(gate.next(), Ok(crate::ubm_reclaim_graph_gate::DrainItem::Finished)), "empty repeated failures must not allocate mandatory nodes");
    }

    #[test]
    fn empty_value_still_charges_retained_callback_storage() {
        let owner = PeerSourceOwner::new();
        let key = key();
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(1, 0);
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), true, &[]));
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut delivered = 0;
        let mut failed = 0;
        owner.drain_reclaim_callbacks(|_, _| delivered += 1, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| failed += 1).unwrap();
        assert_eq!(delivered, 0);
        assert_eq!(failed, 1);
    }

    #[test]
    fn source_reservation_storage_is_preflighted_before_accepting_value() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
        let owner = PeerSourceOwner::new();
        let key = key();
        owner.faults.start_source_generation(1).unwrap();
        let (group, consumer) = owner.faults.open_source_group(1, 8, 64).unwrap();
        let (service, characteristic) = route();
        owner.faults.register_source_route(&group, NotificationRoute {
            service_uuid: service.uuid, service_instance: service.instance,
            characteristic_uuid: characteristic.uuid, characteristic_instance: characteristic.instance,
        }, 1).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.generation = Some(1);
            let mut gate = GraphGate::new(2, std::mem::size_of::<DeferredPeerCallback>() + 1);
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), true, &[1]));
        assert_eq!(consumer.poll(), Poll::Empty);
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| panic!("over-budget value accepted"), |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { .. })));
    }

    #[test]
    fn value_error_token_budget_is_checked_before_holding_terminal() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
        use super::super::central_delegate::PeerDelegateCallback;
        let owner = PeerSourceOwner::new();
        let key = key();
        owner.faults.start_source_generation(1).unwrap();
        let (group, consumer) = owner.faults.open_source_group(1, 8, 64).unwrap();
        let (service, characteristic) = route();
        owner.faults.register_source_route(&group, NotificationRoute {
            service_uuid: service.uuid, service_instance: service.instance,
            characteristic_uuid: characteristic.uuid, characteristic_instance: characteristic.instance,
        }, 1).unwrap();
        let error = crate::PlatformError::new("corebluetooth", "original", "failure");
        {
            let mut state = owner.waiters.lock().unwrap();
            state.generation = Some(1);
            let mut gate = GraphGate::new(2, std::mem::size_of::<DeferredPeerCallback>() + retained_error_bytes(&error));
            gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(owner.admit_delegate_callback(PeerDelegateCallback::ValueError { route: route(), notifying: true, error }));
        assert_eq!(consumer.poll(), Poll::Empty);
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| panic!("over-budget error accepted"), |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, .. }, .. }) if error.code == "reclaim-callback-reservation-failed"));
    }

    #[test]
    fn idle_delegate_admission_captures_the_waiter_before_a_later_registration() {
        use super::super::central_delegate::PeerDelegateCallback;
        use std::future::Future;
        use std::task::{Context, Poll};
        let owner = PeerSourceOwner::new();
        let mut before = Box::pin(CoreBluetoothReplyFuture::default());
        let mut after = Box::pin(CoreBluetoothReplyFuture::default());
        owner.with_waiters(|state| state.enable(route(), before.get_state_clone()).unwrap());
        let effect = owner.prepare_delegate_callback(PeerDelegateCallback::NotificationError {
            route: route(), error: crate::PlatformError::new("corebluetooth", "original", "failed"),
        }).unwrap().expect("idle effect executes outside the lock");
        owner.with_waiters(|state| state.enable(route(), after.get_state_clone()).unwrap());
        assert!(owner.execute_delegate_capture(effect).is_none());
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(before.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "original"));
        assert!(after.as_mut().poll(&mut cx).is_pending());
    }

    #[test]
    fn deferred_disconnect_preserves_native_error_for_waiter_and_replay() {
        use super::super::central_delegate::PeerLifecycle;
        use futures::FutureExt;
        let owner = PeerSourceOwner::new();
        let key = key();
        let waiter = CoreBluetoothReplyFuture::default();
        let native = crate::PlatformError::new("corebluetooth", "7", "link lost")
            .with("nsErrorDomain", "CBErrorDomain");
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.read(route(), waiter.get_state_clone());
            let mut gate = GraphGate::new(4, 8192);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(owner.prepare_lifecycle(PeerLifecycle::DisconnectedWithError(native.clone())).unwrap().is_none());
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut replayed = None;
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |event| replayed = Some(event), |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(waiter.now_or_never(), Some(CoreBluetoothReply::Failed(error)) if error == native));
        assert!(matches!(replayed, Some(PeerLifecycle::DisconnectedWithError(error)) if error == native));
    }

    #[test]
    fn deferred_duplicate_disconnect_keeps_distinct_native_error() {
        use super::super::central_delegate::PeerLifecycle;
        let owner = PeerSourceOwner::new();
        let key = key();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new(4, 8192);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(owner.prepare_lifecycle(PeerLifecycle::Disconnected).unwrap().is_none());
        let native = crate::PlatformError::new("corebluetooth", "7", "link lost");
        assert!(owner.prepare_lifecycle(PeerLifecycle::DisconnectedWithError(native.clone())).unwrap().is_none());
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut events = Vec::new();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |event| events.push(event), |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(events.as_slice(), [PeerLifecycle::Disconnected, PeerLifecycle::DisconnectedWithError(error)] if error == &native));
    }

    #[test]
    fn idle_lifecycle_admission_retires_the_graph_before_execution() {
        use super::super::central_delegate::PeerLifecycle;
        let owner = PeerSourceOwner::new();
        let before = CoreBluetoothReplyFuture::default();
        owner.with_waiters(|state| { state.ready = true; state.read(route(), before.get_state_clone()); });
        let effect = owner.prepare_lifecycle(PeerLifecycle::Disconnected).unwrap().expect("idle lifecycle executes outside the lock");
        assert!(!owner.waiters.lock().unwrap().ready);
        assert!(matches!(effect, DeferredPeerCallback::Lifecycle { replies, .. }
            if replies.0.len() == 1 && Arc::ptr_eq(&replies.0[0].0, &before.get_state_clone())));
    }

    #[test]
    fn authority_replay_rejects_foreign_source_and_keeps_original_receipt() {
        use crate::ubm_reclaim_authority::{AuthorityError, GraphTarget, PermitStatus};
        let authorities = Arc::new(std::sync::Mutex::new(crate::ubm_reclaim_authority::ReclaimAuthorities::default()));
        let owner = PeerSourceOwner::with_authorities(authorities.clone(), Arc::default(), Arc::default(), None);
        let foreign = PeerSourceOwner::with_authorities(authorities.clone(), Arc::default(), Arc::default(), None);
        let graph = ("corebluetooth".to_owned(), owner.graph_attachment.unwrap(), 1);
        let key = GraphKey { graph: graph.clone(), ..key() };
        let target = GraphTarget { peer_id: key.peer_id.clone(), connection_generation: key.connection_generation.clone(), graph: graph.clone() };
        {
            let mut registry = authorities.lock().unwrap();
            registry.reserve_open(1).unwrap();
            registry.resolve_open(1, true).unwrap();
        }
        let binding = {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.revision = 1;
            state.reclaim_binding.attach();
            state.reclaim_binding.publish(graph.clone());
            state.reclaim_binding.capture(key.peer_id.clone(), key.connection_generation.clone(), &graph).unwrap()
        };
        {
            let mut registry = authorities.lock().unwrap();
            registry.reserve_permit_token_bound(1, 1, target.clone(), &binding).unwrap();
            registry.resolve_permit_token(1, 1, &target, true).unwrap();
        }
        assert_eq!(owner.acquire_reclaim(&binding, key.clone()), Ok(PermitStatus::Acquired));
        assert_eq!(foreign.acquire_reclaim(&binding, key.clone()), Err(AuthorityError::Conflict));
        assert_eq!(foreign.commit_reclaim(&key), Err(AuthorityError::Conflict));
        assert_eq!(foreign.release_reclaim(&key, |_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}), Err(AuthorityError::Conflict));
        assert_eq!(foreign.forget_reclaim(&key), Err(AuthorityError::Conflict));
        owner.waiters.lock().unwrap().reclaim_binding.disconnect();
        assert_eq!(owner.acquire_reclaim(&binding, key.clone()), Ok(PermitStatus::Acquired));
        assert_eq!(owner.commit_reclaim(&key), Ok(PermitStatus::Committed));
        assert_eq!(owner.release_reclaim(&key, |_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}), Ok(PermitStatus::Released));
        owner.forget_reclaim(&key).unwrap();
    }

    #[test]
    fn discovery_actor_ticket_blocks_permit_after_result_until_drain() {
        use crate::ubm_reclaim_authority::{GraphTarget, PermitStatus};
        let owner = PeerSourceOwner::new();
        let graph = ("corebluetooth".to_owned(), owner.graph_attachment.unwrap(), 1);
        let key = GraphKey { graph: graph.clone(), ..key() };
        let target = GraphTarget { peer_id: key.peer_id.clone(), connection_generation: key.connection_generation.clone(), graph: graph.clone() };
        let binding = {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.revision = 1;
            state.discovery_actor_pending = true;
            state.reclaim_binding.attach();
            state.reclaim_binding.publish(graph.clone());
            state.reclaim_binding.capture(key.peer_id.clone(), key.connection_generation.clone(), &graph).unwrap()
        };
        {
            let mut registry = owner.authorities.lock().unwrap();
            registry.reserve_open(1).unwrap();
            registry.resolve_open(1, true).unwrap();
            registry.reserve_permit_token_bound(1, 1, target.clone(), &binding).unwrap();
            registry.resolve_permit_token(1, 1, &target, true).unwrap();
        }
        assert_eq!(owner.acquire_reclaim(&binding, key), Ok(PermitStatus::Busy));
    }

    #[test]
    fn precommit_invalidation_rejects_commit_and_replays_in_order() {
        let owner = PeerSourceOwner::new();
        let key = key();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new(4, 1024);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), true, &[1]));
        owner.invalidate(crate::PlatformError::new("corebluetooth", "disconnected", "disconnected"));
        {
            let mut state = owner.waiters.lock().unwrap();
            assert!(!state.ready);
            let gate = state.reclaim_gate.as_mut().unwrap();
            assert_eq!(gate.commit(&key), Err(GateError::Invalidated));
            gate.release(&key).unwrap();
        }
        let mut delivered = Vec::new();
        owner.drain_reclaim_callbacks(|_, value| delivered.push(value), |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert_eq!(delivered, vec![vec![1]]);
        assert!(!owner.waiters.lock().unwrap().ready);
    }

    #[test]
    fn held_invalidation_selects_only_waiters_present_at_its_cut() {
        use std::future::Future;
        use std::task::{Context, Poll};
        let owner = PeerSourceOwner::new();
        let key = key();
        let mut before = Box::pin(CoreBluetoothReplyFuture::default());
        let mut after = Box::pin(CoreBluetoothReplyFuture::default());
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.read(route(), before.get_state_clone());
            let mut gate = GraphGate::new(4, 1024);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        owner.invalidate(crate::PlatformError::new("corebluetooth", "disconnected", "disconnected"));
        {
            let mut state = owner.waiters.lock().unwrap();
            state.read(route(), after.get_state_clone());
            state.reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        }
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(before.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(_))));
        assert!(after.as_mut().poll(&mut cx).is_pending(), "later waiter belongs to a different logical state");
    }

    #[test]
    fn lifecycle_cut_holds_old_generation_terminal_behind_accepted_value() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
        use super::super::central_delegate::PeerLifecycle;
        let owner = PeerSourceOwner::new();
        let key = key();
        owner.faults.start_source_generation(1).unwrap();
        let (group, consumer) = owner.faults.open_source_group(1, 8, 64).unwrap();
        let (service, characteristic) = route();
        let member = owner.faults.register_source_route(&group, NotificationRoute {
            service_uuid: service.uuid, service_instance: service.instance,
            characteristic_uuid: characteristic.uuid, characteristic_instance: characteristic.instance,
        }, 1).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.generation = Some(1);
            let mut gate = GraphGate::new(4, 1024);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), true, &[9]));
        assert!(owner.admit_lifecycle(PeerLifecycle::Disconnected));
        assert_eq!(consumer.poll(), Poll::Empty);
        owner.faults.start_source_generation(2).unwrap();
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member));
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::Closed { tail_uncertain: true }, .. })));
    }

    #[test]
    fn lifecycle_overflow_retains_cut_after_accepted_prefix() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
        use super::super::central_delegate::PeerLifecycle;
        let owner = PeerSourceOwner::new();
        let key = key();
        owner.faults.start_source_generation(1).unwrap();
        let (group, consumer) = owner.faults.open_source_group(1, 8, 64).unwrap();
        let (service, characteristic) = route();
        let member = owner.faults.register_source_route(&group, NotificationRoute {
            service_uuid: service.uuid, service_instance: service.instance,
            characteristic_uuid: characteristic.uuid, characteristic_instance: characteristic.instance,
        }, 1).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.generation = Some(1);
            let mut gate = GraphGate::new(1, 1024);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), true, &[9]));
        assert!(owner.admit_lifecycle(PeerLifecycle::Disconnected));
        assert!(!owner.waiters.lock().unwrap().ready, "overflow must still capture the lifecycle cut");
        assert_eq!(consumer.poll(), Poll::Empty);
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut lifecycle = 0;
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| lifecycle += 1, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert_eq!(lifecycle, 1);
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member));
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::Closed { tail_uncertain: true }, .. })));
    }

    #[test]
    fn lifecycle_cut_survives_source_close_reservation_exhaustion() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, Poll, TerminalReason};
        use super::super::central_delegate::PeerLifecycle;
        let owner = PeerSourceOwner::new();
        let key = key();
        owner.faults.start_source_generation(1).unwrap();
        let (_, consumer) = owner.faults.open_source_group(1, 8, 64).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.generation = Some(1);
            // The callback fits, but no bytes remain for the per-group close hold.
            let mut gate = GraphGate::new(1, std::mem::size_of::<DeferredPeerCallback>());
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(owner.admit_lifecycle(PeerLifecycle::Disconnected));
        assert!(!owner.waiters.lock().unwrap().ready, "the source reservation budget cannot suppress a lifecycle cut");
        assert_eq!(consumer.poll(), Poll::Empty);
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut lifecycle = 0;
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| lifecycle += 1, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert_eq!(lifecycle, 1);
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::Closed { tail_uncertain: true }, .. })));
    }

    fn direct_invalidation_overflow_captures_waiter(committed: bool) {
        use std::future::Future;
        use std::task::{Context, Poll};
        let owner = PeerSourceOwner::new();
        let key = key();
        let mut before = Box::pin(CoreBluetoothReplyFuture::default());
        let mut after = Box::pin(CoreBluetoothReplyFuture::default());
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.read(route(), before.get_state_clone());
            let mut gate = GraphGate::new(1, 1024);
            gate.acquire(key.clone()).unwrap();
            if committed { gate.commit(&key).unwrap(); }
            state.reclaim_gate = Some(gate);
        }
        // An actor callback consumes the ordinary lane without consuming the read.
        assert!(matches!(owner.admit_actor_callback(super::super::central_delegate::PeerActorCallback::CharacteristicWritten { route: route() }), crate::ubm_reclaim_graph_gate::Admission::Deferred));
        owner.invalidate(crate::PlatformError::new("corebluetooth", "cut-original", "cut"));
        {
            let mut state = owner.waiters.lock().unwrap();
            assert!(!state.ready, "direct invalidation must take effect at admission");
            state.read(route(), after.get_state_clone());
            let gate = state.reclaim_gate.as_mut().unwrap();
            if !committed { assert_eq!(gate.commit(&key), Err(GateError::Invalidated)); }
            gate.release(&key).unwrap();
        }
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(before.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "cut-original"));
        assert!(after.as_mut().poll(&mut cx).is_pending());
    }

    #[test]
    fn precommit_direct_invalidation_overflow_captures_waiter() {
        direct_invalidation_overflow_captures_waiter(false);
    }

    #[test]
    fn postcommit_direct_invalidation_overflow_captures_waiter() {
        direct_invalidation_overflow_captures_waiter(true);
    }

    #[test]
    fn repeated_empty_direct_cuts_do_not_grow_the_mandatory_queue() {
        let owner = PeerSourceOwner::new();
        let key = key();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new(0, 0);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        for _ in 0..1000 {
            owner.invalidate(crate::PlatformError::new("corebluetooth", "cut", "cut"));
        }
        let mut state = owner.waiters.lock().unwrap();
        let gate = state.reclaim_gate.as_mut().unwrap();
        gate.release(&key).unwrap();
        assert!(matches!(gate.next(), Ok(crate::ubm_reclaim_graph_gate::DrainItem::Event(_))));
        assert!(matches!(gate.next(), Ok(crate::ubm_reclaim_graph_gate::DrainItem::Finished)));
    }

    #[test]
    fn dropped_owner_recovers_captured_invalidation_waiter_and_terminal() {
        use std::future::Future;
        use std::task::{Context, Poll};
        use crate::ubm_peer_notification_ingress::{IngressRecord, Poll as SourcePoll, TerminalReason};
        let owner = PeerSourceOwner::new();
        let key = key();
        let mut waiter = Box::pin(CoreBluetoothReplyFuture::default());
        owner.faults.start_source_generation(1).unwrap();
        let (_, consumer) = owner.faults.open_source_group(1, 8, 64).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.generation = Some(1);
            state.read(route(), waiter.get_state_clone());
            let mut gate = GraphGate::new(0, 0);
            gate.acquire(key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        owner.invalidate(crate::PlatformError::new("corebluetooth", "original-cut", "cut"));
        drop(owner);
        let waker = futures::task::noop_waker();
        assert!(matches!(waiter.as_mut().poll(&mut Context::from_waker(&waker)), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "original-cut"));
        assert!(matches!(consumer.poll(), SourcePoll::Record(IngressRecord::Terminal { reason: TerminalReason::Closed { tail_uncertain: true }, .. })));
    }

    #[test]
    fn dropped_cut_releases_terminal_before_waking_its_waiter() {
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Wake, Waker};
        use crate::ubm_peer_notification_ingress::{IngressRecord, Poll as SourcePoll};
        struct Probe(crate::common::util::SourceGroupConsumer, AtomicBool);
        impl Wake for Probe {
            fn wake(self: Arc<Self>) {
                self.1.store(matches!(self.0.poll(), SourcePoll::Record(IngressRecord::Terminal { .. })), Ordering::SeqCst);
            }
        }
        let owner = PeerSourceOwner::new();
        let key = key();
        let mut waiter = Box::pin(CoreBluetoothReplyFuture::default());
        owner.faults.start_source_generation(1).unwrap();
        let (_, consumer) = owner.faults.open_source_group(1, 8, 64).unwrap();
        let probe = Arc::new(Probe(consumer, AtomicBool::new(false)));
        let waker = Waker::from(probe.clone());
        assert!(waiter.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.generation = Some(1);
            state.read(route(), waiter.get_state_clone());
            let mut gate = GraphGate::new(0, 0);
            gate.acquire(key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        owner.invalidate(crate::PlatformError::new("corebluetooth", "cut", "cut"));
        drop(owner);
        assert!(probe.1.load(Ordering::SeqCst), "the terminal must precede its waiter wake");
    }

    #[tokio::test]
    async fn dropped_owner_publishes_captured_legacy_source_failure() {
        use crate::common::util::{retained_notifications, NotificationFaults};
        let owner = PeerSourceOwner::new();
        let key = key();
        let faults = Arc::new(NotificationFaults::new(8));
        let (sender, receiver) = tokio::sync::broadcast::channel(8);
        let mut legacy = retained_notifications(receiver, faults.clone());
        *owner.legacy.lock().unwrap() = Some((faults, sender));
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(0, 0);
            gate.acquire(key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        owner.fail_source(crate::PlatformError::new("corebluetooth", "original-failure", "cut"), Some(route()));
        drop(owner);
        let note = futures::FutureExt::now_or_never(legacy.next()).expect("failure publication must survive owner drop").unwrap();
        assert_eq!(note.source_failure.unwrap().code, "original-failure");
    }

    #[test]
    fn overflow_cut_merges_reentrant_lifecycle_causes_once_per_generation() {
        use super::super::central_delegate::PeerLifecycle;
        let owner = PeerSourceOwner::new();
        let key = key();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.revision = 7;
            let mut gate = GraphGate::new(1, 1024);
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), true, &[1]));
        assert!(owner.admit_lifecycle(PeerLifecycle::Disconnected));
        assert_eq!(owner.waiters.lock().unwrap().revision, 8);
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let order = std::cell::RefCell::new(Vec::new());
        owner.drain_reclaim_callbacks(|_, value| {
            assert_eq!(value, [1]);
            order.borrow_mut().push("value");
            assert!(owner.admit_lifecycle(PeerLifecycle::ServicesModified));
            assert!(owner.admit_lifecycle(PeerLifecycle::Disconnected));
            assert_eq!(owner.waiters.lock().unwrap().revision, 8);
        }, |_| {}, |event| {
            order.borrow_mut().push(match event {
                PeerLifecycle::Disconnected | PeerLifecycle::DisconnectedWithError(_) => "disconnected",
                PeerLifecycle::ServicesModified => "services-modified",
            });
        }, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert_eq!(*order.borrow(), vec!["value", "disconnected", "services-modified"]);
        assert_eq!(owner.waiters.lock().unwrap().revision, 8);
    }

    #[test]
    fn adapter_failure_during_reclaim_holds_waiter_until_prefix_drains() {
        use std::future::Future;
        use std::task::{Context, Poll};
        let owner = PeerSourceOwner::new();
        let key = key();
        let mut waiter = Box::pin(CoreBluetoothReplyFuture::default());
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.read(route(), waiter.get_state_clone());
            let mut gate = GraphGate::new(1, 1024);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(matches!(owner.admit_actor_callback(super::super::central_delegate::PeerActorCallback::CharacteristicWritten { route: route() }), crate::ubm_reclaim_graph_gate::Admission::Deferred));
        owner.fail_source(crate::PlatformError::new("corebluetooth", "adapter-failure", "exact failure"), None);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(waiter.as_mut().poll(&mut cx).is_pending(), "the captured failure must remain behind the accepted prefix");
        assert!(!owner.waiters.lock().unwrap().ready);
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut delivered = false;
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| delivered = true, |_| {}, |_| {}).unwrap();
        assert!(delivered);
        assert!(matches!(waiter.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "adapter-failure"));
    }

    #[test]
    fn source_failure_does_not_overtake_an_accepted_read_or_wake_a_later_waiter() {
        use std::future::Future;
        use std::task::{Context, Poll};
        let owner = PeerSourceOwner::new();
        let key = key();
        let mut first = Box::pin(CoreBluetoothReplyFuture::default());
        let mut later = Box::pin(CoreBluetoothReplyFuture::default());
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.read(route(), first.get_state_clone());
            let mut gate = GraphGate::new(1, 1024);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), false, &[42]));
        owner.waiters.lock().unwrap().read(route(), later.get_state_clone());
        owner.fail_source(crate::PlatformError::new("corebluetooth", "adapter-failure", "exact failure"), None);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(later.as_mut().poll(&mut cx).is_pending(), "failure must follow the already accepted callback");
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(first.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::CharacteristicRead(value, _)) if value == [42]));
        assert!(matches!(later.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "adapter-failure"));
    }

    #[test]
    fn refused_callback_captures_waiters_at_refusal_not_drain() {
        use std::future::Future;
        use std::task::{Context, Poll};
        use crate::ubm_reclaim_graph_gate::Admission;
        use super::super::central_delegate::PeerActorCallback;

        let owner = PeerSourceOwner::new();
        let key = key();
        let mut affected = Box::pin(CoreBluetoothReplyFuture::default());
        let mut later = Box::pin(CoreBluetoothReplyFuture::default());
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.read(route(), affected.get_state_clone());
            let mut gate = GraphGate::new(1, 1024);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(matches!(owner.admit_actor_callback(PeerActorCallback::CharacteristicWritten { route: route() }), Admission::Deferred));
        assert!(matches!(owner.admit_actor_callback(PeerActorCallback::CharacteristicWritten { route: route() }), Admission::Overflow(_)));
        owner.waiters.lock().unwrap().read(route(), later.get_state_clone());
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(affected.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "reclaim-callback-overflow"));
        assert!(later.as_mut().poll(&mut cx).is_pending(), "a waiter registered after refusal is outside the failure cut");
    }

    #[test]
    fn refused_value_captures_old_generation_without_failing_replacement() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, Poll as SourcePoll, TerminalReason};
        use std::future::Future;
        use std::task::{Context, Poll};

        let owner = PeerSourceOwner::new();
        let key = key();
        let mut affected = Box::pin(CoreBluetoothReplyFuture::default());
        let mut later = Box::pin(CoreBluetoothReplyFuture::default());
        owner.faults.start_source_generation(1).unwrap();
        let (_, old) = owner.faults.open_source_group(1, 8, 64).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.generation = Some(1);
            state.ready = true;
            state.read(route(), affected.get_state_clone());
            let mut gate = GraphGate::new(1, 1024);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(matches!(owner.admit_actor_callback(super::super::central_delegate::PeerActorCallback::CharacteristicWritten { route: route() }), crate::ubm_reclaim_graph_gate::Admission::Deferred));
        assert!(!owner.value(route(), true, &[9]));
        owner.faults.start_source_generation(2).unwrap();
        let (_, replacement) = owner.faults.open_source_group(2, 8, 64).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.generation = Some(2);
            state.ready = true;
            state.read(route(), later.get_state_clone());
            state.reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        }
        assert_eq!(old.poll(), SourcePoll::Empty);
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(affected.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "reclaim-callback-overflow"));
        assert!(later.as_mut().poll(&mut cx).is_pending());
        assert!(matches!(old.poll(), SourcePoll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, .. }, .. }) if error.code == "reclaim-callback-overflow"));
        assert_eq!(replacement.poll(), SourcePoll::Empty);
        assert_eq!(owner.waiters.lock().unwrap().generation, Some(2));
    }

    #[test]
    fn callback_overflow_cut_survives_barrier_and_preserves_source_failure() {
        use std::future::Future;
        use std::task::{Context, Poll};
        use crate::ubm_peer_notification_ingress::{IngressRecord, Poll as SourcePoll, TerminalReason};
        use crate::ubm_reclaim_graph_gate::{Admission, BarrierAdmission, GateError};
        use super::super::central_delegate::PeerActorCallback;

        let owner = PeerSourceOwner::new();
        let key = key();
        let mut affected = Box::pin(CoreBluetoothReplyFuture::default());
        let mut later = Box::pin(CoreBluetoothReplyFuture::default());
        owner.faults.start_source_generation(1).unwrap();
        let (_, consumer) = owner.faults.open_source_group(1, 8, 64).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.generation = Some(1);
            state.read(route(), affected.get_state_clone());
            let mut gate = GraphGate::new_with_barriers(1, 1024, 2, 1024);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(matches!(owner.admit_actor_callback(PeerActorCallback::CharacteristicWritten { route: route() }), Admission::Deferred));
        assert!(matches!(owner.admit_adapter_barrier(1), BarrierAdmission::Queued));
        assert!(matches!(owner.admit_actor_callback(PeerActorCallback::CharacteristicWritten { route: route() }), Admission::Overflow(_)));
        assert_eq!(consumer.poll(), SourcePoll::Empty);
        owner.waiters.lock().unwrap().read(route(), later.get_state_clone());
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        assert_eq!(owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}), Err(GateError::Paused));
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(affected.as_mut().poll(&mut cx).is_pending());
        assert_eq!(consumer.poll(), SourcePoll::Empty);
        owner.resume_adapter_barrier(1).unwrap();
        let mut reported = Vec::new();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |error| reported.push(error)).unwrap();
        assert!(matches!(affected.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "reclaim-callback-overflow"));
        assert!(later.as_mut().poll(&mut cx).is_pending());
        assert!(matches!(consumer.poll(), SourcePoll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, .. }, .. }) if error.code == "reclaim-callback-overflow"));
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].code, "reclaim-callback-overflow");
    }

    #[test]
    fn distinct_source_failure_cuts_keep_waiters_on_their_side_of_a_barrier() {
        use std::future::Future;
        use std::task::{Context, Poll};
        use crate::ubm_reclaim_graph_gate::{BarrierAdmission, GateError};

        let owner = PeerSourceOwner::new();
        let key = key();
        let mut before = Box::pin(CoreBluetoothReplyFuture::default());
        let mut between = Box::pin(CoreBluetoothReplyFuture::default());
        let mut after = Box::pin(CoreBluetoothReplyFuture::default());
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.read(route(), before.get_state_clone());
            let mut gate = GraphGate::new_with_barriers(1, 1024, 1, 1024);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(matches!(owner.admit_actor_callback(super::super::central_delegate::PeerActorCallback::CharacteristicWritten { route: route() }), crate::ubm_reclaim_graph_gate::Admission::Deferred));
        owner.fail_source(crate::PlatformError::new("corebluetooth", "first-cut", "first"), None);
        assert!(matches!(owner.admit_adapter_barrier(1), BarrierAdmission::Queued));
        owner.waiters.lock().unwrap().read(route(), between.get_state_clone());
        owner.fail_source(crate::PlatformError::new("corebluetooth", "second-cut", "second"), None);
        owner.waiters.lock().unwrap().read(route(), after.get_state_clone());

        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(before.as_mut().poll(&mut cx).is_pending());
        assert!(between.as_mut().poll(&mut cx).is_pending());
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut barrier = None;
        assert_eq!(owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |id| barrier = id, |_| {}), Err(GateError::Paused));
        assert_eq!(barrier, Some(1));
        assert!(matches!(before.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "first-cut"));
        assert!(between.as_mut().poll(&mut cx).is_pending(), "the later cut must remain behind the barrier");
        owner.resume_adapter_barrier(1).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(between.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(error)) if error.code == "second-cut"));
        assert!(after.as_mut().poll(&mut cx).is_pending());
    }

    #[test]
    fn source_failure_cut_preserves_waiter_and_source_membership_at_full_gate() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll as SourcePoll, TerminalReason};
        use std::future::Future;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Poll, Wake, Waker};

        struct CheckWake { owner: Arc<PeerSourceOwner>, calls: AtomicUsize }
        impl Wake for CheckWake {
            fn wake(self: Arc<Self>) {
                assert!(self.owner.waiters.try_lock().is_ok(), "waiter must wake outside the peer lock");
                self.calls.fetch_add(1, Ordering::SeqCst);
            }
        }

        let owner = PeerSourceOwner::new();
        let key = key();
        let mut first = Box::pin(CoreBluetoothReplyFuture::default());
        let mut affected = Box::pin(CoreBluetoothReplyFuture::default());
        let mut after = Box::pin(CoreBluetoothReplyFuture::default());
        let check = Arc::new(CheckWake { owner: owner.clone(), calls: AtomicUsize::new(0) });
        let waker = Waker::from(check.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(affected.as_mut().poll(&mut cx).is_pending());
        owner.faults.start_source_generation(1).unwrap();
        let (group, consumer) = owner.faults.open_source_group(1, 8, 64).unwrap();
        let (service, characteristic) = route();
        let member = owner.faults.register_source_route(&group, NotificationRoute {
            service_uuid: service.uuid, service_instance: service.instance,
            characteristic_uuid: characteristic.uuid, characteristic_instance: characteristic.instance,
        }, 1).unwrap();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            state.generation = Some(1);
            state.read(route(), first.get_state_clone());
            let mut gate = GraphGate::new(1, 4096);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), true, &[42]));
        owner.waiters.lock().unwrap().read(route(), affected.get_state_clone());
        let error = crate::PlatformError::new("corebluetooth", "adapter-exact", "original detail").with("native", "reason".to_owned());
        owner.fail_source(error.clone(), None);
        assert!(affected.as_mut().poll(&mut cx).is_pending(), "the failure must wait for the accepted callback");
        assert_eq!(check.calls.load(Ordering::SeqCst), 0);
        assert_eq!(consumer.poll(), SourcePoll::Empty);
        {
            let mut state = owner.waiters.lock().unwrap();
            state.read(route(), after.get_state_clone());
            assert_eq!(state.reclaim_gate.as_mut().unwrap().commit(&key), Err(GateError::Invalidated));
            state.reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        }
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(first.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::CharacteristicRead(value, _)) if value == [42]));
        assert!(matches!(affected.as_mut().poll(&mut cx), Poll::Ready(CoreBluetoothReply::Failed(actual)) if actual == error));
        assert!(after.as_mut().poll(&mut cx).is_pending(), "a later waiter is outside the cut");
        assert_eq!(check.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(consumer.poll(), SourcePoll::Record(IngressRecord::Value { membership, .. }) if membership == member));
        assert!(matches!(consumer.poll(), SourcePoll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: actual, .. }, memberships }) if actual == error && memberships == vec![(NotificationRoute { service_uuid: service.uuid, service_instance: service.instance, characteristic_uuid: characteristic.uuid, characteristic_instance: characteristic.instance }, member)]));
    }

    #[test]
    fn replay_answers_read_and_wakes_after_unlock() {
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Wake, Waker};

        struct CheckWake { owner: Arc<PeerSourceOwner>, called: AtomicBool }
        impl Wake for CheckWake {
            fn wake(self: Arc<Self>) {
                assert!(self.owner.waiters.try_lock().is_ok());
                self.called.store(true, Ordering::SeqCst);
            }
        }
        let owner = PeerSourceOwner::new();
        let key = key();
        let mut read = Box::pin(CoreBluetoothReplyFuture::default());
        let check = Arc::new(CheckWake { owner: owner.clone(), called: AtomicBool::new(false) });
        let waker = Waker::from(check.clone());
        assert!(read.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
        {
            let mut state = owner.waiters.lock().unwrap();
            state.read(route(), read.get_state_clone());
            let mut gate = GraphGate::new(2, 8 + 2 * std::mem::size_of::<DeferredPeerCallback>());
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), false, &[3]));
        assert!(!check.called.load(Ordering::SeqCst));
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| panic!("read-only callback is not a notification"), |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(check.called.load(Ordering::SeqCst));
        assert!(matches!(read.as_mut().poll(&mut Context::from_waker(&waker)), std::task::Poll::Ready(CoreBluetoothReply::CharacteristicRead(value, _)) if value == vec![3]));
    }

    #[test]
    fn reserved_read_wake_can_reenter_callback_during_drain() {
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Wake, Waker};

        struct Reenter { owner: Arc<PeerSourceOwner>, called: AtomicBool }
        impl Wake for Reenter {
            fn wake(self: Arc<Self>) {
                assert!(!self.called.swap(true, Ordering::SeqCst));
                assert!(!self.owner.value(route(), true, &[4]));
            }
        }
        let owner = PeerSourceOwner::new();
        let key = key();
        let mut read = Box::pin(CoreBluetoothReplyFuture::default());
        let check = Arc::new(Reenter { owner: owner.clone(), called: AtomicBool::new(false) });
        let waker = Waker::from(check.clone());
        assert!(read.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
        {
            let mut state = owner.waiters.lock().unwrap();
            state.read(route(), read.get_state_clone());
            let mut gate = GraphGate::new(4, 32 + 4 * std::mem::size_of::<DeferredPeerCallback>());
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), true, &[3]));
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut delivered = Vec::new();
        owner.drain_reclaim_callbacks(|_, value| delivered.push(value), |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(check.called.load(Ordering::SeqCst));
        assert_eq!(delivered, vec![vec![3], vec![4]]);
        assert!(matches!(read.as_mut().poll(&mut Context::from_waker(&waker)), std::task::Poll::Ready(CoreBluetoothReply::CharacteristicRead(value, _)) if value == vec![3]));
    }

    #[tokio::test]
    async fn held_disable_installs_exact_cleanup_before_later_value() {
        use super::super::central_delegate::PeerDelegateCallback;
        use super::super::peer_waiters::WaiterError;
        use crate::api::ReadProvenance;
        let owner = PeerSourceOwner::new();
        let key = key();
        let disable = CoreBluetoothReplyFuture::default();
        let enable = CoreBluetoothReplyFuture::default();
        let read = CoreBluetoothReplyFuture::default();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.disable(route(), disable.get_state_clone()).unwrap();
            let mut gate = GraphGate::new(8, 1024);
            gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(owner.admit_delegate_callback(PeerDelegateCallback::NotificationState { route: route(), notifying: false }));
        {
            let mut state = owner.waiters.lock().unwrap();
            assert_eq!(state.enable(route(), enable.get_state_clone()), Err(WaiterError::CleanupPending));
            state.read(route(), read.get_state_clone());
        }
        assert!(!owner.value(route(), false, &[7]));
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut cleanup = None;
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |event| {
            if let PeerDelegateCallback::CapturedNotificationState { cleanup: id, .. } = event { cleanup = id; }
        }, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(read.await, CoreBluetoothReply::CharacteristicRead(value, ReadProvenance::ReadOrNotification) if value == [7]));
        owner.with_waiters(|state| state.finish_cleanup(route(), cleanup.unwrap(), Ok(())).unwrap());
        assert!(matches!(disable.await, CoreBluetoothReply::Ok));
    }

    #[test]
    fn reentrant_drain_cannot_overtake_current_handoff() {
        use crate::ubm_reclaim_graph_gate::GateError;
        let owner = PeerSourceOwner::new();
        let key = key();
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(4, 32 + 4 * std::mem::size_of::<DeferredPeerCallback>());
            gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), true, &[1]));
        assert!(!owner.value(route(), true, &[2]));
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut delivered = Vec::new();
        owner.drain_reclaim_callbacks(|_, value| {
            delivered.push(value);
            assert_eq!(owner.drain_reclaim_callbacks(|_, _| panic!("nested handoff"), |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}), Err(GateError::Busy));
        }, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert_eq!(delivered, vec![vec![1], vec![2]]);
    }

    #[test]
    fn retained_error_accounting_includes_spare_metadata_capacity() {
        let mut error = crate::PlatformError::new("corebluetooth", String::with_capacity(128), String::with_capacity(256));
        error.metadata.reserve(8);
        error.metadata.push(("key", String::with_capacity(512)));
        assert!(retained_error_bytes(&error) >= 128 + 256 + 512 + 8 * std::mem::size_of::<(&'static str, String)>());
    }

    #[test]
    fn delegate_error_notification_and_lifecycle_share_fifo() {
        use super::super::central_delegate::{PeerDelegateCallback, PeerLifecycle};
        let owner = PeerSourceOwner::new();
        let key = key();
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(4, 1024);
            gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(owner.admit_delegate_callback(PeerDelegateCallback::ValueError {
            route: route(), notifying: true, error: crate::PlatformError::new("corebluetooth", "failed", "failed"),
        }));
        assert!(owner.admit_delegate_callback(PeerDelegateCallback::NotificationState { route: route(), notifying: false }));
        assert!(owner.admit_lifecycle(PeerLifecycle::Disconnected));
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let order = std::cell::RefCell::new(Vec::new());
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| order.borrow_mut().push("disconnect"), |event| {
            order.borrow_mut().push(match event {
                PeerDelegateCallback::ValueError { .. } | PeerDelegateCallback::CapturedValueError { .. } => "value-error",
                PeerDelegateCallback::NotificationState { .. } | PeerDelegateCallback::CapturedNotificationState { .. } => "notification-state",
                PeerDelegateCallback::NotificationError { .. } => "notification-error",
            });
        }, |_| {}, |_| {}, |_| {}).unwrap();
        assert_eq!(*order.borrow(), vec!["value-error", "notification-state", "disconnect"]);
    }

    #[test]
    fn lifecycle_cut_retires_graph_before_later_callbacks() {
        use super::super::central_delegate::PeerLifecycle;
        let owner = PeerSourceOwner::new();
        let key = key();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new(4, 1024);
            gate.acquire(key.clone()).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(owner.admit_lifecycle(PeerLifecycle::Disconnected));
        assert!(!owner.waiters.lock().unwrap().ready, "the logical cut precedes replay");
        assert_eq!(owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().commit(&key), Err(GateError::Invalidated));
    }

    #[test]
    fn postcommit_overflow_fails_source_after_accepted_prefix() {
        let owner = PeerSourceOwner::new();
        let key = key();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new(1, 8 + std::mem::size_of::<DeferredPeerCallback>());
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value(route(), true, &[1]));
        assert!(!owner.value(route(), true, &[2]));
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut delivered = Vec::new();
        owner.drain_reclaim_callbacks(|_, value| {
            assert!(owner.waiters.try_lock().is_ok());
            delivered.push(value);
        }, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert_eq!(delivered, vec![vec![1]]);
        assert!(!owner.waiters.lock().unwrap().ready);
    }
}
