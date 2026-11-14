// vendor/btleplug/src/common/util_source_group_failure_tests.rs

#[test]
fn oversized_future_failure_refuses_before_recipient_install_and_retries() {
    use crate::ubm_peer_notification_ingress::{
        FAILURE_RECOVERY_MAX_CHUNK, IngressRecord, TerminalReason,
    };
    let source = std::sync::Arc::new(NotificationFaults::new(1));
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 4, 16).unwrap();
    let route = crate::ubm_peer_notification_ingress::NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 1,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 2,
    };
    let member = source.register_source_route(&group, route, 9).unwrap();
    let mut owner = source.reserve_future_source_failure(1, 1).unwrap();
    let original = crate::PlatformError::new("winrt", "oversized", "x".repeat(1024 * 1024));
    let returned = match owner.capture(original.clone(), 1024) {
        Err((SourceGroupError::Capacity, error)) => error,
        _ => panic!("oversized capture must refuse"),
    };
    assert!(
        !consumer.0.test_future_failure_installed(),
        "budget refusal installed the recipient before accounting"
    );
    assert_eq!(*returned, original);
    assert_eq!(owner.recipient_count(), Ok(1));
    let captured = owner
        .capture(returned, 2 * 1024 * 1024)
        .unwrap_or_else(|_| panic!("retry refused"));
    captured.release();
    let transfer = consumer.0.peek_transfer().expect("failure head");
    assert!(matches!(transfer.record.as_ref(), IngressRecord::Terminal {
        reason: TerminalReason::SourceFailure { error, .. }, memberships
    } if *error == original && *memberships == vec![(route, member)]));
    let reader = consumer
        .prepare_failure_recovery(&transfer)
        .expect("source reader");
    assert!(!reader.acknowledge());
    let first = reader.read(0, 1024).unwrap();
    assert_eq!(reader.read(0, 1024).unwrap(), first);
    let mut offset = first.len();
    while offset < reader.encoded_len() {
        offset += reader
            .read(offset, FAILURE_RECOVERY_MAX_CHUNK)
            .unwrap()
            .len();
    }
    assert!(reader.acknowledge());
    assert!(!reader.acknowledge());
}

#[test]
fn refused_future_failure_does_not_clone_the_original_message() {
    let source = std::sync::Arc::new(NotificationFaults::new(1));
    source.start_source_generation(1).unwrap();
    source.open_source_group(1, 4, 16).unwrap();
    let mut owner = source.reserve_future_source_failure(1, 1).unwrap();
    let error = crate::PlatformError::new("winrt", "oversized", "x".repeat(4 * 1024 * 1024 + 1));
    let original = error.message.as_ptr();
    let returned = match owner.capture(error, 0) {
        Err((SourceGroupError::Capacity, error)) => error,
        _ => panic!("expected capacity refusal"),
    };
    let retained = owner.refused.as_ref().expect("refused failure");
    assert_eq!(returned.message.as_ptr(), original);
    assert_eq!(
        retained.message.as_ptr(),
        original,
        "refusal cloned the oversized evidence before checking its budget"
    );
    assert!(std::sync::Arc::ptr_eq(&returned, retained));
    assert_eq!(owner.recipient_count(), Ok(1));
    let captured = owner
        .capture(returned, usize::MAX)
        .unwrap_or_else(|_| panic!("retry refused"));
    assert_eq!(captured._error.message.as_ptr(), original);
}

#[test]
fn preowned_terminal_retains_the_original_error_allocation() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, TerminalReason};
    use std::sync::Arc;

    let source = Arc::new(NotificationFaults::new(1));
    source.start_source_generation(1).unwrap();
    let (_, consumer) = source.open_source_group(1, 4, 16).unwrap();
    let mut owner = source.reserve_future_source_failure(1, 1).unwrap();
    let error = Arc::new(crate::PlatformError::new(
        "winrt",
        "oversized",
        "x".repeat(4 * 1024 * 1024 + 1),
    ));
    owner
        .capture(Arc::clone(&error), usize::MAX)
        .unwrap_or_else(|_| panic!("capture refused"))
        .release();
    let transfer = consumer.0.peek_transfer().expect("terminal");
    match transfer.record.as_ref() {
        IngressRecord::Terminal {
            reason: TerminalReason::SourceFailure { error: found, .. },
            ..
        } => {
            assert!(
                Arc::ptr_eq(found.as_arc(), &error),
                "terminal did not reuse the source error"
            );
            assert_eq!(
                found.message.as_ptr(),
                error.message.as_ptr(),
                "terminal cloned the original error"
            );
        }
        other => panic!("expected source failure terminal, got {other:?}"),
    }
    let replay = consumer.0.peek_transfer().expect("terminal replay");
    assert!(
        Arc::ptr_eq(&transfer.record, &replay.record),
        "terminal retry changed identity"
    );
    assert!(consumer.ack_transfer(&transfer));
    assert!(
        !consumer.ack_transfer(&replay),
        "terminal was acknowledged twice"
    );
}

#[test]
fn refused_failure_reaches_source_time_recipient_with_same_allocation() {
    use crate::ubm_peer_notification_ingress::IngressRecord;
    use std::sync::Arc;

    let source = Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (first_group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let (second_group, second_consumer) = source.open_source_group(1, 8, 32).unwrap();
    let route = crate::ubm_peer_notification_ingress::NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 1,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 2,
    };
    let first_member = source
        .register_source_route(&first_group, route, 7)
        .unwrap();
    let second_member = source
        .register_source_route(&second_group, route, 8)
        .unwrap();
    let mut owner = source.reserve_future_source_failure(1, 2).unwrap();
    // A second admitted operation forces the production occurrence path,
    // which must precede the final terminal and preserve its own identity.
    let other = source.reserve_future_source_failure(1, 2).unwrap();
    let error = crate::PlatformError::new("winrt", "oversized", "x".repeat(4 * 1024 * 1024 + 1))
        .with("native", "original metadata");
    let message = error.message.as_ptr();
    let returned = match owner.capture(error, 0) {
        Err((SourceGroupError::Capacity, error)) => error,
        _ => panic!("expected capacity refusal"),
    };
    assert!(
        consumer.0.peek_transfer().is_none(),
        "refused cut became visible before release"
    );
    assert!(
        second_consumer.0.peek_transfer().is_none(),
        "second refused cut became visible before release"
    );
    let captured = owner
        .capture(Arc::clone(&returned), usize::MAX)
        .unwrap_or_else(|_| panic!("retry refused"));
    captured.release();
    for (consumer, member) in [(&consumer, first_member), (&second_consumer, second_member)] {
        let transfer = consumer.0.peek_transfer().expect("failure occurrence");
        match transfer.record.as_ref() {
            IngressRecord::FailureOccurrence { error, memberships } => {
                assert!(
                    Arc::ptr_eq(error.as_arc(), &returned),
                    "recipient received a cloned error"
                );
                assert_eq!(error.message.as_ptr(), message);
                assert_eq!(error.metadata[0].1, "original metadata");
                assert_eq!(memberships, &vec![(route, member)]);
            }
            other => panic!("expected occurrence, got {other:?}"),
        }
        assert!(consumer.ack_transfer(&transfer));
    }
    drop(other);
}

#[test]
fn preowned_failure_terminal_cannot_be_acknowledged_before_other_owner_settles() {
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (_, consumer) = source.open_source_group(1, 4, 16).unwrap();
    let mut first = source.reserve_future_source_failure(1, 2).unwrap();
    let second = source.reserve_future_source_failure(1, 2).unwrap();
    let error = crate::PlatformError::new("winrt", "first", "first failure");
    first
        .capture(error, usize::MAX)
        .unwrap_or_else(|_| panic!("first capture refused"))
        .release();

    // Once acknowledged, a terminal ends the desktop pump. It cannot be
    // offered while an already-admitted operation may still report a failure.
    assert!(
        !matches!(
            consumer
                .0
                .peek_transfer()
                .as_ref()
                .map(|t| t.record.as_ref()),
            Some(crate::ubm_peer_notification_ingress::IngressRecord::Terminal { .. })
        ),
        "terminal became transferable while another admitted owner could still fail"
    );
    drop(second);
}

#[test]
fn replacement_cannot_acknowledge_old_terminal_before_old_owner_settles() {
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (_, old) = source.open_source_group(1, 4, 16).unwrap();
    let owner = source.reserve_future_source_failure(1, 2).unwrap();
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    let (_, replacement) = source.open_source_group(2, 4, 16).unwrap();

    assert!(
        !matches!(
            old.0.peek_transfer().as_ref().map(|t| t.record.as_ref()),
            Some(crate::ubm_peer_notification_ingress::IngressRecord::Terminal { .. })
        ),
        "old terminal became transferable before its operation settled"
    );
    assert!(replacement.0.peek_transfer().is_none());
    drop(owner);
}

#[tokio::test]
async fn settling_old_owner_wakes_the_closed_generation_consumer() {
    use crate::ubm_peer_notification_ingress::IngressRecord;
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (_, old) = source.open_source_group(1, 4, 16).unwrap();
    let owner = source.reserve_future_source_failure(1, 2).unwrap();
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    let waiter = tokio::spawn(async move { old.recv_transfer().await });
    tokio::task::yield_now().await;
    assert!(
        !waiter.is_finished(),
        "terminal overtook the admitted owner"
    );
    drop(owner);
    let transfer = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .expect("consumer was not woken")
        .unwrap()
        .expect("terminal missing");
    assert!(matches!(
        transfer.record.as_ref(),
        IngressRecord::Terminal { .. }
    ));
}

#[test]
fn source_failure_closure_cannot_overtake_admitted_owner() {
    use crate::ubm_peer_notification_ingress::IngressRecord;
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (_, original) = source.open_source_group(1, 4, 16).unwrap();
    let _owner = source.reserve_future_source_failure(1, 2).unwrap();
    source
        .fail_source_generation(
            1,
            crate::PlatformError::new("winrt", "closure", "source failed"),
        )
        .unwrap();
    source.start_source_generation(2).unwrap();
    let (_, replacement) = source.open_source_group(2, 4, 16).unwrap();

    assert!(
        !matches!(
            original
                .0
                .peek_transfer()
                .as_ref()
                .map(|t| t.record.as_ref()),
            Some(IngressRecord::Terminal { .. })
        ),
        "source failure terminal overtook an admitted old operation"
    );
    assert!(replacement.0.peek_transfer().is_none());
}

#[test]
fn source_failure_closure_does_not_revoke_admitted_failure_owner() {
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    source.open_source_group(1, 4, 16).unwrap();
    let mut owner = source.reserve_future_source_failure(1, 2).unwrap();
    source
        .fail_source_generation(
            1,
            crate::PlatformError::new("winrt", "closure", "source failed"),
        )
        .unwrap();
    source.start_source_generation(2).unwrap();
    let error = crate::PlatformError::new("winrt", "operation", "original operation failure")
        .with("native", "original metadata");
    let result = owner.capture(error, usize::MAX);
    assert!(
        result.is_ok(),
        "source failure closure revoked the admitted operation owner"
    );
}

#[test]
fn source_closure_and_old_operation_keep_both_original_failures() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (_, consumer) = source.open_source_group(1, 4, 16).unwrap();
    let mut owner = source.reserve_future_source_failure(1, 2).unwrap();
    let closure = crate::PlatformError::new("winrt", "closure", "source failure")
        .with("native", "closure metadata");
    let operation = crate::PlatformError::new("winrt", "operation", "operation failure")
        .with("native", "operation metadata");
    source.fail_source_generation(1, closure.clone()).unwrap();
    source.start_source_generation(2).unwrap();
    owner
        .capture(operation.clone(), usize::MAX)
        .unwrap_or_else(|_| panic!("old owner refused"))
        .release();
    let occurrence = consumer.0.peek_transfer().expect("operation occurrence");
    assert!(
        matches!(occurrence.record.as_ref(), IngressRecord::FailureOccurrence { error, .. } if *error == operation)
    );
    assert!(consumer.ack_transfer(&occurrence));
    let terminal = consumer.0.peek_transfer().expect("source closure");
    assert!(
        matches!(terminal.record.as_ref(), IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, .. }, .. } if *error == closure)
    );
    assert!(consumer.ack_transfer(&terminal));
}

#[test]
fn concurrent_preowned_failures_do_not_refuse_the_second_error() {
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let mut first = source.reserve_future_source_failure(1, 2).unwrap();
    let mut second = source.reserve_future_source_failure(1, 2).unwrap();
    let first_error = crate::PlatformError::new("winrt", "first", "first failure");
    let second_error = crate::PlatformError::new("winrt", "second", "second failure");
    let captured = first
        .capture(first_error.clone(), usize::MAX)
        .unwrap_or_else(|_| panic!("first capture refused"));
    let later = second.capture(second_error.clone(), usize::MAX);
    assert!(
        later.is_ok(),
        "a previously admitted owner must retain its failure while another cut is held"
    );
    captured.release();
    later.unwrap_or_else(|_| unreachable!()).release();
}

#[test]
fn concurrent_failures_are_distinct_occurrences_before_terminal_and_exact_ack() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (_, consumer) = source.open_source_group(1, 4, 16).unwrap();
    let mut first = source.reserve_future_source_failure(1, 2).unwrap();
    let mut second = source.reserve_future_source_failure(1, 2).unwrap();
    let a = crate::PlatformError::new("winrt", "first", "first").with("native", "a");
    let b = crate::PlatformError::new("winrt", "second", "second").with("native", "b");
    let first_cut = first
        .capture(a.clone(), usize::MAX)
        .unwrap_or_else(|_| panic!("first refused"));
    let second_cut = second
        .capture(b.clone(), usize::MAX)
        .unwrap_or_else(|_| panic!("second refused"));
    first_cut.release();
    second_cut.release();
    let first_transfer = consumer.0.peek_transfer().expect("first failure");
    assert!(
        matches!(first_transfer.record.as_ref(), IngressRecord::FailureOccurrence { error, .. } if *error == a)
    );
    assert!(consumer.ack_transfer(&first_transfer));
    let second_transfer = consumer.0.peek_transfer().expect("second failure");
    assert!(
        matches!(second_transfer.record.as_ref(), IngressRecord::FailureOccurrence { error, .. } if *error == b)
    );
    assert!(!consumer.ack_transfer(&first_transfer));
    assert!(consumer.ack_transfer(&second_transfer));
    assert!(
        matches!(consumer.0.peek_transfer().as_ref().map(|t| t.record.as_ref()),
        Some(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, .. }, .. }) if *error == a)
    );
}

#[test]
fn future_owner_cannot_add_failure_to_already_acknowledged_terminal() {
    use crate::ubm_peer_notification_ingress::IngressRecord;
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 0, 0).unwrap();
    assert!(source.source_group_barrier(&group, 1).is_err());
    let terminal = consumer.0.peek_transfer().expect("overflow terminal");
    assert!(matches!(
        terminal.record.as_ref(),
        IngressRecord::Terminal { .. }
    ));
    let mut owner = source.reserve_future_source_failure(1, 2).unwrap();
    assert_eq!(
        owner.recipient_count(),
        Ok(0),
        "a previously terminated group cannot accept a new owner"
    );
    assert!(consumer.ack_transfer(&terminal));
    let error = crate::PlatformError::new("winrt", "operation", "failure");
    owner
        .capture(error, usize::MAX)
        .unwrap_or_else(|_| panic!("capture refused"))
        .release();
    assert!(
        consumer.0.peek_transfer().is_none(),
        "failure was queued after acknowledged terminal"
    );
}

#[test]
fn preowned_failure_can_be_captured_after_generation_replacement() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (_, original) = source.open_source_group(1, 4, 16).unwrap();
    let mut old = source.reserve_future_source_failure(1, 2).unwrap();
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    let (_, replacement) = source.open_source_group(2, 4, 16).unwrap();
    let error = crate::PlatformError::new("winrt", "old", "old operation failed");
    let captured = old.capture(error.clone(), usize::MAX);
    assert!(
        captured.is_ok(),
        "replacement must not revoke an admitted old operation's failure owner"
    );
    captured.unwrap_or_else(|_| unreachable!()).release();
    assert!(
        matches!(original.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, .. }) if found == error)
    );
    assert_eq!(replacement.poll(), Poll::Empty);
}

#[test]
fn observed_failure_mandatory_cut_follows_full_gate_and_includes_late_group() {
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    use crate::ubm_reclaim_authority::GraphKey;
    use crate::ubm_reclaim_graph_gate::{Admission, DrainItem, GraphGate, MandatoryCut};

    let source = std::sync::Arc::new(NotificationFaults::new(4));
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let (first, first_consumer) = source.open_source_group(1, 8, 32).unwrap();
    let first_member = source
        .register_source_route_for_activation(&first, route, 1, 10)
        .unwrap();
    let prefix = source
        .reserve_source_value_for_activation_bounded(1, route, 10, &note(1, false), usize::MAX)
        .unwrap();

    let key = GraphKey {
        authority_id: 0,
        peer_id: String::new(),
        connection_generation: String::new(),
        graph: (String::new(), 0, 0),
        permit_id: 1,
    };
    let mut gate: GraphGate<Box<dyn FnOnce() + Send>> = GraphGate::new(1, 1);
    gate.acquire(key.clone()).unwrap();
    gate.commit(&key).unwrap();
    assert!(matches!(
        gate.admit(Box::new(move || prefix.release()), 1, false),
        Admission::Deferred
    ));
    let (second, second_consumer) = source.open_source_group(1, 8, 32).unwrap();
    let second_member = source
        .register_source_route_for_activation(&second, route, 2, 10)
        .unwrap();
    let error = crate::PlatformError::new("winrt", "hresult", "original failure")
        .with("hresult", "0x80004005");
    // This owner captures an already observed error with an unbounded caller
    // budget. It does not prove pre-native admission or recipient budgeting.
    let failure = source
        .owned_pending_source_failure(1, route, 10, error.clone(), usize::MAX)
        .unwrap();
    assert!(!gate.admit_mandatory_cut(MandatoryCut::new(Box::new(move || failure.release()))));
    assert_eq!(first_consumer.poll(), Poll::Empty);
    assert_eq!(second_consumer.poll(), Poll::Empty);
    gate.release(&key).unwrap();
    match gate.next().unwrap() {
        DrainItem::Event(callback) => callback(),
        _ => panic!("missing prefix"),
    }
    assert!(
        matches!(first_consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == first_member)
    );
    assert_eq!(second_consumer.poll(), Poll::Empty);
    match gate.next().unwrap() {
        DrainItem::Event(callback) => callback(),
        _ => panic!("missing failure cut"),
    }
    for (consumer, member) in [
        (first_consumer, first_member),
        (second_consumer, second_member),
    ] {
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::SourceFailure { error: found, .. }, memberships
        }) if found == error && memberships == vec![(route, member)])
        );
    }
    assert!(matches!(gate.next().unwrap(), DrainItem::Finished));
}

#[test]
fn callback_and_retirement_share_the_source_lock() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 8).unwrap();
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    let member = source.register_source_route(&group, route, 1).unwrap();
    let characteristic = Characteristic {
        uuid: Uuid::nil(),
        instance: 1,
        service_uuid: Uuid::nil(),
        service_instance: 7,
        properties: Default::default(),
        descriptors: Default::default(),
    };
    let epoch = source.begin(&characteristic).unwrap();
    let (sender, _) = tokio::sync::broadcast::channel(4);
    let guard = source.state.lock().unwrap();
    let other = source.clone();
    let worker =
        std::thread::spawn(move || other.publish_for_epoch(&sender, note(1, false), epoch));
    // The consumer takes only the ingress lock while the source is held.
    assert_eq!(consumer.poll(), Poll::Empty);
    drop(guard);
    source.retire_source_route(&group, route, member).unwrap();
    worker.join().unwrap();
    let first = consumer.poll();
    match first {
        Poll::Record(IngressRecord::Value { membership, .. }) => {
            assert_eq!(membership, member);
            assert_eq!(
                consumer.poll(),
                Poll::Record(IngressRecord::Fence { membership: member })
            );
        }
        Poll::Record(IngressRecord::Fence { membership }) => assert_eq!(membership, member),
        other => panic!("unexpected record: {other:?}"),
    }
    assert_eq!(consumer.poll(), Poll::Empty);
}
#[test]
fn late_groups_observe_only_their_retained_route_failure() {
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    let source = NotificationFaults::new(4);
    source.start_source_generation(1).unwrap();
    let (sender, _) = tokio::sync::broadcast::channel(8);
    let route = |instance| NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: instance,
    };
    let characteristic = |instance| Characteristic {
        uuid: Uuid::nil(),
        instance,
        service_uuid: Uuid::nil(),
        service_instance: 7,
        properties: Default::default(),
        descriptors: Default::default(),
    };
    let epoch = source.begin(&characteristic(1)).unwrap();
    source.publish_for_epoch(&sender, note(1, true), epoch);
    // Failure preceded group creation; attachment to an unrelated route stays live.
    let (healthy, healthy_rx) = source.open_source_group(1, 8, 32).unwrap();
    source.register_source_route(&healthy, route(2), 1).unwrap();
    assert_eq!(healthy_rx.poll(), Poll::Empty);
    // A group may be opened before it chooses the failed route.
    let (late, late_rx) = source.open_source_group(1, 8, 32).unwrap();
    let member = source.register_source_route(&late, route(1), 2).unwrap();
    assert!(
        matches!(late_rx.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(found), .. }, .. }) if found == member)
    );
    source.clear_all();
    assert!(matches!(
        healthy_rx.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Closed {
                tail_uncertain: true
            },
            ..
        })
    ));
    source.start_source_generation(2).unwrap();
    let (fresh, fresh_rx) = source.open_source_group(2, 8, 32).unwrap();
    source.register_source_route(&fresh, route(1), 3).unwrap();
    assert_eq!(fresh_rx.poll(), Poll::Empty);
}
#[test]
fn groups_reject_handles_from_a_different_source() {
    use crate::ubm_peer_notification_ingress::NotificationRoute;
    let first = NotificationFaults::new(2);
    let second = NotificationFaults::new(2);
    first.start_source_generation(1).unwrap();
    second.start_source_generation(1).unwrap();
    let (a, _) = first.open_source_group(1, 4, 32).unwrap();
    let (b, _) = second.open_source_group(1, 4, 32).unwrap();
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    let member = first.register_source_route(&a, route, 1).unwrap();
    assert_eq!(
        second.register_source_route(&a, route, 2),
        Err(SourceGroupError::UnknownGroup)
    );
    assert_eq!(
        second.retire_source_route(&a, route, member),
        Err(SourceGroupError::UnknownGroup)
    );
    assert_eq!(
        second.detach_source_group(&a),
        Err(SourceGroupError::UnknownGroup)
    );
    assert!(second.register_source_route(&b, route, 2).is_ok());
}

#[test]
fn unrelated_callback_failure_and_broadcast_quota_do_not_poison_other_group() {
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    let source = NotificationFaults::new(2);
    source.start_source_generation(1).unwrap();
    let (sender, _) = tokio::sync::broadcast::channel(4);
    let route = |instance| NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: instance,
    };
    let characteristic = |instance| Characteristic {
        uuid: Uuid::nil(),
        instance,
        service_uuid: Uuid::nil(),
        service_instance: 7,
        properties: Default::default(),
        descriptors: Default::default(),
    };
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let member = source.register_source_route(&group, route(2), 2).unwrap();
    let one = source.begin(&characteristic(1)).unwrap();
    let two = source.begin(&characteristic(2)).unwrap();
    // Exhaust broadcast retention with unrelated legacy faults; direct source
    // admission and its independent retained state must remain usable.
    source.publish(&sender, note(8, true));
    source.publish(&sender, note(9, true));
    source.publish(&sender, note(10, true));
    source.publish_for_epoch(&sender, note(1, true), one);
    source.publish_for_epoch(&sender, note(2, false), two);
    assert!(
        matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member)
    );
    assert_eq!(consumer.poll(), Poll::Empty);
    let (late, late_rx) = source.open_source_group(1, 8, 32).unwrap();
    let failed = source.register_source_route(&late, route(1), 1).unwrap();
    assert!(
        matches!(late_rx.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(membership), .. }, .. }) if membership == failed)
    );
}
#[test]
fn combined_peer_loss_and_route_failure_are_scoped_and_not_replayed_as_loss() {
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    let source = NotificationFaults::new(3);
    source.start_source_generation(1).unwrap();
    let (sender, _) = tokio::sync::broadcast::channel(4);
    let route = |instance| NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: instance,
    };
    let characteristic = Characteristic {
        uuid: Uuid::nil(),
        instance: 1,
        service_uuid: Uuid::nil(),
        service_instance: 7,
        properties: Default::default(),
        descriptors: Default::default(),
    };
    let (affected, a_rx) = source.open_source_group(1, 8, 32).unwrap();
    let (other, b_rx) = source.open_source_group(1, 8, 32).unwrap();
    let member = source
        .register_source_route(&affected, route(1), 1)
        .unwrap();
    source.register_source_route(&other, route(2), 2).unwrap();
    let epoch = source.begin(&characteristic).unwrap();
    let mut combined = note(1, true);
    combined.lost_before = 3;
    source.publish_for_epoch(&sender, combined, epoch);
    assert!(matches!(
        a_rx.poll(),
        Poll::Record(IngressRecord::Loss {
            skipped_records: 3,
            ..
        })
    ));
    assert!(
        matches!(a_rx.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(found), .. }, .. }) if found == member)
    );
    assert!(matches!(
        b_rx.poll(),
        Poll::Record(IngressRecord::Loss {
            skipped_records: 3,
            ..
        })
    ));
    assert_eq!(b_rx.poll(), Poll::Empty);
    let (late, late_rx) = source.open_source_group(1, 8, 32).unwrap();
    let late_member = source.register_source_route(&late, route(1), 3).unwrap();
    assert!(
        matches!(late_rx.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(found), .. }, .. }) if found == late_member)
    );
}
#[test]
fn source_failure_terminates_all_and_preserves_prior_terminal() {
    use crate::api::NotificationEvent;
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    let source = NotificationFaults::new(2);
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 4, 32).unwrap();
    let (full, full_rx) = source.open_source_group(1, 0, 0).unwrap();
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.register_source_route(&group, route, 1).unwrap();
    source.register_source_route(&full, route, 2).unwrap();
    source
        .publish_source_event(1, NotificationEvent::Value(note(1, false)))
        .unwrap();
    let error = crate::PlatformError::new("test", "failure", "source failed");
    source.fail_source_generation(1, error.clone()).unwrap();
    assert!(matches!(
        consumer.poll(),
        Poll::Record(IngressRecord::Value { .. })
    ));
    assert!(matches!(
        consumer.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::SourceFailure {
                membership: None,
                error: found,
                rejected_loss: None,
                tail_uncertain: true
            },
            ..
        }) if found == error
    ));
    assert!(matches!(
        full_rx.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Overflow { .. },
            ..
        })
    ));
    assert!(matches!(
        source.fail_source_generation(1, crate::PlatformError::new("test", "other", "failed")),
        Err(SourceGroupError::StaleGeneration)
    ));
}
#[test]
fn source_event_provenance_reaches_group() {
    use crate::api::NotificationEvent;
    use crate::ubm_peer_notification_ingress::{
        CallbackProvenance, IngressRecord, NotificationRoute, Poll,
    };
    let source = NotificationFaults::new(2);
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.register_source_route(&group, route, 1).unwrap();
    source
        .publish_source_event_with_provenance(
            1,
            NotificationEvent::Value(note(1, false)),
            CallbackProvenance::ReadOrNotification,
        )
        .unwrap();
    source
        .publish_source_event(1, NotificationEvent::Value(note(1, false)))
        .unwrap();
    assert!(matches!(
        consumer.poll(),
        Poll::Record(IngressRecord::Value {
            provenance: CallbackProvenance::ReadOrNotification,
            ..
        })
    ));
    assert!(matches!(
        consumer.poll(),
        Poll::Record(IngressRecord::Value {
            provenance: CallbackProvenance::Unknown,
            ..
        })
    ));
}
#[test]
fn direct_source_events_order_fences_and_close_generations() {
    use crate::api::NotificationEvent;
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    let source = NotificationFaults::new(2);
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    let old = source.register_source_route(&group, route, 1).unwrap();
    source
        .publish_source_event(1, NotificationEvent::Value(note(1, false)))
        .unwrap();
    source
        .publish_source_event(1, NotificationEvent::Lagged { skipped_records: 2 })
        .unwrap();
    source.retire_source_route(&group, route, old).unwrap();
    let fresh = source.register_source_route(&group, route, 1).unwrap();
    source
        .publish_source_event(1, NotificationEvent::Value(note(1, false)))
        .unwrap();
    assert!(
        matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == old)
    );
    assert!(matches!(
        consumer.poll(),
        Poll::Record(IngressRecord::Loss {
            skipped_records: 2,
            ..
        })
    ));
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Fence { membership: old })
    );
    assert!(
        matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == fresh)
    );
    source.close_source_generation(1).unwrap();
    assert!(matches!(
        consumer.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Closed {
                tail_uncertain: true
            },
            ..
        })
    ));
    assert_eq!(
        source.publish_source_event(1, NotificationEvent::Value(note(1, false))),
        Err(SourceGroupError::StaleGeneration)
    );
    source.start_source_generation(2).unwrap();
    assert_eq!(
        source.close_source_generation(1),
        Err(SourceGroupError::StaleGeneration)
    );
}

#[test]
fn direct_source_fault_capacity_makes_new_unknown_routes_explicitly_unavailable() {
    use crate::api::NotificationEvent;
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    let source = NotificationFaults::new(1);
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let route = |instance| NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: instance,
    };
    source
        .publish_source_event(1, NotificationEvent::Value(note(1, true)))
        .unwrap();
    source
        .publish_source_event(1, NotificationEvent::Value(note(2, true)))
        .unwrap();
    assert_eq!(
        source.register_source_route(&group, route(3), 3),
        Err(SourceGroupError::SourceStateUnavailable)
    );
    let member = source.register_source_route(&group, route(1), 1).unwrap();
    assert!(
        matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(m), .. }, .. }) if m == member)
    );
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    let (fresh, _) = source.open_source_group(2, 8, 32).unwrap();
    assert!(source.register_source_route(&fresh, route(3), 3).is_ok());
}
#[test]
fn closing_generation_rejects_old_native_callback_after_reconnect() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
    let source = NotificationFaults::new(2);
    let (sender, mut broadcast) = tokio::sync::broadcast::channel(4);
    let characteristic = Characteristic {
        uuid: Uuid::nil(),
        instance: 1,
        service_uuid: Uuid::nil(),
        service_instance: 7,
        properties: Default::default(),
        descriptors: Default::default(),
    };
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.start_source_generation(1).unwrap();
    let old = source.begin(&characteristic).unwrap();
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    let (group, consumer) = source.open_source_group(2, 8, 32).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    source.publish_for_epoch(&sender, note(1, false), old);
    assert_eq!(consumer.poll(), Poll::Empty);
    assert!(broadcast.try_recv().is_err());
    let new = source.begin(&characteristic).unwrap();
    source.publish_for_epoch(&sender, note(1, false), old);
    assert_eq!(consumer.poll(), Poll::Empty);
    source.publish_for_epoch(&sender, note(1, false), new);
    assert!(
        matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member)
    );
    assert_eq!(broadcast.try_recv().unwrap().notification.instance, 1);
    assert!(broadcast.try_recv().is_err());
}

#[test]
fn future_failure_slots_follow_group_admission_and_retirement() {
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    source.start_source_generation(1).unwrap();
    let (first, _) = source.open_source_group(1, 8, 32).unwrap();
    let owner = source.reserve_future_source_failure(1, 2).unwrap();
    assert_eq!(owner.recipient_count(), Ok(1));
    let (second, _) = source.open_source_group(1, 8, 32).unwrap();
    assert_eq!(owner.recipient_count(), Ok(2));
    assert!(matches!(
        source.open_source_group(1, 8, 32),
        Err(SourceGroupError::Capacity)
    ));
    assert_eq!(owner.recipient_count(), Ok(2));
    source.detach_source_group(&first).unwrap();
    assert_eq!(owner.recipient_count(), Ok(2));
    assert!(matches!(
        source.open_source_group(1, 8, 32),
        Err(SourceGroupError::Capacity)
    ));
    source.detach_source_group(&second).unwrap();
    drop(owner);
    assert!(source.open_source_group(1, 8, 32).is_ok());
}

#[test]
fn future_failure_owner_is_fenced_from_replacement_generation() {
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let owner = source.reserve_future_source_failure(1, 1).unwrap();
    source.open_source_group(1, 8, 32).unwrap();
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    assert_eq!(
        owner.recipient_count(),
        Err(SourceGroupError::StaleGeneration)
    );
    assert!(source.open_source_group(2, 8, 32).is_ok());
    drop(owner);
    assert!(source.open_source_group(2, 8, 32).is_ok());
}

#[test]
fn future_failure_refusal_retains_owner_and_late_recipients_receive_exact_failure_after_prefix() {
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    use crate::ubm_reclaim_authority::GraphKey;
    use crate::ubm_reclaim_graph_gate::{Admission, DrainItem, GraphGate, MandatoryCut};
    let source = std::sync::Arc::new(NotificationFaults::new(3));
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.start_source_generation(1).unwrap();
    let (first, first_rx) = source.open_source_group(1, 8, 32).unwrap();
    let member = source.register_source_route(&first, route, 1).unwrap();
    let mut owner = source.reserve_future_source_failure(1, 2).unwrap();
    let prefix = source
        .reserve_source_value(
            1,
            &note(1, false),
            crate::ubm_peer_notification_ingress::CallbackProvenance::Unknown,
        )
        .unwrap();
    let key = GraphKey {
        authority_id: 0,
        peer_id: String::new(),
        connection_generation: String::new(),
        graph: (String::new(), 0, 0),
        permit_id: 1,
    };
    let mut gate: GraphGate<Box<dyn FnOnce() + Send>> = GraphGate::new(1, 1);
    gate.acquire(key.clone()).unwrap();
    gate.commit(&key).unwrap();
    assert!(matches!(
        gate.admit(Box::new(move || prefix.release()), 1, false),
        Admission::Deferred
    ));
    let (second, second_rx) = source.open_source_group(1, 8, 32).unwrap();
    source.register_source_route(&second, route, 2).unwrap();
    let error =
        crate::PlatformError::new("winrt", "hresult", "original").with("hresult", "0x80004005");
    let error = match owner.capture(error, 0) {
        Err((SourceGroupError::Capacity, error)) => error,
        _ => panic!("expected refusal retaining the original error"),
    };
    assert_eq!(owner.recipient_count(), Ok(2));
    assert_eq!(first_rx.poll(), Poll::Empty);
    assert_eq!(second_rx.poll(), Poll::Empty);
    let failure = match owner.capture(error.clone(), usize::MAX) {
        Ok(failure) => failure,
        Err(_) => panic!("reserved capture should succeed"),
    };
    assert_eq!(first_rx.poll(), Poll::Empty);
    assert_eq!(second_rx.poll(), Poll::Empty);
    assert!(!gate.admit_mandatory_cut(MandatoryCut::new(Box::new(move || failure.release()))));
    gate.release(&key).unwrap();
    match gate.next().unwrap() {
        DrainItem::Event(callback) => callback(),
        _ => panic!("missing prefix"),
    }
    assert!(
        matches!(first_rx.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member)
    );
    assert_eq!(first_rx.poll(), Poll::Empty);
    match gate.next().unwrap() {
        DrainItem::Event(callback) => callback(),
        _ => panic!("missing failure"),
    }
    for consumer in [first_rx, second_rx] {
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::SourceFailure { error: found, .. }, ..
        }) if found == *error)
        );
    }
}

#[test]
fn future_failure_refusal_and_retired_route_do_not_drop_other_owners() {
    use crate::ubm_peer_notification_ingress::NotificationRoute;
    let source = std::sync::Arc::new(NotificationFaults::new(3));
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.start_source_generation(1).unwrap();
    let (group, _) = source.open_source_group(1, 8, 32).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    let first = source.reserve_future_source_failure(1, 1).unwrap();
    let second = source.reserve_future_source_failure(1, 2).unwrap();
    source.retire_source_route(&group, route, member).unwrap();
    assert_eq!(first.recipient_count(), Ok(1));
    assert_eq!(second.recipient_count(), Ok(1));
    assert!(matches!(
        source.open_source_group(1, 8, 32),
        Err(SourceGroupError::Capacity)
    ));
    assert_eq!(second.recipient_count(), Ok(1));
    drop(first);
    assert!(source.open_source_group(1, 8, 32).is_ok());
    assert_eq!(second.recipient_count(), Ok(2));
}

#[test]
fn old_future_failure_retains_original_error_without_affecting_replacement() {
    use crate::ubm_peer_notification_ingress::Poll;
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let mut owner = source.reserve_future_source_failure(1, 1).unwrap();
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    let (_, replacement) = source.open_source_group(2, 8, 32).unwrap();
    let error = crate::PlatformError::new("winrt", "hresult", "old").with("hresult", "0x80004005");
    let returned = match owner.capture(error.clone(), 0) {
        Err((SourceGroupError::Capacity, returned)) => returned,
        _ => panic!("bounded capture must return its exact error"),
    };
    assert_eq!(*returned, error);
    let captured = owner
        .capture(returned, usize::MAX)
        .unwrap_or_else(|_| panic!("old owner must retain its failure"));
    assert_eq!(*captured._error, error);
    captured.release();
    drop(owner);
    assert_eq!(replacement.poll(), Poll::Empty);
}

#[test]
fn future_failure_retry_preserves_recipient_detached_after_capacity_refusal() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let mut owner = source.reserve_future_source_failure(1, 2).unwrap();
    let error = crate::PlatformError::new("winrt", "hresult", "original failure");
    let error = match owner.capture(error, 0) {
        Err((SourceGroupError::Capacity, error)) => error,
        _ => panic!("expected bounded capture refusal"),
    };
    match source.detach_source_group(&group) {
        Ok(()) => {}
        Err(SourceGroupError::SourceStateUnavailable) => {}
        Err(other) => panic!("unexpected detach refusal: {other:?}"),
    }
    let captured = owner
        .capture(error.clone(), usize::MAX)
        .unwrap_or_else(|_| panic!("retry failed"));
    captured.release();
    assert!(
        matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal {
        reason: TerminalReason::SourceFailure { error: found, .. }, ..
    }) if found == *error),
        "detachment must not replace the observed failure"
    );
}

#[test]
fn future_failure_refusal_survives_generation_replacement_and_rejects_different_retry() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (_, old) = source.open_source_group(1, 8, 32).unwrap();
    let mut owner = source.reserve_future_source_failure(1, 2).unwrap();
    let original =
        crate::PlatformError::new("winrt", "hresult", "original").with("hresult", "0x80004005");
    assert!(
        matches!(owner.capture(original.clone(), 0), Err((SourceGroupError::Capacity, e)) if *e == original)
    );
    assert!(matches!(
        source.open_source_group(1, 8, 32),
        Err(SourceGroupError::StaleGeneration)
    ));
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    let (_, replacement) = source.open_source_group(2, 8, 32).unwrap();
    let other = crate::PlatformError::new("winrt", "hresult", "different");
    assert!(
        matches!(owner.capture(other.clone(), usize::MAX), Err((SourceGroupError::TransactionConflict, e)) if *e == other)
    );
    let captured = owner
        .capture(original.clone(), usize::MAX)
        .unwrap_or_else(|_| panic!("retry failed"));
    assert_eq!(old.poll(), Poll::Empty);
    captured.release();
    assert!(
        matches!(old.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, .. }, .. }) if error == original)
    );
    assert_eq!(replacement.poll(), Poll::Empty);
}

#[test]
fn refused_future_failure_drop_settles_cut_and_releases_owner_quota() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(1));
    source.start_source_generation(1).unwrap();
    let (_, old) = source.open_source_group(1, 8, 32).unwrap();
    let mut owner = source.reserve_future_source_failure(1, 1).unwrap();
    let error = crate::PlatformError::new("winrt", "failure", "retained");
    assert!(matches!(
        owner.capture(error.clone(), 0),
        Err((SourceGroupError::Capacity, _))
    ));
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    assert!(matches!(
        source.reserve_future_source_failure(2, 1),
        Err(SourceGroupError::Capacity)
    ));
    drop(owner);
    assert!(
        matches!(old.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, .. }) if found == error)
    );
    assert!(source.reserve_future_source_failure(2, 1).is_ok());
}

#[test]
fn refused_future_failure_transfer_keeps_original_and_memberships() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(1));
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 1,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 2,
    };
    let member = source.register_source_route(&group, route, 9).unwrap();
    let mut owner = source.reserve_future_source_failure(1, 1).unwrap();
    let error =
        crate::PlatformError::new("winrt", "hresult", "original").with("hresult", "0x80004005");
    for _ in 0..3 {
        assert!(
            matches!(owner.capture(error.clone(), 0), Err((SourceGroupError::Capacity, e)) if *e == error)
        );
    }
    let captured = owner
        .capture(error.clone(), usize::MAX)
        .unwrap_or_else(|_| panic!("retry failed"));
    assert!(consumer.0.peek_transfer().is_none());
    captured.release();
    let transfer = consumer.0.peek_transfer().expect("failure transfer");
    assert!(matches!(transfer.record.as_ref(), IngressRecord::Terminal {
        reason: TerminalReason::SourceFailure { error: found, .. }, memberships
    } if *found == error && *memberships == vec![(route, member)]));
    assert!(consumer.ack_transfer(&transfer));
}

#[test]
fn preowned_retirement_failure_survives_full_graph_queue() {
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    use crate::ubm_reclaim_authority::GraphKey;
    use crate::ubm_reclaim_graph_gate::{Admission, DrainItem, GraphGate};
    let source = std::sync::Arc::new(NotificationFaults::new(3));
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 2,
    };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 1).unwrap();
    let (group, consumer) = source.open_source_group(1, 4, 16).unwrap();
    let member = source
        .register_source_route_for_activation(&group, route, 1, 1)
        .unwrap();
    let failure = source
        .reserve_future_failure_cut(1, |captured| captured.release())
        .unwrap();
    let key = GraphKey {
        authority_id: 0,
        peer_id: String::new(),
        connection_generation: String::new(),
        graph: (String::new(), 0, 0),
        permit_id: 1,
    };
    let mut gate: GraphGate<Box<dyn FnOnce() + Send>> = GraphGate::new(1, 1);
    gate.acquire(key.clone()).unwrap();
    let prefix = source
        .reserve_source_value_for_activation_bounded(1, route, 1, &note(2, false), usize::MAX)
        .unwrap();
    assert!(matches!(
        gate.admit(Box::new(move || prefix.release()), 1, false),
        Admission::Deferred
    ));
    let (late, late_consumer) = source.open_source_group(1, 4, 16).unwrap();
    let late_member = source
        .register_source_route_for_activation(&late, route, 2, 1)
        .unwrap();
    let error = crate::PlatformError::new("winrt", "hresult", "original failure")
        .with("hresult", "0x80004005");
    assert!(
        !failure
            .capture_and_enqueue(&mut gate, error.clone())
            .unwrap()
    );
    source.detach_source_group(&late).unwrap();
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    let (_, replacement) = source.open_source_group(2, 4, 16).unwrap();
    gate.release(&key).unwrap();
    if let DrainItem::Event(callback) = gate.next().unwrap() {
        callback();
    } else {
        panic!("missing prefix");
    }
    assert!(
        matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member)
    );
    if let DrainItem::Event(callback) = gate.next().unwrap() {
        callback();
    } else {
        panic!("missing failure");
    }
    assert!(
        matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, .. }) if found == error)
    );
    assert!(
        matches!(late_consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, memberships }) if found == error && memberships == vec![(route, late_member)])
    );
    assert_eq!(replacement.poll(), Poll::Empty);
}

#[test]
fn preowned_cut_retains_more_than_four_megabytes_outside_callback_lane() {
    use crate::ubm_peer_notification_ingress::{
        FAILURE_RECOVERY_MAX_CHUNK, IngressRecord, NotificationRoute, TerminalReason,
    };
    use crate::ubm_reclaim_graph_gate::{DrainItem, GraphGate};
    use futures::FutureExt;

    let source = std::sync::Arc::new(NotificationFaults::new(1));
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 4, 16).unwrap();
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 1,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 2,
    };
    let member = source.register_source_route(&group, route, 9).unwrap();
    let cut = source
        .reserve_future_failure_cut(1, |captured| captured.release())
        .unwrap();
    // An exhausted ordinary callback lane cannot reject the separately owned,
    // source-recoverable failure cut.
    let mut gate: GraphGate<Box<dyn FnOnce() + Send>> = GraphGate::new(0, 0);
    let error = crate::PlatformError::new("winrt", "oversized", "x".repeat(4 * 1024 * 1024 + 1));
    assert!(cut.capture_and_enqueue(&mut gate, error.clone()).unwrap());
    match gate.next().unwrap() {
        DrainItem::Event(callback) => callback(),
        _ => panic!("missing mandatory source cut"),
    }
    let transfer = consumer
        .recv_transfer()
        .now_or_never()
        .flatten()
        .expect("failure terminal");
    assert!(matches!(transfer.record.as_ref(), IngressRecord::Terminal {
        reason: TerminalReason::SourceFailure { error: found, .. }, memberships
    } if *found == error && *memberships == vec![(route, member)]));
    let recovery = consumer
        .prepare_failure_recovery(&transfer)
        .expect("source recovery");
    let mut offset = 0;
    while offset < recovery.encoded_len() {
        offset += recovery
            .read(offset, FAILURE_RECOVERY_MAX_CHUNK)
            .expect("recovery page")
            .len();
    }
    assert!(recovery.acknowledge());
}

#[test]
fn durable_recovery_retirement_is_the_only_source_completion_signal() {
    use crate::ubm_peer_notification_ingress::{FAILURE_RECOVERY_MAX_CHUNK, NotificationRoute};
    use crate::ubm_reclaim_graph_gate::{DrainItem, GraphGate};
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let source = std::sync::Arc::new(NotificationFaults::new(1));
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 4, 16).unwrap();
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 1,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 2,
    };
    source.register_source_route(&group, route, 9).unwrap();
    let completed = std::sync::Arc::new(AtomicUsize::new(0));
    let completion = completed.clone();
    let cut = source
        .reserve_future_failure_cut_with_finalizer(
            1,
            |captured| captured.defer_release(),
            move || {
                completion.fetch_add(1, Ordering::AcqRel);
            },
        )
        .unwrap();
    let mut gate: GraphGate<Box<dyn FnOnce() + Send>> = GraphGate::new(0, 0);
    assert!(
        cut.capture_and_enqueue(
            &mut gate,
            crate::PlatformError::new("winrt", "retained", "exact failure"),
        )
        .unwrap()
    );
    match gate.next().unwrap() {
        DrainItem::Event(callback) => callback(),
        _ => panic!("missing mandatory source cut"),
    }
    let transfer = consumer
        .recv_transfer()
        .now_or_never()
        .flatten()
        .expect("failure terminal");
    let recovery = consumer
        .prepare_failure_recovery(&transfer)
        .expect("source recovery");
    let mut offset = 0;
    while offset < recovery.encoded_len() {
        offset += recovery
            .read(offset, FAILURE_RECOVERY_MAX_CHUNK)
            .expect("recovery page")
            .len();
    }
    assert!(recovery.begin_ack());
    assert_eq!(completed.load(Ordering::Acquire), 0);
    assert!(source.start_source_generation(2).is_err());
    assert!(recovery.retire_ack());
    assert_eq!(completed.load(Ordering::Acquire), 1);
    source.start_source_generation(2).unwrap();
}

#[test]
fn source_completion_waits_for_every_recovery_recipient() {
    use crate::ubm_peer_notification_ingress::{FAILURE_RECOVERY_MAX_CHUNK, NotificationRoute};
    use crate::ubm_reclaim_graph_gate::{DrainItem, GraphGate};
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (first_group, first_consumer) = source.open_source_group(1, 4, 16).unwrap();
    let (second_group, second_consumer) = source.open_source_group(1, 4, 16).unwrap();
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 1,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 2,
    };
    source
        .register_source_route(&first_group, route, 1)
        .unwrap();
    source
        .register_source_route(&second_group, route, 2)
        .unwrap();
    let completed = std::sync::Arc::new(AtomicUsize::new(0));
    let completion = completed.clone();
    let cut = source
        .reserve_future_failure_cut_with_finalizer(
            1,
            |captured| captured.defer_release(),
            move || {
                completion.fetch_add(1, Ordering::AcqRel);
            },
        )
        .unwrap();
    let mut gate: GraphGate<Box<dyn FnOnce() + Send>> = GraphGate::new(0, 0);
    assert!(
        cut.capture_and_enqueue(
            &mut gate,
            crate::PlatformError::new("winrt", "retained", "exact failure"),
        )
        .unwrap()
    );
    match gate.next().unwrap() {
        DrainItem::Event(callback) => callback(),
        _ => panic!("missing mandatory source cut"),
    }
    let retire = |consumer: &crate::common::util::SourceGroupConsumer| {
        let transfer = consumer
            .recv_transfer()
            .now_or_never()
            .flatten()
            .expect("failure terminal");
        let recovery = consumer
            .prepare_failure_recovery(&transfer)
            .expect("source recovery");
        let mut offset = 0;
        while offset < recovery.encoded_len() {
            offset += recovery
                .read(offset, FAILURE_RECOVERY_MAX_CHUNK)
                .expect("recovery page")
                .len();
        }
        assert!(recovery.begin_ack());
        recovery
    };
    let first = retire(&first_consumer);
    let second = retire(&second_consumer);
    assert!(first.retire_ack());
    assert_eq!(completed.load(Ordering::Acquire), 0);
    assert!(source.start_source_generation(2).is_err());
    assert!(second.retire_ack());
    assert_eq!(completed.load(Ordering::Acquire), 1);
    source.start_source_generation(2).unwrap();
}

#[test]
fn configured_evidence_store_globally_bounds_raw_callbacks_across_sources() {
    use crate::ubm_peer_notification_ingress::FileFailureEvidenceStore;

    let directory = std::env::temp_dir().join(format!(
        "ubm-source-failure-pending-{}-{}",
        std::process::id(),
        std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let store = std::sync::Arc::new(
        FileFailureEvidenceStore::open_with_limits(&directory, 1024, 1).expect("configured store"),
    );
    let first = std::sync::Arc::new(NotificationFaults::new_with_failure_evidence_store(
        2,
        Some(store.clone()),
    ));
    let second = std::sync::Arc::new(NotificationFaults::new_with_failure_evidence_store(
        2,
        Some(store),
    ));
    first.start_source_generation(1).unwrap();
    second.start_source_generation(1).unwrap();

    let held = first
        .reserve_future_source_failure(1, 2)
        .expect("first raw callback reservation");
    assert!(
        matches!(
            second.reserve_future_source_failure(1, 2),
            Err(SourceGroupError::Capacity)
        ),
        "a shared store must fence a second raw native callback before it exists"
    );
    drop(held);
    assert!(
        second.reserve_future_source_failure(1, 2).is_ok(),
        "abandoning the first operation must return global raw capacity"
    );
    std::fs::remove_dir(&directory).expect("remove test directory");
}

#[test]
fn configured_evidence_store_releases_large_native_error_after_exact_recovery_ack() {
    use crate::ubm_peer_notification_ingress::{
        FAILURE_RECOVERY_MAX_CHUNK, FileFailureEvidenceStore,
    };
    use crate::ubm_reclaim_graph_gate::{DrainItem, GraphGate};
    use futures::FutureExt;

    let directory = std::env::temp_dir().join(format!(
        "ubm-source-failure-spill-{}-{}",
        std::process::id(),
        std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let store = std::sync::Arc::new(
        FileFailureEvidenceStore::open(&directory, 8 * 1024 * 1024).expect("configured store"),
    );
    let source = std::sync::Arc::new(NotificationFaults::new_with_failure_evidence_store(
        1,
        Some(store),
    ));
    source.start_source_generation(1).unwrap();
    let (_group, consumer) = source.open_source_group(1, 4, 16).unwrap();
    let cut = source
        .reserve_future_failure_cut_with_finalizer(1, |captured| captured.defer_release(), || {})
        .unwrap();
    let mut gate: GraphGate<Box<dyn FnOnce() + Send>> = GraphGate::new(0, 0);
    assert!(
        cut.capture_and_enqueue(
            &mut gate,
            crate::PlatformError::new("winrt", "oversized", "x".repeat(4 * 1024 * 1024 + 1)),
        )
        .unwrap()
    );
    match gate.next().unwrap() {
        DrainItem::Event(callback) => callback(),
        _ => panic!("missing source failure callback"),
    }
    let transfer = consumer
        .recv_transfer()
        .now_or_never()
        .flatten()
        .expect("terminal");
    match transfer.record.as_ref() {
        crate::ubm_peer_notification_ingress::IngressRecord::Terminal {
            reason:
                crate::ubm_peer_notification_ingress::TerminalReason::SourceFailure { error, .. },
            ..
        } => assert_eq!(error.code, "recovery-required"),
        other => panic!("expected retained terminal, got {other:?}"),
    }
    let recovery = consumer
        .prepare_failure_recovery(&transfer)
        .expect("exact recovery");
    let mut bytes = Vec::new();
    while bytes.len() < recovery.encoded_len() {
        bytes.extend(
            recovery
                .read(bytes.len(), FAILURE_RECOVERY_MAX_CHUNK)
                .expect("recovery page"),
        );
    }
    assert!(
        bytes
            .windows(b"oversized".len())
            .any(|window| window == b"oversized")
    );
    assert!(recovery.acknowledge());
    drop(recovery);
    drop(transfer);
    assert!(
        directory
            .read_dir()
            .expect("spilled evidence directory")
            .next()
            .is_none()
    );
    std::fs::remove_dir(&directory).expect("remove test directory");
}

#[test]
fn preowned_retirement_cut_holds_quota_until_replay_and_releases_on_cancel() {
    use crate::ubm_reclaim_authority::GraphKey;
    use crate::ubm_reclaim_graph_gate::{DrainItem, GraphGate};
    let source = std::sync::Arc::new(NotificationFaults::new(1));
    source.start_source_generation(1).unwrap();
    let cancelled = source
        .reserve_future_failure_cut(1, |captured| captured.release())
        .unwrap();
    assert!(matches!(
        source.reserve_future_failure_cut(1, |captured| captured.release()),
        Err(SourceGroupError::Capacity)
    ));
    drop(cancelled);
    let cut = source
        .reserve_future_failure_cut(1, |captured| captured.release())
        .unwrap();
    let key = GraphKey {
        authority_id: 0,
        peer_id: String::new(),
        connection_generation: String::new(),
        graph: (String::new(), 0, 0),
        permit_id: 1,
    };
    let mut gate = GraphGate::new(0, 0);
    gate.acquire(key.clone()).unwrap();
    assert!(
        !cut.capture_and_enqueue(
            &mut gate,
            crate::PlatformError::new("winrt", "hresult", "failure")
        )
        .unwrap()
    );
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    assert!(matches!(
        source.reserve_future_failure_cut(2, |captured| captured.release()),
        Err(SourceGroupError::Capacity)
    ));
    gate.release(&key).unwrap();
    if let DrainItem::Event(callback) = gate.next().unwrap() {
        callback();
    } else {
        panic!("missing cut");
    }
    assert!(
        source
            .reserve_future_failure_cut(2, |captured| captured.release())
            .is_ok()
    );
}
