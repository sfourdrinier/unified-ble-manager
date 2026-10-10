//! Per-session drain queues and wake arming.
//!
//! Two bounded queues share one session-monotonic ordinal: data (`adv`,
//! `value`) bounded by items and bytes, control (everything else) bounded
//! by items. Drain merges them by ordinal, so causality holds (a value
//! that arrived before a link loss drains before it), and a full data
//! queue never pushes out a control record.
//!
//! Wake arming (no lost wakeups): `armed` means JavaScript drained to
//! empty and waits. A producer that makes the session non-empty while
//! armed disarms and wakes exactly once. A drain that empties the queues
//! arms, then re-checks: a record that raced in after the take disarms
//! again and the drain answers `more: true`.

use crate::continuation_journal::{
    APPEND_BATCH_MAX, AppendBatch, ContinuationJournal, JournalError,
};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

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
/// Control records queued per session. Past this, control records are
/// counted and reported as one `ingress-drop{class:"control"}`.
pub const CONTROL_RECORD_CAP: usize = 1024;

struct Entry {
    ordinal: u64,
    record: Value,
    bytes: usize,
}

#[derive(Default)]
struct Queues {
    durable: Option<Durable>,
    data: VecDeque<Entry>,
    data_bytes: usize,
    control: VecDeque<Entry>,
    ordinal: u64,
    /// Control records refused past the cap, reported at the next drain.
    control_lost: u64,
    /// Every control record refused past the cap, ever. Reported in every
    /// drain response (X-R5): a drain that keeps arriving behind data still
    /// sees the gap within a bounded number of drains, without waiting for
    /// the queues to empty and without disturbing record ordinals.
    control_lost_total: u64,
    /// A continuation claim sealed this outbox. Data that reaches the
    /// process after that authoritative cutoff is not silently treated as
    /// backlog for the prior owner; it is counted for the handoff result.
    after_cutoff_items: u64,
    after_cutoff_bytes: u64,
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
    /// The bounded in-memory queue was full. Nothing was counted after a
    /// cutoff, whatever seals the outbox later.
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
    queues: Mutex<Queues>,
    armed: AtomicBool,
    wake: Arc<dyn WakeSink>,
    observer: Arc<Mutex<Option<Observer>>>,
    sealed: AtomicBool,
    durable_enabled: AtomicBool,
}

fn with_ordinal(mut record: Value, ordinal: u64) -> Value {
    if let Value::Object(map) = &mut record {
        map.insert("ordinal".to_owned(), Value::from(ordinal));
    }
    record
}

impl Outbox {
    /// Select one durable cursor before consumer/data ingress or control delivery.
    /// Undelivered process controls may precede the blocking journal setup.
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
            Self::persist(&mut durable, &entry.record)?;
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
        if let Err(failure) = durable.journal.append(
            &context,
            &serde_json::json!({"t":"consumer-registration","consumer":consumer}),
        ) {
            if failure.kind == "storage.stopped" {
                self.sealed.store(true, Ordering::SeqCst);
                lock(&self.observer).take();
                return Err(failure);
            }
            durable.journal.mark_collection_failure(&failure);
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
            durable.journal.mark_collection_failure(failure);
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
        let result = durable
            .journal
            .append(
                &serde_json::json!({"session":durable.context,"consumer":metadata}),
                record,
            )
            .map(|_| ());
        if let Err(failure) = &result {
            Self::retain_failure(durable, failure);
        }
        result
    }

    /// Data records share one journal transaction. Every record needs a
    /// committed consumer registration; one without it fails the whole batch
    /// before any write, so no prefix is claimed.
    fn persist_data(durable: &mut Durable, records: &[Value]) -> Result<AppendBatch, JournalError> {
        if let Some(failure) = &durable.failure {
            return Err(failure.clone());
        }
        let mut contexts = Vec::with_capacity(records.len());
        for record in records {
            let Some(metadata) = record["consumer"]
                .as_str()
                .and_then(|consumer| durable.consumers.get(consumer))
            else {
                let failure =
                    JournalError::invalid("durable value has no committed consumer registration");
                Self::retain_failure(durable, &failure);
                return Err(failure);
            };
            contexts.push(serde_json::json!({"session":durable.context,"consumer":metadata}));
        }
        let items: Vec<(&Value, &Value)> = contexts.iter().zip(records).collect();
        let result = durable.journal.append_batch(&items);
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
            queues: Mutex::new(Queues::default()),
            armed: AtomicBool::new(true),
            wake,
            observer: Arc::default(),
            sealed: AtomicBool::new(false),
            durable_enabled: AtomicBool::new(false),
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
                    tail_items = tail_items.saturating_add(1);
                    tail_bytes = tail_bytes.saturating_add(record.to_string().len() as u64);
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
        let sizes: Vec<usize> = records
            .iter()
            .map(|record| record.to_string().len())
            .collect();
        let refuse = |accepted: usize, failure: DataIngressFailure| DataBatchOutcome {
            accepted,
            rejected: Some(DataBatchRejection {
                failure,
                items: (sizes.len() - accepted) as u64,
                bytes: sizes[accepted..].iter().map(|bytes| *bytes as u64).sum(),
            }),
        };
        let mut queues = lock(&self.queues);
        if self.is_sealed() {
            Self::count_after_cutoff(&mut queues, &sizes);
            return refuse(0, DataIngressFailure::Sealed { bytes: sizes[0] });
        }
        if let Some(durable) = queues.durable.as_mut() {
            let batch = match Self::persist_data(durable, &records) {
                Ok(batch) => batch,
                Err(error) if error.kind == "storage.stopped" => {
                    self.sealed.store(true, Ordering::SeqCst);
                    Self::count_after_cutoff(&mut queues, &sizes);
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
            queues.ordinal += batch.accepted as u64;
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
            queues.ordinal += 1;
            let ordinal = queues.ordinal;
            queues.data_bytes += bytes;
            Self::observe_admitted(&self.observer, &record);
            queues.data.push_back(Entry {
                ordinal,
                record,
                bytes,
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
        queues.after_cutoff_items = queues.after_cutoff_items.saturating_add(sizes.len() as u64);
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
        let queues = lock(&self.queues);
        self.sealed.store(true, Ordering::SeqCst);
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

    /// Queue one control record.
    pub fn push_control(&self, record: Value) {
        self.push_control_record(record, false);
    }

    // Journal admission precedes the memory optimization under the same lock
    // that orders journal attachment and sealing. Durable loss is append-only:
    // a prepared prefix can never be changed by coalescing a later delta.
    fn push_control_record(&self, record: Value, coalesce_loss: bool) {
        {
            let mut queues = lock(&self.queues);
            if !self.is_sealed()
                && let Some(durable) = queues.durable.as_mut()
                && let Err(error) = Self::persist(durable, &record)
            {
                if error.kind == "storage.stopped" {
                    self.sealed.store(true, Ordering::SeqCst);
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
            let coalesced = coalesce_loss
                && queues.control.back_mut().is_some_and(|tail| {
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
            if !coalesced && queues.control.len() >= CONTROL_RECORD_CAP {
                queues.control_lost += 1;
                queues.control_lost_total += 1;
            } else if !coalesced {
                queues.ordinal += 1;
                let ordinal = queues.ordinal;
                queues.control.push_back(Entry {
                    ordinal,
                    record,
                    bytes: 0,
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

    /// Take up to `max_items` records (at least one when any is waiting)
    /// within `max_bytes` of encoded data, in ordinal order.
    #[must_use]
    pub fn drain(&self, max_items: usize, max_bytes: usize) -> Value {
        let mut records = Vec::new();
        let mut taken_bytes = 0usize;
        let more = {
            let mut queues = lock(&self.queues);
            while records.len() < max_items.max(1) {
                let data_head = queues.data.front().map(|entry| entry.ordinal);
                let control_head = queues.control.front().map(|entry| entry.ordinal);
                let take_data = match (data_head, control_head) {
                    (Some(d), Some(c)) => d < c,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => break,
                };
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
                queues.ordinal += 1;
                let ordinal = queues.ordinal;
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
