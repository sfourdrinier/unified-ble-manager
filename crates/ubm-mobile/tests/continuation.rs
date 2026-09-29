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

fn stop_start_declaration() -> String {
    let mut order: serde_json::Value = serde_json::from_str(&declaration()).unwrap();
    let step = |opcode, accepted| {
        json!({"selector":order["resubscribe"][0],"value":[opcode,0],"timeoutMs":40,
            "response":{"subscriptionIndex":0,"prefix":[240,opcode,0],"minLength":4,"maxLength":4,
                "status":{"offset":3,"accepted":accepted}}})
    };
    order["setup"] = json!([step(3, vec![0, 6]), step(2, vec![0])]);
    order.to_string()
}

fn setup_radio() -> (
    std::sync::Arc<Scripted>,
    tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let (writes, received) = tokio::sync::mpsc::unbounded_channel();
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            services[0].characteristics[0].properties.write = true;
            Reply::Now(RadioCompletion::Discovered(services))
        }
        RadioRequest::Write { value, .. } => {
            writes.send(value.clone()).unwrap();
            // ATT completion is not application-response completion.
            Reply::Now(RadioCompletion::Unit)
        }
        _ => polar_responder(request),
    }));
    (radio, received)
}

fn setup_response(host: &ubm_mobile::MobileHost, radio: &Scripted, opcode: u8, status: u8) {
    let epoch = setup_notification_epoch(radio);
    // A delayed peripheral PDU arrives on the currently installed native
    // callback; it does not carry the old mobile session epoch on the wire.
    host.ingest(RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.into(),
            service_uuid: HR_SERVICE.into(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.into(),
            characteristic_occurrence: 0,
        },
        epoch,
        value: vec![240, opcode, 0, status],
    });
}

fn setup_notification_epoch(radio: &Scripted) -> u64 {
    radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|request| match request {
            RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .unwrap()
}

async fn dispose_native(engine: &ubm_desktop::continuation::NativeContinuation) {
    let claim = engine.prepare_claim(256, 65536).await.unwrap();
    let receipt = engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(receipt["disposed"], true, "{receipt}");
    assert_eq!(receipt["disposeFailure"], serde_json::Value::Null);
}

async fn observe_recording<F>(operation: F) -> serde_json::Value
where
    F: FnOnce() -> ubm_desktop::continuation::Result<serde_json::Value> + Send + 'static,
{
    ubm_desktop::continuation_journal::run_blocking(operation)
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn recording_observation_does_not_block_the_intake_runtime() {
    let runtime_thread = std::thread::current().id();
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::sync_channel(1);
    let intake = tokio::spawn(async move {
        observed.await.unwrap();
        release.send(()).unwrap();
    });
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        observe_recording(move || {
            assert_ne!(std::thread::current().id(), runtime_thread);
            entered.send(()).unwrap();
            // Model a synchronous filesystem/SQLite observation held until the
            // intake task runs. A worker-thread poll must never prevent that task.
            held.recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            Ok(json!({"records":2}))
        }),
    )
    .await
    .unwrap();
    assert_eq!(status["records"], 2);
    intake.await.unwrap();
}

#[tokio::test]
async fn cross_claim_late_stop_cannot_advance_setup_on_an_independently_held_generation() {
    let mut outcomes = Vec::new();
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let (radio, mut writes) = setup_radio();
        let (host, _) = open(&radio, platform).await;
        let independent = host.open_session("independent-background").unwrap();
        ok(&call(
            &independent,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"background","operationId":"hold-link"}).to_string(),
        )
        .await);
        ok(&call(
            &independent,
            "gatt.discover",
            &json!({"peerId":POLAR,"lease":"background","operationId":"discover-link"}).to_string(),
        )
        .await);
        let before = ok(&call(&independent, "session.reconcile", "{}").await);
        let engine = host.continuation();
        let order = stop_start_declaration();
        let failure = engine.execute(POLAR, &order).await.unwrap_err();
        assert_eq!(failure["code"], "operation.timed-out");
        assert_eq!(failure["retryability"], "never");
        assert_eq!(writes.recv().await.unwrap(), vec![3, 0]);
        dispose_native(&engine).await;
        assert_eq!(radio.count(RequestKind::Disconnect), 0);

        let worker = engine.clone();
        let mut fresh = tokio::spawn(async move { worker.execute(POLAR, &order).await });
        let fresh_result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::select! {
                result = &mut fresh => result.unwrap(),
                value = writes.recv() => {
                    assert_eq!(value.unwrap(), vec![3, 0], "fresh observation must precede STOP");
                    assert_eq!(radio.count(RequestKind::EnableNotifications), 2, "fresh native session must install its own subscription");
                    // FIFO source ordering: the timed-out old STOP/status6
                    // precedes the fresh command's own rejection/status3.
                    setup_response(&host, &radio, 3, 6);
                    setup_response(&host, &radio, 3, 3);
                    fresh.await.unwrap()
                }
            }
        }).await.expect("fresh setup must settle under its original step budget");
        let fresh_failure = fresh_result.unwrap_err();
        let after = ok(&call(&independent, "session.reconcile", "{}").await);
        assert!(before["links"][0]["connectionGeneration"].is_string());
        assert!(before["links"][0]["databaseGeneration"].is_string());
        assert_eq!(
            before["links"][0]["connectionGeneration"],
            after["links"][0]["connectionGeneration"]
        );
        assert_eq!(
            before["links"][0]["databaseGeneration"],
            after["links"][0]["databaseGeneration"]
        );
        dispose_native(&engine).await;
        assert_eq!(radio.count(RequestKind::Disconnect), 0);
        assert_eq!(
            ok(&call(
                &independent,
                "gatt.read",
                &json!({"peerId":POLAR,"selector":selector(),"operationId":"still-owned"})
                    .to_string()
            )
            .await)["valueB64"],
            "Qg=="
        );
        let start_count = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(
                |request| matches!(request, RadioRequest::Write { value, .. } if value == &[2,0]),
            )
            .count();
        let stop_count = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(
                |request| matches!(request, RadioRequest::Write { value, .. } if value == &[3,0]),
            )
            .count();
        ok(&call(&independent, "session.dispose", "{}").await);
        host.shutdown().await;
        outcomes.push((platform, start_count, stop_count, fresh_failure, failure));
    }
    assert!(
        outcomes.iter().all(|(_, count, _, _, _)| *count == 0),
        "old STOP/status6 must never advance fresh setup to START: {outcomes:?}"
    );
    for (platform, _, stop_count, fresh_failure, original_failure) in outcomes {
        assert_eq!(
            stop_count, 1,
            "{platform:?}: no replacement STOP may be dispatched"
        );
        assert_eq!(
            fresh_failure, original_failure,
            "{platform:?}: retain the exact original uncertainty"
        );
    }
}

#[tokio::test]
async fn authoritative_generation_change_allows_fresh_setup_after_claimed_timeout() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let (radio, mut writes) = setup_radio();
        let (host, _) = open(&radio, platform).await;
        let independent = host.open_session("old-background").unwrap();
        ok(&call(
            &independent,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"old-background","operationId":"old-connect"})
                .to_string(),
        )
        .await);
        ok(&call(
            &independent,
            "gatt.discover",
            &json!({"peerId":POLAR,"lease":"old-background","operationId":"old-discover"})
                .to_string(),
        )
        .await);
        let before = ok(&call(&independent, "session.reconcile", "{}").await);
        let engine = host.continuation();
        let order = stop_start_declaration();
        assert_eq!(
            engine.execute(POLAR, &order).await.unwrap_err()["code"],
            "operation.timed-out"
        );
        assert_eq!(writes.recv().await.unwrap(), vec![3, 0]);
        dispose_native(&engine).await;
        assert_eq!(radio.count(RequestKind::Disconnect), 0);
        ok(&call(&independent, "session.dispose", "{}").await);
        assert_eq!(
            radio.count(RequestKind::Disconnect),
            1,
            "last owner confirms physical release"
        );
        let replacement = host.open_session("new-background").unwrap();
        ok(&call(
            &replacement,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"new-background","operationId":"new-connect"})
                .to_string(),
        )
        .await);
        ok(&call(
            &replacement,
            "gatt.discover",
            &json!({"peerId":POLAR,"lease":"new-background","operationId":"new-discover"})
                .to_string(),
        )
        .await);
        let after = ok(&call(&replacement, "session.reconcile", "{}").await);
        assert!(before["links"][0]["connectionGeneration"].is_string());
        assert!(before["links"][0]["databaseGeneration"].is_string());
        assert_ne!(
            before["links"][0]["connectionGeneration"],
            after["links"][0]["connectionGeneration"]
        );
        assert_ne!(
            before["links"][0]["databaseGeneration"],
            after["links"][0]["databaseGeneration"]
        );
        let worker = engine.clone();
        let running = tokio::spawn(async move { worker.execute(POLAR, &order).await });
        for opcode in [3, 2] {
            let value = tokio::time::timeout(std::time::Duration::from_secs(2), writes.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(value, vec![opcode, 0]);
            setup_response(&host, &radio, opcode, 0);
        }
        assert_eq!(
            running.await.unwrap().unwrap()["event"],
            "continuation.completed"
        );
        assert_eq!(
            radio.count(RequestKind::Connect),
            4,
            "both generations admit an independent and a native lease"
        );
        dispose_native(&engine).await;
        ok(&call(&replacement, "session.dispose", "{}").await);
        host.shutdown().await;
    }
}

#[tokio::test]
async fn public_session_disposal_preserves_native_continuation_lease() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let engine = host.continuation();
        engine.execute(POLAR, &declaration()).await.unwrap();
        let foreground = host.open_session("foreground").unwrap();
        ok(&call(
            &foreground,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"foreground", "operationId":"connect"}).to_string(),
        )
        .await);
        ok(&call(
            &foreground,
            "gatt.discover",
            &json!({"peerId":POLAR,"lease":"foreground", "operationId":"discover"}).to_string(),
        )
        .await);
        ok(&call(&foreground, "session.dispose", "{}").await);
        assert_eq!(
            radio.count(RequestKind::Disconnect),
            0,
            "disposing a public session cannot disconnect the native owner's shared link"
        );
        let claim = engine.prepare_claim(256, 65536).await.unwrap();
        assert_eq!(
            engine
                .acknowledge_claim(claim["claimToken"].as_str().unwrap())
                .await
                .unwrap()["disposed"],
            true
        );
        assert_eq!(radio.count(RequestKind::Disconnect), 1);
        host.shutdown().await;
    }
}

#[tokio::test]
async fn native_claim_preserves_public_lease_until_its_own_disposal() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let engine = host.continuation();
        engine.execute(POLAR, &declaration()).await.unwrap();
        let foreground = host.open_session("foreground").unwrap();
        ok(&call(
            &foreground,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"foreground", "operationId":"connect"}).to_string(),
        )
        .await);
        ok(&call(
            &foreground,
            "gatt.discover",
            &json!({"peerId":POLAR,"lease":"foreground", "operationId":"discover"}).to_string(),
        )
        .await);
        let claim = engine.prepare_claim(256, 65536).await.unwrap();
        assert_eq!(
            engine
                .acknowledge_claim(claim["claimToken"].as_str().unwrap())
                .await
                .unwrap()["disposed"],
            true
        );
        assert_eq!(radio.count(RequestKind::Disconnect), 0);
        ok(&call(
            &foreground,
            "gatt.read",
            &json!({"peerId":POLAR,"selector":selector(),"operationId":"read"}).to_string(),
        )
        .await);
        ok(&call(&foreground, "session.dispose", "{}").await);
        let (error, _) = failure(
            &call(
                &foreground,
                "gatt.read",
                &json!({"peerId":POLAR,"selector":selector(),"operationId":"after-dispose"})
                    .to_string(),
            )
            .await,
        );
        assert_eq!(error["code"], "lifecycle.destroyed");
        assert_eq!(radio.count(RequestKind::Disconnect), 1);
        host.shutdown().await;
    }
}

#[tokio::test]
async fn public_discovery_and_native_wake_share_the_same_physical_snapshot() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let foreground = host.open_session("foreground").unwrap();
        ok(&call(
            &foreground,
            "connection.connect",
            &json!({"peerId":POLAR,"lease":"foreground", "operationId":"connect"}).to_string(),
        )
        .await);
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        radio.set_responder(Box::new(move |request| {
            events.send((request.kind(), request.id())).unwrap();
            if matches!(request, RadioRequest::Discover { .. }) {
                Reply::Hold
            } else {
                polar_responder(request)
            }
        }));
        let public = foreground.clone();
        let discovery = tokio::spawn(async move {
            call(
                &public,
                "gatt.discover",
                &json!({"peerId":POLAR,"lease":"foreground", "operationId":"discover"}).to_string(),
            )
            .await
        });
        let held = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let (kind, id) = received.recv().await.unwrap();
                if kind == RequestKind::Discover {
                    break id;
                }
            }
        })
        .await
        .unwrap();
        let engine = host.continuation();
        let native = engine.clone();
        let continuation = tokio::spawn(async move { native.execute(POLAR, &declaration()).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if received.recv().await.unwrap().0 == RequestKind::Connect {
                    break;
                }
            }
        })
        .await
        .unwrap();
        radio.answer(held, RadioCompletion::Discovered(polar_services()));
        ok(&discovery.await.unwrap());
        tokio::time::timeout(std::time::Duration::from_secs(2), continuation)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            radio.count(RequestKind::Discover),
            1,
            "both owners use one physical generation"
        );
        ok(&call(
            &foreground,
            "gatt.read",
            &json!({"peerId":POLAR,"selector":selector(),"operationId":"read"}).to_string(),
        )
        .await);
        ok(&call(&foreground, "session.dispose", "{}").await);
        assert_eq!(radio.count(RequestKind::Disconnect), 0);
        let claim = engine.prepare_claim(256, 65536).await.unwrap();
        assert_eq!(
            engine
                .acknowledge_claim(claim["claimToken"].as_str().unwrap())
                .await
                .unwrap()["disposed"],
            true
        );
        assert_eq!(radio.count(RequestKind::Disconnect), 1);
        host.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_claim_queues_behind_autonomous_recovery_and_prevents_another_retry() {
    use std::future::Future;
    use std::task::Poll;
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let engine = host.continuation();
        engine.execute(POLAR, &declaration()).await.unwrap();
        radio.set_responder(Box::new(|request| match request {
            RadioRequest::Connect { .. } => Reply::Hold,
            _ => polar_responder(request),
        }));
        host.ingest(RadioIngress::Connection {
            peer_id: POLAR.into(),
            connected: false,
            status: None,
        });
        let connect = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(id) = radio.held_of(RequestKind::Connect).first() {
                    break *id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let claim = engine.prepare_claim(256, 65536);
        tokio::pin!(claim);
        let pending =
            std::future::poll_fn(|cx| Poll::Ready(claim.as_mut().poll(cx).is_pending())).await;
        assert!(
            pending,
            "foreground handoff must queue fairly behind autonomous recovery instead of returning busy"
        );
        radio.answer(
            connect,
            RadioCompletion::Failed(PlatformFailure::new(
                FailureKind::Platform,
                "temporary transport refusal",
            )),
        );
        let prepared = tokio::time::timeout(std::time::Duration::from_secs(2), claim)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prepared["consumerCount"], 1);
        assert_eq!(engine.prepare_claim(256, 65536).await.unwrap(), prepared);
        let released = engine
            .acknowledge_claim(prepared["claimToken"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(released["disposed"], true, "{released}");
        engine.request_recovery(&tokio::runtime::Handle::current());
        // The cached claim awaits the recovery worker's idle notification.
        // A scheduler yield alone cannot prove a delayed retry stayed stopped.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            engine.prepare_claim(256, 65536),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(radio.count(RequestKind::Connect), 2);
        host.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_mobile_collection_retains_context_and_survives_native_claim() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let directory = std::env::temp_dir().join(format!(
            "ubm-mobile-recording-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let engine = host.continuation();
        engine.configure_recording_directory(&directory).unwrap();
        let mut order: serde_json::Value = serde_json::from_str(&declaration()).unwrap();
        order["recording"] = json!({"id":"native-mobile","maxBytes":1048576,"maxRecords":1000});
        engine.execute(POLAR, &order.to_string()).await.unwrap();
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
                peer_id: POLAR.into(),
                service_uuid: HR_SERVICE.into(),
                service_occurrence: 0,
                characteristic_uuid: HR_MEASUREMENT.into(),
                characteristic_occurrence: 0,
            },
            epoch,
            value: vec![0, 72],
        });
        // Observe committed intake without preparing/ACKing partial prefixes.
        // Those synchronous cursor writes contend with the append under test
        // and their fsyncs can monopolize a runtime worker on Windows.
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let reader = engine.clone();
                let status =
                    observe_recording(move || reader.recording_status("native-mobile")).await;
                if status["records"] == 2 {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(status["lostRecords"], 0);
        assert_eq!(status["collectionFailure"], serde_json::Value::Null);
        let reader = engine.clone();
        let prepared =
            observe_recording(move || reader.recording_prepare("native-mobile", 100, 65536)).await;
        assert_eq!(
            prepared["token"], "native-mobile:1",
            "observation must not mutate the journal cursor"
        );
        let entry = prepared["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["record"]["valueB64"] == "AEg=")
            .unwrap();
        assert_eq!(entry["metadata"]["consumer"]["peerId"], POLAR);
        assert!(entry["metadata"]["consumer"]["databaseGeneration"].is_string());
        let claim = engine.prepare_claim(256, 65536).await.unwrap();
        assert_eq!(claim["recording"], json!({"id":"native-mobile"}));
        engine
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(
            engine
                .recording_prepare("native-mobile", 100, 65536)
                .unwrap(),
            prepared
        );
        engine
            .recording_acknowledge("native-mobile", prepared["token"].as_str().unwrap())
            .unwrap();
        engine.recording_stop("native-mobile").unwrap();
        host.shutdown().await;
        drop(engine);
        drop(host);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_refused_subscription_then_new_database_never_reuses_committed_identity() {
    let directory = std::env::temp_dir().join(format!(
        "ubm-recording-recovery-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let mut first = true;
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, RadioRequest::EnableNotifications { .. }) && first {
            first = false;
            return Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
                FailureKind::PermissionDenied,
                "first enable refused",
            )));
        }
        polar_responder(request)
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let engine = host.continuation();
    engine.configure_recording_directory(&directory).unwrap();
    let mut order: serde_json::Value = serde_json::from_str(&declaration()).unwrap();
    order["recording"] = json!({"id":"recovery","maxBytes":1048576,"maxRecords":1000});
    assert_eq!(
        engine.execute(POLAR, &order.to_string()).await.unwrap_err()["code"],
        "permission.denied"
    );
    host.ingest(RadioIngress::ServicesChanged {
        peer_id: POLAR.into(),
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if radio.count(RequestKind::EnableNotifications) == 2
                && engine.describe_backlog().await.is_ok()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("new database must admit a fresh durable consumer identity");
    let prepared = engine.recording_prepare("recovery", 100, 65536).unwrap();
    let registrations: Vec<_> = prepared["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["record"]["t"] == "consumer-registration")
        .collect();
    assert_eq!(registrations.len(), 2);
    assert_ne!(
        registrations[0]["record"]["consumer"],
        registrations[1]["record"]["consumer"]
    );
    assert_ne!(
        registrations[0]["metadata"]["consumer"]["databaseGeneration"],
        registrations[1]["metadata"]["consumer"]["databaseGeneration"]
    );
    let claim = engine.prepare_claim(256, 65536).await.unwrap();
    assert_eq!(claim["consumerCount"], 2);
    engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    engine.recording_stop("recovery").unwrap();
    host.shutdown().await;
    drop(engine);
    drop(host);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pristine_apple_restored_native_setup_collects_before_att_completion_without_js() {
    const PEER: &str = "232859A3-172E-CD86-20F1-0D2331118FEC";
    const PMD: &str = "fb005c80-02e7-f387-1cad-8acd2d8df0c8";
    const CP: &str = "fb005c81-02e7-f387-1cad-8acd2d8df0c8";
    const DATA: &str = "fb005c82-02e7-f387-1cad-8acd2d8df0c8";
    let directory = std::env::temp_dir().join(format!(
        "ubm-apple-pristine-intake-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let (writes, mut received) = tokio::sync::mpsc::unbounded_channel();
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            let mut cp = services[0].characteristics[0].clone();
            cp.uuid = CP.into();
            cp.properties.write = true;
            let mut data = cp.clone();
            data.uuid = DATA.into();
            data.properties.write = false;
            services.push(ubm_desktop::ServiceSnapshot {
                uuid: PMD.into(),
                occurrence: 0,
                characteristics: vec![cp, data],
            });
            Reply::Now(RadioCompletion::Discovered(services))
        }
        RadioRequest::Write { id, value, .. } => {
            writes.send((*id, value.clone())).unwrap();
            if value == &[3, 0] {
                Reply::Hold
            } else {
                Reply::Now(RadioCompletion::Unit)
            }
        }
        _ => polar_responder(request),
    }));
    let (host, wakes) = open(&radio, MobilePlatform::Apple).await;
    assert_eq!(
        host.ingest(RadioIngress::Restored {
            peers: vec![ubm_mobile::RestoredPeer {
                peer_id: PEER.into(),
                name: Some("SIM pristine native".into()),
                connected: true,
            }],
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    // No public session, JS listener, or JS wake route exists. This is the
    // shared Rust execution boundary used after the native restoration callback.
    let engine = host.continuation();
    engine.configure_recording_directory(&directory).unwrap();
    let selector = |service, characteristic| {
        json!({"serviceUuid":service,"serviceOccurrence":1,
            "characteristicUuid":characteristic,"characteristicOccurrence":1})
    };
    let cp = selector(PMD, CP);
    let order = json!({"onAppearance":"native",
    "resubscribe":[selector(HR_SERVICE,HR_MEASUREMENT),cp,selector(PMD,DATA)],
    "recording":{"id":"pristine-apple","maxBytes":1048576,"maxRecords":1000},
    "setup":[
        {"selector":cp,"value":[3,0],"timeoutMs":2000,
         "response":{"subscriptionIndex":1,"prefix":[240,3,0],
            "minLength":4,"maxLength":4,"status":{"offset":3,"accepted":[6]}}},
        {"selector":cp,"value":[2,0],"timeoutMs":2000}
    ]})
    .to_string();
    engine.seed_declaration(&order).unwrap();
    let executor = engine.clone();
    let execute = tokio::spawn(async move { executor.execute(PEER, &order).await });
    let (stop_id, stop) = tokio::time::timeout(std::time::Duration::from_secs(3), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stop, vec![3, 0]);
    let enabled: Vec<_> = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| {
            if let RadioRequest::EnableNotifications {
                instance, epoch, ..
            } = request
            {
                Some((instance.clone(), *epoch))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(enabled.len(), 3);
    for (instance, epoch) in &enabled {
        assert_eq!(instance.peer_id, PEER);
        assert_eq!(instance.service_occurrence, 0);
        assert_eq!(instance.characteristic_occurrence, 0);
        let value = match instance.characteristic_uuid.as_str() {
            HR_MEASUREMENT => vec![0, 72],
            CP => vec![240, 3, 0, 6],
            DATA => vec![0, 1, 2],
            other => panic!("unexpected enabled characteristic {other}"),
        };
        assert_eq!(
            host.ingest(RadioIngress::Notification {
                instance: instance.clone(),
                epoch: *epoch,
                value,
            }),
            ubm_mobile::IngressStatus::Accepted
        );
    }
    // Observe durable intake, not a clock delay or an immutable prepare taken
    // too early. All three values must be journaled while ATT is still held.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if engine.recording_status("pristine-apple").unwrap()["records"] == 6 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("three native values must reach the journal before ATT completion");
    assert_eq!(radio.count(RequestKind::Write), 1);
    let prepared = engine
        .recording_prepare("pristine-apple", 100, 65536)
        .unwrap();
    let records = prepared["records"].as_array().unwrap();
    for (characteristic, bytes) in [(HR_MEASUREMENT, "AEg="), (CP, "8AMABg=="), (DATA, "AAEC")] {
        assert!(
            records.iter().any(|entry| {
                entry["record"]["t"] == "value"
                    && entry["record"]["valueB64"] == bytes
                    && entry["metadata"]["consumer"]["selector"]["characteristicUuid"]
                        == characteristic
            }),
            "missing durable value for {characteristic}: {prepared}"
        );
    }
    assert_eq!(
        radio.answer(stop_id, RadioCompletion::Unit),
        ubm_mobile::CompletionStatus::Delivered
    );
    let (_, start) = tokio::time::timeout(std::time::Duration::from_secs(2), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(start, vec![2, 0], "timely STOP reply must advance setup");
    tokio::time::timeout(std::time::Duration::from_secs(2), execute)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(wakes.count.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(host.ingress_failure().is_none());
    let claim = engine.prepare_claim(256, 65536).await.unwrap();
    assert_eq!(claim["consumerCount"], 3);
    assert_eq!(
        claim["disposed"], false,
        "prepare must not release radio ownership before ACK"
    );
    let disposal = engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(disposal["disposed"], true, "{disposal}");
    assert_eq!(disposal["disposeFailure"], serde_json::Value::Null);
    assert_eq!(
        engine
            .recording_prepare("pristine-apple", 100, 65536)
            .unwrap(),
        prepared
    );
    engine.recording_stop("pristine-apple").unwrap();
    host.shutdown().await;
    assert_eq!(radio.count(RequestKind::DisableNotifications), 3);
    assert_eq!(radio.count(RequestKind::Disconnect), 1);
    drop(engine);
    drop(host);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_ack_before_att_completion_is_retained_and_replayed_after_native_link_recovery() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::new(Box::new(|request| match request {
            RadioRequest::Discover { .. } => {
                let mut services = polar_services();
                services[0].characteristics[0].properties.write = true;
                Reply::Now(RadioCompletion::Discovered(services))
            }
            RadioRequest::Write { .. } => Reply::Hold,
            _ => polar_responder(request),
        }));
        let (host, _) = open(&radio, platform).await;
        let engine = host.continuation();
        let mut order: serde_json::Value = serde_json::from_str(&declaration()).unwrap();
        order["link"] =
            json!({"mtu":{"requested":512,"timeoutMs":1000,"onUnsupported":"continue"}});
        order["setup"] = json!([{"selector":order["resubscribe"][0],"value":[2,0],"timeoutMs":2000,"response":{"subscriptionIndex":0,"prefix":[240,2,0],"minLength":4,"maxLength":5,"status":{"offset":3,"accepted":[0]},"trailing":{"offset":4,"accepted":[0]}}}]);
        let run = engine.clone();
        let first = tokio::spawn(async move { run.execute(POLAR, &order.to_string()).await });
        for generation in 0..2 {
            let write = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if let Some(id) = radio.held_of(RequestKind::Write).first() {
                        break *id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("native recipe must reach write without JavaScript");
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
                    peer_id: POLAR.into(),
                    service_uuid: HR_SERVICE.into(),
                    service_occurrence: 0,
                    characteristic_uuid: HR_MEASUREMENT.into(),
                    characteristic_occurrence: 0,
                },
                epoch,
                value: vec![240, 2, 0, 0, 0],
            });
            radio.answer(write, RadioCompletion::Unit);
            if generation == 0 {
                // Wait until ownership leaves execute before provoking actual
                // link recovery; no second execute/wake call drives it.
                tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    loop {
                        if engine.describe_backlog().await.is_ok() {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                host.ingest(RadioIngress::Connection {
                    peer_id: POLAR.into(),
                    connected: false,
                    status: None,
                });
            }
        }
        let initial = first.await.unwrap().unwrap();
        assert_eq!(
            initial["link"]["mtu"]["outcome"],
            if platform == MobilePlatform::Android {
                "negotiated"
            } else {
                "unsupported"
            }
        );
        assert_eq!(
            radio.count(RequestKind::RequestMtu),
            if platform == MobilePlatform::Android {
                2
            } else {
                0
            }
        );
        let claim = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Ok(claim) = engine.prepare_claim(256, 65536).await {
                    break claim;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let values: Vec<_> = claim["batches"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|batch| serde_json::from_str::<serde_json::Value>(batch.as_str().unwrap()).unwrap()["records"].as_array().unwrap().clone())
            .filter(|record| record["t"] == "value")
            .collect();
        assert_eq!(
            values.len(),
            2,
            "application acknowledgements are retained across both native generations"
        );
        assert_eq!(radio.count(RequestKind::Write), 2);
        assert_eq!(claim["consumerCount"], 2);
        engine
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap();
        host.shutdown().await;
    }
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
        claim["consumerCount"], 3,
        "successful first selector plus refused and successful second admissions retain distinct identities"
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
        claim["consumerCount"], 5,
        "each generation and the refused replacement retain immutable selector identities"
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
