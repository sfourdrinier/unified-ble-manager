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

/// Finding 217 follow-up: the effective ATT MTU derivation this OS answers.
/// Desktop builds report `limited` with the derivation named; a build for
/// any other target keeps the previous `unsupported` answer with its reason,
/// so a platform that genuinely cannot answer still says so precisely.
#[cfg(target_os = "macos")]
const EFFECTIVE_MTU_STATE: (&str, &str, &str) = (
    "limited",
    "corebluetooth-derived-effective-mtu",
    "The effective ATT MTU is derived per link as CBPeripheral.maximumWriteValueLength(for: .withResponse) + 3 (finding 217, the same derivation as the Apple React Native route); deterministic host evidence until physical-radio qualification.",
);
#[cfg(target_os = "windows")]
const EFFECTIVE_MTU_STATE: (&str, &str, &str) = (
    "limited",
    "winrt-gattsession-max-pdu-size",
    "The effective ATT MTU is the GattSession.MaxPduSize btleplug tracks from MaxPduSizeChanged; deterministic host evidence until physical-radio qualification.",
);
#[cfg(target_os = "linux")]
const EFFECTIVE_MTU_STATE: (&str, &str, &str) = (
    "limited",
    "bluez-gatt-characteristic-mtu",
    "The effective ATT MTU is the org.bluez.GattCharacteristic1 MTU of the link's characteristics; deterministic host evidence until physical-radio qualification.",
);
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
const EFFECTIVE_MTU_STATE: (&str, &str, &str) = (
    "unsupported",
    "effective-mtu-boundary-unavailable",
    "The dispatcher exposes no authoritative current ATT MTU observation; the OS-measured MTU already bounds every write through the core maximum-write-length.",
);

pub(crate) fn snapshot(backend_generation: &str) -> IpcValue {
    object([
        ("schemaVersion", number(2)),
        ("backendGeneration", string(backend_generation)),
        (
            "descriptors",
            IpcValue::Array(
                TAURI_CAPABILITIES.iter().map(|id| {
                    if *id == "connection:effective-mtu" {
                        let (state, code, explanation) = EFFECTIVE_MTU_STATE;
                        descriptor(
                            id,
                            state,
                            if state == "limited" {
                                "connection.rssi-and-att-mtu-capability-contract"
                            } else {
                                "capability.truth-limits-evidence-and-binding"
                            },
                            code,
                            explanation,
                        )
                    } else if let Some((_, scenario, code, explanation)) =
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
    /// long-write like the desktop core over the same Rust core. Finding
    /// 217 follow-up: the effective MTU answers `limited` with the
    /// derivation this OS names — the desktop core's per-OS answer, so
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
        let (state, code) = row(&snap, "connection:effective-mtu");
        if cfg!(target_os = "macos") {
            assert_eq!(state, "limited");
            assert_eq!(code, "corebluetooth-derived-effective-mtu");
        } else if cfg!(target_os = "windows") {
            assert_eq!(state, "limited");
            assert_eq!(code, "winrt-gattsession-max-pdu-size");
        } else if cfg!(target_os = "linux") {
            assert_eq!(state, "limited");
            assert_eq!(code, "bluez-gatt-characteristic-mtu");
        } else {
            assert_eq!(state, "unsupported");
            assert_eq!(code, "effective-mtu-boundary-unavailable");
        }
    }
}
