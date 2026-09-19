//! In-process simulator fingerprint for the fidelity test.
//!
//! Builds the same `h10-capture` fingerprint shape the driver scenario
//! produces against a real strap, but answered from [`SimConfig`] /
//! [`TimingProfile`] with no radio involved. The GATT rows come from the
//! same [`crate::radio::h10_services`] declaration the backends serve, so
//! the fingerprint cannot drift from what goes over the air. The fidelity
//! test below compares every committed real fingerprint in
//! `fixtures/h10-fingerprints/` against it with
//! [`crate::compare::compare_fingerprints`].
//!
//! Only fields the comparator reads are modelled precisely; central-side
//! latencies (connect, discovery, first-HR, single-shot PMD latencies) are
//! the central's own answers and are omitted, which the comparator reports
//! as skips — never as passes.

use serde_json::{json, Value};

use crate::gatt_spec;
use crate::radio::{h10_services, CharProperty};
use crate::sim::{SimConfig, SimState};
use crate::timing::TimingProfile;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The ten SIG property flags in fingerprint order, plus the availability
/// map the capture always reports as all-known (the sim declares every
/// property explicitly, so nothing is ever unknown).
fn properties_json(properties: &[CharProperty]) -> Value {
    let has = |want: CharProperty| properties.contains(&want);
    let flags = [
        ("broadcast", false),
        ("read", has(CharProperty::Read)),
        ("writeWithResponse", has(CharProperty::Write)),
        (
            "writeWithoutResponse",
            has(CharProperty::WriteWithoutResponse),
        ),
        ("authenticatedSignedWrites", false),
        ("notify", has(CharProperty::Notify)),
        ("indicate", has(CharProperty::Indicate)),
        ("extendedProperties", false),
        ("reliableWrite", false),
        ("writableAuxiliaries", false),
    ];
    let mut map = serde_json::Map::new();
    for (name, value) in flags {
        map.insert(name.to_string(), Value::Bool(value));
    }
    let mut availability = serde_json::Map::new();
    for (name, _) in flags {
        availability.insert(name.to_string(), Value::String("known".to_string()));
    }
    map.insert("availability".to_string(), Value::Object(availability));
    Value::Object(map)
}

fn status_name(status: u8) -> &'static str {
    match status {
        x if x == gatt_spec::PMD_STATUS_SUCCESS => "SUCCESS",
        x if x == gatt_spec::PMD_STATUS_INVALID_OP => "ERROR_INVALID_OP_CODE",
        x if x == gatt_spec::PMD_STATUS_ALREADY_IN_STATE => "ERROR_ALREADY_IN_STATE",
        x if x == gatt_spec::PMD_STATUS_NOT_SUPPORTED => "ERROR_NOT_SUPPORTED",
        x if x == gatt_spec::PMD_STATUS_INVALID_MEASUREMENT_TYPE => {
            "ERROR_INVALID_MEASUREMENT_TYPE"
        }
        _ => "ERROR_UNKNOWN",
    }
}

/// A timing distribution pinned to one configured value: the comparator
/// only reads `p50`, and a point distribution states honestly that this is
/// the sim's configured model — not a measurement.
fn point_distribution(p50: f64) -> Value {
    json!({
        "n": 1,
        "min": p50,
        "p10": p50,
        "p50": p50,
        "p90": p50,
        "max": p50,
        "mean": p50,
        "stdev": 0.0,
    })
}

/// Builds the simulator fingerprint for `config` + `timing`. Any structural
/// problem is an `Err` naming the cause — never a silent partial document.
pub fn sim_fingerprint(config: &SimConfig, timing: &TimingProfile) -> Result<Value, String> {
    let services = h10_services(config).map_err(|error| error.to_string())?;
    let service_rows: Vec<Value> = services
        .iter()
        .map(|service| {
            json!({
                "uuid": service.uuid.to_string().to_lowercase(),
                "occurrence": 0,
                "primary": true,
            })
        })
        .collect();
    let mut characteristic_rows = Vec::new();
    let mut descriptor_count = 0;
    for service in &services {
        for characteristic in &service.characteristics {
            characteristic_rows.push(json!({
                "uuid": characteristic.uuid.to_string().to_lowercase(),
                "occurrence": 0,
                "properties": properties_json(&characteristic.properties),
            }));
            if characteristic.properties.contains(&CharProperty::Notify)
                || characteristic.properties.contains(&CharProperty::Indicate)
            {
                descriptor_count += 1;
            }
        }
    }
    let descriptor_rows: Vec<Value> = (0..descriptor_count)
        .map(|_| {
            json!({
                "uuid": "00002902-0000-1000-8000-00805f9b34fb",
                "occurrence": 0,
            })
        })
        .collect();

    let dis = |text: &str| {
        let bytes = gatt_spec::encode_dis_string(text);
        json!({"ok": true, "text": String::from_utf8_lossy(&bytes), "raw": hex(&bytes)})
    };
    let system_id =
        gatt_spec::encode_system_id(config.system_id_manufacturer, config.system_id_oui);
    let battery = gatt_spec::encode_battery_level(config.battery_percent);
    let features = gatt_spec::encode_pmd_features();
    let bitmap = features[1];
    let values = json!({
        "batteryLevelPercent": {
            "ok": true,
            "text": String::from(char::from(battery[0])),
            "raw": hex(&battery),
        },
        "manufacturerName": dis(&config.manufacturer),
        "modelNumber": dis(&config.model),
        "serialNumber": dis(&config.serial),
        "hardwareRevision": dis(&config.hardware),
        "firmwareRevision": dis(&config.firmware),
        "softwareRevision": dis(&config.software),
        "systemId": {
            "ok": true,
            "text": String::from_utf8_lossy(&system_id),
            "raw": hex(&system_id),
        },
        "bodySensorLocation": {"ok": true, "text": "\x01", "raw": "01"},
        "pnpId": {"ok": false, "error": {"code": "gatt.not-found"}},
        "pmdFeatures": {
            "ok": true,
            "ecg": bitmap & 0x01 != 0,
            "ppg": bitmap & 0x02 != 0,
            "acc": bitmap & 0x04 != 0,
            "ppi": bitmap & 0x08 != 0,
            "raw": hex(&features),
        },
        "pmdSettingsEcg": {"ok": true, "value": {"SAMPLE_RATE": [130], "RESOLUTION": [14]}},
    });

    // Behaviour probes, driven through the same handler the radio uses, in
    // the scenario's order: start, repeated start, stop, idle stop, invalid.
    let mut sim = SimState::new(config.clone());
    let start = [0x02, 0x00, 0x00, 0x01, 0x82, 0x00, 0x01, 0x01, 0x0E, 0x00];
    let stop = [0x03, 0x00];
    let probe = |sim: &mut SimState, op: &str, bytes: &[u8]| {
        let outcome = sim.handle_pmd_write(bytes);
        // The live loop commits each decision when its indication goes out;
        // the probe does the same before the next command arrives.
        sim.apply_pmd_action(outcome.action);
        let status = outcome
            .indicate
            .as_ref()
            .and_then(|answer| answer.get(3).copied());
        match status {
            Some(code) => json!({"op": op, "errorCode": code, "errorName": status_name(code)}),
            None => json!({"op": op, "error": "pmd.write-unanswered"}),
        }
    };
    let start_without_mtu = probe(&mut sim, "start-without-mtu", &start);
    let repeated_start = probe(&mut sim, "repeated-start", &start);
    let stopped = probe(&mut sim, "stop", &stop);
    let stop_when_stopped = probe(&mut sim, "stop-when-stopped", &stop);
    let invalid = probe(&mut sim, "invalid-op", &[0xFF, 0x00]);
    let behaviour = json!({
        "pmdStartWithoutMtu": start_without_mtu,
        "repeatedStart": repeated_start,
        "stop": stopped,
        "stopWhenStopped": stop_when_stopped,
        "invalidPmdCommand": invalid,
        "controlPointReadWhileNotifying": {
            "ok": true,
            "ecg": bitmap & 0x01 != 0,
            "ppg": bitmap & 0x02 != 0,
            "acc": bitmap & 0x04 != 0,
            "ppi": bitmap & 0x08 != 0,
            "raw": hex(&features),
        },
        // `effectiveDelivery` is the central backend's observation, not a
        // strap answer (btleplug reports `unknown`, CoreBluetooth
        // `notification` against the same strap); the comparator judges
        // `ok`/`release` only, so the Tauri-observed value is recorded.
        "batteryNotify": {"ok": true, "effectiveDelivery": "unknown", "release": "released"},
    });

    let ecg_frame_interval_ms = 1000.0 / config.ecg_frames_per_sec;
    let timings = json!({
        "hrNotificationIntervalMs": point_distribution(timing.hr_interval.median_ms),
        "pmdResponseMs": point_distribution(timing.pmd_response.median_ms),
        "ecgFrameIntervalMs": point_distribution(ecg_frame_interval_ms),
        "ecgSamplesPerFrame": {
            "frames": 1,
            "min": config.ecg_frame_samples,
            "max": config.ecg_frame_samples,
            "consistent": true,
            "expectedPerSecond": 130,
        },
        "mtu": {"requested": 517},
    });

    Ok(json!({
        "version": 1,
        "device": {"name": config.name},
        "advertisement": {
            "ok": true,
            "localName": config.name,
            "serviceUuids": [
                "0000180d-0000-1000-8000-00805f9b34fb",
                "0000feee-0000-1000-8000-00805f9b34fb",
            ],
            "manufacturerData": [
                {"companyId": config.mfr_company, "data": hex(&config.mfr_payload)},
            ],
            "serviceData": [],
        },
        "gatt": {
            "services": service_rows,
            "characteristics": characteristic_rows,
            "descriptors": descriptor_rows,
            "serviceCount": services.len(),
            "characteristicCount": services.iter().map(|service| service.characteristics.len()).sum::<usize>(),
            "descriptorCount": descriptor_count,
        },
        "values": values,
        "timings": timings,
        "behaviour": behaviour,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::{compare_fingerprints, Tolerances};

    fn stock_sim_fingerprint() -> Value {
        let profile = crate::profile::parse_profile(
            include_str!("../profiles/stock-h10.json"),
            "<builtin stock-h10>",
        )
        .expect("stock profile must parse");
        let config = SimConfig::from_profile(&profile).expect("stock profile must build a config");
        let timing: TimingProfile =
            serde_json::from_str(include_str!("../profiles/timing-h10-measured.json"))
                .expect("measured timing profile must parse");
        sim_fingerprint(&config, &timing).expect("sim fingerprint must build")
    }

    fn load_fixture(name: &str) -> Value {
        let text = std::fs::read_to_string(format!("fixtures/h10-fingerprints/{name}"))
            .expect("committed fingerprint must exist");
        serde_json::from_str(&text).expect("committed fingerprint must parse")
    }

    /// Strips the platform-injected GAP/GATT services from the Android
    /// capture: Android exposes generic access `1800` + generic attribute
    /// `1801` (and the Service Changed CCCD) that CoreBluetooth/btleplug
    /// hide on the iOS/Tauri captures. They are central-platform surface,
    /// not strap behaviour.
    fn strip_android_platform_services(fingerprint: &mut Value) {
        let platform_services = [
            "00001800-0000-1000-8000-00805f9b34fb",
            "00001801-0000-1000-8000-00805f9b34fb",
        ];
        let platform_chars = [
            "00002a00-0000-1000-8000-00805f9b34fb",
            "00002a01-0000-1000-8000-00805f9b34fb",
            "00002a04-0000-1000-8000-00805f9b34fb",
            "00002aa6-0000-1000-8000-00805f9b34fb",
            "00002a05-0000-1000-8000-00805f9b34fb",
        ];
        let keep = |uuid: &Value| {
            uuid.get("uuid")
                .and_then(Value::as_str)
                .is_none_or(|id| !platform_services.contains(&id) && !platform_chars.contains(&id))
        };
        if let Some(services) = fingerprint
            .get_mut("gatt")
            .and_then(|gatt| gatt.get_mut("services"))
            .and_then(Value::as_array_mut)
        {
            services.retain(&keep);
        }
        if let Some(characteristics) = fingerprint
            .get_mut("gatt")
            .and_then(|gatt| gatt.get_mut("characteristics"))
            .and_then(Value::as_array_mut)
        {
            characteristics.retain(&keep);
        }
        // The five stripped characteristics carried exactly one CCCD (Service
        // Changed indicates); the reads have none.
        if let Some(descriptors) = fingerprint
            .get_mut("gatt")
            .and_then(|gatt| gatt.get_mut("descriptors"))
            .and_then(Value::as_array_mut)
        {
            descriptors.pop();
        }
    }

    #[test]
    fn sim_fingerprint_declares_seventeen_characteristics_and_seven_cccds() {
        let fingerprint = stock_sim_fingerprint();
        assert_eq!(fingerprint["gatt"]["serviceCount"], 6);
        assert_eq!(fingerprint["gatt"]["characteristicCount"], 17);
        assert_eq!(fingerprint["gatt"]["descriptorCount"], 7);
    }

    #[test]
    fn sim_fingerprint_matches_the_tauri_capture() {
        let real = load_fixture("tauri-macos-unknown-engine-E9B93D29-2026-09-19.json");
        let sim = stock_sim_fingerprint();
        let report = compare_fingerprints(&real, &sim, Tolerances::default());
        assert!(
            report.passed,
            "fidelity gap vs Tauri capture: {}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    }

    #[test]
    fn sim_fingerprint_matches_the_ios_capture() {
        let real = load_fixture("expo-ios-ios-phone-E9B93D29-2026-09-19.json");
        let sim = stock_sim_fingerprint();
        let report = compare_fingerprints(&real, &sim, Tolerances::default());
        assert!(
            report.passed,
            "fidelity gap vs iOS capture: {}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    }

    #[test]
    fn sim_fingerprint_matches_the_android_capture_modulo_platform_services() {
        let mut real = load_fixture("expo-android-samsung-sm-a376u1-2-E9B93D29-2026-09-19.json");
        strip_android_platform_services(&mut real);
        let sim = stock_sim_fingerprint();
        let report = compare_fingerprints(&real, &sim, Tolerances::default());
        assert!(
            report.passed,
            "fidelity gap vs Android capture: {}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    }
}
