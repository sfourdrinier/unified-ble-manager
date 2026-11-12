// crates/ubm-mobile/src/ordered_renewal_lifecycle_tests.rs
use super::*;
use crate::test_common::{self, Reply, Scripted};
use std::sync::atomic::{AtomicUsize, Ordering};
use ubm_core::central::PathSelector;
use ubm_core::ordered_notifications::{Poll, Record, SourceGeneration};
use ubm_desktop::{OpControl, OrderedClaimRequest};

#[tokio::test]
async fn service_change_during_late_registration_preserves_staged_value_and_cleanup() {
    let queue = Arc::new(StdMutex::new(Vec::<OrderedRecord>::new()));
    let source_queue = queue.clone();
    let batches = Arc::new(AtomicUsize::new(1));
    let batch_ids = batches.clone();
    let discoveries = Arc::new(AtomicUsize::new(1));
    let discovery_ids = discoveries.clone();
    let old = crate::OrderedRegistration {
        member_id: 1,
        source_member: 5,
        epoch: 3,
    };
    let new = crate::OrderedRegistration {
        member_id: 1,
        source_member: 6,
        epoch: 4,
    };
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => Reply::Now(RadioCompletion::DiscoveredScoped {
            services: test_common::polar_services(),
            identity: ubm_desktop::boundary::GattSnapshotIdentity {
                owner: "native-graph".into(),
                attachment: 1,
                revision: discovery_ids.fetch_add(1, Ordering::SeqCst) as u64,
            },
        }),
        RadioRequest::OrderedOpen { .. } => Reply::Now(RadioCompletion::OrderedOpened {
            generation: SourceGeneration::Counter(77),
            members: vec![old],
        }),
        RadioRequest::OrderedRetainClaim {
            generation, member, ..
        } => Reply::Now(RadioCompletion::OrderedClaim {
            generation: generation.clone(),
            member: *member,
        }),
        RadioRequest::OrderedEnableNotifications { .. } => Reply::Now(
            RadioCompletion::NotifyEnabled(ubm_desktop::ObservedDelivery::Notification),
        ),
        RadioRequest::OrderedRetire { member, .. } => {
            crate::foreign::lock(&source_queue).push(OrderedRecord::Fence {
                member: *member,
                uncertain_tail: true,
            });
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::OrderedRegister { .. } => Reply::Hold,
        RadioRequest::OrderedBarrier { barrier_id, .. } => {
            crate::foreign::lock(&source_queue).push(OrderedRecord::Barrier {
                barrier_id: *barrier_id,
                uncertain_tail: false,
            });
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::OrderedClose { .. } => {
            crate::foreign::lock(&source_queue).push(OrderedRecord::Terminal {
                reason: OrderedEnd::Closed,
                uncertain_tail: true,
                native_domain: None,
                native_code: None,
                failure_member: None,
                rejected: None,
            });
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::OrderedDrain { .. } => {
            let records = std::mem::take(&mut *crate::foreign::lock(&source_queue));
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: if records.is_empty() {
                    None
                } else {
                    Some(batch_ids.fetch_add(1, Ordering::SeqCst) as u64)
                },
                records,
            })
        }
        RadioRequest::OrderedEnableSettlement { .. } => Reply::Now(
            RadioCompletion::OrderedEnableStatus(crate::OrderedEnableStatus::Released(None)),
        ),
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let session = host.open_session("renewal-lifecycle").unwrap();
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
    let central = &host.inner.central;
    let group = central.reserve_ordered_batch(test_common::POLAR).unwrap();
    let request = OrderedClaimRequest {
        member_id: 1,
        selector: PathSelector {
            service_uuid: test_common::HR_SERVICE.into(),
            service_occurrence: Some(0),
            characteristic_uuid: Some(test_common::HR_MEASUREMENT.into()),
            characteristic_occurrence: Some(0),
            descriptor_uuid: None,
            descriptor_occurrence: None,
        },
        consumer: "ordered".into(),
        delivery: None,
        preference: None,
    };
    let ctl = OpControl::unbounded().with_connection_lease(format!("s{}:lease-1", session.id()));
    central
        .open_reserved_ordered_batch(
            test_common::POLAR,
            group,
            connected["connectionGeneration"].as_str().unwrap(),
            discovered["databaseGeneration"].as_str().unwrap(),
            &[request],
            8,
            128,
            8,
            ctl,
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            central.renew_ordered_member(test_common::POLAR, group, 1, OpControl::unbounded())
        )
        .await
        .is_err()
    );
    let held = radio.held_of(crate::RequestKind::OrderedRegister);
    assert_eq!(held.len(), 1);
    assert_eq!(
        radio.answer(held[0], RadioCompletion::Unit),
        crate::CompletionStatus::Mismatched
    );
    host.ingest(crate::RadioIngress::ServicesChanged {
        peer_id: test_common::POLAR.into(),
    });
    tokio::task::yield_now().await;
    assert_eq!(radio.count(crate::RequestKind::OrderedRemove), 0);
    crate::foreign::lock(&queue).push(OrderedRecord::Value {
        member: new,
        ordinal: 1,
        value: vec![42],
        delivery: ubm_desktop::ObservedDelivery::Notification,
        provenance: ubm_core::central::NotificationProvenance::Notification,
    });
    host.ingest(crate::RadioIngress::OrderedWake {
        generation: SourceGeneration::Counter(77),
        group_id: group,
    });
    assert_eq!(
        radio.answer(held[0], RadioCompletion::OrderedRegistered(new)),
        crate::CompletionStatus::Late
    );
    let records = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        test_common::drain_until(&session, |records| {
            !test_common::of_type(records, "db-changed").is_empty()
        })
        .await
    })
    .await
    .expect("service change remained blocked after late registration");
    assert!(!test_common::of_type(&records, "db-changed").is_empty());
    let mut saw_value = false;
    let mut saw_terminal = false;
    for _ in 0..20 {
        match central
            .poll_retained_ordered_batch(test_common::POLAR, group)
            .await
            .unwrap()
        {
            Poll::Record(Record::Value { value, .. }) if value == [42] => saw_value = true,
            Poll::Terminal(terminal) => {
                assert!(terminal.tail_uncertain);
                saw_terminal = true;
                break;
            }
            _ => tokio::task::yield_now().await,
        }
    }
    assert!(
        saw_value && saw_terminal,
        "staged value or uncertain terminal missing"
    );
    assert_eq!(radio.count(crate::RequestKind::OrderedRemove), 0);
    let cleanup = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        central.retry_retained_ordered_batch(test_common::POLAR, group, OpControl::budget_ms(1000)),
    )
    .await
    .expect("cleanup blocked")
    .unwrap();
    let status = central
        .ordered_batch_source_status(test_common::POLAR, group)
        .await;
    assert!(
        matches!(
            cleanup,
            ubm_desktop::central::OrderedCleanupOutcome::Released
        ),
        "unexpected cleanup outcome: {cleanup:?}, status: {status:?}, requests: {:?}",
        radio.kinds()
    );
    assert!(radio.requests.lock().unwrap().iter().any(|request| matches!(request, RadioRequest::OrderedForgetEnableReceipt { activation, .. } if activation.member == old)), "exact old physical-origin receipt was not retired");
    assert_eq!(radio.count(crate::RequestKind::OrderedRemove), 1);
    let args = serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"replacement"}).to_string();
    let replacement = test_common::ok(
        &tokio::time::timeout(
            std::time::Duration::from_secs(2),
            test_common::call(&session, "gatt.discover", &args),
        )
        .await
        .expect("replacement discovery blocked"),
    );
    assert_ne!(
        replacement["databaseGeneration"],
        discovered["databaseGeneration"]
    );
}

#[tokio::test]
async fn android_failed_source_is_sealed_when_late_renewal_is_abandoned() {
    failed_source_after_late_renewal(false).await;
}

#[tokio::test]
async fn android_failed_source_observation_precedes_subsequent_failures() {
    failed_source_after_late_renewal(true).await;
}

async fn failed_source_after_late_renewal(with_subsequent_failures: bool) {
    use ubm_core::ordered_notifications::{EndReason, Membership};
    use ubm_desktop::boundary::{
        OrderedRenewalAbandon, OrderedSourceIngress, OrderedSourceLease, RadioBoundary,
    };

    let queue = Arc::new(StdMutex::new(Vec::<OrderedRecord>::new()));
    let source_queue = queue.clone();
    let batches = AtomicUsize::new(1);
    let old = crate::OrderedRegistration {
        member_id: 1,
        source_member: 5,
        epoch: 3,
    };
    let new = crate::OrderedRegistration {
        member_id: 1,
        source_member: 6,
        epoch: 4,
    };
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => Reply::Now(RadioCompletion::DiscoveredScoped {
            services: test_common::polar_services(),
            identity: ubm_desktop::boundary::GattSnapshotIdentity {
                owner: "native-graph".into(),
                attachment: 1,
                revision: 1,
            },
        }),
        RadioRequest::OrderedOpen { .. } => Reply::Now(RadioCompletion::OrderedOpened {
            generation: SourceGeneration::Counter(77),
            members: vec![old],
        }),
        RadioRequest::OrderedRetainClaim {
            generation, member, ..
        } => Reply::Now(RadioCompletion::OrderedClaim {
            generation: generation.clone(),
            member: *member,
        }),
        RadioRequest::OrderedEnableNotifications { .. } => Reply::Now(
            RadioCompletion::NotifyEnabled(ubm_desktop::ObservedDelivery::Notification),
        ),
        RadioRequest::OrderedRetire { member, .. } => {
            crate::foreign::lock(&source_queue).push(OrderedRecord::Fence {
                member: *member,
                uncertain_tail: true,
            });
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::OrderedRegister { .. } => Reply::Hold,
        // A terminated Android source rejects barriers. Do not manufacture a
        // barrier receipt that could mask the wrong abandonment path.
        RadioRequest::OrderedBarrier { .. } => Reply::Now(RadioCompletion::Failed(
            crate::PlatformFailure::new(crate::FailureKind::Platform, "source already ended"),
        )),
        RadioRequest::OrderedDrain { .. } => {
            let records = std::mem::take(&mut *crate::foreign::lock(&source_queue));
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: (!records.is_empty())
                    .then(|| batches.fetch_add(1, Ordering::SeqCst) as u64),
                records,
            })
        }
        RadioRequest::OrderedEnableSettlement { .. } => Reply::Now(
            RadioCompletion::OrderedEnableStatus(crate::OrderedEnableStatus::Released(None)),
        ),
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let session = host.open_session("failed-renewal-lifecycle").unwrap();
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
    let central = &host.inner.central;
    let group = central.reserve_ordered_batch(test_common::POLAR).unwrap();
    let request = OrderedClaimRequest {
        member_id: 1,
        selector: PathSelector {
            service_uuid: test_common::HR_SERVICE.into(),
            service_occurrence: Some(0),
            characteristic_uuid: Some(test_common::HR_MEASUREMENT.into()),
            characteristic_occurrence: Some(0),
            descriptor_uuid: None,
            descriptor_occurrence: None,
        },
        consumer: "ordered".into(),
        delivery: None,
        preference: None,
    };
    central
        .open_reserved_ordered_batch(
            test_common::POLAR,
            group,
            connected["connectionGeneration"].as_str().unwrap(),
            discovered["databaseGeneration"].as_str().unwrap(),
            &[request],
            8,
            2048,
            8,
            OpControl::unbounded().with_connection_lease(format!("s{}:lease-1", session.id())),
        )
        .await
        .unwrap();

    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            central.renew_ordered_member(test_common::POLAR, group, 1, OpControl::unbounded()),
        )
        .await
        .is_err()
    );
    let held = radio.held_of(crate::RequestKind::OrderedRegister);
    assert_eq!(held.len(), 1);
    assert_eq!(
        radio.answer(held[0], RadioCompletion::OrderedRegistered(new)),
        crate::CompletionStatus::Late
    );
    let new_membership = central
        .renew_ordered_member(test_common::POLAR, group, 1, OpControl::unbounded())
        .await
        .unwrap();
    assert_eq!(
        new_membership,
        Membership {
            member: new.source_member,
            epoch: new.epoch
        }
    );

    // The central has rebound the exact late registration. Exercise the real
    // abandonment hook at its resumable re-entry point with that same lease.
    let lease = OrderedSourceLease {
        peer_id: test_common::POLAR.into(),
        connection_generation: connected["connectionGeneration"].as_str().unwrap().into(),
        collection_id: group,
        source_generation: SourceGeneration::Counter(77),
        native_group: group,
        members: vec![(1, new_membership)],
        ingress: OrderedSourceIngress::CentralRetainedBatch,
    };
    crate::foreign::lock(&queue).push(OrderedRecord::SourceFailed {
        uncertain_tail: true,
        failure: crate::PlatformFailure {
            gatt_status: Some(42),
            ..crate::PlatformFailure::new(crate::FailureKind::GattStatus, "Android source failed")
        },
        native_domain: Some("android".into()),
        native_code: Some("42".into()),
        failure_member: Some(new),
        rejected: None,
    });
    if with_subsequent_failures {
        for detail in ["late failure one", "late failure two"] {
            crate::foreign::lock(&queue).push(OrderedRecord::SubsequentSourceFailure {
                failure: crate::PlatformFailure::new(crate::FailureKind::Platform, detail),
            });
        }
    }
    let abandoned = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        host.inner.radio.abandon_ordered_renewal(
            &lease,
            1,
            Membership {
                member: old.source_member,
                epoch: old.epoch,
            },
            Some(new_membership),
        ),
    )
    .await
    .expect("abandonment blocked")
    .expect("abandonment failed");
    assert_eq!(
        abandoned,
        OrderedRenewalAbandon::Retired {
            tail_uncertain: true
        }
    );
    assert_eq!(radio.count(crate::RequestKind::OrderedSeal), 1);
    assert_eq!(radio.count(crate::RequestKind::OrderedBarrier), 0);
    assert_eq!(radio.count(crate::RequestKind::OrderedAcknowledgeBatch), 2);
    {
        let requests = radio.requests.lock().unwrap();
        let terminal_ack = requests.iter().position(|request| matches!(request,
            RadioRequest::OrderedAcknowledgeBatch { generation: SourceGeneration::Counter(77), group_id, batch_id: 2, .. } if *group_id == group
        )).expect("structured terminal batch was not acknowledged");
        let seal = requests.iter().position(|request| matches!(request,
            RadioRequest::OrderedSeal { generation: SourceGeneration::Counter(77), group_id, .. } if *group_id == group
        )).expect("exact source was not sealed");
        assert!(
            terminal_ack < seal,
            "source sealed before terminal acknowledgment"
        );
    }

    assert!(matches!(
        central
            .poll_retained_ordered_batch(test_common::POLAR, group)
            .await
            .unwrap(),
        Poll::Record(Record::Loss {
            scope: ubm_core::ordered_notifications::LossScope::Member(1),
            count: None,
            ..
        })
    ));
    let observation = central
        .poll_retained_ordered_batch(test_common::POLAR, group)
        .await
        .unwrap();
    let observed_failure = match observation {
        Poll::Record(Record::FailureObservation {
            failure: Some(failure),
            source_rejection: None,
            source_failure_member: Some(member),
            rejected_loss: None,
            tail_uncertain: true,
        }) if member == new_membership => failure,
        other => panic!("missing structured source observation: {other:?}"),
    };
    assert_eq!(observed_failure.platform_domain.as_deref(), Some("android"));
    assert_eq!(observed_failure.platform_code.as_deref(), Some("42"));
    assert!(
        observed_failure
            .detail
            .as_deref()
            .unwrap()
            .contains("Android source failed")
    );
    if with_subsequent_failures {
        for expected in ["late failure one", "late failure two"] {
            let observation = central
                .poll_retained_ordered_batch(test_common::POLAR, group)
                .await
                .unwrap();
            assert!(
                matches!(observation, Poll::Record(Record::FailureObservation {
                failure: Some(ref failure), ..
            }) if failure.detail.as_deref().is_some_and(|detail| detail.contains(expected))),
                "missing or reordered {expected}: {observation:?}"
            );
        }
    }
    let terminal = match central
        .poll_retained_ordered_batch(test_common::POLAR, group)
        .await
        .unwrap()
    {
        Poll::Terminal(terminal) => terminal,
        other => panic!("expected structured source terminal, got {other:?}"),
    };
    assert!(terminal.tail_uncertain);
    assert_eq!(terminal.source_failure_member, None); // retained in the preceding observation
    match terminal.reason {
        EndReason::SourceFailed(failure) => {
            assert_eq!(failure, observed_failure);
            assert_eq!(failure.platform_domain.as_deref(), Some("android"));
            assert_eq!(failure.platform_code.as_deref(), Some("42"));
            assert!(
                failure
                    .detail
                    .as_deref()
                    .unwrap()
                    .contains("Android source failed")
            );
        }
        other => panic!("unexpected terminal reason: {other:?}"),
    }
    let cleanup = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        central.retry_retained_ordered_batch(test_common::POLAR, group, OpControl::budget_ms(1000)),
    )
    .await
    .expect("cleanup blocked")
    .unwrap();
    assert_eq!(
        cleanup,
        ubm_desktop::central::OrderedCleanupOutcome::Released
    );
    assert_eq!(radio.count(crate::RequestKind::OrderedBarrier), 0);
    assert_eq!(radio.count(crate::RequestKind::OrderedRemove), 1);
    assert_eq!(
        radio.count(crate::RequestKind::OrderedForgetRemovalReceipt),
        1
    );
    assert!(radio.requests.lock().unwrap().iter().any(|request| matches!(request, RadioRequest::OrderedForgetEnableReceipt { activation, .. } if activation.member == old)));
    host.shutdown().await;
}
