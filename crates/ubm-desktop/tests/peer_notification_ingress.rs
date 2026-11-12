// crates/ubm-desktop/tests/peer_notification_ingress.rs
#![cfg(feature = "btleplug")]
use btleplug::api::{NotificationEvent, ValueNotification};
use ubm_desktop::btleplug_backend::NotificationRoute;
use ubm_desktop::peer_notification_ingress::{
    CallbackProvenance, IngressError, IngressRecord, PeerNotificationIngress, Poll, RejectedRecord,
    TerminalReason, native_route,
};
use uuid::Uuid;

#[test]
fn native_transfer_acknowledges_identical_occurrences_only_once() {
    let ingress = PeerNotificationIngress::new(7, 2, 0, 0);
    for _ in 0..2 {
        ingress
            .admit(7, None, NotificationEvent::Lagged { skipped_records: 2 })
            .unwrap();
    }
    let first = ingress.peek_transfer().unwrap();
    let retry = ingress.peek_transfer().unwrap();
    assert!(std::sync::Arc::ptr_eq(&first.record, &retry.record));
    assert_eq!(retry.record, first.record);
    assert_eq!(
        ingress.admit(7, None, NotificationEvent::Lagged { skipped_records: 2 }),
        Err(IngressError::Terminated)
    );
    assert!(ingress.ack_transfer(&first));
    assert!(!ingress.ack_transfer(&retry));
    let second = ingress.peek_transfer().unwrap();
    assert_eq!(second.record, first.record);
    assert!(!PeerNotificationIngress::new(7, 2, 0, 0).ack_transfer(&second));
    assert!(ingress.ack_transfer(&second));
    let terminal = ingress.peek_transfer().unwrap();
    assert!(matches!(
        terminal.record.as_ref(),
        IngressRecord::Terminal { .. }
    ));
    assert!(ingress.ack_transfer(&terminal));
    assert!(!ingress.ack_transfer(&terminal));
    assert_eq!(ingress.poll(), Poll::Finished);
}

#[test]
fn native_transfer_keeps_value_and_fence_until_acknowledged() {
    let ingress = PeerNotificationIngress::new(7, 4, 4, 0);
    let route = route(2);
    let membership = ingress.register(7, route, 5).unwrap();
    ingress
        .admit(7, Some(membership), value(2, &[1, 2, 3, 4]))
        .unwrap();
    ingress.retire(7, route, membership).unwrap();
    ingress.barrier(7, 42).unwrap();
    let pending = ingress.peek_transfer().unwrap();
    assert!(
        matches!(pending.record.as_ref(), IngressRecord::Value { value, .. } if value.value == [1, 2, 3, 4])
    );
    assert_eq!(
        ingress.close_after_barrier(7, 42),
        Err(IngressError::NotCurrent)
    );
    assert!(ingress.ack_transfer(&pending));
    let fence = ingress.peek_transfer().unwrap();
    assert!(
        matches!(fence.record.as_ref(), IngressRecord::Fence { membership: m } if *m == membership)
    );
    assert_eq!(
        ingress.close_after_barrier(7, 42),
        Err(IngressError::NotCurrent)
    );
    assert!(ingress.ack_transfer(&fence));
    let barrier = ingress.peek_transfer().unwrap();
    assert!(matches!(
        barrier.record.as_ref(),
        IngressRecord::Barrier { id: 42 }
    ));
    assert_eq!(
        ingress.close_after_barrier(7, 42),
        Err(IngressError::NotCurrent)
    );
    assert!(ingress.ack_transfer(&barrier));
    ingress.close_after_barrier(7, 42).unwrap();
    let terminal = ingress.peek_transfer().unwrap();
    assert!(matches!(
        terminal.record.as_ref(),
        IngressRecord::Terminal {
            reason: TerminalReason::Closed {
                tail_uncertain: false
            },
            ..
        }
    ));
    assert!(ingress.ack_transfer(&terminal));
}

#[test]
fn native_transfer_does_not_free_value_byte_budget_before_ack() {
    let ingress = PeerNotificationIngress::new(7, 4, 4, 0);
    let membership = ingress.register(7, route(2), 5).unwrap();
    ingress
        .admit(7, Some(membership), value(2, &[1, 2, 3, 4]))
        .unwrap();
    let transfer = ingress.peek_transfer().unwrap();
    assert_eq!(
        ingress.admit(7, Some(membership), value(2, &[5])),
        Err(IngressError::Terminated)
    );
    assert!(
        matches!(transfer.record.as_ref(), IngressRecord::Value { value, .. } if value.value == [1, 2, 3, 4])
    );
    assert!(ingress.ack_transfer(&transfer));
    assert!(matches!(
        ingress.peek_transfer().unwrap().record.as_ref(),
        IngressRecord::Terminal {
            reason: TerminalReason::Overflow {
                rejected: RejectedRecord::Value { bytes: 1 },
                ..
            },
            ..
        }
    ));
}

#[tokio::test]
async fn native_transfer_wait_is_cancellable_and_replays_after_wake() {
    let ingress = PeerNotificationIngress::new(7, 2, 8, 0);
    let waiter_ingress = ingress.clone();
    let waiter = tokio::spawn(async move { waiter_ingress.recv_transfer().await });
    tokio::task::yield_now().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    ingress
        .admit(7, None, NotificationEvent::Lagged { skipped_records: 3 })
        .unwrap();
    let first = ingress.recv_transfer().await.unwrap();
    let retry = ingress.recv_transfer().await.unwrap();
    assert!(std::sync::Arc::ptr_eq(&first.record, &retry.record));
    assert!(ingress.ack_transfer(&first));
    ingress.close(7, false).unwrap();
    let terminal = ingress.recv_transfer().await.unwrap();
    assert!(ingress.ack_transfer(&terminal));
    assert!(ingress.recv_transfer().await.is_none());
}

fn route(instance: u64) -> ubm_desktop::peer_notification_ingress::NotificationRoute {
    native_route(NotificationRoute::new(
        Uuid::nil(),
        1,
        Uuid::nil(),
        instance,
    ))
}
fn value(instance: u64, bytes: &[u8]) -> NotificationEvent {
    NotificationEvent::Value(ValueNotification {
        uuid: Uuid::nil(),
        instance,
        service_uuid: Uuid::nil(),
        service_instance: 1,
        value: bytes.to_vec(),
        source_failure: None,
        lost_before: 0,
    })
}
fn failure(instance: u64) -> NotificationEvent {
    let NotificationEvent::Value(mut note) = value(instance, &[]) else {
        unreachable!()
    };
    note.source_failure = Some(btleplug::PlatformError::new(
        "test",
        "failure",
        "source failed",
    ));
    NotificationEvent::Value(note)
}
#[test]
fn same_route_readd_fences_old_callbacks_and_preserves_order() {
    let ingress = PeerNotificationIngress::new(7, 8, 20, 1);
    let old = ingress.register(7, route(1), 10).unwrap();
    let b = ingress.register(7, route(2), 20).unwrap();
    ingress.admit(7, Some(old), value(1, &[11])).unwrap();
    ingress.clone().admit(7, Some(b), value(2, &[22])).unwrap();
    ingress.retire(7, route(1), old).unwrap();
    let new = ingress.register(7, route(1), 10).unwrap();
    assert_ne!(old, new);
    ingress.admit(7, Some(old), value(1, &[33])).unwrap();
    ingress.admit(7, Some(new), value(1, &[44])).unwrap();
    assert_eq!(
        ingress.admit(8, Some(b), value(2, &[55])),
        Err(IngressError::StaleGeneration)
    );
    assert!(
        matches!(ingress.poll(), Poll::Record(IngressRecord::Value { ordinal: 1, membership, value, .. }) if membership == old && value.value == [11])
    );
    assert!(
        matches!(ingress.poll(), Poll::Record(IngressRecord::Value { ordinal: 2, membership, .. }) if membership == b)
    );
    assert_eq!(
        ingress.poll(),
        Poll::Record(IngressRecord::Fence { membership: old })
    );
    assert!(
        matches!(ingress.poll(), Poll::Record(IngressRecord::Value { ordinal: 3, membership, .. }) if membership == new)
    );
    assert_eq!(ingress.poll(), Poll::Empty);
}
#[test]
fn byte_and_item_overflow_are_explicit_and_preserve_accepted_records() {
    for (items, bytes) in [(1, 10), (2, 1)] {
        let ingress = PeerNotificationIngress::new(1, items, bytes, 0);
        let member = ingress.register(1, route(1), 1).unwrap();
        ingress.admit(1, Some(member), value(1, &[1])).unwrap();
        assert_eq!(
            ingress.admit(1, Some(member), value(1, &[2])),
            Err(IngressError::Terminated)
        );
        assert!(matches!(
            ingress.poll(),
            Poll::Record(IngressRecord::Value { ordinal: 0, .. })
        ));
        assert_eq!(
            ingress.poll(),
            Poll::Record(IngressRecord::Terminal {
                reason: TerminalReason::Overflow {
                    rejected: RejectedRecord::Value { bytes: 1 },
                    tail_uncertain: true
                },
                memberships: vec![(route(1), member)]
            })
        );
        assert_eq!(ingress.poll(), Poll::Finished);
    }
}
#[tokio::test]
async fn gap_closure_and_wake_are_distinct_from_empty() {
    let ingress = PeerNotificationIngress::new(1, 2, 10, 0);
    let waiting = ingress.clone();
    let task = tokio::spawn(async move { waiting.recv().await });
    tokio::task::yield_now().await;
    ingress
        .admit(1, None, NotificationEvent::Lagged { skipped_records: 4 })
        .unwrap();
    assert_eq!(
        task.await.unwrap(),
        Some(IngressRecord::Loss {
            skipped_records: 4,
            memberships: vec![]
        })
    );
    ingress.close(1, true).unwrap();
    assert_eq!(
        ingress.recv().await,
        Some(IngressRecord::Terminal {
            reason: TerminalReason::Closed {
                tail_uncertain: true
            },
            memberships: vec![]
        })
    );
    assert_eq!(ingress.recv().await, None);
}
#[test]
fn exhaustion_and_source_failures_are_terminal() {
    let ingress = PeerNotificationIngress::new(1, 2, 10, u64::MAX);
    let m = ingress.register(1, route(1), 1).unwrap();
    ingress.admit(1, Some(m), value(1, &[1])).unwrap();
    assert_eq!(
        ingress.admit(1, Some(m), value(1, &[2])),
        Err(IngressError::Terminated)
    );
    assert!(matches!(
        ingress.poll(),
        Poll::Record(IngressRecord::Value {
            ordinal: u64::MAX,
            ..
        })
    ));
    assert!(matches!(
        ingress.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::OrdinalExhausted {
                rejected_bytes: 1,
                tail_uncertain: true
            },
            ..
        })
    ));
    for scoped in [true, false] {
        let ingress = PeerNotificationIngress::new(1, 2, 10, 0);
        let m = ingress.register(1, route(1), 1).unwrap();
        assert_eq!(
            ingress.admit(1, if scoped { Some(m) } else { None }, failure(1)),
            Err(IngressError::Terminated)
        );
        assert!(
            matches!(ingress.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership, tail_uncertain: true, .. }, .. }) if membership == if scoped { Some(m) } else { None })
        );
    }
}

#[test]
fn source_failure_survives_saturated_preceding_loss() {
    let ingress = PeerNotificationIngress::new(1, 0, 0, 0);
    let member = ingress.register(1, route(1), 1).unwrap();
    let NotificationEvent::Value(mut note) = failure(1) else {
        unreachable!()
    };
    note.lost_before = 9;
    assert_eq!(
        ingress.admit(1, Some(member), NotificationEvent::Value(note)),
        Err(IngressError::Terminated)
    );
    assert!(
        matches!(ingress.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(owner), error, rejected_loss: Some(9), tail_uncertain: true }, .. }) if owner == member && error.code == "failure")
    );
}

#[test]
fn callback_provenance_preserves_source_classification() {
    let ingress = PeerNotificationIngress::new(1, 4, 10, 0);
    let member = ingress.register(1, route(1), 1).unwrap();
    ingress.admit(1, Some(member), value(1, &[1])).unwrap();
    for provenance in [
        CallbackProvenance::Notification,
        CallbackProvenance::ReadOrNotification,
        CallbackProvenance::Unknown,
    ] {
        ingress
            .admit_with_provenance(1, Some(member), value(1, &[2]), provenance)
            .unwrap();
    }
    for expected in [
        CallbackProvenance::Unknown,
        CallbackProvenance::Notification,
        CallbackProvenance::ReadOrNotification,
        CallbackProvenance::Unknown,
    ] {
        assert!(
            matches!(ingress.poll(), Poll::Record(IngressRecord::Value { provenance, .. }) if provenance == expected)
        );
    }
}
