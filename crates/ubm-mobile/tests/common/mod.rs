//! ScriptedPlatformRadio: a deterministic platform adapter for the mobile
//! owner tests. Every request is recorded; a responder answers it at once
//! or holds it for the test to answer (or never answer) later.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;
use ubm_desktop::{
    CharacteristicSnapshot, DescriptorSnapshot, ObservedDelivery, PropertyFlags, ServiceSnapshot,
};
use ubm_mobile::{
    AdapterAuthorization, AdapterAvailability, AdapterPower, AdapterSnapshot, HostOptions,
    MobileHost, MobilePlatform, MobileSession, PlatformRadio, RadioCompletion, RadioRequest,
    RequestKind, WakeSink,
};

pub const HR_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
pub const HR_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";
pub const CCCD: &str = "00002902-0000-1000-8000-00805f9b34fb";
pub const POLAR: &str = "A0:9E:1A:00:00:01";

pub enum Reply {
    Now(RadioCompletion),
    Hold,
}

type Responder = Box<dyn FnMut(&RadioRequest) -> Reply + Send>;

pub struct Scripted {
    pub requests: Mutex<Vec<RadioRequest>>,
    pub cancels: Mutex<Vec<u64>>,
    pub held: Mutex<HashMap<u64, RadioRequest>>,
    host: OnceLock<MobileHost>,
    responder: Mutex<Responder>,
}

impl Scripted {
    pub fn new(responder: Responder) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            cancels: Mutex::new(Vec::new()),
            held: Mutex::new(HashMap::new()),
            host: OnceLock::new(),
            responder: Mutex::new(responder),
        })
    }

    pub fn polar() -> Arc<Self> {
        Self::new(Box::new(polar_responder))
    }

    pub fn set_responder(&self, responder: Responder) {
        *self.responder.lock().unwrap() = responder;
    }

    pub fn kinds(&self) -> Vec<RequestKind> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(RadioRequest::kind)
            .collect()
    }

    pub fn count(&self, kind: RequestKind) -> usize {
        self.kinds().into_iter().filter(|k| *k == kind).count()
    }

    pub fn held_of(&self, kind: RequestKind) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .held
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, request)| request.kind() == kind)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    pub fn answer(&self, id: u64, completion: RadioCompletion) -> ubm_mobile::CompletionStatus {
        self.held.lock().unwrap().remove(&id);
        self.host.get().unwrap().complete(id, completion)
    }
}

impl PlatformRadio for Scripted {
    fn submit(&self, request: RadioRequest) {
        self.requests.lock().unwrap().push(request.clone());
        let reply = (self.responder.lock().unwrap())(&request);
        match reply {
            Reply::Now(completion) => {
                self.host.get().unwrap().complete(request.id(), completion);
            }
            Reply::Hold => {
                self.held.lock().unwrap().insert(request.id(), request);
            }
        }
    }

    fn cancel(&self, request_id: u64) {
        self.cancels.lock().unwrap().push(request_id);
    }
}

pub fn adapter_on() -> AdapterSnapshot {
    AdapterSnapshot {
        availability: AdapterAvailability::Available,
        authorization: AdapterAuthorization::Granted,
        power: AdapterPower::On,
        safe_reason: None,
    }
}

pub fn polar_services() -> Vec<ServiceSnapshot> {
    vec![ServiceSnapshot {
        uuid: HR_SERVICE.to_owned(),
        occurrence: 0,
        characteristics: vec![CharacteristicSnapshot {
            uuid: HR_MEASUREMENT.to_owned(),
            occurrence: 0,
            properties: PropertyFlags {
                read: false,
                write: false,
                write_without_response: false,
                notify: true,
                indicate: false,
            },
            descriptors: vec![DescriptorSnapshot {
                uuid: CCCD.to_owned(),
                occurrence: 0,
            }],
        }],
    }]
}

pub fn polar_responder(request: &RadioRequest) -> Reply {
    Reply::Now(match request {
        RadioRequest::AdapterState { .. } => RadioCompletion::Adapter(adapter_on()),
        RadioRequest::Discover { .. } => RadioCompletion::Discovered(polar_services()),
        RadioRequest::Read { .. } => RadioCompletion::Read {
            value: vec![0x42],
            provenance: ubm_mobile::ReadProvenance::ReadResponse,
        },
        RadioRequest::ReadDescriptor { .. } => RadioCompletion::Bytes(vec![0x42]),
        RadioRequest::EnableNotifications { requested, .. } => {
            RadioCompletion::NotifyEnabled(match requested {
                Some(ubm_desktop::DeliveryMode::Indication) => ObservedDelivery::Indication,
                _ => ObservedDelivery::Notification,
            })
        }
        RadioRequest::ReadMtu { .. } => RadioCompletion::Mtu(Some(247)),
        RadioRequest::ReadWriteLimits { .. } => {
            RadioCompletion::WriteLimits(ubm_mobile::WriteLimits {
                with_response: 512,
                without_response: 244,
            })
        }
        RadioRequest::RequestMtu { mtu, .. } => RadioCompletion::Mtu(Some(*mtu)),
        RadioRequest::ReadRssi { .. } => RadioCompletion::Rssi(-61),
        RadioRequest::Close { .. } => RadioCompletion::Closed(Vec::new()),
        _ => RadioCompletion::Unit,
    })
}

#[derive(Default)]
pub struct Wakes {
    pub count: AtomicU64,
    pub per_session: Mutex<HashMap<u64, u64>>,
}

impl WakeSink for Wakes {
    fn wake(&self, session_id: u64) {
        self.count.fetch_add(1, Ordering::SeqCst);
        *self
            .per_session
            .lock()
            .unwrap()
            .entry(session_id)
            .or_default() += 1;
    }
}

pub async fn open(radio: &Arc<Scripted>, platform: MobilePlatform) -> (MobileHost, Arc<Wakes>) {
    let wakes = Arc::new(Wakes::default());
    let host = MobileHost::open(
        Arc::clone(radio) as Arc<dyn PlatformRadio>,
        Arc::clone(&wakes) as Arc<dyn WakeSink>,
        HostOptions {
            platform,
            owner: "ubm-mobile-test".to_owned(),
            adapter_label: "scripted-adapter".to_owned(),
        },
        tokio::runtime::Handle::current(),
    )
    .await
    .expect("host opens");
    assert!(radio.host.set(host.clone()).is_ok());
    (host, wakes)
}

pub fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("envelope is JSON")
}

/// The `value` of a success envelope; panics with the envelope otherwise.
pub fn ok(text: &str) -> Value {
    let envelope = parse(text);
    assert_eq!(
        envelope["ok"],
        Value::Bool(true),
        "expected success: {text}"
    );
    envelope["value"].clone()
}

/// The failure envelope (`error` + `commit`); panics on success.
pub fn failure(text: &str) -> (Value, Value) {
    let envelope = parse(text);
    assert_eq!(
        envelope["ok"],
        Value::Bool(false),
        "expected failure: {text}"
    );
    (envelope["error"].clone(), envelope["commit"].clone())
}

/// Drain until `done(all records so far)` holds, or fail after 3 s.
pub async fn drain_until(session: &MobileSession, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let mut records = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let batch = parse(&session.drain(256, 65536));
        records.extend(batch["records"].as_array().cloned().unwrap_or_default());
        if done(&records) {
            return records;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "drain condition not met; records: {records:#?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub fn of_type<'a>(records: &'a [Value], t: &str) -> Vec<&'a Value> {
    records.iter().filter(|r| r["t"] == t).collect()
}

pub fn selector() -> Value {
    serde_json::json!({
        "serviceUuid": "180D",
        "serviceOccurrence": 0,
        "characteristicUuid": "2A37",
        "characteristicOccurrence": 0
    })
}

/// What a wire client does (finding 109): every invoke that carries an
/// operation identity gets the session's next `admission`, assigned in send
/// order (allocation and the synchronous `invoke` happen under one lock);
/// `op.cancel` names its target's admission. A cancel for an id not invoked
/// yet reserves the admission that id's invoke will then carry. Args that
/// already carry an `admission` pass unchanged; `scan.stop`'s `operationId`
/// names a scan membership, not an operation.
pub async fn call(session: &MobileSession, op: &str, args: &str) -> String {
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let mut clients = clients().lock().unwrap();
        let text = with_admission(&mut clients, session, op, args);
        session.invoke(
            op,
            &text,
            Box::new(move |envelope| {
                let _ = tx.send(envelope);
            }),
        );
    }
    rx.await.expect("the owner answers every invoke")
}

/// The args text [`call`] would send (sequential callers only).
pub fn admitted_args(session: &MobileSession, op: &str, args: &str) -> String {
    with_admission(&mut clients().lock().unwrap(), session, op, args)
}

type Clients = HashMap<usize, (u64, HashMap<String, u64>)>;

fn clients() -> &'static Mutex<Clients> {
    static CLIENTS: OnceLock<Mutex<Clients>> = OnceLock::new();
    CLIENTS.get_or_init(Default::default)
}

fn with_admission(clients: &mut Clients, session: &MobileSession, op: &str, args: &str) -> String {
    let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(args) else {
        return args.to_owned();
    };
    if op == "scan.stop" || map.contains_key("admission") {
        return args.to_owned();
    }
    let Some(id) = map
        .get("operationId")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return args.to_owned();
    };
    let (next, admissions) = clients.entry(session.instance_key()).or_default();
    let admission = *admissions.entry(id).or_insert_with(|| {
        *next += 1;
        *next
    });
    map.insert("admission".to_owned(), Value::from(admission));
    Value::Object(map).to_string()
}
