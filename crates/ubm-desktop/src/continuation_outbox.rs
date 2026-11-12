//! Per-session drain queues and wake arming.
//!
//! Two bounded queues share one session-monotonic ordinal: data (`adv`,
//! `value`) bounded by items and bytes, control (everything else) bounded
//! by items. Drain merges them by ordinal, so causality holds (a value
//! that arrived before a link loss drains before it), and a full data
//! queue never pushes out a control record. Reserved terminal controls share
//! that ordering but have a separate bounded quota, retained through receiver
//! acknowledgement in claimed mode.
//!
//! Wake arming (no lost wakeups): `armed` means JavaScript drained to
//! empty and waits. A producer that makes the session non-empty while
//! armed disarms and wakes exactly once. A drain that empties the queues
//! arms, then re-checks: a record that raced in after the take disarms
//! again and the drain answers `more: true`.

use crate::continuation_journal::{
    APPEND_BATCH_MAX, AppendBatch, ContinuationJournal, JournalError,
    ScanFailureRecordingRecoveryAuthority,
};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};
use ubm_core::oversized_evidence::{
    EvidenceChunk, EvidenceLimits, EvidenceSource, EvidenceToken, MAX_CHUNK_BYTES, RecoveryError,
    SCAN_FAILURE_REPRESENTATION, ScanFailureDescriptor, ScanFailureIdentity, ScanFailureLimits,
    ScanFailureMember, ScanFailureOccurrence, ScanFailurePreparedSnapshot,
    ScanFailureReceiptSnapshot,
};

fn scan_owner_envelope(slot: &ScanFailureSlot) -> Result<Vec<u8>, JournalError> {
    let identity = slot
        .occurrence_identity
        .ok_or_else(|| JournalError::invalid("scan owner has no identity"))?;
    let receipts = slot
        .occurrence
        .as_ref()
        .and_then(ScanFailureOccurrence::prepared_snapshot)
        .map(|snapshot| {
            serde_json::json!({
                "encodedLength": snapshot.encoded_length,
                "chunkBytes": snapshot.chunk_bytes,
                "receipts": snapshot.receipts.into_iter().map(|receipt| serde_json::json!({
                    "issued": encode_base64(&receipt.issued), "remaining": receipt.remaining,
                    "acknowledged": receipt.acknowledged, "retired": receipt.retired,
                })).collect::<Vec<_>>(),
            })
        });
    serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "sessionId": slot.session_id,
        "identity": [identity.host, identity.scan, identity.start, identity.source_generation, identity.occurrence],
        "ordinal": slot.ordinal,
        "capability": encode_base64(&slot.capability), "cleanupKey": encode_base64(&slot.cleanup_key),
        "recoveryAuthority": slot.recovery_authority.map(|authority| encode_base64(&authority)),
        "limits": {"maxEncodedBytes": slot.limits.evidence.max_encoded_bytes, "maxDepth": slot.limits.evidence.max_depth,
          "maxTokens": slot.limits.evidence.max_tokens, "maxChunks": slot.limits.evidence.max_chunks,
          "chunkBytes": slot.limits.evidence.chunk_bytes, "maxRecipients": slot.limits.max_recipients,
          "maxBookkeepingBytes": slot.limits.max_bookkeeping_bytes},
        "budget": slot.budget, "sourceBudget": slot.source_budget, "sourcePreAdmitted": slot.source_pre_admitted,
        "attached": slot.attached, "prepared": slot.prepared, "producerFinished": slot.producer_finished,
        "rawRetired": slot.raw_retired, "rawReleased": slot.raw_released,
        "released": slot.released.map(|head| serde_json::json!({
            "protocol": head.evidence.protocol,
            "representation": head.evidence.representation,
            "identity": [head.evidence.identity.host, head.evidence.identity.scan, head.evidence.identity.start, head.evidence.identity.source_generation, head.evidence.identity.occurrence],
            "member": {"recipient": head.evidence.member.recipient, "delivery": head.evidence.member.delivery},
            "encodedLength": head.evidence.encoded_length,
            "chunkBytes": head.evidence.chunk_bytes,
        })),
        "preparedState": receipts,
    })).map_err(|_| JournalError::invalid("scan owner envelope could not encode"))
}

fn persist_scan_owner(
    durable: &ContinuationJournal,
    slot: &ScanFailureSlot,
    previous: Option<&[u8]>,
) -> Result<(), JournalError> {
    let identity = slot
        .occurrence_identity
        .ok_or_else(|| JournalError::invalid("scan owner has no identity"))?;
    let next = scan_owner_envelope(slot)?;
    match previous {
        Some(previous) => durable.advance_scan_failure_owner_envelope(identity, previous, &next),
        None => durable.store_scan_failure_owner_envelope(identity, &next),
    }
}

fn persist_scan_owner_successor(
    queues: &Queues,
    index: usize,
    successor: &ScanFailureSlot,
) -> Result<(), JournalError> {
    let Some(durable) = queues.durable.as_ref() else {
        return Ok(());
    };
    let identity = queues.scan_failures[index]
        .occurrence_identity
        .ok_or_else(|| JournalError::invalid("scan owner has no identity"))?;
    let previous = durable
        .journal
        .scan_failure_owner_envelope(identity)?
        .ok_or_else(|| JournalError::invalid("scan owner envelope missing"))?;
    persist_scan_owner(&durable.journal, successor, Some(&previous))
}

fn owner_value<'a>(
    object: &'a serde_json::Map<String, Value>,
    name: &str,
) -> Result<&'a Value, JournalError> {
    object
        .get(name)
        .ok_or_else(|| JournalError::invalid("scan owner envelope is incomplete"))
}

fn owner_u64(object: &serde_json::Map<String, Value>, name: &str) -> Result<u64, JournalError> {
    owner_value(object, name)?
        .as_u64()
        .ok_or_else(|| JournalError::invalid("scan owner envelope has invalid number"))
}

fn owner_bool(object: &serde_json::Map<String, Value>, name: &str) -> Result<bool, JournalError> {
    owner_value(object, name)?
        .as_bool()
        .ok_or_else(|| JournalError::invalid("scan owner envelope has invalid boolean"))
}

fn owner_bytes(value: &Value, width: usize) -> Result<Vec<u8>, JournalError> {
    let values = value
        .as_array()
        .ok_or_else(|| JournalError::invalid("scan owner envelope has invalid bytes"))?;
    if values.len() != width {
        return Err(JournalError::invalid(
            "scan owner envelope has invalid byte width",
        ));
    }
    values
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(|| JournalError::invalid("scan owner envelope has invalid byte"))
        })
        .collect()
}

fn owner_fixed<const N: usize>(value: &Value) -> Result<[u8; N], JournalError> {
    let bytes = owner_bytes(value, N)?;
    let mut result = [0; N];
    result.copy_from_slice(&bytes);
    Ok(result)
}

fn restore_scan_owner(
    body: &[u8],
    raw: crate::continuation_journal::DurableScanFailureRawCut,
) -> Result<ScanFailureSlot, JournalError> {
    let object = serde_json::from_slice::<Value>(body)
        .map_err(|_| JournalError::invalid("scan owner envelope is not JSON"))?
        .as_object()
        .cloned()
        .ok_or_else(|| JournalError::invalid("scan owner envelope is not an object"))?;
    if owner_u64(&object, "version")? != 1 {
        return Err(JournalError::invalid(
            "scan owner envelope version is not supported",
        ));
    }
    let identity_parts = owner_value(&object, "identity")?
        .as_array()
        .ok_or_else(|| JournalError::invalid("scan owner identity is invalid"))?;
    if identity_parts.len() != 5 {
        return Err(JournalError::invalid(
            "scan owner identity width is invalid",
        ));
    }
    let identity = ScanFailureIdentity {
        host: owner_fixed(&identity_parts[0])?,
        scan: owner_fixed(&identity_parts[1])?,
        start: owner_fixed(&identity_parts[2])?,
        source_generation: owner_fixed(&identity_parts[3])?,
        occurrence: owner_fixed(&identity_parts[4])?,
    };
    if identity != raw.identity {
        return Err(JournalError::invalid(
            "scan owner identity does not match raw source",
        ));
    }
    let decode = |name: &str, width: usize| -> Result<Vec<u8>, JournalError> {
        let text = owner_value(&object, name)?
            .as_str()
            .ok_or_else(|| JournalError::invalid("scan owner envelope has invalid secret"))?;
        let bytes = decode_base64(text, width)
            .map_err(|_| JournalError::invalid("scan owner envelope has invalid secret"))?;
        if bytes.len() != width {
            return Err(JournalError::invalid(
                "scan owner envelope secret width is invalid",
            ));
        }
        Ok(bytes)
    };
    let session_id = owner_u64(&object, "sessionId")?;
    if session_id == 0 {
        return Err(JournalError::invalid(
            "scan owner session identity is invalid",
        ));
    }
    let capability: [u8; 16] = decode("capability", 16)?
        .try_into()
        .map_err(|_| JournalError::invalid("scan owner capability width is invalid"))?;
    let cleanup_key: [u8; 32] = decode("cleanupKey", 32)?
        .try_into()
        .map_err(|_| JournalError::invalid("scan owner cleanup key width is invalid"))?;
    let recovery_authority = match owner_value(&object, "recoveryAuthority")? {
        Value::Null => None,
        Value::String(_) => Some(decode("recoveryAuthority", 32)?.try_into().map_err(|_| {
            JournalError::invalid("scan owner recovery authority width is invalid")
        })?),
        _ => {
            return Err(JournalError::invalid(
                "scan owner recovery authority is invalid",
            ));
        }
    };
    let limits = owner_value(&object, "limits")?
        .as_object()
        .ok_or_else(|| JournalError::invalid("scan owner limits are invalid"))?;
    let usize_limit = |name: &str| {
        usize::try_from(owner_u64(limits, name)?)
            .map_err(|_| JournalError::invalid("scan owner limit overflows"))
    };
    let evidence = EvidenceLimits {
        max_encoded_bytes: owner_u64(limits, "maxEncodedBytes")?,
        max_depth: usize_limit("maxDepth")?,
        max_tokens: usize_limit("maxTokens")?,
        max_chunks: owner_u64(limits, "maxChunks")?,
        chunk_bytes: u32::try_from(owner_u64(limits, "chunkBytes")?)
            .map_err(|_| JournalError::invalid("scan owner chunk width is invalid"))?,
    };
    let scan_limits = ScanFailureLimits {
        evidence,
        max_recipients: usize_limit("maxRecipients")?,
        max_bookkeeping_bytes: usize_limit("maxBookkeepingBytes")?,
    };
    let prepared = owner_bool(&object, "prepared")?;
    let raw_retired = owner_bool(&object, "rawRetired")?;
    let raw_released = owner_bool(&object, "rawReleased")?;
    let producer_finished = owner_bool(&object, "producerFinished")?;
    let released = match owner_value(&object, "released")? {
        Value::Null => None,
        Value::Object(released) => {
            let member = owner_value(released, "member")?
                .as_object()
                .ok_or_else(|| JournalError::invalid("scan owner release member is invalid"))?;
            let released_identity = owner_value(released, "identity")?
                .as_array()
                .ok_or_else(|| JournalError::invalid("scan owner release identity is invalid"))?;
            if released_identity.len() != 5 {
                return Err(JournalError::invalid(
                    "scan owner release identity width is invalid",
                ));
            }
            let representation = owner_value(released, "representation")?
                .as_str()
                .ok_or_else(|| {
                    JournalError::invalid("scan owner release representation is invalid")
                })?;
            if representation != SCAN_FAILURE_REPRESENTATION {
                return Err(JournalError::invalid(
                    "scan owner release representation is invalid",
                ));
            }
            let released_scan_identity = ScanFailureIdentity {
                host: owner_fixed(&released_identity[0])?,
                scan: owner_fixed(&released_identity[1])?,
                start: owner_fixed(&released_identity[2])?,
                source_generation: owner_fixed(&released_identity[3])?,
                occurrence: owner_fixed(&released_identity[4])?,
            };
            if released_scan_identity != identity {
                return Err(JournalError::invalid(
                    "scan owner release identity does not match source",
                ));
            }
            Some(ScanFailureHead {
                ordinal: owner_u64(&object, "ordinal")?,
                capability,
                evidence: ScanFailureDescriptor {
                    protocol: u8::try_from(owner_u64(released, "protocol")?).map_err(|_| {
                        JournalError::invalid("scan owner release protocol is invalid")
                    })?,
                    representation: SCAN_FAILURE_REPRESENTATION,
                    identity: released_scan_identity,
                    member: ScanFailureMember {
                        recipient: owner_fixed(owner_value(member, "recipient")?)?,
                        delivery: owner_fixed(owner_value(member, "delivery")?)?,
                    },
                    encoded_length: owner_u64(released, "encodedLength")?,
                    chunk_bytes: u32::try_from(owner_u64(released, "chunkBytes")?).map_err(
                        |_| JournalError::invalid("scan owner release chunk width is invalid"),
                    )?,
                },
                contiguous_read: owner_u64(released, "encodedLength")?,
                acknowledged: true,
            })
        }
        _ => return Err(JournalError::invalid("scan owner release is invalid")),
    };
    if raw_released && !raw_retired {
        return Err(JournalError::invalid(
            "scan owner raw release is inconsistent",
        ));
    }
    let mut occurrence = Some(ScanFailureOccurrence::new(
        identity,
        raw.members,
        Arc::new(EvidenceSource::new(raw.tokens)),
    ));
    if prepared && released.is_none() {
        let state = owner_value(&object, "preparedState")?
            .as_object()
            .ok_or_else(|| JournalError::invalid("prepared scan owner has no state"))?;
        let receipts = owner_value(state, "receipts")?
            .as_array()
            .ok_or_else(|| JournalError::invalid("prepared scan owner receipts are invalid"))?
            .iter()
            .map(|value| {
                let receipt = value
                    .as_object()
                    .ok_or_else(|| JournalError::invalid("prepared scan receipt is invalid"))?;
                Ok(ScanFailureReceiptSnapshot {
                    issued: decode_base64(
                        owner_value(receipt, "issued")?.as_str().ok_or_else(|| {
                            JournalError::invalid("prepared scan receipt is invalid")
                        })?,
                        evidence.max_chunks as usize,
                    )
                    .map_err(|_| JournalError::invalid("prepared scan receipt is invalid"))?,
                    remaining: owner_u64(receipt, "remaining")?,
                    acknowledged: owner_bool(receipt, "acknowledged")?,
                    retired: owner_bool(receipt, "retired")?,
                })
            })
            .collect::<Result<Vec<_>, JournalError>>()?;
        occurrence
            .as_mut()
            .expect("new occurrence")
            .restore_prepared_snapshot(
                scan_limits,
                &ScanFailurePreparedSnapshot {
                    encoded_length: owner_u64(state, "encodedLength")?,
                    chunk_bytes: u32::try_from(owner_u64(state, "chunkBytes")?).map_err(|_| {
                        JournalError::invalid("prepared scan chunk width is invalid")
                    })?,
                    receipts,
                },
            )
            .map_err(|_| JournalError::invalid("prepared scan owner does not match raw source"))?;
    } else if !prepared && owner_value(&object, "preparedState")? != &Value::Null {
        return Err(JournalError::invalid(
            "unprepared scan owner has prepared state",
        ));
    }
    if raw_released || released.is_some() {
        occurrence = None;
    }
    let ordinal = owner_u64(&object, "ordinal")?;
    if ordinal == 0 {
        return Err(JournalError::invalid("scan owner ordinal is invalid"));
    }
    Ok(ScanFailureSlot {
        identity: Arc::new(()),
        session_id,
        capability,
        cleanup_key,
        recovery_authority,
        limits: scan_limits,
        budget: owner_u64(&object, "budget")?,
        source_budget: owner_u64(&object, "sourceBudget")?,
        source_pre_admitted: owner_bool(&object, "sourcePreAdmitted")?,
        ordinal: Some(ordinal),
        occurrence,
        occurrence_identity: Some(identity),
        attached: false,
        prepared,
        producer_finished,
        released,
        raw_retired,
        raw_released,
    })
}

/// One bounded, non-destructive ingress observation. Matching is evaluated
/// only after queue admission, so an acknowledgement never hides data loss.
pub type RecordMatcher = Arc<dyn Fn(&Value) -> bool + Send + Sync>;
struct Observer {
    identity: Arc<()>,
    consumer: String,
    matcher: RecordMatcher,
    sender: tokio::sync::oneshot::Sender<Value>,
}
pub struct Observation {
    pub receiver: tokio::sync::oneshot::Receiver<Value>,
    identity: Arc<()>,
    slot: Arc<Mutex<Option<Observer>>>,
}
impl Drop for Observation {
    fn drop(&mut self) {
        let mut slot = lock(&self.slot);
        if slot
            .as_ref()
            .is_some_and(|observer| Arc::ptr_eq(&observer.identity, &self.identity))
        {
            slot.take();
        }
    }
}

/// Private wire encoding, shared by native continuation hosts.
#[must_use]
pub fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).map_or(0, |b| u32::from(*b));
        let b2 = chunk.get(2).map_or(0, |b| u32::from(*b));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        let sextet = |shift: u32| char::from(ALPHABET[((triple >> shift) & 0x3f) as usize]);
        out.push(sextet(18));
        out.push(sextet(12));
        out.push(if chunk.len() > 1 { sextet(6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(0) } else { '=' });
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum Base64Error {
    Invalid,
    TooLarge,
}

/// Strict RFC 4648 padded decoding shared by both native adapters.
pub fn decode_base64(text: &str, max_bytes: usize) -> Result<Vec<u8>, Base64Error> {
    let input = text.as_bytes();
    if input.len() > 4 * max_bytes.div_ceil(3) {
        return Err(Base64Error::TooLarge);
    }
    if !input.len().is_multiple_of(4) {
        return Err(Base64Error::Invalid);
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let padding = match (input[input.len() - 2], input[input.len() - 1]) {
        (b'=', b'=') => 2,
        (_, b'=') => 1,
        _ => 0,
    };
    let size = input.len() / 4 * 3 - padding;
    if size > max_bytes {
        return Err(Base64Error::TooLarge);
    }
    let mut output = Vec::with_capacity(size);
    for (index, chunk) in input.as_chunks::<4>().0.iter().enumerate() {
        let pad = if index == input.len() / 4 - 1 {
            padding
        } else {
            0
        };
        let mut value = 0u32;
        for (position, byte) in chunk.iter().enumerate() {
            let digit = if position >= 4 - pad {
                0
            } else {
                match byte {
                    b'A'..=b'Z' => u32::from(byte - b'A'),
                    b'a'..=b'z' => u32::from(byte - b'a') + 26,
                    b'0'..=b'9' => u32::from(byte - b'0') + 52,
                    b'+' => 62,
                    b'/' => 63,
                    _ => return Err(Base64Error::Invalid),
                }
            };
            value = (value << 6) | digit;
        }
        if (pad == 1 && value & 0xff != 0) || (pad == 2 && value & 0xffff != 0) {
            return Err(Base64Error::Invalid);
        }
        output.push((value >> 16) as u8);
        if pad < 2 {
            output.push((value >> 8) as u8);
        }
        if pad == 0 {
            output.push(value as u8);
        }
    }
    Ok(output)
}

use std::sync::{MutexGuard, PoisonError};

/// Wake delivery belongs to the host, not the queue.
pub trait WakeSink: Send + Sync + 'static {
    fn wake(&self, session_id: u64);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IngressClass {
    Advertisement,
    Notification,
    Control,
}

impl IngressClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Advertisement => "advertisement",
            Self::Notification => "notification",
            Self::Control => "control",
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn object(entries: Vec<(&str, Value)>) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

/// Data records (`adv`, `value`) queued per session.
pub const DATA_RECORD_CAP: usize = 2048;
/// Encoded bytes of queued data records per session.
pub const DATA_RECORD_BYTES: usize = 4 << 20;
/// Ordinary control records queued per session. Past this, ordinary controls
/// are counted and reported as one `ingress-drop{class:"control"}`. Reserved
/// terminals have their own quota and do not consume this capacity.
pub const CONTROL_RECORD_CAP: usize = 1024;
/// Maximum simultaneous reserved or retained terminal controls per outbox.
pub const RESERVED_CONTROL_CAP: usize = 64;
/// Maximum serialized size of a reserved terminal, before its ordinal is added.
pub const RESERVED_CONTROL_BYTES: usize = 64 * 1024;

/// Scan owners and their receipt tombstones share this per-outbox quota.
pub const SCAN_FAILURE_OWNER_CAP: usize = 64;
/// Sum of pre-capture encoding reservations and retained reader bookkeeping,
/// excluding allocator overhead. Captured source capacity has its own bound.
pub const SCAN_FAILURE_BUDGET_BYTES: u64 = 64 * 1024 * 1024;
/// Aggregate reserved or captured source and member allocation capacities. Allocator
/// overhead and output chunks are separate; producer-held copies are external.
pub const SCAN_FAILURE_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum ScanFailureOutboxError {
    Sealed,
    Capacity,
    InvalidReservation,
    OrdinalExhausted,
    IdentityUnavailable,
    NotHead,
    Recovery(RecoveryError),
    Durability(JournalError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanFailureHead {
    pub ordinal: u64,
    /// Random per-reservation capability; transports must protect it.
    pub capability: [u8; 16],
    pub evidence: ScanFailureDescriptor,
    /// Exact persisted receipt status at the time this head was minted.
    pub contiguous_read: u64,
    pub acknowledged: bool,
}

/// Reserves one owner and its declared logical budget before scan ingress.
/// Keep the outbox alive while captured owners exist, including after seal.
pub struct ScanFailureReservation {
    queues: Weak<Mutex<Queues>>,
    identity: Option<Arc<()>>,
}

impl Drop for ScanFailureReservation {
    fn drop(&mut self) {
        if let (Some(queues), Some(identity)) = (self.queues.upgrade(), self.identity.take()) {
            let mut queues = lock(&queues);
            if let Some(index) = queues
                .scan_failures
                .iter()
                .position(|slot| Arc::ptr_eq(&slot.identity, &identity))
            {
                if queues.scan_failures[index].ordinal.is_some() {
                    queues.scan_failures[index].attached = false;
                } else {
                    queues.scan_failures.remove(index);
                }
            }
        }
    }
}

#[derive(Clone)]
struct ScanFailureSlot {
    identity: Arc<()>,
    session_id: u64,
    capability: [u8; 16],
    cleanup_key: [u8; 32],
    /// Opaque, session-owner capability persisted only in the private v2
    /// sidecar. It authenticates recovery; it is never rendered into exports.
    recovery_authority: Option<[u8; 32]>,
    limits: ScanFailureLimits,
    budget: u64,
    source_budget: u64,
    source_pre_admitted: bool,
    ordinal: Option<u64>,
    occurrence: Option<ScanFailureOccurrence>,
    occurrence_identity: Option<ScanFailureIdentity>,
    attached: bool,
    prepared: bool,
    producer_finished: bool,
    released: Option<ScanFailureHead>,
    raw_retired: bool,
    raw_released: bool,
}

/// Capability for reattaching a canceled pending capture. Protect it as a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingScanFailure {
    pub ordinal: u64,
    pub capability: [u8; 16],
}

/// Recovery inventory for a captured owner. Its capability and membership are
/// secret: a transport must authenticate the outbox owner before returning it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFailureRecoveryOwner {
    pub key: PendingScanFailure,
    pub prepared: bool,
    pub producer_finished: bool,
    pub raw_retired: bool,
    pub raw_released: bool,
    pub member_count: usize,
    pub released: Option<ScanFailureHead>,
}

/// Authenticated proof that an exact release confirmation succeeded. The host
/// must persist the confirmation response before finalizing. Finalization may
/// be retried with this proof while this outbox remains alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanFailureReleaseConfirmation {
    pub ordinal: u64,
    pub capability: [u8; 16],
    pub raw: bool,
    pub authenticator: [u8; 32],
}

/// Opaque capability that permits one finalized cleanup receipt to be retired.
/// Its bytes are deliberately private: foreign hosts retain this only as an
/// opaque object and must not serialize it into application-visible data.
#[derive(Clone, PartialEq, Eq)]
pub struct ScanFailureCleanupRetirement {
    cleanup_receipt: [u8; 32],
    authenticator: [u8; 32],
}

fn scan_release_mac(
    key: &[u8; 32],
    ordinal: u64,
    capability: &[u8; 16],
    raw: bool,
) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(b"ubm-scan-release-finalize/1");
    mac.update(&ordinal.to_be_bytes());
    mac.update(capability);
    mac.update(&[u8::from(raw)]);
    mac
}

fn scan_release_confirmation(
    cleanup_key: &[u8; 32],
    ordinal: u64,
    capability: [u8; 16],
    raw: bool,
) -> ScanFailureReleaseConfirmation {
    ScanFailureReleaseConfirmation {
        ordinal,
        capability,
        raw,
        authenticator: scan_release_mac(cleanup_key, ordinal, &capability, raw)
            .finalize()
            .into_bytes()
            .into(),
    }
}

/// The journal retains only a one-way, domain-separated identifier for a
/// completed cleanup. The confirmation authenticator itself remains solely in
/// the caller's bounded receipt, rather than becoming another durable bearer
/// secret.
fn scan_cleanup_receipt_id(receipt: &ScanFailureReleaseConfirmation) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"ubm-scan-release-cleanup-receipt/1");
    digest.update(receipt.ordinal.to_be_bytes());
    digest.update(receipt.capability);
    digest.update([u8::from(receipt.raw)]);
    digest.update(receipt.authenticator);
    digest.finalize().into()
}

/// Raw source inspection is independent of preparation and validation. The
/// caller authenticates the capability and preserves the ordered member cut.
#[derive(Debug, PartialEq, Eq)]
pub struct ScanFailureRawInfo {
    pub identity: ScanFailureIdentity,
    pub member_count: usize,
    pub token_count: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ScanFailureRawToken {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64Bits(u64),
    Text { total: usize, bytes: Vec<u8> },
    Bytes { total: usize, bytes: Vec<u8> },
    Map(u64),
    Array(u64),
}

/// Owns one terminal slot. Dropping an unused token releases its quota.
pub struct ControlReservation {
    queues: Weak<Mutex<Queues>>,
    identity: Option<Arc<()>>,
}
impl Drop for ControlReservation {
    fn drop(&mut self) {
        if let (Some(queues), Some(identity)) = (self.queues.upgrade(), self.identity.take()) {
            lock(&queues)
                .reservations
                .retain(|slot| !Arc::ptr_eq(&slot.identity, &identity));
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ControlReservationError {
    Sealed,
    QuotaExceeded,
    InvalidToken,
    RecordTooLarge,
    InvalidRecord,
    OrdinalExhausted,
}

/// Memory retention succeeded; any journal error is reported separately.
#[derive(Debug)]
pub struct ReservedControlCommit {
    pub journal_error: Option<JournalError>,
}

struct ReservationSlot {
    identity: Arc<()>,
    committed: bool,
}

// Process-wide identities cannot alias across simultaneously live sessions.
static NEXT_DRAIN_CLAIM: AtomicU64 = AtomicU64::new(1);

/// A retained prefix. Its records stay owned by the outbox until acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainClaim {
    pub id: u64,
    pub records: Vec<Value>,
    pub control_lost: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainClaimError {
    RecordTooLarge { required_bytes: usize },
    IdentityExhausted,
    ModeConflict,
}

#[derive(Default, PartialEq, Eq)]
enum DrainMode {
    #[default]
    Unselected,
    Legacy,
    Claimed,
    PreservedLegacy,
}

#[derive(Clone)]
struct PendingDrain {
    id: u64,
    last_ordinal: u64,
    control_lost: u64,
    /// The only public view for this exact retained raw prefix.  Keeping it
    /// here, rather than reconstructing it from the queue, is what prevents a
    /// retry under a different wire version from silently changing the claim.
    records: Vec<Value>,
}

struct Entry {
    ordinal: u64,
    record: Value,
    bytes: usize,
    reservation: Option<Arc<()>>,
}

#[derive(Default)]
struct Queues {
    durable: Option<Durable>,
    data: VecDeque<Entry>,
    data_bytes: usize,
    control: VecDeque<Entry>,
    reservations: Vec<ReservationSlot>,
    scan_failures: Vec<ScanFailureSlot>,
    ordinal: u64,
    /// Control records refused past the cap, reported at the next drain.
    control_lost: u64,
    /// Every control record refused past the cap, ever. Reported in every
    /// drain response (X-R5): a drain that keeps arriving behind data still
    /// sees the gap within a bounded number of drains, without waiting for
    /// the queues to empty and without disturbing record ordinals.
    control_lost_total: u64,
    pending_drain: Option<PendingDrain>,
    last_acknowledged_drain: Option<u64>,
    drain_mode: DrainMode,
    evidence_owner: Option<Arc<()>>,
    /// A continuation claim sealed this outbox. Data that reaches the
    /// process after that authoritative cutoff is not silently treated as
    /// backlog for the prior owner; it is counted for the handoff result.
    after_cutoff_items: u64,
    after_cutoff_bytes: u64,
}

impl Queues {
    /// Check before any journal side effect; callers hold the queue lock until
    /// the corresponding admission is committed or refused.
    fn ordinal_after(&self, count: usize) -> Option<u64> {
        self.ordinal.checked_add(u64::try_from(count).ok()?)
    }

    fn allocate_ordinals(&mut self, count: usize) -> Option<u64> {
        let ordinal = self.ordinal_after(count)?;
        self.ordinal = ordinal;
        Some(ordinal)
    }
}

fn ordinal_ingress_failure(bytes: usize) -> DataIngressFailure {
    DataIngressFailure::Overflow { bytes }
}

/// Caller-owned, process-memory evidence for a single outbox. Retain this
/// object beyond session disposal; dropping it or the process loses its data.
/// It is independent of optional recording journals and does not promise disk
/// durability. Limits include the serialized complete original records.
pub struct LegacyEvidenceOwner {
    identity: Arc<()>,
    state: Mutex<LegacyEvidenceState>,
    budget: Option<Arc<LegacyEvidenceBudget>>,
}

/// One continuation owner may issue many independently recoverable strict-V1
/// claims.  Their exact originals share this reservation, so a sequence of
/// small claims cannot bypass the per-claim evidence bound.  Reservations are
/// made before the source prefix is acknowledged and released only when the
/// exact original is retired (or its owner is dropped).
pub struct LegacyEvidenceBudget {
    max_items: usize,
    max_bytes: usize,
    items: AtomicUsize,
    bytes: AtomicUsize,
}

impl LegacyEvidenceBudget {
    #[must_use]
    pub fn new(max_items: usize, max_bytes: usize) -> Self {
        Self {
            max_items,
            max_bytes,
            items: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
        }
    }

    fn reserve(&self, items: usize, bytes: usize) -> bool {
        fn reserve_one(value: &AtomicUsize, amount: usize, maximum: usize) -> bool {
            let mut current = value.load(Ordering::Acquire);
            loop {
                let Some(next) = current.checked_add(amount) else {
                    return false;
                };
                if next > maximum {
                    return false;
                }
                match value.compare_exchange_weak(
                    current,
                    next,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return true,
                    Err(observed) => current = observed,
                }
            }
        }
        if !reserve_one(&self.items, items, self.max_items) {
            return false;
        }
        if reserve_one(&self.bytes, bytes, self.max_bytes) {
            return true;
        }
        self.items.fetch_sub(items, Ordering::AcqRel);
        false
    }

    fn release(&self, items: usize, bytes: usize) {
        if items != 0 {
            self.items.fetch_sub(items, Ordering::AcqRel);
        }
        if bytes != 0 {
            self.bytes.fetch_sub(bytes, Ordering::AcqRel);
        }
    }
}

#[derive(Default)]
struct LegacyEvidenceState {
    source: Option<Weak<Mutex<Queues>>>,
    records: VecDeque<(u64, Value, usize)>,
    bytes: usize,
    max_items: usize,
    max_bytes: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LegacyEvidenceError {
    Capacity,
    RecordTooLarge { required_bytes: usize },
    ModeConflict,
    OwnerMismatch,
    InvalidProjection,
    OrdinalExhausted,
}

impl LegacyEvidenceOwner {
    #[must_use]
    pub fn new(max_items: usize, max_bytes: usize) -> Self {
        Self {
            identity: Arc::new(()),
            state: Mutex::new(LegacyEvidenceState {
                max_items,
                max_bytes,
                ..LegacyEvidenceState::default()
            }),
            budget: None,
        }
    }

    #[must_use]
    pub fn with_budget(
        max_items: usize,
        max_bytes: usize,
        budget: Arc<LegacyEvidenceBudget>,
    ) -> Self {
        Self {
            identity: Arc::new(()),
            state: Mutex::new(LegacyEvidenceState {
                max_items,
                max_bytes,
                ..LegacyEvidenceState::default()
            }),
            budget: Some(budget),
        }
    }

    /// Change the explicit quota, refusing to invalidate retained evidence.
    pub fn set_limits(&self, max_items: usize, max_bytes: usize) -> bool {
        let mut state = lock(&self.state);
        if state.records.len() > max_items || state.bytes > max_bytes {
            return false;
        }
        state.max_items = max_items;
        state.max_bytes = max_bytes;
        true
    }

    /// Atomically retain complete originals for a host that owns an
    /// equivalent source queue outside `Outbox`. Every record must carry a
    /// strictly increasing source ordinal; the caller must retain its source
    /// prefix until the companion continuation acknowledgement succeeds.
    ///
    /// This is intentionally not an acknowledgement: capacity refusal leaves
    /// the caller's source untouched, and exact bytes remain here until an
    /// explicit `retire` or owner drop.
    pub fn stage_exact_records(&self, records: &[Value]) -> Result<(), LegacyEvidenceError> {
        let mut staged = Vec::with_capacity(records.len());
        let mut previous = None;
        let mut bytes = 0usize;
        for record in records {
            let ordinal = record
                .get("ordinal")
                .and_then(Value::as_u64)
                .ok_or(LegacyEvidenceError::InvalidProjection)?;
            if previous.is_some_and(|prior| ordinal <= prior) {
                return Err(LegacyEvidenceError::InvalidProjection);
            }
            previous = Some(ordinal);
            let size = record.to_string().len();
            bytes = bytes
                .checked_add(size)
                .ok_or(LegacyEvidenceError::Capacity)?;
            staged.push((ordinal, record.clone(), size));
        }
        let mut state = lock(&self.state);
        if state.records.len().saturating_add(staged.len()) > state.max_items
            || state.bytes.saturating_add(bytes) > state.max_bytes
        {
            return Err(LegacyEvidenceError::Capacity);
        }
        if let Some(budget) = &self.budget
            && !budget.reserve(staged.len(), bytes)
        {
            return Err(LegacyEvidenceError::Capacity);
        }
        state.bytes += bytes;
        state.records.extend(staged);
        Ok(())
    }

    /// Read an exact original ordinal, including fields omitted by a projection.
    #[must_use]
    pub fn get(&self, ordinal: u64) -> Option<Value> {
        lock(&self.state)
            .records
            .iter()
            .find(|(id, _, _)| *id == ordinal)
            .map(|(_, value, _)| value.clone())
    }

    /// List retained ordinals in order, starting strictly after `after`.
    #[must_use]
    pub fn ordinals(&self, after: u64, max_items: usize) -> Vec<u64> {
        lock(&self.state)
            .records
            .iter()
            .filter(|(id, _, _)| *id > after)
            .take(max_items)
            .map(|(id, _, _)| *id)
            .collect()
    }

    /// Whether this owner still retains any exact original. Callers use this
    /// only when retiring a claim receipt: an active zero-record claim still
    /// needs its owner until that receipt is superseded or retired.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        lock(&self.state).records.is_empty()
    }

    /// Read one bounded, ordered page of exact retained originals without
    /// acknowledging them. The page is selected while holding the evidence
    /// lock, so retirement cannot make an advertised ordinal disappear before
    /// it is copied into this result. An oversized first record is refused
    /// whole with its required size; callers never receive a partial JSON
    /// record.
    pub fn page(
        &self,
        after: u64,
        max_items: usize,
        max_bytes: usize,
    ) -> Result<(Vec<Value>, bool), LegacyEvidenceError> {
        let state = lock(&self.state);
        let mut records = Vec::new();
        let mut bytes = 0usize;
        let limit = max_items.max(1);
        for (_, record, record_bytes) in state
            .records
            .iter()
            .filter(|(ordinal, _, _)| *ordinal > after)
        {
            if records.len() == limit {
                break;
            }
            if !records.is_empty() && bytes.saturating_add(*record_bytes) > max_bytes {
                break;
            }
            if records.is_empty() && *record_bytes > max_bytes {
                return Err(LegacyEvidenceError::RecordTooLarge {
                    required_bytes: *record_bytes,
                });
            }
            bytes = bytes.saturating_add(*record_bytes);
            records.push(record.clone());
        }
        let last = records
            .last()
            .and_then(|record| record.get("ordinal"))
            .and_then(Value::as_u64)
            .unwrap_or(after);
        let more = state.records.iter().any(|(ordinal, _, _)| *ordinal > last);
        Ok((records, more))
    }

    /// Retire exactly one ordinal after its independent consumer has accepted it.
    pub fn retire(&self, ordinal: u64) -> bool {
        let mut state = lock(&self.state);
        let Some(index) = state.records.iter().position(|(id, _, _)| *id == ordinal) else {
            return false;
        };
        let (_, _, bytes) = state.records.remove(index).expect("index found");
        state.bytes -= bytes;
        if let Some(budget) = &self.budget {
            budget.release(1, bytes);
        }
        true
    }
}

impl Drop for LegacyEvidenceOwner {
    fn drop(&mut self) {
        let state = lock(&self.state);
        if let Some(budget) = &self.budget {
            budget.release(state.records.len(), state.bytes);
        }
    }
}

// Count exact JSON bytes without allocating an encoded copy of unvalidated or
// unattempted records. Loss reports still include the entire refused suffix.
fn serialized_size(record: &Value) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, record).expect("counting JSON bytes cannot fail");
    counter.0
}

/// Observed data refused after an owner sealed its outbox for handoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AfterCutoffLoss {
    pub items: u64,
    pub bytes: u64,
}

/// A data record the session could not queue: the caller turns it into a
/// terminal (`stream-end overflow`) or an `ingress-drop`. The variant is the
/// cause as decided under the queue lock by the push itself, so a caller never
/// forms a second opinion by reading the outbox afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataIngressFailure {
    /// The journal stopped under this very push; the outbox is now sealed and
    /// the value was counted after the cutoff.
    Stopped {
        bytes: usize,
    },
    /// The outbox was already sealed when the push took the queue lock; the
    /// value was counted after the cutoff.
    Sealed {
        bytes: usize,
    },
    /// The bounded in-memory queue or its ordinal space was exhausted.
    /// Nothing was counted after a cutoff, whatever seals the outbox later.
    Overflow {
        bytes: usize,
    },
    Storage {
        bytes: usize,
        error: JournalError,
    },
}

impl DataIngressFailure {
    /// The refusal came from the handoff cutoff, which already counted every
    /// value of the refused group (and the tail behind it) after the cutoff.
    #[must_use]
    pub const fn after_cutoff(&self) -> bool {
        matches!(self, Self::Stopped { .. } | Self::Sealed { .. })
    }
}

/// Outcome of [`Outbox::push_data_batch`]: the records of the batch's leading
/// `accepted` prefix are admitted (and observable) in order; everything from
/// the first rejection on is `rejected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataBatchOutcome {
    pub accepted: usize,
    pub rejected: Option<DataBatchRejection>,
}

/// Private-sidecar recovery identity. This is deliberately not a diagnostic
/// descriptor or raw export: callers must already hold the configured journal
/// authority before they can obtain it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ScanFailureRecoveryAuthority {
    pub session_id: u64,
    pub authority: [u8; 32],
}

impl std::fmt::Debug for ScanFailureRecoveryAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScanFailureRecoveryAuthority")
            .field("session_id", &self.session_id)
            .field("authority", &"[REDACTED]")
            .finish()
    }
}

/// Why a batch stopped admitting, with every uncommitted record counted once.
/// `failure` names the precise cause for the first rejected record; `items`
/// and `bytes` also cover each later record the caller already holds, so none
/// of them can vanish unaccounted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataBatchRejection {
    pub failure: DataIngressFailure,
    pub items: u64,
    pub bytes: u64,
}

struct Durable {
    journal: Arc<ContinuationJournal>,
    context: Value,
    consumers: HashMap<String, Value>,
    failure: Option<JournalError>,
}

/// One session's outgoing records plus its wake state.
pub struct Outbox {
    session_id: u64,
    queues: Arc<Mutex<Queues>>,
    armed: AtomicBool,
    wake: Arc<dyn WakeSink>,
    observer: Arc<Mutex<Option<Observer>>>,
    sealed: AtomicBool,
    durable_enabled: AtomicBool,
    scan_recovery_authority: Mutex<Option<[u8; 32]>>,
}

fn with_ordinal(mut record: Value, ordinal: u64) -> Value {
    if let Value::Object(map) = &mut record {
        map.insert("ordinal".to_owned(), Value::from(ordinal));
    }
    record
}

// Both queues are already ordered; merge lazily so one-record claims do not
// sort the entire retained backlog for every acknowledgement.
fn ordered_entries(queues: &Queues) -> impl Iterator<Item = &Entry> {
    let mut data = queues.data.iter().peekable();
    let mut control = queues.control.iter().peekable();
    std::iter::from_fn(move || match (data.peek(), control.peek()) {
        (Some(left), Some(right)) if left.ordinal < right.ordinal => data.next(),
        (Some(_), Some(_)) => control.next(),
        (Some(_), None) => data.next(),
        (None, Some(_)) => control.next(),
        (None, None) => None,
    })
}

fn raw_scan_occurrence<'a>(
    queues: &'a Queues,
    key: &PendingScanFailure,
) -> Result<&'a ScanFailureOccurrence, ScanFailureOutboxError> {
    let slot = queues
        .scan_failures
        .iter()
        .find(|slot| {
            slot.ordinal == Some(key.ordinal) && slot.capability == key.capability && !slot.prepared
        })
        .ok_or(ScanFailureOutboxError::NotHead)?;
    if !slot.raw_retired
        && ordered_entries(queues)
            .next()
            .is_none_or(|entry| entry.ordinal != key.ordinal)
    {
        return Err(ScanFailureOutboxError::NotHead);
    }
    slot.occurrence
        .as_ref()
        .ok_or(ScanFailureOutboxError::NotHead)
}

// Receipt progress is an observation, not authority. A caller can retain a
// head minted before ACK while the durable release tombstone truthfully records
// completed progress. Both identify the same authenticated recipient source.
fn same_scan_failure_head_identity(left: &ScanFailureHead, right: &ScanFailureHead) -> bool {
    left.ordinal == right.ordinal
        && left.capability == right.capability
        && left.evidence == right.evidence
}

impl Outbox {
    /// Inspect only authenticated v2 owner envelopes under an already-open
    /// private journal. Older envelopes lack this capability and remain
    /// export-only instead of being granted invented recovery authority.
    pub fn recovery_authority_from_journal(
        journal: &ContinuationJournal,
    ) -> Result<Option<ScanFailureRecoveryAuthority>, JournalError> {
        let recording_authority = journal.scan_failure_recording_recovery_authority()?;
        let envelopes = journal.scan_failure_owner_envelopes()?;
        let Some(first) = envelopes.first() else {
            return Ok(
                recording_authority.map(|authority| ScanFailureRecoveryAuthority {
                    session_id: authority.session_id,
                    authority: authority.authority,
                }),
            );
        };
        let raw = journal.load_scan_failure_raw_cut(first.identity)?;
        let first = restore_scan_owner(&first.body, raw)?;
        let authority = first
            .recovery_authority
            .ok_or_else(|| JournalError::invalid("scan owner has no recovery authority"))?;
        let result = ScanFailureRecoveryAuthority {
            session_id: first.session_id,
            authority,
        };
        for envelope in envelopes.into_iter().skip(1) {
            let raw = journal.load_scan_failure_raw_cut(envelope.identity)?;
            let slot = restore_scan_owner(&envelope.body, raw)?;
            if slot.session_id != result.session_id
                || slot.recovery_authority != Some(result.authority)
            {
                return Err(JournalError::invalid(
                    "scan owners do not share a session recovery authority",
                ));
            }
        }
        if recording_authority.is_some_and(|authority| {
            authority.session_id != result.session_id || authority.authority != result.authority
        }) {
            return Err(JournalError::invalid(
                "scan recording authority differs from owner envelopes",
            ));
        }
        Ok(Some(result))
    }
    /// Bind the opaque owner capability before a scan can reserve a failure
    /// source. A later change would make one durable recording ambiguous.
    pub fn configure_scan_failure_recovery_authority(
        &self,
        authority: [u8; 32],
    ) -> Result<(), ScanFailureOutboxError> {
        let queues = lock(&self.queues);
        if !queues.scan_failures.is_empty() {
            return Err(ScanFailureOutboxError::InvalidReservation);
        }
        drop(queues);
        let mut configured = lock(&self.scan_recovery_authority);
        match *configured {
            Some(existing) if existing != authority => {
                Err(ScanFailureOutboxError::InvalidReservation)
            }
            Some(_) => Ok(()),
            None => {
                *configured = Some(authority);
                Ok(())
            }
        }
    }

    /// Return the sole persisted recovery authority after sidecar attachment.
    /// Missing capabilities identify legacy/export-only owners; disagreements
    /// mean the recording cannot be safely attributed to one session.
    pub fn scan_failure_recovery_authority(&self) -> Result<Option<[u8; 32]>, JournalError> {
        let queues = lock(&self.queues);
        let Some(first) = queues.scan_failures.first() else {
            return Ok(*lock(&self.scan_recovery_authority));
        };
        let authority = first
            .recovery_authority
            .ok_or_else(|| JournalError::invalid("scan owner has no recovery authority"))?;
        let persisted_session_id = first.session_id;
        if queues.scan_failures.iter().any(|slot| {
            slot.session_id != persisted_session_id || slot.recovery_authority != Some(authority)
        }) {
            return Err(JournalError::invalid(
                "scan owners do not share a session recovery authority",
            ));
        }
        Ok(Some(authority))
    }
    /// Select one durable cursor before consumer/data ingress or control delivery.
    /// Controls queued before attachment retain their insertion order; when the
    /// journal has a blocking recovered scan source they are rebased after it.
    /// Native drain/claim never removes or acknowledges these durable records.
    pub fn attach_journal(
        &self,
        journal: Arc<ContinuationJournal>,
        context: Value,
    ) -> Result<(), JournalError> {
        ContinuationJournal::validate_metadata(&context)?;
        let mut queues = lock(&self.queues);
        if self.is_sealed()
            || queues.durable.is_some()
            || queues
                .scan_failures
                .iter()
                .any(|slot| slot.occurrence.is_some())
            || !queues.data.is_empty()
            || queues.ordinal != queues.control.len() as u64
            || queues
                .control
                .iter()
                .any(|entry| entry.record.get("consumer").is_some())
        {
            return Err(JournalError::invalid(
                "journal must attach before consumer ingress or delivery",
            ));
        }
        // Reconstruct only v2 owners that have both an opaque owner envelope
        // and the exact raw source cut.  v1 sidecars enumerate no envelopes,
        // so they remain export-only rather than acquiring invented authority.
        let restored = journal.scan_failure_owner_envelopes()?;
        let mut restored_slots = Vec::with_capacity(restored.len());
        let mut maximum_restored_ordinal: Option<u64> = None;
        let mut persisted_session_id: Option<u64> = None;
        let mut persisted_recovery_authority: Option<Option<[u8; 32]>> = None;
        for envelope in restored {
            let raw = journal.load_scan_failure_raw_cut(envelope.identity)?;
            let slot = restore_scan_owner(&envelope.body, raw)?;
            let ordinal = slot.ordinal.expect("validated scan owner ordinal");
            if persisted_session_id.is_some_and(|session_id| session_id != slot.session_id) {
                return Err(JournalError::invalid(
                    "scan owners do not share a persisted session",
                ));
            }
            if persisted_recovery_authority
                .is_some_and(|authority| authority != slot.recovery_authority)
            {
                return Err(JournalError::invalid(
                    "scan owners do not share a recovery authority",
                ));
            }
            if queues
                .scan_failures
                .iter()
                .chain(restored_slots.iter())
                .any(|owner| owner.ordinal == Some(ordinal))
            {
                return Err(JournalError::invalid("scan owner ordinal is duplicated"));
            }
            maximum_restored_ordinal =
                Some(maximum_restored_ordinal.map_or(ordinal, |maximum| maximum.max(ordinal)));
            persisted_session_id = Some(slot.session_id);
            persisted_recovery_authority = Some(slot.recovery_authority);
            restored_slots.push(slot);
        }
        // The private sidecar's authenticated identity index has no capture
        // order semantics. Rebuild the in-memory fence by the persisted
        // session ordinal, otherwise a lexicographically earlier identity can
        // let a later failure overtake an unresolved source after restart.
        restored_slots.sort_by_key(|slot| slot.ordinal.expect("validated scan owner ordinal"));
        if let Some(maximum_ordinal) = maximum_restored_ordinal {
            // A process can queue controls while durable attachment waits.
            // Their local ordinals are a new process-local domain, whereas
            // restored owners carry the previous process's authenticated
            // source order. Keep every control, but append it after the
            // recovered domain and rewrite its public ordinal at the same
            // time. The control deque must remain sorted for `ordered_entries`.
            // Validate the entire rebase before moving an entry. A failed
            // attach leaves the caller-owned controls available for another
            // explicit disposition instead of losing them behind overflow.
            let control_count = u64::try_from(queues.control.len())
                .map_err(|_| JournalError::invalid("scan recovery control count overflows"))?;
            let final_ordinal = maximum_ordinal
                .checked_add(control_count)
                .ok_or_else(|| JournalError::invalid("scan recovery ordinal exhausted"))?;
            let mut pre_attach_controls = std::mem::take(&mut queues.control);
            let mut next = maximum_ordinal;
            for entry in &mut pre_attach_controls {
                // `final_ordinal` proved that this entire bounded loop fits.
                next = next
                    .checked_add(1)
                    .expect("preflighted scan recovery ordinal");
                entry.ordinal = next;
                entry.record = with_ordinal(std::mem::take(&mut entry.record), next);
            }
            for slot in restored_slots {
                let ordinal = slot.ordinal.expect("validated scan owner ordinal");
                if !slot.raw_retired && slot.released.is_none() {
                    queues.control.push_back(Entry {
                        ordinal,
                        record: Value::Null,
                        bytes: 0,
                        reservation: Some(Arc::clone(&slot.identity)),
                    });
                }
                queues.scan_failures.push(slot);
            }
            queues.control.extend(pre_attach_controls);
            debug_assert_eq!(next, final_ordinal);
            queues.ordinal = final_ordinal;
        }
        let mut durable = Durable {
            journal,
            context,
            consumers: HashMap::new(),
            failure: None,
        };
        // Process controls can arrive after native session creation while its
        // blocking journal admission is pending. Retain their ordinary delivery
        // and persist only the declared-peer/global subset under this same lock.
        for entry in &queues.control {
            // Rehydrated scan fences use a private sentinel rather than a
            // public control record.  It was already durably represented by
            // the owner envelope; appending JSON null would both fail the
            // journal's object contract and duplicate its source ordinal.
            if !entry.record.is_null() {
                Self::persist(&mut durable, &entry.record)?;
            }
        }
        if queues.control_lost != 0 {
            Self::persist(
                &mut durable,
                &serde_json::json!({"t":"ingress-drop","class":"control","count":queues.control_lost}),
            )?;
        }
        queues.durable = Some(durable);
        self.durable_enabled.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Commit immutable generation/selector context before the corresponding
    /// subscribe can synchronously produce its first value.
    pub fn register_journal_consumer(
        &self,
        consumer: &str,
        metadata: Value,
    ) -> Result<(), JournalError> {
        let mut queues = lock(&self.queues);
        if self.is_sealed() {
            return Err(JournalError::invalid(
                "sealed outbox refuses journal registration",
            ));
        }
        let Some(durable) = queues.durable.as_mut() else {
            return Ok(());
        };
        if consumer.is_empty() || consumer.len() > 256 || durable.consumers.len() >= 4096 {
            return Err(JournalError::invalid(
                "invalid or excessive journal consumers",
            ));
        }
        if let Some(failure) = &durable.failure {
            return Err(failure.clone());
        }
        if let Some(previous) = durable.consumers.get(consumer) {
            return if previous == &metadata {
                Ok(())
            } else {
                Err(JournalError::invalid(
                    "journal consumer metadata is immutable",
                ))
            };
        }
        ContinuationJournal::validate_metadata(&metadata)?;
        let context = serde_json::json!({"session":durable.context,"consumer":metadata});
        let registration = serde_json::json!({"t":"consumer-registration","consumer":consumer});
        let admission = durable
            .journal
            .append_batch_terminal_checked(&[(&context, &registration)], |_| Ok(()))
            .and_then(|batch| match batch.rejected {
                Some(rejected) => Err(rejected.error),
                None => Ok(()),
            });
        if let Err(failure) = admission {
            if failure.kind == "storage.stopped" {
                self.sealed.store(true, Ordering::SeqCst);
                queues.reservations.retain(|slot| slot.committed);
                lock(&self.observer).take();
                return Err(failure);
            }
            durable.failure = Some(failure.clone());
            return Err(failure);
        }
        durable.consumers.insert(consumer.to_owned(), metadata);
        Ok(())
    }

    #[must_use]
    pub fn journal_failure(&self) -> Option<JournalError> {
        lock(&self.queues)
            .durable
            .as_ref()
            .and_then(|durable| durable.failure.clone())
    }

    /// Called on the blocking worker after an ingress worker failed. No pending
    /// acknowledgement may become a success after this terminal storage fault.
    pub fn fail_collection_worker(&self) {
        let failure = JournalError {
            kind: "storage.io",
            detail: "native collection worker did not complete",
            operation: "ingress-worker",
            sqlite_extended_code: None,
            sqlite_code: None,
        };
        let mut queues = lock(&self.queues);
        if let Some(durable) = queues.durable.as_mut() {
            durable.journal.mark_collection_failure(&failure);
            durable.failure = Some(failure.clone());
        }
        self.storage_terminal(Value::Null, &failure);
    }

    /// A terminal storage fault is retained with its precise cause; a stopped
    /// journal is an admission result, not collection failure evidence.
    fn retain_failure(durable: &mut Durable, failure: &JournalError) {
        if failure.kind != "storage.stopped" {
            durable.failure = Some(failure.clone());
        }
    }

    fn persist(durable: &mut Durable, record: &Value) -> Result<(), JournalError> {
        // Ordinary mobile control delivery is process-wide, while a durable
        // recording belongs to one declared peer. Foreign controls remain in
        // the ordinary outbox; they must never acquire this journal's context.
        // Global controls without a subject are retained unchanged.
        if let Some(peer) = record.get("peerId").and_then(Value::as_str)
            && durable.context.get("peerId").and_then(Value::as_str) != Some(peer)
        {
            return Ok(());
        }
        if let Some(failure) = &durable.failure {
            return Err(failure.clone());
        }
        let metadata = record["consumer"]
            .as_str()
            .and_then(|consumer| durable.consumers.get(consumer));
        let context = serde_json::json!({"session":durable.context,"consumer":metadata});
        let result = durable
            .journal
            .append_batch_terminal_checked(&[(&context, record)], |_| Ok(()))
            .and_then(|batch| match batch.rejected {
                Some(rejected) => Err(rejected.error),
                None => Ok(()),
            });
        if let Err(failure) = &result {
            Self::retain_failure(durable, failure);
        }
        result
    }

    /// Data records share one journal transaction. Each reached record needs a
    /// committed consumer registration; a validation error rolls the batch back.
    fn persist_data(durable: &mut Durable, records: &[Value]) -> Result<AppendBatch, JournalError> {
        if let Some(failure) = &durable.failure {
            return Err(failure.clone());
        }
        let contexts: Vec<Value> = records
            .iter()
            .map(|record| {
                let metadata = record["consumer"]
                    .as_str()
                    .and_then(|consumer| durable.consumers.get(consumer));
                serde_json::json!({"session":durable.context,"consumer":metadata})
            })
            .collect();
        let items: Vec<(&Value, &Value)> = contexts.iter().zip(records).collect();
        let result = durable
            .journal
            .append_batch_terminal_checked(&items, |index| {
                if records[index]["consumer"]
                    .as_str()
                    .is_some_and(|consumer| durable.consumers.contains_key(consumer))
                {
                    Ok(())
                } else {
                    Err(JournalError::invalid(
                        "durable value has no committed consumer registration",
                    ))
                }
            });
        match &result {
            Err(failure) => Self::retain_failure(durable, failure),
            Ok(batch) => {
                if let Some(rejected) = &batch.rejected {
                    Self::retain_failure(durable, &rejected.error);
                }
            }
        }
        result
    }

    fn storage_terminal(&self, consumer: Value, error: &JournalError) {
        let mut observer = lock(&self.observer);
        if let Some(observer) = observer.take() {
            let _=observer.sender.send(serde_json::json!({"t":"stream-end","consumer":consumer,"reason":"source-failed","error":crate::continuation::recording_failure(error.clone())}));
        }
    }
    #[must_use]
    pub fn new(session_id: u64, wake: Arc<dyn WakeSink>) -> Self {
        Self {
            session_id,
            queues: Arc::new(Mutex::new(Queues::default())),
            armed: AtomicBool::new(true),
            wake,
            observer: Arc::default(),
            sealed: AtomicBool::new(false),
            durable_enabled: AtomicBool::new(false),
            scan_recovery_authority: Mutex::new(None),
        }
    }

    fn signal(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.wake.wake(self.session_id);
        }
    }

    /// At most one setup step owns observation admission. The queue lock
    /// orders registration and sealing with ingress; no historical replay.
    pub fn observe(
        &self,
        consumer: &str,
        matcher: RecordMatcher,
    ) -> Result<Observation, &'static str> {
        let queues = lock(&self.queues);
        let mut slot = lock(&self.observer);
        if self.is_sealed()
            || slot.is_some()
            || queues
                .durable
                .as_ref()
                .is_some_and(|durable| durable.failure.is_some())
        {
            return Err("outbox sealed or observation already active");
        }
        let identity = Arc::new(());
        let (sender, receiver) = tokio::sync::oneshot::channel();
        *slot = Some(Observer {
            identity: identity.clone(),
            consumer: consumer.to_owned(),
            matcher,
            sender,
        });
        Ok(Observation {
            receiver,
            identity,
            slot: self.observer.clone(),
        })
    }

    /// Queue one data record (`adv` / `value`). The one-element case of
    /// [`Self::push_data_batch`].
    pub fn push_data(&self, record: Value) -> Result<(), DataIngressFailure> {
        match self.push_data_batch(vec![record]).rejected {
            Some(rejection) => Err(rejection.failure),
            None => Ok(()),
        }
    }

    /// Queue records the caller already holds, in order, committing durable
    /// ones in one journal transaction. Observation is evaluated only for
    /// admitted records, after their commit, in order.
    ///
    /// Admission is the sequential prefix: it stops at the first refusal. A
    /// durable storage fault rolls the whole transaction back, so `accepted`
    /// is 0 and no record is observed. A sealed or stopped outbox counts every
    /// record of the batch after the cutoff. Every record from the first
    /// refusal on is counted once in the rejection.
    ///
    /// No commit carries more than [`APPEND_BATCH_MAX`] records: a longer batch
    /// is committed in consecutive bounded groups, stopping at the first refusal.
    pub fn push_data_batch(&self, records: Vec<Value>) -> DataBatchOutcome {
        let controls: Vec<bool> = records
            .iter()
            .map(|record| record["t"] == "peer-source-loss")
            .collect();
        let mut remaining = records.into_iter();
        let mut accepted = 0;
        loop {
            let group: Vec<Value> = remaining.by_ref().take(APPEND_BATCH_MAX).collect();
            if group.is_empty() {
                return DataBatchOutcome {
                    accepted,
                    rejected: None,
                };
            }
            let mut outcome = self.push_data_group(group);
            accepted += outcome.accepted;
            if let Some(rejected) = outcome.rejected.as_mut() {
                let mut tail_items = 0u64;
                let mut tail_bytes = 0u64;
                for record in remaining {
                    if record["t"] != "peer-source-loss" {
                        tail_items = tail_items.saturating_add(1);
                        tail_bytes = tail_bytes.saturating_add(serialized_size(&record) as u64);
                    }
                }
                rejected.items = rejected.items.saturating_add(tail_items);
                rejected.bytes = rejected.bytes.saturating_add(tail_bytes);
                // A refusal by the cutoff (already sealed, or a journal that
                // stopped and sealed under the first group) puts the values held
                // beyond that group after the same cutoff, too. The cause is the
                // group's own answer: a seal that lands after a queue or storage
                // refusal changes neither where those values are counted nor
                // how many times.
                if rejected.failure.after_cutoff() {
                    let mut queues = lock(&self.queues);
                    queues.after_cutoff_items =
                        queues.after_cutoff_items.saturating_add(tail_items);
                    queues.after_cutoff_bytes =
                        queues.after_cutoff_bytes.saturating_add(tail_bytes);
                }
                let lost_controls = controls[accepted..]
                    .iter()
                    .filter(|control| **control)
                    .count() as u64;
                if lost_controls > 0 {
                    let mut queues = lock(&self.queues);
                    queues.control_lost = queues.control_lost.saturating_add(lost_controls);
                    queues.control_lost_total =
                        queues.control_lost_total.saturating_add(lost_controls);
                    drop(queues);
                    self.signal();
                }
                return DataBatchOutcome {
                    accepted,
                    rejected: outcome.rejected,
                };
            }
        }
    }

    fn push_data_group(&self, records: Vec<Value>) -> DataBatchOutcome {
        if records.is_empty() {
            return DataBatchOutcome {
                accepted: 0,
                rejected: None,
            };
        }
        let sizes: Vec<usize> = records.iter().map(serialized_size).collect();
        let loss_sizes: Vec<usize> = records
            .iter()
            .zip(&sizes)
            .map(|(record, size)| {
                if record["t"] == "peer-source-loss" {
                    0
                } else {
                    *size
                }
            })
            .collect();
        let refuse = |accepted: usize, failure: DataIngressFailure| DataBatchOutcome {
            accepted,
            rejected: Some(DataBatchRejection {
                failure,
                items: loss_sizes[accepted..]
                    .iter()
                    .filter(|size| **size != 0)
                    .count() as u64,
                bytes: loss_sizes[accepted..]
                    .iter()
                    .map(|bytes| *bytes as u64)
                    .sum(),
            }),
        };
        let mut queues = lock(&self.queues);
        if self.is_sealed() {
            Self::count_after_cutoff(&mut queues, &loss_sizes);
            return refuse(0, DataIngressFailure::Sealed { bytes: sizes[0] });
        }
        let available = u64::MAX - queues.ordinal;
        let durable_prefix = usize::try_from(available)
            .unwrap_or(usize::MAX)
            .min(records.len());
        if queues.durable.is_some() && durable_prefix == 0 {
            return refuse(0, ordinal_ingress_failure(sizes[0]));
        }
        if let Some(durable) = queues.durable.as_mut() {
            let batch = match Self::persist_data(durable, &records[..durable_prefix]) {
                Ok(batch) => batch,
                Err(error) if error.kind == "storage.stopped" => {
                    self.sealed.store(true, Ordering::SeqCst);
                    queues.reservations.retain(|slot| slot.committed);
                    Self::count_after_cutoff(&mut queues, &loss_sizes);
                    lock(&self.observer).take();
                    return refuse(0, DataIngressFailure::Stopped { bytes: sizes[0] });
                }
                Err(error) => {
                    self.storage_terminal(records[0]["consumer"].clone(), &error);
                    return refuse(
                        0,
                        DataIngressFailure::Storage {
                            bytes: sizes[0],
                            error,
                        },
                    );
                }
            };
            queues
                .allocate_ordinals(batch.accepted)
                .expect("batch ordinal checked before journal commit");
            // The commit has happened; observe the admitted prefix in order.
            for record in &records[..batch.accepted] {
                Self::observe_admitted(&self.observer, record);
            }
            return match batch.rejected {
                Some(rejected) => {
                    self.storage_terminal(
                        records[batch.accepted]["consumer"].clone(),
                        &rejected.error,
                    );
                    refuse(
                        batch.accepted,
                        DataIngressFailure::Storage {
                            bytes: sizes[batch.accepted],
                            error: rejected.error,
                        },
                    )
                }
                None if batch.accepted < records.len() => refuse(
                    batch.accepted,
                    ordinal_ingress_failure(sizes[batch.accepted]),
                ),
                None => DataBatchOutcome {
                    accepted: batch.accepted,
                    rejected: None,
                },
            };
        }
        let mut accepted = 0;
        let mut overflow = None;
        for (record, bytes) in records.into_iter().zip(sizes.iter().copied()) {
            if queues.data.len() >= DATA_RECORD_CAP || queues.data_bytes + bytes > DATA_RECORD_BYTES
            {
                overflow = Some(DataIngressFailure::Overflow { bytes });
                break;
            }
            let Some(ordinal) = queues.allocate_ordinals(1) else {
                overflow = Some(ordinal_ingress_failure(bytes));
                break;
            };
            queues.data_bytes += bytes;
            Self::observe_admitted(&self.observer, &record);
            queues.data.push_back(Entry {
                ordinal,
                record,
                bytes,
                reservation: None,
            });
            accepted += 1;
        }
        drop(queues);
        if accepted > 0 {
            self.signal();
        }
        match overflow {
            Some(failure) => refuse(accepted, failure),
            None => DataBatchOutcome {
                accepted,
                rejected: None,
            },
        }
    }

    fn count_after_cutoff(queues: &mut Queues, sizes: &[usize]) {
        queues.after_cutoff_items = queues
            .after_cutoff_items
            .saturating_add(sizes.iter().filter(|size| **size != 0).count() as u64);
        queues.after_cutoff_bytes = queues
            .after_cutoff_bytes
            .saturating_add(sizes.iter().map(|bytes| *bytes as u64).sum());
    }

    /// Hand an admitted record to the active observation, at most once. A
    /// dropped receiver means its scoped step already ended; the record
    /// remains retained independently.
    fn observe_admitted(slot: &Mutex<Option<Observer>>, record: &Value) {
        let mut observer = lock(slot);
        if observer.as_ref().is_some_and(|observer| {
            record["consumer"] == observer.consumer && (observer.matcher)(record)
        }) && let Some(observer) = observer.take()
        {
            let _ = observer.sender.send(record.clone());
        }
    }

    /// Establishes the handoff cutoff. The same mutex orders this state
    /// change with every data admission: an accepted record is before the
    /// cutoff and remains drainable; a later attempt is counted explicitly.
    pub fn seal(&self) -> AfterCutoffLoss {
        let mut queues = lock(&self.queues);
        self.seal_locked(&mut queues)
    }

    /// Attempt a cutoff without waiting behind an in-flight producer.
    pub fn try_seal(&self) -> Option<AfterCutoffLoss> {
        let mut queues = match self.queues.try_lock() {
            Ok(queues) => queues,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        Some(self.seal_locked(&mut queues))
    }

    fn seal_locked(&self, queues: &mut Queues) -> AfterCutoffLoss {
        self.sealed.store(true, Ordering::SeqCst);
        queues.reservations.retain(|slot| slot.committed);
        lock(&self.observer).take();
        AfterCutoffLoss {
            items: queues.after_cutoff_items,
            bytes: queues.after_cutoff_bytes,
        }
    }

    #[must_use]
    pub fn after_cutoff_loss(&self) -> AfterCutoffLoss {
        let queues = lock(&self.queues);
        AfterCutoffLoss {
            items: queues.after_cutoff_items,
            bytes: queues.after_cutoff_bytes,
        }
    }

    #[must_use]
    pub fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn has_journal(&self) -> bool {
        self.durable_enabled.load(Ordering::SeqCst)
    }

    /// Preserve upstream loss observed after handoff, even when the source
    /// could not retain bytes for an ordinary data admission.
    ///
    /// Returns whether the loss was counted: `false` means the outbox is not
    /// sealed and the caller still owns reporting it.
    pub fn note_after_cutoff_loss(&self, items: u64, bytes: u64) -> bool {
        let mut queues = lock(&self.queues);
        if self.is_sealed() {
            queues.after_cutoff_items = queues.after_cutoff_items.saturating_add(items);
            queues.after_cutoff_bytes = queues.after_cutoff_bytes.saturating_add(bytes);
            true
        } else {
            false
        }
    }

    /// Report ordered controls that could not be retained.
    pub fn note_control_loss(&self, count: u64) {
        if count == 0 {
            return;
        }
        let mut queues = lock(&self.queues);
        queues.control_lost = queues.control_lost.saturating_add(count);
        queues.control_lost_total = queues.control_lost_total.saturating_add(count);
        drop(queues);
        self.signal();
    }

    /// Queue a coalesced, payload-free wake for a committed recording change.
    pub fn recording_changed(&self, consumer: &str, recording_id: &str) {
        {
            let mut queues = lock(&self.queues);
            let claimed_through = queues
                .pending_drain
                .as_ref()
                .map(|claim| claim.last_ordinal);
            if queues.control.iter().any(|entry| {
                entry.reservation.is_none()
                    && !claimed_through.is_some_and(|last| entry.ordinal <= last)
                    && entry.record["t"] == "recording-changed"
                    && entry.record["consumer"] == consumer
                    && entry.record["recordingId"] == recording_id
            }) {
                return;
            }
            if queues
                .control
                .iter()
                .filter(|entry| entry.reservation.is_none())
                .count()
                >= CONTROL_RECORD_CAP
            {
                queues.control_lost += 1;
                queues.control_lost_total += 1;
            } else {
                let Some(ordinal) = queues.allocate_ordinals(1) else {
                    queues.control_lost = queues.control_lost.saturating_add(1);
                    queues.control_lost_total = queues.control_lost_total.saturating_add(1);
                    drop(queues);
                    self.signal();
                    return;
                };
                queues.control.push_back(Entry { ordinal, bytes: 0, reservation: None,
                    record: serde_json::json!({"t":"recording-changed", "consumer":consumer, "recordingId":recording_id}) });
            }
        }
        self.signal();
    }

    /// Retire queued hints together with their route.
    pub fn remove_recording_hints(&self, consumer: &str) {
        let mut queues = lock(&self.queues);
        let claimed_through = queues
            .pending_drain
            .as_ref()
            .map(|claim| claim.last_ordinal);
        queues.control.retain(|entry| {
            entry.reservation.is_some()
                || claimed_through.is_some_and(|last| entry.ordinal <= last)
                || !(entry.record["t"] == "recording-changed"
                    && entry.record["consumer"] == consumer)
        });
    }

    /// Reserve a bounded owner before starting the scan. Captured source and
    /// member capacities have a separate aggregate cap checked at transfer.
    /// The producer must bound its own allocations and one output chunk.
    pub fn reserve_scan_failure(
        &self,
        limits: ScanFailureLimits,
    ) -> Result<ScanFailureReservation, ScanFailureOutboxError> {
        self.reserve_scan_failure_inner(limits, None)
    }

    /// Reserve source and recipient allocation capacity before admitting the
    /// producer. The declared capacity covers the same allocations measured by
    /// `ScanFailureOccurrence::retained_source_capacity`; allocator overhead,
    /// producer-owned representations, and output chunks remain separate.
    pub fn reserve_scan_failure_with_source_capacity(
        &self,
        limits: ScanFailureLimits,
        source_capacity: u64,
    ) -> Result<ScanFailureReservation, ScanFailureOutboxError> {
        if source_capacity == 0 {
            return Err(ScanFailureOutboxError::Capacity);
        }
        self.reserve_scan_failure_inner(limits, Some(source_capacity))
    }

    fn reserve_scan_failure_inner(
        &self,
        limits: ScanFailureLimits,
        source_capacity: Option<u64>,
    ) -> Result<ScanFailureReservation, ScanFailureOutboxError> {
        let budget = limits
            .evidence
            .max_encoded_bytes
            .checked_add(limits.max_bookkeeping_bytes as u64)
            .filter(|budget| *budget > 0)
            .ok_or(ScanFailureOutboxError::Capacity)?;
        let mut queues = lock(&self.queues);
        if self.is_sealed() {
            return Err(ScanFailureOutboxError::Sealed);
        }
        if queues.scan_failures.len() >= SCAN_FAILURE_OWNER_CAP
            || queues
                .scan_failures
                .iter()
                .try_fold(budget, |sum, slot| sum.checked_add(slot.budget))
                .is_none_or(|sum| sum > SCAN_FAILURE_BUDGET_BYTES)
            || queues
                .scan_failures
                .iter()
                .try_fold(source_capacity.unwrap_or(0), |sum, slot| {
                    sum.checked_add(slot.source_budget)
                })
                .is_none_or(|sum| sum > SCAN_FAILURE_SOURCE_BYTES)
        {
            return Err(ScanFailureOutboxError::Capacity);
        }
        // `Outbox::session_id` routes wakes for this process.  Once a durable
        // scan-owner domain is recovered, however, new owners must continue
        // that persisted domain: a later cold reopen validates one coherent
        // recovery session, not one wake identity per process generation.
        let owner_session_id = queues
            .scan_failures
            .first()
            .map_or(self.session_id, |slot| slot.session_id);
        let owner_recovery_authority = queues
            .scan_failures
            .first()
            .map_or(*lock(&self.scan_recovery_authority), |slot| {
                slot.recovery_authority
            });
        if queues.scan_failures.iter().any(|slot| {
            slot.session_id != owner_session_id
                || slot.recovery_authority != owner_recovery_authority
        }) {
            return Err(ScanFailureOutboxError::InvalidReservation);
        }
        let mut capability = [0; 16];
        getrandom::fill(&mut capability)
            .map_err(|_| ScanFailureOutboxError::IdentityUnavailable)?;
        let mut cleanup_key = [0; 32];
        getrandom::fill(&mut cleanup_key)
            .map_err(|_| ScanFailureOutboxError::IdentityUnavailable)?;
        let identity = Arc::new(());
        queues.scan_failures.push(ScanFailureSlot {
            identity: Arc::clone(&identity),
            session_id: owner_session_id,
            capability,
            cleanup_key,
            recovery_authority: owner_recovery_authority,
            limits,
            budget,
            source_budget: source_capacity.unwrap_or(0),
            source_pre_admitted: source_capacity.is_some(),
            ordinal: None,
            occurrence: None,
            occurrence_identity: None,
            attached: true,
            prepared: false,
            producer_finished: false,
            released: None,
            raw_retired: false,
            raw_released: false,
        });
        Ok(ScanFailureReservation {
            queues: Arc::downgrade(&self.queues),
            identity: Some(identity),
        })
    }

    /// Resize a reservation before capture or after a preparation refusal.
    /// A refused resize leaves the original budget and ordering fence intact.
    pub fn resize_scan_failure_reservation(
        &self,
        token: &ScanFailureReservation,
        limits: ScanFailureLimits,
    ) -> Result<(), ScanFailureOutboxError> {
        let mut queues = lock(&self.queues);
        let identity = token
            .identity
            .as_ref()
            .ok_or(ScanFailureOutboxError::InvalidReservation)?;
        if !Weak::ptr_eq(&token.queues, &Arc::downgrade(&self.queues)) {
            return Err(ScanFailureOutboxError::InvalidReservation);
        }
        let index = queues
            .scan_failures
            .iter()
            .position(|slot| Arc::ptr_eq(&slot.identity, identity) && slot.attached)
            .ok_or(ScanFailureOutboxError::InvalidReservation)?;
        if self.is_sealed() && queues.scan_failures[index].ordinal.is_none() {
            return Err(ScanFailureOutboxError::Sealed);
        }
        if queues.scan_failures[index].prepared {
            return Err(ScanFailureOutboxError::InvalidReservation);
        }
        // The encoded form is streamed, not retained as a second body. Once
        // captured, the separate source-capacity quota owns those bytes.
        let budget = if queues.scan_failures[index].ordinal.is_some() {
            limits.max_bookkeeping_bytes as u64
        } else {
            limits
                .evidence
                .max_encoded_bytes
                .checked_add(limits.max_bookkeeping_bytes as u64)
                .filter(|budget| *budget > 0)
                .ok_or(ScanFailureOutboxError::Capacity)?
        };
        if queues
            .scan_failures
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .try_fold(budget, |sum, (_, slot)| sum.checked_add(slot.budget))
            .is_none_or(|sum| sum > SCAN_FAILURE_BUDGET_BYTES)
        {
            return Err(ScanFailureOutboxError::Capacity);
        }
        queues.scan_failures[index].limits = limits;
        queues.scan_failures[index].budget = budget;
        drop(queues);
        self.signal();
        Ok(())
    }

    /// Atomically captures the occurrence and its failure-time cut before preparation.
    /// Once the complete owner envelope is durable, `source` is taken even if
    /// preparation fails; retry with `None` and the same reservation. A raw or
    /// owner persistence failure leaves it owned by the caller. A captured
    /// owner can be retried after seal. A successful
    /// commit can be retried with `None` and the same token until receipt release.
    /// The producer
    /// must encode any upstream source ordinal and raw failure in the occurrence.
    /// A nonrecoverable preparation error also leaves the fence and exact source
    /// retained; later records cannot pass it. The host must retain the outbox,
    /// report that error, and provide a recovery path rather than discard it.
    pub fn commit_scan_failure(
        &self,
        token: &mut ScanFailureReservation,
        source: &mut Option<ScanFailureOccurrence>,
    ) -> Result<u64, ScanFailureOutboxError> {
        let mut queues = lock(&self.queues);
        let identity = token
            .identity
            .as_ref()
            .ok_or(ScanFailureOutboxError::InvalidReservation)?;
        if !Weak::ptr_eq(&token.queues, &Arc::downgrade(&self.queues)) {
            return Err(ScanFailureOutboxError::InvalidReservation);
        }
        let index = queues
            .scan_failures
            .iter()
            .position(|slot| Arc::ptr_eq(&slot.identity, identity) && slot.attached)
            .ok_or(ScanFailureOutboxError::InvalidReservation)?;
        if queues.scan_failures[index].ordinal.is_none() {
            if self.is_sealed() {
                return Err(ScanFailureOutboxError::Sealed);
            }
            let occurrence = source
                .as_ref()
                .ok_or(ScanFailureOutboxError::InvalidReservation)?;
            let source_budget = occurrence
                .retained_source_capacity(SCAN_FAILURE_SOURCE_BYTES)
                .ok_or(ScanFailureOutboxError::Capacity)?;
            if (queues.scan_failures[index].source_pre_admitted
                && source_budget > queues.scan_failures[index].source_budget)
                || queues
                    .scan_failures
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != index)
                    .try_fold(source_budget, |sum, (_, slot)| {
                        sum.checked_add(slot.source_budget)
                    })
                    .is_none_or(|sum| sum > SCAN_FAILURE_SOURCE_BYTES)
            {
                return Err(ScanFailureOutboxError::Capacity);
            }
            if queues
                .scan_failures
                .iter()
                .any(|slot| slot.occurrence_identity == Some(occurrence.identity()))
            {
                return Err(ScanFailureOutboxError::Recovery(
                    RecoveryError::IdentityMismatch,
                ));
            }
            // A pre-prepared occurrence could have been admitted under larger
            // limits; require this reservation to perform its first preparation.
            if occurrence.members().first().is_some_and(|member| {
                !matches!(
                    occurrence.descriptor(*member),
                    Err(RecoveryError::NotPrepared)
                )
            }) {
                return Err(ScanFailureOutboxError::InvalidReservation);
            }
            // Construct the complete owner successor from a clone while the
            // producer still owns its exact occurrence.  The raw cut and this
            // envelope form one recoverable logical capture: a failed owner
            // write must not consume either the producer source or an ordinal.
            let ordinal = queues
                .ordinal_after(1)
                .ok_or(ScanFailureOutboxError::OrdinalExhausted)?;
            let mut successor = queues.scan_failures[index].clone();
            successor.ordinal = Some(ordinal);
            successor.source_budget = source_budget;
            successor.budget = successor.limits.max_bookkeeping_bytes as u64;
            successor.occurrence_identity = Some(occurrence.identity());
            successor.occurrence = Some(occurrence.clone());
            if let Some(durable) = queues.durable.as_ref() {
                // A configured journal is an ownership boundary. Persist the
                // exact raw source-time cut and its complete owner in one
                // SQLite transaction before taking the producer source or
                // advancing the session ordinal. A failed owner insert must
                // therefore leave neither a raw-only orphan nor a fence.
                let owner =
                    scan_owner_envelope(&successor).map_err(ScanFailureOutboxError::Durability)?;
                durable
                    .journal
                    .capture_scan_failure_occurrence_with_owner(
                        occurrence,
                        &owner,
                        successor.recovery_authority.map(|authority| {
                            ScanFailureRecordingRecoveryAuthority {
                                session_id: successor.session_id,
                                authority,
                            }
                        }),
                    )
                    .map_err(ScanFailureOutboxError::Durability)?;
            }
            // Install only after durable ownership succeeded, then move (not
            // clone) the caller's occurrence into the committed slot.
            successor.occurrence = source.take();
            queues.scan_failures[index] = successor;
            queues.ordinal = ordinal;
            queues.control.push_back(Entry {
                ordinal,
                record: Value::Null,
                bytes: 0,
                reservation: Some(Arc::clone(identity)),
            });
        } else if source.is_some() {
            return Err(ScanFailureOutboxError::InvalidReservation);
        }
        let (ordinal, result) = {
            let ordinal = queues.scan_failures[index]
                .ordinal
                .expect("captured ordinal");
            if queues.scan_failures[index].prepared {
                return Ok(ordinal);
            }
            let mut successor = queues.scan_failures[index].clone();
            let result = successor
                .occurrence
                .as_mut()
                .expect("captured occurrence")
                .prepare(successor.limits)
                .map_err(ScanFailureOutboxError::Recovery);
            if result.is_ok() {
                successor.prepared = true;
                // Retain only the frozen occurrence's actual bookkeeping bound.
                let bookkeeping = successor
                    .occurrence
                    .as_ref()
                    .expect("captured occurrence")
                    .prepared_bookkeeping_bytes()
                    .expect("prepared bookkeeping");
                successor.budget = bookkeeping as u64;
                persist_scan_owner_successor(&queues, index, &successor)
                    .map_err(ScanFailureOutboxError::Durability)?;
                queues.scan_failures[index] = successor;
            }
            (ordinal, result)
        };
        if result.is_err() {
            // Capturing the source consumes its source-time cut even if local
            // preparation rejects its representation.  A durable session must
            // retain that exact unresolved failure before any suffix can be
            // admitted, including after a restart.
            let failure = JournalError {
                kind: "source.failure",
                detail: "captured scan failure preparation remains unresolved",
                operation: "scan-failure.prepare",
                sqlite_extended_code: None,
                sqlite_code: None,
            };
            if let Some(durable) = queues.durable.as_mut() {
                durable.journal.mark_collection_failure(&failure);
                durable.failure = Some(failure);
            }
        }
        drop(queues);
        self.signal();
        result.map(|()| ordinal)
    }

    /// Save this capability before dropping a pending reservation. Dropping a
    /// captured reservation keeps both its fence and occurrence in the outbox.
    pub fn pending_scan_failure(
        &self,
        token: &ScanFailureReservation,
    ) -> Result<PendingScanFailure, ScanFailureOutboxError> {
        if !Weak::ptr_eq(&token.queues, &Arc::downgrade(&self.queues)) {
            return Err(ScanFailureOutboxError::InvalidReservation);
        }
        let identity = token
            .identity
            .as_ref()
            .ok_or(ScanFailureOutboxError::InvalidReservation)?;
        let queues = lock(&self.queues);
        let slot = queues
            .scan_failures
            .iter()
            .find(|slot| {
                Arc::ptr_eq(&slot.identity, identity) && slot.attached && slot.released.is_none()
            })
            .ok_or(ScanFailureOutboxError::InvalidReservation)?;
        Ok(PendingScanFailure {
            ordinal: slot
                .ordinal
                .ok_or(ScanFailureOutboxError::InvalidReservation)?,
            capability: slot.capability,
        })
    }

    /// Find the earliest detached retryable producer or finished unprepared
    /// source. Finished prepared owners use their receipt path instead.
    /// Authenticate discovery at the host boundary.
    #[must_use]
    pub fn detached_scan_failure(&self) -> Option<PendingScanFailure> {
        let queues = lock(&self.queues);
        queues
            .scan_failures
            .iter()
            .filter(|slot| {
                !slot.attached
                    && (!slot.producer_finished || !slot.prepared)
                    && slot.released.is_none()
                    && !slot.raw_released
            })
            .filter_map(|slot| {
                slot.ordinal.map(|ordinal| PendingScanFailure {
                    ordinal,
                    capability: slot.capability,
                })
            })
            .min_by_key(|pending| pending.ordinal)
    }

    /// Enumerate captured owners, including release tombstones. This does not
    /// cross an ordering fence or consume a receipt. The caller must authenticate
    /// the session before exposing the returned capabilities.
    #[must_use]
    pub fn scan_failure_recovery_owners(&self) -> Vec<ScanFailureRecoveryOwner> {
        let queues = lock(&self.queues);
        let mut owners: Vec<_> = queues
            .scan_failures
            .iter()
            .filter_map(|slot| {
                let ordinal = slot.ordinal?;
                Some(ScanFailureRecoveryOwner {
                    key: PendingScanFailure {
                        ordinal,
                        capability: slot.capability,
                    },
                    prepared: slot.prepared,
                    producer_finished: slot.producer_finished,
                    raw_retired: slot.raw_retired,
                    raw_released: slot.raw_released,
                    member_count: slot
                        .occurrence
                        .as_ref()
                        .map_or(0, |source| source.members().len()),
                    released: slot.released,
                })
            })
            .collect();
        owners.sort_unstable_by_key(|owner| owner.key.ordinal);
        owners
    }

    /// True only after this outbox has no owner and its attached private
    /// journal has neither a resumable v2 owner envelope nor a finalized
    /// cleanup receipt. Hosts use this exact durable predicate before
    /// discarding a recovery lease.
    pub fn scan_failure_recovery_is_fully_retired(&self) -> Result<bool, ScanFailureOutboxError> {
        let queues = lock(&self.queues);
        if !queues.scan_failures.is_empty() {
            return Ok(false);
        }
        let Some(durable) = queues.durable.as_ref() else {
            return Ok(true);
        };
        durable
            .journal
            .scan_failure_recovery_is_empty()
            .map_err(ScanFailureOutboxError::Durability)
    }

    /// Read one member without allocating the full membership cut.
    pub fn scan_failure_recovery_member(
        &self,
        key: &PendingScanFailure,
        index: usize,
    ) -> Result<ScanFailureMember, ScanFailureOutboxError> {
        let queues = lock(&self.queues);
        let slot = queues
            .scan_failures
            .iter()
            .find(|slot| slot.ordinal == Some(key.ordinal) && slot.capability == key.capability)
            .ok_or(ScanFailureOutboxError::NotHead)?;
        slot.occurrence
            .as_ref()
            .and_then(|source| source.members().get(index))
            .copied()
            .ok_or(ScanFailureOutboxError::Recovery(
                RecoveryError::InvalidOffset,
            ))
    }

    /// Reattach exactly one canceled owner, including after seal.
    pub fn resume_scan_failure(
        &self,
        pending: PendingScanFailure,
    ) -> Result<ScanFailureReservation, ScanFailureOutboxError> {
        let mut queues = lock(&self.queues);
        let slot = queues
            .scan_failures
            .iter_mut()
            .find(|slot| {
                slot.ordinal == Some(pending.ordinal)
                    && slot.capability == pending.capability
                    && !slot.attached
                    && !slot.producer_finished
                    && slot.released.is_none()
            })
            .ok_or(ScanFailureOutboxError::InvalidReservation)?;
        slot.attached = true;
        Ok(ScanFailureReservation {
            queues: Arc::downgrade(&self.queues),
            identity: Some(Arc::clone(&slot.identity)),
        })
    }

    /// Confirm the producer no longer needs to retry commit. For a failed
    /// preparation this also permits the authority to retire after raw export.
    /// Cancellation only detaches; a detached producer can reattach and retry.
    pub fn finish_scan_failure_producer(
        &self,
        token: &mut ScanFailureReservation,
    ) -> Result<(), ScanFailureOutboxError> {
        if !Weak::ptr_eq(&token.queues, &Arc::downgrade(&self.queues)) {
            return Err(ScanFailureOutboxError::InvalidReservation);
        }
        let identity = Arc::clone(
            token
                .identity
                .as_ref()
                .ok_or(ScanFailureOutboxError::InvalidReservation)?,
        );
        let mut queues = lock(&self.queues);
        let index = queues
            .scan_failures
            .iter()
            .position(|slot| {
                Arc::ptr_eq(&slot.identity, &identity)
                    && slot.attached
                    && slot.ordinal.is_some()
                    && slot.released.is_none()
            })
            .ok_or(ScanFailureOutboxError::InvalidReservation)?;
        let mut successor = queues.scan_failures[index].clone();
        successor.producer_finished = true;
        successor.attached = false;
        persist_scan_owner_successor(&queues, index, &successor)
            .map_err(ScanFailureOutboxError::Durability)?;
        queues.scan_failures[index] = successor;
        token.identity = None;
        Ok(())
    }

    /// Inspect the unprepared head even when validation can never succeed.
    /// The host must authenticate this capability and retain the outbox.
    pub fn inspect_scan_failure(
        &self,
        key: &PendingScanFailure,
    ) -> Result<ScanFailureRawInfo, ScanFailureOutboxError> {
        let queues = lock(&self.queues);
        let occurrence = raw_scan_occurrence(&queues, key)?;
        let tokens = occurrence
            .source_tokens()
            .ok_or(ScanFailureOutboxError::NotHead)?;
        Ok(ScanFailureRawInfo {
            identity: occurrence.identity(),
            member_count: occurrence.members().len(),
            token_count: tokens.len(),
        })
    }

    pub fn inspect_scan_failure_member(
        &self,
        key: &PendingScanFailure,
        index: usize,
    ) -> Result<ScanFailureMember, ScanFailureOutboxError> {
        let queues = lock(&self.queues);
        raw_scan_occurrence(&queues, key)?
            .members()
            .get(index)
            .copied()
            .ok_or(ScanFailureOutboxError::Recovery(
                RecoveryError::InvalidOffset,
            ))
    }

    /// Read one token, with text and byte payloads sliced to at most 64 KiB.
    /// Text is returned as bytes so a window may split a UTF-8 code point.
    pub fn inspect_scan_failure_token(
        &self,
        key: &PendingScanFailure,
        index: usize,
        offset: usize,
        max_bytes: usize,
    ) -> Result<ScanFailureRawToken, ScanFailureOutboxError> {
        if max_bytes == 0 || max_bytes > MAX_CHUNK_BYTES as usize {
            return Err(ScanFailureOutboxError::Recovery(
                RecoveryError::InvalidLimits,
            ));
        }
        let queues = lock(&self.queues);
        let token = raw_scan_occurrence(&queues, key)?
            .source_tokens()
            .and_then(|tokens| tokens.get(index))
            .ok_or(ScanFailureOutboxError::Recovery(
                RecoveryError::InvalidOffset,
            ))?;
        let bytes = |value: &[u8]| -> Result<Vec<u8>, ScanFailureOutboxError> {
            if offset > value.len() {
                return Err(ScanFailureOutboxError::Recovery(
                    RecoveryError::InvalidOffset,
                ));
            }
            Ok(value[offset..value.len().min(offset.saturating_add(max_bytes))].to_vec())
        };
        if offset != 0 && !matches!(token, EvidenceToken::Text(_) | EvidenceToken::Bytes(_)) {
            return Err(ScanFailureOutboxError::Recovery(
                RecoveryError::InvalidOffset,
            ));
        }
        Ok(match token {
            EvidenceToken::Null => ScanFailureRawToken::Null,
            EvidenceToken::Bool(value) => ScanFailureRawToken::Bool(*value),
            EvidenceToken::I64(value) => ScanFailureRawToken::I64(*value),
            EvidenceToken::U64(value) => ScanFailureRawToken::U64(*value),
            EvidenceToken::F64Bits(value) => ScanFailureRawToken::F64Bits(*value),
            EvidenceToken::Text(value) => ScanFailureRawToken::Text {
                total: value.len(),
                bytes: bytes(value.as_bytes())?,
            },
            EvidenceToken::Bytes(value) => ScanFailureRawToken::Bytes {
                total: value.len(),
                bytes: bytes(value)?,
            },
            EvidenceToken::Map(value) => ScanFailureRawToken::Map(*value),
            EvidenceToken::Array(value) => ScanFailureRawToken::Array(*value),
        })
    }

    /// Attest that the complete raw source, identity and ordered membership
    /// were durably received by the caller. This releases the ordering fence,
    /// while retaining the source and exact receipt for retries. The outbox
    /// cannot verify the caller's persistence; the host owns that assertion.
    pub fn retire_unprepared_scan_failure(
        &self,
        key: &PendingScanFailure,
    ) -> Result<(), ScanFailureOutboxError> {
        let mut queues = lock(&self.queues);
        let index = queues
            .scan_failures
            .iter()
            .position(|slot| {
                slot.ordinal == Some(key.ordinal)
                    && slot.capability == key.capability
                    && !slot.prepared
            })
            .ok_or(ScanFailureOutboxError::NotHead)?;
        if !queues.scan_failures[index].producer_finished {
            return Err(ScanFailureOutboxError::InvalidReservation);
        }
        if queues.scan_failures[index].raw_retired {
            return Ok(());
        }
        if ordered_entries(&queues)
            .next()
            .is_none_or(|entry| entry.ordinal != key.ordinal)
        {
            return Err(ScanFailureOutboxError::NotHead);
        }
        let mut successor = queues.scan_failures[index].clone();
        successor.raw_retired = true;
        persist_scan_owner_successor(&queues, index, &successor)
            .map_err(ScanFailureOutboxError::Durability)?;
        queues.scan_failures[index] = successor;
        queues.control.pop_front();
        drop(queues);
        self.signal();
        Ok(())
    }

    /// Release the exported source after the raw retirement reply was observed;
    /// the exact release stays replayable until explicit confirmation.
    pub fn release_unprepared_scan_failure_receipt(
        &self,
        key: &PendingScanFailure,
    ) -> Result<(), ScanFailureOutboxError> {
        let mut queues = lock(&self.queues);
        let index = queues
            .scan_failures
            .iter()
            .position(|slot| {
                slot.ordinal == Some(key.ordinal)
                    && slot.capability == key.capability
                    && slot.raw_retired
            })
            .ok_or(ScanFailureOutboxError::NotHead)?;
        if queues.scan_failures[index].raw_released {
            return Ok(());
        }
        // Persist the exact release tombstone while the raw cut is still held
        // in memory. A failed write must leave the cut and its ordering proof
        // intact for retry; only a committed tombstone may discard it.
        let mut successor = queues.scan_failures[index].clone();
        successor.raw_released = true;
        successor.occurrence = None;
        successor.source_budget = 0;
        successor.budget = 0;
        // Persist the exact post-release tombstone. The original slot still
        // holds the raw cut until this CAS succeeds.
        persist_scan_owner_successor(&queues, index, &successor)
            .map_err(ScanFailureOutboxError::Durability)?;
        queues.scan_failures[index] = successor;
        Ok(())
    }

    /// Confirm the raw release; retain its tombstone until final cleanup.
    pub fn confirm_unprepared_scan_failure_release(
        &self,
        key: &PendingScanFailure,
    ) -> Result<ScanFailureReleaseConfirmation, ScanFailureOutboxError> {
        let queues = lock(&self.queues);
        let slot = queues
            .scan_failures
            .iter()
            .find(|slot| {
                slot.ordinal == Some(key.ordinal)
                    && slot.capability == key.capability
                    && slot.raw_released
            })
            .ok_or(ScanFailureOutboxError::NotHead)?;
        Ok(scan_release_confirmation(
            &slot.cleanup_key,
            key.ordinal,
            key.capability,
            true,
        ))
    }

    /// Returns a descriptor only after every earlier outbox entry is removed.
    /// A captured head that has not prepared returns `Recovery(NotPrepared)`.
    /// The caller must authenticate the member and budget transport framing.
    pub fn scan_failure_head(
        &self,
        member: ScanFailureMember,
    ) -> Result<Option<ScanFailureHead>, ScanFailureOutboxError> {
        let queues = lock(&self.queues);
        let Some(entry) = ordered_entries(&queues).next() else {
            return Ok(None);
        };
        let Some(slot) = queues
            .scan_failures
            .iter()
            .find(|slot| slot.ordinal == Some(entry.ordinal))
        else {
            return Ok(None);
        };
        let occurrence = slot
            .occurrence
            .as_ref()
            .ok_or(ScanFailureOutboxError::NotHead)?;
        let evidence = occurrence
            .descriptor(member)
            .map_err(ScanFailureOutboxError::Recovery)?;
        let (contiguous_read, acknowledged) = occurrence
            .receipt_progress(&evidence)
            .map_err(ScanFailureOutboxError::Recovery)?;
        Ok(Some(ScanFailureHead {
            ordinal: entry.ordinal,
            capability: slot.capability,
            evidence,
            contiguous_read,
            acknowledged,
        }))
    }

    /// Captured owners, including those awaiting preparation or receipt-lease
    /// release. A host must retain the outbox while this is nonzero, including
    /// across session disposal and seal. Once final cleanup removes the last
    /// tombstone, the host must independently retain the outbox while any
    /// cleanup response can still be retried. Process death loses this memory.
    #[must_use]
    pub fn pending_scan_failure_owners(&self) -> usize {
        lock(&self.queues)
            .scan_failures
            .iter()
            .filter(|slot| slot.ordinal.is_some())
            .count()
    }

    /// Reads do not release the occurrence. A lost response can be retried.
    pub fn read_scan_failure(
        &self,
        head: &ScanFailureHead,
        offset: u64,
    ) -> Result<EvidenceChunk, ScanFailureOutboxError> {
        let mut queues = lock(&self.queues);
        if ordered_entries(&queues)
            .next()
            .is_none_or(|entry| entry.ordinal != head.ordinal)
        {
            return Err(ScanFailureOutboxError::NotHead);
        }
        let index = queues
            .scan_failures
            .iter()
            .position(|slot| {
                slot.ordinal == Some(head.ordinal) && slot.capability == head.capability
            })
            .ok_or(ScanFailureOutboxError::NotHead)?;
        let mut successor = queues.scan_failures[index].clone();
        let chunk = successor
            .occurrence
            .as_mut()
            .ok_or(ScanFailureOutboxError::NotHead)?
            .read(&head.evidence, offset)
            .map_err(ScanFailureOutboxError::Recovery)?;
        persist_scan_owner_successor(&queues, index, &successor)
            .map_err(ScanFailureOutboxError::Durability)?;
        queues.scan_failures[index] = successor;
        Ok(chunk)
    }

    /// ACK and retire retain exact receipts for lost-response retries.
    pub fn acknowledge_scan_failure(
        &self,
        head: &ScanFailureHead,
    ) -> Result<(), ScanFailureOutboxError> {
        let mut queues = lock(&self.queues);
        let index = queues
            .scan_failures
            .iter()
            .position(|slot| {
                slot.ordinal == Some(head.ordinal) && slot.capability == head.capability
            })
            .ok_or(ScanFailureOutboxError::NotHead)?;
        let mut successor = queues.scan_failures[index].clone();
        let occurrence = successor
            .occurrence
            .as_mut()
            .ok_or(ScanFailureOutboxError::NotHead)?;
        occurrence
            .acknowledge(&head.evidence)
            .map_err(ScanFailureOutboxError::Recovery)?;
        successor.source_budget = occurrence
            .retained_source_capacity(SCAN_FAILURE_SOURCE_BYTES)
            .expect("retained capacity cannot increase after acknowledgement");
        successor.budget = occurrence
            .prepared_bookkeeping_bytes()
            .expect("prepared occurrence") as u64;
        persist_scan_owner_successor(&queues, index, &successor)
            .map_err(ScanFailureOutboxError::Durability)?;
        queues.scan_failures[index] = successor;
        Ok(())
    }

    pub fn retire_scan_failure(
        &self,
        head: &ScanFailureHead,
    ) -> Result<(), ScanFailureOutboxError> {
        let mut queues = lock(&self.queues);
        let index = queues
            .scan_failures
            .iter()
            .position(|slot| {
                slot.ordinal == Some(head.ordinal) && slot.capability == head.capability
            })
            .ok_or(ScanFailureOutboxError::NotHead)?;
        let mut successor = queues.scan_failures[index].clone();
        let occurrence = successor
            .occurrence
            .as_mut()
            .ok_or(ScanFailureOutboxError::NotHead)?;
        occurrence
            .retire(&head.evidence)
            .map_err(ScanFailureOutboxError::Recovery)?;
        if occurrence.is_retired()
            && queues
                .control
                .front()
                .is_some_and(|entry| entry.ordinal == head.ordinal)
        {
            persist_scan_owner_successor(&queues, index, &successor)
                .map_err(ScanFailureOutboxError::Durability)?;
            queues.scan_failures[index] = successor;
            queues.control.pop_front();
            drop(queues);
            self.signal();
        } else {
            persist_scan_owner_successor(&queues, index, &successor)
                .map_err(ScanFailureOutboxError::Durability)?;
            queues.scan_failures[index] = successor;
        }
        Ok(())
    }

    /// Release the occurrence after the producer explicitly finishes and all
    /// retirement replies were observed. The exact release remains replayable
    /// until the authenticated lease owner explicitly confirms its receipt.
    pub fn release_scan_failure_receipt(
        &self,
        head: &ScanFailureHead,
    ) -> Result<(), ScanFailureOutboxError> {
        let mut queues = lock(&self.queues);
        let index = queues
            .scan_failures
            .iter()
            .position(|slot| {
                slot.ordinal == Some(head.ordinal) && slot.capability == head.capability
            })
            .ok_or(ScanFailureOutboxError::NotHead)?;
        if let Some(released) = queues.scan_failures[index].released {
            return if same_scan_failure_head_identity(&released, head) {
                Ok(())
            } else {
                Err(ScanFailureOutboxError::NotHead)
            };
        }
        if !queues.scan_failures[index].producer_finished {
            return Err(ScanFailureOutboxError::InvalidReservation);
        }
        let mut successor = queues.scan_failures[index].clone();
        let occurrence = successor
            .occurrence
            .as_mut()
            .ok_or(ScanFailureOutboxError::NotHead)?;
        if !matches!(
            occurrence.descriptor(head.evidence.member),
            Err(RecoveryError::Retired)
        ) {
            return Err(ScanFailureOutboxError::Recovery(RecoveryError::Incomplete));
        }
        occurrence
            .retire(&head.evidence)
            .map_err(ScanFailureOutboxError::Recovery)?;
        if !occurrence.is_retired() {
            return Err(ScanFailureOutboxError::Recovery(RecoveryError::Incomplete));
        }
        // A release is permitted only after all receipts retired. Canonicalize
        // this terminal observation so the in-memory tombstone agrees with
        // the durable rehydration path, which no longer retains a live receipt
        // bitmap. The original head remains valid for confirmation because its
        // identity, rather than an earlier progress observation, authorizes it.
        successor.released = Some(ScanFailureHead {
            contiguous_read: head.evidence.encoded_length,
            acknowledged: true,
            ..*head
        });
        // The durable tombstone must describe the exact post-release state.
        // Writing it while retaining the raw source in the serialized owner
        // would make a successful release recover as an owner that still
        // claims source capacity after a process reopen.
        successor.occurrence = None;
        successor.budget = 0;
        successor.source_budget = 0;
        persist_scan_owner_successor(&queues, index, &successor)
            .map_err(ScanFailureOutboxError::Durability)?;
        queues.scan_failures[index] = successor;
        Ok(())
    }

    /// Confirm the exact prepared release; retain its tombstone until cleanup.
    pub fn confirm_scan_failure_receipt_release(
        &self,
        head: &ScanFailureHead,
    ) -> Result<ScanFailureReleaseConfirmation, ScanFailureOutboxError> {
        let queues = lock(&self.queues);
        let slot = queues
            .scan_failures
            .iter()
            .find(|slot| {
                slot.released
                    .as_ref()
                    .is_some_and(|released| same_scan_failure_head_identity(released, head))
            })
            .ok_or(ScanFailureOutboxError::NotHead)?;
        Ok(scan_release_confirmation(
            &slot.cleanup_key,
            head.ordinal,
            head.capability,
            false,
        ))
    }

    /// After observing confirmation, discard its tombstone. This exact proof
    /// makes lost cleanup replies retryable without unbounded retained receipts.
    /// The host must retain this outbox until the cleanup response is observed,
    /// even when pending_scan_failure_owners() is zero. Once finalized, earlier
    /// release/confirmation operations no longer replay.
    pub fn finalize_scan_failure_release(
        &self,
        receipt: &ScanFailureReleaseConfirmation,
    ) -> Result<(), ScanFailureOutboxError> {
        let mut queues = lock(&self.queues);
        let cleanup_receipt = scan_cleanup_receipt_id(receipt);
        if let Some(index) = queues
            .scan_failures
            .iter()
            .position(|slot| slot.ordinal == Some(receipt.ordinal))
        {
            let slot = &queues.scan_failures[index];
            if slot.capability != receipt.capability
                || (if receipt.raw {
                    !slot.raw_released
                } else {
                    slot.released.is_none()
                })
            {
                return Err(ScanFailureOutboxError::NotHead);
            }
            scan_release_mac(
                &slot.cleanup_key,
                receipt.ordinal,
                &receipt.capability,
                receipt.raw,
            )
            .verify_slice(&receipt.authenticator)
            .map_err(|_| ScanFailureOutboxError::NotHead)?;
            if let (Some(durable), Some(identity)) = (&queues.durable, slot.occurrence_identity) {
                durable
                    .journal
                    .remove_scan_failure_owner(
                        identity,
                        &cleanup_receipt,
                        slot.recovery_authority.map(|authority| {
                            ScanFailureRecordingRecoveryAuthority {
                                session_id: slot.session_id,
                                authority,
                            }
                        }),
                    )
                    .map_err(ScanFailureOutboxError::Durability)?;
            }
            queues.scan_failures.remove(index);
        } else {
            let Some(durable) = queues.durable.as_ref() else {
                return Err(ScanFailureOutboxError::NotHead);
            };
            if !durable
                .journal
                .has_scan_failure_cleanup_receipt(&cleanup_receipt)
                .map_err(ScanFailureOutboxError::Durability)?
            {
                return Err(ScanFailureOutboxError::NotHead);
            }
        }
        Ok(())
    }

    /// Finalize a release and return the opaque capability required to reclaim
    /// its durable replay receipt. Unlike the legacy finalization method, the
    /// caller can explicitly bound sidecar retention without giving up exact
    /// lost-response retry semantics.
    pub fn finalize_scan_failure_release_with_retirement(
        &self,
        receipt: &ScanFailureReleaseConfirmation,
    ) -> Result<ScanFailureCleanupRetirement, ScanFailureOutboxError> {
        self.finalize_scan_failure_release(receipt)?;
        let cleanup_receipt = scan_cleanup_receipt_id(receipt);
        let journal = lock(&self.queues)
            .durable
            .as_ref()
            .map(|durable| Arc::clone(&durable.journal))
            .ok_or(ScanFailureOutboxError::NotHead)?;
        let authenticator = journal
            .scan_failure_cleanup_retirement(&cleanup_receipt)
            .map_err(ScanFailureOutboxError::Durability)?;
        Ok(ScanFailureCleanupRetirement {
            cleanup_receipt,
            authenticator,
        })
    }

    /// Reacquire opaque retirement capabilities for finalized cleanups that
    /// remain active after a process restart. No receipt bytes or durable
    /// authority key cross this package boundary.
    pub fn scan_failure_cleanup_retirements(
        &self,
    ) -> Result<Vec<ScanFailureCleanupRetirement>, ScanFailureOutboxError> {
        let journal = lock(&self.queues)
            .durable
            .as_ref()
            .map(|durable| Arc::clone(&durable.journal))
            .ok_or(ScanFailureOutboxError::NotHead)?;
        journal
            .scan_failure_cleanup_retirements()
            .map(|retirements| {
                retirements
                    .into_iter()
                    .map(
                        |(cleanup_receipt, authenticator)| ScanFailureCleanupRetirement {
                            cleanup_receipt,
                            authenticator,
                        },
                    )
                    .collect()
            })
            .map_err(ScanFailureOutboxError::Durability)
    }

    /// Authenticated, exact and idempotent cleanup receipt retirement. A
    /// replay after a lost response succeeds only for this capability; an
    /// altered or cross-recording capability never becomes a false success.
    pub fn retire_scan_failure_cleanup(
        &self,
        retirement: &ScanFailureCleanupRetirement,
    ) -> Result<(), ScanFailureOutboxError> {
        let journal = lock(&self.queues)
            .durable
            .as_ref()
            .map(|durable| Arc::clone(&durable.journal))
            .ok_or(ScanFailureOutboxError::NotHead)?;
        journal
            .retire_scan_failure_cleanup_receipt(
                &retirement.cleanup_receipt,
                &retirement.authenticator,
            )
            .map_err(ScanFailureOutboxError::Durability)
    }

    /// Reserve before starting an operation whose terminal must be retained.
    /// The token is tied to this outbox and its unsealed lifecycle.
    pub fn reserve_control(&self) -> Result<ControlReservation, ControlReservationError> {
        let mut queues = lock(&self.queues);
        if self.is_sealed() {
            return Err(ControlReservationError::Sealed);
        }
        if queues.reservations.len() >= RESERVED_CONTROL_CAP {
            return Err(ControlReservationError::QuotaExceeded);
        }
        let identity = Arc::new(());
        queues.reservations.push(ReservationSlot {
            identity: identity.clone(),
            committed: false,
        });
        Ok(ControlReservation {
            queues: Arc::downgrade(&self.queues),
            identity: Some(identity),
        })
    }

    /// Retain exactly one terminal at this call's source ordinal. The caller
    /// keeps the original on rejection. Records must be JSON objects within
    /// `RESERVED_CONTROL_BYTES`; the producer must guarantee this before it
    /// starts the operation. Delivery may be retried via claims; quota remains
    /// charged until legacy drain or exact receiver ACK. A successful return
    /// means memory retention, not journal success: inspect `journal_error`.
    pub fn commit_reserved_control(
        &self,
        token: &mut ControlReservation,
        record: &Value,
    ) -> Result<ReservedControlCommit, ControlReservationError> {
        if !record.is_object() {
            return Err(ControlReservationError::InvalidRecord);
        }
        // Count through a bounded writer instead of allocating an encoded copy
        // of a caller-supplied, potentially enormous value.
        struct Limit(usize);
        impl std::io::Write for Limit {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() > self.0 {
                    return Err(std::io::Error::other("terminal exceeds reservation"));
                }
                self.0 -= bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        if serde_json::to_writer(Limit(RESERVED_CONTROL_BYTES), record).is_err() {
            return Err(ControlReservationError::RecordTooLarge);
        }
        let mut queues = lock(&self.queues);
        if !Weak::ptr_eq(&token.queues, &Arc::downgrade(&self.queues)) {
            return Err(ControlReservationError::InvalidToken);
        }
        let Some(identity) = token.identity.as_ref() else {
            return Err(ControlReservationError::InvalidToken);
        };
        let Some(index) = queues
            .reservations
            .iter()
            .position(|slot| !slot.committed && Arc::ptr_eq(&slot.identity, identity))
        else {
            return Err(ControlReservationError::InvalidToken);
        };
        if self.is_sealed() {
            return Err(ControlReservationError::Sealed);
        }
        let ordinal = queues
            .ordinal_after(1)
            .ok_or(ControlReservationError::OrdinalExhausted)?;
        let journal_error = queues
            .durable
            .as_mut()
            .and_then(|durable| Self::persist(durable, record).err());
        if let Some(error) = &journal_error {
            if error.kind == "storage.stopped" {
                self.sealed.store(true, Ordering::SeqCst);
                lock(&self.observer).take();
            } else {
                self.storage_terminal(record["consumer"].clone(), error);
            }
        }
        if record["t"] == "stream-end" {
            let mut observer = lock(&self.observer);
            if observer
                .as_ref()
                .is_some_and(|observer| record["consumer"] == observer.consumer)
                && let Some(observer) = observer.take()
            {
                let _ = observer.sender.send(record.clone());
            }
        }
        queues.reservations[index].committed = true;
        let identity = token.identity.take();
        let allocated = queues
            .allocate_ordinals(1)
            .expect("ordinal checked before journal commit");
        debug_assert_eq!(allocated, ordinal);
        queues.control.push_back(Entry {
            ordinal,
            record: record.clone(),
            bytes: 0,
            reservation: identity,
        });
        if self.is_sealed() {
            queues.reservations.retain(|slot| slot.committed);
        }
        drop(queues);
        self.signal();
        Ok(ReservedControlCommit { journal_error })
    }

    fn release_committed(queues: &mut Queues, entry: &Entry) {
        if let Some(identity) = &entry.reservation {
            queues
                .reservations
                .retain(|slot| !Arc::ptr_eq(&slot.identity, identity));
        }
    }

    /// Queue one control record.
    pub fn push_control(&self, record: Value) {
        self.push_control_record(record, false, false);
    }

    /// Admit a source terminal only before the collection cutoff.
    pub fn push_control_before_seal(&self, record: Value) {
        self.push_control_record(record, false, true);
    }

    // Journal admission precedes the memory optimization under the same lock
    // that orders journal attachment and sealing. Durable loss is append-only:
    // a prepared prefix can never be changed by coalescing a later delta.
    fn push_control_record(&self, record: Value, coalesce_loss: bool, before_seal: bool) {
        {
            let mut queues = lock(&self.queues);
            if before_seal && self.is_sealed() {
                return;
            }
            if queues.ordinal_after(1).is_none() {
                queues.control_lost = queues.control_lost.saturating_add(1);
                queues.control_lost_total = queues.control_lost_total.saturating_add(1);
                drop(queues);
                self.signal();
                return;
            }
            if !self.is_sealed()
                && let Some(durable) = queues.durable.as_mut()
                && let Err(error) = Self::persist(durable, &record)
            {
                if error.kind == "storage.stopped" {
                    self.sealed.store(true, Ordering::SeqCst);
                    queues.reservations.retain(|slot| slot.committed);
                    lock(&self.observer).take();
                } else {
                    self.storage_terminal(record["consumer"].clone(), &error);
                }
            }
            let mut observer = lock(&self.observer);
            if record["t"] == "stream-end"
                && observer
                    .as_ref()
                    .is_some_and(|observer| record["consumer"] == observer.consumer)
                && let Some(observer) = observer.take()
            {
                // The terminal is still retained (or loss-accounted) below.
                let _ = observer.sender.send(record.clone());
            }
            let claimed_through = queues
                .pending_drain
                .as_ref()
                .map(|claim| claim.last_ordinal);
            let coalesced = coalesce_loss
                && queues.control.back_mut().is_some_and(|tail| {
                    if tail.reservation.is_some()
                        || claimed_through.is_some_and(|last| tail.ordinal <= last)
                    {
                        return false;
                    }
                    if tail.record["t"] != "ingress-drop" || tail.record["class"] != record["class"]
                    {
                        return false;
                    }
                    let total = tail.record["count"].as_u64().and_then(|previous| {
                        record["count"]
                            .as_u64()
                            .and_then(|delta| previous.checked_add(delta))
                    });
                    if let Some(total) = total
                        && let Value::Object(map) = &mut tail.record
                    {
                        map.insert("count".to_owned(), Value::from(total));
                        return true;
                    }
                    false
                });
            if !coalesced
                && queues
                    .control
                    .iter()
                    .filter(|entry| entry.reservation.is_none())
                    .count()
                    >= CONTROL_RECORD_CAP
            {
                queues.control_lost += 1;
                queues.control_lost_total += 1;
            } else if !coalesced {
                let ordinal = queues
                    .allocate_ordinals(1)
                    .expect("ordinal checked before journal commit");
                queues.control.push_back(Entry {
                    ordinal,
                    record,
                    bytes: 0,
                    reservation: None,
                });
            }
        }
        self.signal();
    }

    /// Report one ingress drop, coalescing into an undrained
    /// `ingress-drop` record of the same class at the control tail.
    pub fn push_ingress_drop(&self, class: IngressClass) {
        self.push_ingress_drop_count(class, 1);
    }

    /// Report `count` ingress drops at once (signal-overflow accounting),
    /// coalescing into an undrained `ingress-drop` record of the same class
    /// at the control tail. The journal admits each delta before coalescing.
    /// If a new memory record cannot fit, the drain counter reports one
    /// refused control record, independently of the delta's upstream count.
    pub fn push_ingress_drop_count(&self, class: IngressClass, count: u64) {
        if count == 0 {
            return;
        }
        self.push_control_record(
            object(vec![
                ("t", Value::from("ingress-drop")),
                ("class", Value::from(class.as_str())),
                ("count", Value::from(count)),
            ]),
            true,
            false,
        );
    }

    /// Queued data records (retained byte buffers).
    #[must_use]
    pub fn queued_data(&self) -> usize {
        // Durable values have their own cursor and never enter this queue.
        // Runtime diagnostics must not wait on the journal's commit mutex.
        if self.has_journal() {
            return 0;
        }
        lock(&self.queues).data.len()
    }

    /// Inspect the retained ordered queue without selecting a drain mode,
    /// assigning an ordinal, creating a claim, or changing wake state.
    pub fn has_retained_tail(&self) -> Result<bool, DrainClaimError> {
        let queues = lock(&self.queues);
        if matches!(
            queues.drain_mode,
            DrainMode::Legacy | DrainMode::PreservedLegacy
        ) {
            return Err(DrainClaimError::ModeConflict);
        }
        Ok(ordered_entries(&queues).next().is_some() || queues.control_lost > 0)
    }

    /// Retain an ordered prefix until the caller acknowledges its exact ID.
    /// Repeated claims return the same prefix even if their requested bounds
    /// differ. `max_bytes` bounds the serialized records array, not a host's
    /// outer wire envelope; a transport must also budget its own framing.
    pub fn claim_drain(
        &self,
        max_items: usize,
        max_bytes: usize,
    ) -> Result<Option<DrainClaim>, DrainClaimError> {
        let mut queues = lock(&self.queues);
        if matches!(
            queues.drain_mode,
            DrainMode::Legacy | DrainMode::PreservedLegacy
        ) {
            return Err(DrainClaimError::ModeConflict);
        }
        queues.drain_mode = DrainMode::Claimed;
        if let Some(pending) = &queues.pending_drain {
            return Ok(Some(Self::replay_claim(pending)));
        }

        // Give the eventual in-band control-loss report an ordinal only when
        // all older entries have gone; it then obeys the same claim rules.
        if queues.data.is_empty() && queues.control.is_empty() && queues.control_lost > 0 {
            let ordinal = queues
                .allocate_ordinals(1)
                .ok_or(DrainClaimError::IdentityExhausted)?;
            let count = std::mem::take(&mut queues.control_lost);
            queues.control.push_back(Entry {
                ordinal,
                bytes: 0,
                reservation: None,
                record: object(vec![
                    ("t", Value::from("ingress-drop")),
                    ("class", Value::from(IngressClass::Control.as_str())),
                    ("count", Value::from(count)),
                ]),
            });
        }

        let mut records = Vec::new();
        let mut bytes = 2usize; // the JSON array brackets
        let mut last_ordinal = 0;
        for entry in ordered_entries(&queues).take(max_items.max(1)) {
            if queues
                .scan_failures
                .iter()
                .any(|slot| slot.ordinal == Some(entry.ordinal))
            {
                if records.is_empty() {
                    self.armed.store(true, Ordering::SeqCst);
                    return Err(DrainClaimError::ModeConflict);
                }
                break;
            }
            let record = with_ordinal(entry.record.clone(), entry.ordinal);
            let required = bytes
                .saturating_add(record.to_string().len())
                .saturating_add(usize::from(!records.is_empty()));
            if required > max_bytes {
                if records.is_empty() {
                    return Err(DrainClaimError::RecordTooLarge {
                        required_bytes: required,
                    });
                }
                break;
            }
            bytes = required;
            last_ordinal = entry.ordinal;
            records.push(record);
        }

        if records.is_empty() {
            drop(queues);
            // Arm before checking again so a racing producer cannot lose its
            // wake. The caller must wait for that wake after an empty claim.
            self.armed.store(true, Ordering::SeqCst);
            let refilled = {
                let queues = lock(&self.queues);
                !(queues.data.is_empty() && queues.control.is_empty() && queues.control_lost == 0)
            };
            if refilled {
                self.signal();
            }
            return Ok(None);
        }
        let id = NEXT_DRAIN_CLAIM
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1))
            .map_err(|_| DrainClaimError::IdentityExhausted)?;
        let control_lost = queues.control_lost_total;
        let pending = PendingDrain {
            id,
            last_ordinal,
            control_lost,
            records: records.clone(),
        };
        queues.pending_drain = Some(pending);
        Ok(Some(DrainClaim {
            id,
            records,
            control_lost,
        }))
    }

    fn replay_claim(pending: &PendingDrain) -> DrainClaim {
        DrainClaim {
            id: pending.id,
            records: pending.records.clone(),
            control_lost: pending.control_lost,
        }
    }

    /// Claim a raw prefix while exposing only a deterministic strict view.
    ///
    /// The raw entries deliberately remain in the outbox until
    /// `acknowledge_drain`; `owner` receives complete originals independently
    /// so a 5.2.1 reader can page facts it cannot decode.  A retry returns the
    /// same stored view, even if a caller subsequently requests another
    /// protocol version or different bounds.
    pub fn claim_drain_projected<F>(
        &self,
        owner: &LegacyEvidenceOwner,
        max_items: usize,
        max_bytes: usize,
        project: F,
    ) -> Result<Option<DrainClaim>, LegacyEvidenceError>
    where
        F: Fn(&Value) -> Result<Option<Value>, ()>,
    {
        let mut queues = lock(&self.queues);
        if matches!(
            queues.drain_mode,
            DrainMode::Legacy | DrainMode::PreservedLegacy
        ) {
            return Err(LegacyEvidenceError::ModeConflict);
        }
        if queues
            .evidence_owner
            .as_ref()
            .is_some_and(|id| !Arc::ptr_eq(id, &owner.identity))
        {
            return Err(LegacyEvidenceError::OwnerMismatch);
        }
        if let Some(pending) = &queues.pending_drain {
            return Ok(Some(Self::replay_claim(pending)));
        }
        let mut evidence = lock(&owner.state);
        if evidence
            .source
            .as_ref()
            .is_some_and(|source| !Weak::ptr_eq(source, &Arc::downgrade(&self.queues)))
        {
            return Err(LegacyEvidenceError::OwnerMismatch);
        }

        // Match normal claim selection exactly: the raw source byte bound
        // determines which prefix is owned, while the visible projection is
        // separately bounded by the caller's wire envelope.
        let mut originals = Vec::new();
        let mut records = Vec::new();
        let mut raw_bytes = 2usize;
        let mut view_bytes = 2usize;
        let mut last_ordinal = 0;
        for entry in ordered_entries(&queues).take(max_items.max(1)) {
            if queues
                .scan_failures
                .iter()
                .any(|slot| slot.ordinal == Some(entry.ordinal))
            {
                if originals.is_empty() {
                    return Err(LegacyEvidenceError::ModeConflict);
                }
                break;
            }
            let original = with_ordinal(entry.record.clone(), entry.ordinal);
            let raw_required = raw_bytes
                .saturating_add(original.to_string().len())
                .saturating_add(usize::from(!originals.is_empty()));
            if raw_required > max_bytes {
                if originals.is_empty() {
                    return Err(LegacyEvidenceError::RecordTooLarge {
                        required_bytes: raw_required,
                    });
                }
                break;
            }
            let projected =
                project(&original).map_err(|()| LegacyEvidenceError::InvalidProjection)?;
            if let Some(value) = &projected {
                if value.get("ordinal").and_then(Value::as_u64) != Some(entry.ordinal) {
                    return Err(LegacyEvidenceError::InvalidProjection);
                }
                let required = view_bytes
                    .saturating_add(value.to_string().len())
                    .saturating_add(usize::from(!records.is_empty()));
                if required > max_bytes {
                    if originals.is_empty() {
                        return Err(LegacyEvidenceError::RecordTooLarge {
                            required_bytes: required,
                        });
                    }
                    break;
                }
                view_bytes = required;
            }
            raw_bytes = raw_required;
            last_ordinal = entry.ordinal;
            let bytes = original.to_string().len();
            originals.push((entry.ordinal, original, bytes));
            if let Some(value) = projected {
                records.push(value);
            }
        }
        if originals.is_empty() {
            drop(evidence);
            drop(queues);
            return self
                .claim_drain(max_items, max_bytes)
                .map_err(|error| match error {
                    DrainClaimError::RecordTooLarge { required_bytes } => {
                        LegacyEvidenceError::RecordTooLarge { required_bytes }
                    }
                    DrainClaimError::IdentityExhausted => LegacyEvidenceError::OrdinalExhausted,
                    DrainClaimError::ModeConflict => LegacyEvidenceError::ModeConflict,
                });
        }
        let retained_bytes = originals.iter().map(|(_, _, bytes)| *bytes).sum::<usize>();
        if evidence.records.len().saturating_add(originals.len()) > evidence.max_items
            || evidence.bytes.saturating_add(retained_bytes) > evidence.max_bytes
        {
            return Err(LegacyEvidenceError::Capacity);
        }
        let id = NEXT_DRAIN_CLAIM
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1))
            .map_err(|_| LegacyEvidenceError::OrdinalExhausted)?;
        // This reservation happens while the source prefix is still queued.
        // A shared-capacity refusal therefore leaves both the source and its
        // exact recovery path intact for a later retry.
        if let Some(budget) = &owner.budget
            && !budget.reserve(originals.len(), retained_bytes)
        {
            return Err(LegacyEvidenceError::Capacity);
        }
        queues.drain_mode = DrainMode::Claimed;
        queues.evidence_owner = Some(Arc::clone(&owner.identity));
        evidence.source = Some(Arc::downgrade(&self.queues));
        evidence.bytes += retained_bytes;
        evidence.records.extend(originals);
        let pending = PendingDrain {
            id,
            last_ordinal,
            control_lost: queues.control_lost_total,
            records,
        };
        queues.pending_drain = Some(pending.clone());
        Ok(Some(Self::replay_claim(&pending)))
    }

    /// Acknowledge only this exact prefix. Repeating the most recent ACK is
    /// idempotent, allowing a caller to recover a lost acknowledgement reply.
    pub fn acknowledge_drain(&self, id: u64) -> bool {
        let mut queues = lock(&self.queues);
        if queues.last_acknowledged_drain == Some(id) {
            return true;
        }
        let Some(last_ordinal) = queues
            .pending_drain
            .as_ref()
            .filter(|pending| pending.id == id)
            .map(|pending| pending.last_ordinal)
        else {
            return false;
        };
        while queues
            .data
            .front()
            .is_some_and(|entry| entry.ordinal <= last_ordinal)
        {
            if let Some(entry) = queues.data.pop_front() {
                queues.data_bytes = queues.data_bytes.saturating_sub(entry.bytes);
            }
        }
        while queues
            .control
            .front()
            .is_some_and(|entry| entry.ordinal <= last_ordinal)
        {
            if let Some(entry) = queues.control.pop_front() {
                Self::release_committed(&mut queues, &entry);
            }
        }
        queues.pending_drain = None;
        // A projected V1 claim transfers its exact originals to the
        // claim-token-owned evidence object before this ACK can remove the
        // source prefix.  The identity fence only protects the *pending*
        // source prefix from a competing evidence owner; retaining it after
        // the prefix is removed would incorrectly reject the next V1 claim
        // (each claim deliberately has its own token-scoped recovery page).
        // The earlier evidence owner remains independently reachable through
        // its claim token and cannot be replaced or retired by this release.
        queues.evidence_owner = None;
        queues.last_acknowledged_drain = Some(id);
        true
    }

    /// Atomically transfer a legacy drain prefix into a separately retained
    /// evidence owner before releasing it from this outbox. `project` must be
    /// a non-reentrant, deterministic 5.2.1 codec: return None for a fact that
    /// has no truthful legacy representation. This primitive validates only the
    /// ordinal; it does not assert that the projected shape is codec-compatible.
    /// Both limits and first-record behavior match the legacy source drain.
    /// A capacity or projection failure leaves the entire prefix in the source.
    /// Journal-backed data is owned by the separate recording cursor and is
    /// neither transferred nor acknowledged by this outbox operation.
    pub fn drain_legacy_preserving<F>(
        &self,
        owner: &LegacyEvidenceOwner,
        max_items: usize,
        max_bytes: usize,
        project: F,
    ) -> Result<Value, LegacyEvidenceError>
    where
        F: Fn(&Value) -> Result<Option<Value>, ()>,
    {
        let mut queues = lock(&self.queues);
        if matches!(queues.drain_mode, DrainMode::Legacy | DrainMode::Claimed) {
            return Err(LegacyEvidenceError::ModeConflict);
        }
        if queues
            .evidence_owner
            .as_ref()
            .is_some_and(|id| !Arc::ptr_eq(id, &owner.identity))
        {
            return Err(LegacyEvidenceError::OwnerMismatch);
        }
        let mut evidence = lock(&owner.state);
        if evidence
            .source
            .as_ref()
            .is_some_and(|source| !Weak::ptr_eq(source, &Arc::downgrade(&self.queues)))
        {
            return Err(LegacyEvidenceError::OwnerMismatch);
        }
        let mut originals = Vec::new();
        let mut projections = Vec::new();
        let mut data_bytes = 0usize;
        let mut retained_bytes = 0usize;
        for entry in ordered_entries(&queues).take(max_items.max(1)) {
            if queues
                .scan_failures
                .iter()
                .any(|slot| slot.ordinal == Some(entry.ordinal))
            {
                if originals.is_empty() {
                    self.armed.store(true, Ordering::SeqCst);
                    return Err(LegacyEvidenceError::ModeConflict);
                }
                break;
            }
            if !originals.is_empty() && data_bytes.saturating_add(entry.bytes) > max_bytes {
                break;
            }
            data_bytes = data_bytes.saturating_add(entry.bytes);
            let original = with_ordinal(entry.record.clone(), entry.ordinal);
            let projected =
                project(&original).map_err(|()| LegacyEvidenceError::InvalidProjection)?;
            if let Some(value) = projected {
                if value.get("ordinal").and_then(Value::as_u64) != Some(entry.ordinal) {
                    return Err(LegacyEvidenceError::InvalidProjection);
                }
                projections.push(value);
            }
            let bytes = original.to_string().len();
            retained_bytes = retained_bytes
                .checked_add(bytes)
                .ok_or(LegacyEvidenceError::Capacity)?;
            originals.push((entry.ordinal, original, bytes));
        }
        let mut synthetic_loss = false;
        if originals.len() < max_items.max(1)
            && originals.len() == queues.data.len() + queues.control.len()
            && queues.control_lost > 0
        {
            let ordinal = queues
                .ordinal_after(1)
                .ok_or(LegacyEvidenceError::OrdinalExhausted)?;
            let original = with_ordinal(
                object(vec![
                    ("t", Value::from("ingress-drop")),
                    ("class", Value::from(IngressClass::Control.as_str())),
                    ("count", Value::from(queues.control_lost)),
                ]),
                ordinal,
            );
            let projected =
                project(&original).map_err(|()| LegacyEvidenceError::InvalidProjection)?;
            if let Some(value) = projected {
                if value.get("ordinal").and_then(Value::as_u64) != Some(ordinal) {
                    return Err(LegacyEvidenceError::InvalidProjection);
                }
                projections.push(value);
            }
            let bytes = original.to_string().len();
            retained_bytes = retained_bytes
                .checked_add(bytes)
                .ok_or(LegacyEvidenceError::Capacity)?;
            originals.push((ordinal, original, bytes));
            synthetic_loss = true;
        }
        let total_items = evidence
            .records
            .len()
            .checked_add(originals.len())
            .ok_or(LegacyEvidenceError::Capacity)?;
        let total_bytes = evidence
            .bytes
            .checked_add(retained_bytes)
            .ok_or(LegacyEvidenceError::Capacity)?;
        if total_items > evidence.max_items || total_bytes > evidence.max_bytes {
            return Err(LegacyEvidenceError::Capacity);
        }
        // Reserve shared exact-evidence capacity while both the source and
        // this owner are still locked. A refusal happens before any source
        // entry is removed, so retry can preserve the same prefix exactly.
        evidence
            .records
            .try_reserve(originals.len())
            .map_err(|_| LegacyEvidenceError::Capacity)?;
        if let Some(budget) = &owner.budget
            && !budget.reserve(originals.len(), retained_bytes)
        {
            return Err(LegacyEvidenceError::Capacity);
        }
        queues.drain_mode = DrainMode::PreservedLegacy;
        queues.evidence_owner = Some(Arc::clone(&owner.identity));
        evidence.source = Some(Arc::downgrade(&self.queues));
        let last = originals.last().map(|(ordinal, _, _)| *ordinal);
        evidence.bytes = total_bytes;
        evidence.records.extend(originals);
        if let Some(last) = last {
            while queues
                .data
                .front()
                .is_some_and(|entry| entry.ordinal <= last)
            {
                let entry = queues.data.pop_front().expect("front exists");
                queues.data_bytes -= entry.bytes;
            }
            while queues
                .control
                .front()
                .is_some_and(|entry| entry.ordinal <= last)
            {
                let entry = queues.control.pop_front().expect("front exists");
                Self::release_committed(&mut queues, &entry);
            }
        }
        if synthetic_loss {
            queues
                .allocate_ordinals(1)
                .expect("synthetic ordinal checked before retention");
            queues.control_lost = 0;
        }
        let more =
            !(queues.data.is_empty() && queues.control.is_empty() && queues.control_lost == 0);
        let control_lost = queues.control_lost_total;
        drop(evidence);
        drop(queues);
        let more = if more {
            true
        } else {
            self.armed.store(true, Ordering::SeqCst);
            let queues = lock(&self.queues);
            let refilled =
                !(queues.data.is_empty() && queues.control.is_empty() && queues.control_lost == 0);
            refilled && self.armed.swap(false, Ordering::SeqCst)
        };
        Ok(object(vec![
            ("more", Value::Bool(more)),
            ("records", Value::Array(projections)),
            ("controlLost", Value::from(control_lost)),
        ]))
    }

    /// Take up to `max_items` records (at least one when any is waiting)
    /// within `max_bytes` of encoded data, in ordinal order.
    #[must_use]
    pub fn drain(&self, max_items: usize, max_bytes: usize) -> Value {
        let mut records = Vec::new();
        let mut taken_bytes = 0usize;
        let more = {
            let mut queues = lock(&self.queues);
            if matches!(
                queues.drain_mode,
                DrainMode::Claimed | DrainMode::PreservedLegacy
            ) {
                return object(vec![("error", Value::from("drain-mode-conflict"))]);
            }
            queues.drain_mode = DrainMode::Legacy;
            while records.len() < max_items.max(1) {
                let data_head = queues.data.front().map(|entry| entry.ordinal);
                let control_head = queues.control.front().map(|entry| entry.ordinal);
                let take_data = match (data_head, control_head) {
                    (Some(d), Some(c)) => d < c,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => break,
                };
                if !take_data
                    && control_head.is_some_and(|ordinal| {
                        queues
                            .scan_failures
                            .iter()
                            .any(|slot| slot.ordinal == Some(ordinal))
                    })
                {
                    if records.is_empty() {
                        self.armed.store(true, Ordering::SeqCst);
                        return object(vec![("error", Value::from("retained-scan-failure"))]);
                    }
                    break;
                }
                let next_bytes = if take_data {
                    queues.data.front().map_or(0, |entry| entry.bytes)
                } else {
                    0
                };
                if !records.is_empty() && taken_bytes + next_bytes > max_bytes {
                    break;
                }
                let entry = if take_data {
                    let entry = queues.data.pop_front();
                    if let Some(entry) = &entry {
                        queues.data_bytes = queues.data_bytes.saturating_sub(entry.bytes);
                    }
                    entry
                } else {
                    queues.control.pop_front()
                };
                if let Some(entry) = entry {
                    Self::release_committed(&mut queues, &entry);
                    taken_bytes += entry.bytes;
                    records.push(with_ordinal(entry.record, entry.ordinal));
                }
            }
            // The lost-control report takes the next ordinal, so it only goes
            // out once every earlier record has drained (ordinals increase).
            if queues.control_lost > 0
                && queues.data.is_empty()
                && queues.control.is_empty()
                && records.len() < max_items.max(1)
            {
                let Some(ordinal) = queues.allocate_ordinals(1) else {
                    if records.is_empty() {
                        return object(vec![("error", Value::from("ordinal-exhausted"))]);
                    }
                    return object(vec![
                        ("records", Value::Array(records)),
                        ("more", Value::Bool(true)),
                        ("controlLost", Value::from(queues.control_lost_total)),
                    ]);
                };
                let lost = std::mem::take(&mut queues.control_lost);
                records.push(with_ordinal(
                    object(vec![
                        ("t", Value::from("ingress-drop")),
                        ("class", Value::from(IngressClass::Control.as_str())),
                        ("count", Value::from(lost)),
                    ]),
                    ordinal,
                ));
            }
            !(queues.data.is_empty() && queues.control.is_empty() && queues.control_lost == 0)
        };
        let more = if more {
            true
        } else {
            // Arm, then re-check: a producer that pushed between the take
            // above and this store saw `armed == false` and did not wake.
            self.armed.store(true, Ordering::SeqCst);
            let refilled = {
                let queues = lock(&self.queues);
                !(queues.data.is_empty() && queues.control.is_empty() && queues.control_lost == 0)
            };
            refilled && self.armed.swap(false, Ordering::SeqCst)
        };
        let control_lost_total = lock(&self.queues).control_lost_total;
        object(vec![
            ("more", Value::Bool(more)),
            ("records", Value::Array(records)),
            ("controlLost", Value::from(control_lost_total)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::continuation_journal::JournalQuota;
    use serde_json::json;

    use super::*;

    struct NoWake;

    impl WakeSink for NoWake {
        fn wake(&self, _session_id: u64) {}
    }

    fn control(n: u64) -> Value {
        object(vec![("t", Value::from("lifecycle")), ("n", Value::from(n))])
    }

    fn data(n: u64) -> Value {
        object(vec![("t", Value::from("adv")), ("n", Value::from(n))])
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn terminal_outbox_insert_fences_peer_even_if_caller_is_cancelled() {
        use crate::continuation_journal::JournalQuota;
        use std::sync::{Mutex, mpsc};
        use std::time::Duration;

        for mode in ["registration", "control", "data"] {
            for reject_persistence in [false, true] {
                let dir = std::env::temp_dir().join(format!(
                    "ubm-outbox-atomic-{}-{:?}-{mode}-{reject_persistence}-{:?}",
                    std::process::id(),
                    std::thread::current().id(),
                    std::time::SystemTime::now()
                ));
                std::fs::create_dir(&dir).unwrap();
                let path = dir.join("journal.sqlite");
                let quota = JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 100,
                };
                let journal =
                    Arc::new(ContinuationJournal::open(&path, "r", &json!({}), quota).unwrap());
                let peer = ContinuationJournal::open(&path, "r", &json!({}), quota).unwrap();
                let outbox = Arc::new(Outbox::new(1, Arc::new(NoWake)));
                outbox.attach_journal(journal.clone(), json!({})).unwrap();
                if mode == "data" {
                    outbox.register_journal_consumer("c", json!({})).unwrap();
                }
                let before = peer.status().unwrap()["records"].as_u64().unwrap();
                let injector = rusqlite::Connection::open(&path).unwrap();
                injector.execute_batch("CREATE TRIGGER reject_record BEFORE INSERT ON records WHEN new.body LIKE '%reject-me%' BEGIN SELECT RAISE(ABORT, 'insert'); END;").unwrap();
                if reject_persistence {
                    injector.execute_batch("CREATE TRIGGER reject_failure BEFORE UPDATE OF collection_failure ON journal BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
                }
                let (entered_tx, entered_rx) = mpsc::channel();
                let (resume_tx, resume_rx) = mpsc::channel();
                let resume_rx = Mutex::new(resume_rx);
                journal.test_failed_append_hook(move || {
                    entered_tx.send(()).unwrap();
                    resume_rx.lock().unwrap().recv().unwrap();
                });
                let (done_tx, done_rx) = mpsc::channel();
                let worker = outbox.clone();
                let join = tokio::task::spawn_blocking(move || {
                    match mode {
                        "registration" => {
                            let _ = worker.register_journal_consumer("reject-me", json!({}));
                        }
                        "control" => {
                            worker.push_control(json!({"t":"control", "value":"reject-me"}))
                        }
                        _ => {
                            let _ = worker.push_data_batch(vec![
                                json!({"t":"value", "consumer":"c", "value":"reject-me"}),
                            ]);
                        }
                    }
                    done_tx.send(()).unwrap();
                });
                entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let locked = peer.test_shared_connection_busy();
                // Aborting a started blocking task must not interrupt the
                // synchronous storage transition.
                join.abort();
                resume_tx.send(()).unwrap();
                done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(
                    locked,
                    "{mode} released authority before terminal publication"
                );
                assert_eq!(
                    peer.append(&json!({}), &json!({"later":true}))
                        .unwrap_err()
                        .kind,
                    "storage.collection-failed"
                );
                let status = peer.status().unwrap();
                assert_eq!(status["records"], before);
                assert_eq!(
                    status["collectionFailure"]["persisted"],
                    !reject_persistence
                );
                let failure = outbox.journal_failure().unwrap();
                assert_eq!(failure.operation, "append");
                if reject_persistence {
                    assert!(
                        status["collectionFailure"]["persistenceFailure"]["sqliteExtendedCode"]
                            .is_number()
                    );
                    injector
                        .execute_batch("DROP TRIGGER reject_failure")
                        .unwrap();
                    journal.mark_collection_failure(&failure);
                    journal.mark_collection_failure(&failure);
                    let retried = peer.status().unwrap();
                    assert_eq!(retried["collectionFailure"]["persisted"], true);
                    assert_eq!(
                        retried["collectionFailure"]["sqliteExtendedCode"],
                        failure.sqlite_extended_code.unwrap()
                    );
                }
            }
        }
    }

    #[test]
    fn attached_journal_does_not_claim_suffix_after_unresolved_scan_failure() {
        use crate::continuation_journal::JournalQuota;
        use ubm_core::oversized_evidence::{
            EvidenceLimits, EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureMember,
        };
        let dir = std::env::temp_dir().join(format!(
            "ubm-journal-fence-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let journal = Arc::new(
            ContinuationJournal::open(
                &dir.join("journal.sqlite"),
                "r",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 100,
                },
            )
            .unwrap(),
        );
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox.attach_journal(journal.clone(), json!({})).unwrap();
        outbox.register_journal_consumer("c", json!({})).unwrap();
        outbox
            .push_data(json!({"t":"value", "consumer":"c", "n":"A"}))
            .unwrap();
        let mut terminal = outbox.reserve_control().unwrap();
        let mut reservation = outbox
            .reserve_scan_failure(ScanFailureLimits {
                evidence: EvidenceLimits {
                    max_depth: 8,
                    max_tokens: 8,
                    max_encoded_bytes: 1,
                    max_chunks: 8,
                    chunk_bytes: 8,
                },
                max_recipients: 1,
                max_bookkeeping_bytes: 1024,
            })
            .unwrap();
        let mut source = Some(ScanFailureOccurrence::new(
            ScanFailureIdentity {
                host: [1; 16],
                scan: [2; 16],
                start: [3; 16],
                source_generation: [4; 16],
                occurrence: [5; 16],
            },
            vec![ScanFailureMember {
                recipient: [6; 16],
                delivery: [7; 16],
            }],
            Arc::new(EvidenceSource::new(vec![
                EvidenceToken::Array(3),
                EvidenceToken::Null,
                EvidenceToken::Null,
                EvidenceToken::Null,
            ])),
        ));
        assert!(
            outbox
                .commit_scan_failure(&mut reservation, &mut source)
                .is_err()
        );
        assert!(
            source.is_none(),
            "failure was captured before preparation refused it"
        );
        let pending = outbox
            .pending_scan_failure(&reservation)
            .expect("the failed capture keeps its exact recovery capability");
        // Cancellation detaches the producer but must not let a later normal
        // record jump the captured source.  This is the in-memory half of the
        // restart fence below: the next producer may resume the same capability
        // but cannot turn an unprepared source into an ordinary drain prefix.
        drop(reservation);
        assert_eq!(outbox.detached_scan_failure(), Some(pending));
        assert!(matches!(
            outbox.claim_drain(100, 65_536),
            Err(DrainClaimError::ModeConflict)
        ));
        let mut resumed = outbox
            .resume_scan_failure(pending)
            .expect("a canceled source producer may resume its exact capture");
        assert!(matches!(
            outbox.commit_scan_failure(&mut resumed, &mut None),
            Err(ScanFailureOutboxError::Recovery(_))
        ));
        outbox
            .finish_scan_failure_producer(&mut resumed)
            .expect("the producer can hand off an unprepared source for raw recovery");
        let _ = outbox.push_data(json!({"t":"value", "consumer":"c", "n":"B"}));
        outbox.push_control(json!({"t":"scan-end", "n":"C"}));
        let _ = outbox.commit_reserved_control(&mut terminal, &json!({"t":"terminal", "n":"D"}));
        let _ = outbox.register_journal_consumer("later", json!({}));
        let page = journal.prepare(100, 65536).unwrap();
        let records = page["records"].as_array().unwrap();
        assert!(records.iter().any(|row| row["record"]["n"] == "A"));
        let has_suffix = |rows: &[Value]| {
            rows.iter().any(|row| {
                ["B", "C", "D"].iter().any(|n| row["record"]["n"] == *n)
                    || row["record"]["consumer"] == "later"
            })
        };
        let recording_crossed = has_suffix(records);
        let evidence_page = if journal.begin_evidence_transfer("evidence").is_ok() {
            journal
                .prepare_evidence_transfer("evidence", 100, 65536)
                .ok()
        } else {
            None
        };
        let evidence_crossed = evidence_page
            .as_ref()
            .and_then(|value| value["records"].as_array())
            .is_some_and(|rows| has_suffix(rows));
        if recording_crossed {
            journal
                .acknowledge(page["token"].as_str().unwrap())
                .unwrap();
        }
        if evidence_crossed {
            journal
                .acknowledge_evidence_transfer(
                    "evidence",
                    evidence_page.as_ref().unwrap()["token"].as_str().unwrap(),
                )
                .unwrap();
        }
        assert!(
            !recording_crossed && !evidence_crossed,
            "journal exposed and acknowledged suffix past captured failure; recording={page}; evidence={evidence_page:?}"
        );
        drop(outbox);
        drop(journal);
        // The collection failure is persisted, rather than merely an in-memory
        // outbox flag.  A cold owner cannot register a fresh consumer or append
        // a suffix past the unresolved source.
        let reopened = Arc::new(
            ContinuationJournal::open(
                &dir.join("journal.sqlite"),
                "r",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 100,
                },
            )
            .unwrap(),
        );
        let restarted = Outbox::new(2, Arc::new(NoWake));
        restarted
            .attach_journal(reopened.clone(), json!({}))
            .unwrap();
        let error = restarted
            .register_journal_consumer("after-restart", json!({}))
            .expect_err("restart must retain the unresolved source fence");
        assert_eq!(error.kind, "storage.collection-failed");
        assert!(
            restarted
                .push_data(json!({"t":"value", "consumer":"after-restart", "n":"E"}))
                .is_err()
        );
        let reopened_page = reopened.prepare(100, 65_536).unwrap();
        assert!(
            !has_suffix(reopened_page["records"].as_array().unwrap()),
            "cold reopen admitted a suffix past the unresolved source: {reopened_page}"
        );
        drop(restarted);
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_ack_releases_bookkeeping_but_keeps_receipt_cost() {
        use ubm_core::oversized_evidence::{
            EvidenceLimits, EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureMember,
        };
        let outbox = Outbox::new(1, Arc::new(NoWake));
        let limits = ScanFailureLimits {
            evidence: EvidenceLimits {
                max_depth: 8,
                max_tokens: 8,
                max_encoded_bytes: 1024,
                max_chunks: 1024,
                chunk_bytes: 8,
            },
            max_recipients: 2,
            max_bookkeeping_bytes: 4096,
        };
        let members = [1, 2].map(|n| ScanFailureMember {
            recipient: [n; 16],
            delivery: [n + 10; 16],
        });
        let occurrence = ScanFailureOccurrence::new(
            ScanFailureIdentity {
                host: [1; 16],
                scan: [2; 16],
                start: [3; 16],
                source_generation: [4; 16],
                occurrence: [5; 16],
            },
            members.to_vec(),
            Arc::new(EvidenceSource::new(vec![
                EvidenceToken::Array(3),
                EvidenceToken::Bytes(vec![0; 80]),
                EvidenceToken::Null,
                EvidenceToken::Null,
            ])),
        );
        let mut token = outbox.reserve_scan_failure(limits).unwrap();
        outbox
            .commit_scan_failure(&mut token, &mut Some(occurrence))
            .unwrap();
        let initial = lock(&outbox.queues).scan_failures[0].budget;
        for (i, member) in members.into_iter().enumerate() {
            let head = outbox.scan_failure_head(member).unwrap().unwrap();
            let mut offset = 0;
            while offset < head.evidence.encoded_length {
                offset = outbox.read_scan_failure(&head, offset).unwrap().next_offset;
            }
            outbox.acknowledge_scan_failure(&head).unwrap();
            let charged = lock(&outbox.queues).scan_failures[0].budget;
            assert!(charged < initial);
            assert!(charged > 0);
            if i == 1 {
                let mut boundary = limits;
                boundary.max_bookkeeping_bytes = 0;
                boundary.evidence.max_encoded_bytes = SCAN_FAILURE_BUDGET_BYTES - charged + 1;
                assert!(matches!(
                    outbox.reserve_scan_failure(boundary),
                    Err(ScanFailureOutboxError::Capacity)
                ));
                boundary.evidence.max_encoded_bytes -= 1;
                assert!(outbox.reserve_scan_failure(boundary).is_ok());
            }
        }
    }

    #[test]
    fn scan_release_tombstone_survives_cold_journal_reopen() {
        use crate::continuation_journal::JournalQuota;
        use ubm_core::oversized_evidence::{
            EvidenceLimits, EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureMember,
        };

        let dir = std::env::temp_dir().join(format!(
            "ubm-scan-release-reopen-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("journal.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        let journal = Arc::new(ContinuationJournal::open(&path, "r", &json!({}), quota).unwrap());
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox
            .configure_scan_failure_recovery_authority([8; 32])
            .unwrap();
        outbox
            .attach_journal(Arc::clone(&journal), json!({}))
            .unwrap();
        let member = ScanFailureMember {
            recipient: [6; 16],
            delivery: [7; 16],
        };
        let mut reservation = outbox
            .reserve_scan_failure(ScanFailureLimits {
                evidence: EvidenceLimits {
                    max_depth: 8,
                    max_tokens: 8,
                    max_encoded_bytes: 1024,
                    max_chunks: 8,
                    chunk_bytes: 8,
                },
                max_recipients: 1,
                max_bookkeeping_bytes: 1024,
            })
            .unwrap();
        outbox
            .commit_scan_failure(
                &mut reservation,
                &mut Some(ScanFailureOccurrence::new(
                    ScanFailureIdentity {
                        host: [1; 16],
                        scan: [2; 16],
                        start: [3; 16],
                        source_generation: [4; 16],
                        occurrence: [5; 16],
                    },
                    vec![member],
                    Arc::new(EvidenceSource::new(vec![
                        EvidenceToken::Array(3),
                        EvidenceToken::Null,
                        EvidenceToken::Null,
                        EvidenceToken::Null,
                    ])),
                )),
            )
            .unwrap();
        let head = outbox.scan_failure_head(member).unwrap().unwrap();
        let mut offset = 0;
        while offset < head.evidence.encoded_length {
            offset = outbox.read_scan_failure(&head, offset).unwrap().next_offset;
        }
        outbox.acknowledge_scan_failure(&head).unwrap();
        outbox.retire_scan_failure(&head).unwrap();
        outbox
            .finish_scan_failure_producer(&mut reservation)
            .unwrap();
        outbox.release_scan_failure_receipt(&head).unwrap();
        let released_head = ScanFailureHead {
            contiguous_read: head.evidence.encoded_length,
            acknowledged: true,
            ..head
        };
        assert_eq!(
            outbox.scan_failure_recovery_owners()[0].released,
            Some(released_head),
            "the live tombstone must use the same completed receipt state as a reopen"
        );
        // A caller can hold the head minted before it observed the terminal
        // receipt progress. That remains the same authenticated source.
        outbox.confirm_scan_failure_receipt_release(&head).unwrap();
        drop(reservation);
        drop(outbox);
        drop(journal);

        let reopened_journal =
            Arc::new(ContinuationJournal::open(&path, "r", &json!({}), quota).unwrap());
        let reopened = Outbox::new(2, Arc::new(NoWake));
        reopened
            .attach_journal(Arc::clone(&reopened_journal), json!({}))
            .unwrap();
        let owners = reopened.scan_failure_recovery_owners();
        assert_eq!(owners.len(), 1);
        assert_eq!(
            owners[0].released,
            Some(released_head),
            "released inventory must report the persisted completed receipt state"
        );
        let receipt = reopened
            .confirm_scan_failure_receipt_release(&head)
            .unwrap();
        let retirement = reopened
            .finalize_scan_failure_release_with_retirement(&receipt)
            .unwrap();
        assert_eq!(reopened.pending_scan_failure_owners(), 0);
        assert_eq!(
            Outbox::recovery_authority_from_journal(&reopened_journal).unwrap(),
            Some(ScanFailureRecoveryAuthority {
                session_id: 1,
                authority: [8; 32],
            }),
            "the final cleanup receipt needs the recorded owner domain after its envelope is gone"
        );
        drop(reopened);
        let cleaned_journal =
            Arc::new(ContinuationJournal::open(&path, "r", &json!({}), quota).unwrap());
        let cleaned = Outbox::new(3, Arc::new(NoWake));
        cleaned
            .attach_journal(Arc::clone(&cleaned_journal), json!({}))
            .unwrap();
        assert!(cleaned.scan_failure_recovery_owners().is_empty());
        // The first finalization can have committed immediately before its
        // reply was lost. A new process has no owner slot, but the exact
        // authenticated confirmation still succeeds; a changed proof does
        // not turn an unknown cleanup into a false success.
        cleaned
            .finalize_scan_failure_release_with_retirement(&receipt)
            .unwrap();
        let rehydrated = cleaned.scan_failure_cleanup_retirements().unwrap();
        assert_eq!(rehydrated.len(), 1);
        let mut wrong_retirement = rehydrated[0].clone();
        wrong_retirement.authenticator[0] ^= 1;
        assert!(matches!(
            cleaned.retire_scan_failure_cleanup(&wrong_retirement),
            Err(ScanFailureOutboxError::Durability(_))
        ));
        // The durable row is reclaimed after explicit confirmation. Retrying
        // the same opaque capability is the lost-response path and succeeds.
        cleaned.retire_scan_failure_cleanup(&retirement).unwrap();
        assert_eq!(
            Outbox::recovery_authority_from_journal(&cleaned_journal).unwrap(),
            None,
            "retiring the final receipt must release the recording-level authority"
        );
        drop(cleaned);
        drop(cleaned_journal);
        let retired_journal =
            Arc::new(ContinuationJournal::open(&path, "r", &json!({}), quota).unwrap());
        let retired = Outbox::new(4, Arc::new(NoWake));
        retired.attach_journal(retired_journal, json!({})).unwrap();
        retired.retire_scan_failure_cleanup(&retirement).unwrap();
        assert!(
            retired
                .scan_failure_cleanup_retirements()
                .unwrap()
                .is_empty()
        );
        let mut altered_receipt = receipt;
        altered_receipt.authenticator[0] ^= 1;
        assert_eq!(
            retired.finalize_scan_failure_release(&altered_receipt),
            Err(ScanFailureOutboxError::NotHead)
        );
        retired
            .register_journal_consumer("after-recovery", json!({}))
            .unwrap();
        retired
            .push_data(json!({"t":"value", "consumer":"after-recovery", "n":"suffix"}))
            .unwrap();
        drop(retired);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn scan_cleanup_test_limits() -> ScanFailureLimits {
        ScanFailureLimits {
            evidence: EvidenceLimits {
                max_depth: 8,
                max_tokens: 8,
                max_encoded_bytes: 1024,
                max_chunks: 8,
                chunk_bytes: 8,
            },
            max_recipients: 1,
            max_bookkeeping_bytes: 1024,
        }
    }

    fn scan_cleanup_test_occurrence(
        generation: u8,
        member: ScanFailureMember,
    ) -> ScanFailureOccurrence {
        ScanFailureOccurrence::new(
            ScanFailureIdentity {
                host: [1; 16],
                scan: [2; 16],
                start: [3; 16],
                source_generation: [generation; 16],
                occurrence: [generation.wrapping_add(1); 16],
            },
            vec![member],
            Arc::new(EvidenceSource::new(vec![
                EvidenceToken::Array(3),
                EvidenceToken::Null,
                EvidenceToken::Null,
                EvidenceToken::Null,
            ])),
        )
    }

    #[test]
    fn recovered_finished_raw_owner_keeps_its_ordering_fence() {
        use crate::continuation_journal::JournalQuota;

        let directory = std::env::temp_dir().join(format!(
            "ubm-recovered-raw-owner-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());
        let writer = Outbox::new(1, Arc::new(NoWake));
        writer
            .configure_scan_failure_recovery_authority([9; 32])
            .unwrap();
        writer
            .attach_journal(Arc::clone(&journal), json!({}))
            .unwrap();
        let mut raw_limits = scan_cleanup_test_limits();
        raw_limits.evidence.max_encoded_bytes = 1;
        let mut reservation = writer.reserve_scan_failure(raw_limits).unwrap();
        let commit = writer.commit_scan_failure(
            &mut reservation,
            &mut Some(scan_cleanup_test_occurrence(
                1,
                ScanFailureMember {
                    recipient: [1; 16],
                    delivery: [2; 16],
                },
            )),
        );
        assert!(matches!(commit, Err(ScanFailureOutboxError::Recovery(_))));
        writer
            .finish_scan_failure_producer(&mut reservation)
            .unwrap();
        drop(reservation);
        drop(writer);
        drop(journal);

        let reopened_journal =
            Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());
        let reopened = Outbox::new(2, Arc::new(NoWake));
        reopened
            .attach_journal(Arc::clone(&reopened_journal), json!({}))
            .unwrap();
        let owner = reopened.scan_failure_recovery_owners().pop().unwrap();
        assert!(owner.producer_finished);
        assert!(
            !owner.prepared,
            "raw recovery is only valid after preparation refused"
        );
        {
            let queues = lock(&reopened.queues);
            assert_eq!(
                queues
                    .control
                    .iter()
                    .map(|entry| (entry.ordinal, entry.record.is_null()))
                    .collect::<Vec<_>>(),
                vec![(owner.key.ordinal, true)],
                "rehydration must restore the first raw ordering fence"
            );
            assert!(queues.data.is_empty());
            assert_eq!(
                ordered_entries(&queues).next().map(|entry| entry.ordinal),
                Some(owner.key.ordinal)
            );
        }
        assert_eq!(
            reopened
                .inspect_scan_failure_token(&owner.key, 0, 0, 64)
                .unwrap(),
            ScanFailureRawToken::Array(3),
            "the restored owner must remain the first blocking source"
        );
        reopened.retire_unprepared_scan_failure(&owner.key).unwrap();
        drop(reopened);
        drop(reopened_journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn attach_journal_rebases_pre_attach_controls_after_recovered_scan_fence() {
        use crate::continuation_journal::JournalQuota;

        let directory = std::env::temp_dir().join(format!(
            "ubm-attach-ordinal-collision-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());

        // The persisted owner receives the first session ordinal.
        let first = Outbox::new(1, Arc::new(NoWake));
        first
            .configure_scan_failure_recovery_authority([9; 32])
            .unwrap();
        first
            .attach_journal(Arc::clone(&journal), json!({}))
            .unwrap();
        let mut reservation = first
            .reserve_scan_failure(scan_cleanup_test_limits())
            .unwrap();
        let member = ScanFailureMember {
            recipient: [1; 16],
            delivery: [2; 16],
        };
        first
            .commit_scan_failure(
                &mut reservation,
                &mut Some(scan_cleanup_test_occurrence(1, member)),
            )
            .unwrap();
        drop(reservation);
        drop(first);
        drop(journal);

        let reopened =
            Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());
        let replacement = Outbox::new(2, Arc::new(NoWake));
        replacement
            .configure_scan_failure_recovery_authority([9; 32])
            .unwrap();
        // This ordinary process control also has ordinal 1, but it was
        // produced after restart.  Its local ordinal cannot share the
        // recovered session's ordering domain.  Attachment must retain it
        // after the authenticated recovered source rather than duplicating
        // that source's identity or losing the process control.
        replacement.push_control(control(1));
        replacement
            .attach_journal(Arc::clone(&reopened), json!({}))
            .unwrap();
        assert_eq!(
            replacement.scan_failure_recovery_authority().unwrap(),
            Some([9; 32]),
            "the persisted recovery session is independent from the new wake session"
        );
        let queues = lock(&replacement.queues);
        assert_eq!(
            queues.scan_failures[0].ordinal,
            Some(1),
            "recovery owner must retain its authenticated source ordinal"
        );
        assert_eq!(queues.control.len(), 2);
        assert_eq!(queues.control[0].ordinal, 1);
        assert!(queues.control[0].record.is_null());
        assert_eq!(queues.control[1].ordinal, 2);
        assert_eq!(queues.control[1].record["ordinal"], 2);
        assert_eq!(queues.ordinal, 2);
        drop(queues);
        assert_eq!(
            replacement.drain(10, 1024)["error"],
            "retained-scan-failure",
            "the later process control cannot overtake the recovered source"
        );
        drop(replacement);
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recovered_scan_session_owns_new_captures_across_multiple_reopens() {
        use crate::continuation_journal::JournalQuota;

        let directory = std::env::temp_dir().join(format!(
            "ubm-recovered-scan-session-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        let first_journal =
            Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());
        let first = Outbox::new(1, Arc::new(NoWake));
        first
            .configure_scan_failure_recovery_authority([9; 32])
            .unwrap();
        first
            .attach_journal(Arc::clone(&first_journal), json!({}))
            .unwrap();
        let mut first_reservation = first
            .reserve_scan_failure(scan_cleanup_test_limits())
            .unwrap();
        first
            .commit_scan_failure(
                &mut first_reservation,
                &mut Some(scan_cleanup_test_occurrence(
                    1,
                    ScanFailureMember {
                        recipient: [1; 16],
                        delivery: [2; 16],
                    },
                )),
            )
            .unwrap();
        drop(first_reservation);
        drop(first);
        drop(first_journal);

        // The second process has a distinct wake ID. It must nevertheless
        // append to the recovered durable owner session, not mint a new
        // session that would make the sidecar unrecoverable on the next boot.
        let second_journal =
            Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());
        let second = Outbox::new(2, Arc::new(NoWake));
        second
            .attach_journal(Arc::clone(&second_journal), json!({}))
            .unwrap();
        let mut second_reservation = second
            .reserve_scan_failure(scan_cleanup_test_limits())
            .unwrap();
        second
            .commit_scan_failure(
                &mut second_reservation,
                &mut Some(scan_cleanup_test_occurrence(
                    2,
                    ScanFailureMember {
                        recipient: [3; 16],
                        delivery: [4; 16],
                    },
                )),
            )
            .unwrap();
        drop(second_reservation);
        drop(second);
        drop(second_journal);

        let third_journal =
            Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());
        let third = Outbox::new(3, Arc::new(NoWake));
        third
            .attach_journal(Arc::clone(&third_journal), json!({}))
            .expect("reopen must accept the original session and the appended owner");
        assert_eq!(
            third.scan_failure_recovery_authority().unwrap(),
            Some([9; 32])
        );
        let queues = lock(&third.queues);
        assert_eq!(queues.scan_failures.len(), 2);
        assert_eq!(
            queues
                .scan_failures
                .iter()
                .map(|slot| (slot.session_id, slot.recovery_authority, slot.ordinal))
                .collect::<Vec<_>>(),
            vec![(1, Some([9; 32]), Some(1)), (1, Some([9; 32]), Some(2))]
        );
        drop(queues);
        drop(third);
        drop(third_journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn attach_journal_rebase_overflow_preserves_pre_attach_control() {
        use crate::continuation_journal::JournalQuota;

        let directory = std::env::temp_dir().join(format!(
            "ubm-attach-ordinal-overflow-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());
        let first = Outbox::new(1, Arc::new(NoWake));
        first
            .attach_journal(Arc::clone(&journal), json!({}))
            .unwrap();
        let mut reservation = first
            .reserve_scan_failure(scan_cleanup_test_limits())
            .unwrap();
        first
            .commit_scan_failure(
                &mut reservation,
                &mut Some(scan_cleanup_test_occurrence(
                    1,
                    ScanFailureMember {
                        recipient: [1; 16],
                        delivery: [2; 16],
                    },
                )),
            )
            .unwrap();
        drop(reservation);
        let owner = journal
            .scan_failure_owner_envelopes()
            .unwrap()
            .pop()
            .unwrap();
        let mut envelope: Value = serde_json::from_slice(&owner.body).unwrap();
        envelope["ordinal"] = Value::from(u64::MAX);
        let successor = serde_json::to_vec(&envelope).unwrap();
        journal
            .advance_scan_failure_owner_envelope(owner.identity, &owner.body, &successor)
            .unwrap();
        drop(first);
        drop(journal);

        let reopened =
            Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());
        let replacement = Outbox::new(2, Arc::new(NoWake));
        replacement.push_control(control(1));
        let error = replacement
            .attach_journal(Arc::clone(&reopened), json!({}))
            .expect_err("ordinal overflow must reject before moving a process control");
        assert_eq!(error.kind, "argument.invalid");
        assert_eq!(
            replacement.drain(10, 1024)["records"][0]["n"],
            1,
            "failed rebase must leave the process control available"
        );
        drop(replacement);
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn attach_journal_rejects_recovered_owners_from_different_sessions() {
        use crate::continuation_journal::JournalQuota;

        let directory = std::env::temp_dir().join(format!(
            "ubm-attach-mixed-session-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());
        let first = Outbox::new(1, Arc::new(NoWake));
        first
            .attach_journal(Arc::clone(&journal), json!({}))
            .unwrap();
        for generation in [1, 2] {
            let mut reservation = first
                .reserve_scan_failure(scan_cleanup_test_limits())
                .unwrap();
            first
                .commit_scan_failure(
                    &mut reservation,
                    &mut Some(scan_cleanup_test_occurrence(
                        generation,
                        ScanFailureMember {
                            recipient: [generation; 16],
                            delivery: [generation.wrapping_add(1); 16],
                        },
                    )),
                )
                .unwrap();
            drop(reservation);
        }
        let owners = journal.scan_failure_owner_envelopes().unwrap();
        assert_eq!(owners.len(), 2);
        let changed = &owners[1];
        let mut envelope: Value = serde_json::from_slice(&changed.body).unwrap();
        envelope["sessionId"] = Value::from(2_u64);
        let successor = serde_json::to_vec(&envelope).unwrap();
        journal
            .advance_scan_failure_owner_envelope(changed.identity, &changed.body, &successor)
            .unwrap();
        drop(first);
        drop(journal);

        let reopened =
            Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());
        let replacement = Outbox::new(99, Arc::new(NoWake));
        let error = replacement
            .attach_journal(reopened, json!({}))
            .expect_err("mixed persisted sessions must not acquire one recovery lease");
        assert_eq!(error.kind, "argument.invalid");
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn finalize_scan_cleanup_for_test(
        outbox: &Outbox,
        generation: u8,
    ) -> ScanFailureCleanupRetirement {
        let member = ScanFailureMember {
            recipient: [generation; 16],
            delivery: [generation.wrapping_add(1); 16],
        };
        let mut reservation = outbox
            .reserve_scan_failure(scan_cleanup_test_limits())
            .unwrap();
        outbox
            .commit_scan_failure(
                &mut reservation,
                &mut Some(scan_cleanup_test_occurrence(generation, member)),
            )
            .unwrap();
        let head = outbox.scan_failure_head(member).unwrap().unwrap();
        let mut offset = 0;
        while offset < head.evidence.encoded_length {
            offset = outbox.read_scan_failure(&head, offset).unwrap().next_offset;
        }
        outbox.acknowledge_scan_failure(&head).unwrap();
        outbox.retire_scan_failure(&head).unwrap();
        outbox
            .finish_scan_failure_producer(&mut reservation)
            .unwrap();
        outbox.release_scan_failure_receipt(&head).unwrap();
        drop(reservation);
        let confirmation = outbox.confirm_scan_failure_receipt_release(&head).unwrap();
        outbox
            .finalize_scan_failure_release_with_retirement(&confirmation)
            .unwrap()
    }

    fn read_cleanup_retirement_from_stdin() -> ScanFailureCleanupRetirement {
        use std::io::Read;

        let mut bytes = [0; 64];
        std::io::stdin().read_exact(&mut bytes).unwrap();
        ScanFailureCleanupRetirement {
            cleanup_receipt: bytes[..32].try_into().unwrap(),
            authenticator: bytes[32..].try_into().unwrap(),
        }
    }

    fn write_cleanup_retirement_to_child(
        child: &mut std::process::Child,
        retirement: &ScanFailureCleanupRetirement,
    ) {
        use std::io::Write;

        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(&retirement.cleanup_receipt).unwrap();
        stdin.write_all(&retirement.authenticator).unwrap();
    }

    fn scan_cleanup_test_child(phase: &str, path: &std::path::Path, quota: JournalQuota) {
        let journal = Arc::new(ContinuationJournal::open(path, "scan", &json!({}), quota).unwrap());
        if phase == "retire" {
            assert_eq!(
                Outbox::recovery_authority_from_journal(&journal).unwrap(),
                Some(ScanFailureRecoveryAuthority {
                    session_id: 1,
                    authority: [8; 32],
                }),
                "the fresh process must recover the authority after the final owner envelope is gone"
            );
        }
        let outbox = Outbox::new(9, Arc::new(NoWake));
        outbox.attach_journal(journal, json!({})).unwrap();
        let retirement = read_cleanup_retirement_from_stdin();
        match phase {
            "retire" => {
                // Reopening derives the same opaque capability from the
                // sidecar's private authority; the child receives the
                // original only through its private stdin pipe, never logs.
                let recovered = outbox.scan_failure_cleanup_retirements().unwrap();
                assert_eq!(recovered.len(), 1);
                assert!(recovered[0] == retirement);
                outbox.retire_scan_failure_cleanup(&retirement).unwrap();
            }
            "replay" => {
                assert!(
                    outbox
                        .scan_failure_cleanup_retirements()
                        .unwrap()
                        .is_empty()
                );
                // A successful delete whose reply was lost remains an exact,
                // authenticated idempotent success after another cold reopen.
                outbox.retire_scan_failure_cleanup(&retirement).unwrap();
            }
            "cross-recording" => {
                let local = outbox.scan_failure_cleanup_retirements().unwrap();
                assert_eq!(local.len(), 1);
                let mut altered_local = local[0].clone();
                altered_local.authenticator[0] ^= 1;
                assert!(matches!(
                    outbox.retire_scan_failure_cleanup(&altered_local),
                    Err(ScanFailureOutboxError::Durability(_))
                ));
                assert!(matches!(
                    outbox.retire_scan_failure_cleanup(&retirement),
                    Err(ScanFailureOutboxError::Durability(_))
                ));
                assert_eq!(outbox.scan_failure_cleanup_retirements().unwrap().len(), 1);
            }
            _ => panic!("unknown scan cleanup subprocess phase"),
        }
    }

    #[test]
    fn scan_cleanup_retirement_is_exact_across_real_process_reopens() {
        const CHILD: &str = "UBM_SCAN_CLEANUP_PROCESS_CHILD";
        const PHASE: &str = "UBM_SCAN_CLEANUP_PROCESS_PHASE";
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 1,
        };
        if let (Some(path), Some(phase)) = (std::env::var_os(CHILD), std::env::var_os(PHASE)) {
            scan_cleanup_test_child(phase.to_str().unwrap(), std::path::Path::new(&path), quota);
            return;
        }

        let directory = std::env::temp_dir().join(format!(
            "ubm-scan-cleanup-process-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        // Process A creates the receipt strictly through the public Outbox
        // lifecycle; it never edits the private sidecar directly.
        let journal =
            Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox
            .configure_scan_failure_recovery_authority([8; 32])
            .unwrap();
        outbox
            .attach_journal(Arc::clone(&journal), json!({}))
            .unwrap();
        let retirement = finalize_scan_cleanup_for_test(&outbox, 1);
        drop(outbox);
        drop(journal);

        for phase in ["retire", "replay"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("continuation_outbox::tests::scan_cleanup_retirement_is_exact_across_real_process_reopens")
                .env(CHILD, &path)
                .env(PHASE, phase)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            write_cleanup_retirement_to_child(&mut child, &retirement);
            assert!(child.wait().unwrap().success());
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn scan_cleanup_retirement_rejects_a_cross_recording_process_capability() {
        const CHILD: &str = "UBM_SCAN_CLEANUP_CROSS_RECORDING_CHILD";
        const PHASE: &str = "UBM_SCAN_CLEANUP_CROSS_RECORDING_PHASE";
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 1,
        };
        if let Some(path) = std::env::var_os(CHILD) {
            scan_cleanup_test_child(
                std::env::var_os(PHASE)
                    .as_deref()
                    .unwrap()
                    .to_str()
                    .unwrap(),
                std::path::Path::new(&path),
                quota,
            );
            return;
        }

        let root = std::env::temp_dir().join(format!(
            "ubm-scan-cleanup-cross-recording-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let first_path = first.join("recording.sqlite");
        let second_path = second.join("recording.sqlite");
        let first_journal =
            Arc::new(ContinuationJournal::open(&first_path, "scan", &json!({}), quota).unwrap());
        let first_outbox = Outbox::new(1, Arc::new(NoWake));
        first_outbox
            .attach_journal(Arc::clone(&first_journal), json!({}))
            .unwrap();
        let first_retirement = finalize_scan_cleanup_for_test(&first_outbox, 1);
        let second_journal =
            Arc::new(ContinuationJournal::open(&second_path, "scan", &json!({}), quota).unwrap());
        let second_outbox = Outbox::new(2, Arc::new(NoWake));
        second_outbox
            .attach_journal(Arc::clone(&second_journal), json!({}))
            .unwrap();
        let _second_retirement = finalize_scan_cleanup_for_test(&second_outbox, 2);
        drop(first_outbox);
        drop(first_journal);
        drop(second_outbox);
        drop(second_journal);

        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("continuation_outbox::tests::scan_cleanup_retirement_rejects_a_cross_recording_process_capability")
            .env(CHILD, &second_path)
            .env(PHASE, "cross-recording")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        write_cleanup_retirement_to_child(&mut child, &first_retirement);
        assert!(child.wait().unwrap().success());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_cleanup_retirement_reclaims_1024_public_lifecycles_across_reopens() {
        const RECEIPTS: u16 = 1_024;
        // One retained owner/receipt is the entire sidecar capacity. Every
        // generation must reclaim it before the next cold reopen succeeds.
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 1,
        };
        let directory = std::env::temp_dir().join(format!(
            "ubm-scan-cleanup-public-reclaim-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        for generation in 1..=RECEIPTS {
            let journal =
                Arc::new(ContinuationJournal::open(&path, "scan", &json!({}), quota).unwrap());
            let outbox = Outbox::new(u64::from(generation), Arc::new(NoWake));
            outbox
                .attach_journal(Arc::clone(&journal), json!({}))
                .unwrap();
            let retirement = finalize_scan_cleanup_for_test(&outbox, generation as u8);
            outbox.retire_scan_failure_cleanup(&retirement).unwrap();
            assert!(
                outbox
                    .scan_failure_cleanup_retirements()
                    .unwrap()
                    .is_empty()
            );
            drop(outbox);
            drop(journal);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn scan_owner_write_failure_keeps_source_and_fence_retryable() {
        use crate::continuation_journal::JournalQuota;
        let dir = std::env::temp_dir().join(format!(
            "ubm-scan-owner-write-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("journal.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        let journal = Arc::new(ContinuationJournal::open(&path, "r", &json!({}), quota).unwrap());
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox
            .attach_journal(Arc::clone(&journal), json!({}))
            .unwrap();
        let mut reservation = outbox
            .reserve_scan_failure(ScanFailureLimits {
                evidence: EvidenceLimits {
                    max_depth: 8,
                    max_tokens: 8,
                    max_encoded_bytes: 1024,
                    max_chunks: 8,
                    chunk_bytes: 8,
                },
                max_recipients: 1,
                max_bookkeeping_bytes: 1024,
            })
            .unwrap();
        let member = ScanFailureMember {
            recipient: [6; 16],
            delivery: [7; 16],
        };
        let mut source = Some(ScanFailureOccurrence::new(
            ScanFailureIdentity {
                host: [1; 16],
                scan: [2; 16],
                start: [3; 16],
                source_generation: [4; 16],
                occurrence: [5; 16],
            },
            vec![member],
            Arc::new(EvidenceSource::new(vec![
                EvidenceToken::Array(3),
                EvidenceToken::Null,
                EvidenceToken::Null,
                EvidenceToken::Null,
            ])),
        ));
        let sidecar = path.with_extension("scan-failure-recovery.sqlite");
        let injector = rusqlite::Connection::open(&sidecar).unwrap();
        injector.execute_batch("CREATE TRIGGER reject_owner BEFORE INSERT ON owner_envelopes BEGIN SELECT RAISE(ABORT, 'owner'); END;").unwrap();
        assert!(matches!(
            outbox.commit_scan_failure(&mut reservation, &mut source),
            Err(ScanFailureOutboxError::Durability(_))
        ));
        assert!(
            source.is_some(),
            "failed owner write must not consume source"
        );
        assert_eq!(outbox.pending_scan_failure_owners(), 0);
        for table in ["occurrences", "members", "tokens", "owner_envelopes"] {
            let retained: i64 = injector
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(retained, 0, "failed atomic capture retained {table}");
        }
        injector.execute_batch("DROP TRIGGER reject_owner").unwrap();
        // Model the process dying after the failed response: neither raw
        // evidence nor a restartable owner is present after reopening, and
        // the same exact source can be captured by the replacement outbox.
        drop(injector);
        drop(reservation);
        drop(outbox);
        drop(journal);
        let reopened_journal =
            Arc::new(ContinuationJournal::open(&path, "r", &json!({}), quota).unwrap());
        let reopened = Outbox::new(2, Arc::new(NoWake));
        reopened
            .attach_journal(Arc::clone(&reopened_journal), json!({}))
            .unwrap();
        assert!(reopened.scan_failure_recovery_owners().is_empty());
        let mut reservation = reopened
            .reserve_scan_failure(ScanFailureLimits {
                evidence: EvidenceLimits {
                    max_depth: 8,
                    max_tokens: 8,
                    max_encoded_bytes: 1024,
                    max_chunks: 8,
                    chunk_bytes: 8,
                },
                max_recipients: 1,
                max_bookkeeping_bytes: 1024,
            })
            .unwrap();
        assert!(
            reopened
                .commit_scan_failure(&mut reservation, &mut source)
                .is_ok()
        );
        assert!(source.is_none());
        assert!(reopened.scan_failure_head(member).unwrap().is_some());
        drop(reservation);
        drop(reopened);
        drop(reopened_journal);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_owner_update_failures_are_atomic_and_retryable_for_each_transition() {
        use crate::continuation_journal::JournalQuota;

        let dir = std::env::temp_dir().join(format!(
            "ubm-scan-owner-update-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("journal.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        let journal = Arc::new(ContinuationJournal::open(&path, "r", &json!({}), quota).unwrap());
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox
            .attach_journal(Arc::clone(&journal), json!({}))
            .unwrap();
        let members = [
            ScanFailureMember {
                recipient: [6; 16],
                delivery: [7; 16],
            },
            ScanFailureMember {
                recipient: [8; 16],
                delivery: [9; 16],
            },
        ];
        let limits = ScanFailureLimits {
            evidence: EvidenceLimits {
                max_depth: 8,
                max_tokens: 8,
                max_encoded_bytes: 1024,
                max_chunks: 8,
                chunk_bytes: 8,
            },
            max_recipients: 2,
            max_bookkeeping_bytes: 1024,
        };
        let mut reservation = outbox.reserve_scan_failure(limits).unwrap();
        let mut source = Some(ScanFailureOccurrence::new(
            ScanFailureIdentity {
                host: [1; 16],
                scan: [2; 16],
                start: [3; 16],
                source_generation: [4; 16],
                occurrence: [5; 16],
            },
            members.to_vec(),
            Arc::new(EvidenceSource::new(vec![
                EvidenceToken::Array(3),
                EvidenceToken::Null,
                EvidenceToken::Null,
                EvidenceToken::Null,
            ])),
        ));
        let sidecar = path.with_extension("scan-failure-recovery.sqlite");
        let injector = rusqlite::Connection::open(&sidecar).unwrap();
        let reject = |name: &str| {
            injector.execute_batch(&format!(
            "CREATE TRIGGER {name} BEFORE UPDATE ON owner_envelopes BEGIN SELECT RAISE(ABORT, 'injected'); END;"
        )).unwrap()
        };
        let allow = |name: &str| {
            injector
                .execute_batch(&format!("DROP TRIGGER {name}"))
                .unwrap()
        };
        let durable_and_ram = || {
            let queues = lock(&outbox.queues);
            let slot = &queues.scan_failures[0];
            let identity = slot.occurrence_identity.unwrap();
            (
                journal
                    .scan_failure_owner_envelope(identity)
                    .unwrap()
                    .unwrap(),
                scan_owner_envelope(slot).unwrap(),
            )
        };

        // Capture stores the owner then prepares it. Rejecting that CAS must
        // retain the captured raw source and the ordering fence, not advance to
        // an in-memory prepared state.
        reject("reject_prepare");
        assert!(matches!(
            outbox.commit_scan_failure(&mut reservation, &mut source),
            Err(ScanFailureOutboxError::Durability(_))
        ));
        assert!(source.is_none());
        let (durable, ram) = durable_and_ram();
        assert_eq!(durable, ram);
        assert!(!outbox.scan_failure_recovery_owners()[0].prepared);
        allow("reject_prepare");
        outbox
            .commit_scan_failure(&mut reservation, &mut source)
            .unwrap();

        let first = outbox.scan_failure_head(members[0]).unwrap().unwrap();
        reject("reject_read");
        let before = durable_and_ram();
        assert!(matches!(
            outbox.read_scan_failure(&first, 0),
            Err(ScanFailureOutboxError::Durability(_))
        ));
        assert_eq!(before, durable_and_ram());
        assert!(outbox.scan_failure_head(members[0]).unwrap().is_some());
        allow("reject_read");
        let chunk = outbox.read_scan_failure(&first, 0).unwrap();
        assert!(chunk.next_offset > 0);
        let mut offset = chunk.next_offset;
        while offset < first.evidence.encoded_length {
            offset = outbox
                .read_scan_failure(&first, offset)
                .unwrap()
                .next_offset;
        }

        reject("reject_ack");
        let before = durable_and_ram();
        assert!(matches!(
            outbox.acknowledge_scan_failure(&first),
            Err(ScanFailureOutboxError::Durability(_))
        ));
        assert_eq!(before, durable_and_ram());
        allow("reject_ack");
        outbox.acknowledge_scan_failure(&first).unwrap();
        // Multiple recipients remain independently replayable; acknowledge
        // the second receipt before testing the shared retirement fence.
        let second = outbox.scan_failure_head(members[1]).unwrap().unwrap();
        let mut offset = 0;
        while offset < second.evidence.encoded_length {
            offset = outbox
                .read_scan_failure(&second, offset)
                .unwrap()
                .next_offset;
        }
        outbox.acknowledge_scan_failure(&second).unwrap();

        reject("reject_retire");
        let before = durable_and_ram();
        assert!(matches!(
            outbox.retire_scan_failure(&first),
            Err(ScanFailureOutboxError::Durability(_))
        ));
        assert_eq!(before, durable_and_ram());
        assert!(
            outbox.scan_failure_head(members[0]).unwrap().is_some(),
            "failed CAS must not pop the fence"
        );
        allow("reject_retire");
        outbox.retire_scan_failure(&first).unwrap();
        outbox.retire_scan_failure(&second).unwrap();

        reject("reject_finish");
        let before = durable_and_ram();
        assert!(matches!(
            outbox.finish_scan_failure_producer(&mut reservation),
            Err(ScanFailureOutboxError::Durability(_))
        ));
        assert_eq!(before, durable_and_ram());
        allow("reject_finish");
        outbox
            .finish_scan_failure_producer(&mut reservation)
            .unwrap();

        reject("reject_release");
        let before = durable_and_ram();
        assert!(matches!(
            outbox.release_scan_failure_receipt(&second),
            Err(ScanFailureOutboxError::Durability(_))
        ));
        assert_eq!(before, durable_and_ram());
        allow("reject_release");
        outbox.release_scan_failure_receipt(&second).unwrap();
        assert_eq!(
            outbox.scan_failure_recovery_owners()[0].released,
            Some(second)
        );

        drop(reservation);
        drop(injector);
        drop(outbox);
        drop(journal);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_owner_cas_recovers_an_ambiguous_commit_but_rejects_a_different_successor_after_reopen()
    {
        use crate::continuation_journal::JournalQuota;

        let dir = std::env::temp_dir().join(format!(
            "ubm-scan-owner-ambiguous-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("journal.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        let journal = Arc::new(ContinuationJournal::open(&path, "r", &json!({}), quota).unwrap());
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox
            .attach_journal(Arc::clone(&journal), json!({}))
            .unwrap();
        let member = ScanFailureMember {
            recipient: [6; 16],
            delivery: [7; 16],
        };
        let mut reservation = outbox
            .reserve_scan_failure(ScanFailureLimits {
                evidence: EvidenceLimits {
                    max_depth: 8,
                    max_tokens: 8,
                    max_encoded_bytes: 1024,
                    max_chunks: 8,
                    chunk_bytes: 8,
                },
                max_recipients: 1,
                max_bookkeeping_bytes: 1024,
            })
            .unwrap();
        outbox
            .commit_scan_failure(
                &mut reservation,
                &mut Some(ScanFailureOccurrence::new(
                    ScanFailureIdentity {
                        host: [1; 16],
                        scan: [2; 16],
                        start: [3; 16],
                        source_generation: [4; 16],
                        occurrence: [5; 16],
                    },
                    vec![member],
                    Arc::new(EvidenceSource::new(vec![
                        EvidenceToken::Array(3),
                        EvidenceToken::Null,
                        EvidenceToken::Null,
                        EvidenceToken::Null,
                    ])),
                )),
            )
            .unwrap();
        let (identity, previous, mut successor) = {
            let queues = lock(&outbox.queues);
            let slot = &queues.scan_failures[0];
            (
                slot.occurrence_identity.unwrap(),
                scan_owner_envelope(slot).unwrap(),
                slot.clone(),
            )
        };
        successor.producer_finished = true;
        let next = scan_owner_envelope(&successor).unwrap();
        // Model a server that committed the UPDATE but lost the response:
        // reopening finds exactly the intended successor, and replaying the
        // same CAS is idempotently successful.
        journal
            .advance_scan_failure_owner_envelope(identity, &previous, &next)
            .unwrap();
        drop(reservation);
        drop(outbox);
        drop(journal);
        let reopened_journal =
            Arc::new(ContinuationJournal::open(&path, "r", &json!({}), quota).unwrap());
        reopened_journal
            .advance_scan_failure_owner_envelope(identity, &previous, &next)
            .unwrap();
        let reopened = Outbox::new(2, Arc::new(NoWake));
        reopened
            .attach_journal(Arc::clone(&reopened_journal), json!({}))
            .unwrap();
        assert!(reopened.scan_failure_recovery_owners()[0].producer_finished);
        let mut altered = successor.clone();
        altered.attached = false;
        let altered = scan_owner_envelope(&altered).unwrap();
        let error = reopened_journal
            .advance_scan_failure_owner_envelope(identity, &previous, &altered)
            .unwrap_err();
        assert_eq!(error.kind, "storage.conflict");
        drop(reopened);
        drop(reopened_journal);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ordinal_exhaustion_refuses_admissions_without_wrapping() {
        let outbox = Outbox::new(1, Arc::new(NoWake));
        let mut token = outbox.reserve_control().unwrap();
        lock(&outbox.queues).ordinal = u64::MAX - 1;
        outbox.push_data(data(1)).unwrap();
        assert!(outbox.push_data(data(2)).is_err());
        assert!(
            outbox
                .commit_reserved_control(&mut token, &control(3))
                .is_err()
        );
        outbox.push_control(control(4));
        outbox.recording_changed("c", "r");
        let queues = lock(&outbox.queues);
        assert_eq!(queues.ordinal, u64::MAX);
        assert_eq!(queues.data.len(), 1);
        assert!(queues.control.is_empty());
        assert_eq!(queues.control_lost, 2);
    }

    #[test]
    fn ordinal_exhaustion_preserves_batch_tail_and_scan_source() {
        use ubm_core::oversized_evidence::{
            EvidenceLimits, EvidenceSource, EvidenceToken, ScanFailureIdentity,
        };
        let outbox = Outbox::new(1, Arc::new(NoWake));
        let limits = ScanFailureLimits {
            evidence: EvidenceLimits {
                max_depth: 4,
                max_tokens: 8,
                max_encoded_bytes: 100,
                max_chunks: 10,
                chunk_bytes: 32,
            },
            max_recipients: 1,
            max_bookkeeping_bytes: 1024,
        };
        let mut token = outbox.reserve_scan_failure(limits).unwrap();
        lock(&outbox.queues).ordinal = u64::MAX - 1;
        let batch = outbox.push_data_batch(vec![data(1), data(2)]);
        assert_eq!(batch.accepted, 1);
        assert!(batch.rejected.is_some());
        let mut source = Some(ScanFailureOccurrence::new(
            ScanFailureIdentity {
                host: [1; 16],
                scan: [2; 16],
                start: [3; 16],
                source_generation: [4; 16],
                occurrence: [5; 16],
            },
            vec![],
            Arc::new(EvidenceSource::new(vec![EvidenceToken::Null])),
        ));
        assert_eq!(
            outbox.commit_scan_failure(&mut token, &mut source),
            Err(ScanFailureOutboxError::OrdinalExhausted)
        );
        assert!(source.is_some());
        assert_eq!(outbox.pending_scan_failure_owners(), 0);
    }

    #[test]
    fn ordinal_exhaustion_refuses_before_journal_commit() {
        use crate::continuation_journal::JournalQuota;
        let dir = std::env::temp_dir().join(format!(
            "ubm-outbox-ordinal-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("journal.sqlite");
        let journal = Arc::new(
            ContinuationJournal::open(
                &path,
                "r",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 100,
                },
            )
            .unwrap(),
        );
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox.attach_journal(journal.clone(), json!({})).unwrap();
        outbox.register_journal_consumer("c", json!({})).unwrap();
        let count = || -> i64 {
            rusqlite::Connection::open(&path)
                .unwrap()
                .query_row("SELECT count(*) FROM records", [], |row| row.get(0))
                .unwrap()
        };
        let before = count();
        let mut token = outbox.reserve_control().unwrap();
        lock(&outbox.queues).ordinal = u64::MAX - 1;
        let tail = json!({"t":"value", "consumer":"unregistered"});
        let batch =
            outbox.push_data_batch(vec![json!({"t":"value", "consumer":"c"}), tail.clone()]);
        assert_eq!(batch.accepted, 1);
        let rejected = batch.rejected.unwrap();
        assert_eq!(rejected.items, 1);
        assert_eq!(rejected.bytes, tail.to_string().len() as u64);
        assert!(matches!(
            rejected.failure,
            DataIngressFailure::Overflow { .. }
        ));
        assert_eq!(count(), before + 1);
        lock(&outbox.queues).ordinal = u64::MAX;
        assert!(
            outbox
                .push_data(json!({"t":"value", "consumer":"c"}))
                .is_err()
        );
        assert!(
            outbox
                .commit_reserved_control(&mut token, &json!({"t":"scan-end"}))
                .is_err()
        );
        outbox.push_control(json!({"t":"scan-end"}));
        assert_eq!(count(), before + 1);
        drop(outbox);
        drop(journal);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn durable_prefix_storage_failure_precedes_ordinal_overflow() {
        use crate::continuation_journal::JournalQuota;
        let dir = std::env::temp_dir().join(format!(
            "ubm-outbox-prefix-failure-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("journal.sqlite");
        let journal = Arc::new(
            ContinuationJournal::open(
                &path,
                "r",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 100,
                },
            )
            .unwrap(),
        );
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox.attach_journal(journal.clone(), json!({})).unwrap();
        outbox.register_journal_consumer("c", json!({})).unwrap();
        let count = || -> i64 {
            rusqlite::Connection::open(&path)
                .unwrap()
                .query_row("SELECT count(*) FROM records", [], |row| row.get(0))
                .unwrap()
        };
        let before = count();
        lock(&outbox.queues).ordinal = u64::MAX - 2;
        let result = outbox.push_data_batch(vec![
            json!({"t":"value", "consumer":"c"}),
            json!({"t":"value", "consumer":"missing"}),
            json!({"t":"value", "consumer":"c"}),
        ]);
        assert_eq!(result.accepted, 0);
        let rejected = result.rejected.unwrap();
        assert!(matches!(
            rejected.failure,
            DataIngressFailure::Storage { .. }
        ));
        assert_eq!(rejected.items, 3);
        assert_eq!(count(), before);
        assert_eq!(lock(&outbox.queues).ordinal, u64::MAX - 2);
        drop(outbox);
        drop(journal);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ordinal_exhaustion_preserves_unreported_control_loss() {
        let outbox = Outbox::new(1, Arc::new(NoWake));
        lock(&outbox.queues).ordinal = u64::MAX;
        outbox.note_control_loss(1);
        assert!(outbox.claim_drain(1, 4096).is_err());
        assert_eq!(lock(&outbox.queues).control_lost, 1);
    }

    #[test]
    fn reserved_terminal_survives_saturation_and_claim_replay() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        let mut reservation = outbox.reserve_control().expect("reservation");
        for n in 0..CONTROL_RECORD_CAP {
            outbox.push_control(control(n as u64));
        }
        outbox.push_data(data(1)).expect("data");
        outbox
            .commit_reserved_control(
                &mut reservation,
                &json!({"t":"scan-end","reason":"radio-failed"}),
            )
            .expect("commit");
        outbox.push_data(data(2)).expect("data");
        let first = outbox
            .claim_drain(CONTROL_RECORD_CAP + 3, 1 << 20)
            .unwrap()
            .unwrap();
        assert_eq!(first.control_lost, 0);
        assert_eq!(first.records[CONTROL_RECORD_CAP]["n"], 1);
        assert_eq!(
            first.records[CONTROL_RECORD_CAP + 1]["reason"],
            "radio-failed"
        );
        assert_eq!(first.records[CONTROL_RECORD_CAP + 2]["n"], 2);
        assert_eq!(outbox.claim_drain(1, 1).unwrap().unwrap(), first);
        assert!(outbox.acknowledge_drain(first.id));
        assert!(outbox.claim_drain(10, 4096).unwrap().is_none());
    }

    #[test]
    fn terminal_committed_behind_claim_waits_for_exact_prefix_ack() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        let mut token = outbox.reserve_control().unwrap();
        outbox.push_control(control(1));
        let prefix = outbox.claim_drain(10, 4096).unwrap().unwrap();
        outbox
            .commit_reserved_control(&mut token, &json!({"t":"scan-end","id":2}))
            .unwrap();
        assert_eq!(outbox.claim_drain(10, 4096).unwrap().unwrap(), prefix);
        assert!(!outbox.acknowledge_drain(prefix.id + 1));
        assert!(outbox.acknowledge_drain(prefix.id));
        let terminal = outbox.claim_drain(10, 4096).unwrap().unwrap();
        assert_eq!(terminal.records.len(), 1);
        assert_eq!(terminal.records[0]["id"], 2);
        assert!(outbox.acknowledge_drain(terminal.id));
    }

    #[test]
    fn reservation_quota_follows_owners_and_exact_ack() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        let mut owners: Vec<_> = (0..RESERVED_CONTROL_CAP)
            .map(|_| outbox.reserve_control().unwrap())
            .collect();
        assert!(matches!(
            outbox.reserve_control(),
            Err(ControlReservationError::QuotaExceeded)
        ));
        drop(owners.pop());
        let mut replacement = outbox.reserve_control().unwrap();
        outbox
            .commit_reserved_control(&mut replacement, &json!({"t":"scan-end","id":1}))
            .unwrap();
        assert!(matches!(
            outbox.commit_reserved_control(&mut replacement, &json!({"t":"scan-end"})),
            Err(ControlReservationError::InvalidToken)
        ));
        assert!(matches!(
            outbox.reserve_control(),
            Err(ControlReservationError::QuotaExceeded)
        ));
        let claim = outbox.claim_drain(10, 4096).unwrap().unwrap();
        assert!(!outbox.acknowledge_drain(claim.id + 1));
        assert!(matches!(
            outbox.reserve_control(),
            Err(ControlReservationError::QuotaExceeded)
        ));
        assert!(outbox.acknowledge_drain(claim.id));
        assert!(outbox.acknowledge_drain(claim.id));
        assert!(outbox.reserve_control().is_ok());
        drop(owners);
        let all: Vec<_> = (0..RESERVED_CONTROL_CAP)
            .map(|_| outbox.reserve_control().unwrap())
            .collect();
        drop(all);
    }

    #[test]
    fn reservation_rejects_foreign_stale_oversized_and_closed_owners() {
        let first = Outbox::new(7, Arc::new(NoWake));
        let second = Outbox::new(7, Arc::new(NoWake));
        let mut token = first.reserve_control().unwrap();
        let terminal = json!({"t":"scan-end"});
        assert!(matches!(
            second.commit_reserved_control(&mut token, &terminal),
            Err(ControlReservationError::InvalidToken)
        ));
        assert!(matches!(
            first.commit_reserved_control(
                &mut token,
                &json!({"huge": "x".repeat(RESERVED_CONTROL_BYTES)})
            ),
            Err(ControlReservationError::RecordTooLarge)
        ));
        assert!(matches!(
            first.commit_reserved_control(&mut token, &json!(123)),
            Err(ControlReservationError::InvalidRecord)
        ));
        first.seal();
        assert!(
            first
                .commit_reserved_control(&mut token, &terminal)
                .is_err()
        );
        assert!(matches!(
            first.reserve_control(),
            Err(ControlReservationError::Sealed)
        ));
        assert_eq!(
            first.drain(10, 4096)["records"].as_array().unwrap().len(),
            0
        );
    }

    #[test]
    fn reserved_legacy_drain_frees_quota_and_preserves_other_owners() {
        let outbox = Outbox::new(1, Arc::new(NoWake));
        let mut first = outbox.reserve_control().unwrap();
        let mut second = outbox.reserve_control().unwrap();
        outbox
            .commit_reserved_control(&mut second, &json!({"t":"scan-end","id":2}))
            .unwrap();
        outbox
            .commit_reserved_control(&mut first, &json!({"t":"scan-end","id":1}))
            .unwrap();
        let batch = outbox.drain(10, 4096);
        assert_eq!(batch["records"][0]["id"], 2);
        assert_eq!(batch["records"][1]["id"], 1);
        assert!(matches!(
            outbox.claim_drain(10, 4096),
            Err(DrainClaimError::ModeConflict)
        ));
        let owners: Vec<_> = (0..RESERVED_CONTROL_CAP)
            .map(|_| outbox.reserve_control().unwrap())
            .collect();
        drop(owners);
    }

    #[test]
    fn reserved_commit_wakes_after_empty_claim_and_cleanup_does_not_remove_it() {
        use std::sync::atomic::AtomicUsize;
        struct Count(AtomicUsize);
        impl WakeSink for Count {
            fn wake(&self, _: u64) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let wake = Arc::new(Count(AtomicUsize::new(0)));
        let outbox = Outbox::new(1, wake.clone());
        let mut token = outbox.reserve_control().unwrap();
        assert!(outbox.claim_drain(1, 4096).unwrap().is_none());
        outbox
            .commit_reserved_control(&mut token, &json!({"t":"recording-changed","consumer":"c"}))
            .unwrap();
        assert_eq!(wake.0.load(Ordering::SeqCst), 1);
        outbox.remove_recording_hints("c");
        let claim = outbox.claim_drain(1, 4096).unwrap().unwrap();
        assert_eq!(claim.records[0]["consumer"], "c");
        assert!(outbox.acknowledge_drain(claim.id));
    }

    #[test]
    fn reserved_terminal_reports_journal_failure_and_remains_drainable() {
        use crate::continuation_journal::JournalQuota;
        let dir = std::env::temp_dir().join(format!(
            "ubm-reserved-journal-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let journal = Arc::new(
            ContinuationJournal::open(
                &dir.join("journal.sqlite"),
                "r",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 1,
                },
            )
            .unwrap(),
        );
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox.attach_journal(journal.clone(), json!({})).unwrap();
        let mut token = outbox.reserve_control().unwrap();
        outbox.push_control(json!({"t":"first"}));
        let outcome = outbox
            .commit_reserved_control(&mut token, &json!({"t":"scan-end","reason":"radio-failed"}))
            .unwrap();
        assert!(outcome.journal_error.is_some());
        assert!(outbox.journal_failure().is_some());
        let records = outbox.drain(10, 4096);
        assert_eq!(records["records"][1]["reason"], "radio-failed");
        drop(outbox);
        drop(journal);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn committed_terminal_survives_seal_but_unused_reservations_do_not() {
        let outbox = Outbox::new(3, Arc::new(NoWake));
        let mut committed = outbox.reserve_control().unwrap();
        let mut unused = outbox.reserve_control().unwrap();
        outbox
            .commit_reserved_control(&mut committed, &json!({"t":"scan-end","id":1}))
            .unwrap();
        outbox.seal();
        assert!(
            outbox
                .commit_reserved_control(&mut unused, &json!({"t":"scan-end","id":2}))
                .is_err()
        );
        let claim = outbox.claim_drain(10, 4096).unwrap().unwrap();
        assert_eq!(claim.records.len(), 1);
        assert_eq!(claim.records[0]["id"], 1);
        assert!(outbox.acknowledge_drain(claim.id));
    }

    #[test]
    fn ordinary_control_capacity_is_independent_of_reserved_entries() {
        let outbox = Outbox::new(4, Arc::new(NoWake));
        let mut token = outbox.reserve_control().unwrap();
        outbox
            .commit_reserved_control(&mut token, &json!({"t":"scan-end"}))
            .unwrap();
        for n in 0..CONTROL_RECORD_CAP {
            outbox.push_control(control(n as u64));
        }
        let batch = outbox.drain(CONTROL_RECORD_CAP + 1, 1 << 20);
        assert_eq!(batch["controlLost"], 0);
        assert_eq!(
            batch["records"].as_array().unwrap().len(),
            CONTROL_RECORD_CAP + 1
        );
    }

    #[test]
    fn reserved_terminal_is_journaled_once_and_memory_ack_is_independent() {
        use crate::continuation_journal::JournalQuota;
        let dir = std::env::temp_dir().join(format!(
            "ubm-reserved-success-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let journal = Arc::new(
            ContinuationJournal::open(
                &dir.join("journal.sqlite"),
                "r",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 100,
                },
            )
            .unwrap(),
        );
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox.attach_journal(journal.clone(), json!({})).unwrap();
        let mut token = outbox.reserve_control().unwrap();
        let record = json!({"t":"scan-end","reason":"radio-failed"});
        assert!(
            outbox
                .commit_reserved_control(&mut token, &record)
                .unwrap()
                .journal_error
                .is_none()
        );
        assert!(outbox.commit_reserved_control(&mut token, &record).is_err());
        let claim = outbox.claim_drain(10, 4096).unwrap().unwrap();
        assert!(outbox.acknowledge_drain(claim.id));
        let batch = journal.prepare(10, 4096).unwrap();
        assert_eq!(batch["records"].as_array().unwrap().len(), 1);
        assert_eq!(batch["records"][0]["record"], record);
        drop(outbox);
        drop(journal);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn journal_stop_from_ordinary_control_releases_unused_slots() {
        use crate::continuation_journal::JournalQuota;
        let dir = std::env::temp_dir().join(format!(
            "ubm-reserved-external-stop-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let journal = Arc::new(
            ContinuationJournal::open(
                &dir.join("journal.sqlite"),
                "r",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 100,
                },
            )
            .unwrap(),
        );
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox.attach_journal(journal.clone(), json!({})).unwrap();
        let token = outbox.reserve_control().unwrap();
        journal.stop().unwrap();
        outbox.push_control(json!({"t":"other"}));
        assert!(outbox.is_sealed());
        assert_eq!(lock(&outbox.queues).reservations.len(), 0);
        drop(token);
        drop(outbox);
        drop(journal);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stopped_journal_is_reported_with_original_terminal_retained() {
        use crate::continuation_journal::JournalQuota;
        let dir = std::env::temp_dir().join(format!(
            "ubm-reserved-stop-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let journal = Arc::new(
            ContinuationJournal::open(
                &dir.join("journal.sqlite"),
                "r",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 100,
                },
            )
            .unwrap(),
        );
        let outbox = Outbox::new(1, Arc::new(NoWake));
        outbox.attach_journal(journal.clone(), json!({})).unwrap();
        let mut token = outbox.reserve_control().unwrap();
        let mut other = outbox.reserve_control().unwrap();
        journal.stop().unwrap();
        let result = outbox
            .commit_reserved_control(&mut token, &json!({"t":"scan-end","reason":"radio-failed"}))
            .unwrap();
        assert_eq!(result.journal_error.unwrap().kind, "storage.stopped");
        assert!(outbox.journal_failure().is_none());
        assert!(outbox.is_sealed());
        assert!(
            outbox
                .commit_reserved_control(&mut other, &json!({"t":"scan-end"}))
                .is_err()
        );
        assert_eq!(
            outbox.drain(10, 4096)["records"][0]["reason"],
            "radio-failed"
        );
        drop(outbox);
        drop(journal);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claimed_drain_retains_prefix_until_exact_acknowledgement() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        outbox.push_data(data(1)).expect("data fits");
        outbox.push_control(control(2));
        let first = outbox
            .claim_drain(10, 4096)
            .expect("claim fits")
            .expect("records");
        assert_eq!(first.records.len(), 2);
        assert_eq!(first.records[0]["n"], 1);
        assert_eq!(first.records[1]["n"], 2);
        assert_eq!(outbox.queued_data(), 1);
        outbox.push_data(data(3)).expect("later data fits");
        assert_eq!(
            outbox.claim_drain(1, 1).expect("replay").expect("records"),
            first
        );
        assert!(!outbox.acknowledge_drain(first.id + 1));
        assert_eq!(outbox.queued_data(), 2);
        assert!(outbox.acknowledge_drain(first.id));
        assert!(
            outbox.acknowledge_drain(first.id),
            "a lost ACK reply is retryable"
        );
        let next = outbox
            .claim_drain(10, 4096)
            .expect("claim fits")
            .expect("records");
        assert_eq!(next.records.len(), 1);
        assert_eq!(next.records[0]["n"], 3);
    }

    #[test]
    fn claimed_control_is_immutable_across_later_coalescing_and_legacy_drain() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        outbox.push_ingress_drop_count(IngressClass::Advertisement, 2);
        let first = outbox
            .claim_drain(10, 4096)
            .expect("claim fits")
            .expect("records");
        outbox.push_ingress_drop_count(IngressClass::Advertisement, 3);
        assert_eq!(
            outbox
                .claim_drain(10, 4096)
                .expect("replay")
                .expect("records"),
            first
        );
        assert_eq!(outbox.drain(10, 4096)["error"], "drain-mode-conflict");
        assert!(outbox.acknowledge_drain(first.id));
        let next = outbox
            .claim_drain(10, 4096)
            .expect("claim fits")
            .expect("records");
        assert_eq!(next.records[0]["count"], 3);
    }

    #[test]
    fn oversized_claim_refusal_preserves_the_original_head() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        outbox.push_data(data(1)).expect("data fits");
        assert!(matches!(
            outbox.claim_drain(10, 1),
            Err(DrainClaimError::RecordTooLarge { .. })
        ));
        assert_eq!(outbox.queued_data(), 1);
        let claim = outbox
            .claim_drain(10, 4096)
            .expect("larger budget")
            .expect("records");
        assert_eq!(claim.records[0]["n"], 1);
    }

    #[test]
    fn claimed_hint_survives_route_retirement_and_later_hint_is_independent() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        outbox.recording_changed("consumer", "recording");
        let first = outbox
            .claim_drain(10, 4096)
            .expect("claim fits")
            .expect("records");
        outbox.recording_changed("consumer", "recording");
        assert_eq!(
            outbox
                .claim_drain(10, 4096)
                .expect("replay")
                .expect("records"),
            first
        );
        assert!(outbox.acknowledge_drain(first.id));
        let next = outbox
            .claim_drain(10, 4096)
            .expect("claim fits")
            .expect("records");
        outbox.remove_recording_hints("consumer");
        assert_eq!(
            outbox
                .claim_drain(10, 4096)
                .expect("replay")
                .expect("records"),
            next
        );
        assert!(outbox.acknowledge_drain(next.id));
        assert!(outbox.claim_drain(10, 4096).expect("empty").is_none());
    }

    #[test]
    fn claimed_ids_are_not_transferable_between_live_outboxes() {
        let first = Outbox::new(7, Arc::new(NoWake));
        let second = Outbox::new(7, Arc::new(NoWake));
        first.push_data(data(1)).expect("data fits");
        second.push_data(data(2)).expect("data fits");
        let first_claim = first.claim_drain(10, 4096).expect("claim").expect("record");
        let second_claim = second
            .claim_drain(10, 4096)
            .expect("claim")
            .expect("record");
        assert_ne!(first_claim.id, second_claim.id);
        assert!(!second.acknowledge_drain(first_claim.id));
        assert_eq!(second.queued_data(), 1);
        assert!(second.acknowledge_drain(second_claim.id));
    }

    #[test]
    fn chosen_drain_protocol_fences_legacy_and_claim_consumers_for_the_session() {
        let claimed = Outbox::new(7, Arc::new(NoWake));
        claimed.push_data(data(1)).expect("data fits");
        let first = claimed
            .claim_drain(10, 4096)
            .expect("claim")
            .expect("record");
        assert!(claimed.acknowledge_drain(first.id));
        claimed.push_data(data(2)).expect("data fits");
        assert_eq!(claimed.drain(10, 4096)["error"], "drain-mode-conflict");
        assert_eq!(claimed.queued_data(), 1);

        let legacy = Outbox::new(7, Arc::new(NoWake));
        assert!(
            legacy.drain(10, 4096)["records"]
                .as_array()
                .expect("array")
                .is_empty()
        );
        legacy.push_data(data(3)).expect("data fits");
        assert_eq!(
            legacy.claim_drain(10, 4096),
            Err(DrainClaimError::ModeConflict)
        );
        assert_eq!(legacy.queued_data(), 1);
    }

    #[test]
    fn concurrent_legacy_and_claimed_drains_cannot_both_take_ownership() {
        use std::sync::Barrier;
        let outbox = Arc::new(Outbox::new(7, Arc::new(NoWake)));
        outbox.push_data(data(1)).expect("data fits");
        let barrier = Arc::new(Barrier::new(3));
        let claimed = Arc::clone(&outbox);
        let claimed_barrier = Arc::clone(&barrier);
        let legacy = Arc::clone(&outbox);
        let legacy_barrier = Arc::clone(&barrier);
        let (claim_result, legacy_result) = std::thread::scope(|scope| {
            let claim_task = scope.spawn(move || {
                claimed_barrier.wait();
                claimed.claim_drain(10, 4096)
            });
            let legacy_task = scope.spawn(move || {
                legacy_barrier.wait();
                legacy.drain(10, 4096)
            });
            barrier.wait();
            (
                claim_task.join().expect("claim thread"),
                legacy_task.join().expect("legacy thread"),
            )
        });
        match claim_result {
            Ok(Some(claim)) => {
                assert_eq!(legacy_result["error"], "drain-mode-conflict");
                assert_eq!(claim.records[0]["n"], 1);
                assert!(outbox.acknowledge_drain(claim.id));
            }
            Err(DrainClaimError::ModeConflict) => assert_eq!(legacy_result["records"][0]["n"], 1),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn empty_claim_arms_the_next_producer_wake() {
        use std::sync::atomic::AtomicUsize;
        struct CountingWake(AtomicUsize);
        impl WakeSink for CountingWake {
            fn wake(&self, _session_id: u64) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let wake = Arc::new(CountingWake(AtomicUsize::new(0)));
        let outbox = Outbox::new(7, wake.clone());
        outbox.push_data(data(1)).expect("data fits");
        assert_eq!(wake.0.load(Ordering::SeqCst), 1);
        let claim = outbox
            .claim_drain(10, 4096)
            .expect("claim")
            .expect("record");
        assert!(outbox.acknowledge_drain(claim.id));
        assert!(outbox.claim_drain(10, 4096).expect("empty").is_none());
        outbox.push_data(data(2)).expect("data fits");
        assert_eq!(wake.0.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn recorded_hints_coalesce_until_drain_and_can_be_enqueued_again() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        outbox.recording_changed("consumer", "recording");
        outbox.recording_changed("consumer", "recording");
        let first = outbox.drain(10, 1024);
        assert_eq!(first["records"].as_array().unwrap().len(), 1);
        assert_eq!(first["records"][0]["recordingId"], "recording");
        outbox.recording_changed("consumer", "recording");
        assert_eq!(
            outbox.drain(10, 1024)["records"].as_array().unwrap().len(),
            1
        );
        outbox.recording_changed("consumer", "recording");
        outbox.remove_recording_hints("consumer");
        assert!(
            outbox.drain(10, 1024)["records"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn refused_recording_hint_increments_control_loss() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        for index in 0..CONTROL_RECORD_CAP {
            outbox.push_control(control(index as u64));
        }
        outbox.recording_changed("consumer", "recording");
        assert_eq!(outbox.drain(1, 1024)["controlLost"], 1);
    }

    /// X-R5: control loss is reported within a bounded number of drains even
    /// while data keeps arriving — never postponed until the queues empty.
    #[test]
    fn control_loss_is_visible_while_data_keeps_arriving() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        for n in 0..(CONTROL_RECORD_CAP as u64 + 7) {
            outbox.push_control(control(n));
        }
        for n in 0..50 {
            outbox.push_data(data(n)).expect("data fits");
        }
        for _ in 0..5 {
            let batch = outbox.drain(10, 65536);
            assert_eq!(batch["controlLost"], json!(7u64));
        }
    }

    /// The eventual in-band loss record still arrives once everything drains.
    #[test]
    fn control_loss_record_arrives_after_a_full_drain() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        for n in 0..(CONTROL_RECORD_CAP as u64 + 7) {
            outbox.push_control(control(n));
        }
        let mut seen = 0u64;
        for _ in 0..300 {
            let batch = outbox.drain(256, 1 << 20);
            for record in batch["records"].as_array().cloned().unwrap_or_default() {
                if record["t"] == json!("ingress-drop") && record["class"] == json!("control") {
                    seen = record["count"].as_u64().unwrap_or(0);
                }
            }
            if batch["more"] == json!(false) {
                break;
            }
        }
        assert_eq!(seen, 7u64);
    }

    #[test]
    fn peer_loss_remains_between_values_in_outbox() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        let loss = json!({"t":"peer-source-loss", "consumer":"c", "peerId":"p", "generation":"9", "skippedRecords":null});
        let records = vec![data(1), loss, data(2)];
        assert!(outbox.push_data_batch(records.clone()).rejected.is_none());
        let drained = outbox.drain(10, 4096);
        let drained = drained["records"].as_array().expect("records");
        assert_eq!(drained[0]["n"], 1);
        assert_eq!(drained[1]["t"], "peer-source-loss");
        assert_eq!(drained[1]["skippedRecords"], Value::Null);
        assert_eq!(drained[2]["n"], 2);
    }

    #[test]
    fn refused_peer_loss_is_control_loss_not_a_lost_value() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        outbox.seal();
        let value = data(2);
        let bytes = value.to_string().len() as u64;
        let loss = json!({"t":"peer-source-loss", "consumer":"c", "peerId":"p", "generation":"9", "skippedRecords":null});
        let outcome = outbox.push_data_batch(vec![loss, value]);
        let rejected = outcome.rejected.expect("sealed batch is refused");
        assert_eq!((rejected.items, rejected.bytes), (1, bytes));
        assert_eq!(
            outbox.after_cutoff_loss(),
            AfterCutoffLoss { items: 1, bytes }
        );
        assert_eq!(outbox.drain(10, 4096)["controlLost"], 1);
    }

    #[test]
    fn sealed_upstream_loss_is_counted_without_fabricating_payloads() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        outbox.note_after_cutoff_loss(3, 6);
        assert_eq!(outbox.after_cutoff_loss().items, 0);
        outbox.seal();
        outbox.note_after_cutoff_loss(2, 5);
        assert_eq!(
            outbox.after_cutoff_loss(),
            AfterCutoffLoss { items: 2, bytes: 5 }
        );
        assert_eq!(outbox.queued_data(), 0);
    }

    #[test]
    fn try_seal_reports_busy_and_source_terminals_stop_at_cutoff() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        let producer = lock(&outbox.queues);
        assert_eq!(outbox.try_seal(), None);
        assert!(!outbox.is_sealed());
        drop(producer);
        outbox.push_control_before_seal(json!({"t":"scan-end", "n":1}));
        assert_eq!(
            outbox.try_seal(),
            Some(AfterCutoffLoss { items: 0, bytes: 0 })
        );
        outbox.push_control_before_seal(json!({"t":"scan-end", "n":2}));
        let records = outbox.drain(10, 4096);
        assert_eq!(records["records"].as_array().unwrap().len(), 1);
        assert_eq!(records["records"][0]["n"], 1);
    }

    #[test]
    fn seal_orders_each_admission_into_the_handoff_or_explicit_loss() {
        let outbox = Outbox::new(7, Arc::new(NoWake));
        outbox
            .push_data(data(1))
            .expect("pre-cutoff record is retained");
        assert_eq!(outbox.seal(), AfterCutoffLoss { items: 0, bytes: 0 });
        let after = data(2);
        let bytes = after.to_string().len() as u64;
        assert!(
            outbox.push_data(after).is_err(),
            "post-cutoff data is refused"
        );

        let batch = outbox.drain(256, 1 << 20);
        assert_eq!(batch["more"], json!(false));
        assert_eq!(batch["records"].as_array().unwrap().len(), 1);
        assert_eq!(batch["records"][0]["n"], json!(1));
        assert_eq!(
            outbox.after_cutoff_loss(),
            AfterCutoffLoss { items: 1, bytes }
        );
    }
}
