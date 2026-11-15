// crates/ubm-desktop/src/central/ordered_source_recovery.rs
//! Recovery and status for retained ordered sources.

use super::ordered_claims::{OrderedClaimPhase, ordered_source_key};
use super::*;

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Increase the byte budget of the retained source identified by its generation.
    pub async fn increase_ordered_batch_byte_capacity(
        &self,
        peer_id: &str,
        collection_id: u64,
        generation: &ubm_core::ordered_notifications::SourceGeneration,
        max_bytes: usize,
    ) -> Result<usize, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.increase-capacity",
                )
            })?;
        let state = owner.lock().await;
        let Some(crate::boundary::OrderedSourceCleanup::Lease(lease)) = &state.source else {
            return Err(contract_error(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                "gatt.ordered.increase-capacity",
            ));
        };
        if &lease.source_generation != generation {
            return Err(contract_error(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                "gatt.ordered.increase-capacity",
            ));
        }
        let source = super::ordered_claims::ordered_source_key(lease);
        if state.empty_sink_installed {
            self.increase_empty_ordered_byte_capacity(&source, collection_id, max_bytes)
        } else {
            match lease.ingress {
                crate::boundary::OrderedSourceIngress::BackendOwned => self
                    .inner
                    .boundary
                    .increase_ordered_byte_capacity(lease, max_bytes),
                crate::boundary::OrderedSourceIngress::CentralRetainedBatch => {
                    self.increase_mobile_ordered_byte_capacity(collection_id, &source, max_bytes)
                }
            }
        }
    }

    /// Recover a mobile Open whose response arrived after its waiter ended.
    /// Quarantine retains only the old generation's bounded observations.
    pub(super) async fn recover_mobile_ordered_owner(
        &self,
        state: &mut super::ordered_claims::OrderedBatchState,
        lease: &crate::boundary::OrderedSourceLease,
    ) -> Result<(), DesktopError> {
        use ubm_core::ordered_notifications::{
            DeliveryMode as OrderedDelivery, Member, OrderedCollection,
        };
        const OP: &str = "gatt.ordered.recover";
        if lease.ingress != crate::boundary::OrderedSourceIngress::CentralRetainedBatch {
            return Ok(());
        }
        if lease.members.is_empty() {
            if !state.empty_sink_installed {
                let (_, bytes, controls) = state.source_limits.ok_or_else(|| {
                    contract_error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Gatt,
                        OP,
                    )
                })?;
                self.install_empty_ordered_ingress_with_byte_limit(
                    ordered_source_key(lease),
                    lease.collection_id,
                    controls,
                    bytes,
                )?;
                state.empty_sink_installed = true;
                self.inner.empty_ordered_ingress.set_wake(
                    &ordered_source_key(lease),
                    lease.collection_id,
                    state.data_ready.clone(),
                )?;
                if state.setup_error.is_none() {
                    state.setup_error = Some(DesktopError::cancelled(OP));
                }
            }
            if !state.source_host_attached {
                self.inner.boundary.ordered_source_installed(lease).await?;
                state.source_host_attached = true;
            }
            return Ok(());
        }
        if !state.central_source_installed {
            let connection = state.connection_generation.clone().ok_or_else(|| {
                contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    OP,
                )
            })?;
            let database = state.database_generation.clone().ok_or_else(|| {
                contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    OP,
                )
            })?;
            let (items, bytes, controls) = state.source_limits.ok_or_else(|| {
                contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    OP,
                )
            })?;
            let mut members = Vec::with_capacity(lease.members.len());
            for (member_id, membership) in &lease.members {
                let claim = state
                    .claims
                    .iter()
                    .find(|claim| claim.member_id == *member_id)
                    .ok_or_else(|| {
                        contract_error(BleErrorCode::ProtocolMalformed, BleErrorDomain::Gatt, OP)
                    })?;
                let delivery = match claim.delivery {
                    Some(DeliveryMode::Notification) => OrderedDelivery::Notification,
                    Some(DeliveryMode::Indication) => OrderedDelivery::Indication,
                    None => OrderedDelivery::Either,
                };
                members.push(Member {
                    id: *member_id,
                    source: *membership,
                    delivery,
                });
            }
            let collection = OrderedCollection::new(
                connection,
                database,
                lease.source_generation.clone(),
                lease.collection_id,
                members,
                items,
                bytes,
                controls,
            )
            .map_err(|error| {
                contract_error(BleErrorCode::ProtocolMalformed, BleErrorDomain::Gatt, OP)
                    .with_detail(format!("{error:?}"))
            })?;
            self.quarantine_mobile_ordered_ingress(ordered_source_key(lease), collection)?;
            state.central_source_installed = true;
            lock_std(&self.inner.mobile_ordered_ingress)
                .set_wake(
                    lease.collection_id,
                    &ordered_source_key(lease),
                    state.data_ready.clone(),
                )
                .map_err(|e| DesktopError::subscribe_failed(format!("ordered wake: {e:?}")))?;
            if state.setup_error.is_none() {
                state.setup_error = Some(DesktopError::cancelled(OP));
            }
        }
        if !state.source_host_attached {
            self.inner.boundary.ordered_source_installed(lease).await?;
            state.source_host_attached = true;
        }
        Ok(())
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Open a previously reserved ordered batch. The caller owns the minted
    /// ID before this future begins, so it can inspect and retry cleanup even
    /// if setup fails or the future is cancelled.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_reserved_ordered_batch(
        &self,
        peer_id: &str,
        collection_id: u64,
        connection_generation: &str,
        database_generation: &str,
        requests: &[super::ordered_claims::OrderedClaimRequest],
        max_items: usize,
        max_bytes: usize,
        max_controls: usize,
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        self.open_reserved_ordered_batch_owned(
            peer_id,
            collection_id,
            None,
            connection_generation,
            database_generation,
            requests,
            max_items,
            max_bytes,
            max_controls,
            ctl,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn open_ordered_recording(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: u64,
        connection_generation: &str,
        database_generation: &str,
        requests: &[super::ordered_claims::OrderedClaimRequest],
        max_items: usize,
        max_bytes: usize,
        max_controls: usize,
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        self.open_reserved_ordered_batch_owned(
            peer_id,
            collection_id,
            Some(token),
            connection_generation,
            database_generation,
            requests,
            max_items,
            max_bytes,
            max_controls,
            ctl,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn open_reserved_ordered_batch_owned(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
        connection_generation: &str,
        database_generation: &str,
        requests: &[super::ordered_claims::OrderedClaimRequest],
        max_items: usize,
        max_bytes: usize,
        max_controls: usize,
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.open",
                )
            })?;
        super::ordered_claims::validate_ordered_claims(
            connection_generation,
            database_generation,
            requests,
        )?;
        // Reject a foreign consumer before it can occupy another peer's FIFO.
        let delivery = Arc::clone(&owner.lock().await.delivery_state);
        super::ordered_batch_cleanup::check_recording_owner(&lock_std(&delivery), token)?;
        // Respect any synchronously pre-bound FIFO ticket before taking the
        // group gate. Otherwise an earlier ticket can wait on this gate while
        // its holder waits behind that ticket.
        let ctl = self.bind_gatt_admission(peer_id, ctl)?;
        let window = ctl.budget.window(LIVENESS_OP);
        let admission = self
            .wait_gatt_admission(peer_id, &ctl, "gatt.ordered.open", window)
            .await?;
        let (gate, delivery) = {
            let owner = owner.lock().await;
            (
                Arc::clone(&owner.delivery_lock),
                Arc::clone(&owner.delivery_state),
            )
        };
        let (_gate, starting) = loop {
            let guard = match drive_link(
                &self.inner,
                peer_id,
                "gatt.ordered.open",
                &ctl.ticket,
                window,
                async { Ok(gate.lock().await) },
            )
            .await
            {
                Wait::Done(result) => result?,
                Wait::Expired => return Err(timed_out("gatt.ordered.open", window)),
                Wait::Cancelled => return Err(ctl.ticket.interruption("gatt.ordered.open")),
            };
            super::ordered_batch_cleanup::check_recording_owner(&lock_std(&delivery), token)?;
            if let Some(starting) = admission.try_begin_starting()? {
                break (guard, starting);
            }
            // A provisional lifecycle may need this group. Do not await its
            // decision while holding the group gate.
            drop(guard);
            match drive_link(
                &self.inner,
                peer_id,
                "gatt.ordered.open",
                &ctl.ticket,
                window,
                admission.wait(),
            )
            .await
            {
                Wait::Done(result) => result?,
                Wait::Expired => return Err(timed_out("gatt.ordered.open", window)),
                Wait::Cancelled => return Err(ctl.ticket.interruption("gatt.ordered.open")),
            }
        };
        self.prepare_ordered_claims_started(
            peer_id,
            collection_id,
            connection_generation,
            database_generation,
            requests,
            ctl.clone(),
            Some(starting),
        )
        .await?;
        self.install_ordered_batch_source(
            peer_id,
            collection_id,
            max_items,
            max_bytes,
            max_controls,
            ctl.clone(),
        )
        .await?;
        self.enable_ordered_batch(peer_id, collection_id, ctl).await
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Report native member acquisition and cleanup state when a source was
    /// installed; absence means setup failed before it produced a lease.
    pub async fn ordered_batch_source_status(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<Option<crate::boundary::OrderedSourceStatus>, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.status",
                )
            })?;
        let mut state = owner.lock().await;
        if state.source_prepare_started
            && !matches!(
                state.source,
                Some(crate::boundary::OrderedSourceCleanup::Lease(_))
            )
        {
            self.recover_ordered_batch_source(&mut state, peer_id, collection_id)
                .await?;
        }
        if let Some(crate::boundary::OrderedSourceCleanup::Lease(lease)) = state.source.clone() {
            self.recover_mobile_ordered_owner(&mut state, &lease)
                .await?;
            if state.empty_sink_installed {
                return Ok(None);
            }
            return self.ordered_owner_status(&lease).map(Some);
        }
        Ok(None)
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Return the exact installed source identity for transport metadata.
    pub async fn ordered_batch_source_lease(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<Option<crate::boundary::OrderedSourceLease>, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.status",
                )
            })?;
        let state = owner.lock().await;
        Ok(match &state.source {
            Some(crate::boundary::OrderedSourceCleanup::Lease(lease)) => Some(lease.clone()),
            _ => None,
        })
    }
}

/// Setup acquisition evidence is retained independently of later cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedMemberAcquisition {
    NotStarted,
    Acquired,
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedMemberSetupStatus {
    pub member_id: u64,
    pub acquisition: OrderedMemberAcquisition,
    pub effective_delivery: ObservedDelivery,
    pub cleanup_error: Option<DesktopError>,
    pub cleanup_released: bool,
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Report all requested members, including those setup never reached.
    pub async fn ordered_batch_member_status(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<Vec<OrderedMemberSetupStatus>, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.status",
                )
            })?;
        let state = owner.lock().await;
        Ok(state
            .requested_members
            .iter()
            .map(|id| {
                let claim = state.claims.iter().find(|claim| claim.member_id == *id);
                let acquisition = if state.acquired_members.contains(id)
                    || claim.is_some_and(|claim| claim.phase == OrderedClaimPhase::Acquired)
                {
                    OrderedMemberAcquisition::Acquired
                } else if state.indeterminate_members.contains(id)
                    || claim.is_some_and(|claim| {
                        matches!(
                            claim.phase,
                            OrderedClaimPhase::Dispatching | OrderedClaimPhase::Indeterminate
                        )
                    })
                {
                    OrderedMemberAcquisition::Indeterminate
                } else {
                    OrderedMemberAcquisition::NotStarted
                };
                let cleanup_error = state.member_cleanup_errors.get(id).cloned();
                let cleanup_released = claim
                    .is_none_or(|claim| claim.phase == OrderedClaimPhase::Released)
                    && (!state.source_claims_started.contains(id)
                        || state.source_claims_confirmed.contains(id));
                let effective_delivery = claim
                    .and_then(|claim| lock_std(&self.inner.deliveries).get(&claim.scope).copied())
                    .unwrap_or(ObservedDelivery::Unknown);
                OrderedMemberSetupStatus {
                    member_id: *id,
                    acquisition,
                    effective_delivery,
                    cleanup_error,
                    cleanup_released,
                }
            })
            .collect())
    }
}
