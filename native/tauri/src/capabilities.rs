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

const TAURI_LIMITED_CAPABILITIES: [(&str, &str, &str, &str); 8] = [
    (
        "discovery:continuous-scan",
        "scan.owner-join-authority-and-signature",
        "one-global-scan-owner",
        "The dispatcher permits one physical scan owner at a time; it does not provide independent concurrent adapter scans.",
    ),
    (
        "peer:resolve-reference",
        "peer.resolve-reference",
        "platform-guid-only",
        "Platform-guid resolution for observed peers is implemented, but this receipt is deterministic host evidence rather than a physical-radio qualification; address domains need OS identity adapters.",
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
        "gatt.indications",
        "delivery-kind-unknown",
        "The btleplug notification stream does not distinguish indications from notifications, so delivery is reported as unknown.",
    ),
    // Finding 190b (owner decision J): the dispatcher answers these through
    // the shared desktop core, so Tauri advertises them like the desktop
    // core instead of bare unsupported.
    (
        "gatt:maximum-write-length",
        "gatt.maximum-write-length",
        "deterministic-only",
        "The largest single write the OS accepts on the link for the requested mode, measured per link through the core; deterministic host evidence until physical-radio qualification.",
    ),
    (
        "gatt:long-write",
        "gatt.long-write",
        "no-prepared-write-path",
        "Prepared-write transactions have no btleplug path; long writes are rejected, never silently single-written.",
    ),
];

/// Unsupported rows that still answer with the desktop core's own reason, so
/// every desktop host reports the same words (finding 190b).
const TAURI_UNSUPPORTED_CAPABILITIES: [(&str, &str, &str); 1] = [(
    "connection:effective-mtu",
    "effective-mtu-boundary-unavailable",
    "The dispatcher exposes no authoritative current ATT MTU observation; the OS-measured MTU already bounds every write through the core maximum-write-length.",
)];

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
                    } else if let Some((_, code, explanation)) =
                        TAURI_UNSUPPORTED_CAPABILITIES.iter().find(|entry| entry.0 == *id)
                    {
                        descriptor(
                            id,
                            "unsupported",
                            "capability.truth-limits-evidence-and-binding",
                            code,
                            explanation,
                        )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &IpcValue, key: &str) -> String {
        match value {
            IpcValue::Object(fields) => match fields.get(key) {
                Some(IpcValue::String(text)) => text.clone(),
                other => panic!("field {key} is not a string: {other:?}"),
            },
            other => panic!("expected an object, got {other:?}"),
        }
    }

    /// One descriptor's state and first limitation code by capability id.
    fn row(snapshot: &IpcValue, id: &str) -> (String, String) {
        let descriptors = match snapshot {
            IpcValue::Object(fields) => match fields.get("descriptors") {
                Some(IpcValue::Array(descriptors)) => descriptors,
                other => panic!("descriptors is not an array: {other:?}"),
            },
            other => panic!("expected an object, got {other:?}"),
        };
        let descriptor = descriptors
            .iter()
            .find(|descriptor| text(descriptor, "id") == id)
            .unwrap_or_else(|| panic!("missing descriptor {id}"));
        let state = text(descriptor, "state");
        let limitations = match descriptor {
            IpcValue::Object(fields) => match fields.get("limitations") {
                Some(IpcValue::Array(limitations)) => limitations,
                other => panic!("limitations is not an array: {other:?}"),
            },
            other => panic!("expected an object, got {other:?}"),
        };
        let code = limitations
            .first()
            .map(|limitation| text(limitation, "code"))
            .unwrap_or_else(|| panic!("{id} has no limitation"));
        (state, code)
    }

    /// Finding 190b (owner decision J): Tauri advertises max-write and
    /// long-write like the desktop core over the same Rust core, and the
    /// effective MTU answers unsupported with the desktop's own reason —
    /// every desktop host answers the same.
    #[test]
    fn finding_190b_write_capabilities_match_the_desktop_core() {
        let snap = snapshot("backend-generation-1");
        assert_eq!(
            row(&snap, "gatt:maximum-write-length"),
            ("limited".to_owned(), "deterministic-only".to_owned())
        );
        assert_eq!(
            row(&snap, "gatt:long-write"),
            ("limited".to_owned(), "no-prepared-write-path".to_owned())
        );
        assert_eq!(
            row(&snap, "connection:effective-mtu"),
            (
                "unsupported".to_owned(),
                "effective-mtu-boundary-unavailable".to_owned()
            )
        );
    }
}
