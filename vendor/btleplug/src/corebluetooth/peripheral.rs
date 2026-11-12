// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.

use super::central_delegate::AttrKey;
use super::internal::{
    CoreBluetoothMessage, CoreBluetoothReply, CoreBluetoothReplyFuture, PeripheralEventInternal,
};
use crate::{
    Error, Result,
    api::{
        self, BDAddr, CentralEvent, CharPropFlags, Characteristic, Descriptor,
        PeripheralProperties, Service, ValueNotification, WriteType,
    },
    common::{
        adapter_manager::AdapterManager,
        util::{
            NotificationEnvelope, NotificationFaults, retained_notification_events,
            retained_notifications_from_sender,
        },
    },
};
use async_trait::async_trait;
use futures::channel::mpsc::{Receiver, SendError, Sender};
use futures::sink::SinkExt;
use futures::stream::{Stream, StreamExt};
use log::*;
use objc2_core_bluetooth::CBPeripheralState;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "serde")]
use serde_cr as serde;
use std::sync::Weak;
use std::{
    collections::{BTreeSet, HashMap},
    fmt::{self, Debug, Display, Formatter},
    pin::Pin,
    sync::{Arc, Mutex, atomic::AtomicU16},
};
use tokio::sync::broadcast;
use tokio::task;
use uuid::Uuid;

#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_cr")
)]
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PeripheralId(Uuid);

impl PeripheralId {
    /// UBM patch (UBM_PATCHES.md #19): the CoreBluetooth identifier.
    pub(crate) fn uuid(&self) -> Uuid {
        self.0
    }
}

impl Display for PeripheralId {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

/// An exact native characteristic activation owner. Its identity is scoped to
/// this peripheral; it cannot be redirected by characteristic reordering.
#[derive(Clone, Debug)]
pub struct NotificationActivationOwner {
    token: Arc<super::internal::NativeActivationToken>,
    peripheral_uuid: Uuid,
    sender: Sender<CoreBluetoothMessage>,
    source_owner: Arc<super::internal::PeerSourceOwner>,
    route: crate::ubm_peer_notification_ingress::NotificationRoute,
    prepared: Arc<Mutex<Option<u64>>>,
}

impl NotificationActivationOwner {
    pub fn identity(&self) -> u64 {
        self.token.identity.id
    }

    /// Bind this exact native claim before registering its ordered source route.
    pub async fn prepare_source_activation(&self) -> Result<u64> {
        let state = self.source_owner.waiters.lock().unwrap();
        if !state.ready || state.revision != self.token.revision {
            return Err(Error::Platform(crate::PlatformError::new(
                "corebluetooth",
                "notification-owner-invalidated",
                "Native notification owner is no longer current",
            )));
        }
        let generation = state
            .generation
            .ok_or_else(|| Error::Other("Notification source generation is not active".into()))?;
        self.source_owner
            .faults
            .start_source_activation(generation, self.route, self.token.claim_id)
            .map_err(|error| {
                Error::Other(format!("Notification source activation failed: {error:?}").into())
            })?;
        *self.prepared.lock().unwrap() = Some(generation);
        Ok(self.token.claim_id)
    }

    fn retire_prepared_source(&self) -> Result<()> {
        let state = self.source_owner.waiters.lock().unwrap();
        let mut prepared = self.prepared.lock().unwrap();
        if let Some(generation) = *prepared {
            if state.generation == Some(generation) {
                self.source_owner
                    .faults
                    .retire_source_activation(generation, self.route, self.token.claim_id)
                    .map_err(|error| {
                        Error::Other(
                            format!("Notification source retirement failed: {error:?}").into(),
                        )
                    })?;
            }
            *prepared = None;
        }
        Ok(())
    }

    pub async fn subscribe(&self) -> Result<()> {
        let owner = self.clone();
        tokio::spawn(async move {
            owner
                .token
                .identity
                .ownership
                .acquire(owner.token.revision, owner.token.claim_id, || {
                    owner.set_enabled(true)
                })
                .await
        })
        .await
        .map_err(|error| Error::RuntimeError(format!("Notification enable task failed: {error}")))?
    }

    pub async fn unsubscribe(&self) -> Result<()> {
        let owner = self.clone();
        tokio::spawn(async move {
            owner
                .token
                .identity
                .ownership
                .release(owner.token.revision, owner.token.claim_id, || {
                    owner.set_enabled(false)
                })
                .await?;
            owner.retire_prepared_source()
        })
        .await
        .map_err(|error| {
            Error::RuntimeError(format!("Notification disable task failed: {error}"))
        })?
    }

    async fn set_enabled(&self, enable: bool) -> Result<()> {
        let future = CoreBluetoothReplyFuture::default();
        self.sender
            .clone()
            .send(CoreBluetoothMessage::SetNotificationOwner {
                peripheral_uuid: self.peripheral_uuid,
                token: self.token.clone(),
                enable,
                future: future.get_state_clone(),
            })
            .await?;
        match future.await {
            CoreBluetoothReply::Ok => Ok(()),
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            CoreBluetoothReply::Err(error) => Err(Error::RuntimeError(error)),
            reply => Err(Error::RuntimeError(format!(
                "Unexpected notification owner reply: {reply:?}"
            ))),
        }
    }
}

/// Implementation of [api::Peripheral](crate::api::Peripheral).
/// UBM patch (UBM_PATCHES.md #2): advertisement fields CoreBluetooth
/// reports and upstream btleplug drops, from the latest advertisement.
/// `None` is a key the advertisement did not carry; `Some(vec![])` a
/// carried key with no entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdvertisementExtras {
    /// `CBAdvertisementDataSolicitedServiceUUIDsKey`.
    pub solicited_service_uuids: Option<Vec<Uuid>>,
    /// `CBAdvertisementDataOverflowServiceUUIDsKey`.
    pub overflow_service_uuids: Option<Vec<Uuid>>,
    /// `CBAdvertisementDataIsConnectable`.
    pub connectable: Option<bool>,
}

#[derive(Clone)]
pub struct Peripheral {
    shared: Arc<Shared>,
}

struct Shared {
    source_owner: Arc<super::internal::PeerSourceOwner>,
    notifications_channel: broadcast::Sender<NotificationEnvelope>,
    notification_faults: Arc<NotificationFaults>,
    legacy_claims:
        tokio::sync::Mutex<HashMap<(AttrKey, AttrKey), Vec<NotificationActivationOwner>>>,
    manager: Weak<AdapterManager<Peripheral>>,
    uuid: Uuid,
    services: Mutex<BTreeSet<Service>>,
    properties: Mutex<PeripheralProperties>,
    message_sender: Sender<CoreBluetoothMessage>,
    mtu: AtomicU16,
    // UBM patch (UBM_PATCHES.md #2): the latest advertisement's extras.
    advertisement_extras: Mutex<AdvertisementExtras>,
    // UBM patch (UBM_PATCHES.md #4): write-without-response readiness.
    write_readiness: broadcast::Sender<bool>,
    // We're not actually holding a peripheral object here, that's held out in
    // the objc thread. We'll just communicate with it through our
    // receiver/sender pair.
}

impl Shared {
    fn emit_event(&self, event: CentralEvent) {
        match self.manager.upgrade() {
            Some(manager) => {
                manager.emit(event);
            }
            _ => {
                trace!("Could not emit an event. AdapterManager has been dropped");
            }
        }
    }
}

struct ReclaimOperationGuard {
    owner: Arc<super::internal::PeerSourceOwner>,
    effectful: bool,
    settled: bool,
    failed: bool,
}

struct RequestCancellationGuard(Arc<super::internal::NativeRequestAdmission>);

impl Drop for RequestCancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Drop for ReclaimOperationGuard {
    fn drop(&mut self) {
        let mut state = self.owner.waiters.lock().expect("CoreBluetooth peer owner lock poisoned");
        state.reclaim_operations -= 1;
        if !self.settled || (self.effectful && self.failed) {
            state.reclaim_uncertain = true;
        }
    }
}

fn retired_graph_identity(old: Option<(String, u64, u64)>, new: Option<&(String, u64, u64)>) -> Option<(String, u64, u64)> {
    old.filter(|identity| Some(identity) != new)
}

struct ReleaseSubmissionGuard {
    owner: Arc<super::internal::PeerSourceOwner>,
    key: crate::ubm_reclaim_authority::GraphKey,
    dispatched: bool,
    completed: bool,
}
impl Drop for ReleaseSubmissionGuard {
    fn drop(&mut self) {
        let mut state = self.owner.waiters.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if (!self.dispatched || self.completed) && state.reclaim_release_pending.as_ref() == Some(&self.key) {
            state.reclaim_release_pending = None;
        }
    }
}

impl Peripheral {
    async fn own_reclaim_operation<T, F>(&self, effectful: bool, future: F) -> Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        self.own_reclaim_operation_with_admission(effectful, None, future).await
    }

    async fn own_reclaim_operation_with_admission<T, F>(
        &self, effectful: bool, admission: Option<Arc<super::internal::NativeRequestAdmission>>, future: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        {
            let owner = &self.shared.source_owner;
            let _adapter = owner.adapter_admission.lock().map_err(|_| Error::Other("CoreBluetooth adapter admission lock poisoned".into()))?;
            let mut state = owner.waiters.lock().map_err(|_| Error::Other("CoreBluetooth peer owner lock poisoned".into()))?;
            if owner.adapter_pending.load(std::sync::atomic::Ordering::Acquire) != 0 || state.reclaim_held || state.reclaim_gate.as_ref().is_some_and(|gate| gate.is_busy()) || state.reclaim_operations >= 256 {
                return Err(Error::Other("CoreBluetooth graph is held or operation capacity exhausted".into()));
            }
            state.reclaim_operations += 1;
        }
        let guard = ReclaimOperationGuard { owner: self.shared.source_owner.clone(), effectful, settled: false, failed: false };
        tokio::spawn(async move {
            let mut guard = guard;
            let result = if let Some(admission) = &admission {
                // Poll cancellation first: a cancelled, never-polled task must
                // not submit a message. A claim prevents cancellation winning,
                // so claimed work continues to wait for its actual reply.
                let cancelled = admission.cancelled();
                futures::pin_mut!(cancelled, future);
                match futures::future::select(cancelled, future).await {
                    futures::future::Either::Left(_) => Err(Error::RuntimeError("CoreBluetooth request cancelled before dispatch".to_owned())),
                    futures::future::Either::Right((result, _)) => result,
                }
            } else {
                future.await
            };
            // Cancellation before claim or an actor refusal before native
            // dispatch proves that this operation had no native effect.
            guard.failed = result.is_err() && !admission.as_ref().is_some_and(|admission| admission.has_no_effect());
            guard.settled = true;
            result
        }).await.map_err(|error| Error::Other(format!("CoreBluetooth graph operation task failed: {error}").into()))?
    }

    /// The caller holds the central connection admission through discovery commit.
    pub fn capture_reclaim_graph_binding(
        &self, peer_id: &str, connection_generation: &str, expected: &(String, u64, u64),
    ) -> std::result::Result<crate::ubm_reclaim_graph_binding::GraphBinding, crate::common::util::SourceGroupError> {
        let state = self.shared.source_owner.waiters.lock().map_err(|_| crate::common::util::SourceGroupError::SourceStateUnavailable)?;
        if !state.ready || state.revision_exhausted || expected.0 != "corebluetooth"
            || self.shared.source_owner.graph_attachment != Some(expected.1) || state.revision != expected.2 {
            return Err(crate::common::util::SourceGroupError::StaleGeneration);
        }
        state.reclaim_binding.capture(peer_id.to_owned(), connection_generation.to_owned(), expected)
            .ok_or(crate::common::util::SourceGroupError::StaleGeneration)
    }
    pub fn acquire_reclaim(&self, binding: &crate::ubm_reclaim_graph_binding::GraphBinding, key: crate::ubm_reclaim_authority::GraphKey) -> std::result::Result<crate::ubm_reclaim_authority::PermitStatus, crate::ubm_reclaim_authority::AuthorityError> {
        self.shared.source_owner.acquire_reclaim(binding, key)
    }

    pub fn commit_reclaim(&self, key: &crate::ubm_reclaim_authority::GraphKey) -> std::result::Result<crate::ubm_reclaim_authority::PermitStatus, crate::ubm_reclaim_authority::AuthorityError> {
        self.shared.source_owner.commit_reclaim(key)
    }

    pub fn forget_reclaim(&self, key: &crate::ubm_reclaim_authority::GraphKey) -> std::result::Result<(), crate::ubm_reclaim_authority::AuthorityError> {
        self.shared.source_owner.forget_reclaim(key)
    }

    /// Once queued, native release and its exact receipt outlive the caller.
    pub async fn release_reclaim(&self, key: &crate::ubm_reclaim_authority::GraphKey) -> std::result::Result<crate::ubm_reclaim_authority::PermitStatus, crate::ubm_reclaim_authority::AuthorityError> {
        use crate::ubm_reclaim_authority::{AuthorityError, PermitStatus};
        tokio::runtime::Handle::try_current().map_err(|_| AuthorityError::Unknown)?;
        let source_owner = self.shared.source_owner.clone();
        {
            let authorities = source_owner.authorities.lock().map_err(|_| AuthorityError::Conflict)?;
            let mut state = source_owner.waiters.lock().map_err(|_| AuthorityError::Conflict)?;
            authorities.authenticate_permit_owner(key, &state.reclaim_binding)?;
            let status = authorities.permit_status(key)?;
            if status == PermitStatus::Released && !state.reclaim_gate.as_ref().is_some_and(|gate| gate.is_draining()) {
                if state.reclaim_release_pending.as_ref() == Some(key) { state.reclaim_release_pending = None; }
                return Ok(status);
            }
            if state.reclaim_release_pending.is_some() { return Err(AuthorityError::Pending); }
            state.reclaim_release_pending = Some(key.clone());
        }
        let mut sender = self.shared.message_sender.clone();
        let peripheral_uuid = self.shared.uuid;
        let key = key.clone();
        let guard = ReleaseSubmissionGuard { owner: source_owner.clone(), key: key.clone(), dispatched: false, completed: false };
        tokio::spawn(async move {
            let mut guard = guard;
            let (result, reply) = tokio::sync::oneshot::channel();
            sender.send(CoreBluetoothMessage::ReleaseReclaim { peripheral_uuid, source_owner, key, result }).await.map_err(|_| AuthorityError::Unknown)?;
            guard.dispatched = true;
            let answer = reply.await.map_err(|_| AuthorityError::Pending)?;
            guard.completed = true;
            answer
        }).await.map_err(|_| AuthorityError::Pending)?
    }

    /// Queue a transaction to the native actor. Once accepted, the actor owns
    /// it even if the initiating waiter is cancelled.
    pub async fn start_ordered_marker_write(
        &self,
        group: &crate::common::util::SourceGroupHandle,
        expected: &(String, u64, u64),
        characteristic: &Characteristic,
        request: crate::common::util::SourceWriteRequest,
    ) -> std::result::Result<
        crate::common::util::SourceWriteReceipt,
        crate::common::util::SourceGroupError,
    > {
        let route = crate::ubm_peer_notification_ingress::NotificationRoute {
            service_uuid: characteristic.service_uuid,
            service_instance: characteristic.service_instance,
            characteristic_uuid: characteristic.uuid,
            characteristic_instance: characteristic.instance,
        };
        if route != request.route
            || request.graph != crate::common::util::marker_graph_identity(expected)
        {
            return Err(crate::common::util::SourceGroupError::TransactionConflict);
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| crate::common::util::SourceGroupError::SourceStateUnavailable)?;
        let (receipt, reserved) = group.reserve_marker_write(request.clone())?;
        if reserved {
            let group = group.clone();
            let mut sender = self.shared.message_sender.clone();
            let peripheral_uuid = self.shared.uuid;
            let expected = expected.clone();
            runtime.spawn(async move {
                let retained_request = request.clone();
                let (result, reply) = tokio::sync::oneshot::channel();
                if sender.send(CoreBluetoothMessage::MarkerWrite { peripheral_uuid, expected, group: group.clone(), request, result }).await.is_err() {
                    if let Err(error) = group.settle_pending_not_admitted(&retained_request, crate::common::util::SourceGroupError::SourceStateUnavailable) {
                        log::warn!("CoreBluetooth marker submission retirement failed: {error:?}");
                    }
                    return;
                }
                match reply.await {
                    Ok(Ok(_)) => {},
                    Ok(Err(error)) => log::warn!("CoreBluetooth marker actor failed: {error:?}"),
                    Err(_) => log::warn!("CoreBluetooth marker actor reply lost; retained transaction remains queryable"),
                }
            });
        }
        Ok(receipt)
    }
    // This calls tokio::task::spawn, so it must be called from the context of a Tokio Runtime.
    /// UBM patch (UBM_PATCHES.md #4): CoreBluetooth's current
    /// `canSendWriteWithoutResponse` for this connection.
    pub async fn can_send_write_without_response(&self) -> Result<bool> {
        let fut = CoreBluetoothReplyFuture::default();
        self.shared
            .message_sender
            .to_owned()
            .send(CoreBluetoothMessage::GetWriteReadiness {
                peripheral_uuid: self.shared.uuid,
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::WriteReadiness(ready) => Ok(ready),
            CoreBluetoothReply::Err(msg) => Err(Error::RuntimeError(msg)),
            // UBM patch (UBM_PATCHES.md #15): the platform's own answer.
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            reply => Err(Error::RuntimeError(format!(
                "Unexpected reply for can_send_write_without_response: {reply:?}"
            ))),
        }
    }

    /// UBM patch (UBM_PATCHES.md #14): read a characteristic and report
    /// what CoreBluetooth can say the value is. CoreBluetooth reports read
    /// responses and notifications through one callback, so while the
    /// characteristic can notify the value is
    /// [`ReadProvenance::ReadOrNotification`](crate::api::ReadProvenance);
    /// otherwise it is the read response. Pending reads complete in request
    /// order.
    pub async fn read_with_provenance(
        &self,
        characteristic: &Characteristic,
    ) -> Result<(Vec<u8>, crate::api::ReadProvenance)> {
        let fut = CoreBluetoothReplyFuture::default();
        self.shared
            .message_sender
            .to_owned()
            .send(CoreBluetoothMessage::ReadValue {
                peripheral_uuid: self.shared.uuid,
                service_uuid: AttrKey {
                    uuid: characteristic.service_uuid,
                    instance: characteristic.service_instance,
                },
                characteristic_uuid: AttrKey {
                    uuid: characteristic.uuid,
                    instance: characteristic.instance,
                },
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::CharacteristicRead(value, provenance) => Ok((value, provenance)),
            CoreBluetoothReply::Err(msg) => Err(Error::RuntimeError(msg)),
            // UBM patch (UBM_PATCHES.md #15): the platform's own answer.
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            reply => Err(Error::RuntimeError(format!(
                "Unexpected reply for a characteristic read: {reply:?}"
            ))),
        }
    }

    /// UBM patch (UBM_PATCHES.md #4): readiness reported each time
    /// CoreBluetooth calls `peripheralIsReadyToSendWriteWithoutResponse:`,
    /// read after the queued writes drained (a lagging receiver sees
    /// `RecvError::Lagged`, never a silent gap).
    pub fn write_readiness_events(&self) -> broadcast::Receiver<bool> {
        self.shared.write_readiness.subscribe()
    }

    /// UBM patch (UBM_PATCHES.md #2): the latest advertisement's extras.
    pub fn advertisement_extras(&self) -> AdvertisementExtras {
        self.shared.advertisement_extras.lock().unwrap().clone()
    }

    /// UBM patch (UBM_PATCHES.md #1): the largest single write CoreBluetooth
    /// accepts on this connection, `(with response, without response)`,
    /// from `-[CBPeripheral maximumWriteValueLengthForType:]`. Those lengths
    /// are write capacities. They are not stored as an ATT MTU: a
    /// with-response length can include a long write.
    pub async fn maximum_write_value_lengths(&self) -> Result<(u16, u16)> {
        let fut = CoreBluetoothReplyFuture::default();
        self.shared
            .message_sender
            .to_owned()
            .send(CoreBluetoothMessage::GetWriteLengths {
                peripheral_uuid: self.shared.uuid,
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::WriteLengths {
                with_response,
                without_response,
            } => {
                let with_response = u16::try_from(with_response).unwrap_or(u16::MAX);
                let without_response = u16::try_from(without_response).unwrap_or(u16::MAX);
                Ok((with_response, without_response))
            }
            CoreBluetoothReply::Err(msg) => Err(Error::RuntimeError(msg)),
            // UBM patch (UBM_PATCHES.md #15): the platform's own answer.
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            reply => Err(Error::RuntimeError(format!(
                "Unexpected reply for maximum_write_value_lengths: {reply:?}"
            ))),
        }
    }

    /// Capture the exact native object before enabling notifications.
    pub async fn notification_activation_owner(
        &self,
        characteristic: &Characteristic,
    ) -> Result<NotificationActivationOwner> {
        let future = CoreBluetoothReplyFuture::default();
        self.shared
            .message_sender
            .clone()
            .send(CoreBluetoothMessage::AcquireNotificationOwner {
                peripheral_uuid: self.shared.uuid,
                service_uuid: AttrKey {
                    uuid: characteristic.service_uuid,
                    instance: characteristic.service_instance,
                },
                characteristic_uuid: AttrKey {
                    uuid: characteristic.uuid,
                    instance: characteristic.instance,
                },
                future: future.get_state_clone(),
            })
            .await?;
        match future.await {
            CoreBluetoothReply::NotificationOwner(token) => Ok(NotificationActivationOwner {
                token,
                peripheral_uuid: self.shared.uuid,
                sender: self.shared.message_sender.clone(),
                source_owner: self.shared.source_owner.clone(),
                route: crate::ubm_peer_notification_ingress::NotificationRoute {
                    service_uuid: characteristic.service_uuid,
                    service_instance: characteristic.service_instance,
                    characteristic_uuid: characteristic.uuid,
                    characteristic_instance: characteristic.instance,
                },
                prepared: Arc::new(Mutex::new(None)),
            }),
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            CoreBluetoothReply::Err(error) => Err(Error::RuntimeError(error)),
            reply => Err(Error::RuntimeError(format!(
                "Unexpected notification owner reply: {reply:?}"
            ))),
        }
    }

    /// The generation currently owned by the native notification source.
    pub fn current_notification_source_generation(
        &self,
    ) -> std::result::Result<Option<u64>, crate::common::util::SourceGroupError> {
        self.shared
            .source_owner
            .waiters
            .lock()
            .map(|state| state.generation)
            .map_err(|_| crate::common::util::SourceGroupError::SourceStateUnavailable)
    }

    pub fn start_notification_source_generation(
        &self,
        generation: u64,
    ) -> std::result::Result<(), crate::common::util::SourceGroupError> {
        let mut state = self.shared.source_owner.waiters.lock().unwrap();
        if !state.ready || state.generation.is_some() {
            return Err(crate::common::util::SourceGroupError::StaleGeneration);
        }
        self.shared
            .source_owner
            .faults
            .start_source_generation(generation)?;
        state.generation = Some(generation);
        Ok(())
    }

    pub fn open_notification_source_group(
        &self,
        generation: u64,
        max_records: usize,
        max_bytes: usize,
    ) -> std::result::Result<
        (
            crate::common::util::SourceGroupHandle,
            crate::common::util::SourceGroupConsumer,
        ),
        crate::common::util::SourceGroupError,
    > {
        let state = self.shared.source_owner.waiters.lock().unwrap();
        if !state.ready || state.generation != Some(generation) {
            return Err(crate::common::util::SourceGroupError::StaleGeneration);
        }
        self.shared
            .source_owner
            .faults
            .open_source_group(generation, max_records, max_bytes)
    }

    /// Identity of the graph published under the native delegate's revision fence.
    pub fn accepted_gatt_graph_identity(&self) -> Result<Option<(String, u64, u64)>> {
        let state = self
            .shared
            .source_owner
            .waiters
            .lock()
            .map_err(|_| Error::Other("CoreBluetooth graph owner lock poisoned".into()))?;
        let attachment = self
            .shared
            .source_owner
            .graph_attachment
            .ok_or_else(|| Error::Other("CoreBluetooth graph attachment exhausted".into()))?;
        if state.revision_exhausted {
            return Err(Error::Other(
                "CoreBluetooth graph revision exhausted".into(),
            ));
        }
        Ok(state
            .ready
            .then(|| ("corebluetooth".to_owned(), attachment, state.revision)))
    }

    pub fn gatt_graph_snapshot(
        &self,
    ) -> Result<(
        BTreeSet<Service>,
        Vec<(Uuid, u64, &'static str)>,
        (String, u64, u64),
    )> {
        let state = self
            .shared
            .source_owner
            .waiters
            .lock()
            .map_err(|_| Error::Other("CoreBluetooth graph owner lock poisoned".into()))?;
        let attachment = self
            .shared
            .source_owner
            .graph_attachment
            .ok_or_else(|| Error::Other("CoreBluetooth graph attachment exhausted".into()))?;
        if !state.ready || state.revision_exhausted {
            return Err(Error::Other("CoreBluetooth graph is not current".into()));
        }
        let services = self
            .shared
            .services
            .lock()
            .map_err(|_| Error::Other("CoreBluetooth services lock poisoned".into()))?
            .clone();
        Ok((
            services,
            Vec::new(),
            ("corebluetooth".to_owned(), attachment, state.revision),
        ))
    }

    /// Compare the native graph and install the group under the delegate owner lock.
    pub fn open_notification_source_group_checked(
        &self,
        generation: u64,
        max_records: usize,
        max_bytes: usize,
        expected: &(String, u64, u64),
    ) -> std::result::Result<
        (
            crate::common::util::SourceGroupHandle,
            crate::common::util::SourceGroupConsumer,
        ),
        crate::common::util::SourceGroupError,
    > {
        let state = self
            .shared
            .source_owner
            .waiters
            .lock()
            .map_err(|_| crate::common::util::SourceGroupError::SourceStateUnavailable)?;
        if !state.ready
            || state.revision_exhausted
            || state.generation != Some(generation)
            || expected.0 != "corebluetooth"
            || self.shared.source_owner.graph_attachment != Some(expected.1)
            || state.revision != expected.2
        {
            return Err(crate::common::util::SourceGroupError::StaleGeneration);
        }
        self.shared
            .source_owner
            .faults
            .open_source_group(generation, max_records, max_bytes)
    }

    pub fn register_notification_source_route(
        &self,
        group: &crate::common::util::SourceGroupHandle,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        member: u64,
    ) -> std::result::Result<
        crate::ubm_peer_notification_ingress::Membership,
        crate::common::util::SourceGroupError,
    > {
        self.register_notification_source_route_inner(group, route, member, None)
    }

    pub fn register_notification_source_route_for_activation(
        &self,
        group: &crate::common::util::SourceGroupHandle,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        member: u64,
        activation: u64,
    ) -> std::result::Result<
        crate::ubm_peer_notification_ingress::Membership,
        crate::common::util::SourceGroupError,
    > {
        self.register_notification_source_route_inner(group, route, member, Some(activation))
    }

    fn register_notification_source_route_inner(
        &self,
        group: &crate::common::util::SourceGroupHandle,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        member: u64,
        activation: Option<u64>,
    ) -> std::result::Result<
        crate::ubm_peer_notification_ingress::Membership,
        crate::common::util::SourceGroupError,
    > {
        let _wake_scope = self.shared.source_owner.faults.defer_wakes();
        let state = self.shared.source_owner.waiters.lock().unwrap();
        if !state.ready {
            return Err(crate::common::util::SourceGroupError::StaleGeneration);
        }
        let services = self
            .shared
            .services
            .lock()
            .map_err(|_| crate::common::util::SourceGroupError::SourceStateUnavailable)?;
        if !services.iter().any(|service| {
            service.uuid == route.service_uuid
                && service.instance == route.service_instance
                && service.characteristics.iter().any(|characteristic| {
                    characteristic.uuid == route.characteristic_uuid
                        && characteristic.instance == route.characteristic_instance
                })
        }) {
            return Err(crate::common::util::SourceGroupError::SourceStateUnavailable);
        }
        match activation {
            Some(id) => self
                .shared
                .source_owner
                .faults
                .register_source_route_for_activation(group, route, member, id),
            None => self
                .shared
                .source_owner
                .faults
                .register_source_route(group, route, member),
        }
    }

    pub fn retire_notification_source_route(
        &self,
        group: &crate::common::util::SourceGroupHandle,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        membership: crate::ubm_peer_notification_ingress::Membership,
    ) -> std::result::Result<(), crate::common::util::SourceGroupError> {
        let _wake_scope = self.shared.source_owner.faults.defer_wakes();
        let _state = self.shared.source_owner.waiters.lock().unwrap();
        self.shared
            .source_owner
            .faults
            .retire_source_route(group, route, membership)
    }

    pub fn notification_source_barrier(
        &self,
        group: &crate::common::util::SourceGroupHandle,
        id: u64,
    ) -> std::result::Result<(), crate::common::util::SourceGroupError> {
        let _wake_scope = self.shared.source_owner.faults.defer_wakes();
        let _state = self.shared.source_owner.waiters.lock().unwrap();
        self.shared
            .source_owner
            .faults
            .source_group_barrier(group, id)
    }

    pub fn detach_notification_source_group_after_barrier(
        &self,
        group: &crate::common::util::SourceGroupHandle,
        barrier_id: u64,
    ) -> std::result::Result<(), crate::common::util::SourceGroupError> {
        let _wake_scope = self.shared.source_owner.faults.defer_wakes();
        let _state = self.shared.source_owner.waiters.lock().unwrap();
        self.shared
            .source_owner
            .faults
            .detach_source_group_after_barrier(group, barrier_id)
    }

    pub fn detach_notification_source_group(
        &self,
        group: &crate::common::util::SourceGroupHandle,
    ) -> std::result::Result<(), crate::common::util::SourceGroupError> {
        let _wake_scope = self.shared.source_owner.faults.defer_wakes();
        let _state = self.shared.source_owner.waiters.lock().unwrap();
        self.shared.source_owner.faults.detach_source_group(group)
    }

    #[cfg(test)]
    pub(super) fn source_owner(&self) -> &Arc<super::internal::PeerSourceOwner> {
        &self.shared.source_owner
    }

    #[cfg(test)]
    pub(crate) fn new(
        uuid: Uuid,
        local_name: Option<String>,
        advertisement_name: Option<String>,
        manager: Weak<AdapterManager<Self>>,
        event_receiver: Receiver<PeripheralEventInternal>,
        message_sender: Sender<CoreBluetoothMessage>,
    ) -> Self {
        Self::new_with_source_owner(
            uuid,
            local_name,
            advertisement_name,
            manager,
            event_receiver,
            message_sender,
            super::internal::PeerSourceOwner::new(),
        )
    }

    pub(super) fn new_with_source_owner(
        uuid: Uuid,
        local_name: Option<String>,
        advertisement_name: Option<String>,
        manager: Weak<AdapterManager<Self>>,
        event_receiver: Receiver<PeripheralEventInternal>,
        message_sender: Sender<CoreBluetoothMessage>,
        source_owner: Arc<super::internal::PeerSourceOwner>,
    ) -> Self {
        // Since we're building the object, we have an active advertisement.
        // Build properties now.
        let properties = Mutex::from(PeripheralProperties {
            address: BDAddr::default(),
            address_type: None,
            local_name,
            advertisement_name,
            tx_power_level: None,
            rssi: None,
            manufacturer_data: HashMap::new(),
            service_data: HashMap::new(),
            services: Vec::new(),
            class: None,
        });
        let (notifications_channel, _) = broadcast::channel(crate::ubm::EVENT_CAPACITY);

        let notification_faults = Arc::new(NotificationFaults::new(crate::ubm::EVENT_CAPACITY));
        *source_owner.legacy.lock().unwrap() =
            Some((notification_faults.clone(), notifications_channel.clone()));
        let shared = Arc::new(Shared {
            source_owner,
            properties,
            manager,
            services: Mutex::new(BTreeSet::new()),
            notifications_channel,
            notification_faults,
            legacy_claims: tokio::sync::Mutex::new(HashMap::new()),
            uuid,
            message_sender,
            mtu: AtomicU16::new(crate::api::DEFAULT_MTU_SIZE),
            advertisement_extras: Mutex::new(AdvertisementExtras::default()),
            write_readiness: broadcast::channel(16).0,
        });
        let shared_clone = shared.clone();
        task::spawn(async move {
            let mut event_receiver = event_receiver;
            let shared = shared_clone;

            loop {
                match event_receiver.next().await {
                    Some(PeripheralEventInternal::Notification(characteristic, service, data)) => {
                        let notification = ValueNotification {
                            uuid: characteristic.uuid,
                            instance: characteristic.instance,
                            service_uuid: service.uuid,
                            service_instance: service.instance,
                            value: data,
                            source_failure: None,
                            lost_before: 0,
                        };

                        // Note: we ignore send errors here which may happen while there are no
                        // receivers...
                        shared
                            .notification_faults
                            .publish(&shared.notifications_channel, notification);
                    }
                    Some(PeripheralEventInternal::NotificationFailed(
                        characteristic,
                        service,
                        error,
                    )) => {
                        shared.notification_faults.publish(
                            &shared.notifications_channel,
                            ValueNotification {
                                uuid: characteristic.uuid,
                                instance: characteristic.instance,
                                service_uuid: service.uuid,
                                service_instance: service.instance,
                                value: Vec::new(),
                                source_failure: Some(error),
                                lost_before: 0,
                            },
                        );
                    }
                    Some(PeripheralEventInternal::ManufacturerData(
                        manufacturer_id,
                        data,
                        rssi,
                    )) => {
                        let mut properties = shared.properties.lock().unwrap();
                        properties.rssi = Some(rssi);
                        properties
                            .manufacturer_data
                            .insert(manufacturer_id, data.clone());
                        shared.emit_event(CentralEvent::ManufacturerDataAdvertisement {
                            id: shared.uuid.into(),
                            manufacturer_data: properties.manufacturer_data.clone(),
                        });
                    }
                    Some(PeripheralEventInternal::ServiceData(service_data, rssi)) => {
                        let mut properties = shared.properties.lock().unwrap();
                        properties.rssi = Some(rssi);
                        properties.service_data.extend(service_data.clone());

                        shared.emit_event(CentralEvent::ServiceDataAdvertisement {
                            id: shared.uuid.into(),
                            service_data,
                        });
                    }
                    Some(PeripheralEventInternal::Services(services, rssi)) => {
                        let mut properties = shared.properties.lock().unwrap();
                        properties.rssi = Some(rssi);
                        properties.services = services.clone();

                        shared.emit_event(CentralEvent::ServicesAdvertisement {
                            id: shared.uuid.into(),
                            services,
                        });
                    }
                    Some(PeripheralEventInternal::ServicesModified) => {
                        shared.services.lock().unwrap().clear();
                        shared.emit_event(CentralEvent::DeviceServicesModified(shared.uuid.into()));
                    }
                    Some(PeripheralEventInternal::WriteReadiness(ready)) => {
                        // No receiver is not a failure: readiness stays
                        // readable through `can_send_write_without_response`.
                        let _ = shared.write_readiness.send(ready);
                    }
                    Some(PeripheralEventInternal::AdvertisementExtras(extras)) => {
                        *shared.advertisement_extras.lock().unwrap() = extras;
                    }
                    Some(PeripheralEventInternal::TxPowerLevel(tx_power_level)) => {
                        let mut properties = shared.properties.lock().unwrap();
                        properties.tx_power_level = Some(tx_power_level);
                    }
                    Some(PeripheralEventInternal::RssiRead(rssi)) => {
                        shared.emit_event(CentralEvent::RssiUpdate {
                            id: shared.uuid.into(),
                            rssi,
                        });
                    }
                    Some(PeripheralEventInternal::NotificationSourceRetired(
                        characteristic,
                        service,
                        ack,
                    )) => {
                        shared.notification_faults.clear_key((
                            service.uuid,
                            service.instance,
                            characteristic.uuid,
                            characteristic.instance,
                        ));
                        let _ = ack.send(());
                    }
                    Some(PeripheralEventInternal::Disconnected) => {
                        shared.notification_faults.clear_all()
                    }
                    None => {
                        info!("Event receiver died, breaking out of corebluetooth device loop.");
                        break;
                    }
                }
            }
        });
        Self { shared: shared }
    }

    pub(super) fn update_name(
        &self,
        local_name: Option<String>,
        advertisement_name: Option<String>,
    ) {
        if let Ok(mut props) = self.shared.properties.lock() {
            let (local, advertised) = fold_discovery_name(
                props.local_name.clone(),
                props.advertisement_name.clone(),
                advertisement_name,
                local_name,
            );
            props.local_name = local;
            props.advertisement_name = advertised;
        }
    }
}

/// UBM patch (UBM_PATCHES.md #17, finding 205): fold one rediscovery into
/// the merged scan name. The merged name tracks the air: the current
/// advertisement's name wins (a rename included); a nameless rediscovery
/// keeps the last advertised name instead of falling back to the cached GAP
/// name. The GAP name seeds a peripheral no advertisement has named yet —
/// without it a nameless beacon would stay nameless — but it never
/// overwrites an advertised one: after a connect resolves the GAP name (the
/// sim's Linux controller name, not its advertised name), every later
/// nameless sighting would otherwise poison the merged name, and with it
/// every scan observation filtered on it.
pub(crate) fn fold_discovery_name(
    previous: Option<String>,
    previous_advertised: Option<String>,
    advertisement_name: Option<String>,
    gap_name: Option<String>,
) -> (Option<String>, Option<String>) {
    let advertised = advertisement_name.or(previous_advertised);
    let local = advertised.clone().or(previous).or(gap_name);
    (local, advertised)
}

#[cfg(test)]
mod ubm_fold_name_tests {
    use super::fold_discovery_name;

    fn owned(value: &str) -> Option<String> {
        Some(value.to_owned())
    }

    #[test]
    fn a_first_named_sighting_reports_the_advertised_name() {
        assert_eq!(
            fold_discovery_name(None, None, owned("Polar H10 SIM0001"), owned("lx5090")),
            (owned("Polar H10 SIM0001"), owned("Polar H10 SIM0001"))
        );
    }

    #[test]
    fn a_first_nameless_sighting_is_seeded_from_the_gap_name() {
        assert_eq!(
            fold_discovery_name(None, None, None, owned("lx5090")),
            (owned("lx5090"), None)
        );
        assert_eq!(fold_discovery_name(None, None, None, None), (None, None));
    }

    #[test]
    fn a_renamed_advertisement_replaces_the_merged_name() {
        assert_eq!(
            fold_discovery_name(
                owned("Polar H10 SIM0043"),
                owned("Polar H10 SIM0043"),
                owned("Polar H10 SIM0044"),
                owned("lx5090"),
            ),
            (owned("Polar H10 SIM0044"), owned("Polar H10 SIM0044"))
        );
    }

    #[test]
    fn a_nameless_rediscovery_keeps_the_advertised_name_despite_the_gap_name() {
        // Finding 205: after a connect resolves the GAP name, nameless
        // sightings must not poison the merged name back to it.
        assert_eq!(
            fold_discovery_name(
                owned("Polar H10 SIM0001"),
                owned("Polar H10 SIM0001"),
                None,
                owned("lx5090"),
            ),
            (owned("Polar H10 SIM0001"), owned("Polar H10 SIM0001"))
        );
    }

    #[test]
    fn a_nameless_rediscovery_of_a_never_named_peer_keeps_the_gap_seed() {
        assert_eq!(
            fold_discovery_name(owned("lx5090"), None, None, owned("lx5090")),
            (owned("lx5090"), None)
        );
    }
}

impl Display for Peripheral {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        // let connected = if self.is_connected() { " connected" } else { "" };
        // let properties = self.properties.lock().unwrap();
        // write!(f, "{} {}{}", self.address, properties.local_name.clone()
        //     .unwrap_or_else(|| "(unknown)".to_string()), connected)
        write!(f, "Peripheral")
    }
}

impl Debug for Peripheral {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("Peripheral")
            .field("uuid", &self.shared.uuid)
            .field("services", &self.shared.services)
            .field("properties", &self.shared.properties)
            .field("message_sender", &self.shared.message_sender)
            .finish()
    }
}

#[async_trait]
impl api::Peripheral for Peripheral {
    fn id(&self) -> PeripheralId {
        PeripheralId(self.shared.uuid)
    }

    fn address(&self) -> BDAddr {
        BDAddr::default()
    }

    fn mtu(&self) -> u16 {
        self.shared.mtu.load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn properties(&self) -> Result<Option<PeripheralProperties>> {
        Ok(Some(
            self.shared
                .properties
                .lock()
                .map_err(Into::<Error>::into)?
                .clone(),
        ))
    }

    fn services(&self) -> BTreeSet<Service> {
        self.shared.services.lock().unwrap().clone()
    }

    async fn is_connected(&self) -> Result<bool> {
        let fut = CoreBluetoothReplyFuture::default();
        self.shared
            .message_sender
            .to_owned()
            .send(CoreBluetoothMessage::IsConnected {
                peripheral_uuid: self.shared.uuid,
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::State(state) => match state {
                CBPeripheralState::Connected => Ok(true),
                _ => Ok(false),
            },
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            _ => panic!("Shouldn't get anything but a State!"),
        }
    }

    async fn connect(&self) -> Result<()> {
        let peripheral = self.clone();
        let admission = Arc::new(super::internal::NativeRequestAdmission::default());
        let _cancellation = RequestCancellationGuard(admission.clone());
        self.own_reclaim_operation_with_admission(true, Some(admission.clone()), async move {
        let fut = CoreBluetoothReplyFuture::default();
        peripheral.shared
            .message_sender
            .to_owned()
            .send(CoreBluetoothMessage::ConnectDevice {
                identity: super::internal::NativeRequestIdentity::new(
                    peripheral.shared.uuid,
                    peripheral.shared.source_owner.clone(),
                    admission,
                ),
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::Connected => {
                peripheral.shared
                    .emit_event(CentralEvent::DeviceConnected(peripheral.shared.uuid.into()));
            }
            CoreBluetoothReply::Err(msg) => return Err(Error::RuntimeError(msg)),
            // UBM patch (UBM_PATCHES.md #15): the platform's own answer.
            CoreBluetoothReply::Failed(error) => return Err(Error::Platform(error)),
            _ => panic!("Shouldn't get anything but connected or err!"),
        }
        trace!("Device connected!");
        Ok(())

        }).await
    }

    async fn disconnect(&self) -> Result<()> {
        let peripheral = self.clone();
        let admission = Arc::new(super::internal::NativeRequestAdmission::default());
        let _cancellation = RequestCancellationGuard(admission.clone());
        self.own_reclaim_operation_with_admission(true, Some(admission.clone()), async move {
        let fut = CoreBluetoothReplyFuture::default();
        peripheral.shared
            .message_sender
            .to_owned()
            .send(CoreBluetoothMessage::DisconnectDevice {
                identity: super::internal::NativeRequestIdentity::new(
                    peripheral.shared.uuid,
                    peripheral.shared.source_owner.clone(),
                    admission,
                ),
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::Ok => {
                peripheral.shared.notification_faults.clear_all();
                // The actor publishes the native lifecycle observation once;
                // each waiter only consumes its own operation result.
                trace!("Device disconnected!");
            }
            CoreBluetoothReply::Failed(error) => return Err(Error::Platform(error)),
            _ => return Err(Error::RuntimeError("Unexpected disconnect reply".into())),
        }
        Ok(())

        }).await
    }

    async fn discover_services(&self) -> Result<()> {
        let peripheral = self.clone();
        self.own_reclaim_operation(false, async move {
        let revision = peripheral.shared.source_owner.waiters.lock().unwrap().revision;
        let fut = CoreBluetoothReplyFuture::default();
        peripheral.shared
            .message_sender
            .to_owned()
            .send(CoreBluetoothMessage::DiscoverServices {
                peripheral_uuid: peripheral.shared.uuid,
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::ServicesDiscovered(services) => {
                let mut state = peripheral.shared.source_owner.waiters.lock().unwrap();
                if state.revision_exhausted || state.revision != revision {
                    return Err(Error::RuntimeError(
                        "Service graph changed during discovery".into(),
                    ));
                }
                let Some(next_revision) = state.revision.checked_add(1) else {
                    state.revision_exhausted = true;
                    state.ready = false;
                    return Err(Error::Other(
                        "CoreBluetooth graph revision exhausted".into(),
                    ));
                };
                let retired = if state.ready {
                    peripheral.shared.source_owner.graph_attachment.map(|attachment| ("corebluetooth".to_owned(), attachment, state.revision))
                } else { None };
                *(peripheral.shared.services.lock().map_err(Into::<Error>::into)?) = services;
                state.revision = next_revision;
                state.ready = true;
                state.reclaim_binding.attach();
                let current = peripheral.shared.source_owner.graph_attachment.map(|attachment| ("corebluetooth".to_owned(), attachment, next_revision));
                if let Some(graph) = &current { state.reclaim_binding.publish(graph.clone()); }
                let retired = retired_graph_identity(retired, current.as_ref());
                drop(state);
                if let Some(graph) = retired {
                    peripheral.shared.emit_event(CentralEvent::DeviceGraphRetired { id: peripheral.shared.uuid.into(), graph });
                }
                return Ok(());
            }
            CoreBluetoothReply::Err(msg) => return Err(Error::RuntimeError(msg)),
            // UBM patch (UBM_PATCHES.md #15): the platform's own answer.
            CoreBluetoothReply::Failed(error) => return Err(Error::Platform(error)),
            _ => panic!("Shouldn't get anything but discovered or err!"),
        }

        }).await
    }

    async fn write(
        &self,
        characteristic: &Characteristic,
        data: &[u8],
        mut write_type: WriteType,
    ) -> Result<()> {
        let fut = CoreBluetoothReplyFuture::default();
        // If we get WriteWithoutResponse for a characteristic that only
        // supports WriteWithResponse, slam the type to WriteWithResponse.
        // Otherwise we won't handle the future correctly.
        if write_type == WriteType::WithoutResponse
            && !characteristic
                .properties
                .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
        {
            write_type = WriteType::WithResponse
        }
        self.shared
            .message_sender
            .to_owned()
            .send(CoreBluetoothMessage::WriteValue {
                peripheral_uuid: self.shared.uuid,
                service_uuid: AttrKey {
                    uuid: characteristic.service_uuid,
                    instance: characteristic.service_instance,
                },
                characteristic_uuid: AttrKey {
                    uuid: characteristic.uuid,
                    instance: characteristic.instance,
                },
                data: Vec::from(data),
                write_type,
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::Ok => {}
            CoreBluetoothReply::Err(msg) => return Err(Error::RuntimeError(msg)),
            // UBM patch (UBM_PATCHES.md #15): the platform's own answer.
            CoreBluetoothReply::Failed(error) => return Err(Error::Platform(error)),
            reply => panic!("Unexpected reply: {:?}", reply),
        }
        Ok(())
    }

    async fn read(&self, characteristic: &Characteristic) -> Result<Vec<u8>> {
        self.read_with_provenance(characteristic)
            .await
            .map(|(value, _)| value)
    }

    async fn subscribe(&self, characteristic: &Characteristic) -> Result<()> {
        let candidate = self.notification_activation_owner(characteristic).await?;
        let route = (
            AttrKey {
                uuid: characteristic.service_uuid,
                instance: characteristic.service_instance,
            },
            AttrKey {
                uuid: characteristic.uuid,
                instance: characteristic.instance,
            },
        );
        let mut claims = self.shared.legacy_claims.lock().await;
        let total_claims: usize = claims.values().map(Vec::len).sum();
        let existing = claims
            .get(&route)
            .and_then(|owners| {
                owners.iter().find(|owner| {
                    owner.token.revision == candidate.token.revision
                        && owner.identity() == candidate.identity()
                })
            })
            .cloned();
        let owner = if let Some(owner) = existing {
            owner
        } else {
            if total_claims >= 4096 {
                return Err(Error::Platform(crate::PlatformError::new(
                    "corebluetooth",
                    "notification-claim-capacity",
                    "Legacy notification claim capacity is full",
                )));
            }
            claims.entry(route).or_default().push(candidate.clone());
            candidate
        };
        owner.subscribe().await
    }

    async fn unsubscribe(&self, characteristic: &Characteristic) -> Result<()> {
        let route = (
            AttrKey {
                uuid: characteristic.service_uuid,
                instance: characteristic.service_instance,
            },
            AttrKey {
                uuid: characteristic.uuid,
                instance: characteristic.instance,
            },
        );
        let mut claims = self.shared.legacy_claims.lock().await;
        let Some(owners) = claims.get_mut(&route) else {
            return Ok(());
        };
        let Some(owner) = owners.first().cloned() else {
            claims.remove(&route);
            return Ok(());
        };
        owner.unsubscribe().await?;
        owners.remove(0);
        if owners.is_empty() {
            claims.remove(&route);
        }
        Ok(())
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
        let fut = CoreBluetoothReplyFuture::default();
        self.shared
            .message_sender
            .to_owned()
            .send(CoreBluetoothMessage::WriteDescriptorValue {
                peripheral_uuid: self.shared.uuid,
                service_uuid: AttrKey {
                    uuid: descriptor.service_uuid,
                    instance: descriptor.service_instance,
                },
                characteristic_uuid: AttrKey {
                    uuid: descriptor.characteristic_uuid,
                    instance: descriptor.characteristic_instance,
                },
                descriptor_uuid: AttrKey {
                    uuid: descriptor.uuid,
                    instance: descriptor.instance,
                },
                data: Vec::from(data),
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::Ok => {}
            // UBM patch (UBM_PATCHES.md #15): a failed descriptor write is
            // answered, never a panic.
            CoreBluetoothReply::Err(msg) => return Err(Error::RuntimeError(msg)),
            CoreBluetoothReply::Failed(error) => return Err(Error::Platform(error)),
            reply => panic!("Unexpected reply: {:?}", reply),
        }
        Ok(())
    }

    async fn read_rssi(&self) -> Result<i16> {
        let fut = CoreBluetoothReplyFuture::default();
        self.shared
            .message_sender
            .to_owned()
            .send(CoreBluetoothMessage::ReadRssi {
                peripheral_uuid: self.shared.uuid,
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::ReadRssi(rssi) => Ok(rssi),
            CoreBluetoothReply::Err(msg) => Err(Error::RuntimeError(msg)),
            // UBM patch (UBM_PATCHES.md #15): the platform's own answer.
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            _ => panic!("Unexpected reply for read_rssi"),
        }
    }

    async fn read_descriptor(&self, descriptor: &Descriptor) -> Result<Vec<u8>> {
        let fut = CoreBluetoothReplyFuture::default();
        self.shared
            .message_sender
            .to_owned()
            .send(CoreBluetoothMessage::ReadDescriptorValue {
                peripheral_uuid: self.shared.uuid,
                service_uuid: AttrKey {
                    uuid: descriptor.service_uuid,
                    instance: descriptor.service_instance,
                },
                characteristic_uuid: AttrKey {
                    uuid: descriptor.characteristic_uuid,
                    instance: descriptor.characteristic_instance,
                },
                descriptor_uuid: AttrKey {
                    uuid: descriptor.uuid,
                    instance: descriptor.instance,
                },
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::ReadResult(chars) => Ok(chars),
            // UBM patch (UBM_PATCHES.md #15): a failed descriptor read is
            // answered, never a panic.
            CoreBluetoothReply::Err(msg) => Err(Error::RuntimeError(msg)),
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            _ => {
                panic!("Shouldn't get anything but read result!");
            }
        }
    }
}

impl From<Uuid> for PeripheralId {
    fn from(uuid: Uuid) -> Self {
        PeripheralId(uuid)
    }
}

impl From<SendError> for Error {
    fn from(_: SendError) -> Self {
        Error::Other("Channel closed".to_string().into())
    }
}

#[cfg(test)]
mod notification_claim_tests {
    use super::super::internal::{NativeActivationIdentity, NativeActivationToken};
    use super::*;
    use crate::api::Peripheral as _;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    #[tokio::test]
    async fn repeated_disconnect_replies_do_not_publish_extra_lifecycle_events() {
        use futures::FutureExt;

        let manager = Arc::new(AdapterManager::<Peripheral>::default());
        let mut events = manager.event_stream();
        let (sender, mut requests) = futures::channel::mpsc::channel(8);
        let (_events, event_receiver) = futures::channel::mpsc::channel(8);
        let peer = Uuid::from_u128(42);
        let peripheral = Peripheral::new(
            peer, None, None, Arc::downgrade(&manager), event_receiver, sender,
        );
        let first = tokio::spawn({
            let peripheral = peripheral.clone();
            async move { peripheral.disconnect().await }
        });
        let second = tokio::spawn(async move { peripheral.disconnect().await });
        let CoreBluetoothMessage::DisconnectDevice { future: first_reply, .. } = requests.next().await.unwrap()
            else { panic!("expected disconnect") };
        let CoreBluetoothMessage::DisconnectDevice { future: second_reply, .. } = requests.next().await.unwrap()
            else { panic!("expected disconnect") };
        first_reply.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
        second_reply.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert!(
            events.next().now_or_never().is_none(),
            "waiters must not each publish a generation-free disconnect"
        );
    }

    fn fixture() -> (
        Peripheral,
        Arc<AtomicU64>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let (sender, mut receiver) = futures::channel::mpsc::channel(16);
        let (_events, event_receiver) = futures::channel::mpsc::channel(16);
        let peripheral =
            Peripheral::new(Uuid::nil(), None, None, Weak::new(), event_receiver, sender);
        let revision = Arc::new(AtomicU64::new(0));
        let enables = Arc::new(AtomicUsize::new(0));
        let disables = Arc::new(AtomicUsize::new(0));
        let identity = Arc::new(NativeActivationIdentity {
            id: 1,
            ownership: Default::default(),
        });
        let rev = revision.clone();
        let on = enables.clone();
        let off = disables.clone();
        tokio::spawn(async move {
            let mut next_claim = 0;
            while let Some(message) = receiver.next().await {
                match message {
                    CoreBluetoothMessage::AcquireNotificationOwner { future, .. } => {
                        let token = Arc::new(NativeActivationToken {
                            identity: identity.clone(),
                            revision: rev.load(Ordering::SeqCst),
                            claim_id: next_claim,
                        });
                        next_claim += 1;
                        future
                            .lock()
                            .unwrap()
                            .set_reply(CoreBluetoothReply::NotificationOwner(token));
                    }
                    CoreBluetoothMessage::SetNotificationOwner {
                        token,
                        enable,
                        future,
                        ..
                    } => {
                        let answer = if token.revision != rev.load(Ordering::SeqCst) {
                            CoreBluetoothReply::Failed(crate::PlatformError::new(
                                "corebluetooth",
                                "notification-owner-invalidated",
                                "invalidated",
                            ))
                        } else {
                            if enable {
                                on.fetch_add(1, Ordering::SeqCst);
                            } else {
                                off.fetch_add(1, Ordering::SeqCst);
                            }
                            CoreBluetoothReply::Ok
                        };
                        future.lock().unwrap().set_reply(answer);
                    }
                    _ => panic!("unexpected fixture command"),
                }
            }
        });
        (peripheral, revision, enables, disables)
    }

    #[tokio::test]
    async fn legacy_receiver_registration_is_atomic_with_publication_and_retains_overflow() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let (peripheral, _, _, _) = fixture();
        let mut existing = peripheral.notifications().await.unwrap();
        let shared = peripheral.shared.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let other = peripheral.clone();
        let (worker, receiver_count_while_locked) =
            shared.notification_faults.with_publication_lock_for_test(|| {
                let worker = std::thread::spawn(move || {
                    started_tx.send(()).unwrap();
                    futures::executor::block_on(other.notifications()).unwrap()
                });
                started_rx.recv().unwrap();
                let deadline = Instant::now() + Duration::from_millis(200);
                while shared.notifications_channel.receiver_count() == 1 && Instant::now() < deadline {
                    std::thread::yield_now();
                }
                (worker, shared.notifications_channel.receiver_count())
            });
        let mut added = worker.join().unwrap();
        assert_eq!(
            receiver_count_while_locked, 1,
            "receiver became visible before its terminal owner could register"
        );

        let hold = shared.notification_faults.hold_notification_delivery();
        for instance in 0..=crate::ubm::EVENT_CAPACITY {
            shared.notification_faults.publish(
                &shared.notifications_channel,
                ValueNotification {
                    uuid: Uuid::nil(),
                    instance: instance as u64,
                    service_uuid: Uuid::nil(),
                    service_instance: 1,
                    value: vec![1],
                    source_failure: None,
                    lost_before: 0,
                },
            );
        }
        shared.notification_faults.clear_all();
        drop(hold);
        tokio::time::timeout(Duration::from_secs(5), async {
            for stream in [&mut existing, &mut added] {
                for instance in 1..crate::ubm::EVENT_CAPACITY {
                    let value = stream.next().await.unwrap();
                    assert_eq!(value.instance, instance as u64);
                    assert_eq!(value.lost_before, u64::from(instance == 1));
                }
                let terminal = stream.next().await.unwrap();
                assert_eq!(terminal.source_failure.unwrap().code, "notification-broadcast-overflow");
            }
            shared.notification_faults.publish(&shared.notifications_channel, ValueNotification {
                uuid: Uuid::nil(), instance: 999, service_uuid: Uuid::nil(),
                service_instance: 1, value: vec![2], source_failure: None, lost_before: 0,
            });
            for stream in [&mut existing, &mut added] {
                let value = stream.next().await.expect("legacy stream remains active after graph clear");
                assert_eq!((value.instance, value.value), (999, vec![2]));
                assert!(value.source_failure.is_none());
                assert_eq!(value.lost_before, 0);
            }
        }).await.expect("both legacy streams deliver overflow and resume");
        drop(existing);
        drop(added);
        assert_eq!(shared.notifications_channel.receiver_count(), 0);
    }

    #[tokio::test]
    async fn replacement_notification_entrypoints_ignore_old_overflow_and_old_lag() {
        use std::time::Duration;
        use tokio::time::timeout;

        fn value(instance: u64) -> ValueNotification {
            ValueNotification {
                uuid: Uuid::nil(),
                instance,
                service_uuid: Uuid::nil(),
                service_instance: 1,
                value: vec![1],
                source_failure: None,
                lost_before: 0,
            }
        }

        let (peripheral, _, _, _) = fixture();
        let shared = peripheral.shared.clone();
        let _existing = peripheral.notifications().await.unwrap();
        let hold = shared.notification_faults.hold_notification_delivery();
        for instance in 0..=crate::ubm::EVENT_CAPACITY {
            shared.notification_faults.publish(
                &shared.notifications_channel,
                value(instance as u64),
            );
        }
        shared.notification_faults.clear_all();
        let mut values = peripheral.notifications().await.unwrap();
        let mut events = peripheral.notification_events().await.unwrap();
        drop(hold);
        shared.notification_faults.publish(&shared.notifications_channel, value(1000));
        assert_eq!(timeout(Duration::from_secs(5), values.next()).await.unwrap(), Some(value(1000)));
        assert_eq!(
            timeout(Duration::from_secs(5), events.next()).await.unwrap(),
            Some(api::NotificationEvent::Value(value(1000)))
        );

        for fresh_count in [1, crate::ubm::EVENT_CAPACITY + 1] {
            let (peripheral, _, _, _) = fixture();
            let shared = peripheral.shared.clone();
            let hold = shared.notification_faults.hold_notification_delivery();
            for instance in 0..crate::ubm::EVENT_CAPACITY {
                shared.notification_faults.publish(
                    &shared.notifications_channel,
                    value(instance as u64),
                );
            }
            shared.notification_faults.clear_all();
            let mut values = peripheral.notifications().await.unwrap();
            let mut events = peripheral.notification_events().await.unwrap();
            drop(hold);
            for instance in 0..fresh_count {
                shared.notification_faults.publish(
                    &shared.notifications_channel,
                    value(1000 + instance as u64),
                );
            }
            let expected_missed = fresh_count.saturating_sub(crate::ubm::EVENT_CAPACITY) as u64;
            if expected_missed != 0 {
                assert_eq!(
                    timeout(Duration::from_secs(5), events.next()).await.unwrap(),
                    Some(api::NotificationEvent::Lagged { skipped_records: expected_missed })
                );
            }
            let first = timeout(Duration::from_secs(5), values.next()).await.unwrap().unwrap();
            assert_eq!(
                (first.instance, first.lost_before),
                (1000 + expected_missed, expected_missed)
            );
            assert!(matches!(
                timeout(Duration::from_secs(5), events.next()).await.unwrap(),
                Some(api::NotificationEvent::Value(value)) if value.instance == first.instance
            ));
        }
    }

    #[tokio::test]
    async fn reclaim_read_only_failure_settles_but_aborted_task_is_uncertain() {
        let (peripheral, _, _, _) = fixture();
        let result: Result<()> = peripheral.own_reclaim_operation(false, async {
            Err(Error::Other("discovery failed".into()))
        }).await;
        assert!(result.is_err());
        {
            let state = peripheral.shared.source_owner.waiters.lock().unwrap();
            assert_eq!(state.reclaim_operations, 0);
            assert!(!state.reclaim_uncertain);
        }
        {
            let mut state = peripheral.shared.source_owner.waiters.lock().unwrap();
            state.reclaim_operations = 1;
        }
        let guard = ReclaimOperationGuard {
            owner: peripheral.shared.source_owner.clone(), effectful: false, settled: false, failed: false,
        };
        drop(guard);
        let state = peripheral.shared.source_owner.waiters.lock().unwrap();
        assert_eq!(state.reclaim_operations, 0);
        assert!(state.reclaim_uncertain);
    }

    #[test]
    fn unchanged_graph_identity_does_not_retire() {
        let graph = ("corebluetooth".to_owned(), 1, 2);
        assert_eq!(retired_graph_identity(Some(graph.clone()), Some(&graph)), None);
        let next = ("corebluetooth".to_owned(), 1, 3);
        assert_eq!(retired_graph_identity(Some(graph.clone()), Some(&next)), Some(graph));
    }

    #[tokio::test]
    async fn rediscovery_after_caller_drop_reports_exact_retired_graph_after_unlock() {
        use std::sync::atomic::AtomicBool;
        use std::task::{Context, Poll, Wake, Waker};
        struct CheckWake { owner: Arc<super::super::internal::PeerSourceOwner>, unlocked: AtomicBool, called: AtomicBool }
        impl Wake for CheckWake {
            fn wake(self: Arc<Self>) { self.unlocked.store(self.owner.waiters.try_lock().is_ok(), Ordering::SeqCst); self.called.store(true, Ordering::SeqCst); }
        }
        let manager = Arc::new(AdapterManager::<Peripheral>::default());
        let mut events = manager.event_stream();
        let (sender, mut receiver) = futures::channel::mpsc::channel(8);
        let (_events, event_receiver) = futures::channel::mpsc::channel(8);
        let peripheral = Peripheral::new(Uuid::nil(), None, None, Arc::downgrade(&manager), event_receiver, sender);
        let run = peripheral.clone();
        let first = tokio::spawn(async move { run.discover_services().await });
        let Some(CoreBluetoothMessage::DiscoverServices { future, .. }) = receiver.next().await else { panic!("first discovery missing") };
        future.lock().unwrap().set_reply(CoreBluetoothReply::ServicesDiscovered(BTreeSet::new()));
        first.await.unwrap().unwrap();
        let old = peripheral.accepted_gatt_graph_identity().unwrap().unwrap();
        let wake = Arc::new(CheckWake { owner: peripheral.shared.source_owner.clone(), unlocked: AtomicBool::new(false), called: AtomicBool::new(false) });
        let waker = Waker::from(wake.clone());
        assert!(matches!(events.as_mut().poll_next(&mut Context::from_waker(&waker)), Poll::Pending));
        let run = peripheral.clone();
        let second = tokio::spawn(async move { run.discover_services().await });
        let Some(CoreBluetoothMessage::DiscoverServices { future, .. }) = receiver.next().await else { panic!("second discovery missing") };
        second.abort();
        future.lock().unwrap().set_reply(CoreBluetoothReply::ServicesDiscovered(BTreeSet::new()));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !wake.called.load(Ordering::SeqCst) { tokio::task::yield_now().await; }
        }).await.unwrap();
        assert!(wake.unlocked.load(Ordering::SeqCst));
        let event = events.next().await;
        assert!(matches!(event, Some(CentralEvent::DeviceGraphRetired { id, graph }) if id == Uuid::nil().into() && graph == old));
        let current = peripheral.accepted_gatt_graph_identity().unwrap().unwrap();
        assert_ne!(current, old);
        let run = peripheral.clone();
        let third = tokio::spawn(async move { run.discover_services().await });
        let Some(CoreBluetoothMessage::DiscoverServices { future, .. }) = receiver.next().await else { panic!("third discovery missing") };
        future.lock().unwrap().set_reply(CoreBluetoothReply::ServicesDiscovered(BTreeSet::new()));
        third.await.unwrap().unwrap();
        assert!(matches!(events.next().await, Some(CentralEvent::DeviceGraphRetired { graph, .. }) if graph == current));
    }

    #[tokio::test]
    async fn reclaim_release_submission_survives_caller_drop_and_replays() {
        use crate::ubm_reclaim_authority::{GraphKey, GraphTarget, PermitStatus};
        let (sender, mut receiver) = futures::channel::mpsc::channel(8);
        let (_events, event_receiver) = futures::channel::mpsc::channel(8);
        let peripheral = Peripheral::new(Uuid::nil(), None, None, Weak::new(), event_receiver, sender);
        let owner = peripheral.shared.source_owner.clone();
        let graph = ("corebluetooth".to_owned(), owner.graph_attachment.unwrap(), 1);
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: graph.clone(), permit_id: 1 };
        let target = GraphTarget { peer_id: key.peer_id.clone(), connection_generation: key.connection_generation.clone(), graph: graph.clone() };
        let binding = {
            let mut state = owner.waiters.lock().unwrap(); state.ready = true; state.revision = 1;
            state.reclaim_binding.attach(); state.reclaim_binding.publish(graph.clone());
            state.reclaim_binding.capture(key.peer_id.clone(), key.connection_generation.clone(), &graph).unwrap()
        };
        {
            let mut authority = owner.authorities.lock().unwrap();
            authority.reserve_open(1).unwrap(); authority.resolve_open(1, true).unwrap();
            authority.reserve_permit_token_bound(1, 1, target.clone(), &binding).unwrap();
            authority.resolve_permit_token(1, 1, &target, true).unwrap();
        }
        assert_eq!(peripheral.acquire_reclaim(&binding, key.clone()), Ok(PermitStatus::Acquired));
        let caller_peripheral = peripheral.clone(); let caller_key = key.clone();
        let caller = tokio::spawn(async move { caller_peripheral.release_reclaim(&caller_key).await });
        let Some(CoreBluetoothMessage::ReleaseReclaim { source_owner, key: queued_key, result, .. }) = receiver.next().await else { panic!("release not queued") };
        assert_eq!(peripheral.release_reclaim(&key).await, Err(crate::ubm_reclaim_authority::AuthorityError::Pending));
        assert!(receiver.try_recv().is_err(), "duplicate retry must not queue another source task");
        caller.abort();
        let answer = source_owner.release_reclaim(&queued_key, |_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {});
        assert_eq!(answer, Ok(PermitStatus::Released));
        result.send(answer).unwrap();
        assert_eq!(peripheral.release_reclaim(&key).await, Ok(PermitStatus::Released));
        assert!(receiver.try_recv().is_err(), "settled retry must replay without dispatch");
        let second = GraphKey { permit_id: 2, ..key };
        {
            let mut authority = owner.authorities.lock().unwrap();
            authority.reserve_permit_token_bound(1, 2, target.clone(), &binding).unwrap();
            authority.resolve_permit_token(1, 2, &target, true).unwrap();
        }
        assert_eq!(peripheral.acquire_reclaim(&binding, second.clone()), Ok(PermitStatus::Acquired));
        let caller_peripheral = peripheral.clone(); let caller_key = second.clone();
        let uncertain = tokio::spawn(async move { caller_peripheral.release_reclaim(&caller_key).await });
        let Some(CoreBluetoothMessage::ReleaseReclaim { source_owner, key: queued_key, result, .. }) = receiver.next().await else { panic!("second release not queued") };
        drop(result);
        assert_eq!(uncertain.await.unwrap(), Err(crate::ubm_reclaim_authority::AuthorityError::Pending));
        assert_eq!(peripheral.release_reclaim(&second).await, Err(crate::ubm_reclaim_authority::AuthorityError::Pending));
        assert!(receiver.try_recv().is_err(), "indeterminate dispatch must not enqueue duplicate work");
        assert_eq!(source_owner.release_reclaim(&queued_key, |_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}), Ok(PermitStatus::Released));
        assert_eq!(peripheral.release_reclaim(&second).await, Ok(PermitStatus::Released));
    }

    #[tokio::test]
    async fn queued_marker_cannot_dispatch_after_adapter_barrier_installs() {
        use crate::common::util::{SourceGroupError, SourceWriteRequest, marker_graph_identity};
        let (sender, mut receiver) = futures::channel::mpsc::channel(16);
        let (_events, event_receiver) = futures::channel::mpsc::channel(16);
        let peripheral = Peripheral::new(Uuid::nil(), None, None, Weak::new(), event_receiver, sender);
        peripheral.shared.source_owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let graph = peripheral.accepted_gatt_graph_identity().unwrap().unwrap();
        let (group, _consumer) = peripheral.open_notification_source_group(1, 8, 128).unwrap();
        let target = characteristic();
        let request = SourceWriteRequest { selector: Default::default(), id: 0, graph: marker_graph_identity(&graph), value: vec![7],
            write_type: WriteType::WithResponse, route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: target.service_uuid, service_instance: target.service_instance,
                characteristic_uuid: target.uuid, characteristic_instance: target.instance,
            } };
        peripheral.start_ordered_marker_write(&group, &graph, &target, request).await.unwrap();
        let Some(CoreBluetoothMessage::MarkerWrite { group, expected, request, .. }) = receiver.next().await else { panic!("queued marker missing") };
        assert!(matches!(peripheral.shared.source_owner.admit_adapter_barrier(1), crate::ubm_reclaim_graph_gate::BarrierAdmission::Drain));
        let result = peripheral.shared.source_owner.start_marker_transaction::<()>(&group, &expected, request, || panic!("barrier admitted marker dispatch"));
        assert!(matches!(result, Err(SourceGroupError::SourceStateUnavailable)));
    }

    #[tokio::test]
    async fn reclaim_operation_survives_caller_cancellation() {
        let (peripheral, _, _, _) = fixture();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let operation = peripheral.clone();
        let caller = tokio::spawn(async move {
            operation.own_reclaim_operation(true, async move {
                let _ = started_tx.send(());
                finish_rx.await.map_err(|_| Error::Other("fixture closed".into()))?;
                Ok(())
            }).await
        });
        started_rx.await.unwrap();
        caller.abort();
        assert_eq!(peripheral.shared.source_owner.waiters.lock().unwrap().reclaim_operations, 1);
        finish_tx.send(()).unwrap();
        loop {
            let state = peripheral.shared.source_owner.waiters.lock().unwrap();
            if state.reclaim_operations == 0 {
                assert!(!state.reclaim_uncertain);
                break;
            }
            drop(state);
            tokio::task::yield_now().await;
        }
    }

    fn characteristic() -> Characteristic {
        Characteristic {
            uuid: Uuid::nil(),
            instance: 0,
            service_uuid: Uuid::nil(),
            service_instance: 0,
            properties: CharPropFlags::NOTIFY,
            descriptors: Default::default(),
        }
    }

    #[tokio::test]
    async fn reclaim_binding_rejects_retired_graph_and_foreign_owner() {
        let (first, _, _, _) = fixture();
        let (second, _, _, _) = fixture();
        let attachment = first.shared.source_owner.graph_attachment.unwrap();
        let graph = ("corebluetooth".to_owned(), attachment, 1);
        {
            let mut state = first.shared.source_owner.waiters.lock().unwrap();
            state.ready = true; state.revision = 1;
            state.reclaim_binding.attach(); state.reclaim_binding.publish(graph.clone());
        }
        let binding = first.capture_reclaim_graph_binding("peer", "central-1", &graph).unwrap();
        assert!(first.shared.source_owner.waiters.lock().unwrap().reclaim_binding.validates(&binding, "peer", "central-1", &graph));
        assert!(!second.shared.source_owner.waiters.lock().unwrap().reclaim_binding.validates(&binding, "peer", "central-1", &graph));
        first.shared.source_owner.invalidate(crate::PlatformError::new("corebluetooth", "disconnect", "test"));
        {
            let mut state = first.shared.source_owner.waiters.lock().unwrap();
            state.ready = true; state.revision = 1;
            state.reclaim_binding.attach(); state.reclaim_binding.publish(graph.clone());
            assert!(!state.reclaim_binding.validates(&binding, "peer", "central-1", &graph));
        }
    }

    #[tokio::test]
    async fn queued_marker_cancel_prevents_actor_dispatch() {
        use crate::common::util::{
            SourceGroupError, SourceUnresolvedWriteRequest, SourceWriteRequest, SourceWriteStart,
            SourceWriteStatus, marker_graph_identity,
        };
        let (sender, mut receiver) = futures::channel::mpsc::channel(1);
        let (_events, event_receiver) = futures::channel::mpsc::channel(1);
        let peripheral =
            Peripheral::new(Uuid::nil(), None, None, Weak::new(), event_receiver, sender);
        peripheral.shared.source_owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let expected = peripheral.accepted_gatt_graph_identity().unwrap().unwrap();
        let (group, consumer) = peripheral
            .open_notification_source_group(1, 8, 128)
            .unwrap();
        let target = characteristic();
        let request = SourceWriteRequest {
            selector: Default::default(),
            id: 0,
            graph: marker_graph_identity(&expected),
            value: vec![7],
            write_type: WriteType::WithResponse,
            route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: target.service_uuid,
                service_instance: target.service_instance,
                characteristic_uuid: target.uuid,
                characteristic_instance: target.instance,
            },
        };
        let unresolved = SourceUnresolvedWriteRequest {
            id: request.id,
            graph: request.graph.clone(),
            selector: request.selector.clone(),
            value: request.value.clone(),
            write_type: request.write_type,
        };
        let pending = peripheral
            .start_ordered_marker_write(&group, &expected, &target, request.clone())
            .await
            .unwrap();
        assert_eq!(pending.status(), SourceWriteStatus::Pending);
        assert_eq!(
            group
                .cancel_unresolved_marker_write(&unresolved)
                .unwrap()
                .status(),
            SourceWriteStatus::NotAdmitted(SourceGroupError::Cancelled)
        );
        let Some(CoreBluetoothMessage::MarkerWrite {
            group: actor_group,
            expected: actor_graph,
            request: actor_request,
            result,
            ..
        }) = receiver.next().await
        else {
            panic!("actor submission missing")
        };
        let SourceWriteStart::Replay(receipt) = peripheral
            .shared
            .source_owner
            .start_marker_transaction::<()>(&actor_group, &actor_graph, actor_request, || {
                panic!("cancelled write dispatched")
            })
            .unwrap()
        else {
            panic!("expected cancelled replay")
        };
        result.send(Ok(receipt)).unwrap();
        assert_eq!(
            consumer.poll(),
            crate::ubm_peer_notification_ingress::Poll::Empty
        );
        assert_eq!(
            peripheral
                .start_ordered_marker_write(&group, &expected, &target, request)
                .await
                .unwrap()
                .status(),
            SourceWriteStatus::NotAdmitted(SourceGroupError::Cancelled)
        );
        group.forget_marker_write(0).unwrap();
    }

    #[tokio::test]
    async fn marker_submission_survives_waiter_drop_and_retries_without_redispatch() {
        use crate::common::util::{
            SourceWriteRequest, SourceWriteStart, SourceWriteStatus, marker_graph_identity,
        };
        let (mut sender, mut receiver) = futures::channel::mpsc::channel(1);
        sender
            .send(CoreBluetoothMessage::StopScanning)
            .await
            .unwrap();
        let (_events, event_receiver) = futures::channel::mpsc::channel(1);
        let peripheral =
            Peripheral::new(Uuid::nil(), None, None, Weak::new(), event_receiver, sender);
        peripheral.shared.source_owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let expected = peripheral.accepted_gatt_graph_identity().unwrap().unwrap();
        let (group, consumer) = peripheral
            .open_notification_source_group(1, 8, 128)
            .unwrap();
        let target = characteristic();
        let request = SourceWriteRequest {
            selector: Default::default(),
            id: 0,
            graph: marker_graph_identity(&expected),
            value: vec![7],
            write_type: WriteType::WithResponse,
            route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: target.service_uuid,
                service_instance: target.service_instance,
                characteristic_uuid: target.uuid,
                characteristic_instance: target.instance,
            },
        };
        let receipt = peripheral
            .start_ordered_marker_write(&group, &expected, &target, request.clone())
            .await
            .unwrap();
        assert_eq!(receipt.status(), SourceWriteStatus::Pending);
        drop(receipt);
        assert!(matches!(
            receiver.next().await,
            Some(CoreBluetoothMessage::StopScanning)
        ));
        let Some(CoreBluetoothMessage::MarkerWrite {
            group: actor_group,
            expected: actor_graph,
            request: actor_request,
            result,
            ..
        }) = receiver.next().await
        else {
            panic!("actor submission missing")
        };
        let SourceWriteStart::Initiated { receipt, wake, .. } = peripheral
            .shared
            .source_owner
            .start_marker_transaction(&actor_group, &actor_graph, actor_request, || Ok(()))
            .unwrap()
        else {
            panic!("dispatch missing")
        };
        for consumer in wake {
            consumer.wake();
        }
        receipt.settle(Ok(()));
        result.send(Ok(receipt)).unwrap();
        let replay = peripheral
            .start_ordered_marker_write(&group, &expected, &target, request)
            .await
            .unwrap();
        assert!(matches!(replay.status(), SourceWriteStatus::Settled { .. }));
        assert!(matches!(
            consumer.poll(),
            crate::ubm_peer_notification_ingress::Poll::Record(
                crate::ubm_peer_notification_ingress::IngressRecord::Marker { id: 0, .. }
            )
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn graph_token_rejects_invalidated_source_and_never_wraps() {
        let (peripheral, _, _, _) = fixture();
        peripheral.shared.source_owner.waiters.lock().unwrap().ready = true;
        let token = peripheral.accepted_gatt_graph_identity().unwrap().unwrap();
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, _) = peripheral
            .open_notification_source_group_checked(1, 8, 64, &token)
            .unwrap();
        peripheral
            .shared
            .source_owner
            .invalidate(crate::PlatformError::new(
                "corebluetooth",
                "services-modified",
                "test",
            ));
        assert!(
            peripheral
                .open_notification_source_group_checked(1, 8, 64, &token)
                .is_err()
        );
        let route = crate::ubm_peer_notification_ingress::NotificationRoute {
            service_uuid: Uuid::nil(),
            service_instance: 0,
            characteristic_uuid: Uuid::nil(),
            characteristic_instance: 0,
        };
        assert!(
            peripheral
                .register_notification_source_route(&group, route, 1)
                .is_err()
        );
        {
            let mut state = peripheral.shared.source_owner.waiters.lock().unwrap();
            state.revision = u64::MAX;
        }
        peripheral
            .shared
            .source_owner
            .invalidate(crate::PlatformError::new(
                "corebluetooth",
                "services-modified",
                "test",
            ));
        assert!(peripheral.accepted_gatt_graph_identity().is_err());
    }

    #[tokio::test]
    async fn clean_source_detach_requires_consumed_barrier() {
        use crate::common::util::SourceGroupError;
        use crate::ubm_peer_notification_ingress::{IngressError, IngressRecord, Poll};
        let (peripheral, _, _, _) = fixture();
        peripheral.shared.source_owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        peripheral.notification_source_barrier(&group, 7).unwrap();
        assert_eq!(
            peripheral.detach_notification_source_group_after_barrier(&group, 7),
            Err(SourceGroupError::Ingress(IngressError::NotCurrent))
        );
        assert_eq!(
            consumer.poll(),
            Poll::Record(IngressRecord::Barrier { id: 7 })
        );
        peripheral
            .detach_notification_source_group_after_barrier(&group, 7)
            .unwrap();
    }

    #[tokio::test]
    async fn source_group_operations_wake_only_after_peer_unlock() {
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::task::{Context, Poll as TaskPoll, Wake, Waker};
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};

        struct Reenter {
            owner: Arc<super::super::internal::PeerSourceOwner>,
            unlocked: AtomicBool,
            calls: AtomicUsize,
        }
        impl Wake for Reenter {
            fn wake(self: Arc<Self>) { self.wake_by_ref(); }
            fn wake_by_ref(self: &Arc<Self>) {
                self.unlocked.store(self.owner.waiters.try_lock().is_ok(), Ordering::SeqCst);
                self.calls.fetch_add(1, Ordering::SeqCst);
            }
        }

        for operation in 0..4 {
            let (peripheral, _, _, _) = fixture();
            let owner = peripheral.shared.source_owner.clone();
            owner.waiters.lock().unwrap().ready = true;
            peripheral.start_notification_source_generation(1).unwrap();
            let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
            let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 1, characteristic_uuid: Uuid::nil(), characteristic_instance: 2 };
            let membership = if operation == 0 {
                Some(owner.faults.register_source_route(&group, route, 1).unwrap())
            } else { None };
            if operation == 2 {
                peripheral.notification_source_barrier(&group, 7).unwrap();
                assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Barrier { id: 7 }));
            }
            let probe = Arc::new(Reenter { owner, unlocked: AtomicBool::new(false), calls: AtomicUsize::new(0) });
            let waker = Waker::from(probe.clone());
            let mut receive = Box::pin(consumer.recv());
            assert!(matches!(receive.as_mut().poll(&mut Context::from_waker(&waker)), TaskPoll::Pending));
            match operation {
                0 => peripheral.retire_notification_source_route(&group, route, membership.unwrap()).unwrap(),
                1 => peripheral.notification_source_barrier(&group, 7).unwrap(),
                2 => peripheral.detach_notification_source_group_after_barrier(&group, 7).unwrap(),
                3 => peripheral.detach_notification_source_group(&group).unwrap(),
                _ => unreachable!(),
            }
            assert!(probe.calls.load(Ordering::SeqCst) > 0, "operation {operation} did not wake the consumer");
            assert!(probe.unlocked.load(Ordering::SeqCst), "operation {operation} woke while holding peer lock");
        }
    }

    #[tokio::test]
    async fn source_barrier_waits_for_reclaim_held_notification() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 1, characteristic_uuid: Uuid::nil(), characteristic_instance: 2 };
        let member = owner.faults.register_source_route(&group, route, 1).unwrap();
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new_with_barriers(8, 64 + 8 * std::mem::size_of::<super::super::internal::DeferredPeerCallback>(), 8, 1024);
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        assert!(!owner.value(keys, true, &[7]));
        peripheral.notification_source_barrier(&group, 7).unwrap();
        assert_eq!(consumer.poll(), Poll::Empty, "barrier passed a callback accepted by the reclaim gate");
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == member && value.value == [7]));
        assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Barrier { id: 7 }));
    }

    #[tokio::test]
    async fn reclaim_overflow_keeps_reserved_prefix_after_group_detaches() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 1, characteristic_uuid: Uuid::nil(), characteristic_instance: 2 };
        let member = owner.faults.register_source_route(&group, route, 1).unwrap();
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(1, 64 + std::mem::size_of::<super::super::internal::DeferredPeerCallback>());
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        assert!(!owner.value(keys, true, &[7]));
        assert!(!owner.value(keys, true, &[8]));
        assert_eq!(peripheral.detach_notification_source_group(&group), Err(crate::common::util::SourceGroupError::StaleGeneration));
        assert_eq!(consumer.poll(), Poll::Empty);
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut handoff = Vec::new();
        let mut overflow = false;
        owner.drain_reclaim_callbacks(|_, data| handoff.push(data), |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| overflow = true).unwrap();
        assert_eq!(handoff, vec![vec![7]]);
        assert!(overflow);
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == member && value.value == [7]));
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, tail_uncertain: true, .. }, .. }) if error.code == "reclaim-callback-overflow"));
    }

    #[tokio::test]
    async fn source_join_excludes_notification_already_held_by_reclaim() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        peripheral.shared.services.lock().unwrap().insert(Service {
            uuid: Uuid::nil(), instance: 0, primary: Some(true), included_services: None,
            characteristics: [characteristic()].into_iter().collect(),
        });
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 0, characteristic_uuid: Uuid::nil(), characteristic_instance: 0 };
        let existing = peripheral.register_notification_source_route(&group, route, 1).unwrap();
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(8, 64 + 8 * std::mem::size_of::<super::super::internal::DeferredPeerCallback>());
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        let attr = AttrKey { uuid: Uuid::nil(), instance: 0 };
        assert!(!owner.value((attr, attr), true, &[7]));
        peripheral.register_notification_source_route(&group, route, 2).unwrap();
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == existing && value.value == [7]));
        assert_eq!(consumer.poll(), Poll::Empty, "late member received a notification accepted before it joined");
    }

    #[tokio::test]
    async fn held_ambiguous_value_preserves_read_and_callback_time_membership() {
        use crate::api::ReadProvenance;
        use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressRecord, NotificationRoute, Poll};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        peripheral.shared.services.lock().unwrap().insert(Service {
            uuid: Uuid::nil(), instance: 0, primary: Some(true), included_services: None,
            characteristics: [characteristic()].into_iter().collect(),
        });
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 0, characteristic_uuid: Uuid::nil(), characteristic_instance: 0 };
        let existing = peripheral.register_notification_source_route(&group, route, 1).unwrap();
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        let attr = AttrKey { uuid: Uuid::nil(), instance: 0 };
        let read = CoreBluetoothReplyFuture::default();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.read((attr, attr), read.get_state_clone());
            let mut gate = GraphGate::new(8, 64 + 8 * std::mem::size_of::<super::super::internal::DeferredPeerCallback>());
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(!owner.value((attr, attr), true, &[7]));
        peripheral.register_notification_source_route(&group, route, 2).unwrap();
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(read.await, CoreBluetoothReply::CharacteristicRead(value, ReadProvenance::ReadOrNotification) if value == [7]));
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, provenance: CallbackProvenance::ReadOrNotification, value, .. }) if membership == existing && value.value == [7]));
        assert_eq!(consumer.poll(), Poll::Empty, "late member received an earlier ambiguous value");
    }

    #[tokio::test]
    async fn read_started_after_held_callback_does_not_claim_earlier_value() {
        use std::future::Future;
        use std::task::{Context, Poll as TaskPoll};
        use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressRecord, NotificationRoute, Poll};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;

        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 1, characteristic_uuid: Uuid::nil(), characteristic_instance: 2 };
        let member = owner.faults.register_source_route(&group, route, 1).unwrap();
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        let mut read = Box::pin(CoreBluetoothReplyFuture::default());
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(8, 64 + 8 * std::mem::size_of::<super::super::internal::DeferredPeerCallback>());
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(matches!(owner.admit_root_discovery(42, 0), crate::ubm_reclaim_graph_gate::Admission::Deferred));
        assert!(!owner.value(keys, true, &[7]));
        owner.waiters.lock().unwrap().read(keys, read.get_state_clone());
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();

        let waker = futures::task::noop_waker();
        assert!(matches!(read.as_mut().poll(&mut Context::from_waker(&waker)), TaskPoll::Pending), "earlier callback completed a later read");
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, provenance: CallbackProvenance::Notification, value, .. }) if membership == member && value.value == [7]));
        assert!(!owner.value(keys, false, &[8]));
        assert!(matches!(read.await, CoreBluetoothReply::CharacteristicRead(value, _) if value == [8]));
        assert_eq!(consumer.poll(), Poll::Empty);
    }

    #[tokio::test]
    async fn held_value_error_cannot_be_overtaken_by_source_barrier() {
        use crate::corebluetooth::central_delegate::PeerDelegateCallback;
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 1, characteristic_uuid: Uuid::nil(), characteristic_instance: 2 };
        let member = owner.faults.register_source_route(&group, route, 1).unwrap();
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(8, 1024);
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        let error = crate::PlatformError::new("corebluetooth", "failed", "original").with("native", "details");
        assert!(owner.admit_delegate_callback(PeerDelegateCallback::ValueError {
            route: keys, notifying: true, error: error.clone(),
        }));
        assert!(peripheral.notification_source_barrier(&group, 7).is_err());
        assert_eq!(consumer.poll(), Poll::Empty, "barrier passed an earlier value failure");
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(found), error: actual, .. }, memberships }) if found == member && actual == error && memberships == vec![(route, member)]));
        assert_eq!(consumer.poll(), Poll::Finished);
    }

    #[tokio::test]
    async fn queued_transition_before_value_error_cannot_let_barrier_overtake() {
        use crate::corebluetooth::central_delegate::PeerDelegateCallback;
        use crate::ubm_peer_notification_ingress::{NotificationRoute, Poll};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 1, characteristic_uuid: Uuid::nil(), characteristic_instance: 2 };
        owner.faults.register_source_route(&group, route, 1).unwrap();
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(8, 1024);
            gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(matches!(owner.admit_root_discovery(42, 0), crate::ubm_reclaim_graph_gate::Admission::Deferred));
        assert!(owner.admit_delegate_callback(PeerDelegateCallback::ValueError { route: keys, notifying: true, error: crate::PlatformError::new("corebluetooth", "failed", "original") }));
        let _ = peripheral.notification_source_barrier(&group, 7);
        assert_eq!(consumer.poll(), Poll::Empty, "barrier passed a value failure after a queued transition");
    }

    #[tokio::test]
    async fn queued_transition_before_value_error_preserves_retirement_and_read() {
        use crate::corebluetooth::central_delegate::PeerDelegateCallback;
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 1, characteristic_uuid: Uuid::nil(), characteristic_instance: 2 };
        let member = owner.faults.register_source_route(&group, route, 1).unwrap();
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        let read = CoreBluetoothReplyFuture::default();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.read(keys, read.get_state_clone());
            let mut gate = GraphGate::new(8, 1024);
            gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(matches!(owner.admit_root_discovery(42, 0), crate::ubm_reclaim_graph_gate::Admission::Deferred));
        let error = crate::PlatformError::new("corebluetooth", "failed", "original").with("native", "details");
        assert!(owner.admit_delegate_callback(PeerDelegateCallback::ValueError { route: keys, notifying: true, error: error.clone() }));
        let _ = peripheral.retire_notification_source_route(&group, route, member);
        assert_eq!(consumer.poll(), Poll::Empty, "retirement passed an earlier value failure");
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(read.await, CoreBluetoothReply::Failed(actual) if actual == error));
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(found), error: actual, .. }, .. }) if found == member && actual == error));
    }

    #[tokio::test]
    async fn held_value_error_retains_original_read_and_source_failure() {
        use std::future::Future;
        use std::task::Context;
        use crate::corebluetooth::central_delegate::PeerDelegateCallback;
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 1, characteristic_uuid: Uuid::nil(), characteristic_instance: 2 };
        let member = owner.faults.register_source_route(&group, route, 1).unwrap();
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        let mut first = Box::pin(CoreBluetoothReplyFuture::default());
        let mut later = Box::pin(CoreBluetoothReplyFuture::default());
        {
            let mut state = owner.waiters.lock().unwrap();
            state.read(keys, first.get_state_clone());
            let mut gate = GraphGate::new(8, 2048);
            gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        let error = crate::PlatformError::new("corebluetooth", "native", "original").with("detail", "value");
        assert!(owner.admit_delegate_callback(PeerDelegateCallback::ValueError { route: keys, notifying: true, error: error.clone() }));
        owner.waiters.lock().unwrap().read(keys, later.get_state_clone());
        let waker = futures::task::noop_waker();
        assert!(first.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
        assert_eq!(consumer.poll(), Poll::Empty);
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut delivered = None;
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |event| delivered = Some(event), |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(first.await, CoreBluetoothReply::Failed(found) if found == error));
        assert!(later.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
        assert!(matches!(delivered, Some(PeerDelegateCallback::CapturedValueError { read_failed: true, .. })));
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(found), error: actual, .. }, .. }) if found == member && actual == error));
    }

    #[tokio::test]
    async fn queued_notification_error_precedes_value_classification() {
        use crate::api::ReadProvenance;
        use crate::corebluetooth::central_delegate::PeerDelegateCallback;
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        let read = CoreBluetoothReplyFuture::default();
        let enable = CoreBluetoothReplyFuture::default();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.read(keys, read.get_state_clone());
            state.enable(keys, enable.get_state_clone()).unwrap();
            let mut gate = GraphGate::new(8, 1024);
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(owner.admit_delegate_callback(PeerDelegateCallback::NotificationError {
            route: keys, error: crate::PlatformError::new("corebluetooth", "failed", "failed"),
        }));
        assert!(!owner.value(keys, false, &[7]));
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| panic!("read-only response must not be notified"), |_| {}, |_| {}, |callback| {
            if let PeerDelegateCallback::NotificationError { route, error } = callback {
                owner.with_waiters(|state| state.notification_error(route, error));
            }
        }, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(read.await, CoreBluetoothReply::CharacteristicRead(value, ReadProvenance::ReadResponse) if value == [7]));
    }

    #[tokio::test]
    async fn queued_transition_value_keeps_arrival_membership_and_read() {
        use crate::api::ReadProvenance;
        use crate::corebluetooth::central_delegate::PeerDelegateCallback;
        use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressRecord, NotificationRoute, Poll};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 1, characteristic_uuid: Uuid::nil(), characteristic_instance: 2 };
        let member = owner.faults.register_source_route(&group, route, 1).unwrap();
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        let read = CoreBluetoothReplyFuture::default();
        let enable = CoreBluetoothReplyFuture::default();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.read(keys, read.get_state_clone());
            state.enable(keys, enable.get_state_clone()).unwrap();
            let mut gate = GraphGate::new(8, 1024);
            gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(owner.admit_delegate_callback(PeerDelegateCallback::NotificationError {
            route: keys, error: crate::PlatformError::new("corebluetooth", "failed", "failed"),
        }));
        assert!(!owner.value(keys, true, &[7]));
        owner.faults.register_source_route(&group, route, 2).unwrap();
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |callback| {
            if let PeerDelegateCallback::NotificationError { route, error } = callback {
                owner.with_waiters(|state| state.notification_error(route, error));
            }
        }, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(read.await, CoreBluetoothReply::CharacteristicRead(value, ReadProvenance::ReadOrNotification) if value == [7]));
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, provenance: CallbackProvenance::ReadOrNotification, value, .. }) if membership == member && value.value == [7]));
        assert_eq!(consumer.poll(), Poll::Empty, "transition replay retargeted an earlier value to a later member");
    }

    #[tokio::test]
    async fn queued_notification_error_cannot_consume_later_enable() {
        use crate::api::ReadProvenance;
        use crate::corebluetooth::central_delegate::PeerDelegateCallback;
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        let read = CoreBluetoothReplyFuture::default();
        let enable = CoreBluetoothReplyFuture::default();
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(8, 1024);
            gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(owner.admit_delegate_callback(PeerDelegateCallback::NotificationError {
            route: keys, error: crate::PlatformError::new("corebluetooth", "failed", "failed"),
        }));
        {
            let mut state = owner.waiters.lock().unwrap();
            state.enable(keys, enable.get_state_clone()).unwrap();
            state.read(keys, read.get_state_clone());
        }
        assert!(!owner.value(keys, false, &[7]));
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        let mut notifications = Vec::new();
        owner.drain_reclaim_callbacks(|_, value| notifications.push(value), |_| {}, |_| {}, |callback| {
            if let PeerDelegateCallback::NotificationError { route, error } = callback {
                owner.with_waiters(|state| state.notification_error(route, error));
            }
        }, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(read.await, CoreBluetoothReply::CharacteristicRead(value, ReadProvenance::ReadOrNotification) if value == [7]));
        assert_eq!(notifications, vec![vec![7]]);
    }

    #[tokio::test]
    async fn held_lifecycle_charges_callback_and_generation_cut() {
        use crate::corebluetooth::central_delegate::PeerLifecycle;
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (_, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(8, 4096);
            gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        assert!(owner.admit_lifecycle(PeerLifecycle::Disconnected));
        let token = std::mem::size_of::<crate::ubm_peer_notification_ingress::IngressReservation>() + 3 * std::mem::size_of::<usize>();
        let callback = std::mem::size_of::<super::super::internal::DeferredPeerCallback>();
        assert!(owner.waiters.lock().unwrap().reclaim_gate.as_ref().unwrap().remaining_bytes() <= 4096 - callback - token);
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(consumer.poll(), crate::ubm_peer_notification_ingress::Poll::Record(crate::ubm_peer_notification_ingress::IngressRecord::Terminal { .. })));
    }

    #[tokio::test]
    async fn source_generation_started_after_held_callback_does_not_receive_it() {
        use crate::ubm_peer_notification_ingress::{NotificationRoute, Poll};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = owner.waiters.lock().unwrap();
            state.ready = true;
            let mut gate = GraphGate::new(8, 64 + 8 * std::mem::size_of::<super::super::internal::DeferredPeerCallback>());
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        assert!(!owner.value(keys, true, &[7]));
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 1, characteristic_uuid: Uuid::nil(), characteristic_instance: 2 };
        owner.faults.register_source_route(&group, route, 1).unwrap();
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert_eq!(consumer.poll(), Poll::Empty, "new generation received an earlier callback");
    }

    #[tokio::test]
    async fn source_retirement_follows_held_value_and_excludes_later_values() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 1, characteristic_uuid: Uuid::nil(), characteristic_instance: 2 };
        let member = owner.faults.register_source_route(&group, route, 1).unwrap();
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(8, 64 + 8 * std::mem::size_of::<super::super::internal::DeferredPeerCallback>());
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        assert!(!owner.value(keys, true, &[7]));
        peripheral.retire_notification_source_route(&group, route, member).unwrap();
        assert!(!owner.value(keys, true, &[8]));
        assert_eq!(consumer.poll(), Poll::Empty, "retirement passed an accepted callback");
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == member && value.value == [7]));
        assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Fence { membership: member }));
        assert_eq!(consumer.poll(), Poll::Empty);
    }

    #[tokio::test]
    async fn source_retirement_waits_for_held_value_with_pending_read() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GraphGate;
        let (peripheral, _, _, _) = fixture();
        let owner = &peripheral.shared.source_owner;
        owner.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        let (group, consumer) = peripheral.open_notification_source_group(1, 8, 64).unwrap();
        let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 1, characteristic_uuid: Uuid::nil(), characteristic_instance: 2 };
        let member = owner.faults.register_source_route(&group, route, 1).unwrap();
        let key = GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("corebluetooth".into(), 1, 1), permit_id: 1 };
        {
            let mut state = owner.waiters.lock().unwrap();
            let mut gate = GraphGate::new(8, 64 + 8 * std::mem::size_of::<super::super::internal::DeferredPeerCallback>());
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            state.reclaim_gate = Some(gate);
        }
        let keys = (AttrKey { uuid: Uuid::nil(), instance: 1 }, AttrKey { uuid: Uuid::nil(), instance: 2 });
        let read = CoreBluetoothReplyFuture::default();
        owner.waiters.lock().unwrap().read(keys, read.get_state_clone());
        assert!(!owner.value(keys, true, &[7]));
        peripheral.retire_notification_source_route(&group, route, member).unwrap();
        peripheral.notification_source_barrier(&group, 7).unwrap();
        assert!(!owner.value(keys, true, &[8]));
        assert_eq!(consumer.poll(), Poll::Empty, "retirement passed an accepted callback");
        owner.waiters.lock().unwrap().reclaim_gate.as_mut().unwrap().release(&key).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == member && value.value == [7]));
        assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Fence { membership: member }));
        assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Barrier { id: 7 }));
        assert!(matches!(read.await, CoreBluetoothReply::CharacteristicRead(value, _) if value == [7]));
        assert_eq!(consumer.poll(), Poll::Empty);
    }

    #[tokio::test]
    async fn prepared_claims_bind_independently_and_stale_generation_is_rejected() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, Poll};
        let (peripheral, _revision, _, _) = fixture();
        let source = &peripheral.shared.source_owner;
        assert_eq!(
            peripheral.current_notification_source_generation().unwrap(),
            None
        );
        source.waiters.lock().unwrap().ready = true;
        peripheral.start_notification_source_generation(1).unwrap();
        assert_eq!(
            peripheral.current_notification_source_generation().unwrap(),
            Some(1)
        );
        let characteristic = characteristic();
        let first = peripheral
            .notification_activation_owner(&characteristic)
            .await
            .unwrap();
        let second = peripheral
            .notification_activation_owner(&characteristic)
            .await
            .unwrap();
        let first_id = first.prepare_source_activation().await.unwrap();
        let second_id = second.prepare_source_activation().await.unwrap();
        assert_ne!(first_id, second_id);
        let (group, consumer) = source.faults.open_source_group(1, 4, 32).unwrap();
        source
            .faults
            .register_source_route_for_activation(&group, first.route, 1, first_id)
            .unwrap();
        let value = || ValueNotification {
            uuid: Uuid::nil(),
            instance: 0,
            service_uuid: Uuid::nil(),
            service_instance: 0,
            value: vec![1],
            source_failure: None,
            lost_before: 0,
        };
        first.retire_prepared_source().unwrap();
        assert!(
            source
                .faults
                .publish_source_value_for_activation(1, first.route, first_id, value())
                .is_err()
        );
        source
            .faults
            .publish_source_value_for_activation(1, second.route, second_id, value())
            .unwrap();
        assert_eq!(consumer.poll(), Poll::Empty);
        source.invalidate(crate::PlatformError::new(
            "corebluetooth",
            "disconnected",
            "disconnected",
        ));
        assert!(matches!(
            consumer.poll(),
            Poll::Record(IngressRecord::Terminal { .. })
        ));
        assert_eq!(
            peripheral.current_notification_source_generation().unwrap(),
            None
        );
        assert!(second.prepare_source_activation().await.is_err());
        second.retire_prepared_source().unwrap();
    }

    #[tokio::test]
    async fn definitive_enable_refusal_allows_another_handle_to_retry_same_object() {
        let (sender, mut receiver) = futures::channel::mpsc::channel(8);
        let identity = Arc::new(NativeActivationIdentity {
            id: 1,
            ownership: Default::default(),
        });
        let owner = |claim_id| NotificationActivationOwner {
            token: Arc::new(NativeActivationToken {
                identity: identity.clone(),
                revision: 0,
                claim_id,
            }),
            peripheral_uuid: Uuid::nil(),
            sender: sender.clone(),
            source_owner: super::super::internal::PeerSourceOwner::new(),
            route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: Uuid::nil(),
                service_instance: 0,
                characteristic_uuid: Uuid::nil(),
                characteristic_instance: 0,
            },
            prepared: Arc::new(Mutex::new(None)),
        };
        let first = owner(1);
        let second = owner(2);
        let request = tokio::spawn(async move { first.subscribe().await });
        let Some(CoreBluetoothMessage::SetNotificationOwner {
            enable: true,
            future,
            ..
        }) = receiver.next().await
        else {
            panic!("expected enable")
        };
        future.lock().unwrap().set_reply(CoreBluetoothReply::Failed(
            super::super::read_notify::enable_not_notifying_error(),
        ));
        assert!(request.await.unwrap().is_err());
        let request = tokio::spawn(async move { second.subscribe().await });
        let Some(CoreBluetoothMessage::SetNotificationOwner {
            enable: true,
            future,
            ..
        }) = receiver.next().await
        else {
            panic!("expected retry enable")
        };
        future.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
        request.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancelled_caller_does_not_cancel_in_flight_enable_ownership() {
        let (sender, mut receiver) = futures::channel::mpsc::channel(8);
        let identity = Arc::new(NativeActivationIdentity {
            id: 1,
            ownership: Default::default(),
        });
        let owner = |claim_id| NotificationActivationOwner {
            token: Arc::new(NativeActivationToken {
                identity: identity.clone(),
                revision: 0,
                claim_id,
            }),
            peripheral_uuid: Uuid::nil(),
            sender: sender.clone(),
            source_owner: super::super::internal::PeerSourceOwner::new(),
            route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: Uuid::nil(),
                service_instance: 0,
                characteristic_uuid: Uuid::nil(),
                characteristic_instance: 0,
            },
            prepared: Arc::new(Mutex::new(None)),
        };
        let first = owner(1);
        let second = owner(2);
        let request = first.clone();
        let caller = tokio::spawn(async move { request.subscribe().await });
        let Some(CoreBluetoothMessage::SetNotificationOwner {
            enable: true,
            future,
            ..
        }) = receiver.next().await
        else {
            panic!("expected enable")
        };
        caller.abort();
        future.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
        tokio::time::timeout(std::time::Duration::from_secs(1), second.subscribe())
            .await
            .unwrap()
            .unwrap();
        first.unsubscribe().await.unwrap();
        let stop = tokio::spawn(async move { second.unsubscribe().await });
        let Some(CoreBluetoothMessage::SetNotificationOwner {
            enable: false,
            future,
            ..
        }) = receiver.next().await
        else {
            panic!("expected last release")
        };
        future.lock().unwrap().set_reply(CoreBluetoothReply::Ok);
        stop.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn legacy_and_exact_claims_share_the_physical_registration() {
        let (peripheral, _revision, enables, disables) = fixture();
        let characteristic = characteristic();
        let exact = peripheral
            .notification_activation_owner(&characteristic)
            .await
            .unwrap();
        exact.subscribe().await.unwrap();
        peripheral.subscribe(&characteristic).await.unwrap();
        assert_eq!(enables.load(Ordering::SeqCst), 1);
        peripheral.unsubscribe(&characteristic).await.unwrap();
        assert_eq!(disables.load(Ordering::SeqCst), 0);
        exact.unsubscribe().await.unwrap();
        assert_eq!(disables.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_legacy_cleanup_never_disables_a_new_generation() {
        let (peripheral, revision, enables, disables) = fixture();
        let characteristic = characteristic();
        peripheral.subscribe(&characteristic).await.unwrap();
        revision.store(1, Ordering::SeqCst);
        peripheral.subscribe(&characteristic).await.unwrap();
        assert_eq!(enables.load(Ordering::SeqCst), 2);
        let error = peripheral.unsubscribe(&characteristic).await.unwrap_err();
        assert!(
            matches!(error, Error::Platform(error) if error.code == "notification-owner-invalidated")
        );
        assert_eq!(disables.load(Ordering::SeqCst), 0);
        assert!(peripheral.unsubscribe(&characteristic).await.is_err());
        assert_eq!(disables.load(Ordering::SeqCst), 0);
    }
}
