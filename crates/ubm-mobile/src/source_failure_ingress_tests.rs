use crate::test_common::*;
use serde_json::json;

fn instance() -> crate::Instance {
    crate::Instance {
        peer_id: POLAR.into(),
        service_uuid: HR_SERVICE.into(),
        service_occurrence: 0,
        characteristic_uuid: HR_MEASUREMENT.into(),
        characteristic_occurrence: 0,
    }
}

#[test]
fn acknowledged_failure_without_native_detail_keeps_original_evidence() {
    use ubm_desktop::errors::PlatformValue;
    let failure = crate::PlatformFailure::not_dispatched(
        crate::FailureKind::PermissionDenied,
        "permission was refused",
    );
    let error = failure.to_acknowledged_source_error(crate::MobilePlatform::Apple);
    let platform = error.platform().unwrap();
    assert_eq!(platform.domain, "ubm-mobile");
    assert_eq!(platform.code, "notification-source-failure-evidence");
    assert_eq!(
        platform.metadata["sourceFailureKind"],
        PlatformValue::Text("permission-denied".into())
    );
    assert_eq!(
        platform.metadata["sourceFailureDetail"],
        PlatformValue::Text("permission was refused".into())
    );
    assert_eq!(
        platform.metadata["sourceFailureDispatched"],
        PlatformValue::Bool(false)
    );
    assert!(!platform.metadata.contains_key("sourceFailureNativeCode"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_failure_retries_after_pressure_then_preserves_exact_terminal() {
    use crate::SourceFailureIngressStatus as Status;
    let radio = Scripted::polar();
    let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
    let session = host.open_session("retry-source").unwrap();
    for (op, id) in [
        ("connection.connect", "connect"),
        ("gatt.discover", "discover"),
    ] {
        ok(&call(
            &session,
            op,
            &json!({"peerId":POLAR,"lease":"lease","operationId":id}).to_string(),
        )
        .await);
    }
    ok(&call(
        &session,
        "gatt.subscribe",
        &json!({"peerId":POLAR,"selector":selector(),"consumer":"hr","operationId":"subscribe"})
            .to_string(),
    )
    .await);
    let epoch = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|request| match request {
            crate::RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .unwrap();
    let fence = host.inner.radio.fence_ingress().await.unwrap();
    for _ in 0..crate::foreign::CONTROL_INGRESS_CAP {
        host.inner
            .radio
            .push_event(ubm_desktop::RadioEvent::EventsLost { skipped: 1 })
            .unwrap();
    }
    let owner = instance();
    let failure = crate::PlatformFailure {
        gatt_status: Some(133),
        native_name: Some("EXACT".into()),
        native_domain: Some("original.domain".into()),
        native_code: Some(987),
        dispatched: false,
        ..crate::PlatformFailure::new(crate::FailureKind::GattStatus, "exact failure")
    };
    let drops = host.inner.radio.counters().control_drops;
    assert_eq!(
        host.ingest_notification_source_failure(&owner, epoch, &failure),
        Status::Full
    );
    assert_eq!(host.inner.radio.counters().control_drops, drops);
    assert!(!host.inner.is_shut_down());
    drop(fence);
    // Only the caller-held receipt is retried once the source FIFO has room.
    // Acceptance transfers it into the ordered control FIFO without changing
    // its native evidence or turning queue pressure into a dropped record.
    let mut accepted = false;
    for _ in 0..100 {
        match host.ingest_notification_source_failure(&owner, epoch, &failure) {
            Status::Accepted => {
                accepted = true;
                break;
            }
            Status::Full => tokio::task::yield_now().await,
            status => panic!("retry changed source-failure ownership: {status:?}"),
        }
    }
    assert!(
        accepted,
        "the released source FIFO must eventually accept its exact terminal"
    );
    assert_eq!(failure.detail, "exact failure");
    assert_eq!(failure.native_code, Some(987));
    assert_eq!(
        epoch,
        radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .find_map(|request| match request {
                crate::RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
                _ => None,
            })
            .unwrap()
    );
    let records = drain_until(&session, |records| {
        !of_type(records, "stream-end").is_empty()
    })
    .await;
    let terminal = &of_type(&records, "stream-end")[0];
    assert_eq!(terminal["reason"], "source-failed");
    assert_eq!(
        terminal["error"]["platform"]["metadata"]["sourceFailureNativeDomain"],
        "original.domain"
    );
    assert_eq!(
        terminal["error"]["platform"]["metadata"]["sourceFailureNativeCode"],
        987
    );
}

#[tokio::test]
async fn source_failure_closed_and_invalid_never_claim_acceptance() {
    use crate::SourceFailureIngressStatus as Status;
    let radio = Scripted::polar();
    let (host, _) = open(&radio, crate::MobilePlatform::Apple).await;
    let owner = instance();
    let failure = crate::PlatformFailure::new(crate::FailureKind::Platform, "retained");
    let mut invalid = owner.clone();
    invalid.service_uuid = "invalid".into();
    assert_eq!(
        host.ingest_notification_source_failure(&invalid, 9, &failure),
        Status::Invalid
    );
    assert!(!host.inner.is_shut_down());
    host.shutdown().await;
    assert_eq!(
        host.ingest_notification_source_failure(&owner, 9, &failure),
        Status::Closed
    );
    assert_eq!(failure.detail, "retained");
}

#[tokio::test]
async fn closed_radio_refuses_even_before_host_shutdown() {
    use crate::SourceFailureIngressStatus as Status;
    let radio = Scripted::polar();
    let (host, _) = open(&radio, crate::MobilePlatform::Apple).await;
    let owner = instance();
    let failure = crate::PlatformFailure::new(crate::FailureKind::Platform, "retained");
    host.inner.radio.close_events();
    assert!(!host.inner.is_shut_down());
    assert_eq!(
        host.ingest_notification_source_failure(&owner, 9, &failure),
        Status::Closed
    );
    assert_eq!(owner, instance());
    assert_eq!(failure.detail, "retained");
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_failure_stays_with_original_owner_across_session_disposal() {
    use crate::SourceFailureIngressStatus as Status;
    let radio = Scripted::polar();
    let (host, _) = open(&radio, crate::MobilePlatform::Android).await;
    let old = host.open_session("old-source").unwrap();
    for (op, id) in [
        ("connection.connect", "connect"),
        ("gatt.discover", "discover"),
    ] {
        ok(&call(
            &old,
            op,
            &json!({"peerId":POLAR,"lease":"lease","operationId":id}).to_string(),
        )
        .await);
    }
    ok(&call(
        &old,
        "gatt.subscribe",
        &json!({"peerId":POLAR,"selector":selector(),"consumer":"old","operationId":"subscribe"})
            .to_string(),
    )
    .await);
    let old_epoch = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|request| match request {
            crate::RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .unwrap();
    let owner = instance();
    let failure = crate::PlatformFailure::new(crate::FailureKind::Platform, "old owner failure");
    let fence = host.inner.radio.fence_ingress().await.unwrap();
    for _ in 0..crate::foreign::CONTROL_INGRESS_CAP {
        host.inner
            .radio
            .push_event(ubm_desktop::RadioEvent::EventsLost { skipped: 1 })
            .unwrap();
    }
    assert_eq!(
        host.ingest_notification_source_failure(&owner, old_epoch, &failure),
        Status::Full
    );
    drop(fence);
    ok(&call(&old, "session.dispose", "{}").await);

    let replacement = host.open_session("new-source").unwrap();
    for (op, id) in [
        ("connection.connect", "new-connect"),
        ("gatt.discover", "new-discover"),
    ] {
        ok(&call(
            &replacement,
            op,
            &json!({"peerId":POLAR,"lease":"lease","operationId":id}).to_string(),
        )
        .await);
    }
    ok(&call(&replacement, "gatt.subscribe", &json!({"peerId":POLAR,"selector":selector(),"consumer":"new","operationId":"new-subscribe"}).to_string()).await);
    let new_epoch = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| match request {
            crate::RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .last()
        .unwrap();
    assert_ne!(new_epoch, old_epoch);
    assert_eq!(
        host.ingest_notification_source_failure(&owner, old_epoch, &failure),
        Status::Accepted
    );
    assert_eq!(
        host.ingest(crate::RadioIngress::Notification {
            instance: owner.clone(),
            epoch: new_epoch,
            value: vec![42]
        }),
        crate::IngressStatus::Accepted
    );
    let records = drain_until(&replacement, |records| {
        !of_type(records, "value").is_empty()
    })
    .await;
    assert!(of_type(&records, "stream-end").is_empty());
    assert_eq!(failure.detail, "old owner failure");
}
