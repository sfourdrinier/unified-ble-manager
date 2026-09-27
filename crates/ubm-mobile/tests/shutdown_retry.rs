//! Actual MobileHost shutdown retry through the native request/completion boundary.
mod common;

use common::*;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use ubm_mobile::{
    FailureKind, MobilePlatform, PlatformFailure, RadioCompletion, RadioRequest, RequestKind,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn half_open_cleanup_retry_routes_after_host_event_pump_shutdown() {
    let mut disconnects = 0;
    let mut refused = PlatformFailure::new(
        FailureKind::Platform,
        "actual compensating disconnect refusal",
    );
    refused.native_name = Some("disconnectRefused".into());
    let expected = refused.to_error(RequestKind::Disconnect, MobilePlatform::Android);
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Connect { .. } => Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
            FailureKind::Platform,
            "connect refused",
        ))),
        RadioRequest::Disconnect { .. } => {
            disconnects += 1;
            if disconnects <= 2 {
                Reply::Now(RadioCompletion::Failed(refused.clone()))
            } else {
                Reply::Hold
            }
        }
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("shutdown-retry").unwrap();
    failure(
        &call(
            &session,
            "connection.connect",
            &json!({
                "peerId": POLAR, "lease": "failed-link", "operationId": "connect"
            })
            .to_string(),
        )
        .await,
    );
    assert_eq!(radio.count(RequestKind::Disconnect), 1);

    let first = parse(
        &tokio::time::timeout(Duration::from_secs(5), host.shutdown())
            .await
            .expect("first shutdown settles"),
    );
    assert_eq!(first["state"], "release-failed", "{first}");
    let failures = first["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1, "no duplicate generic failure: {first}");
    let error = &failures[0];
    assert_eq!(error["resourceKind"], "connection");
    assert_eq!(error["code"], expected.code_str());
    assert_eq!(error["domain"], expected.domain().as_str());
    assert_eq!(error["operation"], expected.operation());
    assert_eq!(error["detail"], expected.detail().unwrap());
    assert_eq!(error["platform"]["domain"], "android");
    assert_eq!(error["platform"]["code"], "disconnectRefused");
    assert_eq!(radio.count(RequestKind::Disconnect), 2);

    // shutdown already closed events/signals and joined its pump. A new native
    // request must still dispatch and accept its actual completion for retry.
    let retry = tokio::spawn({
        let host = Arc::clone(&host);
        async move { host.shutdown().await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while radio.held_of(RequestKind::Disconnect).is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retry reaches native disconnect after pump shutdown");
    assert!(
        !retry.is_finished(),
        "cannot claim release before native completion"
    );
    for id in radio.held_of(RequestKind::Disconnect) {
        radio.answer(id, RadioCompletion::Unit);
    }
    let receipt = parse(
        &tokio::time::timeout(Duration::from_secs(5), retry)
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(receipt, json!({"state":"released", "failures":[]}));
    assert_eq!(radio.count(RequestKind::Disconnect), 3);
    assert_eq!(parse(&host.shutdown().await), receipt);
    assert_eq!(
        radio.count(RequestKind::Disconnect),
        3,
        "confirmed release retires debt"
    );
}
