//! Synthetic session regression for a created source with no member routes.
mod common;
use common::{self as test_common, Reply, Scripted};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use ubm_core::ordered_notifications::{LossScope, LossUnit};
use ubm_mobile::{
    FailureKind, MobilePlatform, OrderedEnd, OrderedOpenOwnership, OrderedRecord, OrderedRejected,
    OrderedSourceGeneration, PlatformFailure, RadioCompletion, RadioRequest,
};

fn late_records(byte_pressure: bool) -> Vec<OrderedRecord> {
    let mut records = Vec::new();
    for status in [257, 258, 259] {
        records.push(OrderedRecord::SubsequentSourceFailure {
            failure: PlatformFailure {
                gatt_status: Some(status),
                ..PlatformFailure::new(
                    FailureKind::GattStatus,
                    if byte_pressure {
                        "x".repeat(260)
                    } else {
                        "late failure".into()
                    },
                )
            },
        });
    }
    records.push(OrderedRecord::Terminal {
        reason: OrderedEnd::Overflow,
        uncertain_tail: true,
        native_domain: None,
        native_code: None,
        failure_member: None,
        rejected: Some(OrderedRejected::Loss {
            scope: LossScope::Unknown,
            unit: LossUnit::Values,
            count: Some(5),
            byte_count: Some(12),
        }),
    });
    records.push(OrderedRecord::FailureObservationsOmitted);
    records
}

#[tokio::test]
async fn failed_empty_setup_drains_one_bounded_batch() {
    run_case(false, false, false, false, false).await;
}

#[tokio::test]
async fn failed_empty_setup_drains_failures_and_overflow_after_seal() {
    run_case(true, false, false, false, false).await;
}

#[tokio::test]
async fn full_preterminal_batch_drains_and_cleanup_releases() {
    run_case(false, true, false, false, false).await;
}

#[tokio::test]
async fn conflicting_exact_rejections_survive_final_drain() {
    run_case(true, false, true, false, false).await;
}

#[tokio::test]
async fn byte_pressure_preserves_delivery_replay_and_cleanup() {
    run_case(true, false, false, true, false).await;
}

#[tokio::test]
async fn oversized_failure_is_reported_after_replaying_prior_delivery() {
    run_case(false, false, false, true, true).await;
}

async fn run_case(
    late_on_seal: bool,
    saturate: bool,
    conflict: bool,
    byte_pressure: bool,
    oversized: bool,
) {
    let queued = Arc::new(Mutex::new(Vec::<OrderedRecord>::new()));
    let source = queued.clone();
    let batch = Arc::new(AtomicUsize::new(1));
    let next_batch = batch.clone();
    let oversized_batch = Arc::new(AtomicUsize::new(0));
    let oversized_batch_id = oversized_batch.clone();
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => Reply::Now(RadioCompletion::DiscoveredScoped {
            services: test_common::polar_services(),
            identity: ubm_desktop::boundary::GattSnapshotIdentity {
                owner: "graph".into(),
                attachment: 1,
                revision: 1,
            },
        }),
        RadioRequest::OrderedOpen { .. } => Reply::Now(RadioCompletion::OrderedOpenFailed {
            failure: PlatformFailure::new(FailureKind::Platform, "partial open"),
            ownership: OrderedOpenOwnership::Created {
                generation: OrderedSourceGeneration::Counter(7),
                members: vec![],
            },
        }),
        RadioRequest::OrderedBarrier { barrier_id, .. } => {
            source.lock().unwrap().push(OrderedRecord::Barrier {
                barrier_id: *barrier_id,
                uncertain_tail: false,
            });
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::OrderedClose { .. } => {
            let mut records = source.lock().unwrap();
            records.push(OrderedRecord::Loss {
                scope: LossScope::Unknown,
                unit: LossUnit::Values,
                count: Some(3),
                byte_count: Some(9),
            });
            if saturate {
                records.push(OrderedRecord::Loss {
                    scope: LossScope::Unknown,
                    unit: LossUnit::Values,
                    count: Some(4),
                    byte_count: Some(10),
                });
            }
            records.push(OrderedRecord::Terminal {
                reason: OrderedEnd::Closed,
                uncertain_tail: false,
                native_domain: None,
                native_code: None,
                failure_member: None,
                rejected: None,
            });
            if !late_on_seal {
                let mut late = if oversized {
                    vec![OrderedRecord::BridgeFailure {
                        code: "native".into(),
                        detail: "x".repeat(3000),
                    }]
                } else {
                    late_records(byte_pressure)
                };
                if saturate {
                    late.retain(|record| {
                        !matches!(
                            record,
                            OrderedRecord::FailureObservationsOmitted
                                | OrderedRecord::Terminal { .. }
                        )
                    });
                }
                records.extend(late);
            }
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::OrderedSeal { .. } if late_on_seal => {
            let mut records = source.lock().unwrap();
            records.extend(late_records(byte_pressure));
            if conflict {
                for (scope, count, bytes) in
                    [(LossScope::Group, 6, 14), (LossScope::Unknown, 5, 12)]
                {
                    records.push(OrderedRecord::Terminal {
                        reason: OrderedEnd::Overflow,
                        uncertain_tail: true,
                        native_domain: None,
                        native_code: None,
                        failure_member: None,
                        rejected: Some(OrderedRejected::Loss {
                            scope,
                            unit: LossUnit::Values,
                            count: Some(count),
                            byte_count: Some(bytes),
                        }),
                    });
                }
            }
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::OrderedDrain { .. } => {
            let records = std::mem::take(&mut *source.lock().unwrap());
            let batch_id =
                (!records.is_empty()).then(|| next_batch.fetch_add(1, Ordering::SeqCst) as u64);
            if oversized && records.iter().any(|record| matches!(record, OrderedRecord::BridgeFailure { detail, .. } if detail.len() == 3000)) {
                oversized_batch_id.store(batch_id.unwrap() as usize, Ordering::SeqCst);
            }
            Reply::Now(RadioCompletion::OrderedRecords { batch_id, records })
        }
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("empty-failed-setup").unwrap();
    test_common::ok(
        &test_common::call(
            &session,
            "scan.start",
            r#"{"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}"#,
        )
        .await,
    );
    host.ingest(ubm_mobile::RadioIngress::Advertisement(
        ubm_mobile::Advertisement {
            peer_id: test_common::POLAR.into(),
            service_uuids: vec!["180D".into()],
            ..Default::default()
        },
    ));
    test_common::drain_until(&session, |records| {
        !test_common::of_type(records, "adv").is_empty()
    })
    .await;
    let connected = test_common::ok(&test_common::call(&session, "connection.connect", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"connect"}).to_string()).await);
    let discovered = test_common::ok(&test_common::call(&session, "gatt.discover", &serde_json::json!({"peerId":test_common::POLAR,"lease":"lease-1","operationId":"discover"}).to_string()).await);
    let reserved = test_common::ok(
        &test_common::call(
            &session,
            "gatt.ordered.reserve",
            &serde_json::json!({"peerId":test_common::POLAR}).to_string(),
        )
        .await,
    );
    let group = reserved["collectionId"].as_str().unwrap();
    let open = serde_json::json!({"wireVersion":2,"peerId":test_common::POLAR,"collectionId":group,"connectionGeneration":connected["connectionGeneration"],"databaseGeneration":discovered["databaseGeneration"],"publicConnectionGeneration":"public-connection","publicDatabaseGeneration":"public-database","members":[{"memberId":"0","publicMemberId":"a","selector":test_common::selector(),"requestedDelivery":"prefer-notification"}],"maxItems":8,"maxBytes":if byte_pressure { 1800 } else { 2048 },"maxControls":if byte_pressure { 8 } else { 1 },"operationId":"open"});
    let failed_open = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        test_common::call(&session, "gatt.ordered.open", &open.to_string()),
    )
    .await
    .expect("failed open must settle");
    test_common::failure(&failed_open);
    let args = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group});
    let close = |attempt| serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"operationId":format!("close-{attempt}")});
    if oversized {
        host.ingest(ubm_mobile::RadioIngress::OrderedWake {
            generation: OrderedSourceGeneration::Counter(7),
            group_id: group.parse().unwrap(),
        });
        let first = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let _ =
                    test_common::call(&session, "gatt.ordered.close", &close(0).to_string()).await;
                let result =
                    test_common::call(&session, "gatt.ordered.poll", &args.to_string()).await;
                let value = test_common::ok(&result);
                if value["deliveryId"].is_string() {
                    break value;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("prior loss must be delivered");
        let record: serde_json::Value =
            serde_json::from_str(first["record"].as_str().unwrap()).unwrap();
        assert_eq!(record["kind"], "loss");
        assert_eq!(
            test_common::ok(
                &test_common::call(&session, "gatt.ordered.poll", &args.to_string()).await
            ),
            first
        );
        let ack = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"deliveryId":first["deliveryId"]});
        test_common::ok(
            &test_common::call(&session, "gatt.ordered.ack-delivery", &ack.to_string()).await,
        );
        test_common::ok(
            &test_common::call(&session, "gatt.ordered.ack-delivery", &ack.to_string()).await,
        );
        let result = test_common::call(&session, "gatt.ordered.poll", &args.to_string()).await;
        let (error, _) = test_common::failure(&result);
        assert_eq!(error["operation"], "gatt.ordered.ingress.record-too-large");
        let retry = test_common::call(&session, "gatt.ordered.poll", &args.to_string()).await;
        let (retry_error, _) = test_common::failure(&retry);
        assert_eq!(retry_error, error);
        let batch_id = oversized_batch.load(Ordering::SeqCst) as u64;
        assert_ne!(batch_id, 0);
        assert!(!radio.requests.lock().unwrap().iter().any(|request| matches!(request,
            RadioRequest::OrderedAcknowledgeBatch { batch_id: acked, .. } if *acked == batch_id
        )), "the oversized native batch must stay unacknowledged");
        let status = test_common::ok(
            &test_common::call(&session, "gatt.ordered.status", &args.to_string()).await,
        );
        let generation = status["sourceGeneration"].clone();
        assert!(!generation.is_null());
        let increase = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"sourceGeneration":generation,"maxBytes":8192,"wireVersion":1});
        let mut wrong = increase.clone();
        wrong["sourceGeneration"] = serde_json::json!({"kind":"counter","value":"999"});
        test_common::failure(
            &test_common::call(
                &session,
                "gatt.ordered.increase-capacity",
                &wrong.to_string(),
            )
            .await,
        );
        let mut over = increase.clone();
        over["maxBytes"] = serde_json::json!(4_194_305);
        test_common::failure(
            &test_common::call(
                &session,
                "gatt.ordered.increase-capacity",
                &over.to_string(),
            )
            .await,
        );
        test_common::ok(
            &test_common::call(
                &session,
                "gatt.ordered.increase-capacity",
                &increase.to_string(),
            )
            .await,
        );
        let mut seen = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let _ = test_common::call(&session, "gatt.ordered.close", &close(1).to_string()).await;
                let delivery = test_common::ok(&test_common::call(&session, "gatt.ordered.poll", &args.to_string()).await);
                if let Some(id) = delivery["deliveryId"].as_str() {
                    let record: serde_json::Value = serde_json::from_str(delivery["record"].as_str().unwrap()).unwrap();
                    let terminal = record["kind"] == "terminal";
                    seen.push(record);
                    let ack = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"deliveryId":id});
                    test_common::ok(&test_common::call(&session, "gatt.ordered.ack-delivery", &ack.to_string()).await);
                    if terminal { break; }
                }
                tokio::task::yield_now().await;
            }
        }).await.expect("recovered evidence and terminal must drain");
        assert_eq!(seen[0]["kind"], "failure-observation");
        assert_eq!(seen[0]["failure"]["detail"], "x".repeat(3000));
        assert!(radio.requests.lock().unwrap().iter().any(|request| matches!(request, RadioRequest::OrderedAcknowledgeBatch { batch_id: acked, .. } if *acked == batch_id)));
        assert_eq!(
            test_common::ok(
                &test_common::call(&session, "gatt.ordered.close", &close(2).to_string()).await
            ),
            "released"
        );
        assert_eq!(radio.count(ubm_mobile::RequestKind::OrderedSeal), 1);
        test_common::ok(
            &test_common::call(&session, "gatt.ordered.ack-group", &args.to_string()).await,
        );
        test_common::failure(
            &test_common::call(
                &session,
                "gatt.ordered.increase-capacity",
                &increase.to_string(),
            )
            .await,
        );
        return;
    }
    let mut seen = Vec::new();
    let mut attempts = 0;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let _ = test_common::call(&session, "gatt.ordered.close", &close(attempts).to_string()).await;
            attempts += 1;
            let delivery = test_common::ok(&test_common::call(&session, "gatt.ordered.poll", &args.to_string()).await);
            if let Some(id) = delivery["deliveryId"].as_str() {
                let record: serde_json::Value = serde_json::from_str(delivery["record"].as_str().unwrap()).unwrap();
                assert_eq!(test_common::ok(&test_common::call(&session, "gatt.ordered.poll", &args.to_string()).await), delivery);
                let terminal = record["kind"] == "terminal";
                seen.push(record);
                let ack = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"deliveryId":id});
                test_common::ok(&test_common::call(&session, "gatt.ordered.ack-delivery", &ack.to_string()).await);
                test_common::ok(&test_common::call(&session, "gatt.ordered.ack-delivery", &ack.to_string()).await);
                if terminal { break; }
            }
            tokio::task::yield_now().await;
        }
    }).await.expect("terminal and native final drain must complete");
    if conflict {
        let kinds: Vec<_> = seen
            .iter()
            .map(|record| record["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "loss",
                "failure-observation",
                "failure-observation",
                "failure-observation",
                "failure-observation",
                "failure-observations-omitted",
                "failure-observation",
                "failure-observation",
                "terminal"
            ],
            "source order and all occurrences must survive: {seen:?}"
        );
        let observations: Vec<_> = seen
            .iter()
            .filter(|record| record["kind"] == "failure-observation")
            .collect();
        assert_eq!(observations[0]["failure"]["platformCode"], "257");
        assert_eq!(observations[1]["failure"]["platformCode"], "258");
        assert_eq!(observations[2]["failure"]["platformCode"], "259");
        assert_eq!(observations[3]["rejectedLoss"]["count"], "5");
        assert_eq!(observations[4]["rejectedLoss"]["count"], "6");
        assert_eq!(observations[4]["rejectedLoss"]["scope"]["kind"], "group");
        assert_eq!(
            observations[3]["rejectedLoss"],
            observations[5]["rejectedLoss"]
        );
        assert!(seen.last().unwrap()["reason"].is_object());
    } else {
        let kinds: Vec<_> = seen
            .iter()
            .map(|record| record["kind"].as_str().unwrap())
            .collect();
        let expected = if saturate {
            vec![
                "loss",
                "loss",
                "failure-observation",
                "failure-observation",
                "failure-observation",
                "terminal",
            ]
        } else {
            vec![
                "loss",
                "failure-observation",
                "failure-observation",
                "failure-observation",
                "failure-observation",
                "failure-observations-omitted",
                "terminal",
            ]
        };
        assert_eq!(
            kinds, expected,
            "every source observation must precede terminal: {seen:?}"
        );
        assert_eq!(seen[0]["kind"], "loss");
        assert_eq!(seen[0]["loss"]["count"], "3");
        assert_eq!(seen[0]["loss"]["bytes"], "9");
        if saturate {
            assert_eq!(seen[1]["loss"]["count"], "4");
        }
        let observations: Vec<_> = seen
            .iter()
            .filter(|record| record["kind"] == "failure-observation")
            .collect();
        for (record, status) in observations.iter().take(3).zip(["257", "258", "259"]) {
            assert_eq!(record["failure"]["platformCode"], status);
        }
        if !saturate {
            assert_eq!(observations[3]["rejectedLoss"]["count"], "5");
            assert_eq!(observations[3]["rejectedLoss"]["bytes"], "12");
        }
        let terminal = seen.last().unwrap();
        assert_eq!(terminal["reason"]["kind"], "setup-failed");
        assert_eq!(terminal["additionalFailureCount"], "0");
        assert_eq!(terminal["failureCountOverflowed"], false);
    }
    let released = test_common::ok(
        &test_common::call(&session, "gatt.ordered.close", &close(attempts).to_string()).await,
    );
    assert_eq!(released, "released");
    assert_eq!(radio.count(ubm_mobile::RequestKind::OrderedSeal), 1);
    assert!(
        radio.count(ubm_mobile::RequestKind::OrderedAcknowledgeBatch)
            >= if late_on_seal { 3 } else { 2 }
    );
    test_common::ok(
        &test_common::call(&session, "gatt.ordered.ack-group", &args.to_string()).await,
    );
}
