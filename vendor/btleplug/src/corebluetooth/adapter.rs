use super::internal::{
    CoreBluetoothEvent, CoreBluetoothMessage, CoreBluetoothReply, CoreBluetoothReplyFuture,
    run_corebluetooth_thread,
};

#[cfg(test)]
mod directory_tests {
    use super::*;
    use futures::FutureExt;

    #[tokio::test]
    async fn directory_closed_receiver_answers_instead_of_losing_reply() {
        let (sender, receiver) = mpsc::channel(8);
        drop(receiver);
        let future = CoreBluetoothReplyFuture::default();
        let state = future.get_state_clone();
        super::super::internal::enqueue_directory_event(
            &sender,
            CoreBluetoothEvent::DirectoryReady {
                peers: Vec::new(),
                future: state.clone(),
            },
            &state,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(future.await, CoreBluetoothReply::Failed(error) if error.code == "directory-queue-closed")
        );
    }

    #[tokio::test]
    async fn directory_receiver_failure_after_registration_never_reports_partial_success() {
        let (sender, mut receiver) = mpsc::channel(8);
        let future = CoreBluetoothReplyFuture::default();
        let state = future.get_state_clone();
        let (_, notifications) = mpsc::channel(1);
        super::super::internal::enqueue_directory_event(
            &sender,
            CoreBluetoothEvent::RetrievedPeripheral {
                source_owner: super::super::internal::PeerSourceOwner::new(),
                uuid: uuid::Uuid::nil(),
                local_name: None,
                event_receiver: notifications,
            },
            &state,
        )
        .await
        .unwrap();
        assert!(matches!(
            receiver.next().await,
            Some(CoreBluetoothEvent::RetrievedPeripheral { .. })
        ));
        drop(receiver);
        super::super::internal::enqueue_directory_event(
            &sender,
            CoreBluetoothEvent::DirectoryReady {
                peers: vec![(uuid::Uuid::nil(), None)],
                future: state.clone(),
            },
            &state,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(future.await, CoreBluetoothReply::Failed(error) if error.code == "directory-queue-closed")
        );
    }

    #[tokio::test]
    async fn directory_reply_registers_same_identity_without_radio_or_discovery_event() {
        let manager = Arc::new(AdapterManager::default());
        let (sender, mut requests) = mpsc::channel(8);
        let adapter = Adapter {
            manager: manager.clone(),
            sender: sender.clone(),
            source_registry: Arc::default(),
            scan_ingress_fences: Arc::default(),
        };
        let id = uuid::Uuid::parse_str("00e2ce71-3ba4-6569-e3de-3081ce0c95fb").unwrap();
        let mut events = adapter.events().await.unwrap();
        let source_owner = super::super::internal::PeerSourceOwner::new();
        let expected_owner = source_owner.clone();
        let work = adapter.directory_peers(Some(vec![id]), None);
        let answer = async {
            let CoreBluetoothMessage::DirectoryLookup {
                services,
                identifier,
                future,
            } = requests.next().await.unwrap()
            else {
                panic!("lookup must not connect")
            };
            assert_eq!(services, Some(vec![id]));
            assert_eq!(identifier, None);
            let (_notification_sender, notifications) = mpsc::channel(8);
            let (mut queued, mut received) = mpsc::channel(8);
            queued
                .send(CoreBluetoothEvent::RetrievedPeripheral {
                    source_owner,
                    uuid: id,
                    local_name: Some("SIM".to_owned()),
                    event_receiver: notifications,
                })
                .await
                .unwrap();
            queued
                .send(CoreBluetoothEvent::DirectoryReady {
                    peers: vec![(id, Some("SIM".to_owned()))],
                    future,
                })
                .await
                .unwrap();
            drop(queued);
            while let Some(event) = received.next().await {
                assert!(handle_directory_event(event, &manager, &sender).is_none());
            }
        };
        let (result, ()) = tokio::join!(work, answer);
        assert_eq!(result.unwrap(), vec![(id, Some("SIM".to_owned()))]);
        let peripheral = adapter.peripheral(&id.into()).await.unwrap();
        assert!(Arc::ptr_eq(peripheral.source_owner(), &expected_owner));
        // Subsequent normal connect uses this exact public handle. Lookup itself
        // has emitted no radio command or fabricated advertisement/link event.
        assert!(requests.next().now_or_never().is_none());
        assert!(events.next().now_or_never().is_none());
    }

    #[tokio::test]
    async fn closed_adapter_source_fails_pending_scan_fence_without_waiting_for_actor() {
        let (sender, _receiver) = mpsc::channel(1);
        let adapter = Adapter {
            manager: Arc::new(AdapterManager::default()),
            sender,
            source_registry: Arc::default(),
            scan_ingress_fences: Arc::default(),
        };
        let fence = CoreBluetoothReplyFuture::default();
        adapter
            .scan_ingress_fences
            .lock()
            .unwrap()
            .register(42, fence.get_state_clone())
            .unwrap();

        adapter.scan_ingress_source_closed().await.unwrap();
        assert!(matches!(
            fence.await,
            CoreBluetoothReply::Failed(error) if error.code == "scan-ingress-source-closed"
        ));
        assert!(matches!(
            adapter.fence_scan_ingress(42).await,
            Err(Error::Platform(error)) if error.code == "scan-ingress-source-closed"
        ));
    }

    #[tokio::test]
    async fn scan_fence_waits_for_adapter_stream_acknowledgement() {
        let (sender, _receiver) = mpsc::channel(1);
        let adapter = Adapter {
            manager: Arc::new(AdapterManager::default()),
            sender,
            source_registry: Arc::default(),
            scan_ingress_fences: Arc::default(),
        };
        let mut fence = CoreBluetoothReplyFuture::default();
        let fence_id = adapter
            .scan_ingress_fences
            .lock()
            .unwrap()
            .register(41, fence.get_state_clone())
            .unwrap();

        // The delegate/actor marker has no authority to complete this reply.
        // The adapter stream must consume the marker and call this method.
        assert!((&mut fence).now_or_never().is_none());
        assert!(adapter.acknowledge_scan_ingress_fence(fence_id));
        assert!(matches!(fence.await, CoreBluetoothReply::Ok));
    }

    #[tokio::test]
    async fn held_adapter_event_consumer_blocks_scan_fence_until_marker_acknowledgement() {
        let (sender, mut requests) = mpsc::channel(1);
        let adapter = Adapter {
            manager: Arc::new(AdapterManager::default()),
            sender,
            source_registry: Arc::default(),
            scan_ingress_fences: Arc::default(),
        };
        let mut events = adapter.events().await.unwrap();
        let first_adapter = adapter.clone();
        let first = tokio::spawn(async move { first_adapter.fence_scan_ingress(41).await });

        let CoreBluetoothMessage::ScanIngress {
            action: super::super::central_delegate::ScanIngressAction::Fence,
            epoch: 41,
            fence_id: Some(first_fence_id),
            ..
        } = requests.next().await.unwrap()
        else {
            panic!("the adapter must request an identified ingress fence");
        };

        // A caller deadline may cancel after the queue command has been
        // accepted but before the downstream marker is acknowledged. The
        // retry must reuse this exact pending marker rather than requiring a
        // second physical stop/retire.
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let waiting_adapter = adapter.clone();
        let mut waiting = tokio::spawn(async move { waiting_adapter.fence_scan_ingress(41).await });
        let CoreBluetoothMessage::ScanIngress {
            action: super::super::central_delegate::ScanIngressAction::Fence,
            epoch: 41,
            fence_id: Some(fence_id),
            ..
        } = requests.next().await.unwrap()
        else {
            panic!("the retry must request the existing ingress fence");
        };
        assert_eq!(fence_id, first_fence_id);

        adapter.manager.emit(CentralEvent::AdvertisementScoped {
            id: uuid::Uuid::nil().into(),
            report: crate::api::AdvertisementReport::default(),
            ingress_epoch: 41,
        });
        adapter.manager.emit(CentralEvent::ScanIngressFence {
            ingress_epoch: 41,
            fence_id,
        });

        // A delegate/actor marker alone cannot release a scan while the
        // adapter stream consumer has not crossed the earlier advertisement.
        assert!((&mut waiting).now_or_never().is_none());
        assert!(matches!(
            events.next().await,
            Some(CentralEvent::AdvertisementScoped { ingress_epoch: 41, .. })
        ));
        assert!((&mut waiting).now_or_never().is_none());

        let Some(CentralEvent::ScanIngressFence {
            ingress_epoch: 41,
            fence_id: observed_id,
        }) = events.next().await
        else {
            panic!("the marker must follow the scoped advertisement");
        };
        assert_eq!(observed_id, fence_id);
        assert!(adapter.acknowledge_scan_ingress_fence(observed_id));
        assert!(waiting.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn unassociated_native_callback_is_a_public_source_uncertainty_event() {
        let manager = Arc::new(AdapterManager::<Peripheral>::default());
        let mut events = manager.event_stream();
        let peer = uuid::Uuid::from_u128(0xCB0B);
        emit_unassociated_connection_observation(
            &manager,
            peer,
            Some(crate::PlatformError::new(
                "corebluetooth",
                "unassociated-fixture",
                "callback had no admitted native cycle",
            )),
        );
        assert!(matches!(events.next().await,
            Some(CentralEvent::DeviceSourceUncertain {
                id,
                graph: None,
                platform: Some(platform),
            }) if id == peer.into()
                && platform.domain == "corebluetooth"
                && platform.code == "unassociated-fixture"
                && platform.message == "callback had no admitted native cycle"
        ));
    }
}
use super::peripheral::{Peripheral, PeripheralId};
use crate::api::{Central, CentralEvent, CentralState, ScanFilter};
use crate::common::adapter_manager::AdapterManager;
use crate::{Error, Result};
use async_trait::async_trait;
use futures::channel::mpsc::{self, Sender};
use futures::sink::SinkExt;
use futures::stream::{Stream, StreamExt};
use log::*;
use objc2_core_bluetooth::CBManagerState;
use std::pin::Pin;
use std::{collections::BTreeMap, sync::Arc};
use tokio::task;

/// Implementation of [api::Central](crate::api::Central).
#[derive(Clone, Debug)]
pub struct Adapter {
    manager: Arc<AdapterManager<Peripheral>>,
    sender: Sender<CoreBluetoothMessage>,
    source_registry: Arc<super::internal::PeerSourceRegistry>,
    /// Fences wait for an actor marker. If the authoritative adapter stream
    /// closes, no actor can consume a queued marker, so these replies must be
    /// completed locally rather than waiting forever.
    scan_ingress_fences: Arc<std::sync::Mutex<ScanIngressFences>>,
}

#[derive(Debug, Default)]
struct ScanIngressFences {
    source_closed: bool,
    next_id: u64,
    /// One retiring epoch owns one downstream marker.  Several callers may
    /// wait for it when an outer lifecycle deadline cancels and retries the
    /// same fence before the Btleplug event consumer acknowledges it.
    pending: BTreeMap<u64, PendingScanIngressFence>,
}

#[derive(Debug)]
struct PendingScanIngressFence {
    id: u64,
    waiters: Vec<super::internal::CoreBluetoothReplyStateShared>,
}

impl ScanIngressFences {
    fn source_closed_error() -> crate::PlatformError {
        crate::PlatformError::new(
            "corebluetooth",
            "scan-ingress-source-closed",
            "The authoritative CoreBluetooth adapter event stream closed before the scan ingress fence completed",
        )
    }

    fn register(
        &mut self,
        epoch: u64,
        reply: super::internal::CoreBluetoothReplyStateShared,
    ) -> std::result::Result<u64, crate::PlatformError> {
        if self.source_closed {
            return Err(Self::source_closed_error());
        }
        if let Some(pending) = self.pending.get_mut(&epoch) {
            pending.waiters.push(reply);
            return Ok(pending.id);
        }
        self.next_id = self.next_id.checked_add(1).ok_or_else(|| {
            crate::PlatformError::new(
                "corebluetooth",
                "scan-ingress-fence-id-exhausted",
                "The CoreBluetooth scan ingress fence identifier space is exhausted",
            )
        })?;
        self.pending.insert(
            epoch,
            PendingScanIngressFence {
                id: self.next_id,
                waiters: vec![reply],
            },
        );
        Ok(self.next_id)
    }

    fn remove_waiter(
        &mut self,
        epoch: u64,
        reply: &super::internal::CoreBluetoothReplyStateShared,
    ) {
        let remove_epoch = if let Some(pending) = self.pending.get_mut(&epoch) {
            pending.waiters.retain(|waiter| !std::sync::Arc::ptr_eq(waiter, reply));
            pending.waiters.is_empty()
        } else {
            false
        };
        if remove_epoch {
            self.pending.remove(&epoch);
        }
    }

    fn acknowledge(
        &mut self,
        id: u64,
    ) -> Option<Vec<super::internal::CoreBluetoothReplyStateShared>> {
        let epoch = self
            .pending
            .iter()
            .find_map(|(&epoch, pending)| (pending.id == id).then_some(epoch))?;
        self.pending.remove(&epoch).map(|pending| pending.waiters)
    }

    fn close(&mut self) -> Vec<super::internal::CoreBluetoothReplyStateShared> {
        self.source_closed = true;
        std::mem::take(&mut self.pending)
            .into_values()
            .flat_map(|pending| pending.waiters)
            .collect()
    }

    fn is_closed(&self) -> bool {
        self.source_closed
    }
}

// UBM patch (UBM_PATCHES.md #7): resetting, unsupported and unauthorized
// are reported as themselves (upstream folded them into `Unknown`).
fn get_central_state(state: CBManagerState) -> CentralState {
    match state {
        CBManagerState::PoweredOn => CentralState::PoweredOn,
        CBManagerState::PoweredOff => CentralState::PoweredOff,
        CBManagerState::Resetting => CentralState::Resetting,
        CBManagerState::Unsupported => CentralState::Unsupported,
        CBManagerState::Unauthorized => CentralState::Unauthorized,
        _ => CentralState::Unknown,
    }
}

fn register_directory_peripheral(
    manager: &Arc<AdapterManager<Peripheral>>,
    sender: &Sender<CoreBluetoothMessage>,
    uuid: uuid::Uuid,
    local_name: Option<String>,
    event_receiver: mpsc::Receiver<super::internal::PeripheralEventInternal>,
    source_owner: Arc<super::internal::PeerSourceOwner>,
) {
    manager.replace_peripheral(Peripheral::new_with_source_owner(
        uuid,
        local_name,
        None,
        Arc::downgrade(manager),
        event_receiver,
        sender.clone(),
        source_owner,
    ));
}

fn handle_directory_event(
    event: CoreBluetoothEvent,
    manager: &Arc<AdapterManager<Peripheral>>,
    sender: &Sender<CoreBluetoothMessage>,
) -> Option<CoreBluetoothEvent> {
    match event {
        CoreBluetoothEvent::DirectoryReady { peers, future } => {
            future
                .lock()
                .unwrap()
                .set_reply(CoreBluetoothReply::DirectoryPeers(peers));
            None
        }
        CoreBluetoothEvent::RetrievedPeripheral {
            source_owner,
            uuid,
            local_name,
            event_receiver,
        } => {
            register_directory_peripheral(
                manager,
                sender,
                uuid,
                local_name,
                event_receiver,
                source_owner,
            );
            None
        }
        event => Some(event),
    }
}

fn emit_unassociated_connection_observation(
    manager: &AdapterManager<Peripheral>,
    uuid: uuid::Uuid,
    error: Option<crate::PlatformError>,
) {
    // The callback had a real native identity but no admitted exact-object
    // cycle. Surface that fact without letting it mutate or disconnect a
    // replacement peripheral with the same UUID. The platform detail is
    // retained by the typed actor-to-adapter event; CentralEvent deliberately
    // reports the portable source-uncertainty vocabulary.
    if let Some(error) = error.as_ref() {
        warn!("unassociated CoreBluetooth connection observation for {uuid}: {error:?}");
    }
    manager.emit(CentralEvent::DeviceSourceUncertain {
        id: uuid.into(),
        graph: None,
        platform: error.map(Into::into),
    });
}

impl Adapter {
    /// Associate one UBM scan operation with CoreBluetooth's serial delegate
    /// callback queue. CoreBluetooth provides neither a scan object nor an
    /// epoch in `didDiscover`, so callers must arm before physical scanning,
    /// then retire and fence after `stopScan` before replacement scanning.
    pub async fn arm_scan_ingress(&self, epoch: u64) -> Result<()> {
        self.scan_ingress(super::central_delegate::ScanIngressAction::Arm, epoch)
            .await
    }

    /// Stop accepting new callback ownership for `epoch`. This does not make
    /// a claim about radio hardware; use [`Self::fence_scan_ingress`] to
    /// drain the accepted callback prefix into the actor.
    pub async fn retire_scan_ingress(&self, epoch: u64) -> Result<()> {
        self.scan_ingress(super::central_delegate::ScanIngressAction::Retire, epoch)
            .await
    }

    /// Wait until CoreBluetooth's serial callback queue and the adapter actor
    /// have both crossed the retiring epoch's ordered marker. This is a
    /// callback-observed fence, not an RF-generation guarantee: Apple gives
    /// `stopScan` neither a completion callback nor a scan identity, so an
    /// OS discovery delivered only after a later re-arm is indistinguishable
    /// from that later scan and must not be claimed as proven old or new.
    pub async fn fence_scan_ingress(&self, epoch: u64) -> Result<()> {
        self.scan_ingress(super::central_delegate::ScanIngressAction::Fence, epoch)
            .await
    }

    /// Mark the adapter event source terminal and settle every fence that was
    /// waiting for its actor marker. The adapter stream is authoritative: once
    /// it ends, there is no safe future callback/actor delivery to wait for.
    pub async fn scan_ingress_source_closed(&self) -> Result<()> {
        let pending = self
            .scan_ingress_fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .close();
        let error = ScanIngressFences::source_closed_error();
        for fence in pending {
            fence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_set_reply(CoreBluetoothReply::Failed(error.clone()));
        }
        Ok(())
    }

    /// Acknowledge the ordered marker only after the adapter event stream has
    /// consumed it. This is deliberately downstream of the CoreBluetooth
    /// actor: an actor-only acknowledgement could release a scan while an
    /// earlier scoped advertisement was still waiting to become a radio event.
    pub fn acknowledge_scan_ingress_fence(&self, fence_id: u64) -> bool {
        let reply = self
            .scan_ingress_fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .acknowledge(fence_id);
        if let Some(replies) = reply {
            for reply in replies {
                reply
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .try_set_reply(CoreBluetoothReply::Ok);
            }
            true
        } else {
            // This is expected if stream closure already failed the fence.
            false
        }
    }

    async fn scan_ingress(
        &self,
        action: super::central_delegate::ScanIngressAction,
        epoch: u64,
    ) -> Result<()> {
        let fut = CoreBluetoothReplyFuture::default();
        let state = fut.get_state_clone();
        let is_fence = matches!(action, super::central_delegate::ScanIngressAction::Fence);
        let fence_id = {
            let mut fences = self
                .scan_ingress_fences
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if fences.is_closed() {
                return Err(Error::Platform(ScanIngressFences::source_closed_error()));
            }
            if is_fence {
                Some(fences.register(epoch, state.clone()).map_err(Error::Platform)?)
            } else {
                None
            }
        };
        self.sender
            .to_owned()
            .send(CoreBluetoothMessage::ScanIngress {
                action,
                epoch,
                fence_id,
                future: state.clone(),
            })
            .await
            .map_err(|error| {
                if is_fence {
                    if fence_id.is_some() {
                        self.scan_ingress_fences
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove_waiter(epoch, &state);
                    }
                }
                error
            })?;
        let reply = fut.await;
        // A successful fence is removed by `acknowledge_scan_ingress_fence`
        // only after its marker traverses the public adapter event stream.
        // A failed source-close completion has already drained the registry.
        if is_fence && !matches!(&reply, CoreBluetoothReply::Ok) {
            if fence_id.is_some() {
                self.scan_ingress_fences
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove_waiter(epoch, &state);
            }
        }
        match reply {
            CoreBluetoothReply::Ok => Ok(()),
            CoreBluetoothReply::Err(detail) => Err(Error::Other(detail.into())),
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            reply => Err(Error::Other(
                format!("unexpected reply to a scan ingress command: {reply:?}").into(),
            )),
        }
    }

    /// Read-only OS directory lookup on this adapter's existing manager.
    pub async fn directory_peers(
        &self,
        services: Option<Vec<uuid::Uuid>>,
        identifier: Option<uuid::Uuid>,
    ) -> Result<Vec<(uuid::Uuid, Option<String>)>> {
        let future = CoreBluetoothReplyFuture::default();
        self.sender
            .clone()
            .send(CoreBluetoothMessage::DirectoryLookup {
                services,
                identifier,
                future: future.get_state_clone(),
            })
            .await?;
        match future.await {
            CoreBluetoothReply::DirectoryPeers(peers) => Ok(peers),
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            reply => Err(Error::Other(
                format!("unexpected directory reply: {reply:?}").into(),
            )),
        }
    }

    pub fn reclaim_authority_handle(&self) -> crate::ubm_reclaim_authority::ReclaimAuthorityHandle {
        crate::ubm_reclaim_authority::ReclaimAuthorityHandle::from_owner(self.source_registry.authorities.clone())
    }

    pub(crate) async fn new(authorities: Arc<std::sync::Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>) -> Result<Self> {
        Self::new_with_failure_evidence_store(authorities, None).await
    }

    pub(crate) async fn new_with_failure_evidence_store(
        authorities: Arc<std::sync::Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
        failure_evidence_store: Option<Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>>,
    ) -> Result<Self> {
        let source_registry = Arc::new(
            super::internal::PeerSourceRegistry::with_authorities_and_failure_evidence_store(
                authorities,
                failure_evidence_store,
            ),
        );
        let (sender, mut receiver) = mpsc::channel(256);
        let adapter_sender = run_corebluetooth_thread(sender, source_registry.clone())?;
        // Since init currently blocked until the state update, we know the
        // receiver is dropped after that. We can pick it up here and make it
        // part of our event loop to update our peripherals.
        debug!("Waiting on adapter connect");
        if !matches!(
            receiver.next().await,
            Some(CoreBluetoothEvent::DidUpdateState { state: _ })
        ) {
            return Err(Error::Other(
                "Adapter failed to connect.".to_string().into(),
            ));
        }
        debug!("Adapter connected");
        let manager = Arc::new(AdapterManager::default());

        let manager_clone = manager.clone();
        let adapter_sender_clone = adapter_sender.clone();
        task::spawn(async move {
            while let Some(msg) = receiver.next().await {
                let Some(msg) = handle_directory_event(msg, &manager_clone, &adapter_sender_clone)
                else {
                    continue;
                };
                match msg {
                    CoreBluetoothEvent::DirectoryReady { .. }
                    | CoreBluetoothEvent::RetrievedPeripheral { .. } => {
                        unreachable!("directory events handled above")
                    }
                    CoreBluetoothEvent::DeviceDiscovered {
                        source_owner,
                        uuid,
                        local_name,
                        advertisement_name,
                        event_receiver,
                    } => {
                        // UBM patch (UBM_PATCHES.md #19): a peripheral
                        // retrieved again replaces the entry it supersedes
                        // (upstream asserted it was new).
                        manager_clone.replace_peripheral(Peripheral::new_with_source_owner(
                            uuid,
                            local_name,
                            advertisement_name,
                            Arc::downgrade(&manager_clone),
                            event_receiver,
                            adapter_sender_clone.clone(),
                            source_owner,
                        ));
                        manager_clone.emit(CentralEvent::DeviceDiscovered(uuid.into()));
                    }
                    CoreBluetoothEvent::DeviceUpdated {
                        uuid,
                        local_name,
                        advertisement_name,
                    } => {
                        let id = uuid.into();
                        if let Some(entry) = manager_clone.peripheral_mut(&id) {
                            entry.value().update_name(local_name, advertisement_name);
                            manager_clone.emit(CentralEvent::DeviceUpdated(id));
                        }
                    }
                    CoreBluetoothEvent::DeviceDisconnected { uuid } => {
                        manager_clone.emit(CentralEvent::DeviceDisconnected(uuid.into()));
                    }
                    CoreBluetoothEvent::DeviceSourceUncertain { uuid, error } => {
                        emit_unassociated_connection_observation(&manager_clone, uuid, error);
                    }
                    CoreBluetoothEvent::Advertised { uuid, report, ingress_epoch } => {
                        let id = uuid.into();
                        if let Some(ingress_epoch) = ingress_epoch {
                            manager_clone.emit(CentralEvent::AdvertisementScoped {
                                id,
                                report,
                                ingress_epoch,
                            });
                        } else {
                            // An unowned callback can still update the
                            // adapter's legacy event stream, but is never
                            // eligible for a UBM scan session.
                            manager_clone.emit(CentralEvent::Advertisement { id, report });
                        }
                    }
                    CoreBluetoothEvent::ScanIngressFence { epoch, fence_id } => {
                        manager_clone.emit(CentralEvent::ScanIngressFence {
                            ingress_epoch: epoch,
                            fence_id,
                        });
                    }
                    CoreBluetoothEvent::DidUpdateState { state } => {
                        let central_state = get_central_state(state);
                        manager_clone.emit(CentralEvent::StateUpdate(central_state));
                    }
                }
            }
        });

        Ok(Adapter {
            manager,
            sender: adapter_sender,
            source_registry,
            scan_ingress_fences: Arc::default(),
        })
    }
}

#[async_trait]
impl Central for Adapter {
    type Peripheral = Peripheral;

    async fn events(&self) -> Result<Pin<Box<dyn Stream<Item = CentralEvent> + Send>>> {
        Ok(self.manager.event_stream())
    }

    async fn start_scan(&self, filter: ScanFilter) -> Result<()> {
        // UBM patch (UBM_PATCHES.md #8): the start is answered by the
        // CoreBluetooth thread, which refuses it unless the manager is
        // powered on (CoreBluetooth ignores a scan requested in any other
        // state, with no error). Upstream returned `Ok` once the request was
        // queued.
        let fut = CoreBluetoothReplyFuture::default();
        self.sender
            .to_owned()
            .send(CoreBluetoothMessage::StartScanning {
                filter,
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::Ok => Ok(()),
            CoreBluetoothReply::Err(detail) => Err(Error::Other(detail.into())),
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            reply => Err(Error::Other(
                format!("unexpected reply to a scan start: {reply:?}").into(),
            )),
        }
    }

    async fn stop_scan(&self) -> Result<()> {
        self.sender
            .to_owned()
            .send(CoreBluetoothMessage::StopScanning)
            .await?;
        Ok(())
    }

    async fn peripherals(&self) -> Result<Vec<Peripheral>> {
        Ok(self.manager.peripherals())
    }

    async fn peripheral(&self, id: &PeripheralId) -> Result<Peripheral> {
        self.manager.peripheral(id).ok_or(Error::DeviceNotFound)
    }

    /// UBM patch (UBM_PATCHES.md #19, finding 127): the peripheral with
    /// this identifier, retrieved from CoreBluetooth when this adapter no
    /// longer holds it (`retrievePeripheralsWithIdentifiers`, as the legacy
    /// addon reconnected without a scan).
    async fn add_peripheral(&self, id: &PeripheralId) -> Result<Peripheral> {
        if let Some(peripheral) = self.manager.peripheral(id) {
            return Ok(peripheral);
        }
        self.directory_peers(None, Some(id.uuid())).await?;
        self.manager.peripheral(id).ok_or(Error::DeviceNotFound)
    }

    async fn clear_peripherals(&self) -> Result<()> {
        self.manager.clear_peripherals();
        Ok(())
    }

    async fn adapter_info(&self) -> Result<String> {
        // TODO: Get information about the adapter.
        Ok("CoreBluetooth".to_string())
    }

    async fn adapter_state(&self) -> Result<CentralState> {
        let fut = CoreBluetoothReplyFuture::default();
        self.sender
            .to_owned()
            .send(CoreBluetoothMessage::GetAdapterState {
                future: fut.get_state_clone(),
            })
            .await?;

        match fut.await {
            CoreBluetoothReply::AdapterState(state) => {
                let central_state = get_central_state(state);
                return Ok(central_state.clone());
            }
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            _ => panic!("Shouldn't get anything but a AdapterState!"),
        }
    }
}
