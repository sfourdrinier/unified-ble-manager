// crates/ubm-desktop/src/central/ordered_batch_cleanup.rs
//! Retained ordered batch inspection and cleanup.

use super::ordered_claims::ordered_source_key;
use super::*;

/// Truthful cleanup progress; pending is not a native failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedCleanupOutcome {
    Released,
    PendingSource,
    PendingDrain,
}

/// One retained transport delivery. Records replay until their exact ID is acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedDelivery {
    pub delivery_id: Option<u64>,
    pub poll: ubm_core::ordered_notifications::Poll,
}

/// Typed evidence of a consumer-side transport discard; this is not source loss.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "discard evidence stays inline so cleanup preserves the exact retained ordered record without an allocation boundary"
)]
pub enum OrderedDiscardEvidence {
    Value {
        member: u64,
        ordinal: u64,
        membership: ubm_core::ordered_notifications::Membership,
        bytes: u64,
    },
    Loss(ubm_core::ordered_notifications::Record),
    /// A payload-free failure observation or explicit omission, retained verbatim.
    Observation(ubm_core::ordered_notifications::Record),
    Marker {
        marker_id: u64,
        ordinal: u64,
    },
    Terminal(ubm_core::ordered_notifications::Terminal),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedDiscardReceipt {
    pub receipt_id: u64,
    pub discarded_delivery_id: u64,
    pub evidence: OrderedDiscardEvidence,
}

// Retain a consumed terminal under the same owner lock as the poll. There is
// no cancellation point between source consumption and durable in-memory state.
fn split_terminal_observation(
    terminal: &mut ubm_core::ordered_notifications::Terminal,
) -> Option<ubm_core::ordered_notifications::Record> {
    use ubm_core::ordered_notifications::{EndReason, LossScope, LossUnit, Record, RejectedLoss};
    if !terminal.has_separable_observation() {
        return None;
    }
    // A rejected-values count has the same unit as an unattributed value loss.
    // If a distinct rejected-loss already exists, retain the legacy terminal
    // instead of combining facts whose relationship is unknown.
    let failure = match &terminal.reason {
        EndReason::SetupFailed(failure)
        | EndReason::SourceFailed(failure)
        | EndReason::HostReset(failure) => Some(failure.clone()),
        _ => None,
    };
    Some(Record::FailureObservation {
        failure,
        source_rejection: terminal.source_rejection.take(),
        source_failure_member: terminal.source_failure_member.take(),
        rejected_loss: terminal.rejected_loss.take().or_else(|| {
            terminal.rejected_values.take().map(|count| RejectedLoss {
                scope: LossScope::Unknown,
                unit: LossUnit::Values,
                count: Some(count),
                bytes: None,
            })
        }),
        tail_uncertain: terminal.tail_uncertain,
    })
}

fn retain_unsealed_terminal(
    state: &mut super::ordered_claims::OrderedBatchState,
    poll: ubm_core::ordered_notifications::Poll,
) -> ubm_core::ordered_notifications::Poll {
    use ubm_core::ordered_notifications::{EndReason, LateFailureSummary, Poll};
    if let Poll::Terminal(terminal) = &poll {
        if let Some(existing) = &mut state.deferred_terminal {
            if terminal.reason == EndReason::SourceOverflow {
                existing.merge_late_failures(LateFailureSummary {
                    tail_uncertain: terminal.tail_uncertain,
                    ..Default::default()
                });
                let mut late = terminal.clone();
                if let Some(observation) = split_terminal_observation(&mut late) {
                    return Poll::Record(observation);
                }
                if terminal.tail_uncertain {
                    return Poll::Record(
                        ubm_core::ordered_notifications::Record::FailureObservation {
                            failure: None,
                            source_rejection: None,
                            source_failure_member: None,
                            rejected_loss: None,
                            tail_uncertain: true,
                        },
                    );
                }
                return Poll::Closed;
            }
        } else {
            let mut retained = terminal.clone();
            let observation = split_terminal_observation(&mut retained);
            state.deferred_terminal = Some(retained);
            if let Some(observation) = observation {
                return Poll::Record(observation);
            }
        }
    }
    poll
}

fn marker_evidence(poll: &ubm_core::ordered_notifications::Poll) -> Option<u64> {
    use ubm_core::ordered_notifications::{Poll, Record, SourceRejection};
    match poll {
        Poll::Record(Record::Marker { marker_id, .. }) => Some(*marker_id),
        Poll::Record(Record::FailureObservation {
            source_rejection: Some(SourceRejection::Marker { marker_id }),
            ..
        }) => Some(*marker_id),
        Poll::Terminal(terminal) => match terminal.source_rejection {
            Some(SourceRejection::Marker { marker_id }) => Some(marker_id),
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn check_recording_owner(
    delivery: &super::ordered_claims::OrderedDeliveryState,
    token: Option<u64>,
) -> Result<(), DesktopError> {
    if delivery.recording_owner != token {
        return Err(contract_error(
            BleErrorCode::OwnershipDenied,
            BleErrorDomain::Gatt,
            "gatt.ordered.recording-owner",
        ));
    }
    Ok(())
}

// A recorder cannot wait on a gate retained by its caller's reclaim: that
// caller may be waiting for stop before releasing the permit. Coalesce wake
// waiters per gate, including when multiple polls or cleanup retries race.
fn wake_when_delivery_available(gate: Arc<Mutex<()>>, wake: Arc<tokio::sync::Notify>) {
    static WAITERS: std::sync::OnceLock<StdMutex<std::collections::HashSet<usize>>> =
        std::sync::OnceLock::new();
    let waiters = WAITERS.get_or_init(|| StdMutex::new(std::collections::HashSet::new()));
    let key = Arc::as_ptr(&gate) as usize;
    if !lock_std(waiters).insert(key) {
        return;
    }
    struct Registration {
        key: usize,
        waiters: &'static StdMutex<std::collections::HashSet<usize>>,
    }
    impl Drop for Registration {
        fn drop(&mut self) {
            lock_std(self.waiters).remove(&self.key);
        }
    }
    let registration = Registration { key, waiters };
    tokio::spawn(async move {
        let guard = gate.lock().await;
        drop(guard);
        drop(registration);
        wake.notify_one();
    });
}

pub(super) async fn recorder_delivery_gate<'a>(
    gate: &'a Arc<Mutex<()>>,
    wake: &Arc<tokio::sync::Notify>,
    recording: bool,
) -> Option<tokio::sync::MutexGuard<'a, ()>> {
    if !recording {
        return Some(gate.lock().await);
    }
    match gate.try_lock() {
        Ok(guard) => Some(guard),
        Err(_) => {
            wake_when_delivery_available(Arc::clone(gate), Arc::clone(wake));
            None
        }
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Fence new supervised recorder registrations before taking a process
    /// shutdown snapshot. Existing owners remain available for cleanup.
    pub fn seal_ordered_recording_registration(&self) {
        let _guard = lock_std(&self.inner.recording_registration_gate);
        self.inner
            .recording_registration_sealed
            .store(true, Ordering::SeqCst);
    }

    pub(crate) fn begin_ordered_recording_registration(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, ()>, DesktopError> {
        let guard = lock_std(&self.inner.recording_registration_gate);
        if self
            .inner
            .recording_registration_sealed
            .load(Ordering::SeqCst)
            || self.inner.shut_down.load(Ordering::SeqCst)
        {
            return Err(contract_error(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Gatt,
                "gatt.ordered.recording-owner",
            ));
        }
        Ok(guard)
    }

    /// Reserve a collection already owned by its recorder, before any Open.
    pub fn reserve_ordered_recording(&self, peer_id: &str) -> Result<(u64, u64), DesktopError> {
        let _guard = self.begin_ordered_recording_registration()?;
        self.reserve_ordered_recording_guarded(peer_id)
    }

    pub(crate) fn reserve_ordered_recording_guarded(
        &self,
        peer_id: &str,
    ) -> Result<(u64, u64), DesktopError> {
        let token = self
            .inner
            .next_ordered_recording_owner
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_add(1)
            })
            .map_err(|_| {
                contract_error(
                    BleErrorCode::LifecycleInvariantViolation,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.recording-owner",
                )
            })?;
        let (id, _) = self
            .inner
            .ordered_batches
            .reserve_with_owner(peer_id.to_owned(), Some(token))?;
        lock_std(&self.inner.ordered_recording_exclusions).insert((peer_id.to_owned(), id));
        Ok((id, token))
    }

    /// Claim exclusive consumption for a recorder. The token is host-lifetime
    /// unique and remains owned across cancellation until explicitly released.
    pub async fn claim_ordered_recording(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<u64, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.recording-owner",
                )
            })?;
        let (gate, state) = {
            let owner = owner.lock().await;
            (
                Arc::clone(&owner.delivery_lock),
                Arc::clone(&owner.delivery_state),
            )
        };
        let _gate = gate.lock().await;
        let mut batch_owner = owner.lock().await;
        if batch_owner.receipt_acknowledged || batch_owner.marker_transaction.is_some() {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.recording-owner",
            ));
        }
        let mut state = lock_std(&state);
        if state.recording_owner.is_some()
            || state.pending.is_some()
            || state.pending_discard.is_some()
        {
            return Err(contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Gatt,
                "gatt.ordered.recording-owner",
            ));
        }
        let token = self
            .inner
            .next_ordered_recording_owner
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_add(1)
            })
            .map_err(|_| {
                contract_error(
                    BleErrorCode::LifecycleInvariantViolation,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.recording-owner",
                )
            })?;
        if let Some((_, _, gate)) = &batch_owner.marker_reservation {
            gate.compare_exchange(
                0,
                2,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.recording-owner",
                )
            })?;
        }
        if let Some(reclaim) = &batch_owner.reclaim_reservation {
            lock_std(reclaim).revoke();
        }
        batch_owner.reclaim_reservation = None;
        batch_owner.marker_reservation = None;
        lock_std(&self.inner.ordered_recording_exclusions)
            .insert((peer_id.to_owned(), collection_id));
        state.recording_owner = Some(token);
        Ok(token)
    }

    /// Coalesced per-collection wake. Register `notified()` before polling.
    pub async fn ordered_recording_wake(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: u64,
    ) -> Result<Arc<tokio::sync::Notify>, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.recording-owner",
                )
            })?;
        let state = owner.lock().await;
        check_recording_owner(&lock_std(&state.delivery_state), Some(token))?;
        Ok(Arc::clone(&state.data_ready))
    }

    /// Exact source generation for durable binding after native Open.
    pub async fn ordered_recording_source_generation(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: u64,
    ) -> Result<Option<ubm_core::ordered_notifications::SourceGeneration>, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.recording-owner",
                )
            })?;
        let state = owner.lock().await;
        check_recording_owner(&lock_std(&state.delivery_state), Some(token))?;
        Ok(state.observed_source_generation.clone())
    }

    pub async fn release_ordered_recording(
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
                    BleErrorDomain::Gatt,
                    "gatt.ordered.recording-owner",
                )
            })?;
        let (gate, state) = {
            let owner = owner.lock().await;
            (
                Arc::clone(&owner.delivery_lock),
                Arc::clone(&owner.delivery_state),
            )
        };
        let _gate = gate.lock().await;
        let mut batch_owner = owner.lock().await;
        if batch_owner.marker_transaction.is_some() {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.recording-owner",
            )
            .with_detail("marker transaction remains owned"));
        }
        let mut state = lock_std(&state);
        check_recording_owner(&state, Some(token))?;
        if state.pending.is_some()
            || state.pending_discard.is_some()
            || state.marker_owner.is_some()
        {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.recording-owner",
            ));
        }
        if let Some((_, _, gate)) = &batch_owner.marker_reservation {
            gate.compare_exchange(
                0,
                2,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| {
                contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.recording-owner",
                )
            })?;
        }
        if let Some(reclaim) = &batch_owner.reclaim_reservation {
            lock_std(reclaim).revoke();
        }
        batch_owner.reclaim_reservation = None;
        batch_owner.marker_reservation = None;
        state.recording_owner = None;
        lock_std(&self.inner.ordered_recording_exclusions)
            .remove(&(peer_id.to_owned(), collection_id));
        Ok(())
    }

    pub async fn poll_ordered_recording(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: u64,
    ) -> Result<OrderedDelivery, DesktopError> {
        self.poll_ordered_delivery_owned(peer_id, collection_id, Some(token))
            .await
    }
    pub async fn acknowledge_ordered_recording(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: u64,
        delivery_id: u64,
    ) -> Result<(), DesktopError> {
        self.acknowledge_ordered_delivery_owned(peer_id, collection_id, Some(token), delivery_id)
            .await
    }

    /// Retain one record or terminal across transport decode/cancellation.
    pub async fn poll_ordered_delivery(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<OrderedDelivery, DesktopError> {
        self.poll_ordered_delivery_owned(peer_id, collection_id, None)
            .await
    }
    async fn poll_ordered_delivery_owned(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
    ) -> Result<OrderedDelivery, DesktopError> {
        use ubm_core::ordered_notifications::Poll;
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.delivery",
                )
            })?;
        let (lock, delivery_state, wake) = {
            let state = owner.lock().await;
            (
                Arc::clone(&state.delivery_lock),
                Arc::clone(&state.delivery_state),
                Arc::clone(&state.data_ready),
            )
        };
        check_recording_owner(&lock_std(&delivery_state), token)?;
        let Some(_delivery) = recorder_delivery_gate(&lock, &wake, token.is_some()).await else {
            // No delivery can be taken until the retained gate is available.
            return Ok(OrderedDelivery {
                delivery_id: None,
                poll: Poll::Empty,
            });
        };
        check_recording_owner(&lock_std(&delivery_state), token)?;
        if lock_std(&delivery_state).pending_discard.is_some() {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.discard-pending",
            )
            .with_detail("discard receipt must be acknowledged before continuing"));
        }
        if let Some((id, poll)) = lock_std(&delivery_state).pending.clone() {
            return Ok(OrderedDelivery {
                delivery_id: Some(id),
                poll,
            });
        }
        let id = lock_std(&delivery_state).next_id.ok_or_else(|| {
            contract_error(
                BleErrorCode::LifecycleInvariantViolation,
                BleErrorDomain::Gatt,
                "gatt.ordered.delivery-id",
            )
            .with_detail("delivery identifiers exhausted")
        })?;
        let poll = self
            .poll_retained_ordered_batch_inner(peer_id, collection_id)
            .await?;
        if matches!(poll, Poll::Empty | Poll::Closed) {
            return Ok(OrderedDelivery {
                delivery_id: None,
                poll,
            });
        }
        // No await between consumption and retention: cancellation cannot lose it.
        let mut delivery = lock_std(&delivery_state);
        delivery.next_id = id.checked_add(1);
        delivery.pending = Some((id, poll.clone()));
        Ok(OrderedDelivery {
            delivery_id: Some(id),
            poll,
        })
    }

    /// Acknowledge exactly the delivery returned by poll_ordered_delivery.
    pub async fn acknowledge_ordered_delivery(
        &self,
        peer_id: &str,
        collection_id: u64,
        delivery_id: u64,
    ) -> Result<(), DesktopError> {
        self.acknowledge_ordered_delivery_owned(peer_id, collection_id, None, delivery_id)
            .await
    }
    async fn acknowledge_ordered_delivery_owned(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
        delivery_id: u64,
    ) -> Result<(), DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.delivery-ack",
                )
            })?;
        let (gate, delivery_state) = {
            let state = owner.lock().await;
            (
                Arc::clone(&state.delivery_lock),
                Arc::clone(&state.delivery_state),
            )
        };
        let _gate = gate.lock().await;
        let mut delivery = lock_std(&delivery_state);
        check_recording_owner(&delivery, token)?;
        if delivery.pending.as_ref().map(|(id, _)| *id) == Some(delivery_id) {
            if token.is_none()
                && delivery.pending.as_ref().is_some_and(|(_, poll)| {
                    crate::ordered_wire::encode_versioned(poll.clone(), 2).is_none()
                })
            {
                return Err(contract_error(
                    BleErrorCode::ProtocolIncompatible,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.delivery-ack",
                )
                .with_detail(
                    "retained delivery is not representable within ordered metadata revision 2",
                ));
            }
            if let Some(marker_id) = delivery
                .pending
                .as_ref()
                .and_then(|(_, poll)| marker_evidence(poll))
            {
                delivery.marker_evidence_acked = Some(marker_id);
            }
            delivery.pending = None;
            delivery.last_acked = Some(delivery_id);
        } else if delivery.last_acked != Some(delivery_id) {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                "gatt.ordered.delivery-ack",
            ));
        }
        Ok(())
    }

    /// Convert an exact unconsumable delivery into bounded typed evidence.
    /// The evidence replays until explicitly acknowledged; it never becomes a
    /// fabricated source-loss control.
    pub async fn discard_ordered_delivery(
        &self,
        peer_id: &str,
        collection_id: u64,
        delivery_id: u64,
    ) -> Result<OrderedDiscardReceipt, DesktopError> {
        use ubm_core::ordered_notifications::{Poll, Record};
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.discard",
                )
            })?;
        let (gate, delivery_state, lease, retired) = {
            let state = owner.lock().await;
            let lease = match &state.source {
                Some(crate::boundary::OrderedSourceCleanup::Lease(lease)) => lease.clone(),
                _ => {
                    return Err(contract_error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.discard",
                    )
                    .with_detail("source is not available for sealing"));
                }
            };
            (
                Arc::clone(&state.delivery_lock),
                Arc::clone(&state.delivery_state),
                lease,
                state.retired,
            )
        };
        let _gate = gate.lock().await;
        check_recording_owner(&lock_std(&delivery_state), None)?;
        if !retired {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.discard",
            )
            .with_detail("source must be fenced and retired before discard"));
        }
        let seal = tokio::time::timeout(
            LIVENESS_CLEANUP,
            self.inner.boundary.seal_ordered_source(&lease),
        )
        .await
        .map_err(|_| {
            contract_error(
                BleErrorCode::OperationTimedOut,
                BleErrorDomain::Gatt,
                "gatt.ordered.seal",
            )
        })??;
        if seal != crate::boundary::OrderedSourceSeal::Sealed {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.discard",
            )
            .with_detail("source seal remains pending"));
        }
        owner.lock().await.source_sealed = true;
        let mut delivery = lock_std(&delivery_state);
        if let Some(receipt) = &delivery.pending_discard {
            if receipt.discarded_delivery_id == delivery_id {
                return Ok(receipt.clone());
            }
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.discard",
            ));
        }
        let evidence = match delivery.pending.as_ref() {
            Some((
                id,
                Poll::Record(Record::Value {
                    member,
                    ordinal,
                    membership,
                    value,
                    ..
                }),
            )) if *id == delivery_id => OrderedDiscardEvidence::Value {
                member: *member,
                ordinal: *ordinal,
                membership: *membership,
                bytes: u64::try_from(value.len()).map_err(|_| {
                    contract_error(
                        BleErrorCode::LifecycleInvariantViolation,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.discard",
                    )
                })?,
            },
            Some((id, Poll::Record(Record::Marker { marker_id, ordinal })))
                if *id == delivery_id =>
            {
                OrderedDiscardEvidence::Marker {
                    marker_id: *marker_id,
                    ordinal: *ordinal,
                }
            }
            Some((id, Poll::Record(record @ Record::Loss { .. }))) if *id == delivery_id => {
                OrderedDiscardEvidence::Loss(record.clone())
            }
            Some((
                id,
                Poll::Record(
                    record @ (Record::FailureObservation { .. }
                    | Record::FailureObservationsOmitted { .. }),
                ),
            )) if *id == delivery_id => OrderedDiscardEvidence::Observation(record.clone()),
            Some((id, Poll::Terminal(terminal))) if *id == delivery_id => {
                OrderedDiscardEvidence::Terminal(terminal.clone())
            }
            _ => {
                return Err(contract_error(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.discard",
                ));
            }
        };
        let receipt_id = delivery.next_id.ok_or_else(|| {
            contract_error(
                BleErrorCode::LifecycleInvariantViolation,
                BleErrorDomain::Gatt,
                "gatt.ordered.delivery-id",
            )
            .with_detail("delivery identifiers exhausted")
        })?;
        let receipt = OrderedDiscardReceipt {
            receipt_id,
            discarded_delivery_id: delivery_id,
            evidence,
        };
        if crate::ordered_wire::encode_discard_receipt_versioned(&receipt, 2).is_none() {
            return Err(contract_error(
                BleErrorCode::ProtocolIncompatible,
                BleErrorDomain::Gatt,
                "gatt.ordered.discard",
            )
            .with_detail(
                "discard evidence is not representable within ordered metadata revision 2",
            ));
        }
        delivery.next_id = receipt_id.checked_add(1);
        delivery.pending = None;
        delivery.pending_discard = Some(receipt.clone());
        Ok(receipt)
    }

    pub async fn poll_ordered_discard_receipt(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<Option<OrderedDiscardReceipt>, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.discard",
                )
            })?;
        let (gate, state) = {
            let owner = owner.lock().await;
            (
                Arc::clone(&owner.delivery_lock),
                Arc::clone(&owner.delivery_state),
            )
        };
        let _gate = gate.lock().await;
        check_recording_owner(&lock_std(&state), None)?;
        let receipt = lock_std(&state).pending_discard.clone();
        Ok(receipt)
    }

    pub async fn acknowledge_ordered_discard_receipt(
        &self,
        peer_id: &str,
        collection_id: u64,
        receipt_id: u64,
    ) -> Result<(), DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.discard-ack",
                )
            })?;
        let (gate, state) = {
            let owner = owner.lock().await;
            (
                Arc::clone(&owner.delivery_lock),
                Arc::clone(&owner.delivery_state),
            )
        };
        let _gate = gate.lock().await;
        check_recording_owner(&lock_std(&state), None)?;
        let mut delivery = lock_std(&state);
        if delivery
            .pending_discard
            .as_ref()
            .map(|receipt| receipt.receipt_id)
            == Some(receipt_id)
        {
            if let Some(receipt) = &delivery.pending_discard {
                let marker = match &receipt.evidence {
                    OrderedDiscardEvidence::Marker { marker_id, .. } => Some(*marker_id),
                    OrderedDiscardEvidence::Terminal(terminal) => match terminal.source_rejection {
                        Some(ubm_core::ordered_notifications::SourceRejection::Marker {
                            marker_id,
                        }) => Some(marker_id),
                        _ => None,
                    },
                    OrderedDiscardEvidence::Observation(record) => marker_evidence(
                        &ubm_core::ordered_notifications::Poll::Record(record.clone()),
                    ),
                    _ => None,
                };
                if let Some(marker_id) = marker {
                    delivery.marker_evidence_acked = Some(marker_id);
                }
            }
            delivery.pending_discard = None;
            delivery.last_discard_acked = Some(receipt_id);
        } else if delivery.last_discard_acked != Some(receipt_id) {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                "gatt.ordered.discard-ack",
            ));
        }
        Ok(())
    }

    /// Poll values in source order; a terminal is withheld until the exact
    /// source is sealed and all retained controls have been acknowledged.
    pub async fn poll_retained_ordered_batch(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<ubm_core::ordered_notifications::Poll, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.poll",
                )
            })?;
        let (gate, state) = {
            let owner = owner.lock().await;
            (
                Arc::clone(&owner.delivery_lock),
                Arc::clone(&owner.delivery_state),
            )
        };
        let _gate = gate.lock().await;
        check_recording_owner(&lock_std(&state), None)?;
        self.poll_retained_ordered_batch_inner(peer_id, collection_id)
            .await
    }
    async fn poll_retained_ordered_batch_inner(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<ubm_core::ordered_notifications::Poll, DesktopError> {
        use ubm_core::ordered_notifications::Poll;
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.poll",
                )
            })?;
        let has_terminal = owner.lock().await.deferred_terminal.is_some();
        if !has_terminal {
            match self
                .poll_unsealed_ordered_batch(peer_id, collection_id)
                .await?
            {
                Poll::Terminal(_) => {}
                other => return Ok(other),
            }
        }
        let lease = {
            let state = owner.lock().await;
            match &state.source {
                Some(crate::boundary::OrderedSourceCleanup::Lease(lease)) => lease.clone(),
                _ => return Ok(Poll::Empty),
            }
        };
        // Drain one retained observation per poll, including while seal is
        // pending. Delivery ownership replays it until the consumer ACKs.
        match self
            .poll_unsealed_ordered_batch(peer_id, collection_id)
            .await?
        {
            Poll::Record(record) => return Ok(Poll::Record(record)),
            Poll::Empty | Poll::Closed => {}
            Poll::Terminal(_) => {
                return Err(contract_error(
                    BleErrorCode::ProtocolViolation,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.post-terminal",
                ));
            }
        }
        if !owner.lock().await.source_sealed {
            let seal = tokio::time::timeout(
                LIVENESS_CLEANUP,
                self.inner.boundary.seal_ordered_source(&lease),
            )
            .await
            .unwrap_or_else(|_| {
                Err(contract_error(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.seal",
                ))
            })?;
            if matches!(seal, crate::boundary::OrderedSourceSeal::Pending) {
                return Ok(Poll::Empty);
            }
            owner.lock().await.source_sealed = true;
        }
        // Seal may have admitted more records. Never publish terminal until
        // the retained prefix has been offered to the consumer.
        match self
            .poll_unsealed_ordered_batch(peer_id, collection_id)
            .await?
        {
            Poll::Record(record) => return Ok(Poll::Record(record)),
            Poll::Empty | Poll::Closed => {}
            Poll::Terminal(_) => {
                return Err(contract_error(
                    BleErrorCode::ProtocolViolation,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.post-terminal",
                ));
            }
        }
        let mut state = owner.lock().await;
        let terminal = state.deferred_terminal.take().ok_or_else(|| {
            contract_error(
                BleErrorCode::LifecycleInvariantViolation,
                BleErrorDomain::Gatt,
                "gatt.ordered.seal",
            )
        })?;
        Ok(Poll::Terminal(terminal))
    }
    async fn poll_unsealed_ordered_batch(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<ubm_core::ordered_notifications::Poll, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.poll",
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
        }
        if state.empty_sink_installed {
            use super::empty_ordered_source::EmptyOrderedPoll;
            use ubm_core::ordered_notifications::{
                EndReason, LossScope, LossUnit, Poll, Record, Terminal,
            };
            let Some(crate::boundary::OrderedSourceCleanup::Lease(lease)) = state.source.clone()
            else {
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.poll",
                ));
            };
            if state.empty_secondary_failure.is_some() {
                return Ok(Poll::Closed);
            }
            loop {
                let record = self.poll_empty_ordered_ingress_internal(
                    &ordered_source_key(&lease),
                    collection_id,
                )?;
                if matches!(
                    record,
                    EmptyOrderedPoll::Record(OrderedIngressRecord::Barrier {
                        uncertain_tail: false,
                        ..
                    })
                ) {
                    continue;
                }
                let result = match record {
                    EmptyOrderedPoll::Empty => Ok(if state.empty_terminal_seen {
                        Poll::Closed
                    } else {
                        Poll::Empty
                    }),
                    EmptyOrderedPoll::Record(OrderedIngressRecord::Loss {
                        scope,
                        unit,
                        count,
                        bytes,
                    }) => Ok(Poll::Record(Record::Loss {
                        scope,
                        unit,
                        count,
                        bytes,
                    })),
                    EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal {
                        reason: EndReason::SourceOverflow,
                        rejection,
                        failure_member,
                        rejected_loss,
                        uncertain_tail,
                    }) if state.empty_terminal_seen => {
                        Ok(Poll::Record(Record::FailureObservation {
                            failure: None,
                            source_rejection: rejection,
                            source_failure_member: failure_member,
                            rejected_loss,
                            tail_uncertain: uncertain_tail,
                        }))
                    }
                    EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal {
                        reason,
                        rejection,
                        failure_member,
                        rejected_loss,
                        uncertain_tail,
                    }) => {
                        state.empty_terminal_seen = true;
                        let failure_observations_omitted = false;
                        Ok(Poll::Terminal(Terminal {
                            reason,
                            tail_uncertain: uncertain_tail,
                            rejected_values: None,
                            rejected_loss,
                            source_rejection: rejection,
                            source_failure_member: failure_member,
                            secondary_failure: None,
                            additional_failure_count: 0,
                            failure_count_overflowed: false,
                            failure_observations_omitted,
                        }))
                    }
                    EmptyOrderedPoll::Record(OrderedIngressRecord::FailureObservationsOmitted) => {
                        Ok(Poll::Record(Record::FailureObservationsOmitted {
                            count: None,
                        }))
                    }
                    EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { failure })
                        if state.empty_terminal_seen =>
                    {
                        Ok(Poll::Record(Record::FailureObservation {
                            failure: Some(failure),
                            source_rejection: None,
                            source_failure_member: None,
                            rejected_loss: None,
                            tail_uncertain: false,
                        }))
                    }
                    EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { failure }) => {
                        state.empty_terminal_seen = true;
                        Ok(Poll::Terminal(Terminal {
                            reason: EndReason::SourceFailed(failure),
                            tail_uncertain: true,
                            rejected_values: None,
                            rejected_loss: None,
                            source_rejection: None,
                            source_failure_member: None,
                            secondary_failure: None,
                            additional_failure_count: 0,
                            failure_count_overflowed: false,
                            failure_observations_omitted: false,
                        }))
                    }
                    EmptyOrderedPoll::Record(OrderedIngressRecord::Barrier {
                        uncertain_tail: false,
                        ..
                    }) => unreachable!("handled before mapping"),
                    EmptyOrderedPoll::Record(OrderedIngressRecord::Barrier {
                        uncertain_tail: true,
                        ..
                    }) if state.empty_terminal_seen => {
                        Ok(Poll::Record(Record::FailureObservation {
                            failure: None,
                            source_rejection: None,
                            source_failure_member: None,
                            rejected_loss: None,
                            tail_uncertain: true,
                        }))
                    }
                    EmptyOrderedPoll::Record(OrderedIngressRecord::Barrier {
                        uncertain_tail: true,
                        ..
                    })
                    | EmptyOrderedPoll::Record(OrderedIngressRecord::Fence { .. }) => {
                        Ok(Poll::Record(Record::Loss {
                            scope: LossScope::Unknown,
                            unit: LossUnit::SourceRecords,
                            count: None,
                            bytes: None,
                        }))
                    }
                    EmptyOrderedPoll::Record(
                        OrderedIngressRecord::Value { .. } | OrderedIngressRecord::Marker { .. },
                    ) => Err(contract_error(
                        BleErrorCode::ProtocolViolation,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.empty-source-value",
                    )),
                }?;
                return Ok(retain_unsealed_terminal(&mut state, result));
            }
        }
        let result = match &state.source {
            Some(crate::boundary::OrderedSourceCleanup::Lease(lease)) => match lease.ingress {
                crate::boundary::OrderedSourceIngress::BackendOwned => {
                    self.inner.boundary.poll_ordered_source(lease).await
                }
                crate::boundary::OrderedSourceIngress::CentralRetainedBatch => self
                    .poll_mobile_ordered_ingress_internal(
                        lease.collection_id,
                        &ordered_source_key(lease),
                    ),
            },
            Some(crate::boundary::OrderedSourceCleanup::Indeterminate { .. }) | None => {
                Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.poll",
                )
                .with_detail("ordered source preparation has not produced an inspectable lease"))
            }
        }?;
        Ok(retain_unsealed_terminal(&mut state, result))
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use ubm_core::ordered_notifications::{Poll, Record, SourceRejection};

    #[test]
    fn first_terminal_rejection_becomes_one_observation_before_final_terminal() {
        use ubm_core::ordered_notifications::{EndReason, Terminal};
        let mut terminal = Terminal {
            reason: EndReason::SourceOverflow,
            tail_uncertain: true,
            rejected_values: None,
            rejected_loss: None,
            source_rejection: Some(SourceRejection::Value { bytes: 7 }),
            source_failure_member: None,
            secondary_failure: None,
            additional_failure_count: 0,
            failure_count_overflowed: false,
            failure_observations_omitted: false,
        };
        assert!(matches!(
            split_terminal_observation(&mut terminal),
            Some(Record::FailureObservation {
                source_rejection: Some(SourceRejection::Value { bytes: 7 }),
                tail_uncertain: true,
                ..
            })
        ));
        assert!(terminal.source_rejection.is_none());
        assert!(split_terminal_observation(&mut terminal).is_none());
        terminal.reason = EndReason::CapacityExceeded;
        terminal.rejected_values = Some(1);
        assert!(matches!(
            split_terminal_observation(&mut terminal),
            Some(Record::FailureObservation {
                rejected_loss: Some(ubm_core::ordered_notifications::RejectedLoss {
                    scope: ubm_core::ordered_notifications::LossScope::Unknown,
                    unit: ubm_core::ordered_notifications::LossUnit::Values,
                    count: Some(1),
                    bytes: None
                }),
                ..
            })
        ));
        assert!(terminal.rejected_values.is_none());
    }

    #[test]
    fn marker_rejection_observation_is_acknowledged_as_marker_evidence() {
        let poll = Poll::Record(Record::FailureObservation {
            failure: None,
            source_rejection: Some(SourceRejection::Marker { marker_id: 17 }),
            source_failure_member: None,
            rejected_loss: None,
            tail_uncertain: false,
        });
        assert_eq!(marker_evidence(&poll), Some(17));
    }
}
