//! Data-only `CleanupRecord` wire codec (gap-logged in C-UBM.0.1.1-DRAFT).
//!
//! The frozen `contracts/src/cleanup.ts` validates a failure with
//! `candidate.error instanceof ContractError`, which cannot survive a JSON
//! crossing: after `JSON.parse` the error is a plain object, so a revived
//! record always fails validation. The gap log (contracts README §6) calls
//! for a data-only encode/decode pair mirrored in Rust. This module is the
//! Rust side of that pair: dependency-free canonical JSON over plain data,
//! so the wire form crosses any boundary (including FFI hosts) without
//! class identity.
//!
//! Wire schema (keys mirror the TypeScript camelCase revival shape):
//!
//! ```json
//! {"state":"released","operationId":null,"failures":[]}
//! {"state":"release-failed","operationId":"op-1",
//!  "failures":[{"resourceKind":"cccd","code":"gatt.subscribe-failed"}]}
//! ```
//!
//! - `encode` is infallible: every field is already validated, and escaping
//!   always succeeds.
//! - `decode` validates fail-closed with the same operation paths the
//!   TypeScript constructors throw (`cleanup.state`, `cleanup.failure`,
//!   `cleanup.released-failures`, `cleanup.release-failed-failures`), all as
//!   `protocol.malformed|boundary`. Oversize input fails
//!   `bytes.too-large|core|cleanup.wire.input`.
//! - `operationId` is always encoded and optional on decode (absent or null
//!   revives to `None`): pre-codec TypeScript JSON carries no operation id,
//!   and stays decodable.
//!
//! Intentional divergences from the frozen TypeScript shape (reported, never
//! edited here):
//!
//! - Rust failures store `{resourceKind, code}` only, while TypeScript stores
//!   `{resourceKind, error: ContractError}` with `code` + `domain` +
//!   `operation`. The wire carries exactly what Rust stores; a TypeScript
//!   revival must attribute `domain`/`operation` at its own layer.
//! - Rust records carry `operationId`; TypeScript records do not. The field
//!   is nullable and ignored by `makeCleanupRecord` at runtime, so the Rust
//!   wire stays forward-compatible.
//! - Full revival (`WireCleanupRecord` back into [`CleanupRecord`]) awaits a
//!   `CleanupFailure` constructor owned by the kernel slice: failure fields
//!   are private to `crate::ownership` with no public constructor, so this
//!   module cannot rebuild failures for non-empty records. Until that
//!   constructor lands, decode yields the validated [`WireCleanupRecord`]
//!   and [`WireCleanupRecord::matches`] proves equality against a live
//!   [`CleanupRecord`]. The pending API is
//!   `CleanupFailure::new(resource_kind, code)` validating a non-empty
//!   `resourceKind` (mirroring TypeScript `requireFailure`), plus
//!   `WireCleanupRecord::revive` on top of it.

use crate::contracts::{BleErrorCode, BleErrorDomain, CoreError, MAX_OPERATION_BYTES};
use crate::ownership::{CleanupRecord, CleanupState};

/// Cap on wire input length. Cleanup batches are bounded server-side by the
/// kernel; the wire form of one record must fit the frozen operation
/// ceiling, exactly like any other payload.
pub const MAX_WIRE_BYTES: u64 = MAX_OPERATION_BYTES;

/// Validated data-only cleanup failure: plain `{resourceKind, code}` with no
/// class identity, so it survives JSON (and any FFI string crossing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireCleanupFailure {
    resource_kind: String,
    code: BleErrorCode,
}

impl WireCleanupFailure {
    /// Borrow the resource kind (always non-empty after [`decode`]).
    #[must_use]
    pub fn resource_kind(&self) -> &str {
        &self.resource_kind
    }

    /// Borrow the frozen failure code.
    #[must_use]
    pub const fn code(&self) -> BleErrorCode {
        self.code
    }
}

/// Validated data-only cleanup record: the [`decode`] output. Compares
/// against a live [`CleanupRecord`] with [`WireCleanupRecord::matches`] and
/// re-encodes canonically with [`WireCleanupRecord::encode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireCleanupRecord {
    operation_id: Option<String>,
    state: CleanupState,
    failures: Vec<WireCleanupFailure>,
}

impl WireCleanupRecord {
    /// Borrow the operation id (`None` when absent or null on the wire).
    #[must_use]
    pub const fn operation_id(&self) -> Option<&String> {
        self.operation_id.as_ref()
    }

    /// Cleanup state.
    #[must_use]
    pub const fn state(&self) -> CleanupState {
        self.state
    }

    /// Borrow validated failures.
    #[must_use]
    pub fn failures(&self) -> &[WireCleanupFailure] {
        &self.failures
    }

    /// Re-encode canonically. `decode(encode(record))` is a fixed point:
    /// decode then re-encode yields the identical string.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::new();
        encode_parts(
            &mut out,
            self.state,
            self.operation_id.as_deref(),
            self.failures
                .iter()
                .map(|failure| (failure.resource_kind.as_str(), failure.code)),
        );
        out
    }

    /// Semantic equality against a live core record: same state, same
    /// operation id, same failures in order.
    #[must_use]
    pub fn matches(&self, record: &CleanupRecord) -> bool {
        if self.state != record.state() {
            return false;
        }
        if self.operation_id.as_deref() != record.operation_id().map(|id| id.as_str()) {
            return false;
        }
        if self.failures.len() != record.failures().len() {
            return false;
        }
        self.failures
            .iter()
            .zip(record.failures().iter())
            .all(|(wire, live)| {
                wire.resource_kind == live.resource_kind() && wire.code == live.code()
            })
    }
}

/// Encode a live record to canonical wire JSON (fixed key order, no
/// whitespace). Infallible: fields are already validated and escaping always
/// succeeds.
#[must_use]
pub fn encode(record: &CleanupRecord) -> String {
    let mut out = String::new();
    encode_parts(
        &mut out,
        record.state(),
        record.operation_id().map(|id| id.as_str()),
        record
            .failures()
            .iter()
            .map(|failure| (failure.resource_kind(), failure.code())),
    );
    out
}

/// Shared canonical writer: fixed key order (`state`, `operationId`,
/// `failures`; `resourceKind`, `code`), no whitespace.
fn encode_parts<'a>(
    out: &mut String,
    state: CleanupState,
    operation_id: Option<&str>,
    failures: impl Iterator<Item = (&'a str, BleErrorCode)>,
) {
    out.push_str("{\"state\":\"");
    out.push_str(match state {
        CleanupState::Released => "released",
        CleanupState::ReleaseFailed => "release-failed",
    });
    out.push_str("\",\"operationId\":");
    match operation_id {
        Some(id) => append_escaped(out, id),
        None => out.push_str("null"),
    }
    out.push_str(",\"failures\":[");
    let mut first = true;
    for (resource_kind, code) in failures {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str("{\"resourceKind\":");
        append_escaped(out, resource_kind);
        out.push_str(",\"code\":\"");
        out.push_str(code.as_str());
        out.push_str("\"}");
    }
    out.push_str("]}");
}

/// Append a JSON string literal. Short escapes for the C0 controls with
/// dedicated forms, `\u00XX` for the rest; all other characters (including
/// non-BMP) cross as UTF-8.
fn append_escaped(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let unit = c as u32;
                out.push_str("\\u00");
                out.push(HEX[((unit >> 4) & 0xF) as usize] as char);
                out.push(HEX[(unit & 0xF) as usize] as char);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Decode and validate wire JSON into its data-only form. Every shape
/// violation fails closed as `protocol.malformed|boundary`; oversize input
/// fails `bytes.too-large|core|cleanup.wire.input`.
pub fn decode(input: &str) -> Result<WireCleanupRecord, CoreError> {
    if input.len() as u64 > MAX_WIRE_BYTES {
        return Err(CoreError::new(
            BleErrorCode::BytesTooLarge,
            BleErrorDomain::Core,
            "cleanup.wire.input",
        ));
    }
    let mut parser = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    let record = parser.parse_record()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(malformed("cleanup.wire.input"));
    }
    Ok(record)
}

fn malformed(operation: &'static str) -> CoreError {
    CoreError::new(
        BleErrorCode::ProtocolMalformed,
        BleErrorDomain::Boundary,
        operation,
    )
}

/// Minimal strict-JSON reader for exactly this schema: objects with
/// string keys, string/null values, and one array of flat objects. No
/// recursion (fixed depth), no numbers, no `true`/`false`: anything outside
/// the schema fails closed.
struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect_byte(&mut self, expected: u8) -> Result<(), CoreError> {
        match self.peek() {
            Some(found) if found == expected => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(malformed("cleanup.wire.input")),
        }
    }

    /// Parse a JSON string starting at the opening quote. Unescaped C0
    /// controls, bad escapes, and lone surrogates fail closed; lone
    /// low surrogates and astral halves that never pair fail closed too.
    fn parse_string(&mut self) -> Result<String, CoreError> {
        self.expect_byte(b'"')?;
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            let byte = match self.peek() {
                Some(byte) => byte,
                None => return Err(malformed("cleanup.wire.input")),
            };
            match byte {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                b'\\' => {
                    self.pos += 1;
                    self.parse_escape(&mut bytes)?;
                }
                0x00..=0x1F => return Err(malformed("cleanup.wire.input")),
                _ => {
                    bytes.push(byte);
                    self.pos += 1;
                }
            }
        }
        String::from_utf8(bytes).map_err(|_| malformed("cleanup.wire.input"))
    }

    fn parse_escape(&mut self, out: &mut Vec<u8>) -> Result<(), CoreError> {
        let byte = match self.peek() {
            Some(byte) => byte,
            None => return Err(malformed("cleanup.wire.input")),
        };
        self.pos += 1;
        match byte {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                let unit = self.parse_hex4()?;
                let point = if (0xD800..0xDC00).contains(&unit) {
                    self.expect_byte(b'\\')?;
                    match self.peek() {
                        Some(b'u') => self.pos += 1,
                        _ => return Err(malformed("cleanup.wire.input")),
                    }
                    let low = self.parse_hex4()?;
                    if !(0xDC00..0xE000).contains(&low) {
                        return Err(malformed("cleanup.wire.input"));
                    }
                    0x1_0000 + ((unit - 0xD800) << 10) + (low - 0xDC00)
                } else if (0xDC00..0xE000).contains(&unit) {
                    return Err(malformed("cleanup.wire.input"));
                } else {
                    unit
                };
                let ch = char::from_u32(point).ok_or_else(|| malformed("cleanup.wire.input"))?;
                let mut encoded = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
            }
            _ => return Err(malformed("cleanup.wire.input")),
        }
        Ok(())
    }

    fn parse_hex4(&mut self) -> Result<u32, CoreError> {
        let mut value: u32 = 0;
        for _ in 0..4 {
            let byte = match self.peek() {
                Some(byte) => byte,
                None => return Err(malformed("cleanup.wire.input")),
            };
            let digit = match byte {
                b'0'..=b'9' => u32::from(byte - b'0'),
                b'a'..=b'f' => u32::from(byte - b'a') + 10,
                b'A'..=b'F' => u32::from(byte - b'A') + 10,
                _ => return Err(malformed("cleanup.wire.input")),
            };
            // Four hex digits top out at 0xFFFF: no overflow possible.
            value = value * 16 + digit;
            self.pos += 1;
        }
        Ok(value)
    }

    /// Consume a `null` literal without accepting a longer identifier prefix.
    /// A trailing identifier character is caught by the caller expecting a
    /// separator, so `nullx` still fails closed.
    fn consume_null(&mut self) -> Result<bool, CoreError> {
        let end = match self.pos.checked_add(4) {
            Some(end) => end,
            None => return Err(malformed("cleanup.wire.input")),
        };
        match self.bytes.get(self.pos..end) {
            Some(slice) if slice == b"null" => {
                self.pos = end;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn parse_record(&mut self) -> Result<WireCleanupRecord, CoreError> {
        self.skip_ws();
        self.expect_byte(b'{')?;
        let mut state: Option<CleanupState> = None;
        let mut operation_id: Option<String> = None;
        let mut seen_operation_id = false;
        let mut failures: Option<Vec<WireCleanupFailure>> = None;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
        } else {
            loop {
                let key = self.parse_string()?;
                self.skip_ws();
                self.expect_byte(b':')?;
                self.skip_ws();
                match key.as_str() {
                    "state" => {
                        if state.is_some() {
                            return Err(malformed("cleanup.wire.input"));
                        }
                        let text = self.parse_string()?;
                        state = Some(match text.as_str() {
                            "released" => CleanupState::Released,
                            "release-failed" => CleanupState::ReleaseFailed,
                            _ => return Err(malformed("cleanup.state")),
                        });
                    }
                    "operationId" => {
                        if seen_operation_id {
                            return Err(malformed("cleanup.wire.input"));
                        }
                        seen_operation_id = true;
                        if self.consume_null()? {
                            operation_id = None;
                        } else {
                            let text = self.parse_string()?;
                            if text.is_empty() {
                                return Err(malformed("cleanup.wire.operation-id"));
                            }
                            operation_id = Some(text);
                        }
                    }
                    "failures" => {
                        if failures.is_some() {
                            return Err(malformed("cleanup.wire.input"));
                        }
                        failures = Some(self.parse_failures()?);
                    }
                    _ => return Err(malformed("cleanup.wire.input")),
                }
                self.skip_ws();
                match self.peek() {
                    Some(b',') => {
                        self.pos += 1;
                        self.skip_ws();
                    }
                    Some(b'}') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(malformed("cleanup.wire.input")),
                }
            }
        }
        let state = match state {
            Some(state) => state,
            None => return Err(malformed("cleanup.state")),
        };
        let failures = match failures {
            Some(failures) => failures,
            None => return Err(malformed("cleanup.failure")),
        };
        match state {
            CleanupState::Released if !failures.is_empty() => {
                return Err(malformed("cleanup.released-failures"));
            }
            CleanupState::ReleaseFailed if failures.is_empty() => {
                return Err(malformed("cleanup.release-failed-failures"));
            }
            _ => {}
        }
        Ok(WireCleanupRecord {
            operation_id,
            state,
            failures,
        })
    }

    fn parse_failures(&mut self) -> Result<Vec<WireCleanupFailure>, CoreError> {
        self.expect_byte(b'[')?;
        let mut failures = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(failures);
        }
        loop {
            failures.push(self.parse_failure()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(malformed("cleanup.wire.input")),
            }
        }
        Ok(failures)
    }

    fn parse_failure(&mut self) -> Result<WireCleanupFailure, CoreError> {
        self.skip_ws();
        self.expect_byte(b'{')?;
        let mut resource_kind: Option<String> = None;
        let mut code: Option<BleErrorCode> = None;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
        } else {
            loop {
                let key = self.parse_string()?;
                self.skip_ws();
                self.expect_byte(b':')?;
                self.skip_ws();
                match key.as_str() {
                    "resourceKind" => {
                        if resource_kind.is_some() {
                            return Err(malformed("cleanup.wire.input"));
                        }
                        let text = self.parse_string()?;
                        if text.is_empty() {
                            return Err(malformed("cleanup.failure"));
                        }
                        resource_kind = Some(text);
                    }
                    "code" => {
                        if code.is_some() {
                            return Err(malformed("cleanup.wire.input"));
                        }
                        let text = self.parse_string()?;
                        code = Some(match BleErrorCode::from_str(text.as_str()) {
                            Some(code) => code,
                            None => return Err(malformed("cleanup.failure")),
                        });
                    }
                    _ => return Err(malformed("cleanup.wire.input")),
                }
                self.skip_ws();
                match self.peek() {
                    Some(b',') => {
                        self.pos += 1;
                        self.skip_ws();
                    }
                    Some(b'}') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(malformed("cleanup.wire.input")),
                }
            }
        }
        let resource_kind = match resource_kind {
            Some(kind) => kind,
            None => return Err(malformed("cleanup.failure")),
        };
        let code = match code {
            Some(code) => code,
            None => return Err(malformed("cleanup.failure")),
        };
        Ok(WireCleanupFailure {
            resource_kind,
            code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{
        AttachmentTuple, Contender, ContenderKind, Generation, HandshakeState, LeaseId, OperationId,
    };
    use crate::ownership::{EffectBatch, Kernel, KernelConfig, KernelInput};

    fn operation_id(value: &str) -> OperationId {
        match OperationId::new(value) {
            Ok(id) => id,
            Err(_) => operation_id("test-operation"),
        }
    }

    fn test_kernel() -> Kernel {
        match Generation::new("gen-1") {
            Ok(generation) => Kernel::new(
                KernelConfig::default(),
                AttachmentTuple::new(
                    match crate::contracts::AttachmentId::new("attach-01") {
                        Ok(id) => id,
                        Err(_) => return test_kernel(),
                    },
                    match crate::contracts::BackendInstanceId::new("backend-01") {
                        Ok(id) => id,
                        Err(_) => return test_kernel(),
                    },
                    match crate::contracts::BackendGeneration::new("bg-3") {
                        Ok(generation) => generation,
                        Err(_) => return test_kernel(),
                    },
                    match crate::contracts::AdapterId::new("adapter-01") {
                        Ok(id) => id,
                        Err(_) => return test_kernel(),
                    },
                    match crate::contracts::AdapterGeneration::new("ag-2") {
                        Ok(generation) => generation,
                        Err(_) => return test_kernel(),
                    },
                ),
                generation,
                HandshakeState { complete: true },
            ),
            Err(_) => test_kernel(),
        }
    }

    fn admit_settled(kernel: &mut Kernel, out: &mut EffectBatch, id: &str) {
        let attachment = AttachmentTuple::new(
            match crate::contracts::AttachmentId::new("attach-01") {
                Ok(id) => id,
                Err(_) => {
                    crate::check(false, "test attachment must validate");
                    return;
                }
            },
            match crate::contracts::BackendInstanceId::new("backend-01") {
                Ok(id) => id,
                Err(_) => {
                    crate::check(false, "test attachment must validate");
                    return;
                }
            },
            match crate::contracts::BackendGeneration::new("bg-3") {
                Ok(generation) => generation,
                Err(_) => {
                    crate::check(false, "test attachment must validate");
                    return;
                }
            },
            match crate::contracts::AdapterId::new("adapter-01") {
                Ok(id) => id,
                Err(_) => {
                    crate::check(false, "test attachment must validate");
                    return;
                }
            },
            match crate::contracts::AdapterGeneration::new("ag-2") {
                Ok(generation) => generation,
                Err(_) => {
                    crate::check(false, "test attachment must validate");
                    return;
                }
            },
        );
        let owner = match LeaseId::new("lease-1") {
            Ok(lease) => lease,
            Err(_) => {
                crate::check(false, "test lease must validate");
                return;
            }
        };
        let operation = operation_id(id);
        let generation = match Generation::new("gen-1") {
            Ok(generation) => generation,
            Err(_) => {
                crate::check(false, "test generation must validate");
                return;
            }
        };
        match kernel.handle(
            KernelInput::Admit {
                operation_id: operation.clone(),
                owner,
                attachment,
                generation: generation.clone(),
                timeout_ms: 1_000,
            },
            0,
            out,
        ) {
            Ok(_) => {}
            Err(_) => crate::check(false, "test admit must succeed"),
        }
        match kernel.handle(
            KernelInput::Dispatch {
                operation_id: operation.clone(),
                generation: generation.clone(),
            },
            10,
            out,
        ) {
            Ok(_) => {}
            Err(_) => {
                crate::check(false, "test dispatch must succeed");
                return;
            }
        }
        // Settle to terminal: release reports only reap terminal operations.
        match kernel.handle(
            KernelInput::Complete {
                operation_id: operation,
                generation,
                contender: Contender {
                    ingress_ordinal: 1,
                    kind: ContenderKind::Success,
                    valid: true,
                },
            },
            20,
            out,
        ) {
            Ok(_) => {}
            Err(_) => crate::check(false, "test complete must succeed"),
        }
    }

    /// Drive a real kernel to a retained `release-failed` record: the only
    /// failure source reachable through the public API. Returns `None` when
    /// the kernel misbehaves so tests fail through `check`, never a panic.
    fn kernel_failed_record() -> Option<CleanupRecord> {
        let mut kernel = test_kernel();
        let mut out = EffectBatch::new(16);
        admit_settled(&mut kernel, &mut out, "op-9");
        match kernel.handle(
            KernelInput::ReleaseReport {
                operation_id: operation_id("op-9"),
                ok: false,
                code: Some(BleErrorCode::ConnectionLost),
            },
            20,
            &mut out,
        ) {
            Ok(_) => {}
            Err(_) => {
                crate::check(false, "test release report must succeed");
                return None;
            }
        }
        let mut drained = kernel.drain_cleanup(8);
        if drained.len() != 1 {
            crate::check(false, "one retained record expected");
            return None;
        }
        drained.pop()
    }

    #[test]
    fn released_record_encodes_canonically() {
        let record = match CleanupRecord::new(None, CleanupState::Released, Vec::new()) {
            Ok(record) => record,
            Err(_) => {
                crate::check(false, "released record must construct");
                return;
            }
        };
        assert_eq!(
            encode(&record),
            "{\"state\":\"released\",\"operationId\":null,\"failures\":[]}"
        );
    }

    #[test]
    fn released_record_with_operation_id_encodes() {
        let record = match CleanupRecord::new(
            Some(operation_id("op-1")),
            CleanupState::Released,
            Vec::new(),
        ) {
            Ok(record) => record,
            Err(_) => {
                crate::check(false, "released record must construct");
                return;
            }
        };
        assert_eq!(
            encode(&record),
            "{\"state\":\"released\",\"operationId\":\"op-1\",\"failures\":[]}"
        );
    }

    #[test]
    fn kernel_failed_record_round_trips_through_wire() {
        let record = match kernel_failed_record() {
            Some(record) => record,
            None => return,
        };
        assert_eq!(record.state(), CleanupState::ReleaseFailed);
        assert_eq!(record.failures().len(), 1);
        let wire = encode(&record);
        assert_eq!(
            wire,
            "{\"state\":\"release-failed\",\"operationId\":\"op-9\",\
             \"failures\":[{\"resourceKind\":\"operation\",\"code\":\"connection.lost\"}]}"
        );
        let revived = match decode(&wire) {
            Ok(revived) => revived,
            Err(_) => {
                crate::check(false, "kernel wire must decode");
                return;
            }
        };
        assert!(revived.matches(&record));
        assert_eq!(revived.encode(), wire, "decode then re-encode is fixed");
    }

    #[test]
    fn decode_accepts_absent_operation_id_for_ts_shaped_json() {
        let wire = "{\"state\":\"release-failed\",\
            \"failures\":[{\"resourceKind\":\"cccd\",\"code\":\"gatt.subscribe-failed\"}]}";
        let revived = match decode(wire) {
            Ok(revived) => revived,
            Err(_) => {
                crate::check(false, "ts-shaped wire must decode");
                return;
            }
        };
        assert_eq!(revived.operation_id(), None);
        assert_eq!(revived.state(), CleanupState::ReleaseFailed);
        assert_eq!(revived.failures().len(), 1);
        assert_eq!(revived.failures()[0].resource_kind(), "cccd");
        assert_eq!(
            revived.failures()[0].code(),
            BleErrorCode::GattSubscribeFailed
        );
    }

    #[test]
    fn decode_accepts_whitespace_and_field_reorder() {
        let wire =
            " { \"failures\" : [] , \"operationId\" : \"op-2\" , \"state\" : \"released\" } ";
        let revived = match decode(wire) {
            Ok(revived) => revived,
            Err(_) => {
                crate::check(false, "reordered wire must decode");
                return;
            }
        };
        assert_eq!(
            revived.operation_id(),
            Some(&String::from("op-2")),
            "operation id must survive reorder"
        );
        assert_eq!(
            revived.encode(),
            "{\"state\":\"released\",\"operationId\":\"op-2\",\"failures\":[]}"
        );
    }

    #[test]
    fn wire_escapes_survive_host_string_crossings() {
        let wire = "{\"state\":\"release-failed\",\"operationId\":\"a\\\"b\\\\c\\u00e9\\n\",\
            \"failures\":[{\"resourceKind\":\"x\\u0041y\",\"code\":\"connection.lost\"}]}";
        let revived = match decode(wire) {
            Ok(revived) => revived,
            Err(_) => {
                crate::check(false, "escaped wire must decode");
                return;
            }
        };
        assert_eq!(revived.operation_id(), Some(&String::from("a\"b\\cé\n")));
        assert_eq!(revived.failures()[0].resource_kind(), "xAy");
        let canonical = revived.encode();
        let again = match decode(&canonical) {
            Ok(again) => again,
            Err(_) => {
                crate::check(false, "canonical re-encode must decode");
                return;
            }
        };
        assert_eq!(again, revived);
    }

    #[test]
    fn mismatched_wire_does_not_match_live_record() {
        let record = match kernel_failed_record() {
            Some(record) => record,
            None => return,
        };
        let other = match decode(
            "{\"state\":\"release-failed\",\"operationId\":\"op-9\",\
             \"failures\":[{\"resourceKind\":\"operation\",\"code\":\"connection.failed\"}]}",
        ) {
            Ok(other) => other,
            Err(_) => {
                crate::check(false, "decodable wire expected");
                return;
            }
        };
        assert!(!other.matches(&record), "different cause must not match");
        let released = match decode("{\"state\":\"released\",\"operationId\":null,\"failures\":[]}")
        {
            Ok(released) => released,
            Err(_) => {
                crate::check(false, "decodable wire expected");
                return;
            }
        };
        assert!(!released.matches(&record), "different state must not match");
    }

    #[test]
    fn decode_rejects_bad_state_and_inconsistent_records() {
        for (wire, operation) in [
            (
                "{\"state\":\"quarantined\",\"operationId\":null,\"failures\":[]}",
                "cleanup.state",
            ),
            (
                "{\"state\":\"released\",\"operationId\":null,\
                  \"failures\":[{\"resourceKind\":\"link\",\"code\":\"connection.lost\"}]}",
                "cleanup.released-failures",
            ),
            (
                "{\"state\":\"release-failed\",\"operationId\":null,\"failures\":[]}",
                "cleanup.release-failed-failures",
            ),
        ] {
            match decode(wire) {
                Ok(_) => crate::check(false, "inconsistent wire must reject"),
                Err(err) => {
                    assert_eq!(err.code(), BleErrorCode::ProtocolMalformed);
                    assert_eq!(err.domain(), BleErrorDomain::Boundary);
                    assert_eq!(err.operation(), operation);
                }
            }
        }
    }

    #[test]
    fn decode_rejects_bad_failures() {
        for wire in [
            // Empty resource kind (mirrors TypeScript `requireFailure`).
            "{\"state\":\"release-failed\",\"operationId\":null,\
              \"failures\":[{\"resourceKind\":\"\",\"code\":\"connection.lost\"}]}",
            // Unknown frozen code.
            "{\"state\":\"release-failed\",\"operationId\":null,\
              \"failures\":[{\"resourceKind\":\"link\",\"code\":\"connection.vanished\"}]}",
            // Empty operation id string (operation ids are never empty).
            "{\"state\":\"released\",\"operationId\":\"\",\"failures\":[]}",
            // Unknown top-level key fails closed.
            "{\"state\":\"released\",\"operationId\":null,\"failures\":[],\"extra\":1}",
            // Duplicate keys fail closed.
            "{\"state\":\"released\",\"state\":\"released\",\"operationId\":null,\"failures\":[]}",
            // Wrong types fail closed.
            "{\"state\":7,\"operationId\":null,\"failures\":[]}",
            "{\"state\":\"released\",\"operationId\":null,\"failures\":{}}",
            // Missing keys fail closed.
            "{\"operationId\":null,\"failures\":[]}",
            "{\"state\":\"released\",\"operationId\":null}",
            // Truncated input and trailing data fail closed.
            "{\"state\":\"released\",\"operationId\":null,\"failures\":[",
            "{\"state\":\"released\",\"operationId\":null,\"failures\":[]} trailing",
            // Lone surrogate fails closed, never a replacement char.
            "{\"state\":\"released\",\"operationId\":\"\\ud800\",\"failures\":[]}",
            // Unescaped control character fails closed.
            "{\"state\":\"relea\tsed\",\"operationId\":null,\"failures\":[]}",
        ] {
            match decode(wire) {
                Ok(_) => crate::check(false, "bad failure wire must reject"),
                Err(err) => assert_eq!(
                    err.code(),
                    BleErrorCode::ProtocolMalformed,
                    "wire {wire} must fail protocol.malformed"
                ),
            }
        }
    }

    #[test]
    fn decode_rejects_oversize_input() {
        let mut big = String::from("{\"state\":\"released\",\"operationId\":null,\"failures\":[]}");
        while (big.len() as u64) <= MAX_WIRE_BYTES {
            big.push(' ');
        }
        match decode(&big) {
            Ok(_) => crate::check(false, "oversize wire must reject"),
            Err(err) => {
                assert_eq!(err.code(), BleErrorCode::BytesTooLarge);
                assert_eq!(err.operation(), "cleanup.wire.input");
            }
        }
    }
}
