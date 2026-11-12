// crates/ubm-desktop/src/central/ordered_cleanup_execution.rs
//! Retained ordered batch cleanup and acknowledgment.

use super::ordered_claims::{OrderedClaimPhase, ordered_source_key};
use super::*;

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Fence the source and retry exact physical claims. Records and the
    /// terminal remain available to `poll_retained_ordered_batch`; cleanup is
    /// not reported complete until the backend confirms the FIFO was drained.
    pub async fn retry_retained_ordered_batch(
        &self,
        peer_id: &str,
        collection_id: u64,
        ctl: OpControl,
    ) -> Result<OrderedCleanupOutcome, DesktopError> {
        self.retry_ordered_cleanup_owned(peer_id, collection_id, None, ctl)
            .await
    }
    pub async fn retry_ordered_recording_cleanup(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: u64,
        ctl: OpControl,
    ) -> Result<OrderedCleanupOutcome, DesktopError> {
        self.retry_ordered_cleanup_owned(peer_id, collection_id, Some(token), ctl)
            .await
    }
    async fn retry_ordered_cleanup_owned(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
        ctl: OpControl,
    ) -> Result<OrderedCleanupOutcome, DesktopError> {
        // Cleanup never dispatches through a caller-owned GATT FIFO slot.
        // Retaining one while waiting for the group gate can block that gate's
        // current owner behind a ticket which cleanup itself cannot consume.
        if ctl.gatt_admission().is_some() {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                "gatt.ordered.cleanup",
            ));
        }
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.cleanup",
                )
            })?;
        let (gate, delivery, wake) = {
            let owner = owner.lock().await;
            (
                Arc::clone(&owner.delivery_lock),
                Arc::clone(&owner.delivery_state),
                Arc::clone(&owner.data_ready),
            )
        };
        super::ordered_batch_cleanup::check_recording_owner(&lock_std(&delivery), token)?;
        let Some(_gate) =
            super::ordered_batch_cleanup::recorder_delivery_gate(&gate, &wake, token.is_some())
                .await
        else {
            return Ok(OrderedCleanupOutcome::PendingSource);
        };
        super::ordered_batch_cleanup::check_recording_owner(&lock_std(&delivery), token)?;
        self.retry_retained_ordered_batch_internal(peer_id, collection_id, ctl)
            .await
    }
    pub(super) async fn retry_retained_ordered_batch_internal(
        &self,
        peer_id: &str,
        collection_id: u64,
        ctl: OpControl,
    ) -> Result<OrderedCleanupOutcome, DesktopError> {
        use crate::boundary::{OrderedSourceCleanup, OrderedSourceTransition};
        use ubm_core::ordered_notifications::{Acquisition, EndReason};
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.cleanup",
                )
            })?;
        let mut state = owner.lock().await;
        if state.cleanup_complete {
            return Ok(OrderedCleanupOutcome::Released);
        }
        // Close admission immediately, even when fencing or retirement fails.
        // The source still owns ordering its terminal after the admitted prefix.
        state.closing = true;
        if state.source_prepare_started
            && !matches!(state.source, Some(OrderedSourceCleanup::Lease(_)))
        {
            self.recover_ordered_batch_source(&mut state, peer_id, collection_id)
                .await?;
        }
        if matches!(
            state.source,
            Some(OrderedSourceCleanup::Indeterminate { .. })
        ) {
            return Ok(OrderedCleanupOutcome::PendingSource);
        }
        if let Some(OrderedSourceCleanup::Lease(lease)) = state.source.clone() {
            self.recover_mobile_ordered_owner(&mut state, &lease)
                .await?;
            if !state.fenced {
                if let Err(error) = self.inner.boundary.fence_ordered_source(&lease).await {
                    state.fence_error = Some(error.clone());
                    if error.code() == BleErrorCode::OperationTimedOut
                        && error.operation() == "gatt.ordered.barrier"
                    {
                        return Ok(OrderedCleanupOutcome::PendingSource);
                    }
                    return Err(error);
                }
                state.fenced = true;
            }
            if !state.retired {
                let reason = match &state.setup_error {
                    Some(primary) => EndReason::SetupFailed(
                        crate::ordered_notification_groups::failure_from_desktop(primary),
                    ),
                    None => state.retirement_reason.clone().unwrap_or(EndReason::Closed),
                };
                if let Err(error) = self
                    .inner
                    .boundary
                    .retire_ordered_source(&lease, reason)
                    .await
                {
                    state.fence_error = Some(error.clone());
                    if error.code() == BleErrorCode::OperationTimedOut
                        && error.operation() == "gatt.ordered.drain"
                    {
                        return Ok(OrderedCleanupOutcome::PendingDrain);
                    }
                    return Err(error);
                }
                state.retired = true;
            }
            if !state.empty_sink_installed {
                let status = self.ordered_owner_status(&lease)?;
                if !matches!(
                    status.phase,
                    ubm_core::ordered_notifications::Phase::Closing
                        | ubm_core::ordered_notifications::Phase::Terminal
                ) {
                    let reason = match &state.setup_error {
                        Some(error) => EndReason::SetupFailed(
                            crate::ordered_notification_groups::failure_from_desktop(error),
                        ),
                        None => state.retirement_reason.clone().unwrap_or(EndReason::Closed),
                    };
                    self.update_ordered_owner(
                        &lease,
                        OrderedSourceTransition::Close {
                            reason,
                            tail_uncertain: true,
                        },
                    )?;
                }
                state.closing = true;
            }
        }
        // The source must be retired and sealed before a terminal rejection
        // can be delivered and ACKed. Keep native marker/physical ownership
        // until that evidence is consumed and the transaction is forgotten.
        if state.marker_transaction.is_some() {
            return Ok(OrderedCleanupOutcome::PendingDrain);
        }
        if {
            let delivery = lock_std(&state.delivery_state);
            delivery.pending.is_some()
                || delivery.pending_discard.is_some()
                || delivery.marker_owner.is_some()
        } {
            return Ok(OrderedCleanupOutcome::PendingDrain);
        }
        let window = ctl.budget.window(LIVENESS_CLEANUP);
        let acquired: Vec<u64> = state
            .claims
            .iter()
            .filter(|claim| claim.phase == OrderedClaimPhase::Acquired)
            .map(|claim| claim.member_id)
            .collect();
        let indeterminate: Vec<u64> = state
            .claims
            .iter()
            .filter(|claim| {
                matches!(
                    claim.phase,
                    OrderedClaimPhase::Dispatching | OrderedClaimPhase::Indeterminate
                )
            })
            .map(|claim| claim.member_id)
            .collect();
        state.acquired_members.extend(acquired);
        state.indeterminate_members.extend(indeterminate);
        let source = state.source.clone();
        for index in 0..state.claims.len() {
            let member_id = state.claims[index].member_id;
            let mut registry_release = false;
            let mut enable_pending = false;
            if let Some(OrderedSourceCleanup::Lease(lease)) = &source {
                if !state.empty_sink_installed {
                    let status = self.ordered_owner_status(lease)?;
                    if let Some(member) = status
                        .members
                        .iter()
                        .find(|member| member.member.id == member_id)
                    {
                        enable_pending = member.enable_pending;
                        if matches!(
                            member.acquisition,
                            Acquisition::Acquired
                                | Acquisition::Indeterminate
                                | Acquisition::ReleaseFailed
                        ) {
                            self.update_ordered_owner(
                                lease,
                                OrderedSourceTransition::BeginRelease { member_id },
                            )?;
                            registry_release = true;
                        } else if member.acquisition == Acquisition::Releasing {
                            registry_release = true;
                        }
                    }
                }
            }
            let mut result = if state.claims[index].phase == OrderedClaimPhase::Released {
                Ok(())
            } else {
                self.rollback_ordered_claim(peer_id, &mut state.claims[index], &ctl.ticket, window)
                    .await
            };
            if result.is_ok()
                && state.source_claims_started.contains(&member_id)
                && !state.source_claims_confirmed.contains(&member_id)
            {
                if let Some(OrderedSourceCleanup::Lease(lease)) = &source {
                    let membership = lease
                        .members
                        .iter()
                        .find(|(id, _)| *id == member_id)
                        .map(|(_, membership)| *membership)
                        .ok_or_else(|| {
                            contract_error(
                                BleErrorCode::ProtocolMalformed,
                                BleErrorDomain::Cleanup,
                                "gatt.ordered.cleanup",
                            )
                        })?;
                    let activation = state.claims[index].activation.clone().ok_or_else(|| {
                        contract_error(
                            BleErrorCode::LifecycleInvalidState,
                            BleErrorDomain::Cleanup,
                            "gatt.ordered.cleanup",
                        )
                    })?;
                    result = self
                        .inner
                        .boundary
                        .confirm_ordered_source_cleanup(lease, member_id, membership, &activation)
                        .await;
                    if result.is_ok() {
                        state.source_claims_confirmed.insert(member_id);
                    }
                }
            }
            if registry_release {
                if let Some(OrderedSourceCleanup::Lease(lease)) = &source {
                    if result.is_ok() && enable_pending {
                        self.update_ordered_owner(
                            lease,
                            OrderedSourceTransition::EnableResult {
                                member_id,
                                outcome: Some(false),
                            },
                        )?;
                    }
                    self.update_ordered_owner(
                        lease,
                        OrderedSourceTransition::ReleaseResult {
                            member_id,
                            result: result
                                .as_ref()
                                .map(|_| ())
                                .map_err(|error| error.to_string()),
                        },
                    )?;
                }
            }
            match &result {
                Ok(()) => {
                    state.member_cleanup_errors.remove(&member_id);
                }
                Err(error) => {
                    state.member_cleanup_errors.insert(member_id, error.clone());
                }
            }
            result?;
        }
        if let Some(cleanup) = &state.source {
            if state.deferred_terminal.is_some() || !state.source_sealed {
                return Ok(OrderedCleanupOutcome::PendingDrain);
            }
            if let OrderedSourceCleanup::Lease(lease) = cleanup {
                if state.empty_sink_installed {
                    if !state.empty_terminal_seen
                        || state.empty_secondary_failure.is_some()
                        || self.empty_ordered_ingress_has_pending(
                            &ordered_source_key(lease),
                            collection_id,
                        )?
                    {
                        return Ok(OrderedCleanupOutcome::PendingDrain);
                    }
                } else {
                    let status = self.ordered_owner_status(lease)?;
                    if status.phase != ubm_core::ordered_notifications::Phase::Terminal
                        || !status.cleanup_complete
                    {
                        return Ok(OrderedCleanupOutcome::PendingDrain);
                    }
                }
            }
            if matches!(
                self.inner.boundary.release_ordered_source(cleanup).await?,
                crate::boundary::OrderedSourceRelease::Pending
            ) {
                return Ok(OrderedCleanupOutcome::PendingSource);
            }
            if let OrderedSourceCleanup::Lease(lease) = cleanup {
                if state.empty_sink_installed {
                    self.remove_empty_ordered_ingress(&ordered_source_key(lease), collection_id)?;
                } else if lease.ingress
                    == crate::boundary::OrderedSourceIngress::CentralRetainedBatch
                {
                    self.remove_mobile_ordered_ingress(&ordered_source_key(lease), collection_id)?;
                }
            }
        }
        state.starting.take();
        state.admission.take();
        state.source = None;
        state.cleanup_complete = true;
        Ok(OrderedCleanupOutcome::Released)
    }

    /// Whether all physical and source cleanup for this retained receipt is complete.
    pub async fn ordered_batch_cleanup_complete(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<bool, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Cleanup,
                    "gatt.ordered.status",
                )
            })?;
        let complete = owner.lock().await.cleanup_complete;
        Ok(complete)
    }

    /// Atomically acknowledge the final recorder receipt and release exclusive
    /// consumption, without exposing a gap for another consumer.
    pub async fn acknowledge_ordered_recording_batch(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: u64,
    ) -> Result<(), DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Cleanup,
                    "gatt.ordered.recording-ack",
                )
            })?;
        let (gate, delivery) = {
            let owner = owner.lock().await;
            (
                Arc::clone(&owner.delivery_lock),
                Arc::clone(&owner.delivery_state),
            )
        };
        let _gate = gate.lock().await;
        let mut state = owner.lock().await;
        let mut delivery = lock_std(&delivery);
        if state.receipt_acknowledged && delivery.last_recording_ack == Some(token) {
            return Ok(());
        }
        super::ordered_batch_cleanup::check_recording_owner(&delivery, Some(token))?;
        if state.marker_transaction.is_some()
            || !state.cleanup_complete
            || state.deferred_terminal.is_some()
            || state.empty_secondary_failure.is_some()
            || delivery.pending.is_some()
            || delivery.pending_discard.is_some()
        {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "gatt.ordered.recording-ack",
            ));
        }
        close_marker_reservations(&state)?;
        delivery.recording_owner = None;
        lock_std(&self.inner.ordered_recording_exclusions)
            .remove(&(peer_id.to_owned(), collection_id));
        delivery.last_recording_ack = Some(token);
        state.receipt_acknowledged = true;
        Ok(())
    }

    /// Acknowledge a fully cleaned receipt without discarding its replay state.
    pub async fn acknowledge_ordered_batch(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<(), DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Cleanup,
                    "gatt.ordered.acknowledge",
                )
            })?;
        let mut state = owner.lock().await;
        super::ordered_batch_cleanup::check_recording_owner(
            &lock_std(&state.delivery_state),
            None,
        )?;
        if !state.cleanup_complete
            || state.deferred_terminal.is_some()
            || state.empty_secondary_failure.is_some()
            || {
                let delivery = lock_std(&state.delivery_state);
                delivery.pending.is_some()
                    || delivery.pending_discard.is_some()
                    || delivery.marker_owner.is_some()
            }
        {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "gatt.ordered.acknowledge",
            ));
        }
        if state.receipt_acknowledged {
            return Ok(());
        }
        close_marker_reservations(&state)?;
        state.receipt_acknowledged = true;
        Ok(())
    }

    /// Forget an acknowledged receipt. Transport must authenticate the exact
    /// originating session before forwarding a retry for an absent issued ID.
    pub async fn forget_ordered_batch(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<(), DesktopError> {
        let Some(owner) = self.inner.ordered_batches.get(peer_id, collection_id) else {
            return if self.inner.ordered_batches.was_forgotten(collection_id) {
                Ok(())
            } else {
                Err(contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Cleanup,
                    "gatt.ordered.forget",
                ))
            };
        };
        let state = owner.lock().await;
        if !state.receipt_acknowledged {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "gatt.ordered.forget",
            ));
        }
        drop(state);
        self.inner
            .ordered_batches
            .remove_if_same(peer_id, collection_id, &owner);
        Ok(())
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Consume a bridge failure observed after the source terminal.
    pub async fn take_ordered_batch_secondary_failure(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<Option<ubm_core::ordered_notifications::Failure>, DesktopError> {
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
        super::ordered_batch_cleanup::check_recording_owner(
            &lock_std(&state.delivery_state),
            None,
        )?;
        Ok(state.empty_secondary_failure.take())
    }

    /// Setup and fence failures remain distinct even when both occurred.
    pub async fn retained_ordered_batch_errors(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<(Option<DesktopError>, Option<DesktopError>), DesktopError> {
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
        Ok((state.setup_error.clone(), state.fence_error.clone()))
    }
}

fn close_marker_reservations(
    state: &super::ordered_claims::OrderedBatchState,
) -> Result<(), DesktopError> {
    if let Some((_, _, gate)) = &state.marker_reservation {
        gate.compare_exchange(
            0,
            2,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .map_err(|_| {
            contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "gatt.ordered.marker-reservation",
            )
        })?;
    }
    if let Some(reclaim) = &state.reclaim_reservation {
        lock_std(reclaim).revoke();
    }
    Ok(())
}
