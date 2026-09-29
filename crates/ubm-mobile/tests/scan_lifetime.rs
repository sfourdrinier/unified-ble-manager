//! Native membership lifetime must not depend on a JavaScript timer or drain.
mod common;

use common::*;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, oneshot};
use ubm_mobile::{
    Advertisement, FailureKind, HostOptions, MobileHost, MobilePlatform, MobileSession,
    PlatformFailure, PlatformRadio, RadioCompletion, RadioIngress, RadioRequest, RequestKind,
    WakeSink,
};

#[derive(Default)]
struct Wake(Notify, AtomicU64);

impl WakeSink for Wake {
    fn wake(&self, _: u64) {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.notify_one();
    }
}

async fn fixture(radio: &Arc<Scripted>, platform: MobilePlatform) -> (Arc<MobileHost>, Arc<Wake>) {
    let wake = Arc::new(Wake::default());
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&wake) as Arc<dyn WakeSink>,
            HostOptions {
                platform,
                owner: "native-scan-lifetime-test".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    (host, wake)
}

async fn terminal(session: &MobileSession, wake: &Wake) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(1), async {
        let mut records = Vec::new();
        loop {
            let notified = wake.0.notified();
            records.extend(
                parse(&session.drain(256, 65536))["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned(),
            );
            if records.iter().any(|record| record["t"] == "scan-end") {
                return records;
            }
            notified.await;
        }
    })
    .await
    .expect("native expiry must produce its own terminal")
}

async fn finite_start(session: &MobileSession) -> Value {
    ok(&call(
        session,
        "scan.start",
        &json!({
            "serviceUuids":["180d"], "duplicatePolicy":"all", "operationId":"finite",
            "lifetimeMs":100
        })
        .to_string(),
    )
    .await)
}

async fn expires_without_drain(platform: MobilePlatform) {
    let (sent, stopped) = oneshot::channel();
    let sent = Mutex::new(Some(sent));
    let radio = Scripted::new(Box::new(move |request| {
        if request.kind() == RequestKind::StopScan
            && let Some(sent) = sent.lock().unwrap().take()
        {
            sent.send(()).unwrap();
        }
        polar_responder(request)
    }));
    let (host, wake) = fixture(&radio, platform).await;
    let session = host.open_session("finite").unwrap();
    let started = finite_start(&session).await;
    // No JS or wire drain occurs until AFTER the real radio receives stop.
    tokio::time::advance(Duration::from_millis(101)).await;
    tokio::time::timeout(Duration::from_secs(1), stopped)
        .await
        .unwrap()
        .unwrap();
    let records = terminal(&session, &wake).await;
    let ends = of_type(&records, "scan-end");
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0]["operationId"], started["operationId"]);
    assert_eq!(ends[0]["reason"], "operation-timed-out");
    assert_eq!(radio.count(RequestKind::StopScan), 1);
    assert!(ok(&call(&session, "session.reconcile", "{}").await)["scan"].is_null());
    assert_eq!(
        ok(&call(&session, "session.dispose", "{}").await)["state"],
        "released"
    );
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(start_paused = true)]
async fn android_finite_scan_expires_without_javascript_or_wire_drain() {
    expires_without_drain(MobilePlatform::Android).await;
}

#[tokio::test(start_paused = true)]
async fn apple_finite_scan_expires_without_javascript_or_wire_drain() {
    expires_without_drain(MobilePlatform::Apple).await;
}

#[tokio::test(start_paused = true)]
async fn finite_member_expiry_preserves_a_shared_unbounded_member() {
    let radio = Scripted::polar();
    let (host, wake) = fixture(&radio, MobilePlatform::Android).await;
    let finite = host.open_session("finite").unwrap();
    let survivor = host.open_session("survivor").unwrap();
    finite_start(&finite).await;
    let survivor_id = ok(&call(
        &survivor,
        "scan.start",
        &json!({
            "serviceUuids":["180d"], "duplicatePolicy":"all", "operationId":"unbounded"
        })
        .to_string(),
    )
    .await)["operationId"]
        .clone();
    assert_eq!(radio.count(RequestKind::StartScan), 1);
    tokio::time::advance(Duration::from_millis(101)).await;
    terminal(&finite, &wake).await;
    assert_eq!(radio.count(RequestKind::StopScan), 0);
    assert_eq!(
        ok(&call(&survivor, "session.reconcile", "{}").await)["scan"],
        survivor_id
    );
    assert_eq!(
        ok(&call(&finite, "session.dispose", "{}").await)["state"],
        "released"
    );
    assert_eq!(
        ok(&call(&survivor, "session.dispose", "{}").await)["state"],
        "released"
    );
    assert_eq!(radio.count(RequestKind::StopScan), 1);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn buffered_advertisement_retains_its_membership_across_restart() {
    let radio = Scripted::polar();
    let (host, wake) = fixture(&radio, MobilePlatform::Android).await;
    let session = host.open_session("restart").unwrap();
    let old = ok(&call(
        &session,
        "scan.start",
        &json!({
            "serviceUuids":["180d"], "duplicatePolicy":"all", "operationId":"old"
        })
        .to_string(),
    )
    .await);
    // Clear setup records before observing an advertisement wake, but do not
    // drain its payload until after the next native membership exists.
    let _ = session.drain(256, 65536);
    let before = wake.1.load(Ordering::SeqCst);
    host.ingest(RadioIngress::Advertisement(Advertisement {
        peer_id: POLAR.into(),
        service_uuids: vec![HR_SERVICE.into()],
        ..Advertisement::default()
    }));
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let notified = wake.0.notified();
            if wake.1.load(Ordering::SeqCst) > before {
                break;
            }
            notified.await;
        }
    })
    .await
    .unwrap();
    ok(&call(
        &session,
        "scan.stop",
        &json!({"operationId":old["operationId"]}).to_string(),
    )
    .await);
    let new = finite_start(&session).await;
    assert_ne!(old["operationId"], new["operationId"]);
    let records = parse(&session.drain(256, 65536));
    let advertisements = of_type(records["records"].as_array().unwrap(), "adv");
    assert_eq!(advertisements.len(), 1, "{records}");
    assert_eq!(advertisements[0]["operationId"], old["operationId"]);
    assert_eq!(advertisements[0]["startOperationId"], "old");
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(start_paused = true)]
async fn stopping_then_restarting_does_not_apply_the_old_deadline() {
    let radio = Scripted::polar();
    let (host, _) = fixture(&radio, MobilePlatform::Android).await;
    let session = host.open_session("restart").unwrap();
    let old = finite_start(&session).await;
    ok(&call(
        &session,
        "scan.stop",
        &json!({"operationId":old["operationId"]}).to_string(),
    )
    .await);
    let new = ok(&call(
        &session,
        "scan.start",
        &json!({
            "serviceUuids":["180d"], "duplicatePolicy":"all", "operationId":"unbounded"
        })
        .to_string(),
    )
    .await);
    tokio::time::advance(Duration::from_millis(101)).await;
    assert_eq!(
        ok(&call(&session, "session.reconcile", "{}").await)["scan"],
        new["operationId"]
    );
    assert_eq!(radio.count(RequestKind::StopScan), 1);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
    assert_eq!(radio.count(RequestKind::StopScan), 2);
}

#[tokio::test(start_paused = true)]
async fn invalid_lifetimes_are_rejected_before_radio_admission() {
    let radio = Scripted::polar();
    let (host, _) = fixture(&radio, MobilePlatform::Android).await;
    let session = host.open_session("invalid").unwrap();
    for lifetime in [
        json!(0),
        json!(-1),
        json!(0.5),
        json!(2147483648_u64),
        json!("100"),
    ] {
        let response = call(
            &session,
            "scan.start",
            &json!({
                "serviceUuids":["180d"], "duplicatePolicy":"all", "operationId":"invalid",
                "lifetimeMs":lifetime
            })
            .to_string(),
        )
        .await;
        assert_eq!(
            failure(&response).0["code"],
            "argument.invalid",
            "{response}"
        );
    }
    assert_eq!(radio.count(RequestKind::StartScan), 0);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn refused_expiry_cleanup_remains_process_owned_after_session_disposal() {
    let (first_tx, first_rx) = oneshot::channel();
    let (retry_tx, retry_rx) = oneshot::channel();
    let mut first = Some(first_tx);
    let mut retry = Some(retry_tx);
    let radio = Scripted::new(Box::new(move |request| {
        if request.kind() == RequestKind::StopScan {
            if let Some(sent) = first.take() {
                sent.send(()).unwrap();
                return Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
                    FailureKind::Platform,
                    "expiry cleanup refusal",
                )));
            }
            if let Some(sent) = retry.take() {
                sent.send(()).unwrap();
            }
        }
        polar_responder(request)
    }));
    let (host, wake) = fixture(&radio, MobilePlatform::Android).await;
    let session = host.open_session("disposed").unwrap();
    finite_start(&session).await;
    tokio::time::timeout(Duration::from_secs(2), first_rx)
        .await
        .unwrap()
        .unwrap();
    let records = terminal(&session, &wake).await;
    assert_eq!(of_type(&records, "scan-end").len(), 1);
    assert!(ok(&call(&session, "session.reconcile", "{}").await)["scan"].is_null());
    assert_eq!(
        ok(&call(&session, "session.dispose", "{}").await)["state"],
        "released"
    );
    // No session remains to issue a retry. The process owner must do it.
    tokio::time::timeout(Duration::from_secs(2), retry_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
    assert_eq!(radio.count(RequestKind::StopScan), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_finishing_after_lifetime_is_compensated_before_publication() {
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, RadioRequest::StartScan { .. }) {
            // The platform answers inside submit only after the original
            // lifetime; a larger admission budget cannot renew that clock.
            std::thread::sleep(Duration::from_millis(60));
        }
        polar_responder(request)
    }));
    let (host, _) = fixture(&radio, MobilePlatform::Android).await;
    let session = host.open_session("late").unwrap();
    let response = call(
        &session,
        "scan.start",
        &json!({
            "serviceUuids":["180d"], "duplicatePolicy":"all", "operationId":"late",
            "lifetimeMs":20, "budgetMs":1000
        })
        .to_string(),
    )
    .await;
    assert_eq!(failure(&response).0["code"], "operation.timed-out");
    assert_eq!(radio.count(RequestKind::StartScan), 1);
    assert_eq!(radio.count(RequestKind::StopScan), 1);
    assert!(ok(&call(&session, "session.reconcile", "{}").await)["scan"].is_null());
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn expiry_during_widening_preserves_the_new_membership() {
    let (start_tx, start_rx) = oneshot::channel();
    let mut start_tx = Some(start_tx);
    let mut starts = 0;
    let radio = Scripted::new(Box::new(move |request| {
        if request.kind() == RequestKind::StartScan {
            starts += 1;
            if starts == 2 {
                start_tx.take().unwrap().send(request.id()).unwrap();
                return Reply::Hold;
            }
        }
        polar_responder(request)
    }));
    let (host, wake) = fixture(&radio, MobilePlatform::Android).await;
    let finite = host.open_session("finite").unwrap();
    let survivor = host.open_session("survivor").unwrap();
    finite_start(&finite).await;
    let pending = tokio::spawn({
        let survivor = survivor.clone();
        async move {
            call(
                &survivor,
                "scan.start",
                &json!({
                    "serviceUuids":[], "duplicatePolicy":"all", "operationId":"wide"
                })
                .to_string(),
            )
            .await
        }
    });
    let held = tokio::time::timeout(Duration::from_secs(1), start_rx)
        .await
        .unwrap()
        .unwrap();
    // Expiry removes the old member while its physical replacement is held.
    // Its cleanup waits behind admission and must preserve the new owner.
    let ended = terminal(&finite, &wake).await;
    assert_eq!(
        of_type(&ended, "scan-end")[0]["reason"],
        "operation-timed-out"
    );
    radio.answer(held, RadioCompletion::Unit);
    let started = ok(&pending.await.unwrap());
    assert_eq!(
        ok(&call(&survivor, "session.reconcile", "{}").await)["scan"],
        started["operationId"]
    );
    assert_eq!(radio.count(RequestKind::StopScan), 1);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
    assert_eq!(radio.count(RequestKind::StopScan), 2);
}
