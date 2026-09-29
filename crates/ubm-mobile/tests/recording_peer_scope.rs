//! Real mobile-host fixture driven by the TypeScript recording controller.
//! stdin/stdout are test transport only; every prepare/ACK reaches SQLite.
mod common;
#[path = "../../test-support/recording_fixture.rs"]
mod recording_fixture;

use common::*;
use recording_fixture::{complete_fixture_process, isolated_fixture_process};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use ubm_desktop::continuation_journal::JournalRegistry;
use ubm_mobile::{
    AuthenticationState, BondState, EncryptionState, HostOptions, Instance, MobileHost,
    MobilePlatform, MobileSession, RadioIngress, RadioRequest, SecureConnectionsState,
    SecurityState, WakeSink,
};

const OTHER: &str = "A0:9E:1A:00:00:02";

#[derive(Default)]
struct EventWakes(Mutex<std::collections::HashMap<u64, Arc<tokio::sync::Notify>>>);

impl EventWakes {
    fn signal(&self, id: u64) -> Arc<tokio::sync::Notify> {
        self.0.lock().unwrap().entry(id).or_default().clone()
    }
}
impl WakeSink for EventWakes {
    fn wake(&self, id: u64) {
        self.signal(id).notify_one();
    }
}

async fn open_events(
    radio: &Arc<Scripted>,
    platform: MobilePlatform,
) -> (Arc<MobileHost>, Arc<EventWakes>) {
    let wakes = Arc::new(EventWakes::default());
    let host = Arc::new(
        MobileHost::open(
            radio.clone(),
            wakes.clone(),
            HostOptions {
                platform,
                owner: "recording-test".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    (host, wakes)
}

async fn drain_events(
    session: &MobileSession,
    wakes: &EventWakes,
    done: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let signal = wakes.signal(session.id());
    let mut rows = Vec::new();
    loop {
        let wake = signal.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let batch = parse(&session.drain(256, 65536));
        rows.extend(batch["records"].as_array().unwrap().clone());
        if done(&rows) {
            return rows;
        }
        if batch["more"] != true {
            wake.await;
        }
    }
}

fn setup_radio() -> (Arc<Scripted>, tokio::sync::oneshot::Receiver<()>) {
    let (ready, observed) = tokio::sync::oneshot::channel();
    let mut ready = Some(ready);
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            services[0].characteristics[0].properties.write = true;
            Reply::Now(ubm_mobile::RadioCompletion::Discovered(services))
        }
        RadioRequest::Write { .. } => {
            ready.take().unwrap().send(()).unwrap();
            Reply::Now(ubm_mobile::RadioCompletion::Unit)
        }
        _ => polar_responder(request),
    }));
    (radio, observed)
}

fn declaration(id: &str) -> String {
    let selector = json!({"serviceUuid":HR_SERVICE,"serviceOccurrence":1,
        "characteristicUuid":HR_MEASUREMENT,"characteristicOccurrence":1});
    json!({"onAppearance":"native","resubscribe":[selector],
        "setup":[{"selector":selector,"value":[99],"timeoutMs":20000,
            "response":{"subscriptionIndex":0,"prefix":[255],"minLength":2,"maxLength":2,
                "status":{"offset":1,"accepted":[0]}}}],
        "recording":{"id":id,"maxBytes":1048576,"maxRecords":1000}})
    .to_string()
}

fn commit_response(host: &MobileHost, epoch: u64) {
    // The native setup matcher resolves only after outbox persistence succeeds.
    // Radio FIFO places this unique response after all source telemetry; await
    // execute's result to establish completed intake without polling SQLite.
    host.ingest(RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.into(),
            service_uuid: HR_SERVICE.into(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.into(),
            characteristic_occurrence: 0,
        },
        epoch,
        value: vec![255, 0],
    });
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
    let Some(directory) = isolated_fixture_process("mobile_recording_controller_transport") else {
        return;
    };
    let platform = match std::env::var("UBM_RECORDING_PLATFORM")
        .unwrap_or_else(|_| "android".into())
        .as_str()
    {
        "android" => MobilePlatform::Android,
        "apple" => MobilePlatform::Apple,
        _ => panic!("explicit mobile platform required"),
    };
    let (radio, ready) = setup_radio();
    let (host, wakes) = open_events(&radio, platform).await;
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
    let worker = engine.clone();
    let mut execution =
        tokio::spawn(async move { worker.execute(POLAR, &declaration("peer-scope")).await });
    tokio::select! { result=ready=>result.unwrap(), result=&mut execution=>panic!("native setup ended before write admission: {result:?}") }
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
    host.ingest(security());
    host.ingest(RadioIngress::ServicesChanged {
        peer_id: OTHER.into(),
    });
    let events = drain_events(&other, &wakes, |rows| {
        rows.iter().any(|row| row["t"] == "db-changed")
    })
    .await;
    host.ingest(RadioIngress::Connection {
        peer_id: OTHER.into(),
        connected: false,
        status: None,
    });
    let mut events = events;
    events.extend(
        drain_events(&other, &wakes, |rows| {
            rows.iter().any(|row| row["t"] == "link")
        })
        .await,
    );
    // A process-global adapter fact is eligible, without a fabricated peer.
    host.ingest(RadioIngress::AdapterState(adapter_on()));
    drain_events(&other, &wakes, |rows| {
        rows.iter().any(|row| row["t"] == "adapter")
    })
    .await;
    for counter in [71, 72] {
        notify(counter);
    }
    commit_response(&host, epoch);
    execution.await.unwrap().unwrap();
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
            json!(["AEY=", "AEc=", "AEg=", "/wA="])
                .as_array()
                .unwrap()
                .clone()
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
    complete_fixture_process(&directory);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sustained_recording_intake_allows_second_peer_and_control_progress() {
    let Some(directory) = isolated_fixture_process(
        "sustained_recording_intake_allows_second_peer_and_control_progress",
    ) else {
        return;
    };
    let (radio, ready) = setup_radio();
    let (host, wakes) = open_events(&radio, MobilePlatform::Android).await;
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
    let worker = engine.clone();
    let mut execution =
        tokio::spawn(async move { worker.execute(POLAR, &declaration("fairness")).await });
    tokio::select! { result=ready=>result.unwrap(), result=&mut execution=>panic!("native setup ended before write admission: {result:?}") }
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
        let rows = drain_events(&other, &wakes, |rows| {
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
    drain_events(&other, &wakes, |rows| {
        rows.iter().any(|row| row["t"] == "db-changed")
            && rows.iter().any(|row| row["t"] == "stream-end")
    })
    .await;
    worst = worst.max(started.elapsed());
    producer.await.unwrap();
    commit_response(&host, epoch);
    execution.await.unwrap().unwrap();
    let status = engine.recording_status("fairness").unwrap();
    let counters = ok(&call(&other, "counters.describe", "{}").await);
    assert_eq!(status["lostRecords"], 0);
    assert_eq!(
        counters["process"]["native"]["ingressDrops"]["notification"],
        0
    );
    assert_eq!(status["records"], 252);
    let batch = engine.recording_prepare("fairness", 1000, 1048576).unwrap();
    let payloads: Vec<Value> = batch["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["record"]["t"] == "value")
        .map(|row| row["record"]["valueB64"].clone())
        .collect();
    let mut expected: Vec<Value> = (0u16..250)
        .map(|sequence| json!(ubm_mobile::wire::encode_base64(&sequence.to_le_bytes())))
        .collect();
    expected.push(json!("/wA="));
    assert_eq!(
        payloads, expected,
        "all source values must remain ordered under load"
    );
    println!(
        "sustained intake:250 ordered telemetry values+1 setup response,5 lower-rate peer values,5 security controls,1 lifecycle transition; worst delay={worst:?}; journal/native notification loss=0; retained journal rows={}",
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
    complete_fixture_process(&directory);
}
