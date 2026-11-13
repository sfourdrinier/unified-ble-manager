// crates/ubm-desktop/src/fake_ordered_source.rs
//! Deterministic ordered-source owner for the scripted radio.
use crate::boundary::{InstanceKey, OrderedSourceLease, PreparedNotificationActivation};
use crate::ordered_notification_groups::{OrderedGroupRegistry, SourceKey};
use std::collections::{HashMap, HashSet, VecDeque};
use ubm_core::ordered_notifications::Membership;

#[derive(Default)]
pub(super) struct FakeOrderedSource {
    pub registry: OrderedGroupRegistry,
    pub marker_transactions: HashMap<
        (u64, u64),
        (
            crate::boundary::OrderedMarkerWriteRequest,
            crate::boundary::OrderedMarkerWriteStatus,
        ),
    >,
    pub leases: HashMap<u64, OrderedSourceLease>,
    pub claims: HashMap<u64, Vec<(u64, Membership, PreparedNotificationActivation)>>,
    pub prepared: HashMap<(InstanceKey, u64), PreparedNotificationActivation>,
    pub settlements: HashMap<(InstanceKey, u64), crate::boundary::OrderedNotificationSettlement>,
    pub origins: HashMap<(InstanceKey, u64), (u64, u64, Membership)>,
    pub ordinals: HashMap<String, u64>,
    pub generations: HashMap<String, u64>,
    pub next_identity: u64,
    pub renewals: HashMap<(u64, u64), FakeRenewal>,
    pub buffered: HashMap<u64, VecDeque<FakeRecord>>,
    pub pending_records: HashMap<SourceKey, VecDeque<FakePendingRecord>>,
    pub bounds: HashMap<u64, (usize, usize)>,
    pub failed: HashSet<u64>,
    pub sealed: HashSet<u64>,
    pub overflows: HashMap<u64, Option<ubm_core::ordered_notifications::SourceRejection>>,
}
#[derive(Clone)]
pub(super) struct FakeRenewal {
    pub old: Membership,
    pub new: Option<Membership>,
    pub rebound: bool,
    pub resumed: bool,
}
#[derive(Clone)]
pub(super) enum FakeRecord {
    Value(
        Membership,
        u64,
        Vec<u8>,
        ubm_core::central::NotificationProvenance,
    ),
    Loss(Vec<Membership>, Option<u64>),
}
pub(super) struct FakePendingRecord {
    lease: ubm_core::ordered_notifications::OccurrenceLease,
    bytes: usize,
}
impl FakeOrderedSource {
    pub(super) fn flush_pending_records(&mut self, source: &SourceKey) -> Result<(), DesktopError> {
        loop {
            let Some(group) = self
                .leases
                .values()
                .find(|lease| Self::source(lease) == *source)
                .map(|lease| lease.collection_id)
            else {
                return Err(error("source missing"));
            };
            let Some(record) = self
                .pending_records
                .get_mut(source)
                .and_then(|q| q.front_mut())
            else {
                return Ok(());
            };
            let result = self.registry.with_collection(group, |owner| {
                owner.retry_occurrence(&source.generation, group, &mut record.lease)
            });
            match result {
                Ok(_) => {
                    let queue = self
                        .pending_records
                        .get_mut(source)
                        .expect("pending synthetic source");
                    queue.pop_front();
                    if queue.is_empty() {
                        self.pending_records.remove(source);
                    }
                }
                Err(crate::ordered_notification_groups::RegistryError::Collection(
                    ubm_core::ordered_notifications::Error::PendingCapacity,
                )) => return Ok(()),
                Err(failure) => return Err(error(failure)),
            }
        }
    }

    fn retain_record(
        &mut self,
        group: u64,
        source: &SourceKey,
        record: ubm_core::ordered_notifications::RetainedRecord,
        bytes: usize,
    ) -> Result<(), DesktopError> {
        let lease = self
            .registry
            .with_collection(group, |owner| {
                owner
                    .retain_occurrence(&source.generation, group, record)
                    .map_err(|(failure, _)| failure)
            })
            .map_err(error)?;
        self.pending_records
            .entry(source.clone())
            .or_default()
            .push_back(FakePendingRecord { lease, bytes });
        self.flush_pending_records(source)
    }

    fn buffer_record(&mut self, group: u64, record: FakeRecord) -> bool {
        let (max_records, max_bytes) = self.bounds.get(&group).copied().unwrap_or((0, 0));
        let Some(queue) = self.buffered.get_mut(&group) else {
            return false;
        };
        let size = |r: &FakeRecord| match r {
            FakeRecord::Value(_, _, v, _) => v.len(),
            FakeRecord::Loss(_, _) => 0,
        };
        if queue.len() >= max_records
            || queue
                .iter()
                .map(size)
                .sum::<usize>()
                .saturating_add(size(&record))
                > max_bytes
        {
            return false;
        }
        queue.push_back(record);
        true
    }
    fn fail_buffer(
        &mut self,
        group: u64,
        source: &SourceKey,
        rejection: Option<ubm_core::ordered_notifications::SourceRejection>,
    ) -> Result<(), DesktopError> {
        let _ = source;
        self.failed.insert(group);
        self.overflows.insert(group, rejection);
        Ok(())
    }
    pub fn source(lease: &OrderedSourceLease) -> SourceKey {
        SourceKey {
            peer_id: lease.peer_id.clone(),
            generation: lease.source_generation.clone(),
            native_group: lease.native_group,
        }
    }
    pub fn next_id(&mut self) -> Option<u64> {
        self.next_identity = self.next_identity.checked_add(1)?;
        Some(self.next_identity)
    }
}

use crate::boundary::{
    FakeRadio, OrderedSourceFailure, OrderedSourceIngress, OrderedSourceRequest,
    OrderedSourceStatus, OrderedSourceTransition, RadioBoundary,
};
use crate::errors::DesktopError;
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
use ubm_core::ordered_notifications::{Member, OrderedCollection, SourceGeneration};
fn error(detail: impl std::fmt::Debug) -> DesktopError {
    DesktopError::new(
        BleErrorCode::LifecycleInvalidState,
        BleErrorDomain::Gatt,
        "fake.ordered",
    )
    .with_detail(format!("{detail:?}"))
}
impl FakeRadio {
    #[expect(
        clippy::result_large_err,
        reason = "The deterministic test-radio mirrors the production OrderedSourceIngress error contract without changing its shared failure layout."
    )]
    pub(crate) fn fake_prepare_ordered(
        &self,
        request: OrderedSourceRequest,
    ) -> Result<OrderedSourceLease, OrderedSourceFailure> {
        let fail = |detail| OrderedSourceFailure {
            error: error(detail),
            cleanup: None,
        };
        let current_snapshot =
            self.gatt_snapshot_identity(&request.peer_id)
                .map_err(|e| OrderedSourceFailure {
                    error: e,
                    cleanup: None,
                })?;
        if current_snapshot != request.snapshot_identity {
            return Err(fail("GATT snapshot identity changed"));
        }
        let mut state = self.ordered.lock().expect("fake ordered source");
        if state.leases.contains_key(&request.collection_id) {
            return Err(fail("duplicate collection"));
        }
        for member in &request.members {
            if state.prepared.get(&(
                member.activation.scope.clone(),
                member.activation.activation_epoch,
            )) != Some(&member.activation)
                || member.activation.scope.0 != request.peer_id
            {
                return Err(fail("invalid activation"));
            }
        }
        let group = state.next_id().ok_or_else(|| fail("identity exhausted"))?;
        let mut members = Vec::new();
        let mut claims = Vec::new();
        for member in &request.members {
            let id = state.next_id().ok_or_else(|| fail("identity exhausted"))?;
            let membership = Membership {
                member: id,
                epoch: 1,
            };
            members.push(Member {
                id: member.member_id,
                source: membership,
                delivery: member.delivery,
            });
            claims.push((member.member_id, membership, member.activation.clone()));
        }
        let generation = SourceGeneration::Counter(
            *state
                .generations
                .entry(request.peer_id.clone())
                .or_insert(1),
        );
        let lease = OrderedSourceLease {
            peer_id: request.peer_id,
            collection_id: request.collection_id,
            connection_generation: request.connection_generation.clone(),
            source_generation: generation.clone(),
            native_group: group,
            members: claims.iter().map(|(id, m, _)| (*id, *m)).collect(),
            ingress: OrderedSourceIngress::BackendOwned,
        };
        let collection = OrderedCollection::new(
            request.connection_generation,
            request.database_generation,
            generation,
            request.collection_id,
            members,
            request.max_items,
            request.max_bytes,
            request.max_controls,
        )
        .map_err(|e| OrderedSourceFailure {
            error: error(e),
            cleanup: None,
        })?;
        state
            .registry
            .install(
                request.collection_id,
                FakeOrderedSource::source(&lease),
                collection,
            )
            .map_err(|e| OrderedSourceFailure {
                error: error(e),
                cleanup: None,
            })?;
        state.bounds.insert(
            request.collection_id,
            (
                request.max_items.saturating_add(request.max_controls),
                request.max_bytes,
            ),
        );
        state.claims.insert(request.collection_id, claims);
        state.leases.insert(request.collection_id, lease.clone());
        Ok(lease)
    }
    pub(crate) fn fake_claim(
        &self,
        lease: &OrderedSourceLease,
        id: u64,
        membership: Membership,
        activation: &PreparedNotificationActivation,
    ) -> Result<(), DesktopError> {
        let state = self.ordered.lock().expect("fake ordered source");
        let valid = state.leases.get(&lease.collection_id).is_some_and(|l| {
            l.peer_id == lease.peer_id
                && l.connection_generation == lease.connection_generation
                && l.source_generation == lease.source_generation
                && l.native_group == lease.native_group
        }) && state
            .claims
            .get(&lease.collection_id)
            .is_some_and(|claims| {
                claims
                    .iter()
                    .any(|(i, m, a)| *i == id && *m == membership && a == activation)
            });
        if valid {
            Ok(())
        } else {
            Err(error("stale claim"))
        }
    }
    pub(crate) fn fake_update(
        &self,
        lease: &OrderedSourceLease,
        transition: OrderedSourceTransition,
    ) -> Result<(), DesktopError> {
        let mut state = self.ordered.lock().expect("fake ordered source");
        if !state
            .registry
            .owns_source(lease.collection_id, &FakeOrderedSource::source(lease))
        {
            return Err(error("stale lease"));
        }
        state
            .registry
            .with_collection(lease.collection_id, |owner| match transition {
                OrderedSourceTransition::BeginEnable { member_id } => owner.begin_enable(member_id),
                OrderedSourceTransition::EnableResult { member_id, outcome } => {
                    owner.enable_result(member_id, outcome)
                }
                OrderedSourceTransition::FailEnable {
                    member_id,
                    outcome,
                    failure,
                    tail_uncertain,
                } => owner.fail_enable(member_id, outcome, failure, tail_uncertain),
                OrderedSourceTransition::BeginRelease { member_id } => {
                    owner.begin_release(member_id)
                }
                OrderedSourceTransition::ReleaseResult { member_id, result } => {
                    owner.release_result(member_id, result)
                }
                OrderedSourceTransition::Close {
                    reason,
                    tail_uncertain,
                } => owner.close(reason, tail_uncertain),
            })
            .map_err(error)
    }
    pub(crate) fn fake_status(
        &self,
        lease: &OrderedSourceLease,
    ) -> Result<OrderedSourceStatus, DesktopError> {
        let mut state = self.ordered.lock().expect("fake ordered source");
        if !state
            .registry
            .owns_source(lease.collection_id, &FakeOrderedSource::source(lease))
        {
            return Err(error("stale lease"));
        }
        state
            .registry
            .with_collection_silent(lease.collection_id, |owner| {
                Ok(OrderedSourceStatus {
                    phase: owner.phase(),
                    members: owner.members().to_vec(),
                    cleanup_complete: owner.cleanup_complete(),
                })
            })
            .map_err(error)
    }
}

impl FakeRadio {
    pub(crate) fn fake_admit_event(
        &self,
        event: &crate::boundary::RadioEvent,
    ) -> Result<(), DesktopError> {
        use crate::boundary::RadioEvent;
        use ubm_core::central::NotificationProvenance;
        if let RadioEvent::Connected(peer) = event {
            let mut state = self.ordered.lock().expect("fake ordered source");
            if let Some(generation) = state.generations.get_mut(peer) {
                *generation = generation
                    .checked_add(1)
                    .ok_or_else(|| error("source generation exhausted"))?;
                state.ordinals.remove(peer);
            } else {
                state.generations.insert(peer.clone(), 1);
            }
            return Ok(());
        }
        let (
            peer,
            service,
            service_occurrence,
            characteristic,
            characteristic_occurrence,
            epoch,
            value,
            provenance,
        ) = match event {
            RadioEvent::Notification {
                peer_id,
                service_uuid,
                service_occurrence,
                characteristic_uuid,
                characteristic_occurrence,
                epoch,
                value,
            } => (
                peer_id,
                service_uuid,
                *service_occurrence,
                characteristic_uuid,
                *characteristic_occurrence,
                *epoch,
                value,
                NotificationProvenance::Unknown,
            ),
            RadioEvent::NotificationWithProvenance {
                peer_id,
                service_uuid,
                service_occurrence,
                characteristic_uuid,
                characteristic_occurrence,
                epoch,
                value,
                provenance,
                ..
            } => (
                peer_id,
                service_uuid,
                *service_occurrence,
                characteristic_uuid,
                *characteristic_occurrence,
                *epoch,
                value,
                *provenance,
            ),
            RadioEvent::NotificationWithReceipt {
                peer_id,
                service_uuid,
                service_occurrence,
                characteristic_uuid,
                characteristic_occurrence,
                epoch,
                value,
                ..
            } => (
                peer_id,
                service_uuid,
                *service_occurrence,
                characteristic_uuid,
                *characteristic_occurrence,
                *epoch,
                value,
                NotificationProvenance::Unknown,
            ),
            _ => return Ok(()),
        };
        let scope = (
            peer.clone(),
            service.clone(),
            service_occurrence,
            characteristic.clone(),
            characteristic_occurrence,
        );
        let mut state = self.ordered.lock().expect("fake ordered source");
        let next = state.ordinals.entry(peer.clone()).or_insert(0);
        let ordinal = *next;
        *next = next
            .checked_add(1)
            .ok_or_else(|| error("source ordinal exhausted"))?;
        let generation = *state.generations.entry(peer.clone()).or_insert(1);
        let mut targets = Vec::new();
        for lease in state.leases.values() {
            let group = lease.collection_id;
            if &lease.peer_id != peer
                || lease.source_generation != SourceGeneration::Counter(generation)
                || state.failed.contains(&group)
            {
                continue;
            }
            if let Some(claims) = state.claims.get(&group) {
                for (id, m, a) in claims {
                    if a.scope != scope || a.activation_epoch != epoch {
                        continue;
                    }
                    let membership = state
                        .renewals
                        .get(&(group, *id))
                        .filter(|r| !r.resumed)
                        .map_or(Some(*m), |r| r.new);
                    if let Some(m) = membership {
                        targets.push((group, FakeOrderedSource::source(lease), m));
                    }
                }
            }
        }
        let mut active_targets = Vec::with_capacity(targets.len());
        for (group, source, membership) in targets {
            let phase = state
                .registry
                .with_collection_silent(group, |owner| Ok(owner.phase()))
                .map_err(error)?;
            if matches!(
                phase,
                ubm_core::ordered_notifications::Phase::Staging
                    | ubm_core::ordered_notifications::Phase::Active
            ) {
                active_targets.push((group, source, membership));
            }
        }
        let targets = active_targets;
        // Reserve space in every synthetic recipient before accepting any of
        // them, so a full queue can be reported without partial acceptance.
        for (group, source, _) in &targets {
            if state.buffered.contains_key(group) {
                continue;
            }
            let limit = state.bounds.get(group).map_or(0, |bound| bound.0);
            let queue = state.pending_records.get(source);
            let count = queue.map_or(0, VecDeque::len);
            let bytes = queue.map_or(0, |queue| {
                queue.iter().map(|record| record.bytes).sum::<usize>()
            });
            if count >= limit
                || value.len()
                    > (ubm_core::contracts::MAX_STREAM_BYTE_CAPACITY as usize).saturating_sub(bytes)
            {
                return Err(error("synthetic ordered source capacity exhausted"));
            }
        }
        for (group, source, m) in targets {
            if state.buffered.contains_key(&group) {
                if !state.buffer_record(
                    group,
                    FakeRecord::Value(m, ordinal, value.clone(), provenance),
                ) {
                    state.fail_buffer(
                        group,
                        &source,
                        Some(ubm_core::ordered_notifications::SourceRejection::Value {
                            bytes: u64::try_from(value.len()).unwrap_or(u64::MAX),
                        }),
                    )?;
                }
            } else {
                state.retain_record(
                    group,
                    &source,
                    ubm_core::ordered_notifications::RetainedRecord::Value {
                        membership: m,
                        ordinal,
                        value: value.clone(),
                        delivery: ubm_core::ordered_notifications::ObservedDelivery::Unknown,
                        provenance,
                    },
                    value.len(),
                )?;
            }
        }
        Ok(())
    }
    pub(crate) fn fake_source_loss(
        &self,
        peer: &str,
        count: Option<u64>,
    ) -> Result<(), DesktopError> {
        let mut state = self.ordered.lock().expect("fake ordered source");
        let generation = *state.generations.entry(peer.to_owned()).or_insert(1);
        let targets: Vec<_> = state
            .leases
            .values()
            .filter(|l| {
                l.peer_id == peer && l.source_generation == SourceGeneration::Counter(generation)
            })
            .map(|l| {
                (
                    FakeOrderedSource::source(l),
                    l.members
                        .iter()
                        .filter_map(|(id, m)| {
                            state
                                .renewals
                                .get(&(l.collection_id, *id))
                                .filter(|r| !r.resumed)
                                .map_or(Some(*m), |r| r.new)
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        let mut active_targets = Vec::with_capacity(targets.len());
        for (source, members) in targets {
            let group = state
                .leases
                .values()
                .find(|l| FakeOrderedSource::source(l) == source)
                .map(|l| l.collection_id)
                .ok_or_else(|| error("source missing"))?;
            let phase = state
                .registry
                .with_collection_silent(group, |owner| Ok(owner.phase()))
                .map_err(error)?;
            if !members.is_empty()
                && matches!(
                    phase,
                    ubm_core::ordered_notifications::Phase::Staging
                        | ubm_core::ordered_notifications::Phase::Active
                )
            {
                active_targets.push((source, members));
            }
        }
        let targets = active_targets;
        for (source, _) in &targets {
            let group = state
                .leases
                .values()
                .find(|l| FakeOrderedSource::source(l) == *source)
                .map(|l| l.collection_id)
                .ok_or_else(|| error("source missing"))?;
            if state.failed.contains(&group) || state.buffered.contains_key(&group) {
                continue;
            }
            let limit = state.bounds.get(&group).map_or(0, |bound| bound.0);
            if state.pending_records.get(source).map_or(0, VecDeque::len) >= limit {
                return Err(error("synthetic ordered source capacity exhausted"));
            }
        }
        for (source, members) in targets {
            if members.is_empty() {
                continue;
            }
            let group = state
                .leases
                .values()
                .find(|l| FakeOrderedSource::source(l) == source)
                .map(|l| l.collection_id)
                .ok_or_else(|| error("source missing"))?;
            if state.failed.contains(&group) {
                continue;
            }
            if state.buffered.contains_key(&group) {
                if !state.buffer_record(group, FakeRecord::Loss(members.clone(), count)) {
                    state.fail_buffer(
                        group,
                        &source,
                        count.map(|skipped_records| {
                            ubm_core::ordered_notifications::SourceRejection::Loss {
                                skipped_records,
                            }
                        }),
                    )?;
                }
            } else {
                state.retain_record(
                    group,
                    &source,
                    ubm_core::ordered_notifications::RetainedRecord::Loss {
                        scope: ubm_core::ordered_notifications::LossScope::Unknown,
                        unit: ubm_core::ordered_notifications::LossUnit::SourceRecords,
                        count,
                        bytes: None,
                    },
                    0,
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::{OrderedSourceMember, RadioBoundary, RadioEvent};
    use ubm_core::ordered_notifications::{DeliveryMode, Poll, Record};

    #[tokio::test]
    async fn stale_snapshot_does_not_reserve_source() {
        use crate::boundary::{GattSnapshotIdentity, OrderedSourceRecovery};
        let radio = FakeRadio::new();
        radio.set_gatt_snapshot_identity(
            "peer",
            GattSnapshotIdentity {
                owner: "daemon".into(),
                attachment: 2,
                revision: 3,
            },
        );
        let result = radio
            .prepare_ordered_source(OrderedSourceRequest {
                peer_id: "peer".into(),
                snapshot_identity: Some(GattSnapshotIdentity {
                    owner: "daemon".into(),
                    attachment: 2,
                    revision: 2,
                }),
                connection_generation: "conn".into(),
                database_generation: "db".into(),
                collection_id: 8,
                members: vec![],
                max_items: 2,
                max_bytes: 10,
                max_controls: 2,
            })
            .await;
        assert!(result.is_err());
        assert!(matches!(
            radio.recover_ordered_source("peer", 8).await.unwrap(),
            OrderedSourceRecovery::NotStarted
        ));
    }

    #[tokio::test]
    async fn direct_push_reports_full_synthetic_source_without_poisoning_it() {
        let radio = FakeRadio::new();
        let scope = ("peer".into(), "svc".into(), 0, "a".into(), 0);
        let activation = radio
            .prepare_notification_activation(&scope, 1, None, None)
            .await
            .unwrap();
        let lease = radio
            .prepare_ordered_source(OrderedSourceRequest {
                peer_id: "peer".into(),
                snapshot_identity: None,
                connection_generation: "conn".into(),
                database_generation: "db".into(),
                collection_id: 7,
                members: vec![OrderedSourceMember {
                    member_id: 1,
                    activation,
                    delivery: DeliveryMode::Notification,
                }],
                max_items: 1,
                max_bytes: 1,
                max_controls: 1,
            })
            .await
            .unwrap();
        radio
            .update_ordered_source(
                &lease,
                OrderedSourceTransition::BeginEnable { member_id: 1 },
            )
            .unwrap();
        radio
            .update_ordered_source(
                &lease,
                OrderedSourceTransition::EnableResult {
                    member_id: 1,
                    outcome: Some(true),
                },
            )
            .unwrap();
        let event = || RadioEvent::Notification {
            peer_id: scope.0.clone(),
            service_uuid: scope.1.clone(),
            service_occurrence: 0,
            characteristic_uuid: scope.3.clone(),
            characteristic_occurrence: 0,
            epoch: 1,
            value: vec![1, 2],
        };
        radio.push_event(event()).unwrap();
        radio.push_event(event()).unwrap();
        assert!(radio.push_event(event()).is_err());
        radio.increase_ordered_byte_capacity(&lease, 8).unwrap();
        assert!(matches!(
            radio.poll_ordered_source(&lease).await.unwrap(),
            Poll::Record(Record::Value { .. })
        ));
    }

    #[tokio::test]
    async fn pending_synthetic_value_is_not_discarded_by_close() {
        let radio = FakeRadio::new();
        let scope = ("peer".into(), "svc".into(), 0, "a".into(), 0);
        let activation = radio
            .prepare_notification_activation(&scope, 1, None, None)
            .await
            .unwrap();
        let lease = radio
            .prepare_ordered_source(OrderedSourceRequest {
                peer_id: "peer".into(),
                snapshot_identity: None,
                connection_generation: "conn".into(),
                database_generation: "db".into(),
                collection_id: 7,
                members: vec![OrderedSourceMember {
                    member_id: 1,
                    activation,
                    delivery: DeliveryMode::Notification,
                }],
                max_items: 1,
                max_bytes: 1,
                max_controls: 1,
            })
            .await
            .unwrap();
        radio
            .update_ordered_source(
                &lease,
                OrderedSourceTransition::BeginEnable { member_id: 1 },
            )
            .unwrap();
        radio
            .update_ordered_source(
                &lease,
                OrderedSourceTransition::EnableResult {
                    member_id: 1,
                    outcome: Some(true),
                },
            )
            .unwrap();
        radio
            .try_push_event(RadioEvent::Notification {
                peer_id: scope.0,
                service_uuid: scope.1,
                service_occurrence: 0,
                characteristic_uuid: scope.3,
                characteristic_occurrence: 0,
                epoch: 1,
                value: vec![1, 2],
            })
            .unwrap();
        let close = || OrderedSourceTransition::Close {
            reason: ubm_core::ordered_notifications::EndReason::Closed,
            tail_uncertain: false,
        };
        radio.update_ordered_source(&lease, close()).unwrap();
        radio.increase_ordered_byte_capacity(&lease, 8).unwrap();
        assert!(
            matches!(radio.poll_ordered_source(&lease).await.unwrap(), Poll::Record(Record::Value { value, .. }) if value == [1, 2])
        );
        assert!(matches!(
            radio.poll_ordered_source(&lease).await.unwrap(),
            Poll::Terminal(_)
        ));
    }

    #[tokio::test]
    async fn paused_renewal_overflow_preserves_accepted_value_then_reports_rejection() {
        let radio = FakeRadio::new();
        let scope = ("peer".into(), "svc".into(), 0, "a".into(), 0);
        let a = radio
            .prepare_notification_activation(&scope, 1, None, None)
            .await
            .unwrap();
        let mut lease = radio
            .prepare_ordered_source(OrderedSourceRequest {
                peer_id: "peer".into(),
                snapshot_identity: None,
                connection_generation: "conn".into(),
                database_generation: "db".into(),
                collection_id: 7,
                members: vec![OrderedSourceMember {
                    member_id: 1,
                    activation: a,
                    delivery: DeliveryMode::Notification,
                }],
                max_items: 1,
                max_bytes: 1,
                max_controls: 1,
            })
            .await
            .unwrap();
        radio
            .update_ordered_source(
                &lease,
                OrderedSourceTransition::BeginEnable { member_id: 1 },
            )
            .unwrap();
        radio
            .update_ordered_source(
                &lease,
                OrderedSourceTransition::EnableResult {
                    member_id: 1,
                    outcome: Some(true),
                },
            )
            .unwrap();
        let old = lease.members[0].1;
        radio.retire_ordered_member(&lease, 1, old).await.unwrap();
        let new = radio
            .reregister_ordered_member(&lease, 1, old)
            .await
            .unwrap();
        for byte in [1, 2] {
            radio
                .push_event(RadioEvent::Notification {
                    peer_id: scope.0.clone(),
                    service_uuid: scope.1.clone(),
                    service_occurrence: scope.2,
                    characteristic_uuid: scope.3.clone(),
                    characteristic_occurrence: scope.4,
                    epoch: 1,
                    value: vec![byte],
                })
                .expect("synthetic event admission failed");
        }
        radio.rebind_ordered_member(&lease, 1, old, new).unwrap();
        lease.members[0].1 = new;
        radio.resume_ordered_member(&lease, 1, new).await.unwrap();
        assert!(
            matches!(radio.poll_ordered_source(&lease).await.unwrap(),Poll::Record(Record::Value{value,..}) if value==[1])
        );
        assert!(
            matches!(radio.poll_ordered_source(&lease).await.unwrap(),Poll::Terminal(t) if t.tail_uncertain && t.source_rejection == Some(ubm_core::ordered_notifications::SourceRejection::Value{bytes:1}))
        );
    }

    #[tokio::test]
    async fn renewal_fences_old_and_stages_callbacks_until_resume() {
        let radio = FakeRadio::new();
        let scope = ("peer".into(), "svc".into(), 0, "a".into(), 0);
        let activation = radio
            .prepare_notification_activation(&scope, 1, None, None)
            .await
            .unwrap();
        let mut lease = radio
            .prepare_ordered_source(OrderedSourceRequest {
                peer_id: "peer".into(),
                snapshot_identity: None,
                connection_generation: "conn".into(),
                database_generation: "db".into(),
                collection_id: 7,
                members: vec![OrderedSourceMember {
                    member_id: 1,
                    activation: activation.clone(),
                    delivery: DeliveryMode::Notification,
                }],
                max_items: 8,
                max_bytes: 100,
                max_controls: 8,
            })
            .await
            .unwrap();
        radio
            .update_ordered_source(
                &lease,
                OrderedSourceTransition::BeginEnable { member_id: 1 },
            )
            .unwrap();
        radio
            .update_ordered_source(
                &lease,
                OrderedSourceTransition::EnableResult {
                    member_id: 1,
                    outcome: Some(true),
                },
            )
            .unwrap();
        let event = |byte| RadioEvent::Notification {
            peer_id: scope.0.clone(),
            service_uuid: scope.1.clone(),
            service_occurrence: scope.2,
            characteristic_uuid: scope.3.clone(),
            characteristic_occurrence: scope.4,
            epoch: 1,
            value: vec![byte],
        };
        radio
            .push_event(event(1))
            .expect("synthetic event admission failed");
        let old = lease.members[0].1;
        radio.retire_ordered_member(&lease, 1, old).await.unwrap();
        let new = radio
            .reregister_ordered_member(&lease, 1, old)
            .await
            .unwrap();
        assert_eq!(
            radio
                .reregister_ordered_member(&lease, 1, old)
                .await
                .unwrap(),
            new
        );
        radio
            .push_event(event(2))
            .expect("synthetic event admission failed");
        assert!(
            matches!(radio.poll_ordered_source(&lease).await.unwrap(),Poll::Record(Record::Value{ordinal:0,value,..}) if value==[1])
        );
        assert!(matches!(
            radio.poll_ordered_source(&lease).await.unwrap(),
            Poll::Empty
        ));
        assert!(matches!(
            radio
                .abandon_ordered_renewal(&lease, 1, old, None)
                .await
                .unwrap(),
            crate::boundary::OrderedRenewalAbandon::Pending
        ));
        assert_eq!(lease.members[0].1, old);
        radio.rebind_ordered_member(&lease, 1, old, new).unwrap();
        lease.members[0].1 = new;
        assert!(matches!(
            radio
                .abandon_ordered_renewal(&lease, 1, old, Some(new))
                .await
                .unwrap(),
            crate::boundary::OrderedRenewalAbandon::Retired {
                tail_uncertain: true
            }
        ));
        assert!(
            matches!(radio.poll_ordered_source(&lease).await.unwrap(),Poll::Record(Record::Value{ordinal:1,value,..}) if value==[2])
        );
        assert!(radio.retire_ordered_member(&lease, 1, old).await.is_err());
    }

    #[tokio::test]
    async fn interleaved_values_and_loss_follow_one_source_order() {
        let radio = FakeRadio::new();
        let a = ("peer".into(), "svc".into(), 0, "a".into(), 0);
        let b = ("peer".into(), "svc".into(), 0, "b".into(), 0);
        let a = radio
            .prepare_notification_activation(&a, 1, None, None)
            .await
            .unwrap();
        let b = radio
            .prepare_notification_activation(&b, 2, None, None)
            .await
            .unwrap();
        let lease = radio
            .prepare_ordered_source(OrderedSourceRequest {
                peer_id: "peer".into(),
                snapshot_identity: None,
                connection_generation: "conn".into(),
                database_generation: "db".into(),
                collection_id: 9,
                members: vec![
                    OrderedSourceMember {
                        member_id: 1,
                        activation: a.clone(),
                        delivery: DeliveryMode::Notification,
                    },
                    OrderedSourceMember {
                        member_id: 2,
                        activation: b.clone(),
                        delivery: DeliveryMode::Notification,
                    },
                ],
                max_items: 8,
                max_bytes: 100,
                max_controls: 8,
            })
            .await
            .unwrap();
        for id in [1, 2] {
            radio
                .update_ordered_source(
                    &lease,
                    OrderedSourceTransition::BeginEnable { member_id: id },
                )
                .unwrap();
            radio
                .update_ordered_source(
                    &lease,
                    OrderedSourceTransition::EnableResult {
                        member_id: id,
                        outcome: Some(true),
                    },
                )
                .unwrap();
        }
        let membership = lease.members.iter().find(|(id, _)| *id == 1).unwrap().1;
        radio
            .reserve_ordered_notification(&lease, 1, membership, &a)
            .unwrap();
        radio.block_op(crate::boundary::FaultOp::Subscribe);
        let enable = radio.dispatch_ordered_notification(&lease, 1, membership, &a);
        tokio::pin!(enable);
        tokio::select! { _ = &mut enable => panic!("enable was not held"), _ = tokio::task::yield_now() => {} }
        assert_eq!(
            radio
                .ordered_notification_settlement(&lease, 1, membership, &a)
                .await
                .unwrap(),
            crate::boundary::OrderedNotificationSettlement::Pending
        );
        radio.unblock_op(crate::boundary::FaultOp::Subscribe);
        enable.await.unwrap();
        assert_eq!(
            radio
                .ordered_notification_settlement(&lease, 1, membership, &a)
                .await
                .unwrap(),
            crate::boundary::OrderedNotificationSettlement::Enabled
        );
        for (handle, byte) in [(&a, 1), (&b, 2), (&a, 3)] {
            radio
                .push_event(RadioEvent::Notification {
                    peer_id: handle.scope.0.clone(),
                    service_uuid: handle.scope.1.clone(),
                    service_occurrence: handle.scope.2,
                    characteristic_uuid: handle.scope.3.clone(),
                    characteristic_occurrence: handle.scope.4,
                    epoch: handle.activation_epoch,
                    value: vec![byte],
                })
                .expect("synthetic event admission failed");
        }
        radio.stage_ordered_source_loss("peer", Some(4)).unwrap();
        assert_eq!(
            radio.seal_ordered_source(&lease).await.unwrap(),
            crate::boundary::OrderedSourceSeal::Pending
        );
        radio
            .retire_ordered_source(
                &lease,
                ubm_core::ordered_notifications::EndReason::Invalidated,
            )
            .await
            .unwrap();
        assert_eq!(
            radio.seal_ordered_source(&lease).await.unwrap(),
            crate::boundary::OrderedSourceSeal::Sealed
        );
        for (ordinal, byte) in [(0, 1), (1, 2), (2, 3)] {
            assert!(
                matches!(radio.poll_ordered_source(&lease).await.unwrap(),Poll::Record(Record::Value{ordinal:o,value,..}) if o==ordinal && value==[byte])
            );
        }
        assert!(matches!(
            radio.poll_ordered_source(&lease).await.unwrap(),
            Poll::Record(Record::Loss { .. })
        ));
        assert!(
            matches!(radio.poll_ordered_source(&lease).await.unwrap(), Poll::Terminal(terminal) if terminal.reason == ubm_core::ordered_notifications::EndReason::Invalidated && terminal.tail_uncertain && terminal.rejected_values.is_none() && terminal.source_rejection.is_none())
        );
        for id in [1, 2] {
            radio
                .update_ordered_source(
                    &lease,
                    OrderedSourceTransition::BeginRelease { member_id: id },
                )
                .unwrap();
            radio
                .update_ordered_source(
                    &lease,
                    OrderedSourceTransition::ReleaseResult {
                        member_id: id,
                        result: Ok(()),
                    },
                )
                .unwrap();
        }
        assert!(matches!(
            radio
                .release_ordered_source(&crate::boundary::OrderedSourceCleanup::Lease(
                    lease.clone()
                ))
                .await
                .unwrap(),
            crate::boundary::OrderedSourceRelease::Pending
        ));
        assert!(
            radio
                .ordered_notification_settlement(&lease, 1, membership, &a)
                .await
                .is_ok()
        );
        radio.release_prepared_notification(&a).await.unwrap();
        assert_eq!(
            radio
                .ordered_notification_settlement(&lease, 1, membership, &a)
                .await
                .unwrap(),
            crate::boundary::OrderedNotificationSettlement::Released
        );
        radio
            .release_ordered_source(&crate::boundary::OrderedSourceCleanup::Lease(lease.clone()))
            .await
            .unwrap();
    }
}
