//! HOST-ANDROID GATT bridge slice (trackourhealth/bun-mono#1188, U5/U9/U10).
//!
//! Android binder threads (GATT callbacks, scan callbacks) must never drive
//! the core or wait on radio I/O: [`CoreSession::enqueue_gatt_event`] only
//! validates and stores a wire line, while [`CoreSession::drain_gatt_events`]
//! applies the queued lines to the REAL session-owned
//! [`ubm_core::central::Central`] on a worker thread and returns one JSON
//! observation object per line (newline-joined).
//!
//! Wire form is positional and pipe-delimited: `kind|arg|...`. No argument
//! may itself contain `|` (host-side MUST-NOT, enforced fail-fast by the
//! Kotlin builders; the drain additionally enforces exact per-kind arity, so
//! any smuggled `|` shifts the position count and rejects
//! `argument.invalid` as DATA). Byte values cross as lowercase hex
//! (uppercase accepted).
//!
//! Step-level core rejections come back as DATA (`{"ok":false,...}` with the
//! frozen contract `code` + `domain`); only the session lifetime fails the
//! drain call itself. Unknown event kinds fail closed with
//! `capability.unsupported|capability`, never silently or faked.

use std::collections::VecDeque;

use ubm_core::central::{validate_scan_request, ScanPlatformEvent};
use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, BackendGeneration, BackendInstanceId,
    ContenderKind, Generation, MAX_OPERATION_BYTES,
};
use ubm_core::ownership::EffectBatch;

use super::core_backend::{central_error, CoreSession, EchoError, DRIVE_EFFECT_CAP};

/// Maximum queued GATT lines per session. Beyond this the enqueue rejects
/// `stream.quota`: back-pressure is explicit, never silent loss.
pub const GATT_QUEUE_CAP: usize = 1024;

/// Maximum wire length of one queued line (payload ceiling plus framing).
pub const GATT_WIRE_MAX: usize = MAX_OPERATION_BYTES as usize + 1024;

/// Maximum decoded notify value bytes accepted in one line.
pub const GATT_NOTIFY_MAX: usize = MAX_OPERATION_BYTES as usize;

fn enqueue_failed(detail: &'static str) -> EchoError {
    EchoError::new("argument.invalid", "core", "gatt-enqueue", detail)
}

fn drain_failed(code: &'static str, domain: &'static str, detail: &'static str) -> EchoError {
    EchoError::new(code, domain, "gatt-drain", detail)
}

fn drain_rejected(detail: &'static str) -> EchoError {
    EchoError::new("capability.unsupported", "capability", "gatt-drain", detail)
}

/// Queue type owned by [`CoreSession`]; the drain applies lines FIFO.
pub type GattQueue = VecDeque<String>;

/// Validates one wire line and pushes it. Never touches the central, so a
/// binder thread is never blocked behind core work.
pub fn enqueue_event(queue: &mut GattQueue, wire: &str) -> Result<usize, EchoError> {
    if wire.is_empty() {
        return Err(enqueue_failed("event-empty"));
    }
    if wire.len() > GATT_WIRE_MAX {
        return Err(EchoError::new(
            "bytes.too-large",
            "core",
            "gatt-enqueue",
            "event-exceeds-wire-max",
        ));
    }
    let kind = wire.split('|').next().unwrap_or("");
    if kind.is_empty() {
        return Err(enqueue_failed("event-kind-empty"));
    }
    if queue.len() >= GATT_QUEUE_CAP {
        return Err(EchoError::new(
            "stream.quota",
            "stream",
            "gatt-enqueue",
            "queue-full",
        ));
    }
    queue.push_back(String::from(wire));
    Ok(queue.len())
}

fn missing(field: &'static str) -> EchoError {
    EchoError::new("argument.invalid", "core", "gatt-drain", field)
}

/// Exact `kind|arg|...` position count per known kind (kind included).
/// Unknown kinds return `None` so they still fail closed with
/// `capability.unsupported` in the drive match; every known kind rejects a
/// short or over-long line with `argument.invalid` DATA before any core
/// transition runs.
fn expected_arity(kind: &str) -> Option<usize> {
    Some(match kind {
        "scan.start" => 7,
        "scan.platform-started" => 2,
        "scan.stop" => 3,
        "scan.platform-event" => 4,
        "peer.resolve" => 3,
        "connect" => 5,
        "link.established" => 2,
        "link.released" => 2,
        "disconnect" => 4,
        "peer.loss" => 3,
        "discovery.begin" => 2,
        "discovery.complete" => 2,
        "discovery.fail" => 2,
        "services-changed" => 2,
        "path.register" => 10,
        "read.start" => 4,
        "write.start" => 8,
        "op.dispatch" => 2,
        "op.settle" => 6,
        "op.cancel" => 3,
        "subscribe" => 8,
        "subscribe.enable-settled" => 4,
        "unsubscribe" => 4,
        "subscribe.disable-settled" => 3,
        "notify.deliver" => 3,
        "expire-sweep" => 2,
        "adapter.reset" => 2,
        "release" => 1,
        _ => return None,
    })
}

fn arg<'a>(parts: &'a [&'a str], index: usize, field: &'static str) -> Result<&'a str, EchoError> {
    parts.get(index).copied().ok_or(missing(field))
}

fn parse_u64(text: &str, _field: &'static str) -> Result<u64, EchoError> {
    match ubm_core::contracts::parse_u64_decimal(text) {
        Ok(value) => Ok(value),
        Err(core) => Err(EchoError::new(
            "bytes.invalid",
            "core",
            "gatt-drain",
            if core.operation() == "u64.range" {
                "u64.range"
            } else {
                "u64.input"
            },
        )),
    }
}

fn parse_usize(text: &str, field: &'static str) -> Result<usize, EchoError> {
    let value = parse_u64(text, field)?;
    usize::try_from(value).map_err(|_| missing(field))
}

fn parse_bool(text: &str, field: &'static str) -> Result<bool, EchoError> {
    match text {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(missing(field)),
    }
}

fn parse_opt_u64(text: &str, field: &'static str) -> Result<Option<u64>, EchoError> {
    if text.is_empty() || text == "-" {
        Ok(None)
    } else {
        parse_u64(text, field).map(Some)
    }
}

fn parse_hex(text: &str) -> Result<Vec<u8>, EchoError> {
    if !text.len().is_multiple_of(2) {
        return Err(EchoError::new(
            "bytes.invalid",
            "core",
            "gatt-drain",
            "hex-odd-length",
        ));
    }
    if text.len() / 2 > GATT_NOTIFY_MAX {
        return Err(EchoError::new(
            "bytes.too-large",
            "core",
            "gatt-drain",
            "notify-exceeds-max-operation-bytes",
        ));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_val(bytes[i])
            .ok_or_else(|| EchoError::new("bytes.invalid", "core", "gatt-drain", "hex-digit"))?;
        let lo = hex_val(bytes[i + 1])
            .ok_or_else(|| EchoError::new("bytes.invalid", "core", "gatt-drain", "hex-digit"))?;
        out.push(hi * 16 + lo);
        i += 2;
    }
    Ok(out)
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn contender_kind(name: &str) -> Result<ContenderKind, EchoError> {
    match name {
        "success" => Ok(ContenderKind::Success),
        "failure" => Ok(ContenderKind::Failure),
        "abort" => Ok(ContenderKind::Abort),
        "timeout" => Ok(ContenderKind::Timeout),
        "disconnect" => Ok(ContenderKind::Disconnect),
        "reset" => Ok(ContenderKind::Reset),
        "destroy" => Ok(ContenderKind::Destroy),
        "adapter-loss" => Ok(ContenderKind::AdapterLoss),
        "session-stop" => Ok(ContenderKind::SessionStop),
        "dispatch-begin" => Ok(ContenderKind::DispatchBegin),
        _ => Err(EchoError::new(
            "argument.invalid",
            "core",
            "gatt-drain",
            "contender-kind-unknown",
        )),
    }
}

fn scan_event(name: &str) -> Result<ScanPlatformEvent, EchoError> {
    match name {
        "platform-started" => Ok(ScanPlatformEvent::PlatformStarted),
        "stop" => Ok(ScanPlatformEvent::Stop),
        "platform-stopped" => Ok(ScanPlatformEvent::PlatformStopped),
        "source-closed" => Ok(ScanPlatformEvent::SourceClosed),
        "start-failed" => Ok(ScanPlatformEvent::StartFailed),
        "source-failed" => Ok(ScanPlatformEvent::SourceFailed),
        "overflow-error-policy" => Ok(ScanPlatformEvent::OverflowErrorPolicy),
        "reset" => Ok(ScanPlatformEvent::Reset),
        "stop-failed" => Ok(ScanPlatformEvent::StopFailed),
        _ => Err(EchoError::new(
            "argument.invalid",
            "core",
            "gatt-drain",
            "scan-event-unknown",
        )),
    }
}

/// Minimal JSON string escaper for observation values (ids carry no quotes
/// by construction, but never trust the host).
pub(crate) fn json_escape_into(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

fn ok_line(kind: &str, body: &str) -> String {
    let mut out = String::from("{\"ok\":true,\"event\":\"");
    json_escape_into(&mut out, kind);
    out.push('"');
    out.push_str(body);
    out.push('}');
    out
}

fn err_line(kind: &str, err: &EchoError) -> String {
    let mut out = String::from("{\"ok\":false,\"event\":\"");
    json_escape_into(&mut out, kind);
    out.push_str("\",\"code\":\"");
    json_escape_into(&mut out, err.code);
    out.push_str("\",\"domain\":\"");
    json_escape_into(&mut out, err.domain);
    out.push_str("\",\"operation\":\"");
    json_escape_into(&mut out, err.operation);
    out.push_str("\",\"detail\":\"");
    json_escape_into(&mut out, err.detail);
    out.push_str("\"}");
    out
}

/// Splice an `,"effects":[...],"observations":[...]` fragment into a
/// rendered `{...}` line before its closing brace. Total: a rendering that
/// somehow lacks the brace keeps its bytes and gains the fragment plus a
/// closing brace, so effects still surface instead of panicking the drain.
fn splice_fragment(rendered: &str, fragment: &str) -> String {
    let mut merged = String::with_capacity(rendered.len() + fragment.len() + 1);
    merged.push_str(rendered.strip_suffix('}').unwrap_or(rendered));
    merged.push_str(fragment);
    merged.push('}');
    merged
}

impl CoreSession {
    /// Applies every queued line FIFO to the session-owned central and
    /// returns newline-joined JSON observations. Each line drives with a
    /// fresh cap-64 batch, then surfaces BOTH the staged kernel effects
    /// (`effects`, for the host to execute) and the drained typed-effect
    /// ledger (`observations`, to publish) on that same line — success or
    /// rejection. Nothing staged is ever counted and dropped.
    pub fn drain_gatt_events(&mut self, operation: &'static str) -> Result<String, EchoError> {
        self.check_usable(operation)?;
        let mut lines: Vec<String> = Vec::new();
        while let Some(wire) = self.gatt_queue.pop_front() {
            let parts: Vec<&str> = wire.split('|').collect();
            let kind = parts.first().copied().unwrap_or("");
            lines.push(self.apply_gatt_line(kind, &parts));
        }
        Ok(lines.join("\n"))
    }

    fn apply_gatt_line(&mut self, kind: &str, parts: &[&str]) -> String {
        // One batch per line, including arity rejections: whatever the core
        // stages while driving — success or rejection — surfaces on THIS
        // line via drain_effects_json. Errors never swallow staged effects.
        let mut out = EffectBatch::new(DRIVE_EFFECT_CAP);
        if let Some(expected) = expected_arity(kind) {
            if parts.len() != expected {
                let fragment = self.drain_effects_json(&mut out);
                return splice_fragment(&err_line(kind, &missing("event-arity")), &fragment);
            }
        }
        let rendered = match self.drive_gatt_line(kind, parts, &mut out) {
            Ok(body) => ok_line(kind, &body),
            Err(err) => err_line(kind, &err),
        };
        let fragment = self.drain_effects_json(&mut out);
        splice_fragment(&rendered, &fragment)
    }

    #[allow(clippy::too_many_lines)]
    fn drive_gatt_line(
        &mut self,
        kind: &str,
        parts: &[&str],
        out: &mut EffectBatch,
    ) -> Result<String, EchoError> {
        const OP: &str = "gatt-drain";
        match kind {
            "scan.start" => {
                let owner = arg(parts, 1, "scan-owner")?;
                let timeout_ms = parse_u64(arg(parts, 2, "scan-timeout")?, "scan-timeout")?;
                let now_ms = parse_u64(arg(parts, 3, "scan-now")?, "scan-now")?;
                let uuid_csv = arg(parts, 4, "scan-uuids")?;
                let dup = arg(parts, 5, "scan-duplicate")?;
                let merge = arg(parts, 6, "scan-merge")?;
                let uuids: Vec<&str> = if uuid_csv.is_empty() {
                    Vec::new()
                } else {
                    uuid_csv.split(',').collect()
                };
                let request = validate_scan_request(&uuids, dup, merge, timeout_ms, false, &[])
                    .map_err(|core| central_error(core, OP))?;
                let id = self
                    .central_mut()
                    .start_scan(&request, None, owner, now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(",\"op\":\"{id}\""))
            }
            "scan.platform-started" => {
                let op = operation_id(arg(parts, 1, "scan-op")?)?;
                self.central_mut()
                    .platform_scan_started(&op)
                    .map_err(|core| central_error(core, OP))?;
                Ok(String::new())
            }
            "scan.stop" => {
                let op = operation_id(arg(parts, 1, "scan-op")?)?;
                let now_ms = parse_u64(arg(parts, 2, "scan-now")?, "scan-now")?;
                self.central_mut()
                    .stop_scan(&op, now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(String::new())
            }
            "scan.platform-event" => {
                let op = operation_id(arg(parts, 1, "scan-op")?)?;
                let event = scan_event(arg(parts, 2, "scan-event")?)?;
                let now_ms = parse_u64(arg(parts, 3, "scan-now")?, "scan-now")?;
                let state = self
                    .central_mut()
                    .note_scan_platform(&op, event, now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(",\"state\":\"{}\"", state.as_str()))
            }
            "peer.resolve" => {
                let domain = arg(parts, 1, "peer-domain")?;
                let value = arg(parts, 2, "peer-value")?;
                let key = self
                    .central_mut()
                    .resolve_peer(domain, value)
                    .map_err(|core| central_error(core, OP))?;
                let mut body = String::from(",\"peer\":\"");
                json_escape_into(&mut body, &key);
                body.push('"');
                Ok(body)
            }
            "connect" => {
                let peer = arg(parts, 1, "peer-key")?;
                let lease = arg(parts, 2, "lease")?;
                let timeout_ms = parse_u64(arg(parts, 3, "connect-timeout")?, "connect-timeout")?;
                let now_ms = parse_u64(arg(parts, 4, "connect-now")?, "connect-now")?;
                let id = self
                    .central_mut()
                    .connect(peer, lease, timeout_ms, now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(",\"op\":\"{id}\""))
            }
            "link.established" => {
                let peer = arg(parts, 1, "peer-key")?;
                self.central_mut()
                    .note_link_established(peer)
                    .map_err(|core| central_error(core, OP))?;
                Ok(String::new())
            }
            "link.released" => {
                let peer = arg(parts, 1, "peer-key")?;
                self.central_mut()
                    .note_link_released(peer)
                    .map_err(|core| central_error(core, OP))?;
                Ok(String::new())
            }
            "disconnect" => {
                let peer = arg(parts, 1, "peer-key")?;
                let lease = arg(parts, 2, "lease")?;
                let now_ms = parse_u64(arg(parts, 3, "disconnect-now")?, "disconnect-now")?;
                self.central_mut()
                    .disconnect(peer, lease, now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(String::new())
            }
            "peer.loss" => {
                let peer = arg(parts, 1, "peer-key")?;
                let now_ms = parse_u64(arg(parts, 2, "peer-loss-now")?, "peer-loss-now")?;
                let state = self
                    .central_mut()
                    .note_peer_loss(peer, now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(",\"state\":\"{}\"", state.as_str()))
            }
            "discovery.begin" => {
                let peer = arg(parts, 1, "peer-key")?;
                self.central_mut()
                    .begin_discovery(peer)
                    .map_err(|core| central_error(core, OP))?;
                Ok(String::new())
            }
            "discovery.complete" => {
                let peer = arg(parts, 1, "peer-key")?;
                self.central_mut()
                    .complete_discovery(peer)
                    .map_err(|core| central_error(core, OP))?;
                Ok(String::new())
            }
            "discovery.fail" => {
                let peer = arg(parts, 1, "peer-key")?;
                self.central_mut()
                    .fail_discovery(peer)
                    .map_err(|core| central_error(core, OP))?;
                Ok(String::new())
            }
            "services-changed" => {
                let peer = arg(parts, 1, "peer-key")?;
                self.central_mut()
                    .services_changed(peer)
                    .map_err(|core| central_error(core, OP))?;
                Ok(String::new())
            }
            "path.register" => {
                let peer = arg(parts, 1, "peer-key")?;
                let svc = arg(parts, 2, "service-uuid")?;
                let svc_occ =
                    parse_u64(arg(parts, 3, "service-occurrence")?, "service-occurrence")?;
                let char_uuid = opt_str(arg(parts, 4, "characteristic-uuid")?);
                let char_occ = parse_opt_occurrence(arg(parts, 5, "characteristic-occurrence")?)?;
                let desc_uuid = opt_str(arg(parts, 6, "descriptor-uuid")?);
                let desc_occ = parse_opt_occurrence(arg(parts, 7, "descriptor-occurrence")?)?;
                let props = parse_u64(arg(parts, 8, "properties")?, "properties")?;
                let props_u8 = u8::try_from(props).map_err(|_| missing("properties-range"))?;
                let lease = arg(parts, 9, "lease")?;
                let index = self
                    .central_mut()
                    .register_path(
                        peer, svc, svc_occ, char_uuid, char_occ, desc_uuid, desc_occ, props_u8,
                        lease,
                    )
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(",\"path\":{index}"))
            }
            "read.start" => {
                let path = parse_usize(arg(parts, 1, "path-index")?, "path-index")?;
                let timeout_ms = parse_u64(arg(parts, 2, "read-timeout")?, "read-timeout")?;
                let now_ms = parse_u64(arg(parts, 3, "read-now")?, "read-now")?;
                let id = self
                    .central_mut()
                    .start_read(path, timeout_ms, now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(",\"op\":\"{id}\""))
            }
            "write.start" => {
                let path = parse_usize(arg(parts, 1, "path-index")?, "path-index")?;
                let mode = arg(parts, 2, "write-mode")?;
                let value_len = parse_u64(arg(parts, 3, "value-length")?, "value-length")?;
                let maximum = parse_opt_u64(arg(parts, 4, "write-maximum")?, "write-maximum")?;
                let supported = parse_bool(arg(parts, 5, "mode-supported")?, "mode-supported")?;
                let timeout_ms = parse_u64(arg(parts, 6, "write-timeout")?, "write-timeout")?;
                let now_ms = parse_u64(arg(parts, 7, "write-now")?, "write-now")?;
                let id = self
                    .central_mut()
                    .start_write(
                        path, mode, value_len, maximum, supported, timeout_ms, now_ms, &mut *out,
                    )
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(",\"op\":\"{id}\""))
            }
            "op.dispatch" => {
                let op = operation_id(arg(parts, 1, "op-id")?)?;
                self.central_mut()
                    .dispatch_op(&op, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(String::new())
            }
            "op.settle" => {
                let op = operation_id(arg(parts, 1, "op-id")?)?;
                let kind = contender_kind(arg(parts, 2, "contender-kind")?)?;
                let valid = parse_bool(arg(parts, 3, "contender-valid")?, "contender-valid")?;
                let ordinal = parse_u64(arg(parts, 4, "ordinal")?, "ordinal")?;
                let now_ms = parse_u64(arg(parts, 5, "settle-now")?, "settle-now")?;
                let outcome = self
                    .central_mut()
                    .settle_op(&op, kind, valid, ordinal, now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(",\"outcome\":\"{}\"", completion_label(&outcome)))
            }
            "op.cancel" => {
                let op = operation_id(arg(parts, 1, "op-id")?)?;
                let now_ms = parse_u64(arg(parts, 2, "cancel-now")?, "cancel-now")?;
                let outcome = self
                    .central_mut()
                    .cancel_op(&op, now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(",\"outcome\":\"{}\"", completion_label(&outcome)))
            }
            "subscribe" => {
                let path = parse_usize(arg(parts, 1, "path-index")?, "path-index")?;
                let policy = arg(parts, 2, "overflow-policy")?;
                let items = parse_u64(arg(parts, 3, "item-capacity")?, "item-capacity")?;
                let bytes = parse_u64(arg(parts, 4, "byte-capacity")?, "byte-capacity")?;
                let consumer = arg(parts, 5, "consumer")?;
                let timeout_ms =
                    parse_u64(arg(parts, 6, "subscribe-timeout")?, "subscribe-timeout")?;
                let now_ms = parse_u64(arg(parts, 7, "subscribe-now")?, "subscribe-now")?;
                let id = self
                    .central_mut()
                    .subscribe(
                        path, policy, items, bytes, consumer, timeout_ms, now_ms, &mut *out,
                    )
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(",\"op\":\"{id}\""))
            }
            "subscribe.enable-settled" => {
                let path = parse_usize(arg(parts, 1, "path-index")?, "path-index")?;
                let success = parse_bool(arg(parts, 2, "enable-success")?, "enable-success")?;
                let now_ms = parse_u64(arg(parts, 3, "enable-now")?, "enable-now")?;
                self.central_mut()
                    .settle_subscribe_enable(path, success, now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(String::new())
            }
            "unsubscribe" => {
                let path = parse_usize(arg(parts, 1, "path-index")?, "path-index")?;
                let consumer = arg(parts, 2, "consumer")?;
                let now_ms = parse_u64(arg(parts, 3, "unsubscribe-now")?, "unsubscribe-now")?;
                let disabled = self
                    .central_mut()
                    .unsubscribe(path, consumer, now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(",\"physical-disable\":{disabled}"))
            }
            "subscribe.disable-settled" => {
                let path = parse_usize(arg(parts, 1, "path-index")?, "path-index")?;
                let now_ms = parse_u64(arg(parts, 2, "disable-now")?, "disable-now")?;
                self.central_mut()
                    .settle_subscribe_disable(path, now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(String::new())
            }
            "notify.deliver" => {
                let path = parse_usize(arg(parts, 1, "path-index")?, "path-index")?;
                let hex = arg(parts, 2, "notify-hex")?;
                let value = parse_hex(hex)?;
                let outcomes = self
                    .central_mut()
                    .deliver_notification_value(path, &value)
                    .map_err(|core| central_error(core, OP))?;
                let mut body = String::from(",\"deliveries\":[");
                let mut first = true;
                for (consumer, outcome) in &outcomes {
                    if !first {
                        body.push(',');
                    }
                    first = false;
                    body.push_str("{\"consumer\":\"");
                    json_escape_into(&mut body, consumer);
                    body.push_str("\",\"outcome\":\"");
                    body.push_str(delivery_label(*outcome));
                    body.push_str("\"}");
                }
                body.push(']');
                Ok(body)
            }
            "expire-sweep" => {
                let now_ms = parse_u64(arg(parts, 1, "sweep-now")?, "sweep-now")?;
                let (settled, truncated) = self
                    .central_mut()
                    .expire_sweep(now_ms, &mut *out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(",\"settled\":{settled},\"truncated\":{truncated}"))
            }
            "adapter.reset" => {
                let now_ms = parse_u64(arg(parts, 1, "reset-now")?, "reset-now")?;
                let settled = self.drive_adapter_reset(now_ms, &mut *out)?;
                Ok(format!(",\"settled\":{settled}"))
            }
            "release" => {
                let record = self
                    .central_mut()
                    .destroy(out)
                    .map_err(|core| central_error(core, OP))?;
                Ok(format!(
                    ",\"state\":\"{}\"",
                    match record.state() {
                        ubm_core::ownership::CleanupState::Released => "released",
                        ubm_core::ownership::CleanupState::ReleaseFailed => "release-failed",
                    }
                ))
            }
            "" => Err(missing("event-kind-empty")),
            _ => Err(drain_rejected("unknown-event-kind")),
        }
    }
}

fn operation_id(text: &str) -> Result<ubm_core::contracts::OperationId, EchoError> {
    ubm_core::contracts::OperationId::new(text).map_err(|_| missing("op-id-empty"))
}

fn opt_str(text: &str) -> Option<&str> {
    if text.is_empty() || text == "-" {
        None
    } else {
        Some(text)
    }
}

fn parse_opt_occurrence(text: &str) -> Result<Option<u64>, EchoError> {
    if text.is_empty() || text == "-" {
        Ok(None)
    } else {
        parse_u64(text, "occurrence").map(Some)
    }
}

fn completion_label(outcome: &ubm_core::central::CompletionOutcome) -> &'static str {
    use ubm_core::central::CompletionOutcome as O;
    match outcome {
        O::Settled { kind, .. } => kind.as_str(),
        O::DuplicateSuppressed { .. } => "duplicate-suppressed",
        O::ContenderIgnored => "contender-ignored",
    }
}

fn delivery_label(outcome: ubm_core::central::DeliveryOutcome) -> &'static str {
    use ubm_core::central::DeliveryOutcome as D;
    match outcome {
        D::Delivered => "delivered",
        D::OverflowNoticed => "overflow-noticed",
        D::QuarantinedPreReady => "quarantined-pre-ready",
        D::DroppedRemoved => "dropped-removed",
        D::DroppedLate => "dropped-late",
    }
}

impl CoreSession {
    fn drive_adapter_reset(
        &mut self,
        now_ms: u64,
        out: &mut EffectBatch,
    ) -> Result<usize, EchoError> {
        self.gatt_resets = self.gatt_resets.wrapping_add(1);
        let epoch = self.gatt_resets;
        let attachment = ubm_core::contracts::AttachmentTuple::new(
            AttachmentId::new(format!("ubm-android-attachment-r{epoch}")).map_err(|_| {
                drain_failed("lifecycle.invariant-violation", "core", "attachment-mint")
            })?,
            BackendInstanceId::new(format!("ubm-android-instance-r{epoch}")).map_err(|_| {
                drain_failed("lifecycle.invariant-violation", "core", "attachment-mint")
            })?,
            BackendGeneration::new(format!("ubm-android-generation-r{epoch}")).map_err(|_| {
                drain_failed("lifecycle.invariant-violation", "core", "attachment-mint")
            })?,
            AdapterId::new(format!("ubm-android-adapter-r{epoch}")).map_err(|_| {
                drain_failed("lifecycle.invariant-violation", "core", "attachment-mint")
            })?,
            AdapterGeneration::new(format!("ubm-android-adapter-generation-r{epoch}")).map_err(
                |_| drain_failed("lifecycle.invariant-violation", "core", "attachment-mint"),
            )?,
        );
        let generation = Generation::new(format!("ubm-android-kernel-generation-r{epoch}"))
            .map_err(|_| {
                drain_failed("lifecycle.invariant-violation", "core", "generation-mint")
            })?;
        self.central_mut()
            .handle_adapter_reset(attachment, generation, now_ms, out)
            .map_err(|core| central_error(core, "gatt-drain"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REV: &str = crate::core_backend::CONTRACT_REVISION;

    fn open_session() -> CoreSession {
        CoreSession::open(REV).expect("test session must open")
    }

    fn drain_ok(session: &mut CoreSession, wire: &str) -> String {
        enqueue_event(&mut session.gatt_queue, wire).expect("enqueue must accept");
        let out = session
            .drain_gatt_events("gatt-drain")
            .expect("drain lifetime must hold");
        assert!(!out.is_empty(), "drain must observe one line");
        assert!(out.contains("\"ok\":true"), "line must succeed: {out}");
        out
    }

    fn drain_one(session: &mut CoreSession, wire: &str) -> String {
        enqueue_event(&mut session.gatt_queue, wire).expect("enqueue must accept");
        session
            .drain_gatt_events("gatt-drain")
            .expect("drain lifetime must hold")
    }

    fn op_of(line: &str) -> String {
        let key = "\"op\":\"";
        let start = line.find(key).expect("line must carry op") + key.len();
        let end = line[start..].find('"').expect("op must terminate") + start;
        String::from(&line[start..end])
    }

    #[test]
    fn enqueue_rejects_empty_and_bounds_the_queue() {
        let mut session = open_session();
        assert!(enqueue_event(&mut session.gatt_queue, "").is_err());
        let oversized = format!("scan.start|{}", "x".repeat(GATT_WIRE_MAX));
        assert!(enqueue_event(&mut session.gatt_queue, &oversized).is_err());
        for i in 0..GATT_QUEUE_CAP {
            enqueue_event(&mut session.gatt_queue, &format!("release|{i}"))
                .expect("queue must accept to cap");
        }
        let full = enqueue_event(&mut session.gatt_queue, "release|overflow");
        assert!(full.is_err());
        let err = full.expect_err("queue-full must reject");
        assert_eq!((err.code, err.domain), ("stream.quota", "stream"));
    }

    #[test]
    fn unknown_kind_rejects_with_capability_identity() {
        let mut session = open_session();
        let line = drain_one(&mut session, "teleport|nowhere|0");
        assert!(line.contains("\"ok\":false"), "{line}");
        assert!(line.contains("capability.unsupported"), "{line}");
        assert!(line.contains("capability"), "{line}");
    }

    #[test]
    fn scan_connect_discover_io_cycle_drives_real_central() {
        let mut session = open_session();
        let scan = drain_ok(&mut session, "scan.start|owner-a|5000|1000||all|none");
        let scan_op = op_of(&scan);
        drain_ok(&mut session, &format!("scan.platform-started|{scan_op}"));
        let peer_line = drain_ok(
            &mut session,
            "peer.resolve|public-address|AA:BB:CC:DD:EE:FF",
        );
        assert!(
            peer_line.contains("\"peer\":\"public-address:"),
            "{peer_line}"
        );
        let peer = peer_line
            .split("\"peer\":\"")
            .nth(1)
            .expect("peer key")
            .split('"')
            .next()
            .expect("peer key end");
        let peer = String::from(peer);
        let connect = drain_ok(&mut session, &format!("connect|{peer}|lease-a|5000|1000"));
        let connect_op = op_of(&connect);
        drain_ok(&mut session, &format!("link.established|{peer}"));
        drain_ok(&mut session, &format!("discovery.begin|{peer}"));
        drain_ok(&mut session, &format!("discovery.complete|{peer}"));
        let path_line = drain_ok(
            &mut session,
            &format!("path.register|{peer}|180d|0|2a37|0|-|-|11|lease-a"),
        );
        assert!(path_line.contains("\"path\":0"), "{path_line}");
        let read = drain_ok(&mut session, "read.start|0|5000|1000");
        let read_op = op_of(&read);
        drain_ok(&mut session, &format!("op.dispatch|{read_op}"));
        let settled = drain_ok(
            &mut session,
            &format!("op.settle|{read_op}|success|true|7|1000"),
        );
        assert!(settled.contains("succeeded"), "{settled}");
        drain_ok(
            &mut session,
            &format!("op.settle|{connect_op}|success|true|8|1000"),
        );
        drain_ok(&mut session, &format!("disconnect|{peer}|lease-a|1000"));
        drain_ok(&mut session, &format!("link.released|{peer}"));
        let status = session.central_status("test").expect("status holds");
        assert!(status.contains("C-UBM.0.1.2-DRAFT"), "{status}");
    }

    #[test]
    fn queued_cancel_never_reaches_radio() {
        let mut session = open_session();
        let peer_line = drain_ok(
            &mut session,
            "peer.resolve|public-address|11:22:33:44:55:66",
        );
        let peer = peer_line
            .split("\"peer\":\"")
            .nth(1)
            .expect("peer key")
            .split('"')
            .next()
            .expect("peer key end");
        let peer = String::from(peer);
        drain_ok(&mut session, &format!("connect|{peer}|lease-b|5000|1000"));
        drain_ok(&mut session, &format!("link.established|{peer}"));
        drain_ok(&mut session, &format!("discovery.begin|{peer}"));
        drain_ok(&mut session, &format!("discovery.complete|{peer}"));
        drain_ok(
            &mut session,
            &format!("path.register|{peer}|180d|0|2a37|0|-|-|11|lease-b"),
        );
        let read = drain_ok(&mut session, "read.start|0|5000|1000");
        let read_op = op_of(&read);
        let cancelled = drain_ok(&mut session, &format!("op.cancel|{read_op}|1000"));
        assert!(cancelled.contains("aborted"), "{cancelled}");
        assert_eq!(session.gatt_queue.len(), 0);
    }

    #[test]
    fn notify_bounds_reject_oversize_and_count_deliveries() {
        let mut session = open_session();
        let peer_line = drain_ok(
            &mut session,
            "peer.resolve|public-address|77:88:99:AA:BB:CC",
        );
        let peer = peer_line
            .split("\"peer\":\"")
            .nth(1)
            .expect("peer key")
            .split('"')
            .next()
            .expect("peer key end");
        let peer = String::from(peer);
        drain_ok(&mut session, &format!("connect|{peer}|lease-c|5000|1000"));
        drain_ok(&mut session, &format!("link.established|{peer}"));
        drain_ok(&mut session, &format!("discovery.begin|{peer}"));
        drain_ok(&mut session, &format!("discovery.complete|{peer}"));
        drain_ok(
            &mut session,
            &format!("path.register|{peer}|180d|0|2a37|0|-|-|11|lease-c"),
        );
        drain_ok(
            &mut session,
            "subscribe|0|error|8|1024|consumer-a|5000|1000",
        );
        drain_ok(&mut session, "subscribe.enable-settled|0|true|1000");
        let delivered = drain_ok(&mut session, "notify.deliver|0|0102");
        assert!(
            delivered.contains("\"outcome\":\"delivered\""),
            "{delivered}"
        );
        let too_big = "notify.deliver|0|".to_string() + &"ab".repeat(GATT_NOTIFY_MAX + 1);
        let rejected = enqueue_event(&mut session.gatt_queue, &too_big);
        let err = rejected.expect_err("oversize notify wire must reject at enqueue");
        assert_eq!((err.code, err.domain), ("bytes.too-large", "core"));
        let bad_hex = drain_one(&mut session, "notify.deliver|0|zz");
        assert!(bad_hex.contains("bytes.invalid"), "{bad_hex}");
    }

    #[test]
    fn service_change_and_adapter_reset_invalidate_paths() {
        let mut session = open_session();
        let peer_line = drain_ok(
            &mut session,
            "peer.resolve|public-address|DE:AD:BE:EF:00:01",
        );
        let peer = peer_line
            .split("\"peer\":\"")
            .nth(1)
            .expect("peer key")
            .split('"')
            .next()
            .expect("peer key end");
        let peer = String::from(peer);
        drain_ok(&mut session, &format!("connect|{peer}|lease-d|5000|1000"));
        drain_ok(&mut session, &format!("link.established|{peer}"));
        drain_ok(&mut session, &format!("discovery.begin|{peer}"));
        drain_ok(&mut session, &format!("discovery.complete|{peer}"));
        drain_ok(
            &mut session,
            &format!("path.register|{peer}|180d|0|2a37|0|-|-|11|lease-d"),
        );
        drain_ok(&mut session, &format!("services-changed|{peer}"));
        let stale = drain_one(&mut session, "read.start|0|5000|1000");
        assert!(stale.contains("\"ok\":false"), "{stale}");
        assert!(stale.contains("gatt.stale-handle"), "{stale}");
        let reset = drain_ok(&mut session, "adapter.reset|2000");
        assert!(reset.contains("\"settled\""), "{reset}");
    }

    #[test]
    fn release_drives_real_destroy_and_stays_idempotent() {
        let mut session = open_session();
        let first = drain_ok(&mut session, "release");
        assert!(first.contains("released"), "{first}");
        let second = drain_ok(&mut session, "release");
        assert!(second.contains("released"), "{second}");
        assert!(session.central_status("test").is_ok());
    }

    #[test]
    fn drain_rejects_wrong_arity_before_any_transition() {
        let mut session = open_session();
        for wire in [
            "release|anything",
            "peer.resolve|public-address|AA:BB:CC:DD:EE:FF|extra",
            "scan.stop|op-only",
            "notify.deliver|0",
        ] {
            let line = drain_one(&mut session, wire);
            assert!(line.contains("\"ok\":false"), "{wire} -> {line}");
            assert!(line.contains("argument.invalid"), "{wire} -> {line}");
            assert!(line.contains("event-arity"), "{wire} -> {line}");
        }
        assert_eq!(session.gatt_queue.len(), 0);
    }

    #[test]
    fn drain_applies_many_lines_fifo_in_one_call() {
        let mut session = open_session();
        for wire in [
            "peer.resolve|public-address|AA:BB:CC:DD:EE:FF",
            "expire-sweep|1000",
            "teleport|nowhere",
        ] {
            enqueue_event(&mut session.gatt_queue, wire).expect("enqueue must accept");
        }
        let out = session
            .drain_gatt_events("gatt-drain")
            .expect("drain lifetime must hold");
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines[0].contains("\"event\":\"peer.resolve\""), "{out}");
        assert!(lines[0].contains("\"ok\":true"), "{out}");
        assert!(lines[1].contains("\"event\":\"expire-sweep\""), "{out}");
        assert!(lines[2].contains("capability.unsupported"), "{out}");
        assert_eq!(session.gatt_queue.len(), 0);
    }

    #[test]
    fn drain_after_release_fails_admit_verbs_as_destroyed_data() {
        let mut session = open_session();
        drain_ok(&mut session, "release");
        let scan = drain_one(&mut session, "scan.start|owner-a|5000|1000||all|none");
        assert!(scan.contains("\"ok\":false"), "{scan}");
        assert!(scan.contains("lifecycle.destroyed"), "{scan}");
        let resolve = drain_one(
            &mut session,
            "peer.resolve|public-address|AA:BB:CC:DD:EE:FF",
        );
        assert!(resolve.contains("\"ok\":true"), "{resolve}");
    }

    /// Count the `{"kind":...}` entries of one named array section in a
    /// drained line (`effects` or `observations`).
    fn count_section(line: &str, section: &str) -> usize {
        let key = format!("\"{section}\":[");
        let start = line
            .find(&key)
            .unwrap_or_else(|| panic!("line must carry {section}: {line}"))
            + key.len();
        let end = line[start..].find(']').expect("section must terminate");
        line[start..start + end].matches("{\"kind\":").count()
    }

    #[test]
    fn drain_surfaces_kernel_effects_and_typed_observations_without_discard() {
        let mut session = open_session();
        // Admission stages a kernel timer.schedule effect plus the typed
        // central.scan-start observation: both must reach the host.
        let scan = drain_ok(&mut session, "scan.start|owner-a|5000|1000||all|none");
        assert!(
            scan.contains("\"effects\":[{\"kind\":\"timer.schedule\""),
            "admission effect must surface: {scan}"
        );
        assert!(
            scan.contains("\"observations\":[{\"kind\":\"central.scan-start\""),
            "typed observation must surface: {scan}"
        );
        // Dispatch stages radio.dispatch plus state.publish on the SAME op
        // the observation belongs to: kinds, op binding, and order survive.
        let op = op_of(&scan);
        let dispatch = drain_ok(&mut session, &format!("op.dispatch|{op}"));
        let radio = dispatch
            .find("\"kind\":\"radio.dispatch\"")
            .expect("radio.dispatch must surface");
        let publish = dispatch
            .find("\"kind\":\"state.publish\"")
            .expect("state.publish must surface");
        assert!(radio < publish, "dispatch order survives: {dispatch}");
        assert!(
            dispatch.contains(&format!("\"op\":\"{op}\"")),
            "effects bind the driving op: {dispatch}"
        );
        // A quiet line surfaces empty sections (never missing, never null):
        // the host parses unconditionally, with no presence probing.
        let sweep = drain_ok(&mut session, "expire-sweep|1000");
        assert!(sweep.contains("\"effects\":[]"), "{sweep}");
        assert!(sweep.contains("\"observations\":[]"), "{sweep}");
        // Nothing double-counted or dropped: every drained line in a mixed
        // cycle carries both sections exactly once.
        for wire in [
            "peer.resolve|public-address|AA:BB:CC:DD:EE:FF",
            "scan.platform-started|op-1",
        ] {
            enqueue_event(&mut session.gatt_queue, wire).expect("enqueue must accept");
        }
        let out = session
            .drain_gatt_events("gatt-drain")
            .expect("drain lifetime must hold");
        for line in out.split('\n') {
            assert_eq!(line.matches("\"effects\":[").count(), 1, "{line}");
            assert_eq!(line.matches("\"observations\":[").count(), 1, "{line}");
            let _ = count_section(line, "effects");
            let _ = count_section(line, "observations");
        }
    }

    #[test]
    fn drain_surfaces_effects_on_core_rejections_too() {
        let mut session = open_session();
        // A rejected line still drains whatever the core staged before the
        // rejection: errors never silently swallow effects.
        let line = drain_one(&mut session, "op.dispatch|op-unknown");
        assert!(line.contains("\"ok\":false"), "{line}");
        assert_eq!(line.matches("\"effects\":[").count(), 1, "{line}");
        assert_eq!(line.matches("\"observations\":[").count(), 1, "{line}");
    }
}
