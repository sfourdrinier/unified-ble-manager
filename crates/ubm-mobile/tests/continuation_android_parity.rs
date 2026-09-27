//! Android executor policy regressions now exercise the shared native owner,
//! for both mobile platform profiles, rather than a second Kotlin state machine.
mod common;

use common::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ubm_desktop::continuation::{
    ContinuationFuture, ContinuationHost, ContinuationSession, NativeContinuation,
};
use ubm_mobile::{
    FailureKind, MobilePlatform, PlatformFailure, RadioCompletion, RadioRequest, RequestKind,
};

const SECOND: &str = "00002a38-0000-1000-8000-00805f9b34fb";

fn selector(characteristic: &str) -> Value {
    json!({"serviceUuid":HR_SERVICE,"serviceOccurrence":1,
        "characteristicUuid":characteristic,"characteristicOccurrence":1})
}

fn declaration() -> String {
    json!({"onAppearance":"native","resubscribe":[selector(HR_MEASUREMENT)]}).to_string()
}

async fn held(radio: &Scripted, kind: RequestKind) -> u64 {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(id) = radio.held_of(kind).first() {
                return *id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("requested radio operation was held")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_appearance_reuses_authoritative_live_link_and_subscriptions() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let executor = host.continuation();
        let initial = executor.execute(POLAR, &declaration()).await.unwrap();
        assert_eq!(
            executor.execute(POLAR, &declaration()).await.unwrap(),
            initial
        );
        assert_eq!(radio.count(RequestKind::Connect), 1);
        assert_eq!(radio.count(RequestKind::Discover), 1);
        assert_eq!(radio.count(RequestKind::EnableNotifications), 1);
        host.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_peer_and_same_count_declaration_replacement_are_refused_without_radio_work() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let executor = host.continuation();
        executor.execute(POLAR, &declaration()).await.unwrap();
        let before = radio.kinds();
        let changed = json!({"onAppearance":"native","resubscribe":[selector(SECOND)]}).to_string();
        assert_eq!(
            executor
                .execute("A0:9E:1A:00:00:02", &declaration())
                .await
                .unwrap_err()["code"],
            "lifecycle.invalid-state"
        );
        assert_eq!(
            executor.execute(POLAR, &changed).await.unwrap_err()["code"],
            "lifecycle.invalid-state"
        );
        assert!(executor.declaration_replacement_failure(&changed).is_some());
        assert!(
            executor
                .declaration_replacement_failure(&declaration())
                .is_none()
        );
        assert_eq!(radio.kinds(), before);
        let claim = executor.prepare_claim(256, 65536).await.unwrap();
        assert_eq!(claim["selectors"], json!([selector(HR_MEASUREMENT)]));
        executor
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap();
        assert!(executor.declaration_replacement_failure(&changed).is_none());
        host.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_connect_reports_platform_reason_and_retains_owned_cleanup() {
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::Connect { .. } => Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
            FailureKind::PermissionDenied,
            "Bluetooth permission refused",
        ))),
        _ => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let executor = host.continuation();
    let failure = executor.execute(POLAR, &declaration()).await.unwrap_err();
    assert_eq!(failure["code"], "permission.denied");
    assert!(failure.to_string().contains("Bluetooth permission refused"));
    assert_eq!(radio.count(RequestKind::EnableNotifications), 0);
    let claim = executor.prepare_claim(256, 65536).await.unwrap();
    assert_eq!(claim["consumerCount"], 0);
    assert!(!claim["claimToken"].as_str().unwrap().is_empty());
    assert_eq!(
        executor
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_subscription_failure_pins_successful_selector_identity() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::new(Box::new(|request| match request {
            RadioRequest::Discover { .. } => {
                let mut services = polar_services();
                let mut second = services[0].characteristics[0].clone();
                second.uuid = SECOND.to_owned();
                services[0].characteristics.push(second);
                Reply::Now(RadioCompletion::Discovered(services))
            }
            RadioRequest::EnableNotifications { instance, .. }
                if instance.characteristic_uuid == SECOND =>
            {
                Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
                    FailureKind::PermissionDenied,
                    "second selector refused",
                )))
            }
            _ => polar_responder(request),
        }));
        let (host, _) = open(&radio, platform).await;
        let executor = host.continuation();
        let order = json!({"onAppearance":"native","resubscribe":[selector(HR_MEASUREMENT),selector(SECOND)]}).to_string();
        assert!(executor.execute(POLAR, &order).await.is_err());
        assert_eq!(radio.count(RequestKind::EnableNotifications), 2);
        assert_eq!(
            executor.execute(POLAR, &order).await.unwrap_err()["code"],
            "permission.denied"
        );
        assert_eq!(
            radio.count(RequestKind::EnableNotifications),
            3,
            "only the missing selector is retried; the successful resource stays owned"
        );
        let claim = executor.prepare_claim(256, 65536).await.unwrap();
        assert_eq!(claim["consumerCount"], 1);
        assert_eq!(claim["selectors"], json!([selector(HR_MEASUREMENT)]));
        assert_eq!(
            executor
                .acknowledge_claim(claim["claimToken"].as_str().unwrap())
                .await
                .unwrap()["disposed"],
            true
        );
        host.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_disposal_is_retryable_and_never_replays_acknowledged_bytes() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let executor = host.continuation();
        executor.execute(POLAR, &declaration()).await.unwrap();
        let claim = executor.prepare_claim(256, 65536).await.unwrap();
        let token = claim["claimToken"].as_str().unwrap();
        radio.set_responder(Box::new(|request| match request {
            RadioRequest::Disconnect { .. } => Reply::Now(RadioCompletion::Failed(
                PlatformFailure::new(FailureKind::Platform, "disconnect refused"),
            )),
            _ => polar_responder(request),
        }));
        let failed = executor.acknowledge_claim(token).await.unwrap();
        assert_eq!(
            failed["disposed"], false,
            "failed native release remains owned: {failed}"
        );
        assert!(failed["disposeFailure"].is_string());
        let retained = executor.prepare_claim(256, 65536).await.unwrap();
        assert_eq!(retained["claimToken"], token);
        assert_eq!(retained["batches"], json!([]));
        assert_eq!(
            executor.execute(POLAR, &declaration()).await.unwrap_err()["code"],
            "lifecycle.invalid-state"
        );
        radio.set_responder(Box::new(polar_responder));
        let released = executor.acknowledge_claim(token).await.unwrap();
        assert_eq!(released["disposed"], true);
        assert_eq!(executor.acknowledge_claim(token).await.unwrap(), released);
        assert_eq!(
            executor.prepare_claim(256, 65536).await.unwrap()["batches"],
            json!([])
        );
        host.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_execution_does_not_block_state_reads_or_allow_racing_claim_or_execution() {
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::Connect { .. } => Reply::Hold,
        _ => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let executor = host.continuation();
    let executing = executor.clone();
    let task = tokio::spawn(async move { executing.execute(POLAR, &declaration()).await });
    let connect = held(&radio, RequestKind::Connect).await;
    assert!(
        executor
            .declaration_replacement_failure(&declaration())
            .is_some()
    );
    let claim = tokio::time::timeout(Duration::from_secs(1), executor.prepare_claim(256, 65536))
        .await
        .unwrap();
    assert_eq!(claim.unwrap_err()["code"], "lifecycle.invalid-state");
    assert_eq!(
        executor.execute(POLAR, &declaration()).await.unwrap_err()["code"],
        "lifecycle.invalid-state"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(1), executor.describe_backlog())
            .await
            .unwrap()
            .is_err()
    );
    radio.answer(connect, RadioCompletion::Unit);
    assert_eq!(task.await.unwrap().unwrap()["resubscribed"], 1);
    assert_eq!(radio.count(RequestKind::Connect), 1);
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_acknowledgement_refuses_new_execution_without_leaking_another_session() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let executor = host.continuation();
    executor.execute(POLAR, &declaration()).await.unwrap();
    let claim = executor.prepare_claim(256, 65536).await.unwrap();
    let token = claim["claimToken"].as_str().unwrap().to_owned();
    radio.set_responder(Box::new(|request| match request {
        RadioRequest::Disconnect { .. } => Reply::Hold,
        _ => polar_responder(request),
    }));
    let acknowledging = executor.clone();
    let task = tokio::spawn(async move { acknowledging.acknowledge_claim(&token).await });
    let disconnect = held(&radio, RequestKind::Disconnect).await;
    assert_eq!(
        executor.execute(POLAR, &declaration()).await.unwrap_err()["code"],
        "lifecycle.invalid-state"
    );
    assert_eq!(radio.count(RequestKind::Connect), 1);
    radio.answer(disconnect, RadioCompletion::Unit);
    assert_eq!(task.await.unwrap().unwrap()["disposed"], true);
    host.shutdown().await;
}

struct RecordingSession(Mutex<Vec<(String, Value)>>);
impl ContinuationSession for RecordingSession {
    fn call<'a>(&'a self, operation: &'a str, arguments: &'a str) -> ContinuationFuture<'a> {
        Box::pin(async move {
            self.0.lock().unwrap().push((
                operation.to_owned(),
                serde_json::from_str(arguments).unwrap(),
            ));
            json!({"ok":true,"value":{}}).to_string()
        })
    }
    fn drain(&self, _: u32, _: u32) -> ContinuationFuture<'_> {
        Box::pin(async { panic!("execution must not drain its native backlog") })
    }
}
struct RecordingHost(Arc<RecordingSession>);
impl ContinuationHost for RecordingHost {
    fn open_session(&self) -> ubm_desktop::continuation::Result<Arc<dyn ContinuationSession>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn native_operations_preserve_direct_intent_and_foreground_scale_budgets() {
    let session = Arc::new(RecordingSession(Mutex::new(Vec::new())));
    let executor = NativeContinuation::new(Arc::new(RecordingHost(session.clone())));
    executor.execute(POLAR, &declaration()).await.unwrap();
    let calls = session.0.lock().unwrap();
    assert_eq!(calls.len(), 3);
    let (connect, connect_args) = &calls[0];
    assert_eq!(connect, "connection.connect");
    assert_eq!(
        connect_args["intent"], "direct",
        "presence wake connects to the just-appeared peer"
    );
    assert_eq!(connect_args["budgetMs"], 15_000);
    let (discover, discover_args) = &calls[1];
    assert_eq!(discover, "gatt.discover");
    assert_eq!(
        discover_args["budgetMs"], 20_000,
        "do not regress the F242 foreground discovery budget"
    );
    let (subscribe, subscribe_args) = &calls[2];
    assert_eq!(subscribe, "gatt.subscribe");
    assert_eq!(subscribe_args["budgetMs"], 10_000);
    assert_eq!(subscribe_args["selector"]["serviceOccurrence"], 0);
    assert_eq!(subscribe_args["selector"]["characteristicOccurrence"], 0);
    assert_eq!(subscribe_args["consumer"], "ubm-continuation-0");
    for (index, (_, arguments)) in calls.iter().enumerate() {
        assert_eq!(
            arguments["admission"],
            index + 1,
            "one monotonically admitted native operation"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_discovery_uses_the_cores_completion_and_does_not_invent_a_latch_failure() {
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::Discover { .. } => Reply::Hold,
        _ => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let executor = host.continuation();
    let task = tokio::spawn(async move { executor.execute(POLAR, &declaration()).await });
    let discovery = held(&radio, RequestKind::Discover).await;
    // The old reduced Kotlin regression used a 500ms local latch. Keep its
    // slow-but-successful scenario while the shared operation retains its 20s budget.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!task.is_finished());
    radio.answer(discovery, RadioCompletion::Discovered(polar_services()));
    assert_eq!(task.await.unwrap().unwrap()["resubscribed"], 1);
    host.shutdown().await;
}
