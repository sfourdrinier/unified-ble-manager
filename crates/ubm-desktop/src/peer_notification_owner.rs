// crates/ubm-desktop/src/peer_notification_owner.rs
use super::*;
use std::collections::HashSet;

use btleplug::platform::{Peripheral, SourceGroupHandle};

#[derive(Default)]
pub(crate) struct PeerGroupState {
    pub(crate) source: SourceGeneration,
    pub(crate) group: Option<Arc<tokio::sync::Mutex<Group>>>,
}

pub(crate) struct OrderedSourceOwner {
    pub(crate) peer_id: String,
    pub(crate) collection_id: u64,
    pub(crate) group: Mutex<Option<Arc<tokio::sync::Mutex<Group>>>>,
    pub(crate) lease: Mutex<Option<crate::boundary::OrderedSourceLease>>,
    pub(crate) renewals: Mutex<HashMap<u64, MemberRenewal>>,
    pub(crate) origins: Mutex<
        HashMap<
            (u64, ubm_core::ordered_notifications::Membership),
            crate::boundary::PreparedNotificationActivation,
        >,
    >,
    pub(crate) claims: Mutex<
        HashMap<
            u64,
            (
                ubm_core::ordered_notifications::Membership,
                crate::boundary::PreparedNotificationActivation,
            ),
        >,
    >,
}

#[derive(Clone)]
pub(crate) struct MemberRenewal {
    pub(crate) old: ubm_core::ordered_notifications::Membership,
    pub(crate) new: Option<ubm_core::ordered_notifications::Membership>,
    pub(crate) retired: bool,
    pub(crate) rebound: bool,
    pub(crate) resumed: bool,
}

pub(crate) struct Group {
    pub(crate) native_group: u64,
    pub(crate) generation: u64,
    pub(crate) detached: bool,
    pause_request: tokio::sync::watch::Sender<bool>,
    pause_ack: tokio::sync::watch::Receiver<bool>,
    pause_owners: HashSet<u64>,
    pub(crate) peripheral: Peripheral,
    pub(crate) handle: SourceGroupHandle,
    pub(crate) members: Arc<Members>,
    pub(crate) barriers: Arc<BarrierAcks>,
    pub(crate) routes:
        HashMap<(crate::boundary::InstanceKey, u64), (NotificationRoute, Membership)>,
    pub(crate) task: Option<tokio::task::JoinHandle<Result<Option<DesktopError>, DesktopError>>>,
    outcome: Option<Result<Option<DesktopError>, DesktopError>>,
}

/// One native generation remains owned if opening its first group fails.
#[derive(Default)]
pub(crate) struct SourceGeneration(Option<u64>);

impl SourceGeneration {
    pub(crate) fn current(&self) -> Option<u64> {
        self.0
    }

    pub(crate) async fn open<T, E, F, Fut, O>(
        &mut self,
        generation: u64,
        start: F,
        open: O,
    ) -> Result<T, OpenGenerationError<E>>
    where
        F: FnOnce(u64) -> Fut,
        Fut: std::future::Future<Output = Result<(), E>>,
        O: FnOnce(u64) -> Result<T, E>,
    {
        match self.0 {
            Some(current) if current != generation => {
                return Err(OpenGenerationError::AlreadyOwned(current));
            }
            Some(_) => {}
            None => {
                start(generation)
                    .await
                    .map_err(OpenGenerationError::Start)?;
                self.0 = Some(generation);
            }
        }
        open(generation).map_err(OpenGenerationError::Open)
    }

    /// Adopt a generation observed as current from this native source owner.
    pub(crate) fn adopt_observed(&mut self, generation: u64) -> bool {
        match self.0 {
            Some(current) => current == generation,
            None => {
                self.0 = Some(generation);
                true
            }
        }
    }

    /// Called only after the source has established this generation ended.
    pub(crate) fn retire(&mut self, generation: u64) -> bool {
        if self.0 != Some(generation) {
            return false;
        }
        self.0 = None;
        true
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OpenGenerationError<E> {
    AlreadyOwned(u64),
    Start(E),
    Open(E),
}

impl Group {
    /// Retain the same join handle across cancelled waiters and remember its
    /// terminal result, including failures, until the owner disposes it.
    pub(crate) async fn settle(&mut self) -> Result<Option<DesktopError>, DesktopError> {
        if self.outcome.is_none() {
            let Some(task) = self.task.as_mut() else {
                return Err(pump_failure("peer notification pump owner is absent"));
            };
            let result = match task.await {
                Ok(result) => result,
                Err(error) => Err(pump_failure(format!(
                    "peer notification pump failed: {error}"
                ))),
            };
            self.task.take();
            self.outcome = Some(result);
        }
        self.outcome.clone().expect("pump outcome retained")
    }

    pub(crate) async fn pause(&mut self, member_id: u64) -> Result<(), DesktopError> {
        self.pause_owners.insert(member_id);
        self.pause_request.send_replace(true);
        while !*self.pause_ack.borrow_and_update() {
            self.pause_ack
                .changed()
                .await
                .map_err(|_| pump_failure("source pump ended while pausing"))?;
        }
        Ok(())
    }

    pub(crate) async fn resume(&mut self, member_id: u64) -> Result<(), DesktopError> {
        self.pause_owners.remove(&member_id);
        if !self.pause_owners.is_empty() {
            return Ok(());
        }
        self.pause_request.send_replace(false);
        while *self.pause_ack.borrow_and_update() {
            self.pause_ack
                .changed()
                .await
                .map_err(|_| pump_failure("source pump ended while resuming"))?;
        }
        Ok(())
    }

    /// Atomically install native memberships and their registry ownership while
    /// the pump cannot resolve any newly admitted callback.
    pub(crate) fn batch<R>(
        &mut self,
        apply: impl FnOnce(
            &mut MemberBatch<'_>,
            &Peripheral,
            &SourceGroupHandle,
            &mut HashMap<(crate::boundary::InstanceKey, u64), (NotificationRoute, Membership)>,
        ) -> R,
    ) -> Result<R, DesktopError> {
        let Self {
            members,
            peripheral,
            handle,
            routes,
            ..
        } = self;
        members.batch(|batch| {
            if batch.is_closed() {
                return Err(pump_failure("peer notification source is closed"));
            }
            Ok(apply(batch, peripheral, handle, routes))
        })
    }

    /// Register the exact source activation while the host mapping is locked,
    /// so an immediate native callback cannot overtake mapping publication.
    pub(crate) fn register(
        &mut self,
        scope: crate::boundary::InstanceKey,
        epoch: u64,
        route: NotificationRoute,
        member: u64,
        activation: u64,
    ) -> Result<Membership, DesktopError> {
        if self.routes.contains_key(&(scope.clone(), epoch)) {
            return Err(pump_failure("duplicate peer notification membership"));
        }
        let target = ForwardTarget {
            peer_id: scope.0.clone(),
            service_uuid: scope.1.clone(),
            service_occurrence: scope.2,
            characteristic_uuid: scope.3.clone(),
            characteristic_occurrence: scope.4,
            epoch,
        };
        let membership = self.members.batch(|batch| {
            if batch.is_closed() {
                return Err(pump_failure("peer notification source is closed"));
            }
            batch
                .register(target, || {
                    self.peripheral
                        .register_notification_source_route_for_activation(
                            &self.handle,
                            route,
                            member,
                            activation,
                        )
                })
                .map_err(|error| {
                    pump_failure(format!(
                        "cannot register peer notification route: {error:?}"
                    ))
                })
        })?;
        self.routes.insert((scope, epoch), (route, membership));
        Ok(membership)
    }

    /// Queue a peer-wide fence under the native source lock. The returned
    /// receiver completes only after the central applies all earlier records.
    pub(crate) fn barrier_native(
        &self,
        id: u64,
    ) -> Result<oneshot::Receiver<()>, btleplug::platform::SourceGroupError> {
        let (ack, received) = PeerSourceFenceAck::new();
        let mut barriers = self.barriers.lock().expect("peer barriers");
        if barriers.contains_key(&id) {
            return Err(btleplug::platform::SourceGroupError::Capacity);
        }
        barriers.insert(id, ack);
        if let Err(error) = self
            .peripheral
            .notification_source_barrier(&self.handle, id)
        {
            barriers.remove(&id);
            return Err(error);
        }
        Ok(received)
    }

    pub(crate) fn barrier(&self, id: u64) -> Result<oneshot::Receiver<()>, DesktopError> {
        self.barrier_native(id).map_err(|error| {
            pump_failure(format!(
                "cannot enqueue peer notification barrier: {error:?}"
            ))
        })
    }

    pub(crate) fn finalize_retire(&mut self, scope: &crate::boundary::InstanceKey, epoch: u64) {
        self.routes.remove(&(scope.clone(), epoch));
    }

    pub(crate) fn retire(
        &self,
        scope: &crate::boundary::InstanceKey,
        epoch: u64,
    ) -> Result<Option<oneshot::Receiver<()>>, DesktopError> {
        let Some((route, membership)) = self.routes.get(&(scope.clone(), epoch)).copied() else {
            return Ok(None);
        };
        self.members
            .retire(membership, || {
                self.peripheral
                    .retire_notification_source_route(&self.handle, route, membership)
            })
            .map_err(|error| {
                pump_failure(format!("cannot retire peer notification route: {error:?}"))
            })
    }
}

/// Opens a source only after a complete graph is published. The pump emits the
/// source-start event before it reads any callback accepted by the native FIFO.
#[expect(
    clippy::too_many_arguments,
    reason = "The host opening boundary retains explicit source, registry, and recovery ownership inputs."
)]
pub(crate) async fn open_group(
    state: &mut SourceGeneration,
    peripheral: Peripheral,
    peer_id: String,
    generation: u64,
    native_group: u64,
    fanout: bool,
    expected_graph: Option<crate::boundary::GattSnapshotIdentity>,
    registry: Arc<Mutex<crate::ordered_notification_groups::OrderedGroupRegistry>>,
    sender: mpsc::Sender<RadioEvent>,
    queued_bytes: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    failure_recoveries: Arc<PeerFailureRecoveryStore>,
) -> Result<Group, DesktopError> {
    let source = &peripheral;
    let (handle, consumer) = state
        .open(
            generation,
            |generation| async move {
                #[cfg(target_os = "linux")]
                let result = source
                    .start_notification_source_generation(generation)
                    .await;
                #[cfg(not(target_os = "linux"))]
                let result = source.start_notification_source_generation(generation);
                result.map_err(|error| {
                    pump_failure(format!("cannot start peer notification source: {error:?}"))
                })
            },
            |generation| {
                #[cfg(target_os = "linux")]
                let result = peripheral.open_notification_source_group(
                    generation,
                    crate::btleplug_backend::NOTIFICATION_CAP,
                    crate::btleplug_backend::NOTIFICATION_BYTES as usize,
                );
                #[cfg(not(target_os = "linux"))]
                let result = if let Some(identity) = &expected_graph {
                    peripheral.open_notification_source_group_checked(
                        generation,
                        crate::btleplug_backend::NOTIFICATION_CAP,
                        crate::btleplug_backend::NOTIFICATION_BYTES as usize,
                        &(
                            identity.owner.clone(),
                            identity.attachment,
                            identity.revision,
                        ),
                    )
                } else {
                    peripheral.open_notification_source_group(
                        generation,
                        crate::btleplug_backend::NOTIFICATION_CAP,
                        crate::btleplug_backend::NOTIFICATION_BYTES as usize,
                    )
                };
                #[cfg(target_os = "linux")]
                let _ = &expected_graph;
                result.map_err(|error| {
                    pump_failure(format!("cannot open peer notification group: {error:?}"))
                })
            },
        )
        .await
        .map_err(|error| match error {
            OpenGenerationError::AlreadyOwned(current) => pump_failure(format!(
                "peer notification source generation {current} is still owned"
            )),
            OpenGenerationError::Start(error) | OpenGenerationError::Open(error) => error,
        })?;
    let members = Arc::new(Members::default());
    let barriers = Arc::new(Mutex::new(HashMap::new()));
    let source = crate::ordered_notification_groups::SourceKey {
        peer_id: peer_id.clone(),
        generation: ubm_core::ordered_notifications::SourceGeneration::Counter(generation),
        native_group,
    };
    let task_members = members.clone();
    let task_barriers = barriers.clone();
    let task_registry = registry.clone();
    let task_sender = sender.clone();
    let (pause_request, pause_receiver) = tokio::sync::watch::channel(false);
    let (pause_ack_sender, pause_ack) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        let result = pump_group_with_failure_recovery(
            peer_id.clone(),
            generation,
            native_group,
            fanout,
            registry,
            consumer,
            task_members.clone(),
            task_barriers.clone(),
            sender,
            queued_bytes,
            dropped,
            Some((pause_receiver, pause_ack_sender)),
            Some(failure_recoveries),
        )
        .await;
        if let Err(failure) = result {
            let error = failure.error;
            let rejected = failure.rejected;
            // Fence new registration before taking the failure-time snapshot.
            let (native, snapshot) = task_members.fail_snapshot();
            let native: Vec<_> = native.into_iter().map(registry_membership).collect();
            let terminal = task_registry
                .lock()
                .expect("ordered notification groups")
                .source_terminal(
                    &source,
                    &native,
                    ubm_core::ordered_notifications::EndReason::SourceFailed(
                        crate::ordered_notification_groups::failure_from_desktop(&error),
                    ),
                    rejected,
                    None,
                    true,
                );
            let reported = match terminal {
                Ok(_) => error,
                Err(registry_error) => pump_failure(format!(
                    "peer pump failed: {error}; collection terminal accounting failed: {registry_error:?}"
                )),
            };
            let rejected_loss = match rejected {
                Some(ubm_core::ordered_notifications::SourceRejection::Value { .. }) => Some(1),
                Some(ubm_core::ordered_notifications::SourceRejection::Loss {
                    skipped_records,
                }) => Some(skipped_records),
                _ => None,
            };
            let sent = if fanout {
                task_sender
                    .send(RadioEvent::PeerNotificationSourceFailed {
                        peer_id,
                        generation,
                        members: snapshot,
                        error: reported.clone(),
                        rejected_loss,
                        tail_uncertain: true,
                    })
                    .await
            } else {
                Ok(())
            };
            let acknowledged = if sent.is_ok() {
                let (ack, received) = PeerSourceFenceAck::new();
                if task_sender
                    .send(RadioEvent::PeerNotificationSourceFence { ack })
                    .await
                    .is_ok()
                {
                    received.await.is_ok()
                } else {
                    false
                }
            } else {
                false
            };
            task_members.close();
            task_barriers.lock().expect("peer barriers").clear();
            return if acknowledged {
                Ok(Some(reported))
            } else {
                Err(pump_failure(format!(
                    "peer pump failure could not be acknowledged: {reported}"
                )))
            };
        }
        Ok(None)
    });
    Ok(Group {
        native_group,
        generation,
        detached: false,
        pause_request,
        pause_ack,
        pause_owners: HashSet::new(),
        peripheral,
        handle,
        members,
        barriers,
        routes: HashMap::new(),
        task: Some(task),
        outcome: None,
    })
}
