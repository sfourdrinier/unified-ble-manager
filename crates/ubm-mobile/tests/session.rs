//! Mobile owner behaviour over the scripted platform radio.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use ubm_mobile::{
    Advertisement, FailureKind, IngressClass, IngressStatus, Instance, ManufacturerData,
    MobilePlatform, PlatformFailure, RadioCompletion, RadioIngress, RequestKind, RestoredPeer,
};

fn polar_advertisement() -> RadioIngress {
    RadioIngress::Advertisement(Advertisement {
        peer_id: POLAR.to_owned(),
        address: Some(POLAR.to_owned()),
        local_name: Some("Polar H10 1234".to_owned()),
        rssi: Some(-58),
        tx_power_level: None,
        service_uuids: vec!["180D".to_owned()],
        manufacturer_data: vec![ManufacturerData {
            company_id: 0x006b,
            payload: vec![0x00, 0x80, 0xff],
        }],
        service_data: Vec::new(),
        ..Advertisement::default()
    })
}

fn hr_value(bytes: &[u8], epoch: u64) -> RadioIngress {
    RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.to_owned(),
            service_uuid: HR_SERVICE.to_owned(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.to_owned(),
            characteristic_occurrence: 0,
        },
        epoch,
        value: bytes.to_vec(),
    }
}

fn enable_epoch(radio: &Scripted) -> u64 {
    radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|request| match request {
            ubm_mobile::RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .expect("an enable was issued")
}

async fn connect(session: &ubm_mobile::MobileSession, op: &str) -> Value {
    ok(&call(
        session,
        "connection.connect",
        &json!({"peerId": POLAR, "lease": "lease-1", "operationId": op}).to_string(),
    )
    .await)
}

async fn counters(session: &ubm_mobile::MobileSession) -> Value {
    ok(&call(session, "counters.describe", "{}").await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polar_h10_script_runs_end_to_end() {
    let radio = Scripted::polar();
    let (host, _wakes) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn-manager").expect("session");
    let baseline = counters(&session).await;

    let scan = ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids": ["180D"], "duplicatePolicy": "all", "operationId": "scan-1"})
            .to_string(),
    )
    .await);
    let membership = scan["operationId"]
        .as_str()
        .expect("membership id")
        .to_owned();
    assert_eq!(radio.count(RequestKind::StartScan), 1);
    assert_eq!(host.ingest(polar_advertisement()), IngressStatus::Accepted);
    let records = drain_until(&session, |r| !of_type(r, "adv").is_empty()).await;
    let adv = of_type(&records, "adv")[0];
    assert_eq!(adv["peerId"], POLAR);
    assert_eq!(adv["serviceUuids"], json!([HR_SERVICE]));
    assert_eq!(
        adv["manufacturerData"],
        json!([{"companyId": 0x006b, "payloadB64": "AID/"}])
    );
    assert_eq!(adv["serviceData"], Value::Null);

    let connected = connect(&session, "connect-1").await;
    let generation = connected["connectionGeneration"]
        .as_str()
        .unwrap()
        .to_owned();
    let tree = ok(&call(
        &session,
        "gatt.discover",
        &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "discover-1"}).to_string(),
    )
    .await);
    assert_eq!(tree["connectionGeneration"], generation.as_str());
    assert_eq!(tree["services"][0]["uuid"], HR_SERVICE);
    assert_eq!(
        tree["services"][0]["characteristics"][0]["uuid"],
        HR_MEASUREMENT
    );
    assert_eq!(
        tree["services"][0]["characteristics"][0]["properties"],
        0x08
    );
    assert_eq!(
        tree["services"][0]["characteristics"][0]["descriptors"],
        json!([{"uuid": CCCD, "occurrence": 0}])
    );

    let subscribed = ok(&call(
        &session,
        "gatt.subscribe",
        &json!({"peerId": POLAR, "selector": selector(), "consumer": "hr",
                    "deliveryMode": "require-notification", "operationId": "sub-1"})
        .to_string(),
    )
    .await);
    assert_eq!(
        subscribed,
        json!({"consumer": "hr", "delivery": "notification"})
    );
    let epoch = enable_epoch(&radio);
    host.ingest(hr_value(&[0x00, 0x55], epoch));
    host.ingest(hr_value(&[0x10, 0x55, 0x20, 0x03], epoch));
    let records = drain_until(&session, |r| of_type(r, "value").len() == 2).await;
    let values: Vec<&str> = of_type(&records, "value")
        .iter()
        .map(|r| r["valueB64"].as_str().unwrap())
        .collect();
    assert_eq!(values, ["AFU=", "EFUgAw=="]);
    assert!(
        of_type(&records, "value")
            .iter()
            .all(|r| r["delivery"] == "notification")
    );

    // Idle link loss reported by the OS.
    host.ingest(RadioIngress::Connection {
        peer_id: POLAR.to_owned(),
        connected: false,
        status: Some(8),
    });
    let records = drain_until(&session, |r| {
        !of_type(r, "link").is_empty() && !of_type(r, "stream-end").is_empty()
    })
    .await;
    let link = of_type(&records, "link")[0];
    assert_eq!(link["reason"], "peer");
    assert_eq!(link["connectionGeneration"], generation.as_str());
    let end = of_type(&records, "stream-end")[0];
    assert_eq!(end["consumer"], "hr");
    assert_eq!(end["reason"], "invalidated");
    let ordinals: Vec<u64> = records
        .iter()
        .map(|r| r["ordinal"].as_u64().unwrap())
        .collect();
    assert!(
        ordinals.windows(2).all(|w| w[0] < w[1]),
        "ordinals increase"
    );

    ok(&call(&session,
            "gatt.unsubscribe",
            &json!({"peerId": POLAR, "selector": selector(), "consumer": "hr", "operationId": "unsub-1"})
                .to_string(),
        )
        .await);
    ok(&call(
        &session,
        "scan.stop",
        &json!({"operationId": membership}).to_string(),
    )
    .await);
    let disposed = ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(disposed, json!({"state": "released", "failures": []}));

    let after = {
        let probe = host.open_session("probe").expect("probe session");
        let value = counters(&probe).await;
        ok(&call(&probe, "session.dispose", "{}").await);
        value
    };
    assert_eq!(after["counters"]["physicalCccdEnablements"], 0);
    assert_eq!(after["counters"]["subscriptionConsumers"], 0);
    assert_eq!(after["counters"]["connectionLeases"], 0);
    assert_eq!(after["counters"]["scanConsumers"], 0);
    assert_eq!(after["counters"]["activeScanControllers"], 0);
    assert_eq!(
        after["counters"]["queuedOperations"],
        baseline["counters"]["queuedOperations"]
    );
    assert_eq!(after["native"]["pendingRadioRequests"], 0);
    assert_eq!(
        after["native"]["liveOps"], 0,
        "the probe's own counters call does not count itself"
    );
    assert_eq!(after["process"]["counters"]["connectionLeases"], 0);
    assert_eq!(after["process"]["counters"]["subscriptionConsumers"], 0);
    assert_eq!(after["process"]["native"]["pendingRadioRequests"], 0);
}

/// A characteristic read reports the radio's own provenance: CoreBluetooth
/// answering a read on a notifying characteristic says the value may be a
/// notification, and the owner carries that verbatim to the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_reports_the_radio_provenance_on_the_wire() {
    for (provenance, wire) in [
        (ubm_mobile::ReadProvenance::ReadResponse, "read-response"),
        (
            ubm_mobile::ReadProvenance::ReadOrNotification,
            "read-or-notification",
        ),
    ] {
        let radio = Scripted::new(Box::new(move |request| match request {
            ubm_mobile::RadioRequest::Read { .. } => Reply::Now(RadioCompletion::Read {
                value: vec![0x0f],
                provenance,
            }),
            ubm_mobile::RadioRequest::Discover { .. } => {
                let mut services = polar_services();
                services[0].characteristics[0].properties.read = true;
                Reply::Now(RadioCompletion::Discovered(services))
            }
            other => polar_responder(other),
        }));
        let (_host, _) = open(&radio, MobilePlatform::Apple).await;
        let session = _host.open_session("rn").unwrap();
        connect(&session, "c").await;
        ok(&call(
            &session,
            "gatt.discover",
            &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "d"}).to_string(),
        )
        .await);
        let reply = call(
            &session,
            "gatt.read",
            &json!({"peerId": POLAR, "selector": selector(), "operationId": "r"}).to_string(),
        )
        .await;
        assert_eq!(ok(&reply)["valueB64"], "Dw==");
        assert_eq!(ok(&reply)["provenance"], wire);
    }
}

/// A radio that answers a characteristic read without saying what the value
/// is (a bare `Bytes`) is refused as the wrong answer shape: the owner never
/// assumes a provenance the platform did not report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_answered_without_provenance_is_refused() {
    assert!(!RadioCompletion::Bytes(vec![1]).answers(RequestKind::Read));
    assert!(
        RadioCompletion::Read {
            value: vec![1],
            provenance: ubm_mobile::ReadProvenance::ReadResponse,
        }
        .answers(RequestKind::Read)
    );
    assert!(RadioCompletion::Bytes(vec![1]).answers(RequestKind::ReadDescriptor));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_targets_exactly_one_operation() {
    let radio = Scripted::new(Box::new(|request| match request {
        ubm_mobile::RadioRequest::Read { .. } => Reply::Hold,
        ubm_mobile::RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            services[0].characteristics[0].properties.read = true;
            Reply::Now(RadioCompletion::Discovered(services))
        }
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    connect(&session, "c").await;
    ok(&call(
        &session,
        "gatt.discover",
        &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "d"}).to_string(),
    )
    .await);
    let read =
        |id: &str| json!({"peerId": POLAR, "selector": selector(), "operationId": id}).to_string();
    let first = tokio::spawn({
        let session = session.clone();
        let args = read("read-a");
        async move { call(&session, "gatt.read", &args).await }
    });
    // read-a reaches the radio before read-b exists, so the cancelled op is
    // the one in flight (spawned tasks otherwise start in any order).
    wait_for(|| !radio.held_of(RequestKind::Read).is_empty()).await;
    let second = tokio::spawn({
        let session = session.clone();
        let args = read("read-b");
        async move { call(&session, "gatt.read", &args).await }
    });
    // The core may queue the second read behind the first (one GATT op in
    // flight per link); either way exactly the cancelled op is cancelled.
    let first_held = radio.held_of(RequestKind::Read)[0];
    let first_is_a = matches!(
        radio.held.lock().unwrap().get(&first_held),
        Some(ubm_mobile::RadioRequest::Read { .. })
    );
    assert!(first_is_a);
    let cancelled = ok(&call(
        &session,
        "op.cancel",
        &json!({"operationId": "read-a"}).to_string(),
    )
    .await);
    assert_eq!(cancelled["state"], "cancellation-requested");
    let (error, _) = failure(&first.await.unwrap());
    assert_eq!(error["code"], "operation.aborted");
    let cancels = radio.cancels.lock().unwrap().clone();
    assert_eq!(cancels.len(), 1, "exactly one platform cancel");
    wait_for(|| {
        radio
            .held_of(RequestKind::Read)
            .iter()
            .any(|id| !cancels.contains(id))
    })
    .await;
    let other = *radio
        .held_of(RequestKind::Read)
        .iter()
        .find(|id| !cancels.contains(id))
        .unwrap();
    radio.answer(
        other,
        RadioCompletion::Read {
            value: vec![7],
            provenance: ubm_mobile::ReadProvenance::ReadResponse,
        },
    );
    let second = second.await.unwrap();
    assert_eq!(ok(&second)["valueB64"], "Bw==");
    assert_eq!(ok(&second)["provenance"], "read-response");
    // A late answer for the cancelled request is counted, not dropped.
    assert_eq!(
        radio.answer(
            cancels[0],
            RadioCompletion::Read {
                value: vec![1],
                provenance: ubm_mobile::ReadProvenance::ReadResponse,
            }
        ),
        ubm_mobile::CompletionStatus::Late
    );
    assert_eq!(host.radio_counters().late_completions, 1);
    let again = ok(&call(
        &session,
        "op.cancel",
        &json!({"operationId": "read-b"}).to_string(),
    )
    .await);
    assert_eq!(again["state"], "already-terminal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_before_admission_has_zero_effects() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    let ack = ok(&call(
        &session,
        "op.cancel",
        &json!({"operationId": "early"}).to_string(),
    )
    .await);
    assert_eq!(ack["state"], "cancellation-requested");
    let before = radio.requests.lock().unwrap().len();
    let (error, commit) = failure(
        &call(
            &session,
            "connection.connect",
            &json!({"peerId": POLAR, "lease": "l", "operationId": "early"}).to_string(),
        )
        .await,
    );
    assert_eq!(error["code"], "operation.aborted");
    assert_eq!(commit, Value::Null);
    assert_eq!(
        radio.requests.lock().unwrap().len(),
        before,
        "no radio request"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_receipts_report_what_the_platform_did() {
    let radio = Scripted::new(Box::new(|request| match request {
        ubm_mobile::RadioRequest::Write {
            with_response: true,
            value,
            ..
        } if value == &[9] => Reply::Hold,
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    connect(&session, "c").await;
    ok(&call(
        &session,
        "gatt.discover",
        &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "d"}).to_string(),
    )
    .await);
    let write = |value: &str, mode: &str, id: &str| {
        json!({"peerId": POLAR, "selector": selector(), "valueB64": value, "mode": mode, "operationId": id})
            .to_string()
    };
    // Scripted HR characteristic is notify-only; the core checks write
    // properties, so these writes exercise the admission path honestly.
    let text = call(
        &session,
        "gatt.write",
        &write("AQ==", "without-response", "w1"),
    )
    .await;
    let envelope = parse(&text);
    if envelope["ok"] == true {
        assert_eq!(envelope["value"]["commitState"], "unknown");
    } else {
        assert_eq!(
            envelope["commit"], "not-dispatched",
            "refused before the radio: {text}"
        );
    }
    // Malformed base64 never reaches the radio.
    let before = radio.count(RequestKind::Write);
    let (error, commit) =
        failure(&call(&session, "gatt.write", &write("AQ=", "with-response", "w2")).await);
    assert_eq!(error["code"], "argument.invalid");
    assert_eq!(commit, "not-dispatched");
    assert_eq!(radio.count(RequestKind::Write), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_commit_is_uncertain_after_dispatch_and_confirmed_on_response() {
    let radio = Scripted::new(Box::new(|request| match request {
        ubm_mobile::RadioRequest::Write { value, .. } if value == &[9] => Reply::Hold,
        ubm_mobile::RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            services[0].characteristics[0].properties.write = true;
            services[0].characteristics[0]
                .properties
                .write_without_response = true;
            Reply::Now(RadioCompletion::Discovered(services))
        }
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    connect(&session, "c").await;
    ok(&call(
        &session,
        "gatt.discover",
        &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "d"}).to_string(),
    )
    .await);
    let write = |value: &str, mode: &str, id: &str| {
        json!({"peerId": POLAR, "selector": selector(), "valueB64": value, "mode": mode, "operationId": id})
            .to_string()
    };
    let confirmed = ok(&call(
        &session,
        "gatt.write",
        &write("AQ==", "with-response", "w1"),
    )
    .await);
    assert_eq!(confirmed["commitState"], "confirmed");
    let unknown = ok(&call(
        &session,
        "gatt.write",
        &write("AQ==", "without-response", "w2"),
    )
    .await);
    assert_eq!(unknown["commitState"], "unknown");

    let pending = tokio::spawn({
        let session = session.clone();
        let args = write("CQ==", "with-response", "w3");
        async move { call(&session, "gatt.write", &args).await }
    });
    wait_for(|| !radio.held_of(RequestKind::Write).is_empty()).await;
    ok(&call(
        &session,
        "op.cancel",
        &json!({"operationId": "w3"}).to_string(),
    )
    .await);
    let (error, commit) = failure(&pending.await.unwrap());
    assert_eq!(error["code"], "operation.aborted");
    assert_eq!(commit, "uncertain", "the write reached the platform");

    // A platform GATT failure after dispatch is uncertain too.
    radio.set_responder(Box::new(|request| match request {
        ubm_mobile::RadioRequest::Write { .. } => {
            Reply::Now(RadioCompletion::Failed(PlatformFailure {
                gatt_status: Some(133),
                ..PlatformFailure::new(FailureKind::GattStatus, "GATT_ERROR")
            }))
        }
        other => polar_responder(other),
    }));
    let (error, commit) = failure(
        &call(
            &session,
            "gatt.write",
            &write("AQ==", "with-response", "w4"),
        )
        .await,
    );
    assert_eq!(error["code"], "platform.failure", "legacy identity (113)");
    assert_eq!(commit, "uncertain");
    assert_eq!(error["platform"]["metadata"]["androidGattStatus"], 133);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispose_reports_release_failures_and_retries() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    connect(&session, "c").await;
    radio.set_responder(Box::new(|request| match request {
        ubm_mobile::RadioRequest::Disconnect { .. } => Reply::Now(RadioCompletion::Failed(
            PlatformFailure::new(FailureKind::Platform, "stack refused disconnect"),
        )),
        other => polar_responder(other),
    }));
    let disposed = ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(disposed["state"], "release-failed");
    assert_eq!(disposed["failures"][0]["resourceKind"], "connection");
    radio.set_responder(Box::new(polar_responder));
    let retried = ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(retried["state"], "released");
    assert!(
        host.session(session.id()).is_none(),
        "released session is gone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_fires_once_per_armed_period() {
    let radio = Scripted::polar();
    let (host, wakes) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "s"}).to_string(),
    )
    .await);
    for _ in 0..20 {
        host.ingest(polar_advertisement());
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        wakes.count.load(Ordering::SeqCst),
        1,
        "20 records, one wake"
    );
    let records = drain_until(&session, |r| of_type(r, "adv").len() == 20).await;
    assert_eq!(records.len(), 20);
    let empty = parse(&session.drain(256, 65536));
    assert_eq!(empty, json!({"more": false, "records": []}));
    host.ingest(polar_advertisement());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        wakes.count.load(Ordering::SeqCst),
        2,
        "re-armed after the empty drain"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_session_costs_nothing() {
    let radio = Scripted::polar();
    let (host, wakes) = open(&radio, MobilePlatform::Android).await;
    let _session = host.open_session("rn").unwrap();
    let before = radio.requests.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        radio.requests.lock().unwrap().len(),
        before,
        "no polling requests"
    );
    assert_eq!(wakes.count.load(Ordering::SeqCst), 0, "no wakes while idle");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingress_overflow_is_counted_and_surfaced() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    let dropped = host.ingest(RadioIngress::Dropped {
        class: IngressClass::Advertisement,
        detail: "manufacturer section shorter than a company id".to_owned(),
    });
    assert_eq!(dropped, IngressStatus::Dropped(IngressClass::Advertisement));
    // Malformed advertisement (RSSI outside i8) is refused and counted.
    let mut bad = match polar_advertisement() {
        RadioIngress::Advertisement(adv) => adv,
        _ => unreachable!(),
    };
    bad.rssi = Some(300);
    assert_eq!(
        host.ingest(RadioIngress::Advertisement(bad)),
        IngressStatus::Dropped(IngressClass::Advertisement)
    );
    let records = drain_until(&session, |r| {
        of_type(r, "ingress-drop")
            .iter()
            .map(|d| d["count"].as_u64().unwrap())
            .sum::<u64>()
            == 2
    })
    .await;
    assert_eq!(
        of_type(&records, "ingress-drop")[0]["class"],
        "advertisement"
    );
    let counters = counters(&session).await;
    assert_eq!(
        counters["process"]["native"]["ingressDrops"]["advertisement"],
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_and_malformed_args_fail_before_effects() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    let before = radio.requests.lock().unwrap().len();
    let huge = "A".repeat(ubm_mobile::wire::MAX_BASE64_LENGTH + 4);
    let (error, commit) = failure(
        &call(
            &session,
            "gatt.write",
            &json!({"peerId": POLAR, "selector": selector(), "valueB64": huge,
                        "mode": "with-response", "operationId": "big"})
            .to_string(),
        )
        .await,
    );
    assert_eq!(error["code"], "bytes.too-large");
    assert_eq!(commit, "not-dispatched");
    let (error, _) = failure(&call(&session, "gatt.read", "{\"peerId\":1}").await);
    assert_eq!(error["code"], "argument.invalid");
    let (error, _) = failure(
        &call(
            &session,
            "gatt.read",
            &json!({"peerId": POLAR, "operationId": "r",
                        "selector": {"serviceUuid": "180D", "serviceOccurrence": 1.5,
                                     "characteristicUuid": "2A37", "characteristicOccurrence": 0}})
            .to_string(),
        )
        .await,
    );
    assert_eq!(
        error["code"], "argument.invalid",
        "fractional occurrence refused"
    );
    let (error, _) = failure(&call(&session, "no.such-op", "{}").await);
    assert_eq!(error["code"], "argument.invalid");
    assert_eq!(radio.requests.lock().unwrap().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sessions_share_one_scan_and_one_radio() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let a = host.open_session("manager-a").unwrap();
    let b = host.open_session("manager-b").unwrap();
    ok(&call(
        &a,
        "scan.start",
        &json!({"serviceUuids": ["180D"], "duplicatePolicy": "all", "operationId": "s"})
            .to_string(),
    )
    .await);
    ok(&call(
        &b,
        "scan.start",
        &json!({"serviceUuids": ["180D"], "duplicatePolicy": "all", "operationId": "s"})
            .to_string(),
    )
    .await);
    assert_eq!(
        radio.count(RequestKind::StartScan),
        1,
        "the second manager joins"
    );
    host.ingest(polar_advertisement());
    drain_until(&a, |r| !of_type(r, "adv").is_empty()).await;
    drain_until(&b, |r| !of_type(r, "adv").is_empty()).await;
    // Widening to a broad scan restarts the physical scan once.
    let c = host.open_session("manager-c").unwrap();
    ok(&call(
        &c,
        "scan.start",
        &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "s"}).to_string(),
    )
    .await);
    assert_eq!(radio.count(RequestKind::StartScan), 2);
    assert_eq!(radio.count(RequestKind::StopScan), 1);
    for session in [&a, &b, &c] {
        ok(&call(session, "session.dispose", "{}").await);
    }
    assert_eq!(
        radio.count(RequestKind::StopScan),
        2,
        "last member stops the scan"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restored_peers_are_listed_and_adopted_by_connect() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    host.ingest(RadioIngress::Restored {
        peers: vec![RestoredPeer {
            peer_id: "5B7C1A2E-0000-4000-8000-000000000001".to_owned(),
            name: Some("Polar H10".to_owned()),
            connected: true,
        }],
    });
    let session = host.open_session("rn").unwrap();
    let restored = ok(&call(&session, "peers.restored", "{}").await);
    assert_eq!(restored[0]["source"], "restored");
    assert_eq!(restored[0]["name"], "Polar H10");
    let adopted = ok(&call(&session,
            "connection.connect",
            &json!({"peerId": "5B7C1A2E-0000-4000-8000-000000000001", "lease": "l", "operationId": "c"})
                .to_string(),
        )
        .await);
    assert!(adopted["connectionGeneration"].is_string());
    let connected = ok(&call(&session, "peers.connected", "{}").await);
    assert_eq!(connected.as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apple_refuses_android_only_controls_before_effects() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let session = host.open_session("rn").unwrap();
    let before = radio.requests.lock().unwrap().len();
    for (op, args) in [
        (
            "connection.request-mtu",
            json!({"peerId": POLAR, "lease": "l", "mtu": 247, "operationId": "m"}),
        ),
        (
            "connection.request-priority",
            json!({"peerId": POLAR, "lease": "l", "priority": "balanced", "operationId": "p"}),
        ),
        (
            "connection.connect",
            json!({"peerId": POLAR, "lease": "l", "intent": "when-available", "operationId": "c"}),
        ),
        (
            "scan.start",
            json!({"serviceUuids": [], "duplicatePolicy": "all", "platform": {"mode": "balanced"}, "operationId": "s"}),
        ),
    ] {
        let (error, _) = failure(&call(&session, op, &args.to_string()).await);
        assert_eq!(error["code"], "capability.unsupported", "{op}");
    }
    assert_eq!(radio.requests.lock().unwrap().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn android_link_controls_reach_the_radio() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    connect(&session, "c").await;
    let rssi = ok(&call(
        &session,
        "connection.rssi",
        &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "r"}).to_string(),
    )
    .await);
    assert_eq!(rssi["rssi"], -61);
    let mtu = ok(&call(
        &session,
        "connection.request-mtu",
        &json!({"peerId": POLAR, "lease": "lease-1", "mtu": 247, "operationId": "m"}).to_string(),
    )
    .await);
    assert_eq!(mtu["mtu"], 247);
    let (error, _) = failure(
        &call(
            &session,
            "connection.request-mtu",
            &json!({"peerId": POLAR, "lease": "other", "mtu": 247, "operationId": "m2"})
                .to_string(),
        )
        .await,
    );
    assert_eq!(error["code"], "ownership.denied");
}

#[tokio::test(start_paused = true)]
async fn shutdown_reports_a_platform_that_never_answers_close() {
    let radio = Scripted::new(Box::new(|request| match request {
        ubm_mobile::RadioRequest::Close { .. } => Reply::Hold,
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let record = parse(&host.shutdown().await);
    assert_eq!(record["state"], "release-failed", "{record}");
    assert_eq!(record["failures"][0]["resourceKind"], "radio");
    assert_eq!(record["failures"][0]["code"], "operation.timed-out");
    assert_eq!(host.ingest(polar_advertisement()), IngressStatus::Closed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_the_radio_and_releases() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    connect(&session, "c").await;
    let record = parse(&host.shutdown().await);
    assert_eq!(record, json!({"state": "released", "failures": []}));
    assert_eq!(radio.count(RequestKind::Close), 1);
    assert_eq!(
        radio.count(RequestKind::Disconnect),
        1,
        "the session's lease was released"
    );
    assert!(
        host.open_session("late").is_err(),
        "no session after shutdown"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_connect_leaves_no_stale_intent() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    ok(&call(
        &session,
        "op.cancel",
        &json!({"operationId": "early"}).to_string(),
    )
    .await);
    // Refused before admission: the stage must not leak into the next connect.
    let refused = call(
        &session,
        "connection.connect",
        &json!({"peerId": POLAR, "lease": "l", "intent": "when-available", "operationId": "early"})
            .to_string(),
    )
    .await;
    failure(&refused);
    connect(&session, "direct").await;
    let auto = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|request| match request {
            ubm_mobile::RadioRequest::Connect { auto_connect, .. } => Some(*auto_connect),
            _ => None,
        });
    assert_eq!(auto, Some(false), "the direct connect is not autoConnect");
}

async fn wait_for(condition: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not met in 3 s"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn responder_with(
    notify: bool,
    indicate: bool,
) -> Box<dyn FnMut(&ubm_mobile::RadioRequest) -> Reply + Send> {
    Box::new(move |request| match request {
        ubm_mobile::RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            let properties = &mut services[0].characteristics[0].properties;
            properties.notify = notify;
            properties.indicate = indicate;
            Reply::Now(RadioCompletion::Discovered(services))
        }
        ubm_mobile::RadioRequest::EnableNotifications { .. } => Reply::Now(
            RadioCompletion::NotifyEnabled(ubm_desktop::ObservedDelivery::Unknown),
        ),
        other => polar_responder(other),
    })
}

async fn discovered_session(
    radio: &std::sync::Arc<Scripted>,
    platform: MobilePlatform,
) -> (ubm_mobile::MobileHost, ubm_mobile::MobileSession) {
    let (host, _) = open(radio, platform).await;
    let session = host.open_session("rn").unwrap();
    connect(&session, "c").await;
    ok(&call(
        &session,
        "gatt.discover",
        &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "d"}).to_string(),
    )
    .await);
    (host, session)
}

async fn subscribe_with(session: &ubm_mobile::MobileSession, mode: &str, id: &str) -> String {
    call(
        session,
        "gatt.subscribe",
        &json!({"peerId": POLAR, "selector": selector(), "consumer": id,
                    "deliveryMode": mode, "operationId": id})
        .to_string(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apple_refuses_require_indication_where_corebluetooth_enables_notifications() {
    let radio = Scripted::new(responder_with(true, true));
    let (_host, session) = discovered_session(&radio, MobilePlatform::Apple).await;
    let before = radio.count(RequestKind::EnableNotifications);
    let (error, _) = failure(&subscribe_with(&session, "require-indication", "s1").await);
    assert_eq!(error["code"], "capability.limited");
    assert_eq!(error["operation"], "gatt.subscribe.delivery");
    assert_eq!(
        radio.count(RequestKind::EnableNotifications),
        before,
        "no effect"
    );
    // require-notification on the same characteristic is honoured: CoreBluetooth
    // enables notifications when the characteristic can notify.
    let value = ok(&subscribe_with(&session, "require-notification", "s2").await);
    assert_eq!(value["delivery"], "unknown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apple_allows_require_indication_on_an_indicate_only_characteristic() {
    let radio = Scripted::new(responder_with(false, true));
    let (_host, session) = discovered_session(&radio, MobilePlatform::Apple).await;
    let value = ok(&subscribe_with(&session, "require-indication", "s1").await);
    assert_eq!(value["delivery"], "unknown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requirement_the_characteristic_lacks_is_refused_on_every_platform() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::new(responder_with(false, true));
        let (_host, session) = discovered_session(&radio, platform).await;
        let (error, _) = failure(&subscribe_with(&session, "require-notification", "s1").await);
        assert_eq!(error["code"], "gatt.property-not-supported", "{platform:?}");
        assert_eq!(radio.count(RequestKind::EnableNotifications), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn android_forwards_a_hard_requirement_to_the_radio() {
    let radio = Scripted::new(responder_with(true, true));
    let (_host, session) = discovered_session(&radio, MobilePlatform::Android).await;
    radio.set_responder(Box::new(polar_responder));
    let value = ok(&subscribe_with(&session, "require-indication", "s1").await);
    assert_eq!(value["delivery"], "indication");
    let requested = radio.requests.lock().unwrap().iter().find_map(|r| match r {
        ubm_mobile::RadioRequest::EnableNotifications { requested, .. } => Some(*requested),
        _ => None,
    });
    assert_eq!(requested, Some(Some(ubm_desktop::DeliveryMode::Indication)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_the_platform_refused_before_sending_is_not_dispatched() {
    let radio = Scripted::new(Box::new(|request| match request {
        ubm_mobile::RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            services[0].characteristics[0]
                .properties
                .write_without_response = true;
            Reply::Now(RadioCompletion::Discovered(services))
        }
        ubm_mobile::RadioRequest::Write { .. } => Reply::Now(RadioCompletion::Failed(
            PlatformFailure::not_dispatched(FailureKind::Busy, "write-without-response queue full"),
        )),
        other => polar_responder(other),
    }));
    let (_host, session) = discovered_session(&radio, MobilePlatform::Apple).await;
    let (error, commit) = failure(
        &call(
            &session,
            "gatt.write",
            &json!({"peerId": POLAR, "selector": selector(), "valueB64": "AQ==",
                        "mode": "without-response", "operationId": "w"})
            .to_string(),
        )
        .await,
    );
    assert_eq!(error["code"], "platform.failure");
    assert_eq!(error["platform"]["code"], "writeFailed");
    assert_eq!(commit, "not-dispatched", "the platform said it never sent");
    assert_eq!(radio.count(RequestKind::Write), 1);
}

// -- PR210-52: restored peers are adopted once per process -----------------

const RESTORED: &str = "5B7C1A2E-0000-4000-8000-000000000001";

fn restored_ingress(peer_ids: &[&str]) -> RadioIngress {
    RadioIngress::Restored {
        peers: peer_ids
            .iter()
            .map(|peer_id| RestoredPeer {
                peer_id: (*peer_id).to_owned(),
                name: Some("Polar H10".to_owned()),
                connected: true,
            })
            .collect(),
    }
}

async fn claim(session: &ubm_mobile::MobileSession, max_peers: u64) -> String {
    call(
        session,
        "peers.claim-restored",
        &json!({ "maxPeers": max_peers }).to_string(),
    )
    .await
}

fn claimed_ids(value: &Value) -> Vec<String> {
    value["peers"]
        .as_array()
        .expect("peers array")
        .iter()
        .map(|peer| peer["peerId"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restored_peers_are_claimed_once_per_process_across_managers() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    host.ingest(restored_ingress(&[RESTORED]));
    let first = host.open_session("manager-a").unwrap();
    let second = host.open_session("manager-b").unwrap();

    let claimed = ok(&claim(&second, 1023).await);
    assert_eq!(claimed_ids(&claimed), [RESTORED]);
    assert_eq!(claimed["peers"][0]["source"], "restored");
    // The other manager, and a repeat by the claimant, find nothing left.
    assert!(claimed_ids(&ok(&claim(&first, 1023).await)).is_empty());
    assert!(claimed_ids(&ok(&claim(&second, 1023).await)).is_empty());
    // Listing stays a read: the OS fact is still reported.
    let listed = ok(&call(&first, "peers.restored", "{}").await);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    // A claim survives the claimant's disposal: a later manager in the same
    // process still cannot adopt the peer again (legacy consumed it).
    ok(&call(&second, "session.dispose", "{}").await);
    let third = host.open_session("manager-c").unwrap();
    assert!(claimed_ids(&ok(&claim(&third, 1023).await)).is_empty());

    // A peer the OS restores later is claimable once more.
    host.ingest(restored_ingress(&["5B7C1A2E-0000-4000-8000-000000000002"]));
    wait_for_restored(&first, 2).await;
    let late = ok(&claim(&first, 1023).await);
    assert_eq!(claimed_ids(&late), ["5B7C1A2E-0000-4000-8000-000000000002"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_over_capacity_is_refused_without_consuming() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    host.ingest(restored_ingress(&[
        RESTORED,
        "5B7C1A2E-0000-4000-8000-000000000002",
    ]));
    let session = host.open_session("rn").unwrap();
    wait_for_restored(&session, 2).await;
    let (error, _) = failure(&claim(&session, 1).await);
    assert_eq!(error["code"], "bytes.too-large");
    let claimed = ok(&claim(&session, 2).await);
    assert_eq!(
        claimed_ids(&claimed).len(),
        2,
        "the refusal consumed nothing"
    );
}

async fn wait_for_restored(session: &ubm_mobile::MobileSession, count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let listed = ok(&call(session, "peers.restored", "{}").await);
        if listed.as_array().unwrap().len() == count {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "restored peers not listed"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

// -- PR210-53: counters describe the session; process totals are separate ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn counters_are_per_session_and_return_to_baseline_independently() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let a = host.open_session("manager-a").unwrap();
    let b = host.open_session("manager-b").unwrap();
    let baseline = counters(&a).await;
    for key in [
        "connectionLeases",
        "scanConsumers",
        "physicalLinks",
        "queuedOperations",
    ] {
        assert_eq!(baseline["counters"][key], 0, "{key}");
    }
    assert_eq!(
        baseline["native"]["liveOps"], 0,
        "the describing call is not counted"
    );
    assert_eq!(baseline["native"]["pendingRadioRequests"], 0);

    // A holds a link and a subscription; B holds a scan.
    connect(&a, "c").await;
    ok(&call(
        &a,
        "gatt.discover",
        &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "d"}).to_string(),
    )
    .await);
    ok(&call(
        &a,
        "gatt.subscribe",
        &json!({"peerId": POLAR, "selector": selector(), "consumer": "hr", "operationId": "s"})
            .to_string(),
    )
    .await);
    ok(&call(
        &b,
        "scan.start",
        &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "scan"}).to_string(),
    )
    .await);

    let seen_by_a = counters(&a).await;
    assert_eq!(seen_by_a["counters"]["connectionLeases"], 1);
    assert_eq!(seen_by_a["counters"]["physicalLinks"], 1);
    assert_eq!(seen_by_a["counters"]["databaseSnapshots"], 1);
    assert_eq!(seen_by_a["counters"]["subscriptionConsumers"], 1);
    assert_eq!(seen_by_a["counters"]["physicalCccdEnablements"], 1);
    assert_eq!(
        seen_by_a["counters"]["scanConsumers"], 0,
        "B's scan is not A's"
    );
    assert_eq!(seen_by_a["counters"]["activeScanControllers"], 0);
    let seen_by_b = counters(&b).await;
    assert_eq!(seen_by_b["counters"]["scanConsumers"], 1);
    assert_eq!(seen_by_b["counters"]["activeScanControllers"], 1);
    assert_eq!(
        seen_by_b["counters"]["connectionLeases"], 0,
        "A's link is not B's"
    );
    assert_eq!(seen_by_b["counters"]["subscriptionConsumers"], 0);
    // The process totals name the whole owner, explicitly.
    for seen in [&seen_by_a, &seen_by_b] {
        assert_eq!(seen["process"]["counters"]["connectionLeases"], 1);
        assert_eq!(seen["process"]["counters"]["scanConsumers"], 1);
        assert_eq!(seen["process"]["counters"]["subscriptionConsumers"], 1);
    }

    // An operation at the radio counts as dispatched for its own session only.
    radio.set_responder(Box::new(|request| match request {
        ubm_mobile::RadioRequest::ReadRssi { .. } => Reply::Hold,
        other => polar_responder(other),
    }));
    let pending = {
        let a = a.clone();
        tokio::spawn(async move {
            call(
                &a,
                "connection.rssi",
                &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "r"}).to_string(),
            )
            .await
        })
    };
    wait_for(|| !radio.held_of(RequestKind::ReadRssi).is_empty()).await;
    let busy_a = counters(&a).await;
    assert_eq!(busy_a["counters"]["dispatchedOperations"], 1);
    assert_eq!(busy_a["native"]["pendingRadioRequests"], 1);
    assert_eq!(busy_a["native"]["liveOps"], 1);
    let busy_b = counters(&b).await;
    assert_eq!(busy_b["counters"]["dispatchedOperations"], 0);
    assert_eq!(busy_b["native"]["pendingRadioRequests"], 0);
    assert_eq!(busy_b["process"]["native"]["pendingRadioRequests"], 1);
    let id = radio.held_of(RequestKind::ReadRssi)[0];
    radio.answer(id, RadioCompletion::Rssi(-60));
    ok(&pending.await.unwrap());

    // A returns to baseline; B's own resources are untouched.
    ok(&call(&a, "session.dispose", "{}").await);
    let disposed_a = counters(&a).await;
    for key in [
        "connectionLeases",
        "physicalLinks",
        "subscriptionConsumers",
        "physicalCccdEnablements",
    ] {
        assert_eq!(
            disposed_a["counters"][key], 0,
            "a disposed session reports baseline: {key}"
        );
    }
    let (refused, _) = failure(&call(&a, "adapter.state", "{}").await);
    assert_eq!(
        refused["code"], "lifecycle.destroyed",
        "only reads of the counters stay open"
    );
    let after_a = counters(&b).await;
    assert_eq!(after_a["counters"]["scanConsumers"], 1);
    assert_eq!(after_a["process"]["counters"]["connectionLeases"], 0);
    assert_eq!(after_a["process"]["counters"]["subscriptionConsumers"], 0);
    ok(&call(
        &b,
        "scan.stop",
        &json!({"operationId": "s2-scan-1"}).to_string(),
    )
    .await);
    let after_b = counters(&b).await;
    for key in [
        "activeScanControllers",
        "scanConsumers",
        "connectionLeases",
        "physicalLinks",
        "subscriptionConsumers",
        "queuedOperations",
        "dispatchedOperations",
        "restorationRecords",
    ] {
        assert_eq!(after_b["counters"][key], 0, "{key}");
    }
    assert_eq!(after_b["native"]["liveOps"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_counts_as_the_claimants_restoration_records() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    host.ingest(restored_ingress(&[RESTORED]));
    let a = host.open_session("manager-a").unwrap();
    let b = host.open_session("manager-b").unwrap();
    wait_for_restored(&a, 1).await;
    ok(&claim(&b, 1023).await);
    assert_eq!(counters(&b).await["counters"]["restorationRecords"], 1);
    let seen_by_a = counters(&a).await;
    assert_eq!(seen_by_a["counters"]["restorationRecords"], 0);
    assert_eq!(seen_by_a["process"]["counters"]["restorationRecords"], 1);
}

// -- PR210-54: preferredPhy at connect -------------------------------------

fn connect_phys(radio: &Scripted) -> Vec<Vec<ubm_mobile::Phy>> {
    radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| match request {
            ubm_mobile::RadioRequest::Connect { preferred_phy, .. } => Some(preferred_phy.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn android_connects_with_the_preferred_phys() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    ok(&call(
        &session,
        "connection.connect",
        &json!({"peerId": POLAR, "lease": "l", "operationId": "c",
                    "preferredPhy": ["le-2m", "le-1m", "le-2m"]})
        .to_string(),
    )
    .await);
    assert_eq!(
        connect_phys(&radio),
        [vec![ubm_mobile::Phy::Le2m, ubm_mobile::Phy::Le1m]],
        "the radio receives the preference, once per PHY"
    );
    ok(&call(
        &session,
        "connection.disconnect",
        &json!({"peerId": POLAR, "lease": "l"}).to_string(),
    )
    .await);
    connect(&session, "plain").await;
    assert_eq!(connect_phys(&radio)[1], Vec::<ubm_mobile::Phy>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phy_preference_that_cannot_apply_is_refused_before_effects() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let holder = host.open_session("holder").unwrap();
    connect(&holder, "held").await;
    let session = host.open_session("rn").unwrap();
    let before = radio.requests.lock().unwrap().len();
    for (args, code) in [
        // Android ignores the connect PHY with autoConnect.
        (
            json!({"peerId": POLAR, "lease": "l", "operationId": "a",
                   "intent": "when-available", "preferredPhy": ["le-2m"]}),
            "capability.unsupported",
        ),
        // The link is already up: its PHYs were chosen when it was established.
        (
            json!({"peerId": POLAR, "lease": "l", "operationId": "b", "preferredPhy": ["le-coded"]}),
            "capability.unsupported",
        ),
        (
            json!({"peerId": POLAR, "lease": "l", "operationId": "c", "preferredPhy": ["le-3m"]}),
            "argument.invalid",
        ),
    ] {
        let (error, _) = failure(&call(&session, "connection.connect", &args.to_string()).await);
        assert_eq!(error["code"], code, "{args}");
    }
    assert_eq!(radio.requests.lock().unwrap().len(), before, "no effect");
    assert_eq!(counters(&session).await["counters"]["connectionLeases"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apple_refuses_a_phy_preference_before_effects() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let session = host.open_session("rn").unwrap();
    let before = radio.requests.lock().unwrap().len();
    let (error, _) = failure(
        &call(
            &session,
            "connection.connect",
            &json!({"peerId": POLAR, "lease": "l", "operationId": "c", "preferredPhy": ["le-1m"]})
                .to_string(),
        )
        .await,
    );
    assert_eq!(error["code"], "capability.unsupported");
    assert_eq!(error["operation"], "connection.connect.preferred-phy");
    assert_eq!(radio.requests.lock().unwrap().len(), before);
}

// -- PR210-69: advertisement appearance and raw record reach JS -------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advertisement_appearance_and_raw_record_travel_with_their_advertisement() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "s"}).to_string(),
    )
    .await);
    host.ingest(RadioIngress::Advertisement(Advertisement {
        appearance: Some(0x0341),
        raw_record: Some(vec![0x02, 0x01, 0x06]),
        ..match polar_advertisement() {
            RadioIngress::Advertisement(advertisement) => advertisement,
            _ => unreachable!(),
        }
    }));
    host.ingest(polar_advertisement());
    let records = drain_until(&session, |r| of_type(r, "adv").len() == 2).await;
    let adv = of_type(&records, "adv");
    assert_eq!(adv[0]["appearance"], 0x0341);
    assert_eq!(adv[0]["rawRecordB64"], "AgEG");
    assert_eq!(adv[1]["appearance"], Value::Null, "not reported stays null");
    assert_eq!(adv[1]["rawRecordB64"], Value::Null);
}

// -- 80/81 (N1): writes are bounded by the platform's per-mode answer -------

fn writable_responder(
    limits: Option<ubm_desktop::WriteLimits>,
) -> Box<dyn FnMut(&ubm_mobile::RadioRequest) -> Reply + Send> {
    Box::new(move |request| match request {
        ubm_mobile::RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            let properties = &mut services[0].characteristics[0].properties;
            properties.write = true;
            properties.write_without_response = true;
            Reply::Now(RadioCompletion::Discovered(services))
        }
        // No MTU exchange has been reported: the MTU readout is unmeasured.
        ubm_mobile::RadioRequest::ReadMtu { .. } => Reply::Now(RadioCompletion::Mtu(None)),
        ubm_mobile::RadioRequest::ReadWriteLimits { .. } => Reply::Now(match limits {
            Some(limits) => RadioCompletion::WriteLimits(limits),
            None => RadioCompletion::Failed(PlatformFailure::not_dispatched(
                FailureKind::NotConnected,
                "peer is not connected",
            )),
        }),
        other => polar_responder(other),
    })
}

async fn write_bytes(
    session: &ubm_mobile::MobileSession,
    len: usize,
    mode: &str,
    id: &str,
) -> String {
    call(
        session,
        "gatt.write",
        &json!({"peerId": POLAR, "selector": selector(),
                    "valueB64": ubm_mobile::wire::encode_base64(&vec![0x5a; len]),
                    "mode": mode, "operationId": id})
        .to_string(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn android_writes_before_any_mtu_exchange_with_long_writes_with_response() {
    // Android before `onMtuChanged`: ATT default MTU 23 bounds a command;
    // the stack performs a long write for a request (up to 512 bytes).
    let radio = Scripted::new(writable_responder(Some(ubm_desktop::WriteLimits {
        with_response: 512,
        without_response: 20,
    })));
    let (_host, session) = discovered_session(&radio, MobilePlatform::Android).await;

    let confirmed = ok(&write_bytes(&session, 512, "with-response", "w1").await);
    assert_eq!(confirmed["commitState"], "confirmed");
    let (error, commit) = failure(&write_bytes(&session, 513, "with-response", "w0").await);
    assert_eq!(
        error["code"], "bytes.too-large",
        "no attribute value exceeds 512 bytes"
    );
    assert_eq!(commit, "not-dispatched");
    let short = ok(&write_bytes(&session, 20, "without-response", "w2").await);
    assert_eq!(short["commitState"], "unknown");

    let writes = radio.count(RequestKind::Write);
    let (error, commit) = failure(&write_bytes(&session, 21, "without-response", "w3").await);
    assert_eq!(
        error["code"], "bytes.too-large",
        "a command never exceeds one ATT payload"
    );
    assert_eq!(commit, "not-dispatched");
    assert_eq!(
        radio.count(RequestKind::Write),
        writes,
        "refused before the radio"
    );
    assert_eq!(
        radio.count(RequestKind::ReadMtu),
        0,
        "limits are read per mode, not guessed from the MTU"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apple_bounds_each_write_mode_by_corebluetooth_maximum_write_value_length() {
    let radio = Scripted::new(writable_responder(Some(ubm_desktop::WriteLimits {
        with_response: 512,
        without_response: 182,
    })));
    let (_host, session) = discovered_session(&radio, MobilePlatform::Apple).await;

    ok(&write_bytes(&session, 512, "with-response", "w1").await);
    ok(&write_bytes(&session, 182, "without-response", "w2").await);
    let (error, commit) = failure(&write_bytes(&session, 183, "without-response", "w3").await);
    assert_eq!(error["code"], "bytes.too-large");
    assert_eq!(commit, "not-dispatched");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_withheld_write_limit_fails_closed_before_the_radio() {
    let radio = Scripted::new(writable_responder(None));
    let (_host, session) = discovered_session(&radio, MobilePlatform::Android).await;
    let (error, commit) = failure(&write_bytes(&session, 1, "with-response", "w1").await);
    assert_eq!(error["code"], "capability.unavailable");
    assert_eq!(commit, "not-dispatched");
    assert_eq!(radio.count(RequestKind::Write), 0);
}

// -- gatt:maximum-write-length: the platform's own per-mode answer -----------

async fn maximum_write_length(
    session: &ubm_mobile::MobileSession,
    lease: &str,
    mode: &str,
    id: &str,
) -> String {
    call(
        session,
        "connection.maximum-write-length",
        &json!({"peerId": POLAR, "lease": lease, "mode": mode, "operationId": id}).to_string(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn android_maximum_write_length_before_and_after_an_mtu_exchange() {
    // Before `onMtuChanged` the adapter answers the ATT default MTU 23 for a
    // command; with response the stack performs the long write up to the
    // ATT maximum attribute value (512).
    let limits = std::sync::Arc::new(std::sync::Mutex::new(ubm_desktop::WriteLimits {
        with_response: 512,
        without_response: 20,
    }));
    let answered = limits.clone();
    let radio = Scripted::new(Box::new(move |request| match request {
        ubm_mobile::RadioRequest::ReadWriteLimits { .. } => {
            Reply::Now(RadioCompletion::WriteLimits(*answered.lock().unwrap()))
        }
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    connect(&session, "c").await;

    let before = ok(&maximum_write_length(&session, "lease-1", "with-response", "m1").await);
    assert_eq!(before, json!({"maximumWriteLength": 512}));
    let before = ok(&maximum_write_length(&session, "lease-1", "without-response", "m2").await);
    assert_eq!(before, json!({"maximumWriteLength": 20}));

    *limits.lock().unwrap() = ubm_desktop::WriteLimits {
        with_response: 512,
        without_response: 229,
    };
    let after = ok(&maximum_write_length(&session, "lease-1", "without-response", "m3").await);
    assert_eq!(after, json!({"maximumWriteLength": 229}));
    assert_eq!(
        radio.count(RequestKind::ReadMtu),
        0,
        "answered from the platform's per-mode limits, never derived from the MTU"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apple_maximum_write_length_is_corebluetooth_maximum_write_value_length() {
    let radio = Scripted::new(writable_responder(Some(ubm_desktop::WriteLimits {
        with_response: 512,
        without_response: 182,
    })));
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let session = host.open_session("rn").unwrap();
    connect(&session, "c").await;
    let with = ok(&maximum_write_length(&session, "lease-1", "with-response", "m1").await);
    assert_eq!(with["maximumWriteLength"], 512);
    let without = ok(&maximum_write_length(&session, "lease-1", "without-response", "m2").await);
    assert_eq!(without["maximumWriteLength"], 182);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maximum_write_length_refusals_are_contract_errors() {
    let radio = Scripted::new(writable_responder(None));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    connect(&session, "c").await;
    let (error, _) =
        failure(&maximum_write_length(&session, "lease-1", "with-response", "m1").await);
    assert_eq!(
        error["code"], "capability.unavailable",
        "a withheld platform answer is never guessed"
    );
    let (error, _) = failure(&maximum_write_length(&session, "other", "with-response", "m2").await);
    assert_eq!(error["code"], "ownership.denied");
    let (error, _) = failure(&maximum_write_length(&session, "lease-1", "long-write", "m3").await);
    assert_eq!(error["code"], "argument.invalid");
}

// -- 87 (N8): foreground-service leases live as long as their scope ---------

fn leasing_responder() -> Box<dyn FnMut(&ubm_mobile::RadioRequest) -> Reply + Send> {
    let mut next = 0u64;
    Box::new(move |request| match request {
        ubm_mobile::RadioRequest::AcquireBackground { .. } => {
            next += 1;
            Reply::Now(RadioCompletion::Lease(format!("background-{next}")))
        }
        other => polar_responder(other),
    })
}

async fn acquire(session: &ubm_mobile::MobileSession) -> String {
    ok(&call(
        session,
        "background.acquire",
        &json!({"kind": "connected-device", "reason": "workout"}).to_string(),
    )
    .await)["leaseId"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn released_leases(radio: &Scripted) -> Vec<String> {
    radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| match request {
            ubm_mobile::RadioRequest::ReleaseBackground { lease_id, .. } => Some(lease_id.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scoped_background_lease_outlives_the_manager_that_acquired_it() {
    let radio = Scripted::new(leasing_responder());
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let first = host.open_scoped_session("rn", "module-1").unwrap();
    let lease = acquire(&first).await;
    let disposed = ok(&call(&first, "session.dispose", "{}").await);
    assert_eq!(disposed["state"], "released");
    assert!(
        released_leases(&radio).is_empty(),
        "manager destroy keeps the foreground service"
    );

    // A later manager of the same module updates and releases it.
    let second = host.open_scoped_session("rn", "module-1").unwrap();
    let other = host.open_scoped_session("rn", "module-2").unwrap();
    let (error, _) = failure(
        &call(
            &other,
            "background.release",
            &json!({"leaseId": lease}).to_string(),
        )
        .await,
    );
    assert_eq!(
        error["code"], "ownership.denied",
        "another module never reaches it"
    );
    ok(&call(
        &second,
        "background.update-notification",
        &json!({"leaseId": lease, "title": "Recording"}).to_string(),
    )
    .await);
    let released = ok(&call(
        &second,
        "background.release",
        &json!({"leaseId": lease}).to_string(),
    )
    .await);
    assert_eq!(released["state"], "released");
    assert_eq!(released_leases(&radio), vec![lease]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn releasing_a_scope_releases_exactly_its_leases_and_retries_failures() {
    let radio = Scripted::new(leasing_responder());
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let a = host.open_scoped_session("rn", "module-a").unwrap();
    let b = host.open_scoped_session("rn", "module-b").unwrap();
    let lease_a = acquire(&a).await;
    let lease_b = acquire(&b).await;
    ok(&call(&a, "session.dispose", "{}").await);

    radio.set_responder(Box::new(|request| match request {
        ubm_mobile::RadioRequest::ReleaseBackground { .. } => Reply::Now(RadioCompletion::Failed(
            PlatformFailure::not_dispatched(FailureKind::Platform, "stopForeground refused"),
        )),
        other => polar_responder(other),
    }));
    let failed = parse(&host.release_background_scope("module-a").await);
    assert_eq!(failed["state"], "release-failed");
    assert_eq!(failed["failures"][0]["resourceKind"], "background");

    radio.set_responder(Box::new(polar_responder));
    let released = parse(&host.release_background_scope("module-a").await);
    assert_eq!(released["state"], "released");
    assert_eq!(released_leases(&radio), vec![lease_a.clone(), lease_a]);
    let again = parse(&host.release_background_scope("module-a").await);
    assert_eq!(again["state"], "released");
    assert_eq!(
        released_leases(&radio).len(),
        2,
        "a released scope holds nothing"
    );

    // module-b's lease is untouched and still releasable by its session.
    let released = ok(&call(
        &b,
        "background.release",
        &json!({"leaseId": lease_b}).to_string(),
    )
    .await);
    assert_eq!(released["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unscoped_session_releases_its_background_at_dispose() {
    let radio = Scripted::new(leasing_responder());
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    let lease = acquire(&session).await;
    let disposed = ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(disposed["state"], "released");
    assert_eq!(released_leases(&radio), vec![lease]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_shutdown_releases_every_scoped_background_lease() {
    let radio = Scripted::new(leasing_responder());
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_scoped_session("rn", "module-1").unwrap();
    let lease = acquire(&session).await;
    ok(&call(&session, "session.dispose", "{}").await);
    let record = parse(&host.shutdown().await);
    assert_eq!(record["state"], "released", "{record}");
    assert_eq!(released_leases(&radio), vec![lease]);
}

/// Finding 95: a discovery the core refuses whole crosses the mobile wire
/// with its typed identity unchanged — a malformed platform UUID is
/// `protocol.malformed`, a database past the ATT handle space
/// `capability.limited` — and nothing is skipped or partially current.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_discovery_crosses_the_wire_whole_and_typed() {
    let cases: Vec<(Vec<ubm_desktop::ServiceSnapshot>, &str, &str)> = vec![
        (
            {
                let mut services = polar_services();
                services[0].characteristics[0].uuid = "not-a-uuid".to_owned();
                services
            },
            "protocol.malformed",
            "discovery.snapshot.uuid",
        ),
        (
            {
                let mut services = polar_services();
                let template = services[0].characteristics[0].clone();
                services[0].characteristics = (0..65_535u32)
                    .map(|index| ubm_desktop::CharacteristicSnapshot {
                        uuid: format!("{:08x}-0000-1000-8000-00805f9b34fb", 0x10_0000 + index),
                        descriptors: Vec::new(),
                        ..template.clone()
                    })
                    .collect();
                services
            },
            "capability.limited",
            "discovery.database-bound",
        ),
    ];
    for (services, code, operation) in cases {
        let radio = Scripted::new(Box::new(move |request| match request {
            ubm_mobile::RadioRequest::Discover { .. } => {
                Reply::Now(RadioCompletion::Discovered(services.clone()))
            }
            other => polar_responder(other),
        }));
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("rn").unwrap();
        connect(&session, "c").await;
        let answer = call(
            &session,
            "gatt.discover",
            &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "d"}).to_string(),
        )
        .await;
        let (error, _) = failure(&answer);
        assert_eq!(error["code"], code, "{answer}");
        assert_eq!(error["operation"], operation, "{answer}");
        assert!(!answer.contains("skipped"), "{answer}");
    }
}

// -- 104/105: every lost control record has an owner re-read ---------------

async fn reconcile(session: &ubm_mobile::MobileSession) -> Value {
    ok(&call(session, "session.reconcile", "{}").await)
}

fn link_of<'a>(snapshot: &'a Value, generation: &Value) -> &'a Value {
    snapshot["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|link| link["connectionGeneration"] == *generation)
        .unwrap_or_else(|| panic!("no link {generation} in {snapshot}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_answers_every_fact_a_lost_control_record_carried() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    let first = connect(&session, "c1").await;
    let discovered = ok(&call(
        &session,
        "gatt.discover",
        &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "d"}).to_string(),
    )
    .await);
    ok(&subscribe_with(&session, "prefer-notification", "s1").await);

    let snapshot = reconcile(&session).await;
    assert_eq!(snapshot["adapter"]["power"], "on");
    assert_eq!(snapshot["scan"], Value::Null);
    assert_eq!(snapshot["restored"], json!([]));
    let live = link_of(&snapshot, &first["connectionGeneration"]);
    assert_eq!(live["peerId"], POLAR);
    assert_eq!(live["state"], "connected");
    assert_eq!(live["reason"], Value::Null);
    assert_eq!(live["databaseGeneration"], discovered["databaseGeneration"]);
    assert_eq!(live["databaseChange"], Value::Null);
    assert_eq!(
        snapshot["subscriptions"],
        json!([{"consumer": "s1", "state": "live"}])
    );

    // db-changed: the same facts the record carried.
    host.ingest(RadioIngress::ServicesChanged {
        peer_id: POLAR.to_owned(),
    });
    let records = drain_until(&session, |r| {
        !of_type(r, "db-changed").is_empty() && !of_type(r, "stream-end").is_empty()
    })
    .await;
    let changed = of_type(&records, "db-changed")[0].clone();
    let ended = of_type(&records, "stream-end")[0].clone();
    let snapshot = reconcile(&session).await;
    let link = link_of(&snapshot, &first["connectionGeneration"]);
    assert_eq!(link["state"], "connected");
    assert_eq!(
        link["databaseGeneration"],
        Value::Null,
        "undiscovered since the change"
    );
    assert_eq!(link["databaseChange"], changed["databaseGeneration"]);
    assert_eq!(
        link["databaseState"], "undiscovered",
        "the core dropped the changed database"
    );
    assert_eq!(
        snapshot["subscriptions"],
        json!([{"consumer": "s1", "state": "ended", "reason": ended["reason"],
                "droppedItems": ended["droppedItems"], "droppedBytes": ended["droppedBytes"]}])
    );

    // security: the platform's last report.
    let bonded = ubm_mobile::SecurityState {
        bond: ubm_mobile::BondState::Bonded,
        encryption: ubm_mobile::EncryptionState::Encrypted,
        authentication: ubm_mobile::AuthenticationState::Unauthenticated,
        secure_connections: ubm_mobile::SecureConnectionsState::Yes,
        pairing_possible: None,
    };
    host.ingest(RadioIngress::SecurityChanged {
        peer_id: POLAR.to_owned(),
        state: bonded,
    });
    let records = drain_until(&session, |r| !of_type(r, "security").is_empty()).await;
    let security = of_type(&records, "security")[0].clone();
    let snapshot = reconcile(&session).await;
    assert_eq!(
        snapshot["security"],
        json!([{"peerId": POLAR, "state": security["state"]}])
    );

    // link loss, then a reconnect under a new generation: both are visible.
    host.ingest(RadioIngress::Connection {
        peer_id: POLAR.to_owned(),
        connected: false,
        status: Some(8),
    });
    let records = drain_until(&session, |r| !of_type(r, "link").is_empty()).await;
    let lost = of_type(&records, "link")[0].clone();
    let snapshot = reconcile(&session).await;
    let ended_link = link_of(&snapshot, &first["connectionGeneration"]);
    assert_eq!(ended_link["state"], "ended");
    assert_eq!(ended_link["reason"], lost["reason"]);
    assert_eq!(ended_link["databaseGeneration"], lost["databaseGeneration"]);

    ok(&call(
        &session,
        "connection.disconnect",
        &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "x"}).to_string(),
    )
    .await);
    let second = connect(&session, "c2").await;
    assert_ne!(
        second["connectionGeneration"],
        first["connectionGeneration"]
    );
    let snapshot = reconcile(&session).await;
    assert_eq!(
        link_of(&snapshot, &second["connectionGeneration"])["state"],
        "connected"
    );
    assert_eq!(
        link_of(&snapshot, &first["connectionGeneration"])["state"],
        "ended"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_answers_the_restored_set_and_the_scan_membership() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let session = host.open_session("rn").unwrap();
    host.ingest(restored_ingress(&[RESTORED]));
    drain_until(&session, |r| !of_type(r, "restored").is_empty()).await;
    let scan = ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "s"}).to_string(),
    )
    .await);
    let snapshot = reconcile(&session).await;
    assert_eq!(
        snapshot["restored"],
        json!([{"peerId": RESTORED, "name": "Polar H10", "connected": true}])
    );
    assert_eq!(snapshot["scan"], scan["operationId"]);
    host.ingest(RadioIngress::ScanFailed {
        detail: "stopped".to_owned(),
    });
    drain_until(&session, |r| !of_type(r, "scan-end").is_empty()).await;
    assert_eq!(reconcile(&session).await["scan"], Value::Null);
}

// -- 109: op.cancel is classified exactly by admission ------------------------

fn bonded_radio() -> std::sync::Arc<Scripted> {
    Scripted::new(Box::new(|request| match request {
        ubm_mobile::RadioRequest::BondedPeers { .. } => {
            Reply::Now(RadioCompletion::BondedPeers(Vec::new()))
        }
        other => polar_responder(other),
    }))
}

fn cancel_args(id: &str, admission: u64) -> String {
    json!({"operationId": id, "admission": admission}).to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_cancel_after_thousands_of_finished_operations_is_already_terminal() {
    let radio = bonded_radio();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    for admission in 1..=5_001u64 {
        ok(&session
            .call(
                "peers.bonded",
                &json!({"operationId": format!("b{admission}"), "admission": admission})
                    .to_string(),
            )
            .await);
    }
    let ack = ok(&session.call("op.cancel", &cancel_args("b1", 1)).await);
    assert_eq!(ack["state"], "already-terminal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_early_cancel_survives_thousands_of_other_early_cancels() {
    let radio = bonded_radio();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    let ack = ok(&session.call("op.cancel", &cancel_args("early", 1)).await);
    assert_eq!(ack["state"], "cancellation-requested");
    for admission in 2..=5_002u64 {
        ok(&session
            .call(
                "op.cancel",
                &cancel_args(&format!("t{admission}"), admission),
            )
            .await);
    }
    let before = radio.count(RequestKind::BondedPeers);
    let (error, _) = failure(
        &session
            .call(
                "peers.bonded",
                &json!({"operationId": "early", "admission": 1}).to_string(),
            )
            .await,
    );
    assert_eq!(error["code"], "operation.aborted");
    assert_eq!(
        radio.count(RequestKind::BondedPeers),
        before,
        "zero effects"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admissions_are_strictly_increasing_and_bound_to_their_operation() {
    let radio = bonded_radio();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    let bonded =
        |id: &str, admission: u64| json!({"operationId": id, "admission": admission}).to_string();
    ok(&session.call("peers.bonded", &bonded("a", 5)).await);
    for (id, admission) in [("b", 5), ("c", 4)] {
        let (error, _) = failure(&session.call("peers.bonded", &bonded(id, admission)).await);
        assert_eq!(error["code"], "argument.invalid", "{id}");
        assert_eq!(error["operation"], "ubm-mobile.wire.args.admission");
    }
    // An operation id without an admission (and the reverse) is refused.
    let (error, _) = failure(
        &session
            .call("peers.bonded", &json!({"operationId": "d"}).to_string())
            .await,
    );
    assert_eq!(error["code"], "argument.invalid");
    let (error, _) = failure(
        &session
            .call("adapter.state", &json!({"admission": 9}).to_string())
            .await,
    );
    assert_eq!(error["code"], "argument.invalid");
    // A pre-admission cancel beyond the window is a protocol violation.
    let (error, _) = failure(
        &session
            .call(
                "op.cancel",
                &cancel_args("far", 10 + ubm_mobile::session::ADMISSION_WINDOW + 1),
            )
            .await,
    );
    assert_eq!(error["code"], "argument.invalid");
}

// -- 112 (B1): a connect without a budget waits as long as legacy did --------

fn holding_connects() -> std::sync::Arc<Scripted> {
    Scripted::new(Box::new(|request| match request {
        ubm_mobile::RadioRequest::Connect { .. } => Reply::Hold,
        other => polar_responder(other),
    }))
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_connect_without_a_budget_waits_past_any_backstop_until_cancelled() {
    for intent in ["direct", "when-available"] {
        let radio = holding_connects();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("rn").unwrap();
        let pending = tokio::spawn({
            let session = session.clone();
            let args = json!({"peerId": POLAR, "lease": "l", "intent": intent, "operationId": "c"})
                .to_string();
            async move { call(&session, "connection.connect", &args).await }
        });
        tokio::time::sleep(Duration::from_secs(60 * 60)).await;
        assert!(
            !pending.is_finished(),
            "{intent}: legacy waited indefinitely"
        );
        assert_eq!(radio.count(RequestKind::Connect), 1);
        ok(&call(
            &session,
            "op.cancel",
            &json!({"operationId": "c"}).to_string(),
        )
        .await);
        let (error, _) = failure(&pending.await.unwrap());
        assert_eq!(error["code"], "operation.aborted", "{intent}");
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_connect_with_a_budget_ends_at_that_budget() {
    let radio = holding_connects();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    let started = tokio::time::Instant::now();
    let text = call(
        &session,
        "connection.connect",
        &json!({"peerId": POLAR, "lease": "l", "operationId": "c", "budgetMs": 500_000})
            .to_string(),
    )
    .await;
    let envelope = parse(&text);
    assert_eq!(envelope["ok"], false);
    // Finding 161: the deadline expired before any link came up — the peer
    // did not answer — so the budgeted connect reports `connection.failed`
    // (caller-decides) with the deadline fact, on every host.
    let (error, _) = failure(&text);
    assert_eq!(error["code"], "connection.failed");
    assert_eq!(envelope["retryability"], "caller-decides");
    assert_eq!(error["platform"]["domain"], "core");
    assert_eq!(error["platform"]["code"], "deadline-expired");
    assert_eq!(started.elapsed(), Duration::from_millis(500_000));
}

// -- 113 (B2): radio failures carry legacy React Native's error identity -----

fn failing(
    platform_failure: PlatformFailure,
) -> Box<dyn FnMut(&ubm_mobile::RadioRequest) -> Reply + Send> {
    Box::new(move |request| match request {
        ubm_mobile::RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            let properties = &mut services[0].characteristics[0].properties;
            properties.read = true;
            properties.write = true;
            Reply::Now(RadioCompletion::Discovered(services))
        }
        ubm_mobile::RadioRequest::Read { .. } | ubm_mobile::RadioRequest::Write { .. } => {
            Reply::Now(RadioCompletion::Failed(platform_failure.clone()))
        }
        other => polar_responder(other),
    })
}

async fn failed_op(platform: MobilePlatform, failure_value: PlatformFailure, op: &str) -> Value {
    let radio = Scripted::new(failing(failure_value));
    let (_host, session) = discovered_session(&radio, platform).await;
    let args = if op == "gatt.write" {
        json!({"peerId": POLAR, "selector": selector(), "valueB64": "AQ==",
               "mode": "with-response", "operationId": "w"})
    } else {
        json!({"peerId": POLAR, "selector": selector(), "operationId": "r"})
    };
    parse(&call(&session, op, &args.to_string()).await)
}

fn gatt_failure(status: i32) -> PlatformFailure {
    PlatformFailure {
        gatt_status: Some(status),
        ..PlatformFailure::new(FailureKind::GattStatus, "GATT_INSUFFICIENT_AUTHENTICATION")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_android_gatt_failure_is_platform_failure_with_the_android_status() {
    let refused = PlatformFailure {
        gatt_status: Some(3),
        ..PlatformFailure::new(FailureKind::GattStatus, "GATT_WRITE_NOT_PERMITTED")
    };
    let envelope = failed_op(MobilePlatform::Android, refused, "gatt.write").await;
    assert_eq!(envelope["error"]["code"], "platform.failure");
    assert_eq!(envelope["error"]["domain"], "platform");
    assert_eq!(
        envelope["error"]["platform"],
        json!({"domain": "android", "code": "writeFailed",
               "message": "GATT_WRITE_NOT_PERMITTED",
               "metadata": {"androidGattStatus": 3}})
    );
    assert_eq!(envelope["commit"], "uncertain");
}

/// Owner decision (5.0): a refusal for lack of authentication, authorization
/// or encryption is `platform.security` (recovery: pair) on every host that
/// can tell, the platform's answer kept — Android GATT 5/8/12/15/137 and
/// Apple `CBATTErrorDomain` 5/8/12/15 alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_security_refusal_is_platform_security_on_both_platforms() {
    for status in [5, 8, 12, 15, 137] {
        let read = failed_op(MobilePlatform::Android, gatt_failure(status), "gatt.read").await;
        assert_eq!(
            read["error"]["code"], "platform.security",
            "android {status}"
        );
        assert_eq!(read["error"]["platform"]["code"], "readFailed");
        assert_eq!(
            read["error"]["platform"]["metadata"]["androidGattStatus"],
            status
        );
    }
    let write = failed_op(MobilePlatform::Android, gatt_failure(5), "gatt.write").await;
    assert_eq!(write["error"]["code"], "platform.security");
    assert_eq!(
        write["commit"], "uncertain",
        "a dispatched write stays uncertain"
    );
    assert_eq!(write["retryability"], "never");
    for code in [5, 15] {
        let att = PlatformFailure {
            gatt_status: Some(code),
            native_domain: Some("CBATTErrorDomain".to_owned()),
            native_code: Some(i64::from(code)),
            ..PlatformFailure::new(FailureKind::GattStatus, "Authentication is insufficient.")
        };
        let read = failed_op(MobilePlatform::Apple, att, "gatt.read").await;
        assert_eq!(read["error"]["code"], "platform.security", "apple {code}");
        assert_eq!(read["error"]["platform"]["domain"], "CBATTErrorDomain");
        assert_eq!(read["error"]["platform"]["code"], code.to_string());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_android_link_loss_is_connection_lost_as_legacy_reported_it() {
    for failure_value in [
        gatt_failure(19),
        PlatformFailure::new(FailureKind::NotConnected, "link lost"),
    ] {
        let envelope = failed_op(MobilePlatform::Android, failure_value, "gatt.read").await;
        assert_eq!(envelope["error"]["code"], "connection.lost");
        assert_eq!(envelope["error"]["domain"], "connection");
        assert_eq!(envelope["error"]["platform"]["domain"], "android");
        assert_eq!(envelope["error"]["platform"]["code"], "connectionLost");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_apple_failure_carries_the_nserror_domain_and_code() {
    let att = PlatformFailure {
        gatt_status: Some(3),
        native_domain: Some("CBATTErrorDomain".to_owned()),
        native_code: Some(3),
        ..PlatformFailure::new(FailureKind::GattStatus, "Writing is not permitted.")
    };
    let envelope = failed_op(MobilePlatform::Apple, att, "gatt.write").await;
    assert_eq!(envelope["error"]["code"], "platform.failure");
    assert_eq!(
        envelope["error"]["platform"],
        json!({"domain": "CBATTErrorDomain", "code": "3",
               "message": "Writing is not permitted.", "metadata": {}})
    );
    // Without an NSError the legacy native code names the verb.
    let bare = PlatformFailure::new(FailureKind::Platform, "no write limit");
    let envelope = failed_op(MobilePlatform::Apple, bare, "gatt.read").await;
    assert_eq!(envelope["error"]["platform"]["domain"], "corebluetooth");
    assert_eq!(envelope["error"]["platform"]["code"], "readFailed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_platform_cancel_is_operation_aborted_without_platform_detail() {
    let envelope = failed_op(
        MobilePlatform::Android,
        PlatformFailure::new(FailureKind::Cancelled, "cancelled"),
        "gatt.read",
    )
    .await;
    assert_eq!(envelope["error"]["code"], "operation.aborted");
    assert_eq!(envelope["error"]["platform"], Value::Null);
}

/// Ported from 4.x `react-native-android-vertical-slice.test.js` ("Android
/// normalizes the native CCCD link-loss terminal…", "Android keeps ordinary
/// CCCD status failures as platform failures").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn android_cccd_failures_keep_their_legacy_identity() {
    for (status, code, native_code) in [
        (19, "connection.lost", "connectionLost"),
        (133, "platform.failure", "subscriptionFailed"),
    ] {
        let radio = Scripted::new(Box::new(move |request| match request {
            ubm_mobile::RadioRequest::EnableNotifications { .. } => {
                Reply::Now(RadioCompletion::Failed(gatt_failure(status)))
            }
            other => polar_responder(other),
        }));
        let (_host, session) = discovered_session(&radio, MobilePlatform::Android).await;
        let (error, _) = failure(&subscribe_with(&session, "prefer-notification", "s").await);
        assert_eq!(error["code"], code, "status {status}");
        assert_eq!(error["platform"]["domain"], "android");
        assert_eq!(error["platform"]["code"], native_code);
        assert_eq!(error["platform"]["metadata"]["androidGattStatus"], status);
    }
}

// -- 123 (S4): user- and OS-driven waits have no backstop -------------------

fn holding_user_waits() -> std::sync::Arc<Scripted> {
    Scripted::new(Box::new(|request| match request {
        ubm_mobile::RadioRequest::SecurityState { .. } => {
            Reply::Now(RadioCompletion::Security(ubm_mobile::SecurityState {
                bond: ubm_mobile::BondState::NotBonded,
                encryption: ubm_mobile::EncryptionState::Unknown,
                authentication: ubm_mobile::AuthenticationState::Unknown,
                secure_connections: ubm_mobile::SecureConnectionsState::Unknown,
                pairing_possible: Some(true),
            }))
        }
        ubm_mobile::RadioRequest::CreateBond { .. }
        | ubm_mobile::RadioRequest::CancelBond { .. }
        | ubm_mobile::RadioRequest::AssociateCompanion { .. } => Reply::Hold,
        other => polar_responder(other),
    }))
}

fn user_wait(op: &str, budget: Option<u64>) -> String {
    let mut args = match op {
        "security.pair" => json!({"peerId": POLAR, "transport": "auto", "operationId": "u"}),
        "security.cancel-pairing" => json!({"peerId": POLAR, "operationId": "u"}),
        _ => json!({"name": "Polar", "operationId": "u"}),
    };
    if let Some(budget) = budget {
        args["budgetMs"] = json!(budget);
    }
    args.to_string()
}

const USER_WAITS: [&str; 3] = [
    "security.pair",
    "security.cancel-pairing",
    "companion.associate",
];

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_user_or_os_wait_without_a_budget_waits_past_any_backstop_until_cancelled() {
    for op in USER_WAITS {
        let radio = holding_user_waits();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("rn").unwrap();
        let pending = tokio::spawn({
            let session = session.clone();
            let args = user_wait(op, None);
            async move { call(&session, op, &args).await }
        });
        tokio::time::sleep(Duration::from_secs(60 * 60)).await;
        assert!(
            !pending.is_finished(),
            "{op}: legacy waited for the OS/user"
        );
        ok(&call(
            &session,
            "op.cancel",
            &json!({"operationId": "u"}).to_string(),
        )
        .await);
        let (error, _) = failure(&pending.await.unwrap());
        assert_eq!(error["code"], "operation.aborted", "{op}");
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_user_or_os_wait_with_a_budget_ends_at_that_budget() {
    for op in USER_WAITS {
        let radio = holding_user_waits();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("rn").unwrap();
        let started = tokio::time::Instant::now();
        let (error, _) = failure(&call(&session, op, &user_wait(op, Some(300_000))).await);
        assert_eq!(error["code"], "operation.timed-out", "{op}");
        assert_eq!(started.elapsed(), Duration::from_millis(300_000), "{op}");
    }
}

/// Every platform reports one word for one fact (owner decision, 5.0,
/// superseding finding 132's Apple identity rule): an operation pending at a
/// disconnect on Apple fails `connection.lost`, as on Android, with the
/// NSError the platform failed it with (owned radio 1016/1020, CoreBluetooth
/// `peripheralDisconnected` 7) kept as the platform detail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_apple_operation_pending_at_a_disconnect_is_connection_lost_with_its_nserror() {
    for (domain, code, op) in [
        (
            "com.sfourdrinier.unifiedblemanager.corebluetooth",
            1020,
            "gatt.read",
        ),
        (
            "com.sfourdrinier.unifiedblemanager.corebluetooth",
            1016,
            "gatt.discover",
        ),
        ("CBErrorDomain", 7, "gatt.discover"),
    ] {
        let disconnected = PlatformFailure {
            native_domain: Some(domain.to_owned()),
            native_code: Some(code),
            ..PlatformFailure::new(FailureKind::NotConnected, "CoreBluetooth disconnected")
        };
        let envelope = if op == "gatt.discover" {
            let radio = Scripted::new(Box::new(move |request| match request {
                ubm_mobile::RadioRequest::Discover { .. } => {
                    Reply::Now(RadioCompletion::Failed(disconnected.clone()))
                }
                other => polar_responder(other),
            }));
            let (host, _) = open(&radio, MobilePlatform::Apple).await;
            let session = host.open_session("rn").unwrap();
            connect(&session, "c").await;
            parse(
                &call(
                    &session,
                    "gatt.discover",
                    &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "d"}).to_string(),
                )
                .await,
            )
        } else {
            failed_op(MobilePlatform::Apple, disconnected, op).await
        };
        assert_eq!(
            envelope["error"]["code"], "connection.lost",
            "{domain}#{code} {op}"
        );
        assert_eq!(envelope["error"]["domain"], "connection");
        assert_eq!(envelope["error"]["platform"]["domain"], domain);
        assert_eq!(envelope["error"]["platform"]["code"], code.to_string());
    }
}

// -- 133: a named Android native code travels to the Expo layer -------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_android_native_code_names_the_platform_detail_whatever_the_kind() {
    for (kind, code) in [
        (FailureKind::Unsupported, "capability.unsupported"),
        (FailureKind::PermissionDenied, "permission.denied"),
        (FailureKind::Platform, "platform.failure"),
    ] {
        let named = PlatformFailure {
            native_name: Some("foregroundServiceNotConfigured".to_owned()),
            ..PlatformFailure::new(kind, "Rebuild with configured notification metadata.")
        };
        let radio = Scripted::new(Box::new(move |request| match request {
            ubm_mobile::RadioRequest::AcquireBackground { .. } => {
                Reply::Now(RadioCompletion::Failed(named.clone()))
            }
            other => polar_responder(other),
        }));
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("rn").unwrap();
        let (error, _) = failure(
            &call(
                &session,
                "background.acquire",
                &json!({"kind": "connected-device", "reason": "workout"}).to_string(),
            )
            .await,
        );
        assert_eq!(error["code"], code);
        assert_eq!(
            error["platform"],
            json!({"domain": "android", "code": "foregroundServiceNotConfigured",
                   "message": "Rebuild with configured notification metadata.", "metadata": {}})
        );
    }
}

// -- 139 (AN-1..3): Android link-control and scan-option identities --------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn android_scan_phy_and_report_delay_are_capability_unsupported_as_legacy() {
    for key in ["phy", "reportDelayMs"] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, MobilePlatform::Android).await;
        let session = host.open_session("rn").unwrap();
        let mut platform = json!({"mode": "balanced"});
        platform[key] = if key == "phy" {
            json!("le-coded")
        } else {
            json!(500)
        };
        let (error, _) = failure(
            &call(
                &session,
                "scan.start",
                &json!({"serviceUuids": [], "duplicatePolicy": "all", "operationId": "s", "platform": platform})
                    .to_string(),
            )
            .await,
        );
        assert_eq!(error["code"], "capability.unsupported", "{key}");
        assert_eq!(error["domain"], "scan", "{key}");
        assert_eq!(error["operation"], "scan.start.platform-options", "{key}");
        assert_eq!(radio.count(RequestKind::StartScan), 0, "{key}: no effect");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_phy_request_is_argument_invalid_in_the_connection_domain_as_legacy() {
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    connect(&session, "c").await;
    let (error, _) = failure(
        &call(
            &session,
            "connection.request-phy",
            &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "p"}).to_string(),
        )
        .await,
    );
    assert_eq!(error["code"], "argument.invalid");
    assert_eq!(error["domain"], "connection");
    assert_eq!(radio.count(RequestKind::RequestPhy), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_mtu_below_23_reaches_the_radio_as_legacy_did() {
    let radio = Scripted::new(Box::new(|request| match request {
        ubm_mobile::RadioRequest::RequestMtu { .. } => Reply::Now(RadioCompletion::Failed(
            PlatformFailure::new(FailureKind::Platform, "requestMtu failed to start"),
        )),
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    connect(&session, "c").await;
    let (error, _) = failure(
        &call(
            &session,
            "connection.request-mtu",
            &json!({"peerId": POLAR, "lease": "lease-1", "mtu": 10, "operationId": "m"})
                .to_string(),
        )
        .await,
    );
    let requested = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|request| match request {
            ubm_mobile::RadioRequest::RequestMtu { mtu, .. } => Some(*mtu),
            _ => None,
        });
    assert_eq!(
        requested,
        Some(10),
        "legacy passed the request to the platform"
    );
    assert_eq!(error["code"], "platform.failure");
    assert_eq!(error["platform"]["code"], "requestMtuFailed");
}

/// Owner decision (5.0): a connect whose link the platform could not
/// establish (Android GATT 133 / HCI 0x3E, CoreBluetooth `connectionFailed`)
/// reports `caller-decides` on the wire, with the platform's answer kept;
/// the owner never retries it. A connect refused for another reason, and
/// a dispatched write, stay `never`: every failure envelope says which.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transient_connect_failure_is_caller_decides_on_the_wire() {
    let cases = [
        (
            MobilePlatform::Android,
            PlatformFailure {
                gatt_status: Some(133),
                ..PlatformFailure::new(
                    FailureKind::GattStatus,
                    "Android GATT connection failed with status 133",
                )
            },
            "caller-decides",
        ),
        (
            MobilePlatform::Android,
            PlatformFailure {
                gatt_status: Some(62),
                ..PlatformFailure::new(
                    FailureKind::GattStatus,
                    "Android GATT connection failed with status 62",
                )
            },
            "caller-decides",
        ),
        (
            MobilePlatform::Android,
            PlatformFailure {
                gatt_status: Some(5),
                ..PlatformFailure::new(FailureKind::GattStatus, "insufficient authentication")
            },
            "never",
        ),
        (
            MobilePlatform::Apple,
            PlatformFailure {
                native_domain: Some("CBErrorDomain".to_owned()),
                native_code: Some(10),
                ..PlatformFailure::new(FailureKind::Platform, "CBErrorDomain#10: Connection failed")
            },
            "caller-decides",
        ),
        (
            MobilePlatform::Apple,
            PlatformFailure {
                native_domain: Some("CBErrorDomain".to_owned()),
                native_code: Some(6),
                ..PlatformFailure::new(
                    FailureKind::Platform,
                    "CBErrorDomain#6: The connection has timed out",
                )
            },
            "caller-decides",
        ),
        (
            MobilePlatform::Apple,
            PlatformFailure {
                native_domain: Some("CBErrorDomain".to_owned()),
                native_code: Some(14),
                ..PlatformFailure::new(
                    FailureKind::Platform,
                    "CBErrorDomain#14: Peer removed pairing information",
                )
            },
            "never",
        ),
    ];
    for (platform, answer, retryability) in cases {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let session = host.open_session("rn").unwrap();
        let refusal = answer.clone();
        radio.set_responder(Box::new(move |request| match request {
            ubm_mobile::RadioRequest::Connect { .. } => {
                Reply::Now(RadioCompletion::Failed(refusal.clone()))
            }
            other => polar_responder(other),
        }));
        let text = call(
            &session,
            "connection.connect",
            &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "c"}).to_string(),
        )
        .await;
        let envelope = parse(&text);
        assert_eq!(envelope["ok"], false, "{text}");
        assert_eq!(envelope["retryability"], retryability, "{answer:?}: {text}");
        assert_eq!(
            envelope["error"]["code"], "connection.failed",
            "one word for a failed connect on every host"
        );
        assert!(
            envelope["error"]["platform"].is_object(),
            "the platform's answer is kept"
        );
        assert_eq!(
            radio.count(RequestKind::Connect),
            1,
            "the owner never retries"
        );
    }
}

/// Physical run (Samsung, Polar H10): the link dropped with GATT status 22
/// while a release was pending, and the lifecycle said the app released
/// it. A disconnect the platform reports with an error status (Android
/// non-zero GATT status, a CoreBluetooth disconnect `NSError`) is a loss
/// even when a release was requested; a clean disconnect (status 0, or no
/// error) confirms the release.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_errored_disconnect_during_a_release_is_a_loss() {
    for (platform, status, reason) in [
        (MobilePlatform::Android, Some(22), "peer"),
        (MobilePlatform::Apple, Some(7), "peer"),
        (MobilePlatform::Android, Some(0), "local"),
        (MobilePlatform::Apple, None, "local"),
    ] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let session = host.open_session("rn").unwrap();
        connect(&session, "c").await;
        radio.set_responder(Box::new(|request| match request {
            ubm_mobile::RadioRequest::Disconnect { .. } => Reply::Hold,
            other => polar_responder(other),
        }));
        let releasing = session.clone();
        let release = tokio::spawn(async move {
            call(
                &releasing,
                "connection.disconnect",
                &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "d"}).to_string(),
            )
            .await
        });
        for _ in 0..3000 {
            if !radio.held_of(RequestKind::Disconnect).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let held = radio.held_of(RequestKind::Disconnect);
        assert_eq!(held.len(), 1, "the release is pending");
        host.ingest(RadioIngress::Connection {
            peer_id: POLAR.to_owned(),
            connected: false,
            status,
        });
        let records = drain_until(&session, |r| !of_type(r, "link").is_empty()).await;
        assert_eq!(
            of_type(&records, "link")[0]["reason"],
            reason,
            "{platform:?} status {status:?}"
        );
        radio.answer(held[0], RadioCompletion::Unit);
        ok(&release.await.unwrap());
    }
}

/// Owner decision (5.0): one word per event. An operation the platform
/// fails because the link is gone is `connection.lost`; the same failure
/// while the app's own release is underway (Android fails every pending
/// operation when the app disconnects) is `operation.disconnected`, the
/// word for "your release cut this off", on every host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_operation_cut_off_by_the_apps_release_is_operation_disconnected() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::new(failing(PlatformFailure::new(
            FailureKind::NotConnected,
            "link down",
        )));
        let (_host, session) = discovered_session(&radio, platform).await;
        radio.set_responder(Box::new(|request| match request {
            ubm_mobile::RadioRequest::Read { .. } | ubm_mobile::RadioRequest::Disconnect { .. } => {
                Reply::Hold
            }
            other => polar_responder(other),
        }));
        let reader = session.clone();
        let read = tokio::spawn(async move {
            call(
                &reader,
                "gatt.read",
                &json!({"peerId": POLAR, "selector": selector(), "operationId": "r"}).to_string(),
            )
            .await
        });
        for _ in 0..3000 {
            if !radio.held_of(RequestKind::Read).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let releasing = session.clone();
        let release = tokio::spawn(async move {
            call(
                &releasing,
                "connection.disconnect",
                &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "x"}).to_string(),
            )
            .await
        });
        for _ in 0..3000 {
            if !radio.held_of(RequestKind::Disconnect).is_empty()
                && !radio.held_of(RequestKind::Read).is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let read_id = radio.held_of(RequestKind::Read)[0];
        radio.answer(
            read_id,
            RadioCompletion::Failed(PlatformFailure::new(
                FailureKind::NotConnected,
                "app disconnect",
            )),
        );
        let (error, _) = failure(&read.await.unwrap());
        // The owner ends the read when the release starts (as Android's
        // stack does), so the platform's late answer finds it settled.
        assert_eq!(error["code"], "operation.disconnected", "{platform:?}");
        assert_eq!(error["domain"], "connection");
        let disconnect_id = radio.held_of(RequestKind::Disconnect)[0];
        radio.answer(disconnect_id, RadioCompletion::Unit);
        ok(&release.await.unwrap());
    }
}

// -- issue #212: companion presence observation ------------------------------

fn presence_responder(request: &ubm_mobile::RadioRequest) -> Reply {
    match request {
        ubm_mobile::RadioRequest::ObservePresence { .. }
        | ubm_mobile::RadioRequest::StopPresence { .. } => Reply::Now(RadioCompletion::Unit),
        other => polar_responder(other),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presence_observe_arms_one_peer_and_unobserve_disarms() {
    let radio = Scripted::new(Box::new(presence_responder));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    let observing = ok(&call(
        &session,
        "presence.observe",
        &json!({"peerId": POLAR, "operationId": "p1"}).to_string(),
    )
    .await);
    assert_eq!(observing["state"], "observing");
    assert_eq!(radio.count(RequestKind::ObservePresence), 1);
    let idle = ok(&call(
        &session,
        "presence.unobserve",
        &json!({"peerId": POLAR, "operationId": "p2"}).to_string(),
    )
    .await);
    assert_eq!(idle["state"], "idle");
    assert_eq!(radio.count(RequestKind::StopPresence), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presence_observation_is_an_android_service() {
    // Apple delivers restoration through willRestoreState; there is no
    // presence observation to arm, so both verbs refuse before any effect.
    let radio = Scripted::new(Box::new(presence_responder));
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let session = host.open_session("rn").unwrap();
    for (index, op) in ["presence.observe", "presence.unobserve"]
        .iter()
        .enumerate()
    {
        let (error, _) = failure(
            &call(
                &session,
                op,
                &json!({"peerId": RESTORED, "operationId": format!("apple-{index}")}).to_string(),
            )
            .await,
        );
        assert_eq!(error["code"], "capability.unsupported", "{op}");
    }
    assert_eq!(radio.count(RequestKind::ObservePresence), 0);
    assert_eq!(radio.count(RequestKind::StopPresence), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presence_arguments_are_validated_before_any_effect() {
    let radio = Scripted::new(Box::new(presence_responder));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    for op in ["presence.observe", "presence.unobserve"] {
        let (error, _) = failure(&call(&session, op, "{}").await);
        assert_eq!(error["code"], "argument.invalid", "{op}");
    }
    assert_eq!(radio.count(RequestKind::ObservePresence), 0);
    assert_eq!(radio.count(RequestKind::StopPresence), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn android_claims_presence_restored_peers_once_per_process() {
    // Issue #212: a presence wake surfaces the same restored peers and the
    // same claim semantics as iOS state restoration.
    let radio = Scripted::new(Box::new(presence_responder));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    host.ingest(restored_ingress(&[POLAR]));
    let first = host.open_session("manager-a").unwrap();
    let second = host.open_session("manager-b").unwrap();
    wait_for_restored(&first, 1).await;

    let claimed = ok(&claim(&second, 1023).await);
    assert_eq!(claimed_ids(&claimed), [POLAR]);
    assert_eq!(claimed["peers"][0]["source"], "restored");
    assert!(claimed_ids(&ok(&claim(&first, 1023).await)).is_empty());
    assert!(claimed_ids(&ok(&claim(&second, 1023).await)).is_empty());
    let (error, _) = failure(&call(&second, "peers.claim-restored", "{}").await);
    assert_eq!(error["code"], "argument.invalid", "maxPeers is required");
}

/// Finding 194 (mobile owner): a connect whose budget expires while the radio
/// holds it must not wedge its peer. The expiry reports the connection
/// failure (finding 161: the peer did not answer), the owner's pending claim
/// is freed, and a retry for the same peer is admitted — never
/// `connection.already-owned`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_connect_budget_frees_the_peer_for_retry() {
    use std::sync::atomic::AtomicBool;
    let hold_first = std::sync::Arc::new(AtomicBool::new(false));
    let hold = std::sync::Arc::clone(&hold_first);
    let radio = Scripted::new(Box::new(move |request| match request {
        ubm_mobile::RadioRequest::Connect { .. } if !hold.swap(true, Ordering::SeqCst) => Reply::Hold,
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("rn").unwrap();
    ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids": ["180D"], "duplicatePolicy": "all", "operationId": "scan-1"}).to_string(),
    )
    .await);
    assert_eq!(host.ingest(polar_advertisement()), IngressStatus::Accepted);
    let expired = tokio::time::timeout(
        Duration::from_secs(5),
        call(
            &session,
            "connection.connect",
            &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "connect-1", "budgetMs": 50})
                .to_string(),
        ),
    )
    .await
    .expect("the owner answers a budgeted connect");
    let (error, _) = failure(&expired);
    assert_eq!(error["code"], "connection.failed", "the peer did not answer");
    assert_eq!(radio.held_of(RequestKind::Connect).len(), 1, "the radio never answered");
    let retry = ok(&call(
        &session,
        "connection.connect",
        &json!({"peerId": POLAR, "lease": "lease-2", "operationId": "connect-2"}).to_string(),
    )
    .await);
    assert!(retry["connectionGeneration"].is_string(), "retry is admitted");
}
