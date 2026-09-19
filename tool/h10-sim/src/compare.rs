//! Equivalence check: a real-strap fingerprint vs a simulator fingerprint.
//!
//! The same `h10-capture` scenario produces both files (once against the
//! strap, once against `h10-sim`), so the comparison is apples to apples:
//! structural parts (advertisement layout, GATT database, values modulo
//! configured identity, behaviour probes) must match field by field, while
//! timing distributions must agree within configurable tolerances. Every
//! check reports its verdict — tolerances are reported, never hidden.
//!
//! Run it with `h10-sim --compare real.json sim.json`. Unit tests below use
//! synthetic fingerprints.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Statistical tolerances for timing comparisons. A timing check passes when
/// `|sim - real| <= max(min_abs_ms, p50_relative * |real|)`: small
/// single-digit-millisecond latencies get the absolute floor, slow
/// distributions get the relative band. Both knobs are CLI-settable and the
/// applied bound is printed per check.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tolerances {
    pub p50_relative: f64,
    pub min_abs_ms: f64,
}

impl Default for Tolerances {
    fn default() -> Self {
        Self {
            p50_relative: 0.25,
            min_abs_ms: 50.0,
        }
    }
}

/// One check verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Fail,
    /// Not comparable (for example advertisement timings on a chooser-only
    /// host whose scan reported `ok: false`): the reason is recorded, never
    /// silently dropped.
    Skipped,
}

/// One compared field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldResult {
    pub field: String,
    pub status: CheckStatus,
    pub detail: String,
}

/// The full comparison: `passed` is true only when no check failed
/// (skips do not fail the run, but stay visible in `fields`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonReport {
    pub passed: bool,
    pub tolerances: Tolerances,
    pub fields: Vec<FieldResult>,
}

/// Identity/state fields excluded from value equality (compared by
/// readability only): the serial is configured per unit, the battery level
/// is charge state, and the local name carries the unit id. Everything else
/// must decode identically.
const VALUE_EQUALITY_EXCLUSIONS: &[&str] = &["serialNumber", "batteryLevelPercent"];

/// Compares two `h10-capture` fingerprints. Structural mismatches and
/// version mismatches fail loudly with the JSON path; a missing section on
/// one side is a failed check, never an assumption.
pub fn compare_fingerprints(real: &Value, sim: &Value, tolerances: Tolerances) -> ComparisonReport {
    let mut fields: Vec<FieldResult> = Vec::new();
    check_version(real, sim, &mut fields);
    compare_advertisement(real, sim, &mut fields);
    compare_gatt(real, sim, &mut fields);
    compare_values(real, sim, &mut fields);
    compare_behaviour(real, sim, &mut fields);
    compare_timings(real, sim, tolerances, &mut fields);
    let passed = fields.iter().all(|field| field.status != CheckStatus::Fail);
    ComparisonReport {
        passed,
        tolerances,
        fields,
    }
}

fn pass(field: &str, detail: String) -> FieldResult {
    FieldResult {
        field: field.to_string(),
        status: CheckStatus::Pass,
        detail,
    }
}

fn fail(field: &str, detail: String) -> FieldResult {
    FieldResult {
        field: field.to_string(),
        status: CheckStatus::Fail,
        detail,
    }
}

fn skip(field: &str, detail: String) -> FieldResult {
    FieldResult {
        field: field.to_string(),
        status: CheckStatus::Skipped,
        detail,
    }
}

fn check_version(real: &Value, sim: &Value, fields: &mut Vec<FieldResult>) {
    let real_version = real.get("version").and_then(Value::as_u64);
    let sim_version = sim.get("version").and_then(Value::as_u64);
    match (real_version, sim_version) {
        (Some(left), Some(right)) if left == right => {
            fields.push(pass("version", format!("both version {left}")));
        }
        _ => fields.push(fail(
            "version",
            format!("version mismatch: real={real_version:?} sim={sim_version:?}"),
        )),
    }
}

fn string_set(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        let mut names: Vec<String> = items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect();
        names.sort();
        names
    })
}

fn compare_advertisement(real: &Value, sim: &Value, fields: &mut Vec<FieldResult>) {
    let real_adv = real.get("advertisement");
    let sim_adv = sim.get("advertisement");
    match (real_adv, sim_adv) {
        (Some(left), Some(right)) => {
            // Service UUID layout must be identical (180D + FEEE on an H10).
            match (
                string_set(&left["serviceUuids"]),
                string_set(&right["serviceUuids"]),
            ) {
                (Some(a), Some(b)) if a == b => {
                    fields.push(pass("advertisement.serviceUuids", format!("{a:?}")));
                }
                (a, b) => fields.push(fail(
                    "advertisement.serviceUuids",
                    format!("real={a:?} sim={b:?}"),
                )),
            }
            // Manufacturer company ids must match (Polar 0x006B = 107).
            let companies = |adv: &Value| {
                adv.get("manufacturerData")
                    .and_then(Value::as_array)
                    .map(|entries| {
                        let mut ids: Vec<i64> = entries
                            .iter()
                            .filter_map(|entry| entry.get("companyId").and_then(Value::as_i64))
                            .collect();
                        ids.sort();
                        ids
                    })
            };
            match (companies(left), companies(right)) {
                (Some(a), Some(b)) if a == b => {
                    fields.push(pass(
                        "advertisement.manufacturerCompanyIds",
                        format!("{a:?}"),
                    ));
                }
                (a, b) => fields.push(fail(
                    "advertisement.manufacturerCompanyIds",
                    format!("real={a:?} sim={b:?}"),
                )),
            }
            // Local names carry per-unit ids: both must be H10 names, the
            // exact id is configured identity, not behaviour.
            let names = (
                left.get("localName").and_then(Value::as_str),
                right.get("localName").and_then(Value::as_str),
            );
            match names {
                (Some(a), Some(b)) if a.starts_with("Polar H10") && b.starts_with("Polar H10") => {
                    fields.push(pass(
                        "advertisement.localName",
                        format!("real={a:?} sim={b:?} (identity modulo prefix)"),
                    ));
                }
                _ => fields.push(fail("advertisement.localName", format!("real={names:?}"))),
            }
        }
        _ => fields.push(fail(
            "advertisement",
            "missing advertisement section on one side".to_string(),
        )),
    }
}

/// Ordered GATT comparison: occurrence order is part of the fingerprint.
fn gatt_rows(database: &Value, key: &str) -> Option<Vec<String>> {
    database.get(key).and_then(Value::as_array).map(|rows| {
        rows.iter()
            .map(|row| {
                let uuid = row.get("uuid").and_then(Value::as_str).unwrap_or("?");
                let occurrence = row.get("occurrence").and_then(Value::as_u64).unwrap_or(0);
                let properties = row
                    .get("properties")
                    .map(|properties| properties.to_string())
                    .unwrap_or_default();
                format!("{occurrence}:{uuid}:{properties}")
            })
            .collect()
    })
}

fn compare_gatt(real: &Value, sim: &Value, fields: &mut Vec<FieldResult>) {
    let real_db = real.get("gatt");
    let sim_db = sim.get("gatt");
    match (real_db, sim_db) {
        (Some(left), Some(right)) => {
            for key in ["services", "characteristics", "descriptors"] {
                match (gatt_rows(left, key), gatt_rows(right, key)) {
                    (Some(a), Some(b)) if a == b => {
                        fields.push(pass(
                            &format!("gatt.{key}"),
                            format!("{} rows identical, order pinned", a.len()),
                        ));
                    }
                    (a, b) => fields.push(fail(
                        &format!("gatt.{key}"),
                        format!("real={a:?} sim={b:?}"),
                    )),
                }
            }
        }
        _ => fields.push(fail("gatt", "missing gatt section on one side".to_string())),
    }
}

fn compare_values(real: &Value, sim: &Value, fields: &mut Vec<FieldResult>) {
    let real_values = real.get("values").and_then(Value::as_object);
    let sim_values = sim.get("values").and_then(Value::as_object);
    match (real_values, sim_values) {
        (Some(left), Some(right)) => {
            // Readability first: the same set of characteristics must answer.
            let mut names: Vec<&String> = left.keys().chain(right.keys()).collect();
            names.sort();
            names.dedup();
            for name in names {
                let left_entry = left.get(name);
                let right_entry = right.get(name);
                let readable = |entry: Option<&Value>| {
                    entry
                        .and_then(|entry| entry.get("ok"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                };
                let (readable_left, readable_right) = (readable(left_entry), readable(right_entry));
                if readable_left != readable_right {
                    fields.push(fail(
                        &format!("values.{name}.readable"),
                        format!("real readable={readable_left} sim readable={readable_right}"),
                    ));
                    continue;
                }
                fields.push(pass(
                    &format!("values.{name}.readable"),
                    format!("readable={readable_left}"),
                ));
                if !readable_left || VALUE_EQUALITY_EXCLUSIONS.contains(&name.as_str()) {
                    continue;
                }
                let text = |entry: Option<&Value>| {
                    entry
                        .and_then(|entry| entry.get("text"))
                        .cloned()
                        .unwrap_or(Value::Null)
                };
                let raw = |entry: Option<&Value>| {
                    entry
                        .and_then(|entry| entry.get("raw"))
                        .cloned()
                        .unwrap_or(Value::Null)
                };
                if text(left_entry) == text(right_entry) && raw(left_entry) == raw(right_entry) {
                    fields.push(pass(
                        &format!("values.{name}"),
                        "decoded value identical".to_string(),
                    ));
                } else {
                    fields.push(fail(
                        &format!("values.{name}"),
                        format!("real={:?} sim={:?}", left_entry, right_entry),
                    ));
                }
            }
        }
        _ => fields.push(fail(
            "values",
            "missing values section on one side".to_string(),
        )),
    }
}

/// Behaviour probes must answer with the same status codes: these are the
/// Polar SDK status codes (`ERROR_NOT_SUPPORTED` 0x03, `INVALID_OP` 0x01,
/// …), identical on a real strap and on the sim.
fn probe_status(behaviour: &Value, probe: &str) -> Option<i64> {
    behaviour
        .get(probe)
        .and_then(|entry| entry.get("errorCode"))
        .and_then(Value::as_i64)
}

fn compare_behaviour(real: &Value, sim: &Value, fields: &mut Vec<FieldResult>) {
    let real_probes = real.get("behaviour");
    let sim_probes = sim.get("behaviour");
    match (real_probes, sim_probes) {
        (Some(left), Some(right)) => {
            for probe in [
                "pmdStartWithoutMtu",
                "repeatedStart",
                "stop",
                "stopWhenStopped",
                "invalidPmdCommand",
            ] {
                match (probe_status(left, probe), probe_status(right, probe)) {
                    (Some(a), Some(b)) if a == b => {
                        fields.push(pass(&format!("behaviour.{probe}"), format!("status {a}")));
                    }
                    (a, b) => fields.push(fail(
                        &format!("behaviour.{probe}"),
                        format!("real={a:?} sim={b:?}"),
                    )),
                }
            }
            // Control-point read while notifying: same readability, same features.
            let features = |probes: &Value| {
                probes
                    .get("controlPointReadWhileNotifying")
                    .map(|entry| entry.to_string())
            };
            if features(left) == features(right) {
                fields.push(pass(
                    "behaviour.controlPointReadWhileNotifying",
                    "identical answer".to_string(),
                ));
            } else {
                fields.push(fail(
                    "behaviour.controlPointReadWhileNotifying",
                    format!("real={:?} sim={:?}", features(left), features(right)),
                ));
            }
            let battery =
                |probes: &Value| probes.get("batteryNotify").map(|entry| entry.to_string());
            if battery(left) == battery(right) {
                fields.push(pass(
                    "behaviour.batteryNotify",
                    "identical answer".to_string(),
                ));
            } else {
                fields.push(fail(
                    "behaviour.batteryNotify",
                    format!("real={:?} sim={:?}", battery(left), battery(right)),
                ));
            }
        }
        _ => fields.push(fail(
            "behaviour",
            "missing behaviour section on one side".to_string(),
        )),
    }
}

fn distribution_p50(timings: &serde_json::Map<String, Value>, field: &str) -> Option<f64> {
    timings
        .get(field)
        .and_then(|dist| dist.get("p50"))
        .and_then(Value::as_f64)
}

fn single_ms(timings: &serde_json::Map<String, Value>, field: &str) -> Option<f64> {
    timings.get(field).and_then(Value::as_f64)
}

fn compare_timings(
    real: &Value,
    sim: &Value,
    tolerances: Tolerances,
    fields: &mut Vec<FieldResult>,
) {
    let timings = (
        real.get("timings").and_then(Value::as_object),
        sim.get("timings").and_then(Value::as_object),
    );
    let (Some(left), Some(right)) = timings else {
        fields.push(fail(
            "timings",
            "missing timings section on one side".to_string(),
        ));
        return;
    };
    // Distributions compared on p50 within tolerance.
    for field in [
        "hrNotificationIntervalMs",
        "pmdResponseMs",
        "ecgFrameIntervalMs",
        "advertisementIntervalMs",
    ] {
        match (
            distribution_p50(left, field),
            distribution_p50(right, field),
        ) {
            (Some(a), Some(b)) => {
                let bound = tolerances.min_abs_ms.max(tolerances.p50_relative * a.abs());
                let delta = (b - a).abs();
                if delta <= bound {
                    fields.push(pass(
                        &format!("timings.{field}.p50"),
                        format!("real={a:.1} sim={b:.1} delta={delta:.1} bound={bound:.1}"),
                    ));
                } else {
                    fields.push(fail(
                        &format!("timings.{field}.p50"),
                        format!("real={a:.1} sim={b:.1} delta={delta:.1} bound={bound:.1}"),
                    ));
                }
            }
            _ => fields.push(skip(
                &format!("timings.{field}.p50"),
                "unmeasured on one side (for example scan unsupported); not comparable".to_string(),
            )),
        }
    }
    // Single-shot latencies: same tolerance rule, reported per field.
    for field in [
        "connectMs",
        "discoveryMs",
        "timeToFirstHrMs",
        "pmdGetSettingsMs",
        "pmdStartMs",
        "pmdStopMs",
    ] {
        match (single_ms(left, field), single_ms(right, field)) {
            (Some(a), Some(b)) => {
                let bound = tolerances.min_abs_ms.max(tolerances.p50_relative * a.abs());
                let delta = (b - a).abs();
                if delta <= bound {
                    fields.push(pass(
                        &format!("timings.{field}"),
                        format!("real={a:.1} sim={b:.1} delta={delta:.1} bound={bound:.1}"),
                    ));
                } else {
                    fields.push(fail(
                        &format!("timings.{field}"),
                        format!("real={a:.1} sim={b:.1} delta={delta:.1} bound={bound:.1}"),
                    ));
                }
            }
            _ => fields.push(skip(
                &format!("timings.{field}"),
                "unmeasured on one side".to_string(),
            )),
        }
    }
    // MTU is the platform's answer on both sides: the request must match,
    // the effective values are reported, never judged.
    let requested = (
        left.get("mtu").and_then(|mtu| mtu.get("requested")),
        right.get("mtu").and_then(|mtu| mtu.get("requested")),
    );
    match requested {
        (Some(a), Some(b)) if a == b => fields.push(pass("timings.mtu.requested", format!("{a}"))),
        _ => fields.push(fail(
            "timings.mtu.requested",
            format!("real={:?} sim={:?}", requested.0, requested.1),
        )),
    }
    fields.push(skip(
        "timings.mtu.effective",
        format!(
            "platform answer on both sides, reported not judged: real={:?} sim={:?}",
            left.get("mtu").and_then(|mtu| mtu.get("effective")),
            right.get("mtu").and_then(|mtu| mtu.get("effective"))
        ),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn distribution(p50: f64) -> Value {
        json!({
            "n": 60, "min": p50 - 50.0, "p10": p50 - 20.0, "p50": p50,
            "p90": p50 + 20.0, "max": p50 + 50.0, "mean": p50, "stdev": 15.0
        })
    }

    fn fingerprint(name: &str, serial: &str, hr_p50: f64) -> Value {
        json!({
            "version": 1,
            "device": {"name": name},
            "advertisement": {
                "ok": true,
                "localName": name,
                "serviceUuids": ["0000180d-0000-1000-8000-00805f9b34fb", "0000180f-0000-1000-8000-00805f9b34fb"],
                "manufacturerData": [{"companyId": 107, "data": ""}],
                "advertisementIntervalMs": distribution(100.0)
            },
            "gatt": {
                "services": [{"uuid": "180d", "occurrence": 0, "primary": true}],
                "characteristics": [{"uuid": "2a37", "occurrence": 0, "properties": {"notify": true}}],
                "descriptors": [{"uuid": "2902", "occurrence": 0}]
            },
            "values": {
                "manufacturerName": {"ok": true, "text": "Polar Electro Oy", "raw": "506f6c6172"},
                "serialNumber": {"ok": true, "text": serial, "raw": "00"},
                "batteryLevelPercent": {"ok": true, "text": "85", "raw": "55"},
                "pnpId": {"ok": false, "error": {"code": "gatt.attribute-not-found"}}
            },
            "timings": {
                "hrNotificationIntervalMs": distribution(hr_p50),
                "pmdResponseMs": distribution(10.0),
                "ecgFrameIntervalMs": distribution(500.0),
                "advertisementIntervalMs": distribution(100.0),
                "connectMs": 200.0, "discoveryMs": 150.0,
                "timeToFirstHrMs": 1100.0,
                "pmdGetSettingsMs": 10.0, "pmdStartMs": 12.0, "pmdStopMs": 9.0,
                "mtu": {"requested": 517}
            },
            "behaviour": {
                "pmdStartWithoutMtu": {"op": "start-without-mtu", "errorCode": 0},
                "repeatedStart": {"op": "repeated-start", "errorCode": 0},
                "stop": {"op": "stop", "errorCode": 0},
                "stopWhenStopped": {"op": "stop-when-stopped", "errorCode": 0},
                "invalidPmdCommand": {"op": "invalid-op", "errorCode": 1},
                "controlPointReadWhileNotifying": {"ok": true},
                "batteryNotify": {"ok": true, "effectiveDelivery": "notification"}
            }
        })
    }

    #[test]
    fn identical_fingerprints_modulo_identity_pass() {
        let real = fingerprint("Polar H10 E997042F", "E997042F", 1000.0);
        let sim = fingerprint("Polar H10 SIM0001", "SIM000001", 1010.0);
        let report = compare_fingerprints(&real, &sim, Tolerances::default());
        assert!(
            report.passed,
            "expected pass, got: {}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    }

    #[test]
    fn gatt_order_mismatch_fails_that_field_only() {
        let real = fingerprint("Polar H10 E997042F", "E997042F", 1000.0);
        let mut sim = fingerprint("Polar H10 SIM0001", "SIM000001", 1000.0);
        sim["gatt"]["characteristics"] =
            json!([{"uuid": "2a38", "occurrence": 0, "properties": {}}]);
        let report = compare_fingerprints(&real, &sim, Tolerances::default());
        assert!(!report.passed);
        let field = report
            .fields
            .iter()
            .find(|field| field.field == "gatt.characteristics")
            .unwrap();
        assert_eq!(field.status, CheckStatus::Fail);
        assert!(
            report
                .fields
                .iter()
                .filter(|field| field.status == CheckStatus::Fail)
                .count()
                == 1
        );
    }

    #[test]
    fn timing_outside_tolerance_fails_with_reported_bound() {
        let real = fingerprint("Polar H10 E997042F", "E997042F", 1000.0);
        let sim = fingerprint("Polar H10 SIM0001", "SIM000001", 2000.0);
        let report = compare_fingerprints(&real, &sim, Tolerances::default());
        assert!(!report.passed);
        let field = report
            .fields
            .iter()
            .find(|field| field.field == "timings.hrNotificationIntervalMs.p50")
            .unwrap();
        assert_eq!(field.status, CheckStatus::Fail);
        assert!(
            field.detail.contains("bound="),
            "tolerance must be reported, got {}",
            field.detail
        );
    }

    #[test]
    fn behaviour_status_mismatch_fails() {
        let real = fingerprint("Polar H10 E997042F", "E997042F", 1000.0);
        let mut sim = fingerprint("Polar H10 SIM0001", "SIM000001", 1000.0);
        sim["behaviour"]["invalidPmdCommand"] = json!({"op": "invalid-op", "errorCode": 0});
        let report = compare_fingerprints(&real, &sim, Tolerances::default());
        assert!(!report.passed);
        let field = report
            .fields
            .iter()
            .find(|field| field.field == "behaviour.invalidPmdCommand")
            .unwrap();
        assert_eq!(field.status, CheckStatus::Fail);
    }

    #[test]
    fn version_mismatch_fails_loudly() {
        let real = fingerprint("Polar H10 E997042F", "E997042F", 1000.0);
        let mut sim = fingerprint("Polar H10 SIM0001", "SIM000001", 1000.0);
        sim["version"] = json!(2);
        let report = compare_fingerprints(&real, &sim, Tolerances::default());
        assert!(!report.passed);
    }
}
