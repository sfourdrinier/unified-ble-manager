use std::collections::BTreeMap;

use serde_json::Number;

use crate::IpcValue;

const CAPABILITY_SCHEMA_VERSION: i64 = 1;
const IMPLEMENTATION_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Complete public catalog: mechanism truth comes from the native instance.
const TAURI_CAPABILITIES: [&str; 42] = [
    "discovery:continuous-scan",
    "discovery:system-chooser",
    "discovery:advertisement-watch",
    "scan:platform-options",
    "peer:resolve-reference",
    "peer:address-targeting",
    "peer:known",
    "peer:system-connected",
    "peer:bonded",
    "peer:origin-authorized",
    "peer:restored",
    "connection:direct",
    "connection:when-available",
    "connection:rssi",
    "connection:effective-mtu",
    "connection:request-mtu",
    "connection:priority",
    "connection:parameters",
    "connection:phy",
    "connection:subrate",
    "security:state",
    "security:pair",
    "security:cancel-pairing",
    "security:unpair",
    "security:custom-ceremony",
    "security:pairing-generation",
    "gatt:descriptors",
    "gatt:indications",
    "gatt:service-changed",
    "gatt:maximum-write-length",
    "gatt:long-write",
    "gatt:reliable-write",
    "gatt:write-without-response-readiness",
    "gatt:high-throughput-acquire",
    "background:apple-restoration",
    "background:android-connected-device-service",
    // Background continuation: a desktop host is never terminated and woken by
    // the OS for a peer's advertisement, so Tauri answers unsupported for all
    // four rather than leaving a capability a caller cannot ask about.
    "background:wake-on-appearance",
    "background:native-resubscribe",
    "background:headless-task",
    "background:wake-notification",
    "background:desktop-maintain-connection",
    "lifecycle:page-persistence",
];

use ubm_core::central::{CapabilityDescriptor, CapabilityState, EvidenceLevel};

/// These are IPC/ownership restrictions, not operating-system assumptions.
/// Their missing routes are pinned by dispatcher tests. Native capability rows
/// otherwise cross unchanged, including instance-specific refusals.
pub(crate) fn transport_restriction(id: &str) -> Option<&'static str> {
    match id {
        "discovery:advertisement-watch" => Some("tauri-advertisement-watch-route-unavailable"),
        "peer:origin-authorized" => Some("tauri-origin-authorized-directory-unavailable"),
        "gatt:reliable-write" => Some("tauri-reliable-write-route-unavailable"),
        "lifecycle:page-persistence" => Some("tauri-rebind-releases-prior-lease"),
        _ => None,
    }
}

pub(crate) fn snapshot(backend_generation: &str, native: &[CapabilityDescriptor]) -> IpcValue {
    object([
        ("schemaVersion", number(2)),
        ("backendGeneration", string(backend_generation)),
        (
            "descriptors",
            IpcValue::Array(
                TAURI_CAPABILITIES
                    .iter()
                    .map(|id| {
                        if let Some(reason) = transport_restriction(id) {
                            return unsupported(id, reason);
                        }
                        let native_id = if *id == "peer:resolve-reference" {
                            reference_resolution_mechanism(native)
                        } else {
                            id
                        };
                        match native.iter().find(|row| row.id() == native_id) {
                            Some(row) => project(id, row),
                            None => unsupported(id, "native-mechanism-not-registered"),
                        }
                    })
                    .collect(),
            ),
        ),
    ])
}

pub(crate) fn reference_resolution_mechanism(native: &[CapabilityDescriptor]) -> &'static str {
    if native.iter().any(|row| {
        row.id() == "peer:known"
            && matches!(
                row.state(),
                CapabilityState::Supported | CapabilityState::Limited
            )
    }) {
        "peer:known"
    } else {
        "peer:bonded"
    }
}

fn unsupported(id: &str, reason: &str) -> IpcValue {
    let row = CapabilityDescriptor::new(
        id,
        CapabilityState::Unsupported,
        &[("availability", 0)],
        &[reason],
        &format!("tauri-transport-{id}"),
        EvidenceLevel::Blocked,
        IMPLEMENTATION_VERSION,
        "tauri-ipc-route-catalog-v3",
        &["capability.truth-limits-evidence-and-binding"],
    )
    .expect("static transport descriptor");
    project(id, &row)
}

fn project(id: &str, native: &CapabilityDescriptor) -> IpcValue {
    let limitations = IpcValue::Array(
        native
            .limitations()
            .iter()
            .map(|reason| {
                object([
                    ("code", string(reason)),
                    ("explanation", string(reason)),
                    (
                        "affectedGuarantee",
                        string(
                            "The native authority bounds this capability with the stated reason.",
                        ),
                    ),
                ])
            })
            .collect(),
    );
    let scenarios = IpcValue::Array(native.scenario_ids().iter().map(string).collect());
    let schema_range = version_range();
    object([
        ("id", string(id)),
        ("state", string(native.state().as_str())),
        ("selectedSchemaRange", schema_range.clone()),
        ("implementationOrigin", string("backend-native")),
        (
            "tck",
            object([
                ("suiteId", string("capability.catalog-v2")),
                ("requiredScenarioIds", scenarios.clone()),
                ("contractRange", schema_range),
            ]),
        ),
        (
            "evidence",
            object([
                ("receiptId", string(native.receipt_id())),
                ("evidenceLevel", string(native.evidence_level().as_str())),
                (
                    "implementationVersion",
                    string(native.implementation_version()),
                ),
                ("sourceDigest", string(native.source_digest())),
                ("scenarioIds", scenarios),
                ("limitations", limitations.clone()),
            ]),
        ),
        ("limitations", limitations),
        (
            "limits",
            IpcValue::Object(
                native
                    .limits()
                    .iter()
                    .map(|(name, maximum)| {
                        (
                            name.clone(),
                            object([
                                ("maximum", IpcValue::Number(Number::from(*maximum))),
                                ("minimum", IpcValue::Null),
                                (
                                    "unit",
                                    string(if name == "availability" {
                                        "boolean"
                                    } else {
                                        "count"
                                    }),
                                ),
                            ]),
                        )
                    })
                    .collect(),
            ),
        ),
    ])
}

fn version_range() -> IpcValue {
    object([
        ("axis", string("capability-schema")),
        ("minimum", version_number()),
        ("maximum", version_number()),
    ])
}
fn version_number() -> IpcValue {
    object([
        ("axis", string("capability-schema")),
        ("value", number(CAPABILITY_SCHEMA_VERSION)),
    ])
}
fn object<const N: usize>(entries: [(&str, IpcValue); N]) -> IpcValue {
    IpcValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<BTreeMap<_, _>>(),
    )
}
fn string(value: impl Into<String>) -> IpcValue {
    IpcValue::String(value.into())
}
fn number(value: i64) -> IpcValue {
    IpcValue::Number(Number::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ubm_core::central::{Central, CentralConfig};
    use ubm_core::contracts::{
        AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
        BackendInstanceId, Generation,
    };

    fn native_for(os: ubm_desktop::DesktopOs) -> Vec<CapabilityDescriptor> {
        let attachment = AttachmentTuple::new(
            AttachmentId::new("test").unwrap(),
            BackendInstanceId::new("test").unwrap(),
            BackendGeneration::new("test").unwrap(),
            AdapterId::new("test").unwrap(),
            AdapterGeneration::new("test").unwrap(),
        );
        let mut core = Central::new(
            attachment,
            Generation::new("test").unwrap(),
            CentralConfig::default(),
        )
        .unwrap();
        ubm_desktop::register_desktop_capabilities_for(&mut core, Some(os), false).unwrap();
        core.registered_capability_descriptors()
    }
    fn json(snapshot: IpcValue) -> serde_json::Value {
        snapshot.into_wire()
    }
    fn row<'a>(snapshot: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
        snapshot["descriptors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap()
    }

    #[test]
    fn native_mechanism_states_are_projected_on_every_desktop_os() {
        for os in ubm_desktop::DesktopOs::ALL {
            let native = native_for(os);
            let projected = json(snapshot("test", &native));
            assert_eq!(
                projected["descriptors"].as_array().unwrap().len(),
                TAURI_CAPABILITIES.len()
            );
            for id in TAURI_CAPABILITIES {
                let native_id = if id == "peer:resolve-reference" {
                    reference_resolution_mechanism(&native)
                } else {
                    id
                };
                let expected = if transport_restriction(id).is_some() {
                    None
                } else {
                    native.iter().find(|row| row.id() == native_id)
                };
                let projected_row = row(&projected, id);
                match expected {
                    Some(native_row) => {
                        assert_eq!(
                            projected_row["state"],
                            native_row.state().as_str(),
                            "{} {id}",
                            os.as_str()
                        );
                        assert_eq!(
                            projected_row["evidence"]["receiptId"],
                            native_row.receipt_id()
                        );
                        assert_eq!(
                            projected_row["evidence"]["evidenceLevel"],
                            native_row.evidence_level().as_str()
                        );
                        assert_eq!(
                            projected_row["evidence"]["implementationVersion"],
                            native_row.implementation_version()
                        );
                        assert_eq!(
                            projected_row["evidence"]["sourceDigest"],
                            native_row.source_digest()
                        );
                        assert_eq!(
                            projected_row["evidence"]["scenarioIds"],
                            serde_json::json!(native_row.scenario_ids())
                        );
                        assert_eq!(
                            projected_row["limitations"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|v| v["code"].as_str().unwrap())
                                .collect::<Vec<_>>(),
                            native_row
                                .limitations()
                                .iter()
                                .map(String::as_str)
                                .collect::<Vec<_>>()
                        );
                        for (key, maximum) in native_row.limits() {
                            assert_eq!(projected_row["limits"][key]["maximum"], *maximum);
                        }
                    }
                    None => assert_eq!(
                        projected_row["state"],
                        "unsupported",
                        "{} {id}",
                        os.as_str()
                    ),
                }
            }
        }
    }

    #[test]
    fn native_fields_and_multiple_limits_survive_projection() {
        let native = CapabilityDescriptor::new(
            "connection:rssi",
            CapabilityState::Limited,
            &[("availability", 1), ("samples", 17)],
            &["first-reason", "second-reason"],
            "native-receipt",
            EvidenceLevel::LivePreview,
            "native-version",
            "native-digest",
            &["native-first", "native-second"],
        )
        .unwrap();
        let projected = json(snapshot("test", &[native]));
        let row = row(&projected, "connection:rssi");
        assert_eq!(row["limits"]["samples"]["maximum"], 17);
        assert_eq!(row["limitations"].as_array().unwrap().len(), 2);
        assert_eq!(row["evidence"]["evidenceLevel"], "live-preview");
        assert_eq!(row["evidence"]["receiptId"], "native-receipt");
        assert_eq!(row["evidence"]["implementationVersion"], "native-version");
        assert_eq!(row["evidence"]["sourceDigest"], "native-digest");
        assert_eq!(
            row["evidence"]["scenarioIds"],
            serde_json::json!(["native-first", "native-second"])
        );
        assert_eq!(row["limitations"], row["evidence"]["limitations"]);
    }

    #[test]
    fn missing_native_rows_never_invent_an_available_mechanism() {
        let projected = json(snapshot("test", &[]));
        for id in TAURI_CAPABILITIES {
            assert_eq!(row(&projected, id)["state"], "unsupported", "{id}");
        }
    }

    #[test]
    fn supported_native_evidence_is_not_replaced_with_blocked_evidence() {
        let native = CapabilityDescriptor::new(
            "connection:rssi",
            CapabilityState::Supported,
            &[("availability", 1)],
            &[],
            "qualified",
            EvidenceLevel::Supported,
            "version",
            "digest",
            &["rssi"],
        )
        .unwrap();
        let projected = json(snapshot("test", &[native]));
        let row = row(&projected, "connection:rssi");
        assert_eq!(row["state"], "supported");
        assert_eq!(row["evidence"]["evidenceLevel"], "supported");
        assert_eq!(row["limitations"], serde_json::json!([]));
    }

    #[test]
    fn unavailable_is_not_rewritten_to_unsupported_or_limited() {
        let native = CapabilityDescriptor::new(
            "connection:effective-mtu",
            CapabilityState::Unavailable,
            &[("availability", 0)],
            &["native-link-measurement-pending"],
            "pending-receipt",
            EvidenceLevel::Blocked,
            "version",
            "digest",
            &["mtu"],
        )
        .unwrap();
        let projected = json(snapshot("test", &[native]));
        let row = row(&projected, "connection:effective-mtu");
        assert_eq!(row["state"], "unavailable");
        assert_eq!(
            row["limitations"][0]["code"],
            "native-link-measurement-pending"
        );
        assert_eq!(row["evidence"]["receiptId"], "pending-receipt");
        assert_eq!(row["limits"]["availability"]["maximum"], 0);
    }

    #[test]
    fn instance_refusal_keeps_zero_availability_and_all_reasons() {
        let native = CapabilityDescriptor::new(
            "connection:direct",
            CapabilityState::Unsupported,
            &[("availability", 0)],
            &["bluez-le-bearer-attestation-required", "instance-refusal"],
            "instance-receipt",
            EvidenceLevel::Blocked,
            "version",
            "digest",
            &["connect"],
        )
        .unwrap();
        let projected = json(snapshot("test", &[native]));
        let row = row(&projected, "connection:direct");
        assert_eq!(row["state"], "unsupported");
        assert_eq!(row["limits"]["availability"]["maximum"], 0);
        assert_eq!(row["limitations"].as_array().unwrap().len(), 2);
        assert_eq!(row["evidence"]["receiptId"], "instance-receipt");
    }
}
