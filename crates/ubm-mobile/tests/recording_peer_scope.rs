//! Real mobile-host fixture driven by the TypeScript recording controller.
//! stdin/stdout are test transport only; every prepare/ACK reaches SQLite.
mod common;

use common::*;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::sync::Arc;
use ubm_desktop::continuation_journal::JournalRegistry;
use ubm_mobile::{
    AuthenticationState, BondState, EncryptionState, Instance, MobilePlatform, RadioIngress,
    RadioRequest, SecureConnectionsState, SecurityState,
};

const OTHER: &str = "A0:9E:1A:00:00:02";

fn directory(label: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "ubm-mobile-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    directory
}

fn security() -> RadioIngress {
    RadioIngress::SecurityChanged {
        peer_id: OTHER.into(),
        state: SecurityState {
            bond: BondState::Bonded,
            encryption: EncryptionState::Encrypted,
            authentication: AuthenticationState::Unknown,
            secure_connections: SecureConnectionsState::Unknown,
            pairing_possible: Some(true),
        },
    }
}

fn respond(value: Value) {
    println!("UBM_RECORDING {}", value);
    std::io::stdout().flush().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mobile_recording_controller_transport() {
    let directory = directory("peer-scope");
    let platform = match std::env::var("UBM_RECORDING_PLATFORM")
        .unwrap_or_else(|_| "android".into())
        .as_str()
    {
        "android" => MobilePlatform::Android,
        "apple" => MobilePlatform::Apple,
        _ => panic!("explicit mobile platform required"),
    };
    let radio = Scripted::polar();
    let (host, _) = open(&radio, platform).await;
    let other = host.open_session("other-peer").unwrap();
    for (operation, id) in [
        ("connection.connect", "connect"),
        ("gatt.discover", "discover"),
    ] {
        ok(&call(
            &other,
            operation,
            &json!({"peerId":OTHER,"lease":"other","operationId":id}).to_string(),
        )
        .await);
    }
    // Queue foreign process controls before native recording admission. The
    // pump may deliver them before or during journal attachment; neither
    // ordering may refuse recording or contaminate its peer-scoped prefix.
    host.ingest(security());
    let engine = host.continuation();
    engine.configure_recording_directory(&directory).unwrap();
    let order = json!({"onAppearance":"native","resubscribe":[{
        "serviceUuid":HR_SERVICE,"serviceOccurrence":1,
        "characteristicUuid":HR_MEASUREMENT,"characteristicOccurrence":1
    }],"recording":{"id":"peer-scope","maxBytes":1048576,"maxRecords":1000}});
    engine.execute(POLAR, &order.to_string()).await.unwrap();
    let epoch = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|request| match request {
            RadioRequest::EnableNotifications {
                epoch, instance, ..
            } if instance.peer_id == POLAR => Some(*epoch),
            _ => None,
        })
        .unwrap();
    let notify = |counter| {
        host.ingest(RadioIngress::Notification {
            instance: Instance {
                peer_id: POLAR.into(),
                service_uuid: HR_SERVICE.into(),
                service_occurrence: 0,
                characteristic_uuid: HR_MEASUREMENT.into(),
                characteristic_occurrence: 0,
            },
            epoch,
            value: vec![0, counter],
        })
    };
    notify(70);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let reader = engine.clone();
            if ubm_desktop::continuation_journal::run_blocking(move || {
                reader.recording_status("peer-scope")
            })
            .await
            .unwrap()["records"]
                == 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    host.ingest(security());
    host.ingest(RadioIngress::ServicesChanged {
        peer_id: OTHER.into(),
    });
    let events = drain_until(&other, |rows| {
        rows.iter().any(|row| row["t"] == "db-changed")
    })
    .await;
    host.ingest(RadioIngress::Connection {
        peer_id: OTHER.into(),
        connected: false,
        status: None,
    });
    let mut events = events;
    events.extend(drain_until(&other, |rows| rows.iter().any(|row| row["t"] == "link")).await);
    // A process-global adapter fact is eligible, without a fabricated peer.
    host.ingest(RadioIngress::AdapterState(adapter_on()));
    drain_until(&other, |rows| rows.iter().any(|row| row["t"] == "adapter")).await;
    for counter in [71, 72] {
        let before = engine.recording_status("peer-scope").unwrap()["records"]
            .as_u64()
            .unwrap();
        notify(counter);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let reader = engine.clone();
                if ubm_desktop::continuation_journal::run_blocking(move || {
                    reader.recording_status("peer-scope")
                })
                .await
                .unwrap()["records"]
                    .as_u64()
                    .unwrap()
                    > before
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    let bridge = std::env::var("UBM_RECORDING_CONTROLLER_BRIDGE").as_deref() == Ok("1");
    let mut restarted: Option<Arc<JournalRegistry>> = None;
    if bridge {
        respond(json!({"ready":true,"events":events}));
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let input = std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                if commands.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        while let Some(line) = received.recv().await {
            let request: Value = serde_json::from_str(&line).unwrap();
            let op = request["op"].as_str().unwrap();
            if op == "finish" {
                break;
            }
            if op == "restart" {
                let registry = Arc::new(JournalRegistry::default());
                registry.configure_directory(&directory).unwrap();
                restarted = Some(registry);
                respond(json!({"ok":true,"value":true}));
                continue;
            }
            let registry = restarted
                .clone()
                .unwrap_or_else(|| engine.recording_registry());
            let host_ref = if restarted.is_none() {
                Some(&engine)
            } else {
                None
            };
            let result = ubm_mobile::continuation::recording_control(
                &registry,
                host_ref,
                op,
                "peer-scope",
                request["token"].as_str().unwrap_or(""),
                u32::try_from(request["maxItems"].as_u64().unwrap_or(0)).unwrap(),
                u32::try_from(request["maxBytes"].as_u64().unwrap_or(0)).unwrap(),
            );
            respond(serde_json::from_str(&result).unwrap());
        }
        input.join().unwrap(); // TypeScript closes stdin with the finish command.
    } else {
        assert!(
            events
                .iter()
                .any(|row| row["t"] == "security" && row["peerId"] == OTHER)
        );
        let mut values = Vec::new();
        loop {
            let batch = engine.recording_prepare("peer-scope", 2, 65536).unwrap();
            let rows = batch["records"].as_array().unwrap();
            if rows.is_empty() {
                break;
            }
            for row in rows {
                let peer = row["record"].get("peerId").and_then(Value::as_str);
                assert!(
                    peer.is_none() || peer == Some(POLAR),
                    "foreign durable record: {row}"
                );
                if row["record"]["t"] == "value" {
                    values.push(row["record"]["valueB64"].clone());
                }
            }
            engine
                .recording_acknowledge("peer-scope", batch["token"].as_str().unwrap())
                .unwrap();
        }
        assert_eq!(
            values,
            json!(["AEY=", "AEc=", "AEg="]).as_array().unwrap().clone()
        );
    }
    let claim = engine.prepare_claim(256, 65536).await.unwrap();
    assert_eq!(
        engine
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
    ok(&call(&other, "session.dispose", "{}").await);
    host.shutdown().await;
    drop(restarted);
    drop(engine);
    drop(other);
    drop(host);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sustained_recording_intake_allows_second_peer_and_control_progress() {
    let directory = directory("drain-fairness");
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let other = host.open_session("fairness-other").unwrap();
    for (operation, id) in [
        ("connection.connect", "connect"),
        ("gatt.discover", "discover"),
    ] {
        ok(&call(
            &other,
            operation,
            &json!({"peerId":OTHER,"lease":"other","operationId":id}).to_string(),
        )
        .await);
    }
    ok(&call(&other,"gatt.subscribe",&json!({"peerId":OTHER,"selector":selector(),"consumer":"other-values","operationId":"other-subscribe"}).to_string()).await);
    let engine = host.continuation();
    engine.configure_recording_directory(&directory).unwrap();
    engine.execute(POLAR,&json!({"onAppearance":"native","resubscribe":[{
        "serviceUuid":HR_SERVICE,"serviceOccurrence":1,"characteristicUuid":HR_MEASUREMENT,"characteristicOccurrence":1
    }],"recording":{"id":"fairness","maxBytes":1048576,"maxRecords":1000}}).to_string()).await.unwrap();
    let epochs: std::collections::HashMap<String, u64> = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| match request {
            RadioRequest::EnableNotifications {
                epoch, instance, ..
            } => Some((instance.peer_id.clone(), *epoch)),
            _ => None,
        })
        .collect();
    let producer_host = host.clone();
    let epoch = epochs[POLAR];
    let producer = tokio::spawn(async move {
        for sequence in 0u16..250 {
            producer_host.ingest(RadioIngress::Notification {
                instance: Instance {
                    peer_id: POLAR.into(),
                    service_uuid: HR_SERVICE.into(),
                    service_occurrence: 0,
                    characteristic_uuid: HR_MEASUREMENT.into(),
                    characteristic_occurrence: 0,
                },
                epoch,
                value: sequence.to_le_bytes().to_vec(),
            });
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    });
    let mut worst = std::time::Duration::ZERO;
    for _ in 0..5 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        host.ingest(RadioIngress::Notification {
            instance: Instance {
                peer_id: OTHER.into(),
                service_uuid: HR_SERVICE.into(),
                service_occurrence: 0,
                characteristic_uuid: HR_MEASUREMENT.into(),
                characteristic_occurrence: 0,
            },
            epoch: epochs[OTHER],
            value: vec![42],
        });
        let started = std::time::Instant::now();
        host.ingest(security());
        let rows = drain_until(&other, |rows| {
            rows.iter().any(|row| row["t"] == "security")
                && rows.iter().any(|row| row["t"] == "value")
        })
        .await;
        assert!(rows.iter().any(|row| row["consumer"] == "other-values"));
        worst = worst.max(started.elapsed());
    }
    let started = std::time::Instant::now();
    host.ingest(RadioIngress::ServicesChanged {
        peer_id: OTHER.into(),
    });
    drain_until(&other, |rows| {
        rows.iter().any(|row| row["t"] == "db-changed")
            && rows.iter().any(|row| row["t"] == "stream-end")
    })
    .await;
    worst = worst.max(started.elapsed());
    producer.await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let reader = engine.clone();
            if ubm_desktop::continuation_journal::run_blocking(move || {
                reader.recording_status("fairness")
            })
            .await
            .unwrap()["records"]
                == 251
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let status = engine.recording_status("fairness").unwrap();
    let counters = ok(&call(&other, "counters.describe", "{}").await);
    assert_eq!(status["lostRecords"], 0);
    assert_eq!(
        counters["process"]["native"]["ingressDrops"]["notification"],
        0
    );
    assert!(
        worst < std::time::Duration::from_millis(500),
        "busy route delayed controls: {worst:?}"
    );
    let batch = engine.recording_prepare("fairness", 1000, 1048576).unwrap();
    let payloads: Vec<Value> = batch["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["record"]["t"] == "value")
        .map(|row| row["record"]["valueB64"].clone())
        .collect();
    let expected: Vec<Value> = (0u16..250)
        .map(|sequence| json!(ubm_mobile::wire::encode_base64(&sequence.to_le_bytes())))
        .collect();
    assert_eq!(
        payloads, expected,
        "all source values must remain ordered under load"
    );
    println!(
        "sustained intake:250 ordered records,5 lower-rate peer values,5 security controls,1 lifecycle transition; worst delay={worst:?}; journal/native notification loss=0; retained journal rows={}",
        status["records"]
    );
    let claim = engine.prepare_claim(256, 65536).await.unwrap();
    assert_eq!(
        engine
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
    ok(&call(&other, "session.dispose", "{}").await);
    host.shutdown().await;
    drop(engine);
    drop(other);
    drop(host);
    std::fs::remove_dir_all(directory).unwrap();
}
