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
use ubm_desktop::OpControl;
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
        extras: ubm_desktop::AdvertisementExtras::default(),
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
        .start_scan("owner-f22", &[], OpControl::budget_ms(60_000))
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
        .start_scan("owner-f22", &[], OpControl::budget_ms(60_000))
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

/// F22 plus the finding-107 audit: every advertisement stays pollable until
/// the host takes it, up to the public stream maximum (legacy CoreBluetooth
/// queued every advertisement for the public scan stream without a bound
/// below the caller's policy), so 300 observations the host has not polled
/// yet are all retained and nothing is evicted.
#[tokio::test]
async fn advertisement_queue_retains_a_burst_the_host_has_not_polled() {
    let central = DesktopCentral::open(FakeRadio::new(), "f22-host")
        .await
        .expect("open central");
    central
        .start_scan("owner-f22", &[], OpControl::budget_ms(60_000))
        .await
        .expect("start scan");
    // Pace pushes to loop speed (one peer resolution per push): the scripted
    // boundary's own control queue is 64 deep.
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
                extras: ubm_desktop::AdvertisementExtras::default(),
            }));
        wait_peer(&central, &peer_id).await;
    }
    let mut observed = Vec::new();
    while let Some(observation) = central.take_advertisement().await {
        observed.push(observation.id);
    }
    assert_eq!(observed.len(), 300, "every observation retained");
    assert_eq!(observed[0], "f22-flood-0", "FIFO from the first");
    assert_eq!(central.advertisement_overflow_count(), 0, "nothing evicted");
    central.shutdown().await;
}

fn sighting(peer_id: &str, name: &str) -> RadioEvent {
    RadioEvent::Advertisement(PeerSnapshot {
        id: peer_id.to_owned(),
        address: None,
        service_uuids: Vec::new(),
        rssi: Some(-70),
        local_name: Some(name.to_owned()),
        manufacturer_data: Vec::new(),
        service_data: Vec::new(),
        tx_power_level: None,
        extras: ubm_desktop::AdvertisementExtras::default(),
    })
}

/// Finding 121: sightings are observations only while a scan runs, and
/// each belongs to the scan that was live when it arrived. A sighting from
/// before any scan, or from an earlier scan, is never delivered as a fresh
/// observation of a later scan (a find-by-name must not match a device
/// that is gone), and each observation carries its scan and its age.
#[tokio::test]
async fn sightings_belong_to_the_scan_that_was_live() {
    let central = DesktopCentral::open(FakeRadio::new(), "f121-host")
        .await
        .expect("open central");
    central
        .boundary()
        .push_event(sighting("before", "Pre-scan"));
    wait_peer(&central, "before").await;
    let first = central
        .start_scan("owner", &[], OpControl::budget_ms(5000))
        .await
        .expect("first scan");
    assert!(
        central.take_scan_observation().await.is_none(),
        "a sighting from before the scan is not an observation"
    );
    central
        .boundary()
        .push_event(sighting("during-first", "First"));
    wait_peer(&central, "during-first").await;
    central
        .boundary()
        .push_event(sighting("tail-first", "Tail"));
    wait_peer(&central, "tail-first").await;
    let taken = central
        .take_scan_observation()
        .await
        .expect("first scan's sighting");
    assert_eq!(taken.snapshot.id, "during-first");
    assert_eq!(&taken.scan_operation_id, first.operation_id());
    assert!(
        taken.age < Duration::from_secs(5),
        "age measured from receipt"
    );
    central
        .stop_scan(first.operation_id(), OpControl::budget_ms(5000))
        .await
        .expect("stop");
    central
        .boundary()
        .push_event(sighting("between", "Between"));
    wait_peer(&central, "between").await;
    let second = central
        .start_scan("owner", &[], OpControl::budget_ms(5000))
        .await
        .expect("second scan");
    assert!(
        central.take_scan_observation().await.is_none(),
        "neither the first scan's tail nor an unscanned sighting reaches the second scan"
    );
    central
        .boundary()
        .push_event(sighting("during-second", "Second"));
    wait_peer(&central, "during-second").await;
    let taken = central
        .take_scan_observation()
        .await
        .expect("second scan's sighting");
    assert_eq!(taken.snapshot.id, "during-second");
    assert_eq!(&taken.scan_operation_id, second.operation_id());
    central.shutdown().await;
}

/// Finding 120 (Tauri cadence): a host that re-read every known peripheral
/// during a scan (Tauri 4.x, every 2 s) keeps that cadence: each known
/// peer is observed again every period while a scan runs, labelled as the
/// OS's device state, and never outside a scan.
#[tokio::test(start_paused = true)]
async fn known_peers_are_re_observed_on_the_configured_cadence() {
    let central = DesktopCentral::open(FakeRadio::new(), "f120-host")
        .await
        .expect("open central");
    central.boundary().set_peers(vec![PeerSnapshot {
        id: "known".to_owned(),
        address: None,
        service_uuids: Vec::new(),
        rssi: Some(-66),
        local_name: Some("Known".to_owned()),
        manufacturer_data: Vec::new(),
        service_data: Vec::new(),
        tx_power_level: None,
        extras: ubm_desktop::AdvertisementExtras::default(),
    }]);
    central.set_known_peer_refresh(Some(Duration::from_secs(2)));
    tokio::time::sleep(Duration::from_secs(5)).await;
    let scan = central
        .start_scan("owner", &[], OpControl::budget_ms(5000))
        .await
        .expect("scan");
    assert!(
        central.take_scan_observation().await.is_none(),
        "no re-read outside a scan reaches it"
    );
    for round in 0..3 {
        tokio::time::sleep(Duration::from_millis(2_100)).await;
        let observation = central
            .take_scan_observation()
            .await
            .unwrap_or_else(|| panic!("re-read {round}"));
        assert_eq!(observation.snapshot.id, "known");
        assert_eq!(&observation.scan_operation_id, scan.operation_id());
        assert_eq!(
            observation.snapshot.extras.source,
            ubm_desktop::ObservationSource::DeviceState
        );
        assert!(
            central.take_scan_observation().await.is_none(),
            "one per period"
        );
    }
    central
        .stop_scan(scan.operation_id(), OpControl::budget_ms(5000))
        .await
        .expect("stop");
    central.shutdown().await;
}
