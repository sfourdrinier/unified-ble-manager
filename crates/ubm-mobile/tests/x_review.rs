//! External-review wave (X-R1/R2/R3): scan admission and connect staging.
//!
//! Each test pins the reviewer's scenario against the scripted platform
//! radio before the fix, so a failure proves the defect.

mod common;

use std::time::Duration;

use common::*;
use serde_json::json;
use ubm_mobile::{MobilePlatform, RadioCompletion, RadioRequest, RequestKind};

async fn wait_for(condition: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not met in 5 s"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn scan_args(op: &str) -> String {
    json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": op}).to_string()
}

/// X-R1: B queues behind A's held scan start; cancelling B must fail B with
/// no membership while A stays healthy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r1_cancelled_queued_scan_start_leaves_no_membership() {
    let radio = Scripted::new(Box::new(|request| match request {
        ubm_mobile::RadioRequest::StartScan { .. } => Reply::Hold,
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session_a = host.open_session("a").unwrap();
    let session_b = host.open_session("b").unwrap();

    let a = tokio::spawn({
        let session = session_a.clone();
        async move { call(&session, "scan.start", &scan_args("a")).await }
    });
    wait_for(|| !radio.held_of(RequestKind::StartScan).is_empty()).await;
    let b = tokio::spawn({
        let session = session_b.clone();
        async move { call(&session, "scan.start", &scan_args("b")).await }
    });
    // B is queued on the shared scan mutex behind A (not yet admitted to the
    // radio: still exactly one held start).
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(radio.held_of(RequestKind::StartScan).len(), 1);
    let ack = ok(&call(
        &session_b,
        "op.cancel",
        &json!({"operationId": "b"}).to_string(),
    )
    .await);
    assert_eq!(ack["state"], "cancellation-requested");
    for id in radio.held_of(RequestKind::StartScan) {
        radio.answer(id, RadioCompletion::Unit);
    }
    let membership_a = ok(&a.await.unwrap())["operationId"]
        .as_str()
        .expect("A membership")
        .to_owned();
    let (error, _) = failure(&b.await.unwrap());
    assert_eq!(error["code"], "operation.aborted", "{error}");
    // A stays healthy: its membership stops cleanly, and the session holds it
    // (reconcile), so B left no membership behind.
    let reconcile = ok(&call(&session_a, "session.reconcile", "{}").await);
    assert_eq!(reconcile["scan"], json!(membership_a));
    ok(&call(
        &session_a,
        "scan.stop",
        &json!({"operationId": membership_a}).to_string(),
    )
    .await);
}

/// X-R1 (deadline): B queues behind A's held scan start with a short budget;
/// the expired B must fail without taking a membership.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r1_expired_queued_scan_start_leaves_no_membership() {
    let radio = Scripted::new(Box::new(|request| match request {
        ubm_mobile::RadioRequest::StartScan { .. } => Reply::Hold,
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session_a = host.open_session("a").unwrap();
    let session_b = host.open_session("b").unwrap();

    let a = tokio::spawn({
        let session = session_a.clone();
        async move { call(&session, "scan.start", &scan_args("a")).await }
    });
    wait_for(|| !radio.held_of(RequestKind::StartScan).is_empty()).await;
    let b = tokio::spawn({
        let session = session_b.clone();
        async move {
            call(
                &session,
                "scan.start",
                &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "b", "budgetMs": 50})
                    .to_string(),
            )
            .await
        }
    });
    // B's budget expires while queued; only then release A.
    let (error, _) = failure(&b.await.unwrap());
    assert_eq!(error["code"], "operation.timed-out", "{error}");
    for id in radio.held_of(RequestKind::StartScan) {
        radio.answer(id, RadioCompletion::Unit);
    }
    let membership_a = ok(&a.await.unwrap())["operationId"]
        .as_str()
        .expect("A membership")
        .to_owned();
    let reconcile = ok(&call(&session_a, "session.reconcile", "{}").await);
    assert_eq!(reconcile["scan"], json!(membership_a));
}

/// X-R2: two concurrent starts on one session while the first is still
/// admitted at the radio. The slot is reserved before the first await, so
/// the second is refused scan.already-active at once; stopping the winner
/// leaves no orphan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r2_concurrent_starts_on_one_session_admit_exactly_one() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let starts = AtomicU64::new(0);
    let radio = Scripted::new(Box::new(move |request| match request {
        ubm_mobile::RadioRequest::StartScan { .. }
            if starts.fetch_add(1, Ordering::SeqCst) == 0 =>
        {
            Reply::Hold
        }
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();

    let first = tokio::spawn({
        let session = session.clone();
        async move { call(&session, "scan.start", &scan_args("s1")).await }
    });
    wait_for(|| !radio.held_of(RequestKind::StartScan).is_empty()).await;
    // The first start is still inside its radio call; the second must be
    // refused promptly, not queued behind it.
    let second = tokio::time::timeout(
        Duration::from_secs(2),
        call(&session, "scan.start", &scan_args("s2")),
    )
    .await
    .expect("the second start is refused promptly, never queued");
    let (error, _) = failure(&second);
    assert_eq!(error["code"], "scan.already-active", "{error}");
    for id in radio.held_of(RequestKind::StartScan) {
        radio.answer(id, RadioCompletion::Unit);
    }
    let winner = ok(&first.await.unwrap())["operationId"]
        .as_str()
        .expect("membership")
        .to_owned();
    ok(&call(
        &session,
        "scan.stop",
        &json!({"operationId": winner}).to_string(),
    )
    .await);
    // No orphan membership: a fresh start succeeds immediately.
    ok(&call(&session, "scan.start", &scan_args("s3")).await);
}
/// X-R3: two same-peer connects with different options, spawned back to
/// back. Staging is keyed by peer, so the second stager overwrites the
/// first; the first dispatch must still carry its own operation's options.
/// Each round attributes the single dispatch to the op that completes from
/// its answer and checks the payload matches that op.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r3_same_peer_connects_keep_their_own_options() {
    let mut mismatches = 0u32;
    let mut decisive = 0u32;
    for _ in 0..12 {
        match one_race_roundtrip().await {
            Some(false) => mismatches += 1,
            Some(true) => decisive += 1,
            // Both ops won a dispatch or neither settled: the round proves
            // nothing and must not pass the suite on its own.
            None => {}
        }
    }
    assert_eq!(mismatches, 0, "every dispatch carries its own options");
    assert!(decisive > 0, "at least one round pinned a dispatch");
}

/// One back-to-back pair: `Some(true)` when the dispatch matches its
/// completer, `Some(false)` on a mismatch, `None` when the round proved
/// nothing (both settled or neither did).
async fn one_race_roundtrip() -> Option<bool> {
    let radio = Scripted::new(Box::new(|request| match request {
        ubm_mobile::RadioRequest::Connect { .. } => Reply::Hold,
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session_a = host.open_session("a").unwrap();
    let session_b = host.open_session("b").unwrap();

    let direct = tokio::spawn({
        let session = session_a.clone();
        async move {
            call(
                &session,
                "connection.connect",
                &json!({"peerId": POLAR, "lease": "la", "operationId": "ca"}).to_string(),
            )
            .await
        }
    });
    let auto = tokio::spawn({
        let session = session_b.clone();
        async move {
            call(
                &session,
                "connection.connect",
                &json!({"peerId": POLAR, "lease": "lb", "intent": "when-available", "operationId": "cb"})
                    .to_string(),
            )
            .await
        }
    });
    // The core admits one connect at a time: exactly one dispatch is held.
    wait_for(|| !radio.held_of(RequestKind::Connect).is_empty()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(radio.held_of(RequestKind::Connect).len(), 1);
    let payload = radio
        .held
        .lock()
        .unwrap()
        .values()
        .find(|request| request.kind() == RequestKind::Connect)
        .cloned()
        .expect("one held connect");
    for id in radio.held_of(RequestKind::Connect) {
        radio.answer(id, RadioCompletion::Unit);
    }
    // Attribute the dispatch: the op that completes from this answer is the
    // one that dispatched it.
    let (first, second) = tokio::join!(direct, auto);
    let (first, second) = (first.unwrap(), second.unwrap());
    let direct_ok = parse(&first)["ok"] == json!(true);
    let auto_ok = parse(&second)["ok"] == json!(true);
    // Park the loser so no task outlives the round.
    if !direct_ok {
        let _ = call(
            &session_a,
            "op.cancel",
            &json!({"operationId": "ca"}).to_string(),
        )
        .await;
    }
    if !auto_ok {
        let _ = call(
            &session_b,
            "op.cancel",
            &json!({"operationId": "cb"}).to_string(),
        )
        .await;
    }
    // Exactly one op wins the dispatch; otherwise the round proves nothing.
    if direct_ok == auto_ok {
        return None;
    }
    let expected_auto = auto_ok && !direct_ok;
    let payload_auto = matches!(
        payload,
        RadioRequest::Connect {
            auto_connect: true,
            ..
        }
    );
    Some(expected_auto == payload_auto)
}
