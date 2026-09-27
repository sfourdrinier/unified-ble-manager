mod common;

use common::*;
use serde_json::json;
use ubm_mobile::{
    FailureKind, Instance, MobilePlatform, PlatformFailure, RadioCompletion, RadioIngress,
    RadioRequest, RequestKind,
};

fn declaration() -> String {
    json!({"onAppearance":"native", "resubscribe":[{
        "serviceUuid":HR_SERVICE,"serviceOccurrence":1,
        "characteristicUuid":HR_MEASUREMENT,"characteristicOccurrence":1
    }]})
    .to_string()
}

#[tokio::test]
async fn mobile_case_equivalent_peers_share_authority_and_canonical_radio_identity() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let engine = host.continuation();
    let lower = POLAR.to_ascii_lowercase();
    let declared = json!({"onAppearance":"native","peerId":lower}).to_string();
    let canonical = json!({"onAppearance":"native","peerId":POLAR}).to_string();
    engine.seed_declaration(&declared).unwrap();
    engine.seed_declaration(&canonical).unwrap();
    let outcome = engine.execute(&lower, &canonical).await.unwrap();
    assert_eq!(outcome["peerAddress"], POLAR);
    assert_eq!(radio.count(RequestKind::Connect), 1);
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_executor_connects_subscribes_and_replays_prepared_handoff() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let executor = host.continuation();
        let outcome = executor.execute(POLAR, &declaration()).await.unwrap();
        assert_eq!(outcome["event"], "continuation.completed");
        assert_eq!(outcome["resubscribed"], 1);
        let claim = executor.prepare_claim(256, 65536).await.unwrap();
        assert_eq!(claim["consumerCount"], 1);
        assert_eq!(executor.prepare_claim(256, 65536).await.unwrap(), claim);
        let token = claim["claimToken"].as_str().unwrap();
        assert!(executor.acknowledge_claim("wrong").await.is_err());
        let ack = executor.acknowledge_claim(token).await.unwrap();
        assert_eq!(ack["disposed"], true);
        assert_eq!(executor.acknowledge_claim(token).await.unwrap(), ack);
        assert_eq!(
            executor.prepare_claim(256, 65536).await.unwrap()["batches"],
            json!([])
        );
        host.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_declaration_has_no_radio_effect() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let executor = host.continuation();
    assert!(
        executor
            .execute(
                POLAR,
                r#"{"onAppearance":"native","resubscribe":[{"serviceUuid":"180d"}]}"#
            )
            .await
            .is_err()
    );
    assert!(radio.requests.lock().unwrap().is_empty());
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_transient_connect_failure_recovers_without_a_second_wake() {
    let mut attempts = 0;
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, RadioRequest::Connect { .. }) {
            attempts += 1;
            if attempts == 1 {
                return Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
                    FailureKind::Platform,
                    "transient initial connection refusal",
                )));
            }
        }
        polar_responder(request)
    }));
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let engine = host.continuation();
    let error = engine.execute(POLAR, &declaration()).await.unwrap_err();
    assert_eq!(error["retryability"], "caller-decides");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while radio.count(RequestKind::EnableNotifications) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accepted native order recovers initial failure without an event or JS");
    assert_eq!(radio.count(RequestKind::Connect), 2);
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_resubscription_recovers_only_missing_selector_without_a_claim() {
    const SECOND: &str = "00002a38-0000-1000-8000-00805f9b34fb";
    let mut second_attempts = 0;
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            let mut characteristic = services[0].characteristics[0].clone();
            characteristic.uuid = SECOND.to_owned();
            services[0].characteristics.push(characteristic);
            Reply::Now(RadioCompletion::Discovered(services))
        }
        RadioRequest::EnableNotifications { instance, .. }
            if instance.characteristic_uuid == SECOND =>
        {
            second_attempts += 1;
            if second_attempts == 1 {
                Reply::Now(RadioCompletion::Failed(PlatformFailure::not_dispatched(
                    FailureKind::Busy,
                    "temporary second selector queue refusal",
                )))
            } else {
                polar_responder(request)
            }
        }
        _ => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let engine = host.continuation();
    let mut order: serde_json::Value = serde_json::from_str(&declaration()).unwrap();
    let mut second = order["resubscribe"][0].clone();
    second["characteristicUuid"] = json!(SECOND);
    order["resubscribe"].as_array_mut().unwrap().push(second);
    let failure = engine.execute(POLAR, &order.to_string()).await.unwrap_err();
    assert_eq!(failure["retryability"], "caller-decides", "{failure}");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while radio.count(RequestKind::EnableNotifications) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("partial native order completes its missing selector without JS or claim");
    assert_eq!(radio.count(RequestKind::Connect), 1);
    assert_eq!(radio.count(RequestKind::Discover), 1);
    let claim = loop {
        match engine.prepare_claim(256, 65536).await {
            Ok(claim) => break claim,
            Err(_) => tokio::task::yield_now().await,
        }
    };
    assert_eq!(
        claim["consumerCount"], 2,
        "successful first selector is neither forgotten nor duplicated"
    );
    engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_replacement_after_link_loss_keeps_both_generations_owned() {
    const SECOND: &str = "00002a38-0000-1000-8000-00805f9b34fb";
    let mut second_attempts = 0;
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            let mut characteristic = services[0].characteristics[0].clone();
            characteristic.uuid = SECOND.to_owned();
            services[0].characteristics.push(characteristic);
            Reply::Now(RadioCompletion::Discovered(services))
        }
        RadioRequest::EnableNotifications { instance, .. }
            if instance.characteristic_uuid == SECOND =>
        {
            second_attempts += 1;
            if second_attempts == 2 {
                Reply::Now(RadioCompletion::Failed(PlatformFailure::not_dispatched(
                    FailureKind::Busy,
                    "temporary replacement queue refusal",
                )))
            } else {
                polar_responder(request)
            }
        }
        _ => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let engine = host.continuation();
    let mut order: serde_json::Value = serde_json::from_str(&declaration()).unwrap();
    let mut second = order["resubscribe"][0].clone();
    second["characteristicUuid"] = json!(SECOND);
    order["resubscribe"].as_array_mut().unwrap().push(second);
    engine.execute(POLAR, &order.to_string()).await.unwrap();
    host.ingest(RadioIngress::Connection {
        peer_id: POLAR.to_owned(),
        connected: false,
        status: None,
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while radio.count(RequestKind::EnableNotifications) < 5 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replacement completes after transient partial failure without foreground help");
    assert_eq!(radio.count(RequestKind::Connect), 2);
    assert_eq!(radio.count(RequestKind::Discover), 2);
    let claim = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match engine.prepare_claim(256, 65536).await {
                Ok(claim) => break claim,
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        claim["consumerCount"], 4,
        "each generation retains its immutable selector identities"
    );
    assert_eq!(claim["selectors"][0], claim["selectors"][2]);
    assert_eq!(claim["selectors"][1], claim["selectors"][3]);
    engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claiming_initial_failure_stops_owned_retries() {
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::Connect { .. } => Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
            FailureKind::Platform,
            "still out of range",
        ))),
        _ => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let engine = host.continuation();
    engine.execute(POLAR, &declaration()).await.unwrap_err();
    let claim = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match engine.prepare_claim(256, 65536).await {
                Ok(claim) => break claim,
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .unwrap();
    engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    let attempts = radio.count(RequestKind::Connect);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        radio.count(RequestKind::Connect),
        attempts,
        "sealed foreground claim stops autonomous retry"
    );
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_change_recovers_without_another_platform_wake_or_js() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let executor = host.continuation();
    executor.execute(POLAR, &declaration()).await.unwrap();
    host.ingest(RadioIngress::ServicesChanged {
        peer_id: POLAR.to_owned(),
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while radio.count(RequestKind::EnableNotifications) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native continuation rediscovered and resubscribed without JS");
    let claim = loop {
        match executor.prepare_claim(256, 65536).await {
            Ok(claim) => break claim,
            Err(error) if error["code"] == "lifecycle.invalid-state" => {
                tokio::task::yield_now().await
            }
            Err(error) => panic!("claim failed: {error}"),
        }
    };
    assert_eq!(
        claim["consumerCount"], 2,
        "old selector identity remains available for backlog decoding"
    );
    executor
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_observation_cannot_terminate_native_service_change_recovery() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let engine = host.continuation();
    engine.execute(POLAR, &declaration()).await.unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let mut observers = Vec::new();
    for _ in 0..4 {
        let engine = engine.clone();
        let finished = finished.clone();
        observers.push(tokio::spawn(async move {
            while !finished.load(Ordering::SeqCst) {
                match engine.describe_backlog().await {
                    Ok(snapshot) => {
                        assert_ne!(
                            snapshot["continuationOutcome"]["error"]["detail"],
                            "continuation execution or handoff is in progress",
                            "diagnostic contention is not a failed native recovery"
                        );
                    }
                    Err(error) => assert_eq!(error["code"], "lifecycle.invalid-state"),
                }
                tokio::task::yield_now().await;
            }
        }));
    }
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        for expected in 2..=17 {
            host.ingest(RadioIngress::ServicesChanged {
                peer_id: POLAR.to_owned(),
            });
            while radio.count(RequestKind::EnableNotifications) < expected
                || engine.describe_backlog().await.is_err()
            {
                tokio::task::yield_now().await;
            }
        }
    })
    .await;
    finished.store(true, Ordering::SeqCst);
    for observer in observers {
        observer.await.unwrap();
    }
    assert!(
        result.is_ok(),
        "status polling must not strand native recovery"
    );
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_loss_retries_transient_failure_and_collects_values_without_js() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let executor = host.continuation();
    executor.execute(POLAR, &declaration()).await.unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = attempts.clone();
    radio.set_responder(Box::new(move |request| {
        if matches!(request, RadioRequest::Connect { .. })
            && observed.fetch_add(1, Ordering::SeqCst) == 0
        {
            Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
                FailureKind::Platform,
                "temporary transport refusal",
            )))
        } else {
            polar_responder(request)
        }
    }));
    host.ingest(RadioIngress::Connection {
        peer_id: POLAR.to_owned(),
        connected: false,
        status: None,
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while radio.count(RequestKind::EnableNotifications) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native recovery retries transient connection failure");
    assert!(attempts.load(Ordering::SeqCst) >= 2);
    let epoch = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|request| match request {
            RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .unwrap();
    // Drain is deliberately not used as a readiness barrier: data must remain
    // queued until the foreground validates and acknowledges its handoff.
    host.ingest(RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.to_owned(),
            service_uuid: HR_SERVICE.to_owned(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.to_owned(),
            characteristic_occurrence: 0,
        },
        epoch,
        value: vec![0, 72],
    });
    // Existing control records can keep wake disarmed; counters are the
    // non-consuming native authority for notification retention.
    let claim = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(snapshot) = executor.describe_backlog().await
                && snapshot["counters"]["retainedByteBuffers"]
                    .as_u64()
                    .is_some_and(|count| count > 0)
                && let Ok(claim) = executor.prepare_claim(256, 65536).await
            {
                break claim;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let records: Vec<serde_json::Value> = claim["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|batch| {
            serde_json::from_str::<serde_json::Value>(batch.as_str().unwrap()).unwrap()["records"]
                .as_array()
                .unwrap()
                .clone()
        })
        .collect();
    assert!(
        records.iter().any(|record| record["t"] == "value"),
        "native values survive reconnect until claim: {records:?}"
    );
    executor
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_stops_held_execution_and_refuses_new_native_admission() {
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::Connect { .. } => Reply::Hold,
        _ => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let engine = host.continuation();
    let running = engine.clone();
    let execution = tokio::spawn(async move { running.execute(POLAR, &declaration()).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while radio.held_of(RequestKind::Connect).is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), host.shutdown())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result).unwrap()["state"],
        "released"
    );
    assert!(execution.await.unwrap().is_err());
    assert!(engine.execute(POLAR, &declaration()).await.is_err());
    assert_eq!(radio.count(RequestKind::Connect), 1);
    assert_eq!(radio.count(RequestKind::EnableNotifications), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_continuation_never_wakes_an_unregistered_javascript_session() {
    use std::sync::atomic::Ordering;
    let radio = Scripted::polar();
    let (host, wakes) = open(&radio, MobilePlatform::Apple).await;
    let engine = host.continuation();
    engine.execute(POLAR, &declaration()).await.unwrap();
    let epoch = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|request| match request {
            RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .unwrap();
    host.ingest(RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.to_owned(),
            service_uuid: HR_SERVICE.to_owned(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.to_owned(),
            characteristic_occurrence: 0,
        },
        epoch,
        value: vec![0, 72],
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(snapshot) = engine.describe_backlog().await
                && snapshot["counters"]["retainedByteBuffers"]
                    .as_u64()
                    .is_some_and(|count| count > 0)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let claim = engine.prepare_claim(256, 65536).await.unwrap();
    engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        wakes.count.load(Ordering::SeqCst),
        0,
        "native records must not enter JS early-wake bookkeeping"
    );
    host.shutdown().await;
}
