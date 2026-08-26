use std::collections::BTreeMap;

use serde_json::Number;

use crate::IpcValue;

const CAPABILITY_SCHEMA_VERSION: i64 = 1;
const IMPLEMENTATION_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Capabilities whose mechanics are implemented by this dispatcher. Every
/// implemented entry remains `limited` until the corresponding physical-radio
/// evidence is qualified; every other catalog entry is explicitly unsupported.
const TAURI_CAPABILITIES: [&str; 38] = [
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
    "background:desktop-maintain-connection",
    "lifecycle:page-persistence",
];

const TAURI_LIMITED_CAPABILITIES: [(&str, &str, &str, &str); 5] = [
    (
        "discovery:continuous-scan",
        "scan.owner-join-authority-and-signature",
        "one-global-scan-owner",
        "The dispatcher permits one physical scan owner at a time; it does not provide independent concurrent adapter scans.",
    ),
    (
        "connection:direct",
        "connection.lease-joins-borrowing-transfer-and-revocation",
        "deterministic-only",
        "Direct connection and ownership cleanup are implemented, but this receipt is deterministic host evidence rather than a physical-radio qualification.",
    ),
    (
        "connection:rssi",
        "connection.rssi-and-att-mtu-capability-contract",
        "deterministic-only",
        "RSSI dispatch is implemented, but this receipt is deterministic host evidence rather than a physical-radio qualification.",
    ),
    (
        "gatt:descriptors",
        "gatt.descriptor-discovery-read-write",
        "deterministic-only",
        "Descriptor discovery, reads, and writes are implemented, but this receipt is deterministic host evidence rather than a physical-radio qualification.",
    ),
    (
        "gatt:indications",
        "gatt.reads-descriptors-write-policy-and-dispatched-cancellation",
        "delivery-kind-unknown",
        "The btleplug notification stream does not distinguish indications from notifications, so delivery is reported as unknown.",
    ),
];

pub(crate) fn snapshot(backend_generation: &str) -> IpcValue {
    object([
        ("schemaVersion", number(2)),
        ("backendGeneration", string(backend_generation)),
        (
            "descriptors",
            IpcValue::Array(
                TAURI_CAPABILITIES.iter().map(|id| {
                    if let Some((_, scenario, code, explanation)) =
                        TAURI_LIMITED_CAPABILITIES.iter().find(|entry| entry.0 == *id)
                    {
                        descriptor(id, "limited", scenario, code, explanation)
                    } else {
                        descriptor(
                            id,
                            "unsupported",
                            "capability.truth-limits-evidence-and-binding",
                            "not-implemented",
                            "The btleplug dispatcher does not implement this capability in the current host.",
                        )
                    }
                })
                    .collect(),
            ),
        ),
    ])
}

fn descriptor(
    id: &str,
    state: &str,
    scenario: &str,
    limitation_code: &str,
    explanation: &str,
) -> IpcValue {
    let limitation = object([
        ("code", string(limitation_code)),
        ("explanation", string(explanation)),
        (
            "affectedGuarantee",
            string("The application must not treat this capability as fully supported."),
        ),
    ]);
    let schema_range = version_range();
    object([
        ("id", string(id)),
        ("state", string(state)),
        ("selectedSchemaRange", schema_range.clone()),
        ("implementationOrigin", string("backend-native")),
        (
            "tck",
            object([
                ("suiteId", string("capability.catalog-v2")),
                (
                    "requiredScenarioIds",
                    IpcValue::Array(vec![string(scenario)]),
                ),
                ("contractRange", schema_range),
            ]),
        ),
        (
            "evidence",
            object([
                (
                    "receiptId",
                    string(format!("tauri-btleplug-capability-{id}-v2")),
                ),
                (
                    "evidenceLevel",
                    string(if state == "limited" {
                        "deterministic"
                    } else {
                        "blocked"
                    }),
                ),
                ("implementationVersion", string(IMPLEMENTATION_VERSION)),
                (
                    "sourceDigest",
                    string("tauri-btleplug-capability-manifest-v2"),
                ),
                ("scenarioIds", IpcValue::Array(vec![string(scenario)])),
                ("limitations", IpcValue::Array(vec![limitation.clone()])),
            ]),
        ),
        ("limitations", IpcValue::Array(vec![limitation])),
        (
            "limits",
            object([(
                "availability",
                object([
                    ("maximum", number(1)),
                    ("minimum", IpcValue::Null),
                    ("unit", string("boolean")),
                ]),
            )]),
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
