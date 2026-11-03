//! Cancellation while recorded subscribe is in flight retains journal metadata.
mod common;
use common::*;
use serde_json::json;
use ubm_mobile::{
    IngressStatus, Instance, MobilePlatform, RadioIngress, RadioRequest, RequestKind,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_inflight_recorded_subscribe_retains_registration_and_no_owner() {
    let directory =
        std::env::temp_dir().join(format!("ubm-recorded-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::EnableNotifications { .. } => Reply::Hold,
        request => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    host.continuation()
        .configure_recording_directory(&directory)
        .unwrap();
    let session = host.open_session("cancel-test").unwrap();
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
    let pending = tokio::spawn({
        let session = session.clone();
        async move {
            call(
                &session,
                "gatt.subscribe-recorded",
                &json!({"peerId":POLAR,"selector":selector(),
            "consumer":"recorded","operationId":"recorded-subscribe",
            "recording":{"id":"cancel","maxBytes":1048576,"maxRecords":100}})
                .to_string(),
            )
            .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while radio.held_of(RequestKind::EnableNotifications).is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let status = ok(&host.continuation_recording_status("cancel"));
    assert_eq!(
        status["records"], 1,
        "registration is committed before CCCD response"
    );
    // A host-global notification drop during CCCD enable must reach this
    // pending journal even though the active value route is not installed.
    let dropped = host.ingest(RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.into(),
            service_uuid: "invalid".into(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.into(),
            characteristic_occurrence: 0,
        },
        epoch: 1,
        value: vec![1],
    });
    assert!(matches!(dropped, IngressStatus::Dropped(_)));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while ok(&host.continuation_recording_status("cancel"))["records"]
            .as_u64()
            .unwrap()
            < 2
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let cancel = ok(&call(
        &session,
        "op.cancel",
        &json!({"operationId":"recorded-subscribe"}).to_string(),
    )
    .await);
    assert_eq!(cancel["state"], "cancellation-requested");
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failure(&result).0["code"], "operation.aborted");
    let batch = ok(&host.continuation_recording_prepare("cancel", 100, 65536));
    let records = batch["records"].as_array().unwrap();
    assert!(
        records
            .iter()
            .any(|row| row["record"]["t"] == "consumer-registration")
    );
    assert!(records.iter().any(|row| row["record"]["t"] == "stream-end"));
    assert!(
        records
            .iter()
            .any(|row| row["record"]["t"] == "ingress-drop"
                && row["record"]["class"] == "notification"
                && row["metadata"]["consumer"].is_null())
    );
    let disposal = ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(disposal["state"], "released");
    host.shutdown().await;
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_waits_for_an_already_queued_global_drop() {
    use ubm_mobile::{RadioCompletion, ReadProvenance};
    const OTHER: &str = "A0:9E:1A:00:00:02";
    let directory =
        std::env::temp_dir().join(format!("ubm-recorded-drop-fence-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::Read { .. } => Reply::Hold,
        RadioRequest::EnableNotifications { instance, .. } if instance.peer_id == OTHER => {
            Reply::Hold
        }
        request => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    host.continuation()
        .configure_recording_directory(&directory)
        .unwrap();
    let session = host.open_session("drop-fence").unwrap();
    for (peer, lease) in [(POLAR, "a"), (OTHER, "b")] {
        for op in ["connection.connect", "gatt.discover"] {
            ok(&call(
                &session,
                op,
                &json!({"peerId":peer,"lease":lease,"operationId":format!("{op}-{lease}")})
                    .to_string(),
            )
            .await);
        }
    }
    ok(&call(
        &session,
        "gatt.subscribe-recorded",
        &json!({"peerId":POLAR,"selector":selector(),
        "consumer":"first","operationId":"subscribe-first",
        "recording":{"id":"first","maxBytes":1048576,"maxRecords":100}})
        .to_string(),
    )
    .await);
    let read = tokio::spawn({
        let session = session.clone();
        async move {
            call(
                &session,
                "gatt.read",
                &json!({"peerId":POLAR,"selector":selector(),"operationId":"read"}).to_string(),
            )
            .await
        }
    });
    let read_id = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(id) = radio.held_of(RequestKind::Read).first() {
                break *id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let remove = tokio::spawn({
        let session = session.clone();
        async move {
            call(
                &session,
                "gatt.unsubscribe",
                &json!({"peerId":POLAR,"selector":selector(),
            "consumer":"first","operationId":"remove-first"})
                .to_string(),
            )
            .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !host.recorded_ingress_paused() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let pending = tokio::spawn({
        let session = session.clone();
        async move {
            call(
                &session,
                "gatt.subscribe-recorded",
                &json!({"peerId":OTHER,"selector":selector(),
            "consumer":"second","operationId":"subscribe-second",
            "recording":{"id":"second","maxBytes":1048576,"maxRecords":100}})
                .to_string(),
            )
            .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while radio.held_of(RequestKind::EnableNotifications).is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let dropped = host.ingest(RadioIngress::Notification {
        instance: Instance {
            peer_id: OTHER.into(),
            service_uuid: "invalid".into(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.into(),
            characteristic_occurrence: 0,
        },
        epoch: 1,
        value: vec![1],
    });
    assert!(matches!(dropped, IngressStatus::Dropped(_)));
    ok(&call(
        &session,
        "op.cancel",
        &json!({"operationId":"subscribe-second"}).to_string(),
    )
    .await);
    radio.answer(
        read_id,
        RadioCompletion::Read {
            value: vec![0],
            provenance: ReadProvenance::ReadResponse,
        },
    );
    ok(&read.await.unwrap());
    ok(
        &tokio::time::timeout(std::time::Duration::from_secs(3), remove)
            .await
            .unwrap()
            .unwrap(),
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failure(&result).0["code"], "operation.aborted");
    let batch = ok(&host.continuation_recording_prepare("second", 100, 65536));
    assert!(
        batch["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["record"]["t"] == "ingress-drop"
                && row["record"]["class"] == "notification"
                && row["metadata"]["consumer"].is_null())
    );
    ok(&call(&session, "session.dispose", "{}").await);
    host.shutdown().await;
    std::fs::remove_dir_all(directory).unwrap();
}
