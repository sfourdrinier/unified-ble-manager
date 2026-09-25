//! Golden wire vectors: real `ubm-mobile-wire/1` text produced by the Rust
//! owner over the scripted radio, replayed by the TS parsers in
//! `__tests__/backends/reactnative/rust-core-wire.golden.test.js`.
//!
//! Check (default): regenerate in memory and fail when the committed file
//! differs. Regenerate: `UBM_MOBILE_GOLDEN_WRITE=1 cargo test -p ubm-mobile --test golden`.
//! Clock-valued fields (`observedAtMs`, `updatedAt`, `lastSeenAtMonotonicMs`)
//! are normalized to 0 so the file is reproducible; every other byte is the
//! owner's own output.

mod common;

use std::path::PathBuf;

use common::*;
use serde_json::{Value, json};
use ubm_mobile::{
    AdapterAuthorization, AdapterAvailability, AdapterPower, AdapterSnapshot, Advertisement,
    AuthenticationState, BondState, EncryptionState, IngressClass, Instance, ManufacturerData,
    MobilePlatform, RadioCompletion, RadioIngress, RadioRequest, RestoredPeer,
    SecureConnectionsState, SecurityState, ServiceData,
};

const CLOCK_FIELDS: &[&str] = &["observedAtMs", "updatedAt", "lastSeenAtMonotonicMs"];

fn normalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if CLOCK_FIELDS.contains(&key.as_str()) && entry.is_u64() {
                    *entry = Value::from(0u64);
                } else {
                    normalize(entry);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize),
        _ => {}
    }
}

fn normalized(text: &str) -> String {
    let mut value: Value = serde_json::from_str(text).expect("owner output is JSON");
    normalize(&mut value);
    value.to_string()
}

struct Recorder {
    invokes: Vec<Value>,
    drains: Vec<Value>,
    last_ordinal: Option<u64>,
}

impl Recorder {
    async fn invoke(
        &mut self,
        session: &ubm_mobile::MobileSession,
        name: &str,
        op: &str,
        args: Value,
    ) {
        let args_text = admitted_args(session, op, &args.to_string());
        let envelope = session.call(op, &args_text).await;
        let kind = if parse(&envelope)["ok"] == true {
            "value"
        } else {
            "failure"
        };
        self.invokes.push(json!({
            "name": name,
            "op": op,
            "args": args_text,
            "expect": kind,
            "envelope": normalized(&envelope),
        }));
    }

    /// Accumulate drained records until `want` arrived, then record them
    /// as one batch (ordinals stay strictly increasing across batches).
    /// The golden flow is loss-free: the cumulative control-loss counter
    /// must read zero, so the vectors pin the field without loss.
    async fn drain(&mut self, session: &ubm_mobile::MobileSession, name: &str, want: usize) {
        eprintln!("golden wire: waiting for {name}");
        let records = drain_until(session, |records| records.len() >= want).await;
        assert_eq!(records.len(), want, "{name}: {records:#?}");
        // Synchronous stretch: no task can interleave, so this drain
        // takes nothing and only reads the cumulative counter.
        let batch = parse(&session.drain(256, 65536));
        assert_eq!(batch["records"], json!([]), "{name}: nothing left behind");
        assert_eq!(
            batch["controlLost"],
            json!(0u64),
            "{name}: golden flow loses nothing"
        );
        let text = json!({"more": false, "records": records, "controlLost": 0}).to_string();
        self.drains.push(json!({
            "name": name,
            "lastOrdinal": self.last_ordinal,
            "text": normalized(&text),
        }));
        self.last_ordinal = records.last().and_then(|record| record["ordinal"].as_u64());
    }
}

fn writable_services() -> Vec<ubm_desktop::ServiceSnapshot> {
    let mut services = polar_services();
    let properties = &mut services[0].characteristics[0].properties;
    properties.write = true;
    properties.write_without_response = true;
    properties.read = true;
    services
}

/// A peer whose link the platform cannot establish (Android GATT 133).
const UNREACHABLE_PEER: &str = "C0:FF:EE:00:01:33";

fn responder(request: &RadioRequest) -> Reply {
    Reply::Now(match request {
        RadioRequest::Discover { .. } => RadioCompletion::Discovered(writable_services()),
        RadioRequest::Connect { peer_id, .. } if peer_id == UNREACHABLE_PEER => {
            RadioCompletion::Failed(ubm_mobile::PlatformFailure {
                gatt_status: Some(133),
                ..ubm_mobile::PlatformFailure::new(
                    ubm_mobile::FailureKind::GattStatus,
                    "Android GATT connection failed with status 133",
                )
            })
        }
        RadioRequest::Write { value, .. } if value == &[0xdd] => {
            RadioCompletion::Failed(ubm_mobile::PlatformFailure {
                gatt_status: Some(5),
                ..ubm_mobile::PlatformFailure::new(
                    ubm_mobile::FailureKind::GattStatus,
                    "GATT_INSUFFICIENT_AUTHENTICATION",
                )
            })
        }
        RadioRequest::Write { value, .. } if value == &[0xee] => {
            RadioCompletion::Failed(ubm_mobile::PlatformFailure::not_dispatched(
                ubm_mobile::FailureKind::Busy,
                "write-without-response queue full",
            ))
        }
        RadioRequest::ReadPhy { .. } => RadioCompletion::Phy(ubm_mobile::PhyObservation {
            tx: ubm_mobile::Phy::Le2m,
            rx: ubm_mobile::Phy::Le1m,
        }),
        RadioRequest::RequestPhy { .. } => RadioCompletion::PhyRequest {
            accepted: true,
            observation: Some(ubm_mobile::PhyObservation {
                tx: ubm_mobile::Phy::Le2m,
                rx: ubm_mobile::Phy::Le2m,
            }),
        },
        RadioRequest::RequestConnectionPriority { .. } => RadioCompletion::Accepted(true),
        RadioRequest::SecurityState { .. } => RadioCompletion::Security(SecurityState {
            bond: BondState::NotBonded,
            encryption: EncryptionState::NotEncrypted,
            authentication: AuthenticationState::Unknown,
            secure_connections: SecureConnectionsState::Unknown,
            pairing_possible: Some(true),
        }),
        RadioRequest::CreateBond { .. } => RadioCompletion::Security(SecurityState {
            bond: BondState::Bonded,
            encryption: EncryptionState::Encrypted,
            authentication: AuthenticationState::Unauthenticated,
            secure_connections: SecureConnectionsState::Yes,
            pairing_possible: None,
        }),
        RadioRequest::BondedPeers { .. } => {
            RadioCompletion::BondedPeers(vec![ubm_mobile::BondedPeer {
                peer_id: "C0:FF:EE:00:00:02".to_owned(),
                name: Some("Bonded Sensor".to_owned()),
            }])
        }
        RadioRequest::AcquireBackground { .. } => RadioCompletion::Lease("fgs-lease-1".to_owned()),
        RadioRequest::ObservePresence { .. } | RadioRequest::StopPresence { .. } => {
            RadioCompletion::Unit
        }
        RadioRequest::AssociateCompanion { .. } => RadioCompletion::Companion {
            association_id: 42,
            peer_id: Some(POLAR.to_owned()),
            display_name: Some("Polar H10".to_owned()),
            already_associated: false,
        },
        RadioRequest::ListCompanion { .. } => {
            RadioCompletion::CompanionList(vec![ubm_mobile::CompanionRecord {
                association_id: 42,
                peer_id: Some(POLAR.to_owned()),
                display_name: Some("Polar H10".to_owned()),
            }])
        }
        RadioRequest::DisassociateCompanion { .. } => RadioCompletion::Unit,
        other => match polar_responder(other) {
            Reply::Now(completion) => completion,
            Reply::Hold => unreachable!("polar responder never holds"),
        },
    })
}

fn hr(epoch: u64, value: &[u8]) -> RadioIngress {
    RadioIngress::Notification {
        instance: Instance {
            peer_id: POLAR.to_owned(),
            service_uuid: HR_SERVICE.to_owned(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.to_owned(),
            characteristic_occurrence: 0,
        },
        epoch,
        value: value.to_vec(),
    }
}

async fn generate() -> String {
    let radio = Scripted::new(Box::new(responder));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("golden").expect("session");
    let mut r = Recorder {
        invokes: Vec::new(),
        drains: Vec::new(),
        last_ordinal: None,
    };
    let sel = selector();
    let peer = POLAR;

    r.invoke(&session, "adapter state", "adapter.state", json!({}))
        .await;
    r.invoke(
        &session,
        "counters at baseline",
        "counters.describe",
        json!({}),
    )
    .await;
    r.invoke(&session, "scan start", "scan.start",
        json!({"serviceUuids": ["180D"], "duplicatePolicy": "all", "operationId": "scan-1", "budgetMs": 30000})).await;
    host.ingest(RadioIngress::Advertisement(Advertisement {
        peer_id: peer.to_owned(),
        address: Some(peer.to_owned()),
        local_name: Some("Polar H10 1234".to_owned()),
        rssi: Some(-58),
        tx_power_level: Some(4),
        service_uuids: vec!["180D".to_owned()],
        manufacturer_data: vec![ManufacturerData {
            company_id: 0x006b,
            payload: vec![0x00, 0x80, 0xff],
        }],
        service_data: vec![ServiceData {
            uuid: "180D".to_owned(),
            payload: Vec::new(),
        }],
        connectable: Some(true),
        solicited_service_uuids: Some(vec!["1812".to_owned()]),
        overflow_service_uuids: Some(Vec::new()),
        appearance: Some(0x0341),
        raw_record: Some(vec![0x02, 0x01, 0x06]),
    }));
    host.ingest(RadioIngress::Advertisement(Advertisement {
        peer_id: peer.to_owned(),
        address: None,
        local_name: None,
        rssi: None,
        tx_power_level: None,
        service_uuids: vec!["180D".to_owned()],
        manufacturer_data: Vec::new(),
        service_data: Vec::new(),
        ..Advertisement::default()
    }));
    r.drain(&session, "advertisements", 2).await;
    r.invoke(&session, "known peers", "peers.known", json!({}))
        .await;
    r.invoke(&session, "resolve known", "peers.resolve",
        json!({"reference": {"version": 1, "backendId": "rn", "scope": "origin", "opaqueId": peer}})).await;
    r.invoke(
        &session,
        "resolve unknown",
        "peers.resolve",
        json!({"reference": {"opaqueId": "nobody"}}),
    )
    .await;
    r.invoke(&session, "connect", "connection.connect",
        json!({"peerId": peer, "lease": "lease-1", "operationId": "connect-1", "budgetMs": 10000, "intent": "direct", "transport": "auto", "preferredPhy": ["le-2m", "le-1m"]})).await;
    r.invoke(
        &session,
        "connect link not established (android gatt 133)",
        "connection.connect",
        json!({"peerId": UNREACHABLE_PEER, "lease": "lease-9", "operationId": "connect-9"}),
    )
    .await;
    r.invoke(&session, "connect phy with when-available", "connection.connect",
        json!({"peerId": peer, "lease": "lease-2", "operationId": "connect-2", "intent": "when-available", "preferredPhy": ["le-coded"]})).await;
    r.invoke(&session, "connected peers", "peers.connected", json!({}))
        .await;
    r.invoke(
        &session,
        "discover",
        "gatt.discover",
        json!({"peerId": peer, "lease": "lease-1", "operationId": "discover-1"}),
    )
    .await;
    r.invoke(
        &session,
        "read",
        "gatt.read",
        json!({"peerId": peer, "selector": sel, "operationId": "read-1"}),
    )
    .await;
    let mut descriptor = sel.clone();
    descriptor["descriptorUuid"] = json!("2902");
    descriptor["descriptorOccurrence"] = json!(0);
    r.invoke(
        &session,
        "read descriptor",
        "gatt.read-descriptor",
        json!({"peerId": peer, "selector": descriptor, "operationId": "rd-1"}),
    )
    .await;
    r.invoke(&session, "write with response", "gatt.write",
        json!({"peerId": peer, "selector": sel, "valueB64": "AID/", "mode": "with-response", "operationId": "w-1"})).await;
    r.invoke(&session, "write without response", "gatt.write",
        json!({"peerId": peer, "selector": sel, "valueB64": "", "mode": "without-response", "operationId": "w-2"})).await;
    r.invoke(&session, "write refused by the peer (android gatt status)", "gatt.write",
        json!({"peerId": peer, "selector": sel, "valueB64": "3Q==", "mode": "with-response", "operationId": "w-5"})).await;
    r.invoke(&session, "write descriptor", "gatt.write-descriptor",
        json!({"peerId": peer, "selector": descriptor, "valueB64": "AQA=", "mode": "with-response", "operationId": "wd-1"})).await;
    r.invoke(
        &session,
        "write refused before sending",
        "gatt.write",
        json!({"peerId": peer, "selector": sel, "valueB64": "7g==", "mode": "without-response", "operationId": "w-4"}),
    )
    .await;
    r.invoke(&session, "write malformed base64", "gatt.write",
        json!({"peerId": peer, "selector": sel, "valueB64": "AID", "mode": "with-response", "operationId": "w-3"})).await;
    r.invoke(&session, "subscribe", "gatt.subscribe",
        json!({"peerId": peer, "selector": sel, "consumer": "hr", "deliveryMode": "require-notification", "operationId": "sub-1"})).await;
    let epoch = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|request| match request {
            RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .expect("enable issued");
    host.ingest(hr(epoch, &[0x00, 0x55]));
    host.ingest(hr(epoch, &[0x10, 0x55, 0x20, 0x03]));
    r.drain(&session, "notification values", 2).await;
    r.invoke(&session, "reconcile live", "session.reconcile", json!({}))
        .await;
    r.invoke(
        &session,
        "rssi",
        "connection.rssi",
        json!({"peerId": peer, "lease": "lease-1", "operationId": "rssi-1"}),
    )
    .await;
    r.invoke(
        &session,
        "effective mtu",
        "connection.effective-mtu",
        json!({"peerId": peer, "lease": "lease-1", "operationId": "mtu-obs-1"}),
    )
    .await;
    r.invoke(
        &session,
        "maximum write length",
        "connection.maximum-write-length",
        json!({"peerId": peer, "lease": "lease-1", "mode": "without-response", "operationId": "mwl-1"}),
    )
    .await;
    r.invoke(
        &session,
        "request mtu",
        "connection.request-mtu",
        json!({"peerId": peer, "lease": "lease-1", "mtu": 247, "operationId": "mtu-1"}),
    )
    .await;
    r.invoke(&session, "request priority", "connection.request-priority",
        json!({"peerId": peer, "lease": "lease-1", "priority": "high-throughput", "operationId": "prio-1"})).await;
    r.invoke(
        &session,
        "read phy",
        "connection.read-phy",
        json!({"peerId": peer, "lease": "lease-1", "operationId": "phy-1"}),
    )
    .await;
    r.invoke(&session, "request phy", "connection.request-phy",
        json!({"peerId": peer, "lease": "lease-1", "tx": "le-2m", "rx": "le-2m", "operationId": "phy-2"})).await;
    r.invoke(
        &session,
        "security state",
        "security.state",
        json!({"peerId": peer}),
    )
    .await;
    r.invoke(
        &session,
        "pair",
        "security.pair",
        json!({"peerId": peer, "transport": "auto", "operationId": "pair-1"}),
    )
    .await;
    r.invoke(
        &session,
        "cancel pairing",
        "security.cancel-pairing",
        json!({"peerId": peer}),
    )
    .await;
    r.invoke(
        &session,
        "bonded peers",
        "peers.bonded",
        json!({"operationId": "bonded-1"}),
    )
    .await;
    r.invoke(&session, "background acquire", "background.acquire",
        json!({"kind": "connected-device", "reason": "heart rate streaming", "operationId": "bg-1"})).await;
    r.invoke(
        &session,
        "background notification",
        "background.update-notification",
        json!({"leaseId": "fgs-lease-1", "title": "Recording", "body": "Polar H10"}),
    )
    .await;
    r.invoke(
        &session,
        "background release",
        "background.release",
        json!({"leaseId": "fgs-lease-1"}),
    )
    .await;
    r.invoke(
        &session,
        "companion associate",
        "companion.associate",
        json!({"name": "Polar", "serviceUuid": "180D"}),
    )
    .await;
    r.invoke(&session, "companion list", "companion.list", json!({}))
        .await;
    r.invoke(
        &session,
        "companion disassociate",
        "companion.disassociate",
        json!({"associationId": 42}),
    )
    .await;
    r.invoke(
        &session,
        "presence observe",
        "presence.observe",
        json!({"peerId": peer, "operationId": "presence-1"}),
    )
    .await;
    r.invoke(
        &session,
        "presence unobserve",
        "presence.unobserve",
        json!({"peerId": peer, "operationId": "presence-2"}),
    )
    .await;
    r.invoke(
        &session,
        "cancel before admission",
        "op.cancel",
        json!({"operationId": "never-seen"}),
    )
    .await;
    r.invoke(
        &session,
        "cancel finished",
        "op.cancel",
        json!({"operationId": "read-1"}),
    )
    .await;
    r.invoke(&session, "unknown op", "gatt.bogus", json!({}))
        .await;
    r.invoke(
        &session,
        "unsupported duplicate policy",
        "scan.start",
        json!({"serviceUuids": [], "duplicatePolicy": "merged", "operationId": "scan-2"}),
    )
    .await;

    host.ingest(RadioIngress::AdapterState(AdapterSnapshot {
        availability: AdapterAvailability::Available,
        authorization: AdapterAuthorization::Granted,
        power: AdapterPower::On,
        safe_reason: None,
    }));
    host.ingest(RadioIngress::SecurityChanged {
        peer_id: peer.to_owned(),
        state: SecurityState {
            bond: BondState::Bonded,
            encryption: EncryptionState::Encrypted,
            authentication: AuthenticationState::Unknown,
            secure_connections: SecureConnectionsState::Unsupported,
            pairing_possible: None,
        },
    });
    host.ingest(RadioIngress::Restored {
        peers: vec![RestoredPeer {
            peer_id: "C0:FF:EE:00:00:03".to_owned(),
            name: None,
            connected: false,
        }],
    });
    host.ingest(RadioIngress::Dropped {
        class: IngressClass::Notification,
        detail: "golden".to_owned(),
    });
    r.drain(&session, "adapter, security, restored, ingress-drop", 4)
        .await;
    r.invoke(&session, "restored peers", "peers.restored", json!({}))
        .await;
    r.invoke(
        &session,
        "claim restored on android",
        "peers.claim-restored",
        json!({"maxPeers": 1023}),
    )
    .await;

    host.ingest(RadioIngress::Connection {
        peer_id: peer.to_owned(),
        connected: false,
        status: Some(8),
    });
    r.drain(&session, "link loss", 2).await;
    r.invoke(
        &session,
        "reconcile after loss",
        "session.reconcile",
        json!({}),
    )
    .await;
    r.invoke(
        &session,
        "unsubscribe",
        "gatt.unsubscribe",
        json!({"peerId": peer, "selector": sel, "consumer": "hr", "operationId": "unsub-1"}),
    )
    .await;
    r.invoke(
        &session,
        "scan stop",
        "scan.stop",
        json!({"operationId": "s1-scan-1"}),
    )
    .await;
    r.invoke(
        &session,
        "disconnect after loss",
        "connection.disconnect",
        json!({"peerId": peer, "lease": "lease-1"}),
    )
    .await;
    r.invoke(&session, "dispose", "session.dispose", json!({}))
        .await;

    // Apple: this golden flow observes a live restoration event. Open the
    // claimant before ingress; pre-session restoration is exercised through
    // the durable peers.restored/claim path in the session tests. This host's
    // records are not included in the Android ordinal chain.
    let apple_radio = Scripted::new(Box::new(responder));
    let (apple, _) = open(&apple_radio, MobilePlatform::Apple).await;
    let claimant = apple.open_session("golden-apple").expect("session");
    apple.ingest(RadioIngress::Restored {
        peers: vec![RestoredPeer {
            peer_id: "5B7C1A2E-0000-4000-8000-000000000001".to_owned(),
            name: Some("Polar H10".to_owned()),
            connected: true,
        }],
    });
    eprintln!("golden wire: waiting for Apple restoration");
    drain_until(&claimant, |records| {
        records.iter().any(|record| record["t"] == "restored")
    })
    .await;
    r.invoke(
        &claimant,
        "claim restored",
        "peers.claim-restored",
        json!({"maxPeers": 1023}),
    )
    .await;
    r.invoke(
        &claimant,
        "claim restored again",
        "peers.claim-restored",
        json!({"maxPeers": 1023}),
    )
    .await;
    r.invoke(
        &claimant,
        "counters after claim",
        "counters.describe",
        json!({}),
    )
    .await;
    r.invoke(&claimant, "apple connect phy", "connection.connect",
        json!({"peerId": "5B7C1A2E-0000-4000-8000-000000000001", "lease": "l", "operationId": "c", "preferredPhy": ["le-2m"]})).await;

    let document = json!({
        "wireRevision": ubm_mobile::WIRE_REVISION,
        "regenerate": "UBM_MOBILE_GOLDEN_WRITE=1 cargo test -p ubm-mobile --test golden",
        "invokes": r.invokes,
        "drains": r.drains,
    });
    let mut text = serde_json::to_string_pretty(&document).expect("serializes");
    text.push('\n');
    text
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn golden_wire_vectors_are_current() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("golden/wire-vectors.json");
    let fresh = generate().await;
    // Both assertions use the same owner output; a second full run only
    // duplicates the asynchronous scenario and its setup cost.
    let lower = fresh.to_ascii_lowercase();
    let leaks: Vec<&str> = lower
        .lines()
        .filter(|line| line.contains("desktop"))
        .collect();
    assert!(
        leaks.is_empty(),
        "desktop names on the mobile wire: {leaks:#?}"
    );
    if std::env::var_os("UBM_MOBILE_GOLDEN_WRITE").is_some() {
        std::fs::write(&path, &fresh).expect("writes golden vectors");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == fresh,
        "golden wire vectors drifted; regenerate with UBM_MOBILE_GOLDEN_WRITE=1 cargo test -p ubm-mobile --test golden"
    );
}
