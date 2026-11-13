//! Process-owned native continuation. Native hosts share this owner; no
//! JavaScript session is necessary to connect, subscribe, or retain values.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::continuation_journal::{ContinuationJournal, JournalQuota, JournalRegistry};
use crate::continuation_outbox::{
    CONTROL_RECORD_CAP, DATA_RECORD_CAP, DrainClaim, DrainClaimError, LegacyEvidenceBudget,
    LegacyEvidenceError, LegacyEvidenceOwner, Observation, RESERVED_CONTROL_CAP, RecordMatcher,
    decode_base64, encode_base64,
};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::Mutex;

pub type Result<T> = std::result::Result<T, Value>;
pub type ContinuationFuture<'a> = Pin<Box<dyn Future<Output = String> + Send + 'a>>;

/// Retained-prefix handoff with exact acknowledgement and receipt retirement.
/// Strict projected records plus separately page-able original evidence.
pub const CONTINUATION_HANDOFF_V1: u32 = 1;
/// Full current continuation vocabulary, including source-failure descriptors.
pub const CONTINUATION_HANDOFF_V2: u32 = 2;
/// Latest explicitly negotiated handoff protocol.  Callers must still name a
/// version; this constant is not a silent fallback for an older decoder.
pub const CONTINUATION_HANDOFF_VERSION: u32 = CONTINUATION_HANDOFF_V2;

pub trait ContinuationSession: Send + Sync {
    /// A legacy drain is allowed only when the source guarantees that every
    /// current and future record is representable by the strict 5.2.1 codec.
    /// The default is conservative because refusing later would strand a
    /// destructively drained record or erase provenance.
    fn legacy_claim_compatible(&self) -> bool {
        false
    }
    fn seal_collection(&self) -> Result<()> {
        Err(failure(
            "capability.unsupported",
            "session cannot seal native collection",
        ))
    }
    fn collection_sealed(&self) -> bool {
        false
    }
    fn attach_journal(&self, _: Arc<ContinuationJournal>, _: Value) -> Result<()> {
        Err(failure(
            "capability.unsupported",
            "session cannot collect durably",
        ))
    }
    fn register_journal_consumer(&self, _: &str, _: Value) -> Result<()> {
        Err(failure(
            "capability.unsupported",
            "session cannot register durable consumer metadata",
        ))
    }
    fn call<'a>(&'a self, op: &'a str, args: &'a str) -> ContinuationFuture<'a>;
    /// Preserve 5.2.1 data-byte accounting and first-record behavior. A host
    /// returns None without side effects when it does not implement this seam;
    /// an error must likewise leave the source unchanged. Successful drains
    /// are synchronously staged by the continuation owner before it yields.
    /// Atomically project a strict 5.2.1 batch while transferring every
    /// original to `evidence`. Implementations return `None` only when they
    /// do not own an outbox and have made no source-side change.
    fn legacy_drain(&self, _: &LegacyEvidenceOwner, _: u32, _: u32) -> Option<Result<Value>> {
        None
    }
    /// Claim a strict V1 view while retaining the exact raw prefix through
    /// acknowledgement.  Implementations return `None` only if they do not
    /// own an atomic projected-claim primitive.
    fn claim_drain_v1(
        &self,
        _: &LegacyEvidenceOwner,
        _: u32,
        _: u32,
    ) -> Option<Result<Option<DrainClaim>>> {
        None
    }
    fn claim_drain(&self, max_items: u32, max_bytes: u32) -> Result<Option<DrainClaim>>;
    fn has_retained_tail(&self) -> Result<bool> {
        Err(failure(
            "capability.unsupported",
            "session cannot inspect retained continuation tail",
        ))
    }
    fn acknowledge_drain(&self, id: u64) -> Result<()>;
    fn observe(&self, _: &str, _: RecordMatcher) -> Result<Observation> {
        Err(failure(
            "capability.unsupported",
            "session cannot observe setup acknowledgements",
        ))
    }
}

pub trait ContinuationHost: Send + Sync {
    fn open_session(&self) -> Result<Arc<dyn ContinuationSession>>;
    /// Host identities are opaque by default. Only the owning platform may
    /// canonicalize a representation it explicitly defines as equivalent.
    fn canonical_peer(&self, peer: &str) -> String {
        peer.to_owned()
    }
}

fn failure(code: &str, detail: &str) -> Value {
    json!({"code":code,"domain":"restoration","operation":"continuation",
        "detail":detail})
}

/// Safe public error identity for durable storage. Never includes a path,
/// SQL text, declaration contents, or sensor bytes.
pub fn recording_failure(error: crate::continuation_journal::JournalError) -> Value {
    let mut metadata = json!({"storageKind":error.kind,"operation":error.operation});
    if let Some(code) = error.sqlite_extended_code {
        metadata["sqliteExtendedCode"] = json!(code);
    }
    if let Some(code) = error.sqlite_code {
        metadata["sqliteCode"] = json!(code);
    }
    json!({"code":if error.kind=="argument.invalid" {"argument.invalid"} else {"platform.failure"},"domain":"platform","operation":"continuation.recording","detail":error.detail,"platform":{"domain":"sqlite","code":error.kind,"message":error.detail,"metadata":metadata}})
}

fn invalid(detail: &str) -> Value {
    failure("argument.invalid", detail)
}
fn busy() -> Value {
    failure(
        "lifecycle.invalid-state",
        "continuation execution or handoff is in progress",
    )
}

const MAX_SETUP_FAILURE_PEERS: usize = 4096;
const MAX_CLAIM_RECEIPTS: usize = 4096;
const MAX_CLAIM_WIRE_BYTES: usize = 1_048_576;
/// Complete originals retained while a 5.2.1 client receives a strict
/// projection. Exhaustion is a safe refusal: the source outbox is untouched.
const LEGACY_EVIDENCE_MAX_ITEMS: usize =
    DATA_RECORD_CAP + CONTROL_RECORD_CAP + RESERVED_CONTROL_CAP;
const LEGACY_EVIDENCE_MAX_BYTES: usize = 64 << 20;

#[derive(Default)]
struct SetupFailures {
    peers: std::collections::HashMap<String, SetupFailure>,
}

struct SetupFailure {
    generation: Value,
    error: Value,
}

impl SetupFailures {
    fn check(&mut self, peer: &str, generation: &Value) -> Result<()> {
        if let Some(previous) = self.peers.get(peer) {
            if &previous.generation == generation {
                return Err(previous.error.clone());
            }
            // Only this peer's authoritative generation change retires its
            // uncertainty. A claim or another peer's execution cannot do so.
            self.peers.remove(peer);
        }
        if self.peers.len() >= MAX_SETUP_FAILURE_PEERS {
            let mut error = failure(
                "lifecycle.invalid-state",
                "setup failure history is full; an authoritative generation change is required",
            );
            error["retryability"] = json!("never");
            return Err(error);
        }
        Ok(())
    }

    fn record(&mut self, peer: &str, generation: Value, error: Value) {
        // check and record share the executor's state lock. Reserve capacity
        // before any write, never evict an unresolved physical generation.
        self.peers
            .insert(peer.to_owned(), SetupFailure { generation, error });
    }
}

#[derive(Default)]
pub(crate) struct State {
    session: Option<Arc<dyn ContinuationSession>>,
    peer: Option<String>,
    selectors: Vec<Value>,
    history: Vec<Value>,
    consumers: Vec<ActiveConsumer>,
    admission: u64,
    token: u64,
    generation: u64,
    executor_identity: Option<String>,
    receipt_token_key: Option<[u8; 32]>,
    prepared: Option<Prepared>,
    receipts: std::collections::HashMap<String, (ClaimMode, Value)>,
    claim_mode: Option<ClaimMode>,
    legacy_stage: Option<LegacyStage>,
    legacy_evidence: std::collections::HashMap<String, Arc<LegacyEvidenceOwner>>,
    /// Aggregate reservation across every exact strict-V1 evidence owner.
    /// Per-token bounds alone would allow an unbounded sequence of claims to
    /// retain process memory after their source prefixes were acknowledged.
    legacy_evidence_budget: Option<Arc<LegacyEvidenceBudget>>,
    cleanup_receipt: Option<Value>,
    sealed: bool,
    execution_declaration: Option<Value>,
    setup_generation: Option<Value>,
    setup_complete: bool,
    setup_failures: SetupFailures,
    link_generation: Option<String>,
    link_outcome: Option<Value>,
    link_failure: Option<Value>,
    recording: Option<String>,
    recording_attached: bool,
}

impl State {
    fn reclaim_empty_legacy_evidence(&mut self, token: &str) {
        if self
            .legacy_evidence
            .get(token)
            .is_some_and(|evidence| evidence.is_empty())
        {
            self.legacy_evidence.remove(token);
        }
    }

    fn archive_prepared_receipt(&mut self) {
        if let Some(prepared) = self.prepared.take() {
            let token = prepared.claim["claimToken"].as_str().expect("own token");
            if prepared.mode == ClaimMode::Legacy {
                let superseded = self
                    .receipts
                    .iter()
                    .filter(|(_, (mode, _))| *mode == ClaimMode::Legacy)
                    .map(|(token, _)| token.clone())
                    .collect::<Vec<_>>();
                self.receipts
                    .retain(|_, (mode, _)| *mode != ClaimMode::Legacy);
                for token in superseded {
                    self.reclaim_empty_legacy_evidence(&token);
                }
            }
            self.receipts.insert(
                token.to_owned(),
                (prepared.mode, prepared.receipt.expect("completed receipt")),
            );
        }
    }

    /// A retired retained-claim token is self-authenticating.  It lets the
    /// owner reclaim an empty evidence map entry without keeping an unbounded
    /// tombstone map solely to answer a delayed page reply.  This authorizes
    /// only an empty result; it never reconstructs or exposes evidence.
    fn authenticated_claim_token(&self, token: &str, required_mode: Option<ClaimMode>) -> bool {
        let (Some(identity), Some(key)) = (&self.executor_identity, &self.receipt_token_key) else {
            return false;
        };
        let prefix = format!("continuation-{identity}-");
        let Some(rest) = token.strip_prefix(&prefix) else {
            return false;
        };
        let mut parts = rest.split('-');
        let (Some(generation), Some(sequence), Some(mode), Some(tag), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return false;
        };
        let Some(token_mode) = ClaimMode::from_token_label(mode) else {
            return false;
        };
        if required_mode.is_some_and(|expected| expected != token_mode) {
            return false;
        }
        let (Ok(generation), Ok(sequence)) = (generation.parse::<u64>(), sequence.parse::<u64>())
        else {
            return false;
        };
        // `token` advances only after the source claim has completed and the
        // token is about to be published (below in `prepare_claim_mode`).
        // Thus a valid MAC plus this bounded issued sequence cannot name a
        // made-up V1 token; an active V1 still has an evidence-map entry and
        // never reaches this fallback.
        if generation == 0
            || generation > self.generation
            || sequence == 0
            || sequence > self.token
            || self
                .receipts
                .get(token)
                .is_some_and(|(receipt_mode, _)| *receipt_mode != token_mode)
            || self
                .prepared
                .as_ref()
                .is_some_and(|prepared| prepared.claim["claimToken"] == token)
        {
            return false;
        }
        let body = format!("{prefix}{generation}-{sequence}-{mode}");
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key");
        mac.update(body.as_bytes());
        let expected: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        tag == expected && token == format!("{body}-{expected}")
    }

    fn retired_v1_claim_token(&self, token: &str) -> bool {
        self.authenticated_claim_token(token, Some(ClaimMode::RetainedV1))
    }

    fn retired_retained_claim_token(&self, token: &str) -> bool {
        self.authenticated_claim_token(token, Some(ClaimMode::RetainedV1))
            || self.authenticated_claim_token(token, Some(ClaimMode::RetainedV2))
    }
}

#[derive(Default)]
struct DeclarationAuthority {
    committed: Option<Value>,
    reservation: Option<DeclarationReservation>,
    sequence: u64,
}

struct DeclarationReservation {
    token: String,
    declaration: Value,
}

struct ActiveConsumer {
    selector_index: usize,
    consumer: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClaimMode {
    Legacy,
    RetainedV1,
    RetainedV2,
}

impl ClaimMode {
    fn token_label(self) -> Option<&'static str> {
        match self {
            Self::Legacy => None,
            Self::RetainedV1 => Some("v1"),
            Self::RetainedV2 => Some("v2"),
        }
    }

    fn from_token_label(label: &str) -> Option<Self> {
        match label {
            "v1" => Some(Self::RetainedV1),
            "v2" => Some(Self::RetainedV2),
            _ => None,
        }
    }
}

struct LegacyStage {
    batches: Vec<String>,
    pending: Option<(DrainClaim, bool)>,
    evidence: Arc<LegacyEvidenceOwner>,
    loss: Value,
    max_items: u32,
    max_bytes: u32,
}

#[doc(hidden)]
#[expect(
    clippy::result_unit_err,
    reason = "The legacy projection's unit error is part of its established strict codec contract."
)]
pub fn project_521_record(record: &Value) -> std::result::Result<Option<Value>, ()> {
    let ordinal = record.get("ordinal").and_then(Value::as_u64).ok_or(())?;
    let select = |keys: &[&str]| -> Option<Value> {
        let source = record.as_object()?;
        let mut projected = serde_json::Map::new();
        for key in keys {
            projected.insert((*key).to_owned(), source.get(*key)?.clone());
        }
        Some(Value::Object(projected))
    };
    let projected = match record.get("t").and_then(Value::as_str) {
        Some("value") => select(&["t", "ordinal", "consumer", "valueB64", "delivery"]),
        Some("stream-end")
            if matches!(
                record.get("reason").and_then(Value::as_str),
                Some("overflow" | "invalidated" | "closed")
            ) && record.get("error").is_none() =>
        {
            select(&[
                "t",
                "ordinal",
                "consumer",
                "reason",
                "droppedItems",
                "droppedBytes",
            ])
        }
        Some("recording-changed") => select(&["t", "ordinal", "consumer", "recordingId"]),
        Some("adapter") => select(&["t", "ordinal", "state"]),
        Some("link")
            if matches!(
                record.get("reason").and_then(Value::as_str),
                Some("local" | "peer" | "adapter")
            ) && record.get("platform").is_none() =>
        {
            select(&[
                "t",
                "ordinal",
                "peerId",
                "connectionGeneration",
                "databaseGeneration",
                "reason",
            ])
        }
        Some("db-changed") => select(&[
            "t",
            "ordinal",
            "peerId",
            "connectionGeneration",
            "databaseGeneration",
        ]),
        Some("ingress-drop") => select(&["t", "ordinal", "class", "count"]),
        Some("security") => select(&["t", "ordinal", "peerId", "state"]),
        Some("security-failed") => select(&["t", "ordinal", "peerId", "error"]),
        Some("readiness") => select(&["t", "ordinal", "peerId", "connectionGeneration", "ready"]),
        Some("restored") => select(&["t", "ordinal", "peers"]),
        Some("scan-end") => select(&["t", "ordinal", "operationId", "reason"]),
        Some("adv") => select(&[
            "t",
            "ordinal",
            "operationId",
            "startOperationId",
            "peerId",
            "localName",
            "rssi",
            "txPower",
            "serviceUuids",
            "manufacturerData",
            "serviceData",
            "connectable",
            "solicitedServiceUuids",
            "overflowServiceUuids",
            "appearance",
            "rawRecordB64",
            "observedAtMs",
            "sourceTimestampMs",
        ]),
        // New vocabulary (including peer-source-loss) must never be forged
        // into a different old fact. It remains available via evidence paging.
        _ => None,
    };
    Ok(projected.filter(|value| value.get("ordinal").and_then(Value::as_u64) == Some(ordinal)))
}

#[doc(hidden)]
pub fn legacy_evidence_error(error: LegacyEvidenceError) -> Value {
    match error {
        LegacyEvidenceError::RecordTooLarge { required_bytes } => {
            let mut value = failure(
                "argument.invalid",
                "legacy evidence record exceeds page byte bound",
            );
            value["requiredBytes"] = json!(required_bytes);
            value
        }
        LegacyEvidenceError::Capacity => failure(
            "lifecycle.invalid-state",
            "legacy evidence capacity exhausted; source remains retained",
        ),
        LegacyEvidenceError::ModeConflict => failure(
            "lifecycle.invalid-state",
            "legacy evidence source has another drain owner",
        ),
        LegacyEvidenceError::OwnerMismatch => failure(
            "lifecycle.invalid-state",
            "legacy evidence source ownership mismatch",
        ),
        LegacyEvidenceError::InvalidProjection => failure(
            "platform.failure",
            "legacy evidence projection rejected an invalid record",
        ),
        LegacyEvidenceError::OrdinalExhausted => failure(
            "lifecycle.invalid-state",
            "legacy evidence ordinals exhausted",
        ),
    }
}

struct Prepared {
    mode: ClaimMode,
    claim: Value,
    complete: bool,
    acknowledged: bool,
    receipt: Option<Value>,
    source_claim: Option<u64>,
}

pub fn continuation_drain_error(error: DrainClaimError) -> Value {
    match error {
        DrainClaimError::RecordTooLarge { required_bytes } => {
            let mut error = failure(
                "argument.invalid",
                "continuation record exceeds claim byte bound",
            );
            error["requiredBytes"] = json!(required_bytes);
            error
        }
        DrainClaimError::IdentityExhausted => failure(
            "lifecycle.invalid-state",
            "continuation claim identities exhausted",
        ),
        DrainClaimError::ModeConflict => failure(
            "lifecycle.invalid-state",
            "continuation outbox already has a different drain owner",
        ),
    }
}

/// One bounded native standing order per process. Public operations fail promptly
/// when another operation owns admission; the internal supervisor waits fairly.
#[derive(Clone)]
pub struct NativeContinuation {
    host: Arc<dyn ContinuationHost>,
    state: Arc<Mutex<State>>,
    // Published only while holding state admission, alongside session install
    // or confirmed disposal. Idle OS events must not contend for that lock.
    session_owned: Arc<AtomicBool>,
    recovery_requested: Arc<AtomicBool>,
    recovering: Arc<AtomicBool>,
    recovery_stopped: Arc<AtomicBool>,
    last_recovery: Arc<std::sync::Mutex<Option<Value>>>,
    recovery_wake: Arc<tokio::sync::Notify>,
    recovery_idle: Arc<tokio::sync::Notify>,
    declarations: Arc<std::sync::Mutex<DeclarationAuthority>>,
    recovery_runtime: Arc<std::sync::OnceLock<tokio::runtime::Handle>>,
    recordings: Arc<JournalRegistry>,
}

impl NativeContinuation {
    async fn check_recording_admission(&self, state: &State) -> Result<()> {
        // An attempted journal open can fail before attachment. Its retry must
        // reach open again; only an attached collection has a status to fence.
        if !state.recording_attached {
            return Ok(());
        }
        let Some(id) = state.recording.clone() else {
            return Ok(());
        };
        let executor = self.clone();
        let session = state.session.clone();
        crate::continuation_journal::run_blocking_result(move || {
            let status = executor.recording_status(&id)?;
            if status["accepting"] != true {
                if let Some(session) = &session {
                    session.seal_collection()?;
                }
                executor.stop_recovery();
                return Err(failure(
                    "lifecycle.invalid-state",
                    "native recording collection is stopped",
                ));
            }
            Ok(())
        })
        .await
    }
    pub fn configure_recording_directory(&self, path: &std::path::Path) -> Result<Value> {
        self.recordings
            .configure_directory(path)
            .map_err(recording_failure)
    }
    pub fn recording_status(&self, id: &str) -> Result<Value> {
        self.recordings
            .get(id)
            .and_then(|journal| journal.status())
            .map_err(recording_failure)
    }
    /// Read exact bytes of the retained original collection summary. Callers
    /// must coordinate summary updates for the duration of a multi-page read.
    pub fn recording_diagnostic_summary_page(
        &self,
        id: &str,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Value> {
        let page = self
            .recordings
            .get(id)
            .and_then(|journal| journal.diagnostic_summary_page(offset, max_bytes))
            .map_err(recording_failure)?;
        Ok(json!({"bytes":page.bytes,"totalBytes":page.total_bytes}))
    }
    /// Discover bounded occurrence identities. Separate calls do not pin a
    /// snapshot; coordinate writers and retirement for a complete traversal.
    pub fn recording_diagnostic_occurrences_page(
        &self,
        id: &str,
        after: Option<[u8; 40]>,
        limit: usize,
    ) -> Result<Vec<[u8; 40]>> {
        self.recordings
            .get(id)
            .and_then(|journal| journal.diagnostic_occurrences_page(after, limit))
            .map_err(recording_failure)
    }

    /// Read exact occurrence bytes without acknowledging the evidence.
    pub fn recording_diagnostic_occurrence_page(
        &self,
        id: &str,
        occurrence: [u8; 40],
        offset: u64,
        max_bytes: u32,
    ) -> Result<crate::continuation_journal::DiagnosticPage> {
        self.recordings
            .get(id)
            .and_then(|journal| journal.diagnostic_occurrence_page(occurrence, offset, max_bytes))
            .map_err(recording_failure)
    }

    /// Retry a retained process-owned loss occurrence using its original identity.
    pub fn recording_retry_diagnostic_occurrence(
        &self,
        id: &str,
        occurrence: [u8; 40],
    ) -> Result<()> {
        self.recordings
            .get(id)
            .and_then(|journal| journal.retry_production_loss(occurrence))
            .map_err(recording_failure)
    }

    pub fn recording_prepare(&self, id: &str, max_items: u32, max_bytes: u32) -> Result<Value> {
        self.recordings
            .get(id)
            .and_then(|journal| journal.prepare(max_items, max_bytes))
            .map_err(recording_failure)
    }
    pub fn recording_acknowledge(&self, id: &str, token: &str) -> Result<Value> {
        self.recordings
            .get(id)
            .and_then(|journal| journal.acknowledge(token))
            .map_err(recording_failure)
    }
    pub fn recording_stop(&self, id: &str) -> Result<Value> {
        let journal = self.recordings.get(id).map_err(recording_failure)?;
        crate::continuation_journal::stop_legacy_recording(&journal)
    }
    pub fn recording_clear(&self, id: &str) -> Result<Value> {
        self.recordings
            .get(id)
            .and_then(|journal| journal.clear())
            .map_err(recording_failure)
    }
    pub fn new(host: Arc<dyn ContinuationHost>) -> Self {
        Self::new_with_recording_registry(host, Arc::default())
    }
    pub fn recording_registry(&self) -> Arc<JournalRegistry> {
        self.recordings.clone()
    }
    /// Inject a process-owned registry before publishing this executor. There
    /// is deliberately no setter that could split already cloned authority.
    pub fn new_with_recording_registry(
        host: Arc<dyn ContinuationHost>,
        recordings: Arc<JournalRegistry>,
    ) -> Self {
        Self {
            host,
            state: Arc::default(),
            session_owned: Arc::default(),
            recovery_requested: Arc::default(),
            recovering: Arc::default(),
            recovery_stopped: Arc::default(),
            last_recovery: Arc::default(),
            recovery_wake: Arc::default(),
            recovery_idle: Arc::default(),
            declarations: Arc::default(),
            recovery_runtime: Arc::default(),
            recordings,
        }
    }

    /// Validate and pin a declaration before native execution has any effect.
    fn parse(&self, peer: &str, text: &str) -> Result<Vec<Value>> {
        if peer.is_empty() || peer.len() > 512 || text.len() > 1 << 20 {
            return Err(invalid("invalid peer or oversized declaration"));
        }
        let root: Value =
            serde_json::from_str(text).map_err(|_| invalid("malformed declaration"))?;
        let object = root
            .as_object()
            .ok_or_else(|| invalid("declaration must be an object"))?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "onAppearance" | "peerId" | "resubscribe" | "setup" | "link" | "recording"
            )
        }) || root["onAppearance"] != "native"
        {
            return Err(invalid(
                "native declaration contains invalid fields or strategy",
            ));
        }
        if object.contains_key("peerId")
            && !root["peerId"].as_str().is_some_and(|declared| {
                self.host.canonical_peer(declared) == self.host.canonical_peer(peer)
            })
        {
            return Err(invalid("wake peer does not match declaration"));
        }
        let selectors = match object.get("resubscribe") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .ok_or_else(|| invalid("resubscribe must be an array"))?
                .clone(),
        };
        if selectors.len() > 64 {
            return Err(invalid("too many selectors"));
        }
        selectors
            .into_iter()
            .map(|mut selector| {
                let entry = selector
                    .as_object_mut()
                    .ok_or_else(|| invalid("selector must be an object"))?;
                if entry.keys().any(|key| {
                    !matches!(
                        key.as_str(),
                        "serviceUuid"
                            | "serviceOccurrence"
                            | "characteristicUuid"
                            | "characteristicOccurrence"
                    )
                }) {
                    return Err(invalid("unknown selector field"));
                }
                for field in ["serviceUuid", "characteristicUuid"] {
                    let uuid = entry
                        .get(field)
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("missing UUID"))?;
                    if uuid.len() != 36
                        || !uuid.bytes().enumerate().all(|(index, byte)| {
                            if [8, 13, 18, 23].contains(&index) {
                                byte == b'-'
                            } else {
                                byte.is_ascii_hexdigit()
                            }
                        })
                    {
                        return Err(invalid("UUID must be canonical"));
                    }
                    entry.insert(field.to_owned(), json!(uuid.to_ascii_lowercase()));
                }
                for field in ["serviceOccurrence", "characteristicOccurrence"] {
                    let occurrence = match entry.get(field) {
                        None => 1,
                        Some(value) => value
                            .as_u64()
                            .ok_or_else(|| invalid("invalid occurrence"))?,
                    };
                    if occurrence == 0 || occurrence > (1 << 53) - 1 {
                        return Err(invalid("invalid occurrence"));
                    }
                    entry.insert(field.to_owned(), json!(occurrence));
                }
                Ok(selector)
            })
            .collect()
    }

    fn setup(&self, root: &Value, subscription_count: usize) -> Result<Vec<Value>> {
        let Some(input) = root.get("setup") else {
            return Ok(Vec::new());
        };
        let steps = input
            .as_array()
            .ok_or_else(|| invalid("setup must be an array"))?;
        if steps.len() > 16 {
            return Err(invalid("too many setup steps"));
        }
        let mut total = 0;
        let mut normalized = Vec::new();
        for step in steps {
            exact(step, &["selector", "value", "timeoutMs", "response"])?;
            let timeout = integer(&step["timeoutMs"], 1, 20000)?;
            total += timeout;
            if total > 60000 {
                return Err(invalid("setup deadline exceeds 60000ms"));
            }
            bytes(&step["value"])?;
            let selector = self
                .parse(
                    "setup",
                    &json!({"onAppearance":"native","resubscribe":[step["selector"].clone()]})
                        .to_string(),
                )?
                .remove(0);
            if let Some(response) = step.get("response") {
                exact(
                    response,
                    &[
                        "subscriptionIndex",
                        "prefix",
                        "minLength",
                        "maxLength",
                        "status",
                        "trailing",
                    ],
                )?;
                if subscription_count == 0 {
                    return Err(invalid("setup response has no subscription"));
                }
                integer(
                    &response["subscriptionIndex"],
                    0,
                    subscription_count as u64 - 1,
                )?;
                let prefix = bytes(&response["prefix"])?;
                let min = integer(&response["minLength"], prefix.len() as u64, 512)?;
                integer(&response["maxLength"], min, 512)?;
                exact(&response["status"], &["offset", "accepted"])?;
                integer(
                    &response["status"]["offset"],
                    prefix.len() as u64,
                    min.saturating_sub(1),
                )?;
                let accepted = response["status"]["accepted"]
                    .as_array()
                    .ok_or_else(|| invalid("missing accepted setup statuses"))?;
                if accepted.is_empty() || accepted.len() > 256 {
                    return Err(invalid("invalid accepted setup statuses"));
                }
                let mut seen = std::collections::HashSet::new();
                for status in accepted {
                    if !seen.insert(integer(status, 0, 255)?) {
                        return Err(invalid("duplicate setup status"));
                    }
                }
                if let Some(trailing) = response.get("trailing") {
                    exact(trailing, &["offset", "accepted"])?;
                    integer(&trailing["offset"], min, min)?;
                    integer(&response["maxLength"], min + 1, min + 1)?;
                    let accepted = trailing["accepted"]
                        .as_array()
                        .ok_or_else(|| invalid("missing trailing accepted bytes"))?;
                    if accepted.is_empty() || accepted.len() > 256 {
                        return Err(invalid("invalid trailing accepted bytes"));
                    }
                    let mut seen = std::collections::HashSet::new();
                    for byte in accepted {
                        if !seen.insert(integer(byte, 0, 255)?) {
                            return Err(invalid("duplicate trailing byte"));
                        }
                    }
                }
            }
            let mut step = step.clone();
            step["selector"] = selector;
            normalized.push(step);
        }
        Ok(normalized)
    }

    /// Refuse replacement while any old generation still owns records/resources.
    pub async fn validate_replacement(&self, peer: &str, declaration: &str) -> Result<Value> {
        let peer = self.host.canonical_peer(peer);
        let selectors = self.parse(&peer, declaration)?;
        let identity = self.declaration_identity(declaration)?;
        let state = self.state.try_lock().map_err(|_| busy())?;
        if state.session.is_some()
            && (state.peer.as_deref() != Some(peer.as_str())
                || state.selectors != selectors
                || state.execution_declaration.as_ref() != Some(&identity))
        {
            return Err(failure(
                "lifecycle.invalid-state",
                "claim the pinned continuation before replacing its declaration",
            ));
        }
        Ok(json!({"state":"accepted"}))
    }

    pub async fn execute(&self, peer: &str, declaration: &str) -> Result<Value> {
        let result = self.execute_once(peer, declaration).await;
        if result
            .as_ref()
            .is_err_and(|error| error["retryability"] == "caller-decides")
        {
            self.request_recovery(&tokio::runtime::Handle::current());
        }
        result
    }

    async fn execute_once(&self, peer: &str, declaration: &str) -> Result<Value> {
        let peer = self.host.canonical_peer(peer);
        let selectors = self.parse(&peer, declaration)?;
        let identity = self.declaration_identity(declaration)?;
        let mut state = self.state.try_lock().map_err(|_| busy())?;
        self.execute_locked(&peer, selectors, identity, &mut state)
            .await
    }

    /// The supervisor already owns admission when it reads its standing
    /// order. Keep that same guard through execution: a diagnostic reader
    /// must not slip between a recovery snapshot and its second admission.
    async fn execute_locked(
        &self,
        peer: &str,
        selectors: Vec<Value>,
        identity: Value,
        state: &mut State,
    ) -> Result<Value> {
        self.check_recording_admission(state).await?;
        {
            let declarations = self
                .declarations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if declarations.reservation.is_some() {
                return Err(busy());
            }
            if declarations
                .committed
                .as_ref()
                .is_some_and(|current| current != &identity)
            {
                return Err(failure(
                    "lifecycle.invalid-state",
                    "wake captured a replaced continuation declaration",
                ));
            }
        }
        if (state.sealed
            || state
                .session
                .as_ref()
                .is_some_and(|session| session.collection_sealed()))
            && state.session.is_some()
        {
            return Err(failure(
                "lifecycle.invalid-state",
                "continuation handoff is sealed",
            ));
        }
        if state.session.is_some()
            && (state.peer.as_deref() != Some(peer)
                || state.selectors != selectors
                || state.execution_declaration.as_ref() != Some(&identity))
        {
            return Err(failure(
                "lifecycle.invalid-state",
                "claim the pinned continuation before replacing its declaration",
            ));
        }
        let mut current_database = false;
        let setup = identity["setup"].as_array().cloned().unwrap_or_default();
        if state.session.is_none() {
            let generation = state.generation.checked_add(1).ok_or_else(|| {
                failure(
                    "lifecycle.invalid-state",
                    "continuation generations exhausted",
                )
            })?;
            if state.prepared.is_some() {
                state.archive_prepared_receipt();
            }
            state.session = Some(self.host.open_session()?);
            state.generation = generation;
            self.session_owned.store(true, Ordering::SeqCst);
            state.cleanup_receipt = None;
            state.claim_mode = None;
            state.legacy_stage = None;
            state.sealed = false;
            self.recovery_stopped.store(false, Ordering::SeqCst);
            state.peer = Some(peer.to_owned());
            state.selectors = selectors.clone();
            state.execution_declaration = Some(identity.clone());
            state.setup_generation = None;
            state.setup_complete = false;
            state.link_generation = None;
            state.link_outcome = None;
            state.link_failure = None;
            state.recording = identity["recording"]["id"].as_str().map(str::to_owned);
            state.recording_attached = false;
        } else {
            let snapshot = invoke(state, "session.reconcile", json!({})).await?;
            let connected = snapshot["links"].as_array().is_some_and(|links| {
                links.iter().any(|link| {
                    link["peerId"] == peer
                        && link["state"] == "connected"
                        && link["databaseState"] == "current"
                })
            });
            current_database = connected;
            let subscriptions = snapshot["subscriptions"].as_array().ok_or_else(|| {
                failure(
                    "platform.failure",
                    "missing authoritative subscription snapshot",
                )
            })?;
            state.consumers.retain(|active| {
                connected
                    && subscriptions
                        .iter()
                        .any(|item| item["consumer"] == active.consumer && item["state"] == "live")
            });
            if connected
                && state.consumers.len() == selectors.len()
                && setup.is_empty()
                && identity.get("link").is_none()
                && (state.recording.is_none() || state.recording_attached)
                && !state
                    .session
                    .as_ref()
                    .is_some_and(|session| session.collection_sealed())
            {
                return Ok(completed(peer, selectors.len(), state));
            }
        }
        if let Some(id) = state.recording.clone()
            && !state.recording_attached
        {
            let executor = self.clone();
            let identity = identity.clone();
            let peer = peer.to_owned();
            let session = state.session.as_ref().expect("owned session").clone();
            crate::continuation_journal::run_blocking_result(move || {
                let options = &identity["recording"];
                let journal = executor
                    .recordings
                    .open(
                        &id,
                        &identity,
                        JournalQuota {
                            max_bytes: options["maxBytes"]
                                .as_u64()
                                .expect("validated recording quota"),
                            max_records: options["maxRecords"]
                                .as_u64()
                                .expect("validated recording quota"),
                        },
                    )
                    .map_err(recording_failure)?;
                crate::continuation_journal::attach_legacy_recording_owner(
                    &journal,
                    &session,
                    &executor.recovery_stopped,
                    &executor.recovery_wake,
                    json!({"peerId":peer}),
                )
            })
            .await?;
            state.recording_attached = true;
        }
        if state.history.len() + selectors.len() - state.consumers.len() > 4096 {
            return Err(failure(
                "lifecycle.invalid-state",
                "continuation selector history is full; claim the retained backlog",
            ));
        }
        if !current_database {
            admitted(state, "connection.connect", json!({"peerId":peer,"lease":"continuation-lease",
            "operationId":"continuation-connect","intent":"direct","transport":"auto","preferredPhy":[],"budgetMs":15000})).await?;
            admitted(
                state,
                "gatt.discover",
                json!({"peerId":peer,"lease":"continuation-lease",
            "operationId":"continuation-discover","budgetMs":20000}),
            )
            .await?;
        }
        if let Some(link) = identity.get("link") {
            let snapshot = invoke(state, "session.reconcile", json!({})).await?;
            let generation = snapshot["links"]
                .as_array()
                .and_then(|links| {
                    links
                        .iter()
                        .find(|link| link["peerId"] == peer && link["state"] == "connected")
                })
                .and_then(|link| link["connectionGeneration"].as_str())
                .ok_or_else(|| {
                    failure(
                        "platform.failure",
                        "MTU prerequisite requires authoritative connection generation",
                    )
                })?
                .to_owned();
            if state.link_generation.as_ref() != Some(&generation) {
                state.link_generation = Some(generation);
                state.link_outcome = None;
                state.link_failure = None;
                let mtu = &link["mtu"];
                let request = admitted(
                    state,
                    "connection.request-mtu",
                    json!({"peerId":peer,"lease":"continuation-lease","mtu":mtu["requested"],"budgetMs":mtu["timeoutMs"],"operationId":"continuation-request-mtu"}),
                );
                let result = tokio::time::timeout(
                    std::time::Duration::from_millis(
                        mtu["timeoutMs"].as_u64().expect("validated MTU timeout"),
                    ),
                    request,
                )
                .await
                .unwrap_or_else(|_| {
                    Err(failure(
                        "operation.timed-out",
                        "MTU prerequisite deadline elapsed",
                    ))
                });
                let outcome=match result {
                    Ok(value)=>{
                        value["mtu"].as_u64().filter(|mtu|(23..=517).contains(mtu))
                            .map(|actual|json!({"mtu":{"requested":mtu["requested"],"outcome":"negotiated","mtu":actual}}))
                            .ok_or_else(||failure("platform.failure","MTU prerequisite returned no valid negotiated MTU"))
                    },
                    Err(mut error) if error["code"]=="capability.unsupported" && mtu["onUnsupported"]=="continue" => {
                        take_retryability(&mut error);
                        Ok(json!({"mtu":{"requested":mtu["requested"],"outcome":"unsupported","error":error}}))
                    },
                    Err(error)=>Err(error),
                };
                match outcome {
                    Ok(outcome) => state.link_outcome = Some(outcome),
                    Err(mut error) => {
                        error["retryability"] = json!("never");
                        state.link_failure = Some(error);
                    }
                }
            }
            if let Some(error) = &state.link_failure {
                return Err(error.clone());
            }
        }
        for (selector_index, selector) in selectors.iter().enumerate() {
            if state
                .consumers
                .iter()
                .any(|active| active.selector_index == selector_index)
            {
                continue;
            }
            let consumer = format!("ubm-continuation-{}", state.history.len());
            let mut wire_selector = selector.clone();
            for field in ["serviceOccurrence", "characteristicOccurrence"] {
                wire_selector[field] =
                    json!(selector[field].as_u64().expect("validated occurrence") - 1);
            }
            let operation = format!("continuation-subscribe-{}", state.history.len());
            if state.recording_attached {
                let snapshot = invoke(state, "session.reconcile", json!({})).await?;
                let link = snapshot["links"]
                    .as_array()
                    .and_then(|links| {
                        links.iter().find(|link| {
                            link["peerId"] == peer
                                && link["state"] == "connected"
                                && link["databaseState"] == "current"
                        })
                    })
                    .ok_or_else(|| {
                        failure(
                            "lifecycle.invalid-state",
                            "durable subscription requires a current database",
                        )
                    })?;
                if !link["connectionGeneration"].is_string()
                    || !link["databaseGeneration"].is_string()
                {
                    return Err(failure(
                        "platform.failure",
                        "durable metadata requires authoritative generations",
                    ));
                }
                let metadata = json!({"consumer":consumer,"peerId":peer,"connectionGeneration":link["connectionGeneration"],"databaseGeneration":link["databaseGeneration"],"selector":selector});
                let session = state.session.as_ref().expect("owned session").clone();
                let consumer = consumer.clone();
                crate::continuation_journal::run_blocking_result(move || {
                    session.register_journal_consumer(&consumer, metadata)
                })
                .await?;
            }
            // Reserve immutable identity before dispatch: native enable may
            // emit a value before returning a refused completion.
            state.history.push(selector.clone());
            admitted(
                state,
                "gatt.subscribe",
                json!({"peerId":peer,"selector":wire_selector,
                "consumer":consumer,"operationId":operation,"budgetMs":10000}),
            )
            .await?;
            state.consumers.push(ActiveConsumer {
                selector_index,
                consumer,
            });
        }
        if !setup.is_empty() {
            let snapshot = invoke(state, "session.reconcile", json!({})).await?;
            let link = snapshot["links"]
                .as_array()
                .and_then(|links| {
                    links.iter().find(|link| {
                        link["peerId"] == peer
                            && link["state"] == "connected"
                            && link["databaseState"] == "current"
                    })
                })
                .ok_or_else(|| {
                    failure(
                        "lifecycle.invalid-state",
                        "setup requires a current database",
                    )
                })?;
            if !link["connectionGeneration"].is_string() || !link["databaseGeneration"].is_string()
            {
                return Err(failure(
                    "platform.failure",
                    "setup requires authoritative generations",
                ));
            }
            let generation = json!([link["connectionGeneration"], link["databaseGeneration"]]);
            if state.setup_generation.as_ref() != Some(&generation) {
                state.setup_generation = Some(generation.clone());
                state.setup_complete = false;
            }
            state.setup_failures.check(peer, &generation)?;
            if !state.setup_complete {
                if let Err(mut error) = run_setup(self, state, peer, &setup).await {
                    // An old reply cannot be correlated safely with a repeated
                    // command. Only authoritative generation change clears this.
                    error["retryability"] = json!("never");
                    state.setup_failures.record(peer, generation, error.clone());
                    return Err(error);
                }
                state.setup_complete = true;
            }
        }
        self.check_recording_admission(state).await?;
        if state
            .session
            .as_ref()
            .is_some_and(|session| session.collection_sealed())
        {
            return Err(failure(
                "lifecycle.invalid-state",
                "native collection was explicitly stopped",
            ));
        }
        Ok(completed(peer, selectors.len(), state))
    }

    async fn observe_state(&self) -> Result<tokio::sync::MutexGuard<'_, State>> {
        match self.state.try_lock() {
            Ok(state) => Ok(state),
            // Autonomous retry must not make foreground observation depend on
            // catching the backoff gap. Join the same FIFO admission queue,
            // allowing the current bounded operation to settle first.
            // Explicit caller executions retain their existing busy contract.
            Err(_) if self.recovering.load(Ordering::SeqCst) => Ok(self.state.lock().await),
            Err(_) => Err(busy()),
        }
    }

    /// Reject an incompatible caller before sealing or claiming the source.
    pub async fn prepare_claim_versioned(
        &self,
        version: u32,
        max_items: u32,
        max_bytes: u32,
    ) -> Result<Value> {
        let mode = match version {
            CONTINUATION_HANDOFF_V1 => ClaimMode::RetainedV1,
            CONTINUATION_HANDOFF_V2 => ClaimMode::RetainedV2,
            _ => {
                return Err(failure(
                    "capability.unsupported",
                    "unsupported continuation handoff version",
                ));
            }
        };
        self.prepare_claim_mode(mode, max_items, max_bytes).await
    }

    pub async fn prepare_claim(&self, max_items: u32, max_bytes: u32) -> Result<Value> {
        self.prepare_claim_mode(ClaimMode::Legacy, max_items, max_bytes)
            .await
    }

    async fn prepare_claim_mode(
        &self,
        mode: ClaimMode,
        max_items: u32,
        max_bytes: u32,
    ) -> Result<Value> {
        if max_items == 0 || max_bytes == 0 {
            return Err(invalid("claim bounds must be positive"));
        }
        let mut state = self.observe_state().await?;
        if state.claim_mode.is_some_and(|selected| selected != mode) {
            return Err(failure(
                "lifecycle.invalid-state",
                "continuation handoff protocol conflict",
            ));
        }
        if let Some(prepared) = &state.prepared {
            // An unacknowledged claim, or an acknowledged one whose cleanup
            // receipt is still pending, must replay exactly. Once the receipt
            // exists the source prefix is settled and the next prepare owns a
            // new prefix; retaining a completed claim here would permanently
            // reuse its token and strand every later V1 prefix.
            if !prepared.acknowledged || prepared.receipt.is_none() {
                let mut claim = prepared.claim.clone();
                if prepared.acknowledged {
                    claim["batches"] = json!([]);
                }
                drop(state);
                self.await_stopped_recovery().await;
                return Ok(claim);
            }
            state.archive_prepared_receipt();
        }
        if state.session.is_none() {
            return Ok(
                json!({"consumerCount":0,"batches":[],"disposed":false,"disposeFailure":null,
                "afterCutoffLoss":{"items":0,"bytes":0},"selectors":[]}),
            );
        }
        if mode == ClaimMode::Legacy
            && state.claim_mode.is_none()
            && !state
                .session
                .as_ref()
                .expect("session retained")
                .legacy_claim_compatible()
        {
            return Err(failure(
                "protocol.incompatible",
                "legacy continuation cannot represent this source; a versioned handoff is required",
            ));
        }
        if state.receipts.len() >= MAX_CLAIM_RECEIPTS {
            return Err(failure(
                "lifecycle.invalid-state",
                "continuation receipt capacity exhausted; retire acknowledged ownership first",
            ));
        }
        let next_token = state.token.checked_add(1).ok_or_else(|| {
            failure(
                "lifecycle.invalid-state",
                "continuation claim identities exhausted",
            )
        })?;
        if state.executor_identity.is_none() {
            let mut bytes = [0u8; 16];
            getrandom::fill(&mut bytes).map_err(|_| {
                failure(
                    "platform.failure",
                    "continuation identity entropy unavailable",
                )
            })?;
            state.executor_identity =
                Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect());
        }
        if matches!(mode, ClaimMode::RetainedV1 | ClaimMode::RetainedV2)
            && state.receipt_token_key.is_none()
        {
            let mut key = [0u8; 32];
            getrandom::fill(&mut key).map_err(|_| {
                failure(
                    "platform.failure",
                    "continuation receipt entropy unavailable",
                )
            })?;
            state.receipt_token_key = Some(key);
        }
        // Both the inner batch and its enclosing native envelope are bounded.
        // Budget for repeated JSON escaping as well as selector/framing bytes
        // before asking the source to select a replayable prefix.
        let max_bytes = if matches!(mode, ClaimMode::RetainedV1 | ClaimMode::RetainedV2) {
            let frame = json!({"ok":true,"value":{"consumerCount":state.history.len(),"batches":[],
                "disposed":false,"disposeFailure":null,"afterCutoffLoss":{"items":u64::MAX,"bytes":u64::MAX},
                "selectors":state.history,"recording":state.recording,"claimToken":"x".repeat(128)}}).to_string().len();
            let budget = MAX_CLAIM_WIRE_BYTES
                .checked_sub(frame.saturating_add(1024))
                .map(|remaining| remaining / 4)
                .filter(|remaining| *remaining >= 2)
                .ok_or_else(|| {
                    failure(
                        "bytes.too-large",
                        "continuation claim metadata exceeds wire limit",
                    )
                })?;
            max_bytes.min(u32::try_from(budget).unwrap_or(u32::MAX))
        } else {
            max_bytes
        };
        state.claim_mode = Some(mode);
        let loss = if let Some(stage) = &state.legacy_stage {
            stage.loss.clone()
        } else if let Some(receipt) = &state.cleanup_receipt {
            receipt["afterCutoffLoss"].clone()
        } else {
            let cutoff = invoke(&state, "session.quiesce", json!({})).await?;
            if cutoff["state"] != "sealed" {
                return Err(failure("platform.failure", "quiesce did not seal"));
            }
            cutoff_loss(&cutoff)?
        };
        state.sealed = true;
        self.recovery_stopped.store(true, Ordering::SeqCst);
        self.recovery_wake.notify_one();
        let (batches, source_claim, complete, drain_failure, legacy_evidence) = if mode
            == ClaimMode::Legacy
        {
            if state.legacy_stage.is_none() {
                let budget = state
                    .legacy_evidence_budget
                    .get_or_insert_with(|| {
                        Arc::new(LegacyEvidenceBudget::new(
                            LEGACY_EVIDENCE_MAX_ITEMS,
                            LEGACY_EVIDENCE_MAX_BYTES,
                        ))
                    })
                    .clone();
                state.legacy_stage = Some(LegacyStage {
                    batches: Vec::new(),
                    pending: None,
                    evidence: Arc::new(LegacyEvidenceOwner::with_budget(
                        LEGACY_EVIDENCE_MAX_ITEMS,
                        LEGACY_EVIDENCE_MAX_BYTES,
                        budget,
                    )),
                    loss: loss.clone(),
                    max_items,
                    max_bytes,
                });
            }
            let session = state.session.as_ref().expect("session retained").clone();
            let stage = state.legacy_stage.as_mut().expect("stage retained");
            if stage.max_items == 0 {
                stage.max_items = max_items;
                stage.max_bytes = max_bytes;
            }
            let mut complete = stage.batches.last().is_some_and(|batch| {
                serde_json::from_str::<Value>(batch).is_ok_and(|value| value["more"] == false)
            });
            let mut drain_failure = None;
            while !complete && stage.batches.len() < 32 {
                if let Some(result) =
                    session.legacy_drain(&stage.evidence, stage.max_items, stage.max_bytes)
                {
                    let batch = match result {
                        Ok(batch) => batch,
                        Err(error) if !stage.batches.is_empty() => {
                            drain_failure =
                                Some(format!("continuation drain remains retained: {error}"));
                            break;
                        }
                        Err(error) => return Err(error),
                    };
                    if !batch["records"].is_array()
                        || !batch["more"].is_boolean()
                        || !batch["controlLost"].is_u64()
                    {
                        return Err(failure("platform.failure", "invalid legacy drain shape"));
                    }
                    complete = batch["more"] == false;
                    stage.batches.push(batch.to_string());
                    continue;
                }
                if stage.pending.is_none() {
                    let source = match session.claim_drain(stage.max_items, stage.max_bytes) {
                        Err(error) if error["requiredBytes"].as_u64().is_some() => {
                            let required = error["requiredBytes"]
                                .as_u64()
                                .and_then(|n| u32::try_from(n).ok())
                                .ok_or(error)?;
                            session.claim_drain(1, required)
                        }
                        result => result,
                    };
                    match source {
                        Ok(Some(claim)) => stage.pending = Some((claim, false)),
                        Ok(None) => {
                            if stage.batches.is_empty() {
                                stage.batches.push(
                                    json!({"records":[],"more":false,"controlLost":0}).to_string(),
                                );
                            }
                            complete = true;
                            break;
                        }
                        Err(error) if !stage.batches.is_empty() => {
                            drain_failure =
                                Some(format!("continuation drain remains retained: {error}"));
                            break;
                        }
                        Err(error) => return Err(error),
                    }
                }
                // Stage the source identity and payload before ACK. If an ACK
                // reply is lost, retry the same identity without losing data.
                let (pending, acknowledged) = stage.pending.as_mut().expect("claim staged");
                if !*acknowledged {
                    session.acknowledge_drain(pending.id)?;
                    *acknowledged = true;
                }
                let more = session.has_retained_tail()?;
                let (claim, _) = stage.pending.take().expect("claim staged");
                stage.batches.push(
                    json!({"records":claim.records,"more":more,"controlLost":claim.control_lost})
                        .to_string(),
                );
                if !more {
                    complete = true;
                    break;
                }
            }
            let stage = state.legacy_stage.take().expect("stage retained");
            (
                stage.batches,
                None,
                complete,
                drain_failure,
                Some(stage.evidence),
            )
        } else {
            let session = state.session.as_ref().expect("session retained").clone();
            let evidence = if mode == ClaimMode::RetainedV1 {
                let budget = state
                    .legacy_evidence_budget
                    .get_or_insert_with(|| {
                        Arc::new(LegacyEvidenceBudget::new(
                            LEGACY_EVIDENCE_MAX_ITEMS,
                            LEGACY_EVIDENCE_MAX_BYTES,
                        ))
                    })
                    .clone();
                Some(Arc::new(LegacyEvidenceOwner::with_budget(
                    LEGACY_EVIDENCE_MAX_ITEMS,
                    LEGACY_EVIDENCE_MAX_BYTES,
                    budget,
                )))
            } else {
                None
            };
            let source = if let Some(evidence) = &evidence {
                session
                    .claim_drain_v1(evidence, max_items, max_bytes)
                    .ok_or_else(|| failure("capability.unsupported", "session cannot atomically project continuation V1"))?
            } else {
                session.claim_drain(max_items, max_bytes)
            }
                .map_err(|error| {
                    if error["requiredBytes"].as_u64().is_some() {
                        failure("bytes.too-large", "continuation record exceeds the requested or wire byte limit; the source remains retained")
                    } else {
                        error
                    }
                })?;
            let source_claim = source.as_ref().map(|claim| claim.id);
            let batches = source.map(|source| vec![
                json!({"records":source.records,"more":true,"controlLost":source.control_lost}).to_string()
            ]).unwrap_or_default();
            (
                batches,
                source_claim,
                source_claim.is_none(),
                None,
                evidence,
            )
        };
        state.token = next_token;
        let mut token = format!(
            "continuation-{}-{}-{}",
            state
                .executor_identity
                .as_deref()
                .expect("identity assigned"),
            state.generation,
            state.token
        );
        if let Some(label) = mode.token_label() {
            token.push('-');
            token.push_str(label);
            let mut mac = Hmac::<Sha256>::new_from_slice(
                state.receipt_token_key.as_ref().expect("key assigned"),
            )
            .expect("HMAC key");
            mac.update(token.as_bytes());
            token.push('-');
            for byte in mac.finalize().into_bytes() {
                token.push_str(&format!("{byte:02x}"));
            }
        }
        let mut claim = json!({"consumerCount":state.history.len(),"batches":batches,"disposed":false,
            "disposeFailure":if complete || matches!(mode, ClaimMode::RetainedV1 | ClaimMode::RetainedV2) {None} else {Some(drain_failure.unwrap_or_else(|| "continuation claim has a retained unread tail".to_owned()))},"afterCutoffLoss":loss,"selectors":state.history,
            "claimToken":token});
        if let Some(id) = &state.recording {
            claim["recording"] = json!({"id":id});
        }
        if let Some(evidence) = legacy_evidence {
            state.legacy_evidence.insert(token.clone(), evidence);
        }
        state.prepared = Some(Prepared {
            mode,
            claim: claim.clone(),
            complete,
            acknowledged: false,
            receipt: None,
            source_claim,
        });
        drop(state);
        self.await_stopped_recovery().await;
        Ok(claim)
    }

    pub async fn acknowledge_claim(&self, token: &str) -> Result<Value> {
        self.acknowledge_claim_mode(ClaimMode::Legacy, token).await
    }

    pub async fn acknowledge_claim_v1(&self, token: &str) -> Result<Value> {
        self.acknowledge_claim_mode(ClaimMode::RetainedV1, token)
            .await
    }

    /// Acknowledge an explicitly negotiated V2 prefix.  V2 has a distinct
    /// mode so an older V1 handle can never ACK a richer claim by accident.
    pub async fn acknowledge_claim_v2(&self, token: &str) -> Result<Value> {
        self.acknowledge_claim_mode(ClaimMode::RetainedV2, token)
            .await
    }

    async fn acknowledge_claim_mode(&self, mode: ClaimMode, token: &str) -> Result<Value> {
        // The operation owns its state until completion even if the caller
        // drops the reply future while native cleanup is in flight.
        let owner = self.clone();
        let token = token.to_owned();
        tokio::spawn(async move { owner.acknowledge_claim_owned(mode, &token).await })
            .await
            .map_err(|_| {
                failure(
                    "platform.failure",
                    "continuation acknowledgement worker failed",
                )
            })?
    }

    /// Explicitly release recovery ownership of one acknowledged retained
    /// receipt. Legacy receipts remain replayable until superseded by the
    /// next Legacy handoff, preserving the released 5.2.1 lost-reply contract.
    pub async fn retire_claim_receipt(&self, token: &str) -> Result<()> {
        self.retire_claim_receipt_mode(None, token).await
    }

    /// Retire an acknowledged V1 retained-prefix receipt only. A V1 bridge
    /// must not be able to release recovery ownership held by a V2 receipt.
    pub async fn retire_claim_receipt_v1(&self, token: &str) -> Result<()> {
        self.retire_claim_receipt_mode(Some(ClaimMode::RetainedV1), token)
            .await
    }

    async fn retire_claim_receipt_mode(
        &self,
        required_mode: Option<ClaimMode>,
        token: &str,
    ) -> Result<()> {
        let mut state = self.state.try_lock().map_err(|_| busy())?;
        if let Some((mode, _)) = state.receipts.get(token) {
            if *mode == ClaimMode::Legacy {
                return Err(invalid("Legacy claim receipts cannot be retired"));
            }
            if required_mode.is_some_and(|expected| expected != *mode) {
                return Err(invalid("claim receipt mode does not match this operation"));
            }
            state.receipts.remove(token);
            state.reclaim_empty_legacy_evidence(token);
            return Ok(());
        }
        if state.prepared.as_ref().is_some_and(|prepared| {
            prepared.claim["claimToken"] == token
                && prepared.receipt.is_some()
                && matches!(prepared.mode, ClaimMode::RetainedV1 | ClaimMode::RetainedV2)
                && required_mode.is_none_or(|expected| expected == prepared.mode)
        }) {
            state.prepared = None;
            state.reclaim_empty_legacy_evidence(token);
            return Ok(());
        }
        // A later issued sequence proves that all earlier prepared claims
        // were acknowledged. An authenticated token allows idempotent retry
        // of retirement without keeping an unbounded tombstone ledger.
        if required_mode.is_some_and(|expected| expected == ClaimMode::RetainedV1) {
            if state.retired_v1_claim_token(token) {
                return Ok(());
            }
        } else if state.retired_retained_claim_token(token) {
            return Ok(());
        }
        Err(invalid("no acknowledged receipt for claim token"))
    }

    /// Page exact originals retained for a strict 5.2.1 projection. The
    /// claim token identifies the same process-owned evidence even after the
    /// radio session has been released. This is deliberately additive: the
    /// legacy claim's wire shape remains unchanged for 5.2.1 callers.
    pub async fn legacy_evidence_page(
        &self,
        token: &str,
        after_ordinal: u64,
        max_items: u32,
        max_bytes: u32,
    ) -> Result<Value> {
        if max_items == 0 || max_bytes == 0 {
            return Err(invalid("legacy evidence page bounds must be positive"));
        }
        let state = self.state.try_lock().map_err(|_| busy())?;
        let Some(evidence) = state.legacy_evidence.get(token) else {
            if state.retired_v1_claim_token(token) {
                return Ok(json!({"records":[],"more":false}));
            }
            return Err(invalid("no legacy evidence for claim token"));
        };
        let (records, more) = evidence
            .page(after_ordinal, max_items as usize, max_bytes as usize)
            .map_err(legacy_evidence_error)?;
        Ok(json!({"records":records,"more":more}))
    }

    /// Retire one exact original only after its recovery consumer has
    /// durably accepted it. Retiring an absent or another claim's ordinal is
    /// a visible error, never an idempotent success that could hide loss.
    pub async fn legacy_evidence_retire(&self, token: &str, ordinal: u64) -> Result<()> {
        let mut state = self.state.try_lock().map_err(|_| busy())?;
        let Some(evidence) = state.legacy_evidence.get(token).cloned() else {
            return Err(invalid("no legacy evidence for claim token"));
        };
        if !evidence.retire(ordinal) {
            return Err(invalid("legacy evidence ordinal is not retained"));
        }
        // The owner has released every exact byte. Keep no empty per-token
        // object: `retired_v1_claim_token` provides the authenticated empty-page
        // response needed for a delayed page retry.
        if evidence.is_empty() {
            state.legacy_evidence.remove(token);
        }
        Ok(())
    }

    async fn acknowledge_claim_owned(&self, mode: ClaimMode, token: &str) -> Result<Value> {
        let mut state = self.state.try_lock().map_err(|_| busy())?;
        if let Some((receipt_mode, receipt)) = state.receipts.get(token) {
            return if *receipt_mode == mode {
                Ok(receipt.clone())
            } else {
                Err(invalid("claim protocol mismatch"))
            };
        }
        let prepared = state
            .prepared
            .as_ref()
            .ok_or_else(|| invalid("no prepared claim"))?;
        if prepared.mode != mode {
            return Err(invalid("claim protocol mismatch"));
        }
        if prepared.claim["claimToken"] != token {
            return Err(invalid("claim token mismatch"));
        }
        if let Some(receipt) = &prepared.receipt {
            return Ok(receipt.clone());
        }
        if !prepared.acknowledged {
            let source_claim = prepared.source_claim;
            if let Some(id) = source_claim {
                state
                    .session
                    .as_ref()
                    .ok_or_else(|| {
                        failure(
                            "lifecycle.invalid-state",
                            "continuation source owner is unavailable",
                        )
                    })?
                    .acknowledge_drain(id)?;
            }
            // No suspension between source ACK and recording its transfer receipt.
            state
                .prepared
                .as_mut()
                .expect("prepared handoff retained")
                .acknowledged = true;
        }
        if mode == ClaimMode::Legacy
            && !state
                .prepared
                .as_ref()
                .expect("prepared handoff retained")
                .complete
        {
            let prepared = state.prepared.as_mut().expect("prepared handoff retained");
            let receipt = json!({"disposed":false,"afterCutoffLoss":prepared.claim["afterCutoffLoss"],"disposeFailure":prepared.claim["disposeFailure"]});
            prepared.receipt = Some(receipt.clone());
            return Ok(receipt);
        }
        if !state
            .prepared
            .as_ref()
            .expect("prepared handoff retained")
            .complete
        {
            let tail = state
                .session
                .as_ref()
                .ok_or_else(|| {
                    failure(
                        "lifecycle.invalid-state",
                        "continuation source owner is unavailable",
                    )
                })?
                .has_retained_tail();
            match tail {
                Ok(false) => {
                    state
                        .prepared
                        .as_mut()
                        .expect("prepared handoff retained")
                        .complete = true
                }
                result => {
                    let detail = match result {
                        Ok(true) => "continuation claim has a retained unread tail".to_owned(),
                        Err(error) => format!("continuation tail remains retained: {error}"),
                        Ok(false) => unreachable!(),
                    };
                    let prepared = state.prepared.as_mut().expect("prepared handoff retained");
                    let receipt = json!({"disposed":false,"afterCutoffLoss":prepared.claim["afterCutoffLoss"],"disposeFailure":detail});
                    prepared.receipt = Some(receipt.clone());
                    return Ok(receipt);
                }
            }
        }
        let receipt = if let Some(receipt) = &state.cleanup_receipt {
            receipt.clone()
        } else {
            let disposal = invoke(&state, "session.continuation-dispose", json!({})).await?;
            let released = disposal["state"] == "released";
            let receipt = json!({"disposed":released,"afterCutoffLoss":cutoff_loss(&disposal)?,
                "disposeFailure":if released {None} else {Some(format!("continuation disposal remains owned: {disposal}"))}});
            if !released {
                return Ok(receipt);
            }
            state.cleanup_receipt = Some(receipt.clone());
            receipt
        };
        // Cleanup may emit terminal controls after the earlier empty check.
        // Keep the exact source alive until those records are handed off too.
        let session = state
            .session
            .as_ref()
            .expect("source owner retained")
            .clone();
        let tail = if mode == ClaimMode::Legacy {
            let token = state
                .prepared
                .as_ref()
                .expect("prepared handoff retained")
                .claim["claimToken"]
                .as_str()
                .expect("own token");
            let evidence = state.legacy_evidence.get(token).cloned().ok_or_else(|| {
                failure(
                    "lifecycle.invalid-state",
                    "legacy evidence owner is unavailable",
                )
            })?;
            match session.legacy_drain(&evidence, 1, 1) {
                Some(Ok(batch))
                    if batch["records"]
                        .as_array()
                        .is_some_and(|records| !records.is_empty()) =>
                {
                    state.legacy_stage = Some(LegacyStage {
                        batches: vec![batch.to_string()],
                        pending: None,
                        evidence,
                        loss: receipt["afterCutoffLoss"].clone(),
                        max_items: 0,
                        max_bytes: 0,
                    });
                    Ok(true)
                }
                Some(Ok(batch)) if batch["records"].is_array() && batch["more"] == false => {
                    Ok(false)
                }
                Some(Ok(_)) => Err(failure("platform.failure", "invalid legacy cleanup drain")),
                Some(Err(error)) => Err(error),
                None => session.has_retained_tail(),
            }
        } else {
            session.has_retained_tail()
        };
        if !matches!(tail, Ok(false)) {
            let prepared = state.prepared.as_mut().expect("prepared handoff retained");
            prepared.complete = false;
            let pending = json!({"disposed":false,"afterCutoffLoss":receipt["afterCutoffLoss"],
                "disposeFailure":match tail { Err(error) => format!("continuation cleanup tail remains retained: {error}"), _ => "continuation cleanup has a retained unread tail".to_owned() }});
            prepared.receipt = Some(pending.clone());
            return Ok(pending);
        }
        state.session = None;
        self.session_owned.store(false, Ordering::SeqCst);
        // The registry retains the durable journal independently. Only
        // this released radio generation's recording attachment is retired.
        state.recording = None;
        state.recording_attached = false;
        state.peer = None;
        state.selectors.clear();
        state.history.clear();
        state.consumers.clear();
        state
            .prepared
            .as_mut()
            .expect("prepared handoff retained")
            .receipt = Some(receipt.clone());
        Ok(receipt)
    }
}

impl NativeContinuation {
    // A published handoff must not race the stopped supervisor for admission.
    // Quiescence already acquired the state lock, so this waits only for the
    // worker to observe its stop signal, never for an in-flight radio operation.
    async fn await_stopped_recovery(&self) {
        loop {
            let notified = self.recovery_idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.recovering.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    fn declaration_identity(&self, text: &str) -> Result<Value> {
        if text.len() > 1 << 20 {
            return Err(invalid("oversized declaration"));
        }
        let root: Value =
            serde_json::from_str(text).map_err(|_| invalid("malformed declaration"))?;
        let object = root
            .as_object()
            .ok_or_else(|| invalid("declaration must be an object"))?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "onAppearance"
                    | "peerId"
                    | "resubscribe"
                    | "headlessTaskName"
                    | "foregroundService"
                    | "setup"
                    | "link"
                    | "recording"
            )
        }) {
            return Err(invalid("unknown declaration field"));
        }
        if root["onAppearance"] == "native" {
            let peer = root
                .get("peerId")
                .map_or(Ok("continuation-any-peer"), |value| {
                    value
                        .as_str()
                        .ok_or_else(|| invalid("peerId must be a string"))
                })?;
            let selectors = self.parse(peer, text)?;
            let setup = self.setup(&root, selectors.len())?;
            let mut identity = json!({"onAppearance":"native","resubscribe":selectors});
            if let Some(recording) = root.get("recording") {
                exact(recording, &["id", "maxBytes", "maxRecords"])?;
                let id = recording["id"]
                    .as_str()
                    .ok_or_else(|| invalid("recording id required"))?;
                if id.is_empty()
                    || id.len() > 64
                    || !id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
                {
                    return Err(invalid("invalid recording id"));
                }
                integer(&recording["maxBytes"], 1 << 20, 1 << 30)?;
                integer(&recording["maxRecords"], 1, 1_000_000)?;
                identity["recording"] = recording.clone();
            }
            if let Some(link) = root.get("link") {
                exact(link, &["mtu"])?;
                let mtu = &link["mtu"];
                exact(mtu, &["requested", "timeoutMs", "onUnsupported"])?;
                integer(&mtu["requested"], 23, 517)?;
                integer(&mtu["timeoutMs"], 1, 20000)?;
                if !matches!(mtu["onUnsupported"].as_str(), Some("continue" | "fail")) {
                    return Err(invalid("invalid unsupported MTU policy"));
                }
                identity["link"] = link.clone();
            }
            if !setup.is_empty() {
                identity["setup"] = json!(setup);
            }
            if object.contains_key("peerId") {
                identity["peerId"] = json!(self.host.canonical_peer(peer));
            }
            Ok(identity)
        } else if matches!(
            root["onAppearance"].as_str(),
            Some("record-only" | "headless-task" | "foreground-service")
        ) || !object.contains_key("onAppearance")
        {
            if object.contains_key("setup")
                || object.contains_key("link")
                || object.contains_key("recording")
            {
                return Err(invalid("setup and link require native continuation"));
            }
            Ok(root)
        } else {
            Err(invalid("unknown declaration strategy"))
        }
    }

    /// Reserve authority before the platform writes persistent configuration.
    /// While reserved, no old or new captured wake may create native work.
    pub fn reserve_declaration(&self, declaration: &str) -> Result<Value> {
        let identity = self.declaration_identity(declaration)?;
        let state = self.state.try_lock().map_err(|_| busy())?;
        let mut declarations = self
            .declarations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if declarations.reservation.is_some() {
            return Err(busy());
        }
        if state.session.is_some() && state.execution_declaration.as_ref() != Some(&identity) {
            return Err(failure(
                "lifecycle.invalid-state",
                "claim the pinned continuation before replacing its declaration",
            ));
        }
        declarations.sequence = declarations.sequence.checked_add(1).ok_or_else(|| {
            failure(
                "lifecycle.invalid-state",
                "declaration generation exhausted",
            )
        })?;
        let token = format!("continuation-declaration-{}", declarations.sequence);
        declarations.reservation = Some(DeclarationReservation {
            token: token.clone(),
            declaration: identity,
        });
        Ok(json!({"reservationToken":token}))
    }

    pub fn commit_declaration(&self, token: &str) -> Result<Value> {
        let mut declarations = self
            .declarations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !declarations
            .reservation
            .as_ref()
            .is_some_and(|reservation| reservation.token == token)
        {
            return Err(invalid("declaration reservation token mismatch"));
        }
        declarations.committed = declarations
            .reservation
            .take()
            .map(|reservation| reservation.declaration);
        drop(declarations);
        if let Some(runtime) = self.recovery_runtime.get() {
            self.request_recovery(runtime);
        }
        Ok(json!({"state":"committed"}))
    }

    pub fn cancel_declaration(&self, token: &str) -> Result<Value> {
        let mut declarations = self
            .declarations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !declarations
            .reservation
            .as_ref()
            .is_some_and(|reservation| reservation.token == token)
        {
            return Err(invalid("declaration reservation token mismatch"));
        }
        declarations.reservation = None;
        drop(declarations);
        if let Some(runtime) = self.recovery_runtime.get() {
            self.request_recovery(runtime);
        }
        Ok(json!({"state":"cancelled"}))
    }

    /// Cold-start seeding cannot overwrite a declaration already committed by
    /// another native caller, even when no session has been opened yet.
    pub fn seed_declaration(&self, declaration: &str) -> Result<Value> {
        let identity = self.declaration_identity(declaration)?;
        let state = self.state.try_lock().map_err(|_| busy())?;
        let mut declarations = self
            .declarations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if declarations.reservation.is_some() {
            return Err(busy());
        }
        if declarations
            .committed
            .as_ref()
            .is_some_and(|current| current != &identity)
        {
            return Err(failure(
                "lifecycle.invalid-state",
                "persisted declaration is older than native authority",
            ));
        }
        if state.session.is_some() && state.execution_declaration.as_ref() != Some(&identity) {
            return Err(failure(
                "lifecycle.invalid-state",
                "native session owns a different declaration",
            ));
        }
        declarations.committed = Some(identity);
        Ok(json!({"state":"seeded"}))
    }
    /// Stops automatic retry at authoritative owner shutdown, without claiming
    /// successful resource cleanup or deleting a retained handoff.
    pub fn stop_recovery(&self) {
        self.recovery_stopped.store(true, Ordering::SeqCst);
        self.recovery_wake.notify_one();
    }
    /// Called after the host has reconciled a lifecycle/adapter event. One
    /// worker coalesces events; radio refusal receives bounded backoff and is
    /// retained in diagnostics, never represented as successful recovery.
    pub fn request_recovery(&self, runtime: &tokio::runtime::Handle) {
        self.recovery_runtime.get_or_init(|| runtime.clone());
        if self.recovery_stopped.load(Ordering::SeqCst)
            || !self.session_owned.load(Ordering::SeqCst)
        {
            return;
        }
        self.recovery_requested.store(true, Ordering::SeqCst);
        if self.recovering.swap(true, Ordering::SeqCst) {
            self.recovery_wake.notify_one();
            return;
        }
        let executor = self.clone();
        runtime.spawn(async move {
            loop {
                executor.recovery_requested.store(false, Ordering::SeqCst);
                let mut attempt = 0_u32;
                loop {
                    if executor.recovery_stopped.load(Ordering::SeqCst) || !executor.session_owned.load(Ordering::SeqCst) { break; }
                    let result = {
                        // The internal worker waits fairly for admission. Public
                        // callers still fail promptly, but repeated status reads
                        // cannot starve an already-requested native recovery.
                        let mut state = tokio::select! {
                            biased;
                            _ = executor.recovery_wake.notified() => { continue; },
                            state = executor.state.lock() => state,
                        };
                        if executor.recovery_stopped.load(Ordering::SeqCst) || state.session.is_none() || state.sealed { None }
                        else if let Some((peer, identity)) = state.peer.clone().zip(state.execution_declaration.clone()) {
                            let selectors = state.selectors.clone();
                            Some(executor.execute_locked(&peer, selectors, identity, &mut state).await)
                        } else {
                            Some(Err(failure("lifecycle.invalid-state", "owned continuation has no declaration identity")))
                        }
                    };
                    let Some(result) = result else { break; };
                    let retry = match &result {
                        Ok(_) => false,
                        Err(error) => error["retryability"] == "caller-decides",
                    };
                    *executor.last_recovery.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(match result {
                        Ok(mut outcome) => { outcome["attempt"] = json!(attempt.saturating_add(1)); outcome },
                        Err(mut error) => {
                            let retryability = take_retryability(&mut error);
                            json!({"event":"continuation.failed","strategy":"native","error":error,"retryability":retryability,"attempt":attempt.saturating_add(1)})
                        },
                    });
                    if !retry { break; }
                    tokio::select! {
                        _ = executor.recovery_wake.notified() => {},
                        _ = tokio::time::sleep(std::time::Duration::from_millis((100_u64 << attempt.min(9)).min(30_000))) => {},
                    }
                    attempt = attempt.saturating_add(1);
                }
                executor.recovering.store(false, Ordering::SeqCst);
                if !executor.recovery_requested.swap(false, Ordering::SeqCst) || executor.recovering.swap(true, Ordering::SeqCst) { break; }
            }
            executor.recovery_idle.notify_waiters();
        });
    }

    /// A synchronous, nonblocking declaration admission check for persistence.
    /// No live state means the platform validator remains authoritative.
    pub fn declaration_replacement_failure(&self, declaration: &str) -> Option<String> {
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(_) => return Some("continuation execution or handoff is in progress".to_owned()),
        };
        state.session.as_ref()?;
        match self.declaration_identity(declaration) {
            Ok(identity) if state.execution_declaration.as_ref() == Some(&identity) => None,
            _ => Some("claim the pinned continuation before replacing its declaration".to_owned()),
        }
    }

    pub async fn describe_backlog(&self) -> Result<Value> {
        let state = self.observe_state().await?;
        if state.session.is_none() {
            Ok(Value::Null)
        } else {
            let mut counters = invoke(&state, "counters.describe", json!({})).await?;
            counters["continuationOutcome"] = self
                .last_recovery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .unwrap_or(Value::Null);
            Ok(counters)
        }
    }
}

pub fn envelope(result: Result<Value>) -> String {
    match result {
        Ok(value) => json!({"ok":true,"value":value}).to_string(),
        Err(mut error) => {
            let retryability = take_retryability(&mut error);
            json!({"ok":false,"error":error,"commit":null,"retryability":retryability}).to_string()
        }
    }
}

fn take_retryability(error: &mut Value) -> Value {
    error
        .as_object_mut()
        .and_then(|error| error.remove("retryability"))
        .unwrap_or(json!("never"))
}

fn completed(peer: &str, count: usize, state: &State) -> Value {
    let mut result = json!({"event":"continuation.completed","strategy":"native","peerAddress":peer,"resubscribed":count});
    if let Some(link) = &state.link_outcome {
        result["link"] = link.clone();
    }
    result
}

fn exact(value: &Value, keys: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("setup object required"))?;
    if object.keys().any(|key| !keys.contains(&key.as_str())) {
        return Err(invalid("unknown setup field"));
    }
    Ok(())
}
fn integer(value: &Value, min: u64, max: u64) -> Result<u64> {
    value
        .as_u64()
        .filter(|value| *value >= min && *value <= max)
        .ok_or_else(|| invalid("invalid setup integer"))
}
fn bytes(value: &Value) -> Result<Vec<u8>> {
    let bytes = value
        .as_array()
        .ok_or_else(|| invalid("setup bytes must be an array"))?;
    if bytes.is_empty() || bytes.len() > 512 {
        return Err(invalid("invalid setup byte length"));
    }
    bytes
        .iter()
        .map(|value| integer(value, 0, 255).map(|byte| byte as u8))
        .collect()
}

async fn run_setup(
    executor: &NativeContinuation,
    state: &mut State,
    peer: &str,
    steps: &[Value],
) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        let timeout = std::time::Duration::from_millis(
            step["timeoutMs"].as_u64().expect("validated setup timeout"),
        );
        let deadline = tokio::time::Instant::now() + timeout;
        // The step budget starts before observer admission, which can wait
        // behind durable journal I/O. A late observer must never dispatch a
        // setup write after the caller's deadline has already elapsed.
        let result = tokio::time::timeout_at(deadline, async {
        executor.check_recording_admission(state).await?;
        let mut observation = if let Some(response) = step.get("response") {
            let subscription = response["subscriptionIndex"]
                .as_u64()
                .expect("validated subscription index") as usize;
            let consumer = &state
                .consumers
                .iter()
                .find(|consumer| consumer.selector_index == subscription)
                .ok_or_else(|| {
                    failure(
                        "lifecycle.invalid-state",
                        "setup response subscription is not live",
                    )
                })?
                .consumer;
            let prefix = bytes(&response["prefix"])?;
            let matcher: RecordMatcher = Arc::new(move |record| {
                // Malformed internal bytes are a correlated boundary failure,
                // never silently ignored until timeout.
                record["valueB64"]
                    .as_str()
                    .and_then(|value| decode_base64(value, 512).ok())
                    .is_none_or(|value| value.starts_with(&prefix))
            });
            let session = state.session.as_ref().expect("owned session").clone();
            let consumer = consumer.clone();
            Some(if state.recording.is_some() {
                crate::continuation_journal::run_blocking_result(move || {
                    session.observe(&consumer, matcher)
                })
                .await?
            } else {
                session.observe(&consumer, matcher)?
            })
        } else {
            None
        };
        let mut selector = step["selector"].clone();
        for field in ["serviceOccurrence", "characteristicOccurrence"] {
            selector[field] = json!(selector[field].as_u64().expect("validated occurrence") - 1);
        }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(failure("operation.timed-out", &format!("setup step {index} expired before its write")));
            }
            let mut args = json!({"peerId":peer,"selector":selector,"valueB64":encode_base64(&bytes(&step["value"])?),"mode":"with-response","operationId":format!("continuation-setup-{index}"),"budgetMs":remaining.as_millis().max(1)});
            state.admission += 1;
            args["admission"] = json!(state.admission);
            invoke_with_deadline(state, "gatt.write", args, Some(deadline.into_std())).await?;
            if let Some(observation) = &mut observation {
                let record = (&mut observation.receiver).await.map_err(|_|failure("stream.closed", "setup acknowledgement observation closed"))?;
                if record["t"] == "stream-end" {
                    if let Some(error)=record.get("error") {return Err(error.clone());}
                    return Err(failure("stream.closed", &format!("setup acknowledgement stream ended: {}",record["reason"])));
                }
                let value = record["valueB64"].as_str().and_then(|value|decode_base64(value,512).ok()).ok_or_else(||failure("platform.failure", "malformed setup acknowledgement bytes"))?;
                let response = &step["response"];
                let min = response["minLength"].as_u64().expect("validated minimum") as usize;
                let max = response["maxLength"].as_u64().expect("validated maximum") as usize;
                if value.len()<min || value.len()>max {return Err(failure("platform.failure", "correlated setup acknowledgement has invalid length"));}
                let status = value[response["status"]["offset"].as_u64().expect("validated offset") as usize];
                if !response["status"]["accepted"].as_array().expect("validated statuses").contains(&json!(status)) {
                    return Err(failure("platform.failure", &format!("setup step {index} rejected with application status {status}")));
                }
                if let Some(trailing) = response.get("trailing") {
                    let offset = trailing["offset"].as_u64().expect("validated trailing offset") as usize;
                    if let Some(byte) = value.get(offset)
                        && !trailing["accepted"].as_array().expect("validated trailing values").contains(&json!(byte)) {
                            return Err(failure("platform.failure", &format!("setup step {index} rejected trailing acknowledgement byte {byte}")));
                    }
                }
            }
            Ok(())
        }).await.map_err(|_|failure("operation.timed-out", &format!("setup step {index} exceeded its command deadline")))?;
        result?;
    }
    Ok(())
}

fn cutoff_loss(value: &Value) -> Result<Value> {
    Ok(
        json!({"items":value["afterCutoffItems"].as_u64().ok_or_else(|| failure("platform.failure", "missing cutoff items"))?,
        "bytes":value["afterCutoffBytes"].as_u64().ok_or_else(|| failure("platform.failure", "missing cutoff bytes"))?}),
    )
}

async fn admitted(state: &mut State, op: &str, mut args: Value) -> Result<Value> {
    state.admission += 1;
    args["admission"] = json!(state.admission);
    invoke(state, op, args).await
}

async fn invoke(state: &State, op: &str, args: Value) -> Result<Value> {
    invoke_with_deadline(state, op, args, None).await
}

fn setup_write_budget(args: &mut Value, deadline: std::time::Instant) -> Result<()> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err(failure(
            "operation.timed-out",
            "setup write deadline elapsed before native dispatch",
        ));
    }
    // Round a sub-millisecond remainder up so the native call has a bounded,
    // nonzero budget; never reuse the original full step budget after admission.
    args["budgetMs"] = json!(remaining.as_millis().max(1));
    Ok(())
}

async fn invoke_with_deadline(
    state: &State,
    op: &str,
    mut args: Value,
    deadline: Option<std::time::Instant>,
) -> Result<Value> {
    let session = state
        .session
        .as_ref()
        .ok_or_else(|| failure("lifecycle.invalid-state", "no continuation session"))?;
    if session.collection_sealed()
        && matches!(
            op,
            "connection.connect"
                | "connection.request-mtu"
                | "gatt.discover"
                | "gatt.subscribe"
                | "gatt.write"
        )
    {
        return Err(failure(
            "lifecycle.invalid-state",
            "native collection was explicitly stopped",
        ));
    }
    let reply = if state.recording.is_some() {
        let session = session.clone();
        let op = op.to_owned();
        let runtime = tokio::runtime::Handle::current();
        crate::continuation_journal::run_blocking_result(move || {
            if let Some(deadline) = deadline {
                setup_write_budget(&mut args, deadline)?;
            }
            Ok(runtime.block_on(session.call(&op, &args.to_string())))
        })
        .await?
    } else {
        if let Some(deadline) = deadline {
            setup_write_budget(&mut args, deadline)?;
        }
        session.call(op, &args.to_string()).await
    };
    let envelope: Value = serde_json::from_str(&reply)
        .map_err(|_| failure("platform.failure", "invalid core reply"))?;
    if envelope["ok"] == true && envelope.get("value").is_some() {
        Ok(envelope["value"].clone())
    } else if envelope["ok"] == false && envelope["error"]["code"].is_string() {
        let mut error = envelope["error"].clone();
        // Preserve the operation's decision. A code alone cannot authorize a
        // reconnect; adapters that omit retryability are conservatively never.
        error["retryability"] = match envelope["retryability"].as_str() {
            Some("caller-decides") => json!("caller-decides"),
            _ => json!("never"),
        };
        Err(error)
    } else {
        Err(failure(
            "platform.failure",
            "malformed continuation operation envelope",
        ))
    }
}

#[cfg(test)]
mod idle_recovery_tests {
    use super::*;

    #[test]
    fn setup_uncertainty_survives_other_peers_and_only_its_generation_change_clears_it() {
        let mut failures = SetupFailures::default();
        let generation = json!(["connection-1", "database-1"]);
        let original = failure("operation.timed-out", "original STOP deadline");
        failures.check("peer-a", &generation).unwrap();
        failures.record("peer-a", generation.clone(), original.clone());
        failures.check("peer-b", &generation).unwrap();
        failures.record(
            "peer-b",
            generation.clone(),
            failure("platform.failure", "B"),
        );
        assert_eq!(failures.check("peer-a", &generation), Err(original.clone()));
        failures
            .check("peer-b", &json!(["connection-2", "database-2"]))
            .unwrap();
        assert_eq!(failures.check("peer-a", &generation), Err(original));
        failures
            .check("peer-a", &json!(["connection-2", "database-2"]))
            .unwrap();
        assert!(failures.peers.is_empty());
    }

    #[test]
    fn setup_uncertainty_capacity_never_evicts_a_live_generation() {
        let mut failures = SetupFailures::default();
        let generation = json!(["connection-1", "database-1"]);
        let original = failure("operation.timed-out", "original STOP deadline");
        for index in 0..MAX_SETUP_FAILURE_PEERS {
            let peer = format!("peer-{index}");
            failures.check(&peer, &generation).unwrap();
            failures.record(&peer, generation.clone(), original.clone());
        }
        assert_eq!(
            failures.check("new-peer", &generation).unwrap_err()["code"],
            "lifecycle.invalid-state"
        );
        assert_eq!(failures.check("peer-0", &generation), Err(original));
        failures
            .check("peer-0", &json!(["connection-2", "database-2"]))
            .unwrap();
        failures.check("new-peer", &generation).unwrap();
        assert_eq!(failures.peers.len(), MAX_SETUP_FAILURE_PEERS - 1);
    }

    #[derive(Default)]
    struct Host(Arc<Session>);
    #[derive(Default)]
    struct Session {
        release: AtomicBool,
        hold_connect: AtomicBool,
        connect_entered: tokio::sync::Notify,
        connected: tokio::sync::Notify,
    }

    impl ContinuationHost for Host {
        fn open_session(&self) -> Result<Arc<dyn ContinuationSession>> {
            Ok(self.0.clone())
        }
    }
    impl ContinuationSession for Session {
        fn legacy_claim_compatible(&self) -> bool {
            true
        }
        fn call<'a>(&'a self, op: &'a str, _: &'a str) -> ContinuationFuture<'a> {
            Box::pin(async move {
                if op == "connection.connect" && self.hold_connect.load(Ordering::SeqCst) {
                    self.connect_entered.notify_one();
                    self.connected.notified().await;
                }
                envelope(Ok(match op {
                    "session.quiesce" => {
                        json!({"state":"sealed","afterCutoffItems":0,"afterCutoffBytes":0})
                    }
                    "session.continuation-dispose" => {
                        json!({"state":if self.release.load(Ordering::SeqCst) {"released"} else {"release-failed"},"afterCutoffItems":0,"afterCutoffBytes":0})
                    }
                    "counters.describe" => json!({"queuedData":1,"lastError":null}),
                    "session.reconcile" => json!({"links":[],"subscriptions":[]}),
                    _ => json!({}),
                }))
            })
        }
        fn claim_drain(&self, _: u32, _: u32) -> Result<Option<DrainClaim>> {
            Ok(None)
        }
        fn has_retained_tail(&self) -> Result<bool> {
            Ok(false)
        }
        fn acknowledge_drain(&self, _: u64) -> Result<()> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn full_receipt_ledger_refuses_before_sealing_and_preserves_receipts() {
        let engine = NativeContinuation::new(Arc::new(Host::default()));
        engine
            .execute("peer", r#"{"onAppearance":"native"}"#)
            .await
            .unwrap();
        {
            let mut state = engine.state.lock().await;
            for index in 0..MAX_CLAIM_RECEIPTS {
                state.receipts.insert(
                    format!("receipt-{index}"),
                    (ClaimMode::RetainedV1, json!({"index":index})),
                );
            }
        }
        assert_eq!(
            engine.prepare_claim(1, 1024).await.unwrap_err()["code"],
            "lifecycle.invalid-state"
        );
        assert!(!engine.state.lock().await.sealed);
        assert_eq!(
            engine.acknowledge_claim_v1("receipt-0").await.unwrap(),
            json!({"index":0})
        );
        engine.retire_claim_receipt("receipt-0").await.unwrap();
        assert!(engine.prepare_claim(1, 1024).await.is_ok());
    }

    #[test]
    fn superseding_a_legacy_receipt_reclaims_only_its_empty_evidence_owner() {
        let mut state = State::default();
        let prior = "legacy-prior".to_owned();
        let current = "legacy-current".to_owned();
        state
            .legacy_evidence
            .insert(prior.clone(), Arc::new(LegacyEvidenceOwner::new(1, 1024)));
        state
            .receipts
            .insert(prior.clone(), (ClaimMode::Legacy, json!({"disposed":true})));
        state
            .legacy_evidence
            .insert(current.clone(), Arc::new(LegacyEvidenceOwner::new(1, 1024)));
        state.prepared = Some(Prepared {
            mode: ClaimMode::Legacy,
            claim: json!({"claimToken":current}),
            complete: true,
            acknowledged: true,
            receipt: Some(json!({"disposed":true})),
            source_claim: None,
        });

        state.archive_prepared_receipt();

        assert!(
            !state.legacy_evidence.contains_key(&prior),
            "a superseded empty Legacy receipt cannot keep an owner forever"
        );
        assert!(
            state.legacy_evidence.contains_key(&current),
            "the current receipt still needs its empty-page owner until it is superseded"
        );
    }

    #[tokio::test]
    async fn retiring_an_empty_v1_receipt_reclaims_its_evidence_owner() {
        let engine = NativeContinuation::new(Arc::new(Host::default()));
        let token = "continuation-test-1-1-v1-deadbeef".to_owned();
        {
            let mut state = engine.state.lock().await;
            state
                .legacy_evidence
                .insert(token.clone(), Arc::new(LegacyEvidenceOwner::new(1, 1024)));
            state.prepared = Some(Prepared {
                mode: ClaimMode::RetainedV1,
                claim: json!({"claimToken":token}),
                complete: true,
                acknowledged: true,
                receipt: Some(json!({"disposed":true})),
                source_claim: None,
            });
        }

        engine.retire_claim_receipt(&token).await.unwrap();

        assert!(
            !engine
                .state
                .lock()
                .await
                .legacy_evidence
                .contains_key(&token),
            "an empty V1 owner has no ordinal that could otherwise release it"
        );
    }

    #[tokio::test]
    async fn a_released_empty_legacy_receipt_is_reclaimed_only_when_superseded() {
        let engine = NativeContinuation::new(Arc::new(Host::default()));
        let token = "legacy-empty-current".to_owned();
        {
            let mut state = engine.state.lock().await;
            state
                .legacy_evidence
                .insert(token.clone(), Arc::new(LegacyEvidenceOwner::new(1, 1024)));
            state.prepared = Some(Prepared {
                mode: ClaimMode::Legacy,
                claim: json!({"claimToken":token}),
                complete: true,
                acknowledged: true,
                receipt: Some(json!({"disposed":true})),
                source_claim: None,
            });
        }

        assert!(engine.retire_claim_receipt(&token).await.is_err());

        assert!(
            engine
                .state
                .lock()
                .await
                .legacy_evidence
                .contains_key(&token),
            "the current Legacy receipt remains replayable until superseded"
        );

        {
            let mut state = engine.state.lock().await;
            // Production preparation archives the completed current handoff
            // before it installs a successor.  Do the same here: replacing
            // `prepared` directly would lose the supersession transition this
            // regression is meant to exercise.
            state.archive_prepared_receipt();
            state.prepared = Some(Prepared {
                mode: ClaimMode::Legacy,
                claim: json!({"claimToken":"legacy-empty-successor"}),
                complete: true,
                acknowledged: true,
                receipt: Some(json!({"disposed":true})),
                source_claim: None,
            });
            state.archive_prepared_receipt();
        }
        assert!(
            !engine
                .state
                .lock()
                .await
                .legacy_evidence
                .contains_key(&token),
            "supersession releases the old empty Legacy owner without discarding its current receipt early"
        );
    }

    #[test]
    fn empty_legacy_receipt_supersession_stays_bounded_across_many_generations() {
        let mut state = State::default();
        for sequence in 1..=MAX_CLAIM_RECEIPTS + 4 {
            let token = format!("legacy-empty-{sequence}");
            state
                .legacy_evidence
                .insert(token.clone(), Arc::new(LegacyEvidenceOwner::new(1, 1024)));
            state.prepared = Some(Prepared {
                mode: ClaimMode::Legacy,
                claim: json!({"claimToken":token}),
                complete: true,
                acknowledged: true,
                receipt: Some(json!({"disposed":true})),
                source_claim: None,
            });
            state.archive_prepared_receipt();
            assert!(
                state.legacy_evidence.len() <= 1,
                "a Legacy empty owner must be reclaimed as soon as its receipt is superseded"
            );
        }
    }

    #[tokio::test]
    async fn empty_v1_receipt_retirement_stays_bounded_across_many_generations() {
        let engine = NativeContinuation::new(Arc::new(Host::default()));
        for sequence in 1..=MAX_CLAIM_RECEIPTS + 4 {
            let token = format!("continuation-test-1-{sequence}-v1-deadbeef");
            {
                let mut state = engine.state.lock().await;
                state
                    .legacy_evidence
                    .insert(token.clone(), Arc::new(LegacyEvidenceOwner::new(1, 1024)));
                state.prepared = Some(Prepared {
                    mode: ClaimMode::RetainedV1,
                    claim: json!({"claimToken":token}),
                    complete: true,
                    acknowledged: true,
                    receipt: Some(json!({"disposed":true})),
                    source_claim: None,
                });
            }
            engine.retire_claim_receipt(&token).await.unwrap();
            assert!(
                engine.state.lock().await.legacy_evidence.is_empty(),
                "an empty V1 owner must not accumulate after receipt retirement"
            );
        }
    }

    #[tokio::test]
    async fn backlog_status_joins_autonomous_recovery_but_explicit_execution_remains_busy() {
        use std::{future::Future, task::Poll};
        let host = Arc::new(Host::default());
        host.0.release.store(true, Ordering::SeqCst);
        let engine = NativeContinuation::new(host.clone());
        engine
            .execute("peer", r#"{"onAppearance":"native"}"#)
            .await
            .unwrap();
        host.0.hold_connect.store(true, Ordering::SeqCst);
        engine.request_recovery(&tokio::runtime::Handle::current());
        host.0.connect_entered.notified().await;
        let status = engine.describe_backlog();
        tokio::pin!(status);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(status.as_mut().poll(cx).is_pending())).await,
            "autonomous recovery must not make retained backlog status a timing-dependent busy refusal"
        );
        engine.stop_recovery();
        host.0.connected.notify_one();
        assert_eq!(status.await.unwrap()["queuedData"], 1);
        engine.await_stopped_recovery().await;
        let explicit = engine.state.lock().await;
        assert_eq!(
            engine.describe_backlog().await.unwrap_err()["code"],
            "lifecycle.invalid-state"
        );
        drop(explicit);
        let prepared = engine.prepare_claim(8, 1024).await.unwrap();
        assert_eq!(
            engine
                .acknowledge_claim(prepared["claimToken"].as_str().unwrap())
                .await
                .unwrap()["disposed"],
            true
        );
    }

    #[tokio::test]
    async fn idle_adapter_event_cannot_compete_with_first_wake_admission() {
        let engine = NativeContinuation::new(Arc::new(Host::default()));
        // Deterministically hold the idle inspection point. A power-on event
        // must not enqueue a supervisor that will acquire this admission lock
        // ahead of the first genuine restoration wake.
        let idle = engine.state.lock().await;
        engine.request_recovery(&tokio::runtime::Handle::current());
        assert!(
            !engine.recovering.load(Ordering::SeqCst),
            "no owned session means no recovery worker"
        );
        assert!(!engine.recovery_requested.load(Ordering::SeqCst));
        drop(idle);
        let declaration = r#"{"onAppearance":"native"}"#;
        engine.seed_declaration(declaration).unwrap();
        assert_eq!(
            engine.execute("peer", declaration).await.unwrap()["event"],
            "continuation.completed"
        );
        // Actual live admission still fails promptly rather than being hidden
        // by an asynchronous retry in the public/native wake boundary.
        let active = engine.state.lock().await;
        assert_eq!(
            engine.seed_declaration(declaration).unwrap_err()["code"],
            "lifecycle.invalid-state"
        );
        drop(active);
        engine.stop_recovery();
    }

    #[tokio::test]
    async fn recovery_ownership_survives_held_install_and_failed_disposal() {
        use std::{future::Future, task::Poll};
        let host = Arc::new(Host::default());
        host.0.hold_connect.store(true, Ordering::SeqCst);
        let engine = NativeContinuation::new(host.clone());
        let execute = engine.execute("peer", r#"{"onAppearance":"native"}"#);
        tokio::pin!(execute);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(execute.as_mut().poll(cx).is_pending())).await
        );
        assert!(engine.session_owned.load(Ordering::SeqCst));
        engine.request_recovery(&tokio::runtime::Handle::current());
        assert!(
            engine.recovering.load(Ordering::SeqCst),
            "event during accepted execution remains queued"
        );
        // Stop this test's queued worker before draining; the obligation flag
        // must remain true independently of the retry-policy stop flag.
        engine.stop_recovery();
        host.0.connected.notify_one();
        execute.await.unwrap();
        let claim = engine.prepare_claim(8, 1024).await.unwrap();
        let token = claim["claimToken"].as_str().unwrap();
        assert_eq!(
            engine.acknowledge_claim(token).await.unwrap()["disposed"],
            false
        );
        assert!(engine.session_owned.load(Ordering::SeqCst));
        assert!(engine.state.lock().await.session.is_some());
        host.0.release.store(true, Ordering::SeqCst);
        assert_eq!(
            engine.acknowledge_claim(token).await.unwrap()["disposed"],
            true
        );
        assert!(!engine.session_owned.load(Ordering::SeqCst));
        assert!(engine.state.lock().await.session.is_none());
    }
}
