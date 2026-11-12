// crates/ubm-mobile/src/ordered_batch_tests.rs

use super::*;
use crate::radio::{MobilePlatform, OrderedProvenance, OrderedRegistration};
use crate::test_common::{self, Reply, Scripted};
use std::sync::atomic::{AtomicUsize, Ordering};
use ubm_core::ordered_notifications::{DeliveryMode, Member, Poll, Record, SourceGeneration};
use ubm_desktop::boundary::OrderedSourceTransition;

#[test]
fn oversized_native_failure_is_reported_without_truncating_it() {
    let detail = "x".repeat(ubm_core::contracts::MAX_STREAM_BYTE_CAPACITY as usize + 1);
    let record = OrderedRecord::SubsequentSourceFailure {
        failure: crate::PlatformFailure::new(crate::FailureKind::Platform, detail),
    };
    let Err(error) = convert(record, None, MobilePlatform::Android) else {
        panic!("oversized native failure was accepted");
    };
    assert_eq!(error.code(), BleErrorCode::ArgumentInvalid);
    assert_eq!(error.operation(), "gatt.ordered.ingress.record-too-large");
}

#[tokio::test]
async fn canceled_ack_retries_without_duplicate_and_drains_next_batch() {
    let drains = Arc::new(AtomicUsize::new(0));
    let acks = Arc::new(AtomicUsize::new(0));
    let observed_drains = drains.clone();
    let observed_acks = acks.clone();
    let barrier_ids = Arc::new(StdMutex::new(Vec::new()));
    let observed_barriers = barrier_ids.clone();
    let member = OrderedRegistration {
        member_id: 1,
        source_member: 4,
        epoch: 2,
    };
    let fence_member = OrderedRegistration {
        member_id: 2,
        source_member: 5,
        epoch: 3,
    };
    let new_member = OrderedRegistration {
        member_id: 2,
        source_member: 6,
        epoch: 4,
    };
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::OrderedDrain { .. } => {
            let index = observed_drains.fetch_add(1, Ordering::SeqCst);
            if index < 2 {
                Reply::Now(RadioCompletion::OrderedRecords {
                    batch_id: Some(index as u64 + 10),
                    records: {
                        let mut records = vec![OrderedRecord::Value {
                            member,
                            ordinal: index as u64 + 1,
                            value: vec![index as u8],
                            delivery: ubm_desktop::ObservedDelivery::Notification,
                            provenance: OrderedProvenance::Notification,
                        }];
                        if index == 0 {
                            records.push(OrderedRecord::Fence {
                                member: fence_member,
                                uncertain_tail: false,
                            });
                            records.push(OrderedRecord::Barrier {
                                barrier_id: 33,
                                uncertain_tail: false,
                            });
                        }
                        records
                    },
                })
            } else if index == 3 {
                let barrier_id = *crate::foreign::lock(&observed_barriers).last().unwrap();
                Reply::Now(RadioCompletion::OrderedRecords {
                    batch_id: Some(12),
                    records: vec![
                        OrderedRecord::Barrier {
                            barrier_id,
                            uncertain_tail: false,
                        },
                        OrderedRecord::Value {
                            member,
                            ordinal: 3,
                            value: vec![2],
                            delivery: ubm_desktop::ObservedDelivery::Notification,
                            provenance: OrderedProvenance::Notification,
                        },
                        OrderedRecord::Value {
                            member: new_member,
                            ordinal: 4,
                            value: vec![3],
                            delivery: ubm_desktop::ObservedDelivery::Notification,
                            provenance: OrderedProvenance::Notification,
                        },
                    ],
                })
            } else if index == 5 {
                Reply::Now(RadioCompletion::OrderedRecords {
                    batch_id: Some(13),
                    records: vec![OrderedRecord::Terminal {
                        reason: OrderedEnd::WakeFailed,
                        uncertain_tail: true,
                        native_domain: Some("test".into()),
                        native_code: Some("wake-rejected".into()),
                        failure_member: None,
                        rejected: None,
                    }],
                })
            } else {
                Reply::Now(RadioCompletion::OrderedRecords {
                    batch_id: None,
                    records: vec![],
                })
            }
        }
        RadioRequest::OrderedRegister { .. } => Reply::Hold,
        RadioRequest::OrderedBarrier { barrier_id, .. } => {
            let mut ids = crate::foreign::lock(&observed_barriers);
            ids.push(*barrier_id);
            if ids.len() == 1 {
                Reply::Hold
            } else {
                Reply::Now(RadioCompletion::Unit)
            }
        }
        RadioRequest::OrderedAcknowledgeBatch { .. }
            if observed_acks.fetch_add(1, Ordering::SeqCst) == 0 =>
        {
            Reply::Hold
        }
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("batch-test").unwrap();
    test_common::ok(
        &test_common::call(
            &session,
            "scan.start",
            r#"{"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}"#,
        )
        .await,
    );
    host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
        peer_id: test_common::POLAR.into(),
        service_uuids: vec!["180D".into()],
        ..crate::Advertisement::default()
    }));
    test_common::drain_until(&session, |records| {
        !test_common::of_type(records, "adv").is_empty()
    })
    .await;
    let connected = test_common::ok(&test_common::call(&session, "connection.connect", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"connect"}).to_string()).await);
    let discovered = test_common::ok(&test_common::call(&session, "gatt.discover", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"discover"}).to_string()).await);
    let generation = SourceGeneration::Counter(8);
    let source = SourceKey {
        peer_id: test_common::POLAR.into(),
        generation: generation.clone(),
        native_group: 9,
    };
    let owner = OrderedCollection::new(
        connected["connectionGeneration"].as_str().unwrap().into(),
        discovered["databaseGeneration"].as_str().unwrap().into(),
        generation,
        19,
        vec![
            Member {
                id: 1,
                source: membership(member),
                delivery: DeliveryMode::Notification,
            },
            Member {
                id: 2,
                source: membership(fence_member),
                delivery: DeliveryMode::Notification,
            },
        ],
        8,
        64,
        8,
    )
    .unwrap();
    let admission = host.inner.central.admit_gatt(test_common::POLAR).unwrap();
    let consumer = host
        .install_ordered_consumer(source.clone(), owner, &admission)
        .await
        .unwrap();
    host.inner
        .central
        .update_mobile_ordered_ingress(
            19,
            &source,
            OrderedSourceTransition::BeginEnable { member_id: 1 },
        )
        .unwrap();
    host.inner
        .central
        .update_mobile_ordered_ingress(
            19,
            &source,
            OrderedSourceTransition::EnableResult {
                member_id: 1,
                outcome: Some(true),
            },
        )
        .unwrap();
    host.inner
        .central
        .update_mobile_ordered_ingress(
            19,
            &source,
            OrderedSourceTransition::BeginEnable { member_id: 2 },
        )
        .unwrap();
    host.inner
        .central
        .update_mobile_ordered_ingress(
            19,
            &source,
            OrderedSourceTransition::EnableResult {
                member_id: 2,
                outcome: Some(true),
            },
        )
        .unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(10),
            consumer.drain_to_empty()
        )
        .await
        .is_err()
    );
    assert!(!consumer.has_consumed_fence(fence_member));
    assert_eq!(consumer.take_consumed_barrier(33), None);
    drop(consumer);
    let consumer = host
        .ordered_consumer(&source)
        .expect("host retains batch progress");
    consumer.drain_to_empty().await.unwrap();
    assert!(consumer.has_consumed_fence(fence_member));
    assert_eq!(consumer.take_consumed_barrier(33), Some(false));
    for expected in [0, 1] {
        assert!(
            matches!(host.inner.central.poll_mobile_ordered_ingress(19, &source).unwrap(), Poll::Record(Record::Value { value, delivery: ubm_core::ordered_notifications::ObservedDelivery::Notification, .. }) if value == [expected])
        );
    }
    assert!(matches!(
        host.inner
            .central
            .poll_mobile_ordered_ingress(19, &source)
            .unwrap(),
        Poll::Empty
    ));
    assert_eq!(drains.load(Ordering::SeqCst), 3);
    assert_eq!(acks.load(Ordering::SeqCst), 3);
    // Native registration completes after the caller is canceled. Routing
    // stays paused until that exact result is recovered and centrally rebound.
    let lease = ubm_desktop::boundary::OrderedSourceLease {
        peer_id: test_common::POLAR.into(),
        connection_generation: connected["connectionGeneration"].as_str().unwrap().into(),
        collection_id: 19,
        source_generation: source.generation.clone(),
        native_group: 9,
        members: vec![(1, membership(member)), (2, membership(fence_member))],
        ingress: ubm_desktop::boundary::OrderedSourceIngress::CentralRetainedBatch,
    };
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(10),
            ubm_desktop::RadioBoundary::reregister_ordered_member(
                &host.inner.radio,
                &lease,
                2,
                membership(fence_member)
            )
        )
        .await
        .is_err()
    );
    assert!(consumer.routing_paused());
    assert!(matches!(
        ubm_desktop::RadioBoundary::release_ordered_source(
            &host.inner.radio,
            &ubm_desktop::boundary::OrderedSourceCleanup::Lease(lease.clone())
        )
        .await
        .unwrap(),
        ubm_desktop::boundary::OrderedSourceRelease::Pending
    ));
    assert_eq!(radio.count(crate::RequestKind::OrderedRemove), 0);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(10),
            consumer.drain_to_empty()
        )
        .await
        .is_err()
    );
    assert_eq!(drains.load(Ordering::SeqCst), 3);
    let held = radio.held_of(crate::RequestKind::OrderedRegister);
    assert_eq!(held.len(), 1);
    assert_eq!(
        radio.answer(held[0], RadioCompletion::OrderedRegistered(new_member)),
        crate::CompletionStatus::Late
    );
    let rebound = ubm_desktop::RadioBoundary::reregister_ordered_member(
        &host.inner.radio,
        &lease,
        2,
        membership(fence_member),
    )
    .await
    .unwrap();
    assert_eq!(rebound, membership(new_member));
    host.inner
        .central
        .rebind_mobile_ordered_member(&source, 19, 2, membership(fence_member), rebound)
        .unwrap();
    ubm_desktop::RadioBoundary::resume_ordered_member(&host.inner.radio, &lease, 2, rebound)
        .await
        .unwrap();
    ubm_desktop::RadioBoundary::resume_ordered_member(&host.inner.radio, &lease, 2, rebound)
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        ubm_desktop::RadioBoundary::flush_peer_notification_source(
            &host.inner.radio,
            test_common::POLAR,
        ),
    )
    .await
    .expect("a lost barrier reply must not stall cleanup")
    .unwrap();
    let ids = crate::foreign::lock(&barrier_ids).clone();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], ids[1]);
    assert!(
        matches!(host.inner.central.poll_mobile_ordered_ingress(19, &source).unwrap(), Poll::Record(Record::Value { value, .. }) if value == [2])
    );
    assert!(
        matches!(host.inner.central.poll_mobile_ordered_ingress(19, &source).unwrap(), Poll::Record(Record::Value { value, .. }) if value == [3])
    );
    // Canceling a routing waiter cannot release a persistent rebind pause.
    consumer.pause_routing().await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(10),
            consumer.drain_to_empty()
        )
        .await
        .is_err()
    );
    assert!(consumer.routing_paused());
    let before_wake = drains.load(Ordering::SeqCst);
    // A rejected wake itself drives recovery after binding; no later
    // sensor callback is needed to make the retained terminal visible.
    host.ordered_wakes().begin(9).unwrap();
    assert_eq!(
        host.ingest(crate::RadioIngress::OrderedWakeFailure {
            generation: source.generation.clone(),
            group_id: 9
        }),
        crate::IngressStatus::Accepted
    );
    host.ordered_wakes()
        .bind(9, source.generation.clone())
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(drains.load(Ordering::SeqCst), before_wake);
    consumer.resume_routing();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if consumer.terminal_acked() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        host.inner
            .central
            .poll_mobile_ordered_ingress(19, &source)
            .unwrap(),
        Poll::Terminal(_)
    ));
}
#[test]
fn terminal_preserves_unknown_rejected_loss_and_bridge_failure() {
    use ubm_core::ordered_notifications::{LossScope, LossUnit};
    let terminal = OrderedRecord::Terminal {
        reason: OrderedEnd::Overflow,
        uncertain_tail: true,
        native_domain: None,
        native_code: None,
        failure_member: None,
        rejected: Some(OrderedRejected::Loss {
            scope: LossScope::Unknown,
            unit: LossUnit::Values,
            count: None,
            byte_count: None,
        }),
    };
    assert!(matches!(
        convert(terminal, None, MobilePlatform::Android).unwrap(),
        OrderedIngressRecord::Terminal {
            rejected_loss: Some(RejectedLoss {
                scope: LossScope::Unknown,
                unit: LossUnit::Values,
                count: None,
                bytes: None
            }),
            uncertain_tail: true,
            ..
        }
    ));
    let closed = OrderedRecord::Terminal {
        reason: OrderedEnd::Closed,
        uncertain_tail: true,
        native_domain: None,
        native_code: None,
        failure_member: None,
        rejected: None,
    };
    assert!(matches!(
        convert(
            closed,
            Some(&EndReason::Invalidated),
            MobilePlatform::Android
        )
        .unwrap(),
        OrderedIngressRecord::Terminal {
            reason: EndReason::Invalidated,
            uncertain_tail: true,
            ..
        }
    ));
    assert!(matches!(
        convert(
            OrderedRecord::BridgeFailure {
                code: "wake".into(),
                detail: "failed".into()
            },
            None,
            MobilePlatform::Android
        )
        .unwrap(),
        OrderedIngressRecord::BridgeFailure { .. }
    ));
}

#[test]
fn explicit_omission_converts_without_fabricating_overflow() {
    assert!(matches!(
        convert(
            OrderedRecord::FailureObservationsOmitted,
            None,
            MobilePlatform::Android
        )
        .unwrap(),
        OrderedIngressRecord::FailureObservationsOmitted
    ));
}

#[test]
fn unencodable_native_failure_is_rejected_before_batch_admission() {
    let record = OrderedRecord::SubsequentSourceFailure {
        failure: crate::PlatformFailure::new(crate::FailureKind::Platform, "x".repeat(65_537)),
    };
    let error = convert(record, None, MobilePlatform::Android)
        .expect_err("oversized failure field must not be admitted");
    assert_eq!(error.code(), BleErrorCode::ArgumentInvalid);
}
