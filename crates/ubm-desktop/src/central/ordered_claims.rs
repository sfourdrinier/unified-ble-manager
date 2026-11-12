// crates/ubm-desktop/src/central/ordered_claims.rs
//! Exact claim state retained while an ordered batch owns GATT admission.

use super::*;
use crate::boundary::PreparedNotificationActivation;

/// Phase is recorded before awaiting a native call, so cancellation leaves
/// enough information to reconcile an effect whose outcome was not observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OrderedClaimPhase {
    Admitted,
    Preparing,
    Prepared,
    Dispatching,
    Acquired,
    Indeterminate,
    Released,
}

pub(super) struct OrderedClaim {
    pub(super) member_id: u64,
    pub(super) selector: PathSelector,
    pub(super) consumer: String,
    pub(super) scope: InstanceKey,
    pub(super) path_index: usize,
    pub(super) epoch: u64,
    pub(super) operation: OperationId,
    pub(super) ticket: OpTicket,
    pub(super) drive_enable: bool,
    pub(super) delivery: Option<DeliveryMode>,
    pub(super) preference: Option<DeliveryMode>,
    pub(super) activation: Option<PreparedNotificationActivation>,
    pub(super) phase: OrderedClaimPhase,
}

pub(super) struct OrderedDeliveryState {
    pub next_id: Option<u64>,
    pub pending: Option<(u64, ubm_core::ordered_notifications::Poll)>,
    pub last_acked: Option<u64>,
    pub pending_discard: Option<super::ordered_batch_cleanup::OrderedDiscardReceipt>,
    pub last_discard_acked: Option<u64>,
    pub recording_owner: Option<u64>,
    pub marker_owner: Option<(u64, Option<u64>)>,
    pub marker_evidence_acked: Option<u64>,
    pub last_recording_ack: Option<u64>,
}

/// Source cleanup is retained even if the caller stops awaiting setup.
/// Observations remain in the bounded native collection until inspected.
pub(super) struct OrderedBatchState {
    pub(super) peer_id: String,
    pub(super) collection_id: u64,
    pub(super) connection_generation: Option<String>,
    pub(super) database_generation: Option<String>,
    pub(super) snapshot_identity: Option<GattSnapshotIdentity>,
    pub(super) fenced: bool,
    pub(super) retired: bool,
    pub(super) retirement_reason: Option<ubm_core::ordered_notifications::EndReason>,
    pub(super) closing: bool,
    pub(super) cleanup_complete: bool,
    pub(super) receipt_acknowledged: bool,
    pub(super) delivery_lock: Arc<Mutex<()>>,
    pub(super) data_ready: Arc<tokio::sync::Notify>,
    pub(super) delivery_state: Arc<StdMutex<OrderedDeliveryState>>,
    pub(super) preparation_started: bool,
    pub(super) admission: Option<Arc<crate::GattAdmission>>,
    pub(super) starting: Option<crate::gatt_admission::StartingPoll>,
    pub(super) claims: Vec<OrderedClaim>,
    pub(super) requested_members: Vec<u64>,
    pub(super) acquired_members: HashSet<u64>,
    pub(super) indeterminate_members: HashSet<u64>,
    pub(super) member_cleanup_errors: HashMap<u64, DesktopError>,
    pub(super) renewals: HashMap<u64, super::ordered_member_renewal::MemberRenewal>,
    pub(super) reclaim_reservation:
        Option<Arc<StdMutex<crate::reclaim_reservation::ReclaimReservationState>>>,
    pub(super) marker_reservation: Option<(
        Option<u64>,
        Arc<Mutex<crate::marker_reservation::MarkerReservationState>>,
        Arc<std::sync::atomic::AtomicU8>,
    )>,
    pub(super) marker_publication_wake: Arc<tokio::sync::Notify>,
    pub(super) marker_gate: Arc<Mutex<()>>,
    pub(super) marker_transaction:
        Option<Arc<StdMutex<super::ordered_marker_write::MarkerTransaction>>>,
    pub(super) next_marker_id: Option<u64>,
    pub(super) retired_marker_high_water: Option<u64>,
    pub(super) source: Option<crate::boundary::OrderedSourceCleanup>,
    pub(super) observed_source_generation:
        Option<ubm_core::ordered_notifications::SourceGeneration>,
    pub(super) source_prepare_started: bool,
    pub(super) source_open_driver_active: bool,
    pub(super) source_open_first_polled: bool,
    pub(super) source_limits: Option<(usize, usize, usize)>,
    pub(super) central_source_installed: bool,
    pub(super) source_host_attached: bool,
    pub(super) empty_sink_installed: bool,
    pub(super) empty_terminal_seen: bool,
    pub(super) empty_secondary_failure: Option<ubm_core::ordered_notifications::Failure>,
    pub(super) deferred_terminal: Option<ubm_core::ordered_notifications::Terminal>,
    pub(super) source_sealed: bool,
    pub(super) source_claims_started: HashSet<u64>,
    pub(super) source_claims_confirmed: HashSet<u64>,
    pub(super) setup_error: Option<DesktopError>,
    pub(super) fence_error: Option<DesktopError>,
}

/// Fixed-capacity ownership ledger. Reservation occurs before any claim or
/// native source effect; an entry is never replaced by another collection.
pub(super) struct OrderedBatchRegistry {
    state: StdMutex<OrderedBatchRegistryState>,
}

struct OrderedBatchRegistryState {
    entries: HashMap<(String, u64), Arc<Mutex<OrderedBatchState>>>,
    next_id: Option<u64>,
}

impl Default for OrderedBatchRegistryState {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            next_id: Some(0),
        }
    }
}

impl Default for OrderedBatchRegistry {
    fn default() -> Self {
        Self {
            state: StdMutex::new(OrderedBatchRegistryState::default()),
        }
    }
}

impl OrderedBatchRegistry {
    pub(super) const CAPACITY: usize = 64;

    pub(super) fn reserve(
        &self,
        peer_id: String,
    ) -> Result<(u64, Arc<Mutex<OrderedBatchState>>), DesktopError> {
        self.reserve_with_owner(peer_id, None)
    }
    pub(super) fn reserve_with_owner(
        &self,
        peer_id: String,
        recording_owner: Option<u64>,
    ) -> Result<(u64, Arc<Mutex<OrderedBatchState>>), DesktopError> {
        if peer_id.is_empty() {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                "gatt.ordered.reserve",
            ));
        }
        let mut ledger = lock_std(&self.state);
        if ledger.entries.len() >= Self::CAPACITY {
            return Err(contract_error(
                BleErrorCode::CapabilityLimited,
                BleErrorDomain::Gatt,
                "gatt.ordered.reserve",
            )
            .with_detail("ordered collection ownership capacity exhausted"));
        }
        let collection_id = ledger.next_id.ok_or_else(|| {
            contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.reserve",
            )
            .with_detail("ordered collection identity exhausted")
        })?;
        let key = (peer_id.clone(), collection_id);
        let owner = Arc::new(Mutex::new(OrderedBatchState {
            peer_id,
            collection_id,
            connection_generation: None,
            database_generation: None,
            snapshot_identity: None,
            fenced: false,
            retired: false,
            retirement_reason: None,
            closing: false,
            cleanup_complete: false,
            receipt_acknowledged: false,
            delivery_lock: Arc::new(Mutex::new(())),
            data_ready: Arc::new(tokio::sync::Notify::new()),
            delivery_state: Arc::new(StdMutex::new(OrderedDeliveryState {
                next_id: Some(1),
                pending: None,
                last_acked: None,
                pending_discard: None,
                last_discard_acked: None,
                recording_owner,
                marker_owner: None,
                marker_evidence_acked: None,
                last_recording_ack: None,
            })),
            preparation_started: false,
            admission: None,
            starting: None,
            claims: Vec::new(),
            requested_members: Vec::new(),
            acquired_members: HashSet::new(),
            indeterminate_members: HashSet::new(),
            member_cleanup_errors: HashMap::new(),
            renewals: HashMap::new(),
            marker_reservation: None,
            reclaim_reservation: None,
            marker_publication_wake: Arc::new(tokio::sync::Notify::new()),
            marker_gate: Arc::new(Mutex::new(())),
            marker_transaction: None,
            next_marker_id: Some(0),
            retired_marker_high_water: None,
            source: None,
            observed_source_generation: None,
            source_prepare_started: false,
            source_open_driver_active: false,
            source_open_first_polled: false,
            source_limits: None,
            central_source_installed: false,
            source_host_attached: false,
            empty_sink_installed: false,
            empty_terminal_seen: false,
            empty_secondary_failure: None,
            deferred_terminal: None,
            source_sealed: false,
            source_claims_started: HashSet::new(),
            source_claims_confirmed: HashSet::new(),
            setup_error: None,
            fence_error: None,
        }));
        ledger.next_id = collection_id.checked_add(1);
        ledger.entries.insert(key, Arc::clone(&owner));
        Ok((collection_id, owner))
    }

    pub(super) fn get(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Option<Arc<Mutex<OrderedBatchState>>> {
        lock_std(&self.state)
            .entries
            .get(&(peer_id.to_owned(), collection_id))
            .cloned()
    }

    /// True only for an issued identifier that is no longer active. The
    /// transport must authorize the originating session before using this.
    pub(super) fn was_forgotten(&self, collection_id: u64) -> bool {
        let ledger = lock_std(&self.state);
        let issued = ledger.next_id.is_none_or(|next| collection_id < next);
        issued && !ledger.entries.keys().any(|(_, id)| *id == collection_id)
    }

    /// Snapshot bounded owners for lifecycle fencing; callers must verify the
    /// captured connection/database generation while holding each state lock.
    pub(super) fn owners_for_peer(&self, peer_id: &str) -> Vec<Arc<Mutex<OrderedBatchState>>> {
        lock_std(&self.state)
            .entries
            .iter()
            .filter(|((peer, _), _)| peer == peer_id)
            .map(|(_, owner)| Arc::clone(owner))
            .collect()
    }

    /// The caller verifies source cleanup, member release and drained terminal
    /// while holding the state lock before removing its only retry key.
    pub(super) fn remove_if_same(
        &self,
        peer_id: &str,
        collection_id: u64,
        owner: &Arc<Mutex<OrderedBatchState>>,
    ) -> bool {
        let mut ledger = lock_std(&self.state);
        let key = (peer_id.to_owned(), collection_id);
        if !ledger
            .entries
            .get(&key)
            .is_some_and(|stored| Arc::ptr_eq(stored, owner))
        {
            return false;
        }
        ledger.entries.remove(&key);
        true
    }
}

#[cfg(test)]
#[path = "ordered_claims_tests.rs"]
mod tests;

/// A collection-local claim against one exact characteristic selector.
pub struct OrderedClaimRequest {
    pub member_id: u64,
    pub selector: PathSelector,
    pub consumer: String,
    pub delivery: Option<DeliveryMode>,
    pub preference: Option<DeliveryMode>,
}

pub(super) fn validate_ordered_claims(
    connection_generation: &str,
    database_generation: &str,
    requests: &[OrderedClaimRequest],
) -> Result<(), DesktopError> {
    const OP: &str = "gatt.ordered.prepare";
    if requests.is_empty() || connection_generation.is_empty() || database_generation.is_empty() {
        return Err(contract_error(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Gatt,
            OP,
        ));
    }
    let mut ids = HashSet::new();
    let mut consumers = HashSet::new();
    if requests.iter().any(|r| {
        r.consumer.is_empty() || !ids.insert(r.member_id) || !consumers.insert(r.consumer.clone())
    }) {
        return Err(contract_error(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Gatt,
            OP,
        ));
    }
    Ok(())
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Reserve ownership-only claims under one GATT admission. The ledger is
    /// installed before any await so cancellation retains every admitted claim.
    pub(super) async fn prepare_ordered_claims(
        &self,
        peer_id: &str,
        collection_id: u64,
        connection_generation: &str,
        database_generation: &str,
        requests: &[OrderedClaimRequest],
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        self.prepare_ordered_claims_started(
            peer_id,
            collection_id,
            connection_generation,
            database_generation,
            requests,
            ctl,
            None,
        )
        .await
    }

    pub(super) async fn prepare_ordered_claims_started(
        &self,
        peer_id: &str,
        collection_id: u64,
        connection_generation: &str,
        database_generation: &str,
        requests: &[OrderedClaimRequest],
        ctl: OpControl,
        starting: Option<crate::gatt_admission::StartingPoll>,
    ) -> Result<(), DesktopError> {
        const OP: &str = "gatt.ordered.prepare";
        validate_ordered_claims(connection_generation, database_generation, requests)?;
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(BleErrorCode::OwnershipDenied, BleErrorDomain::Gatt, OP)
            })?;
        {
            let mut state = owner.lock().await;
            if state.preparation_started {
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    OP,
                ));
            }
            state.preparation_started = true;
            state.starting = starting;
            state.requested_members = requests.iter().map(|request| request.member_id).collect();
        }
        let result = self
            .prepare_ordered_claims_inner(
                &owner,
                peer_id,
                connection_generation,
                database_generation,
                requests,
                &ctl,
            )
            .await;
        if let Err(error) = &result {
            owner.lock().await.setup_error = Some(error.clone());
        }
        result
    }

    async fn prepare_ordered_claims_inner(
        &self,
        owner: &Arc<Mutex<OrderedBatchState>>,
        peer_id: &str,
        connection_generation: &str,
        database_generation: &str,
        requests: &[OrderedClaimRequest],
        ctl: &OpControl,
    ) -> Result<(), DesktopError> {
        const OP: &str = "gatt.ordered.prepare";
        self.precheck(ctl, OP)?;
        let window = ctl.budget.window(LIVENESS_OP);
        let admission = self.wait_gatt_admission(peer_id, ctl, OP, window).await?;
        {
            let mut state = owner.lock().await;
            state.admission = Some(Arc::clone(&admission));
            state.connection_generation = Some(connection_generation.to_owned());
            state.database_generation = Some(database_generation.to_owned());
        }
        let needs_starting = { owner.lock().await.starting.is_none() };
        if needs_starting {
            let starting = match drive_link(
                &self.inner,
                peer_id,
                OP,
                &ctl.ticket,
                window,
                admission.begin_starting(),
            )
            .await
            {
                Wait::Done(result) => result?,
                Wait::Expired => return Err(timed_out(OP, window)),
                Wait::Cancelled => return Err(ctl.ticket.interruption(OP)),
            };
            owner.lock().await.starting = Some(starting);
        }
        let peer_key = self.known_peer_key(peer_id).await?;
        let peer_epoch = self.routing_epoch(peer_id).await;
        let coordinator = discovery_coordinator(&self.inner, peer_id);
        let snapshot = match drive_link(&self.inner, peer_id, OP, &ctl.ticket, window, async {
            Ok(coordinator.snapshot.lock().await)
        })
        .await
        {
            Wait::Done(result) => result?,
            Wait::Expired => return Err(timed_out(OP, window)),
            Wait::Cancelled => return Err(ctl.ticket.interruption(OP)),
        };
        let mut state = owner.lock().await;
        if snapshot.generations.as_ref()
            != Some(&(
                connection_generation.to_owned(),
                database_generation.to_owned(),
            ))
        {
            return Err(contract_error(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        let identity = snapshot.identity.clone();
        if self
            .inner
            .boundary
            .ordered_source_requires_snapshot_identity()
            && identity.is_none()
        {
            return Err(contract_error(
                BleErrorCode::CapabilityUnavailable,
                BleErrorDomain::Gatt,
                OP,
            )
            .with_detail("ordered source requires a committed native snapshot identity"));
        }
        if self.inner.boundary.gatt_snapshot_identity(peer_id)? != identity {
            return Err(contract_error(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        state.snapshot_identity = identity;
        let mut scopes = HashSet::new();
        for request in requests {
            self.refuse_before_admission(ctl, OP)?;
            admission.assert_current()?;
            let candidate_scope = {
                let core = self.inner.core.lock().await;
                let (_, scope, _) =
                    self.resolve_instance(&core, &peer_key, peer_id, &request.selector, OP, false)?;
                scope
            };
            if self
                .inner
                .failed_disables
                .lock()
                .await
                .contains(&candidate_scope)
            {
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    OP,
                )
                .with_detail("physical disable pending; complete it before subscribing"));
            }
            let mut routes = self.inner.subscriptions.lock().await;
            let mut core = self.inner.core.lock().await;
            if core.connection_generation(&peer_key).as_deref() != Some(connection_generation)
                || core.database_generation(&peer_key).as_deref() != Some(database_generation)
            {
                return Err(contract_error(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    OP,
                ));
            }
            let (index, scope, _) =
                self.resolve_instance(&core, &peer_key, peer_id, &request.selector, OP, false)?;
            core.validate_gatt_admission(ctl.gatt_path(index), OP)
                .map_err(DesktopError::from)?;
            self.inner
                .acquired
                .assert_available(&scope, crate::acquired_gatt::AcquisitionKind::Notify)?;
            if !scopes.insert(scope.clone()) {
                return Err(contract_error(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Gatt,
                    OP,
                ));
            }
            if let Some(failure) = lock_std(&self.inner.notification_failures).get(&index) {
                return Err(failure.error.clone());
            }
            if let Some(required) = request.delivery {
                if routes.contains_key(&scope) {
                    let observed = lock_std(&self.inner.deliveries)
                        .get(&scope)
                        .copied()
                        .unwrap_or(ObservedDelivery::Unknown);
                    if !observed.satisfies(required) {
                        return Err(Self::delivery_unenforced(
                            required,
                            observed,
                            "a join cannot rewrite the live CCCD",
                        ));
                    }
                }
            }
            let next_epoch = {
                let epochs = lock_std(&self.inner.notification_activation_epochs);
                match epochs.get(&scope) {
                    Some(previous) => previous
                        .checked_add(1)
                        .ok_or_else(|| {
                            contract_error(
                                BleErrorCode::LifecycleInvalidState,
                                BleErrorDomain::Gatt,
                                OP,
                            )
                        })?
                        .max(peer_epoch),
                    None => peer_epoch,
                }
            };
            let ticket = OpTicket::new();
            self.track(&ticket);
            let mut out = batch();
            let before = core.typed_effects().len();
            let operation = core
                .subscribe_ownership_only(
                    ctl.gatt_path(index),
                    &request.consumer,
                    window.core_timeout_ms(),
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            let drive_enable = core.typed_effects()[before..].iter().any(|effect| {
                effect.kind() == CentralEffectKind::SubscribeEnable
                    && effect.operation_id() == &operation
            });
            if let Err(error) = publish_or_refuse(&mut core, &ticket, &operation, OP, None) {
                let _ = core.unsubscribe(index, &request.consumer, now_ms(), &mut out);
                let _ = out.drain();
                sweep_terminal_successes(&mut core);
                return Err(error);
            }
            let epoch = if drive_enable {
                lock_std(&self.inner.notification_activation_epochs)
                    .insert(scope.clone(), next_epoch);
                next_epoch
            } else if let Some((_, epoch)) = routes.get(&scope) {
                *epoch
            } else {
                let _ = core.cancel_op(&operation, now_ms(), &mut out);
                let _ = core.unsubscribe(index, &request.consumer, now_ms(), &mut out);
                let _ = out.drain();
                sweep_terminal_successes(&mut core);
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    OP,
                ));
            };
            let dispatchable = matches!(
                core.operation_state(&operation),
                Some(ubm_core::ownership::OpStateView::Queued)
            );
            routes.insert(scope.clone(), (index, epoch));
            lock_std(&self.inner.retained_claims)
                .entry((index, epoch))
                .or_default()
                .insert(request.consumer.clone());
            // No await may separate admission from recording its cleanup owner.
            state.claims.push(OrderedClaim {
                member_id: request.member_id,
                selector: request.selector.clone(),
                consumer: request.consumer.clone(),
                scope,
                path_index: index,
                epoch,
                operation,
                ticket,
                drive_enable,
                delivery: request.delivery,
                preference: request.preference,
                activation: None,
                phase: OrderedClaimPhase::Admitted,
            });
            if dispatchable {
                let claim = state.claims.last().ok_or_else(|| {
                    contract_error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Gatt,
                        OP,
                    )
                })?;
                core.dispatch_op(&claim.operation, &mut out)
                    .map_err(DesktopError::from)?;
            }
        }
        drop(state);
        drop(snapshot);
        Ok(())
    }
}

pub(super) fn ordered_source_key(
    lease: &crate::boundary::OrderedSourceLease,
) -> crate::ordered_notification_groups::SourceKey {
    crate::ordered_notification_groups::SourceKey {
        peer_id: lease.peer_id.clone(),
        generation: lease.source_generation.clone(),
        native_group: lease.native_group,
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    pub(super) fn update_ordered_owner(
        &self,
        lease: &crate::boundary::OrderedSourceLease,
        transition: crate::boundary::OrderedSourceTransition,
    ) -> Result<(), DesktopError> {
        match lease.ingress {
            crate::boundary::OrderedSourceIngress::BackendOwned => {
                self.inner.boundary.update_ordered_source(lease, transition)
            }
            crate::boundary::OrderedSourceIngress::CentralRetainedBatch => self
                .update_mobile_ordered_ingress(
                    lease.collection_id,
                    &ordered_source_key(lease),
                    transition,
                ),
        }
    }

    pub(super) fn ordered_owner_status(
        &self,
        lease: &crate::boundary::OrderedSourceLease,
    ) -> Result<crate::boundary::OrderedSourceStatus, DesktopError> {
        match lease.ingress {
            crate::boundary::OrderedSourceIngress::BackendOwned => {
                self.inner.boundary.ordered_source_status(lease)
            }
            crate::boundary::OrderedSourceIngress::CentralRetainedBatch => {
                self.mobile_ordered_ingress_status(lease.collection_id, &ordered_source_key(lease))
            }
        }
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Mint a host-lifetime group ID before any source or CCCD effect.
    pub fn reserve_ordered_batch(&self, peer_id: &str) -> Result<u64, DesktopError> {
        self.inner
            .ordered_batches
            .reserve(peer_id.to_owned())
            .map(|(id, _)| id)
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    pub(super) async fn recover_ordered_batch_source(
        &self,
        state: &mut OrderedBatchState,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<(), DesktopError> {
        use crate::boundary::{OrderedSourceCleanup, OrderedSourceRecovery};
        match self
            .inner
            .boundary
            .recover_ordered_source(peer_id, collection_id)
            .await?
        {
            OrderedSourceRecovery::NotStarted if state.source_open_driver_active => {
                state.source = Some(OrderedSourceCleanup::Indeterminate {
                    peer_id: peer_id.to_owned(),
                    collection_id,
                });
            }
            OrderedSourceRecovery::NotStarted => {
                state.source_prepare_started = false;
                state.source = None;
            }
            OrderedSourceRecovery::Pending => {
                state.source = Some(OrderedSourceCleanup::Indeterminate {
                    peer_id: peer_id.to_owned(),
                    collection_id,
                });
            }
            OrderedSourceRecovery::Ready(lease) => {
                if lease.ingress == crate::boundary::OrderedSourceIngress::BackendOwned {
                    self.inner
                        .boundary
                        .attach_ordered_wake(&lease, state.data_ready.clone())?;
                }
                state.data_ready.notify_one();
                state.observed_source_generation = Some(lease.source_generation.clone());
                state.source = Some(OrderedSourceCleanup::Lease(lease))
            }
        }
        Ok(())
    }
}
