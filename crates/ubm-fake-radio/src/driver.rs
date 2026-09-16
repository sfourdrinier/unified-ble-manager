//! Synchronous deterministic staged driver over the REAL `ubm-core`
//! transition kernel (U7 slice, trackourhealth/bun-mono#1188).
//!
//! NO BLE hardware exists on this host, so every radio/host event here is
//! SYNTHETIC: scripted deterministic programs ([`StagedDriver::run_step`]
//! text lines) stand in for the OS radio at the core-transition seam. The
//! state machine underneath is never synthetic: every transition runs
//! through the real [`ubm_core::central::Central`] (which owns the one
//! scheduling kernel), and every rejection preserves the frozen contract
//! identity verbatim. A scripted byte payload crosses the staged path
//! losslessly; a scripted fault (timeout, disconnect, error, out-of-order
//! delivery) settles exactly as the frozen tables prescribe.
//!
//! Why this crate exists instead of reusing `ubm-desktop`'s `FakeRadio`:
//! that boundary is `async` over `tokio::sync` primitives and models the
//! btleplug OS-radio seam. Bindings (`napi`, `wasm`, `uniffi`, `jni`) are
//! synchronous FFI contexts whose dependency closures must stay minimal
//! (they also check for `wasm32`); pulling Tokio and btleplug into them is
//! disproportionate and platform-wrong. This crate depends ONLY on
//! `ubm-core`, is fully synchronous, and drives the core-transition seam
//! (bounded staged effect batches, op/path/peer registries, receipts),
//! not the OS-radio seam.
//!
//! Bounded batches: every drive call stages kernel effects into an
//! [`EffectBatch`] of at most [`STAGED_BATCH_MAX`] (64) slots. A full batch
//! fails loudly as `stream.quota` and increments
//! [`StagedDriver::dropped_not_staged`]: dropped-not-staged accounting is
//! preserved, never silent. Observations per step are deterministic JSON
//! objects (see [`StagedDriver::run_step`]); the parity harness compares
//! them verbatim against pinned frozen-rule expectations.

use ubm_core::central::{
    CapabilityAdmission, CapabilityDescriptor, CapabilityState, Central, CentralConfig,
    CompletionOutcome, EvidenceLevel, PathSelector, ScanPlatformEvent,
};
use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
    BackendInstanceId, ContenderKind, CoreError, Generation, MonotonicTime, OperationId,
};
use ubm_core::ownership::EffectBatch;

use crate::json::{JsonValue, hex_of, parse_hex, push_quoted};

/// Maximum staged effect-batch capacity (the slice bound; batches are
/// `<= 64` by construction and the driver rejects larger caps).
pub const STAGED_BATCH_MAX: usize = 64;

/// Default staged batch capacity: the full slice bound.
pub const STAGED_BATCH_DEFAULT: usize = 64;

/// Frozen contract revision spoken by the staged core, single-owned by
/// `ubm-core` (re-exported for binding surfaces; never redefined).
pub use ubm_core::contracts::CONTRACT_REVISION;

/// Typed staged failure carrying a frozen `code|domain` pair plus the
/// staged operation under test. Never silent: every rejection names its
/// identity on the wire as `code|domain|operation|detail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedError {
    code: String,
    domain: String,
    operation: String,
    detail: String,
}

impl StagedError {
    /// Build a staged failure. Binding surfaces use this for lifetime
    /// failures (e.g. post-close calls); step-level core rejections are
    /// mapped through the frozen contract identity instead.
    pub fn new(code: &str, domain: &str, operation: &str, detail: &str) -> Self {
        Self {
            code: String::from(code),
            domain: String::from(domain),
            operation: String::from(operation),
            detail: String::from(detail),
        }
    }

    /// Frozen contract code (e.g. `capability.unsupported`).
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Frozen contract domain (e.g. `capability`).
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Staged operation under test (e.g. `staged-step`).
    pub fn operation(&self) -> &str {
        &self.operation
    }

    /// Rejector detail (frozen core rejector or staged seam detail).
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Wire form shared by every binding: `code|domain|operation|detail`.
    pub fn wire_message(&self) -> String {
        let mut out = String::with_capacity(
            self.code
                .len()
                .saturating_add(self.domain.len())
                .saturating_add(self.operation.len())
                .saturating_add(self.detail.len())
                .saturating_add(3),
        );
        out.push_str(&self.code);
        out.push('|');
        out.push_str(&self.domain);
        out.push('|');
        out.push_str(&self.operation);
        out.push('|');
        out.push_str(&self.detail);
        out
    }
}

impl core::fmt::Display for StagedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.wire_message())
    }
}

/// Map a core rejection to the staged wire form. The frozen contract
/// identity (code + domain) is preserved verbatim; the operation names the
/// staged step under test and the detail names the frozen rejector
/// (`CoreError::operation`, e.g. `scan.arbitration`, `path.resolve`).
fn central_error(core: &CoreError, operation: &str) -> StagedError {
    StagedError::new(
        core.code().as_str(),
        core.domain().as_str(),
        operation,
        core.operation(),
    )
}

/// Tiny deterministic JSON object writer for observations.
struct Ob {
    buf: String,
    first: bool,
}

impl Ob {
    fn new() -> Self {
        Self {
            buf: String::from("{"),
            first: true,
        }
    }

    fn sep(&mut self) {
        if self.first {
            self.first = false;
        } else {
            self.buf.push(',');
        }
    }

    fn str_field(&mut self, name: &str, value: &str) {
        self.sep();
        push_quoted(&mut self.buf, name);
        self.buf.push(':');
        push_quoted(&mut self.buf, value);
    }

    fn num_field(&mut self, name: &str, value: u64) {
        self.sep();
        push_quoted(&mut self.buf, name);
        self.buf.push(':');
        self.buf.push_str(&value.to_string());
    }

    fn bool_field(&mut self, name: &str, value: bool) {
        self.sep();
        push_quoted(&mut self.buf, name);
        self.buf.push(':');
        self.buf.push_str(if value { "true" } else { "false" });
    }

    fn null_field(&mut self, name: &str) {
        self.sep();
        push_quoted(&mut self.buf, name);
        self.buf.push_str(":null");
    }

    fn finish(mut self) -> String {
        self.buf.push('}');
        self.buf
    }
}

fn ok_ob(step: &str) -> Ob {
    let mut ob = Ob::new();
    ob.str_field("step", step);
    ob.bool_field("ok", true);
    ob
}

fn err_ob(step: &str, error: &StagedError) -> String {
    let mut ob = Ob::new();
    ob.str_field("step", step);
    ob.bool_field("ok", false);
    ob.str_field("error", &error.wire_message());
    ob.finish()
}

/// Render one kernel completion outcome as observation fields.
fn render_outcome(ob: &mut Ob, outcome: &CompletionOutcome) {
    match outcome {
        CompletionOutcome::Settled {
            kind,
            cause,
            reached_radio,
            commit,
            suppressed,
        } => {
            ob.str_field("settle", "settled");
            ob.str_field("terminal", kind.as_str());
            match cause {
                Some(code) => ob.str_field("cause", code.as_str()),
                None => ob.null_field("cause"),
            }
            ob.bool_field("reached_radio", *reached_radio);
            ob.str_field("commit", commit.as_str());
            ob.num_field("suppressed", *suppressed);
        }
        CompletionOutcome::DuplicateSuppressed { suppressed } => {
            ob.str_field("settle", "duplicate-suppressed");
            ob.num_field("suppressed", *suppressed);
        }
        CompletionOutcome::ContenderIgnored => {
            ob.str_field("settle", "contender-ignored");
        }
    }
}

fn delivery_str(kind: ubm_core::central::DeliveryOutcome) -> &'static str {
    match kind {
        ubm_core::central::DeliveryOutcome::Delivered => "delivered",
        ubm_core::central::DeliveryOutcome::OverflowNoticed => "overflow-noticed",
        ubm_core::central::DeliveryOutcome::QuarantinedPreReady => "quarantined-pre-ready",
        ubm_core::central::DeliveryOutcome::DroppedRemoved => "dropped-removed",
        ubm_core::central::DeliveryOutcome::DroppedLate => "dropped-late",
    }
}

/// Parse a contender kind from its frozen wire string.
fn parse_contender(text: &str) -> Option<ContenderKind> {
    match text {
        "success" => Some(ContenderKind::Success),
        "failure" => Some(ContenderKind::Failure),
        "abort" => Some(ContenderKind::Abort),
        "timeout" => Some(ContenderKind::Timeout),
        "disconnect" => Some(ContenderKind::Disconnect),
        "reset" => Some(ContenderKind::Reset),
        "destroy" => Some(ContenderKind::Destroy),
        "adapter-loss" => Some(ContenderKind::AdapterLoss),
        "session-stop" => Some(ContenderKind::SessionStop),
        "dispatch-begin" => Some(ContenderKind::DispatchBegin),
        _ => None,
    }
}

fn parse_scan_event(text: &str) -> Option<ScanPlatformEvent> {
    match text {
        "platform-started" => Some(ScanPlatformEvent::PlatformStarted),
        "stop" => Some(ScanPlatformEvent::Stop),
        "platform-stopped" => Some(ScanPlatformEvent::PlatformStopped),
        "source-closed" => Some(ScanPlatformEvent::SourceClosed),
        "start-failed" => Some(ScanPlatformEvent::StartFailed),
        "source-failed" => Some(ScanPlatformEvent::SourceFailed),
        "overflow-error-policy" => Some(ScanPlatformEvent::OverflowErrorPolicy),
        "reset" => Some(ScanPlatformEvent::Reset),
        "stop-failed" => Some(ScanPlatformEvent::StopFailed),
        _ => None,
    }
}

/// One staged GATT occurrence node in a synthetic discovery snapshot.
#[derive(Debug, Clone)]
struct SnapshotNode {
    uuid: String,
    occurrence: u64,
    properties: u8,
    descriptors: Vec<SnapshotNode>,
    characteristics: Vec<SnapshotNode>,
}

fn parse_prop_flag(text: &str) -> Option<u8> {
    match text {
        "read" => Some(ubm_core::central::GATT_PROP_READ),
        "write" => Some(ubm_core::central::GATT_PROP_WRITE),
        "write-without-response" => Some(ubm_core::central::GATT_PROP_WRITE_NO_RESPONSE),
        "notify" => Some(ubm_core::central::GATT_PROP_NOTIFY),
        "indicate" => Some(ubm_core::central::GATT_PROP_INDICATE),
        _ => None,
    }
}

/// Parse a comma-or-plus separated property list (`"read+notify"`) into the
/// frozen GATT property bitmask. Unknown flags are a staged error.
fn parse_properties(text: &str) -> Option<u8> {
    if text.is_empty() {
        return Some(0);
    }
    let mut bits: u8 = 0;
    for part in text.split('+') {
        bits |= parse_prop_flag(part.trim())?;
    }
    Some(bits)
}

/// Capability rows the staged slice projects. The synthetic radio proves
/// core-transition behavior only, so every row carries the explicit
/// `synthetic-radio-only` limitation at `Deterministic` evidence: projected
/// truth, never re-described support.
const STAGED_CAPABILITY_IDS: [&str; 6] = [
    "central.scan",
    "central.connect",
    "central.discover",
    "central.read",
    "central.write",
    "central.subscribe",
];

/// Synchronous deterministic staged driver over one REAL [`Central`].
///
/// The driver owns the session attachment scope, a monotonic host clock, a
/// staged op-name registry, peer aliases, synthetic read payloads, and the
/// deterministic observation log. Every `*_step` runs one core transition
/// against a bounded [`EffectBatch`] and returns one JSON observation
/// object (also appended to the log drained by [`StagedDriver::drain_log`]).
#[derive(Debug)]
pub struct StagedDriver {
    central: Central,
    staged_cap: usize,
    staged_total: u64,
    dropped_not_staged: u64,
    truncated_sweeps: u64,
    now_ms: MonotonicTime,
    ordinal: u64,
    ops: Vec<(String, OperationId)>,
    peers: Vec<(String, String)>,
    read_values: Vec<(String, Vec<u8>)>,
    log: Vec<String>,
}

impl StagedDriver {
    /// Open a staged session: one REAL [`Central`] bound to the fixed
    /// staged attachment scope with a completed handshake. Every label is
    /// validated, so construction fails loudly instead of operating
    /// degraded.
    pub fn open() -> Result<Self, StagedError> {
        const OP: &str = "staged-open";
        let fail = |detail: &'static str| {
            StagedError::new("lifecycle.invariant-violation", "core", OP, detail)
        };
        let attachment = AttachmentTuple::new(
            AttachmentId::new("ubm-staged-attachment")
                .map_err(|_| fail("central-construct-failed"))?,
            BackendInstanceId::new("ubm-staged-instance")
                .map_err(|_| fail("central-construct-failed"))?,
            BackendGeneration::new("ubm-staged-generation-0")
                .map_err(|_| fail("central-construct-failed"))?,
            AdapterId::new("ubm-staged-adapter").map_err(|_| fail("central-construct-failed"))?,
            AdapterGeneration::new("ubm-staged-adapter-generation-0")
                .map_err(|_| fail("central-construct-failed"))?,
        );
        let generation = Generation::new("ubm-staged-kernel-generation-0")
            .map_err(|_| fail("central-construct-failed"))?;
        let central = Central::new(attachment, generation, CentralConfig::default())
            .map_err(|core| central_error(&core, OP))?;
        Ok(Self {
            central,
            staged_cap: STAGED_BATCH_DEFAULT,
            staged_total: 0,
            dropped_not_staged: 0,
            truncated_sweeps: 0,
            now_ms: 0,
            ordinal: 0,
            ops: Vec::new(),
            peers: Vec::new(),
            read_values: Vec::new(),
            log: Vec::new(),
        })
    }

    /// Current staged batch capacity (`<= 64`).
    pub const fn staged_cap(&self) -> usize {
        self.staged_cap
    }

    /// Total kernel effects staged so far (bounded-batch receipts).
    pub const fn staged_total(&self) -> u64 {
        self.staged_total
    }

    /// Effects that could not be staged because the batch was full.
    /// Preserved accounting: a full batch fails loudly AND counts here,
    /// never silently.
    pub const fn dropped_not_staged(&self) -> u64 {
        self.dropped_not_staged
    }

    /// Sweeps that reported truncation (the flag itself is the preserved
    /// accounting for sweep overflow; surfaced per observation too).
    pub const fn truncated_sweeps(&self) -> u64 {
        self.truncated_sweeps
    }

    /// Drain the deterministic observation log (FIFO, JSON lines).
    pub fn drain_log(&mut self) -> Vec<String> {
        core::mem::take(&mut self.log)
    }

    fn next_ordinal(&mut self) -> u64 {
        let ordinal = self.ordinal;
        self.ordinal = self.ordinal.saturating_add(1);
        ordinal
    }

    fn batch(&self) -> EffectBatch {
        EffectBatch::new(self.staged_cap)
    }

    /// Record one drive call's staging: kernel effect count plus the typed
    /// central effects drained in order (sequencing receipts).
    fn note_staged(&mut self, ob: &mut Ob, out: &mut EffectBatch) {
        let staged = out.len() as u64;
        self.staged_total = self.staged_total.saturating_add(staged);
        ob.num_field("staged", staged);
        let typed = self.central.drain_typed_effects();
        let mut seq = String::new();
        for (index, effect) in typed.iter().enumerate() {
            if index > 0 {
                seq.push(';');
            }
            seq.push_str(effect.kind().as_str());
            seq.push('#');
            seq.push_str(effect.operation_id().as_str());
            seq.push(':');
            seq.push_str(effect.detail());
        }
        let _ = out.drain();
        ob.str_field("effects", &seq);
    }

    /// A drive call failed because the bounded batch was full: count the
    /// dropped-not-staged effect loudly (the error itself still reports).
    fn note_batch_full(&mut self, core: &CoreError) -> bool {
        let full =
            core.code().as_str() == "stream.quota" && core.operation() == "effect-batch.full";
        if full {
            self.dropped_not_staged = self.dropped_not_staged.saturating_add(1);
        }
        full
    }

    fn advance_clock(&mut self, obj: &JsonValue, op: &str) -> Result<MonotonicTime, StagedError> {
        let now = u64_param(obj, "now", Some(self.now_ms), op)?;
        if now < self.now_ms {
            return Err(StagedError::new(
                "argument.invalid",
                "core",
                op,
                "clock-regressed",
            ));
        }
        self.now_ms = now;
        Ok(now)
    }

    fn peer_key_of(&self, obj: &JsonValue, op: &str) -> Result<String, StagedError> {
        let alias = str_param(obj, "peer", op)?;
        match self.peers.iter().find(|(name, _)| name == alias) {
            Some((_, key)) => Ok(key.clone()),
            None => Err(StagedError::new(
                "peer.not-found",
                "peer",
                op,
                "staged-peer-unknown",
            )),
        }
    }

    fn op_id_of(&self, obj: &JsonValue, op: &str) -> Result<OperationId, StagedError> {
        let name = str_param(obj, "op", op)?;
        match self.ops.iter().find(|(known, _)| known == name) {
            Some((_, id)) => Ok(id.clone()),
            None => Err(StagedError::new(
                "argument.invalid",
                "core",
                op,
                "staged-op-unknown",
            )),
        }
    }

    fn register_op(&mut self, name: &str, id: OperationId, op: &str) -> Result<(), StagedError> {
        if let Some((_, prior)) = self.ops.iter().find(|(known, _)| known == name) {
            let live = matches!(
                self.central.operation_state(prior),
                Some(
                    ubm_core::ownership::OpStateView::Queued
                        | ubm_core::ownership::OpStateView::Dispatched
                )
            );
            if live {
                return Err(StagedError::new(
                    "ownership.denied",
                    "core",
                    op,
                    "staged-op-name-in-use",
                ));
            }
        }
        if let Some(slot) = self.ops.iter_mut().find(|(known, _)| known == name) {
            slot.1 = id;
        } else {
            self.ops.push((String::from(name), id));
        }
        Ok(())
    }

    fn path_index_of(&self, obj: &JsonValue, op: &str) -> Result<usize, StagedError> {
        let raw = u64_param(obj, "path", None, op)?;
        usize::try_from(raw)
            .map_err(|_| StagedError::new("argument.invalid", "core", op, "staged-path-range"))
    }

    /// Append one observation line to the deterministic log and return it.
    fn emit(&mut self, _step: &str, _op: &str, line: String) -> String {
        self.log.push(line.clone());
        line
    }

    fn scan_start_step(&mut self, obj: &JsonValue) -> String {
        const OP: &str = "staged-scan-start";
        const STEP: &str = "scan.start";
        let line = match self.scan_start_inner(obj) {
            Ok(line) => line,
            Err(error) => err_ob(STEP, &error),
        };
        self.emit(STEP, OP, line)
    }

    fn scan_start_inner(&mut self, obj: &JsonValue) -> Result<String, StagedError> {
        const OP: &str = "staged-scan-start";
        const STEP: &str = "scan.start";
        let now = self.advance_clock(obj, OP)?;
        let owner = str_param_default(obj, "owner", "staged-owner", OP)?;
        let duplicate = str_param_default(obj, "duplicate", "first", OP)?;
        let merge = str_param_default(obj, "merge", "none", OP)?;
        let timeout_ms = u64_param(obj, "timeout_ms", Some(5_000), OP)?;
        let services = str_list_param(obj, "services", OP)?;
        let service_refs: Vec<&str> = services.iter().map(String::as_str).collect();
        let request = ubm_core::central::validate_scan_request(
            &service_refs,
            duplicate,
            merge,
            timeout_ms,
            false,
            &[],
        )
        .map_err(|core| central_error(&core, OP))?;
        let share = opt_str_param(obj, "share_token", OP)?;
        let mut out = self.batch();
        let id = match self
            .central
            .start_scan(&request, share, owner, now, &mut out)
        {
            Ok(id) => id,
            Err(core) => {
                self.note_batch_full(&core);
                return Err(central_error(&core, OP));
            }
        };
        let op_name = str_param(obj, "op", OP)?;
        self.register_op(op_name, id.clone(), OP)?;
        let mut ob = ok_ob(STEP);
        ob.str_field("op_id", id.as_str());
        ob.str_field("owner", owner);
        self.note_staged(&mut ob, &mut out);
        Ok(ob.finish())
    }

    fn scan_platform_step(&mut self, obj: &JsonValue) -> String {
        const OP: &str = "staged-scan-platform";
        const STEP: &str = "scan.platform";
        let line = match self.scan_platform_inner(obj) {
            Ok(line) => line,
            Err(error) => err_ob(STEP, &error),
        };
        self.emit(STEP, OP, line)
    }

    fn scan_platform_inner(&mut self, obj: &JsonValue) -> Result<String, StagedError> {
        const OP: &str = "staged-scan-platform";
        const STEP: &str = "scan.platform";
        let now = self.advance_clock(obj, OP)?;
        let id = self.op_id_of(obj, OP)?;
        let event_name = str_param(obj, "event", OP)?;
        let event = parse_scan_event(event_name)
            .ok_or_else(|| StagedError::new("argument.invalid", "core", OP, "staged-scan-event"))?;
        let mut out = self.batch();
        let state = if event == ScanPlatformEvent::PlatformStarted
            && self.central.scan_session_state(&id)
                == Some(ubm_core::central::ScanSessionState::Starting)
        {
            self.central
                .platform_scan_started(&id)
                .map(|()| ubm_core::central::ScanSessionState::Active)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, OP)
                })?;
            ubm_core::central::ScanSessionState::Active
        } else if event == ScanPlatformEvent::Stop {
            self.central.stop_scan(&id, now, &mut out).map_err(|core| {
                self.note_batch_full(&core);
                central_error(&core, OP)
            })?;
            self.central
                .scan_session_state(&id)
                .unwrap_or(ubm_core::central::ScanSessionState::Failed)
        } else {
            self.central
                .note_scan_platform(&id, event, now, &mut out)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, OP)
                })?
        };
        let mut ob = ok_ob(STEP);
        ob.str_field("event", event_name);
        ob.str_field("state", state.as_str());
        self.note_staged(&mut ob, &mut out);
        Ok(ob.finish())
    }

    fn advertise_step(&mut self, obj: &JsonValue) -> String {
        const OP: &str = "staged-peer-advertise";
        const STEP: &str = "peer.advertise";
        let line = match self.advertise_inner(obj) {
            Ok(line) => line,
            Err(error) => err_ob(STEP, &error),
        };
        self.emit(STEP, OP, line)
    }

    fn advertise_inner(&mut self, obj: &JsonValue) -> Result<String, StagedError> {
        const OP: &str = "staged-peer-advertise";
        const STEP: &str = "peer.advertise";
        let alias = str_param(obj, "peer", OP)?;
        let domain = str_param_default(obj, "domain", "platform-guid", OP)?;
        let value = str_param(obj, "value", OP)?;
        let key = self
            .central
            .resolve_peer(domain, value)
            .map_err(|core| central_error(&core, OP))?;
        if let Some(slot) = self.peers.iter_mut().find(|(name, _)| name == alias) {
            slot.1 = key.clone();
        } else {
            self.peers.push((String::from(alias), key.clone()));
        }
        let mut ob = ok_ob(STEP);
        ob.str_field("peer_key", &key);
        ob.str_field("domain", domain);
        self.note_staged_empty(&mut ob);
        Ok(ob.finish())
    }

    /// Record a step with no kernel staging (pure registry reads stay
    /// observable in the sequencing log with an explicit zero).
    fn note_staged_empty(&mut self, ob: &mut Ob) {
        let mut out = self.batch();
        self.note_staged(ob, &mut out);
    }

    fn connect_step(&mut self, obj: &JsonValue) -> String {
        const OP: &str = "staged-link-connect";
        const STEP: &str = "link.connect";
        let line = match self.connect_inner(obj) {
            Ok(line) => line,
            Err(error) => err_ob(STEP, &error),
        };
        self.emit(STEP, OP, line)
    }

    fn connect_inner(&mut self, obj: &JsonValue) -> Result<String, StagedError> {
        const OP: &str = "staged-link-connect";
        const STEP: &str = "link.connect";
        let now = self.advance_clock(obj, OP)?;
        let peer_key = self.peer_key_of(obj, OP)?;
        let lease = str_param_default(obj, "lease", "staged-lease", OP)?;
        let timeout_ms = u64_param(obj, "timeout_ms", Some(5_000), OP)?;
        let mut out = self.batch();
        let id = match self
            .central
            .connect(&peer_key, lease, timeout_ms, now, &mut out)
        {
            Ok(id) => id,
            Err(core) => {
                self.note_batch_full(&core);
                return Err(central_error(&core, OP));
            }
        };
        let op_name = str_param(obj, "op", OP)?;
        self.register_op(op_name, id.clone(), OP)?;
        let mut ob = ok_ob(STEP);
        ob.str_field("op_id", id.as_str());
        ob.str_field("peer_key", &peer_key);
        self.note_staged(&mut ob, &mut out);
        Ok(ob.finish())
    }

    fn link_lease_step(&mut self, obj: &JsonValue, step: &'static str, op: &'static str) -> String {
        let line = match self.link_lease_inner(obj, step, op) {
            Ok(line) => line,
            Err(error) => err_ob(step, &error),
        };
        self.emit(step, op, line)
    }

    fn link_lease_inner(
        &mut self,
        obj: &JsonValue,
        step: &str,
        op: &'static str,
    ) -> Result<String, StagedError> {
        let now = self.advance_clock(obj, op)?;
        // Sharing support is a central-wide host report (the backend tells
        // the core whether links are shareable); it needs no peer and must
        // precede `connect` because each connection captures the flag at
        // admission.
        if step == "link.sharing" {
            let supported = bool_param(obj, "supported", true, op)?;
            self.central.set_sharing_supported(supported);
            let mut out = self.batch();
            let mut ob = ok_ob(step);
            ob.bool_field("supported", supported);
            self.note_staged(&mut ob, &mut out);
            return Ok(ob.finish());
        }
        let peer_key = self.peer_key_of(obj, op)?;
        let mut out = self.batch();
        let mut ob = ok_ob(step);
        if step == "link.borrow" {
            let lease = str_param(obj, "lease", op)?;
            let timeout_ms = u64_param(obj, "timeout_ms", Some(5_000), op)?;
            let id = self
                .central
                .borrow_connection(&peer_key, lease, timeout_ms, now, &mut out)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
            let op_name = str_param(obj, "op", op)?.to_string();
            self.register_op(&op_name, id.clone(), op)?;
            ob.str_field("op_id", id.as_str());
        } else if step == "link.transfer" {
            let source = str_param(obj, "source", op)?;
            let dest = str_param(obj, "dest", op)?;
            let generation = self
                .central
                .connection_generation(&peer_key)
                .unwrap_or_default();
            let generation = opt_str_param(obj, "generation", op)?.unwrap_or(&generation);
            let generation = generation.to_string();
            let epoch = u64_param(obj, "epoch", Some(0), op)?;
            self.central
                .transfer_lease(&peer_key, source, dest, &generation, epoch)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
        } else {
            let lease = str_param(obj, "lease", op)?;
            let released = self
                .central
                .release_lease(&peer_key, lease, now, &mut out)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
            ob.bool_field("released", released);
        }
        ob.num_field(
            "lease_count",
            self.central.connection_lease_count(&peer_key) as u64,
        );
        self.note_staged(&mut ob, &mut out);
        Ok(ob.finish())
    }

    fn link_event_step(&mut self, obj: &JsonValue, step: &'static str, op: &'static str) -> String {
        let line = match self.link_event_inner(obj, step, op) {
            Ok(line) => line,
            Err(error) => err_ob(step, &error),
        };
        self.emit(step, op, line)
    }

    fn link_event_inner(
        &mut self,
        obj: &JsonValue,
        step: &str,
        op: &'static str,
    ) -> Result<String, StagedError> {
        let now = self.advance_clock(obj, op)?;
        let peer_key = self.peer_key_of(obj, op)?;
        let mut out = self.batch();
        let mut ob = ok_ob(step);
        if step == "link.established" {
            self.central
                .note_link_established(&peer_key)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
            // The synthetic radio reports the link up: settle the staged
            // connect op as a radio success so receipts stay truthful.
            if let Some(name) = obj.field("op").and_then(JsonValue::as_str) {
                let id = self.op_id_of(obj, op)?;
                let ordinal = self.next_ordinal();
                self.central.dispatch_op(&id, &mut out).map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
                let outcome = self
                    .central
                    .settle_op(&id, ContenderKind::Success, true, ordinal, now, &mut out)
                    .map_err(|core| {
                        self.note_batch_full(&core);
                        central_error(&core, op)
                    })?;
                render_outcome(&mut ob, &outcome);
                let _ = name;
            }
        } else if step == "link.loss" {
            let state = self
                .central
                .note_peer_loss(&peer_key, now, &mut out)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
            ob.str_field("state", state.as_str());
        } else if step == "link.released" {
            self.central.note_link_released(&peer_key).map_err(|core| {
                self.note_batch_full(&core);
                central_error(&core, op)
            })?;
        } else if step == "link.disconnect" {
            let lease = str_param_default(obj, "lease", "staged-lease", op)?;
            self.central
                .disconnect(&peer_key, lease, now, &mut out)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
        } else {
            let code = str_param_default(obj, "code", "connection-failed", op)?;
            let failure = parse_disconnect_code(code).ok_or_else(|| {
                StagedError::new("argument.invalid", "core", op, "staged-disconnect-code")
            })?;
            self.central
                .report_disconnect_failure(&peer_key, failure)
                .map_err(|core| central_error(&core, op))?;
        }
        if let Some(state) = self.central.connection_state(&peer_key) {
            ob.str_field("connection", state.as_str());
        } else {
            ob.null_field("connection");
        }
        self.note_staged(&mut ob, &mut out);
        Ok(ob.finish())
    }
}

/// Read one required string parameter.
fn str_param<'b>(obj: &'b JsonValue, name: &str, op: &str) -> Result<&'b str, StagedError> {
    obj.require_str(name)
        .map_err(|field| StagedError::new("argument.invalid", "core", op, field.detail()))
}

/// Read one optional string parameter.
fn opt_str_param<'b>(
    obj: &'b JsonValue,
    name: &str,
    op: &str,
) -> Result<Option<&'b str>, StagedError> {
    obj.optional_str(name)
        .map_err(|field| StagedError::new("argument.invalid", "core", op, field.detail()))
}

/// Read one string parameter with a default when absent or null.
fn str_param_default<'b>(
    obj: &'b JsonValue,
    name: &str,
    default: &'b str,
    op: &str,
) -> Result<&'b str, StagedError> {
    match obj.field(name) {
        None | Some(JsonValue::Null) => Ok(default),
        Some(JsonValue::Str(text)) => Ok(text),
        Some(_) => Err(StagedError::new(
            "argument.invalid",
            "core",
            op,
            "staged-field-not-string",
        )),
    }
}

/// Read one u64 parameter (JSON number or decimal string), with a default
/// when absent or null. Malformed decimals fail closed as staged errors.
fn u64_param(
    obj: &JsonValue,
    name: &str,
    default: Option<u64>,
    op: &str,
) -> Result<u64, StagedError> {
    let bad = |detail: &'static str| StagedError::new("argument.invalid", "core", op, detail);
    match obj.field(name) {
        None | Some(JsonValue::Null) => default.ok_or_else(|| bad("staged-missing-field")),
        Some(JsonValue::Number(raw)) => raw.parse::<u64>().map_err(|_| bad("staged-bad-number")),
        Some(JsonValue::Str(text)) => text.parse::<u64>().map_err(|_| bad("staged-bad-number")),
        Some(_) => Err(bad("staged-field-not-number")),
    }
}

/// Read one boolean parameter with a default when absent or null.
fn bool_param(obj: &JsonValue, name: &str, default: bool, op: &str) -> Result<bool, StagedError> {
    match obj.field(name) {
        None | Some(JsonValue::Null) => Ok(default),
        Some(JsonValue::Bool(value)) => Ok(*value),
        Some(_) => Err(StagedError::new(
            "argument.invalid",
            "core",
            op,
            "staged-field-not-bool",
        )),
    }
}

/// Read one string-list parameter (absent/null means empty).
fn str_list_param(obj: &JsonValue, name: &str, op: &str) -> Result<Vec<String>, StagedError> {
    match obj.field(name) {
        None | Some(JsonValue::Null) => Ok(Vec::new()),
        Some(JsonValue::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    JsonValue::Str(text) => out.push(text.clone()),
                    _ => {
                        return Err(StagedError::new(
                            "argument.invalid",
                            "core",
                            op,
                            "staged-field-not-string-list",
                        ));
                    }
                }
            }
            Ok(out)
        }
        Some(_) => Err(StagedError::new(
            "argument.invalid",
            "core",
            op,
            "staged-field-not-list",
        )),
    }
}

/// Parse a disconnect-failure code from its frozen wire string.
fn parse_disconnect_code(text: &str) -> Option<ubm_core::contracts::BleErrorCode> {
    use ubm_core::contracts::BleErrorCode as C;
    match text {
        "connection-failed" => Some(C::ConnectionFailed),
        "connection-lost" => Some(C::ConnectionLost),
        "operation-timed-out" => Some(C::OperationTimedOut),
        "adapter-unavailable" => Some(C::AdapterUnavailable),
        _ => None,
    }
}

impl StagedDriver {
    fn discover_step(&mut self, obj: &JsonValue) -> String {
        const OP: &str = "staged-gatt-discover";
        const STEP: &str = "gatt.discover";
        let line = match self.discover_inner(obj) {
            Ok(line) => line,
            Err(error) => err_ob(STEP, &error),
        };
        self.emit(STEP, OP, line)
    }

    fn discover_inner(&mut self, obj: &JsonValue) -> Result<String, StagedError> {
        const OP: &str = "staged-gatt-discover";
        const STEP: &str = "gatt.discover";
        let peer_key = self.peer_key_of(obj, OP)?;
        let owner = str_param_default(obj, "owner", "staged-lease", OP)?;
        let services = parse_snapshot(obj, OP)?;
        let mut out = self.batch();
        self.central.begin_discovery(&peer_key).map_err(|core| {
            self.note_batch_full(&core);
            central_error(&core, OP)
        })?;
        // The snapshot lands first (`snapshot-complete` moves the database
        // to `current` under a fresh generation); stored occurrence paths
        // register against the current database, mirroring the core's own
        // discovery flow.
        self.central.complete_discovery(&peer_key).map_err(|core| {
            self.note_batch_full(&core);
            central_error(&core, OP)
        })?;
        let mut first_index: Option<usize> = None;
        let mut count: usize = 0;
        for service in &services {
            let index = self.register_snapshot(&peer_key, service, None, None, owner, OP)?;
            if first_index.is_none() {
                first_index = Some(index);
            }
            count = count.saturating_add(1);
        }
        let live = self
            .central
            .snapshot_path_count(&peer_key)
            .map_err(|core| central_error(&core, OP))?;
        let mut ob = ok_ob(STEP);
        ob.str_field("peer_key", &peer_key);
        ob.num_field("services", count as u64);
        ob.num_field("paths", live as u64);
        match first_index {
            Some(index) => ob.num_field("first_path", index as u64),
            None => ob.null_field("first_path"),
        }
        self.note_staged(&mut ob, &mut out);
        Ok(ob.finish())
    }

    /// Register one service node plus its characteristics/descriptors as
    /// stored occurrence paths. Returns the service-level path index.
    fn register_snapshot(
        &mut self,
        peer_key: &str,
        service: &SnapshotNode,
        characteristic: Option<&SnapshotNode>,
        descriptor: Option<&SnapshotNode>,
        owner: &str,
        op: &str,
    ) -> Result<usize, StagedError> {
        let index = self
            .central
            .register_path(
                peer_key,
                &service.uuid,
                service.occurrence,
                characteristic.map(|node| node.uuid.as_str()),
                characteristic.map(|node| node.occurrence),
                descriptor.map(|node| node.uuid.as_str()),
                descriptor.map(|node| node.occurrence),
                snapshot_properties(service, characteristic, descriptor),
                owner,
            )
            .map_err(|core| central_error(&core, op))?;
        if characteristic.is_none() {
            for char_node in &service.characteristics {
                self.register_snapshot(peer_key, service, Some(char_node), None, owner, op)?;
                for desc_node in &char_node.descriptors {
                    self.register_snapshot(
                        peer_key,
                        service,
                        Some(char_node),
                        Some(desc_node),
                        owner,
                        op,
                    )?;
                }
            }
        }
        let _ = descriptor;
        Ok(index)
    }

    fn database_event_step(
        &mut self,
        obj: &JsonValue,
        step: &'static str,
        op: &'static str,
    ) -> String {
        let line = match self.peer_key_of(obj, op).and_then(|peer_key| {
            let result = if step == "gatt.discovery-failed" {
                self.central.fail_discovery(&peer_key)
            } else if step == "gatt.services-changed" {
                self.central.services_changed(&peer_key)
            } else {
                self.central.require_rediscovery(&peer_key)
            };
            result.map(|()| peer_key.clone()).map_err(|core| {
                self.note_batch_full(&core);
                central_error(&core, op)
            })
        }) {
            Ok(peer_key) => {
                let mut ob = ok_ob(step);
                ob.str_field("peer_key", &peer_key);
                match self.central.database_state(&peer_key) {
                    Some(state) => ob.str_field("database", state.as_str()),
                    None => ob.null_field("database"),
                }
                let mut out = self.batch();
                self.note_staged(&mut ob, &mut out);
                ob.finish()
            }
            Err(error) => err_ob(step, &error),
        };
        self.emit(step, op, line)
    }

    fn resolve_step(&mut self, obj: &JsonValue) -> String {
        const OP: &str = "staged-gatt-resolve";
        const STEP: &str = "gatt.resolve";
        let line = match self.resolve_inner(obj) {
            Ok(line) => line,
            Err(error) => err_ob(STEP, &error),
        };
        self.emit(STEP, OP, line)
    }

    fn resolve_inner(&mut self, obj: &JsonValue) -> Result<String, StagedError> {
        const OP: &str = "staged-gatt-resolve";
        const STEP: &str = "gatt.resolve";
        let peer_key = self.peer_key_of(obj, OP)?;
        let selector = parse_selector(obj, OP)?;
        let index = self
            .central
            .resolve_path(&peer_key, &selector)
            .map_err(|core| central_error(&core, OP))?;
        let mut ob = ok_ob(STEP);
        ob.num_field("path", index as u64);
        self.note_staged_empty(&mut ob);
        Ok(ob.finish())
    }

    fn io_step(&mut self, obj: &JsonValue, step: &'static str, op: &'static str) -> String {
        let line = match self.io_inner(obj, step, op) {
            Ok(line) => line,
            Err(error) => err_ob(step, &error),
        };
        self.emit(step, op, line)
    }

    fn io_inner(
        &mut self,
        obj: &JsonValue,
        step: &str,
        op: &'static str,
    ) -> Result<String, StagedError> {
        let now = self.advance_clock(obj, op)?;
        let path_index = self.path_index_of(obj, op)?;
        let timeout_ms = u64_param(obj, "timeout_ms", Some(5_000), op)?;
        let hex = str_param_default(obj, "value", "", op)?;
        let payload = parse_hex(hex)
            .ok_or_else(|| StagedError::new("argument.invalid", "core", op, "staged-bad-hex"))?;
        let mut out = self.batch();
        let is_read = step == "gatt.read" || step == "gatt.read-descriptor";
        let op_name = str_param(obj, "op", op)?.to_string();
        let id = if step == "gatt.read" {
            self.central
                .start_read(path_index, timeout_ms, now, &mut out)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?
        } else if step == "gatt.read-descriptor" {
            self.central
                .start_read_descriptor(path_index, timeout_ms, now, &mut out)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?
        } else {
            let mode = str_param_default(obj, "mode", "with-response", op)?;
            let supported = bool_param(obj, "mode_supported", true, op)?;
            let maximum = match obj.field("maximum") {
                None | Some(JsonValue::Null) => None,
                Some(_) => Some(u64_param(obj, "maximum", None, op)?),
            };
            if step == "gatt.write" {
                self.central
                    .start_write(
                        path_index,
                        mode,
                        payload.len() as u64,
                        maximum,
                        supported,
                        timeout_ms,
                        now,
                        &mut out,
                    )
                    .map_err(|core| {
                        self.note_batch_full(&core);
                        central_error(&core, op)
                    })?
            } else {
                self.central
                    .start_write_descriptor(
                        path_index,
                        payload.len() as u64,
                        maximum,
                        timeout_ms,
                        now,
                        &mut out,
                    )
                    .map_err(|core| {
                        self.note_batch_full(&core);
                        central_error(&core, op)
                    })?
            }
        };
        self.register_op(&op_name, id.clone(), op)?;
        // The synthetic radio answers immediately: dispatch, then settle as
        // a radio success unless the script names another contender.
        let settle = str_param_default(obj, "settle", "dispatched", op)?;
        let mut ob = ok_ob(step);
        ob.str_field("op_id", id.as_str());
        if settle != "admitted" {
            self.central.dispatch_op(&id, &mut out).map_err(|core| {
                self.note_batch_full(&core);
                central_error(&core, op)
            })?;
        }
        if settle == "success" {
            let ordinal = self.next_ordinal();
            let outcome = self
                .central
                .settle_op(&id, ContenderKind::Success, true, ordinal, now, &mut out)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
            render_outcome(&mut ob, &outcome);
        } else if settle != "admitted" && settle != "dispatched" {
            return Err(StagedError::new(
                "argument.invalid",
                "core",
                op,
                "staged-settle-mode",
            ));
        }
        if is_read {
            self.read_values.retain(|(known, _)| known != &op_name);
            self.read_values.push((op_name, payload.clone()));
            ob.str_field("bytes", &hex_of(&payload));
        } else {
            ob.str_field("bytes", &hex_of(&payload));
        }
        self.note_staged(&mut ob, &mut out);
        Ok(ob.finish())
    }
}

/// Parse one synthetic discovery snapshot (`services` array) into nodes.
fn parse_snapshot(obj: &JsonValue, op: &str) -> Result<Vec<SnapshotNode>, StagedError> {
    let bad = |detail: &'static str| StagedError::new("argument.invalid", "core", op, detail);
    let services = match obj.field("services") {
        None | Some(JsonValue::Null) => return Ok(Vec::new()),
        Some(JsonValue::Array(items)) => items,
        Some(_) => return Err(bad("staged-services-not-list")),
    };
    let mut out = Vec::with_capacity(services.len());
    for service in services {
        out.push(parse_service_node(service).map_err(|_| bad("staged-bad-snapshot"))?);
    }
    Ok(out)
}

fn node_uuid(obj: &JsonValue) -> Option<String> {
    obj.require_str("uuid").ok().map(String::from)
}

fn node_occurrence(obj: &JsonValue) -> u64 {
    match obj.field("occurrence") {
        Some(JsonValue::Number(raw)) => raw.parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

fn parse_service_node(obj: &JsonValue) -> Result<SnapshotNode, ()> {
    let uuid = node_uuid(obj).ok_or(())?;
    let occurrence = node_occurrence(obj);
    let mut characteristics = Vec::new();
    if let Some(JsonValue::Array(items)) = obj.field("characteristics") {
        for item in items {
            characteristics.push(parse_char_node(item)?);
        }
    }
    Ok(SnapshotNode {
        uuid,
        occurrence,
        properties: 0,
        descriptors: Vec::new(),
        characteristics,
    })
}

fn parse_char_node(obj: &JsonValue) -> Result<SnapshotNode, ()> {
    let uuid = node_uuid(obj).ok_or(())?;
    let occurrence = node_occurrence(obj);
    let properties = match obj.field("properties") {
        Some(JsonValue::Str(text)) => parse_properties(text).ok_or(())?,
        None | Some(JsonValue::Null) => 0,
        Some(_) => return Err(()),
    };
    let mut descriptors = Vec::new();
    if let Some(JsonValue::Array(items)) = obj.field("descriptors") {
        for item in items {
            let uuid = node_uuid(item).ok_or(())?;
            descriptors.push(SnapshotNode {
                uuid,
                occurrence: node_occurrence(item),
                properties: 0,
                descriptors: Vec::new(),
                characteristics: Vec::new(),
            });
        }
    }
    Ok(SnapshotNode {
        uuid,
        occurrence,
        properties,
        descriptors,
        characteristics: Vec::new(),
    })
}

/// Property mask for one registered path: service-level paths carry none,
/// characteristic paths carry their flags, descriptor paths carry the
/// parent characteristic flags (readability gate for descriptor IO).
fn snapshot_properties(
    service: &SnapshotNode,
    characteristic: Option<&SnapshotNode>,
    descriptor: Option<&SnapshotNode>,
) -> u8 {
    let _ = service;
    let _ = descriptor;
    characteristic.map(|node| node.properties).unwrap_or(0)
}

/// Parse a GATT path selector (`service` + optional occurrences/levels).
fn parse_selector(obj: &JsonValue, op: &str) -> Result<PathSelector, StagedError> {
    let bad = |detail: &'static str| StagedError::new("argument.invalid", "core", op, detail);
    let service_uuid = obj
        .require_str("service")
        .map_err(|_| bad("staged-selector-service"))?;
    let service_occurrence = match obj.field("service_occurrence") {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::Number(raw)) => Some(
            raw.parse::<u64>()
                .map_err(|_| bad("staged-selector-number"))?,
        ),
        Some(_) => return Err(bad("staged-selector-number")),
    };
    let characteristic_uuid = match obj.field("characteristic") {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::Str(text)) => Some(text.clone()),
        Some(_) => return Err(bad("staged-selector-characteristic")),
    };
    let characteristic_occurrence = match obj.field("characteristic_occurrence") {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::Number(raw)) => Some(
            raw.parse::<u64>()
                .map_err(|_| bad("staged-selector-number"))?,
        ),
        Some(_) => return Err(bad("staged-selector-number")),
    };
    let descriptor_uuid = match obj.field("descriptor") {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::Str(text)) => Some(text.clone()),
        Some(_) => return Err(bad("staged-selector-descriptor")),
    };
    let descriptor_occurrence = match obj.field("descriptor_occurrence") {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::Str(text)) => Some(
            text.parse::<u64>()
                .map_err(|_| bad("staged-selector-number"))?,
        ),
        Some(JsonValue::Number(raw)) => Some(
            raw.parse::<u64>()
                .map_err(|_| bad("staged-selector-number"))?,
        ),
        Some(_) => return Err(bad("staged-selector-number")),
    };
    Ok(PathSelector {
        service_uuid: String::from(service_uuid),
        service_occurrence,
        characteristic_uuid,
        characteristic_occurrence,
        descriptor_uuid,
        descriptor_occurrence,
    })
}

impl StagedDriver {
    fn op_step(&mut self, obj: &JsonValue, step: &'static str, op: &'static str) -> String {
        let line = match self.op_inner(obj, step, op) {
            Ok(line) => line,
            Err(error) => err_ob(step, &error),
        };
        self.emit(step, op, line)
    }

    fn op_inner(
        &mut self,
        obj: &JsonValue,
        step: &str,
        op: &'static str,
    ) -> Result<String, StagedError> {
        let now = self.advance_clock(obj, op)?;
        let mut out = self.batch();
        let mut ob = ok_ob(step);
        if step == "op.expire-sweep" {
            let (settled, truncated) =
                self.central.expire_sweep(now, &mut out).map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
            if truncated {
                self.truncated_sweeps = self.truncated_sweeps.saturating_add(1);
            }
            ob.num_field("settled", settled as u64);
            ob.bool_field("truncated", truncated);
            self.note_staged(&mut ob, &mut out);
            return Ok(ob.finish());
        }
        let id = self.op_id_of(obj, op)?;
        if step == "op.dispatch" {
            self.central.dispatch_op(&id, &mut out).map_err(|core| {
                self.note_batch_full(&core);
                central_error(&core, op)
            })?;
            ob.str_field("op_id", id.as_str());
        } else if step == "op.cancel" {
            let outcome = self.central.cancel_op(&id, now, &mut out).map_err(|core| {
                self.note_batch_full(&core);
                central_error(&core, op)
            })?;
            render_outcome(&mut ob, &outcome);
        } else {
            let kind_name = str_param(obj, "kind", op)?;
            let kind = parse_contender(kind_name).ok_or_else(|| {
                StagedError::new("argument.invalid", "core", op, "staged-contender-kind")
            })?;
            let valid = bool_param(obj, "valid", true, op)?;
            let ordinal = match obj.field("ordinal") {
                None | Some(JsonValue::Null) => self.next_ordinal(),
                Some(_) => u64_param(obj, "ordinal", None, op)?,
            };
            let outcome = self
                .central
                .settle_op(&id, kind, valid, ordinal, now, &mut out)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
            render_outcome(&mut ob, &outcome);
        }
        self.note_staged(&mut ob, &mut out);
        Ok(ob.finish())
    }

    fn subscribe_step(&mut self, obj: &JsonValue) -> String {
        const OP: &str = "staged-subscribe";
        const STEP: &str = "sub.subscribe";
        let line = match self.subscribe_inner(obj) {
            Ok(line) => line,
            Err(error) => err_ob(STEP, &error),
        };
        self.emit(STEP, OP, line)
    }

    fn subscribe_inner(&mut self, obj: &JsonValue) -> Result<String, StagedError> {
        const OP: &str = "staged-subscribe";
        const STEP: &str = "sub.subscribe";
        let now = self.advance_clock(obj, OP)?;
        let path_index = self.path_index_of(obj, OP)?;
        let policy = str_param_default(obj, "policy", "error", OP)?;
        let items = u64_param(obj, "items", Some(8), OP)?;
        let bytes = u64_param(obj, "bytes", Some(256), OP)?;
        let consumer = str_param_default(obj, "consumer", "staged-consumer", OP)?;
        let timeout_ms = u64_param(obj, "timeout_ms", Some(5_000), OP)?;
        let mut out = self.batch();
        let id = self
            .central
            .subscribe(
                path_index, policy, items, bytes, consumer, timeout_ms, now, &mut out,
            )
            .map_err(|core| {
                self.note_batch_full(&core);
                central_error(&core, OP)
            })?;
        let op_name = str_param(obj, "op", OP)?.to_string();
        self.register_op(&op_name, id.clone(), OP)?;
        let mut ob = ok_ob(STEP);
        ob.str_field("op_id", id.as_str());
        match self.central.consumer_state(path_index, consumer) {
            Some(state) => ob.str_field("consumer", state.as_str()),
            None => ob.null_field("consumer"),
        }
        ob.bool_field("cccd", self.central.physical_cccd_enabled(path_index));
        self.note_staged(&mut ob, &mut out);
        Ok(ob.finish())
    }

    fn sub_event_step(&mut self, obj: &JsonValue, step: &'static str, op: &'static str) -> String {
        let line = match self.sub_event_inner(obj, step, op) {
            Ok(line) => line,
            Err(error) => err_ob(step, &error),
        };
        self.emit(step, op, line)
    }

    fn sub_event_inner(
        &mut self,
        obj: &JsonValue,
        step: &str,
        op: &'static str,
    ) -> Result<String, StagedError> {
        let now = self.advance_clock(obj, op)?;
        let path_index = self.path_index_of(obj, op)?;
        let mut out = self.batch();
        let mut ob = ok_ob(step);
        if step == "sub.settle-enable" {
            let success = bool_param(obj, "success", true, op)?;
            self.central
                .settle_subscribe_enable(path_index, success, now, &mut out)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
        } else if step == "sub.unsubscribe" {
            let consumer = str_param_default(obj, "consumer", "staged-consumer", op)?;
            let physical = self
                .central
                .unsubscribe(path_index, consumer, now, &mut out)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
            ob.bool_field("physical_disable", physical);
        } else if step == "sub.settle-disable" {
            self.central
                .settle_subscribe_disable(path_index, now, &mut out)
                .map_err(|core| {
                    self.note_batch_full(&core);
                    central_error(&core, op)
                })?;
        } else if step == "sub.notify" {
            let hex = str_param_default(obj, "value", "", op)?;
            let payload = parse_hex(hex).ok_or_else(|| {
                StagedError::new("argument.invalid", "core", op, "staged-bad-hex")
            })?;
            let outcomes = self
                .central
                .deliver_notification_value(path_index, &payload)
                .map_err(|core| central_error(&core, op))?;
            let mut seq = String::new();
            for (index, (lease, outcome)) in outcomes.iter().enumerate() {
                if index > 0 {
                    seq.push(';');
                }
                seq.push_str(lease);
                seq.push('=');
                seq.push_str(delivery_str(*outcome));
            }
            ob.str_field("bytes", &hex_of(&payload));
            ob.str_field("delivery", &seq);
        } else if step == "sub.take" {
            let consumer = str_param_default(obj, "consumer", "staged-consumer", op)?;
            match self.central.take_notification_value(path_index, consumer) {
                Some(value) => ob.str_field("bytes", &hex_of(&value)),
                None => ob.null_field("bytes"),
            }
        } else if step == "sub.take-terminal" {
            let consumer = str_param_default(obj, "consumer", "staged-consumer", op)?;
            match self.central.take_terminal(path_index, consumer) {
                Some(terminal) => {
                    ob.str_field("reason", terminal.reason());
                    ob.num_field("dropped_items", terminal.dropped_items());
                    ob.num_field("dropped_bytes", terminal.dropped_bytes());
                    ob.num_field("replaced_items", terminal.replaced_items());
                }
                None => ob.null_field("terminal"),
            }
        } else if step == "sub.quarantined" {
            let consumer = str_param_default(obj, "consumer", "staged-consumer", op)?;
            match self.central.quarantined_count(path_index, consumer) {
                Some(count) => ob.num_field("quarantined", count),
                None => ob.null_field("quarantined"),
            }
            self.note_staged_empty(&mut ob);
            return Ok(ob.finish());
        } else {
            return Err(StagedError::new(
                "argument.invalid",
                "core",
                op,
                "staged-sub-event",
            ));
        }
        if step == "sub.subscribe" || step.starts_with("sub.") {
            let consumer = str_param_default(obj, "consumer", "staged-consumer", op)?;
            match self.central.consumer_state(path_index, consumer) {
                Some(state) => ob.str_field("consumer", state.as_str()),
                None => ob.null_field("consumer"),
            }
            ob.bool_field("cccd", self.central.physical_cccd_enabled(path_index));
        }
        self.note_staged(&mut ob, &mut out);
        Ok(ob.finish())
    }

    fn cap_step(&mut self, obj: &JsonValue, step: &'static str, op: &'static str) -> String {
        let line = match self.cap_inner(obj, step, op) {
            Ok(line) => line,
            Err(error) => err_ob(step, &error),
        };
        self.emit(step, op, line)
    }

    fn cap_inner(
        &mut self,
        obj: &JsonValue,
        step: &str,
        op: &'static str,
    ) -> Result<String, StagedError> {
        let mut ob = ok_ob(step);
        if step == "cap.project" {
            for id in STAGED_CAPABILITY_IDS {
                let scenario = match id {
                    "central.scan" => "scan.owner-join-authority-and-signature",
                    "central.connect" => "connection.lease-joins-borrowing-transfer-and-revocation",
                    "central.discover" => "gatt.discovery-complete-paths-and-services-changed",
                    "central.read" => {
                        "gatt.reads-descriptors-write-policy-and-dispatched-cancellation"
                    }
                    "central.write" => "gatt.descriptor-discovery-read-write",
                    "central.subscribe" => "subscription.enable-ready-shared-cccd-and-fanout",
                    _ => "scenario.scan-connect-discover-read-notify-destroy",
                };
                let descriptor = CapabilityDescriptor::new(
                    id,
                    CapabilityState::Limited,
                    &[("staged-batch", STAGED_BATCH_MAX as u64)],
                    &["synthetic-radio-only"],
                    "staged-synthetic-001",
                    EvidenceLevel::Deterministic,
                    "0.1.0-staged",
                    "synthetic-no-radio",
                    &[scenario],
                )
                .map_err(|core| central_error(&core, op))?;
                self.central
                    .register_capability(descriptor)
                    .map_err(|core| central_error(&core, op))?;
            }
            ob.num_field("rows", STAGED_CAPABILITY_IDS.len() as u64);
        } else if step == "cap.check" {
            let id = str_param(obj, "id", op)?;
            let operation = str_param_default(obj, "operation", "staged-capability", op)?;
            match self.central.check_capability(id, operation) {
                Ok(CapabilityAdmission::Proceed) => {
                    ob.str_field("admission", "proceed");
                }
                Ok(CapabilityAdmission::ProceedWithLimitation) => {
                    ob.str_field("admission", "proceed-with-limitation");
                }
                Err(core) => return Err(central_error(&core, op)),
            }
        } else if step == "cap.rows" {
            let rows = self.central.parity_rows();
            let mut seq = String::new();
            for (index, (id, state)) in rows.iter().enumerate() {
                if index > 0 {
                    seq.push(';');
                }
                seq.push_str(id);
                seq.push('=');
                seq.push_str(match state {
                    CapabilityState::Supported => "supported",
                    CapabilityState::Limited => "limited",
                    CapabilityState::Unsupported => "unsupported",
                    CapabilityState::Unavailable => "unavailable",
                });
            }
            ob.str_field("rows", &seq);
            ob.num_field("count", rows.len() as u64);
        } else if step == "cap.set" {
            let id = str_param(obj, "id", op)?;
            let state_name = str_param(obj, "state", op)?;
            let state = match state_name {
                "supported" => CapabilityState::Supported,
                "limited" => CapabilityState::Limited,
                "unsupported" => CapabilityState::Unsupported,
                "unavailable" => CapabilityState::Unavailable,
                _ => {
                    return Err(StagedError::new(
                        "argument.invalid",
                        "core",
                        op,
                        "staged-capability-state",
                    ));
                }
            };
            let limitations: &[&str] = match state {
                CapabilityState::Supported => &[],
                _ => &["synthetic-radio-only"],
            };
            let descriptor = CapabilityDescriptor::new(
                id,
                state,
                &[("staged-batch", STAGED_BATCH_MAX as u64)],
                limitations,
                "staged-synthetic-001",
                EvidenceLevel::Deterministic,
                "0.1.0-staged",
                "synthetic-no-radio",
                &["scenario.scan-connect-discover-read-notify-destroy"],
            )
            .map_err(|core| central_error(&core, op))?;
            self.central
                .register_capability(descriptor)
                .map_err(|core| central_error(&core, op))?;
            ob.str_field("id", id);
            ob.str_field("state", state_name);
        } else {
            return Err(StagedError::new(
                "argument.invalid",
                "core",
                op,
                "staged-cap-step",
            ));
        }
        self.note_staged_empty(&mut ob);
        Ok(ob.finish())
    }

    fn misc_step(&mut self, obj: &JsonValue, step: &'static str, op: &'static str) -> String {
        let line = match self.misc_inner(obj, step, op) {
            Ok(line) => line,
            Err(error) => err_ob(step, &error),
        };
        self.emit(step, op, line)
    }

    fn misc_inner(
        &mut self,
        obj: &JsonValue,
        step: &str,
        op: &'static str,
    ) -> Result<String, StagedError> {
        let mut ob = ok_ob(step);
        if step == "batch.set-cap" {
            let cap = u64_param(obj, "cap", None, op)?;
            if cap == 0 || cap > STAGED_BATCH_MAX as u64 {
                return Err(StagedError::new(
                    "argument.invalid",
                    "core",
                    op,
                    "staged-batch-cap-range",
                ));
            }
            self.staged_cap = cap as usize;
            ob.num_field("cap", cap);
        } else if step == "clock.set" {
            let now = u64_param(obj, "now", None, op)?;
            if now < self.now_ms {
                return Err(StagedError::new(
                    "argument.invalid",
                    "core",
                    op,
                    "clock-regressed",
                ));
            }
            self.now_ms = now;
            ob.num_field("now_ms", now);
        } else if step == "staged.status" {
            ob.str_field("revision", crate::driver::CONTRACT_REVISION);
            ob.num_field("cap", self.staged_cap as u64);
            ob.num_field("staged_total", self.staged_total);
            ob.num_field("dropped_not_staged", self.dropped_not_staged);
            ob.num_field("truncated_sweeps", self.truncated_sweeps);
            ob.num_field("now_ms", self.now_ms);
        } else if step == "staged.destroy" {
            let mut out = self.batch();
            let record = self.central.destroy(&mut out).map_err(|core| {
                self.note_batch_full(&core);
                central_error(&core, op)
            })?;
            ob.str_field(
                "state",
                match record.state() {
                    ubm_core::ownership::CleanupState::Released => "released",
                    ubm_core::ownership::CleanupState::ReleaseFailed => "release-failed",
                },
            );
            self.note_staged(&mut ob, &mut out);
            return Ok(ob.finish());
        } else if step == "staged.counters" {
            ob.num_field("staged_total", self.staged_total);
            ob.num_field("dropped_not_staged", self.dropped_not_staged);
            ob.num_field("truncated_sweeps", self.truncated_sweeps);
            ob.num_field("cap", self.staged_cap as u64);
        } else if step == "read.taken" {
            let name = str_param(obj, "op", op)?;
            match self.read_values.iter().find(|(known, _)| known == name) {
                Some((_, value)) => ob.str_field("bytes", &hex_of(value)),
                None => ob.null_field("bytes"),
            }
        } else {
            return Err(StagedError::new(
                "argument.invalid",
                "core",
                op,
                "staged-misc-step",
            ));
        }
        self.note_staged_empty(&mut ob);
        Ok(ob.finish())
    }

    /// Run one scripted synthetic-radio step line (a JSON object with a
    /// `step` name) against the REAL central and return one JSON
    /// observation object. Unknown steps, malformed lines, and unusable
    /// fields fail loudly as staged observations; nothing is skipped
    /// silently.
    pub fn run_step(&mut self, line: &str) -> String {
        let parsed = match crate::json::parse(line) {
            Ok(value) => value,
            Err(error) => {
                let staged = StagedError::new(
                    "argument.invalid",
                    "core",
                    "staged-step",
                    "staged-line-not-json",
                );
                let mut ob = Ob::new();
                ob.str_field("step", "staged-step");
                ob.bool_field("ok", false);
                ob.str_field("error", &staged.wire_message());
                ob.num_field("at", error.position() as u64);
                let line = ob.finish();
                self.log.push(line.clone());
                return line;
            }
        };
        let obj = match &parsed {
            JsonValue::Object(_) => &parsed,
            _ => {
                return self.misc_step(
                    &JsonValue::Object(Vec::new()),
                    "staged-step",
                    "staged-step",
                );
            }
        };
        let name = obj.field("step").and_then(JsonValue::as_str).unwrap_or("");
        match name {
            "scan.start" => self.scan_start_step(obj),
            "scan.platform" => self.scan_platform_step(obj),
            "peer.advertise" => self.advertise_step(obj),
            "link.connect" => self.connect_step(obj),
            "link.established" => {
                self.link_event_step(obj, "link.established", "staged-link-established")
            }
            "link.loss" => self.link_event_step(obj, "link.loss", "staged-link-loss"),
            "link.released" => self.link_event_step(obj, "link.released", "staged-link-released"),
            "link.disconnect" => {
                self.link_event_step(obj, "link.disconnect", "staged-link-disconnect")
            }
            "link.disconnect-failed" => self.link_event_step(
                obj,
                "link.disconnect-failed",
                "staged-link-disconnect-failed",
            ),
            "link.borrow" => self.link_lease_step(obj, "link.borrow", "staged-link-borrow"),
            "link.transfer" => self.link_lease_step(obj, "link.transfer", "staged-link-transfer"),
            "link.release" => self.link_lease_step(obj, "link.release", "staged-link-release"),
            "link.sharing" => self.link_lease_step(obj, "link.sharing", "staged-link-sharing"),
            "gatt.discover" => self.discover_step(obj),
            "gatt.discovery-failed" => self.database_event_step(
                obj,
                "gatt.discovery-failed",
                "staged-gatt-discovery-failed",
            ),
            "gatt.services-changed" => self.database_event_step(
                obj,
                "gatt.services-changed",
                "staged-gatt-services-changed",
            ),
            "gatt.require-rediscovery" => self.database_event_step(
                obj,
                "gatt.require-rediscovery",
                "staged-gatt-require-rediscovery",
            ),
            "gatt.resolve" => self.resolve_step(obj),
            "gatt.read" => self.io_step(obj, "gatt.read", "staged-gatt-read"),
            "gatt.write" => self.io_step(obj, "gatt.write", "staged-gatt-write"),
            "gatt.read-descriptor" => {
                self.io_step(obj, "gatt.read-descriptor", "staged-gatt-read-descriptor")
            }
            "gatt.write-descriptor" => {
                self.io_step(obj, "gatt.write-descriptor", "staged-gatt-write-descriptor")
            }
            "op.dispatch" => self.op_step(obj, "op.dispatch", "staged-op-dispatch"),
            "op.settle" => self.op_step(obj, "op.settle", "staged-op-settle"),
            "op.cancel" => self.op_step(obj, "op.cancel", "staged-op-cancel"),
            "op.expire-sweep" => self.op_step(obj, "op.expire-sweep", "staged-op-expire-sweep"),
            "sub.subscribe" => self.subscribe_step(obj),
            "sub.settle-enable" => {
                self.sub_event_step(obj, "sub.settle-enable", "staged-sub-settle-enable")
            }
            "sub.unsubscribe" => {
                self.sub_event_step(obj, "sub.unsubscribe", "staged-sub-unsubscribe")
            }
            "sub.settle-disable" => {
                self.sub_event_step(obj, "sub.settle-disable", "staged-sub-settle-disable")
            }
            "sub.notify" => self.sub_event_step(obj, "sub.notify", "staged-sub-notify"),
            "sub.take" => self.sub_event_step(obj, "sub.take", "staged-sub-take"),
            "sub.take-terminal" => {
                self.sub_event_step(obj, "sub.take-terminal", "staged-sub-take-terminal")
            }
            "sub.quarantined" => {
                self.sub_event_step(obj, "sub.quarantined", "staged-sub-quarantined")
            }
            "cap.project" => self.cap_step(obj, "cap.project", "staged-cap-project"),
            "cap.check" => self.cap_step(obj, "cap.check", "staged-cap-check"),
            "cap.rows" => self.cap_step(obj, "cap.rows", "staged-cap-rows"),
            "cap.set" => self.cap_step(obj, "cap.set", "staged-cap-set"),
            "batch.set-cap" => self.misc_step(obj, "batch.set-cap", "staged-batch-set-cap"),
            "clock.set" => self.misc_step(obj, "clock.set", "staged-clock-set"),
            "staged.status" => self.misc_step(obj, "staged.status", "staged-status"),
            "staged.counters" => self.misc_step(obj, "staged.counters", "staged-counters"),
            "staged.destroy" => self.misc_step(obj, "staged.destroy", "staged-destroy"),
            "read.taken" => self.misc_step(obj, "read.taken", "staged-read-taken"),
            _ => {
                let staged = StagedError::new(
                    "argument.invalid",
                    "core",
                    "staged-step",
                    "staged-unknown-step",
                );
                let line = err_ob("staged-step", &staged);
                self.emit("staged-step", "staged-step", line)
            }
        }
    }
}
