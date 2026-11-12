// crates/ubm-mobile/src/ordered_late_open_tests.rs
use super::*;
use crate::test_common::{self, Reply, Scripted};
use std::sync::atomic::{AtomicUsize, Ordering};
use ubm_core::central::PathSelector;
use ubm_core::ordered_notifications::{LossScope, LossUnit, Poll, Record, SourceGeneration};
use ubm_desktop::{OpControl, OrderedClaimRequest};

#[tokio::test]
async fn late_open_after_database_change_quarantines_old_source_and_cleans_up() {
    let discoveries = Arc::new(AtomicUsize::new(0));
    let discovery_count = discoveries.clone();
    let queue = Arc::new(StdMutex::new(Vec::<OrderedRecord>::new()));
    let source_queue = queue.clone();
    let next_batch = Arc::new(AtomicUsize::new(1));
    let source_next_batch = next_batch.clone();
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => {
            let revision = discovery_count.fetch_add(1, Ordering::SeqCst) as u64 + 1;
            Reply::Now(RadioCompletion::DiscoveredScoped {
                services: test_common::polar_services(),
                identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "native-graph".into(),
                    attachment: 1,
                    revision,
                },
            })
        }
        RadioRequest::OrderedOpen { .. } => {
            crate::foreign::lock(&source_queue).push(OrderedRecord::Loss {
                scope: LossScope::Unknown,
                unit: LossUnit::Values,
                count: None,
                byte_count: None,
            });
            Reply::Hold
        }
        RadioRequest::OrderedRecoverOpen { group_id, .. } => {
            Reply::Now(RadioCompletion::OrderedOpenFailed {
                failure: crate::PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "open remains pending",
                ),
                ownership: crate::OrderedOpenOwnership::Indeterminate {
                    recovery_key: *group_id,
                },
            })
        }
        RadioRequest::OrderedAcknowledgeOpen { version, .. } => {
            assert_eq!(*version, crate::ORDERED_ACKNOWLEDGE_OPEN_VERSION);
            Reply::Now(RadioCompletion::Unit)
        }
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
            crate::foreign::lock(&source_queue).push(OrderedRecord::BridgeFailure {
                code: "bridge-1".into(),
                detail: "first".into(),
            });
            crate::foreign::lock(&source_queue).push(OrderedRecord::BridgeFailure {
                code: "bridge-2".into(),
                detail: "second".into(),
            });
            crate::foreign::lock(&source_queue).push(OrderedRecord::BridgeFailure {
                code: "bridge-3".into(),
                detail: "third".into(),
            });
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::OrderedDrain { .. } => {
            let records = std::mem::take(&mut *crate::foreign::lock(&source_queue));
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: if records.is_empty() {
                    None
                } else {
                    Some(source_next_batch.fetch_add(1, Ordering::SeqCst) as u64)
                },
                records,
            })
        }
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let session = host.open_session("late-open").unwrap();
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
    let connection = test_common::ok(&test_common::call(&session, "connection.connect", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"connect"}).to_string()).await);
    let database = test_common::ok(&test_common::call(&session, "gatt.discover", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"discover-1"}).to_string()).await);
    let connection_generation = connection["connectionGeneration"].as_str().unwrap();
    let database_generation = database["databaseGeneration"].as_str().unwrap();
    let central = &host.inner.central;
    let id = central.reserve_ordered_batch(test_common::POLAR).unwrap();
    let requests = [OrderedClaimRequest {
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
    }];
    let ctl = OpControl::unbounded().with_connection_lease(format!("s{}:lease-1", session.id()));
    let opening = tokio::time::timeout(
        std::time::Duration::from_millis(30),
        central.open_reserved_ordered_batch(
            test_common::POLAR,
            id,
            connection_generation,
            database_generation,
            &requests,
            8,
            4096,
            8,
            ctl,
        ),
    )
    .await;
    assert!(opening.is_err(), "unexpected open completion: {opening:?}");
    let held = radio.held_of(crate::RequestKind::OrderedOpen);
    assert_eq!(held.len(), 1);
    assert_eq!(
        radio.answer(held[0], RadioCompletion::Unit),
        crate::CompletionStatus::Mismatched
    );
    assert!(
        host.inner
            .radio
            .retained_ordered_open(id)
            .is_some_and(|entry| entry.completion.is_none()),
        "wrong-shaped callback cannot become the retained source receipt"
    );
    host.ingest(crate::RadioIngress::ServicesChanged {
        peer_id: test_common::POLAR.into(),
    });
    test_common::drain_until(&session, |records| {
        !test_common::of_type(records, "db-changed").is_empty()
    })
    .await;
    let replacement_args = serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"discover-2"}).to_string();
    let replacement = test_common::ok(
        &tokio::time::timeout(
            std::time::Duration::from_secs(2),
            test_common::call(&session, "gatt.discover", &replacement_args),
        )
        .await
        .expect("replacement discovery blocked"),
    );
    assert_ne!(
        replacement["databaseGeneration"],
        database["databaseGeneration"]
    );
    let native_generation = SourceGeneration::Counter(77);
    let failure =
        crate::PlatformFailure::new(crate::FailureKind::GattStatus, "partial native open");
    assert_eq!(
        radio.answer(
            held[0],
            RadioCompletion::OrderedOpenFailed {
                failure,
                ownership: crate::OrderedOpenOwnership::Created {
                    generation: native_generation.clone(),
                    members: vec![]
                }
            }
        ),
        crate::CompletionStatus::Late
    );
    assert!(matches!(
        host.inner.radio.retained_ordered_open(id).and_then(|entry| entry.completion),
        Some(RadioCompletion::OrderedOpenFailed {
            ownership: crate::OrderedOpenOwnership::Created { generation, .. },
            ..
        }) if generation == native_generation
    ));
    host.ingest(crate::RadioIngress::OrderedWake {
        generation: native_generation,
        group_id: id,
    });
    let loss = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match central
                .poll_retained_ordered_batch(test_common::POLAR, id)
                .await
            {
                Ok(Poll::Record(record)) => break record,
                Ok(Poll::Empty) => tokio::task::yield_now().await,
                other => panic!("unexpected quarantine poll: {other:?}"),
            }
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        loss,
        Record::Loss {
            scope: LossScope::Unknown,
            count: None,
            bytes: None,
            ..
        }
    ));
    assert!(matches!(
        central
            .retry_retained_ordered_batch(test_common::POLAR, id, OpControl::budget_ms(1000))
            .await
            .unwrap(),
        ubm_desktop::central::OrderedCleanupOutcome::PendingDrain
    ));
    let mut observation = central
        .poll_retained_ordered_batch(test_common::POLAR, id)
        .await
        .unwrap();
    for expected in ["bridge-1", "bridge-2", "bridge-3"] {
        assert!(
            matches!(&observation, Poll::Record(Record::FailureObservation {
            failure: Some(failure), ..
        }) if failure.platform_code.as_deref() == Some(expected)),
            "unexpected poll: {observation:?}"
        );
        observation = central
            .poll_retained_ordered_batch(test_common::POLAR, id)
            .await
            .unwrap();
    }
    assert!(
        matches!(observation, Poll::Terminal(_)),
        "unexpected poll: {observation:?}"
    );
    assert_eq!(radio.count(crate::RequestKind::OrderedSeal), 1);
    assert!(matches!(
        central
            .retry_retained_ordered_batch(test_common::POLAR, id, OpControl::budget_ms(1000))
            .await
            .unwrap(),
        ubm_desktop::central::OrderedCleanupOutcome::Released
    ));
    assert!(radio.count(crate::RequestKind::OrderedAcknowledgeBatch) >= 2);
    assert_eq!(radio.count(crate::RequestKind::OrderedRemove), 1);
    let peers = central.peer_records().await;
    assert!(peers.iter().any(|peer| peer.peer_id == test_common::POLAR
        && peer.database_generation.as_deref() == replacement["databaseGeneration"].as_str()));
}
