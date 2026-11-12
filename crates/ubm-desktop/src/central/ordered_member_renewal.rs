// crates/ubm-desktop/src/central/ordered_member_renewal.rs
//! Exact source-member renewal without changing the public member set.

use super::ordered_claims::ordered_source_key;
use super::*;
use crate::boundary::{OrderedSourceCleanup, OrderedSourceIngress};
use ubm_core::ordered_notifications::{Membership, Phase};

#[derive(Debug, Clone, Copy)]
pub(super) enum MemberRenewal {
    Retiring { old: Membership },
    Registering { old: Membership },
    Rebinding { old: Membership, new: Membership },
    Resuming { old: Membership, new: Membership },
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Retryable internal renewal after an explicit owner-requested retirement.
    /// Every phase is recorded before awaiting the native owner, so dropping
    /// this future preserves the exact identity needed by the next retry.
    pub async fn renew_ordered_member(
        &self,
        peer_id: &str,
        collection_id: u64,
        member_id: u64,
        ctl: OpControl,
    ) -> Result<Membership, DesktopError> {
        const OP: &str = "gatt.ordered.member-renew";
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(BleErrorCode::OwnershipDenied, BleErrorDomain::Gatt, OP)
            })?;
        let mut state = owner.lock().await;
        if state.closing || state.retired {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        let mut lease = match &state.source {
            Some(OrderedSourceCleanup::Lease(lease)) => lease.clone(),
            _ => {
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    OP,
                ));
            }
        };
        if self.ordered_owner_status(&lease)?.phase != Phase::Active {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        let old = lease
            .members
            .iter()
            .find(|(id, _)| *id == member_id)
            .map(|(_, member)| *member)
            .ok_or_else(|| {
                contract_error(BleErrorCode::ArgumentInvalid, BleErrorDomain::Gatt, OP)
            })?;
        state
            .renewals
            .entry(member_id)
            .or_insert(MemberRenewal::Retiring { old });
        let peer_key = self.known_peer_key(peer_id).await?;
        let window = ctl.budget.window(LIVENESS_OP);
        loop {
            self.refuse_before_admission(&ctl, OP)?;
            {
                let core = self.inner.core.lock().await;
                if core.connection_generation(&peer_key) != state.connection_generation
                    || core.database_generation(&peer_key) != state.database_generation
                {
                    return Err(contract_error(
                        BleErrorCode::GattStaleHandle,
                        BleErrorDomain::Gatt,
                        OP,
                    ));
                }
            }
            let phase = *state.renewals.get(&member_id).ok_or_else(|| {
                contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    OP,
                )
            })?;
            match phase {
                MemberRenewal::Retiring { old } => {
                    match drive_link(
                        &self.inner,
                        peer_id,
                        OP,
                        &ctl.ticket,
                        window,
                        self.inner
                            .boundary
                            .retire_ordered_member(&lease, member_id, old),
                    )
                    .await
                    {
                        Wait::Done(result) => result?,
                        Wait::Expired => return Err(timed_out(OP, window)),
                        Wait::Cancelled => return Err(ctl.ticket.interruption(OP)),
                    }
                    state
                        .renewals
                        .insert(member_id, MemberRenewal::Registering { old });
                }
                MemberRenewal::Registering { old } => {
                    let new = match drive_link(
                        &self.inner,
                        peer_id,
                        OP,
                        &ctl.ticket,
                        window,
                        self.inner
                            .boundary
                            .reregister_ordered_member(&lease, member_id, old),
                    )
                    .await
                    {
                        Wait::Done(result) => result?,
                        Wait::Expired => return Err(timed_out(OP, window)),
                        Wait::Cancelled => return Err(ctl.ticket.interruption(OP)),
                    };
                    state
                        .renewals
                        .insert(member_id, MemberRenewal::Rebinding { old, new });
                }
                MemberRenewal::Rebinding { old, new } => {
                    match lease.ingress {
                        OrderedSourceIngress::CentralRetainedBatch => self
                            .rebind_mobile_ordered_member(
                                &ordered_source_key(&lease),
                                collection_id,
                                member_id,
                                old,
                                new,
                            )?,
                        OrderedSourceIngress::BackendOwned => self
                            .inner
                            .boundary
                            .rebind_ordered_member(&lease, member_id, old, new)?,
                    }
                    let Some((_, membership)) =
                        lease.members.iter_mut().find(|(id, _)| *id == member_id)
                    else {
                        return Err(contract_error(
                            BleErrorCode::LifecycleInvalidState,
                            BleErrorDomain::Gatt,
                            OP,
                        ));
                    };
                    *membership = new;
                    state.source = Some(OrderedSourceCleanup::Lease(lease.clone()));
                    state
                        .renewals
                        .insert(member_id, MemberRenewal::Resuming { old, new });
                }
                MemberRenewal::Resuming { old: _, new } => {
                    match drive_link(
                        &self.inner,
                        peer_id,
                        OP,
                        &ctl.ticket,
                        window,
                        self.inner
                            .boundary
                            .resume_ordered_member(&lease, member_id, new),
                    )
                    .await
                    {
                        Wait::Done(result) => result?,
                        Wait::Expired => return Err(timed_out(OP, window)),
                        Wait::Cancelled => return Err(ctl.ticket.interruption(OP)),
                    }
                    state.renewals.remove(&member_id);
                    return Ok(new);
                }
            }
        }
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Reserve downstream capacity bound to the current exclusive consumer.
    pub(crate) async fn reserve_ordered_marker_credit(
        &self,
        peer_id: &str,
        collection_id: u64,
        marker_id: u64,
        recording_token: Option<u64>,
    ) -> Result<(), DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-credit",
                )
            })?;
        let gate = Arc::clone(&owner.lock().await.delivery_lock);
        let _gate = gate.lock().await;
        let state = owner.lock().await;
        {
            let delivery = lock_std(&state.delivery_state);
            super::ordered_batch_cleanup::check_recording_owner(&delivery, recording_token)?;
            if delivery
                .marker_owner
                .is_some_and(|owner| owner != (marker_id, recording_token))
            {
                return Err(contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-credit",
                ));
            }
        }
        if state.closing || state.retired {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.marker-credit",
            ));
        }
        let lease = match &state.source {
            Some(OrderedSourceCleanup::Lease(lease)) => lease,
            _ => {
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-credit",
                ));
            }
        };
        match lease.ingress {
            OrderedSourceIngress::BackendOwned => self
                .inner
                .boundary
                .reserve_ordered_marker(lease, marker_id)?,
            OrderedSourceIngress::CentralRetainedBatch => {
                lock_std(&self.inner.mobile_ordered_ingress)
                    .reserve_marker(collection_id, &ordered_source_key(lease), marker_id)
                    .map_err(|error| {
                        contract_error(
                            BleErrorCode::CapabilityLimited,
                            BleErrorDomain::Gatt,
                            "gatt.ordered.marker-credit",
                        )
                        .with_detail(format!("{error:?}"))
                    })?
            }
        }
        lock_std(&state.delivery_state).marker_owner = Some((marker_id, recording_token));
        Ok(())
    }

    /// Called only by the transaction owner after authoritative NotAdmitted.
    pub(crate) async fn release_ordered_marker_credit(
        &self,
        peer_id: &str,
        collection_id: u64,
        marker_id: u64,
        recording_token: Option<u64>,
    ) -> Result<(), DesktopError> {
        self.settle_marker_credit(peer_id, collection_id, marker_id, recording_token, true)
            .await
    }

    /// Called after matching marker/rejection has actually reached the FIFO.
    pub(crate) async fn finish_ordered_marker_credit(
        &self,
        peer_id: &str,
        collection_id: u64,
        marker_id: u64,
        recording_token: Option<u64>,
    ) -> Result<(), DesktopError> {
        self.settle_marker_credit(peer_id, collection_id, marker_id, recording_token, false)
            .await
    }

    async fn settle_marker_credit(
        &self,
        peer_id: &str,
        collection_id: u64,
        marker_id: u64,
        recording_token: Option<u64>,
        not_admitted: bool,
    ) -> Result<(), DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-credit",
                )
            })?;
        let gate = Arc::clone(&owner.lock().await.delivery_lock);
        let _gate = gate.lock().await;
        let state = owner.lock().await;
        {
            let delivery = lock_std(&state.delivery_state);
            super::ordered_batch_cleanup::check_recording_owner(&delivery, recording_token)?;
            if delivery.marker_owner != Some((marker_id, recording_token)) {
                return Err(contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-credit",
                ));
            }
        }
        let lease = match &state.source {
            Some(OrderedSourceCleanup::Lease(lease)) => lease,
            _ => {
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-credit",
                ));
            }
        };
        if not_admitted {
            match lease.ingress {
                OrderedSourceIngress::BackendOwned => self
                    .inner
                    .boundary
                    .release_ordered_marker(lease, marker_id)?,
                OrderedSourceIngress::CentralRetainedBatch => {
                    lock_std(&self.inner.mobile_ordered_ingress)
                        .release_marker(collection_id, &ordered_source_key(lease), marker_id)
                        .map_err(|error| {
                            contract_error(
                                BleErrorCode::LifecycleInvalidState,
                                BleErrorDomain::Gatt,
                                "gatt.ordered.marker-credit",
                            )
                            .with_detail(format!("{error:?}"))
                        })?
                }
            }
        } else {
            let pending = match lease.ingress {
                OrderedSourceIngress::BackendOwned => self
                    .inner
                    .boundary
                    .ordered_marker_credit_pending(lease, marker_id)?,
                OrderedSourceIngress::CentralRetainedBatch => {
                    lock_std(&self.inner.mobile_ordered_ingress)
                        .marker_reserved(collection_id, &ordered_source_key(lease), marker_id)
                        .map_err(|error| {
                            contract_error(
                                BleErrorCode::LifecycleInvalidState,
                                BleErrorDomain::Gatt,
                                "gatt.ordered.marker-credit",
                            )
                            .with_detail(format!("{error:?}"))
                        })?
                }
            };
            if pending {
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-credit",
                )
                .with_detail("matching marker has not been admitted"));
            }
        }
        lock_std(&state.delivery_state).marker_owner = None;
        Ok(())
    }
}
