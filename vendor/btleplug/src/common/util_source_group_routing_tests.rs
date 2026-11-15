// vendor/btleplug/src/common/util_source_group_routing_tests.rs

#[test]
fn deferred_retirement_orders_loss_and_excludes_only_retired_membership() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
    let source = std::sync::Arc::new(NotificationFaults::new(8));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    source.start_source_activation(1, route, 11).unwrap();
    let (group, consumer) = source.open_source_group(1, 16, 128).unwrap();
    let retired = source.register_source_route_for_activation(&group, route, 1, 10).unwrap();
    let survivor = source.register_source_route_for_activation(&group, route, 2, 10).unwrap();
    let replacement = source.register_source_route_for_activation(&group, route, 3, 11).unwrap();
    let prefix = source.reserve_source_value_for_activation_bounded(1, route, 10, &note(1, false), usize::MAX).unwrap();
    source.source_group_barrier(&group, 9).unwrap();
    let retirement = source.reserve_source_route_retirement(&group, route, retired).unwrap();
    assert!(source.state.lock().unwrap().groups.values().any(|group| group.routes.iter().any(|entry| entry.1 == retired)), "physical route retired before the queued cut");
    let late = source.register_source_route_for_activation(&group, route, 1, 10).unwrap();
    let mut loss = note(1, false);
    loss.lost_before = 4;
    let later = source.reserve_source_value_for_activation_bounded(1, route, 10, &loss, usize::MAX).unwrap();
    prefix.release();
    later.release();
    for member in [retired, survivor] {
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, ordinal: 0, .. }) if membership == member));
    }
    assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Barrier { id: 9 }));
    assert_eq!(consumer.poll(), Poll::Empty);
    retirement.release();
    assert!(!source.state.lock().unwrap().groups.values().any(|group| group.routes.iter().any(|entry| entry.1 == retired)));
    assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Fence { membership: retired }));
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Loss { skipped_records: 4, memberships }) if memberships == vec![(route, survivor), (route, replacement), (route, late)]));
    for member in [survivor, late] {
        assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, ordinal: 1, .. }) if membership == member));
    }
    assert_eq!(consumer.poll(), Poll::Empty);
}

#[test]
fn queued_failure_before_retirement_keeps_its_membership_and_precedence() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(8));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    let error = crate::PlatformError::new("winrt", "original", "failure");
    let (_, failure) = source.reserve_pending_source_failure(1, error.clone(), usize::MAX).unwrap().unwrap();
    assert!(source.open_source_group(1, 8, 32).is_err());
    let retirement = source.reserve_source_route_retirement(&group, route, member).unwrap();
    assert_eq!(consumer.poll(), Poll::Empty);
    source.fail_source_generation(1, error.clone()).unwrap();
    failure.release();
    retirement.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, memberships }) if found == error && memberships == vec![(route, member)]));
}

#[test]
fn abandoned_held_retirement_preserves_prefix_and_reports_uncertain_tail() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    let prefix = source.reserve_source_value(1, &note(1, false), crate::ubm_peer_notification_ingress::CallbackProvenance::Unknown).unwrap();
    let retirement = source.reserve_source_route_retirement(&group, route, member).unwrap();
    drop(retirement);
    assert_eq!(consumer.poll(), Poll::Empty);
    prefix.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member));
    assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Fence { membership: member }));
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::Closed { tail_uncertain: true }, .. })));
}

#[test]
fn retirement_overflow_retains_rejected_fence_after_held_prefix() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, RejectedRecord, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 1, 32).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    let prefix = source.reserve_source_value(1, &note(1, false), crate::ubm_peer_notification_ingress::CallbackProvenance::Unknown).unwrap();
    let retirement = source.reserve_source_route_retirement(&group, route, member).unwrap();
    prefix.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member));
    assert_eq!(consumer.poll(), Poll::Empty);
    retirement.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::Overflow { rejected: RejectedRecord::Fence { membership }, .. }, memberships }) if membership == member && memberships == vec![(route, member)]));
}

#[test]
fn pending_failure_budget_refusal_keeps_the_source_admissible() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
    let source = NotificationFaults::new(4);
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 4, 32).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    assert!(matches!(source.reserve_pending_source_failure(1, crate::PlatformError::new("winrt", "failure", "reason"), 0), Err(SourceGroupError::Capacity)));
    let value = source.reserve_source_value(1, &note(1, false), crate::ubm_peer_notification_ingress::CallbackProvenance::Unknown).unwrap();
    value.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member));
}

#[test]
fn failure_reservation_charges_only_groups_receiving_a_new_terminal() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, Poll, TerminalReason};
    let error = crate::PlatformError::new("winrt", "failure", "reason");
    let baseline = NotificationFaults::new(2);
    baseline.start_source_generation(1).unwrap();
    let (_group, _consumer) = baseline.open_source_group(1, 2, 32).unwrap();
    let (_, one_recipient) = baseline.reserve_pending_source_failure(1, error.clone(), usize::MAX).unwrap().unwrap();
    let budget = one_recipient.retained_bytes();

    let source = NotificationFaults::new(2);
    source.start_source_generation(1).unwrap();
    let (terminated, earlier) = source.open_source_group(1, 1, 32).unwrap();
    let (_live, live) = source.open_source_group(1, 2, 32).unwrap();
    source.source_group_barrier(&terminated, 1).unwrap();
    let _ = source.source_group_barrier(&terminated, 2);
    assert_eq!(earlier.poll(), Poll::Record(IngressRecord::Barrier { id: 1 }));
    assert!(matches!(earlier.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::Overflow { .. }, .. })));
    let (_, held) = source.reserve_pending_source_failure(1, error.clone(), budget)
        .expect("a group with an earlier terminal must not consume another recipient quota")
        .unwrap();
    held.release();
    assert!(matches!(live.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, .. }) if found == error));
}

#[test]
fn retry_after_failure_budget_refusal_must_not_capture_a_replacement_generation() {
    let source = NotificationFaults::new(4);
    source.start_source_generation(1).unwrap();
    let (_old, _) = source.open_source_group(1, 4, 32).unwrap();
    let error = crate::PlatformError::new("winrt", "hresult", "removal failed")
        .with("hresult", "0x80004005");

    // The caller has retained the error and the generation of its operation.
    // A refused reservation must not allow a later retry to bind it to the
    // current generation after the original generation has been replaced.
    assert!(matches!(source.reserve_pending_source_failure(1, error.clone(), 0), Err(SourceGroupError::Capacity)));
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    let (_replacement, consumer) = source.open_source_group(2, 4, 32).unwrap();
    match source.reserve_pending_source_failure(1, error, usize::MAX) {
        Err(SourceGroupError::StaleGeneration) => {},
        Ok(Some((generation, held))) => {
            assert_eq!(generation, 1, "a failure from generation 1 was rebound to its replacement");
            drop(held);
        },
        _ => panic!("retry must recover its original cut or report its stale generation"),
    }
    assert_eq!(consumer.poll(), crate::ubm_peer_notification_ingress::Poll::Empty);
}

#[test]
fn caller_owned_failure_retry_and_drop_preserve_exact_cut() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let (old, consumer) = source.open_source_group(1, 4, 32).unwrap();
    let member = source.register_source_route_for_activation(&old, route, 1, 10).unwrap();
    let prefix = source.reserve_source_value_for_activation_bounded(1, route, 10, &note(1, false), usize::MAX).unwrap();
    let error = crate::PlatformError::new("winrt", "hresult", "removal failed").with("hresult", "0x80004005");
    let mut failure = source.owned_pending_source_failure(1, route, 10, error.clone(), usize::MAX).unwrap();
    assert!(matches!(failure.try_reserve(0), Err(SourceGroupError::Capacity)));
    failure.try_reserve(usize::MAX).unwrap();
    assert_eq!(consumer.poll(), Poll::Empty);
    drop(failure);
    prefix.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member));
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, memberships }) if found == error && memberships == vec![(route, member)]));
    source.start_source_generation(2).unwrap();
    let (_replacement, replacement_consumer) = source.open_source_group(2, 4, 32).unwrap();
    assert_eq!(replacement_consumer.poll(), Poll::Empty);
}

#[test]
fn abandoned_unreserved_failure_keeps_exact_terminal_after_prefix() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let (group, consumer) = source.open_source_group(1, 4, 32).unwrap();
    let member = source.register_source_route_for_activation(&group, route, 1, 10).unwrap();
    let prefix = source.reserve_source_value_for_activation_bounded(1, route, 10, &note(1, false), usize::MAX).unwrap();
    source.source_group_barrier(&group, 9).unwrap();
    let error = crate::PlatformError::new("winrt", "hresult", "original failure").with("hresult", "0x80004005");
    let mut failure = source.owned_pending_source_failure(1, route, 10, error.clone(), usize::MAX).unwrap();
    assert!(matches!(failure.try_reserve(0), Err(SourceGroupError::Capacity)));
    drop(failure);
    prefix.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member));
    assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Barrier { id: 9 }));
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, memberships }) if found == error && memberships == vec![(route, member)]));
}

#[test]
fn refused_failure_owner_retains_old_cut_across_replacement() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let (group, old) = source.open_source_group(1, 4, 32).unwrap();
    let member = source.register_source_route_for_activation(&group, route, 1, 10).unwrap();
    let prefix = source.reserve_source_value_for_activation_bounded(1, route, 10, &note(1, false), usize::MAX).unwrap();
    source.source_group_barrier(&group, 9).unwrap();
    let error = crate::PlatformError::new("winrt", "hresult", "original failure").with("hresult", "0x80004005");
    let mut failure = source.owned_pending_source_failure(1, route, 10, error.clone(), usize::MAX).unwrap();
    assert!(matches!(failure.try_reserve(0), Err(SourceGroupError::Capacity)));
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    let (_replacement, new) = source.open_source_group(2, 4, 32).unwrap();
    drop(failure);
    prefix.release();
    assert!(matches!(old.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member));
    assert_eq!(old.poll(), Poll::Record(IngressRecord::Barrier { id: 9 }));
    assert!(matches!(old.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, memberships }) if found == error && memberships == vec![(route, member)]));
    assert_eq!(new.poll(), Poll::Empty);
}

#[test]
fn failure_owner_quota_is_held_across_generation_replacement_until_settlement() {
    use crate::ubm_peer_notification_ingress::NotificationRoute;
    let source = std::sync::Arc::new(NotificationFaults::new(1));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let failure = source.owned_pending_source_failure(1, route, 10, crate::PlatformError::new("winrt", "first", "first"), usize::MAX).unwrap();
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    source.start_source_activation(2, route, 11).unwrap();
    assert!(matches!(source.owned_pending_source_failure(2, route, 11, crate::PlatformError::new("winrt", "second", "second"), usize::MAX), Err((SourceGroupError::Capacity, _))));
    drop(failure);
    let replacement = source.owned_pending_source_failure(2, route, 11, crate::PlatformError::new("winrt", "second", "second"), usize::MAX).unwrap();
    drop(replacement);
}

#[test]
fn failure_capture_refusal_returns_the_original_error_allocation() {
    use crate::ubm_peer_notification_ingress::NotificationRoute;
    let source = std::sync::Arc::new(NotificationFaults::new(1));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let held = source.owned_pending_source_failure(1, route, 10, crate::PlatformError::new("winrt", "first", "first"), usize::MAX).unwrap();
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    source.start_source_activation(2, route, 11).unwrap();
    let mut message = String::with_capacity(4000); message.push('x');
    let error = crate::PlatformError::new("winrt", "second", message).with("hresult", "exact");
    let pointer = error.message.as_ptr();
    let (reason, returned) = match source.owned_pending_source_failure(2, route, 11, error, usize::MAX) {
        Err(refusal) => refusal,
        Ok(_) => panic!("owner quota must refuse the second capture"),
    };
    assert_eq!(reason, SourceGroupError::Capacity);
    assert_eq!(returned.message.as_ptr(), pointer);
    assert_eq!(returned.metadata, vec![("hresult", String::from("exact"))]);
    drop(held);
    let retry = source.owned_pending_source_failure(2, route, 11, returned, usize::MAX).unwrap();
    drop(retry);
}

#[test]
fn failure_capture_budget_refuses_before_cloning_and_returns_error_for_retry() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let (_group, consumer) = source.open_source_group(1, 4, 32).unwrap();
    let mut message = String::with_capacity(4096); message.push('x');
    let error = crate::PlatformError::new("winrt", "original", message).with("detail", "preserved");
    let pointer = error.message.as_ptr();
    let (reason, error) = match source.owned_pending_source_failure(1, route, 10, error, 0) {
        Err(refusal) => refusal,
        Ok(_) => panic!("capture should exceed the budget"),
    };
    assert_eq!(reason, SourceGroupError::Capacity);
    assert_eq!(error.message.as_ptr(), pointer);
    assert_eq!(consumer.poll(), Poll::Empty);
    let expected = error.clone();
    let failure = source.owned_pending_source_failure(1, route, 10, error, usize::MAX).unwrap();
    failure.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error, .. }, .. }) if error == expected));
}

#[test]
fn replacement_admission_refusal_and_owner_drop_preserve_old_failure() {
    use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(1));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let (old, consumer) = source.open_source_group(1, 4, 32).unwrap();
    let member = source.register_source_route_for_activation(&old, route, 1, 10).unwrap();
    let prefix = source.reserve_source_value_for_activation_bounded(1, route, 10, &note(1, false), usize::MAX).unwrap();
    let error = crate::PlatformError::new("winrt", "hresult", "original failure").with("hresult", "0x80004005");
    let failure = source.owned_pending_source_failure(1, route, 10, error.clone(), usize::MAX).unwrap();
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    assert!(matches!(source.open_source_group(2, 4, 32), Err(SourceGroupError::Capacity)));
    drop(failure);
    assert_eq!(consumer.poll(), Poll::Empty);
    prefix.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, provenance: CallbackProvenance::Unknown, .. }) if membership == member));
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, memberships }) if found == error && memberships == vec![(route, member)]));
    let (_replacement, replacement) = source.open_source_group(2, 4, 32).unwrap();
    assert_eq!(replacement.poll(), Poll::Empty);
}

#[test]
fn owned_failure_replay_does_not_touch_replacement() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let (old, consumer) = source.open_source_group(1, 4, 32).unwrap();
    source.register_source_route_for_activation(&old, route, 1, 10).unwrap();
    let error = crate::PlatformError::new("winrt", "original", "failure").with("detail", "retained");
    let mut failure = source.owned_pending_source_failure(1, route, 10, error.clone(), usize::MAX).unwrap();
    assert!(failure.try_reserve(usize::MAX).unwrap());
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    let (_new, replacement) = source.open_source_group(2, 4, 32).unwrap();
    failure.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, .. }) if found == error));
    assert_eq!(replacement.poll(), Poll::Empty);
    assert!(source.open_source_group(2, 4, 32).is_ok());
}

#[test]
fn observed_failure_transfer_can_retry_after_activation_and_generation_replacement() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let (group, old) = source.open_source_group(1, 4, 32).unwrap();
    let member = source.register_source_route_for_activation(&group, route, 1, 10).unwrap();
    let error = crate::PlatformError::new("winrt", "hresult", "original");
    let mut failure = source.owned_pending_source_failure(1, route, 10, error.clone(), usize::MAX).unwrap();
    assert!(matches!(failure.try_reserve(0), Err(SourceGroupError::Capacity)));
    source.retire_source_activation(1, route, 10).unwrap();
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    source.start_source_activation(2, route, 11).unwrap();
    let (replacement_group, replacement) = source.open_source_group(2, 4, 32).unwrap();
    source.register_source_route_for_activation(&replacement_group, route, 2, 11).unwrap();
    assert!(failure.try_reserve(usize::MAX).unwrap());
    failure.release();
    assert!(matches!(old.poll(), Poll::Record(IngressRecord::Terminal {
        reason: TerminalReason::SourceFailure { error: found, .. }, memberships
    }) if found == error && memberships == vec![(route, member)]));
    assert_eq!(replacement.poll(), Poll::Empty);
}

#[test]
fn owned_failure_retry_preserves_retired_activation_cut() {
    use crate::ubm_peer_notification_ingress::NotificationRoute;
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let (_group, _) = source.open_source_group(1, 4, 32).unwrap();
    let mut failure = source.owned_pending_source_failure(1, route, 10, crate::PlatformError::new("winrt", "original", "failure"), usize::MAX).unwrap();
    assert!(matches!(failure.try_reserve(0), Err(SourceGroupError::Capacity)));
    source.retire_source_activation(1, route, 10).unwrap();
    source.start_source_activation(1, route, 11).unwrap();
    assert!(failure.try_reserve(usize::MAX).unwrap());
}

#[test]
fn owned_failure_transfer_budget_includes_original_error_capacity_without_recipients() {
    use crate::ubm_peer_notification_ingress::NotificationRoute;
    let source = std::sync::Arc::new(NotificationFaults::new(1));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let mut message = String::with_capacity(4096);
    message.push('x');
    let error = crate::PlatformError::new("winrt", "hresult", message);
    let mut failure = source.owned_pending_source_failure(1, route, 10, error, usize::MAX).unwrap();

    // There are no recipient tokens, but the failure owner still retains the
    // original error allocation; a zero-byte transfer cannot cover it.
    assert!(matches!(failure.try_reserve(0), Err(SourceGroupError::Capacity)));
}

#[test]
fn owned_failure_charges_actual_nested_allocations_and_retains_original_on_refusal() {
    use crate::ubm_peer_notification_ingress::NotificationRoute;
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let mut code = String::with_capacity(300); code.push('c');
    let mut message = String::with_capacity(400); message.push('m');
    let mut value = String::with_capacity(500); value.push('v');
    let mut error = crate::PlatformError::new("winrt", code, message);
    error.metadata = Vec::with_capacity(16);
    error.metadata.push(("nested", value));
    let message_ptr = error.message.as_ptr();
    let metadata_ptr = error.metadata.as_ptr();
    let value_ptr = error.metadata[0].1.as_ptr();
    let minimum = std::mem::size_of::<OwnedPendingSourceFailure>()
        + error.code.capacity() + error.message.capacity()
        + error.metadata.capacity() * std::mem::size_of::<(&'static str, String)>()
        + error.metadata[0].1.capacity();
    let mut failure = source.owned_pending_source_failure(1, route, 10, error, usize::MAX).unwrap();
    assert!(matches!(failure.try_reserve(minimum - 1), Err(SourceGroupError::Capacity)));
    assert_eq!(failure.error.message.as_ptr(), message_ptr);
    assert_eq!(failure.error.metadata.as_ptr(), metadata_ptr);
    assert_eq!(failure.error.metadata[0].1.as_ptr(), value_ptr);
    assert!(failure.try_reserve(minimum).unwrap());
    failure.release();
}

#[test]
fn refused_owned_failure_keeps_original_after_generation_replacement() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let (old_group, old) = source.open_source_group(1, 4, 32).unwrap();
    source.register_source_route_for_activation(&old_group, route, 1, 10).unwrap();
    let error = crate::PlatformError::new("winrt", "hresult", "original").with("detail", "nested");
    let mut failure = source.owned_pending_source_failure(1, route, 10, error.clone(), usize::MAX).unwrap();
    assert!(matches!(failure.try_reserve(0), Err(SourceGroupError::Capacity)));
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    source.start_source_activation(2, route, 11).unwrap();
    let (_new_group, new) = source.open_source_group(2, 4, 32).unwrap();
    assert!(failure.try_reserve(usize::MAX).unwrap());
    drop(failure);
    assert!(matches!(old.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, .. }) if found == error));
    assert_eq!(new.poll(), Poll::Empty);
}

#[test]
fn owned_failure_keeps_a_prior_terminal() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let (group, consumer) = source.open_source_group(1, 1, 32).unwrap();
    source.register_source_route_for_activation(&group, route, 1, 10).unwrap();
    source.publish_source_event(1, crate::api::NotificationEvent::Value(note(1, false))).unwrap();
    source.publish_source_event(1, crate::api::NotificationEvent::Value(note(1, false))).unwrap();
    let mut failure = source.owned_pending_source_failure(1, route, 10, crate::PlatformError::new("winrt", "later", "failure"), usize::MAX).unwrap();
    assert!(failure.try_reserve(usize::MAX).unwrap());
    drop(failure);
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { .. })));
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::Overflow { .. }, .. })));
}

#[test]
fn replay_of_old_failure_does_not_fence_replacement_generation() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = NotificationFaults::new(4);
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7,
        characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    let (old, old_consumer) = source.open_source_group(1, 4, 32).unwrap();
    let old_member = source.register_source_route(&old, route, 1).unwrap();
    let error = crate::PlatformError::new("winrt", "original", "failure");
    let (_, failure) = source.reserve_pending_source_failure(1, error.clone(), usize::MAX).unwrap().unwrap();
    source.close_source_generation(1).unwrap();
    source.start_source_generation(2).unwrap();
    let (new, new_consumer) = source.open_source_group(2, 4, 32).unwrap();
    let new_member = source.register_source_route(&new, route, 1).unwrap();
    assert_eq!(source.fail_source_generation(1, error.clone()), Err(SourceGroupError::StaleGeneration));
    failure.release();
    assert!(matches!(old_consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, memberships }) if found == error && memberships == vec![(route, old_member)]));
    source.publish_source_event(2, crate::api::NotificationEvent::Value(note(1, false))).unwrap();
    assert!(matches!(new_consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == new_member));
}

#[test]
fn shared_source_ordinals_survive_late_group_and_exhaustion() {
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    let source = NotificationFaults::new(8);
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.start_source_generation(7).unwrap();
    let (first, a) = source.open_source_group(7, 8, 32).unwrap();
    let a_member = source.register_source_route(&first, route, 1).unwrap();
    source
        .publish_source_event(7, crate::api::NotificationEvent::Value(note(1, false)))
        .unwrap();
    assert!(
        matches!(a.poll(), Poll::Record(IngressRecord::Value { ordinal: 0, membership, .. }) if membership == a_member)
    );
    let (second, b) = source.open_source_group(7, 8, 32).unwrap();
    let b_member = source.register_source_route(&second, route, 2).unwrap();
    source
        .publish_source_event(7, crate::api::NotificationEvent::Value(note(1, false)))
        .unwrap();
    assert!(
        matches!(a.poll(), Poll::Record(IngressRecord::Value { ordinal: 1, membership, .. }) if membership == a_member)
    );
    assert!(
        matches!(b.poll(), Poll::Record(IngressRecord::Value { ordinal: 1, membership, .. }) if membership == b_member)
    );
    source.state.lock().unwrap().next_source_ordinal = Some(u64::MAX);
    source
        .publish_source_event(7, crate::api::NotificationEvent::Value(note(1, false)))
        .unwrap();
    assert!(matches!(
        a.poll(),
        Poll::Record(IngressRecord::Value {
            ordinal: u64::MAX,
            ..
        })
    ));
    assert!(matches!(
        b.poll(),
        Poll::Record(IngressRecord::Value {
            ordinal: u64::MAX,
            ..
        })
    ));
    source
        .publish_source_event(7, crate::api::NotificationEvent::Value(note(1, false)))
        .unwrap();
    for consumer in [a, b] {
        assert!(matches!(
            consumer.poll(),
            Poll::Record(IngressRecord::Terminal {
                reason: TerminalReason::OrdinalExhausted {
                    rejected_bytes: 1,
                    ..
                },
                ..
            })
        ));
    }
}

#[test]
fn overlapping_members_share_callback_ordinal_and_fence_independently() {
    use crate::ubm_peer_notification_ingress::{
        CallbackProvenance, IngressRecord, NotificationRoute, Poll,
    };
    let source = NotificationFaults::new(8);
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.start_source_generation(9).unwrap();
    source.start_source_activation(9, route, 10).unwrap();
    source.start_source_activation(9, route, 11).unwrap();
    let (group, consumer) = source.open_source_group(9, 16, 128).unwrap();
    let a = source
        .register_source_route_for_activation(&group, route, 1, 10)
        .unwrap();
    let b = source
        .register_source_route_for_activation(&group, route, 2, 10)
        .unwrap();
    let c = source
        .register_source_route_for_activation(&group, route, 3, 11)
        .unwrap();
    source
        .publish_source_event_with_provenance(
            9,
            crate::api::NotificationEvent::Value(note(1, false)),
            CallbackProvenance::ReadOrNotification,
        )
        .unwrap();
    for member in [a, b, c] {
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Value { ordinal: 0, membership, provenance: CallbackProvenance::ReadOrNotification, .. }) if membership == member)
        );
    }
    source.retire_source_route(&group, route, b).unwrap();
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Fence { membership: b })
    );
    source
        .publish_source_value_for_activation(9, route, 10, note(1, false))
        .unwrap();
    assert!(
        matches!(consumer.poll(), Poll::Record(IngressRecord::Value { ordinal: 1, membership, .. }) if membership == a)
    );
    assert_eq!(consumer.poll(), Poll::Empty);
    source
        .publish_source_value_for_activation(9, route, 11, note(1, false))
        .unwrap();
    assert!(
        matches!(consumer.poll(), Poll::Record(IngressRecord::Value { ordinal: 2, membership, .. }) if membership == c)
    );
    assert_eq!(consumer.poll(), Poll::Empty);
}

#[test]
fn overlapping_fanout_overflow_rejects_one_source_callback_atomically() {
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, RejectedRecord, TerminalReason,
    };
    let source = NotificationFaults::new(8);
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 1, 32).unwrap();
    source.register_source_route(&group, route, 1).unwrap();
    source.register_source_route(&group, route, 2).unwrap();
    source
        .publish_source_event(1, crate::api::NotificationEvent::Value(note(1, false)))
        .unwrap();
    assert!(matches!(
        consumer.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Overflow {
                rejected: RejectedRecord::Value { bytes: 1 },
                ..
            },
            ..
        })
    ));
    assert_eq!(consumer.poll(), Poll::Finished);
}

#[test]
fn activation_retirement_clears_only_its_own_retained_failure() {
    use crate::ubm_peer_notification_ingress::{
        IngressError, IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    let source = NotificationFaults::new(4);
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    source
        .publish_source_value_for_activation(1, route, 10, note(1, true))
        .unwrap();
    source.retire_source_activation(1, route, 10).unwrap();
    let (fresh, consumer) = source.open_source_group(1, 8, 32).unwrap();
    source.register_source_route(&fresh, route, 1).unwrap();
    assert_eq!(consumer.poll(), Poll::Empty);
    source.start_source_activation(1, route, 11).unwrap();
    source
        .publish_source_value_for_activation(1, route, 11, note(1, true))
        .unwrap();
    assert_eq!(
        source.retire_source_activation(1, route, 10),
        Err(SourceGroupError::Ingress(IngressError::NotCurrent))
    );
    assert_eq!(
        source.publish_source_value_for_activation(1, route, 10, note(1, false)),
        Err(SourceGroupError::Ingress(IngressError::NotCurrent))
    );
    let (late, late_consumer) = source.open_source_group(1, 8, 32).unwrap();
    source
        .register_source_route_for_activation(&late, route, 2, 11)
        .unwrap();
    assert!(matches!(
        late_consumer.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::SourceFailure { .. },
            ..
        })
    ));
    source.retire_source_activation(1, route, 11).unwrap();
    let (last, last_consumer) = source.open_source_group(1, 8, 32).unwrap();
    source.register_source_route(&last, route, 3).unwrap();
    assert_eq!(last_consumer.poll(), Poll::Empty);
}

#[test]
fn snapshots_and_barrier_follow_source_membership_order() {
    use crate::api::NotificationEvent;
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    let source = NotificationFaults::new(4);
    let route = |instance| NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: instance,
    };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let first = source.register_source_route(&group, route(1), 1).unwrap();
    source
        .publish_source_event(1, NotificationEvent::Lagged { skipped_records: 3 })
        .unwrap();
    let second = source.register_source_route(&group, route(2), 2).unwrap();
    source.source_group_barrier(&group, 9).unwrap();
    source.retire_source_route(&group, route(1), first).unwrap();
    let replacement = source.register_source_route(&group, route(1), 1).unwrap();
    source.close_source_generation(1).unwrap();
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Loss {
            skipped_records: 3,
            memberships: vec![(route(1), first)]
        })
    );
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Barrier { id: 9 })
    );
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Fence { membership: first })
    );
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Closed {
                tail_uncertain: true
            },
            memberships: vec![(route(2), second), (route(1), replacement)]
        })
    );
    assert_eq!(
        source.source_group_barrier(&group, 10),
        Err(SourceGroupError::StaleGeneration)
    );
}

#[test]
fn simultaneous_activations_keep_failures_isolated() {
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    let source = NotificationFaults::new(4);
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    source.start_source_activation(1, route, 11).unwrap();
    let (old, old_rx) = source.open_source_group(1, 8, 32).unwrap();
    let (new, new_rx) = source.open_source_group(1, 8, 32).unwrap();
    source
        .register_source_route_for_activation(&old, route, 1, 10)
        .unwrap();
    source
        .register_source_route_for_activation(&new, route, 2, 11)
        .unwrap();
    source
        .publish_source_value_for_activation(1, route, 10, note(1, true))
        .unwrap();
    assert!(matches!(
        old_rx.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::SourceFailure { .. },
            ..
        })
    ));
    assert_eq!(new_rx.poll(), Poll::Empty);
    source.retire_source_activation(1, route, 10).unwrap();
    source
        .publish_source_value_for_activation(1, route, 11, note(1, false))
        .unwrap();
    assert!(matches!(
        new_rx.poll(),
        Poll::Record(IngressRecord::Value { .. })
    ));
}

#[test]
fn barrier_overflow_terminates_with_current_membership_snapshot() {
    use crate::ubm_peer_notification_ingress::{
        IngressError, IngressRecord, NotificationRoute, Poll, RejectedRecord, TerminalReason,
    };
    let source = NotificationFaults::new(2);
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 1, 8).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    source
        .publish_source_event(1, crate::api::NotificationEvent::Value(note(1, false)))
        .unwrap();
    assert_eq!(
        source.source_group_barrier(&group, 7),
        Err(SourceGroupError::Ingress(IngressError::Terminated))
    );
    assert!(matches!(
        consumer.poll(),
        Poll::Record(IngressRecord::Value { .. })
    ));
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Overflow {
                rejected: RejectedRecord::Barrier { id: 7 },
                tail_uncertain: true
            },
            memberships: vec![(route, member)]
        })
    );
}

#[test]
fn source_groups_order_callbacks_fences_and_generation() {
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    let source = NotificationFaults::new(4);
    let (sender, mut broadcast) = tokio::sync::broadcast::channel(8);
    source.start_source_generation(3).unwrap();
    let (a, a_rx) = source.open_source_group(3, 8, 64).unwrap();
    let (b, b_rx) = source.open_source_group(3, 8, 64).unwrap();
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
    let a1 = source.register_source_route(&a, route(1), 11).unwrap();
    let b1 = source.register_source_route(&b, route(1), 21).unwrap();
    let b2 = source.register_source_route(&b, route(2), 22).unwrap();
    let old = source.begin(&characteristic(1)).unwrap();
    let epoch2 = source.begin(&characteristic(2)).unwrap();
    source.publish_for_epoch(&sender, note(1, false), old);
    source.publish_for_epoch(&sender, note(2, false), epoch2);
    source.retire_source_route(&a, route(1), a1).unwrap();
    let a_new = source.register_source_route(&a, route(1), 11).unwrap();
    source.clear(&characteristic(1));
    let fresh = source.begin(&characteristic(1)).unwrap();
    source.publish_for_epoch(&sender, note(1, false), old);
    source.publish_for_epoch(&sender, note(1, false), fresh);
    assert!(
        matches!(a_rx.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == a1)
    );
    assert_eq!(
        a_rx.poll(),
        Poll::Record(IngressRecord::Fence { membership: a1 })
    );
    assert!(
        matches!(a_rx.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == a_new)
    );
    assert_eq!(a_rx.poll(), Poll::Empty);
    assert!(
        matches!(b_rx.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == b1)
    );
    assert!(
        matches!(b_rx.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == b2)
    );
    assert!(
        matches!(b_rx.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == b1)
    );
    assert_eq!(broadcast.try_recv().unwrap().notification.instance, 1);
    assert_eq!(broadcast.try_recv().unwrap().notification.instance, 2);
    assert_eq!(broadcast.try_recv().unwrap().notification.instance, 1);
    assert!(broadcast.try_recv().is_err());
    source.clear_all();
    assert!(matches!(
        a_rx.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Closed {
                tail_uncertain: true
            },
            ..
        })
    ));
    assert!(matches!(
        b_rx.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Closed {
                tail_uncertain: true
            },
            ..
        })
    ));
    assert_eq!(
        source.register_source_route(&a, route(1), 12),
        Err(SourceGroupError::StaleGeneration)
    );
    assert_eq!(
        source.start_source_generation(3),
        Err(SourceGroupError::StaleGeneration)
    );
    source.start_source_generation(4).unwrap();
    assert_eq!(
        source.register_source_route(&a, route(1), 12),
        Err(SourceGroupError::StaleGeneration)
    );
}

#[test]
fn source_group_overflow_is_local_and_terminal_is_reserved() {
    use crate::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, Poll, RejectedRecord, TerminalReason,
    };
    let source = NotificationFaults::new(2);
    source.start_source_generation(1).unwrap();
    let (a, ar) = source.open_source_group(1, 1, 8).unwrap();
    let (b, br) = source.open_source_group(1, 4, 8).unwrap();
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    let characteristic = Characteristic {
        uuid: Uuid::nil(),
        instance: 1,
        service_uuid: Uuid::nil(),
        service_instance: 7,
        properties: Default::default(),
        descriptors: Default::default(),
    };
    let member = source.register_source_route(&a, route, 1).unwrap();
    source.register_source_route(&b, route, 2).unwrap();
    let epoch = source.begin(&characteristic).unwrap();
    let (sender, _) = tokio::sync::broadcast::channel(2);
    source.publish_for_epoch(&sender, note(1, false), epoch);
    assert_eq!(
        source.retire_source_route(&a, route, member),
        Err(SourceGroupError::Ingress(
            crate::ubm_peer_notification_ingress::IngressError::Terminated
        ))
    );
    assert!(matches!(
        ar.poll(),
        Poll::Record(IngressRecord::Value { .. })
    ));
    assert!(matches!(
        ar.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Overflow {
                rejected: RejectedRecord::Fence { membership: found },
                tail_uncertain: true
            },
            ..
        }) if found == member
    ));
    source.publish_for_epoch(&sender, note(1, false), epoch);
    assert!(matches!(
        br.poll(),
        Poll::Record(IngressRecord::Value { .. })
    ));
    assert!(matches!(
        br.poll(),
        Poll::Record(IngressRecord::Value { .. })
    ));
    source.detach_source_group(&b).unwrap();
    assert!(matches!(
        br.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Closed {
                tail_uncertain: true
            },
            ..
        })
    ));
    assert_eq!(
        source.detach_source_group(&b),
        Err(SourceGroupError::UnknownGroup)
    );
}

#[test]
fn reserved_activation_snapshots_exact_membership_before_barrier_and_replacement() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
    let source = NotificationFaults::new(8);
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7, characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    source.start_source_activation(1, route, 11).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 64).unwrap();
    let old = source.register_source_route_for_activation(&group, route, 1, 10).unwrap();
    let replacement = source.register_source_route_for_activation(&group, route, 2, 11).unwrap();
    let held = source.reserve_source_value_for_activation_bounded(1, route, 10, &note(1, false), usize::MAX).unwrap();
    source.source_group_barrier(&group, 7).unwrap();
    source.register_source_route_for_activation(&group, route, 3, 10).unwrap();
    assert_eq!(consumer.poll(), Poll::Empty);
    held.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == old));
    assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Barrier { id: 7 }), "replacement {replacement:?} must not receive the old activation");
    assert_eq!(consumer.poll(), Poll::Empty);
}

#[test]
fn reserved_activation_failure_is_retained_only_until_its_retirement() {
    use crate::ubm_peer_notification_ingress::{IngressError, IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = NotificationFaults::new(8);
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7, characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    source.start_source_activation(1, route, 11).unwrap();
    let (old, consumer) = source.open_source_group(1, 8, 64).unwrap();
    source.register_source_route_for_activation(&old, route, 1, 10).unwrap();
    let held = source.reserve_source_value_for_activation_bounded(1, route, 10, &note(1, true), usize::MAX).unwrap();
    let (late, late_consumer) = source.open_source_group(1, 8, 64).unwrap();
    assert!(matches!(source.register_source_route_for_activation(&late, route, 4, 10), Err(SourceGroupError::SourceStateUnavailable)));
    assert_eq!(late_consumer.poll(), Poll::Empty);
    let (replacement, replacement_consumer) = source.open_source_group(1, 8, 64).unwrap();
    source.register_source_route_for_activation(&replacement, route, 2, 11).unwrap();
    assert_eq!(consumer.poll(), Poll::Empty);
    held.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { .. }, .. })));
    source.register_source_route_for_activation(&late, route, 4, 10).unwrap();
    assert!(matches!(late_consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { .. }, .. })));
    assert_eq!(replacement_consumer.poll(), Poll::Empty);
    source.retire_source_activation(1, route, 10).unwrap();
    assert!(matches!(source.reserve_source_value_for_activation_bounded(1, route, 10, &note(1, false), usize::MAX), Err(SourceGroupError::Ingress(IngressError::NotCurrent))));
    let (retry, retry_consumer) = source.open_source_group(1, 8, 64).unwrap();
    source.register_source_route_for_activation(&retry, route, 3, 11).unwrap();
    assert_eq!(retry_consumer.poll(), Poll::Empty);
}

#[test]
fn retiring_held_failure_does_not_release_reused_activation_failure() {
    use crate::ubm_peer_notification_ingress::{NotificationRoute, Poll};
    let source = NotificationFaults::new(8);
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7, characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let old = source.reserve_source_value_for_activation_bounded(1, route, 10, &note(1, true), usize::MAX).unwrap();
    source.retire_source_activation(1, route, 10).unwrap();
    source.start_source_activation(1, route, 10).unwrap();
    let current = source.reserve_source_value_for_activation_bounded(1, route, 10, &note(1, true), usize::MAX).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 64).unwrap();
    old.release();
    assert!(matches!(source.register_source_route_for_activation(&group, route, 1, 10), Err(SourceGroupError::SourceStateUnavailable)));
    assert_eq!(consumer.poll(), Poll::Empty);
    current.release();
    source.register_source_route_for_activation(&group, route, 1, 10).unwrap();
    assert!(matches!(consumer.poll(), Poll::Record(_)));
}

#[test]
fn reserved_callback_snapshots_membership_and_precedes_barrier_and_retirement() {
    use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressRecord, NotificationRoute, Poll};
    let source = NotificationFaults::new(8);
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7, characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 64).unwrap();
    let first = source.register_source_route(&group, route, 1).unwrap();
    let reservation = source.reserve_source_value(1, &note(1, false), CallbackProvenance::Notification).unwrap();
    source.source_group_barrier(&group, 7).unwrap();
    source.retire_source_route(&group, route, first).unwrap();
    source.register_source_route(&group, route, 2).unwrap();
    assert_eq!(consumer.poll(), Poll::Empty);
    reservation.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { ordinal: 0, membership, .. }) if membership == first));
    assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Barrier { id: 7 }));
    assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Fence { membership: first }));
    assert_eq!(consumer.poll(), Poll::Empty);
}

#[test]
fn abandoned_callback_recovers_after_group_detaches() {
    use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = NotificationFaults::new(8);
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7, characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 64).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    let reservation = source.reserve_source_value(1, &note(1, false), CallbackProvenance::Notification).unwrap();
    source.detach_source_group(&group).unwrap();
    assert_eq!(consumer.poll(), Poll::Empty);
    drop(reservation);
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member));
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::Closed { tail_uncertain: true }, .. })));
}

#[test]
fn reserved_loss_and_failure_preserve_memberships_until_replay_and_recovery() {
    use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = NotificationFaults::new(8);
    let route = |instance| NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7, characteristic_uuid: Uuid::nil(), characteristic_instance: instance };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 64).unwrap();
    let member = source.register_source_route(&group, route(1), 1).unwrap();
    let (other, other_rx) = source.open_source_group(1, 8, 64).unwrap();
    let other_member = source.register_source_route(&other, route(2), 2).unwrap();
    let mut failed = note(1, true);
    failed.lost_before = 3;
    let original = failed.source_failure.clone().unwrap();
    let held = source.reserve_source_value(1, &failed, CallbackProvenance::Unknown).unwrap();
    source.source_group_barrier(&other, 9).unwrap();
    assert_eq!(consumer.poll(), Poll::Empty);
    assert_eq!(other_rx.poll(), Poll::Empty);
    drop(held);
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Loss { skipped_records: 3, memberships }) if memberships == vec![(route(1), member)]));
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(m), error, .. }, memberships }) if m == member && error == original && memberships == vec![(route(1), member)]));
    assert!(matches!(other_rx.poll(), Poll::Record(IngressRecord::Loss { skipped_records: 3, memberships }) if memberships == vec![(route(2), other_member)]));
    assert_eq!(other_rx.poll(), Poll::Record(IngressRecord::Barrier { id: 9 }));
    assert!(matches!(other_rx.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::Closed { tail_uncertain: true }, .. })));
}

#[test]
fn late_route_waits_for_held_failure_replay() {
    use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = NotificationFaults::new(8);
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7, characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    let failed = note(1, true);
    let error = failed.source_failure.clone().unwrap();
    let held = source.reserve_source_value(1, &failed, CallbackProvenance::Unknown).unwrap();
    let (late, rx) = source.open_source_group(1, 8, 64).unwrap();
    assert!(matches!(source.register_source_route(&late, route, 4), Err(SourceGroupError::SourceStateUnavailable)));
    assert_eq!(rx.poll(), Poll::Empty);
    held.release();
    let member = source.register_source_route(&late, route, 4).unwrap();
    assert!(matches!(rx.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(m), error: e, .. }, .. }) if m == member && e == error));
}

#[test]
fn reserved_ordinal_exhaustion_is_held_until_callback_replay() {
    use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressRecord, Poll, TerminalReason};
    let source = NotificationFaults::new(8);
    source.start_source_generation(1).unwrap();
    let (_, consumer) = source.open_source_group(1, 8, 64).unwrap();
    source.state.lock().unwrap().next_source_ordinal = None;
    let held = source.reserve_source_value(1, &note(1, false), CallbackProvenance::Unknown)
        .expect("exhaustion must retain its terminal ownership");
    assert_eq!(consumer.poll(), Poll::Empty);
    held.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal {
        reason: TerminalReason::OrdinalExhausted { .. }, ..
    })));
}

#[test]
fn generation_cut_holds_empty_terminal_and_allows_a_new_generation() {
    use crate::ubm_peer_notification_ingress::{IngressRecord, Poll, TerminalReason};
    let source = NotificationFaults::new(8);
    source.start_source_generation(1).unwrap();
    let (_, old) = source.open_source_group(1, 8, 64).unwrap();
    let cut = source.reserve_close_source_generation(1).unwrap();
    assert_eq!(old.poll(), Poll::Empty, "the terminal belongs to the deferred cut");
    source.start_source_generation(2).unwrap();
    let (_, current) = source.open_source_group(2, 8, 64).unwrap();
    assert_eq!(current.poll(), Poll::Empty);
    cut.release();
    assert!(matches!(old.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::Closed { .. }, .. })));
}

#[test]
fn generation_cut_charges_retained_per_group_holds() {
    let source = NotificationFaults::new(8);
    source.start_source_generation(1).unwrap();
    let (_, first) = source.open_source_group(1, 8, 64).unwrap();
    let (_, second) = source.open_source_group(1, 8, 64).unwrap();
    let cut = source.reserve_close_source_generation(1).unwrap();
    let minimum = 2 * (std::mem::size_of::<crate::ubm_peer_notification_ingress::IngressReservation>()
        + 3 * std::mem::size_of::<usize>());
    assert!(cut.retained_bytes() >= minimum, "both held terminal tokens must be charged");
    assert_eq!(first.poll(), crate::ubm_peer_notification_ingress::Poll::Empty);
    assert_eq!(second.poll(), crate::ubm_peer_notification_ingress::Poll::Empty);
    cut.release();
}

#[test]
fn generation_cut_budget_refusal_keeps_generation_and_prefix() {
    use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = NotificationFaults::new(8);
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7, characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 64).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    let prefix = source.reserve_source_value(1, &note(1, false), CallbackProvenance::Notification).unwrap();
    assert!(matches!(source.reserve_close_source_generation_bounded(1, 0), Err(SourceGroupError::Capacity)));
    assert_eq!(consumer.poll(), Poll::Empty);
    let cut = source.reserve_close_source_generation(1).unwrap();
    cut.release();
    assert_eq!(consumer.poll(), Poll::Empty);
    prefix.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member));
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::Closed { .. }, .. })));
}

#[test]
fn abandoned_generation_cut_exposes_prefix_and_uncertain_terminal() {
    use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressRecord, NotificationRoute, Poll, TerminalReason};
    let source = NotificationFaults::new(8);
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7, characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 64).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    let value = source.reserve_source_value(1, &note(1, false), CallbackProvenance::Notification).unwrap();
    let cut = source.reserve_close_source_generation(1).unwrap();
    assert_eq!(consumer.poll(), Poll::Empty);
    drop(cut);
    assert_eq!(consumer.poll(), Poll::Empty, "earlier callback still owns its prefix");
    value.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { membership, .. }) if membership == member));
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::Closed { tail_uncertain: true }, .. })));
}

#[test]
fn inconsistent_group_generation_fails_before_reserving_any_fanout() {
    use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressError, NotificationRoute, PeerNotificationIngress, Poll};
    let source = NotificationFaults::new(8);
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7, characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    let (first, consumer) = source.open_source_group(1, 8, 64).unwrap();
    source.register_source_route(&first, route, 1).unwrap();
    let (second, _) = source.open_source_group(1, 8, 64).unwrap();
    source.register_source_route(&second, route, 2).unwrap();
    source.state.lock().unwrap().groups.get_mut(&second.id).unwrap().ingress = PeerNotificationIngress::new(2, 8, 64, 0);
    assert!(matches!(source.reserve_source_value(1, &note(1, false), CallbackProvenance::Notification), Err(SourceGroupError::Ingress(IngressError::StaleGeneration))));
    assert_eq!(consumer.poll(), Poll::Empty, "failed preflight must not expose a partial callback");
    assert_eq!(source.state.lock().unwrap().next_source_ordinal, Some(0));
}

#[test]
fn reservation_budget_refusal_preserves_ordinal_and_existing_held_prefix() {
    use crate::ubm_peer_notification_ingress::{CallbackProvenance, IngressRecord, NotificationRoute, Poll};
    let source = NotificationFaults::new(8);
    let route = NotificationRoute { service_uuid: Uuid::nil(), service_instance: 7, characteristic_uuid: Uuid::nil(), characteristic_instance: 1 };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 64).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    let held = source.reserve_source_value(1, &note(1, false), CallbackProvenance::Notification).unwrap();
    assert!(matches!(source.reserve_source_value_bounded(1, &note(1, true), CallbackProvenance::Unknown, 0), Err(SourceGroupError::Capacity)));
    assert_eq!(source.state.lock().unwrap().next_source_ordinal, Some(1));
    assert_eq!(consumer.poll(), Poll::Empty);
    held.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { ordinal: 0, membership, .. }) if membership == member));
    assert_eq!(consumer.poll(), Poll::Empty);
    let next = source.reserve_source_value(1, &note(1, false), CallbackProvenance::Notification).unwrap();
    next.release();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Value { ordinal: 1, membership, .. }) if membership == member));
}
