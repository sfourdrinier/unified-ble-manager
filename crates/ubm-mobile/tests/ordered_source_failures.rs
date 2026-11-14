//! Public mobile session projection of native ordered source failures.
mod common;
use common::{self as test_common, Reply, Scripted};
use std::sync::Arc;
use ubm_mobile::{
    MobilePlatform, OrderedEnd, OrderedRecord, OrderedRegistration,
    OrderedSourceGeneration as SourceGeneration, RadioCompletion, RadioRequest,
};

#[tokio::test]
async fn source_failures_reach_the_public_ordered_poll_with_their_actual_causes() {
    for case in 0..6 {
        run_case(case).await;
    }
}

#[tokio::test]
async fn multiple_later_failures_reach_public_poll_without_blocking_final_drain() {
    run_case(6).await;
}

#[tokio::test]
async fn overflow_after_native_ack_preserves_the_accepted_prefix_and_unknown_tail() {
    run_case(7).await;
}

#[tokio::test]
async fn typed_later_failure_and_unknown_omissions_survive_ack_seal_poll_and_cleanup() {
    run_case(8).await;
}

#[tokio::test]
async fn capacity_one_active_source_drains_multiple_observations_added_by_seal() {
    run_case(9).await;
}

#[tokio::test]
async fn capacity_one_retains_identical_and_conflicting_failures_before_native_ack() {
    run_case(10).await;
}

async fn run_case(case: usize) {
    use std::sync::Mutex;
    use ubm_mobile::{FailureKind, PlatformFailure};

    let queued = Arc::new(Mutex::new(Vec::<OrderedRecord>::new()));
    let source = queued.clone();
    let after_ack = queued.clone();
    let injected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let member = OrderedRegistration {
        member_id: 0,
        source_member: 11,
        epoch: 2,
    };
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => Reply::Now(RadioCompletion::DiscoveredScoped {
            services: test_common::polar_services(),
            identity: ubm_desktop::boundary::GattSnapshotIdentity {
                owner: "graph".into(),
                attachment: 1,
                revision: 1,
            },
        }),
        RadioRequest::OrderedOpen { .. } => Reply::Now(RadioCompletion::OrderedOpened {
            generation: SourceGeneration::Counter(7),
            members: vec![member],
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
        RadioRequest::OrderedEnableSettlement { .. } => Reply::Now(
            RadioCompletion::OrderedEnableStatus(ubm_mobile::OrderedEnableStatus::Enabled(
                ubm_desktop::ObservedDelivery::Notification,
            )),
        ),
        RadioRequest::OrderedDrain { .. } => {
            let records = std::mem::take(&mut *source.lock().unwrap());
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: (!records.is_empty()).then_some(1),
                records,
            })
        }
        RadioRequest::OrderedAcknowledgeBatch { .. }
            if (case == 7 || case == 8)
                && !injected.swap(true, std::sync::atomic::Ordering::SeqCst) =>
        {
            let mut records = after_ack.lock().unwrap();
            if case == 8 {
                records.push(OrderedRecord::SubsequentSourceFailure {
                    failure: PlatformFailure {
                        gatt_status: Some(257),
                        dispatched: true,
                        ..PlatformFailure::new(
                            FailureKind::GattStatus,
                            "Android notification source failure",
                        )
                    },
                });
                records.push(OrderedRecord::FailureObservationsOmitted);
            } else {
                records.push(OrderedRecord::Terminal {
                    reason: OrderedEnd::Overflow,
                    uncertain_tail: true,
                    native_domain: None,
                    native_code: None,
                    failure_member: None,
                    rejected: None,
                });
            }
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::OrderedSeal { .. }
            if case == 9 && !injected.swap(true, std::sync::atomic::Ordering::SeqCst) =>
        {
            let mut records = after_ack.lock().unwrap();
            for status in [257, 258] {
                records.push(OrderedRecord::SubsequentSourceFailure {
                    failure: PlatformFailure {
                        gatt_status: Some(status),
                        ..PlatformFailure::new(
                            FailureKind::GattStatus,
                            "Android notification source failure",
                        )
                    },
                });
            }
            Reply::Now(RadioCompletion::Unit)
        }
        _ => test_common::polar_responder(request),
    }));
    let platform = if case == 4 {
        MobilePlatform::Apple
    } else {
        MobilePlatform::Android
    };
    let (host, _) = test_common::open(&radio, platform).await;
    let session = host.open_session("source-failure").unwrap();
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
            ..ubm_mobile::Advertisement::default()
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
    let open = serde_json::json!({"wireVersion":2,"failureMetadataVersion":1,"peerId":test_common::POLAR,"collectionId":group,"connectionGeneration":connected["connectionGeneration"],"databaseGeneration":discovered["databaseGeneration"],"publicConnectionGeneration":"public-connection","publicDatabaseGeneration":"public-database","members":[{"memberId":"0","publicMemberId":"a","selector":test_common::selector(),"requestedDelivery":"prefer-notification"}],"maxItems":8,"maxBytes":4096,"maxControls":if case == 6 || case == 9 || case == 10 { 1 } else { 8 },"operationId":"open"});
    test_common::ok(&test_common::call(&session, "gatt.ordered.open", &open.to_string()).await);
    let record = match case {
        0 | 5 | 6 | 7 | 8 | 9 | 10 => OrderedRecord::SourceFailed {
            uncertain_tail: true,
            failure: PlatformFailure::new(
                FailureKind::PermissionDenied,
                "Bluetooth permission denied while maintaining notification source",
            ),
            native_domain: None,
            native_code: None,
            failure_member: Some(member),
            rejected: None,
        },
        1 => OrderedRecord::SourceFailed {
            uncertain_tail: true,
            failure: PlatformFailure {
                gatt_status: Some(42),
                ..PlatformFailure::new(
                    FailureKind::GattStatus,
                    "Android notification source GATT failure",
                )
            },
            native_domain: Some("android".into()),
            native_code: Some("42".into()),
            failure_member: Some(member),
            rejected: None,
        },
        2 => OrderedRecord::SourceFailed {
            uncertain_tail: true,
            failure: PlatformFailure::new(
                FailureKind::Platform,
                "notification delivery may have been interrupted by an abandoned disable",
            ),
            native_domain: None,
            native_code: None,
            failure_member: Some(member),
            rejected: None,
        },
        3 => OrderedRecord::Terminal {
            reason: OrderedEnd::SourceFailed,
            uncertain_tail: true,
            native_domain: None,
            native_code: None,
            failure_member: Some(member),
            rejected: None,
        },
        _ => OrderedRecord::Terminal {
            reason: OrderedEnd::SourceFailed,
            uncertain_tail: true,
            native_domain: Some("CBErrorDomain".into()),
            native_code: Some("7".into()),
            failure_member: Some(member),
            rejected: None,
        },
    };
    if case == 7 {
        queued.lock().unwrap().push(OrderedRecord::Value {
            member,
            ordinal: 1,
            value: vec![7, 8],
            delivery: ubm_desktop::ObservedDelivery::Notification,
            provenance: ubm_mobile::OrderedProvenance::Notification,
        });
    }
    queued.lock().unwrap().push(record);
    if case == 10 {
        for status in [257, 257, 258, 257] {
            queued
                .lock()
                .unwrap()
                .push(OrderedRecord::SubsequentSourceFailure {
                    failure: PlatformFailure {
                        gatt_status: Some(status),
                        ..PlatformFailure::new(
                            FailureKind::GattStatus,
                            "Android notification source failure",
                        )
                    },
                });
        }
    }
    if case == 5 || case == 6 {
        queued
            .lock()
            .unwrap()
            .push(OrderedRecord::SubsequentSourceFailure {
                failure: PlatformFailure {
                    gatt_status: Some(257),
                    ..PlatformFailure::new(
                        FailureKind::GattStatus,
                        "Android notification source failure",
                    )
                },
            });
        if case == 6 {
            queued
                .lock()
                .unwrap()
                .push(OrderedRecord::SubsequentSourceFailure {
                    failure: PlatformFailure {
                        gatt_status: Some(258),
                        ..PlatformFailure::new(
                            FailureKind::GattStatus,
                            "Android notification source failure",
                        )
                    },
                });
        }
    }
    host.ingest(ubm_mobile::RadioIngress::OrderedWake {
        generation: SourceGeneration::Counter(7),
        group_id: group.parse().unwrap(),
    });
    let args = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group});
    if case == 7 {
        let prefix = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let value = test_common::ok(
                    &test_common::call(&session, "gatt.ordered.poll", &args.to_string()).await,
                );
                if !value["deliveryId"].is_null() {
                    break value;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let value: serde_json::Value =
            serde_json::from_str(prefix["record"].as_str().unwrap()).unwrap();
        assert_eq!(value["kind"], "value");
        assert_eq!(value["bytes"], serde_json::json!([7, 8]));
        let ack = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"deliveryId":prefix["deliveryId"]});
        test_common::ok(
            &test_common::call(&session, "gatt.ordered.ack-delivery", &ack.to_string()).await,
        );
    }
    let mut observations = Vec::new();
    let mut observation_ids = Vec::new();
    let delivery = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let value = test_common::ok(
                &test_common::call(&session, "gatt.ordered.poll", &args.to_string()).await,
            );
            if !value["deliveryId"].is_null() {
                let record: serde_json::Value = serde_json::from_str(value["record"].as_str().unwrap()).unwrap();
                if record["kind"] == "terminal" { break value; }
                let replay = test_common::ok(&test_common::call(&session, "gatt.ordered.poll", &args.to_string()).await);
                assert_eq!(replay, value, "each observation must replay until acknowledged");
                assert!(!observation_ids.contains(&value["deliveryId"]), "distinct observations reused a delivery identity");
                observation_ids.push(value["deliveryId"].clone());
                if case == 10 && observations.len() <= 1 {
                    assert_eq!(radio.count(ubm_mobile::RequestKind::OrderedAcknowledgeBatch), 0,
                        "native batch was acknowledged before its retained failures were admitted");
                }
                let ack = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"deliveryId":value["deliveryId"]});
                test_common::ok(&test_common::call(&session, "gatt.ordered.ack-delivery", &ack.to_string()).await);
                observations.push(record);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let record: serde_json::Value =
        serde_json::from_str(delivery["record"].as_str().unwrap()).unwrap();
    assert_eq!(record["kind"], "terminal");
    assert_eq!(record["reason"]["kind"], "source-failed");
    assert_eq!(record["uncertainTail"], true);
    assert!(record["sourceFailureMember"].is_null());
    let error = &record["reason"]["error"];
    assert!(
        !observations.is_empty(),
        "source failure observation missing"
    );
    let source_observation = observations.remove(0);
    assert_eq!(source_observation["kind"], "failure-observation");
    assert_eq!(source_observation["sourceFailureMember"]["member"], "11");
    assert_eq!(source_observation["sourceFailureMember"]["epoch"], "2");
    assert_eq!(source_observation["tailUncertain"], true);
    assert_eq!(&source_observation["failure"], error);
    match case {
        0 | 5 | 6 | 7 | 8 | 9 | 10 => {
            assert_eq!(error["code"], "permission.denied");
            assert!(error["platformCode"].is_null());
            assert!(
                error["detail"]
                    .as_str()
                    .unwrap()
                    .contains("Bluetooth permission denied")
            );
            if case == 5 || case == 6 || case == 8 || case == 9 {
                let later = &observations[0]["failure"];
                assert_eq!(later["code"], "platform.failure");
                assert_eq!(later["platformDomain"], "android");
                assert_eq!(later["platformCode"], "257");
                assert_eq!(
                    later["platformMetadata"]["androidGattStatus"]["value"],
                    "257"
                );
                assert_eq!(observations[0]["kind"], "failure-observation");
                if case == 6 || case == 9 {
                    assert_eq!(observations[1]["failure"]["platformCode"], "258");
                }
                if case == 8 {
                    assert_eq!(observations[1]["kind"], "failure-observations-omitted");
                }
            }
            if case == 7 {
                assert_eq!(observations[0]["kind"], "failure-observation");
                assert_eq!(observations[0]["tailUncertain"], true);
            }
            if case == 10 {
                assert_eq!(observations.len(), 4);
                let codes: Vec<_> = observations
                    .iter()
                    .map(|entry| entry["failure"]["platformCode"].as_str().unwrap())
                    .collect();
                assert_eq!(codes, ["257", "257", "258", "257"]);
                assert_eq!(
                    radio.count(ubm_mobile::RequestKind::OrderedAcknowledgeBatch),
                    1
                );
            }
        }
        1 => {
            assert_eq!(error["code"], "platform.failure");
            assert_eq!(error["platformDomain"], "android");
            assert_eq!(error["platformCode"], "42");
            assert_eq!(
                error["platformMetadata"]["androidGattStatus"]["value"],
                "42"
            );
        }
        2 => {
            assert_eq!(error["code"], "platform.failure");
            assert!(error["platformCode"].is_null());
            assert!(
                error["detail"]
                    .as_str()
                    .unwrap()
                    .contains("abandoned disable")
            );
        }
        3 => {
            assert_eq!(error["code"], "platform.failure");
            assert!(error["detail"].is_null());
            assert!(error["platformCode"].is_null());
        }
        _ => {
            assert_eq!(error["code"], "platform.failure");
            assert_eq!(error["platformDomain"], "CBErrorDomain");
            assert_eq!(error["platformCode"], "7");
        }
    }
    if case == 6 || case == 8 {
        let replay = test_common::ok(
            &test_common::call(&session, "gatt.ordered.poll", &args.to_string()).await,
        );
        assert_eq!(replay, delivery, "published terminal must replay unchanged");
        let close = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"operationId":"close"});
        test_common::ok(
            &test_common::call(&session, "gatt.ordered.close", &close.to_string()).await,
        );
        let discard_args = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"deliveryId":delivery["deliveryId"]});
        let receipt = test_common::ok(
            &test_common::call(&session, "gatt.ordered.discard", &discard_args.to_string()).await,
        );
        let evidence: serde_json::Value =
            serde_json::from_str(receipt["evidence"].as_str().unwrap()).unwrap();
        assert!(evidence["secondaryFailure"].is_null());
        assert_eq!(evidence["additionalFailureCount"], "0");
        assert_eq!(evidence["failureObservationsOmitted"], false);
        let again = test_common::ok(
            &test_common::call(&session, "gatt.ordered.poll-discard", &args.to_string()).await,
        );
        assert_eq!(receipt, again);
        let ack = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"deliveryId":receipt["receiptId"]});
        test_common::ok(
            &test_common::call(&session, "gatt.ordered.ack-discard", &ack.to_string()).await,
        );
        if case == 8 {
            let final_close = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"operationId":"close-after-discard"});
            let response =
                test_common::call(&session, "gatt.ordered.close", &final_close.to_string()).await;
            let envelope: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(
                envelope["ok"],
                true,
                "close response: {response}; requests: {:?}",
                radio.kinds()
            );
            let released = test_common::ok(&response);
            assert_eq!(released, "released");
            test_common::ok(
                &test_common::call(&session, "gatt.ordered.ack-group", &args.to_string()).await,
            );
            let requests = radio.requests.lock().unwrap();
            assert!(
                requests
                    .iter()
                    .any(|request| matches!(request, RadioRequest::OrderedSeal { .. }))
            );
            assert!(
                requests
                    .iter()
                    .any(|request| matches!(request, RadioRequest::OrderedRemove { .. }))
            );
        }
    } else if case == 5 || case == 10 {
        if case == 10 {
            let replay = test_common::ok(
                &test_common::call(&session, "gatt.ordered.poll", &args.to_string()).await,
            );
            assert_eq!(
                replay, delivery,
                "terminal identity and evidence must replay until acknowledged"
            );
        }
        let ack = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"deliveryId":delivery["deliveryId"]});
        test_common::ok(
            &test_common::call(&session, "gatt.ordered.ack-delivery", &ack.to_string()).await,
        );
        if case == 10 {
            let close = serde_json::json!({"peerId":test_common::POLAR,"collectionId":group,"operationId":"close"});
            let released = test_common::ok(
                &test_common::call(&session, "gatt.ordered.close", &close.to_string()).await,
            );
            assert_eq!(released, "released");
            test_common::ok(
                &test_common::call(&session, "gatt.ordered.ack-group", &args.to_string()).await,
            );
            assert_eq!(radio.count(ubm_mobile::RequestKind::OrderedSeal), 1);
            assert_eq!(radio.count(ubm_mobile::RequestKind::OrderedRemove), 1);
            assert_eq!(
                radio.count(ubm_mobile::RequestKind::OrderedAcknowledgeBatch),
                1
            );
        }
    }
}
