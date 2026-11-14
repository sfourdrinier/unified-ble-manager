// crates/ubm-desktop/src/central/empty_ordered_source.rs
//! Bounded evidence for a failed native open that created no member routes.

use super::{DesktopCentral, OrderedIngressReceipt, OrderedIngressRecord, lock_std};
use crate::boundary::RadioBoundary;
use crate::errors::DesktopError;
use crate::ordered_notification_groups::SourceKey;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use ubm_core::contracts::{
    BleErrorCode, BleErrorDomain, MAX_STREAM_BYTE_CAPACITY, MAX_STREAM_ITEM_CAPACITY,
};
#[cfg(test)]
use ubm_core::ordered_notifications::Failure;
use ubm_core::ordered_notifications::{EndReason, failure_allocation_cost};

const MAX_EMPTY_SOURCES: usize = 64;

/// A bounded failed-setup receipt in source order.
#[derive(Debug, Clone)]
#[expect(
    clippy::large_enum_variant,
    reason = "the inline record preserves bounded failed-open evidence without heap indirection on the empty path"
)]
pub enum EmptyOrderedPoll {
    Record(OrderedIngressRecord),
    Empty,
}

struct Entry {
    capacity: usize,
    max_bytes: usize,
    queued_bytes: usize,
    records: VecDeque<OrderedIngressRecord>,
    terminal: Option<OrderedIngressRecord>,
    late: VecDeque<OrderedIngressRecord>,
    terminal_seen: bool,
    wake: Option<std::sync::Arc<tokio::sync::Notify>>,
}

#[derive(Default)]
pub(super) struct EmptyOrderedSources {
    entries: Mutex<HashMap<(SourceKey, u64), Entry>>,
}

fn failure(detail: &'static str) -> DesktopError {
    DesktopError::new(
        BleErrorCode::ProtocolViolation,
        BleErrorDomain::Gatt,
        "gatt.ordered.failed-setup",
    )
    .with_detail(detail.to_owned())
}

fn record_size(record: &OrderedIngressRecord) -> usize {
    match record {
        OrderedIngressRecord::BridgeFailure { failure } => failure_allocation_cost(failure),
        OrderedIngressRecord::Terminal {
            reason:
                EndReason::SetupFailed(failure)
                | EndReason::SourceFailed(failure)
                | EndReason::HostReset(failure),
            ..
        } => failure_allocation_cost(failure),
        _ => 0,
    }
}

fn record_too_large(bytes: usize, max_bytes: usize) -> DesktopError {
    DesktopError::new(
        BleErrorCode::ArgumentInvalid,
        BleErrorDomain::Gatt,
        "gatt.ordered.ingress.record-too-large",
    )
    .with_detail(format!(
        "failed-setup evidence requires {bytes} bytes; configured capacity is {max_bytes}"
    ))
}

impl EmptyOrderedSources {
    fn increase_byte_capacity(
        &self,
        source: &SourceKey,
        collection_id: u64,
        max_bytes: usize,
    ) -> Result<usize, DesktopError> {
        let mut entries = lock_std(&self.entries);
        let entry = entries
            .get_mut(&(source.clone(), collection_id))
            .ok_or_else(|| {
                DesktopError::new(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.increase-capacity",
                )
            })?;
        if max_bytes < entry.max_bytes
            || u64::try_from(max_bytes).map_or(true, |value| value > MAX_STREAM_BYTE_CAPACITY)
        {
            return Err(DesktopError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                "gatt.ordered.increase-capacity",
            ));
        }
        let previous = entry.max_bytes;
        entry.max_bytes = max_bytes;
        if let Some(wake) = &entry.wake {
            wake.notify_one();
        }
        Ok(previous)
    }

    fn install(
        &self,
        source: SourceKey,
        collection_id: u64,
        capacity: usize,
        max_bytes: usize,
    ) -> Result<(), DesktopError> {
        if source.peer_id.is_empty()
            || capacity == 0
            || max_bytes == 0
            || u64::try_from(max_bytes).map_or(true, |v| v > MAX_STREAM_BYTE_CAPACITY)
            || u64::try_from(capacity).map_or(true, |v| v > MAX_STREAM_ITEM_CAPACITY)
        {
            return Err(failure("invalid failed-setup receipt capacity or source"));
        }
        let mut entries = lock_std(&self.entries);
        let key = (source, collection_id);
        if entries.contains_key(&key) {
            return Err(failure("failed-setup source already installed"));
        }
        if entries.len() >= MAX_EMPTY_SOURCES {
            return Err(failure("failed-setup source capacity exceeded"));
        }
        entries.insert(
            key,
            Entry {
                capacity,
                max_bytes,
                queued_bytes: 0,
                records: VecDeque::new(),
                terminal: None,
                late: VecDeque::new(),
                terminal_seen: false,
                wake: None,
            },
        );
        Ok(())
    }
    pub(super) fn set_wake(
        &self,
        source: &SourceKey,
        collection_id: u64,
        wake: std::sync::Arc<tokio::sync::Notify>,
    ) -> Result<(), DesktopError> {
        let mut entries = lock_std(&self.entries);
        let entry = entries
            .get_mut(&(source.clone(), collection_id))
            .ok_or_else(|| failure("failed-setup source is unavailable"))?;
        entry.wake = Some(wake.clone());
        wake.notify_one();
        Ok(())
    }
    fn admit(
        &self,
        source: &SourceKey,
        collection_id: u64,
        record: OrderedIngressRecord,
    ) -> Result<OrderedIngressReceipt, DesktopError> {
        let mut entries = lock_std(&self.entries);
        let entry = entries
            .get_mut(&(source.clone(), collection_id))
            .ok_or_else(|| failure("failed-setup source is unavailable"))?;
        match &record {
            OrderedIngressRecord::Value { .. }
            | OrderedIngressRecord::Fence { .. }
            | OrderedIngressRecord::Marker { .. } => {
                return Err(failure("member record is invalid for an empty source"));
            }
            _ => {}
        }
        let bytes = record_size(&record);
        if bytes > entry.max_bytes {
            return Err(record_too_large(bytes, entry.max_bytes));
        }
        if entry.terminal_seen {
            match &record {
                OrderedIngressRecord::BridgeFailure { .. }
                | OrderedIngressRecord::FailureObservationsOmitted => {}
                OrderedIngressRecord::Terminal {
                    reason: EndReason::SourceOverflow,
                    rejection,
                    failure_member,
                    rejected_loss,
                    uncertain_tail,
                } if rejection.is_some()
                    || failure_member.is_some()
                    || rejected_loss.is_some()
                    || *uncertain_tail => {}
                OrderedIngressRecord::Barrier {
                    uncertain_tail: true,
                    ..
                } => {}
                OrderedIngressRecord::Barrier {
                    uncertain_tail: false,
                    ..
                } => return Ok(OrderedIngressReceipt::Admitted),
                _ => return Err(failure("record follows source terminal")),
            }
            if entry.records.len() + entry.late.len() >= entry.capacity
                || bytes > entry.max_bytes.saturating_sub(entry.queued_bytes)
            {
                return Ok(OrderedIngressReceipt::PendingCapacity);
            }
            entry.queued_bytes += bytes;
            entry.late.push_back(record.clone());
        } else if matches!(
            record,
            OrderedIngressRecord::Terminal { .. } | OrderedIngressRecord::BridgeFailure { .. }
        ) {
            // Reserve the terminal independently of the bounded control lane.
            entry.terminal = Some(record.clone());
            entry.terminal_seen = true;
        } else {
            if matches!(
                record,
                OrderedIngressRecord::Barrier {
                    uncertain_tail: false,
                    ..
                }
            ) {
                return Ok(OrderedIngressReceipt::Admitted);
            }
            if entry.records.len() + entry.late.len() >= entry.capacity
                || bytes > entry.max_bytes.saturating_sub(entry.queued_bytes)
            {
                return Ok(OrderedIngressReceipt::PendingCapacity);
            }
            entry.queued_bytes += bytes;
            entry.records.push_back(record.clone());
        }
        if let Some(wake) = &entry.wake {
            wake.notify_one();
        }
        Ok(if entry.terminal_seen {
            OrderedIngressReceipt::Terminalized
        } else {
            OrderedIngressReceipt::Admitted
        })
    }
    fn fail(
        &self,
        source: &SourceKey,
        collection_id: u64,
        cause: ubm_core::ordered_notifications::Failure,
    ) -> Result<(), DesktopError> {
        match self.admit(
            source,
            collection_id,
            OrderedIngressRecord::BridgeFailure { failure: cause },
        )? {
            OrderedIngressReceipt::PendingCapacity => Err(failure(
                "failed-setup evidence capacity pending; retry the same failure",
            )),
            _ => Ok(()),
        }
    }
    // Poll transfers ownership to the caller. The ordered delivery layer
    // separately retains its one unacknowledged delivery for replay.
    fn poll(
        &self,
        source: &SourceKey,
        collection_id: u64,
    ) -> Result<EmptyOrderedPoll, DesktopError> {
        let mut entries = lock_std(&self.entries);
        let entry = entries
            .get_mut(&(source.clone(), collection_id))
            .ok_or_else(|| failure("failed-setup source is unavailable"))?;
        if let Some(record) = entry.records.pop_front() {
            entry.queued_bytes -= record_size(&record);
            return Ok(EmptyOrderedPoll::Record(record));
        }
        if let Some(terminal) = entry.terminal.take() {
            return Ok(EmptyOrderedPoll::Record(terminal));
        }
        if let Some(record) = entry.late.pop_front() {
            entry.queued_bytes -= record_size(&record);
            return Ok(EmptyOrderedPoll::Record(record));
        }
        Ok(EmptyOrderedPoll::Empty)
    }
    fn has_pending(&self, source: &SourceKey, collection_id: u64) -> Result<bool, DesktopError> {
        let entries = lock_std(&self.entries);
        let entry = entries
            .get(&(source.clone(), collection_id))
            .ok_or_else(|| failure("failed-setup source is unavailable"))?;
        Ok(!entry.records.is_empty() || entry.terminal.is_some() || !entry.late.is_empty())
    }
    fn remove(&self, source: &SourceKey, collection_id: u64) -> Result<(), DesktopError> {
        let mut entries = lock_std(&self.entries);
        let key = (source.clone(), collection_id);
        let entry = entries
            .get(&key)
            .ok_or_else(|| failure("failed-setup source is unavailable"))?;
        if !entry.records.is_empty() || entry.terminal.is_some() || !entry.late.is_empty() {
            return Err(failure("failed-setup evidence remains unread"));
        }
        entries.remove(&key);
        Ok(())
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Increase an exact failed-setup source's existing byte budget.
    pub fn increase_empty_ordered_byte_capacity(
        &self,
        source: &SourceKey,
        collection_id: u64,
        max_bytes: usize,
    ) -> Result<usize, DesktopError> {
        self.inner
            .empty_ordered_ingress
            .increase_byte_capacity(source, collection_id, max_bytes)
    }
    /// Install before binding a zero-member source wake; no physical route is created.
    pub fn install_empty_ordered_ingress(
        &self,
        source: SourceKey,
        collection_id: u64,
        capacity: usize,
    ) -> Result<(), DesktopError> {
        self.install_empty_ordered_ingress_with_byte_limit(
            source,
            collection_id,
            capacity,
            MAX_STREAM_BYTE_CAPACITY as usize,
        )
    }
    /// Install with the configured variable-allocation accounting budget. Fixed
    /// record storage is bounded by capacity; this is not an exact RSS limit.
    /// The independently reserved terminal holds one additional budget-sized payload.
    pub fn install_empty_ordered_ingress_with_byte_limit(
        &self,
        source: SourceKey,
        collection_id: u64,
        capacity: usize,
        max_bytes: usize,
    ) -> Result<(), DesktopError> {
        self.inner
            .empty_ordered_ingress
            .install(source, collection_id, capacity, max_bytes)
    }
    pub fn admit_empty_ordered_record(
        &self,
        source: &SourceKey,
        collection_id: u64,
        record: OrderedIngressRecord,
    ) -> Result<OrderedIngressReceipt, DesktopError> {
        match self.try_admit_empty_ordered_record(source, collection_id, record)? {
            OrderedIngressReceipt::PendingCapacity => Err(failure(
                "failed-setup evidence capacity pending; retry the same record",
            )),
            receipt => Ok(receipt),
        }
    }
    /// Admit without consuming a full lane; retry the identical record after polling.
    pub fn try_admit_empty_ordered_record(
        &self,
        source: &SourceKey,
        collection_id: u64,
        record: OrderedIngressRecord,
    ) -> Result<OrderedIngressReceipt, DesktopError> {
        self.inner
            .empty_ordered_ingress
            .admit(source, collection_id, record)
    }

    pub fn fail_empty_ordered_ingress(
        &self,
        source: &SourceKey,
        collection_id: u64,
        cause: ubm_core::ordered_notifications::Failure,
    ) -> Result<(), DesktopError> {
        self.inner
            .empty_ordered_ingress
            .fail(source, collection_id, cause)
    }
    pub fn poll_empty_ordered_ingress(
        &self,
        source: &SourceKey,
        collection_id: u64,
    ) -> Result<EmptyOrderedPoll, DesktopError> {
        let exclusions = lock_std(&self.inner.ordered_recording_exclusions);
        if exclusions.contains(&(source.peer_id.clone(), collection_id)) {
            return Err(failure("ordered recording owns this collection"));
        }
        self.poll_empty_ordered_ingress_internal(source, collection_id)
    }
    pub(super) fn poll_empty_ordered_ingress_internal(
        &self,
        source: &SourceKey,
        collection_id: u64,
    ) -> Result<EmptyOrderedPoll, DesktopError> {
        self.inner.empty_ordered_ingress.poll(source, collection_id)
    }
    pub fn empty_ordered_ingress_has_pending(
        &self,
        source: &SourceKey,
        collection_id: u64,
    ) -> Result<bool, DesktopError> {
        self.inner
            .empty_ordered_ingress
            .has_pending(source, collection_id)
    }
    pub fn remove_empty_ordered_ingress(
        &self,
        source: &SourceKey,
        collection_id: u64,
    ) -> Result<(), DesktopError> {
        self.inner
            .empty_ordered_ingress
            .remove(source, collection_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ubm_core::ordered_notifications::{
        LossScope, LossUnit, RejectedLoss, SourceGeneration, SourceRejection,
    };
    fn source() -> SourceKey {
        SourceKey {
            peer_id: "peer".into(),
            generation: SourceGeneration::Counter(1),
            native_group: 4,
        }
    }
    fn loss() -> OrderedIngressRecord {
        OrderedIngressRecord::Loss {
            scope: LossScope::Unknown,
            unit: LossUnit::Values,
            count: None,
            bytes: None,
        }
    }
    #[test]
    fn capacity_one_retries_exact_late_observations_without_inventing_omission() {
        let entries = EmptyOrderedSources::default();
        let key = source();
        entries
            .install(key.clone(), 4, 1, MAX_STREAM_BYTE_CAPACITY as usize)
            .unwrap();
        entries
            .admit(
                &key,
                4,
                OrderedIngressRecord::Terminal {
                    reason: EndReason::Closed,
                    rejection: None,
                    failure_member: None,
                    rejected_loss: None,
                    uncertain_tail: false,
                },
            )
            .unwrap();
        let observation = |count| OrderedIngressRecord::Terminal {
            reason: EndReason::SourceOverflow,
            rejection: Some(SourceRejection::Marker { marker_id: count }),
            failure_member: None,
            rejected_loss: Some(RejectedLoss {
                scope: LossScope::Unknown,
                unit: LossUnit::Values,
                count: Some(count),
                bytes: None,
            }),
            uncertain_tail: true,
        };
        entries.admit(&key, 4, observation(5)).unwrap();
        assert_eq!(
            format!("{:?}", entries.admit(&key, 4, observation(6)).unwrap()),
            "PendingCapacity"
        );
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal {
                reason: EndReason::Closed,
                ..
            })
        ));
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal {
                rejection: Some(SourceRejection::Marker { marker_id: 5 }),
                rejected_loss: Some(RejectedLoss { count: Some(5), .. }),
                uncertain_tail: true,
                ..
            })
        ));
        entries.admit(&key, 4, observation(6)).unwrap();
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal {
                rejection: Some(SourceRejection::Marker { marker_id: 6 }),
                ..
            })
        ));
        entries.admit(&key, 4, observation(6)).unwrap();
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal {
                rejection: Some(SourceRejection::Marker { marker_id: 6 }),
                ..
            })
        ));
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Empty
        ));
    }
    #[test]
    fn rejects_member_records_and_other_source_identity() {
        let entries = EmptyOrderedSources::default();
        let key = source();
        entries
            .install(key.clone(), 4, 2, MAX_STREAM_BYTE_CAPACITY as usize)
            .unwrap();
        let value = OrderedIngressRecord::Value {
            member_id: 1,
            membership: ubm_core::ordered_notifications::Membership {
                member: 1,
                epoch: 1,
            },
            ordinal: 1,
            value: vec![1],
            delivery: ubm_core::ordered_notifications::ObservedDelivery::Unknown,
            provenance: ubm_core::central::NotificationProvenance::Unknown,
        };
        assert!(entries.admit(&key, 4, value).is_err());
        let mut other = key.clone();
        other.generation = SourceGeneration::Counter(2);
        assert!(entries.admit(&other, 4, loss()).is_err());
    }
    #[test]
    fn explicit_omission_and_controls_keep_source_order() {
        let entries = EmptyOrderedSources::default();
        let key = source();
        entries
            .install(key.clone(), 4, 2, MAX_STREAM_BYTE_CAPACITY as usize)
            .unwrap();
        entries.admit(&key, 4, loss()).unwrap();
        entries
            .admit(&key, 4, OrderedIngressRecord::FailureObservationsOmitted)
            .unwrap();
        assert_eq!(
            entries.admit(&key, 4, loss()).unwrap(),
            OrderedIngressReceipt::PendingCapacity
        );
        entries
            .admit(
                &key,
                4,
                OrderedIngressRecord::Terminal {
                    reason: EndReason::Closed,
                    rejection: None,
                    failure_member: None,
                    rejected_loss: None,
                    uncertain_tail: false,
                },
            )
            .unwrap();
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Record(OrderedIngressRecord::Loss { .. })
        ));
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Record(OrderedIngressRecord::FailureObservationsOmitted)
        ));
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal { .. })
        ));
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Empty
        ));
        entries.remove(&key, 4).unwrap();
    }
    #[test]
    fn repeated_failures_and_explicit_omission_remain_separate() {
        use ubm_core::ordered_notifications::Failure;
        let entries = EmptyOrderedSources::default();
        let key = source();
        entries
            .install(key.clone(), 4, 3, MAX_STREAM_BYTE_CAPACITY as usize)
            .unwrap();
        entries
            .admit(
                &key,
                4,
                OrderedIngressRecord::Terminal {
                    reason: EndReason::Closed,
                    rejection: None,
                    failure_member: None,
                    rejected_loss: None,
                    uncertain_tail: false,
                },
            )
            .unwrap();
        let cause = Failure {
            code: "failure".into(),
            domain: "gatt".into(),
            operation: "source".into(),
            detail: Some("same".into()),
            platform_domain: None,
            platform_code: None,
            platform_message: None,
            platform_metadata: Default::default(),
            platform_metadata_entries: None,
            platform_cleanup: Vec::new(),
            platform_cleanup_failures: Vec::new(),
        };
        entries.fail(&key, 4, cause.clone()).unwrap();
        entries.fail(&key, 4, cause.clone()).unwrap();
        entries
            .admit(&key, 4, OrderedIngressRecord::FailureObservationsOmitted)
            .unwrap();
        assert!(entries.fail(&key, 4, cause.clone()).is_err());
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal { .. })
        ));
        for _ in 0..2 {
            assert!(
                matches!(entries.poll(&key, 4).unwrap(), EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { failure }) if failure == cause)
            );
        }
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Record(OrderedIngressRecord::FailureObservationsOmitted)
        ));
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Empty
        ));
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;
    use ubm_core::ordered_notifications::{
        DeliveryMode, Error, FailureMetadataEntry, FailureValue, Member, Membership,
        OrderedCollection, Record, SourceGeneration,
    };

    fn source() -> SourceKey {
        SourceKey {
            peer_id: "peer".into(),
            generation: SourceGeneration::Counter(1),
            native_group: 4,
        }
    }
    #[test]
    fn increase_preserves_failed_setup_queue_and_requires_exact_identity() {
        let entries = EmptyOrderedSources::default();
        let key = source();
        entries.install(key.clone(), 4, 2, 16).unwrap();
        entries
            .admit(
                &key,
                4,
                OrderedIngressRecord::Terminal {
                    reason: EndReason::Closed,
                    rejection: None,
                    failure_member: None,
                    rejected_loss: None,
                    uncertain_tail: false,
                },
            )
            .unwrap();
        let evidence = cause();
        assert!(
            entries
                .admit(
                    &key,
                    4,
                    OrderedIngressRecord::BridgeFailure {
                        failure: evidence.clone()
                    }
                )
                .is_err()
        );
        let mut wrong = key.clone();
        wrong.generation = SourceGeneration::Counter(2);
        assert!(entries.increase_byte_capacity(&wrong, 4, 4096).is_err());
        assert!(
            entries
                .increase_byte_capacity(&key, 4, MAX_STREAM_BYTE_CAPACITY as usize + 1)
                .is_err()
        );
        assert_eq!(entries.increase_byte_capacity(&key, 4, 4096).unwrap(), 16);
        entries
            .admit(
                &key,
                4,
                OrderedIngressRecord::BridgeFailure {
                    failure: evidence.clone(),
                },
            )
            .unwrap();
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal {
                reason: EndReason::Closed,
                ..
            })
        ));
        assert!(
            matches!(entries.poll(&key, 4).unwrap(), EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { failure }) if failure == evidence)
        );
    }
    fn cause() -> Failure {
        Failure {
            code: "a".into(),
            domain: String::new(),
            operation: String::new(),
            detail: None,
            platform_domain: None,
            platform_code: None,
            platform_message: None,
            platform_metadata: Default::default(),
            platform_metadata_entries: Some(vec![
                FailureMetadataEntry {
                    key: "k".into(),
                    value: FailureValue::Int(7),
                },
                FailureMetadataEntry {
                    key: "k".into(),
                    value: FailureValue::Bool(true),
                },
            ]),
            platform_cleanup: Vec::new(),
            platform_cleanup_failures: Vec::new(),
        }
    }
    fn active_accepts(failure: &Failure, limit: usize) -> bool {
        let generation = SourceGeneration::Counter(1);
        let mut group = OrderedCollection::new(
            "c".into(),
            "d".into(),
            generation.clone(),
            4,
            vec![Member {
                id: 1,
                source: Membership {
                    member: 1,
                    epoch: 1,
                },
                delivery: DeliveryMode::Either,
            }],
            1,
            limit,
            1,
        )
        .unwrap();
        group.source_installed().unwrap();
        match group.admit_failure_record(
            &generation,
            4,
            Record::FailureObservation {
                failure: Some(failure.clone()),
                source_rejection: None,
                source_failure_member: None,
                rejected_loss: None,
                tail_uncertain: false,
            },
        ) {
            Ok(()) => true,
            Err(Error::RecordTooLarge) => false,
            other => panic!("unexpected result: {other:?}"),
        }
    }
    fn empty_accepts(failure: &Failure, limit: usize) -> bool {
        let entries = EmptyOrderedSources::default();
        let key = source();
        entries.install(key.clone(), 4, 1, limit).unwrap();
        entries
            .admit(
                &key,
                4,
                OrderedIngressRecord::BridgeFailure {
                    failure: failure.clone(),
                },
            )
            .is_ok()
    }
    #[test]
    fn marker_is_rejected_without_admission_or_terminal_mutation() {
        let entries = EmptyOrderedSources::default();
        let key = source();
        entries.install(key.clone(), 4, 1, 4096).unwrap();
        assert!(
            entries
                .admit(
                    &key,
                    4,
                    OrderedIngressRecord::Marker {
                        marker_id: 17,
                        ordinal: 3
                    }
                )
                .is_err()
        );
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Empty
        ));
        entries
            .admit(
                &key,
                4,
                OrderedIngressRecord::Terminal {
                    reason: EndReason::Closed,
                    rejection: None,
                    failure_member: None,
                    rejected_loss: None,
                    uncertain_tail: false,
                },
            )
            .unwrap();
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal {
                reason: EndReason::Closed,
                ..
            })
        ));
    }
    #[test]
    fn failed_setup_and_active_failure_thresholds_match() {
        let failure = cause();
        let mut nested = cause();
        nested.platform_cleanup_failures = vec![failure.clone(); 3];
        let mut mapped = cause();
        mapped
            .platform_metadata
            .insert("key".into(), FailureValue::Text("text".into()));
        for failure in [failure, nested, mapped] {
            let threshold = (1..16384)
                .find(|&limit| active_accepts(&failure, limit))
                .unwrap();
            for limit in [20, threshold - 1, threshold, threshold + 1] {
                assert_eq!(
                    empty_accepts(&failure, limit),
                    active_accepts(&failure, limit),
                    "limit {limit}"
                );
            }
        }
    }
    #[test]
    fn reserved_terminal_and_late_failure_keep_exact_evidence_and_credits() {
        let failure = cause();
        let bytes = failure_allocation_cost(&failure);
        let entries = EmptyOrderedSources::default();
        let key = source();
        entries.install(key.clone(), 4, 2, bytes).unwrap();
        entries
            .admit(
                &key,
                4,
                OrderedIngressRecord::Terminal {
                    reason: EndReason::SetupFailed(failure.clone()),
                    rejection: None,
                    failure_member: None,
                    rejected_loss: None,
                    uncertain_tail: false,
                },
            )
            .unwrap();
        let late = || OrderedIngressRecord::BridgeFailure {
            failure: failure.clone(),
        };
        assert_eq!(
            entries.admit(&key, 4, late()).unwrap(),
            OrderedIngressReceipt::Terminalized
        );
        assert_eq!(
            entries.admit(&key, 4, late()).unwrap(),
            OrderedIngressReceipt::PendingCapacity
        );
        assert!(
            matches!(entries.poll(&key, 4).unwrap(), EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal { reason: EndReason::SetupFailed(actual), .. }) if actual == failure)
        );
        // Taking the reserved terminal does not release the late record's bytes.
        assert_eq!(
            entries.admit(&key, 4, late()).unwrap(),
            OrderedIngressReceipt::PendingCapacity
        );
        assert!(
            matches!(entries.poll(&key, 4).unwrap(), EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { failure: actual }) if actual == failure)
        );
        assert_eq!(
            entries.admit(&key, 4, late()).unwrap(),
            OrderedIngressReceipt::Terminalized
        );
        assert!(
            matches!(entries.poll(&key, 4).unwrap(), EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { failure: actual }) if actual == failure)
        );
        assert!(matches!(
            entries.poll(&key, 4).unwrap(),
            EmptyOrderedPoll::Empty
        ));
    }
    #[test]
    fn empty_entry_and_nested_failure_allocations_are_bounded() {
        let mut entries = cause();
        entries.platform_metadata_entries = Some(vec![
            FailureMetadataEntry {
                key: String::new(),
                value: FailureValue::Text(String::new())
            };
            128
        ]);
        let mut nested = cause();
        nested.platform_metadata_entries = None;
        let leaf = nested.clone();
        nested.platform_cleanup_failures = vec![leaf; 128];
        for (failure, limit) in [
            (
                entries,
                128 * std::mem::size_of::<FailureMetadataEntry>() - 1,
            ),
            (nested, 128 * std::mem::size_of::<Failure>() - 1),
        ] {
            assert!(!empty_accepts(&failure, limit));
        }
    }
}
