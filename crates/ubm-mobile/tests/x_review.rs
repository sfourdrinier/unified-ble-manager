//! External-review wave (X-R1/R2/R3): scan admission and connect staging.
//!
//! Each test pins the reviewer's scenario against the scripted platform
//! radio before the fix, so a failure proves the defect.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::*;
use serde_json::json;
use ubm_mobile::{
    FailureKind, MobilePlatform, PlatformFailure, RadioCompletion, RadioRequest, RequestKind,
};

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

fn filtered_scan_args(op: &str) -> String {
    json!({"serviceUuids": [HR_SERVICE], "duplicatePolicy": "all", "operationId": op}).to_string()
}

/// V01: the widening start itself can succeed after the joining caller's
/// deadline. If compensating stop succeeds, the previous physical scan is
/// gone too, so every existing member receives a terminal instead of being
/// left with a membership backed by no radio scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v01_expired_widening_start_ends_existing_members_after_cleanup() {
    let stops = Arc::new(AtomicU64::new(0));
    let starts = AtomicU64::new(0);
    let observed_stops = Arc::clone(&stops);
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::StartScan { .. } => {
            if starts.fetch_add(1, Ordering::SeqCst) == 1 {
                // `submit` is synchronous. This makes the OS answer success
                // after the caller budget while the central's work branch is
                // already being polled, deterministically reaching the host's
                // post-start liveness check.
                std::thread::sleep(Duration::from_millis(60));
            }
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::StopScan { .. } => {
            observed_stops.fetch_add(1, Ordering::SeqCst);
            Reply::Now(RadioCompletion::Unit)
        }
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session_a = host.open_session("a").unwrap();
    let session_b = host.open_session("b").unwrap();
    let membership_a =
        ok(&call(&session_a, "scan.start", &filtered_scan_args("a")).await)["operationId"]
            .as_str()
            .expect("A membership")
            .to_owned();

    let (error, _) = failure(
        &call(
            &session_b,
            "scan.start",
            &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "b", "budgetMs": 20})
                .to_string(),
        )
        .await,
    );
    assert_eq!(error["code"], "operation.timed-out", "{error}");
    let records = drain_until(&session_a, |records| {
        !of_type(records, "scan-end").is_empty()
    })
    .await;
    let ended = of_type(&records, "scan-end");
    assert_eq!(ended.len(), 1, "{records:#?}");
    assert_eq!(ended[0]["operationId"], membership_a);
    assert_eq!(ended[0]["reason"], "source-failed");
    assert_eq!(
        ok(&call(&session_a, "session.reconcile", "{}").await)["scan"],
        json!(null)
    );
    assert_eq!(stops.load(Ordering::SeqCst), 2);
}

/// V01 cleanup refusal: the replacement scan is still physically alive.
/// Keep it and the existing membership so that the member's ordinary stop
/// retries the exact retained operation instead of forgetting the debt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v01_expired_widening_start_retains_refused_cleanup_for_retry() {
    let stops = Arc::new(AtomicU64::new(0));
    let starts = AtomicU64::new(0);
    let observed_stops = Arc::clone(&stops);
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::StartScan { .. } => {
            if starts.fetch_add(1, Ordering::SeqCst) == 1 {
                std::thread::sleep(Duration::from_millis(60));
            }
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::StopScan { .. } => {
            let attempt = observed_stops.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 2 {
                Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
                    FailureKind::Platform,
                    "scripted cleanup refusal",
                )))
            } else {
                Reply::Now(RadioCompletion::Unit)
            }
        }
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session_a = host.open_session("a").unwrap();
    let session_b = host.open_session("b").unwrap();
    let membership_a =
        ok(&call(&session_a, "scan.start", &filtered_scan_args("a")).await)["operationId"]
            .as_str()
            .expect("A membership")
            .to_owned();

    let (error, _) = failure(
        &call(
            &session_b,
            "scan.start",
            &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "b", "budgetMs": 20})
                .to_string(),
        )
        .await,
    );
    assert_eq!(error["code"], "operation.timed-out", "{error}");
    assert_eq!(
        ok(&call(&session_a, "session.reconcile", "{}").await)["scan"],
        membership_a
    );
    ok(&call(
        &session_a,
        "scan.stop",
        &json!({"operationId": membership_a}).to_string(),
    )
    .await);
    assert_eq!(
        stops.load(Ordering::SeqCst),
        3,
        "the retained cleanup is retried"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_scanner_refused_compensation_is_retried_without_a_member() {
    let stops = Arc::new(AtomicU64::new(0));
    let observed_stops = Arc::clone(&stops);
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::StartScan { .. } => {
            std::thread::sleep(Duration::from_millis(60));
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::StopScan { .. } => {
            if observed_stops.fetch_add(1, Ordering::SeqCst) == 0 {
                Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
                    FailureKind::Platform,
                    "first cleanup refused",
                )))
            } else {
                Reply::Now(RadioCompletion::Unit)
            }
        }
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("first").unwrap();
    let (error, _) = failure(
        &call(
            &session,
            "scan.start",
            &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "first", "budgetMs": 20})
                .to_string(),
        )
        .await,
    );
    assert_eq!(error["code"], "operation.timed-out");
    assert_eq!(
        ok(&call(&session, "session.reconcile", "{}").await)["scan"],
        json!(null)
    );
    assert_eq!(
        ok(&call(&session, "session.dispose", "{}").await)["state"],
        "released"
    );
    wait_for(|| stops.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        stops.load(Ordering::SeqCst),
        2,
        "process owner retries the orphan once"
    );
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
/// back. Under join semantics (FX1B) both ops lease the one live link, so
/// each dispatches its own radio dial in turn; the per-peer section keeps
/// the waves serialized so every dispatch carries exactly its own
/// operation's options. Each round attributes every wave to the op that
/// completes from its answer and checks the payload matches that op.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r3_same_peer_connects_keep_their_own_options() {
    let mut mismatches = 0u32;
    let mut decisive = 0u32;
    for _ in 0..12 {
        // A future section cycle must fail loudly, never hang CI: the
        // whole round is bounded.
        let round = tokio::time::timeout(Duration::from_secs(30), one_race_roundtrip())
            .await
            .expect("one round completes within 30 s; a hang is a connect-section deadlock");
        match round {
            Some(false) => mismatches += 1,
            Some(true) => decisive += 1,
            // The round proved nothing and must not pass the suite alone.
            None => {}
        }
    }
    assert_eq!(mismatches, 0, "every dispatch carries its own options");
    assert!(decisive > 0, "at least one round pinned a dispatch");
}

/// One back-to-back pair: `Some(true)` when every wave matches its
/// completer and both ops join the one link, `Some(false)` on a mismatch,
/// `None` when the round proved nothing.
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
    // The section serializes the two dials: exactly one wave is held at a
    // time. Drain every wave (bounded): answering only the first strands
    // the joiner's dial and hangs the join below (FXA).
    let mut matched = true;
    let (mut direct_done, mut auto_done) = (false, false);
    for wave in 1..=2 {
        // The section admits one connect at a time: exactly one dispatch
        // is held.
        wait_for(|| !radio.held_of(RequestKind::Connect).is_empty()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            radio.held_of(RequestKind::Connect).len(),
            1,
            "wave {wave} is one dispatch"
        );
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
        // Attribute the wave: exactly one op completes from its answer —
        // the one that dispatched it. An op cannot complete before its own
        // wave is answered (its radio reply is its completion), so the
        // completion count tracks answered waves.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let done = usize::from(direct.is_finished()) + usize::from(auto.is_finished());
            assert!(
                tokio::time::Instant::now() < deadline,
                "wave {wave} strands its joiner: no op completed in 5 s"
            );
            if done == wave {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let owner_auto = !auto_done && auto.is_finished();
        let owner_direct = !direct_done && direct.is_finished();
        if owner_auto == owner_direct {
            // Zero or two new completions: the wave cannot be attributed.
            return None;
        }
        direct_done = direct.is_finished();
        auto_done = auto.is_finished();
        let payload_auto = matches!(
            payload,
            RadioRequest::Connect {
                auto_connect: true,
                ..
            }
        );
        matched = matched && (payload_auto == owner_auto);
    }
    // Both dials ran, so both ops are done: the join below cannot block.
    assert_eq!(
        radio.count(RequestKind::Connect),
        2,
        "one dial per caller, no more"
    );
    let (first, second) = tokio::join!(direct, auto);
    let (first, second) = (first.unwrap(), second.unwrap());
    // A second connect to a connected peer joins: both succeed on the one
    // link generation.
    let direct_value = ok(&first);
    let auto_value = ok(&second);
    matched = matched
        && direct_value["peerKey"] == auto_value["peerKey"]
        && direct_value["connectionGeneration"] == auto_value["connectionGeneration"];
    Some(matched)
}
