// crates/ubm-mobile/tests/ordinary_recording_loss.rs
mod common;
use common::*;
use serde_json::json;
use ubm_mobile::{Instance, MobilePlatform, RadioIngress, RadioRequest};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_final_drain_reports_loss_after_radio_release() {
    use ubm_mobile::{RadioCompletion, RequestKind};
    let directory =
        std::env::temp_dir().join(format!("ubm-recorded-disable-loss-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::DisableNotifications { .. } => Reply::Hold,
        request => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    host.continuation()
        .configure_recording_directory(&directory)
        .unwrap();
    let session = host.open_session("recorded-disable-test").unwrap();
    for (op, id) in [
        ("connection.connect", "connect"),
        ("gatt.discover", "discover"),
    ] {
        ok(&call(
            &session,
            op,
            &json!({"peerId":POLAR,"lease":"lease", "operationId":id}).to_string(),
        )
        .await);
    }
    ok(&call(
        &session,
        "gatt.subscribe-recorded",
        &json!({"peerId":POLAR,"selector":selector(),
        "consumer":"recorded", "operationId":"recorded-subscribe",
        "recording":{"id":"disable", "maxBytes":1048576, "maxRecords":1}})
        .to_string(),
    )
    .await);
    let epoch = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|request| match request {
            RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .unwrap();
    let pending = tokio::spawn({
        let session = session.clone();
        async move {
            call(
                &session,
                "gatt.unsubscribe",
                &json!({"peerId":POLAR,"selector":selector(),
            "consumer":"recorded", "operationId":"recorded-unsubscribe"})
                .to_string(),
            )
            .await
        }
    });
    let id = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(id) = radio.held_of(RequestKind::DisableNotifications).first() {
                break *id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
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
        value: vec![7, 8, 9],
    });
    host.ingest(RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.into(),
            service_uuid: HR_SERVICE.into(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.into(),
            characteristic_occurrence: 0,
        },
        epoch,
        value: vec![1, 2, 3, 4],
    });
    radio.answer(id, RadioCompletion::Unit);
    ok(
        &tokio::time::timeout(std::time::Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap(),
    );
    let status = ok(&host.continuation_recording_status("disable"));
    assert_eq!(status["lostRecords"], 2);
    assert_eq!(
        status["collectionFailure"]["knownRecordedLoss"],
        json!({"items":2,"bytes":7})
    );
    assert_eq!(status["collectionFailure"]["persisted"], true);
    ok(&call(&session, "session.dispose", "{}").await);
    host.shutdown().await;
    std::fs::remove_dir_all(directory).unwrap();
}
