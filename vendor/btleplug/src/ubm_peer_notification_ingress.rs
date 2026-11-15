// vendor/btleplug/src/ubm_peer_notification_ingress.rs
//! A bounded peer-ingress source owner for desktop radio notifications.
//!
//! The integrating source owner MUST serialize registration and retirement with
//! callback acceptance at the native source, before any buffered event stream.
//! The source must admit or reserve each callback before buffering or deferring
//! it, and serialize admission with `register` and `retire`. Alternatively, an
//! actor barrier must drain all previously accepted callbacks before retirement.
//! Capturing a token without admission is not a fence.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Host-installed terminal-evidence boundary.  Native source ownership keeps
/// this opaque token from preactivation through its final ACK; the host commits
/// canonical bytes before acknowledging the source.  The vendor never picks a
/// filesystem path or provides a volatile fallback.
pub trait TerminalEvidencePort: Send + Sync {
    fn reserve(
        &self,
        producer: &str,
        generation: u64,
        maximum_bytes: usize,
    ) -> std::io::Result<String>;
    fn release(&self, reservation: &str) -> std::io::Result<()>;
}

use crate::api::{NotificationEvent, ValueNotification};
use tokio::sync::Notify;

/// Completion for one source-time recovery recipient.  The source owner keeps
/// the original native cut until every distinct ticket has completed after its
/// durable recovery receipt is retired.
pub(crate) trait FailureRecoveryFinalizer: Send + Sync {
    /// Retire the last source-time recipient. An exact evidence store may
    /// refuse final deletion; in that case the journal receipt and source
    /// recovery remain retryable rather than reporting a completed ACK.
    fn complete(&self) -> std::io::Result<()>;
}

/// Complete native GATT identity; UUIDs alone do not distinguish siblings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationRoute {
    pub service_uuid: uuid::Uuid,
    pub service_instance: u64,
    pub characteristic_uuid: uuid::Uuid,
    pub characteristic_instance: u64,
}

impl NotificationRoute {
    pub fn matches(&self, value: &ValueNotification) -> bool {
        self.service_uuid == value.service_uuid
            && self.service_instance == value.service_instance
            && self.characteristic_uuid == value.uuid
            && self.characteristic_instance == value.instance
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Membership {
    pub member: u64,
    pub epoch: u64,
}

/// What the native callback establishes about the origin of its value.
/// This does not distinguish BLE notifications from indications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackProvenance {
    Notification,
    ReadOrNotification,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressRecord {
    Value {
        ordinal: u64,
        membership: Membership,
        provenance: CallbackProvenance,
        value: ValueNotification,
    },
    Loss {
        skipped_records: u64,
        memberships: Vec<(NotificationRoute, Membership)>,
    },
    Fence {
        membership: Membership,
    },
    Barrier {
        id: u64,
    },
    Marker {
        id: u64,
        ordinal: u64,
    },
    /// A failure from an admitted native operation. Multiple failures can
    /// precede a single source terminal, and each has its own FIFO identity.
    FailureOccurrence {
        error: SharedPlatformError,
        memberships: Vec<(NotificationRoute, Membership)>,
    },
    Terminal {
        reason: TerminalReason,
        memberships: Vec<(NotificationRoute, Membership)>,
    },
}

impl IngressRecord {
    fn complete_failure_recovery_finalizer(&self) -> std::io::Result<()> {
        match self {
            Self::FailureOccurrence { error, .. } => error.complete_recovery_finalizer(),
            Self::Terminal {
                reason: TerminalReason::SourceFailure { error, .. },
                ..
            } => error.complete_recovery_finalizer(),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectedRecord {
    Value { bytes: usize },
    Loss { skipped_records: u64 },
    Fence { membership: Membership },
    Barrier { id: u64 },
    Marker { id: u64 },
}

/// Shared native evidence retained by the source and every terminal recipient.
/// Cloning this handle never copies the potentially large native payload.
#[derive(Clone)]
pub struct SharedPlatformError {
    summary: Arc<crate::PlatformError>,
    exact: ExactPlatformError,
    finalizer: Option<Arc<dyn FailureRecoveryFinalizer>>,
}

#[derive(Clone)]
enum ExactPlatformError {
    Inline(Arc<crate::PlatformError>),
    Durable(Arc<dyn FailureEvidence>),
}

impl std::fmt::Debug for SharedPlatformError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedPlatformError")
            .field("summary", &self.summary)
            .field(
                "exact_bytes",
                &match &self.exact {
                    ExactPlatformError::Inline(error) => platform_error_encoded_len(error),
                    ExactPlatformError::Durable(evidence) => evidence.len(),
                },
            )
            .finish()
    }
}

impl PartialEq for SharedPlatformError {
    fn eq(&self, other: &Self) -> bool {
        self.summary == other.summary
            && match (&self.exact, &other.exact) {
                (ExactPlatformError::Inline(left), ExactPlatformError::Inline(right)) => {
                    left == right
                }
                (ExactPlatformError::Durable(left), ExactPlatformError::Durable(right)) => {
                    Arc::ptr_eq(left, right)
                }
                _ => false,
            }
    }
}

impl Eq for SharedPlatformError {}

impl SharedPlatformError {
    pub fn as_arc(&self) -> &Arc<crate::PlatformError> {
        &self.summary
    }

    pub(crate) fn store_exact(
        error: crate::PlatformError,
        store: &dyn FailureEvidenceStore,
    ) -> std::io::Result<Self> {
        let exact = store.store_exact(&encode_platform_error(&error))?;
        let summary = Arc::new(
            crate::PlatformError::new(
                error.domain,
                "recovery-required",
                "exact native failure evidence is retained by the configured recovery store",
            )
            .with("representation", "ubm-source-failure/1"),
        );
        Ok(Self {
            summary,
            exact: ExactPlatformError::Durable(exact),
            finalizer: None,
        })
    }

    /// Bind a distinct source-time recipient finalizer. A shared descriptor
    /// must never let one recipient release another recipient's source hold.
    pub(crate) fn with_recovery_finalizer(
        mut self,
        finalizer: Arc<dyn FailureRecoveryFinalizer>,
    ) -> Self {
        self.finalizer = Some(finalizer);
        self
    }

    fn write_exact(&self, writer: &mut RecoveryWriter) -> Option<()> {
        match &self.exact {
            ExactPlatformError::Inline(error) => writer.platform_error(error),
            ExactPlatformError::Durable(evidence) => writer.evidence(evidence.as_ref()),
        }
    }

    fn complete_recovery_finalizer(&self) -> std::io::Result<()> {
        if let Some(finalizer) = &self.finalizer {
            finalizer.complete()
        } else {
            Ok(())
        }
    }

    pub(crate) fn retire_exact_evidence(&self) -> std::io::Result<()> {
        match &self.exact {
            ExactPlatformError::Inline(_) => Ok(()),
            ExactPlatformError::Durable(evidence) => evidence.retire(),
        }
    }
}

impl From<crate::PlatformError> for SharedPlatformError {
    fn from(error: crate::PlatformError) -> Self {
        let error = Arc::new(error);
        Self {
            summary: error.clone(),
            exact: ExactPlatformError::Inline(error),
            finalizer: None,
        }
    }
}

impl From<Arc<crate::PlatformError>> for SharedPlatformError {
    fn from(error: Arc<crate::PlatformError>) -> Self {
        Self {
            summary: error.clone(),
            exact: ExactPlatformError::Inline(error),
            finalizer: None,
        }
    }
}

impl std::ops::Deref for SharedPlatformError {
    type Target = crate::PlatformError;
    fn deref(&self) -> &Self::Target {
        &self.summary
    }
}

impl PartialEq<crate::PlatformError> for SharedPlatformError {
    fn eq(&self, other: &crate::PlatformError) -> bool {
        matches!(&self.exact, ExactPlatformError::Inline(error) if error.as_ref() == other)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalReason {
    Closed {
        tail_uncertain: bool,
    },
    Overflow {
        rejected: RejectedRecord,
        tail_uncertain: bool,
    },
    OrdinalExhausted {
        rejected_bytes: usize,
        tail_uncertain: bool,
    },
    MarkerOrdinalExhausted {
        id: u64,
        tail_uncertain: bool,
    },
    SourceFailure {
        membership: Option<Membership>,
        error: SharedPlatformError,
        rejected_loss: Option<u64>,
        tail_uncertain: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngressError {
    StaleGeneration,
    RouteOccupied,
    NotCurrent,
    Terminated,
    EpochExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    Record(IngressRecord),
    Empty,
    Finished,
}

/// A view of one source occurrence. The opaque identity distinguishes even
/// byte-for-byte identical consecutive records and cannot cross source owners.
#[derive(Debug, Clone)]
pub struct IngressTransfer {
    pub record: Arc<IngressRecord>,
    identity: Arc<()>,
}

/// Maximum raw bytes returned by one internal retained-failure read.
pub const FAILURE_RECOVERY_MAX_CHUNK: usize = 64 * 1024;

/// Host-owned durable storage for raw failure evidence. The BLE source never
/// chooses a directory: callers configure this store explicitly and retain
/// authority over its lifecycle and retention policy.
///
/// A full store applies backpressure. It does not authorize advancing a
/// source head or replacing exact evidence with a truncated diagnostic.
pub trait FailureEvidenceStore: Send + Sync {
    fn store_exact(&self, bytes: &[u8]) -> std::io::Result<std::sync::Arc<dyn FailureEvidence>>;
    /// Reserve one unavoidable raw native callback before a source enters the
    /// platform operation that may produce it. The reservation is released
    /// after exact bytes reach the store, or when the source abandons its
    /// pre-admitted cut. Stores that cannot account globally return `None`;
    /// configured file stores provide a bounded reservation.
    fn reserve_pending_failure(
        &self,
    ) -> std::io::Result<Option<std::sync::Arc<dyn PendingFailureReservation>>> {
        Ok(None)
    }
    /// Notified after settled evidence returns capacity. A source may wait on
    /// this instead of discarding the one raw callback it is retrying.
    fn capacity_released(&self) -> std::sync::Arc<Notify>;
}

/// One globally accounted raw callback that has not reached durable evidence
/// storage yet. Releasing the final owner returns the slot to the configured
/// store and wakes sources waiting for backpressure.
pub trait PendingFailureReservation: Send + Sync {}

/// One exact, independently-owned raw evidence object. A custom store may use
/// an encrypted database, a host vault, or another durable medium; the source
/// only needs bounded range reads and deterministic cleanup.
pub trait FailureEvidence: Send + Sync {
    fn len(&self) -> usize;
    fn read(&self, offset: usize, max_bytes: usize) -> std::io::Result<Vec<u8>>;
    /// Retire the exact source evidence after every source-time recipient has
    /// durably committed. Failure preserves the record for an explicit retry.
    fn retire(&self) -> std::io::Result<()>;
}

/// A configured, private-directory implementation of [`FailureEvidenceStore`].
/// Each live evidence owner has one create-new file; dropping its final owner
/// removes that file and returns its bytes to the aggregate budget.
pub struct FileFailureEvidenceStore {
    directory: std::sync::Arc<std::path::PathBuf>,
    max_bytes: usize,
    active_bytes: std::sync::Arc<Mutex<usize>>,
    capacity_released: std::sync::Arc<Notify>,
    next: std::sync::atomic::AtomicU64,
    max_pending_failures: usize,
    pending_failures: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl FileFailureEvidenceStore {
    /// Create a store in a host-selected directory. The caller must choose a
    /// directory with the retention and access controls appropriate for raw
    /// native error evidence; this library does not select a system path.
    pub fn open(
        directory: impl Into<std::path::PathBuf>,
        max_bytes: usize,
    ) -> std::io::Result<Self> {
        Self::open_with_limits(directory, max_bytes, 1)
    }

    /// Open with an explicit global bound for raw callbacks that have been
    /// admitted by native code but have not yet been copied into this store.
    /// The bound is shared across every peer using this store, not per peer.
    pub fn open_with_limits(
        directory: impl Into<std::path::PathBuf>,
        max_bytes: usize,
        max_pending_failures: usize,
    ) -> std::io::Result<Self> {
        if max_pending_failures == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "failure-evidence pending reservation limit must be positive",
            ));
        }
        let directory = directory.into();
        std::fs::create_dir_all(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        // A crash can leave an exact spill behind after the durable recovery
        // ledger has not yet recorded its final receipt. Never reset the live
        // budget to zero in that state: account every file this store owns and
        // allocate subsequent identities after the greatest recovered one.
        // The host ledger remains responsible for reconciling or retiring the
        // orphan; until then it deliberately consumes capacity rather than
        // letting a new source obscure a retained native failure.
        let mut recovered_bytes = 0usize;
        let mut greatest_id = 0u64;
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(id) = name
                .strip_prefix("failure-")
                .and_then(|name| name.strip_suffix(".bin"))
                .filter(|id| id.len() == 16)
                .and_then(|id| u64::from_str_radix(id, 16).ok())
            else {
                continue;
            };
            recovered_bytes = recovered_bytes
                .checked_add(usize::try_from(entry.metadata()?.len()).map_err(|_| {
                    std::io::Error::other("recovered failure-evidence file is too large")
                })?)
                .ok_or_else(|| {
                    std::io::Error::other("recovered failure-evidence budget overflow")
                })?;
            greatest_id = greatest_id.max(id);
        }
        let next = greatest_id
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("failure-evidence identity exhausted"))?;
        Ok(Self {
            directory: std::sync::Arc::new(directory),
            max_bytes,
            active_bytes: std::sync::Arc::new(Mutex::new(recovered_bytes)),
            capacity_released: std::sync::Arc::new(Notify::new()),
            next: std::sync::atomic::AtomicU64::new(next),
            max_pending_failures,
            pending_failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }
}

impl FailureEvidenceStore for FileFailureEvidenceStore {
    fn store_exact(&self, bytes: &[u8]) -> std::io::Result<std::sync::Arc<dyn FailureEvidence>> {
        let length = bytes.len();
        let mut active = self
            .active_bytes
            .lock()
            .expect("failure evidence budget poisoned");
        if length > self.max_bytes.saturating_sub(*active) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "configured failure-evidence store is full",
            ));
        }
        let (path, mut file) = loop {
            let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if id == u64::MAX {
                return Err(std::io::Error::other("failure-evidence identity exhausted"));
            }
            let path = self.directory.join(format!("failure-{id:016x}.bin"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => break (path, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        use std::io::Write;
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        *active = active.saturating_add(length);
        drop(active);
        Ok(std::sync::Arc::new(FileFailureEvidence {
            path,
            length,
            active_bytes: self.active_bytes.clone(),
            capacity_released: self.capacity_released.clone(),
            retired: Mutex::new(false),
        }))
    }

    fn capacity_released(&self) -> std::sync::Arc<Notify> {
        self.capacity_released.clone()
    }

    fn reserve_pending_failure(
        &self,
    ) -> std::io::Result<Option<std::sync::Arc<dyn PendingFailureReservation>>> {
        loop {
            let current = self
                .pending_failures
                .load(std::sync::atomic::Ordering::Acquire);
            if current >= self.max_pending_failures {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "configured failure-evidence pending reservation limit is full",
                ));
            }
            if self
                .pending_failures
                .compare_exchange_weak(
                    current,
                    current + 1,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
            {
                return Ok(Some(std::sync::Arc::new(FilePendingFailureReservation {
                    pending_failures: self.pending_failures.clone(),
                    capacity_released: self.capacity_released.clone(),
                })));
            }
        }
    }
}

struct FilePendingFailureReservation {
    pending_failures: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    capacity_released: std::sync::Arc<Notify>,
}

impl PendingFailureReservation for FilePendingFailureReservation {}

impl Drop for FilePendingFailureReservation {
    fn drop(&mut self) {
        self.pending_failures
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.capacity_released.notify_waiters();
    }
}

/// Exact evidence retained by [`FileFailureEvidenceStore`]. It is intentionally
/// non-cloneable: a source transfer owns one durable record until its final
/// ACK, cancellation recovery, or teardown owner settles it.
pub struct FileFailureEvidence {
    path: std::path::PathBuf,
    length: usize,
    active_bytes: std::sync::Arc<Mutex<usize>>,
    capacity_released: std::sync::Arc<Notify>,
    retired: Mutex<bool>,
}

impl FileFailureEvidence {
    pub fn len(&self) -> usize {
        self.length
    }

    pub fn read(&self, offset: usize, max_bytes: usize) -> std::io::Result<Vec<u8>> {
        if offset > self.length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "failure evidence offset is past the retained record",
            ));
        }
        let count = max_bytes.min(self.length - offset);
        let mut file = std::fs::File::open(&self.path)?;
        use std::io::{Read, Seek, SeekFrom};
        file.seek(SeekFrom::Start(offset as u64))?;
        let mut bytes = vec![0; count];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }
}

impl FailureEvidence for FileFailureEvidence {
    fn len(&self) -> usize {
        self.len()
    }

    fn read(&self, offset: usize, max_bytes: usize) -> std::io::Result<Vec<u8>> {
        self.read(offset, max_bytes)
    }

    fn retire(&self) -> std::io::Result<()> {
        let mut retired = self
            .retired
            .lock()
            .expect("failure evidence retirement poisoned");
        if *retired {
            return Ok(());
        }
        std::fs::remove_file(&self.path)?;
        let mut active = self
            .active_bytes
            .lock()
            .expect("failure evidence budget poisoned");
        *active = active.saturating_sub(self.length);
        drop(active);
        *retired = true;
        self.capacity_released.notify_waiters();
        Ok(())
    }
}

impl Drop for FileFailureEvidence {
    fn drop(&mut self) {
        // Drop cannot honestly report a filesystem failure. Normal cleanup is
        // therefore explicit and fallible through `FailureEvidence::retire`.
        // An unretired record is intentionally left for restart reconciliation.
    }
}

/// An in-memory reader for the exact source head. This is not a durable copy
/// or a public transport capability. The embedding host must bind its own
/// opaque capability to this reader and its authorized collection owner.
pub struct FailureRecovery {
    ingress: PeerNotificationIngress,
    transfer: IngressTransfer,
    generation: u64,
    encoded_len: usize,
    state: Mutex<FailureRecoveryState>,
}

/// State retained between source acknowledgement and the durable owner's final
/// retirement.  A source acknowledgement advances the live ingress head, but
/// must not make the exact recovery bytes disappear while a journal is still
/// committing its receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FailureRecoveryState {
    contiguous_read: usize,
    source_acknowledged: bool,
    retired: bool,
}

// Representation ubm-source-failure/1: magic, generation (u64 BE), kind
// (1 occurrence, 2 terminal), terminal-only optional membership, optional
// rejected-loss and tail flag, followed by a u64 count of (route, membership)
// pairs and the error. UUIDs are 16 bytes; integer fields are u64 BE; options
// start with 0/1. Error domain, code, message and each metadata key/value are
// u64-byte-length-prefixed UTF-8, in their original order, including duplicate
// metadata keys. This serializes the retained value, not native wire bytes.
struct RecoveryWriter {
    offset: usize,
    cursor: usize,
    limit: usize,
    bytes: Vec<u8>,
}

impl RecoveryWriter {
    fn write(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.cursor.checked_add(bytes.len())?;
        let start_copy = self.offset.max(self.cursor);
        let end_copy = self.offset.saturating_add(self.limit).min(end);
        if start_copy < end_copy {
            self.bytes
                .extend_from_slice(&bytes[start_copy - self.cursor..end_copy - self.cursor]);
        }
        self.cursor = end;
        Some(())
    }
    fn number(&mut self, value: u64) -> Option<()> {
        self.write(&value.to_be_bytes())
    }
    fn text(&mut self, value: &str) -> Option<()> {
        self.number(u64::try_from(value.len()).ok()?)?;
        self.write(value.as_bytes())
    }
    fn membership(&mut self, member: Membership) -> Option<()> {
        self.number(member.member)?;
        self.number(member.epoch)
    }

    fn platform_error(&mut self, error: &crate::PlatformError) -> Option<()> {
        self.text(error.domain)?;
        self.text(&error.code)?;
        self.text(&error.message)?;
        self.number(u64::try_from(error.metadata.len()).ok()?)?;
        for (key, value) in &error.metadata {
            self.text(key)?;
            self.text(value)?;
        }
        Some(())
    }

    fn evidence(&mut self, evidence: &dyn FailureEvidence) -> Option<()> {
        let length = evidence.len();
        let end = self.cursor.checked_add(length)?;
        let start_copy = self.offset.max(self.cursor);
        let end_copy = self.offset.saturating_add(self.limit).min(end);
        if start_copy < end_copy {
            let offset = start_copy - self.cursor;
            let bytes = evidence.read(offset, end_copy - start_copy).ok()?;
            if bytes.len() != end_copy - start_copy {
                return None;
            }
            self.bytes.extend_from_slice(&bytes);
        }
        self.cursor = end;
        Some(())
    }
}

fn encode_platform_error(error: &crate::PlatformError) -> Vec<u8> {
    let length = platform_error_encoded_len(error);
    let mut writer = RecoveryWriter {
        offset: 0,
        cursor: 0,
        limit: length,
        bytes: Vec::with_capacity(length),
    };
    writer
        .platform_error(error)
        .expect("platform error encoding is representable");
    writer.bytes
}

fn platform_error_encoded_len(error: &crate::PlatformError) -> usize {
    let text = |value: &str| 8usize.saturating_add(value.len());
    error.metadata.iter().fold(
        text(error.domain)
            .saturating_add(text(&error.code))
            .saturating_add(text(&error.message))
            .saturating_add(8),
        |total, (key, value)| total.saturating_add(text(key)).saturating_add(text(value)),
    )
}

fn encode_failure_recovery(
    record: &IngressRecord,
    generation: u64,
    offset: usize,
    limit: usize,
) -> Option<RecoveryWriter> {
    let (kind, error, memberships, terminal) = match record {
        IngressRecord::FailureOccurrence { error, memberships } => (1, error, memberships, None),
        IngressRecord::Terminal {
            reason:
                TerminalReason::SourceFailure {
                    membership,
                    error,
                    rejected_loss,
                    tail_uncertain,
                },
            memberships,
        } => (
            2,
            error,
            memberships,
            Some((*membership, *rejected_loss, *tail_uncertain)),
        ),
        _ => return None,
    };
    let mut out = RecoveryWriter {
        offset,
        cursor: 0,
        limit,
        bytes: Vec::with_capacity(limit),
    };
    out.write(b"ubm-source-failure/1\0")?;
    out.number(generation)?;
    out.write(&[kind])?;
    if let Some((member, loss, tail)) = terminal {
        out.write(&[u8::from(member.is_some())])?;
        if let Some(member) = member {
            out.membership(member)?;
        }
        out.write(&[u8::from(loss.is_some())])?;
        if let Some(loss) = loss {
            out.number(loss)?;
        }
        out.write(&[u8::from(tail)])?;
    }
    out.number(u64::try_from(memberships.len()).ok()?)?;
    for (route, member) in memberships {
        out.write(route.service_uuid.as_bytes())?;
        out.number(route.service_instance)?;
        out.write(route.characteristic_uuid.as_bytes())?;
        out.number(route.characteristic_instance)?;
        out.membership(*member)?;
    }
    error.write_exact(&mut out)?;
    Some(out)
}

impl FailureRecovery {
    pub fn encoded_len(&self) -> usize {
        self.encoded_len
    }

    /// Repeatable byte-range read. Only contiguous reads count toward the
    /// completion prerequisite; an out-of-order read cannot authorize an ACK.
    pub fn read(&self, offset: usize, max_bytes: usize) -> Option<Vec<u8>> {
        if max_bytes == 0 || max_bytes > FAILURE_RECOVERY_MAX_CHUNK || offset > self.encoded_len {
            return None;
        }
        let mut recovery = self.state.lock().expect("failure recovery lock poisoned");
        if recovery.retired {
            return None;
        }
        let state = self
            .ingress
            .0
            .state
            .lock()
            .expect("peer ingress lock poisoned");
        if !recovery.source_acknowledged
            && (!Arc::ptr_eq(&state.transfer_identity, &self.transfer.identity)
                || !state
                    .transfer_record
                    .as_ref()
                    .is_some_and(|record| Arc::ptr_eq(record, &self.transfer.record)))
        {
            return None;
        }
        drop(state);
        let out =
            encode_failure_recovery(&self.transfer.record, self.generation, offset, max_bytes)?;
        let end = offset.checked_add(out.bytes.len())?;
        if offset <= recovery.contiguous_read {
            recovery.contiguous_read = recovery.contiguous_read.max(end);
        }
        Some(out.bytes)
    }

    /// Advance the live source head after a full, contiguous read while
    /// retaining the exact representation for a durable owner.  This operation
    /// is idempotent: a lost response can be retried without re-advancing the
    /// source.  Call [`Self::retire_ack`] only after the durable owner commits
    /// its receipt.
    pub fn begin_ack(&self) -> bool {
        let mut recovery = self.state.lock().expect("failure recovery lock poisoned");
        if recovery.retired || recovery.contiguous_read != self.encoded_len {
            return false;
        }
        if recovery.source_acknowledged {
            return true;
        }
        if !self.ingress.ack_transfer(&self.transfer) {
            return false;
        }
        recovery.source_acknowledged = true;
        true
    }

    /// Report whether a durable owner may prepare its private source-ACK
    /// receipt without advancing the native FIFO head.  The corresponding
    /// [`Self::begin_ack`] remains the only operation that advances that
    /// head; desktop uses this predicate to persist its intent first.
    pub fn ready_for_begin_ack(&self) -> bool {
        let recovery = self.state.lock().expect("failure recovery lock poisoned");
        !recovery.retired && recovery.contiguous_read == self.encoded_len
    }

    /// Release this recovery only after [`Self::begin_ack`] has succeeded and
    /// the durable owner has committed the corresponding source-ack receipt.
    /// Repeating a completed retirement is refused so callers cannot mistake a
    /// stale capability for a new successful transfer.
    pub fn try_retire_ack(&self) -> std::io::Result<bool> {
        let mut recovery = self.state.lock().expect("failure recovery lock poisoned");
        if !recovery.source_acknowledged || recovery.retired {
            return Ok(false);
        }
        self.transfer.record.complete_failure_recovery_finalizer()?;
        recovery.retired = true;
        Ok(true)
    }

    /// Legacy convenience result. Call [`Self::try_retire_ack`] at a durable
    /// boundary so an exact-evidence retirement failure is retained and
    /// reported for retry rather than reduced to this boolean outcome.
    pub fn retire_ack(&self) -> bool {
        matches!(self.try_retire_ack(), Ok(true))
    }

    /// ACK only after every serialized byte has been read contiguously. The
    /// source cannot verify downstream ownership: the host must call this only
    /// after *all* source-time recipients have accepted this exact occurrence
    /// or durable owners have committed it. Reads alone are not a transfer.
    pub fn try_acknowledge(&self) -> std::io::Result<bool> {
        if !self.begin_ack() {
            return Ok(false);
        }
        self.try_retire_ack()
    }

    /// Legacy convenience result. Durable collectors must use
    /// [`Self::try_acknowledge`] to retain an explicit filesystem failure.
    pub fn acknowledge(&self) -> bool {
        matches!(self.try_acknowledge(), Ok(true))
    }
}

// A held record already owns its FIFO position and capacity, but is not
// observable until the source replays the callback that owns this token.
struct QueuedRecord {
    record: Arc<IngressRecord>,
    hold: Option<Arc<std::sync::atomic::AtomicU8>>,
}

const RESERVATION_HELD: u8 = 0;
const RESERVATION_RELEASED: u8 = 1;
const RESERVATION_ABANDONED: u8 = 2;

impl AsRef<IngressRecord> for QueuedRecord {
    fn as_ref(&self) -> &IngressRecord {
        self.record.as_ref()
    }
}

impl QueuedRecord {
    fn ready(&self) -> bool {
        self.hold.as_ref().is_none_or(|hold| {
            hold.load(std::sync::atomic::Ordering::Acquire) == RESERVATION_RELEASED
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReservationError {
    Ingress(IngressError),
}

/// Retains a callback's admitted FIFO records until its owner replays it.
/// A dropped token never discards or prematurely exposes accepted records.
#[must_use = "the callback owner must release its reservation on replay"]
pub(crate) struct IngressReservation {
    ingress: PeerNotificationIngress,
    released: Arc<std::sync::atomic::AtomicU8>,
}

/// A future failure hold allocated while a group joins an in-flight owner.
/// It does not change ingress state until the operation has actually failed.
pub(crate) struct FutureFailureRecipient {
    ingress: PeerNotificationIngress,
    released: Arc<std::sync::atomic::AtomicU8>,
}

impl PeerNotificationIngress {
    #[cfg(test)]
    pub(crate) fn test_future_failure_installed(&self) -> bool {
        let state = self.0.state.lock().expect("peer ingress lock poisoned");
        state.future_failure.is_some() || state.future_occurrences
    }

    pub(crate) fn future_failure_recipient(&self) -> Option<FutureFailureRecipient> {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        // An existing terminal may already have been offered to the consumer.
        // New work cannot retroactively become an owner of that group.
        if state.terminal.is_some() {
            return None;
        }
        state.future_failure_owners += 1;
        drop(state);
        Some(FutureFailureRecipient {
            ingress: self.clone(),
            released: Arc::new(std::sync::atomic::AtomicU8::new(RESERVATION_HELD)),
        })
    }
}

impl FutureFailureRecipient {
    /// Freeze the callback-time cut without cloning an arbitrary native error
    /// into every recipient. The shared error is materialized only on delivery.
    pub(crate) fn install_shared(
        self,
        generation: u64,
        error: SharedPlatformError,
    ) -> Option<IngressReservation> {
        let mut state = self
            .ingress
            .0
            .state
            .lock()
            .expect("peer ingress lock poisoned");
        assert_eq!(
            state.generation, generation,
            "future failure generation mismatch"
        );
        let existing_failure = state.future_failure.is_some()
            || state
                .terminal
                .as_ref()
                .is_some_and(|(reason, _)| !matches!(reason, TerminalReason::Closed { .. }));
        if state.future_failure_owners > 1 || state.future_occurrences || existing_failure {
            state.future_occurrences = true;
            if state.occurrence_terminal_failure.is_none() {
                state.occurrence_terminal_failure = Some(error.clone());
            }
            let memberships = state
                .terminal
                .as_ref()
                .map_or_else(|| state.routes.clone(), |(_, members)| members.clone());
            state.queue.push_back(QueuedRecord {
                record: Arc::new(IngressRecord::FailureOccurrence { error, memberships }),
                hold: Some(self.released.clone()),
            });
        } else {
            if state.terminal.is_none() {
                state.stop(TerminalReason::Closed {
                    tail_uncertain: true,
                });
            }
            state.future_failure = Some(error);
            state.terminal_hold = Some(self.released.clone());
        }
        drop(state);
        Some(IngressReservation {
            ingress: self.ingress.clone(),
            released: self.released.clone(),
        })
    }
}

impl Drop for FutureFailureRecipient {
    fn drop(&mut self) {
        let mut state = self
            .ingress
            .0
            .state
            .lock()
            .expect("peer ingress lock poisoned");
        state.future_failure_owners -= 1;
        drop(state);
        self.ingress.wake();
    }
}

/// Heap storage retained by this exact error, including spare capacity.
pub(crate) fn platform_error_heap_bytes(error: &crate::PlatformError) -> usize {
    error.metadata.iter().fold(
        error
            .code
            .capacity()
            .saturating_add(error.message.capacity())
            .saturating_add(
                error
                    .metadata
                    .capacity()
                    .saturating_mul(std::mem::size_of::<(&'static str, String)>()),
            ),
        |total, (_, value)| total.saturating_add(value.capacity()),
    )
}

/// Atomically size and install a peer-wide failure across the actual
/// recipients. The caller holds the source lock, so no other source operation
/// can acquire multiple ingress locks in a different order. Holding every
/// ingress lock also excludes independent abandoned-reservation recovery.
pub(crate) fn reserve_failure_recipients<'a>(
    generation: u64,
    ingresses: impl Iterator<Item = &'a PeerNotificationIngress>,
    error: &crate::PlatformError,
    budget: usize,
) -> Result<Option<(Vec<IngressReservation>, usize)>, IngressError> {
    let mut locked: Vec<_> = ingresses
        .map(|ingress| {
            (
                ingress,
                ingress.0.state.lock().expect("peer ingress lock poisoned"),
            )
        })
        .collect();
    if locked
        .iter()
        .any(|(_, state)| state.generation != generation)
    {
        return Err(IngressError::StaleGeneration);
    }
    let recipients = locked
        .iter()
        .filter(|(_, state)| state.terminal.is_none())
        .count();
    let minimum_error_bytes = error.metadata.iter().fold(
        error.code.len().saturating_add(error.message.len()),
        |total, (_, value)| {
            total
                .saturating_add(value.len())
                .saturating_add(std::mem::size_of::<(&'static str, String)>())
        },
    );
    let token_bytes =
        std::mem::size_of::<IngressReservation>().saturating_add(3 * std::mem::size_of::<usize>());
    if recipients.saturating_mul(token_bytes.saturating_add(minimum_error_bytes)) > budget {
        return Ok(None);
    }
    let mut reservations = Vec::with_capacity(recipients);
    let mut retained_bytes = reservations.capacity().saturating_mul(token_bytes);
    if retained_bytes > budget {
        return Ok(None);
    }
    // Measure the allocations of the actual clones before changing any ingress.
    // The lower-bound check above bounds cloning by the transfer budget.
    let mut errors = Vec::with_capacity(recipients);
    for _ in 0..recipients {
        let copy = error.clone();
        retained_bytes = retained_bytes.saturating_add(platform_error_heap_bytes(&copy));
        if retained_bytes > budget {
            return Ok(None);
        }
        errors.push(copy);
    }
    let mut errors = errors.into_iter();
    for (ingress, state) in &mut locked {
        if state.terminal.is_some() {
            continue;
        }
        let released = Arc::new(std::sync::atomic::AtomicU8::new(RESERVATION_HELD));
        state.stop(TerminalReason::SourceFailure {
            membership: None,
            error: errors.next().expect("one error per recipient").into(),
            rejected_loss: None,
            tail_uncertain: true,
        });
        state.terminal_hold = Some(released.clone());
        reservations.push(IngressReservation {
            ingress: (*ingress).clone(),
            released,
        });
    }
    Ok(Some((reservations, retained_bytes)))
}

impl IngressReservation {
    pub(crate) fn release(self) {
        self.released
            .store(RESERVATION_RELEASED, std::sync::atomic::Ordering::Release);
        self.ingress.wake();
    }

    /// The source owner cannot replay this callback. Keep its accepted
    /// records and report the uncertain tail even after source retirement.
    pub(crate) fn abandon_and_recover(self) {
        let ingress = self.ingress.clone();
        drop(self);
        ingress.recover_abandoned_reservations();
    }
}

impl Drop for IngressReservation {
    fn drop(&mut self) {
        if self
            .released
            .compare_exchange(
                RESERVATION_HELD,
                RESERVATION_ABANDONED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
        {
            log::error!(
                "notification ingress reservation dropped before replay; accepted records remain held"
            );
        }
    }
}

struct State {
    generation: u64,
    routes: Vec<(NotificationRoute, Membership)>,
    queue: VecDeque<QueuedRecord>,
    max_records: usize,
    max_bytes: usize,
    queued_bytes: usize,
    next_ordinal: Option<u64>,
    next_epoch: Option<u64>,
    consumed_barrier: Option<u64>,
    terminal: Option<(TerminalReason, Vec<(NotificationRoute, Membership)>)>,
    future_failure: Option<SharedPlatformError>,
    occurrence_terminal_failure: Option<SharedPlatformError>,
    future_failure_owners: usize,
    future_occurrences: bool,
    terminal_hold: Option<Arc<std::sync::atomic::AtomicU8>>,
    terminal_read: bool,
    transfer_identity: Arc<()>,
    transfer_record: Option<Arc<IngressRecord>>,
}
struct Shared {
    state: Mutex<State>,
    notify: Notify,
    coordinator: Option<Arc<WakeCoordinator>>,
    slot_live: std::sync::atomic::AtomicBool,
}

struct WakeState {
    depth: usize,
    delivery_holds: usize,
    pending: Vec<std::sync::Weak<Shared>>,
    live: usize,
    capacity: usize,
}
pub(crate) struct WakeCoordinator(Mutex<WakeState>);

impl WakeCoordinator {
    pub(crate) fn new(capacity: usize) -> Self {
        Self(Mutex::new(WakeState {
            depth: 0,
            delivery_holds: 0,
            pending: Vec::new(),
            live: 0,
            capacity,
        }))
    }

    pub(crate) fn reserve(&self) -> bool {
        let mut state = self.0.lock().expect("peer wake lock poisoned");
        if state.live >= state.capacity {
            return false;
        }
        state.live += 1;
        true
    }

    pub(crate) fn defer(&self) {
        self.0.lock().expect("peer wake lock poisoned").depth += 1;
    }

    pub(crate) fn hold_delivery(&self) {
        self.0
            .lock()
            .expect("peer wake lock poisoned")
            .delivery_holds += 1;
    }

    pub(crate) fn release_delivery(&self) {
        let mut state = self.0.lock().expect("peer wake lock poisoned");
        state.delivery_holds -= 1;
        let pending = if state.depth == 0 && state.delivery_holds == 0 {
            std::mem::take(&mut state.pending)
        } else {
            Vec::new()
        };
        drop(state);
        for weak in pending {
            if let Some(ingress) = weak.upgrade() {
                ingress.notify.notify_one();
            }
        }
    }

    pub(crate) fn release(&self) {
        let mut state = self.0.lock().expect("peer wake lock poisoned");
        state.depth -= 1;
        let pending = if state.depth == 0 && state.delivery_holds == 0 {
            std::mem::take(&mut state.pending)
        } else {
            Vec::new()
        };
        drop(state);
        for weak in pending {
            if let Some(ingress) = weak.upgrade() {
                ingress.notify.notify_one();
            }
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.release_slot();
    }
}

impl Shared {
    fn release_slot(&self) {
        use std::sync::atomic::Ordering;
        if !self.slot_live.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some(coordinator) = &self.coordinator {
            let mut state = coordinator.0.lock().expect("peer wake lock poisoned");
            state.live -= 1;
            state
                .pending
                .retain(|entry| !std::ptr::eq(entry.as_ptr(), self));
        }
    }
}

/// One logical peer owner. Clones share state; `recv` supports one consumer.
#[derive(Clone)]
pub struct PeerNotificationIngress(Arc<Shared>);

impl PeerNotificationIngress {
    /// Enqueue while holding the source admission lock. The owner must call
    /// `wake` after releasing that lock, including when admission fails.
    pub(crate) fn admit_marker_at_ordinal(
        &self,
        generation: u64,
        id: u64,
        ordinal: u64,
    ) -> Result<(), IngressError> {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        state.check(generation)?;
        state.enqueue(
            IngressRecord::Marker { id, ordinal },
            0,
            RejectedRecord::Marker { id },
        )
    }

    pub(crate) fn marker_admission_terminated(
        &self,
        generation: u64,
    ) -> Result<bool, IngressError> {
        let state = self.0.state.lock().expect("peer ingress lock poisoned");
        if state.generation != generation {
            return Err(IngressError::StaleGeneration);
        }
        Ok(state.terminal.is_some())
    }

    pub(crate) fn wake(&self) {
        if !self.0.slot_live.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        if let Some(coordinator) = &self.0.coordinator {
            let mut wake = coordinator.0.lock().expect("peer wake lock poisoned");
            if !self.0.slot_live.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            if wake.depth != 0 || wake.delivery_holds != 0 {
                wake.pending.retain(|entry| entry.strong_count() != 0);
                if !wake
                    .pending
                    .iter()
                    .any(|entry| std::ptr::eq(entry.as_ptr(), Arc::as_ptr(&self.0)))
                {
                    wake.pending.push(Arc::downgrade(&self.0));
                }
                return;
            }
        }
        self.0.notify.notify_one();
    }

    pub fn new(generation: u64, max_records: usize, max_bytes: usize, first_ordinal: u64) -> Self {
        Self::new_inner(generation, max_records, max_bytes, first_ordinal, None)
    }

    pub(crate) fn new_source_owned(
        generation: u64,
        max_records: usize,
        max_bytes: usize,
        coordinator: Arc<WakeCoordinator>,
    ) -> Self {
        Self::new_inner(generation, max_records, max_bytes, 0, Some(coordinator))
    }

    fn new_inner(
        generation: u64,
        max_records: usize,
        max_bytes: usize,
        first_ordinal: u64,
        coordinator: Option<Arc<WakeCoordinator>>,
    ) -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(State {
                generation,
                routes: Vec::new(),
                queue: VecDeque::new(),
                max_records,
                max_bytes,
                queued_bytes: 0,
                next_ordinal: Some(first_ordinal),
                next_epoch: Some(0),
                consumed_barrier: None,
                terminal: None,
                future_failure: None,
                occurrence_terminal_failure: None,
                future_failure_owners: 0,
                future_occurrences: false,
                terminal_hold: None,
                terminal_read: false,
                transfer_identity: Arc::new(()),
                transfer_record: None,
            }),
            notify: Notify::new(),
            coordinator,
            slot_live: std::sync::atomic::AtomicBool::new(true),
        }))
    }

    /// Call only at the serialized native source owner, before enabling delivery.
    pub fn register(
        &self,
        generation: u64,
        route: NotificationRoute,
        member: u64,
    ) -> Result<Membership, IngressError> {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        state.check(generation)?;
        if state.routes.iter().any(|(_, m)| m.member == member) {
            return Err(IngressError::RouteOccupied);
        }
        let epoch = state.next_epoch.ok_or(IngressError::EpochExhausted)?;
        state.next_epoch = epoch.checked_add(1);
        let membership = Membership { member, epoch };
        state.consumed_barrier = None;
        state.routes.push((route, membership));
        Ok(membership)
    }

    /// Fence the exact membership at the native source owner before re-registering.
    pub fn retire(
        &self,
        generation: u64,
        route: NotificationRoute,
        membership: Membership,
    ) -> Result<(), IngressError> {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        state.check(generation)?;
        let Some(index) = state.routes.iter().position(|r| *r == (route, membership)) else {
            return Err(IngressError::NotCurrent);
        };
        let result = state.enqueue(
            IngressRecord::Fence { membership },
            0,
            RejectedRecord::Fence { membership },
        );
        if result.is_ok() {
            state.routes.remove(index);
        }
        drop(state);
        self.wake();
        result
    }

    /// Reserve the fence at admission, while holding its visibility until the
    /// source executes the corresponding retirement in its ordered queue.
    pub(crate) fn reserve_retirement(
        &self,
        generation: u64,
        route: NotificationRoute,
        membership: Membership,
    ) -> Result<Option<IngressReservation>, IngressError> {
        let released = Arc::new(std::sync::atomic::AtomicU8::new(RESERVATION_HELD));
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        if state.generation != generation {
            return Err(IngressError::StaleGeneration);
        }
        // An earlier terminal already owns the cut and its membership snapshot.
        if state.terminal.is_some() {
            return Ok(None);
        }
        let index = state
            .routes
            .iter()
            .position(|entry| *entry == (route, membership))
            .ok_or(IngressError::NotCurrent)?;
        if state.queue.len() >= state.max_records {
            state.stop(TerminalReason::Overflow {
                rejected: RejectedRecord::Fence { membership },
                tail_uncertain: true,
            });
            state.terminal_hold = Some(released.clone());
        } else {
            state.routes.remove(index);
            state.consumed_barrier = None;
            state.queue.push_back(QueuedRecord {
                record: Arc::new(IngressRecord::Fence { membership }),
                hold: Some(released.clone()),
            });
        }
        Ok(Some(IngressReservation {
            ingress: self.clone(),
            released,
        }))
    }

    /// Enqueue a non-retiring FIFO boundary at the serialized source.
    pub fn barrier(&self, generation: u64, id: u64) -> Result<(), IngressError> {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        state.check(generation)?;
        let result = state.enqueue(
            IngressRecord::Barrier { id },
            0,
            RejectedRecord::Barrier { id },
        );
        drop(state);
        self.wake();
        result
    }

    /// Call synchronously at the serialized source before buffering the callback.
    /// A token captured earlier without admission cannot establish fence ordering.
    /// Peer-wide lag and failures need no membership and are never attributed.
    pub fn admit(
        &self,
        generation: u64,
        membership: Option<Membership>,
        event: NotificationEvent,
    ) -> Result<(), IngressError> {
        self.admit_with_provenance(generation, membership, event, CallbackProvenance::Unknown)
    }

    /// Admit at the same source boundary with the callback origin established
    /// by that platform; use `Unknown` if it cannot establish one.
    pub fn admit_with_provenance(
        &self,
        generation: u64,
        membership: Option<Membership>,
        event: NotificationEvent,
        provenance: CallbackProvenance,
    ) -> Result<(), IngressError> {
        let memberships = membership.into_iter().collect::<Vec<_>>();
        self.admit_members_with_provenance(generation, &memberships, event, provenance)
    }

    /// Admit one source callback for all matching members atomically. Every
    /// resulting delivery shares its source ordinal, and a refused fanout is
    /// one rejected callback rather than several source losses.
    pub fn admit_members_with_provenance(
        &self,
        generation: u64,
        memberships: &[Membership],
        event: NotificationEvent,
        provenance: CallbackProvenance,
    ) -> Result<(), IngressError> {
        self.admit_members_inner(generation, memberships, event, provenance, None)
    }

    /// Admit a callback ordinal assigned once by the shared source owner.
    pub fn admit_members_at_ordinal(
        &self,
        generation: u64,
        memberships: &[Membership],
        event: NotificationEvent,
        provenance: CallbackProvenance,
        ordinal: u64,
    ) -> Result<(), IngressError> {
        self.admit_members_inner(generation, memberships, event, provenance, Some(ordinal))
    }

    /// Capture the callback and its exact memberships at the serialized source.
    /// Both loss and terminal evidence remain invisible until replay.
    pub(crate) fn reserve_members_at_ordinal(
        &self,
        generation: u64,
        memberships: &[Membership],
        value: &ValueNotification,
        provenance: CallbackProvenance,
        ordinal: u64,
    ) -> Result<IngressReservation, ReservationError> {
        let released = Arc::new(std::sync::atomic::AtomicU8::new(RESERVATION_HELD));
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        state.check(generation).map_err(ReservationError::Ingress)?;
        let current: Vec<_> = memberships
            .iter()
            .copied()
            .filter(|membership| {
                state
                    .routes
                    .iter()
                    .any(|(route, owner)| owner == membership && route.matches(value))
            })
            .collect();
        let failure_member = if current.len() == 1 {
            current.first().copied()
        } else {
            None
        };
        let mut payload = value.clone();
        if payload.lost_before != 0 {
            let skipped_records = std::mem::take(&mut payload.lost_before);
            if state.queue.len() >= state.max_records {
                let reason = if let Some(error) = payload.source_failure.take() {
                    TerminalReason::SourceFailure {
                        membership: failure_member,
                        error: error.into(),
                        rejected_loss: Some(skipped_records),
                        tail_uncertain: true,
                    }
                } else {
                    TerminalReason::Overflow {
                        rejected: RejectedRecord::Loss { skipped_records },
                        tail_uncertain: true,
                    }
                };
                state.stop(reason);
                state.terminal_hold = Some(released.clone());
                drop(state);
                return Ok(IngressReservation {
                    ingress: self.clone(),
                    released,
                });
            }
            let routes = state.routes.clone();
            state.consumed_barrier = None;
            state.queue.push_back(QueuedRecord {
                record: Arc::new(IngressRecord::Loss {
                    skipped_records,
                    memberships: routes,
                }),
                hold: Some(released.clone()),
            });
        }
        if let Some(error) = payload.source_failure.take() {
            state.stop(TerminalReason::SourceFailure {
                membership: failure_member,
                error: error.into(),
                rejected_loss: None,
                tail_uncertain: true,
            });
            state.terminal_hold = Some(released.clone());
        } else {
            let bytes = payload.value.len();
            let fits = current.len() <= state.max_records.saturating_sub(state.queue.len())
                && bytes.checked_mul(current.len()).is_some_and(|total| {
                    total <= state.max_bytes.saturating_sub(state.queued_bytes)
                });
            if !fits {
                state.stop(TerminalReason::Overflow {
                    rejected: RejectedRecord::Value { bytes },
                    tail_uncertain: true,
                });
                // The terminal belongs to this callback even without a loss record.
                state.terminal_hold = Some(released.clone());
                drop(state);
                return Ok(IngressReservation {
                    ingress: self.clone(),
                    released,
                });
            }
            if !current.is_empty() {
                state.consumed_barrier = None;
            }
            for membership in current {
                state.queued_bytes += bytes;
                state.queue.push_back(QueuedRecord {
                    record: Arc::new(IngressRecord::Value {
                        ordinal,
                        membership,
                        provenance,
                        value: payload.clone(),
                    }),
                    hold: Some(released.clone()),
                });
            }
        }
        drop(state);
        Ok(IngressReservation {
            ingress: self.clone(),
            released,
        })
    }

    /// Recover dropped reservations after their source owner has established
    /// that replay cannot resume. Preserve the admitted prefix and fail the
    /// remaining tail; an earlier terminal retains its original reason.
    pub(crate) fn recover_abandoned_reservations(&self) -> usize {
        use std::sync::atomic::Ordering;
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        let mut recovered = 0;
        for entry in &state.queue {
            if let Some(hold) = &entry.hold {
                if hold
                    .compare_exchange(
                        RESERVATION_ABANDONED,
                        RESERVATION_RELEASED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    recovered += 1;
                }
            }
        }
        if let Some(hold) = &state.terminal_hold {
            if hold
                .compare_exchange(
                    RESERVATION_ABANDONED,
                    RESERVATION_RELEASED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                recovered += 1;
            }
        }
        if recovered != 0 {
            if let Some((reason, _)) = &mut state.terminal {
                match reason {
                    TerminalReason::Closed { tail_uncertain }
                    | TerminalReason::Overflow { tail_uncertain, .. }
                    | TerminalReason::OrdinalExhausted { tail_uncertain, .. }
                    | TerminalReason::MarkerOrdinalExhausted { tail_uncertain, .. }
                    | TerminalReason::SourceFailure { tail_uncertain, .. } => {
                        *tail_uncertain = true
                    }
                }
            } else {
                state.stop(TerminalReason::Closed {
                    tail_uncertain: true,
                });
            }
        }
        drop(state);
        if recovered != 0 {
            self.wake();
        }
        recovered
    }

    fn admit_members_inner(
        &self,
        generation: u64,
        memberships: &[Membership],
        event: NotificationEvent,
        provenance: CallbackProvenance,
        source_ordinal: Option<u64>,
    ) -> Result<(), IngressError> {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        state.check(generation)?;
        let result = match event {
            NotificationEvent::Lagged { skipped_records } => {
                let memberships = state.routes.clone();
                state.enqueue(
                    IngressRecord::Loss {
                        skipped_records,
                        memberships,
                    },
                    0,
                    RejectedRecord::Loss { skipped_records },
                )
            }
            NotificationEvent::Value(mut value) => {
                let current: Vec<Membership> = memberships
                    .iter()
                    .copied()
                    .filter(|m| {
                        state
                            .routes
                            .iter()
                            .any(|(route, owner)| *owner == *m && route.matches(&value))
                    })
                    .collect();
                let failure_member = if current.len() == 1 {
                    current.first().copied()
                } else {
                    None
                };
                if value.lost_before != 0 {
                    let skipped_records = value.lost_before;
                    value.lost_before = 0;
                    let memberships = state.routes.clone();
                    if let Err(error) = state.enqueue(
                        IngressRecord::Loss {
                            skipped_records,
                            memberships,
                        },
                        0,
                        RejectedRecord::Loss { skipped_records },
                    ) {
                        if let Some(failure) = value.source_failure.take() {
                            state.stop(TerminalReason::SourceFailure {
                                membership: failure_member,
                                error: failure.into(),
                                rejected_loss: Some(skipped_records),
                                tail_uncertain: true,
                            });
                        }
                        drop(state);
                        self.wake();
                        return Err(error);
                    }
                }
                if let Some(error) = value.source_failure.take() {
                    // An unmatched native failure still indicates an uncertain peer stream.
                    state.stop(TerminalReason::SourceFailure {
                        membership: failure_member,
                        error: error.into(),
                        rejected_loss: None,
                        tail_uncertain: true,
                    });
                    Err(IngressError::Terminated)
                } else if !current.is_empty() {
                    let bytes = value.value.len();
                    if let Some(ordinal) = source_ordinal.or(state.next_ordinal) {
                        let total_bytes = bytes.checked_mul(current.len());
                        let fits = current.len()
                            <= state.max_records.saturating_sub(state.queue.len())
                            && total_bytes.is_some_and(|total| {
                                total <= state.max_bytes.saturating_sub(state.queued_bytes)
                            });
                        if !fits {
                            state.stop(TerminalReason::Overflow {
                                rejected: RejectedRecord::Value { bytes },
                                tail_uncertain: true,
                            });
                            Err(IngressError::Terminated)
                        } else {
                            state.consumed_barrier = None;
                            for membership in current {
                                state.queued_bytes += bytes;
                                state.queue.push_back(QueuedRecord {
                                    record: Arc::new(IngressRecord::Value {
                                        ordinal,
                                        membership,
                                        provenance,
                                        value: value.clone(),
                                    }),
                                    hold: None,
                                });
                            }
                            if source_ordinal.is_none() {
                                state.next_ordinal = ordinal.checked_add(1);
                            }
                            Ok(())
                        }
                    } else {
                        state.stop(TerminalReason::OrdinalExhausted {
                            rejected_bytes: bytes,
                            tail_uncertain: true,
                        });
                        Err(IngressError::Terminated)
                    }
                } else {
                    Ok(())
                }
            }
        };
        drop(state);
        self.wake();
        result
    }

    /// Retain ordinal exhaustion at the callback's deferred FIFO position.
    pub(crate) fn reserve_source_ordinal_exhausted(
        &self,
        generation: u64,
        rejected_bytes: usize,
    ) -> Option<IngressReservation> {
        let released = Arc::new(std::sync::atomic::AtomicU8::new(RESERVATION_HELD));
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        if state.check(generation).is_err() {
            return None;
        }
        state.stop(TerminalReason::OrdinalExhausted {
            rejected_bytes,
            tail_uncertain: true,
        });
        state.terminal_hold = Some(released.clone());
        drop(state);
        Some(IngressReservation {
            ingress: self.clone(),
            released,
        })
    }

    /// End a group because its source exhausted its connection-wide ordinal.
    pub fn exhaust_source_ordinal(&self, generation: u64, rejected_bytes: usize) {
        self.exhaust_source_ordinal_deferred(generation, rejected_bytes);
        self.wake();
    }

    pub(crate) fn exhaust_source_ordinal_deferred(&self, generation: u64, rejected_bytes: usize) {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        if state.check(generation).is_ok() {
            state.stop(TerminalReason::OrdinalExhausted {
                rejected_bytes,
                tail_uncertain: true,
            });
        }
    }

    pub(crate) fn exhaust_marker_ordinal_deferred(&self, generation: u64, id: u64) {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        if state.check(generation).is_ok() {
            state.stop(TerminalReason::MarkerOrdinalExhausted {
                id,
                tail_uncertain: true,
            });
        }
    }

    /// Terminate the peer stream on a source-wide failure, without inventing a route or loss count.
    pub fn fail_source(
        &self,
        generation: u64,
        error: crate::PlatformError,
    ) -> Result<(), IngressError> {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        state.check(generation)?;
        state.stop(TerminalReason::SourceFailure {
            membership: None,
            error: error.into(),
            rejected_loss: None,
            tail_uncertain: true,
        });
        drop(state);
        self.wake();
        Ok(())
    }

    pub fn close(&self, generation: u64, tail_uncertain: bool) -> Result<(), IngressError> {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        state.check(generation)?;
        state.stop(TerminalReason::Closed { tail_uncertain });
        drop(state);
        self.wake();
        Ok(())
    }

    /// Reserve a generation cut without exposing its terminal before replay.
    /// An earlier terminal keeps both its reason and its existing hold.
    pub(crate) fn reserve_close(
        &self,
        generation: u64,
    ) -> Result<Option<IngressReservation>, IngressError> {
        self.reserve_terminal(
            generation,
            TerminalReason::Closed {
                tail_uncertain: true,
            },
        )
    }

    pub(crate) fn reserve_failure(
        &self,
        generation: u64,
        error: crate::PlatformError,
    ) -> Result<Option<IngressReservation>, IngressError> {
        self.reserve_terminal(
            generation,
            TerminalReason::SourceFailure {
                membership: None,
                error: error.into(),
                rejected_loss: None,
                tail_uncertain: true,
            },
        )
    }

    fn reserve_terminal(
        &self,
        generation: u64,
        reason: TerminalReason,
    ) -> Result<Option<IngressReservation>, IngressError> {
        let released = Arc::new(std::sync::atomic::AtomicU8::new(RESERVATION_HELD));
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        if state.generation != generation {
            return Err(IngressError::StaleGeneration);
        }
        if state.terminal.is_some() {
            return Ok(None);
        }
        state.stop(reason);
        state.terminal_hold = Some(released.clone());
        drop(state);
        Ok(Some(IngressReservation {
            ingress: self.clone(),
            released,
        }))
    }

    pub(crate) fn has_generation(&self, generation: u64) -> bool {
        self.0
            .state
            .lock()
            .expect("peer ingress lock poisoned")
            .generation
            == generation
    }

    /// Close cleanly after the caller has confirmed downstream acknowledgement
    /// of this exact barrier and fenced further source admission. The local
    /// queue and live routes must still be empty under the ingress lock, and
    /// every pre-admitted failure owner must have settled.
    pub fn close_after_barrier(
        &self,
        generation: u64,
        barrier_id: u64,
    ) -> Result<(), IngressError> {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        state.check(generation)?;
        if !state.routes.is_empty()
            || !state.queue.is_empty()
            || state.consumed_barrier != Some(barrier_id)
            || state.future_failure_owners != 0
        {
            return Err(IngressError::NotCurrent);
        }
        state.stop(TerminalReason::Closed {
            tail_uncertain: false,
        });
        drop(state);
        self.wake();
        Ok(())
    }

    /// Inspect without releasing FIFO bytes, the record slot, or terminal slot.
    /// The single consumer must acknowledge the exact transfer only after the
    /// next owner has accepted or retained it.
    pub fn peek_transfer(&self) -> Option<IngressTransfer> {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        if state.transfer_record.is_none() {
            let record = if let Some(record) = state.queue.front() {
                if !record.ready() {
                    return None;
                }
                record.record.clone()
            } else if !state.terminal_read {
                if !state.terminal_ready() {
                    return None;
                }
                let (reason, memberships) = state.terminal.as_ref()?;
                Arc::new(IngressRecord::Terminal {
                    reason: state.terminal_reason(reason),
                    memberships: memberships.clone(),
                })
            } else {
                return None;
            };
            state.transfer_record = Some(record);
        }
        Some(IngressTransfer {
            record: state
                .transfer_record
                .as_ref()
                .expect("transfer record")
                .clone(),
            identity: state.transfer_identity.clone(),
        })
    }

    /// Prepare an exact current failure head for bounded internal reads.
    /// Foreign, stale, or non-failure transfers are refused without consuming
    /// the source. The descriptor and reads retain the same source allocation.
    pub fn prepare_failure_recovery(&self, transfer: &IngressTransfer) -> Option<FailureRecovery> {
        let state = self.0.state.lock().expect("peer ingress lock poisoned");
        if !Arc::ptr_eq(&state.transfer_identity, &transfer.identity)
            || !state
                .transfer_record
                .as_ref()
                .is_some_and(|record| Arc::ptr_eq(record, &transfer.record))
        {
            return None;
        }
        let generation = state.generation;
        let encoded_len = encode_failure_recovery(&transfer.record, generation, 0, 0)?.cursor;
        Some(FailureRecovery {
            ingress: self.clone(),
            transfer: transfer.clone(),
            generation,
            encoded_len,
            state: Mutex::new(FailureRecoveryState {
                contiguous_read: 0,
                source_acknowledged: false,
                retired: false,
            }),
        })
    }

    /// Returns false for stale, repeated, or foreign acknowledgements.
    pub fn ack_transfer(&self, transfer: &IngressTransfer) -> bool {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        if !Arc::ptr_eq(&state.transfer_identity, &transfer.identity)
            || !state
                .transfer_record
                .as_ref()
                .is_some_and(|record| Arc::ptr_eq(record, &transfer.record))
        {
            return false;
        }
        if state.take_front().is_some() {
            return true;
        }
        if !state.terminal_read && state.terminal.is_some() {
            state.terminal_read = true;
            state.transfer_identity = Arc::new(());
            state.transfer_record = None;
            self.0.release_slot();
            return true;
        }
        false
    }

    /// Wait for a transfer without consuming it. Dropping this future leaves
    /// the source FIFO and its reserved terminal unchanged.
    pub async fn recv_transfer(&self) -> Option<IngressTransfer> {
        loop {
            let notified = self.0.notify.notified();
            if let Some(transfer) = self.peek_transfer() {
                return Some(transfer);
            }
            if self
                .0
                .state
                .lock()
                .expect("peer ingress lock poisoned")
                .terminal_read
            {
                return None;
            }
            notified.await;
        }
    }

    pub fn poll(&self) -> Poll {
        let mut state = self.0.state.lock().expect("peer ingress lock poisoned");
        self.consume_locked(&mut state)
    }

    fn consume_locked(&self, state: &mut State) -> Poll {
        if let Some(record) = state.take_front() {
            return Poll::Record(
                Arc::try_unwrap(record).unwrap_or_else(|record| (*record).clone()),
            );
        }
        if !state.queue.is_empty() {
            return Poll::Empty;
        }
        if state.terminal_read {
            return Poll::Finished;
        }
        if !state.terminal_ready() {
            return Poll::Empty;
        }
        if let Some((reason, memberships)) = &state.terminal {
            let reason = state.terminal_reason(reason);
            let memberships = memberships.clone();
            state.terminal_read = true;
            state.transfer_identity = Arc::new(());
            state.transfer_record = None;
            self.0.release_slot();
            return Poll::Record(IngressRecord::Terminal {
                reason,
                memberships,
            });
        }
        Poll::Empty
    }

    /// Wait for the next record; `None` means the reserved terminal was consumed.
    pub async fn recv(&self) -> Option<IngressRecord> {
        loop {
            let notified = self.0.notify.notified();
            match self.poll() {
                Poll::Record(record) => return Some(record),
                Poll::Finished => return None,
                Poll::Empty => notified.await,
            }
        }
    }
}

impl State {
    fn terminal_reason(&self, reason: &TerminalReason) -> TerminalReason {
        let error = self.future_failure.as_ref().or_else(|| match reason {
            TerminalReason::Closed { .. } => self.occurrence_terminal_failure.as_ref(),
            _ => None,
        });
        match error {
            Some(error) => TerminalReason::SourceFailure {
                membership: None,
                error: error.clone(),
                rejected_loss: None,
                tail_uncertain: true,
            },
            None => reason.clone(),
        }
    }
    fn terminal_ready(&self) -> bool {
        self.future_failure_owners == 0
            && self.terminal_hold.as_ref().is_none_or(|hold| {
                hold.load(std::sync::atomic::Ordering::Acquire) == RESERVATION_RELEASED
            })
    }
    fn take_front(&mut self) -> Option<Arc<IngressRecord>> {
        if !self.queue.front()?.ready() {
            return None;
        }
        let record = self.queue.pop_front()?.record;
        self.transfer_identity = Arc::new(());
        self.transfer_record = None;
        match record.as_ref() {
            IngressRecord::Value { value, .. } => {
                self.queued_bytes -= value.value.len();
                self.consumed_barrier = None;
            }
            IngressRecord::Barrier { id } => self.consumed_barrier = Some(*id),
            _ => self.consumed_barrier = None,
        }
        Some(record)
    }
    fn check(&self, generation: u64) -> Result<(), IngressError> {
        if self.generation != generation {
            return Err(IngressError::StaleGeneration);
        }
        if self.terminal.is_some() {
            return Err(IngressError::Terminated);
        }
        Ok(())
    }
    fn stop(&mut self, reason: TerminalReason) {
        self.future_failure = None;
        let memberships = self.terminal.take().map_or_else(
            || std::mem::take(&mut self.routes),
            |(_, memberships)| memberships,
        );
        self.terminal = Some((reason, memberships));
    }
    fn enqueue(
        &mut self,
        record: IngressRecord,
        bytes: usize,
        rejected: RejectedRecord,
    ) -> Result<(), IngressError> {
        if self.queue.len() >= self.max_records
            || bytes > self.max_bytes.saturating_sub(self.queued_bytes)
        {
            self.stop(TerminalReason::Overflow {
                rejected,
                tail_uncertain: true,
            });
            return Err(IngressError::Terminated);
        }
        self.consumed_barrier = None;
        self.queued_bytes += bytes;
        self.queue.push_back(QueuedRecord {
            record: Arc::new(record),
            hold: None,
        });
        Ok(())
    }
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    #[test]
    fn configured_file_evidence_store_replays_exact_bytes_and_removes_them_on_last_owner() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-failure-evidence-{}-{}",
            std::process::id(),
            std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let store = FileFailureEvidenceStore::open(&directory, 64).expect("configured store");
        let evidence = store
            .store_exact(b"exact native evidence")
            .expect("stored evidence");
        assert_eq!(evidence.len(), 21);
        assert_eq!(evidence.read(6, 6).expect("page"), b"native");
        assert!(
            directory
                .read_dir()
                .expect("evidence entry")
                .next()
                .is_some()
        );
        evidence.retire().expect("retire exact evidence");
        drop(evidence);
        assert!(
            directory
                .read_dir()
                .expect("removed evidence")
                .next()
                .is_none()
        );
        std::fs::remove_dir(&directory).expect("remove test directory");
    }

    #[test]
    fn configured_file_evidence_store_applies_backpressure_until_an_owner_settles() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-failure-evidence-budget-{}-{}",
            std::process::id(),
            std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let store = FileFailureEvidenceStore::open(&directory, 6).expect("configured store");
        let first = store.store_exact(b"exact!").expect("first evidence");
        match store.store_exact(b"later") {
            Err(blocked) => assert_eq!(blocked.kind(), std::io::ErrorKind::WouldBlock),
            Ok(_) => panic!("full store must backpressure"),
        }
        first.retire().expect("retire first evidence");
        drop(first);
        let second = store
            .store_exact(b"later")
            .expect("settled bytes reopen budget");
        second.retire().expect("retire second evidence");
        drop(second);
        std::fs::remove_dir(&directory).expect("remove test directory");
    }

    #[test]
    fn configured_file_evidence_store_accounts_orphaned_spill_files_on_restart() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-failure-evidence-orphan-{}-{}",
            std::process::id(),
            std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).expect("test directory");
        let orphan = directory.join("failure-0000000000000007.bin");
        std::fs::write(&orphan, b"held").expect("orphan spill");

        let store = FileFailureEvidenceStore::open(&directory, 4).expect("reopened store");
        match store.store_exact(b"x") {
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock),
            Ok(_) => panic!("an orphaned spill file must remain budgeted after restart"),
        }
        std::fs::remove_file(orphan).expect("remove orphan");
        std::fs::remove_dir(&directory).expect("remove test directory");
    }

    #[test]
    fn file_evidence_retirement_failure_preserves_the_receipt_for_retry() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-failure-evidence-retire-{}-{}",
            std::process::id(),
            std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).expect("test directory");
        let path = directory.join("failure-0000000000000001.bin");
        let evidence = FileFailureEvidence {
            path: path.clone(),
            length: 1,
            active_bytes: std::sync::Arc::new(Mutex::new(1)),
            capacity_released: std::sync::Arc::new(Notify::new()),
            retired: Mutex::new(false),
        };

        assert!(
            evidence.retire().is_err(),
            "missing source must not ACK retirement"
        );
        std::fs::write(&path, b"x").expect("restore retained receipt");
        evidence.retire().expect("retry retirement");
        assert!(!path.exists());
        std::fs::remove_dir(&directory).expect("remove test directory");
    }

    #[test]
    fn oversized_failure_recovery_requires_complete_read_and_exact_head() {
        let ingress = PeerNotificationIngress::new(7, 2, 0, 0);
        let other = PeerNotificationIngress::new(7, 2, 0, 0);
        let error = crate::PlatformError::new("winrt", "hresult", "x".repeat(4 * 1024 * 1024 + 1))
            .with("duplicate", "first")
            .with("duplicate", "second");
        ingress.fail_source(7, error).unwrap();
        let transfer = ingress.peek_transfer().unwrap();
        assert!(other.prepare_failure_recovery(&transfer).is_none());
        let recovery = ingress.prepare_failure_recovery(&transfer).unwrap();
        assert!(
            !recovery.acknowledge(),
            "an unread head cannot be acknowledged"
        );
        let length = recovery.encoded_len();
        assert!(length > 4 * 1024 * 1024);
        let first = recovery.read(0, 1024).unwrap();
        assert_eq!(first, recovery.read(0, 1024).unwrap());
        assert!(recovery.read(0, FAILURE_RECOVERY_MAX_CHUNK + 1).is_none());
        let mut offset = first.len();
        while offset < length {
            let chunk = recovery.read(offset, FAILURE_RECOVERY_MAX_CHUNK).unwrap();
            assert!(!chunk.is_empty());
            offset += chunk.len();
        }
        assert!(recovery.acknowledge());
        assert!(!recovery.acknowledge());
        assert!(recovery.read(0, 1).is_none());
    }

    #[test]
    fn begun_failure_ack_keeps_exact_pages_until_durable_retirement() {
        let ingress = PeerNotificationIngress::new(7, 2, 0, 0);
        ingress
            .fail_source(7, crate::PlatformError::new("winrt", "hresult", "retained"))
            .unwrap();
        let transfer = ingress.peek_transfer().unwrap();
        let recovery = ingress.prepare_failure_recovery(&transfer).unwrap();
        let length = recovery.encoded_len();
        let mut offset = 0;
        while offset < length {
            let page = recovery.read(offset, FAILURE_RECOVERY_MAX_CHUNK).unwrap();
            offset += page.len();
        }
        assert!(recovery.begin_ack());
        assert!(recovery.begin_ack(), "lost producer-ack reply is retryable");
        assert_eq!(
            recovery.read(0, FAILURE_RECOVERY_MAX_CHUNK),
            recovery.read(0, FAILURE_RECOVERY_MAX_CHUNK),
            "the durable owner can re-read after source advance"
        );
        assert!(recovery.retire_ack());
        assert!(!recovery.retire_ack());
        assert!(recovery.read(0, 1).is_none());
    }

    #[test]
    fn failure_recovery_preserves_source_cut_and_duplicate_metadata() {
        let ingress = PeerNotificationIngress::new(7, 2, 0, 0);
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 3,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 4,
        };
        let member = ingress.register(7, route, 9).unwrap();
        ingress
            .fail_source(
                7,
                crate::PlatformError::new("winrt", "code", "é")
                    .with("k", "one")
                    .with("k", "two"),
            )
            .unwrap();
        let transfer = ingress.peek_transfer().unwrap();
        let recovery = ingress.prepare_failure_recovery(&transfer).unwrap();
        let mut expected = b"ubm-source-failure/1\0".to_vec();
        fn number(out: &mut Vec<u8>, value: u64) {
            out.extend_from_slice(&value.to_be_bytes());
        }
        fn string(out: &mut Vec<u8>, value: &str) {
            number(out, value.len() as u64);
            out.extend_from_slice(value.as_bytes());
        }
        number(&mut expected, 7);
        expected.extend_from_slice(&[2, 0, 0, 1]);
        number(&mut expected, 1);
        expected.extend_from_slice(uuid::Uuid::nil().as_bytes());
        number(&mut expected, 3);
        expected.extend_from_slice(uuid::Uuid::nil().as_bytes());
        number(&mut expected, 4);
        number(&mut expected, member.member);
        number(&mut expected, member.epoch);
        string(&mut expected, "winrt");
        string(&mut expected, "code");
        string(&mut expected, "é");
        number(&mut expected, 2);
        string(&mut expected, "k");
        string(&mut expected, "one");
        string(&mut expected, "k");
        string(&mut expected, "two");
        assert_eq!(recovery.encoded_len(), expected.len());
        assert_eq!(recovery.read(4, 8).unwrap(), expected[4..12]);
        assert!(
            !recovery.acknowledge(),
            "out-of-order read cannot authorize ACK"
        );
        let mut bytes = Vec::new();
        while bytes.len() < recovery.encoded_len() {
            bytes.extend(recovery.read(bytes.len(), 7).unwrap());
        }
        assert_eq!(bytes, expected);
        assert!(recovery.acknowledge());
    }

    #[test]
    fn acknowledgement_requires_the_exact_offered_record() {
        let ingress = PeerNotificationIngress::new(7, 2, 0, 0);
        ingress.admit_marker_at_ordinal(7, 42, 9).unwrap();
        let offered = ingress.peek_transfer().unwrap();
        let mut substituted = offered.clone();
        substituted.record = Arc::new(IngressRecord::Marker { id: 42, ordinal: 9 });
        assert!(!ingress.ack_transfer(&substituted));
        let replay = ingress
            .peek_transfer()
            .expect("original record remains owned");
        assert!(Arc::ptr_eq(&offered.record, &replay.record));
        assert!(ingress.ack_transfer(&offered));
        assert!(!ingress.ack_transfer(&replay));
    }

    #[test]
    fn transfer_shares_the_retained_value_allocation() {
        let ingress = PeerNotificationIngress::new(7, 2, 16, 0);
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 0,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 0,
        };
        let membership = ingress.register(7, route, 1).unwrap();
        ingress
            .admit(
                7,
                Some(membership),
                NotificationEvent::Value(ValueNotification {
                    uuid: uuid::Uuid::nil(),
                    instance: 0,
                    service_uuid: uuid::Uuid::nil(),
                    service_instance: 0,
                    value: vec![1, 2, 3],
                    source_failure: None,
                    lost_before: 0,
                }),
            )
            .unwrap();
        let address = {
            let state = ingress.0.state.lock().unwrap();
            let IngressRecord::Value { value, .. } = state.queue.front().unwrap().as_ref() else {
                panic!("value expected")
            };
            value.value.as_ptr()
        };
        let transfer = ingress.peek_transfer().unwrap();
        let IngressRecord::Value { value, .. } = transfer.record.as_ref() else {
            panic!("value expected")
        };
        assert_eq!(
            address,
            value.value.as_ptr(),
            "peek must not duplicate the FIFO payload"
        );
    }

    #[test]
    fn marker_uses_fifo_capacity_and_retains_its_ordinal() {
        let ingress = PeerNotificationIngress::new(7, 1, 0, 0);
        ingress.admit_marker_at_ordinal(7, 42, 9).unwrap();
        assert_eq!(
            ingress.admit_marker_at_ordinal(7, 43, 10),
            Err(IngressError::Terminated)
        );
        assert_eq!(
            ingress.poll(),
            Poll::Record(IngressRecord::Marker { id: 42, ordinal: 9 })
        );
        assert!(matches!(
            ingress.poll(),
            Poll::Record(IngressRecord::Terminal {
                reason: TerminalReason::Overflow {
                    rejected: RejectedRecord::Marker { id: 43 },
                    ..
                },
                ..
            })
        ));
    }
}

#[cfg(test)]
mod reservation_tests {
    use super::*;

    fn route() -> NotificationRoute {
        NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 2,
        }
    }

    fn value(bytes: &[u8]) -> ValueNotification {
        let route = route();
        ValueNotification {
            uuid: route.characteristic_uuid,
            instance: route.characteristic_instance,
            service_uuid: route.service_uuid,
            service_instance: route.service_instance,
            value: bytes.to_vec(),
            source_failure: None,
            lost_before: 0,
        }
    }

    fn reserve(
        ingress: &PeerNotificationIngress,
        members: &[Membership],
        bytes: &[u8],
    ) -> IngressReservation {
        ingress
            .reserve_members_at_ordinal(
                1,
                members,
                &value(bytes),
                CallbackProvenance::ReadOrNotification,
                42,
            )
            .unwrap()
    }

    #[test]
    fn held_value_blocks_poll_transfer_fence_and_terminal_until_replay() {
        let ingress = PeerNotificationIngress::new(1, 3, 8, 0);
        let first = ingress.register(1, route(), 1).unwrap();
        let reservation = reserve(&ingress, &[first], &[7]);
        ingress.retire(1, route(), first).unwrap();
        let second = ingress.register(1, route(), 2).unwrap();
        ingress.barrier(1, 9).unwrap();
        ingress.close(1, true).unwrap();
        assert_eq!(ingress.poll(), Poll::Empty);
        assert!(ingress.peek_transfer().is_none());
        reservation.release();
        assert!(
            matches!(ingress.poll(), Poll::Record(IngressRecord::Value { ordinal: 42, membership, provenance: CallbackProvenance::ReadOrNotification, value }) if membership == first && value.value == [7])
        );
        assert_eq!(
            ingress.poll(),
            Poll::Record(IngressRecord::Fence { membership: first })
        );
        assert_eq!(
            ingress.poll(),
            Poll::Record(IngressRecord::Barrier { id: 9 })
        );
        assert!(
            matches!(ingress.poll(), Poll::Record(IngressRecord::Terminal { memberships, .. }) if memberships == vec![(route(), second)])
        );
    }

    #[test]
    fn reservations_keep_fanout_and_later_release_in_source_order() {
        let ingress = PeerNotificationIngress::new(1, 5, 8, 0);
        let a = ingress.register(1, route(), 1).unwrap();
        let b = ingress.register(1, route(), 2).unwrap();
        let first = reserve(&ingress, &[a, b], &[7]);
        ingress.retire(1, route(), a).unwrap();
        let later = reserve(&ingress, &[b], &[8]);
        later.release();
        assert_eq!(ingress.poll(), Poll::Empty);
        first.release();
        for member in [a, b] {
            let transfer = ingress.peek_transfer().unwrap();
            assert!(
                matches!(transfer.record.as_ref(), IngressRecord::Value { ordinal: 42, membership, value, .. } if *membership == member && value.value == [7])
            );
            assert!(ingress.ack_transfer(&transfer));
        }
        assert_eq!(
            ingress.poll(),
            Poll::Record(IngressRecord::Fence { membership: a })
        );
        assert!(
            matches!(ingress.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == b && value.value == [8])
        );
        assert_eq!(ingress.poll(), Poll::Empty);
    }

    #[test]
    fn reserved_bytes_and_records_count_toward_overflow_without_losing_accepted_value() {
        let ingress = PeerNotificationIngress::new(1, 2, 2, 0);
        let member = ingress.register(1, route(), 1).unwrap();
        let held = reserve(&ingress, &[member], &[7, 8]);
        assert_eq!(
            ingress.admit(1, Some(member), NotificationEvent::Value(value(&[9]))),
            Err(IngressError::Terminated)
        );
        assert_eq!(ingress.poll(), Poll::Empty);
        held.release();
        assert!(
            matches!(ingress.poll(), Poll::Record(IngressRecord::Value { value, .. }) if value.value == [7, 8])
        );
        assert!(matches!(
            ingress.poll(),
            Poll::Record(IngressRecord::Terminal {
                reason: TerminalReason::Overflow {
                    rejected: RejectedRecord::Value { bytes: 1 },
                    ..
                },
                ..
            })
        ));
        assert_eq!(ingress.poll(), Poll::Finished);
    }

    #[test]
    fn reserved_fanout_is_atomic_when_capacity_is_insufficient() {
        let ingress = PeerNotificationIngress::new(1, 1, 16, 0);
        let a = ingress.register(1, route(), 1).unwrap();
        let b = ingress.register(1, route(), 2).unwrap();
        let held = ingress
            .reserve_members_at_ordinal(
                1,
                &[a, b],
                &value(&[7]),
                CallbackProvenance::Notification,
                3,
            )
            .expect("overflow terminal must have an owner");
        assert_eq!(ingress.poll(), Poll::Empty);
        held.release();
        assert!(matches!(
            ingress.poll(),
            Poll::Record(IngressRecord::Terminal {
                reason: TerminalReason::Overflow {
                    rejected: RejectedRecord::Value { bytes: 1 },
                    ..
                },
                ..
            })
        ));
        assert_eq!(ingress.poll(), Poll::Finished);
    }

    #[test]
    fn dropping_reservation_does_not_expose_or_discard_accepted_value() {
        let ingress = PeerNotificationIngress::new(1, 2, 8, 0);
        let member = ingress.register(1, route(), 1).unwrap();
        let reservation = reserve(&ingress, &[member], &[7]);
        drop(reservation);
        ingress.close(1, true).unwrap();
        assert_eq!(ingress.poll(), Poll::Empty);
        assert!(ingress.peek_transfer().is_none());
    }

    #[test]
    fn abandoned_reservation_can_be_recovered_without_losing_its_fifo_prefix() {
        let ingress = PeerNotificationIngress::new(1, 4, 8, 0);
        let member = ingress.register(1, route(), 1).unwrap();
        let abandoned = reserve(&ingress, &[member], &[7]);
        ingress.barrier(1, 9).unwrap();
        drop(abandoned);
        assert_eq!(ingress.poll(), Poll::Empty);
        assert_eq!(ingress.recover_abandoned_reservations(), 1);
        assert!(
            matches!(ingress.poll(), Poll::Record(IngressRecord::Value { value, .. }) if value.value == [7])
        );
        assert_eq!(
            ingress.poll(),
            Poll::Record(IngressRecord::Barrier { id: 9 })
        );
        assert!(matches!(
            ingress.poll(),
            Poll::Record(IngressRecord::Terminal {
                reason: TerminalReason::Closed {
                    tail_uncertain: true
                },
                ..
            })
        ));
        assert_eq!(ingress.recover_abandoned_reservations(), 0);
    }

    #[test]
    fn recovery_preserves_live_holds_and_existing_overflow() {
        let ingress = PeerNotificationIngress::new(1, 2, 8, 0);
        let member = ingress.register(1, route(), 1).unwrap();
        let abandoned = reserve(&ingress, &[member], &[7]);
        let live = reserve(&ingress, &[member], &[8]);
        let overflow = ingress
            .reserve_members_at_ordinal(1, &[member], &value(&[9]), CallbackProvenance::Unknown, 3)
            .expect("overflow terminal must have an owner");
        drop(abandoned);
        assert_eq!(ingress.recover_abandoned_reservations(), 1);
        assert!(
            matches!(ingress.poll(), Poll::Record(IngressRecord::Value { value, .. }) if value.value == [7])
        );
        assert_eq!(ingress.poll(), Poll::Empty);
        live.release();
        assert!(
            matches!(ingress.poll(), Poll::Record(IngressRecord::Value { value, .. }) if value.value == [8])
        );
        assert_eq!(ingress.poll(), Poll::Empty);
        overflow.release();
        assert!(matches!(
            ingress.poll(),
            Poll::Record(IngressRecord::Terminal {
                reason: TerminalReason::Overflow { .. },
                ..
            })
        ));
    }

    #[test]
    fn recovery_releases_every_member_of_one_abandoned_callback() {
        let ingress = PeerNotificationIngress::new(1, 3, 8, 0);
        let first = ingress.register(1, route(), 1).unwrap();
        let second = ingress.register(1, route(), 2).unwrap();
        let held = reserve(&ingress, &[first, second], &[7]);
        ingress.close(1, false).unwrap();
        drop(held);
        assert_eq!(ingress.recover_abandoned_reservations(), 1);
        for expected in [first, second] {
            assert!(
                matches!(ingress.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == expected && value.value == [7])
            );
        }
        assert!(matches!(
            ingress.poll(),
            Poll::Record(IngressRecord::Terminal {
                reason: TerminalReason::Closed {
                    tail_uncertain: true
                },
                ..
            })
        ));
    }

    #[test]
    fn replay_wakes_a_waiting_transfer_consumer() {
        use std::future::Future;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Poll as TaskPoll, Wake, Waker};
        struct Counter(AtomicUsize);
        impl Wake for Counter {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let ingress = PeerNotificationIngress::new(1, 2, 8, 0);
        let member = ingress.register(1, route(), 1).unwrap();
        let held = reserve(&ingress, &[member], &[7]);
        let mut receive = Box::pin(ingress.recv_transfer());
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut context = Context::from_waker(&waker);
        assert!(matches!(
            receive.as_mut().poll(&mut context),
            TaskPoll::Pending
        ));
        held.release();
        assert!(counter.0.load(Ordering::SeqCst) > 0);
        assert!(matches!(
            receive.as_mut().poll(&mut context),
            TaskPoll::Ready(Some(_))
        ));
    }

    #[test]
    fn abandoned_failure_terminal_stays_exact_after_retirement() {
        let ingress = PeerNotificationIngress::new(1, 2, 8, 0);
        let member = ingress.register(1, route(), 1).unwrap();
        let mut failed = value(&[]);
        let error =
            crate::PlatformError::new("test", "native", "original").with("detail", "retained");
        failed.source_failure = Some(error.clone());
        let held = ingress
            .reserve_members_at_ordinal(1, &[member], &failed, CallbackProvenance::Unknown, 1)
            .unwrap();
        assert!(ingress.peek_transfer().is_none());
        assert_eq!(ingress.close(1, true), Err(IngressError::Terminated));
        held.abandon_and_recover();
        assert!(
            matches!(ingress.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(m), error: found, tail_uncertain: true, .. }, memberships }) if m == member && found == error && memberships == vec![(route(), member)])
        );
        assert_eq!(ingress.poll(), Poll::Finished);
    }

    #[test]
    fn reservation_holds_loss_and_failure_even_when_loss_overflows() {
        let ingress = PeerNotificationIngress::new(1, 0, 8, 0);
        let member = ingress.register(1, route(), 1).unwrap();
        let mut failed = value(&[7]);
        failed.lost_before = 2;
        let error =
            crate::PlatformError::new("test", "exact", "original").with("detail", "retained");
        failed.source_failure = Some(error.clone());
        let held = ingress
            .reserve_members_at_ordinal(1, &[member], &failed, CallbackProvenance::Unknown, 1)
            .unwrap();
        assert_eq!(ingress.poll(), Poll::Empty);
        held.release();
        assert!(
            matches!(ingress.poll(), Poll::Record(IngressRecord::Terminal { reason: TerminalReason::SourceFailure { membership: Some(m), error: found, rejected_loss: Some(2), .. }, .. }) if m == member && found == error)
        );
    }
}

/// The source-time identity of one admitted native operation. Memberships are
/// captured by the source while registration and callback admission are serialized.
#[derive(Debug, PartialEq, Eq)]
pub struct FailureOccurrenceContext {
    pub generation: u64,
    pub activation: u64,
    pub memberships: Vec<(NotificationRoute, Membership)>,
}

/// A failure occurrence owns the original platform error and source-time cut.
#[derive(Debug)]
pub struct FailureOccurrence {
    pub context: FailureOccurrenceContext,
    pub error: crate::PlatformError,
}

/// Opaque identity for one admitted operation, valid even after its generation
/// has been replaced. Dropping this handle does not cancel an admitted operation.
#[derive(Debug, Clone)]
#[must_use = "settle or explicitly cancel the admitted operation"]
pub struct FailureOccurrenceTicket(Arc<()>);

/// Replayed views share the original occurrence. A downstream owner must retain
/// its own reference before acknowledging the source.
#[derive(Debug, Clone)]
pub struct FailureOccurrenceTransfer {
    pub occurrence: Arc<FailureOccurrence>,
    identity: Arc<()>,
}

#[derive(Debug)]
pub enum FailureOccurrencePoll {
    Occurrence(FailureOccurrenceTransfer),
    Pending,
    /// The source is sealed and every admitted operation has settled and every
    /// failure has been acknowledged. Only now can a terminal be propagated.
    Drained,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureOccurrenceError {
    Sealed,
    Capacity,
    NotPending,
    EarlierPending,
}

enum FailureOccurrenceSlot {
    Pending {
        identity: Arc<()>,
        context: FailureOccurrenceContext,
        bytes: usize,
        error_budget: usize,
        failure_observed: bool,
    },
    Ready {
        identity: Arc<()>,
        occurrence: Arc<FailureOccurrence>,
        bytes: usize,
    },
}

/// Bounded FIFO receipt owner. The source serializes calls with admission and
/// sealing; reservations establish FIFO position before the operation starts.
/// The byte budget charges retained variable-size allocations, fixed slot
/// overhead and error headroom reserved before admission. Errors too large for
/// their reservation are returned intact for
/// the producer to retain and retry, without settling the reservation.
pub struct FailureOccurrences {
    slots: VecDeque<FailureOccurrenceSlot>,
    max_occurrences: usize,
    max_bytes: usize,
    retained_bytes: usize,
    sealed: bool,
}

impl FailureOccurrences {
    pub fn new(max_occurrences: usize, max_bytes: usize) -> Self {
        Self {
            slots: VecDeque::new(),
            max_occurrences,
            max_bytes,
            retained_bytes: 0,
            sealed: false,
        }
    }

    /// Reserve an operation and its error heap-byte headroom before native work.
    /// Refusal returns the unmodified context to the caller.
    pub fn reserve_with_error_budget(
        &mut self,
        context: FailureOccurrenceContext,
        error_budget: usize,
    ) -> Result<FailureOccurrenceTicket, (FailureOccurrenceError, FailureOccurrenceContext)> {
        if self.sealed {
            return Err((FailureOccurrenceError::Sealed, context));
        }
        let bytes = std::mem::size_of::<FailureOccurrenceSlot>()
            .saturating_add(std::mem::size_of::<FailureOccurrence>())
            .saturating_add(4 * std::mem::size_of::<usize>())
            .saturating_add(
                context
                    .memberships
                    .capacity()
                    .saturating_mul(std::mem::size_of::<(NotificationRoute, Membership)>()),
            )
            .saturating_add(error_budget);
        if self.slots.len() >= self.max_occurrences
            || bytes > self.max_bytes.saturating_sub(self.retained_bytes)
        {
            return Err((FailureOccurrenceError::Capacity, context));
        }
        let identity = Arc::new(());
        self.slots.push_back(FailureOccurrenceSlot::Pending {
            identity: identity.clone(),
            context,
            bytes,
            error_budget,
            failure_observed: false,
        });
        self.retained_bytes += bytes;
        Ok(FailureOccurrenceTicket(identity))
    }

    /// Settle a previously admitted operation, including after sealing. On
    /// backpressure or an invalid ticket, return the same error allocation.
    pub fn settle_failure(
        &mut self,
        ticket: &FailureOccurrenceTicket,
        error: crate::PlatformError,
    ) -> Result<(), (FailureOccurrenceError, crate::PlatformError)> {
        let Some(index) = self.slots.iter().position(|slot| {
            matches!(slot,
            FailureOccurrenceSlot::Pending { identity, .. } if Arc::ptr_eq(identity, &ticket.0))
        }) else {
            return Err((FailureOccurrenceError::NotPending, error));
        };
        // Each slot has its own precharged error budget. Settlement can happen
        // in any order without consuming another operation's reservation;
        // polling still waits for and delivers the FIFO head first.
        let bytes = platform_error_heap_bytes(&error);
        let FailureOccurrenceSlot::Pending {
            error_budget,
            failure_observed,
            ..
        } = &mut self.slots[index]
        else {
            unreachable!()
        };
        *failure_observed = true;
        if bytes > *error_budget {
            return Err((FailureOccurrenceError::Capacity, error));
        }
        // Removing and reinserting at the same position moves both allocations
        // without cloning them and keeps the admitted FIFO order.
        let old = self.slots.remove(index).expect("located slot");
        let FailureOccurrenceSlot::Pending {
            identity,
            context,
            bytes: reserved,
            ..
        } = old
        else {
            unreachable!()
        };
        self.slots.insert(
            index,
            FailureOccurrenceSlot::Ready {
                identity,
                occurrence: Arc::new(FailureOccurrence { context, error }),
                bytes: reserved,
            },
        );
        Ok(())
    }

    /// Increase the error reservation while retaining the same admitted ticket.
    /// Failure leaves both the budget and the pending operation unchanged.
    pub fn increase_error_budget(
        &mut self,
        ticket: &FailureOccurrenceTicket,
        additional: usize,
    ) -> Result<(), FailureOccurrenceError> {
        let Some(FailureOccurrenceSlot::Pending { bytes, error_budget, .. }) = self.slots.iter_mut().find(|slot|
            matches!(slot, FailureOccurrenceSlot::Pending { identity, .. } if Arc::ptr_eq(identity, &ticket.0))) else {
            return Err(FailureOccurrenceError::NotPending);
        };
        if additional > self.max_bytes.saturating_sub(self.retained_bytes) {
            return Err(FailureOccurrenceError::Capacity);
        }
        *bytes += additional;
        *error_budget += additional;
        self.retained_bytes += additional;
        Ok(())
    }

    /// Resolve an admitted operation only when its owner knows it cannot report
    /// a failure. A failure already offered for transfer, including one the
    /// producer must retry after a capacity refusal, cannot be cancelled.
    pub fn cancel(&mut self, ticket: &FailureOccurrenceTicket) -> bool {
        let Some(index) = self.slots.iter().position(|slot| matches!(slot,
            FailureOccurrenceSlot::Pending { identity, failure_observed: false, .. } if Arc::ptr_eq(identity, &ticket.0))) else { return false; };
        if let Some(FailureOccurrenceSlot::Pending { bytes, .. }) = self.slots.remove(index) {
            self.retained_bytes -= bytes;
            true
        } else {
            unreachable!()
        }
    }

    /// Stop admitting new operations. Existing tickets may still settle.
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    pub fn poll(&self) -> FailureOccurrencePoll {
        match self.slots.front() {
            Some(FailureOccurrenceSlot::Ready {
                identity,
                occurrence,
                ..
            }) => FailureOccurrencePoll::Occurrence(FailureOccurrenceTransfer {
                identity: identity.clone(),
                occurrence: occurrence.clone(),
            }),
            Some(FailureOccurrenceSlot::Pending { .. }) => FailureOccurrencePoll::Pending,
            None if self.sealed => FailureOccurrencePoll::Drained,
            None => FailureOccurrencePoll::Pending,
        }
    }

    /// Only the exact currently offered FIFO head may release its capacity.
    pub fn ack(&mut self, transfer: &FailureOccurrenceTransfer) -> bool {
        if !matches!(self.slots.front(), Some(FailureOccurrenceSlot::Ready { identity, occurrence, .. })
            if Arc::ptr_eq(identity, &transfer.identity) && Arc::ptr_eq(occurrence, &transfer.occurrence))
        {
            return false;
        }
        if let Some(FailureOccurrenceSlot::Ready { bytes, .. }) = self.slots.pop_front() {
            self.retained_bytes -= bytes;
            true
        } else {
            unreachable!()
        }
    }
}

#[cfg(test)]
mod acknowledged_failure_occurrence_tests {
    use super::*;

    fn context(generation: u64, activation: u64, member: u64) -> FailureOccurrenceContext {
        FailureOccurrenceContext {
            generation,
            activation,
            memberships: vec![(
                NotificationRoute {
                    service_uuid: uuid::Uuid::nil(),
                    service_instance: 4,
                    characteristic_uuid: uuid::Uuid::nil(),
                    characteristic_instance: 5,
                },
                Membership { member, epoch: 9 },
            )],
        }
    }

    #[test]
    fn admitted_failures_replay_fifo_across_seal_and_exact_ack() {
        let mut source = FailureOccurrences::new(2, 4096);
        let first = source
            .reserve_with_error_budget(context(1, 10, 11), 512)
            .unwrap();
        let second = source
            .reserve_with_error_budget(context(1, 20, 22), 512)
            .unwrap();
        let first_error = crate::PlatformError::new("winrt", "one", "first");
        let second_error = crate::PlatformError::new("winrt", "two", "second");
        source.seal();
        assert!(matches!(source.poll(), FailureOccurrencePoll::Pending));
        source.settle_failure(&first, first_error.clone()).unwrap();
        source
            .settle_failure(&second, second_error.clone())
            .unwrap();
        let one = match source.poll() {
            FailureOccurrencePoll::Occurrence(v) => v,
            _ => panic!("first missing"),
        };
        let replay = match source.poll() {
            FailureOccurrencePoll::Occurrence(v) => v,
            _ => panic!("replay missing"),
        };
        assert!(Arc::ptr_eq(&one.occurrence, &replay.occurrence));
        assert_eq!(one.occurrence.context, context(1, 10, 11));
        assert_eq!(one.occurrence.error, first_error);
        assert!(source.ack(&one));
        assert!(!source.ack(&replay));
        let two = match source.poll() {
            FailureOccurrencePoll::Occurrence(v) => v,
            _ => panic!("second missing"),
        };
        assert_eq!(two.occurrence.context, context(1, 20, 22));
        assert_eq!(two.occurrence.error, second_error);
        assert!(source.ack(&two));
        assert!(matches!(source.poll(), FailureOccurrencePoll::Drained));
    }

    #[test]
    fn admission_reserves_error_headroom_and_returns_refused_context() {
        let mut source = FailureOccurrences::new(2, 1024);
        let first = source
            .reserve_with_error_budget(context(1, 1, 1), 700)
            .unwrap();
        let original = context(1, 2, 2);
        let (reason, returned) = source.reserve_with_error_budget(original, 700).unwrap_err();
        assert_eq!(reason, FailureOccurrenceError::Capacity);
        assert_eq!(returned, context(1, 2, 2));
        source
            .settle_failure(&first, crate::PlatformError::new("winrt", "first", "first"))
            .unwrap();
        let head = match source.poll() {
            FailureOccurrencePoll::Occurrence(v) => v,
            _ => panic!("missing"),
        };
        assert!(source.ack(&head));
        assert!(source.reserve_with_error_budget(returned, 700).is_ok());
    }

    #[test]
    fn later_reserved_failure_can_settle_before_earlier_operation() {
        let mut source = FailureOccurrences::new(2, 4096);
        let first = source
            .reserve_with_error_budget(context(1, 1, 1), 128)
            .unwrap();
        let second = source
            .reserve_with_error_budget(context(1, 2, 2), 3200)
            .unwrap();
        let mut later_error = crate::PlatformError::new("winrt", "later", "later");
        later_error.message.reserve(3000);
        let pointer = later_error.message.as_ptr();
        source.settle_failure(&second, later_error).unwrap();
        source.seal();
        assert!(matches!(source.poll(), FailureOccurrencePoll::Pending));
        source
            .settle_failure(&first, crate::PlatformError::new("winrt", "first", "first"))
            .unwrap();
        let head = match source.poll() {
            FailureOccurrencePoll::Occurrence(v) => v,
            _ => panic!("missing"),
        };
        assert_eq!(head.occurrence.error.code, "first");
        assert!(source.ack(&head));
        let second = match source.poll() {
            FailureOccurrencePoll::Occurrence(v) => v,
            _ => panic!("missing second"),
        };
        assert_eq!(second.occurrence.error.message.as_ptr(), pointer);
        assert!(source.ack(&second));
        assert!(matches!(source.poll(), FailureOccurrencePoll::Drained));
    }

    #[test]
    fn bounded_refusal_returns_original_error_and_keeps_reservation() {
        let mut source = FailureOccurrences::new(1, 4096);
        let ticket = source
            .reserve_with_error_budget(context(5, 7, 8), 32)
            .unwrap();
        let mut error = crate::PlatformError::new("winrt", "error", "original");
        error.message.reserve(1024);
        let original_ptr = error.message.as_ptr();
        let (_, returned) = source.settle_failure(&ticket, error).unwrap_err();
        assert_eq!(returned.message.as_ptr(), original_ptr);
        assert!(returned.message.capacity() >= 1024);
        source.seal();
        assert!(matches!(source.poll(), FailureOccurrencePoll::Pending));
        source.increase_error_budget(&ticket, 2048).unwrap();
        source.settle_failure(&ticket, returned).unwrap();
        let transfer = match source.poll() {
            FailureOccurrencePoll::Occurrence(v) => v,
            _ => panic!("missing"),
        };
        assert_eq!(transfer.occurrence.error.message.as_ptr(), original_ptr);
        assert!(source.ack(&transfer));
        assert!(matches!(source.poll(), FailureOccurrencePoll::Drained));
    }

    #[test]
    fn oversized_observed_failure_cannot_be_cancelled_as_success() {
        let mut source = FailureOccurrences::new(1, 4096);
        let ticket = source
            .reserve_with_error_budget(context(5, 7, 8), 16)
            .unwrap();
        let mut error = crate::PlatformError::new("winrt", "error", "original");
        error.message.reserve(1024);
        let original_ptr = error.message.as_ptr();
        let (_, returned) = source.settle_failure(&ticket, error).unwrap_err();
        source.seal();
        assert!(
            !source.cancel(&ticket),
            "an observed failure must remain pending for retry"
        );
        assert!(matches!(source.poll(), FailureOccurrencePoll::Pending));
        source.increase_error_budget(&ticket, 2048).unwrap();
        source.settle_failure(&ticket, returned).unwrap();
        let transfer = match source.poll() {
            FailureOccurrencePoll::Occurrence(v) => v,
            _ => panic!("missing"),
        };
        assert_eq!(transfer.occurrence.error.message.as_ptr(), original_ptr);
        assert!(source.ack(&transfer));
        assert!(matches!(source.poll(), FailureOccurrencePoll::Drained));
    }

    #[test]
    fn successful_operation_can_be_cancelled_after_sealing() {
        let mut source = FailureOccurrences::new(1, 4096);
        let ticket = source
            .reserve_with_error_budget(context(5, 7, 8), 16)
            .unwrap();
        source.seal();
        assert!(source.cancel(&ticket));
        assert!(!source.cancel(&ticket));
        assert!(matches!(source.poll(), FailureOccurrencePoll::Drained));
    }

    #[test]
    fn foreign_and_out_of_order_acknowledgements_do_not_advance() {
        let mut first_source = FailureOccurrences::new(1, 4096);
        let mut second_source = FailureOccurrences::new(1, 4096);
        let a = first_source
            .reserve_with_error_budget(context(3, 4, 5), 512)
            .unwrap();
        let b = second_source
            .reserve_with_error_budget(context(3, 4, 5), 512)
            .unwrap();
        first_source
            .settle_failure(&a, crate::PlatformError::new("winrt", "same", "same"))
            .unwrap();
        second_source
            .settle_failure(&b, crate::PlatformError::new("winrt", "same", "same"))
            .unwrap();
        let transfer = match first_source.poll() {
            FailureOccurrencePoll::Occurrence(v) => v,
            _ => panic!("missing"),
        };
        assert!(!second_source.ack(&transfer));
        assert!(first_source.ack(&transfer));
        assert!(!first_source.ack(&transfer));
    }
}
