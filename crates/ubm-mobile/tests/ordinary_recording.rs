//! Recorded ordinary consumers share the radio without sharing delivery.
mod common;
use common::*;
use serde_json::json;
use ubm_mobile::{Instance, MobilePlatform, RadioIngress, RadioRequest};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recorded_and_ordinary_consumers_have_independent_delivery_and_cursor() {
    let directory =
        std::env::temp_dir().join(format!("ubm-ordinary-recording-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    host.continuation()
        .configure_recording_directory(&directory)
        .unwrap();
    let session = host.open_session("ordinary-recording-test").unwrap();
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
        "recording":{"id":"ordinary", "maxBytes":1048576, "maxRecords":1000}})
        .to_string(),
    )
    .await);
    ok(&call(
        &session,
        "gatt.subscribe",
        &json!({"peerId":POLAR,"selector":selector(),
        "consumer":"ordinary", "operationId":"ordinary-subscribe"})
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
    host.ingest(RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.into(),
            service_uuid: HR_SERVICE.into(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.into(),
            characteristic_occurrence: 0,
        },
        epoch,
        value: vec![1, 2, 3],
    });
    let ordinary = drain_until(&session, |records| {
        records
            .iter()
            .any(|row| row["t"] == "value" && row["consumer"] == "ordinary")
            && records.iter().any(|row| {
                row["t"] == "recording-changed"
                    && row["consumer"] == "recorded"
                    && row["recordingId"] == "ordinary"
            })
    })
    .await;
    assert!(
        !ordinary
            .iter()
            .any(|row| row["t"] == "value" && row["consumer"] == "recorded")
    );
    // The hint is delivered only after the journal commit: no status polling
    // or additional wake is needed before prepare sees the value.
    let first = ok(&host.continuation_recording_prepare("ordinary", 100, 65536));
    let value = first["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["record"]["t"] == "value")
        .unwrap();
    assert_eq!(value["record"]["valueB64"], "AQID");
    assert_eq!(value["record"]["receipt"]["origin"], "native");
    assert_eq!(
        value["metadata"]["consumer"]["selector"]["serviceOccurrence"],
        1
    );
    let again = ok(&host.continuation_recording_prepare("ordinary", 100, 65536));
    assert_eq!(first, again);
    let token = first["token"].as_str().unwrap();
    ok(&host.continuation_recording_acknowledge("ordinary", token));
    host.ingest(RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.into(),
            service_uuid: HR_SERVICE.into(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.into(),
            characteristic_occurrence: 0,
        },
        epoch,
        value: vec![4, 5, 6],
    });
    ok(&call(
        &session,
        "gatt.unsubscribe",
        &json!({"peerId":POLAR,"selector":selector(),
        "consumer":"recorded", "operationId":"recorded-unsubscribe"})
        .to_string(),
    )
    .await);
    let tail = ok(&host.continuation_recording_prepare("ordinary", 100, 65536));
    assert!(
        tail["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["record"]["valueB64"] == "BAUG"),
        "accepted final notification must be journaled before removal: {tail}"
    );
    ok(&call(&session, "session.dispose", "{}").await);
    host.shutdown().await;
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_queued_during_disable_is_recorded_before_release() {
    use ubm_mobile::{RadioCompletion, RequestKind};
    let directory =
        std::env::temp_dir().join(format!("ubm-recorded-disable-{}", std::process::id()));
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
        "recording":{"id":"disable", "maxBytes":1048576, "maxRecords":1000}})
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
    radio.answer(id, RadioCompletion::Unit);
    ok(
        &tokio::time::timeout(std::time::Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap(),
    );
    let batch = ok(&host.continuation_recording_prepare("disable", 100, 65536));
    let entry = batch["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["record"]["valueB64"] == "BwgJ")
        .unwrap();
    assert_eq!(entry["record"]["receipt"]["origin"], "native");
    ok(&call(&session, "session.dispose", "{}").await);
    host.shutdown().await;
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_recording_reopens_after_process_exit_before_ack() {
    const KEY: &str = "UBM_ORDINARY_RESTART_DIRECTORY";
    if let Some(path) = std::env::var_os(KEY) {
        let directory = std::path::PathBuf::from(path);
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        host.continuation()
            .configure_recording_directory(&directory)
            .unwrap();
        let session = host.open_session("restart-producer").unwrap();
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
            "recording":{"id":"restart", "maxBytes":1048576, "maxRecords":1000}})
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
        host.ingest(RadioIngress::Notification {
            instance: Instance {
                peer_id: POLAR.into(),
                service_uuid: HR_SERVICE.into(),
                service_occurrence: 0,
                characteristic_uuid: HR_MEASUREMENT.into(),
                characteristic_occurrence: 0,
            },
            epoch,
            value: vec![10, 11, 12],
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while ok(&host.continuation_recording_status("restart"))["records"]
                .as_u64()
                .unwrap()
                < 2
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let batch = ok(&host.continuation_recording_prepare("restart", 100, 65536));
        assert!(
            batch["records"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["record"]["valueB64"] == "CgsM")
        );
        std::fs::write(directory.join("prepared.json"), batch.to_string()).unwrap();
        std::process::exit(0);
    }
    let directory =
        std::env::temp_dir().join(format!("ubm-recorded-restart-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "committed_recording_reopens_after_process_exit_before_ack",
        ])
        .env(KEY, &directory)
        .status()
        .unwrap();
    assert!(result.success());
    let expected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("prepared.json")).unwrap()).unwrap();
    let registry = ubm_desktop::continuation_journal::JournalRegistry::default();
    registry.configure_directory(&directory).unwrap();
    let reopened = ok(&ubm_mobile::continuation::recording_control(
        &registry, None, "prepare", "restart", "", 100, 65536,
    ));
    assert_eq!(reopened, expected);
    ok(&ubm_mobile::continuation::recording_control(
        &registry,
        None,
        "acknowledge",
        "restart",
        expected["token"].as_str().unwrap(),
        0,
        0,
    ));
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recording_id_refuses_other_peer_and_quota_before_cccd() {
    use ubm_mobile::RequestKind;
    const OTHER: &str = "A0:9E:1A:00:00:02";
    let directory = std::env::temp_dir().join(format!("ubm-recorded-owner-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    host.continuation()
        .configure_recording_directory(&directory)
        .unwrap();
    let session = host.open_session("recording-owners").unwrap();
    for (peer, suffix) in [(POLAR, "a"), (OTHER, "b")] {
        for op in ["connection.connect", "gatt.discover"] {
            ok(&call(
                &session,
                op,
                &json!({"peerId":peer,"lease":suffix,"operationId":format!("{op}-{suffix}")})
                    .to_string(),
            )
            .await);
        }
    }
    let subscribe = |peer, consumer, max_records| {
        json!({"peerId":peer,"selector":selector(),
        "consumer":consumer,"operationId":format!("subscribe-{consumer}"),
        "recording":{"id":"owners","maxBytes":1048576,"maxRecords":max_records}})
        .to_string()
    };
    ok(&call(
        &session,
        "gatt.subscribe-recorded",
        &subscribe(POLAR, "first", 100),
    )
    .await);
    let before = radio.count(RequestKind::EnableNotifications);
    let (other, _) = failure(
        &call(
            &session,
            "gatt.subscribe-recorded",
            &subscribe(OTHER, "other", 100),
        )
        .await,
    );
    assert_eq!(other["platform"]["code"], "storage.identity");
    let (quota, _) = failure(
        &call(
            &session,
            "gatt.subscribe-recorded",
            &subscribe(POLAR, "quota", 101),
        )
        .await,
    );
    assert_eq!(quota["platform"]["code"], "storage.identity");
    assert_eq!(radio.count(RequestKind::EnableNotifications), before);
    ok(&call(&session, "session.dispose", "{}").await);
    host.shutdown().await;
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_refusal_is_explicit_and_never_becomes_an_ordinary_value() {
    let directory = std::env::temp_dir().join(format!("ubm-recorded-quota-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    host.continuation()
        .configure_recording_directory(&directory)
        .unwrap();
    let session = host.open_session("quota-test").unwrap();
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
        "recording":{"id":"quota", "maxBytes":1048576, "maxRecords":1}})
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
    host.ingest(RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.into(),
            service_uuid: HR_SERVICE.into(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.into(),
            characteristic_occurrence: 0,
        },
        epoch,
        value: vec![11],
    });
    let records = drain_until(&session, |records| {
        records
            .iter()
            .any(|row| row["t"] == "stream-end" && row["consumer"] == "recorded")
    })
    .await;
    assert!(
        !records
            .iter()
            .any(|row| row["t"] == "value" && row["consumer"] == "recorded")
    );
    let status = ok(&host.continuation_recording_status("quota"));
    assert_eq!(status["accepting"], false);
    assert!(status["lostRecords"].as_u64().unwrap() >= 1);
    assert!(status["collectionFailure"].is_object());
    ok(&call(&session, "session.dispose", "{}").await);
    host.shutdown().await;
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_peers_record_independently_in_one_session() {
    const OTHER: &str = "A0:9E:1A:00:00:02";
    let directory = std::env::temp_dir().join(format!("ubm-recorded-peers-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    host.continuation()
        .configure_recording_directory(&directory)
        .unwrap();
    let session = host.open_session("recording-peers").unwrap();
    for (peer, id) in [(POLAR, "a"), (OTHER, "b")] {
        for op in ["connection.connect", "gatt.discover"] {
            ok(&call(
                &session,
                op,
                &json!({"peerId":peer,"lease":id,"operationId":format!("{op}-{id}")}).to_string(),
            )
            .await);
        }
        ok(&call(
            &session,
            "gatt.subscribe-recorded",
            &json!({"peerId":peer,"selector":selector(),
            "consumer":id,"operationId":format!("subscribe-{id}"),
            "recording":{"id":id,"maxBytes":1048576,"maxRecords":100}})
            .to_string(),
        )
        .await);
    }
    let epochs: Vec<_> = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| match request {
            RadioRequest::EnableNotifications {
                instance, epoch, ..
            } => Some((instance.peer_id.clone(), *epoch)),
            _ => None,
        })
        .collect();
    for (peer, value) in [(POLAR, 42), (OTHER, 43)] {
        let epoch = epochs.iter().find(|(id, _)| id == peer).unwrap().1;
        host.ingest(RadioIngress::Notification {
            instance: Instance {
                peer_id: peer.into(),
                service_uuid: HR_SERVICE.into(),
                service_occurrence: 0,
                characteristic_uuid: HR_MEASUREMENT.into(),
                characteristic_occurrence: 0,
            },
            epoch,
            value: vec![value],
        });
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while ["a", "b"].iter().any(|id| {
            ok(&host.continuation_recording_status(id))["records"]
                .as_u64()
                .unwrap()
                < 2
        }) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    for (id, peer, payload) in [("a", POLAR, "Kg=="), ("b", OTHER, "Kw==")] {
        let batch = ok(&host.continuation_recording_prepare(id, 100, 65536));
        let value = batch["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["record"]["t"] == "value")
            .unwrap();
        assert_eq!(value["metadata"]["session"]["peerId"], peer);
        assert_eq!(value["record"]["valueB64"], payload);
    }
    ok(&call(&session, "session.dispose", "{}").await);
    host.shutdown().await;
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_cccd_cutoff_excludes_later_ingress_but_keeps_ordinary_delivery() {
    use ubm_mobile::{RadioCompletion, ReadProvenance, RequestKind};
    let directory =
        std::env::temp_dir().join(format!("ubm-recorded-cutoff-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::Read { .. } => Reply::Hold,
        request => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    host.continuation()
        .configure_recording_directory(&directory)
        .unwrap();
    let session = host.open_session("cutoff-test").unwrap();
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
        "recording":{"id":"cutoff", "maxBytes":1048576, "maxRecords":1000}})
        .to_string(),
    )
    .await);
    ok(&call(
        &session,
        "gatt.subscribe",
        &json!({"peerId":POLAR,"selector":selector(),
        "consumer":"ordinary", "operationId":"ordinary-subscribe"})
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
    let notification = |value| RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.into(),
            service_uuid: HR_SERVICE.into(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.into(),
            characteristic_occurrence: 0,
        },
        epoch,
        value: vec![value],
    };
    host.ingest(notification(1));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while ok(&host.continuation_recording_status("cutoff"))["records"]
            .as_u64()
            .unwrap()
            < 2
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let read = tokio::spawn({
        let session = session.clone();
        async move {
            call(
                &session,
                "gatt.read",
                &json!({"peerId":POLAR,"selector":selector(),"operationId":"held-read"})
                    .to_string(),
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
            "consumer":"recorded", "operationId":"remove-recorded"})
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
    host.ingest(notification(2));
    let dropped = host.ingest(RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.into(),
            service_uuid: "invalid".into(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.into(),
            characteristic_occurrence: 0,
        },
        epoch,
        value: vec![3],
    });
    assert!(matches!(dropped, ubm_mobile::IngressStatus::Dropped(_)));
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
    let observed = drain_until(&session, |records| {
        records.iter().any(|row| {
            row["t"] == "value" && row["consumer"] == "ordinary" && row["valueB64"] == "Ag=="
        })
    })
    .await;
    assert!(
        !observed
            .iter()
            .any(|row| row["t"] == "value" && row["consumer"] == "recorded")
    );
    let batch = ok(&host.continuation_recording_prepare("cutoff", 100, 65536));
    assert!(
        batch["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["record"]["t"] == "ingress-drop"
                && row["record"]["class"] == "notification"
                && row["record"]["count"] == 1
                && row["metadata"]["consumer"].is_null())
    );
    let values: Vec<_> = batch["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["record"]["t"] == "value")
        .map(|row| row["record"]["valueB64"].as_str().unwrap())
        .collect();
    assert_eq!(values, vec!["AQ=="]);
    ok(&call(&session, "session.dispose", "{}").await);
    host.shutdown().await;
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_physical_cleanup_retains_owner_and_can_retry() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use ubm_mobile::{FailureKind, PlatformFailure, RadioCompletion};
    let directory = std::env::temp_dir().join(format!("ubm-recorded-retry-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let first = AtomicBool::new(true);
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::DisableNotifications { .. } if first.swap(false, Ordering::SeqCst) => {
            Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
                FailureKind::Platform,
                "disable refused",
            )))
        }
        request => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    host.continuation()
        .configure_recording_directory(&directory)
        .unwrap();
    let session = host.open_session("retry-test").unwrap();
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
        "recording":{"id":"retry", "maxBytes":1048576, "maxRecords":1000}})
        .to_string(),
    )
    .await);
    let remove = |id| {
        json!({"peerId":POLAR,"selector":selector(),"consumer":"recorded","operationId":id})
            .to_string()
    };
    let first = call(&session, "gatt.unsubscribe", &remove("remove-1")).await;
    assert!(!parse(&first)["ok"].as_bool().unwrap());
    #[cfg(debug_assertions)]
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while host.recorded_ingress_paused() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    ok(&tokio::time::timeout(
        std::time::Duration::from_secs(3),
        call(&session, "gatt.unsubscribe", &remove("remove-2")),
    )
    .await
    .unwrap());
    let retained = ok(&host.continuation_recording_prepare("retry", 100, 65536));
    assert!(
        retained["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["record"]["t"] == "consumer-registration")
    );
    ok(&call(&session, "session.dispose", "{}").await);
    host.shutdown().await;
    std::fs::remove_dir_all(directory).unwrap();
}
