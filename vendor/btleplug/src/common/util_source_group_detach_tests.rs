// vendor/btleplug/src/common/util_source_group_detach_tests.rs

#[test]
fn admitted_failure_survives_forced_detach_and_clean_detach_waits() {
    use crate::ubm_peer_notification_ingress::{IngressError, IngressRecord, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(4));
    source.start_source_generation(1).unwrap();
    let (forced, forced_rx) = source.open_source_group(1, 8, 32).unwrap();
    let (clean, clean_rx) = source.open_source_group(1, 8, 32).unwrap();
    let mut owner = source.reserve_future_source_failure(1, 4).unwrap();
    source.source_group_barrier(&clean, 9).unwrap();
    assert_eq!(clean_rx.poll(), Poll::Record(IngressRecord::Barrier { id: 9 }));
    assert_eq!(source.detach_source_group_after_barrier(&clean, 9), Err(SourceGroupError::Ingress(IngressError::NotCurrent)));
    source.detach_source_group(&forced).unwrap();
    assert_eq!(forced_rx.poll(), Poll::Empty);
    let error = crate::PlatformError::new("winrt", "hresult", "retirement failure");
    let captured = owner.capture(error.clone(), usize::MAX).unwrap_or_else(|_| panic!("capture"));
    captured.release();
    for rx in [forced_rx, clean_rx] {
        assert!(matches!(rx.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { error: found, .. }, .. }) if found == error));
    }
}

#[test]
fn clean_detach_succeeds_after_admitted_operation_settles_without_failure() {
    use crate::ubm_peer_notification_ingress::{IngressError, IngressRecord, Poll, TerminalReason};
    let source = std::sync::Arc::new(NotificationFaults::new(2));
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let owner = source.reserve_future_source_failure(1, 2).unwrap();
    source.source_group_barrier(&group, 9).unwrap();
    assert_eq!(consumer.poll(), Poll::Record(IngressRecord::Barrier { id: 9 }));
    assert_eq!(source.detach_source_group_after_barrier(&group, 9), Err(SourceGroupError::Ingress(IngressError::NotCurrent)));
    drop(owner);
    source.detach_source_group_after_barrier(&group, 9).unwrap();
    assert!(matches!(consumer.poll(), Poll::Record(IngressRecord::Terminal {
        reason: TerminalReason::Closed { tail_uncertain: false }, ..
    })));
}

#[test]
fn clean_detach_requires_consumed_current_barrier_and_no_routes() {
    use crate::ubm_peer_notification_ingress::{
        IngressError, IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    let source = NotificationFaults::new(8);
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    source.source_group_barrier(&group, 9).unwrap();
    assert_eq!(
        source.detach_source_group_after_barrier(&group, 9),
        Err(SourceGroupError::Ingress(IngressError::NotCurrent))
    );
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Barrier { id: 9 })
    );
    assert_eq!(
        source.detach_source_group_after_barrier(&group, 9),
        Err(SourceGroupError::Ingress(IngressError::NotCurrent))
    );
    source.retire_source_route(&group, route, member).unwrap();
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Fence { membership: member })
    );
    assert_eq!(
        source.detach_source_group_after_barrier(&group, 9),
        Err(SourceGroupError::Ingress(IngressError::NotCurrent))
    );
    source.source_group_barrier(&group, 10).unwrap();
    assert_eq!(
        source.detach_source_group_after_barrier(&group, 10),
        Err(SourceGroupError::Ingress(IngressError::NotCurrent))
    );
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Barrier { id: 10 })
    );
    source
        .detach_source_group_after_barrier(&group, 10)
        .unwrap();
    assert!(matches!(
        consumer.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Closed {
                tail_uncertain: false
            },
            ..
        })
    ));
}

#[test]
fn records_after_barrier_invalidate_its_detach_proof() {
    use crate::ubm_peer_notification_ingress::{
        IngressError, IngressRecord, NotificationRoute, Poll,
    };
    let source = NotificationFaults::new(8);
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 7,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 1,
    };
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    source.source_group_barrier(&group, 9).unwrap();
    source.retire_source_route(&group, route, member).unwrap();
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Barrier { id: 9 })
    );
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Fence { membership: member })
    );
    assert_eq!(
        source.detach_source_group_after_barrier(&group, 9),
        Err(SourceGroupError::Ingress(IngressError::NotCurrent))
    );
}

#[test]
fn forced_detach_with_unconsumed_fence_retains_uncertain_tail() {
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
    source.start_source_generation(1).unwrap();
    let (group, consumer) = source.open_source_group(1, 8, 32).unwrap();
    let member = source.register_source_route(&group, route, 1).unwrap();
    source.retire_source_route(&group, route, member).unwrap();
    source.detach_source_group(&group).unwrap();
    assert_eq!(
        consumer.poll(),
        Poll::Record(IngressRecord::Fence { membership: member })
    );
    assert!(matches!(
        consumer.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Closed {
                tail_uncertain: true
            },
            ..
        })
    ));
}
