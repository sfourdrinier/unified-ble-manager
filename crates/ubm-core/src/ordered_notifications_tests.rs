// crates/ubm-core/src/ordered_notifications_tests.rs
#![cfg(test)]

use super::*;

fn collection(items: usize, controls: usize) -> OrderedCollection {
    OrderedCollection::new(
        "connection".into(),
        "database".into(),
        SourceGeneration::Counter(7),
        9,
        vec![
            Member {
                id: 10,
                source: Membership {
                    member: 1,
                    epoch: 4,
                },
                delivery: DeliveryMode::Notification,
            },
            Member {
                id: 11,
                source: Membership {
                    member: 2,
                    epoch: 8,
                },
                delivery: DeliveryMode::Indication,
            },
        ],
        items,
        32,
        controls,
    )
    .unwrap()
}

#[test]
fn preclose_pending_value_drains_before_terminal() {
    let mut group = collection(1, 1);
    let source = SourceGeneration::Counter(7);
    let member = Membership {
        member: 1,
        epoch: 4,
    };
    group.source_installed().unwrap();
    for id in [10, 11] {
        group.begin_enable(id).unwrap();
        group.enable_result(id, Some(true)).unwrap();
    }
    let admit = |group: &mut OrderedCollection, ordinal| {
        group.admit_value(
            &source,
            9,
            member,
            ordinal,
            &[ordinal as u8],
            ObservedDelivery::Notification,
            NotificationProvenance::Notification,
        )
    };
    admit(&mut group, 1).unwrap();
    let mut retained = group
        .retain_occurrence(
            &source,
            9,
            RetainedRecord::Value {
                membership: member,
                ordinal: 2,
                value: vec![2],
                delivery: ObservedDelivery::Notification,
                provenance: NotificationProvenance::Notification,
            },
        )
        .unwrap();
    assert_eq!(
        group.retry_occurrence(&source, 9, &mut retained),
        Err(Error::PendingCapacity)
    );
    group.close(EndReason::Closed, false).unwrap();
    assert!(matches!(
        group.poll(),
        Poll::Record(Record::Value { ordinal: 1, .. })
    ));
    assert_eq!(group.poll(), Poll::Empty);
    assert_eq!(group.retry_occurrence(&source, 9, &mut retained), Ok(()));
    assert_eq!(
        group.retry_occurrence(&source, 9, &mut retained),
        Err(Error::InvalidPhase)
    );
    assert!(matches!(
        group.poll(),
        Poll::Record(Record::Value { ordinal: 2, .. })
    ));
    assert!(matches!(group.poll(), Poll::Terminal(_)));
}

#[test]
fn preclose_value_and_loss_leases_hold_fifo_and_terminal_across_capacity() {
    let mut group = collection(1, 1);
    let source = SourceGeneration::Counter(7);
    let member = Membership {
        member: 1,
        epoch: 4,
    };
    group.source_installed().unwrap();
    for id in [10, 11] {
        group.begin_enable(id).unwrap();
        group.enable_result(id, Some(true)).unwrap();
    }
    group
        .admit_value(
            &source,
            9,
            member,
            1,
            &[1],
            ObservedDelivery::Notification,
            NotificationProvenance::Notification,
        )
        .unwrap();
    group
        .admit_loss(
            &source,
            9,
            LossScope::Group,
            LossUnit::SourceRecords,
            Some(1),
            Some(3),
        )
        .unwrap();
    let value = RetainedRecord::Value {
        membership: member,
        ordinal: 2,
        value: vec![2],
        delivery: ObservedDelivery::Notification,
        provenance: NotificationProvenance::Notification,
    };
    let loss = || RetainedRecord::Loss {
        scope: LossScope::Group,
        unit: LossUnit::SourceRecords,
        count: Some(2),
        bytes: Some(4),
    };
    let mut held_value = group.retain_occurrence(&source, 9, value).unwrap();
    let mut held_loss = group.retain_occurrence(&source, 9, loss()).unwrap();
    assert_eq!(
        group.reserve_marker(&source, 9, 77),
        Err(Error::PendingCapacity)
    );
    assert_eq!(
        group.retry_occurrence(&source, 9, &mut held_loss),
        Err(Error::PendingCapacity)
    );
    assert_eq!(
        group.retry_occurrence(&source, 9, &mut held_value),
        Err(Error::PendingCapacity)
    );
    group.close(EndReason::Closed, false).unwrap();
    assert_eq!(
        group.retain_occurrence(&source, 9, loss()).unwrap_err().0,
        Error::InvalidPhase
    );
    assert_eq!(
        group.retry_occurrence(&SourceGeneration::Counter(8), 9, &mut held_value),
        Err(Error::WrongSource)
    );
    let mut replacement = collection(1, 1);
    replacement.source_installed().unwrap();
    assert_eq!(
        replacement.retry_occurrence(&source, 9, &mut held_value),
        Err(Error::WrongSource)
    );
    assert!(matches!(
        group.poll(),
        Poll::Record(Record::Value { ordinal: 1, .. })
    ));
    assert_eq!(group.retry_occurrence(&source, 9, &mut held_value), Ok(()));
    assert_eq!(
        group.retry_occurrence(&source, 9, &mut held_loss),
        Err(Error::PendingCapacity)
    );
    assert!(matches!(
        group.poll(),
        Poll::Record(Record::Loss { count: Some(1), .. })
    ));
    assert!(
        matches!(group.poll(), Poll::Record(Record::Value { ordinal: 2, value, .. }) if value == [2])
    );
    assert_eq!(group.poll(), Poll::Empty);
    assert_eq!(group.retry_occurrence(&source, 9, &mut held_loss), Ok(()));
    assert!(matches!(
        group.poll(),
        Poll::Record(Record::Loss {
            count: Some(2),
            bytes: Some(4),
            ..
        })
    ));
    assert!(matches!(
        group.poll(),
        Poll::Terminal(Terminal {
            reason: EndReason::Closed,
            ..
        })
    ));
    assert_eq!(group.poll(), Poll::Closed);
    assert_eq!(
        group.retry_occurrence(&source, 9, &mut held_loss),
        Err(Error::InvalidPhase)
    );
}

#[test]
fn canceled_lease_returns_occurrence_and_cannot_be_retried() {
    let mut group = collection(1, 1);
    let source = SourceGeneration::Counter(7);
    group.source_installed().unwrap();
    for id in [10, 11] {
        group.begin_enable(id).unwrap();
        group.enable_result(id, Some(true)).unwrap();
    }
    let mut lease = group
        .retain_occurrence(
            &source,
            9,
            RetainedRecord::Loss {
                scope: LossScope::Unknown,
                unit: LossUnit::Values,
                count: None,
                bytes: Some(9),
            },
        )
        .unwrap();
    group.close(EndReason::Closed, false).unwrap();
    assert_eq!(group.poll(), Poll::Empty);
    assert!(matches!(
        group.cancel_occurrence(&source, 9, &mut lease),
        Ok(RetainedRecord::Loss {
            count: None,
            bytes: Some(9),
            ..
        })
    ));
    assert_eq!(
        group.cancel_occurrence(&source, 9, &mut lease).unwrap_err(),
        Error::InvalidPhase
    );
    assert_eq!(
        group.retry_occurrence(&source, 9, &mut lease),
        Err(Error::InvalidPhase)
    );
    assert!(matches!(group.poll(), Poll::Terminal(_)));
}

#[test]
fn retained_value_increased_budget_precedes_separable_terminal_and_late_failure() {
    let mut group = collection(1, 2);
    let source = SourceGeneration::Counter(7);
    let member = Membership {
        member: 1,
        epoch: 4,
    };
    group.source_installed().unwrap();
    for id in [10, 11] {
        group.begin_enable(id).unwrap();
        group.enable_result(id, Some(true)).unwrap();
    }
    let mut lease = group
        .retain_occurrence(
            &source,
            9,
            RetainedRecord::Value {
                membership: member,
                ordinal: 11,
                value: vec![8; 40],
                delivery: ObservedDelivery::Unknown,
                provenance: NotificationProvenance::Unknown,
            },
        )
        .unwrap();
    assert_eq!(
        group.retry_occurrence(&source, 9, &mut lease),
        Err(Error::PendingCapacity)
    );
    group
        .source_terminal(EndReason::SourceOverflow, None, Some(member), true)
        .unwrap();
    let late = Record::FailureObservationsOmitted { count: None };
    group
        .admit_failure_record(&source, 9, late.clone())
        .unwrap();
    assert_eq!(group.poll(), Poll::Empty);
    group.increase_byte_capacity(&source, 9, 40).unwrap();
    group.retry_occurrence(&source, 9, &mut lease).unwrap();
    assert!(
        matches!(group.poll(), Poll::Record(Record::Value { ordinal: 11, value, .. }) if value == vec![8; 40])
    );
    assert!(
        matches!(group.poll(), Poll::Terminal(Terminal { reason: EndReason::SourceOverflow, source_failure_member: Some(actual), .. }) if actual == member)
    );
    assert_eq!(group.poll(), Poll::Record(late));
    assert_eq!(group.poll(), Poll::Closed);
}

#[test]
fn capacity_increase_is_source_fenced_and_preserves_terminal_and_queue() {
    let mut group = collection(2, 2);
    let source = SourceGeneration::Counter(7);
    group.source_installed().unwrap();
    group.close(EndReason::Closed, false).unwrap();
    let failure = Failure {
        code: "native".into(),
        domain: String::new(),
        operation: String::new(),
        detail: Some("x".repeat(64)),
        platform_domain: None,
        platform_code: None,
        platform_message: None,
        platform_metadata: BTreeMap::new(),
        platform_metadata_entries: None,
        platform_cleanup: Vec::new(),
        platform_cleanup_failures: Vec::new(),
    };
    let record = Record::FailureObservation {
        failure: Some(failure),
        source_rejection: None,
        source_failure_member: None,
        rejected_loss: None,
        tail_uncertain: false,
    };
    assert_eq!(
        group.admit_failure_record(&source, 9, record.clone()),
        Err(Error::RecordTooLarge)
    );
    assert_eq!(
        group.increase_byte_capacity(&SourceGeneration::Counter(8), 9, 128),
        Err(Error::WrongSource)
    );
    assert_eq!(
        group.increase_byte_capacity(&source, 9, (MAX_STREAM_BYTE_CAPACITY + 1) as usize),
        Err(Error::InvalidConfiguration)
    );
    assert_eq!(group.increase_byte_capacity(&source, 9, 128), Ok(32));
    assert_eq!(
        group.increase_byte_capacity(&source, 9, 31),
        Err(Error::InvalidConfiguration)
    );
    group
        .admit_failure_record(&source, 9, record.clone())
        .unwrap();
    assert_eq!(group.poll(), Poll::Record(record));
    assert!(matches!(
        group.poll(),
        Poll::Terminal(Terminal {
            reason: EndReason::Closed,
            ..
        })
    ));
}

#[test]
fn retained_value_capacity_retry_preserves_ordinal_and_close_fence() {
    let mut group = collection(1, 1);
    let source = SourceGeneration::Counter(7);
    let member = Membership {
        member: 1,
        epoch: 4,
    };
    group.source_installed().unwrap();
    for id in [10, 11] {
        group.begin_enable(id).unwrap();
        group.enable_result(id, Some(true)).unwrap();
    }
    let admit = |owner: &mut OrderedCollection, ordinal, bytes: &[u8]| {
        owner.admit_value(
            &source,
            9,
            member,
            ordinal,
            bytes,
            ObservedDelivery::Notification,
            NotificationProvenance::Notification,
        )
    };
    admit(&mut group, 12, &[1]).unwrap();
    assert_eq!(admit(&mut group, 14, &[2]), Err(Error::PendingCapacity));
    assert_eq!(admit(&mut group, 14, &[2]), Err(Error::PendingCapacity));
    assert!(
        matches!(group.poll(), Poll::Record(Record::Value { ordinal: 12, value, .. }) if value == [1])
    );
    admit(&mut group, 14, &[2]).unwrap();
    assert!(
        matches!(group.poll(), Poll::Record(Record::Value { ordinal: 14, value, .. }) if value == [2])
    );
    assert_eq!(group.poll(), Poll::Empty);
    admit(&mut group, 15, &[3]).unwrap();
    assert_eq!(admit(&mut group, 16, &[4]), Err(Error::PendingCapacity));
    group.close(EndReason::Closed, false).unwrap();
    assert_eq!(admit(&mut group, 16, &[4]), Err(Error::InvalidPhase));
    assert!(
        matches!(group.poll(), Poll::Record(Record::Value { ordinal: 15, value, .. }) if value == [3])
    );
    assert!(matches!(
        group.poll(),
        Poll::Terminal(Terminal {
            reason: EndReason::Closed,
            tail_uncertain: false,
            ..
        })
    ));
}

#[test]
fn individually_oversized_value_can_be_retried_without_consuming_ordinal() {
    let mut group = collection(2, 2);
    let source = SourceGeneration::Counter(7);
    let member = Membership {
        member: 1,
        epoch: 4,
    };
    group.source_installed().unwrap();
    group.begin_enable(10).unwrap();
    group.enable_result(10, Some(true)).unwrap();
    group.begin_enable(11).unwrap();
    group.enable_result(11, Some(true)).unwrap();
    let value = vec![42; 40];
    assert_eq!(
        group.validate_individual_value(&source, 9, value.len()),
        Err(Error::RecordTooLarge)
    );
    assert_eq!(
        group.admit_value(
            &source,
            9,
            member,
            1,
            &value,
            ObservedDelivery::Unknown,
            NotificationProvenance::Unknown
        ),
        Err(Error::PendingCapacity)
    );
    group.increase_byte_capacity(&source, 9, 64).unwrap();
    group
        .validate_individual_value(&source, 9, value.len())
        .unwrap();
    group
        .admit_value(
            &source,
            9,
            member,
            1,
            &value,
            ObservedDelivery::Unknown,
            NotificationProvenance::Unknown,
        )
        .unwrap();
    assert!(
        matches!(group.poll(), Poll::Record(Record::Value { ordinal: 1, value: actual, .. }) if actual == value)
    );
}

#[test]
fn hostile_failure_depth_is_rejected_without_recursive_work() {
    let mut failure = Failure {
        code: String::new(),
        domain: String::new(),
        operation: String::new(),
        detail: None,
        platform_domain: None,
        platform_code: None,
        platform_message: None,
        platform_metadata: BTreeMap::new(),
        platform_metadata_entries: None,
        platform_cleanup: Vec::new(),
        platform_cleanup_failures: Vec::new(),
    };
    for _ in 0..1000 {
        failure = Failure {
            code: String::new(),
            domain: String::new(),
            operation: String::new(),
            detail: None,
            platform_domain: None,
            platform_code: None,
            platform_message: None,
            platform_metadata: BTreeMap::new(),
            platform_metadata_entries: None,
            platform_cleanup: Vec::new(),
            platform_cleanup_failures: vec![failure],
        };
    }
    assert_eq!(failure_allocation_cost(&failure), usize::MAX);
}

#[test]
fn hostile_platform_detail_depth_is_rejected_and_dropped_iteratively() {
    let mut detail = FailurePlatformDetail {
        domain: String::new(),
        code: String::new(),
        message: None,
        metadata: BTreeMap::new(),
        metadata_entries: None,
        cleanup: Vec::new(),
        cleanup_failures: Vec::new(),
    };
    for _ in 0..20_000 {
        detail = FailurePlatformDetail {
            domain: String::new(),
            code: String::new(),
            message: None,
            metadata: BTreeMap::new(),
            metadata_entries: None,
            cleanup: vec![detail],
            cleanup_failures: Vec::new(),
        };
    }
    let failure = Failure {
        code: String::new(),
        domain: String::new(),
        operation: String::new(),
        detail: None,
        platform_domain: None,
        platform_code: None,
        platform_message: None,
        platform_metadata: BTreeMap::new(),
        platform_metadata_entries: None,
        platform_cleanup: vec![detail],
        platform_cleanup_failures: Vec::new(),
    };
    assert_eq!(failure_allocation_cost(&failure), usize::MAX);
}

#[test]
fn oversized_first_terminal_keeps_marker_credit_and_phase() {
    let mut group = collection(2, 2);
    group.source_installed().unwrap();
    group
        .reserve_marker(&SourceGeneration::Counter(7), 9, 17)
        .unwrap();
    let failure = Failure {
        code: "source".into(),
        domain: String::new(),
        operation: String::new(),
        detail: Some("x".repeat(33)),
        platform_domain: None,
        platform_code: None,
        platform_message: None,
        platform_metadata: BTreeMap::new(),
        platform_metadata_entries: None,
        platform_cleanup: Vec::new(),
        platform_cleanup_failures: Vec::new(),
    };
    assert_eq!(
        group.source_terminal(
            EndReason::SourceFailed(failure),
            Some(SourceRejection::Marker { marker_id: 17 }),
            None,
            false
        ),
        Err(Error::RecordTooLarge)
    );
    assert_eq!(group.phase(), Phase::Staging);
    group
        .source_terminal(
            EndReason::SourceOverflow,
            Some(SourceRejection::Marker { marker_id: 17 }),
            None,
            false,
        )
        .unwrap();
    assert!(matches!(
        group.poll(),
        Poll::Terminal(Terminal {
            source_rejection: Some(SourceRejection::Marker { marker_id: 17 }),
            ..
        })
    ));
}

#[test]
fn failure_observations_retry_without_mutation_and_keep_occurrences() {
    let mut group = collection(4, 1);
    group.source_installed().unwrap();
    group.begin_enable(10).unwrap();
    group.enable_result(10, Some(true)).unwrap();
    group.begin_enable(11).unwrap();
    group.enable_result(11, Some(true)).unwrap();
    group.close(EndReason::Closed, false).unwrap();
    let failure = Failure {
        code: "a".into(),
        domain: "b".into(),
        operation: "c".into(),
        detail: None,
        platform_domain: None,
        platform_code: None,
        platform_message: None,
        platform_metadata: BTreeMap::new(),
        platform_metadata_entries: None,
        platform_cleanup: Vec::new(),
        platform_cleanup_failures: Vec::new(),
    };
    let observation = Record::FailureObservation {
        failure: Some(failure.clone()),
        source_rejection: None,
        source_failure_member: None,
        rejected_loss: None,
        tail_uncertain: false,
    };
    let omission = Record::FailureObservationsOmitted { count: None };
    let source = SourceGeneration::Counter(7);
    assert_eq!(
        group.admit_failure_record(&SourceGeneration::Counter(8), 9, observation.clone()),
        Err(Error::WrongSource)
    );
    group
        .admit_failure_record(&source, 9, observation.clone())
        .unwrap();
    assert_eq!(
        group.admit_failure_record(&source, 9, observation.clone()),
        Err(Error::PendingCapacity)
    );
    assert_eq!(group.poll(), Poll::Record(observation.clone()));
    group
        .admit_failure_record(&source, 9, observation.clone())
        .unwrap();
    assert_eq!(
        group.admit_failure_record(&source, 9, omission.clone()),
        Err(Error::PendingCapacity)
    );
    assert_eq!(group.poll(), Poll::Record(observation));
    group
        .admit_failure_record(&source, 9, omission.clone())
        .unwrap();
    assert_eq!(group.poll(), Poll::Record(omission));
    assert!(matches!(
        group.poll(),
        Poll::Terminal(Terminal {
            reason: EndReason::Closed,
            failure_observations_omitted: false,
            ..
        })
    ));
    let mut failure = failure;
    failure.detail = Some("x".repeat(33));
    let huge = Record::FailureObservation {
        failure: Some(failure),
        source_rejection: None,
        source_failure_member: None,
        rejected_loss: None,
        tail_uncertain: false,
    };
    assert_eq!(
        group.admit_failure_record(&source, 9, huge),
        Err(Error::RecordTooLarge)
    );
    assert_eq!(group.poll(), Poll::Closed);
}

#[test]
fn source_terminal_keeps_its_position_before_later_failure_records() {
    let mut group = collection(4, 2);
    let source = SourceGeneration::Counter(7);
    group.source_installed().unwrap();
    group.begin_enable(10).unwrap();
    group.enable_result(10, Some(true)).unwrap();
    group.begin_enable(11).unwrap();
    group.enable_result(11, Some(true)).unwrap();
    group
        .admit_loss(&source, 9, LossScope::Unknown, LossUnit::Values, None, None)
        .unwrap();
    let member = Membership {
        member: 1,
        epoch: 4,
    };
    group
        .source_terminal(EndReason::SourceOverflow, None, Some(member), true)
        .unwrap();
    let later = Record::FailureObservationsOmitted { count: None };
    group
        .admit_failure_record(&source, 9, later.clone())
        .unwrap();
    assert!(matches!(group.poll(), Poll::Record(Record::Loss { .. })));
    assert!(
        matches!(group.poll(), Poll::Terminal(Terminal { source_failure_member: Some(actual), .. }) if actual == member)
    );
    assert_eq!(group.poll(), Poll::Record(later));
    assert_eq!(group.poll(), Poll::Closed);
}

#[test]
fn failure_observation_bytes_share_value_budget() {
    let mut group = collection(4, 2);
    let source = SourceGeneration::Counter(7);
    let membership = Membership {
        member: 1,
        epoch: 4,
    };
    group.source_installed().unwrap();
    group.begin_enable(10).unwrap();
    group.enable_result(10, Some(true)).unwrap();
    group.begin_enable(11).unwrap();
    group.enable_result(11, Some(true)).unwrap();
    group
        .admit_value(
            &source,
            9,
            membership,
            1,
            &[1],
            ObservedDelivery::Unknown,
            NotificationProvenance::Unknown,
        )
        .unwrap();
    let failure = Failure {
        code: "a".into(),
        domain: "b".into(),
        operation: "c".into(),
        detail: Some("x".repeat(29)),
        platform_domain: None,
        platform_code: None,
        platform_message: None,
        platform_metadata: BTreeMap::new(),
        platform_metadata_entries: None,
        platform_cleanup: Vec::new(),
        platform_cleanup_failures: Vec::new(),
    };
    let observation = Record::FailureObservation {
        failure: Some(failure),
        source_rejection: None,
        source_failure_member: None,
        rejected_loss: None,
        tail_uncertain: false,
    };
    assert_eq!(
        group.admit_failure_record(&source, 9, observation.clone()),
        Err(Error::PendingCapacity)
    );
    assert!(matches!(group.poll(), Poll::Record(Record::Value { .. })));
    group
        .admit_failure_record(&source, 9, observation.clone())
        .unwrap();
    assert_eq!(group.poll(), Poll::Record(observation));
}

#[test]
fn repeated_empty_metadata_entries_consume_bounded_control_storage() {
    let mut group = collection(4, 2);
    group.source_installed().unwrap();
    let failure = Failure {
        code: "a".into(),
        domain: "b".into(),
        operation: "c".into(),
        detail: None,
        platform_domain: None,
        platform_code: None,
        platform_message: None,
        platform_metadata: BTreeMap::new(),
        platform_metadata_entries: Some(vec![
            FailureMetadataEntry {
                key: String::new(),
                value: FailureValue::Bool(true)
            };
            4
        ]),
        platform_cleanup: Vec::new(),
        platform_cleanup_failures: Vec::new(),
    };
    let observation = Record::FailureObservation {
        failure: Some(failure),
        source_rejection: None,
        source_failure_member: None,
        rejected_loss: None,
        tail_uncertain: false,
    };
    assert_eq!(
        group.admit_failure_record(&SourceGeneration::Counter(7), 9, observation),
        Err(Error::RecordTooLarge)
    );
}

#[test]
fn opaque_source_generation_is_not_an_untagged_counter() {
    let member = Member {
        id: 1,
        source: Membership {
            member: 1,
            epoch: 0,
        },
        delivery: DeliveryMode::Either,
    };
    let mut group = OrderedCollection::new(
        "conn".into(),
        "db".into(),
        SourceGeneration::Opaque("123".into()),
        1,
        vec![member],
        1,
        64,
        1,
    )
    .unwrap();
    group.source_installed().unwrap();
    assert_eq!(
        group.admit_value(
            &SourceGeneration::Counter(123),
            1,
            member.source,
            0,
            &[1],
            ObservedDelivery::Unknown,
            NotificationProvenance::Unknown
        ),
        Err(Error::WrongSource)
    );
    group
        .admit_value(
            &SourceGeneration::Opaque("123".into()),
            1,
            member.source,
            0,
            &[1],
            ObservedDelivery::Unknown,
            NotificationProvenance::Unknown,
        )
        .unwrap();
}

#[test]
fn staging_keeps_interleaved_native_order_and_source_identity() {
    let mut group = collection(4, 2);
    assert_eq!(group.begin_enable(10), Err(Error::InvalidPhase));
    group.source_installed().unwrap();
    group.begin_enable(10).unwrap();
    group
        .admit_value(
            &SourceGeneration::Counter(7),
            9,
            Membership {
                member: 1,
                epoch: 4,
            },
            100,
            &[1],
            ObservedDelivery::Notification,
            NotificationProvenance::Notification,
        )
        .unwrap();
    assert_eq!(
        group.poll(),
        Poll::Empty,
        "setup values remain staged until every member is acquired"
    );
    group.enable_result(10, Some(true)).unwrap();
    group.begin_enable(11).unwrap();
    group
        .admit_value(
            &SourceGeneration::Counter(7),
            9,
            Membership {
                member: 2,
                epoch: 8,
            },
            u64::MAX,
            &[2],
            ObservedDelivery::Indication,
            NotificationProvenance::Unknown,
        )
        .unwrap();
    group.enable_result(11, Some(true)).unwrap();
    assert_eq!(group.phase(), Phase::Active);
    assert!(
        matches!(group.poll(), Poll::Record(Record::Value { ordinal: 100, member: 10, value, .. }) if value == [1])
    );
    assert!(
        matches!(group.poll(), Poll::Record(Record::Value { ordinal: u64::MAX, member: 11, value, .. }) if value == [2])
    );
    assert_eq!(
        group.admit_value(
            &SourceGeneration::Counter(8),
            9,
            Membership {
                member: 1,
                epoch: 4
            },
            1,
            &[],
            ObservedDelivery::Notification,
            NotificationProvenance::Unknown
        ),
        Err(Error::WrongSource)
    );
    group
        .admit_value(
            &SourceGeneration::Counter(7),
            9,
            Membership {
                member: 1,
                epoch: 4,
            },
            u64::MAX,
            &[],
            ObservedDelivery::Notification,
            NotificationProvenance::Unknown,
        )
        .unwrap();
    assert!(matches!(
        group.poll(),
        Poll::Terminal(Terminal {
            reason: EndReason::OrdinalInvalid,
            tail_uncertain: true,
            ..
        })
    ));
}

#[test]
fn failed_setup_drains_observations_and_retains_late_enable_and_cleanup() {
    let mut group = collection(4, 2);
    group.source_installed().unwrap();
    group.begin_enable(10).unwrap();
    group.enable_result(10, Some(true)).unwrap();
    group.begin_enable(11).unwrap();
    group
        .admit_value(
            &SourceGeneration::Counter(7),
            9,
            Membership {
                member: 1,
                epoch: 4,
            },
            1,
            &[7],
            ObservedDelivery::Notification,
            NotificationProvenance::Unknown,
        )
        .unwrap();
    group
        .admit_loss(
            &SourceGeneration::Counter(7),
            9,
            LossScope::Unknown,
            LossUnit::SourceRecords,
            None,
            None,
        )
        .unwrap();
    let failure = Failure {
        code: "subscription.failed".into(),
        domain: "gatt".into(),
        operation: "subscribe".into(),
        detail: None,
        platform_domain: Some("native".into()),
        platform_code: Some("17".into()),
        platform_message: Some("native failure".into()),
        platform_metadata: BTreeMap::from([("status".into(), FailureValue::Int(17))]),
        platform_metadata_entries: None,
        platform_cleanup: Vec::new(),
        platform_cleanup_failures: Vec::new(),
    };
    group.fail_enable(11, None, failure.clone(), true).unwrap();
    assert_eq!(group.cleanup_candidates().len(), 2);
    assert!(matches!(group.poll(), Poll::Record(Record::Value { value, .. }) if value == [7]));
    assert!(matches!(
        group.poll(),
        Poll::Record(Record::Loss {
            scope: LossScope::Unknown,
            unit: LossUnit::SourceRecords,
            count: None,
            bytes: None
        })
    ));
    assert_eq!(
        group.poll(),
        Poll::Terminal(Terminal {
            reason: EndReason::SetupFailed(failure),
            tail_uncertain: true,
            rejected_values: None,
            rejected_loss: None,
            source_rejection: None,
            source_failure_member: None,
            secondary_failure: None,
            additional_failure_count: 0,
            failure_count_overflowed: false,
            failure_observations_omitted: false,
        })
    );
    group.begin_release(10).unwrap();
    group.release_result(10, Err("refused".into())).unwrap();
    assert_eq!(group.members()[0].acquisition, Acquisition::ReleaseFailed);
    group.begin_release(10).unwrap();
    group.release_result(10, Ok(())).unwrap();
    group.begin_release(11).unwrap();
    group.release_result(11, Ok(())).unwrap();
    assert!(
        !group.cleanup_complete(),
        "pending enable can still succeed"
    );
    group.enable_result(11, Some(true)).unwrap();
    assert_eq!(group.members()[1].acquisition, Acquisition::Acquired);
    group.begin_release(11).unwrap();
    group.release_result(11, Ok(())).unwrap();
    assert!(group.cleanup_complete());
}

#[test]
fn enable_success_during_release_remains_cleanup_debt() {
    let mut group = collection(2, 2);
    group.source_installed().unwrap();
    group.begin_enable(10).unwrap();
    group.close(EndReason::Closed, true).unwrap();
    group.begin_release(10).unwrap();
    group.enable_result(10, Some(true)).unwrap();
    group.release_result(10, Ok(())).unwrap();
    assert_eq!(group.members()[0].acquisition, Acquisition::Acquired);
    assert!(!group.cleanup_complete());
    group.begin_release(10).unwrap();
    group.release_result(10, Ok(())).unwrap();
    assert!(group.cleanup_complete());
}

#[test]
fn bounded_controls_and_data_preserve_terminal_after_retained_prefix() {
    let mut group = collection(1, 1);
    group.source_installed().unwrap();
    group
        .admit_value(
            &SourceGeneration::Counter(7),
            9,
            Membership {
                member: 1,
                epoch: 4,
            },
            1,
            &[1],
            ObservedDelivery::Notification,
            NotificationProvenance::Unknown,
        )
        .unwrap();
    group
        .admit_loss(
            &SourceGeneration::Counter(7),
            9,
            LossScope::Group,
            LossUnit::SourceRecords,
            Some(3),
            None,
        )
        .unwrap();
    assert_eq!(
        group.admit_loss(
            &SourceGeneration::Counter(7),
            9,
            LossScope::Unknown,
            LossUnit::SourceRecords,
            Some(u64::MAX),
            None,
        ),
        Err(Error::PendingCapacity)
    );
    // The terminal reserve remains available for a real upstream overflow.
    group
        .source_terminal_with_rejected_loss(
            EndReason::SourceOverflow,
            None,
            None,
            Some(RejectedLoss {
                scope: LossScope::Unknown,
                unit: LossUnit::SourceRecords,
                count: Some(u64::MAX),
                bytes: None,
            }),
            true,
        )
        .unwrap();
    assert!(matches!(group.poll(), Poll::Record(Record::Value { .. })));
    assert!(matches!(
        group.poll(),
        Poll::Record(Record::Loss {
            scope: LossScope::Group,
            unit: LossUnit::SourceRecords,
            count: Some(3),
            bytes: None
        })
    ));
    assert!(matches!(
        group.poll(),
        Poll::Terminal(Terminal {
            reason: EndReason::SourceOverflow,
            tail_uncertain: true,
            rejected_values: None,
            rejected_loss: Some(RejectedLoss {
                scope: LossScope::Unknown,
                unit: LossUnit::SourceRecords,
                count: Some(u64::MAX),
                bytes: None
            }),
            source_rejection: None,
            source_failure_member: None,
            secondary_failure: None,
            additional_failure_count: 0,
            failure_count_overflowed: false,
            failure_observations_omitted: false,
        })
    ));
    assert_eq!(group.poll(), Poll::Closed);
}

#[test]
fn source_terminal_preserves_native_rejection_and_structured_failure() {
    let mut group = collection(2, 2);
    group.max_bytes = 4096;
    group.source_installed().unwrap();
    let failure = Failure {
        code: "platform.failure".into(),
        domain: "gatt".into(),
        operation: "notification.source".into(),
        detail: Some("source ended".into()),
        platform_domain: Some("native".into()),
        platform_code: Some("17".into()),
        platform_message: Some("read failed".into()),
        platform_metadata: BTreeMap::from([("status".into(), FailureValue::Int(17))]),
        platform_metadata_entries: None,
        platform_cleanup: Vec::new(),
        platform_cleanup_failures: Vec::new(),
    };
    let native_member = Membership {
        member: 2,
        epoch: 8,
    };
    group
        .source_terminal(
            EndReason::SourceFailed(failure.clone()),
            Some(SourceRejection::Value { bytes: 97 }),
            Some(native_member),
            true,
        )
        .unwrap();
    assert_eq!(
        group.poll(),
        Poll::Terminal(Terminal {
            reason: EndReason::SourceFailed(failure),
            tail_uncertain: true,
            rejected_values: None,
            rejected_loss: None,
            source_rejection: Some(SourceRejection::Value { bytes: 97 }),
            source_failure_member: Some(native_member),
            secondary_failure: None,
            additional_failure_count: 0,
            failure_count_overflowed: false,
            failure_observations_omitted: false,
        })
    );
}

#[test]
fn source_terminal_preserves_known_and_unknown_rejected_loss() {
    for loss in [
        RejectedLoss {
            scope: LossScope::Member(10),
            unit: LossUnit::SourceRecords,
            count: Some(u64::MAX),
            bytes: None,
        },
        RejectedLoss {
            scope: LossScope::Unknown,
            unit: LossUnit::SourceRecords,
            count: None,
            bytes: None,
        },
        RejectedLoss {
            scope: LossScope::Group,
            unit: LossUnit::Values,
            count: Some(1),
            bytes: Some(7),
        },
    ] {
        let mut group = collection(2, 2);
        group.source_installed().unwrap();
        group
            .source_terminal_with_rejected_loss(
                EndReason::SourceOverflow,
                None,
                None,
                Some(loss),
                true,
            )
            .unwrap();
        assert!(
            matches!(group.poll(), Poll::Terminal(Terminal { rejected_loss: Some(actual), tail_uncertain: true, .. }) if actual == loss)
        );
    }
}

#[test]
fn source_marker_shares_ordinal_order_and_reports_control_overflow() {
    let mut group = collection(4, 1);
    group.source_installed().unwrap();
    for id in [10, 11] {
        group.begin_enable(id).unwrap();
        group.enable_result(id, Some(true)).unwrap();
    }
    let source = SourceGeneration::Counter(7);
    let member = Membership {
        member: 1,
        epoch: 4,
    };
    group
        .admit_value(
            &source,
            9,
            member,
            1,
            &[1],
            ObservedDelivery::Unknown,
            NotificationProvenance::Unknown,
        )
        .unwrap();
    group.reserve_marker(&source, 9, 42).unwrap();
    group.admit_marker(&source, 9, 42, 2).unwrap();
    group
        .admit_value(
            &source,
            9,
            member,
            3,
            &[2],
            ObservedDelivery::Unknown,
            NotificationProvenance::Unknown,
        )
        .unwrap();
    assert_eq!(
        group.reserve_marker(&source, 9, 43),
        Err(Error::MarkerCreditUnavailable)
    );
    assert!(matches!(
        group.poll(),
        Poll::Record(Record::Value { ordinal: 1, .. })
    ));
    assert_eq!(
        group.poll(),
        Poll::Record(Record::Marker {
            marker_id: 42,
            ordinal: 2
        })
    );
    assert!(matches!(
        group.poll(),
        Poll::Record(Record::Value { ordinal: 3, .. })
    ));
    assert_eq!(group.poll(), Poll::Empty);
    group.reserve_marker(&source, 9, 43).unwrap();
    group
        .source_terminal(
            EndReason::SourceOverflow,
            Some(SourceRejection::Marker { marker_id: 43 }),
            None,
            true,
        )
        .unwrap();
    assert!(matches!(
        group.poll(),
        Poll::Terminal(Terminal {
            reason: EndReason::SourceOverflow,
            source_rejection: Some(SourceRejection::Marker { marker_id: 43 }),
            ..
        })
    ));
}

#[test]
fn marker_credit_is_reserved_against_other_controls_and_survives_source_failure() {
    let mut group = collection(4, 2);
    group.source_installed().unwrap();
    for id in [10, 11] {
        group.begin_enable(id).unwrap();
        group.enable_result(id, Some(true)).unwrap();
    }
    let source = SourceGeneration::Counter(7);
    group.reserve_marker(&source, 9, 0).unwrap();
    group.reserve_marker(&source, 9, 0).unwrap(); // exact retry reuses credit
    assert_eq!(
        group.reserve_marker(&source, 9, 1),
        Err(Error::MarkerCreditUnavailable)
    );
    assert_eq!(
        group.source_terminal(
            EndReason::SourceOrdinalExhausted,
            Some(SourceRejection::Marker { marker_id: 1 }),
            None,
            true
        ),
        Err(Error::MarkerCreditUnavailable)
    );
    assert!(
        group.marker_reserved(0),
        "a mismatched source rejection must not consume another marker's credit"
    );
    group
        .admit_loss(
            &source,
            9,
            LossScope::Unknown,
            LossUnit::SourceRecords,
            Some(1),
            None,
        )
        .unwrap();
    assert_eq!(
        group.admit_loss(
            &source,
            9,
            LossScope::Unknown,
            LossUnit::SourceRecords,
            Some(2),
            None
        ),
        Err(Error::PendingCapacity)
    );
    assert!(matches!(
        group.poll(),
        Poll::Record(Record::Loss { count: Some(1), .. })
    ));
    group
        .admit_loss(
            &source,
            9,
            LossScope::Unknown,
            LossUnit::SourceRecords,
            Some(2),
            None,
        )
        .unwrap();
    assert!(matches!(
        group.poll(),
        Poll::Record(Record::Loss { count: Some(2), .. })
    ));
    group.close(EndReason::Closed, false).unwrap();
    assert!(matches!(
        group.poll(),
        Poll::Terminal(Terminal {
            reason: EndReason::Closed,
            ..
        })
    ));
    for id in [10, 11] {
        group.begin_release(id).unwrap();
        group.release_result(id, Ok(())).unwrap();
    }
    assert!(
        !group.cleanup_complete(),
        "uncertain marker retains its reserved ownership"
    );
    assert_eq!(group.release_marker(1), Err(Error::InvalidConfiguration));
    group.release_marker(0).unwrap(); // only after authoritative NotAdmitted
    assert!(group.cleanup_complete());
}

#[test]
fn late_failure_summary_preserves_first_count_and_unknown_overflow() {
    let failure = |operation: &str| Failure {
        code: "platform.failure".into(),
        domain: "gatt".into(),
        operation: operation.into(),
        detail: None,
        platform_domain: None,
        platform_code: None,
        platform_message: None,
        platform_metadata: Default::default(),
        platform_metadata_entries: None,
        platform_cleanup: Vec::new(),
        platform_cleanup_failures: Vec::new(),
    };
    let mut first = LateFailureSummary::default();
    first.observe(failure("first"));
    let mut next = LateFailureSummary::default();
    next.observe(failure("second"));
    next.observe(failure("third"));
    next.omitted_unknown = true;
    first.merge(next);
    assert_eq!(first.first.unwrap().operation, "first");
    assert_eq!(first.additional_count, 2);
    assert!(first.omitted_unknown);
    assert!(!first.count_overflowed);
    let mut saturated = LateFailureSummary {
        first: Some(failure("first")),
        additional_count: u64::MAX,
        ..Default::default()
    };
    saturated.observe(failure("another"));
    assert!(saturated.count_overflowed);
}

#[test]
fn source_overflow_alone_does_not_prove_observations_were_omitted() {
    let mut group = collection(4, 1);
    group.source_installed().unwrap();
    group
        .source_terminal(EndReason::SourceOverflow, None, None, true)
        .unwrap();
    assert!(matches!(
        group.poll(),
        Poll::Terminal(Terminal {
            failure_observations_omitted: false,
            tail_uncertain: true,
            ..
        })
    ));
}

#[test]
fn omission_does_not_imply_uncertain_tail() {
    let mut group = collection(4, 1);
    group.source_installed().unwrap();
    group
        .source_terminal(EndReason::Closed, None, None, false)
        .unwrap();
    let Poll::Terminal(mut terminal) = group.poll() else {
        panic!("terminal expected")
    };
    terminal.merge_late_failures(LateFailureSummary {
        omitted_unknown: true,
        ..Default::default()
    });
    assert!(terminal.failure_observations_omitted);
    assert!(!terminal.tail_uncertain);
}

#[test]
fn failure_budget_counts_map_nodes_and_nested_failure_storage() {
    let empty_failure = || Failure {
        code: String::new(),
        domain: String::new(),
        operation: String::new(),
        detail: None,
        platform_domain: None,
        platform_code: None,
        platform_message: None,
        platform_metadata: BTreeMap::new(),
        platform_metadata_entries: None,
        platform_cleanup: Vec::new(),
        platform_cleanup_failures: Vec::new(),
    };
    let mut mapped = empty_failure();
    for index in 0..128 {
        mapped
            .platform_metadata
            .insert(index.to_string(), FailureValue::Text(String::new()));
    }
    let mut nested = empty_failure();
    nested.platform_cleanup_failures = vec![empty_failure(); 128];
    let mut details = empty_failure();
    details.platform_cleanup = vec![
        FailurePlatformDetail {
            domain: String::new(),
            code: String::new(),
            message: None,
            metadata: BTreeMap::new(),
            metadata_entries: None,
            cleanup: Vec::new(),
            cleanup_failures: Vec::new(),
        };
        128
    ];
    for (failure, limit) in [
        (
            mapped,
            128 * std::mem::size_of::<(String, FailureValue)>() - 1,
        ),
        (nested, 128 * std::mem::size_of::<Failure>() - 1),
        (
            details,
            128 * std::mem::size_of::<FailurePlatformDetail>() - 1,
        ),
    ] {
        let mut group = OrderedCollection::new(
            "c".into(),
            "d".into(),
            SourceGeneration::Counter(7),
            9,
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
        let record = Record::FailureObservation {
            failure: Some(failure),
            source_rejection: None,
            source_failure_member: None,
            rejected_loss: None,
            tail_uncertain: false,
        };
        assert_eq!(
            group.admit_failure_record(&SourceGeneration::Counter(7), 9, record),
            Err(Error::RecordTooLarge)
        );
    }
}
