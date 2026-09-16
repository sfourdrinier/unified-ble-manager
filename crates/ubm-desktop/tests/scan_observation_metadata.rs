//! F22: the native discovery path preserves the complete matcher fact set.
//!
//! Replays the shared vendor advertisement fixtures
//! (`fixtures/vendor_advertisements.json`, mirrored in bun-mono for the real
//! `advertisementMatchesKind`) through the production native path —
//! [`FakeRadio`] advertisement events into [`DesktopCentral`] — and asserts
//! every fact the first-consumer matcher consumes survives exactly:
//! `localName` (null vs empty vs value), `serviceUuids` (exact strings),
//! `manufacturerData` (company IDs plus payload bytes verbatim), and `rssi`
//! (null vs value). Match expectations mirror the consumer predicate,
//! including the Oura safety rule (a name-only Oura advertisement must NOT
//! match); the authoritative matcher verdicts run against the real matcher
//! in bun-mono (`nativeScanObservation.test.ts`).

use std::time::Duration;

use serde_json::Value;
use ubm_desktop::{DesktopCentral, FakeRadio, PeerSnapshot, RadioEvent};

const FIXTURES: &str = include_str!("fixtures/vendor_advertisements.json");

// Canonical discovery hints, mirroring bun-mono
// `packages/sharedCore/src/bleDevices/{session,vendors/*}/discovery.ts`
// (UUIDs in canonical 128-bit lowercase; the real matcher canonicalizes
// both sides before comparing).
struct VendorHints {
    kind: &'static str,
    names: &'static [&'static str],
    services: &'static [&'static str],
    manufacturers: &'static [u16],
}

const HINTS: &[VendorHints] = &[
    VendorHints {
        kind: "polar",
        names: &["Polar"],
        services: &["0000180d-0000-1000-8000-00805f9b34fb"],
        manufacturers: &[107],
    },
    VendorHints {
        kind: "movesense",
        names: &["Movesense", "MS", "MoveSense"],
        services: &["34802252-7185-4d5d-b431-630e7050e8f0"],
        manufacturers: &[],
    },
    VendorHints {
        kind: "genericHrs",
        names: &[],
        services: &["0000180d-0000-1000-8000-00805f9b34fb"],
        manufacturers: &[],
    },
    VendorHints {
        kind: "nexring",
        names: &["SR", "SR09", "SR23", "SR28"],
        services: &["00001822-0000-1000-8000-00805f9b34fb"],
        manufacturers: &[],
    },
    VendorHints {
        kind: "oura",
        names: &["Oura"],
        services: &["98ed0001-a541-11e4-b6a0-0002a5d5c51b"],
        manufacturers: &[690],
    },
];

/// Canonicalize one observed service UUID the way the consumer matcher
/// does: strip `0x`, lowercase, expand 16-bit forms to the BLE base UUID.
fn canonical_service(uuid: &str) -> String {
    let stripped = uuid.strip_prefix("0x").unwrap_or(uuid).to_lowercase();
    if stripped.len() == 4 && stripped.chars().all(|c| c.is_ascii_hexdigit()) {
        return format!("0000{stripped}-0000-1000-8000-00805f9b34fb");
    }
    if stripped.len() == 8 && stripped.chars().all(|c| c.is_ascii_hexdigit()) {
        return format!("{stripped}-0000-1000-8000-00805f9b34fb");
    }
    stripped
}

/// Mirror of `advertisementMatchesKind`: name prefixes (never for Oura),
/// then service UUIDs, then manufacturer company IDs.
fn matches_kind(kind: &str, snapshot: &PeerSnapshot) -> bool {
    let hints = HINTS
        .iter()
        .find(|hints| hints.kind == kind)
        .expect("known kind");
    if kind != "oura"
        && let Some(local_name) = snapshot.local_name.as_deref()
        && hints
            .names
            .iter()
            .any(|prefix| !prefix.is_empty() && local_name.starts_with(prefix))
    {
        return true;
    }
    if !hints.services.is_empty() && !snapshot.service_uuids.is_empty() {
        let observed: Vec<String> = snapshot
            .service_uuids
            .iter()
            .map(|uuid| canonical_service(uuid))
            .collect();
        if hints
            .services
            .iter()
            .any(|wanted| observed.iter().any(|uuid| uuid == wanted))
        {
            return true;
        }
    }
    if !hints.manufacturers.is_empty()
        && snapshot
            .manufacturer_data
            .iter()
            .any(|entry| hints.manufacturers.contains(&entry.company_id))
    {
        return true;
    }
    false
}

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "fixture hex must pair: {hex}");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("fixture hex digit"))
        .collect()
}

fn opt_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => panic!("fixture string-or-null, got {other}"),
    }
}

fn snapshot_from_fixture(event: &Value) -> PeerSnapshot {
    let manufacturer_data = event["manufacturerData"]
        .as_array()
        .expect("manufacturerData array")
        .iter()
        .map(|entry| ubm_desktop::ManufacturerData {
            company_id: entry["companyId"].as_u64().expect("companyId") as u16,
            payload: hex_to_bytes(entry["payloadHex"].as_str().expect("payloadHex")),
        })
        .collect();
    let service_data = event["serviceData"]
        .as_array()
        .expect("serviceData array")
        .iter()
        .map(|entry| ubm_desktop::ServiceData {
            uuid: entry["uuid"].as_str().expect("uuid").to_owned(),
            payload: hex_to_bytes(entry["payloadHex"].as_str().expect("payloadHex")),
        })
        .collect();
    PeerSnapshot {
        id: event["peerId"].as_str().expect("peerId").to_owned(),
        address: opt_string(&event["address"]),
        service_uuids: event["serviceUuids"]
            .as_array()
            .expect("serviceUuids array")
            .iter()
            .map(|uuid| uuid.as_str().expect("uuid string").to_owned())
            .collect(),
        rssi: event["rssi"].as_i64().map(|rssi| rssi as i16),
        local_name: opt_string(&event["localName"]),
        manufacturer_data,
        service_data,
        tx_power_level: event["txPowerLevel"].as_i64().map(|tx| tx as i16),
    }
}

fn assert_snapshot_matches_fixture(case: &str, event: &Value, snapshot: &PeerSnapshot) {
    assert_eq!(
        snapshot.id,
        event["peerId"].as_str().expect("peerId"),
        "{case}: peer id"
    );
    assert_eq!(
        snapshot.address,
        opt_string(&event["address"]),
        "{case}: address null-vs-value"
    );
    assert_eq!(
        snapshot.local_name,
        opt_string(&event["localName"]),
        "{case}: localName null-vs-empty-vs-value"
    );
    let expected_services: Vec<String> = event["serviceUuids"]
        .as_array()
        .expect("serviceUuids array")
        .iter()
        .map(|uuid| uuid.as_str().expect("uuid string").to_owned())
        .collect();
    assert_eq!(
        snapshot.service_uuids, expected_services,
        "{case}: serviceUuids exact"
    );
    let expected_manufacturer: Vec<(u16, Vec<u8>)> = event["manufacturerData"]
        .as_array()
        .expect("manufacturerData array")
        .iter()
        .map(|entry| {
            (
                entry["companyId"].as_u64().expect("companyId") as u16,
                hex_to_bytes(entry["payloadHex"].as_str().expect("payloadHex")),
            )
        })
        .collect();
    let observed_manufacturer: Vec<(u16, Vec<u8>)> = snapshot
        .manufacturer_data
        .iter()
        .map(|entry| (entry.company_id, entry.payload.clone()))
        .collect();
    assert_eq!(
        observed_manufacturer, expected_manufacturer,
        "{case}: manufacturerData company IDs plus payload bytes verbatim"
    );
    assert_eq!(
        snapshot.rssi,
        event["rssi"].as_i64().map(|rssi| rssi as i16),
        "{case}: rssi null-vs-value"
    );
    let expected_service: Vec<(String, Vec<u8>)> = event["serviceData"]
        .as_array()
        .expect("serviceData array")
        .iter()
        .map(|entry| {
            (
                entry["uuid"].as_str().expect("uuid").to_owned(),
                hex_to_bytes(entry["payloadHex"].as_str().expect("payloadHex")),
            )
        })
        .collect();
    let observed_service: Vec<(String, Vec<u8>)> = snapshot
        .service_data
        .iter()
        .map(|entry| (entry.uuid.clone(), entry.payload.clone()))
        .collect();
    assert_eq!(
        observed_service, expected_service,
        "{case}: serviceData UUIDs plus payload bytes verbatim"
    );
    assert_eq!(
        snapshot.tx_power_level,
        event["txPowerLevel"].as_i64().map(|tx| tx as i16),
        "{case}: txPowerLevel null-vs-value"
    );
}

async fn take_one(central: &DesktopCentral<FakeRadio>, case: &str) -> PeerSnapshot {
    for _ in 0..200 {
        if let Some(snapshot) = central.take_advertisement().await {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("{case}: timed out waiting for a native scan observation");
}

async fn wait_peer(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
    for _ in 0..200 {
        if central.peer_key_for(peer_id).await.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for peer {peer_id}");
}

#[tokio::test]
async fn replay_vendor_advertisements_preserve_matcher_facts() {
    let fixtures: Value = serde_json::from_str(FIXTURES).expect("fixtures parse");
    assert_eq!(fixtures["revision"], 1, "fixture revision");
    let cases = fixtures["cases"].as_array().expect("cases array");
    assert!(!cases.is_empty(), "fixtures must carry cases");

    // Live-empty first: no observation before any advertisement.
    let central = DesktopCentral::open(FakeRadio::new(), "f22-host")
        .await
        .expect("open central");
    central
        .start_scan("owner-f22", &[], 60_000)
        .await
        .expect("start scan");
    assert!(
        central.take_advertisement().await.is_none(),
        "no observation before any advertisement"
    );

    for case in cases {
        let name = case["name"].as_str().expect("case name");
        central
            .boundary()
            .push_event(RadioEvent::Advertisement(snapshot_from_fixture(case)));
        wait_peer(&central, case["peerId"].as_str().expect("peerId")).await;
        let snapshot = take_one(&central, name).await;
        assert_snapshot_matches_fixture(name, case, &snapshot);
        let expected = case["expectMatches"].as_object().expect("expectMatches");
        for hints in HINTS {
            let verdict = matches_kind(hints.kind, &snapshot);
            let wanted = expected[hints.kind].as_bool().expect("expected verdict");
            assert_eq!(
                verdict, wanted,
                "{name}: matcher verdict for {} (Oura must never match by name alone)",
                hints.kind,
            );
        }
    }
    central.shutdown().await;
}

#[tokio::test]
async fn scan_response_updates_arrive_in_order() {
    let fixtures: Value = serde_json::from_str(FIXTURES).expect("fixtures parse");
    let sequences = fixtures["sequences"].as_array().expect("sequences array");
    let sequence = sequences
        .iter()
        .find(|sequence| sequence["name"] == "scan-response-update")
        .expect("scan-response-update sequence");
    let events = sequence["events"].as_array().expect("sequence events");
    assert_eq!(events.len(), 2, "sequence carries two observations");

    let central = DesktopCentral::open(FakeRadio::new(), "f22-host")
        .await
        .expect("open central");
    central
        .start_scan("owner-f22", &[], 60_000)
        .await
        .expect("start scan");
    for event in events {
        central
            .boundary()
            .push_event(RadioEvent::Advertisement(snapshot_from_fixture(event)));
    }
    for (index, event) in events.iter().enumerate() {
        let snapshot = take_one(&central, "scan-response-update").await;
        assert_snapshot_matches_fixture(
            &format!("scan-response-update[{index}]"),
            event,
            &snapshot,
        );
    }
    central.shutdown().await;
}

#[tokio::test]
async fn advertisement_queue_is_bounded_with_explicit_overflow() {
    let central = DesktopCentral::open(FakeRadio::new(), "f22-host")
        .await
        .expect("open central");
    central
        .start_scan("owner-f22", &[], 60_000)
        .await
        .expect("start scan");
    // Pace pushes to loop speed (one peer resolution per push): the scripted
    // boundary's own control queue is 64 deep, so an unpaced 300-push burst
    // would drop at the boundary instead of exercising the central's 256 cap.
    for index in 0..300 {
        let peer_id = format!("f22-flood-{index}");
        central
            .boundary()
            .push_event(RadioEvent::Advertisement(PeerSnapshot {
                id: peer_id.clone(),
                address: None,
                service_uuids: Vec::new(),
                rssi: Some(-80),
                local_name: None,
                manufacturer_data: Vec::new(),
                service_data: Vec::new(),
                tx_power_level: None,
            }));
        wait_peer(&central, &peer_id).await;
    }
    let mut observed = 0;
    for _ in 0..600 {
        if central.take_advertisement().await.is_some() {
            observed += 1;
        } else {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if observed == 256 {
            break;
        }
    }
    assert_eq!(observed, 256, "bounded queue retains exactly its cap");
    assert_eq!(
        central.advertisement_overflow_count(),
        44,
        "every evicted observation is counted, never silent"
    );
    assert!(
        central.take_advertisement().await.is_none(),
        "no observation beyond the retained cap plus counted overflow"
    );
    central.shutdown().await;
}
