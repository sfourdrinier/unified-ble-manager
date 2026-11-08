// crates/ubm-core/src/ordered_notifications_tests.rs
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
    group
        .admit_loss(
            &SourceGeneration::Counter(7),
            9,
            LossScope::Unknown,
            LossUnit::SourceRecords,
            Some(u64::MAX),
            None,
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
            reason: EndReason::CapacityExceeded,
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
        })
    ));
    assert_eq!(group.poll(), Poll::Closed);
}

#[test]
fn source_terminal_preserves_native_rejection_and_structured_failure() {
    let mut group = collection(2, 2);
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
        })
    );
}
