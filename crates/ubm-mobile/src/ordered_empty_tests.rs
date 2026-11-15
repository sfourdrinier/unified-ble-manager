// crates/ubm-mobile/src/ordered_empty_tests.rs
use super::*;
use crate::test_common::{self, Reply, Scripted};
use std::sync::atomic::{AtomicUsize, Ordering};
use ubm_core::ordered_notifications::{LossScope, LossUnit, SourceGeneration};
use ubm_desktop::boundary::{OrderedSourceIngress, OrderedSourceLease};
use ubm_desktop::central::empty_ordered_source::EmptyOrderedPoll;

#[tokio::test]
async fn installed_pump_keeps_oversized_batch_retryable_without_source_failure() {
    let drains = Arc::new(AtomicUsize::new(0));
    let observed = drains.clone();
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::OrderedDrain { .. } => {
            observed.fetch_add(1, Ordering::SeqCst);
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: Some(525),
                records: vec![
                    OrderedRecord::Terminal {
                        reason: OrderedEnd::Closed,
                        uncertain_tail: false,
                        native_domain: None,
                        native_code: None,
                        failure_member: None,
                        rejected: None,
                    },
                    OrderedRecord::BridgeFailure {
                        code: "native".into(),
                        detail: "too large".into(),
                    },
                ],
            })
        }
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let source = SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(1),
        native_group: 525,
    };
    host.inner
        .central
        .install_empty_ordered_ingress_with_byte_limit(source.clone(), 525, 4, 1)
        .unwrap();
    host.ordered_wakes().begin(525).unwrap();
    let lease = OrderedSourceLease {
        peer_id: "peer".into(),
        connection_generation: "old".into(),
        collection_id: 525,
        source_generation: source.generation.clone(),
        native_group: 525,
        members: vec![],
        ingress: OrderedSourceIngress::CentralRetainedBatch,
    };
    host.inner.attach_ordered_source(&lease).unwrap();
    let consumer = host.ordered_consumer(&source).unwrap();
    host.ingest(crate::RadioIngress::OrderedWake {
        generation: source.generation.clone(),
        group_id: 525,
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while drains.load(Ordering::SeqCst) == 0 || host.ordered_wakes().has_inflight(525) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !consumer.failed(),
        "capacity refusal must not permanently fail the source"
    );
    for _ in 0..2 {
        assert_eq!(
            consumer
                .finish_sealed_drain()
                .await
                .unwrap_err()
                .operation(),
            "gatt.ordered.ingress.record-too-large"
        );
    }
    assert_eq!(drains.load(Ordering::SeqCst), 1);
    assert_eq!(radio.count(crate::RequestKind::OrderedAcknowledgeBatch), 0);
    let mut stale = source.clone();
    stale.generation = SourceGeneration::Counter(2);
    assert!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&stale, 525)
            .is_err()
    );
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&source, 525)
            .unwrap(),
        EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal { .. })
    ));
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&source, 525)
            .unwrap(),
        EmptyOrderedPoll::Empty
    ));
}

#[tokio::test]
async fn malformed_marker_in_empty_source_is_never_native_acknowledged() {
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::OrderedDrain { .. } => Reply::Now(RadioCompletion::OrderedRecords {
            batch_id: Some(526),
            records: vec![OrderedRecord::Marker {
                marker_id: 17,
                ordinal: 3,
            }],
        }),
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let source = SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(1),
        native_group: 526,
    };
    host.inner
        .central
        .install_empty_ordered_ingress(source.clone(), 526, 4)
        .unwrap();
    let consumer = OrderedBatchConsumer::attach(&host.inner, source.clone(), 526, true);
    assert!(consumer.drain_to_empty().await.is_err());
    assert!(consumer.drain_to_empty().await.is_err());
    assert_eq!(radio.count(crate::RequestKind::OrderedDrain), 1);
    assert_eq!(radio.count(crate::RequestKind::OrderedAcknowledgeBatch), 0);
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&source, 526)
            .unwrap(),
        EmptyOrderedPoll::Empty
    ));
}

#[tokio::test]
async fn oversized_native_record_retains_batch_without_acknowledgment() {
    let drains = Arc::new(AtomicUsize::new(0));
    let observed = drains.clone();
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::OrderedDrain { .. } => {
            observed.fetch_add(1, Ordering::SeqCst);
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: Some(74),
                records: vec![OrderedRecord::BridgeFailure {
                    code: "native".into(),
                    detail: "x".repeat(ubm_core::contracts::MAX_STREAM_BYTE_CAPACITY as usize + 1),
                }],
            })
        }
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let source = SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(1),
        native_group: 74,
    };
    host.inner
        .central
        .install_empty_ordered_ingress(source.clone(), 74, 1)
        .unwrap();
    let consumer = OrderedBatchConsumer::attach(&host.inner, source, 74, true);
    for _ in 0..2 {
        let error =
            tokio::time::timeout(std::time::Duration::from_secs(1), consumer.drain_to_empty())
                .await
                .unwrap()
                .unwrap_err();
        assert_eq!(error.operation(), "gatt.ordered.ingress.record-too-large");
    }
    assert_eq!(drains.load(Ordering::SeqCst), 1);
    assert_eq!(radio.count(crate::RequestKind::OrderedAcknowledgeBatch), 0);
}

#[tokio::test]
async fn wire_ceiling_refusal_retains_batch_without_permanently_failing_source() {
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::OrderedDrain { .. } => Reply::Now(RadioCompletion::OrderedRecords {
            batch_id: Some(75),
            records: vec![OrderedRecord::BridgeFailure {
                code: "native".into(),
                detail: "x".repeat(1_048_576),
            }],
        }),
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let source = SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(1),
        native_group: 75,
    };
    host.inner
        .central
        .install_empty_ordered_ingress(source.clone(), 75, 1)
        .unwrap();
    let consumer = OrderedBatchConsumer::attach(&host.inner, source, 75, true);
    let error = consumer.drain_to_empty().await.unwrap_err();
    assert_eq!(
        error.operation(),
        "gatt.ordered.ingress.failure-unrepresentable"
    );
    consumer.mark_failed_on_drain_error(&error);
    assert!(!consumer.failed());
    assert_eq!(
        consumer.drain_to_empty().await.unwrap_err().operation(),
        error.operation()
    );
    assert_eq!(radio.count(crate::RequestKind::OrderedDrain), 1);
    assert_eq!(radio.count(crate::RequestKind::OrderedAcknowledgeBatch), 0);
}

#[tokio::test]
async fn capacity_release_redrives_native_batch_without_another_wake() {
    let drains = Arc::new(AtomicUsize::new(0));
    let captured = drains.clone();
    let acknowledgments = Arc::new(StdMutex::new(Vec::new()));
    let captured_acks = acknowledgments.clone();
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::OrderedDrain { .. } if captured.fetch_add(1, Ordering::SeqCst) == 0 => {
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: Some(73),
                records: vec![
                    OrderedRecord::Terminal {
                        reason: OrderedEnd::Closed,
                        uncertain_tail: false,
                        native_domain: None,
                        native_code: None,
                        failure_member: None,
                        rejected: None,
                    },
                    OrderedRecord::BridgeFailure {
                        code: "first".into(),
                        detail: "first".into(),
                    },
                    OrderedRecord::BridgeFailure {
                        code: "second".into(),
                        detail: "second".into(),
                    },
                ],
            })
        }
        RadioRequest::OrderedDrain { .. } => Reply::Now(RadioCompletion::OrderedRecords {
            batch_id: None,
            records: vec![],
        }),
        RadioRequest::OrderedAcknowledgeBatch { batch_id, .. } => {
            let mut acks = crate::foreign::lock(&captured_acks);
            acks.push(*batch_id);
            if acks.len() == 1 {
                Reply::Hold
            } else {
                Reply::Now(RadioCompletion::Unit)
            }
        }
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let source = SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(1),
        native_group: 73,
    };
    host.inner
        .central
        .install_empty_ordered_ingress(source.clone(), 73, 1)
        .unwrap();
    host.ordered_wakes().begin(73).unwrap();
    let lease = OrderedSourceLease {
        peer_id: "peer".into(),
        connection_generation: "old".into(),
        collection_id: 73,
        source_generation: source.generation.clone(),
        native_group: 73,
        members: vec![],
        ingress: OrderedSourceIngress::CentralRetainedBatch,
    };
    host.inner.attach_ordered_source(&lease).unwrap();
    host.ingest(crate::RadioIngress::OrderedWake {
        generation: source.generation.clone(),
        group_id: 73,
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while drains.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(radio.count(crate::RequestKind::OrderedAcknowledgeBatch), 0);
    for expected in [None, Some("first"), Some("second")] {
        let record = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            loop {
                if let EmptyOrderedPoll::Record(record) = host
                    .inner
                    .central
                    .poll_empty_ordered_ingress(&source, 73)
                    .unwrap()
                {
                    break record;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("capacity release must resume the same batch");
        match (expected, record) {
            (None, OrderedIngressRecord::Terminal { .. }) => {}
            (Some(expected), OrderedIngressRecord::BridgeFailure { failure }) => {
                assert_eq!(failure.platform_code.as_deref(), Some(expected))
            }
            other => panic!("unexpected retained record: {other:?}"),
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while radio.count(crate::RequestKind::OrderedAcknowledgeBatch) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*crate::foreign::lock(&acknowledgments), vec![73, 73]);
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&source, 73)
            .unwrap(),
        EmptyOrderedPoll::Empty
    ));
}

#[tokio::test]
async fn zero_member_source_retains_controls_before_ack() {
    let drains = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&drains);
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::OrderedDrain { .. } if captured.fetch_add(1, Ordering::SeqCst) == 0 => {
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: Some(1),
                records: vec![
                    OrderedRecord::Loss {
                        scope: LossScope::Unknown,
                        unit: LossUnit::Values,
                        count: None,
                        byte_count: None,
                    },
                    OrderedRecord::Terminal {
                        reason: OrderedEnd::WakeFailed,
                        uncertain_tail: true,
                        native_domain: Some("test".into()),
                        native_code: Some("wake".into()),
                        failure_member: None,
                        rejected: None,
                    },
                ],
            })
        }
        RadioRequest::OrderedDrain { .. } => Reply::Now(RadioCompletion::OrderedRecords {
            batch_id: None,
            records: vec![],
        }),
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let generation = SourceGeneration::Opaque("native-source".into());
    let source = SourceKey {
        peer_id: "peer".into(),
        generation: generation.clone(),
        native_group: 7,
    };
    host.inner
        .central
        .install_empty_ordered_ingress(source.clone(), 7, 4)
        .unwrap();
    host.ordered_wakes().begin(7).unwrap();
    assert_eq!(
        host.ingest(crate::RadioIngress::OrderedWake {
            generation: generation.clone(),
            group_id: 7
        }),
        crate::IngressStatus::Accepted
    );
    let lease = OrderedSourceLease {
        peer_id: "peer".into(),
        connection_generation: "old".into(),
        collection_id: 7,
        source_generation: generation,
        native_group: 7,
        members: vec![],
        ingress: OrderedSourceIngress::CentralRetainedBatch,
    };
    host.inner.attach_ordered_source(&lease).unwrap();
    let consumer = host.ordered_consumer(&source).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !consumer.terminal_acked() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&source, 7)
            .unwrap(),
        EmptyOrderedPoll::Record(OrderedIngressRecord::Loss {
            count: None,
            bytes: None,
            ..
        })
    ));
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&source, 7)
            .unwrap(),
        EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal {
            uncertain_tail: true,
            ..
        })
    ));
    assert_eq!(radio.count(crate::RequestKind::OrderedAcknowledgeBatch), 1);
}

#[tokio::test]
async fn one_group_admission_failure_does_not_stop_another_group() {
    let second_drains = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&second_drains);
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::OrderedDrain { group_id: 7, .. } => {
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: Some(7),
                records: vec![OrderedRecord::Value {
                    member: crate::OrderedRegistration {
                        member_id: 1,
                        source_member: 1,
                        epoch: 1,
                    },
                    ordinal: 1,
                    value: vec![1],
                    delivery: ubm_desktop::ObservedDelivery::Unknown,
                    provenance: ubm_core::central::NotificationProvenance::Unknown,
                }],
            })
        }
        RadioRequest::OrderedDrain { group_id: 8, .. }
            if captured.fetch_add(1, Ordering::SeqCst) == 0 =>
        {
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: Some(8),
                records: vec![OrderedRecord::Terminal {
                    reason: OrderedEnd::WakeFailed,
                    uncertain_tail: true,
                    native_domain: Some("test".into()),
                    native_code: Some("wake".into()),
                    failure_member: None,
                    rejected: None,
                }],
            })
        }
        RadioRequest::OrderedDrain { group_id: 8, .. } => {
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: None,
                records: vec![],
            })
        }
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let session = host.open_session("isolation-link").unwrap();
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
    test_common::ok(&test_common::call(&session, "connection.connect", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"connect"}).to_string()).await);
    let generation = SourceGeneration::Counter(1);
    let mut sources = Vec::new();
    for id in [7, 8] {
        let source = SourceKey {
            peer_id: "peer".into(),
            generation: generation.clone(),
            native_group: id,
        };
        host.inner
            .central
            .install_empty_ordered_ingress(source.clone(), id, 4)
            .unwrap();
        if id == 7 {
            host.inner
                .central
                .fail_empty_ordered_ingress(
                    &source,
                    id,
                    failure_from_desktop(&failure("earlier failure")),
                )
                .unwrap();
        }
        host.ordered_wakes().begin(id).unwrap();
        let lease = OrderedSourceLease {
            peer_id: "peer".into(),
            connection_generation: "old".into(),
            collection_id: id,
            source_generation: generation.clone(),
            native_group: id,
            members: vec![],
            ingress: OrderedSourceIngress::CentralRetainedBatch,
        };
        host.inner.attach_ordered_source(&lease).unwrap();
        host.ingest(crate::RadioIngress::OrderedWake {
            generation: generation.clone(),
            group_id: id,
        });
        sources.push(source);
    }
    let first = host.ordered_consumer(&sources[0]).unwrap();
    let second = host.ordered_consumer(&sources[1]).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !first.failed() || !second.terminal_acked() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(host.ingress_failure().is_none());
    assert!(
        host.inner
            .central
            .peer_records()
            .await
            .iter()
            .any(|peer| peer.peer_id == test_common::POLAR
                && peer.connection_state == Some(ubm_core::central::ConnectionState::Connected))
    );
    assert!(first.unreported_failure().is_none());
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&sources[0], 7)
            .unwrap(),
        EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { .. })
    ));
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&sources[0], 7)
            .unwrap(),
        EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { .. })
    ));
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&sources[1], 8)
            .unwrap(),
        EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal { .. })
    ));
    assert_eq!(radio.count(crate::RequestKind::OrderedAcknowledgeBatch), 1);
}

#[tokio::test]
async fn sealed_finality_waits_for_ack_and_retains_post_terminal_control() {
    let drains = Arc::new(AtomicUsize::new(0));
    let acks = Arc::new(AtomicUsize::new(0));
    let captured_drains = drains.clone();
    let captured_acks = acks.clone();
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::OrderedDrain { .. }
            if captured_drains.fetch_add(1, Ordering::SeqCst) == 0 =>
        {
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: Some(1),
                records: vec![
                    OrderedRecord::Terminal {
                        reason: OrderedEnd::Closed,
                        uncertain_tail: true,
                        native_domain: None,
                        native_code: None,
                        failure_member: None,
                        rejected: None,
                    },
                    OrderedRecord::BridgeFailure {
                        code: "wake".into(),
                        detail: "failed".into(),
                    },
                ],
            })
        }
        RadioRequest::OrderedDrain { .. } => Reply::Now(RadioCompletion::OrderedRecords {
            batch_id: None,
            records: vec![],
        }),
        RadioRequest::OrderedAcknowledgeBatch { .. }
            if captured_acks.fetch_add(1, Ordering::SeqCst) == 0 =>
        {
            Reply::Hold
        }
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let source = SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(1),
        native_group: 9,
    };
    host.inner
        .central
        .install_empty_ordered_ingress(source.clone(), 19, 4)
        .unwrap();
    // The native owner has already acknowledged its seal before this call.
    let consumer = OrderedBatchConsumer::attach(&host.inner, source.clone(), 19, true);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(10),
            consumer.finish_sealed_drain()
        )
        .await
        .is_err()
    );
    assert!(!consumer.source_final());
    consumer.finish_sealed_drain().await.unwrap();
    assert!(consumer.source_final());
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&source, 19)
            .unwrap(),
        EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal { .. })
    ));
    let EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { failure }) = host
        .inner
        .central
        .poll_empty_ordered_ingress(&source, 19)
        .unwrap()
    else {
        panic!("post-terminal bridge failure was not retained");
    };
    assert_eq!(failure.platform_code.as_deref(), Some("wake"));
    assert_eq!(failure.detail.as_deref(), Some("failed"));
    assert_eq!(acks.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn failed_setup_byte_pressure_retains_batch_offset_until_evidence_is_polled() {
    let drains = Arc::new(AtomicUsize::new(0));
    let observed = drains.clone();
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::OrderedDrain { .. } if observed.fetch_add(1, Ordering::SeqCst) == 0 => {
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: Some(522),
                records: vec![
                    OrderedRecord::Terminal {
                        reason: OrderedEnd::Closed,
                        uncertain_tail: false,
                        native_domain: None,
                        native_code: None,
                        failure_member: None,
                        rejected: None,
                    },
                    OrderedRecord::BridgeFailure {
                        code: "a".into(),
                        detail: "x".repeat(100),
                    },
                    OrderedRecord::BridgeFailure {
                        code: "b".into(),
                        detail: "y".repeat(100),
                    },
                ],
            })
        }
        RadioRequest::OrderedDrain { .. } => Reply::Now(RadioCompletion::OrderedRecords {
            batch_id: None,
            records: vec![],
        }),
        RadioRequest::OrderedAcknowledgeBatch { .. } => Reply::Now(RadioCompletion::Unit),
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let source = SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(1),
        native_group: 522,
    };
    host.inner
        .central
        .install_empty_ordered_ingress_with_byte_limit(source.clone(), 522, 4, 180)
        .unwrap();
    let consumer = OrderedBatchConsumer::attach(&host.inner, source.clone(), 522, true);
    assert!(!consumer.drain_to_empty().await.unwrap());
    assert_eq!(radio.count(crate::RequestKind::OrderedAcknowledgeBatch), 0);
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&source, 522)
            .unwrap(),
        EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal { .. })
    ));
    assert!(
        !consumer.drain_to_empty().await.unwrap(),
        "polling the reserved terminal must not release observation bytes"
    );
    assert!(
        matches!(host.inner.central.poll_empty_ordered_ingress(&source, 522).unwrap(), EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { failure }) if failure.platform_code.as_deref() == Some("a"))
    );
    assert!(consumer.drain_to_empty().await.unwrap());
    assert_eq!(radio.count(crate::RequestKind::OrderedAcknowledgeBatch), 1);
    assert_eq!(drains.load(Ordering::SeqCst), 2);
    assert!(
        matches!(host.inner.central.poll_empty_ordered_ingress(&source, 522).unwrap(), EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { failure }) if failure.platform_code.as_deref() == Some("b"))
    );
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&source, 522)
            .unwrap(),
        EmptyOrderedPoll::Empty
    ));
}

#[tokio::test]
async fn configured_oversized_evidence_stays_in_the_native_batch_without_ack() {
    let drains = Arc::new(AtomicUsize::new(0));
    let observed = drains.clone();
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::OrderedDrain { .. } => {
            observed.fetch_add(1, Ordering::SeqCst);
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: Some(523),
                records: vec![
                    OrderedRecord::Terminal {
                        reason: OrderedEnd::Closed,
                        uncertain_tail: false,
                        native_domain: None,
                        native_code: None,
                        failure_member: None,
                        rejected: None,
                    },
                    OrderedRecord::BridgeFailure {
                        code: "native".into(),
                        detail: "too large".into(),
                    },
                ],
            })
        }
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let source = SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(1),
        native_group: 523,
    };
    host.inner
        .central
        .install_empty_ordered_ingress_with_byte_limit(source.clone(), 523, 4, 1)
        .unwrap();
    let consumer = OrderedBatchConsumer::attach(&host.inner, source.clone(), 523, true);
    for _ in 0..2 {
        assert_eq!(
            consumer.drain_to_empty().await.unwrap_err().operation(),
            "gatt.ordered.ingress.record-too-large"
        );
    }
    assert_eq!(drains.load(Ordering::SeqCst), 1);
    assert_eq!(radio.count(crate::RequestKind::OrderedAcknowledgeBatch), 0);
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&source, 523)
            .unwrap(),
        EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal { .. })
    ));
    assert!(matches!(
        host.inner
            .central
            .poll_empty_ordered_ingress(&source, 523)
            .unwrap(),
        EmptyOrderedPoll::Empty
    ));
}

#[tokio::test]
async fn failed_setup_counts_nested_typed_evidence_and_reserves_terminal() {
    use ubm_core::ordered_notifications::{
        EndReason, Failure, FailureMetadataEntry, FailurePlatformDetail, FailureValue,
    };
    let radio = Scripted::new(Box::new(test_common::polar_responder));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let source = SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(3),
        native_group: 524,
    };
    let central = &host.inner.central;
    let leaf = |code: &str| Failure {
        code: code.into(),
        domain: String::new(),
        operation: String::new(),
        detail: None,
        platform_domain: None,
        platform_code: None,
        platform_message: None,
        platform_metadata: Default::default(),
        platform_metadata_entries: None,
        platform_cleanup: vec![],
        platform_cleanup_failures: vec![],
    };
    let mut cause = leaf("a");
    cause.platform_metadata_entries = Some(vec![
        FailureMetadataEntry {
            key: "k".into(),
            value: FailureValue::Int(7),
        },
        FailureMetadataEntry {
            key: "k".into(),
            value: FailureValue::Bool(true),
        },
    ]);
    cause.platform_cleanup.push(FailurePlatformDetail {
        domain: "D".into(),
        code: "C".into(),
        message: None,
        metadata: Default::default(),
        metadata_entries: None,
        cleanup: vec![],
        cleanup_failures: vec![leaf("z")],
    });
    cause.platform_cleanup_failures.push(leaf("q"));
    let capacity = ubm_core::ordered_notifications::failure_allocation_cost(&cause);
    central
        .install_empty_ordered_ingress_with_byte_limit(source.clone(), 524, 3, capacity)
        .unwrap();
    let observation = OrderedIngressRecord::BridgeFailure {
        failure: cause.clone(),
    };
    let terminal = OrderedIngressRecord::Terminal {
        reason: EndReason::SourceFailed(cause.clone()),
        rejection: None,
        failure_member: None,
        rejected_loss: None,
        uncertain_tail: false,
    };
    assert_eq!(
        central
            .try_admit_empty_ordered_record(&source, 524, terminal)
            .unwrap(),
        OrderedIngressReceipt::Terminalized
    );
    assert_eq!(
        central
            .try_admit_empty_ordered_record(&source, 524, observation.clone())
            .unwrap(),
        OrderedIngressReceipt::Terminalized
    );
    assert_eq!(
        central
            .try_admit_empty_ordered_record(&source, 524, observation.clone())
            .unwrap(),
        OrderedIngressReceipt::PendingCapacity
    );
    let mut stale = source.clone();
    stale.generation = SourceGeneration::Counter(2);
    assert!(
        central
            .try_admit_empty_ordered_record(&stale, 524, observation.clone())
            .is_err()
    );
    assert!(central.remove_empty_ordered_ingress(&source, 524).is_err());
    assert!(matches!(
        central.poll_empty_ordered_ingress(&source, 524).unwrap(),
        EmptyOrderedPoll::Record(OrderedIngressRecord::Terminal { .. })
    ));
    assert_eq!(
        central
            .try_admit_empty_ordered_record(&source, 524, observation.clone())
            .unwrap(),
        OrderedIngressReceipt::PendingCapacity
    );
    assert!(
        matches!(central.poll_empty_ordered_ingress(&source, 524).unwrap(), EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { failure }) if failure == cause)
    );
    assert_eq!(
        central
            .try_admit_empty_ordered_record(&source, 524, observation)
            .unwrap(),
        OrderedIngressReceipt::Terminalized
    );
    assert!(
        matches!(central.poll_empty_ordered_ingress(&source, 524).unwrap(), EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { failure }) if failure == cause)
    );
    central.remove_empty_ordered_ingress(&source, 524).unwrap();
    assert!(central.poll_empty_ordered_ingress(&source, 524).is_err());
    central
        .install_empty_ordered_ingress_with_byte_limit(source.clone(), 524, 3, capacity - 1)
        .unwrap();
    let error = central
        .try_admit_empty_ordered_record(
            &source,
            524,
            OrderedIngressRecord::BridgeFailure { failure: cause },
        )
        .unwrap_err();
    assert_eq!(error.operation(), "gatt.ordered.ingress.record-too-large");
    central.remove_empty_ordered_ingress(&source, 524).unwrap();
}
