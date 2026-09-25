//! R03: rejected-but-acquired scans retain cleanup authority across every
//! membership count and post-start cancellation/deadline outcome.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::*;
use serde_json::json;
use ubm_mobile::{
    FailureKind, MobilePlatform, PlatformFailure, RadioCompletion, RadioRequest, RequestKind,
};

#[derive(Clone, Copy, Debug)]
enum Interruption {
    Cancel,
    Deadline,
}

#[derive(Clone, Copy, Debug)]
enum Compensation {
    Success,
    Refused,
    Timeout,
}

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

async fn replacement_scan_works(host: &Arc<ubm_mobile::MobileHost>, label: &str) {
    let replacement = host.open_session("replacement-manager").unwrap();
    let replacement_id = ok(&call(
        &replacement,
        "scan.start",
        &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "replacement"})
            .to_string(),
    )
    .await)["operationId"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(&call(
        &replacement,
        "scan.stop",
        &json!({"operationId": replacement_id}).to_string(),
    )
    .await);
    assert_eq!(
        ok(&call(&replacement, "session.dispose", "{}").await)["state"],
        "released",
        "{label}"
    );
}

async fn case(members: usize, interruption: Interruption, compensation: Compensation) {
    let label =
        format!("members={members} interruption={interruption:?} compensation={compensation:?}");
    let starts = Arc::new(AtomicU64::new(0));
    let stops = Arc::new(AtomicU64::new(0));
    let observed_starts = Arc::clone(&starts);
    let observed_stops = Arc::clone(&stops);
    let late_start_index = if members == 0 { 0 } else { 1 };
    let compensation_stop_index = if members == 0 { 1 } else { 2 };
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::StartScan { .. } => {
            if observed_starts.fetch_add(1, Ordering::SeqCst) == late_start_index {
                if matches!(interruption, Interruption::Cancel) {
                    return Reply::Hold;
                }
                // submit is synchronous: completion reaches the host after
                // the caller's deadline has passed.
                std::thread::sleep(Duration::from_millis(80));
            }
            Reply::Now(RadioCompletion::Unit)
        }
        RadioRequest::StopScan { .. } => {
            let attempt = observed_stops.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == compensation_stop_index {
                match compensation {
                    Compensation::Success => Reply::Now(RadioCompletion::Unit),
                    Compensation::Refused => Reply::Now(RadioCompletion::Failed(
                        PlatformFailure::new(FailureKind::Platform, "scripted stop refusal"),
                    )),
                    Compensation::Timeout => Reply::Hold,
                }
            } else {
                Reply::Now(RadioCompletion::Unit)
            }
        }
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let mut previous = Vec::new();
    for index in 0..members {
        let session = host.open_session(&format!("prior-{index}")).unwrap();
        let membership = ok(&call(
            &session,
            "scan.start",
            &json!({"serviceUuids": [HR_SERVICE], "duplicatePolicy": "all", "operationId": format!("prior-{index}")}).to_string(),
        ).await)["operationId"].as_str().unwrap().to_owned();
        previous.push((session, membership));
    }
    assert_eq!(
        radio.count(RequestKind::StartScan),
        usize::from(members > 0),
        "{label}"
    );
    let joiner = host.open_session("joining").unwrap();
    let start_args = json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "joining", "budgetMs": 20}).to_string();
    let result = match interruption {
        Interruption::Deadline => call(&joiner, "scan.start", &start_args).await,
        Interruption::Cancel => {
            let pending = tokio::spawn({
                let joining = joiner.clone();
                async move {
                    call(&joining, "scan.start", &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "joining"}).to_string()).await
                }
            });
            wait_for(|| !radio.held_of(RequestKind::StartScan).is_empty()).await;
            let ack = ok(&call(
                &joiner,
                "op.cancel",
                &json!({"operationId": "joining"}).to_string(),
            )
            .await);
            assert_eq!(ack["state"], "cancellation-requested", "{label}");
            for id in radio.held_of(RequestKind::StartScan) {
                radio.answer(id, RadioCompletion::Unit);
            }
            pending.await.unwrap()
        }
    };
    assert_eq!(
        parse(&result)["ok"],
        false,
        "{label}: expected rejection: {result}"
    );
    let (error, _) = failure(&result);
    assert_eq!(
        error["code"],
        match interruption {
            Interruption::Cancel => "operation.aborted",
            Interruption::Deadline => "operation.timed-out",
        },
        "{label}: {error}"
    );
    assert_eq!(
        ok(&call(&joiner, "session.reconcile", "{}").await)["scan"],
        json!(null),
        "{label}"
    );

    if matches!(interruption, Interruption::Cancel) {
        // Cancellation while the radio start is outstanding settles inside
        // the central. No replacement scan was committed to the mobile host,
        // so its prior members end. Retained cleanup must recover while this
        // host remains alive, not merely during shutdown.
        for (session, membership) in &previous {
            let records =
                drain_until(session, |records| !of_type(records, "scan-end").is_empty()).await;
            let ended = of_type(&records, "scan-end");
            assert_eq!(ended.len(), 1, "{label}: {records:#?}");
            assert_eq!(ended[0]["operationId"], membership.as_str(), "{label}");
            assert_eq!(ended[0]["reason"], "source-failed", "{label}");
            assert_eq!(
                ok(&call(session, "session.reconcile", "{}").await)["scan"],
                json!(null),
                "{label}"
            );
        }
        for (session, _) in previous {
            assert_eq!(
                ok(&call(&session, "session.dispose", "{}").await)["state"],
                "released",
                "{label}"
            );
        }
        assert_eq!(
            ok(&call(&joiner, "session.dispose", "{}").await)["state"],
            "released",
            "{label}"
        );
        if !matches!(compensation, Compensation::Success) {
            wait_for(|| stops.load(Ordering::SeqCst) > compensation_stop_index).await;
        }
        replacement_scan_works(&host, &label).await;
        assert_eq!(
            parse(&host.shutdown().await)["state"],
            "released",
            "{label}"
        );
        return;
    }

    let compensation_stops = compensation_stop_index as usize;
    assert_eq!(
        radio.count(RequestKind::StopScan),
        compensation_stops,
        "{label}"
    );
    match compensation {
        Compensation::Success => {
            for (session, membership) in &previous {
                let records =
                    drain_until(session, |records| !of_type(records, "scan-end").is_empty()).await;
                let ended = of_type(&records, "scan-end");
                assert_eq!(ended.len(), 1, "{label}: {records:#?}");
                assert_eq!(ended[0]["operationId"], membership.as_str(), "{label}");
                assert_eq!(ended[0]["reason"], "source-failed", "{label}");
                assert_eq!(
                    ok(&call(session, "session.reconcile", "{}").await)["scan"],
                    json!(null),
                    "{label}"
                );
            }
        }
        Compensation::Refused | Compensation::Timeout => {
            if members == 0 {
                // The failed first scan has no public membership. The host
                // itself must retry, without a new session or React reload.
                wait_for(|| stops.load(Ordering::SeqCst) > compensation_stop_index).await;
            } else {
                for (session, membership) in &previous {
                    assert_eq!(
                        ok(&call(session, "session.reconcile", "{}").await)["scan"],
                        membership.as_str(),
                        "{label}"
                    );
                    ok(&call(
                        session,
                        "scan.stop",
                        &json!({"operationId": membership}).to_string(),
                    )
                    .await);
                }
                assert_eq!(
                    radio.count(RequestKind::StopScan),
                    compensation_stops + 1,
                    "{label}"
                );
            }
            if matches!(compensation, Compensation::Timeout) {
                let held = radio.held_of(RequestKind::StopScan);
                assert_eq!(held.len(), 1, "{label}");
                // A completion from the timed-out stop cannot retire a new
                // scan's generation after the cleanup retry has succeeded.
                let replacement = ok(&call(&joiner, "scan.start", &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "replacement"}).to_string()).await)["operationId"].as_str().unwrap().to_owned();
                radio.answer(held[0], RadioCompletion::Unit);
                assert_eq!(
                    ok(&call(&joiner, "session.reconcile", "{}").await)["scan"],
                    replacement,
                    "{label}"
                );
                ok(&call(
                    &joiner,
                    "scan.stop",
                    &json!({"operationId": replacement}).to_string(),
                )
                .await);
            }
        }
    }
    for (session, _) in previous {
        assert_eq!(
            ok(&call(&session, "session.dispose", "{}").await)["state"],
            "released",
            "{label}"
        );
    }
    assert_eq!(
        ok(&call(&joiner, "session.dispose", "{}").await)["state"],
        "released",
        "{label}"
    );
    replacement_scan_works(&host, &label).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 24)]
async fn rejected_scan_compensation_matrix() {
    let mut cases = tokio::task::JoinSet::new();
    for members in [0, 1, 2] {
        for interruption in [Interruption::Cancel, Interruption::Deadline] {
            for compensation in [
                Compensation::Success,
                Compensation::Refused,
                Compensation::Timeout,
            ] {
                cases.spawn(case(members, interruption, compensation));
            }
        }
    }
    while let Some(result) = cases.join_next().await {
        result.expect("R03 case completed without panic");
    }
}
