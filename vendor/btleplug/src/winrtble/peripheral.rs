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
    ble::characteristic::{BLECharacteristic, NotifyEventHandler, value_changed},
    ble::descriptor::{AttributeKey, BLEDescriptor},
    ble::device::{BLEDevice, CharacteristicList, PartialBLEDevice},
    ble::service::BLEService,
    gatt_model::{self, index_unique},
    utils,
};
use crate::{
    Error, Result,
    api::{
        self, AddressType, BDAddr, CentralEvent, Characteristic, ConnectionParameterPreset,
        ConnectionParameters, ConnectionParametersReport, Descriptor, Peripheral as ApiPeripheral,
        PeripheralProperties, Service, ValueNotification, WriteType,
    },
    common::{
        adapter_manager::AdapterManager,
        util::{
            NotificationEnvelope, NotificationFaults, SourceWriteReceipt, SourceWriteRequest,
            SourceWriteStart, retained_notification_events, retained_notifications_from_sender,
        },
    },
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
    sync::{Arc, Mutex, RwLock},
};
use tokio::sync::broadcast;
use uuid::Uuid;

use std::sync::Weak;

pub use crate::common::util::{SourceGroupConsumer, SourceGroupError, SourceGroupHandle};
use crate::ubm_peer_notification_ingress::{Membership, NotificationRoute};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattClientCharacteristicConfigurationDescriptorValue, GattDescriptor,
    GattDeviceService,
};
use windows::Devices::Bluetooth::{Advertisement::*, BluetoothAddressType};

#[cfg(test)]
#[path = "lifecycle_review_tests.rs"]
mod lifecycle_review_tests;
#[cfg(test)]
#[path = "residual_discovery_tests.rs"]
mod residual_discovery_tests;
#[cfg(test)]
#[path = "service_handoff_retry_tests.rs"]
mod service_handoff_retry_tests;

#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_cr")
)]
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PeripheralId(BDAddr, u8);

impl PeripheralId {
    pub fn with_address_type(address: BDAddr, kind: Option<AddressType>) -> Self {
        Self(
            address,
            match kind {
                None => 0,
                Some(AddressType::Public) => 1,
                Some(AddressType::Random) => 2,
            },
        )
    }

    pub fn address_type(&self) -> Option<AddressType> {
        match self.1 {
            1 => Some(AddressType::Public),
            2 => Some(AddressType::Random),
            _ => None,
        }
    }
    /// UBM patch (UBM_PATCHES.md #19): the peripheral's Bluetooth address.
    pub(crate) fn address(&self) -> BDAddr {
        self.0
    }
}

impl Display for PeripheralId {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let kind = match self.1 {
            1 => "public",
            2 => "random",
            _ => "unknown",
        };
        write!(f, "{kind}:{}", self.0)
    }
}

impl std::str::FromStr for PeripheralId {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (raw, kind) = if let Some(raw) = value.strip_prefix("public:") {
            (raw, Some(AddressType::Public))
        } else if let Some(raw) = value.strip_prefix("random:") {
            (raw, Some(AddressType::Random))
        } else if let Some(raw) = value.strip_prefix("unknown:") {
            (raw, None)
        } else {
            (value, None)
        };
        let address = raw
            .parse::<BDAddr>()
            .map_err(|_| "invalid WinRT peer address")?;
        Ok(Self::with_address_type(address, kind))
    }
}

/// Implementation of [api::Peripheral](crate::api::Peripheral).
#[derive(Clone)]
pub struct Peripheral {
    shared: Arc<Shared>,
}

/// One native owner per address within a manager, independent of adapter caches.
/// Capacity is a hard admission limit: unresolved effects cannot be evicted
/// safely merely to make room for another peer.
pub(crate) struct PeerRegistry {
    peers: Mutex<HashMap<PeripheralId, Arc<Shared>>>,
    /// Explicit host-owned evidence storage. A registry never synthesizes a
    /// filesystem location; every peer created by this registry receives the
    /// same configured owner so aggregate backpressure is actually shared.
    failure_evidence_store:
        Option<Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>>,
}

impl PeerRegistry {
    pub(crate) fn new(
        failure_evidence_store: Option<
            Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>,
        >,
    ) -> Self {
        Self {
            peers: Mutex::new(HashMap::new()),
            failure_evidence_store,
        }
    }

    pub(crate) fn failure_evidence_configured(&self) -> bool {
        self.failure_evidence_store.is_some()
    }
}

impl Default for PeerRegistry {
    fn default() -> Self {
        Self::new(None)
    }
}

pub(crate) fn shared_peer_owners() -> (
    Arc<PeerRegistry>,
    Arc<Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
) {
    static OWNERS: std::sync::OnceLock<(
        Arc<PeerRegistry>,
        Arc<Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
    )> = std::sync::OnceLock::new();
    OWNERS
        .get_or_init(|| {
            (
                Arc::new(PeerRegistry::default()),
                Arc::new(Mutex::new(Default::default())),
            )
        })
        .clone()
}
impl PeerRegistry {
    pub(crate) fn get(
        &self,
        adapter: Weak<AdapterManager<Peripheral>>,
        identity: PeripheralId,
        authorities: Arc<Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
    ) -> Result<Peripheral> {
        // All registry keys must be native-resolved before admission. A bare
        // address cannot distinguish public and random peers with equal bits.
        if identity.address_type().is_none() {
            return Err(Error::Other(
                "WinRT peer address type must be resolved before admission".into(),
            ));
        }
        let mut peers = self
            .peers
            .lock()
            .map_err(|_| Error::Other("WinRT peer registry lock poisoned".into()))?;
        if let Some(shared) = peers.get(&identity) {
            shared.add_adapter(adapter);
            return Ok(Peripheral {
                shared: shared.clone(),
            });
        }
        peers.retain(|_, shared| Arc::strong_count(shared) != 1 || shared.must_retain());
        if peers.len() >= 4096 {
            return Err(Error::Other("WinRT peer owner capacity exhausted".into()));
        }
        let peripheral = Peripheral::new_with_id_with_failure_evidence_store(
            adapter,
            identity.clone(),
            authorities,
            self.failure_evidence_store.clone(),
        );
        peers.insert(identity, peripheral.shared.clone());
        Ok(peripheral)
    }
}

struct Shared {
    identity: PeripheralId,
    device: Arc<tokio::sync::Mutex<Option<BLEDevice>>>,
    partial_device: Arc<tokio::sync::Mutex<Option<PartialBLEDevice>>>,
    adapters: Mutex<Vec<Weak<AdapterManager<Peripheral>>>>,
    address: BDAddr,
    mtu: AtomicU16,
    connected: AtomicBool,
    /// UBM patch (`winrt-attribute-instances`): keyed by (UUID,
    /// `AttributeHandle`), so repeated service UUIDs stay distinct.
    ble_services: DashMap<AttributeKey, BLEService>,
    // Displaced services remain owners until every registration is removed.
    retired_services: Mutex<Vec<BLEService>>,
    notifications_channel: broadcast::Sender<NotificationEnvelope>,
    notification_faults: Arc<NotificationFaults>,
    managed_callbacks: Arc<super::callback_retirement::CallbackRetirement>,
    // Keeps an old owner's fence from closing a replacement owner's gates.
    // Never acquire while holding source or graph locks.
    callback_transition: Mutex<()>,
    // Serializes source generation activation with disconnect fencing.
    source_connection: Mutex<SourceConnection>,
    graph: Mutex<NativeGraph>,
    // At most one source-failure cut can wait for durable evidence capacity:
    // admission fences the generation before a second cut can exist. Keeping
    // this owner is intentional backpressure, never a logged-and-dropped
    // native error.
    pending_failure_spill: Mutex<Option<crate::common::util::RetriableFutureFailureCut>>,
    // Set from native failure admission through the final durable source-ACK
    // retirement. A persisted blob is not completion: service changes and
    // teardown must retain their exact work until every recipient has settled.
    pending_durable_failure: AtomicBool,
    deferred_services_changed: Mutex<Option<DeferredServicesChanged>>,
    reclaim_drain_changed: tokio::sync::Notify,
    reclaim_authorities: Arc<Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
    // Detached native operations hold a read lease until their actual
    // completion; native close and replacement require the exclusive lease.
    notification_lifecycle: Arc<tokio::sync::RwLock<()>>,
    ordinary_cccd_lock: tokio::sync::Mutex<()>,
    ordinary_cccd_uncertain: Mutex<HashSet<(Uuid, u64, Uuid, u64)>>,
    ordinary_cccd_local_cleanup: Mutex<HashSet<(Uuid, u64, Uuid, u64)>>,
    /// Observed WinRT connection parameters. The sender outlives the
    /// device so a subscriber can see the channel close on disconnect.
    connection_parameters: broadcast::Sender<ConnectionParametersReport>,

    // Mutable, advertised, state...
    address_type: RwLock<Option<AddressType>>,
    explicit_address_type: RwLock<Option<AddressType>>,
    local_name: RwLock<Option<String>>,
    advertisement_name: RwLock<Option<String>>,
    last_tx_power_level: RwLock<Option<i16>>, // XXX: would be nice to avoid lock here!
    last_rssi: RwLock<Option<i16>>,           // XXX: would be nice to avoid lock here!
    latest_manufacturer_data: RwLock<HashMap<u16, Vec<u8>>>,
    latest_service_data: RwLock<HashMap<Uuid, Vec<u8>>>,
    services: RwLock<HashSet<Uuid>>,
    class: RwLock<Option<u32>>,
}

/// A cleanup reserves the sole source-failure cut before it enters WinRT so a
/// native failure can retain the exact source-time recipient set. Successful
/// cleanup must release that reservation too: dropping `FutureFailureCut`
/// removes its source slots but does not invoke the durable-failure finalizer
/// because no failure was published. This RAII owner makes every early return
/// restore service-change and disconnect admission.
struct ReservedFailureCut {
    shared: Arc<Shared>,
    cut: Option<crate::common::util::FutureFailureCut>,
}

impl ReservedFailureCut {
    fn new(shared: Arc<Shared>, cut: Option<crate::common::util::FutureFailureCut>) -> Self {
        Self { shared, cut }
    }

    fn take(&mut self) -> Option<crate::common::util::FutureFailureCut> {
        self.cut.take()
    }
}

impl Drop for ReservedFailureCut {
    fn drop(&mut self) {
        if let Some(cut) = self.cut.take() {
            self.shared.abandon_notification_failure_cut(cut);
        }
    }
}

struct DeferredServicesChanged {
    owner: Arc<AtomicBool>,
    identity: PeripheralId,
}

struct NativeGraph {
    attachment: Option<u64>,
    revision: u64,
    exhausted: bool,
    ready: bool,
    reclaim_binding: crate::ubm_reclaim_graph_binding::GraphBindingOwner,
    reclaim_operations: usize,
    reclaim_effect_uncertain: bool,
    // A task was destroyed without a terminal observation. A lease cannot
    // survive task destruction, so prohibit replacement of its native owner.
    native_operation_abandoned: bool,
    reclaim_callback_uncertain: bool,
    reclaim_callback_recoverable: bool,
    reclaim_overflow_epoch: Option<u64>,
    reclaim_gate: crate::ubm_reclaim_graph_gate::GraphGate<Box<dyn FnOnce() + Send>>,
    reclaim_drain_graph: Option<(String, u64, u64)>,
}
struct ReclaimOperationGuard {
    shared: Arc<Shared>,
    effectful: bool,
    settled: bool,
    completed: bool,
}

fn ordinary_cccd_key(characteristic: &Characteristic) -> (Uuid, u64, Uuid, u64) {
    (
        characteristic.service_uuid,
        characteristic.service_instance,
        characteristic.uuid,
        characteristic.instance,
    )
}

#[derive(Debug, PartialEq, Eq)]
enum CccdRecoveryAction {
    RemoveRegistration,
    Unsubscribe,
    Disconnect,
}

fn cccd_recovery_action(graph_ready: bool, local_only: bool) -> CccdRecoveryAction {
    if local_only {
        CccdRecoveryAction::RemoveRegistration
    } else if graph_ready {
        CccdRecoveryAction::Unsubscribe
    } else {
        CccdRecoveryAction::Disconnect
    }
}

fn cccd_recovery_error(graph_ready: bool) -> Error {
    match cccd_recovery_action(graph_ready, false) {
        CccdRecoveryAction::Unsubscribe => Error::Other("WinRT CCCD outcome is uncertain; unsubscribe to recover".into()),
        CccdRecoveryAction::Disconnect => Error::Other("WinRT CCCD outcome is uncertain and the service graph changed; disconnect and reconnect to recover".into()),
        CccdRecoveryAction::RemoveRegistration => unreachable!(),
    }
}

async fn settle_discovery_queries<F, T>(queries: impl IntoIterator<Item = F>) -> Result<Vec<T>>
where
    F: std::future::Future<Output = Result<T>>,
{
    futures::future::join_all(queries)
        .await
        .into_iter()
        .collect()
}

// Installation can succeed even when later registration removal fails. The
// native owner must move with the table before any fallible cleanup step.
fn install_and_retire_services<T>(
    owner: &mut T,
    install: impl FnOnce() -> Result<()>,
    activate: impl FnOnce(&mut T),
    retire_registrations: impl FnOnce() -> Result<()>,
    close_retired: impl FnOnce(&mut T) -> Result<()>,
) -> Result<()> {
    install()?;
    activate(owner);
    retire_registrations()?;
    close_retired(owner)
}
impl Drop for ReclaimOperationGuard {
    fn drop(&mut self) {
        let mut graph = self
            .shared
            .graph
            .lock()
            .expect("WinRT graph owner lock poisoned");
        graph.reclaim_operations -= 1;
        if self.effectful && !self.settled {
            graph.reclaim_effect_uncertain = true;
        }
        if !self.completed {
            graph.native_operation_abandoned = true;
        }
    }
}

struct MarkerReplyGuard {
    operation: ReclaimOperationGuard,
    receipt: SourceWriteReceipt,
}
impl Drop for MarkerReplyGuard {
    fn drop(&mut self) {
        self.receipt.settle(Err(crate::PlatformError::new(
            "winrt",
            "write-reply-abandoned",
            "The write reply was abandoned; the remote effect is unknown",
        )));
    }
}

fn marker_write_definitively_settled(
    result: &std::result::Result<(), crate::PlatformError>,
) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => {
            error.domain == "winrt"
                && error.code == "gatt-status"
                && error
                    .metadata
                    .iter()
                    .any(|(key, value)| *key == "gattStatus" && value == "protocol-error")
                && error
                    .metadata
                    .iter()
                    .any(|(key, value)| *key == "attError" && value.parse::<u8>().is_ok())
        }
    }
}

fn spawn_marker_write_reply(
    runtime: &tokio::runtime::Handle,
    shared: Arc<Shared>,
    lease: tokio::sync::OwnedRwLockReadGuard<()>,
    receipt: SourceWriteReceipt,
    operation: impl std::future::Future<Output = std::result::Result<(), crate::PlatformError>>
    + Send
    + 'static,
) {
    let guard = MarkerReplyGuard {
        operation: ReclaimOperationGuard {
            shared,
            effectful: true,
            settled: false,
            completed: false,
        },
        receipt,
    };
    runtime.spawn(async move {
        let _lease = lease;
        let mut guard = guard;
        let result = operation.await;
        let result = if marker_write_definitively_settled(&result) {
            result
        } else {
            result.map_err(|error| error.with("remoteEffect", "unknown"))
        };
        guard.operation.completed = true;
        // A terminal native response fences future replies, even when the
        // receipt must report that the remote effect remains unknown.
        guard.operation.settled = true;
        guard.receipt.settle(result);
    });
}

impl NativeGraph {
    fn note_callback_overflow(&mut self) {
        self.reclaim_callback_uncertain = true;
        self.reclaim_callback_recoverable = false;
        self.reclaim_overflow_epoch = self
            .reclaim_overflow_epoch
            .and_then(|epoch| epoch.checked_add(1));
    }

    fn finish_callback_fence(&mut self, epoch: Option<u64>, source_fenced: bool) {
        self.reclaim_callback_recoverable =
            source_fenced && epoch.is_some() && self.reclaim_overflow_epoch == epoch;
    }

    fn publish_discovery(&mut self, attachment: u64, revision: u64) {
        self.reclaim_binding.attach();
        if self
            .reclaim_binding
            .publish(("winrt".to_owned(), attachment, revision))
            && !self.reclaim_gate.is_busy()
            && self.reclaim_callback_recoverable
        {
            self.reclaim_callback_uncertain = false;
            self.reclaim_callback_recoverable = false;
        }
    }

    fn invalidate(&mut self) {
        self.reclaim_binding.disconnect();
        self.ready = false;
        match self.revision.checked_add(1) {
            Some(next) => self.revision = next,
            None => self.exhausted = true,
        }
    }
    fn identity(&self) -> Result<Option<(String, u64, u64)>> {
        let attachment = self
            .attachment
            .ok_or_else(|| Error::Other("WinRT graph attachment exhausted".into()))?;
        if self.exhausted {
            return Err(Error::Other("WinRT graph revision exhausted".into()));
        }
        Ok(self
            .ready
            .then(|| ("winrt".to_owned(), attachment, self.revision)))
    }
}

/// Checks the graph at the operation's native initiation boundary.
fn start_on_current_graph<T>(
    graph: &NativeGraph,
    expected: &(String, u64, u64),
    start: impl FnOnce() -> T,
) -> Result<T> {
    if graph.identity().ok().flatten().as_ref() != Some(expected) {
        return Err(Error::Other(
            "WinRT service graph changed before native initiation".into(),
        ));
    }
    Ok(start())
}

fn start_marker_on_current_owner<T>(
    connection: &Mutex<SourceConnection>,
    graph: &Mutex<NativeGraph>,
    faults: &NotificationFaults,
    group: &SourceGroupHandle,
    expected: &(String, u64, u64),
    request: SourceWriteRequest,
    initiate: impl FnOnce() -> std::result::Result<T, crate::PlatformError>,
) -> std::result::Result<SourceWriteStart<T>, SourceGroupError> {
    if let Some(receipt) = group.replay_marker_write(&request)? {
        return Ok(SourceWriteStart::Replay(receipt));
    }
    let _wake_scope = faults.defer_wakes();
    let connection = connection
        .lock()
        .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
    let mut graph = graph
        .lock()
        .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
    if !connection.connected
        || graph.identity().ok().flatten().as_ref() != Some(expected)
        || request.graph != crate::common::util::marker_graph_identity(expected)
    {
        return Err(SourceGroupError::StaleGeneration);
    }
    if graph.reclaim_gate.is_busy() || graph.reclaim_operations >= 256 {
        return Err(SourceGroupError::SourceStateUnavailable);
    }
    let start = faults.start_source_marker_write(group, request, initiate)?;
    if matches!(&start, SourceWriteStart::Initiated { .. }) {
        graph.reclaim_operations += 1;
    }
    Ok(start)
}

struct SourceConnection {
    owner: Option<Arc<AtomicBool>>,
    connected: bool,
    activations: HashMap<(u64, u16), Arc<tokio::sync::Mutex<ActivationState>>>,
    physical_lanes: HashMap<u16, Arc<tokio::sync::Mutex<()>>>,
    source_generation: Option<u64>,
    next_activation: u64,
}

use super::activation_cache::graph_activation;

impl SourceConnection {
    fn physical_lane(&mut self, handle: u16) -> Arc<tokio::sync::Mutex<()>> {
        self.physical_lanes
            .entry(handle)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
    fn begin_disconnect(&mut self) {
        self.connected = false;
        self.source_generation = None;
    }

    fn observe_disconnect(&mut self, owner: &Arc<AtomicBool>) -> bool {
        owner.store(true, Ordering::Relaxed);
        if !self
            .owner
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, owner))
        {
            return false;
        }
        self.connected = false;
        self.source_generation = None;
        true
    }

    fn finish_connect(&mut self, owner: &Arc<AtomicBool>) -> bool {
        if owner.load(Ordering::Relaxed)
            || !self
                .owner
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, owner))
        {
            return false;
        }
        self.connected = true;
        true
    }
}

/// Identity of one native ATT characteristic within a particular graph.
#[derive(Clone, Debug)]
pub struct NotificationActivationIdentity {
    connection: Arc<AtomicBool>,
    handle: u16,
    graph_revision: u64,
}

impl PartialEq for NotificationActivationIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.handle == other.handle
            && self.graph_revision == other.graph_revision
            && Arc::ptr_eq(&self.connection, &other.connection)
    }
}
impl Eq for NotificationActivationIdentity {}
impl std::hash::Hash for NotificationActivationIdentity {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&Arc::as_ptr(&self.connection), state);
        std::hash::Hash::hash(&self.handle, state);
        std::hash::Hash::hash(&self.graph_revision, state);
    }
}

struct ActivationState {
    gatt: GattCharacteristic,
    characteristic: Characteristic,
    registration: Option<ActivationRegistration>,
    prepared: Arc<Mutex<Option<PreparedActivation>>>,
}

#[derive(Clone, Copy)]
struct PreparedActivation {
    generation: u64,
    id: u64,
    claims: usize,
}

struct ActivationRegistration {
    token: Option<i64>,
    epoch: u64,
    config: GattClientCharacteristicConfigurationDescriptorValue,
    claims: usize,
    phase: ActivationPhase,
    live: Arc<AtomicBool>,
    callbacks: Arc<super::callback_retirement::CallbackRetirement>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActivationPhase {
    Enabling,
    Active,
    Retiring,
    FailedRetirement,
    Indeterminate,
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
enum RetirementAction {
    LocalOnly,
    GraphUncertain,
    Uncertain,
    WriteCccd,
}

fn retirement_action(
    phase: ActivationPhase,
    connection_retired: bool,
    graph_current: bool,
) -> RetirementAction {
    // Failed is entered only after the enable was proven to have had no
    // remote effect and removal of its exact local handler failed. Retrying
    // that removal never writes to a possibly replaced characteristic.
    if connection_retired || phase == ActivationPhase::Failed {
        RetirementAction::LocalOnly
    } else if !graph_current {
        RetirementAction::GraphUncertain
    } else if matches!(
        phase,
        ActivationPhase::Enabling | ActivationPhase::Retiring | ActivationPhase::Indeterminate
    ) {
        RetirementAction::Uncertain
    } else {
        RetirementAction::WriteCccd
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ActivationRecovery {
    Uncertain,
    FinishRetirement,
    Join,
}

impl ActivationRegistration {
    fn recovery(
        &self,
        config: GattClientCharacteristicConfigurationDescriptorValue,
    ) -> Result<ActivationRecovery> {
        match self.phase {
            ActivationPhase::Enabling => {
                if self.config != config {
                    return Err(Error::NotSupported(
                        "Conflicting notification configuration".into(),
                    ));
                }
                Ok(ActivationRecovery::Uncertain)
            }
            ActivationPhase::Retiring | ActivationPhase::Indeterminate => {
                Ok(ActivationRecovery::Uncertain)
            }
            ActivationPhase::FailedRetirement | ActivationPhase::Failed => {
                Ok(ActivationRecovery::FinishRetirement)
            }
            ActivationPhase::Active => Ok(ActivationRecovery::Join),
        }
    }

    fn join(
        &mut self,
        claim: &mut Option<u64>,
        config: GattClientCharacteristicConfigurationDescriptorValue,
    ) -> Result<()> {
        if claim_matches(*claim, self.epoch) {
            return if self.phase == ActivationPhase::Active {
                Ok(())
            } else {
                Err(Error::Other(
                    "WinRT notification activation requires cleanup".into(),
                ))
            };
        }
        if self.phase != ActivationPhase::Active {
            return Err(Error::Other(
                "WinRT notification activation requires cleanup".into(),
            ));
        }
        if self.config != config {
            return Err(Error::NotSupported(
                "Conflicting notification configuration".into(),
            ));
        }
        self.claims = self
            .claims
            .checked_add(1)
            .ok_or_else(|| Error::Other("WinRT notification claim count exhausted".into()))?;
        *claim = Some(self.epoch);
        Ok(())
    }

    /// Returns true only for the claim that must retire the native registration.
    fn release_shared(&mut self, claim: &mut Option<u64>) -> bool {
        if !claim_matches(*claim, self.epoch) {
            return false;
        }
        if self.claims > 1 {
            self.claims -= 1;
            *claim = None;
            return false;
        }
        true
    }
}

/// Retains the exact COM object and registration used by this activation.
/// Owners with the same physical identity serialize their operations.
/// Connection teardown and replacement wait for the native CCCD operation to
/// settle. Dropping an operation's waiter does not cancel that operation; it
/// retains its lifecycle lease until completion and bookkeeping have settled.
#[derive(Clone)]
pub struct NotificationActivationOwner {
    identity: NotificationActivationIdentity,
    state: Arc<tokio::sync::Mutex<ActivationState>>,
    claim: Arc<Mutex<Option<u64>>>,
    prepared_claim: Arc<Mutex<Option<u64>>>,
    shared: Arc<Shared>,
    graph_identity: (String, u64, u64),
    physical_lane: Arc<tokio::sync::Mutex<()>>,
}

fn notification_route(characteristic: &Characteristic) -> NotificationRoute {
    NotificationRoute {
        service_uuid: characteristic.service_uuid,
        service_instance: characteristic.service_instance,
        characteristic_uuid: characteristic.uuid,
        characteristic_instance: characteristic.instance,
    }
}

fn marker_write_error(error: Error) -> crate::PlatformError {
    match error {
        Error::Platform(error) => error,
        error => crate::PlatformError::new("winrt", "write-error", error.to_string()),
    }
}

fn notification_retirement_error(error: &Error) -> crate::PlatformError {
    match error {
        Error::Platform(platform) => platform.clone(),
        error => {
            crate::PlatformError::new("winrt", "notification-retirement-failed", error.to_string())
        }
    }
}

fn claim_matches(claim: Option<u64>, registration_epoch: u64) -> bool {
    claim == Some(registration_epoch)
}

fn notification_retained_bytes(notification: &ValueNotification) -> usize {
    let mut bytes = notification.value.capacity();
    if let Some(error) = &notification.source_failure {
        bytes = bytes
            .saturating_add(error.code.capacity())
            .saturating_add(error.message.capacity());
        bytes = bytes.saturating_add(
            error
                .metadata
                .capacity()
                .saturating_mul(std::mem::size_of::<(&'static str, String)>()),
        );
        for (_, value) in &error.metadata {
            bytes = bytes.saturating_add(value.capacity());
        }
    }
    bytes
}

// Dropping an API waiter must not drop a WinRT operation that may still
// complete physically. The task owns the per-characteristic lock until the
// operation and its bookkeeping have settled.
#[cfg(test)]
async fn drive_independently<F, T>(operation: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    tokio::spawn(operation).await.map_err(|error| {
        Error::Other(format!("WinRT notification operation task failed: {error}").into())
    })?
}

impl NotificationActivationOwner {
    pub fn identity(&self) -> NotificationActivationIdentity {
        self.identity.clone()
    }

    fn is_current(&self) -> bool {
        let connection = self.shared.source_connection.lock().unwrap();
        connection.connected
            && connection
                .owner
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &self.identity.connection))
    }

    /// Bind this physical activation to the current source generation before
    /// registering a route. No native CCCD write is performed here.
    pub async fn prepare_source_activation(&self) -> Result<u64> {
        let _lifecycle = self.shared.notification_lifecycle.read().await;
        let state = self.state.lock().await;
        let mut connection = self.shared.source_connection.lock().unwrap();
        if !connection.connected
            || !connection
                .owner
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &self.identity.connection))
        {
            return Err(Error::NotConnected);
        }
        if self
            .shared
            .graph
            .lock()
            .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?
            .identity()?
            .as_ref()
            != Some(&self.graph_identity)
        {
            return Err(Error::Other("WinRT notification graph is stale".into()));
        }
        let generation = connection.source_generation.ok_or(Error::NotConnected)?;
        let mut claim = self.prepared_claim.lock().unwrap();
        let mut prepared_slot = state.prepared.lock().unwrap();
        if let Some(prepared) = prepared_slot.as_mut() {
            if prepared.generation != generation {
                return Err(Error::NotConnected);
            }
            if *claim != Some(prepared.id) {
                prepared.claims = prepared
                    .claims
                    .checked_add(1)
                    .ok_or_else(|| Error::Other("WinRT source claim capacity exhausted".into()))?;
                *claim = Some(prepared.id);
            }
            return Ok(prepared.id);
        }
        let id = connection.next_activation;
        connection.next_activation = id
            .checked_add(1)
            .ok_or_else(|| Error::Other("WinRT source activation identity exhausted".into()))?;
        let route = notification_route(&state.characteristic);
        self.shared
            .notification_faults
            .start_source_activation(generation, route, id)
            .map_err(|error| {
                Error::Other(format!("WinRT source activation failed: {error:?}").into())
            })?;
        *prepared_slot = Some(PreparedActivation {
            generation,
            id,
            claims: 1,
        });
        *claim = Some(id);
        Ok(id)
    }

    pub async fn subscribe(&self) -> Result<()> {
        self.subscribe_selecting(None).await
    }

    pub async fn subscribe_selecting(&self, notify: Option<bool>) -> Result<()> {
        let owner = self.clone();
        let peripheral = Peripheral {
            shared: self.shared.clone(),
        };
        peripheral
            .own_reclaim_write(async move { owner.subscribe_selecting_inner(notify).await })
            .await
    }

    async fn subscribe_selecting_inner(&self, notify: Option<bool>) -> Result<()> {
        let _delivery = self.shared.notification_faults.hold_notification_delivery();
        let _lifecycle = self.shared.notification_lifecycle.read().await;
        let _lane = self.physical_lane.lock().await;
        let mut state = self.state.lock().await;
        if !self.is_current() {
            return Err(Error::NotConnected);
        }
        let config = match notify {
            Some(true) => GattClientCharacteristicConfigurationDescriptorValue::Notify,
            Some(false) => GattClientCharacteristicConfigurationDescriptorValue::Indicate,
            None => utils::to_descriptor_value(state.gatt.CharacteristicProperties()?),
        };
        if config == GattClientCharacteristicConfigurationDescriptorValue::None {
            return Err(Error::NotSupported("Can not subscribe to attribute".into()));
        }
        if let Some(registration) = state.registration.as_ref() {
            match registration.recovery(config)? {
                ActivationRecovery::Uncertain => {
                    return Err(Error::Other(
                        "WinRT notification operation outcome is uncertain; registration retained"
                            .into(),
                    ));
                }
                ActivationRecovery::FinishRetirement => {
                    self.retire_registration(&mut state).await?;
                }
                ActivationRecovery::Join => {}
            }
        }
        if let Some(registration) = state.registration.as_mut() {
            let graph = self
                .shared
                .graph
                .lock()
                .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
            if graph.identity()?.as_ref() != Some(&self.graph_identity) {
                return Err(Error::Other("WinRT notification graph is stale".into()));
            }
            return registration.join(&mut self.claim.lock().unwrap(), config);
        }
        // A retained owner from an older graph must not replace the current
        // route's enable epoch before discovering that its graph is stale.
        if self
            .shared
            .graph
            .lock()
            .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?
            .identity()?
            .as_ref()
            != Some(&self.graph_identity)
        {
            return Err(Error::Other("WinRT notification graph is stale".into()));
        }
        let characteristic = state.characteristic.clone();
        let epoch = self.shared.notification_faults.begin(&characteristic)?;
        let sender = self.shared.notifications_channel.clone();
        let faults = self.shared.notification_faults.clone();
        let source = state.prepared.clone();
        let live = Arc::new(AtomicBool::new(true));
        let callbacks = Arc::new(super::callback_retirement::CallbackRetirement::new());
        let callback_callbacks = callbacks.clone();
        let callback_shared = Arc::downgrade(&self.shared);
        let handler: NotifyEventHandler = Arc::new(move |value| {
            let Some(shared) = callback_shared.upgrade() else {
                return;
            };
            let (value, source_failure) = match value {
                Ok(bytes) => (bytes, None),
                Err(error) => (Vec::new(), Some(error)),
            };
            let notification = ValueNotification {
                uuid: characteristic.uuid,
                instance: characteristic.instance,
                service_uuid: characteristic.service_uuid,
                service_instance: characteristic.service_instance,
                value,
                source_failure,
                lost_before: 0,
            };
            let bytes = notification_retained_bytes(&notification);
            let invalidates = notification.source_failure.is_some();
            let faults = faults.clone();
            let sender = sender.clone();
            let source = source.clone();
            let route = notification_route(&characteristic);
            let source_value = notification.clone();
            super::callback_retirement::admit_managed(
                &shared.managed_callbacks,
                &callback_callbacks,
                |callback| {
                    shared.admit_managed_source_callback(
                        &source,
                        route,
                        &source_value,
                        bytes,
                        invalidates,
                        callback,
                    );
                },
                move |tickets| {
                    let delivery = faults.hold_notification_delivery();
                    faults.publish_legacy_for_epoch(&sender, notification, epoch);
                    drop(tickets);
                    drop(delivery);
                },
            );
        });
        let token_result = {
            let graph = self
                .shared
                .graph
                .lock()
                .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
            if graph.identity().ok().flatten().as_ref() != Some(&self.graph_identity) {
                self.shared
                    .notification_faults
                    .clear_for_epoch(&state.characteristic, epoch);
                return Err(Error::Other("WinRT notification graph is stale".into()));
            }
            state.gatt.ValueChanged(&value_changed(handler))
        };
        let token = match token_result {
            Ok(token) => token,
            Err(error) => {
                self.shared.retire_managed_callbacks(&callbacks).await?;
                self.shared
                    .notification_faults
                    .clear_for_epoch(&state.characteristic, epoch);
                return Err(error.into());
            }
        };
        state.registration = Some(ActivationRegistration {
            token: Some(token),
            epoch,
            config,
            claims: 1,
            phase: ActivationPhase::Enabling,
            live,
            callbacks,
        });
        *self.claim.lock().unwrap() = Some(epoch);
        let started = {
            let graph = self
                .shared
                .graph
                .lock()
                .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
            if graph.identity().ok().flatten().as_ref() != Some(&self.graph_identity) {
                Err(super::ble::characteristic::CccdWriteFailure {
                    error: Error::Other("WinRT notification graph is stale".into()),
                    proven_no_effect: true,
                })
            } else {
                BLECharacteristic::start_client_configuration(&state.gatt, config)
            }
        };
        let outcome = match started {
            Ok(operation) => {
                BLECharacteristic::finish_client_configuration(operation, "subscribe").await
            }
            Err(failure) => Err(failure),
        };
        if let Err(failure) = outcome {
            if !failure.proven_no_effect {
                if let Some(registration) = state.registration.as_mut() {
                    registration.phase = ActivationPhase::Indeterminate;
                }
                return Err(failure.error);
            }
            let error = failure.error;
            if let Some(registration) = state.registration.as_ref() {
                self.shared
                    .retire_managed_callbacks(&registration.callbacks)
                    .await?;
                registration.live.store(false, Ordering::Release);
            }
            if let Err(cleanup) = self.shared.remove_value_changed(&state.gatt, token).await {
                if let Some(registration) = state.registration.as_mut() {
                    registration.phase = ActivationPhase::Failed;
                }
                return Err(Error::WithCleanup {
                    primary: Box::new(error),
                    cleanup: Box::new(cleanup),
                });
            }
            state.registration = None;
            *self.claim.lock().unwrap() = None;
            self.shared
                .notification_faults
                .clear_for_epoch(&state.characteristic, epoch);
            return Err(error);
        }
        if let Some(registration) = state.registration.as_mut() {
            registration.phase = ActivationPhase::Active;
        }
        Ok(())
    }

    async fn retire_registration(&self, state: &mut ActivationState) -> Result<()> {
        let Some(registration) = state.registration.as_ref() else {
            return Ok(());
        };
        let epoch = registration.epoch;
        let token = registration.token;
        let live = registration.live.clone();
        let callbacks = registration.callbacks.clone();
        // Capture the operation's source identity before any native await.
        let prepared = *state.prepared.lock().unwrap();
        // A retired graph's exact local handler remains ours to remove, but
        // another graph may already own the same ATT handle and its CCCD.
        let graph_current = self
            .shared
            .graph
            .lock()
            .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?
            .identity()
            .ok()
            .flatten()
            .as_ref()
            == Some(&self.graph_identity);
        let disconnected = self.identity.connection.load(Ordering::Relaxed);
        let action = retirement_action(registration.phase, disconnected, graph_current);
        if action == RetirementAction::Uncertain {
            return Err(Error::Other(
                "WinRT notification operation outcome is uncertain; registration retained".into(),
            ));
        }
        // Own both recipient slots and a graph node before either native
        // retirement step can fail. The owned operation task outlives caller
        // cancellation, and the cut follows it through both steps.
        let failure_cut = if action == RetirementAction::WriteCccd {
            if let Some(prepared) = prepared {
                match self
                    .shared
                    .reserve_notification_failure_cut(prepared.generation)
                {
                    Ok(cut) => Some(cut),
                    // A prior failure or replacement already fenced this generation.
                    Err(SourceGroupError::StaleGeneration) => None,
                    Err(error) => {
                        return Err(Error::Other(
                            format!("WinRT notification failure owner admission failed: {error:?}")
                                .into(),
                        ));
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        let mut failure_cut = ReservedFailureCut::new(self.shared.clone(), failure_cut);
        self.shared.retire_managed_callbacks(&callbacks).await?;
        if action == RetirementAction::GraphUncertain {
            {
                let _source = state.prepared.lock().unwrap();
                live.store(false, Ordering::Release);
            }
            if let Some(token) = token {
                self.shared.remove_value_changed(&state.gatt, token).await?;
                if let Some(registration) = state.registration.as_mut() {
                    registration.token = None;
                }
            }
            return Err(Error::Other(
                "WinRT retired graph CCCD cleanup is unconfirmed until disconnect".into(),
            ));
        }
        if action == RetirementAction::LocalOnly {
            {
                let _source = state.prepared.lock().unwrap();
                live.store(false, Ordering::Release);
            }
            if let Some(token) = token {
                self.shared.remove_value_changed(&state.gatt, token).await?;
            }
            self.shared
                .notification_faults
                .clear_for_epoch(&state.characteristic, epoch);
            state.registration = None;
            return Ok(());
        }
        let retired = registration.phase == ActivationPhase::FailedRetirement;
        {
            let _source = state.prepared.lock().unwrap();
            live.store(false, Ordering::Release);
        }
        if let Some(token) = token {
            if let Err(error) = self.shared.remove_value_changed(&state.gatt, token).await {
                if let Some(registration) = state.registration.as_mut() {
                    registration.phase = ActivationPhase::FailedRetirement;
                }
                if let Some(cut) = failure_cut.take() {
                    self.shared.queue_preowned_notification_failure(
                        cut,
                        notification_retirement_error(&error),
                    );
                } else if prepared.is_none() {
                    warn!(
                        "WinRT notification handler removal failed without a prepared source identity: {error}"
                    );
                }
                return Err(error);
            }
            if let Some(registration) = state.registration.as_mut() {
                registration.token = None;
            }
        }
        let current = retired
            || self
                .shared
                .notification_faults
                .clear_for_epoch(&state.characteristic, epoch);
        if current {
            if !self.is_current() {
                return Err(Error::Other(
                    "WinRT notification cleanup awaits confirmed disconnect".into(),
                ));
            }
            if let Some(registration) = state.registration.as_mut() {
                registration.phase = ActivationPhase::Retiring;
            }
            let started = {
                let graph = self
                    .shared
                    .graph
                    .lock()
                    .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
                start_on_current_graph(&graph, &self.graph_identity, || {
                    BLECharacteristic::start_client_configuration(
                        &state.gatt,
                        GattClientCharacteristicConfigurationDescriptorValue::None,
                    )
                })?
            };
            let outcome = match started {
                Ok(operation) => {
                    BLECharacteristic::finish_client_configuration(operation, "unsubscribe").await
                }
                Err(failure) => Err(failure),
            };
            if let Err(failure) = outcome {
                if let Some(registration) = state.registration.as_mut() {
                    registration.phase = if failure.proven_no_effect {
                        ActivationPhase::FailedRetirement
                    } else {
                        ActivationPhase::Indeterminate
                    };
                }
                if let Some(cut) = failure_cut.take() {
                    self.shared.queue_preowned_notification_failure(
                        cut,
                        notification_retirement_error(&failure.error),
                    );
                } else if prepared.is_none() {
                    warn!(
                        "WinRT notification disable failed without a prepared source identity: {}",
                        failure.error
                    );
                }
                return Err(failure.error);
            }
        }
        state.registration = None;
        Ok(())
    }

    fn release_prepared(&self, state: &mut ActivationState) -> Result<()> {
        let mut claim = self.prepared_claim.lock().unwrap();
        let mut slot = state.prepared.lock().unwrap();
        let Some(prepared) = slot.as_mut() else {
            *claim = None;
            return Ok(());
        };
        if *claim != Some(prepared.id) {
            return Ok(());
        }
        if prepared.claims > 1 {
            prepared.claims -= 1;
        } else {
            match self.shared.notification_faults.retire_source_activation(
                prepared.generation,
                notification_route(&state.characteristic),
                prepared.id,
            ) {
                Ok(())
                | Err(SourceGroupError::StaleGeneration)
                | Err(SourceGroupError::Ingress(
                    crate::ubm_peer_notification_ingress::IngressError::NotCurrent,
                )) => {}
                Err(error) => {
                    return Err(Error::Other(
                        format!("WinRT source retirement failed: {error:?}").into(),
                    ));
                }
            }
            *slot = None;
        }
        *claim = None;
        Ok(())
    }

    pub async fn unsubscribe(&self) -> Result<()> {
        let owner = self.clone();
        let peripheral = Peripheral {
            shared: self.shared.clone(),
        };
        peripheral
            .own_reclaim_write(async move { owner.unsubscribe_inner().await })
            .await
    }

    async fn unsubscribe_inner(&self) -> Result<()> {
        let _delivery = self.shared.notification_faults.hold_notification_delivery();
        let _lifecycle = self.shared.notification_lifecycle.read().await;
        let _lane = self.physical_lane.lock().await;
        let mut state = self.state.lock().await;
        if let Some(registration) = state.registration.as_mut() {
            if registration.release_shared(&mut self.claim.lock().unwrap()) {
                self.retire_registration(&mut state).await?;
                *self.claim.lock().unwrap() = None;
            }
        }
        self.release_prepared(&mut state)
    }
}

impl Shared {
    fn pending_failure_spill_error(&self) -> Result<()> {
        let spill_pending = self
            .pending_failure_spill
            .lock()
            .map_err(|_| Error::Other("WinRT retained failure spill lock poisoned".into()))?
            .is_some();
        if spill_pending || self.pending_durable_failure.load(Ordering::Acquire) {
            return Err(Error::Other(
                "WinRT notification teardown is waiting for exact retained failure recovery; retry after every recipient durable acknowledgement completes".into(),
            ));
        }
        Ok(())
    }

    fn apply_services_changed(&self, owner: &Arc<AtomicBool>, identity: PeripheralId) {
        // Do not let a service-change cleanup destroy the only owner of a
        // backpressured or durably staged native failure. A file spill is not
        // enough: the original source-time recipient cut stays authoritative
        // until the final recovery acknowledgement retires it.
        let spill_pending = self
            .pending_failure_spill
            .lock()
            .map_or(true, |pending| pending.is_some());
        if spill_pending || self.pending_durable_failure.load(Ordering::Acquire) {
            if let Ok(mut deferred) = self.deferred_services_changed.lock() {
                match deferred.as_ref() {
                    Some(existing) if Arc::ptr_eq(&existing.owner, owner) => {}
                    _ => {
                        *deferred = Some(DeferredServicesChanged {
                            owner: owner.clone(),
                            identity: identity.clone(),
                        });
                    }
                }
            }
            let graph = self
                .graph
                .lock()
                .expect("WinRT graph owner lock poisoned")
                .identity()
                .ok()
                .flatten();
            self.emit(CentralEvent::DeviceSourceUncertain {
                id: identity,
                graph,
                platform: None,
            });
            return;
        }
        let _wake_scope = self.notification_faults.defer_wakes();
        let _transition = self.callback_transition.lock().unwrap();
        let source = self.source_connection.lock().unwrap();
        if !source
            .owner
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, owner))
        {
            return;
        }
        drop(source);
        let mut source = self.source_connection.lock().unwrap();
        self.graph.lock().unwrap().ready = false;
        source.source_generation = None;
        self.notification_faults.clear_all();
        drop(source);
        self.fence_graph_callbacks();
        let source = self.source_connection.lock().unwrap();
        if !source
            .owner
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, owner))
        {
            return;
        }
        self.graph.lock().unwrap().invalidate();
        drop(source);
        drop(_transition);
        self.emit(CentralEvent::DeviceServicesModified(identity));
    }

    fn resume_deferred_services_changed(self: &Arc<Self>) {
        let deferred = self
            .deferred_services_changed
            .lock()
            .expect("WinRT deferred service-change lock poisoned")
            .take();
        if let Some(deferred) = deferred {
            self.apply_services_changed(&deferred.owner, deferred.identity);
        }
    }

    fn fence_graph_callbacks(&self) {
        // Snapshot registrations before waiting: DashMap guards are graph
        // ownership locks too, and admitted callbacks must be able to finish.
        let gates: Vec<_> = self
            .ble_services
            .iter()
            .flat_map(|service| {
                service
                    .characteristics
                    .values()
                    .filter_map(|characteristic| characteristic.callback_gate())
                    .collect::<Vec<_>>()
            })
            .collect();
        self.managed_callbacks.close();
        for gate in gates {
            gate.set(false);
        }
    }

    // A managed activation owns its exact characteristic independently of the
    // service table. Never close a service while such a claim is outstanding.
    async fn ensure_managed_activations_retired(&self) -> Result<()> {
        let states: Vec<_> = self
            .source_connection
            .lock()
            .map_err(|_| Error::Other("WinRT connection owner lock poisoned".into()))?
            .activations
            .values()
            .cloned()
            .collect();
        for state in states {
            let state = state.lock().await;
            if state.registration.is_some() || state.prepared.lock().unwrap().is_some() {
                return Err(Error::Other("WinRT managed notification activation requires cleanup before service replacement".into()));
            }
        }
        Ok(())
    }

    // Disconnect owns the exclusive lifecycle lease. Stop callback admission
    // and remove each exact local handler before any service/device Close.
    // A failed removal leaves both its token and service owner retryable.
    async fn remove_managed_handlers_for_close(&self) -> Result<()> {
        let states: Vec<_> = self
            .source_connection
            .lock()
            .map_err(|_| Error::Other("WinRT connection owner lock poisoned".into()))?
            .activations
            .values()
            .cloned()
            .collect();
        for state in states {
            let mut state = state.lock().await;
            let Some(registration) = state.registration.as_ref() else {
                continue;
            };
            {
                let _source = state.prepared.lock().unwrap();
                registration.live.store(false, Ordering::Release);
            }
            if let Some(token) = registration.token {
                self.remove_value_changed(&state.gatt, token).await?;
                if let Some(registration) = state.registration.as_mut() {
                    registration.token = None;
                }
            }
        }
        Ok(())
    }

    async fn remove_value_changed(&self, gatt: &GattCharacteristic, token: i64) -> Result<()> {
        let gatt = gatt.clone();
        self.run_native_cleanup(move || gatt.RemoveValueChanged(token).map_err(Error::from))
            .await
    }

    async fn run_native_cleanup<F, T>(&self, work: F) -> Result<T>
    where
        F: FnOnce() -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        match super::marker_executor::run_blocking_owned(work).await {
            Ok(result) => result,
            Err(error) => {
                if error == "WinRT cleanup thread exited without a result" {
                    self.graph
                        .lock()
                        .expect("WinRT graph owner lock poisoned")
                        .native_operation_abandoned = true;
                }
                Err(Error::Other(error.into()))
            }
        }
    }

    async fn close_owned_device(
        self: &Arc<Self>,
        mut device: tokio::sync::OwnedMutexGuard<Option<BLEDevice>>,
    ) -> Result<tokio::sync::OwnedMutexGuard<Option<BLEDevice>>> {
        self.remove_managed_handlers_for_close().await?;
        let mut partial =
            self.partial_device.clone().try_lock_owned().map_err(|_| {
                Error::Other("WinRT partial device cleanup is pending; retry".into())
            })?;
        let shared = self.clone();
        let result = super::marker_executor::run_blocking_owned(move || {
            let result = (|| -> Result<()> {
                for mut service in shared.ble_services.iter_mut() {
                    for characteristic in service.characteristics.values_mut() {
                        characteristic.take_registration()?;
                    }
                }
                for service in shared
                    .retired_services
                    .lock()
                    .map_err(|_| Error::Other("WinRT retired service lock poisoned".into()))?
                    .iter_mut()
                {
                    for characteristic in service.characteristics.values_mut() {
                        characteristic.take_registration()?;
                    }
                }
                Ok(())
            })()
            .and_then(|()| match partial.as_mut() {
                Some(owner) => owner.close(),
                None => Ok(()),
            })
            .and_then(|()| match device.as_mut() {
                Some(device) => device.close(),
                None => Ok(()),
            });
            if result.is_ok() {
                shared.ble_services.clear();
                shared
                    .retired_services
                    .lock()
                    .expect("WinRT retired service lock poisoned")
                    .clear();
                *device = None;
                *partial = None;
                shared
                    .ordinary_cccd_uncertain
                    .lock()
                    .expect("WinRT CCCD owner lock poisoned")
                    .clear();
                shared
                    .ordinary_cccd_local_cleanup
                    .lock()
                    .expect("WinRT CCCD cleanup lock poisoned")
                    .clear();
            }
            (device, result)
        })
        .await;
        match result {
            Ok((device, result)) => {
                result?;
                self.source_connection
                    .lock()
                    .map_err(|_| Error::Other("WinRT connection owner lock poisoned".into()))?
                    .activations
                    .clear();
                Ok(device)
            }
            Err(error) => {
                if error == "WinRT cleanup thread exited without a result" {
                    self.graph
                        .lock()
                        .expect("WinRT graph owner lock poisoned")
                        .native_operation_abandoned = true;
                }
                Err(Error::Other(error.into()))
            }
        }
    }

    fn ensure_native_owner_replaceable(&self) -> Result<()> {
        if self
            .graph
            .lock()
            .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?
            .native_operation_abandoned
        {
            return Err(Error::Other(
                "WinRT native owner teardown is fenced after an abandoned operation".into(),
            ));
        }
        Ok(())
    }
    fn add_adapter(&self, adapter: Weak<AdapterManager<Peripheral>>) {
        let mut adapters = self
            .adapters
            .lock()
            .expect("WinRT adapter owners lock poisoned");
        adapters.retain(|entry| entry.strong_count() != 0);
        if adapter.strong_count() != 0 && !adapters.iter().any(|entry| entry.ptr_eq(&adapter)) {
            adapters.push(adapter);
        }
    }

    fn emit(&self, event: CentralEvent) {
        let adapters: Vec<_> = self
            .adapters
            .lock()
            .expect("WinRT adapter owners lock poisoned")
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        if adapters.is_empty() {
            trace!("Could not emit an event. AdapterManager has been dropped");
        }
        for adapter in adapters {
            adapter.emit(event.clone());
        }
    }

    fn must_retain(&self) -> bool {
        let graph = self.graph.lock().expect("WinRT graph owner lock poisoned");
        if graph.reclaim_operations != 0
            || graph.native_operation_abandoned
            || graph.reclaim_effect_uncertain
            || graph.reclaim_callback_uncertain
            || graph.reclaim_gate.is_busy()
        {
            return true;
        }
        drop(graph);
        !self
            .ordinary_cccd_uncertain
            .lock()
            .expect("WinRT CCCD owner lock poisoned")
            .is_empty()
            || !self
                .retired_services
                .lock()
                .expect("WinRT retired service lock poisoned")
                .is_empty()
            || self
                .partial_device
                .try_lock()
                .map_or(true, |device| device.is_some())
            || self
                .device
                .try_lock()
                .map_or(true, |device| device.is_some())
    }
    fn emit_retired_graph(
        &self,
        previous: Option<(String, u64, u64)>,
        current: Option<(String, u64, u64)>,
    ) {
        if let Some(graph) = previous.filter(|graph| Some(graph) != current.as_ref()) {
            self.emit(CentralEvent::DeviceGraphRetired {
                id: self.identity.clone(),
                graph,
            });
        }
    }

    fn drain_reclaim_callbacks(self: &Arc<Self>) {
        use crate::ubm_reclaim_graph_gate::DrainItem;
        loop {
            let (item, drain_graph, overflow_epoch) = {
                let mut graph = self.graph.lock().expect("WinRT graph owner lock poisoned");
                let item = graph.reclaim_gate.next();
                let drain_graph = graph.reclaim_drain_graph.clone();
                if matches!(item, Ok(DrainItem::Finished)) {
                    graph.reclaim_drain_graph = None;
                }
                (item, drain_graph, graph.reclaim_overflow_epoch)
            };
            let unexpected_barrier = matches!(
                &item,
                Ok(DrainItem::Barrier { .. } | DrainItem::BarrierOverflow) | Err(_)
            );
            if unexpected_barrier {
                warn!("WinRT reclaim drain encountered an unexpected barrier or gate error");
                let mut graph = self.graph.lock().expect("WinRT graph owner lock poisoned");
                graph.reclaim_effect_uncertain = true;
                graph.note_callback_overflow();
            }
            match item {
                Ok(DrainItem::Event(callback)) => callback(),
                Ok(
                    DrainItem::Overflow | DrainItem::Barrier { .. } | DrainItem::BarrierOverflow,
                )
                | Err(_) => {
                    let _wake_scope = self.notification_faults.defer_wakes();
                    let generation = {
                        let mut source = self
                            .source_connection
                            .lock()
                            .expect("WinRT source owner lock poisoned");
                        let generation = source.source_generation.take();
                        let mut graph = self.graph.lock().expect("WinRT graph owner lock poisoned");
                        graph.invalidate();
                        generation
                    };
                    let source_fenced = match generation {
                        Some(generation) => match self.notification_faults.fail_source_generation(
                            generation,
                            crate::PlatformError::new(
                                "winrt",
                                "reclaim-callback-overflow",
                                "WinRT reclaim callback queue overflow",
                            ),
                        ) {
                            Ok(()) => true,
                            Err(error) => {
                                warn!("WinRT reclaim overflow fencing failed: {error:?}");
                                false
                            }
                        },
                        None => true,
                    };
                    self.graph
                        .lock()
                        .expect("WinRT graph owner lock poisoned")
                        .finish_callback_fence(
                            overflow_epoch,
                            source_fenced && !unexpected_barrier,
                        );
                    drop(_wake_scope);
                    self.emit(CentralEvent::DeviceSourceUncertain {
                        id: self.identity.clone(),
                        graph: drain_graph,
                        platform: None,
                    });
                    if unexpected_barrier {
                        self.reclaim_drain_changed.notify_waiters();
                        break;
                    }
                }
                Ok(DrainItem::Finished) => {
                    self.reclaim_drain_changed.notify_waiters();
                    break;
                }
            }
        }
    }

    fn reserve_notification_failure_cut(
        self: &Arc<Self>,
        generation: u64,
    ) -> std::result::Result<crate::common::util::FutureFailureCut, SourceGroupError> {
        // This flag spans the unavoidable native callback allocation through
        // final durable recovery retirement. It is intentionally broader than
        // a spill-capacity wait: service-change cleanup must not clear a
        // source whose staged recipients still own exact evidence.
        if self.pending_durable_failure.swap(true, Ordering::AcqRel) {
            return Err(SourceGroupError::StaleGeneration);
        }
        let delivery_shared = Arc::downgrade(self);
        let completion_shared = Arc::downgrade(self);
        self.notification_faults
            .reserve_future_failure_cut_with_finalizer(
                generation,
                move |captured| {
                    if let Some(shared) = delivery_shared.upgrade() {
                        let _delivery = shared.notification_faults.hold_notification_delivery();
                        // Keep the native source generation fenced in the
                        // captured owner. `defer_release` hands it to exact
                        // recovery recipients instead of clearing it here.
                        captured.defer_release();
                    } else {
                        captured.release();
                    }
                },
                move || {
                    if let Some(shared) = completion_shared.upgrade() {
                        shared
                            .pending_durable_failure
                            .store(false, Ordering::Release);
                        shared.resume_deferred_services_changed();
                    }
                },
            )
            .inspect_err(|_| self.pending_durable_failure.store(false, Ordering::Release))
    }

    /// Cancel a pre-admitted cleanup failure cut when its native cleanup
    /// succeeds or exits before producing a failure.  The cut's drop releases
    /// its source slots; this method releases the paired WinRT fence and wakes
    /// service-change work.  A published failure transfers the cut out of the
    /// guard and never reaches this path.
    fn abandon_notification_failure_cut(
        self: &Arc<Self>,
        cut: crate::common::util::FutureFailureCut,
    ) {
        drop(cut);
        let spill_pending = self
            .pending_failure_spill
            .lock()
            .map_or(true, |pending| pending.is_some());
        if !spill_pending && self.pending_durable_failure.swap(false, Ordering::AcqRel) {
            self.resume_deferred_services_changed();
        }
    }

    fn retry_pending_notification_failure(self: &Arc<Self>) {
        let pending = self
            .pending_failure_spill
            .lock()
            .expect("WinRT retained failure spill lock poisoned")
            .take();
        if let Some(pending) = pending {
            self.queue_preowned_notification_failure(pending.cut, (*pending.error).clone());
        }
    }

    fn schedule_failure_spill_retry(self: &Arc<Self>) {
        let Some(store) = self.notification_faults.failure_evidence_store() else {
            return;
        };
        let capacity_released = store.capacity_released();
        let shared = Arc::downgrade(self);
        tokio::spawn(async move {
            // Store capacity normally changes on the exact recovery ACK. A
            // bounded delay also retries transient I/O errors without making
            // the native callback disappear; the pending owner remains the
            // generation fence until this succeeds or teardown takes it.
            tokio::select! {
                () = capacity_released.notified() => {},
                () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
            }
            if let Some(shared) = shared.upgrade() {
                shared.retry_pending_notification_failure();
            }
        });
    }

    fn queue_preowned_notification_failure(
        self: &Arc<Self>,
        cut: crate::common::util::FutureFailureCut,
        error: crate::PlatformError,
    ) {
        // The cut bounds owner and recipient counts. Large errors have an
        // exact paged recovery path once admitted; they are not rejected just
        // because their message or metadata exceeds an ordinary callback
        // envelope. The retained representation itself still needs a bounded
        // durable-owner policy before this can be called a bounded-evidence
        // lane.
        let _wakes = self.notification_faults.defer_wakes();
        let mut graph = self.graph.lock().expect("WinRT graph owner lock poisoned");
        let was_idle = !graph.reclaim_gate.is_busy();
        if was_idle {
            graph.reclaim_drain_graph = graph.identity().ok().flatten();
        }
        let start_drain = match cut.capture_and_enqueue(&mut graph.reclaim_gate, error) {
            Ok(start) => start,
            Err(pending) => {
                warn!(
                    "WinRT pre-owned notification failure is waiting for durable evidence ({:?}): {}",
                    pending.reason, pending.error
                );
                let previous = self
                    .pending_failure_spill
                    .lock()
                    .expect("WinRT retained failure spill lock poisoned")
                    .replace(pending);
                debug_assert!(
                    previous.is_none(),
                    "one source generation cannot retain two spill failures"
                );
                false
            }
        };
        if was_idle && !start_drain {
            graph.reclaim_drain_graph = None;
        }
        drop(graph);
        if start_drain {
            self.drain_reclaim_callbacks();
        }
        if !start_drain {
            self.schedule_failure_spill_retry();
        }
    }

    // The Windows-only fixtures enter the same pre-owned production cut.
    #[cfg(test)]
    fn queue_notification_source_failure(
        self: &Arc<Self>,
        generation: u64,
        error: crate::PlatformError,
    ) {
        let cut = match self.reserve_notification_failure_cut(generation) {
            Ok(cut) => cut,
            Err(SourceGroupError::StaleGeneration) => return,
            Err(error) => panic!("failure fixture admission failed: {error:?}"),
        };
        self.queue_preowned_notification_failure(cut, error);
    }

    // A refused callback drops its ticket before the peer-wide overflow cut.
    // Wait for that cut instead of closing the source from an unrelated route.
    async fn retire_managed_callbacks(
        &self,
        callbacks: &super::callback_retirement::CallbackRetirement,
    ) -> Result<()> {
        callbacks.close();
        callbacks.wait().await;
        self.wait_pending_reclaim_overflow().await
    }

    async fn wait_pending_reclaim_overflow(&self) -> Result<()> {
        super::callback_retirement::wait_for_peer_fence(&self.reclaim_drain_changed, || {
            let graph = self.graph.lock().expect("WinRT graph owner lock poisoned");
            if !graph.reclaim_callback_uncertain || graph.reclaim_callback_recoverable {
                return Ok(true);
            }
            if graph.reclaim_effect_uncertain || !graph.reclaim_gate.is_busy() {
                return Err(Error::Other(
                    "WinRT reclaim overflow fence is unconfirmed".into(),
                ));
            }
            Ok(false)
        })
        .await
    }

    fn admit_managed_source_callback(
        self: &Arc<Self>,
        source: &Mutex<Option<PreparedActivation>>,
        route: NotificationRoute,
        value: &ValueNotification,
        bytes: usize,
        invalidates: bool,
        callback: Box<dyn FnOnce() + Send>,
    ) -> bool {
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::Admission;
        let _wakes = self.notification_faults.defer_wakes();
        let synthetic = GraphKey {
            authority_id: 0,
            peer_id: String::new(),
            connection_generation: String::new(),
            graph: (String::new(), 0, 0),
            permit_id: 0,
        };
        let bytes = bytes
            .saturating_add(std::mem::size_of::<Box<dyn FnOnce() + Send>>())
            .saturating_add(std::mem::size_of::<
                Option<crate::common::util::SourceValueReservation>,
            >())
            .saturating_add(std::mem::size_of::<Arc<NotificationFaults>>());
        let mut graph = self.graph.lock().expect("WinRT graph owner lock poisoned");
        let owner = !graph.reclaim_gate.is_busy();
        if owner {
            graph
                .reclaim_gate
                .acquire(synthetic.clone())
                .expect("idle callback gate");
            graph.reclaim_drain_graph = graph.identity().ok().flatten();
            graph
                .reclaim_gate
                .commit(&synthetic)
                .expect("new callback gate");
        }
        let mut reservation = None;
        // A decoding failure still uses ordinary callback admission. If the
        // gate or recipient budget is full, fail_tail reports overflow rather
        // than retaining that callback's native error. Retirement's preowned
        // mandatory cut does not cover this path.
        let mut failed = !graph.reclaim_gate.can_reserve_before_admit(bytes);
        if !failed {
            if let Some(prepared) = *source.lock().unwrap() {
                let budget = graph.reclaim_gate.remaining_bytes().saturating_sub(bytes);
                match self
                    .notification_faults
                    .reserve_source_value_for_activation_bounded(
                        prepared.generation,
                        route,
                        prepared.id,
                        value,
                        budget,
                    ) {
                    Ok(held) => reservation = Some(held),
                    Err(
                        SourceGroupError::StaleGeneration
                        | SourceGroupError::Ingress(
                            crate::ubm_peer_notification_ingress::IngressError::NotCurrent,
                        ),
                    ) => {}
                    Err(error) => {
                        warn!("WinRT notification reservation failed: {error:?}");
                        failed = true;
                    }
                }
            }
        }
        let exhausted = reservation
            .as_ref()
            .is_some_and(|held| held.ordinal_exhausted());
        let bytes =
            bytes.saturating_add(reservation.as_ref().map_or(0, |held| held.retained_bytes()));
        let mut rejected = None;
        let accepted = if failed {
            graph.reclaim_gate.fail_tail();
            graph.note_callback_overflow();
            rejected = Some(callback);
            false
        } else {
            let faults = self.notification_faults.clone();
            let replay = Box::new(move || {
                let delivery = faults.hold_notification_delivery();
                if let Some(held) = reservation {
                    held.release();
                }
                callback();
                drop(delivery);
            });
            match graph.reclaim_gate.admit(replay, bytes, invalidates) {
                Admission::Deferred => {
                    if exhausted {
                        graph.reclaim_gate.fail_tail();
                        graph.note_callback_overflow();
                    }
                    true
                }
                Admission::Overflow(callback) => {
                    graph.note_callback_overflow();
                    rejected = Some(callback);
                    false
                }
                Admission::Apply(_) => unreachable!("callback gate is held"),
            }
        };
        if owner {
            graph
                .reclaim_gate
                .release(&synthetic)
                .expect("callback gate owner");
        }
        drop(graph);
        drop(rejected);
        if owner {
            self.drain_reclaim_callbacks();
        }
        accepted
    }

    fn admit_reclaim_callback(
        self: &Arc<Self>,
        bytes: usize,
        invalidates: bool,
        callback: impl FnOnce() + Send + 'static,
    ) -> bool {
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::Admission;
        let synthetic = GraphKey {
            authority_id: 0,
            peer_id: String::new(),
            connection_generation: String::new(),
            graph: (String::new(), 0, 0),
            permit_id: 0,
        };
        let bytes = bytes.saturating_add(std::mem::size_of_val(&callback));
        let mut graph = self.graph.lock().expect("WinRT graph owner lock poisoned");
        let owner = !graph.reclaim_gate.is_busy();
        if owner {
            graph
                .reclaim_gate
                .acquire(synthetic.clone())
                .expect("idle callback gate");
            graph.reclaim_drain_graph = graph.identity().ok().flatten();
            graph
                .reclaim_gate
                .commit(&synthetic)
                .expect("new callback gate");
        }
        let (accepted, rejected) =
            match graph
                .reclaim_gate
                .admit(Box::new(callback), bytes, invalidates)
            {
                Admission::Deferred => (true, None),
                Admission::Overflow(callback) => {
                    graph.note_callback_overflow();
                    (false, Some(callback))
                }
                Admission::Apply(_) => unreachable!("callback gate is held"),
            };
        if owner {
            graph
                .reclaim_gate
                .release(&synthetic)
                .expect("callback gate owner");
        }
        drop(graph);
        // Rejected callbacks own admission tickets: dropping one may wake a
        // lifecycle waiter, which must never run under the graph owner lock.
        drop(rejected);
        if owner {
            self.drain_reclaim_callbacks();
        }
        accepted
    }
}

impl Peripheral {
    fn begin_graph_replacement(
        &self,
        revision: u64,
        previous: Option<(String, u64, u64)>,
    ) -> Result<()> {
        let _delivery = self.shared.notification_faults.hold_notification_delivery();
        let _transition = self.shared.callback_transition.lock().unwrap();
        {
            let mut graph = self
                .shared
                .graph
                .lock()
                .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
            if graph.exhausted
                || graph.revision != revision
                || !self.shared.connected.load(Ordering::Relaxed)
            {
                return Err(Error::Other(
                    "WinRT service graph changed during discovery".into(),
                ));
            }
            graph.ready = false;
        }
        // WinRT provides no cut proving continuity across characteristic objects.
        // Terminate first with an uncertain tail, then suppress old registrations.
        super::callback_gate::terminate_handoff(
            || {
                let mut source = self.shared.source_connection.lock().unwrap();
                source.source_generation = None;
                self.shared.notification_faults.clear_all();
            },
            || self.shared.fence_graph_callbacks(),
        );
        {
            let mut graph = self
                .shared
                .graph
                .lock()
                .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
            if graph.exhausted
                || graph.revision != revision
                || !self.shared.connected.load(Ordering::Relaxed)
            {
                return Err(Error::Other(
                    "WinRT service graph changed during discovery".into(),
                ));
            }
            graph.ready = false;
        }
        drop(_transition);
        self.shared.emit_retired_graph(previous, None);
        Ok(())
    }

    fn finish_graph_replacement(&self, revision: u64) -> Result<()> {
        let _transition = self.shared.callback_transition.lock().unwrap();
        let mut source = self
            .shared
            .source_connection
            .lock()
            .map_err(|_| Error::Other("WinRT connection owner lock poisoned".into()))?;
        let mut graph = self
            .shared
            .graph
            .lock()
            .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
        if graph.exhausted
            || graph.revision != revision
            || !self.shared.connected.load(Ordering::Relaxed)
        {
            return Err(Error::Other(
                "WinRT service graph changed during discovery".into(),
            ));
        }
        let Some(next) = graph.revision.checked_add(1) else {
            graph.exhausted = true;
            graph.ready = false;
            return Err(Error::Other("WinRT graph revision exhausted".into()));
        };
        graph.revision = next;
        graph.ready = true;
        if let Some(attachment) = graph.attachment {
            graph.publish_discovery(attachment, next);
        }
        drop(graph);
        // The pre-replacement check proved old states have no registrations
        // or prepared claims. Do not discard a current-graph owner.
        source
            .activations
            .retain(|(revision, _), _| *revision == next);
        drop(source);
        self.shared.managed_callbacks.open();
        let gates: Vec<_> = self
            .shared
            .ble_services
            .iter()
            .flat_map(|service| {
                service
                    .characteristics
                    .values()
                    .filter_map(|characteristic| characteristic.callback_gate())
                    .collect::<Vec<_>>()
            })
            .collect();
        for gate in gates {
            gate.set(true);
        }
        Ok(())
    }

    fn note_remote_cccd_uncertainty(&self, key: (Uuid, u64, Uuid, u64)) {
        self.shared
            .ordinary_cccd_local_cleanup
            .lock()
            .unwrap()
            .remove(&key);
        self.shared
            .ordinary_cccd_uncertain
            .lock()
            .unwrap()
            .insert(key);
    }

    fn operation_graph(&self) -> Result<(String, u64, u64)> {
        self.shared
            .graph
            .lock()
            .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?
            .identity()?
            .ok_or_else(|| Error::Other("WinRT service graph is not ready".into()))
    }

    pub fn acquire_reclaim(
        &self,
        binding: &crate::ubm_reclaim_graph_binding::GraphBinding,
        key: crate::ubm_reclaim_authority::GraphKey,
    ) -> std::result::Result<
        crate::ubm_reclaim_authority::PermitStatus,
        crate::ubm_reclaim_authority::AuthorityError,
    > {
        use crate::ubm_reclaim_authority::{AuthorityError, PermitStatus};
        let mut authority = self
            .shared
            .reclaim_authorities
            .lock()
            .map_err(|_| AuthorityError::Conflict)?;
        let connection = self
            .shared
            .source_connection
            .lock()
            .map_err(|_| AuthorityError::Conflict)?;
        let mut graph = self
            .shared
            .graph
            .lock()
            .map_err(|_| AuthorityError::Conflict)?;
        if !graph.reclaim_binding.owns(binding) {
            return Err(AuthorityError::Conflict);
        }
        let valid = connection.connected
            && graph.identity().ok().flatten().as_ref() == Some(&key.graph)
            && graph.reclaim_binding.validates(
                binding,
                &key.peer_id,
                &key.connection_generation,
                &key.graph,
            );
        let busy = graph.reclaim_gate.is_busy()
            || graph.reclaim_operations != 0
            || graph.native_operation_abandoned
            || graph.reclaim_effect_uncertain
            || graph.reclaim_callback_uncertain
            || !self
                .shared
                .ordinary_cccd_uncertain
                .lock()
                .map_err(|_| AuthorityError::Conflict)?
                .is_empty();
        let status = authority.acquire_bound(key.clone(), binding, valid, busy)?;
        if status == PermitStatus::Acquired && !graph.reclaim_gate.is_busy() {
            graph
                .reclaim_gate
                .acquire(key.clone())
                .map_err(|_| AuthorityError::Conflict)?;
            graph.reclaim_drain_graph = Some(key.graph);
        }
        Ok(status)
    }

    pub fn commit_reclaim(
        &self,
        key: &crate::ubm_reclaim_authority::GraphKey,
    ) -> std::result::Result<
        crate::ubm_reclaim_authority::PermitStatus,
        crate::ubm_reclaim_authority::AuthorityError,
    > {
        use crate::ubm_reclaim_authority::{AuthorityError, PermitStatus};
        use crate::ubm_reclaim_graph_gate::GateError;
        let mut authority = self
            .shared
            .reclaim_authorities
            .lock()
            .map_err(|_| AuthorityError::Conflict)?;
        let _connection = self
            .shared
            .source_connection
            .lock()
            .map_err(|_| AuthorityError::Conflict)?;
        let mut graph = self
            .shared
            .graph
            .lock()
            .map_err(|_| AuthorityError::Conflict)?;
        authority.authenticate_permit_owner(key, &graph.reclaim_binding)?;
        match authority.permit_status(key)? {
            PermitStatus::Committed => return Ok(PermitStatus::Committed),
            PermitStatus::Invalidated => return Ok(PermitStatus::Invalidated),
            PermitStatus::Acquired => {}
            _ => return Err(AuthorityError::Conflict),
        }
        match graph.reclaim_gate.commit(key) {
            Ok(()) => authority.commit(key),
            Err(GateError::Invalidated) => authority.invalidate_before_commit(key),
            Err(_) => Err(AuthorityError::Conflict),
        }
    }

    pub fn release_reclaim(
        &self,
        key: &crate::ubm_reclaim_authority::GraphKey,
    ) -> std::result::Result<
        crate::ubm_reclaim_authority::PermitStatus,
        crate::ubm_reclaim_authority::AuthorityError,
    > {
        use crate::ubm_reclaim_authority::{AuthorityError, PermitStatus};
        let mut authority = self
            .shared
            .reclaim_authorities
            .lock()
            .map_err(|_| AuthorityError::Conflict)?;
        let _connection = self
            .shared
            .source_connection
            .lock()
            .map_err(|_| AuthorityError::Conflict)?;
        let mut graph = self
            .shared
            .graph
            .lock()
            .map_err(|_| AuthorityError::Conflict)?;
        authority.authenticate_permit_owner(key, &graph.reclaim_binding)?;
        if authority.permit_status(key)? == PermitStatus::Released {
            return if graph.reclaim_gate.is_draining() {
                Err(AuthorityError::Pending)
            } else {
                Ok(PermitStatus::Released)
            };
        }
        graph
            .reclaim_gate
            .release(key)
            .map_err(|_| AuthorityError::Conflict)?;
        let status = authority.release(key)?;
        drop(graph);
        drop(_connection);
        drop(authority);
        self.shared.drain_reclaim_callbacks();
        Ok(status)
    }

    pub fn forget_reclaim(
        &self,
        key: &crate::ubm_reclaim_authority::GraphKey,
    ) -> std::result::Result<(), crate::ubm_reclaim_authority::AuthorityError> {
        use crate::ubm_reclaim_authority::AuthorityError;
        let mut authority = self
            .shared
            .reclaim_authorities
            .lock()
            .map_err(|_| AuthorityError::Conflict)?;
        let _connection = self
            .shared
            .source_connection
            .lock()
            .map_err(|_| AuthorityError::Conflict)?;
        let graph = self
            .shared
            .graph
            .lock()
            .map_err(|_| AuthorityError::Conflict)?;
        match authority.authenticate_permit_owner(key, &graph.reclaim_binding) {
            Ok(()) | Err(AuthorityError::Unknown) => {}
            Err(error) => return Err(error),
        }
        if graph.reclaim_gate.is_draining() {
            return Err(AuthorityError::Pending);
        }
        authority.forget_permit(key)
    }

    async fn own_reclaim_operation<T, F>(&self, effectful: bool, future: F) -> Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        self.own_reclaim_operation_inner(effectful, false, future)
            .await
    }

    async fn own_reclaim_write<T, F>(&self, future: F) -> Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        self.own_reclaim_operation_inner(true, false, future).await
    }

    async fn own_reclaim_operation_inner<T, F>(
        &self,
        effectful: bool,
        cleanup: bool,
        future: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        // Initialize the host executor before admission or any native work.
        super::marker_executor::handle().map_err(|error| Error::Other(error.into()))?;
        {
            let _connection = self
                .shared
                .source_connection
                .lock()
                .map_err(|_| Error::Other("WinRT connection owner lock poisoned".into()))?;
            let mut graph = self
                .shared
                .graph
                .lock()
                .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
            if graph.reclaim_gate.is_busy() || (!cleanup && graph.reclaim_operations >= 256) {
                return Err(Error::Other(
                    "WinRT graph is held or operation capacity exhausted".into(),
                ));
            }
            graph.reclaim_operations += 1;
        }
        let guard = ReclaimOperationGuard {
            shared: self.shared.clone(),
            effectful,
            settled: false,
            completed: false,
        };
        super::marker_executor::spawn_owned(async move {
            let mut guard = guard;
            let result = future.await;
            guard.completed = true;
            // A returned error may leave the remote effect unknown, but this
            // task has reached its terminal boundary. Resource-specific
            // recovery is retained separately from the in-flight fence.
            guard.settled = true;
            result
        })
        .map_err(|error| Error::Other(error.into()))?
        .await
        .map_err(|error| {
            Error::Other(format!("WinRT graph operation task failed: {error}").into())
        })?
    }
    /// Capture while the caller holds central connection admission.
    pub fn capture_reclaim_graph_binding(
        &self,
        peer_id: &str,
        connection_generation: &str,
        expected: &(String, u64, u64),
    ) -> std::result::Result<crate::ubm_reclaim_graph_binding::GraphBinding, SourceGroupError> {
        let connection = self
            .shared
            .source_connection
            .lock()
            .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        let graph = self
            .shared
            .graph
            .lock()
            .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        if !connection.connected || graph.identity().ok().flatten().as_ref() != Some(expected) {
            return Err(SourceGroupError::StaleGeneration);
        }
        graph
            .reclaim_binding
            .capture(
                peer_id.to_owned(),
                connection_generation.to_owned(),
                expected,
            )
            .ok_or(SourceGroupError::StaleGeneration)
    }

    /// Starts the OS operation under the same owner fence as source admission.
    pub async fn start_ordered_marker_write(
        &self,
        group: &SourceGroupHandle,
        expected: &(String, u64, u64),
        characteristic: &Characteristic,
        request: SourceWriteRequest,
    ) -> std::result::Result<SourceWriteReceipt, SourceGroupError> {
        let _wake_scope = self.shared.notification_faults.defer_wakes();
        if request.graph != crate::common::util::marker_graph_identity(expected)
            || notification_route(characteristic) != request.route
        {
            return Err(SourceGroupError::TransactionConflict);
        }
        if let Some(receipt) = group.replay_marker_write(&request)? {
            return Ok(receipt);
        }
        // Acquire before initiating the WinRT operation, then transfer the
        // lease to the detached reply task even if the caller goes away.
        let lease = self
            .shared
            .notification_lifecycle
            .clone()
            .read_owned()
            .await;
        let retained_request = request.clone();
        let outcome = (|| {
            let runtime = super::marker_executor::handle().map_err(|error| {
                warn!("{error}");
                SourceGroupError::SourceStateUnavailable
            })?;
            let value = request.value.clone();
            let mode = request.write_type;
            let start = start_marker_on_current_owner(
                &self.shared.source_connection,
                &self.shared.graph,
                &self.shared.notification_faults,
                group,
                expected,
                request,
                || {
                    let gatt = self
                        .gatt_characteristic(characteristic, "marker write")
                        .map_err(marker_write_error)?;
                    BLECharacteristic::start_marker_write_value(&gatt, &value, mode)
                        .map_err(marker_write_error)
                },
            )?;
            match start {
                SourceWriteStart::Replay(receipt) => Ok(receipt),
                SourceWriteStart::Rejected { receipt, wake }
                | SourceWriteStart::InitiationFailed { receipt, wake } => {
                    for consumer in wake {
                        consumer.wake();
                    }
                    Ok(receipt)
                }
                SourceWriteStart::Initiated {
                    receipt,
                    operation,
                    wake,
                } => {
                    spawn_marker_write_reply(
                        runtime,
                        self.shared.clone(),
                        lease,
                        receipt.clone(),
                        async move { operation.await.map_err(marker_write_error) },
                    );
                    for consumer in wake {
                        consumer.wake();
                    }
                    Ok(receipt)
                }
            }
        })();
        match outcome {
            Err(SourceGroupError::TransactionConflict) => {
                Err(SourceGroupError::TransactionConflict)
            }
            Err(error) => group.record_marker_not_admitted(retained_request, error),
            Ok(receipt) => Ok(receipt),
        }
    }

    /// Capture the native characteristic and its connection-scoped ATT identity.
    pub async fn notification_activation_owner(
        &self,
        characteristic: &Characteristic,
    ) -> Result<NotificationActivationOwner> {
        let mut connection = self.shared.source_connection.lock().unwrap();
        if !connection.connected {
            return Err(Error::NotConnected);
        }
        let owner = connection.owner.clone().ok_or(Error::NotConnected)?;
        let graph = self
            .shared
            .graph
            .lock()
            .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
        let graph_identity = graph
            .identity()?
            .ok_or_else(|| Error::Other("WinRT notification graph is stale".into()))?;
        let gatt = self.gatt_characteristic(characteristic, "notification activation")?;
        let handle = gatt.AttributeHandle()?;
        let physical_lane = connection.physical_lane(handle);
        let state = graph_activation(
            &mut connection.activations,
            graph_identity.2,
            handle,
            || {
                Arc::new(tokio::sync::Mutex::new(ActivationState {
                    gatt,
                    characteristic: characteristic.clone(),
                    registration: None,
                    prepared: Arc::new(Mutex::new(None)),
                }))
            },
        );
        Ok(NotificationActivationOwner {
            identity: NotificationActivationIdentity {
                connection: owner,
                handle,
                graph_revision: graph_identity.2,
            },
            state,
            claim: Arc::new(Mutex::new(None)),
            prepared_claim: Arc::new(Mutex::new(None)),
            shared: self.shared.clone(),
            graph_identity,
            physical_lane,
        })
    }

    /// Bind the caller's connection generation to this native notification source.
    /// The generation currently owned by the native notification source.
    pub fn current_notification_source_generation(
        &self,
    ) -> std::result::Result<Option<u64>, SourceGroupError> {
        self.shared
            .source_connection
            .lock()
            .map(|connection| connection.source_generation)
            .map_err(|_| SourceGroupError::SourceStateUnavailable)
    }

    pub fn start_notification_source_generation(
        &self,
        generation: u64,
    ) -> std::result::Result<(), SourceGroupError> {
        let mut connection = self.shared.source_connection.lock().unwrap();
        if !connection.connected
            || self
                .shared
                .graph
                .lock()
                .map_err(|_| SourceGroupError::SourceStateUnavailable)?
                .identity()
                .ok()
                .flatten()
                .is_none()
        {
            return Err(SourceGroupError::StaleGeneration);
        }
        self.shared
            .notification_faults
            .start_source_generation(generation)?;
        connection.source_generation = Some(generation);
        Ok(())
    }

    pub fn open_notification_source_group(
        &self,
        generation: u64,
        max_records: usize,
        max_bytes: usize,
    ) -> std::result::Result<(SourceGroupHandle, SourceGroupConsumer), SourceGroupError> {
        self.shared
            .notification_faults
            .open_source_group(generation, max_records, max_bytes)
    }

    pub fn accepted_gatt_graph_identity(&self) -> Result<Option<(String, u64, u64)>> {
        self.shared
            .graph
            .lock()
            .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?
            .identity()
    }

    pub fn gatt_graph_snapshot(
        &self,
    ) -> Result<(
        BTreeSet<Service>,
        Vec<(Uuid, u64, &'static str)>,
        (String, u64, u64),
    )> {
        let graph = self
            .shared
            .graph
            .lock()
            .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
        let identity = graph
            .identity()?
            .ok_or_else(|| Error::Other("WinRT graph is not current".into()))?;
        Ok((
            self.raw_services(),
            self.raw_service_restrictions(),
            identity,
        ))
    }

    pub fn open_notification_source_group_checked(
        &self,
        generation: u64,
        max_records: usize,
        max_bytes: usize,
        expected: &(String, u64, u64),
    ) -> std::result::Result<(SourceGroupHandle, SourceGroupConsumer), SourceGroupError> {
        let graph = self
            .shared
            .graph
            .lock()
            .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        if graph.identity().ok().flatten().as_ref() != Some(expected) {
            return Err(SourceGroupError::StaleGeneration);
        }
        self.shared
            .notification_faults
            .open_source_group(generation, max_records, max_bytes)
    }

    pub fn register_notification_source_route(
        &self,
        group: &SourceGroupHandle,
        route: NotificationRoute,
        member: u64,
    ) -> std::result::Result<Membership, SourceGroupError> {
        self.shared
            .notification_faults
            .register_source_route(group, route, member)
    }

    pub fn register_notification_source_route_for_activation(
        &self,
        group: &SourceGroupHandle,
        route: NotificationRoute,
        member: u64,
        activation: u64,
    ) -> std::result::Result<Membership, SourceGroupError> {
        self.shared
            .notification_faults
            .register_source_route_for_activation(group, route, member, activation)
    }

    pub fn retire_notification_source_route(
        &self,
        group: &SourceGroupHandle,
        route: NotificationRoute,
        membership: Membership,
    ) -> std::result::Result<(), SourceGroupError> {
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::Admission;
        let _wakes = self.shared.notification_faults.defer_wakes();
        let synthetic = GraphKey {
            authority_id: 0,
            peer_id: String::new(),
            connection_generation: String::new(),
            graph: (String::new(), 0, 0),
            permit_id: 0,
        };
        let bytes = std::mem::size_of::<Box<dyn FnOnce() + Send>>()
            .saturating_add(std::mem::size_of::<
                crate::common::util::SourceRouteRetirement,
            >())
            .saturating_add(std::mem::size_of::<
                crate::ubm_peer_notification_ingress::IngressReservation,
            >())
            .saturating_add(3 * std::mem::size_of::<usize>());
        let mut graph = self
            .shared
            .graph
            .lock()
            .expect("WinRT graph owner lock poisoned");
        let owner = !graph.reclaim_gate.is_busy();
        if owner {
            graph
                .reclaim_gate
                .acquire(synthetic.clone())
                .expect("idle callback gate");
            graph.reclaim_drain_graph = graph.identity().ok().flatten();
            graph
                .reclaim_gate
                .commit(&synthetic)
                .expect("new callback gate");
        }
        let result = if !graph.reclaim_gate.can_reserve_before_admit(bytes) {
            graph.reclaim_gate.fail_tail();
            graph.note_callback_overflow();
            Err(SourceGroupError::SourceStateUnavailable)
        } else {
            match self
                .shared
                .notification_faults
                .reserve_source_route_retirement(group, route, membership)
            {
                Ok(held) => {
                    let callback: Box<dyn FnOnce() + Send> = Box::new(move || held.release());
                    match graph.reclaim_gate.admit(callback, bytes, false) {
                        Admission::Deferred => Ok(()),
                        Admission::Overflow(_) | Admission::Apply(_) => {
                            unreachable!("preflight under graph lock")
                        }
                    }
                }
                Err(error) => Err(error),
            }
        };
        if owner {
            graph
                .reclaim_gate
                .release(&synthetic)
                .expect("callback gate owner");
        }
        drop(graph);
        if owner {
            self.shared.drain_reclaim_callbacks();
        }
        result
    }

    pub fn notification_source_barrier(
        &self,
        group: &SourceGroupHandle,
        id: u64,
    ) -> std::result::Result<(), SourceGroupError> {
        self.shared
            .notification_faults
            .source_group_barrier(group, id)
    }

    pub fn detach_notification_source_group_after_barrier(
        &self,
        group: &SourceGroupHandle,
        barrier_id: u64,
    ) -> std::result::Result<(), SourceGroupError> {
        self.shared
            .notification_faults
            .detach_source_group_after_barrier(group, barrier_id)
    }

    pub fn detach_notification_source_group(
        &self,
        group: &SourceGroupHandle,
    ) -> std::result::Result<(), SourceGroupError> {
        self.shared.notification_faults.detach_source_group(group)
    }

    pub async fn connection_phy(&self) -> Result<crate::connection_phy_source::ConnectionPhy> {
        let device = self.shared.device.lock().await;
        let owner = device.as_ref().ok_or(Error::NotConnected)?;
        owner.get_connection_phy()
    }
    /// Observed connection-parameter changes for this peer. Lagged
    /// receivers report the gap; the channel closes when the peripheral
    /// is dropped.
    pub fn connection_parameter_events(&self) -> broadcast::Receiver<ConnectionParametersReport> {
        self.shared.connection_parameters.subscribe()
    }

    pub(crate) fn set_explicit_address_type(&self, requested: AddressType) -> Result<()> {
        let mut kind = self
            .shared
            .explicit_address_type
            .write()
            .map_err(Error::from)?;
        let advertised = *self.shared.address_type.read().map_err(Error::from)?;
        if kind
            .or(advertised)
            .is_some_and(|existing| existing != requested)
        {
            return Err(Error::RuntimeError(
                "explicit address type conflicts with the existing peripheral identity".into(),
            ));
        }
        *kind = Some(requested);
        Ok(())
    }

    pub(crate) fn new_with_id(
        adapter: Weak<AdapterManager<Self>>,
        identity: PeripheralId,
        reclaim_authorities: Arc<Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
    ) -> Self {
        Self::new_with_id_with_failure_evidence_store(adapter, identity, reclaim_authorities, None)
    }

    pub(crate) fn new_with_id_with_failure_evidence_store(
        adapter: Weak<AdapterManager<Self>>,
        identity: PeripheralId,
        reclaim_authorities: Arc<Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
        failure_evidence_store: Option<
            Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>,
        >,
    ) -> Self {
        static NEXT_GRAPH_ATTACHMENT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        let attachment = NEXT_GRAPH_ATTACHMENT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .ok();
        let address = identity.address();
        let address_type = identity.address_type();
        let (broadcast_sender, _) = broadcast::channel(crate::ubm::EVENT_CAPACITY);
        let (connection_parameters, _) = broadcast::channel(crate::ubm::EVENT_CAPACITY);
        Peripheral {
            shared: Arc::new(Shared {
                identity,
                adapters: Mutex::new(vec![adapter]),
                reclaim_authorities,
                device: Arc::new(tokio::sync::Mutex::new(None)),
                partial_device: Arc::new(tokio::sync::Mutex::new(None)),
                address,
                mtu: AtomicU16::new(api::DEFAULT_MTU_SIZE),
                connected: AtomicBool::new(false),
                ble_services: DashMap::new(),
                retired_services: Mutex::new(Vec::new()),
                notifications_channel: broadcast_sender,
                notification_faults: Arc::new(NotificationFaults::new_with_failure_evidence_store(
                    crate::ubm::EVENT_CAPACITY,
                    failure_evidence_store,
                )),
                managed_callbacks: {
                    let callbacks = Arc::new(super::callback_retirement::CallbackRetirement::new());
                    callbacks.close();
                    callbacks
                },
                callback_transition: Mutex::new(()),
                notification_lifecycle: Arc::new(tokio::sync::RwLock::new(())),
                ordinary_cccd_lock: tokio::sync::Mutex::new(()),
                ordinary_cccd_uncertain: Mutex::new(HashSet::new()),
                ordinary_cccd_local_cleanup: Mutex::new(HashSet::new()),
                source_connection: Mutex::new(SourceConnection {
                    activations: HashMap::new(),
                    physical_lanes: HashMap::new(),
                    source_generation: None,
                    next_activation: 1,
                    owner: None,
                    connected: false,
                }),
                graph: Mutex::new(NativeGraph {
                    attachment,
                    revision: 0,
                    exhausted: false,
                    ready: false,
                    reclaim_binding: Default::default(),
                    reclaim_operations: 0,
                    reclaim_effect_uncertain: false,
                    native_operation_abandoned: false,
                    reclaim_callback_uncertain: false,
                    reclaim_callback_recoverable: false,
                    reclaim_overflow_epoch: Some(0),
                    reclaim_gate: crate::ubm_reclaim_graph_gate::GraphGate::new(256, 524_288),
                    reclaim_drain_graph: None,
                }),
                pending_failure_spill: Mutex::new(None),
                pending_durable_failure: AtomicBool::new(false),
                deferred_services_changed: Mutex::new(None),
                reclaim_drain_changed: tokio::sync::Notify::new(),
                connection_parameters,
                address_type: RwLock::new(address_type),
                explicit_address_type: RwLock::new(None),
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
            address_type: self.shared.explicit_address_type.read().unwrap().or(*self
                .shared
                .address_type
                .read()
                .unwrap()),
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
        address_type: Option<AddressType>,
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
            address_type,
            source: api::ReportSource::Advertisement,
            local_name,
            rssi: Some(args.RawSignalStrengthInDBm()?),
            tx_power_level,
            manufacturer_data,
            service_data,
            services,
            solicited_services: None,
            overflow_services: None,
            connectable: gatt_model::advertisement_connectable(
                args.AdvertisementType()
                    .map(|kind| kind.0)
                    .unwrap_or(i32::MIN),
                args.IsConnectable().ok(),
            ),
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
                    id: self.id(),
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
                    id: self.id(),
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
                    id: self.id(),
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
                    id: self.id(),
                    rssi,
                });
            }
        }
    }

    /// UBM patch (`winrt-cccd-mode`): write `value` to the CCCD of exactly
    /// this characteristic instance (service and characteristic addressed by
    /// UUID and handle), on the same GATT object the subscription uses.
    /// The desktop enable path does not call this as a second write: the
    /// first subscribe writes the selected mode.
    pub async fn write_client_configuration(
        &self,
        characteristic: &Characteristic,
        value: GattClientCharacteristicConfigurationDescriptorValue,
    ) -> Result<()> {
        let expected = self.operation_graph()?;
        let peripheral = self.clone();
        let characteristic = characteristic.clone();
        self.own_reclaim_write(async move {
            let _lease = peripheral.shared.notification_lifecycle.read().await;
            let _cccd = peripheral.shared.ordinary_cccd_lock.lock().await;
            if peripheral
                .shared
                .ordinary_cccd_uncertain
                .lock()
                .unwrap()
                .contains(&ordinary_cccd_key(&characteristic))
            {
                return Err(Error::Other(
                    "WinRT CCCD outcome is uncertain; unsubscribe to recover".into(),
                ));
            }
            let gatt = peripheral.gatt_characteristic(&characteristic, "CCCD write")?;
            let started = {
                let graph = peripheral
                    .shared
                    .graph
                    .lock()
                    .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
                start_on_current_graph(&graph, &expected, || {
                    BLECharacteristic::start_client_configuration(&gatt, value)
                })?
            };
            let outcome = match started {
                Ok(operation) => {
                    BLECharacteristic::finish_client_configuration(operation, "CCCD write").await
                }
                Err(failure) => Err(failure),
            };
            match outcome {
                Ok(()) => Ok(()),
                Err(failure) => {
                    if !failure.proven_no_effect {
                        peripheral.note_remote_cccd_uncertainty(ordinary_cccd_key(&characteristic));
                    }
                    Err(failure.error)
                }
            }
        })
        .await
    }

    /// Write `value` on the first subscribe. A proven refusal rolls back the
    /// handler; an uncertain outcome retains it until explicit recovery.
    pub async fn subscribe_with_configuration(
        &self,
        characteristic: &Characteristic,
        value: GattClientCharacteristicConfigurationDescriptorValue,
    ) -> Result<()> {
        self.subscribe_writing(characteristic, Some(value)).await
    }

    /// `None` keeps Indicate-if-possible. `Some` writes that value once.
    async fn subscribe_writing(
        &self,
        characteristic: &Characteristic,
        configured: Option<GattClientCharacteristicConfigurationDescriptorValue>,
    ) -> Result<()> {
        let expected = self.operation_graph()?;
        let peripheral = self.clone();
        let characteristic = characteristic.clone();
        self.own_reclaim_write(async move {
            let _lease = peripheral.shared.notification_lifecycle.read().await;
            let _cccd = peripheral.shared.ordinary_cccd_lock.lock().await;
            peripheral
                .subscribe_writing_inner(&characteristic, configured, &expected)
                .await
        })
        .await
    }

    async fn subscribe_writing_inner(
        &self,
        characteristic: &Characteristic,
        configured: Option<GattClientCharacteristicConfigurationDescriptorValue>,
        expected: &(String, u64, u64),
    ) -> Result<()> {
        let key = ordinary_cccd_key(characteristic);
        if self
            .shared
            .ordinary_cccd_uncertain
            .lock()
            .unwrap()
            .contains(&key)
        {
            return Err(cccd_recovery_error(true));
        }
        let notification_epoch = self.shared.notification_faults.begin(characteristic)?;
        let notifications_sender = self.shared.notifications_channel.clone();
        let notification_faults = self.shared.notification_faults.clone();
        let uuid = characteristic.uuid;
        let instance = characteristic.instance;
        let service_uuid = characteristic.service_uuid;
        let service_instance = characteristic.service_instance;
        let handler: NotifyEventHandler = std::sync::Arc::new(move |value| {
            let (value, source_failure) = match value {
                Ok(bytes) => (bytes, None),
                Err(error) => (Vec::new(), Some(error)),
            };
            let notification = ValueNotification {
                uuid,
                instance,
                service_uuid,
                service_instance,
                value,
                source_failure,
                lost_before: 0,
            };
            notification_faults.publish_for_epoch(
                &notifications_sender,
                notification,
                notification_epoch,
            );
        });
        let peer = self.clone();
        let route = characteristic.clone();
        let registration_faults = self.shared.notification_faults.clone();
        let registration = self
            .shared
            .run_native_cleanup(move || {
                peer.with_characteristic_mut(&route, "subscribe", |ble_characteristic| {
                    let (config, token) = match configured {
                        Some(value) => {
                            ble_characteristic.register_with(handler, value, registration_faults)?
                        }
                        None => ble_characteristic.register(handler, registration_faults)?,
                    };
                    Ok((ble_characteristic.gatt().clone(), config, token))
                })
            })
            .await;
        let (gatt, config, token) = match registration {
            Ok(registration) => registration,
            Err(error) => {
                self.shared.notification_faults.clear(characteristic);
                return Err(error);
            }
        };
        let started = {
            let graph = self
                .shared
                .graph
                .lock()
                .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
            match start_on_current_graph(&graph, expected, || {
                BLECharacteristic::start_client_configuration(&gatt, config)
            }) {
                Ok(result) => result,
                Err(error) => Err(super::ble::characteristic::CccdWriteFailure {
                    error,
                    proven_no_effect: true,
                }),
            }
        };
        let written = match started {
            Ok(operation) => {
                BLECharacteristic::finish_client_configuration(operation, "subscribe").await
            }
            Err(failure) => Err(failure),
        };
        if let Err(failure) = written {
            if !failure.proven_no_effect {
                self.note_remote_cccd_uncertainty(key);
                return Err(failure.error);
            }
            let error = failure.error;
            let peer = self.clone();
            let route = characteristic.clone();
            if let Err(rollback) = self
                .shared
                .run_native_cleanup(move || {
                    peer.with_characteristic_mut(&route, "subscribe", |ble_characteristic| {
                        ble_characteristic.deregister(token)
                    })
                })
                .await
            {
                self.shared
                    .ordinary_cccd_uncertain
                    .lock()
                    .unwrap()
                    .insert(key);
                self.shared
                    .ordinary_cccd_local_cleanup
                    .lock()
                    .unwrap()
                    .insert(key);
                return Err(Error::WithCleanup {
                    primary: Box::new(error),
                    cleanup: Box::new(rollback),
                });
            }
            self.shared.notification_faults.clear(characteristic);
            return Err(error);
        }
        Ok(())
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

    /// Remove displaced registrations after terminating the old source. Failed
    /// removals retain their exact service owner for retry; no handler is adopted
    /// by a discovered characteristic, so consumers must subscribe afresh.
    fn recover_retired_services(&self) -> Result<()> {
        let mut retired = self
            .shared
            .retired_services
            .lock()
            .map_err(|_| Error::Other("WinRT retired service lock poisoned".into()))?;
        super::ble::device::recover_matching(
            &mut retired,
            |_| true,
            |previous| {
                for old in previous.characteristics.values_mut() {
                    old.take_registration()?;
                }
                Ok(())
            },
        )
    }

    // A proven unsuccessful write, or a successful disable followed by a
    // failed handler removal, needs only the exact old registration removed.
    // This remains safe after service change and performs no GATT write.
    fn recover_local_cccd_registrations(&self) -> Result<()> {
        let routes: Vec<_> = self
            .shared
            .ordinary_cccd_local_cleanup
            .lock()
            .map_err(|_| Error::Other("WinRT CCCD cleanup lock poisoned".into()))?
            .iter()
            .copied()
            .collect();
        for (service_uuid, service_instance, uuid, instance) in routes {
            let mut service = self
                .shared
                .ble_services
                .get_mut(&(service_uuid, service_instance))
                .ok_or_else(|| Error::Other("WinRT CCCD registration owner missing".into()))?;
            let characteristic = service
                .characteristics
                .get_mut(&(uuid, instance))
                .ok_or_else(|| Error::Other("WinRT CCCD registration owner missing".into()))?;
            characteristic.take_registration()?;
            let route = characteristic.to_characteristic(service_uuid, service_instance);
            drop(service);
            let key = (service_uuid, service_instance, uuid, instance);
            self.shared
                .ordinary_cccd_local_cleanup
                .lock()
                .unwrap()
                .remove(&key);
            self.shared
                .ordinary_cccd_uncertain
                .lock()
                .unwrap()
                .remove(&key);
            self.shared.notification_faults.clear(&route);
        }
        Ok(())
    }

    fn install_services(&self, table: HashMap<AttributeKey, BLEService>) -> Result<()> {
        let stale: Vec<_> = self
            .shared
            .ble_services
            .iter()
            .map(|entry| *entry.key())
            .filter(|key| !table.contains_key(key))
            .collect();
        let mut retired = self
            .shared
            .retired_services
            .lock()
            .map_err(|_| Error::Other("WinRT retired service lock poisoned".into()))?;
        for (key, service) in table {
            if let Some(previous) = self.shared.ble_services.insert(key, service) {
                retired.push(previous);
            }
        }
        for key in stale {
            if let Some((_, previous)) = self.shared.ble_services.remove(&key) {
                retired.push(previous);
            }
        }
        Ok(())
    }

    fn raw_services(&self) -> BTreeSet<Service> {
        self.shared
            .ble_services
            .iter()
            .map(|item| item.value().to_service())
            .collect()
    }

    fn raw_service_restrictions(&self) -> Vec<(Uuid, u64, &'static str)> {
        self.shared
            .ble_services
            .iter()
            .filter_map(|item| {
                let service = item.value();
                let label = match service.access {
                    gatt_model::ServiceRestriction::Open => return None,
                    gatt_model::ServiceRestriction::OsReserved => "os-reserved",
                    gatt_model::ServiceRestriction::AccessDenied => "access-denied",
                };
                Some((service.uuid, service.instance, label))
            })
            .collect()
    }

    fn emit_event(&self, event: CentralEvent) {
        self.shared.emit(event);
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
        self.shared.identity.clone()
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
        let Ok(graph) = self.shared.graph.lock() else {
            log::error!("WinRT graph owner lock poisoned while listing services");
            return BTreeSet::new();
        };
        if !matches!(graph.identity(), Ok(Some(_))) {
            return BTreeSet::new();
        }
        self.raw_services()
    }

    fn service_restrictions(&self) -> Vec<(Uuid, u64, &'static str)> {
        let Ok(graph) = self.shared.graph.lock() else {
            log::error!("WinRT graph owner lock poisoned while listing service restrictions");
            return Vec::new();
        };
        if !matches!(graph.identity(), Ok(Some(_))) {
            return Vec::new();
        }
        self.raw_service_restrictions()
    }

    /// Returns true iff we are currently connected to the device.
    async fn is_connected(&self) -> Result<bool> {
        Ok(self.shared.connected.load(Ordering::Relaxed))
    }

    /// Creates a connection to the device. This is a synchronous operation; if this method returns
    /// Ok there has been successful connection. Note that peripherals allow only one connection at
    /// a time. Operations that attempt to communicate with a device will fail until it is connected.
    async fn connect(&self) -> Result<()> {
        let peripheral = self.clone();
        self.own_reclaim_operation_inner(true, true, async move {
            let peripheral = &peripheral;
            let _delivery_hold = peripheral.shared.notification_faults.hold_wake_delivery();
            let _lifecycle = super::marker_executor::try_graph_exclusive(
                &peripheral.shared.notification_lifecycle,
            )
            .map_err(|error| Error::Other(error.into()))?;
            peripheral.shared.ensure_native_owner_replaceable()?;
            // Keep the identity claim through recovery and replacement. Unknown
            // address types overlap both resolved identities for the same address.
            let recovery = Arc::new(super::ble::device::RecoveryLease::acquire(
                peripheral.shared.address,
                peripheral.shared.identity.address_type(),
            )?);
            let recovery_worker = recovery.clone();
            peripheral
                .shared
                .run_native_cleanup(move || recovery_worker.recover())
                .await?;
            // Serialize native owner creation with disconnect, including its await.
            let d = peripheral
                .shared
                .device
                .clone()
                .try_lock_owned()
                .map_err(|_| {
                    Error::Other("WinRT native device is busy; retry connection cleanup".into())
                })?;
            peripheral.shared.managed_callbacks.close();
            peripheral.shared.managed_callbacks.wait().await;
            {
                let _transition = peripheral.shared.callback_transition.lock().unwrap();
                let mut source = peripheral.shared.source_connection.lock().unwrap();
                source.begin_disconnect();
                drop(source);
                peripheral.shared.notification_faults.clear_all();
                peripheral.shared.fence_graph_callbacks();
                peripheral.shared.graph.lock().unwrap().invalidate();
                peripheral.shared.connected.store(false, Ordering::Relaxed);
            }
            // Retire the previous owner before creating another candidate. A
            // failure leaves that owner in the shared slot for an explicit retry.
            let mut d = peripheral.shared.close_owned_device(d).await?;
            let owner = Arc::new(AtomicBool::new(false));
            let identity = peripheral.id();

            let connection_status_changed = Box::new({
                let shared_clone = Arc::downgrade(&peripheral.shared);
                let owner = owner.clone();
                move |is_connected: bool| {
                    let Some(shared) = shared_clone.upgrade() else {
                        return;
                    };
                    let shared_weak = shared_clone.clone();
                    let owner = owner.clone();
                    let identity = identity.clone();
                    shared.admit_reclaim_callback(0, !is_connected, move || {
                        let Some(shared) = shared_weak.upgrade() else {
                            return;
                        };
                        let _wake_scope = shared.notification_faults.defer_wakes();
                        let _transition = shared.callback_transition.lock().unwrap();
                        let mut source = shared.source_connection.lock().unwrap();
                        if !is_connected && !source.observe_disconnect(&owner) {
                            return;
                        }
                        if !source
                            .owner
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, &owner))
                        {
                            return;
                        }
                        drop(source);
                        if !is_connected {
                            shared.notification_faults.clear_all();
                            shared.fence_graph_callbacks();
                            let source = shared.source_connection.lock().unwrap();
                            if !source
                                .owner
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &owner))
                            {
                                return;
                            }
                            shared.graph.lock().unwrap().invalidate();
                            shared.connected.store(false, Ordering::Relaxed);
                        }
                        drop(_transition);
                        if !is_connected {
                            shared.emit(CentralEvent::DeviceDisconnected(identity));
                        }
                    });
                }
            });

            let max_pdu_size_changed = Box::new({
                let shared_clone = Arc::downgrade(&peripheral.shared);
                move |mtu| {
                    if let Some(shared) = shared_clone.upgrade() {
                        shared.mtu.store(mtu, Ordering::Relaxed);
                    }
                }
            });

            let address_type = peripheral
                .shared
                .explicit_address_type
                .read()
                .map_err(Error::from)?
                .or(*peripheral.shared.address_type.read().map_err(Error::from)?);
            let connection_parameters_changed = Box::new({
                let sender = peripheral.shared.connection_parameters.clone();
                move |params| {
                    let _ = sender.send(params);
                }
            });
            let services_changed = Box::new({
                let shared = Arc::downgrade(&peripheral.shared);
                let owner = owner.clone();
                let identity = peripheral.id();
                move || {
                    let Some(current) = shared.upgrade() else {
                        return;
                    };
                    let shared = shared.clone();
                    let owner = owner.clone();
                    let identity = identity.clone();
                    current.admit_reclaim_callback(0, true, move || {
                        let Some(shared) = shared.upgrade() else {
                            return;
                        };
                        shared.apply_services_changed(&owner, identity);
                    });
                }
            });
            let address = peripheral.shared.address;
            // Constructor rollback may call synchronous native cleanup too. Its
            // dedicated runtime cannot stall completion processing for other peers.
            let construction = super::marker_executor::run_blocking_owned(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .map_err(|_| "WinRT constructor runtime initialization failed")?;
                Ok::<_, &'static str>(runtime.block_on(BLEDevice::new_retained(
                    address,
                    address_type,
                    connection_status_changed,
                    max_pdu_size_changed,
                    connection_parameters_changed,
                    services_changed,
                )))
            })
            .await
            .map_err(|error| {
                if error == "WinRT cleanup thread exited without a result" {
                    peripheral
                        .shared
                        .graph
                        .lock()
                        .expect("WinRT graph owner lock poisoned")
                        .native_operation_abandoned = true;
                }
                Error::Other(error.into())
            })?
            .map_err(|error| Error::Other(error.into()))?;
            let device = match construction {
                Ok(device) => device,
                Err(failure) => {
                    *peripheral.shared.partial_device.lock().await = failure.owner;
                    return Err(match failure.cleanup_error {
                        Some(cleanup) => Error::WithCleanup {
                            primary: Box::new(failure.error),
                            cleanup: Box::new(cleanup),
                        },
                        None => failure.error,
                    });
                }
            };

            // Publish the native owner before awaiting connection discovery. A
            // cancelled connect must leave its query and device available to the
            // explicit disconnect/close retry rather than running blocking Drop.
            {
                let _transition = peripheral.shared.callback_transition.lock().unwrap();
                let mut source = peripheral.shared.source_connection.lock().unwrap();
                if let Some(previous) = source.owner.take() {
                    previous.store(true, Ordering::Relaxed);
                }
                source.owner = Some(owner.clone());
                source.activations.clear();
                source.physical_lanes.clear();
                source.source_generation = None;
                source.connected = false;
                peripheral.shared.graph.lock().unwrap().invalidate();
                peripheral.shared.connected.store(false, Ordering::Relaxed);
                peripheral.shared.notification_faults.clear_all();
            }
            *d = Some(device);
            let Some(device) = d.as_mut() else {
                return Err(Error::NotConnected);
            };
            device.connect().await?;
            // Query the system-cached device name (GAP name) and update local_name
            if let Ok(name) = device.name() {
                let name_str = name.to_string();
                if !name_str.is_empty() {
                    let mut local_name_guard = peripheral.shared.local_name.write().unwrap();
                    *local_name_guard = Some(name_str);
                }
            }
            {
                let mut source = peripheral.shared.source_connection.lock().unwrap();
                if !source.finish_connect(&owner) {
                    return Err(Error::NotConnected);
                }
                peripheral.shared.connected.store(true, Ordering::Relaxed);
            }
            peripheral.emit_event(CentralEvent::DeviceConnected(peripheral.id()));
            Ok(())
        })
        .await
    }

    /// Terminates a connection to the device. This is a synchronous operation.
    async fn disconnect(&self) -> Result<()> {
        let peripheral = self.clone();
        self.own_reclaim_operation_inner(true, true, async move {
            let peripheral = &peripheral;
            peripheral.shared.pending_failure_spill_error()?;
            let _delivery_hold = peripheral.shared.notification_faults.hold_wake_delivery();
            let _lifecycle = super::marker_executor::try_graph_exclusive(
                &peripheral.shared.notification_lifecycle,
            )
            .map_err(|error| Error::Other(error.into()))?;
            peripheral.shared.ensure_native_owner_replaceable()?;
            // Keep the identity claim through recovery and replacement. Unknown
            // address types overlap both resolved identities for the same address.
            let recovery = Arc::new(super::ble::device::RecoveryLease::acquire(
                peripheral.shared.address,
                peripheral.shared.identity.address_type(),
            )?);
            let recovery_worker = recovery.clone();
            peripheral
                .shared
                .run_native_cleanup(move || recovery_worker.recover())
                .await?;
            let device = peripheral
                .shared
                .device
                .clone()
                .try_lock_owned()
                .map_err(|_| {
                    Error::Other("WinRT native device is busy; retry disconnect".into())
                })?;
            peripheral.shared.managed_callbacks.close();
            peripheral.shared.managed_callbacks.wait().await;
            {
                let _transition = peripheral.shared.callback_transition.lock().unwrap();
                let mut source = peripheral.shared.source_connection.lock().unwrap();
                source.begin_disconnect();
                drop(source);
                peripheral.shared.notification_faults.clear_all();
                peripheral.shared.fence_graph_callbacks();
                peripheral.shared.graph.lock().unwrap().invalidate();
                peripheral.shared.connected.store(false, Ordering::Relaxed);
            }
            // Explicit failure preserves this owner for the caller's cleanup retry.
            let mut device = peripheral.shared.close_owned_device(device).await?;
            // We need to clear the services because if this device is re-connected,
            // the cached service objects will no longer be valid (they must be refreshed).
            peripheral.shared.ble_services.clear();
            *device = None;
            {
                let _transition = peripheral.shared.callback_transition.lock().unwrap();
                let mut source = peripheral.shared.source_connection.lock().unwrap();
                if let Some(owner) = source.owner.take() {
                    owner.store(true, Ordering::Relaxed);
                }
            }
            peripheral.shared.connected.store(false, Ordering::Relaxed);
            peripheral.emit_event(CentralEvent::DeviceDisconnected(peripheral.id()));
            Ok(())
        })
        .await
    }

    /// Discovers all characteristics for the device. This is a synchronous operation.
    ///
    /// UBM patch (`winrt-attribute-instances`, `winrt-uncached-discovery`):
    /// every service, characteristic and descriptor is queried `Uncached`
    /// and kept as its own instance keyed by (UUID, `AttributeHandle`). The
    /// discovered database REPLACES the previous one, so a discovery after
    /// `GattServicesChanged` drops removed and changed attributes. A query
    /// the peer or the link refuses fails the discovery (naming the
    /// attribute and the status). Rediscovery terminates the prior notification
    /// source with an uncertain tail; fresh subscriptions are required. If
    /// registration removal fails, displaced owners remain available for retry.
    /// Native service close failures likewise leave the graph unavailable;
    /// another discovery or disconnect retries the retained cleanup.
    /// A known OS-reserved service, and an ordinary service Windows denies
    /// without an ATT byte, stay in the table with no characteristics.
    async fn discover_services(&self) -> Result<()> {
        let peripheral = self.clone();
        self.own_reclaim_operation(false, async move {
            let peripheral = &peripheral;
            let _lifecycle = super::marker_executor::try_graph_exclusive(
                &peripheral.shared.notification_lifecycle,
            )
            .map_err(|error| Error::Other(error.into()))?;
            peripheral
                .shared
                .ensure_managed_activations_retired()
                .await?;
            let local_pending = !peripheral
                .shared
                .ordinary_cccd_local_cleanup
                .lock()
                .unwrap()
                .is_empty();
            if cccd_recovery_action(false, local_pending) == CccdRecoveryAction::RemoveRegistration
            {
                let local_peer = peripheral.clone();
                peripheral
                    .shared
                    .run_native_cleanup(move || local_peer.recover_local_cccd_registrations())
                    .await?;
            }
            let cccd_pending = !peripheral
                .shared
                .ordinary_cccd_uncertain
                .lock()
                .unwrap()
                .is_empty();
            if cccd_pending {
                let ready = peripheral
                    .shared
                    .graph
                    .lock()
                    .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?
                    .identity()?
                    .is_some();
                return Err(cccd_recovery_error(ready));
            }
            let recovery_peer = peripheral.clone();
            peripheral
                .shared
                .run_native_cleanup(move || recovery_peer.recover_retired_services())
                .await?;
            let (revision, previous_graph) = {
                let _source = peripheral
                    .shared
                    .source_connection
                    .lock()
                    .map_err(|_| Error::Other("WinRT connection owner lock poisoned".into()))?;
                let graph = peripheral
                    .shared
                    .graph
                    .lock()
                    .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
                if graph.exhausted || graph.attachment.is_none() {
                    return Err(Error::Other("WinRT graph identity exhausted".into()));
                }
                (graph.revision, graph.identity()?)
            };
            let mut device_guard = peripheral.shared.device.clone().lock_owned().await;
            // Registration recovery above precedes retirement of any failed
            // candidate. Native Close may block, so keep the exact device owner
            // in the cleanup worker and return it only after retirement succeeds.
            device_guard = peripheral
                .shared
                .run_native_cleanup(move || {
                    let device = device_guard.as_mut().ok_or(Error::NotConnected)?;
                    device.prepare_service_candidate()?;
                    Ok(device_guard)
                })
                .await?;
            let Some(device) = device_guard.as_mut() else {
                return Err(Error::NotConnected);
            };
            let mut gatt_services = device.discover_services().await?;
            let mut discovered = Vec::with_capacity(gatt_services.len());
            let mut visited = HashSet::new();
            while let Some(service) = gatt_services.pop() {
                let key = (
                    utils::to_uuid(&service.Uuid()?),
                    u64::from(service.AttributeHandle()?),
                );
                if !visited.insert(key) {
                    continue;
                }
                let mut graph_service = discover_service(device, service.clone()).await?;
                if graph_service.access == gatt_model::ServiceRestriction::Open {
                    if let Some(included) = device
                        .get_included_services(&service)
                        .await
                        .map_err(|error| annotate_attribute("service", key.0, key.1, error))?
                    {
                        for target in &included {
                            device.retain_included_service(target);
                        }
                        graph_service.included_services = Some(
                            included
                                .iter()
                                .map(|target| {
                                    Ok(crate::api::IncludedService {
                                        uuid: utils::to_uuid(&target.Uuid()?),
                                        instance: u64::from(target.AttributeHandle()?),
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?,
                        );
                        gatt_services.extend(included);
                    }
                }
                discovered.push(graph_service);
            }
            let table = index_unique(
                discovered
                    .into_iter()
                    .map(|service| (service.key(), service)),
            )
            .map_err(|(uuid, handle)| {
                Error::Other(
                    format!("service discovery listed service {uuid} at handle {handle} twice")
                        .into(),
                )
            })?;
            // The old identity is unavailable from this point even if handoff or
            // native service Close fails. Consumers must evict that identity now.
            peripheral.begin_graph_replacement(revision, previous_graph)?;
            let peer = peripheral.clone();
            let _device_guard = peripheral
                .shared
                .run_native_cleanup(move || {
                    let device = device_guard.as_mut().ok_or(Error::NotConnected)?;
                    install_and_retire_services(
                        device,
                        || peer.install_services(table),
                        BLEDevice::publish_services,
                        || peer.recover_retired_services(),
                        BLEDevice::close_retired_services,
                    )?;
                    Ok(device_guard)
                })
                .await?;
            peripheral.finish_graph_replacement(revision)
        })
        .await
    }

    /// Write some data to the characteristic. Returns an error if the write couldn't be send or (in
    /// the case of a write-with-response) if the device returns an error.
    async fn write(
        &self,
        characteristic: &Characteristic,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<()> {
        let expected = self.operation_graph()?;
        let peripheral = self.clone();
        let characteristic = characteristic.clone();
        let data = data.to_vec();
        self.own_reclaim_write(async move {
            let _lease = peripheral.shared.notification_lifecycle.read().await;
            let gatt = peripheral.gatt_characteristic(&characteristic, "write")?;
            let operation = {
                let graph = peripheral
                    .shared
                    .graph
                    .lock()
                    .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
                start_on_current_graph(&graph, &expected, || {
                    BLECharacteristic::start_write_value(&gatt, &data, write_type)
                })??
            };
            operation.await
        })
        .await
    }

    /// Enables either notify or indicate (depending on support) for the specified characteristic.
    /// This is a synchronous call. Non-ubm callers keep Indicate-if-possible.
    async fn subscribe(&self, characteristic: &Characteristic) -> Result<()> {
        self.subscribe_writing(characteristic, None).await
    }

    /// The first CCCD write is the selected mode. `None` stays Indicate-if-possible.
    async fn subscribe_selecting(
        &self,
        characteristic: &Characteristic,
        notify: Option<bool>,
    ) -> Result<()> {
        let configured = notify.map(|notify| {
            if notify {
                GattClientCharacteristicConfigurationDescriptorValue::Notify
            } else {
                GattClientCharacteristicConfigurationDescriptorValue::Indicate
            }
        });
        self.subscribe_writing(characteristic, configured).await
    }

    /// Disables either notify or indicate (depending on support) for the specified characteristic.
    /// This is a synchronous call.
    async fn unsubscribe(&self, characteristic: &Characteristic) -> Result<()> {
        let expected = match self.operation_graph() {
            Ok(expected) => expected,
            Err(error) => {
                let key = ordinary_cccd_key(characteristic);
                if self
                    .shared
                    .ordinary_cccd_local_cleanup
                    .lock()
                    .unwrap()
                    .contains(&key)
                {
                    return Err(Error::Other("WinRT local CCCD registration cleanup is pending; retry discovery to recover".into()));
                }
                if self
                    .shared
                    .ordinary_cccd_uncertain
                    .lock()
                    .unwrap()
                    .contains(&key)
                {
                    return Err(cccd_recovery_error(false));
                }
                return Err(error);
            }
        };
        let peripheral = self.clone();
        let characteristic = characteristic.clone();
        self.own_reclaim_write(async move {
            let _lease = peripheral.shared.notification_lifecycle.read().await;
            let _cccd = peripheral.shared.ordinary_cccd_lock.lock().await;
            let gatt = peripheral.gatt_characteristic(&characteristic, "unsubscribe")?;
            let started = {
                let graph = peripheral
                    .shared
                    .graph
                    .lock()
                    .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
                start_on_current_graph(&graph, &expected, || {
                    BLECharacteristic::start_client_configuration(
                        &gatt,
                        GattClientCharacteristicConfigurationDescriptorValue::None,
                    )
                })?
            };
            let outcome = match started {
                Ok(operation) => {
                    BLECharacteristic::finish_client_configuration(operation, "unsubscribe").await
                }
                Err(failure) => Err(failure),
            };
            if let Err(failure) = outcome {
                if !failure.proven_no_effect {
                    peripheral.note_remote_cccd_uncertainty(ordinary_cccd_key(&characteristic));
                }
                return Err(failure.error);
            }
            let peer = peripheral.clone();
            let route = characteristic.clone();
            if let Err(error) = peripheral
                .shared
                .run_native_cleanup(move || {
                    peer.with_characteristic_mut(&route, "unsubscribe", |ble_characteristic| {
                        ble_characteristic.take_registration()
                    })
                })
                .await
            {
                peripheral
                    .shared
                    .ordinary_cccd_uncertain
                    .lock()
                    .unwrap()
                    .insert(ordinary_cccd_key(&characteristic));
                peripheral
                    .shared
                    .ordinary_cccd_local_cleanup
                    .lock()
                    .unwrap()
                    .insert(ordinary_cccd_key(&characteristic));
                return Err(error);
            }
            peripheral
                .shared
                .ordinary_cccd_uncertain
                .lock()
                .unwrap()
                .remove(&ordinary_cccd_key(&characteristic));
            peripheral
                .shared
                .ordinary_cccd_local_cleanup
                .lock()
                .unwrap()
                .remove(&ordinary_cccd_key(&characteristic));
            peripheral.shared.notification_faults.clear(&characteristic);
            Ok(())
        })
        .await
    }

    async fn read(&self, characteristic: &Characteristic) -> Result<Vec<u8>> {
        let expected = self.operation_graph()?;
        let peripheral = self.clone();
        let characteristic = characteristic.clone();
        self.own_reclaim_operation(false, async move {
            let _lease = peripheral.shared.notification_lifecycle.read().await;
            let gatt = peripheral.gatt_characteristic(&characteristic, "read")?;
            let operation = {
                let graph = peripheral
                    .shared
                    .graph
                    .lock()
                    .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
                start_on_current_graph(&graph, &expected, || {
                    BLECharacteristic::start_read_value(&gatt)
                })??
            };
            operation.await
        })
        .await
    }

    async fn notifications(&self) -> Result<Pin<Box<dyn Stream<Item = ValueNotification> + Send>>> {
        Ok(retained_notifications_from_sender(
            &self.shared.notifications_channel,
            self.shared.notification_faults.clone(),
        ))
    }

    async fn notification_events(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = api::NotificationEvent> + Send>>> {
        Ok(retained_notification_events(
            &self.shared.notifications_channel,
            self.shared.notification_faults.clone(),
        ))
    }

    async fn write_descriptor(&self, descriptor: &Descriptor, data: &[u8]) -> Result<()> {
        let expected = self.operation_graph()?;
        let peripheral = self.clone();
        let descriptor = descriptor.clone();
        let data = data.to_vec();
        self.own_reclaim_write(async move {
            let _lease = peripheral.shared.notification_lifecycle.read().await;
            let gatt = peripheral.gatt_descriptor(&descriptor, "write")?;
            let operation = {
                let graph = peripheral
                    .shared
                    .graph
                    .lock()
                    .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
                start_on_current_graph(&graph, &expected, || {
                    BLEDescriptor::start_write_value(&gatt, &data)
                })??
            };
            operation.await
        })
        .await
    }

    async fn read_descriptor(&self, descriptor: &Descriptor) -> Result<Vec<u8>> {
        let expected = self.operation_graph()?;
        let peripheral = self.clone();
        let descriptor = descriptor.clone();
        self.own_reclaim_operation(false, async move {
            let _lease = peripheral.shared.notification_lifecycle.read().await;
            let gatt = peripheral.gatt_descriptor(&descriptor, "read")?;
            let operation = {
                let graph = peripheral
                    .shared
                    .graph
                    .lock()
                    .map_err(|_| Error::Other("WinRT graph owner lock poisoned".into()))?;
                start_on_current_graph(&graph, &expected, || {
                    BLEDescriptor::start_read_value(&gatt)
                })??
            };
            operation.await
        })
        .await
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
        let peripheral = self.clone();
        self.own_reclaim_operation(true, async move {
            let mut device = peripheral
                .shared
                .device
                .clone()
                .try_lock_owned()
                .map_err(|_| {
                    Error::Other(
                        "WinRT native device is busy; retry connection parameter request".into(),
                    )
                })?;
            let result =
                super::marker_executor::run_blocking_owned(move || match device.as_mut() {
                    Some(device) => device.request_connection_parameters(preset),
                    None => Err(Error::NotConnected),
                })
                .await;
            match result {
                Ok(result) => result,
                Err(error) => {
                    if error == "WinRT cleanup thread exited without a result" {
                        peripheral
                            .shared
                            .graph
                            .lock()
                            .expect("WinRT graph owner lock poisoned")
                            .native_operation_abandoned = true;
                    }
                    Err(Error::Other(error.into()))
                }
            }
        })
        .await
    }
}

impl From<BDAddr> for PeripheralId {
    fn from(address: BDAddr) -> Self {
        Self::with_address_type(address, None)
    }
}

#[cfg(test)]
mod peer_identity_tests {
    use super::PeripheralId;
    use crate::api::{AddressType, BDAddr};
    #[test]
    fn equal_address_bits_have_distinct_immutable_native_peer_identities() {
        let address: BDAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let public = PeripheralId::with_address_type(address, Some(AddressType::Public));
        let random = PeripheralId::with_address_type(address, Some(AddressType::Random));
        let unknown = PeripheralId::from(address);
        assert_ne!(public, random);
        assert_ne!(public, unknown);
        assert_ne!(random, unknown);
        for id in [public, random, unknown] {
            assert_eq!(id.to_string().parse::<PeripheralId>().unwrap(), id);
            assert_eq!(id.address(), address);
        }
        assert!("invalid:AA:BB:CC:DD:EE:FF".parse::<PeripheralId>().is_err());
    }
}

fn not_found(kind: &str, uuid: Uuid, instance: u64, operation: &str) -> Error {
    Error::NotSupported(format!(
        "{kind} {uuid} (instance {instance}) not found for {operation}"
    ))
}

/// Keep a typed platform error. Add the attribute's identity as metadata
/// instead of folding the error into a string.
fn annotate_attribute(kind: &str, uuid: Uuid, handle: u64, error: Error) -> Error {
    match error {
        Error::Platform(platform) => Error::Platform(
            platform
                .with("attribute", kind)
                .with("uuid", uuid.to_string())
                .with("handle", handle.to_string()),
        ),
        Error::WithCleanup { primary, cleanup } => Error::WithCleanup {
            primary: Box::new(annotate_attribute(kind, uuid, handle, *primary)),
            cleanup,
        },
        other => Error::Other(format!("{kind} {uuid} (handle {handle}): {other}").into()),
    }
}

fn restricted_service(
    uuid: Uuid,
    instance: u64,
    access: gatt_model::ServiceRestriction,
) -> BLEService {
    BLEService {
        uuid,
        instance,
        characteristics: HashMap::new(),
        access,
        included_services: None,
    }
}

/// UBM patch (`winrt-attribute-instances`, `winrt-uncached-discovery`): one
/// service with every characteristic and descriptor it lists, each kept as
/// its own instance. A failed query keeps its platform error and names the
/// service. A known OS-reserved UUID is recorded and not queried. An
/// ordinary AccessDenied with no ATT byte keeps the service identity.
async fn discover_service(device: &BLEDevice, service: GattDeviceService) -> Result<BLEService> {
    let uuid = utils::to_uuid(&service.Uuid()?);
    let instance = u64::from(service.AttributeHandle()?);
    let context = |error: Error| annotate_attribute("service", uuid, instance, error);
    if gatt_model::windows_reserves_service(uuid.as_u128()) {
        trace!("service {uuid} (handle {instance}) is reserved by Windows");
        return Ok(restricted_service(
            uuid,
            instance,
            gatt_model::ServiceRestriction::OsReserved,
        ));
    }
    let characteristics = match device
        .get_characteristics(&service)
        .await
        .map_err(context)?
    {
        CharacteristicList::AccessDenied => {
            trace!("service {uuid} (handle {instance}) denied characteristic discovery");
            return Ok(restricted_service(
                uuid,
                instance,
                gatt_model::ServiceRestriction::AccessDenied,
            ));
        }
        CharacteristicList::Ready(characteristics) => characteristics,
    };
    let characteristics = settle_discovery_queries(
        characteristics
            .into_iter()
            .map(|characteristic| discover_characteristic(device, characteristic)),
    )
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
        access: gatt_model::ServiceRestriction::Open,
        included_services: None,
    })
}

async fn discover_characteristic(
    device: &BLEDevice,
    characteristic: GattCharacteristic,
) -> Result<BLECharacteristic> {
    let uuid = utils::to_uuid(&characteristic.Uuid()?);
    let handle = characteristic.AttributeHandle()?;
    let context =
        |error: Error| annotate_attribute("characteristic", uuid, u64::from(handle), error);
    let descriptors = device
        .get_characteristic_descriptors(&characteristic)
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

#[cfg(test)]
mod source_connection_tests {
    #[test]
    fn address_alias_admission_is_order_independent_and_keeps_typed_peers_distinct() {
        use std::sync::{Arc, Mutex, Weak};
        let address = crate::api::BDAddr::from([1, 2, 3, 4, 5, 6]);
        let unknown = super::PeripheralId::with_address_type(address, None);
        let public =
            super::PeripheralId::with_address_type(address, Some(crate::api::AddressType::Public));
        let random =
            super::PeripheralId::with_address_type(address, Some(crate::api::AddressType::Random));
        // Simulate the authoritative result of FromBluetoothAddressAsync for
        // each possible native identity, in both admission orders.
        for (resolved, native_kind) in [
            (
                public.clone(),
                windows::Devices::Bluetooth::BluetoothAddressType::Public,
            ),
            (
                random.clone(),
                windows::Devices::Bluetooth::BluetoothAddressType::Random,
            ),
        ] {
            for unknown_first in [false, true] {
                let registry = super::PeerRegistry::default();
                let authorities = Arc::new(Mutex::new(Default::default()));
                assert!(
                    registry
                        .get(Weak::new(), unknown.clone(), authorities.clone())
                        .is_err()
                );
                let get = |id| registry.get(Weak::new(), id, authorities.clone()).unwrap();
                let (alias, typed) = if unknown_first {
                    (
                        get(
                            super::super::adapter::resolved_peer_identity(address, native_kind)
                                .unwrap(),
                        ),
                        get(resolved.clone()),
                    )
                } else {
                    let typed = get(resolved.clone());
                    (
                        get(
                            super::super::adapter::resolved_peer_identity(address, native_kind)
                                .unwrap(),
                        ),
                        typed,
                    )
                };
                assert!(Arc::ptr_eq(&alias.shared, &typed.shared));
                let other = get(if resolved == public {
                    random.clone()
                } else {
                    public.clone()
                });
                assert!(!Arc::ptr_eq(&alias.shared, &other.shared));
            }
        }
    }

    #[test]
    fn peer_registry_reuses_and_retains_uncertain_owner_across_caches() {
        use std::sync::{Arc, Mutex, Weak};
        let registry = super::PeerRegistry::default();
        let authorities = Arc::new(Mutex::new(Default::default()));
        let id = super::PeripheralId::with_address_type(
            crate::api::BDAddr::from([1, 2, 3, 4, 5, 6]),
            Some(crate::api::AddressType::Public),
        );
        let first = registry
            .get(Weak::new(), id.clone(), authorities.clone())
            .unwrap();
        let second = registry
            .get(Weak::new(), id.clone(), authorities.clone())
            .unwrap();
        assert!(Arc::ptr_eq(&first.shared, &second.shared));
        first.shared.graph.lock().unwrap().reclaim_operations = 1;
        drop(second);
        let second = registry
            .get(Weak::new(), id.clone(), authorities.clone())
            .unwrap();
        assert_eq!(second.shared.graph.lock().unwrap().reclaim_operations, 1);
        first.shared.graph.lock().unwrap().reclaim_operations = 0;
        first.shared.graph.lock().unwrap().reclaim_effect_uncertain = true;
        drop(first);
        drop(second);
        let again = registry.get(Weak::new(), id, authorities).unwrap();
        assert!(again.shared.graph.lock().unwrap().reclaim_effect_uncertain);
    }
    fn reclaim_fixture() -> (
        super::Peripheral,
        crate::ubm_reclaim_graph_binding::GraphBinding,
        crate::ubm_reclaim_authority::GraphKey,
    ) {
        use crate::ubm_reclaim_authority::{GraphKey, GraphTarget, ReclaimAuthorities};
        use std::sync::{Arc, Mutex, Weak};
        let authorities = Arc::new(Mutex::new(ReclaimAuthorities::default()));
        let peer = super::Peripheral::new_with_id(
            Weak::new(),
            super::PeripheralId::from(crate::api::BDAddr::from([1, 2, 3, 4, 5, 6])),
            authorities.clone(),
        );
        let graph_id = {
            let mut graph = peer.shared.graph.lock().unwrap();
            graph.ready = true;
            let id = graph.identity().unwrap().unwrap();
            graph.reclaim_binding.attach();
            assert!(graph.reclaim_binding.publish(id.clone()));
            id
        };
        peer.shared.source_connection.lock().unwrap().connected = true;
        let binding = peer
            .capture_reclaim_graph_binding("peer", "connection", &graph_id)
            .unwrap();
        let target = GraphTarget {
            peer_id: "peer".into(),
            connection_generation: "connection".into(),
            graph: graph_id.clone(),
        };
        let mut owner = authorities.lock().unwrap();
        owner.reserve_open(1).unwrap();
        owner.resolve_open(1, true).unwrap();
        owner
            .reserve_permit_token_bound(1, 1, target.clone(), &binding)
            .unwrap();
        owner.resolve_permit_token(1, 1, &target, true).unwrap();
        drop(owner);
        (
            peer,
            binding,
            GraphKey {
                authority_id: 1,
                peer_id: "peer".into(),
                connection_generation: "connection".into(),
                graph: graph_id,
                permit_id: 1,
            },
        )
    }

    #[test]
    fn reclaim_receipt_replay_requires_its_physical_owner() {
        use crate::ubm_reclaim_authority::{AuthorityError, PermitStatus};
        use std::sync::Weak;
        let (peer, binding, key) = reclaim_fixture();
        let other = super::Peripheral::new_with_id(
            Weak::new(),
            super::PeripheralId::from(crate::api::BDAddr::from([6, 5, 4, 3, 2, 1])),
            peer.shared.reclaim_authorities.clone(),
        );
        {
            let mut graph = other.shared.graph.lock().unwrap();
            graph.attachment = Some(key.graph.1);
            graph.revision = key.graph.2;
            graph.ready = true;
            graph.reclaim_binding.attach();
            assert!(graph.reclaim_binding.publish(key.graph.clone()));
        }
        other.shared.source_connection.lock().unwrap().connected = true;
        let other_binding = other
            .capture_reclaim_graph_binding("peer", "connection", &key.graph)
            .unwrap();
        assert_eq!(other.forget_reclaim(&key), Err(AuthorityError::Conflict));
        assert_eq!(
            peer.acquire_reclaim(&binding, key.clone()),
            Ok(PermitStatus::Acquired)
        );
        assert_eq!(
            other.acquire_reclaim(&binding, key.clone()),
            Err(AuthorityError::Conflict)
        );
        assert_eq!(
            other.acquire_reclaim(&other_binding, key.clone()),
            Err(AuthorityError::Conflict)
        );
        assert_eq!(other.commit_reclaim(&key), Err(AuthorityError::Conflict));
        assert_eq!(other.release_reclaim(&key), Err(AuthorityError::Conflict));
        assert_eq!(other.forget_reclaim(&key), Err(AuthorityError::Conflict));
        peer.shared.graph.lock().unwrap().invalidate();
        assert_eq!(
            peer.acquire_reclaim(&binding, key.clone()),
            Ok(PermitStatus::Acquired)
        );
        assert_eq!(peer.commit_reclaim(&key), Ok(PermitStatus::Committed));
        assert_eq!(other.commit_reclaim(&key), Err(AuthorityError::Conflict));
        assert_eq!(peer.release_reclaim(&key), Ok(PermitStatus::Released));
        assert_eq!(other.release_reclaim(&key), Err(AuthorityError::Conflict));
        assert_eq!(other.forget_reclaim(&key), Err(AuthorityError::Conflict));
        peer.forget_reclaim(&key).unwrap();
        // A lost forget response replays only for the source that owned it.
        assert_eq!(other.forget_reclaim(&key), Err(AuthorityError::Conflict));
        peer.forget_reclaim(&key).unwrap();
        let handle = crate::ubm_reclaim_authority::ReclaimAuthorityHandle::from_owner(
            peer.shared.reclaim_authorities.clone(),
        );
        assert_eq!(
            handle.close(1, key.authority_id),
            Err(AuthorityError::StillOwned)
        );
        assert_eq!(
            handle.ack_permit_retirement(&key, &other_binding),
            Err(AuthorityError::Conflict)
        );
        assert_eq!(
            handle.ack_permit_retirement(&key, &binding),
            Ok(crate::ubm_reclaim_authority::RetirementAck::Acknowledged)
        );
        // The first acknowledgement may have reached the source despite a lost reply.
        assert_eq!(
            handle.ack_permit_retirement(&key, &binding),
            Ok(crate::ubm_reclaim_authority::RetirementAck::AlreadyRetiredNoEffect)
        );
        assert_eq!(peer.forget_reclaim(&key), Err(AuthorityError::Unknown));
        handle.close(1, key.authority_id).unwrap();
        handle.forget_closed(1, key.authority_id).unwrap();
        handle.forget_closed(1, key.authority_id).unwrap();
        assert_eq!(
            handle.ack_open_retirement(1, 1),
            Ok(crate::ubm_reclaim_authority::RetirementAck::Acknowledged)
        );
    }

    #[test]
    fn reclaim_replays_and_precommit_invalidation_fences_callbacks() {
        use crate::ubm_reclaim_authority::PermitStatus;
        use std::sync::{Arc, Mutex};
        let (peer, binding, key) = reclaim_fixture();
        assert_eq!(
            peer.acquire_reclaim(&binding, key.clone()),
            Ok(PermitStatus::Acquired)
        );
        assert_eq!(
            peer.acquire_reclaim(&binding, key.clone()),
            Ok(PermitStatus::Acquired)
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        for value in [1, 2] {
            let events = events.clone();
            peer.shared
                .admit_reclaim_callback(1, value == 2, move || events.lock().unwrap().push(value));
        }
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(peer.commit_reclaim(&key), Ok(PermitStatus::Invalidated));
        assert_eq!(peer.release_reclaim(&key), Ok(PermitStatus::Released));
        assert_eq!(*events.lock().unwrap(), vec![1, 2]);
        assert_eq!(peer.release_reclaim(&key), Ok(PermitStatus::Released));
        peer.forget_reclaim(&key).unwrap();
    }

    #[test]
    fn source_barrier_waits_for_queued_notification() {
        use crate::api::ValueNotification;
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
        use std::sync::mpsc;
        let (peer, _, _) = reclaim_fixture();
        peer.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peer.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        peer.shared
            .notification_faults
            .start_source_activation(1, route, 10)
            .unwrap();
        let source = Mutex::new(Some(PreparedActivation {
            generation: 1,
            id: 10,
            claims: 1,
        }));
        let member = peer
            .register_notification_source_route_for_activation(&group, route, 1, 10)
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let shared = peer.shared.clone();
        let worker = std::thread::spawn(move || {
            shared.admit_reclaim_callback(0, false, move || {
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        let value = ValueNotification {
            uuid: uuid::Uuid::nil(),
            instance: 2,
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            value: vec![7],
            source_failure: None,
            lost_before: 0,
        };
        assert!(peer.shared.admit_managed_source_callback(
            &source,
            route,
            &value,
            1,
            false,
            Box::new(|| {})
        ));
        peer.notification_source_barrier(&group, 7).unwrap();
        let before_drain = consumer.poll();
        resume_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(
            before_drain,
            Poll::Empty,
            "barrier passed a callback accepted by the WinRT queue"
        );
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == member && value.value == [7])
        );
        assert_eq!(
            consumer.poll(),
            Poll::Record(IngressRecord::Barrier { id: 7 })
        );
    }

    #[test]
    fn source_join_excludes_already_queued_notification() {
        use crate::api::ValueNotification;
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
        use std::sync::mpsc;
        let (peer, _, _) = reclaim_fixture();
        peer.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peer.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        peer.shared
            .notification_faults
            .start_source_activation(1, route, 10)
            .unwrap();
        peer.shared
            .notification_faults
            .start_source_activation(1, route, 11)
            .unwrap();
        let source = Mutex::new(Some(PreparedActivation {
            generation: 1,
            id: 10,
            claims: 1,
        }));
        let existing = peer
            .register_notification_source_route_for_activation(&group, route, 1, 10)
            .unwrap();
        peer.register_notification_source_route_for_activation(&group, route, 3, 11)
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let shared = peer.shared.clone();
        let worker = std::thread::spawn(move || {
            shared.admit_reclaim_callback(0, false, move || {
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        let value = ValueNotification {
            uuid: uuid::Uuid::nil(),
            instance: 2,
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            value: vec![7],
            source_failure: None,
            lost_before: 0,
        };
        assert!(peer.shared.admit_managed_source_callback(
            &source,
            route,
            &value,
            1,
            false,
            Box::new(|| {})
        ));
        peer.register_notification_source_route_for_activation(&group, route, 2, 10)
            .unwrap();
        resume_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == existing && value.value == [7])
        );
        assert_eq!(
            consumer.poll(),
            Poll::Empty,
            "late member received a notification accepted before it joined"
        );
    }

    #[test]
    fn retired_route_excludes_callbacks_admitted_after_retirement() {
        use crate::api::ValueNotification;
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
        use std::sync::mpsc;
        let (peer, _, _) = reclaim_fixture();
        peer.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peer.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        peer.shared
            .notification_faults
            .start_source_activation(1, route, 10)
            .unwrap();
        let source = Mutex::new(Some(PreparedActivation {
            generation: 1,
            id: 10,
            claims: 1,
        }));
        let member = peer
            .register_notification_source_route_for_activation(&group, route, 1, 10)
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let shared = peer.shared.clone();
        let worker = std::thread::spawn(move || {
            shared.admit_reclaim_callback(0, false, move || {
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        let value = ValueNotification {
            uuid: uuid::Uuid::nil(),
            instance: 2,
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            value: vec![7],
            source_failure: None,
            lost_before: 0,
        };
        assert!(peer.shared.admit_managed_source_callback(
            &source,
            route,
            &value,
            1,
            false,
            Box::new(|| {})
        ));
        peer.retire_notification_source_route(&group, route, member)
            .unwrap();
        let mut after = value.clone();
        after.lost_before = 2;
        assert!(peer.shared.admit_managed_source_callback(
            &source,
            route,
            &after,
            1,
            false,
            Box::new(|| {})
        ));
        assert_eq!(consumer.poll(), Poll::Empty);
        resume_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == member && value.value == [7])
        );
        assert_eq!(
            consumer.poll(),
            Poll::Record(IngressRecord::Fence { membership: member })
        );
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Loss { skipped_records: 2, memberships }) if memberships.is_empty())
        );
        assert_eq!(consumer.poll(), Poll::Empty);
    }

    #[test]
    fn queued_retirement_fences_only_the_exact_membership_and_activation() {
        use crate::api::ValueNotification;
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
        use std::sync::mpsc;
        let (peer, _, _) = reclaim_fixture();
        peer.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peer.open_notification_source_group(1, 16, 128).unwrap();
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        peer.shared
            .notification_faults
            .start_source_activation(1, route, 10)
            .unwrap();
        peer.shared
            .notification_faults
            .start_source_activation(1, route, 11)
            .unwrap();
        let original = Mutex::new(Some(PreparedActivation {
            generation: 1,
            id: 10,
            claims: 1,
        }));
        let replacement = Mutex::new(Some(PreparedActivation {
            generation: 1,
            id: 11,
            claims: 1,
        }));
        let retired = peer
            .register_notification_source_route_for_activation(&group, route, 1, 10)
            .unwrap();
        let survivor = peer
            .register_notification_source_route_for_activation(&group, route, 2, 10)
            .unwrap();
        let other_activation = peer
            .register_notification_source_route_for_activation(&group, route, 3, 11)
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let shared = peer.shared.clone();
        let worker = std::thread::spawn(move || {
            shared.admit_reclaim_callback(0, false, move || {
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        let mut value = ValueNotification {
            uuid: uuid::Uuid::nil(),
            instance: 2,
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            value: vec![7],
            source_failure: None,
            lost_before: 0,
        };
        assert!(peer.shared.admit_managed_source_callback(
            &original,
            route,
            &value,
            1,
            false,
            Box::new(|| {})
        ));
        peer.retire_notification_source_route(&group, route, retired)
            .unwrap();
        value.value = vec![8];
        assert!(peer.shared.admit_managed_source_callback(
            &original,
            route,
            &value,
            1,
            false,
            Box::new(|| {})
        ));
        value.value = vec![9];
        assert!(peer.shared.admit_managed_source_callback(
            &replacement,
            route,
            &value,
            1,
            false,
            Box::new(|| {})
        ));
        assert_eq!(consumer.poll(), Poll::Empty);
        resume_tx.send(()).unwrap();
        worker.join().unwrap();
        for membership in [retired, survivor] {
            assert!(
                matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership: found, value, .. }) if found == membership && value.value == [7])
            );
        }
        assert_eq!(
            consumer.poll(),
            Poll::Record(IngressRecord::Fence {
                membership: retired
            })
        );
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == survivor && value.value == [8])
        );
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == other_activation && value.value == [9])
        );
        assert_eq!(consumer.poll(), Poll::Empty);
    }

    #[test]
    fn earlier_queued_source_failure_precedes_route_retirement() {
        use crate::ubm_peer_notification_ingress::{
            IngressRecord, NotificationRoute, Poll, TerminalReason,
        };
        use std::sync::mpsc;
        let (peer, _, _) = reclaim_fixture();
        peer.start_notification_source_generation(1).unwrap();
        peer.shared
            .source_connection
            .lock()
            .unwrap()
            .source_generation = Some(1);
        let (group, consumer) = peer.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        let member = peer
            .register_notification_source_route(&group, route, 1)
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let shared = peer.shared.clone();
        let worker = std::thread::spawn(move || {
            shared.admit_reclaim_callback(0, false, move || {
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        peer.shared.queue_notification_source_failure(
            1,
            crate::PlatformError::new("winrt", "earlier-failure", "original failure"),
        );
        peer.retire_notification_source_route(&group, route, member)
            .unwrap();
        assert_eq!(consumer.poll(), Poll::Empty);
        resume_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, .. }, memberships }) if error.code == "earlier-failure" && memberships == vec![(route, member)])
        );
        assert_eq!(consumer.poll(), Poll::Empty);
    }

    #[tokio::test]
    async fn unrelated_retirement_waits_for_actual_peer_overflow_drain() {
        use crate::api::{NotificationEvent, ValueNotification};
        use crate::ubm_peer_notification_ingress::{
            IngressRecord, NotificationRoute, Poll, TerminalReason,
        };
        use std::sync::{Arc, mpsc};
        let (peer, _, _) = reclaim_fixture();
        let shared = peer.shared.clone();
        shared
            .notification_faults
            .start_source_generation(1)
            .unwrap();
        shared.source_connection.lock().unwrap().source_generation = Some(1);
        let (group, consumer) = shared
            .notification_faults
            .open_source_group(1, 8, 64)
            .unwrap();
        let route = |instance| NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: instance,
        };
        shared
            .notification_faults
            .register_source_route(&group, route(1), 1)
            .unwrap();
        let member = shared
            .notification_faults
            .register_source_route(&group, route(2), 2)
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let drainer = shared.clone();
        let worker = std::thread::spawn(move || {
            drainer.admit_reclaim_callback(0, false, move || {
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        let faults = shared.notification_faults.clone();
        assert!(shared.admit_reclaim_callback(0, false, move || {
            faults
                .publish_source_event(
                    1,
                    NotificationEvent::Value(ValueNotification {
                        uuid: uuid::Uuid::nil(),
                        instance: 2,
                        service_uuid: uuid::Uuid::nil(),
                        service_instance: 1,
                        value: vec![7],
                        source_failure: None,
                        lost_before: 0,
                    }),
                )
                .unwrap();
        }));
        assert!(!shared.admit_reclaim_callback(524_289, false, || {}));
        let a = Arc::new(super::super::callback_retirement::CallbackRetirement::new());
        let retirement = {
            let shared = shared.clone();
            tokio::spawn(async move { shared.retire_managed_callbacks(&a).await })
        };
        tokio::task::yield_now().await;
        assert!(!retirement.is_finished());
        assert_eq!(consumer.poll(), Poll::Empty);
        resume_tx.send(()).unwrap();
        worker.join().unwrap();
        retirement.await.unwrap().unwrap();
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == member && value.value == [7])
        );
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, tail_uncertain: true, .. }, .. }) if error.code == "reclaim-callback-overflow")
        );
        assert_eq!(consumer.poll(), Poll::Finished);
    }

    #[test]
    fn services_changed_preserves_held_broadcast_overflow_for_existing_receiver() {
        use futures::{FutureExt, StreamExt};
        use std::sync::{Arc, atomic::AtomicBool};
        let (peer, _, _) = reclaim_fixture();
        let shared = peer.shared.clone();
        let owner = Arc::new(AtomicBool::new(false));
        shared.source_connection.lock().unwrap().owner = Some(owner.clone());
        let (sender, _) = tokio::sync::broadcast::channel(crate::ubm::EVENT_CAPACITY * 2 + 2);
        let mut events = crate::common::util::retained_notification_events(
            &sender,
            shared.notification_faults.clone(),
        );
        let hold = shared.notification_faults.hold_notification_delivery();
        for instance in 0..=crate::ubm::EVENT_CAPACITY {
            shared.notification_faults.publish(
                &sender,
                crate::api::ValueNotification {
                    uuid: uuid::Uuid::nil(),
                    instance: instance as u64,
                    service_uuid: uuid::Uuid::nil(),
                    service_instance: 1,
                    value: vec![7],
                    source_failure: None,
                    lost_before: 0,
                },
            );
        }
        let callback_shared = shared.clone();
        let identity = shared.identity.clone();
        assert!(shared.admit_reclaim_callback(0, true, move || {
            callback_shared.apply_services_changed(&owner, identity)
        }));
        let mut replacement = crate::common::util::retained_notification_events(
            &sender,
            shared.notification_faults.clone(),
        );
        drop(hold);
        for instance in 0..crate::ubm::EVENT_CAPACITY {
            assert!(
                matches!(events.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.instance == instance as u64 && value.source_failure.is_none())
            );
        }
        assert!(
            matches!(events.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.source_failure.as_ref().is_some_and(|error| error.code == "notification-broadcast-overflow"))
        );
        assert!(events.next().now_or_never().unwrap().is_none());
        assert!(replacement.next().now_or_never().is_none());
    }

    #[test]
    fn full_callback_queue_preserves_the_original_peer_failure() {
        use crate::ubm_peer_notification_ingress::{
            IngressRecord, NotificationRoute, Poll, TerminalReason,
        };
        let (peer, _, _) = reclaim_fixture();
        let shared = peer.shared.clone();
        shared
            .notification_faults
            .start_source_generation(1)
            .unwrap();
        shared.source_connection.lock().unwrap().source_generation = Some(1);
        let (group, consumer) = shared
            .notification_faults
            .open_source_group(1, 8, 64)
            .unwrap();
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        let member = shared
            .notification_faults
            .register_source_route(&group, route, 2)
            .unwrap();
        shared.graph.lock().unwrap().reclaim_gate =
            crate::ubm_reclaim_graph_gate::GraphGate::new(0, 0);

        shared.queue_notification_source_failure(
            1,
            crate::PlatformError::new(
                "winrt",
                "notification-retirement-failed",
                "original removal failure",
            ),
        );

        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::SourceFailure { error, .. }, memberships
        }) if error.code == "notification-retirement-failed"
            && error.message == "original removal failure"
            && memberships == vec![(route, member)])
        );
    }

    #[test]
    fn saturated_retirement_failure_includes_group_admitted_before_failure() {
        use crate::ubm_peer_notification_ingress::{
            IngressRecord, NotificationRoute, Poll, TerminalReason,
        };
        use crate::ubm_reclaim_authority::GraphKey;

        let (peer, _, _) = reclaim_fixture();
        let shared = peer.shared.clone();
        let faults = &shared.notification_faults;
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        faults.start_source_generation(1).unwrap();
        faults.start_source_activation(1, route, 10).unwrap();
        shared.source_connection.lock().unwrap().source_generation = Some(1);
        let (first, first_consumer) = faults.open_source_group(1, 8, 64).unwrap();
        let first_member = faults
            .register_source_route_for_activation(&first, route, 1, 10)
            .unwrap();

        let key = GraphKey {
            authority_id: 0,
            peer_id: String::new(),
            connection_generation: String::new(),
            graph: (String::new(), 0, 0),
            permit_id: 9,
        };
        {
            let mut graph = shared.graph.lock().unwrap();
            graph.reclaim_gate = crate::ubm_reclaim_graph_gate::GraphGate::new(1, 4096);
            graph.reclaim_gate.acquire(key.clone()).unwrap();
            graph.reclaim_gate.commit(&key).unwrap();
        }
        let value = crate::api::ValueNotification {
            uuid: uuid::Uuid::nil(),
            instance: 2,
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            value: vec![7],
            source_failure: None,
            lost_before: 0,
        };
        let prefix = faults
            .reserve_source_value_for_activation_bounded(1, route, 10, &value, usize::MAX)
            .unwrap();
        assert!(shared.admit_reclaim_callback(0, false, move || prefix.release()));

        // A second owner joins after the prefix was captured and before failure.
        let (second, second_consumer) = faults.open_source_group(1, 8, 64).unwrap();
        let second_member = faults
            .register_source_route_for_activation(&second, route, 2, 10)
            .unwrap();
        let error = crate::PlatformError::new("winrt", "hresult", "original disable failure")
            .with("hresult", "0x80004005");
        shared.queue_notification_source_failure(1, error.clone());
        assert_eq!(first_consumer.poll(), Poll::Empty);
        assert_eq!(second_consumer.poll(), Poll::Empty);
        shared
            .graph
            .lock()
            .unwrap()
            .reclaim_gate
            .release(&key)
            .unwrap();
        shared.drain_reclaim_callbacks();

        assert!(
            matches!(first_consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == first_member && value.value == [7])
        );
        assert!(
            matches!(first_consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, memberships }) if found == error && memberships == vec![(route, first_member)])
        );
        assert!(
            matches!(second_consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, memberships }) if found == error && memberships == vec![(route, second_member)])
        );
    }

    #[test]
    fn stale_retirement_failure_does_not_fail_replacement_source() {
        use crate::ubm_peer_notification_ingress::{NotificationRoute, Poll};
        let (peer, _, _) = reclaim_fixture();
        let shared = peer.shared.clone();
        shared
            .notification_faults
            .start_source_generation(1)
            .unwrap();
        shared
            .notification_faults
            .close_source_generation(1)
            .unwrap();
        shared
            .notification_faults
            .start_source_generation(2)
            .unwrap();
        shared.source_connection.lock().unwrap().source_generation = Some(2);
        let (group, consumer) = shared
            .notification_faults
            .open_source_group(2, 8, 64)
            .unwrap();
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        shared
            .notification_faults
            .register_source_route(&group, route, 2)
            .unwrap();
        shared.graph.lock().unwrap().reclaim_gate =
            crate::ubm_reclaim_graph_gate::GraphGate::new(0, 0);

        shared.queue_notification_source_failure(
            1,
            crate::PlatformError::new("winrt", "hresult", "old failure")
                .with("hresult", "0x80004005"),
        );

        assert_eq!(consumer.poll(), Poll::Empty);
        assert_eq!(
            shared.source_connection.lock().unwrap().source_generation,
            Some(2)
        );
    }

    #[test]
    fn saturated_failure_cut_keeps_old_prefix_and_does_not_fence_replacement() {
        use crate::ubm_peer_notification_ingress::{
            IngressRecord, NotificationRoute, Poll, TerminalReason,
        };
        use crate::ubm_reclaim_authority::GraphKey;
        let (peer, _, _) = reclaim_fixture();
        let shared = peer.shared.clone();
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        shared
            .notification_faults
            .start_source_generation(1)
            .unwrap();
        shared
            .notification_faults
            .start_source_activation(1, route, 10)
            .unwrap();
        shared.source_connection.lock().unwrap().source_generation = Some(1);
        let (old, consumer) = shared
            .notification_faults
            .open_source_group(1, 8, 64)
            .unwrap();
        let member = shared
            .notification_faults
            .register_source_route_for_activation(&old, route, 2, 10)
            .unwrap();
        let value = crate::api::ValueNotification {
            uuid: uuid::Uuid::nil(),
            instance: 2,
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            value: vec![7],
            source_failure: None,
            lost_before: 0,
        };
        let prefix = shared
            .notification_faults
            .reserve_source_value_for_activation_bounded(1, route, 10, &value, usize::MAX)
            .unwrap();
        shared
            .notification_faults
            .source_group_barrier(&old, 9)
            .unwrap();
        let key = GraphKey {
            authority_id: 0,
            peer_id: String::new(),
            connection_generation: String::new(),
            graph: (String::new(), 0, 0),
            permit_id: 9,
        };
        {
            let mut graph = shared.graph.lock().unwrap();
            graph.reclaim_gate = crate::ubm_reclaim_graph_gate::GraphGate::new(1, 1024);
            graph.reclaim_gate.acquire(key.clone()).unwrap();
            graph.reclaim_gate.commit(&key).unwrap();
        }
        assert!(shared.admit_reclaim_callback(0, false, move || prefix.release()));
        let error = crate::PlatformError::new("winrt", "hresult", "original failure")
            .with("hresult", "0x80004005");
        shared.queue_notification_source_failure(1, error.clone());
        shared
            .notification_faults
            .close_source_generation(1)
            .unwrap();
        shared
            .notification_faults
            .start_source_generation(2)
            .unwrap();
        shared.source_connection.lock().unwrap().source_generation = Some(2);
        let (_new, replacement) = shared
            .notification_faults
            .open_source_group(2, 8, 64)
            .unwrap();
        shared
            .graph
            .lock()
            .unwrap()
            .reclaim_gate
            .release(&key)
            .unwrap();
        shared.drain_reclaim_callbacks();
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == member && value.value == [7])
        );
        assert_eq!(
            consumer.poll(),
            Poll::Record(IngressRecord::Barrier { id: 9 })
        );
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, .. }) if found == error)
        );
        assert_eq!(replacement.poll(), Poll::Empty);
        assert_eq!(
            shared.source_connection.lock().unwrap().source_generation,
            Some(2)
        );
    }

    #[test]
    fn exhausted_failure_reservation_budget_preserves_the_original_peer_failure() {
        use crate::ubm_peer_notification_ingress::{
            IngressRecord, NotificationRoute, Poll, TerminalReason,
        };
        let (peer, _, _) = reclaim_fixture();
        let shared = peer.shared.clone();
        shared
            .notification_faults
            .start_source_generation(1)
            .unwrap();
        shared.source_connection.lock().unwrap().source_generation = Some(1);
        let (group, consumer) = shared
            .notification_faults
            .open_source_group(1, 8, 64)
            .unwrap();
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        let member = shared
            .notification_faults
            .register_source_route(&group, route, 2)
            .unwrap();
        let error = crate::PlatformError::new(
            "winrt",
            "notification-retirement-failed",
            "original disable failure",
        );
        let callback_bytes = std::mem::size_of::<Box<dyn FnOnce() + Send>>()
            + std::mem::size_of::<crate::common::util::SourceValueReservation>()
            + std::mem::size_of::<std::sync::Weak<super::Shared>>()
            + std::mem::size_of::<u64>()
            + std::mem::size_of::<crate::PlatformError>()
            + error.code.len()
            + error.message.len();
        shared.graph.lock().unwrap().reclaim_gate =
            crate::ubm_reclaim_graph_gate::GraphGate::new(1, callback_bytes);

        shared.queue_notification_source_failure(1, error);

        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::SourceFailure { error, .. }, memberships
        }) if error.code == "notification-retirement-failed"
            && error.message == "original disable failure"
            && memberships == vec![(route, member)])
        );
    }

    #[test]
    fn retirement_failure_follows_other_characteristic_callbacks() {
        use crate::api::{NotificationEvent, ValueNotification};
        use crate::ubm_peer_notification_ingress::{
            IngressRecord, NotificationRoute, Poll, TerminalReason,
        };
        use std::sync::mpsc;
        let (peer, _, _) = reclaim_fixture();
        let shared = peer.shared.clone();
        shared
            .notification_faults
            .start_source_generation(1)
            .unwrap();
        shared.source_connection.lock().unwrap().source_generation = Some(1);
        let (group, consumer) = shared
            .notification_faults
            .open_source_group(1, 8, 64)
            .unwrap();
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        let member = shared
            .notification_faults
            .register_source_route(&group, route, 2)
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let drainer = shared.clone();
        let worker = std::thread::spawn(move || {
            drainer.admit_reclaim_callback(0, false, move || {
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        let faults = shared.notification_faults.clone();
        assert!(shared.admit_reclaim_callback(0, false, move || {
            faults
                .publish_source_event(
                    1,
                    NotificationEvent::Value(ValueNotification {
                        uuid: uuid::Uuid::nil(),
                        instance: 2,
                        service_uuid: uuid::Uuid::nil(),
                        service_instance: 1,
                        value: vec![7],
                        source_failure: None,
                        lost_before: 0,
                    }),
                )
                .unwrap()
        }));
        shared.queue_notification_source_failure(
            1,
            crate::PlatformError::new("winrt", "notification-retirement-failed", "disable failed"),
        );
        assert_eq!(consumer.poll(), Poll::Empty);
        resume_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == member && value.value == [7])
        );
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, .. }, .. }) if error.code == "notification-retirement-failed")
        );
    }

    #[test]
    fn reclaim_callback_budget_includes_error_allocations() {
        let error = crate::PlatformError::new("winrt", "code", "message")
            .with("detail", "x".repeat(524_288));
        let notification = crate::api::ValueNotification {
            uuid: uuid::Uuid::nil(),
            instance: 0,
            service_uuid: uuid::Uuid::nil(),
            service_instance: 0,
            value: Vec::new(),
            source_failure: Some(error),
            lost_before: 0,
        };
        assert!(super::notification_retained_bytes(&notification) > 524_288);
        let (peer, binding, key) = reclaim_fixture();
        peer.acquire_reclaim(&binding, key.clone()).unwrap();
        peer.commit_reclaim(&key).unwrap();
        let bytes = super::notification_retained_bytes(&notification);
        peer.shared
            .admit_reclaim_callback(bytes, true, move || drop(notification));
        peer.release_reclaim(&key).unwrap();
        assert!(peer.shared.graph.lock().unwrap().reclaim_callback_uncertain);
    }

    #[tokio::test]
    async fn graph_retirement_reports_only_the_replaced_accepted_identity() {
        use futures::{FutureExt, StreamExt};
        use std::sync::{Arc, Mutex};
        let manager = Arc::new(crate::common::adapter_manager::AdapterManager::<
            super::Peripheral,
        >::default());
        let mut events = manager.event_stream();
        let peer = super::Peripheral::new_with_id(
            Arc::downgrade(&manager),
            super::PeripheralId::from(crate::api::BDAddr::from([1, 2, 3, 4, 5, 6])),
            Arc::new(Mutex::new(
                crate::ubm_reclaim_authority::ReclaimAuthorities::default(),
            )),
        );
        let old = ("winrt".to_owned(), 7, 1);
        let new = ("winrt".to_owned(), 7, 2);
        peer.shared.emit_retired_graph(None, Some(old.clone()));
        peer.shared
            .emit_retired_graph(Some(old.clone()), Some(old.clone()));
        assert!(events.next().now_or_never().is_none());
        peer.shared
            .emit_retired_graph(Some(old.clone()), Some(new.clone()));
        assert!(
            matches!(events.next().await, Some(crate::api::CentralEvent::DeviceGraphRetired { id, graph }) if id == peer.shared.identity && graph == old)
        );
        assert!(events.next().now_or_never().is_none());
    }

    #[tokio::test]
    async fn reclaim_overflow_reports_uncertain_graph_and_retains_physical_owner() {
        use futures::StreamExt;
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        let manager = Arc::new(crate::common::adapter_manager::AdapterManager::<
            super::Peripheral,
        >::default());
        let mut events = manager.event_stream();
        let peer = super::Peripheral::new_with_id(
            Arc::downgrade(&manager),
            super::PeripheralId::from(crate::api::BDAddr::from([1, 2, 3, 4, 5, 6])),
            Arc::new(Mutex::new(
                crate::ubm_reclaim_authority::ReclaimAuthorities::default(),
            )),
        );
        let owner = Arc::new(AtomicBool::new(false));
        {
            let mut source = peer.shared.source_connection.lock().unwrap();
            source.owner = Some(owner.clone());
            source.connected = true;
        }
        let expected_graph = {
            let mut graph = peer.shared.graph.lock().unwrap();
            graph.ready = true;
            let identity = graph.identity().unwrap().unwrap();
            graph.reclaim_binding.attach();
            assert!(graph.reclaim_binding.publish(identity.clone()));
            identity
        };
        peer.shared.connected.store(true, Ordering::Relaxed);
        let binding = peer
            .capture_reclaim_graph_binding("peer", "connection", &expected_graph)
            .unwrap();
        let target = crate::ubm_reclaim_authority::GraphTarget {
            peer_id: "peer".into(),
            connection_generation: "connection".into(),
            graph: expected_graph.clone(),
        };
        {
            let mut authority = peer.shared.reclaim_authorities.lock().unwrap();
            authority.reserve_open(1).unwrap();
            authority.resolve_open(1, true).unwrap();
            authority
                .reserve_permit_token_bound(1, 1, target.clone(), &binding)
                .unwrap();
            authority.resolve_permit_token(1, 1, &target, true).unwrap();
        }
        let key = crate::ubm_reclaim_authority::GraphKey {
            authority_id: 1,
            peer_id: "peer".into(),
            connection_generation: "connection".into(),
            graph: expected_graph.clone(),
            permit_id: 1,
        };
        peer.acquire_reclaim(&binding, key.clone()).unwrap();
        peer.commit_reclaim(&key).unwrap();
        let weak = Arc::downgrade(&peer.shared);
        peer.shared.admit_reclaim_callback(0, true, move || {
            if let Some(shared) = weak.upgrade() {
                shared.graph.lock().unwrap().invalidate();
            }
        });
        peer.shared.admit_reclaim_callback(524_289, false, || {});
        peer.release_reclaim(&key).unwrap();
        assert!(
            matches!(events.next().await, Some(crate::api::CentralEvent::DeviceSourceUncertain { id, graph: Some(graph), .. }) if id == peer.shared.identity && graph == expected_graph)
        );
        peer.shared.admit_reclaim_callback(524_289, false, || {});
        assert!(
            matches!(events.next().await, Some(crate::api::CentralEvent::DeviceSourceUncertain { id, graph: None, .. }) if id == peer.shared.identity)
        );
        assert!(peer.shared.source_connection.lock().unwrap().connected);
        assert!(peer.shared.connected.load(Ordering::Relaxed));
        assert!(
            peer.shared
                .source_connection
                .lock()
                .unwrap()
                .owner
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &owner))
        );
        assert!(!owner.load(Ordering::Relaxed));
        let graph = peer.shared.graph.lock().unwrap();
        assert!(graph.reclaim_callback_uncertain);
        assert!(graph.identity().unwrap().is_none());
    }

    #[test]
    fn stale_overflow_fence_cannot_cover_a_later_dropped_callback() {
        let (peer, _, _) = reclaim_fixture();
        let mut graph = peer.shared.graph.lock().unwrap();
        graph.note_callback_overflow();
        let first_epoch = graph.reclaim_overflow_epoch;
        graph.note_callback_overflow();
        graph.finish_callback_fence(first_epoch, true);
        assert!(graph.reclaim_callback_uncertain);
        assert!(!graph.reclaim_callback_recoverable);
        graph.reclaim_overflow_epoch = Some(u64::MAX);
        graph.note_callback_overflow();
        assert!(graph.reclaim_overflow_epoch.is_none());
        graph.finish_callback_fence(None, true);
        assert!(!graph.reclaim_callback_recoverable);
    }

    #[test]
    fn fresh_discovery_clears_only_fenced_callback_uncertainty_after_drain() {
        let (peer, binding, key) = reclaim_fixture();
        peer.acquire_reclaim(&binding, key.clone()).unwrap();
        peer.commit_reclaim(&key).unwrap();
        peer.shared.admit_reclaim_callback(524_289, false, || {});
        {
            let mut graph = peer.shared.graph.lock().unwrap();
            graph.reclaim_effect_uncertain = true;
            let attachment = graph.attachment.unwrap();
            let revision = graph.revision;
            graph.publish_discovery(attachment, revision);
            assert!(graph.reclaim_callback_uncertain);
        }
        peer.release_reclaim(&key).unwrap();
        let mut graph = peer.shared.graph.lock().unwrap();
        assert!(graph.reclaim_callback_recoverable);
        let attachment = graph.attachment.unwrap();
        let revision = graph.revision;
        graph.publish_discovery(attachment, revision);
        assert!(!graph.reclaim_callback_uncertain);
        assert!(graph.reclaim_effect_uncertain);
    }

    #[test]
    fn reclaim_rejects_stale_binding_and_overflow_fails_closed() {
        use crate::ubm_reclaim_authority::PermitStatus;
        let (stale_peer, stale_binding, stale_key) = reclaim_fixture();
        stale_peer.shared.graph.lock().unwrap().invalidate();
        assert_eq!(
            stale_peer.acquire_reclaim(&stale_binding, stale_key),
            Ok(PermitStatus::Stale)
        );
        let (peer, binding, key) = reclaim_fixture();
        assert_eq!(
            peer.acquire_reclaim(&binding, key.clone()),
            Ok(PermitStatus::Acquired)
        );
        assert_eq!(peer.commit_reclaim(&key), Ok(PermitStatus::Committed));
        for _ in 0..257 {
            peer.shared.admit_reclaim_callback(1, false, || {});
        }
        assert_eq!(peer.release_reclaim(&key), Ok(PermitStatus::Released));
        assert!(peer.shared.graph.lock().unwrap().reclaim_callback_uncertain);
        assert!(peer.shared.source_connection.lock().unwrap().connected);
        assert!(
            peer.capture_reclaim_graph_binding("peer", "connection", &key.graph)
                .is_err()
        );
        let target = crate::ubm_reclaim_authority::GraphTarget {
            peer_id: key.peer_id.clone(),
            connection_generation: key.connection_generation.clone(),
            graph: key.graph.clone(),
        };
        {
            let mut authority = peer.shared.reclaim_authorities.lock().unwrap();
            authority
                .reserve_permit_token_bound(1, 2, target.clone(), &binding)
                .unwrap();
            authority.resolve_permit_token(1, 2, &target, true).unwrap();
        }
        let mut next_key = key.clone();
        next_key.permit_id = 2;
        assert_eq!(
            peer.acquire_reclaim(&binding, next_key),
            Ok(PermitStatus::Busy)
        );
    }

    #[tokio::test]
    async fn cancelled_waiter_leaves_native_operation_owned_until_completion() {
        let (peer, binding, key) = reclaim_fixture();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let operation_peer = peer.clone();
        let waiter = tokio::spawn(async move {
            operation_peer
                .own_reclaim_operation(true, async move {
                    let _ = started_tx.send(());
                    let _ = finish_rx.await;
                    Ok(())
                })
                .await
        });
        started_rx.await.unwrap();
        waiter.abort();
        assert_eq!(
            peer.acquire_reclaim(&binding, key.clone()),
            Ok(crate::ubm_reclaim_authority::PermitStatus::Busy)
        );
        finish_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while peer.shared.graph.lock().unwrap().reclaim_operations != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned operation did not settle");
    }
    #[test]
    fn native_graph_invalidation_rejects_stale_identity_and_never_wraps() {
        let mut graph = super::NativeGraph {
            attachment: Some(7),
            revision: 1,
            exhausted: false,
            ready: true,
            reclaim_binding: Default::default(),
            reclaim_operations: 0,
            reclaim_effect_uncertain: false,
            native_operation_abandoned: false,
            reclaim_callback_uncertain: false,
            reclaim_callback_recoverable: false,
            reclaim_overflow_epoch: Some(0),
            reclaim_gate: crate::ubm_reclaim_graph_gate::GraphGate::new(256, 524_288),
            reclaim_drain_graph: None,
        };
        let old = graph.identity().unwrap().unwrap();
        graph.invalidate();
        assert!(graph.identity().unwrap().is_none());
        graph.ready = true;
        assert_ne!(graph.identity().unwrap().unwrap(), old);
        graph.revision = u64::MAX;
        graph.invalidate();
        assert!(graph.identity().is_err());
    }
    #[test]
    fn service_change_between_retirement_checks_refuses_cccd_write() {
        use super::{ActivationPhase, RetirementAction, retirement_action, start_on_current_graph};
        let mut graph = super::NativeGraph {
            attachment: Some(7),
            revision: 1,
            exhausted: false,
            ready: true,
            reclaim_binding: Default::default(),
            reclaim_operations: 0,
            reclaim_effect_uncertain: false,
            native_operation_abandoned: false,
            reclaim_callback_uncertain: false,
            reclaim_callback_recoverable: false,
            reclaim_overflow_epoch: Some(0),
            reclaim_gate: crate::ubm_reclaim_graph_gate::GraphGate::new(256, 524_288),
            reclaim_drain_graph: None,
        };
        let expected = graph.identity().unwrap().unwrap();
        assert_eq!(
            retirement_action(ActivationPhase::Active, false, true),
            RetirementAction::WriteCccd
        );
        // Model the callback after the initial decision and local-handler removal.
        graph.invalidate();
        let mut writes = 0;
        assert!(
            start_on_current_graph(&graph, &expected, || {
                writes += 1;
            })
            .is_err()
        );
        assert_eq!(writes, 0);
        assert_eq!(
            retirement_action(ActivationPhase::Retiring, false, false),
            RetirementAction::GraphUncertain
        );
        assert_eq!(
            retirement_action(ActivationPhase::Retiring, true, false),
            RetirementAction::LocalOnly
        );
    }

    #[test]
    fn proven_failed_enable_needs_only_local_retirement_after_graph_change() {
        use super::{ActivationPhase, RetirementAction, retirement_action};
        assert_eq!(
            retirement_action(ActivationPhase::Failed, false, false),
            RetirementAction::LocalOnly
        );
        assert_eq!(
            retirement_action(ActivationPhase::Active, false, false),
            RetirementAction::GraphUncertain
        );
        assert_eq!(
            retirement_action(ActivationPhase::Indeterminate, false, false),
            RetirementAction::GraphUncertain
        );
        assert_eq!(
            retirement_action(ActivationPhase::FailedRetirement, false, false),
            RetirementAction::GraphUncertain
        );
    }
    #[test]
    fn service_change_fences_an_already_open_source_group() {
        use crate::ubm_peer_notification_ingress::NotificationRoute;
        let faults = crate::common::util::NotificationFaults::new(8);
        faults.start_source_generation(1).unwrap();
        let (group, _) = faults.open_source_group(1, 8, 64).unwrap();
        faults.close_source_generation(1).unwrap();
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 0,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 0,
        };
        assert!(faults.register_source_route(&group, route, 1).is_err());
    }

    #[test]
    fn successful_unsubscribe_abandons_preowned_failure_cut_and_unfences_peer() {
        use std::sync::atomic::Ordering;

        let (peer, _, _) = reclaim_fixture();
        let shared = peer.shared.clone();
        shared
            .notification_faults
            .start_source_generation(1)
            .expect("source generation");
        shared.source_connection.lock().unwrap().source_generation = Some(1);

        let cut = shared
            .reserve_notification_failure_cut(1)
            .expect("pre-admitted cleanup failure cut");
        assert!(shared.pending_durable_failure.load(Ordering::Acquire));

        // A successful handler removal and CCCD disable never publish a
        // source failure. It must release the paired WinRT fence instead of
        // waiting for a finalizer that only a published recovery can invoke.
        shared.abandon_notification_failure_cut(cut);
        assert!(!shared.pending_durable_failure.load(Ordering::Acquire));
        assert!(shared.pending_failure_spill.lock().unwrap().is_none());
        let second = shared
            .reserve_notification_failure_cut(1)
            .expect("source is usable after successful cleanup");
        shared.abandon_notification_failure_cut(second);
    }

    #[test]
    fn durable_failure_defers_service_change_until_every_recipient_retries_file_retirement() {
        use crate::ubm_peer_notification_ingress::{
            FAILURE_RECOVERY_MAX_CHUNK, FailureEvidenceStore, FileFailureEvidenceStore,
            NotificationRoute,
        };
        use std::{
            sync::{Arc, Mutex, Weak},
            sync::atomic::{AtomicBool, AtomicU64, Ordering},
        };

        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
        let directory = std::env::temp_dir().join(format!(
            "ubm-winrt-deferred-service-change-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        let store: Arc<dyn FailureEvidenceStore> = Arc::new(
            FileFailureEvidenceStore::open(&directory, 1024 * 1024)
                .expect("host-owned evidence store"),
        );
        let authorities = Arc::new(Mutex::new(Default::default()));
        let peer = super::Peripheral::new_with_id_with_failure_evidence_store(
            Weak::new(),
            super::PeripheralId::from(crate::api::BDAddr::from([1, 2, 3, 4, 5, 6])),
            authorities,
            Some(store),
        );
        let shared = peer.shared.clone();
        let owner = Arc::new(AtomicBool::new(false));
        {
            let mut graph = shared.graph.lock().expect("graph");
            graph.ready = true;
        }
        {
            let mut source = shared.source_connection.lock().expect("source");
            source.owner = Some(owner.clone());
            source.connected = true;
            source.source_generation = Some(1);
        }
        peer.start_notification_source_generation(1)
            .expect("source generation");

        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        let (first_group, first_consumer) = peer
            .open_notification_source_group(1, 8, 64)
            .expect("first source recipient");
        peer.register_notification_source_route(&first_group, route, 1)
            .expect("first route");
        let (second_group, second_consumer) = peer
            .open_notification_source_group(1, 8, 64)
            .expect("second source recipient");
        peer.register_notification_source_route(&second_group, route, 2)
            .expect("second route");

        shared.queue_notification_source_failure(
            1,
            crate::PlatformError::new("winrt", "hresult", "native callback removal failed"),
        );
        assert!(shared.pending_durable_failure.load(Ordering::Acquire));

        let first_transfer = futures::executor::block_on(first_consumer.recv_transfer())
            .expect("first exact source head");
        let second_transfer = futures::executor::block_on(second_consumer.recv_transfer())
            .expect("second exact source head");
        let first = first_consumer
            .prepare_failure_recovery(&first_transfer)
            .expect("first recovery");
        let second = second_consumer
            .prepare_failure_recovery(&second_transfer)
            .expect("second recovery");
        for recovery in [&first, &second] {
            let mut offset = 0;
            while offset < recovery.encoded_len() {
                let chunk = recovery
                    .read(offset, FAILURE_RECOVERY_MAX_CHUNK)
                    .expect("contiguous exact recovery read");
                assert!(!chunk.is_empty());
                offset += chunk.len();
            }
            assert!(recovery.ready_for_begin_ack());
            assert!(recovery.begin_ack());
        }

        // This uses the production service-change path while a real retained
        // source failure is still owned. It must defer rather than invalidate
        // the source before the recipients have durably retired it.
        shared.apply_services_changed(&owner, shared.identity.clone());
        assert!(shared.deferred_services_changed.lock().unwrap().is_some());
        assert!(shared.graph.lock().unwrap().ready);
        assert_eq!(shared.source_connection.lock().unwrap().source_generation, Some(1));

        // The first recipient is not the final owner, so it cannot release
        // the service-change fence or delete the shared evidence.
        assert!(first.try_retire_ack().expect("first receipt retirement"));
        assert!(shared.pending_durable_failure.load(Ordering::Acquire));
        assert!(shared.deferred_services_changed.lock().unwrap().is_some());

        // Move (rather than delete) the retained file after both exact reads.
        // FileFailureEvidence::retire now receives the actual filesystem
        // deletion failure while the source receipt remains retained for the
        // retry below.
        let evidence = std::fs::read_dir(&directory)
            .expect("evidence directory")
            .map(|entry| entry.expect("evidence entry").path())
            .find(|path| path.is_file())
            .expect("retained exact evidence");
        let held = directory.join("held-for-retirement-retry.bin");
        std::fs::rename(&evidence, &held).expect("inject evidence deletion failure");
        assert!(held.is_file(), "the exact receipt remains retained");

        assert!(second.try_retire_ack().is_err(), "delete failure is retryable");
        assert!(shared.pending_durable_failure.load(Ordering::Acquire));
        assert!(shared.deferred_services_changed.lock().unwrap().is_some());
        assert!(shared.graph.lock().unwrap().ready);
        assert_eq!(shared.source_connection.lock().unwrap().source_generation, Some(1));

        std::fs::rename(&held, &evidence).expect("restore retained receipt path");
        assert!(second
            .try_retire_ack()
            .expect("retry exact evidence retirement"));
        assert!(!shared.pending_durable_failure.load(Ordering::Acquire));
        assert!(shared.deferred_services_changed.lock().unwrap().is_none());
        assert!(!shared.graph.lock().unwrap().ready);
        assert_eq!(shared.source_connection.lock().unwrap().source_generation, None);
        assert!(
            !second.try_retire_ack().expect("completed recovery is not reusable"),
            "the deferred service change finalizes exactly once"
        );

        std::fs::remove_dir(&directory).expect("retired evidence directory");
    }

    #[test]
    fn configured_peer_registry_injects_one_host_owned_evidence_store() {
        use std::sync::{Arc, Mutex, Weak};

        static NEXT_DIRECTORY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

        let directory = std::env::temp_dir().join(format!(
            "ubm-winrt-evidence-registry-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let store = Arc::new(
            crate::ubm_peer_notification_ingress::FileFailureEvidenceStore::open(&directory, 1024)
                .expect("host-owned evidence store"),
        );
        let registry = super::PeerRegistry::new(Some(store));
        let authorities = Arc::new(Mutex::new(Default::default()));
        let identity = super::PeripheralId::with_address_type(
            crate::api::BDAddr::from([1, 2, 3, 4, 5, 6]),
            Some(crate::api::AddressType::Public),
        );
        let peer = registry
            .get(Weak::new(), identity, authorities)
            .expect("configured peer");
        assert!(
            peer.shared
                .notification_faults
                .failure_evidence_store()
                .is_some()
        );
        std::fs::remove_dir_all(directory).expect("empty evidence directory");
    }
    use super::{
        Arc, AtomicBool, HashMap, Mutex, NotificationActivationIdentity, PreparedActivation,
        SourceConnection, claim_matches,
    };

    #[test]
    fn marker_dispatch_uses_graph_and_connection_fence_and_replays() {
        use super::{NativeGraph, start_marker_on_current_owner};
        use crate::common::util::{
            NotificationFaults, SourceGroupError, SourceUnresolvedWriteRequest, SourceWriteRequest,
            SourceWriteStart, SourceWriteStatus, marker_graph_identity,
        };
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
        use std::sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        };
        let connection = Mutex::new(SourceConnection {
            activations: HashMap::new(),
            physical_lanes: HashMap::new(),
            source_generation: Some(1),
            next_activation: 1,
            owner: None,
            connected: true,
        });
        let graph = Mutex::new(NativeGraph {
            attachment: Some(7),
            revision: 2,
            exhausted: false,
            ready: true,
            reclaim_binding: Default::default(),
            reclaim_operations: 0,
            reclaim_effect_uncertain: false,
            native_operation_abandoned: false,
            reclaim_callback_uncertain: false,
            reclaim_callback_recoverable: false,
            reclaim_overflow_epoch: Some(0),
            reclaim_gate: crate::ubm_reclaim_graph_gate::GraphGate::new(256, 524_288),
            reclaim_drain_graph: None,
        });
        let faults = NotificationFaults::new(4);
        faults.start_source_generation(1).unwrap();
        let (group, consumer) = faults.open_source_group(1, 8, 128).unwrap();
        let expected = ("winrt".to_owned(), 7, 2);
        let request = SourceWriteRequest {
            selector: Default::default(),
            id: 0,
            graph: marker_graph_identity(&expected),
            value: vec![1],
            write_type: crate::api::WriteType::WithResponse,
            route: NotificationRoute {
                service_uuid: uuid::Uuid::nil(),
                service_instance: 1,
                characteristic_uuid: uuid::Uuid::nil(),
                characteristic_instance: 2,
            },
        };
        let unresolved = SourceUnresolvedWriteRequest {
            id: request.id,
            graph: request.graph.clone(),
            selector: request.selector.clone(),
            value: request.value.clone(),
            write_type: request.write_type,
        };
        let calls = AtomicUsize::new(0);
        let SourceWriteStart::Initiated { receipt, wake, .. } = start_marker_on_current_owner(
            &connection,
            &graph,
            &faults,
            &group,
            &expected,
            request.clone(),
            || {
                assert!(connection.try_lock().is_err());
                assert!(graph.try_lock().is_err());
                assert_eq!(
                    group
                        .cancel_unresolved_marker_write(&unresolved)
                        .unwrap()
                        .status(),
                    SourceWriteStatus::Admitted { ordinal: 0 }
                );
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap() else {
            panic!("expected dispatch")
        };
        assert_eq!(graph.lock().unwrap().reclaim_operations, 1);
        for consumer in wake {
            consumer.wake();
        }
        assert!(matches!(
            consumer.poll(),
            Poll::Record(IngressRecord::Marker { id: 0, ordinal: 0 })
        ));
        drop(receipt);
        let SourceWriteStart::Replay(receipt) = start_marker_on_current_owner::<()>(
            &connection,
            &graph,
            &faults,
            &group,
            &expected,
            request.clone(),
            || panic!("redispatched"),
        )
        .unwrap() else {
            panic!("expected replay")
        };
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            group
                .cancel_unresolved_marker_write(&unresolved)
                .unwrap()
                .status(),
            SourceWriteStatus::Started { ordinal: 0 }
        );
        receipt.settle(Ok(()));
        graph.lock().unwrap().reclaim_operations -= 1;
        assert!(matches!(
            group
                .replay_marker_write(&request)
                .unwrap()
                .unwrap()
                .status(),
            SourceWriteStatus::Settled { .. }
        ));
        group.forget_marker_write(0).unwrap();
        let mut next = request;
        next.id = 1;
        let cancelled = SourceUnresolvedWriteRequest {
            id: next.id,
            graph: next.graph.clone(),
            selector: next.selector.clone(),
            value: next.value.clone(),
            write_type: next.write_type,
        };
        assert_eq!(
            group
                .cancel_unresolved_marker_write(&cancelled)
                .unwrap()
                .status(),
            SourceWriteStatus::NotAdmitted(SourceGroupError::Cancelled)
        );
        assert!(matches!(
            start_marker_on_current_owner::<()>(
                &connection,
                &graph,
                &faults,
                &group,
                &expected,
                next.clone(),
                || panic!("cancelled write dispatched")
            )
            .unwrap(),
            SourceWriteStart::Replay(_)
        ));
        group.forget_marker_write(1).unwrap();
        next.id = 2;
        graph.lock().unwrap().invalidate();
        assert!(matches!(
            start_marker_on_current_owner::<()>(
                &connection,
                &graph,
                &faults,
                &group,
                &expected,
                next,
                || panic!("stale dispatch")
            ),
            Err(SourceGroupError::StaleGeneration)
        ));
    }

    #[test]
    fn abandoned_read_operation_keeps_reclaim_and_owner_fenced() {
        let (peer, binding, key) = reclaim_fixture();
        {
            let mut graph = peer.shared.graph.lock().unwrap();
            graph.reclaim_operations += 1;
        }
        drop(super::ReclaimOperationGuard {
            shared: peer.shared.clone(),
            effectful: false,
            settled: false,
            completed: false,
        });
        assert!(peer.shared.must_retain());
        assert!(peer.shared.ensure_native_owner_replaceable().is_err());
        assert_eq!(
            peer.acquire_reclaim(&binding, key),
            Ok(crate::ubm_reclaim_authority::PermitStatus::Busy)
        );
    }

    #[test]
    fn completed_operation_errors_release_in_flight_fence() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let (peer, binding, key) = reclaim_fixture();
        let result: super::Result<()> =
            runtime.block_on(peer.own_reclaim_write(async { Err(super::Error::NotConnected) }));
        assert!(result.is_err());
        assert_eq!(peer.shared.graph.lock().unwrap().reclaim_operations, 0);
        assert_eq!(
            peer.acquire_reclaim(&binding, key),
            Ok(crate::ubm_reclaim_authority::PermitStatus::Acquired)
        );
    }

    #[test]
    fn retired_graph_cannot_open_a_new_notification_source() {
        let (peer, _, _) = reclaim_fixture();
        peer.shared.graph.lock().unwrap().ready = false;
        assert_eq!(
            peer.start_notification_source_generation(1),
            Err(crate::common::util::SourceGroupError::StaleGeneration)
        );
        assert_eq!(peer.current_notification_source_generation(), Ok(None));
    }

    #[test]
    fn delayed_operation_cannot_start_on_replaced_graph() {
        let (peer, _, _) = reclaim_fixture();
        let expected = peer
            .shared
            .graph
            .lock()
            .unwrap()
            .identity()
            .unwrap()
            .unwrap();
        peer.shared.graph.lock().unwrap().invalidate();
        let graph = peer.shared.graph.lock().unwrap();
        assert!(
            super::start_on_current_graph(&graph, &expected, || panic!("stale native start"))
                .is_err()
        );
    }

    #[test]
    fn uncertain_ordinary_cccd_keeps_reclaim_fenced_until_recovery() {
        let (peer, binding, key) = reclaim_fixture();
        let route = (uuid::Uuid::nil(), 1, uuid::Uuid::nil(), 2);
        peer.shared
            .ordinary_cccd_uncertain
            .lock()
            .unwrap()
            .insert(route);
        assert!(peer.shared.must_retain());
        assert_eq!(
            peer.acquire_reclaim(&binding, key.clone()),
            Ok(crate::ubm_reclaim_authority::PermitStatus::Busy)
        );
        peer.shared
            .ordinary_cccd_uncertain
            .lock()
            .unwrap()
            .remove(&route);
        assert_eq!(
            peer.acquire_reclaim(&binding, key),
            Ok(crate::ubm_reclaim_authority::PermitStatus::Acquired)
        );
    }

    #[test]
    fn graph_operation_guards_survive_requesting_runtime_shutdown() {
        use std::time::Duration;
        for (effectful, write) in [(false, false), (true, false), (true, true)] {
            let caller = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            let (peer, binding, key) = reclaim_fixture();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            let worker = peer.clone();
            let shared = peer.shared.clone();
            caller.spawn(async move {
                let operation = async move {
                    let _lease = shared.notification_lifecycle.read().await;
                    started_tx.send(()).unwrap();
                    finish_rx.await.unwrap();
                    Ok(())
                };
                if write {
                    let _ = worker.own_reclaim_write(operation).await;
                } else {
                    let _ = worker.own_reclaim_operation(effectful, operation).await;
                }
            });
            caller.block_on(async {
                tokio::time::timeout(Duration::from_secs(2), async {
                    while started_rx.try_recv().is_err() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("owned operation did not start");
            });
            drop(caller);
            assert_eq!(peer.shared.graph.lock().unwrap().reclaim_operations, 1);
            assert!(peer.shared.notification_lifecycle.try_write().is_err());
            assert_eq!(
                peer.acquire_reclaim(&binding, key.clone()),
                Ok(crate::ubm_reclaim_authority::PermitStatus::Busy)
            );
            finish_tx.send(()).unwrap();
            for _ in 0..400 {
                if peer.shared.graph.lock().unwrap().reclaim_operations == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let graph = peer.shared.graph.lock().unwrap();
            assert_eq!(graph.reclaim_operations, 0);
            assert!(!graph.reclaim_effect_uncertain);
            assert!(!graph.native_operation_abandoned);
            drop(graph);
            assert!(peer.shared.notification_lifecycle.try_write().is_ok());
            assert_eq!(
                peer.acquire_reclaim(&binding, key.clone()),
                Ok(crate::ubm_reclaim_authority::PermitStatus::Acquired)
            );
        }
    }

    #[test]
    fn marker_reply_shutdown_and_refusal_settle_the_retained_owner() {
        use crate::common::util::{
            SourceWriteRequest, SourceWriteStart, SourceWriteStatus, marker_graph_identity,
        };
        use crate::ubm_peer_notification_ingress::NotificationRoute;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        for poll_reply in [false, true] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let (peer, binding, key) = reclaim_fixture();
            peer.start_notification_source_generation(1).unwrap();
            let (group, _) = peer.open_notification_source_group(1, 8, 128).unwrap();
            let request = SourceWriteRequest {
                selector: Default::default(),
                id: 0,
                graph: marker_graph_identity(&key.graph),
                value: vec![1],
                write_type: crate::api::WriteType::WithResponse,
                route: NotificationRoute {
                    service_uuid: uuid::Uuid::nil(),
                    service_instance: 1,
                    characteristic_uuid: uuid::Uuid::nil(),
                    characteristic_instance: 2,
                },
            };
            let polled = Arc::new(AtomicBool::new(false));
            let flag = polled.clone();
            let SourceWriteStart::Initiated {
                receipt, operation, ..
            } = super::start_marker_on_current_owner(
                &peer.shared.source_connection,
                &peer.shared.graph,
                &peer.shared.notification_faults,
                &group,
                &key.graph,
                request.clone(),
                || {
                    Ok(async move {
                        flag.store(true, Ordering::SeqCst);
                        std::future::pending::<std::result::Result<(), crate::PlatformError>>()
                            .await
                    })
                },
            )
            .unwrap()
            else {
                panic!("marker was not started")
            };
            let lease = peer
                .shared
                .notification_lifecycle
                .clone()
                .try_read_owned()
                .unwrap();
            super::spawn_marker_write_reply(
                &runtime.handle(),
                peer.shared.clone(),
                lease,
                receipt.clone(),
                operation,
            );
            if poll_reply {
                runtime.block_on(async {
                    for _ in 0..100 {
                        if polled.load(Ordering::SeqCst) {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                });
            }
            assert_eq!(polled.load(Ordering::SeqCst), poll_reply);
            assert_eq!(
                peer.acquire_reclaim(&binding, key.clone()),
                Ok(crate::ubm_reclaim_authority::PermitStatus::Busy)
            );
            drop(runtime);
            let graph = peer.shared.graph.lock().unwrap();
            assert_eq!(graph.reclaim_operations, 0);
            assert!(graph.reclaim_effect_uncertain);
            assert!(graph.native_operation_abandoned);
            drop(graph);
            assert!(peer.shared.ensure_native_owner_replaceable().is_err());
            assert!(matches!(receipt.status(), SourceWriteStatus::Settled {
                ordinal: 0, result: Err(error),
            } if error.code == "write-reply-abandoned"));
            assert_eq!(
                group
                    .replay_marker_write(&request)
                    .unwrap()
                    .unwrap()
                    .status(),
                receipt.status()
            );
            group.forget_marker_write(0).unwrap();
        }
    }

    #[test]
    fn marker_reply_survives_requesting_runtime_shutdown() {
        use crate::common::util::{
            SourceWriteRequest, SourceWriteStart, SourceWriteStatus, marker_graph_identity,
        };
        use crate::ubm_peer_notification_ingress::NotificationRoute;
        use crate::ubm_reclaim_authority::PermitStatus;

        for poll_before_shutdown in [false, true] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let (peer, binding, key) = reclaim_fixture();
            peer.start_notification_source_generation(1).unwrap();
            let (group, _) = peer.open_notification_source_group(1, 8, 128).unwrap();
            let request = SourceWriteRequest {
                selector: Default::default(),
                id: 0,
                graph: marker_graph_identity(&key.graph),
                value: vec![1],
                write_type: crate::api::WriteType::WithResponse,
                route: NotificationRoute {
                    service_uuid: uuid::Uuid::nil(),
                    service_instance: 1,
                    characteristic_uuid: uuid::Uuid::nil(),
                    characteristic_instance: 2,
                },
            };
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            let (poll_tx, poll_rx) = std::sync::mpsc::channel();
            let (allow_tx, allow_rx) = tokio::sync::oneshot::channel::<()>();
            let SourceWriteStart::Initiated {
                receipt, operation, ..
            } = super::start_marker_on_current_owner(
                &peer.shared.source_connection,
                &peer.shared.graph,
                &peer.shared.notification_faults,
                &group,
                &key.graph,
                request,
                || {
                    Ok(async move {
                        let _ = poll_tx.send(());
                        finish_rx.await.unwrap()
                    })
                },
            )
            .unwrap()
            else {
                panic!("marker was not started")
            };
            let lease = peer
                .shared
                .notification_lifecycle
                .clone()
                .try_read_owned()
                .unwrap();
            let mut allow_tx = Some(allow_tx);
            if poll_before_shutdown {
                allow_tx.take().unwrap().send(()).unwrap();
            }
            runtime.block_on(async {
                super::spawn_marker_write_reply(
                    super::super::marker_executor::handle().unwrap(),
                    peer.shared.clone(),
                    lease,
                    receipt.clone(),
                    async move {
                        allow_rx.await.unwrap();
                        operation.await
                    },
                );
            });
            if poll_before_shutdown {
                poll_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
            } else {
                assert!(poll_rx.try_recv().is_err());
            }
            drop(runtime);
            assert_eq!(receipt.status(), SourceWriteStatus::Started { ordinal: 0 });
            assert!(peer.shared.notification_lifecycle.try_write().is_err());
            assert_eq!(
                peer.acquire_reclaim(&binding, key.clone()),
                Ok(PermitStatus::Busy)
            );
            if !poll_before_shutdown {
                allow_tx.take().unwrap().send(()).unwrap();
                poll_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
            }
            finish_tx.send(Ok(())).unwrap();
            let replacement = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            replacement.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while peer.shared.graph.lock().unwrap().reclaim_operations != 0
                        || peer.shared.notification_lifecycle.try_write().is_err()
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            });
            assert_eq!(
                receipt.status(),
                SourceWriteStatus::Settled {
                    ordinal: 0,
                    result: Ok(())
                }
            );
            assert_eq!(
                peer.acquire_reclaim(&binding, key),
                Ok(PermitStatus::Acquired)
            );
        }
    }

    #[test]
    fn marker_att_refusal_is_definite_but_transport_failure_is_uncertain() {
        use windows::Devices::Bluetooth::GenericAttributeProfile::GattCommunicationStatus;
        let classify = |status, att| {
            super::marker_write_error(super::utils::gatt_status_error("write", status, att))
        };
        let refusal = classify(GattCommunicationStatus::ProtocolError, Some(5));
        let no_byte = classify(GattCommunicationStatus::ProtocolError, None);
        let transport = classify(GattCommunicationStatus::Unreachable, None);
        assert!(super::marker_write_definitively_settled(&Err(refusal)));
        assert!(!super::marker_write_definitively_settled(&Err(no_byte)));
        assert!(!super::marker_write_definitively_settled(&Err(transport)));
    }

    #[tokio::test]
    async fn marker_os_reply_owns_reclaim_through_cancellation_and_failure() {
        use super::start_marker_on_current_owner;
        use crate::common::util::{
            SourceUnresolvedWriteRequest, SourceWriteRequest, SourceWriteStart, SourceWriteStatus,
            marker_graph_identity,
        };
        use crate::ubm_peer_notification_ingress::NotificationRoute;
        use crate::ubm_reclaim_authority::PermitStatus;
        let refusal = crate::PlatformError::new("winrt", "gatt-status", "ATT refusal")
            .with("gattStatus", "protocol-error")
            .with("attError", "5");
        let transport = crate::PlatformError::new("winrt", "gatt-status", "unreachable")
            .with("gattStatus", "unreachable");
        for (answer, uncertain) in [
            (Ok(()), false),
            (Err(refusal), false),
            (Err(transport), true),
        ] {
            let (peer, binding, key) = reclaim_fixture();
            peer.start_notification_source_generation(1).unwrap();
            let (group, _) = peer.open_notification_source_group(1, 8, 128).unwrap();
            let request = SourceWriteRequest {
                selector: Default::default(),
                id: 0,
                graph: marker_graph_identity(&key.graph),
                value: vec![1],
                write_type: crate::api::WriteType::WithResponse,
                route: NotificationRoute {
                    service_uuid: uuid::Uuid::nil(),
                    service_instance: 1,
                    characteristic_uuid: uuid::Uuid::nil(),
                    characteristic_instance: 2,
                },
            };
            let unresolved = SourceUnresolvedWriteRequest {
                id: request.id,
                graph: request.graph.clone(),
                selector: request.selector.clone(),
                value: request.value.clone(),
                write_type: request.write_type,
            };
            let (finish_tx, finish_rx) =
                tokio::sync::oneshot::channel::<std::result::Result<(), crate::PlatformError>>();
            let SourceWriteStart::Initiated {
                receipt, operation, ..
            } = start_marker_on_current_owner(
                &peer.shared.source_connection,
                &peer.shared.graph,
                &peer.shared.notification_faults,
                &group,
                &key.graph,
                request,
                || Ok(finish_rx),
            )
            .unwrap()
            else {
                panic!("marker was not started")
            };
            let lease = peer
                .shared
                .notification_lifecycle
                .clone()
                .try_read_owned()
                .unwrap();
            super::spawn_marker_write_reply(
                &tokio::runtime::Handle::current(),
                peer.shared.clone(),
                lease,
                receipt.clone(),
                async move { operation.await.unwrap() },
            );
            // Disconnect and reconnect both take the exclusive lifecycle lock.
            // They must not close or replace this owner while its write is pending.
            assert!(peer.shared.notification_lifecycle.try_write().is_err());
            assert_eq!(
                peer.acquire_reclaim(&binding, key.clone()),
                Ok(PermitStatus::Busy)
            );
            assert_eq!(
                group
                    .cancel_unresolved_marker_write(&unresolved)
                    .unwrap()
                    .status(),
                SourceWriteStatus::Started { ordinal: 0 }
            );
            finish_tx.send(answer.clone()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while peer.shared.graph.lock().unwrap().reclaim_operations != 0
                    || peer.shared.notification_lifecycle.try_write().is_err()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(peer.shared.notification_lifecycle.try_write().is_ok());
            assert!(peer.shared.ensure_native_owner_replaceable().is_ok());
            let expected = if uncertain {
                answer.map_err(|error| error.with("remoteEffect", "unknown"))
            } else {
                answer
            };
            assert_eq!(
                receipt.status(),
                SourceWriteStatus::Settled {
                    ordinal: 0,
                    result: expected
                }
            );
            assert!(!peer.shared.graph.lock().unwrap().reclaim_effect_uncertain);
            assert_eq!(
                peer.acquire_reclaim(&binding, key),
                Ok(PermitStatus::Acquired)
            );
        }
    }

    #[test]
    fn cancelled_waiter_does_not_release_physical_operation_before_completion() {
        use super::drive_independently;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            let first_lock = lock.clone();
            let waiter = tokio::spawn(async move {
                drive_independently(async move {
                    let _guard = first_lock.lock().await;
                    started_tx.send(()).unwrap();
                    finish_rx.await.unwrap();
                    Ok(())
                })
                .await
            });
            started_rx.await.unwrap();
            waiter.abort();
            let (second_tx, mut second_rx) = tokio::sync::oneshot::channel();
            let second = tokio::spawn(async move {
                drive_independently(async move {
                    let _guard = lock.lock().await;
                    second_tx.send(()).unwrap();
                    Ok(())
                })
                .await
            });
            tokio::task::yield_now().await;
            assert!(matches!(
                second_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            finish_tx.send(()).unwrap();
            second_rx.await.unwrap();
            second.await.unwrap().unwrap();
        });
    }

    #[test]
    fn detached_native_write_finishes_before_disconnect_and_replacement() {
        use super::drive_independently;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let barrier = Arc::new(tokio::sync::RwLock::new(()));
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            let native_barrier = barrier.clone();
            let waiter = tokio::spawn(async move {
                drive_independently(async move {
                    let _lease = native_barrier.read().await;
                    started_tx.send(()).unwrap();
                    finish_rx.await.unwrap();
                    Ok(())
                })
                .await
            });
            started_rx.await.unwrap();
            waiter.abort();
            let (retired_tx, mut retired_rx) = tokio::sync::oneshot::channel();
            let close_barrier = barrier.clone();
            let close = tokio::spawn(async move {
                let _lease = close_barrier.write().await;
                retired_tx.send(()).unwrap();
            });
            tokio::task::yield_now().await;
            assert!(matches!(
                retired_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            finish_tx.send(()).unwrap();
            retired_rx.await.unwrap();
            close.await.unwrap();
            // A later generation can only acquire its lease after retirement.
            let _replacement = barrier.read().await;
        });
    }

    #[test]
    fn old_callback_cannot_publish_to_replacement_activation() {
        use crate::api::ValueNotification;
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
        let (peer, _, _) = reclaim_fixture();
        let faults = &peer.shared.notification_faults;
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        };
        faults.start_source_generation(1).unwrap();
        faults.start_source_activation(1, route, 10).unwrap();
        faults.start_source_activation(1, route, 11).unwrap();
        let (old_group, old_consumer) = faults.open_source_group(1, 8, 64).unwrap();
        let (new_group, new_consumer) = faults.open_source_group(1, 8, 64).unwrap();
        faults
            .register_source_route_for_activation(&old_group, route, 1, 10)
            .unwrap();
        faults
            .register_source_route_for_activation(&new_group, route, 2, 11)
            .unwrap();
        let old = Mutex::new(Some(PreparedActivation {
            generation: 1,
            id: 10,
            claims: 1,
        }));
        let value = ValueNotification {
            uuid: uuid::Uuid::nil(),
            instance: 2,
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            value: vec![1],
            source_failure: None,
            lost_before: 0,
        };
        assert!(peer.shared.admit_managed_source_callback(
            &old,
            route,
            &value,
            1,
            false,
            Box::new(|| {})
        ));
        assert!(matches!(
            old_consumer.poll(),
            Poll::Record(IngressRecord::Value { .. })
        ));
        assert_eq!(new_consumer.poll(), Poll::Empty);
        faults.retire_source_activation(1, route, 10).unwrap();
        assert!(peer.shared.admit_managed_source_callback(
            &old,
            route,
            &value,
            1,
            false,
            Box::new(|| {})
        ));
        assert_eq!(new_consumer.poll(), Poll::Empty);
    }

    #[test]
    fn physical_identity_uses_connection_graph_and_handle() {
        let connection = Arc::new(AtomicBool::new(false));
        let next_connection = Arc::new(AtomicBool::new(false));
        let first = NotificationActivationIdentity {
            connection: connection.clone(),
            handle: 7,
            graph_revision: 1,
        };
        let rediscovered = NotificationActivationIdentity {
            connection: connection.clone(),
            handle: 7,
            graph_revision: 1,
        };
        let sibling = NotificationActivationIdentity {
            connection,
            handle: 8,
            graph_revision: 1,
        };
        let reconnected = NotificationActivationIdentity {
            connection: next_connection,
            handle: 7,
            graph_revision: 1,
        };
        let mut owners = HashMap::new();
        owners.insert(first.clone(), "first");
        owners.insert(sibling.clone(), "sibling");
        assert_eq!(owners.get(&rediscovered), Some(&"first"));
        assert_ne!(first, sibling);
        assert_ne!(first, reconnected);
        let changed = NotificationActivationIdentity {
            connection: first.connection.clone(),
            handle: 7,
            graph_revision: 2,
        };
        assert_ne!(first, changed);
        owners.insert(changed.clone(), "new graph");
        owners.remove(&first);
        assert_eq!(owners.get(&changed), Some(&"new graph"));
    }

    #[test]
    fn rediscovered_handle_keeps_old_owner_until_disconnect() {
        use super::{ActivationPhase, RetirementAction, retirement_action};
        let old_object = Arc::new("old characteristic");
        let replacement = Arc::new("new characteristic");
        let mut cache = HashMap::new();
        let retained_old = super::graph_activation(&mut cache, 1, 7, || old_object.clone());
        assert!(Arc::ptr_eq(
            &retained_old,
            &super::graph_activation(&mut cache, 1, 7, || panic!("same graph must share"))
        ));
        let new = super::graph_activation(&mut cache, 2, 7, || replacement.clone());
        assert!(!Arc::ptr_eq(&retained_old, &new));
        assert_eq!(
            retirement_action(ActivationPhase::Active, false, false),
            RetirementAction::GraphUncertain
        );
        assert_eq!(
            retirement_action(ActivationPhase::Active, true, false),
            RetirementAction::LocalOnly
        );
        drop(retained_old);
        assert!(Arc::ptr_eq(&replacement, cache.get(&(2, 7)).unwrap()));
    }

    #[test]
    fn same_handle_serializes_operations_across_graphs() {
        let mut source = SourceConnection {
            activations: HashMap::new(),
            physical_lanes: HashMap::new(),
            source_generation: None,
            next_activation: 1,
            owner: None,
            connected: true,
        };
        let old = source.physical_lane(7);
        let in_flight = old.try_lock().unwrap();
        let replacement = source.physical_lane(7);
        assert!(Arc::ptr_eq(&old, &replacement));
        assert!(replacement.try_lock().is_err());
        drop(in_flight);
        assert!(replacement.try_lock().is_ok());
        assert!(!Arc::ptr_eq(&old, &source.physical_lane(8)));
    }

    #[test]
    fn shared_claims_only_retire_on_last_release_and_fence_old_claims() {
        use super::{ActivationPhase, ActivationRecovery, ActivationRegistration};
        use windows::Devices::Bluetooth::GenericAttributeProfile::GattClientCharacteristicConfigurationDescriptorValue as Config;
        let mut registration = ActivationRegistration {
            token: Some(11),
            epoch: 9,
            config: Config::Notify,
            claims: 1,
            phase: ActivationPhase::Active,
            live: Arc::new(AtomicBool::new(true)),
            callbacks: Arc::new(super::super::callback_retirement::CallbackRetirement::new()),
        };
        let mut first = Some(9);
        let mut second = None;
        registration.phase = ActivationPhase::Enabling;
        assert_eq!(
            registration.recovery(Config::Notify).unwrap(),
            ActivationRecovery::Uncertain
        );
        assert!(registration.recovery(Config::Indicate).is_err());
        assert!(registration.release_shared(&mut first));
        registration.phase = ActivationPhase::Active;
        registration.join(&mut second, Config::Notify).unwrap();
        assert_eq!(registration.claims, 2);
        assert!(!registration.release_shared(&mut first));
        assert_eq!(registration.claims, 1);
        assert!(!registration.release_shared(&mut first));
        assert!(registration.release_shared(&mut second));
        registration.phase = ActivationPhase::Retiring;
        let mut third = None;
        assert_eq!(
            registration.recovery(Config::Notify).unwrap(),
            ActivationRecovery::Uncertain
        );
        registration.phase = ActivationPhase::Indeterminate;
        assert_eq!(
            registration.recovery(Config::Notify).unwrap(),
            ActivationRecovery::Uncertain
        );
        assert!(registration.release_shared(&mut second));
        assert_eq!(registration.claims, 1);
        registration.phase = ActivationPhase::FailedRetirement;
        assert_eq!(
            registration.recovery(Config::Notify).unwrap(),
            ActivationRecovery::FinishRetirement
        );
        assert!(registration.release_shared(&mut second));
        registration.epoch = 10;
        registration.phase = ActivationPhase::Active;
        assert!(!registration.release_shared(&mut second));
        assert!(registration.join(&mut third, Config::Indicate).is_err());
    }

    #[test]
    fn stale_owner_cannot_claim_replacement_registration() {
        assert!(claim_matches(Some(4), 4));
        assert!(!claim_matches(Some(4), 5));
        assert!(!claim_matches(None, 5));
    }

    #[test]
    fn disconnected_candidate_cannot_finish_and_old_owner_remains_live_until_transfer() {
        let old = Arc::new(AtomicBool::new(false));
        let candidate = Arc::new(AtomicBool::new(false));
        let mut source = SourceConnection {
            activations: HashMap::new(),
            physical_lanes: HashMap::new(),
            source_generation: None,
            next_activation: 1,
            owner: Some(old.clone()),
            connected: true,
        };
        assert!(!source.observe_disconnect(&candidate));
        assert!(source.connected);
        assert!(source.finish_connect(&old));
        source.owner = Some(candidate.clone());
        source.connected = false;
        assert!(!source.finish_connect(&candidate));
        assert!(!source.connected);
    }

    #[test]
    fn indeterminate_old_connection_retires_only_after_confirmed_disconnect() {
        use super::{ActivationPhase, Ordering, RetirementAction, retirement_action};
        let old = Arc::new(AtomicBool::new(false));
        let replacement = Arc::new(AtomicBool::new(false));
        let mut source = SourceConnection {
            activations: HashMap::new(),
            physical_lanes: HashMap::new(),
            source_generation: None,
            next_activation: 1,
            owner: Some(old.clone()),
            connected: true,
        };
        source.begin_disconnect();
        assert_eq!(
            retirement_action(
                ActivationPhase::Indeterminate,
                old.load(Ordering::Relaxed),
                true
            ),
            RetirementAction::Uncertain
        );
        assert!(source.observe_disconnect(&old));
        source.owner = Some(replacement.clone());
        assert_eq!(
            retirement_action(
                ActivationPhase::Indeterminate,
                old.load(Ordering::Relaxed),
                true
            ),
            RetirementAction::LocalOnly
        );
        assert!(!replacement.load(Ordering::Relaxed));
        assert!(source.finish_connect(&replacement));
        assert!(old.load(Ordering::Relaxed));
    }

    #[test]
    fn stale_callback_cannot_close_replacement() {
        let old = Arc::new(AtomicBool::new(false));
        let current = Arc::new(AtomicBool::new(false));
        let mut source = SourceConnection {
            activations: HashMap::new(),
            physical_lanes: HashMap::new(),
            source_generation: None,
            next_activation: 1,
            owner: Some(current.clone()),
            connected: false,
        };
        assert!(source.finish_connect(&current));
        assert!(!source.observe_disconnect(&old));
        assert!(source.connected);
        assert!(source.observe_disconnect(&current));
        assert!(!source.finish_connect(&current));
    }
    #[test]
    fn failed_cleanup_retains_owner_for_physical_disconnect_and_retry() {
        let owner = Arc::new(AtomicBool::new(false));
        let mut source = SourceConnection {
            activations: HashMap::new(),
            physical_lanes: HashMap::new(),
            source_generation: None,
            next_activation: 1,
            owner: Some(owner.clone()),
            connected: true,
        };
        source.begin_disconnect();
        assert!(!source.connected);
        assert!(
            source
                .owner
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &owner))
        );
        assert!(source.observe_disconnect(&owner));
        assert!(!source.connected);
        source.begin_disconnect();
        assert!(source.owner.is_some());
        source.owner = None;
        assert!(!source.observe_disconnect(&owner));
    }
}
