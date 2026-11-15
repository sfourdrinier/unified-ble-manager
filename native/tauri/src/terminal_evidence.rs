use std::collections::BTreeMap;

use super::{into_object, required_string, required_value, DispatchError, IpcValue};
use ubm_core::contracts::BleErrorCode;

const FORMAT_VERSION: i64 = 1;
const ENCODING_VERSION: i64 = 2;

/// Exact loss accounting carried by a terminal observation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct TerminalCounters {
    dropped_items: u64,
    dropped_bytes: u64,
    replaced_items: u64,
}

impl TerminalCounters {
    pub(super) const ZERO: Self = Self {
        dropped_items: 0,
        dropped_bytes: 0,
        replaced_items: 0,
    };

    #[cfg(test)]
    pub(super) const fn new(dropped_items: u64, dropped_bytes: u64, replaced_items: u64) -> Self {
        Self {
            dropped_items,
            dropped_bytes,
            replaced_items,
        }
    }

    pub(super) const fn dropped_items(self) -> u64 {
        self.dropped_items
    }

    pub(super) const fn dropped_bytes(self) -> u64 {
        self.dropped_bytes
    }

    pub(super) const fn replaced_items(self) -> u64 {
        self.replaced_items
    }
}

impl From<ubm_core::central::SubscriptionTerminal> for TerminalCounters {
    fn from(terminal: ubm_core::central::SubscriptionTerminal) -> Self {
        Self {
            dropped_items: terminal.dropped_items(),
            dropped_bytes: terminal.dropped_bytes(),
            replaced_items: terminal.replaced_items(),
        }
    }
}

/// The Rust-side form of `encodeTerminalEvidence` in
/// `src/core/terminal-evidence-codec.ts`.  It deliberately encodes the
/// terminal snapshot, rather than the Tauri event wrapper, so the bytes are
/// portable across source-native hosts.
pub(super) fn encode_terminal(
    scope: &ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope,
    reason: &str,
    error: Option<&IpcValue>,
    counters: TerminalCounters,
) -> Vec<u8> {
    let mut terminal = BTreeMap::from([
        (
            "droppedBytes".to_owned(),
            IpcValue::Number(counters.dropped_bytes.into()),
        ),
        (
            "droppedItems".to_owned(),
            IpcValue::Number(counters.dropped_items.into()),
        ),
        ("kind".to_owned(), IpcValue::String("terminal".to_owned())),
        ("reason".to_owned(), IpcValue::String(reason.to_owned())),
        (
            "replacedItems".to_owned(),
            IpcValue::Number(counters.replaced_items.into()),
        ),
    ]);
    if let Some(error) = error {
        terminal.insert("error".to_owned(), error.clone());
    }
    let terminal = IpcValue::Object(terminal);
    let marked_encoding = has_ordinary_bytes_named_object(&terminal);
    // `IpcValue::String` is Rust UTF-8 and therefore cannot represent an
    // unpaired JavaScript surrogate. The ledger retains the validated raw
    // UTF-16 sidecar precisely for this serialization boundary.
    format!(
        "{{{}\"formatVersion\":{FORMAT_VERSION},\"scope\":{{\"incarnation\":{},\"managerId\":{},\"operationId\":{}}},\"terminal\":{}}}",
        if marked_encoding {
            format!("\"encodingVersion\":{ENCODING_VERSION},")
        } else {
            String::new()
        },
        ubm_desktop::terminal_evidence_ledger::canonical_scope_json_string(
            scope.scope_utf16le.as_deref(),
            3,
            &scope.incarnation,
        ),
        ubm_desktop::terminal_evidence_ledger::canonical_scope_json_string(
            scope.scope_utf16le.as_deref(),
            1,
            &scope.manager_id,
        ),
        ubm_desktop::terminal_evidence_ledger::canonical_scope_json_string(
            scope.scope_utf16le.as_deref(),
            2,
            &scope.operation_id,
        ),
        stable_json(&terminal, marked_encoding),
    )
    .into_bytes()
}

fn has_ordinary_bytes_named_object(value: &IpcValue) -> bool {
    match value {
        IpcValue::Array(values) => values.iter().any(has_ordinary_bytes_named_object),
        IpcValue::Object(values) => {
            (values.len() == 1 && values.contains_key("$bytes"))
                || values.values().any(has_ordinary_bytes_named_object)
        }
        IpcValue::Null
        | IpcValue::Bool(_)
        | IpcValue::Number(_)
        | IpcValue::String(_)
        | IpcValue::Bytes(_) => false,
    }
}

fn stable_json(value: &IpcValue, marked_encoding: bool) -> String {
    match value {
        IpcValue::Null => "null".to_owned(),
        IpcValue::Bool(value) => value.to_string(),
        IpcValue::Number(value) => value.to_string(),
        IpcValue::String(value) => serde_json::to_string(value).expect("string serialization"),
        IpcValue::Bytes(value) => format!(
            r#"{{"$bytes":"{}"}}"#,
            value
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
        IpcValue::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|value| stable_json(value, marked_encoding))
                .collect::<Vec<_>>()
                .join(",")
        ),
        IpcValue::Object(values)
            if marked_encoding && values.len() == 1 && values.contains_key("$bytes") =>
        {
            let value = values.get("$bytes").expect("single key is present");
            format!("{{\"$bytes\":[{}]}}", stable_json(value, true))
        }
        IpcValue::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{}:{}",
                    serde_json::to_string(key).expect("key serialization"),
                    stable_json(value, marked_encoding)
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Scope {
    pub manager_id: String,
    pub operation_id: String,
    pub incarnation: String,
    pub scope_utf16le: Option<Vec<u8>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Reserve {
    pub producer: String,
    pub maximum_bytes: u64,
    pub scope: Scope,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Commit {
    pub reservation_id: String,
    pub digest: String,
    pub canonical_bytes: Vec<u8>,
}

pub(super) fn parse_source_scope(
    payload: &BTreeMap<String, IpcValue>,
    producer: &str,
    operation: &'static str,
) -> Result<Option<ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope>, DispatchError> {
    let Some(value) = payload.get("terminalEvidenceScope") else {
        return Ok(None);
    };
    let scope = into_object(value.clone(), operation)?;
    exact_scope_keys(&scope, operation)?;
    Ok(Some(
        ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
            producer: producer.to_owned(),
            manager_id: required_string(&scope, "managerId", operation)?,
            operation_id: required_string(&scope, "operationId", operation)?,
            incarnation: required_string(&scope, "incarnation", operation)?,
            scope_utf16le: optional_scope_utf16le(&scope, operation)?,
        },
    ))
}

fn malformed(operation: &'static str) -> DispatchError {
    DispatchError::new(BleErrorCode::ProtocolMalformed, "ipc", operation)
}

fn argument(operation: &'static str) -> DispatchError {
    DispatchError::new(BleErrorCode::ArgumentInvalid, "restoration", operation)
}

fn exact_keys(
    payload: &BTreeMap<String, IpcValue>,
    expected: &[&str],
    operation: &'static str,
) -> Result<(), DispatchError> {
    (payload.len() == expected.len() && payload.keys().all(|key| expected.contains(&key.as_str())))
        .then_some(())
        .ok_or_else(|| malformed(operation))
}

fn v1(payload: &BTreeMap<String, IpcValue>, operation: &'static str) -> Result<(), DispatchError> {
    matches!(payload.get("version"), Some(IpcValue::Number(value)) if value.as_i64() == Some(FORMAT_VERSION))
        .then_some(())
        .ok_or_else(|| malformed(operation))
}

pub(super) fn parse_reserve(
    payload: &BTreeMap<String, IpcValue>,
) -> Result<Reserve, DispatchError> {
    const OPERATION: &str = "tauri.terminal-evidence.reserve";
    exact_keys(
        payload,
        &[
            "version",
            "producer",
            "maximumBytes",
            "formatVersion",
            "scope",
        ],
        OPERATION,
    )?;
    v1(payload, OPERATION)?;
    matches!(payload.get("formatVersion"), Some(IpcValue::Number(value)) if value.as_i64() == Some(FORMAT_VERSION))
        .then_some(())
        .ok_or_else(|| malformed(OPERATION))?;
    let producer = required_string(payload, "producer", OPERATION)?;
    if producer != "scan" && producer != "subscription" {
        return Err(argument(OPERATION));
    }
    let maximum_bytes = match payload.get("maximumBytes") {
        Some(IpcValue::Number(value)) => value.as_u64(),
        _ => None,
    }
    .filter(|value| *value > 0)
    .ok_or_else(|| argument(OPERATION))?;
    let scope = into_object(
        required_value(payload, "scope", OPERATION)?.clone(),
        OPERATION,
    )?;
    exact_scope_keys(&scope, OPERATION)?;
    Ok(Reserve {
        producer,
        maximum_bytes,
        scope: Scope {
            manager_id: required_string(&scope, "managerId", OPERATION)?,
            operation_id: required_string(&scope, "operationId", OPERATION)?,
            incarnation: required_string(&scope, "incarnation", OPERATION)?,
            scope_utf16le: optional_scope_utf16le(&scope, OPERATION)?,
        },
    })
}

/// The Tauri transport's tagged `Uint8Array` representation reaches this
/// parser as `IpcValue::Bytes`. It is deliberately nested in the scope so an
/// ordinary JSON numeric array cannot become an identity sidecar.
fn optional_scope_utf16le(
    scope: &BTreeMap<String, IpcValue>,
    operation: &'static str,
) -> Result<Option<Vec<u8>>, DispatchError> {
    match scope.get("scopeUtf16le") {
        None => Ok(None),
        Some(IpcValue::Bytes(bytes)) if !bytes.is_empty() => Ok(Some(bytes.clone())),
        _ => Err(malformed(operation)),
    }
}

fn exact_scope_keys(
    scope: &BTreeMap<String, IpcValue>,
    operation: &'static str,
) -> Result<(), DispatchError> {
    let expected = ["managerId", "operationId", "incarnation"];
    let expected_with_sidecar = ["managerId", "operationId", "incarnation", "scopeUtf16le"];
    if scope.contains_key("scopeUtf16le") {
        exact_keys(scope, &expected_with_sidecar, operation)
    } else {
        exact_keys(scope, &expected, operation)
    }
}

pub(super) fn parse_commit(
    payload: &BTreeMap<String, IpcValue>,
    binary_payload: Option<Vec<u8>>,
) -> Result<Commit, DispatchError> {
    const OPERATION: &str = "tauri.terminal-evidence.commit";
    exact_keys(payload, &["version", "reservationId", "digest"], OPERATION)?;
    v1(payload, OPERATION)?;
    let canonical_bytes = binary_payload.ok_or_else(|| malformed(OPERATION))?;
    if canonical_bytes.is_empty() {
        return Err(argument(OPERATION));
    }
    Ok(Commit {
        reservation_id: required_string(payload, "reservationId", OPERATION)?,
        digest: required_string(payload, "digest", OPERATION)?,
        canonical_bytes,
    })
}

pub(super) fn parse_reference(
    payload: &BTreeMap<String, IpcValue>,
    command: &'static str,
) -> Result<String, DispatchError> {
    exact_keys(payload, &["version", "ref"], command)?;
    v1(payload, command)?;
    required_string(payload, "ref", command)
}

pub(super) fn parse_release(payload: &BTreeMap<String, IpcValue>) -> Result<String, DispatchError> {
    const OPERATION: &str = "tauri.terminal-evidence.release";
    exact_keys(payload, &["version", "reservationId"], OPERATION)?;
    v1(payload, OPERATION)?;
    required_string(payload, "reservationId", OPERATION)
}

pub(super) fn parse_recovery_list(
    payload: &BTreeMap<String, IpcValue>,
) -> Result<(), DispatchError> {
    const OPERATION: &str = "tauri.terminal-evidence.recovery.list";
    exact_keys(payload, &["version"], OPERATION)?;
    v1(payload, OPERATION)
}

pub(super) fn parse_recovery_list_with_status(
    payload: &BTreeMap<String, IpcValue>,
) -> Result<(), DispatchError> {
    const OPERATION: &str = "tauri.terminal-evidence.recovery.list-with-status";
    exact_keys(payload, &["version"], OPERATION)?;
    v1(payload, OPERATION)
}

pub(super) fn parse_recovery_read(
    payload: &BTreeMap<String, IpcValue>,
) -> Result<(String, u64), DispatchError> {
    const OPERATION: &str = "tauri.terminal-evidence.recovery.read";
    exact_keys(payload, &["version", "handle", "maximumBytes"], OPERATION)?;
    v1(payload, OPERATION)?;
    let maximum_bytes = match payload.get("maximumBytes") {
        Some(IpcValue::Number(value)) => value.as_u64(),
        _ => None,
    }
    .filter(|value| *value > 0)
    .ok_or_else(|| argument(OPERATION))?;
    Ok((
        required_string(payload, "handle", OPERATION)?,
        maximum_bytes,
    ))
}

pub(super) fn parse_recovery_ack(
    payload: &BTreeMap<String, IpcValue>,
) -> Result<String, DispatchError> {
    const OPERATION: &str = "tauri.terminal-evidence.recovery.ack";
    exact_keys(payload, &["version", "handle"], OPERATION)?;
    v1(payload, OPERATION)?;
    required_string(payload, "handle", OPERATION)
}

pub(super) fn digest_bytes(digest: &str) -> Result<[u8; 32], DispatchError> {
    const OPERATION: &str = "tauri.terminal-evidence.commit";
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(argument(OPERATION));
    }
    let mut output = [0_u8; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digest[index * 2..index * 2 + 2], 16)
            .map_err(|_| argument(OPERATION))?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed_scope(fields: [&[u16]; 4]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for field in fields {
            bytes.extend_from_slice(&(field.len() as u32).to_le_bytes());
        }
        for field in fields {
            for unit in field {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
        }
        bytes
    }

    #[test]
    fn reserve_requires_the_v1_scope_and_a_positive_safe_byte_limit() {
        let request = BTreeMap::from([
            ("version".to_owned(), IpcValue::Number(1.into())),
            ("producer".to_owned(), IpcValue::String("scan".to_owned())),
            ("maximumBytes".to_owned(), IpcValue::Number(64.into())),
            ("formatVersion".to_owned(), IpcValue::Number(1.into())),
            (
                "scope".to_owned(),
                IpcValue::Object(BTreeMap::from([
                    (
                        "managerId".to_owned(),
                        IpcValue::String("manager".to_owned()),
                    ),
                    (
                        "operationId".to_owned(),
                        IpcValue::String("operation".to_owned()),
                    ),
                    (
                        "incarnation".to_owned(),
                        IpcValue::String("incarnation".to_owned()),
                    ),
                ])),
            ),
        ]);

        let parsed = parse_reserve(&request).expect("v1 reserve must parse");
        assert_eq!(parsed.producer, "scan");
        assert_eq!(parsed.maximum_bytes, 64);
        assert_eq!(parsed.scope.manager_id, "manager");

        let mut invalid = request;
        invalid.insert("maximumBytes".to_owned(), IpcValue::Number(0.into()));
        assert!(parse_reserve(&invalid).is_err());
    }

    #[test]
    fn reserve_and_source_scope_preserve_the_tagged_lossless_utf16_sidecar() {
        // JSON cannot carry the first field's unpaired high surrogate. The
        // visible scalar is its U+FFFD projection; the tagged byte value is
        // the authenticated, lossless identity consumed by the ledger.
        let producer = "scan".encode_utf16().collect::<Vec<_>>();
        let manager = vec![0x006d, 0xd800];
        let operation = "operation".encode_utf16().collect::<Vec<_>>();
        let incarnation = "incarnation".encode_utf16().collect::<Vec<_>>();
        let raw = packed_scope([
            producer.as_slice(),
            manager.as_slice(),
            operation.as_slice(),
            incarnation.as_slice(),
        ]);
        let scope = IpcValue::Object(BTreeMap::from([
            (
                "managerId".to_owned(),
                IpcValue::String("m\u{fffd}".to_owned()),
            ),
            (
                "operationId".to_owned(),
                IpcValue::String("operation".to_owned()),
            ),
            (
                "incarnation".to_owned(),
                IpcValue::String("incarnation".to_owned()),
            ),
            ("scopeUtf16le".to_owned(), IpcValue::Bytes(raw.clone())),
        ]));
        let reserve = parse_reserve(&BTreeMap::from([
            ("version".to_owned(), IpcValue::Number(1.into())),
            ("producer".to_owned(), IpcValue::String("scan".to_owned())),
            ("maximumBytes".to_owned(), IpcValue::Number(64.into())),
            ("formatVersion".to_owned(), IpcValue::Number(1.into())),
            ("scope".to_owned(), scope.clone()),
        ]))
        .expect("lossless reserve scope");
        assert_eq!(reserve.scope.scope_utf16le, Some(raw.clone()));

        let source = parse_source_scope(
            &BTreeMap::from([("terminalEvidenceScope".to_owned(), scope)]),
            "scan",
            "tauri.test.source-scope",
        )
        .expect("lossless source scope")
        .expect("scope");
        assert_eq!(source.scope_utf16le, Some(raw));
        assert_eq!(source.manager_id, "m\u{fffd}");
    }

    #[test]
    fn commit_keeps_bytes_out_of_json_payloads() {
        let payload = BTreeMap::from([
            ("version".to_owned(), IpcValue::Number(1.into())),
            (
                "reservationId".to_owned(),
                IpcValue::String("reservation".to_owned()),
            ),
            ("digest".to_owned(), IpcValue::String("digest".to_owned())),
        ]);

        let parsed = parse_commit(&payload, Some(vec![1, 2, 3])).expect("binary commit must parse");
        assert_eq!(parsed.reservation_id, "reservation");
        assert_eq!(parsed.canonical_bytes, vec![1, 2, 3]);
        assert!(parse_commit(&payload, None).is_err());
    }

    #[test]
    fn canonical_terminal_bytes_match_the_shared_v1_codec_fixture() {
        let scope = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
            producer: "scan".to_owned(),
            manager_id: "manager".to_owned(),
            operation_id: "operation".to_owned(),
            incarnation: "incarnation".to_owned(),
            scope_utf16le: None,
        };
        assert_eq!(
            String::from_utf8(encode_terminal(
                &scope,
                "closed",
                None,
                TerminalCounters::ZERO
            ))
            .expect("UTF-8 JSON"),
            r#"{"formatVersion":1,"scope":{"incarnation":"incarnation","managerId":"manager","operationId":"operation"},"terminal":{"droppedBytes":0,"droppedItems":0,"kind":"terminal","reason":"closed","replacedItems":0}}"#
        );
    }

    #[test]
    fn canonical_terminal_bytes_mark_and_escape_an_ordinary_bytes_named_object() {
        let scope = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
            producer: "scan".to_owned(),
            manager_id: "manager".to_owned(),
            operation_id: "operation".to_owned(),
            incarnation: "incarnation".to_owned(),
            scope_utf16le: None,
        };
        let error = IpcValue::Object(BTreeMap::from([(
            "$bytes".to_owned(),
            IpcValue::String("ordinary".to_owned()),
        )]));

        assert_eq!(
            String::from_utf8(encode_terminal(
                &scope,
                "closed",
                Some(&error),
                TerminalCounters::ZERO,
            ))
            .expect("UTF-8 JSON"),
            r#"{"encodingVersion":2,"formatVersion":1,"scope":{"incarnation":"incarnation","managerId":"manager","operationId":"operation"},"terminal":{"droppedBytes":0,"droppedItems":0,"error":{"$bytes":["ordinary"]},"kind":"terminal","reason":"closed","replacedItems":0}}"#,
        );
    }

    #[test]
    fn marked_terminal_encoding_escapes_nested_literals_but_keeps_real_bytes() {
        let scope = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
            producer: "scan".to_owned(),
            manager_id: "manager".to_owned(),
            operation_id: "operation".to_owned(),
            incarnation: "incarnation".to_owned(),
            scope_utf16le: None,
        };
        let error = IpcValue::Object(BTreeMap::from([(
            "array".to_owned(),
            IpcValue::Array(vec![
                IpcValue::Object(BTreeMap::from([(
                    "$bytes".to_owned(),
                    IpcValue::String("not-hex".to_owned()),
                )])),
                IpcValue::Bytes(vec![0, 255]),
                IpcValue::Object(BTreeMap::from([(
                    "nested".to_owned(),
                    IpcValue::Object(BTreeMap::from([("$bytes".to_owned(), IpcValue::Null)])),
                )])),
            ]),
        )]));

        assert_eq!(
            String::from_utf8(encode_terminal(
                &scope,
                "closed",
                Some(&error),
                TerminalCounters::ZERO,
            ))
            .expect("UTF-8 JSON"),
            r#"{"encodingVersion":2,"formatVersion":1,"scope":{"incarnation":"incarnation","managerId":"manager","operationId":"operation"},"terminal":{"droppedBytes":0,"droppedItems":0,"error":{"array":[{"$bytes":["not-hex"]},{"$bytes":"00ff"},{"nested":{"$bytes":[null]}}]},"kind":"terminal","reason":"closed","replacedItems":0}}"#,
        );
    }

    #[test]
    fn canonical_terminal_bytes_use_raw_utf16_scope_units_when_present() {
        let producer = "scan".encode_utf16().collect::<Vec<_>>();
        let manager = vec![0x006d, 0xd800];
        let operation = "operation".encode_utf16().collect::<Vec<_>>();
        let incarnation = "incarnation".encode_utf16().collect::<Vec<_>>();
        let scope = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
            producer: "scan".to_owned(),
            manager_id: "m\u{fffd}".to_owned(),
            operation_id: "operation".to_owned(),
            incarnation: "incarnation".to_owned(),
            scope_utf16le: Some(packed_scope([
                producer.as_slice(),
                manager.as_slice(),
                operation.as_slice(),
                incarnation.as_slice(),
            ])),
        };
        assert_eq!(
            String::from_utf8(encode_terminal(
                &scope,
                "closed",
                None,
                TerminalCounters::ZERO
            ))
            .expect("UTF-8 JSON"),
            r#"{"formatVersion":1,"scope":{"incarnation":"incarnation","managerId":"m\ud800","operationId":"operation"},"terminal":{"droppedBytes":0,"droppedItems":0,"kind":"terminal","reason":"closed","replacedItems":0}}"#
        );
    }

    #[test]
    fn canonical_terminal_bytes_preserve_nonzero_overflow_counters() {
        let scope = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
            producer: "subscription".to_owned(),
            manager_id: "manager".to_owned(),
            operation_id: "operation".to_owned(),
            incarnation: "incarnation".to_owned(),
            scope_utf16le: None,
        };

        assert_eq!(
            String::from_utf8(encode_terminal(
                &scope,
                "overflow",
                None,
                TerminalCounters::new(1, 23, 0),
            ))
            .expect("UTF-8 JSON"),
            r#"{"formatVersion":1,"scope":{"incarnation":"incarnation","managerId":"manager","operationId":"operation"},"terminal":{"droppedBytes":23,"droppedItems":1,"kind":"terminal","reason":"overflow","replacedItems":0}}"#
        );
    }

    #[tokio::test]
    async fn trusted_host_configuration_opens_one_real_registry_sidecar() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-tauri-terminal-evidence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir(&directory).expect("private fixture directory");
        let dispatcher =
            super::super::BtleplugDispatcher::new(super::super::BtleplugDispatcherOptions {
                terminal_evidence_maximum_bytes: Some(64),
                ..Default::default()
            });
        dispatcher
            .continuation_configure_recording_directory(&directory)
            .await
            .expect("trusted host configures directory");
        let ledger = dispatcher
            .terminal_evidence_ledger
            .lock()
            .expect("ledger mutex")
            .clone()
            .expect("trusted option installs ledger");
        let reservation = ledger
            .reserve(
                &ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
                    producer: "scan".to_owned(),
                    manager_id: "manager".to_owned(),
                    operation_id: "operation".to_owned(),
                    incarnation: "incarnation".to_owned(),
                    scope_utf16le: None,
                },
                16,
            )
            .expect("reserve before source activation");
        let bytes = b"terminal receipt";
        use sha2::Digest;

        let digest: [u8; 32] = sha2::Sha256::digest(bytes).into();
        let committed = ledger
            .commit(&reservation.reservation_id, bytes, digest)
            .expect("exact bytes synchronously committed");
        assert_eq!(
            ledger
                .read(&committed.reference, 16)
                .expect("read sidecar")
                .0,
            bytes
        );
        drop(ledger);
        drop(dispatcher);
        std::fs::remove_dir_all(directory).expect("remove fixture directory");
    }
}
