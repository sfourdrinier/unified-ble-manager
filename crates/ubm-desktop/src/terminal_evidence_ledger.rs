//! Host-private, crash-safe terminal-evidence ledger.
//!
//! This deliberately stores no BLE policy. A host reserves bounded capacity
//! before it activates a source, commits canonical terminal bytes serially,
//! and retains the resulting opaque reference until authenticated recovery
//! acknowledgement retires it. Runtime teardown may abandon an unused
//! reservation, but never retires staged or committed evidence.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalEvidenceScope {
    pub producer: String,
    pub manager_id: String,
    pub operation_id: String,
    pub incarnation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalEvidenceReservation {
    pub reservation_id: String,
    pub maximum_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedTerminalEvidence {
    pub reference: String,
    pub byte_length: u64,
    pub digest: [u8; 32],
    pub format_version: u8,
}

/// Exact terminal bytes staged durably after source observation but before a
/// retryable commit succeeds. A host may recover these after process restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedTerminalEvidence {
    pub reservation_id: String,
    pub scope: TerminalEvidenceScope,
    pub canonical_bytes: Vec<u8>,
}

/// A native-host recoverable terminal.  The reservation id is deliberately
/// private to the trusted host; foreign bindings must issue an opaque alias
/// before allowing a reader to address it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverableTerminalEvidence {
    pub reservation_id: String,
    pub scope: TerminalEvidenceScope,
    pub state: TerminalEvidenceRecoveryState,
    pub reference: Option<String>,
    pub byte_length: Option<u64>,
    pub digest: Option<[u8; 32]>,
    pub format_version: Option<u8>,
    pub canonical_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalEvidenceRecoveryState {
    Staged,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverableTerminalEvidencePage {
    pub entries: Vec<RecoverableTerminalEvidence>,
    pub more: bool,
}

/// Internal recovery-page result that reports contention without omitting
/// independent cold records. Hosts must retry when `busy` is true; they must
/// never interpret it as an empty or complete ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverableTerminalEvidenceContentionPage {
    pub entries: Vec<RecoverableTerminalEvidence>,
    pub more: bool,
    pub busy: bool,
}

/// Result of ending a transient runtime reservation lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalEvidenceAbandonment {
    ReleasedUnusedReservation,
    RetainedForRecovery,
}

#[derive(Debug)]
pub enum TerminalEvidenceError {
    Invalid(&'static str),
    Quota,
    Unresolved,
    Missing,
    Conflict,
    Corrupt,
    Busy,
    Storage(rusqlite::Error),
}

impl std::fmt::Display for TerminalEvidenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(value) => {
                write!(formatter, "terminal evidence argument is invalid: {value}")
            }
            Self::Quota => formatter.write_str("terminal evidence quota is exhausted"),
            Self::Unresolved => formatter.write_str("terminal evidence reservation is unresolved"),
            Self::Missing => formatter.write_str("terminal evidence was not found"),
            Self::Conflict => formatter
                .write_str("terminal evidence digest or bytes conflict with an earlier commit"),
            Self::Corrupt => {
                formatter.write_str("terminal evidence storage integrity check failed")
            }
            Self::Busy => formatter
                .write_str("terminal evidence storage is busy; retry without changing the request"),
            Self::Storage(error) => write!(formatter, "terminal evidence storage failed: {error}"),
        }
    }
}
impl std::error::Error for TerminalEvidenceError {}
impl From<rusqlite::Error> for TerminalEvidenceError {
    fn from(error: rusqlite::Error) -> Self {
        if matches!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        ) {
            Self::Busy
        } else {
            Self::Storage(error)
        }
    }
}

type Result<T> = std::result::Result<T, TerminalEvidenceError>;

pub struct TerminalEvidenceLedger {
    connection: Mutex<Connection>,
    maximum_bytes: u64,
    /// A staged row belongs to the process that holds this per-reservation
    /// authority. The fixed metadata gate serializes sidecar creation and
    /// removal, so cleanup never creates two lock domains for one record.
    /// Process exit releases the OS lock, which is the cold-recovery boundary.
    reservation_authorities: Mutex<HashMap<String, ReservationAuthority>>,
    authority_directory: PathBuf,
    authority_stem: String,
}

/// The holder is deliberately private to the trusted host.  A recovery lease
/// is distinct from a live source, even when both are served by the same
/// ledger object: treating a local lock as automatically recoverable would
/// let recovery retire a terminal before its source has delivered it.
enum ReservationAuthority {
    Source(File),
    Recovery { lease_id: String, file: File },
}

const LEGACY_RECOVERY_LEASE: &str = "ubm-terminal-evidence-legacy-recovery";

impl TerminalEvidenceLedger {
    pub fn open(path: &Path, maximum_bytes: u64) -> Result<Self> {
        if maximum_bytes == 0 || maximum_bytes > i64::MAX as u64 {
            return Err(TerminalEvidenceError::Invalid("maximum bytes"));
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(std::time::Duration::ZERO)?;
        connection.execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA; PRAGMA secure_delete=ON;
             CREATE TABLE IF NOT EXISTS terminal_evidence_config(id INTEGER PRIMARY KEY CHECK(id=1), maximum_bytes INTEGER NOT NULL CHECK(maximum_bytes>0));
             CREATE TABLE IF NOT EXISTS terminal_evidence_reservations(
               id TEXT PRIMARY KEY, producer TEXT NOT NULL, manager_id TEXT NOT NULL, operation_id TEXT NOT NULL,
               incarnation TEXT NOT NULL, maximum_bytes INTEGER NOT NULL CHECK(maximum_bytes>0),
               state TEXT NOT NULL CHECK(state IN ('reserved','committed','released')),
               reference TEXT UNIQUE, digest BLOB, bytes BLOB, staged_bytes BLOB, staged_recipient TEXT, staged_published INTEGER NOT NULL DEFAULT 0, format_version INTEGER, released_sequence INTEGER
             );"
        )?;
        let has_release_sequence: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('terminal_evidence_reservations') WHERE name='released_sequence')",
            [],
            |row| row.get(0),
        )?;
        if !has_release_sequence {
            connection.execute_batch(
                "ALTER TABLE terminal_evidence_reservations ADD COLUMN released_sequence INTEGER;",
            )?;
        }
        let has_producer: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('terminal_evidence_reservations') WHERE name='producer')", [], |row| row.get(0))?;
        if !has_producer {
            connection.execute_batch(
                "ALTER TABLE terminal_evidence_reservations ADD COLUMN producer TEXT;",
            )?;
        }
        let has_staged_bytes: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('terminal_evidence_reservations') WHERE name='staged_bytes')",
            [], |row| row.get(0),
        )?;
        if !has_staged_bytes {
            connection.execute_batch(
                "ALTER TABLE terminal_evidence_reservations ADD COLUMN staged_bytes BLOB;",
            )?;
        }
        let has_recipient: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('terminal_evidence_reservations') WHERE name='staged_recipient')", [], |row| row.get(0))?;
        if !has_recipient {
            connection.execute_batch(
                "ALTER TABLE terminal_evidence_reservations ADD COLUMN staged_recipient TEXT;",
            )?;
        }
        let has_published: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('terminal_evidence_reservations') WHERE name='staged_published')", [], |row| row.get(0))?;
        if !has_published {
            connection.execute_batch("ALTER TABLE terminal_evidence_reservations ADD COLUMN staged_published INTEGER NOT NULL DEFAULT 0;")?;
        }
        let stored: Option<i64> = connection
            .query_row(
                "SELECT maximum_bytes FROM terminal_evidence_config WHERE id=1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match stored {
            Some(stored) if stored as u64 != maximum_bytes => {
                return Err(TerminalEvidenceError::Conflict);
            }
            Some(_) => {}
            None => {
                connection.execute(
                    "INSERT INTO terminal_evidence_config(id,maximum_bytes) VALUES(1,?1)",
                    [maximum_bytes as i64],
                )?;
            }
        }
        let authority_directory = path
            .parent()
            .ok_or(TerminalEvidenceError::Invalid("ledger path"))?
            .to_path_buf();
        let authority_stem = path
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .ok_or(TerminalEvidenceError::Invalid("ledger path"))?
            .to_owned();
        Ok(Self {
            connection: Mutex::new(connection),
            maximum_bytes,
            reservation_authorities: Mutex::new(HashMap::new()),
            authority_directory,
            authority_stem,
        })
    }

    fn authority_path(&self, reservation_id: &str) -> PathBuf {
        let identity = hex(&Sha256::digest(reservation_id.as_bytes()));
        self.authority_directory
            .join(format!(".{}-{}.authority", self.authority_stem, identity))
    }

    fn authority_gate_path(&self) -> PathBuf {
        self.authority_directory
            .join(format!(".{}-authority-gate", self.authority_stem))
    }

    fn open_authority_file(&self, path: &Path) -> Result<File> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        let file = options
            .open(path)
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        let metadata = file
            .metadata()
            .map_err(|_| TerminalEvidenceError::Corrupt)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != 0 {
            return Err(TerminalEvidenceError::Corrupt);
        }
        Ok(file)
    }

    fn acquire_authority_gate(&self) -> Result<File> {
        let file = self.open_authority_file(&self.authority_gate_path())?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => TerminalEvidenceError::Busy,
            std::fs::TryLockError::Error(_) => TerminalEvidenceError::Unresolved,
        })?;
        Ok(file)
    }

    /// Acquire one record's OS lifetime authority.  Different reservations
    /// have different sidecars, so independent managers remain concurrent.
    /// Returns whether this call acquired a new authority rather than reusing
    /// the current ledger's existing ownership.
    fn acquire_source_authority(&self, reservation_id: &str) -> Result<bool> {
        // The gate stays held only while opening or removing the record lock.
        // It prevents unlink/recreate from making two lock domains for one
        // reservation while leaving different reservations independent.
        let _gate = self.acquire_authority_gate()?;
        let mut authorities = self
            .reservation_authorities
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        if let Some(authority) = authorities.get(reservation_id) {
            return match authority {
                ReservationAuthority::Source(_) => Ok(false),
                ReservationAuthority::Recovery { .. } => Err(TerminalEvidenceError::Busy),
            };
        }
        let file = self.open_authority_file(&self.authority_path(reservation_id))?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => TerminalEvidenceError::Busy,
            std::fs::TryLockError::Error(_) => TerminalEvidenceError::Unresolved,
        })?;
        authorities.insert(
            reservation_id.to_owned(),
            ReservationAuthority::Source(file),
        );
        Ok(true)
    }

    /// Acquire one row for a particular recovery lease.  A repeated list/read
    /// from the same lease is idempotent; another recovery lease and a live
    /// source both see a retryable Busy outcome.
    fn acquire_recovery_authority(&self, reservation_id: &str, lease_id: &str) -> Result<bool> {
        if lease_id.is_empty() {
            return Err(TerminalEvidenceError::Invalid("recovery lease"));
        }
        let _gate = self.acquire_authority_gate()?;
        let mut authorities = self
            .reservation_authorities
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        if let Some(authority) = authorities.get(reservation_id) {
            return match authority {
                ReservationAuthority::Recovery { lease_id: held, .. } if held == lease_id => {
                    Ok(false)
                }
                ReservationAuthority::Source(_) | ReservationAuthority::Recovery { .. } => {
                    Err(TerminalEvidenceError::Busy)
                }
            };
        }
        let file = self.open_authority_file(&self.authority_path(reservation_id))?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => TerminalEvidenceError::Busy,
            std::fs::TryLockError::Error(_) => TerminalEvidenceError::Unresolved,
        })?;
        authorities.insert(
            reservation_id.to_owned(),
            ReservationAuthority::Recovery {
                lease_id: lease_id.to_owned(),
                file,
            },
        );
        Ok(true)
    }

    fn has_recovery_authority_held(
        &self,
        authorities: &HashMap<String, ReservationAuthority>,
        reservation_id: &str,
        lease_id: &str,
    ) -> Result<()> {
        match authorities.get(reservation_id) {
            Some(ReservationAuthority::Recovery { lease_id: held, .. }) if held == lease_id => {
                Ok(())
            }
            Some(_) => Err(TerminalEvidenceError::Busy),
            None => Err(TerminalEvidenceError::Missing),
        }
    }

    /// The pre-lease recovery API is observational: it may inspect bytes
    /// owned by this ledger's live source, but it must still fail closed when
    /// another process or recovery controller owns the sidecar. It never
    /// retains authority after returning a snapshot.
    fn observe_legacy_recovery(&self, reservation_id: &str) -> Result<()> {
        let _gate = self.acquire_authority_gate()?;
        let authorities = self
            .reservation_authorities
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        match authorities.get(reservation_id) {
            Some(ReservationAuthority::Source(_)) => Ok(()),
            Some(ReservationAuthority::Recovery { .. }) => Err(TerminalEvidenceError::Busy),
            None => {
                let file = self.open_authority_file(&self.authority_path(reservation_id))?;
                file.try_lock().map_err(|error| match error {
                    std::fs::TryLockError::WouldBlock => TerminalEvidenceError::Busy,
                    std::fs::TryLockError::Error(_) => TerminalEvidenceError::Unresolved,
                })?;
                Ok(())
            }
        }
    }

    /// Release only authorities held by one private recovery controller. The
    /// durable rows remain untouched: a later controller may recover them,
    /// while a live source authority is never released through this path.
    pub fn release_recovery_lease(&self, lease_id: &str) -> Result<()> {
        // Match acquisition order (gate, then in-process map).  In
        // particular, an old controller's close must not race a new source
        // acquiring this record between validation and lock removal.
        let _gate = self.acquire_authority_gate()?;
        let mut authorities = self
            .reservation_authorities
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        authorities.retain(|_, authority| {
            !matches!(authority, ReservationAuthority::Recovery { lease_id: held, .. } if held == lease_id)
        });
        Ok(())
    }

    fn release_reservation_authority(&self, reservation_id: &str) {
        if let Ok(mut authorities) = self.reservation_authorities.lock() {
            authorities.remove(reservation_id);
        }
    }

    /// Remove a record sidecar only after its state no longer needs live
    /// staging authority. The metadata gate closes the unlink/recreate race:
    /// no other process can create a replacement authority file until this
    /// process has dropped the old OS lock and removed its name.
    fn retire_reservation_authority(&self, reservation_id: &str) -> Result<()> {
        let _gate = self.acquire_authority_gate()?;
        self.release_reservation_authority(reservation_id);
        if let Err(error) = std::fs::remove_file(self.authority_path(reservation_id)) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(TerminalEvidenceError::Unresolved);
            }
        }
        Ok(())
    }

    pub fn reserve(
        &self,
        scope: &TerminalEvidenceScope,
        maximum_bytes: u64,
    ) -> Result<TerminalEvidenceReservation> {
        if maximum_bytes == 0
            || maximum_bytes > i64::MAX as u64
            || scope.producer.is_empty()
            || scope.manager_id.is_empty()
            || scope.operation_id.is_empty()
            || scope.incarnation.is_empty()
        {
            return Err(TerminalEvidenceError::Invalid("reservation"));
        }
        let id = terminal_id(scope);
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(i64, String)> = tx
            .query_row(
                "SELECT maximum_bytes,state FROM terminal_evidence_reservations WHERE id=?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored, state)) = existing {
            if stored as u64 != maximum_bytes {
                return Err(TerminalEvidenceError::Conflict);
            }
            if state == "released" {
                return Err(TerminalEvidenceError::Missing);
            }
            tx.commit()?;
            return Ok(TerminalEvidenceReservation {
                reservation_id: id,
                maximum_bytes,
            });
        }
        let used: i64 = tx.query_row(
            "SELECT coalesce(sum(CASE WHEN state='reserved' THEN maximum_bytes WHEN state='committed' THEN coalesce(length(staged_bytes),length(bytes)) ELSE 0 END),0) FROM terminal_evidence_reservations",
            [], |row| row.get(0),
        )?;
        if maximum_bytes > self.maximum_bytes.saturating_sub(used as u64) {
            return Err(TerminalEvidenceError::Quota);
        }
        tx.execute(
            "INSERT INTO terminal_evidence_reservations(id,producer,manager_id,operation_id,incarnation,maximum_bytes,state) VALUES(?1,?2,?3,?4,?5,?6,'reserved')",
            params![id, scope.producer, scope.manager_id, scope.operation_id, scope.incarnation, maximum_bytes as i64],
        )?;
        tx.commit()?;
        Ok(TerminalEvidenceReservation {
            reservation_id: id,
            maximum_bytes,
        })
    }

    /// Reattach only the exact durable preactivation reservation. This never
    /// allocates capacity or accepts a caller-provided reservation identity.
    pub fn attach(&self, scope: &TerminalEvidenceScope) -> Result<TerminalEvidenceReservation> {
        let id = terminal_id(scope);
        let connection = self
            .connection
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        let row: Option<(i64, String)> = connection
            .query_row(
                "SELECT maximum_bytes,state FROM terminal_evidence_reservations WHERE id=?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match row {
            Some((maximum_bytes, state)) if state != "released" => {
                Ok(TerminalEvidenceReservation {
                    reservation_id: id,
                    maximum_bytes: maximum_bytes as u64,
                })
            }
            Some(_) => Err(TerminalEvidenceError::Missing),
            None => Err(TerminalEvidenceError::Unresolved),
        }
    }

    /// Looks up the exact bound reserved for an active producer before it
    /// decodes an untrusted encoded record.
    pub fn reservation_maximum_bytes(&self, reservation_id: &str) -> Result<u64> {
        if reservation_id.is_empty() {
            return Err(TerminalEvidenceError::Invalid("reservation"));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        let row: Option<(i64, String)> = connection
            .query_row(
                "SELECT maximum_bytes,state FROM terminal_evidence_reservations WHERE id=?1",
                [reservation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match row {
            Some((maximum_bytes, state)) if state != "released" => Ok(maximum_bytes as u64),
            Some(_) => Err(TerminalEvidenceError::Missing),
            None => Err(TerminalEvidenceError::Missing),
        }
    }

    /// Persist exact canonical terminal bytes before a retryable commit. The
    /// same reservation may only ever stage the same bytes.
    pub fn stage(
        &self,
        reservation_id: &str,
        recipient: &str,
        canonical_bytes: &[u8],
    ) -> Result<()> {
        if reservation_id.is_empty() || recipient.is_empty() || canonical_bytes.is_empty() {
            return Err(TerminalEvidenceError::Invalid("stage"));
        }
        let acquired = self.acquire_source_authority(reservation_id)?;
        let outcome = (|| {
            let mut connection = self
                .connection
                .lock()
                .map_err(|_| TerminalEvidenceError::Unresolved)?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row: Option<(i64, String, Option<Vec<u8>>, Option<String>)> = tx.query_row(
                "SELECT maximum_bytes,state,staged_bytes,staged_recipient FROM terminal_evidence_reservations WHERE id=?1", [reservation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).optional()?;
            let Some((maximum, state, staged, staged_recipient)) = row else {
                return Err(TerminalEvidenceError::Missing);
            };
            if state == "released" {
                return Err(TerminalEvidenceError::Missing);
            }
            if canonical_bytes.len() as u64 > maximum as u64 {
                return Err(TerminalEvidenceError::Quota);
            }
            if let Some(staged) = staged {
                if staged != canonical_bytes || staged_recipient.as_deref() != Some(recipient) {
                    return Err(TerminalEvidenceError::Conflict);
                }
            } else {
                tx.execute("UPDATE terminal_evidence_reservations SET staged_bytes=?2,staged_recipient=?3,staged_published=0 WHERE id=?1", params![reservation_id, canonical_bytes, recipient])?;
            }
            tx.commit()?;
            Ok(())
        })();
        if outcome.is_err() && acquired {
            self.release_reservation_authority(reservation_id);
        }
        outcome
    }

    pub fn staged(&self, producer: &str, recipient: &str) -> Result<Vec<StagedTerminalEvidence>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        let mut statement = connection.prepare(
            "SELECT id,manager_id,operation_id,incarnation,staged_bytes FROM terminal_evidence_reservations WHERE producer=?1 AND state!='released' AND staged_bytes IS NOT NULL AND staged_published=0 AND staged_recipient=?2"
        )?;
        statement
            .query_map(params![producer, recipient], |row| {
                Ok(StagedTerminalEvidence {
                    reservation_id: row.get(0)?,
                    scope: TerminalEvidenceScope {
                        producer: producer.to_owned(),
                        manager_id: row.get(1)?,
                        operation_id: row.get(2)?,
                        incarnation: row.get(3)?,
                    },
                    canonical_bytes: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Returns exact retained terminal bytes only for the original trusted
    /// recipient.  A caller never supplies a reservation id to select a row.
    /// Both pre-commit staged and committed-but-unacknowledged rows remain
    /// recoverable because their staged bytes are the original canonical
    /// evidence.
    pub fn recoverable(
        &self,
        producer: &str,
        recipient: &str,
    ) -> Result<Vec<RecoverableTerminalEvidence>> {
        Ok(self
            .recoverable_page(producer, recipient, u32::MAX, u64::MAX)?
            .entries)
    }

    /// Returns at most the requested recoverable rows and bytes. Row metadata
    /// is inspected first; retained bytes are selected only after the row fits
    /// both limits, so a caller cannot force an unbounded payload clone merely
    /// by requesting a small page.
    pub fn recoverable_page(
        &self,
        producer: &str,
        recipient: &str,
        maximum_items: u32,
        maximum_bytes: u64,
    ) -> Result<RecoverableTerminalEvidencePage> {
        // This compatibility entrypoint predates recovery-controller leases:
        // it only reads a snapshot and returns no authority-bearing handle.
        // Keep that behavior so a legacy inspection cannot block the live
        // source from committing, nor a later legacy inspection. New hosts
        // that need selected-row authority use `recoverable_page_for_lease`.
        let page = self.recoverable_page_for_lease_internal(
            producer,
            recipient,
            LEGACY_RECOVERY_LEASE,
            maximum_items,
            maximum_bytes,
            false,
            false,
        )?;
        debug_assert!(!page.busy);
        Ok(RecoverableTerminalEvidencePage {
            entries: page.entries,
            more: page.more,
        })
    }

    /// The lease is host-private. It binds the rows returned by this page to
    /// the recovery controller that selected them, so another controller
    /// cannot read or retire a live source's staged terminal by sharing the
    /// same process ledger.
    pub fn recoverable_page_for_lease(
        &self,
        producer: &str,
        recipient: &str,
        lease_id: &str,
        maximum_items: u32,
        maximum_bytes: u64,
    ) -> Result<RecoverableTerminalEvidencePage> {
        let page = self.recoverable_page_for_lease_internal(
            producer,
            recipient,
            lease_id,
            maximum_items,
            maximum_bytes,
            false,
            true,
        )?;
        debug_assert!(!page.busy);
        Ok(RecoverableTerminalEvidencePage {
            entries: page.entries,
            more: page.more,
        })
    }

    /// Enumerate cold rows held by this lease even when an unrelated live
    /// source owns another matching row. `busy` is an explicit retry signal
    /// for every skipped contended row; strict recovery callers should use
    /// [`Self::recoverable_page_for_lease`] instead.
    pub fn recoverable_page_for_lease_with_contention(
        &self,
        producer: &str,
        recipient: &str,
        lease_id: &str,
        maximum_items: u32,
        maximum_bytes: u64,
    ) -> Result<RecoverableTerminalEvidenceContentionPage> {
        self.recoverable_page_for_lease_internal(
            producer,
            recipient,
            lease_id,
            maximum_items,
            maximum_bytes,
            true,
            true,
        )
    }

    fn recoverable_page_for_lease_internal(
        &self,
        producer: &str,
        recipient: &str,
        lease_id: &str,
        maximum_items: u32,
        maximum_bytes: u64,
        skip_busy: bool,
        claim_authority: bool,
    ) -> Result<RecoverableTerminalEvidenceContentionPage> {
        if producer.is_empty() || recipient.is_empty() {
            return Err(TerminalEvidenceError::Invalid("recovery recipient"));
        }
        if maximum_items == 0 || maximum_bytes == 0 {
            return Err(TerminalEvidenceError::Invalid("recovery page bounds"));
        }
        let metadata = {
            let connection = self
                .connection
                .lock()
                .map_err(|_| TerminalEvidenceError::Unresolved)?;
            let mut statement = connection.prepare(
                "SELECT id,manager_id,operation_id,incarnation,state,reference,digest,format_version,maximum_bytes,length(staged_bytes) \
                 FROM terminal_evidence_reservations \
                 WHERE producer=?1 AND state!='released' AND staged_bytes IS NOT NULL AND staged_recipient=?2 \
                 ORDER BY id"
            )?;
            statement
                .query_map(params![producer, recipient], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<Vec<u8>>>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, i64>(9)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| match error {
                    rusqlite::Error::InvalidQuery => TerminalEvidenceError::Corrupt,
                    other => other.into(),
                })?
        };
        let mut newly_acquired = Vec::new();
        let page_result = (|| -> Result<RecoverableTerminalEvidenceContentionPage> {
            let mut total = 0_u64;
            let mut recovered = Vec::new();
            let mut more = false;
            let mut busy = false;
            for (
                id,
                manager_id,
                operation_id,
                incarnation,
                state,
                reference,
                digest,
                format_version,
                reserved,
                staged_length,
            ) in metadata
            {
                let staged_length =
                    u64::try_from(staged_length).map_err(|_| TerminalEvidenceError::Corrupt)?;
                if reserved <= 0 || staged_length == 0 || staged_length > reserved as u64 {
                    return Err(TerminalEvidenceError::Corrupt);
                }
                // Lease-aware recovery takes the per-reservation authority
                // before materializing bytes. A live writer racing that
                // snapshot therefore produces Busy rather than an unsafe
                // clone. The pre-lease compatibility reader intentionally
                // remains observational and owns no recovery authority.
                let acquired = if claim_authority {
                    match self.acquire_recovery_authority(&id, lease_id) {
                        Ok(true) => true,
                        // This is the same recovery controller re-listing a
                        // row it already owns. A source or another controller
                        // has a different authority kind/id and returns Busy.
                        Ok(false) => false,
                        Err(TerminalEvidenceError::Busy) if skip_busy => {
                            busy = true;
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                } else {
                    self.observe_legacy_recovery(&id)?;
                    false
                };
                if acquired {
                    newly_acquired.push(id.clone());
                }
                if recovered.len() >= maximum_items as usize
                    || total
                        .checked_add(staged_length)
                        .ok_or(TerminalEvidenceError::Corrupt)?
                        > maximum_bytes
                {
                    if acquired {
                        self.release_reservation_authority(&id);
                        newly_acquired.pop();
                    }
                    more = true;
                    break;
                }
                let (staged, committed_bytes): (Vec<u8>, Option<Vec<u8>>) = self
                    .connection
                    .lock()
                    .map_err(|_| TerminalEvidenceError::Unresolved)?
                    .query_row(
                        "SELECT staged_bytes,bytes FROM terminal_evidence_reservations WHERE id=?1",
                        [&id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?;
                if staged.len() as u64 != staged_length {
                    return Err(TerminalEvidenceError::Corrupt);
                }
                let (recovery_state, reference, digest, byte_length, format_version) =
                    if state == "committed" {
                        let digest = digest.ok_or(TerminalEvidenceError::Corrupt)?;
                        if digest.len() != 32
                            || format_version != Some(1)
                            || committed_bytes
                                .as_deref()
                                .is_some_and(|bytes| bytes != staged)
                        {
                            return Err(TerminalEvidenceError::Corrupt);
                        }
                        let mut exact = [0; 32];
                        exact.copy_from_slice(&digest);
                        if Sha256::digest(&staged).as_slice() != exact {
                            return Err(TerminalEvidenceError::Corrupt);
                        }
                        (
                            TerminalEvidenceRecoveryState::Committed,
                            Some(reference.ok_or(TerminalEvidenceError::Corrupt)?),
                            Some(exact),
                            Some(staged_length),
                            Some(1),
                        )
                    } else if state == "reserved" {
                        (
                            TerminalEvidenceRecoveryState::Staged,
                            None,
                            None,
                            None,
                            None,
                        )
                    } else {
                        return Err(TerminalEvidenceError::Corrupt);
                    };
                total += staged_length;
                recovered.push(RecoverableTerminalEvidence {
                    reservation_id: id,
                    scope: TerminalEvidenceScope {
                        producer: producer.to_owned(),
                        manager_id,
                        operation_id,
                        incarnation,
                    },
                    state: recovery_state,
                    reference,
                    byte_length,
                    digest,
                    format_version,
                    canonical_bytes: staged,
                });
            }
            Ok(RecoverableTerminalEvidenceContentionPage {
                entries: recovered,
                more,
                busy,
            })
        })();
        if page_result.is_err() {
            for reservation_id in newly_acquired {
                self.release_reservation_authority(&reservation_id);
            }
        }
        page_result
    }

    pub fn has_recoverable(&self, producer: &str, recipient: &str) -> Result<bool> {
        if producer.is_empty() || recipient.is_empty() {
            return Err(TerminalEvidenceError::Invalid("recovery recipient"));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        Ok(connection.query_row(
            "SELECT 1 FROM terminal_evidence_reservations WHERE producer=?1 AND state!='released' AND staged_bytes IS NOT NULL AND staged_recipient=?2 LIMIT 1",
            params![producer, recipient],
            |_| Ok(()),
        ).optional()?.is_some())
    }

    pub fn commit(
        &self,
        reservation_id: &str,
        canonical_bytes: &[u8],
        digest: [u8; 32],
    ) -> Result<CommittedTerminalEvidence> {
        let acquired = self.acquire_source_authority(reservation_id)?;
        self.commit_with_held_authority(reservation_id, canonical_bytes, digest, acquired)
    }

    /// Promote bytes selected by a particular recovery lease. This is not a
    /// public protocol operation: the native host supplies the lease only
    /// after it has authenticated an opaque recovery handle.
    pub fn commit_recovered(
        &self,
        lease_id: &str,
        reservation_id: &str,
        canonical_bytes: &[u8],
        digest: [u8; 32],
    ) -> Result<CommittedTerminalEvidence> {
        let _gate = self.acquire_authority_gate()?;
        let _authorities = self
            .reservation_authorities
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        self.has_recovery_authority_held(&_authorities, reservation_id, lease_id)?;
        self.commit_with_held_authority(reservation_id, canonical_bytes, digest, false)
    }

    fn commit_with_held_authority(
        &self,
        reservation_id: &str,
        canonical_bytes: &[u8],
        digest: [u8; 32],
        release_on_error: bool,
    ) -> Result<CommittedTerminalEvidence> {
        if reservation_id.is_empty() || canonical_bytes.is_empty() {
            return Err(TerminalEvidenceError::Invalid("commit"));
        }
        if Sha256::digest(canonical_bytes).as_slice() != digest {
            return Err(TerminalEvidenceError::Conflict);
        }
        let outcome = (|| {
            let mut connection = self
                .connection
                .lock()
                .map_err(|_| TerminalEvidenceError::Unresolved)?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row: Option<(
                i64,
                String,
                Option<String>,
                Option<Vec<u8>>,
                Option<Vec<u8>>,
                Option<Vec<u8>>,
            )> = tx.query_row(
                "SELECT maximum_bytes,state,reference,digest,bytes,staged_bytes FROM terminal_evidence_reservations WHERE id=?1", [reservation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            ).optional()?;
            let Some((maximum, state, reference, stored_digest, stored_bytes, staged_bytes)) = row
            else {
                return Err(TerminalEvidenceError::Missing);
            };
            if state == "released" {
                return Err(TerminalEvidenceError::Missing);
            }
            if state == "committed" {
                let stored_canonical = staged_bytes.as_deref().or(stored_bytes.as_deref());
                if stored_digest.as_deref() != Some(digest.as_slice())
                    || stored_canonical != Some(canonical_bytes)
                {
                    return Err(TerminalEvidenceError::Conflict);
                }
                let reference = reference.ok_or(TerminalEvidenceError::Unresolved)?;
                tx.commit()?;
                return Ok(CommittedTerminalEvidence {
                    reference,
                    byte_length: canonical_bytes.len() as u64,
                    digest,
                    format_version: 1,
                });
            }
            if canonical_bytes.len() as u64 > maximum as u64 {
                return Err(TerminalEvidenceError::Quota);
            }
            if staged_bytes
                .as_deref()
                .is_some_and(|staged| staged != canonical_bytes)
            {
                return Err(TerminalEvidenceError::Conflict);
            }
            let reference = opaque_reference()?;
            tx.execute(
                "UPDATE terminal_evidence_reservations SET state='committed',reference=?2,digest=?3,bytes=NULL,staged_bytes=coalesce(staged_bytes,?4),format_version=1 WHERE id=?1",
                params![reservation_id, reference, digest.as_slice(), canonical_bytes],
            )?;
            tx.commit()?;
            Ok(CommittedTerminalEvidence {
                reference,
                byte_length: canonical_bytes.len() as u64,
                digest,
                format_version: 1,
            })
        })();
        match outcome {
            Ok(committed) => {
                // Keep the authority through the live source's terminal
                // delivery. `abandon` hands it to cold recovery only after
                // the source has stopped owning the record; otherwise a
                // concurrent recovery could ACK bytes before delivery.
                Ok(committed)
            }
            Err(error) => {
                if release_on_error {
                    self.release_reservation_authority(reservation_id);
                }
                Err(error)
            }
        }
    }

    pub fn read(&self, reference: &str, maximum_bytes: u64) -> Result<(Vec<u8>, [u8; 32])> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        let row: Option<(Vec<u8>, Vec<u8>)> = connection.query_row(
            "SELECT coalesce(staged_bytes,bytes),digest FROM terminal_evidence_reservations WHERE reference=?1 AND state='committed'", [reference],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        let Some((bytes, digest)) = row else {
            return Err(TerminalEvidenceError::Missing);
        };
        if bytes.len() as u64 > maximum_bytes || digest.len() != 32 {
            return Err(TerminalEvidenceError::Invalid("read bound"));
        }
        let mut exact = [0; 32];
        exact.copy_from_slice(&digest);
        if Sha256::digest(&bytes).as_slice() != exact {
            return Err(TerminalEvidenceError::Corrupt);
        }
        Ok((bytes, exact))
    }

    /// End a transient runtime lease. An unused reservation frees its
    /// pre-admitted quota. Once bytes have been staged or committed this is a
    /// deliberate no-op: restart recovery, not stream drainage, owns their
    /// retirement.
    pub fn abandon(&self, reservation_id: &str) -> Result<TerminalEvidenceAbandonment> {
        let acquired = self.acquire_source_authority(reservation_id)?;
        let outcome = (|| {
            let mut connection = self
                .connection
                .lock()
                .map_err(|_| TerminalEvidenceError::Unresolved)?;
            let changed = connection.execute(
                "UPDATE terminal_evidence_reservations SET state='released',bytes=NULL,digest=NULL,staged_bytes=NULL,staged_recipient=NULL,released_sequence=(SELECT coalesce(max(released_sequence),0)+1 FROM terminal_evidence_reservations) WHERE id=?1 AND state='reserved' AND staged_bytes IS NULL", [reservation_id],
            )?;
            if changed != 0 {
                Self::prune_released(&connection)?;
                return Ok(TerminalEvidenceAbandonment::ReleasedUnusedReservation);
            }
            let state: Option<String> = connection
                .query_row(
                    "SELECT state FROM terminal_evidence_reservations WHERE id=?1",
                    [reservation_id],
                    |row| row.get(0),
                )
                .optional()?;
            match state.as_deref() {
                Some("reserved") | Some("committed") => {}
                Some("released") | None => return Err(TerminalEvidenceError::Missing),
                Some(_) => return Err(TerminalEvidenceError::Corrupt),
            }
            Self::prune_released(&connection)?;
            Ok(TerminalEvidenceAbandonment::RetainedForRecovery)
        })();
        match outcome {
            Ok(TerminalEvidenceAbandonment::ReleasedUnusedReservation) => {
                self.retire_reservation_authority(reservation_id)?;
                Ok(TerminalEvidenceAbandonment::ReleasedUnusedReservation)
            }
            Ok(TerminalEvidenceAbandonment::RetainedForRecovery) => {
                // Teardown hands staged bytes to recovery, so stop presenting
                // this process as their live writer.
                self.release_reservation_authority(reservation_id);
                Ok(TerminalEvidenceAbandonment::RetainedForRecovery)
            }
            Err(error) => {
                if acquired {
                    self.release_reservation_authority(reservation_id);
                }
                Err(error)
            }
        }
    }

    /// Irreversibly retire committed evidence only after the recovery owner
    /// acknowledges its opaque reference. Retrying after retirement is a
    /// no-op; a foreign owner cannot retire a live record.
    pub fn acknowledge_recovered(
        &self,
        lease_id: &str,
        reservation_id: &str,
        reference: &str,
        producer: &str,
        recipient: &str,
    ) -> Result<()> {
        if producer.is_empty() || recipient.is_empty() || !is_opaque_reference(reference) {
            return Err(TerminalEvidenceError::Invalid("recovery acknowledgement"));
        }
        // Keep the authority-map entry through the database state transition.
        // `release_recovery_lease` and every new source/recovery acquisition
        // take the same gate then map order, so a stale controller cannot
        // validate one owner and retire a later owner's record.
        let _gate = self.acquire_authority_gate()?;
        let mut authorities = self
            .reservation_authorities
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        self.has_recovery_authority_held(&authorities, reservation_id, lease_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        let changed = connection.execute(
            "UPDATE terminal_evidence_reservations SET state='released',bytes=NULL,digest=NULL,staged_bytes=NULL,staged_recipient=NULL,released_sequence=(SELECT coalesce(max(released_sequence),0)+1 FROM terminal_evidence_reservations) WHERE id=?1 AND reference=?2 AND producer=?3 AND coalesce(staged_recipient,'')=?4 AND state='committed'",
            params![reservation_id, reference, producer, recipient],
        )?;
        if changed == 0 {
            let row: Option<(String, String, String, Option<String>)> = connection
                .query_row(
                    "SELECT id,state,producer,staged_recipient FROM terminal_evidence_reservations WHERE reference=?1",
                    [reference],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            match row {
                Some((id, _, _, _)) if id != reservation_id => {
                    return Err(TerminalEvidenceError::Missing);
                }
                Some((_, state, stored_producer, stored_recipient))
                    if state == "committed" || state == "reserved" =>
                {
                    if stored_producer != producer
                        || stored_recipient.as_deref().unwrap_or("") != recipient
                    {
                        return Err(TerminalEvidenceError::Missing);
                    }
                    return Err(TerminalEvidenceError::Corrupt);
                }
                Some((_, state, _, _)) if state == "released" => {}
                Some(_) => return Err(TerminalEvidenceError::Corrupt),
                None => {}
            }
        }
        drop(connection);
        authorities.remove(reservation_id);
        drop(authorities);
        if let Err(error) = std::fs::remove_file(self.authority_path(reservation_id)) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(TerminalEvidenceError::Unresolved);
            }
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        Self::prune_released(&connection)?;
        Ok(())
    }

    pub fn acknowledge(&self, reference: &str, producer: &str, recipient: &str) -> Result<()> {
        if producer.is_empty() || recipient.is_empty() || !is_opaque_reference(reference) {
            return Err(TerminalEvidenceError::Invalid("recovery acknowledgement"));
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        let reservation_id: Option<String> = connection
            .query_row(
                "SELECT id FROM terminal_evidence_reservations WHERE reference=?1",
                [reference],
                |row| row.get(0),
            )
            .optional()?;
        let changed = connection.execute(
            "UPDATE terminal_evidence_reservations SET state='released',bytes=NULL,digest=NULL,staged_bytes=NULL,staged_recipient=NULL,released_sequence=(SELECT coalesce(max(released_sequence),0)+1 FROM terminal_evidence_reservations) WHERE reference=?1 AND producer=?2 AND coalesce(staged_recipient,'')=?3 AND state='committed'", params![reference, producer, recipient],
        )?;
        if changed == 0 {
            let row: Option<(String, String, Option<String>)> = connection.query_row(
                "SELECT state,producer,staged_recipient FROM terminal_evidence_reservations WHERE reference=?1",
                [reference],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional()?;
            match row {
                Some((state, stored_producer, stored_recipient))
                    if state == "committed" || state == "reserved" =>
                {
                    if stored_producer != producer
                        || stored_recipient.as_deref().unwrap_or("") != recipient
                    {
                        return Err(TerminalEvidenceError::Missing);
                    }
                    return Err(TerminalEvidenceError::Corrupt);
                }
                Some((state, _, _)) if state == "released" => {}
                Some(_) => return Err(TerminalEvidenceError::Corrupt),
                None => {}
            }
        }
        drop(connection);
        if let Some(reservation_id) = reservation_id {
            self.retire_reservation_authority(&reservation_id)?;
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| TerminalEvidenceError::Unresolved)?;
        Self::prune_released(&connection)?;
        Ok(())
    }

    fn prune_released(connection: &Connection) -> Result<()> {
        connection.execute(
            "DELETE FROM terminal_evidence_reservations WHERE state='released' AND released_sequence < coalesce((SELECT max(released_sequence)-255 FROM terminal_evidence_reservations WHERE state='released'),0)",
            [],
        )?;
        Ok(())
    }
}

fn terminal_id(scope: &TerminalEvidenceScope) -> String {
    let mut digest = Sha256::new();
    for field in [
        &scope.producer,
        &scope.manager_id,
        &scope.operation_id,
        &scope.incarnation,
    ] {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn opaque_reference() -> Result<String> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|_| TerminalEvidenceError::Unresolved)?;
    Ok(format!("te1-{}", hex(&bytes)))
}

fn is_opaque_reference(reference: &str) -> bool {
    reference
        .strip_prefix("te1-")
        .and_then(parse_hex::<24>)
        .is_some()
}

fn parse_hex<const N: usize>(encoded: &str) -> Option<[u8; N]> {
    let encoded = encoded.as_bytes();
    if encoded.len() != N.checked_mul(2)? {
        return None;
    }
    let mut bytes = [0_u8; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte =
            hex_nibble(encoded[index * 2])?.checked_mul(16)? | hex_nibble(encoded[index * 2 + 1])?;
    }
    Some(bytes)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("string write");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    fn path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "ubm-terminal-evidence-{}.sqlite",
            uuid::Uuid::new_v4()
        ))
    }
    fn scope() -> TerminalEvidenceScope {
        TerminalEvidenceScope {
            producer: "scan".into(),
            manager_id: "manager".into(),
            operation_id: "scan".into(),
            incarnation: "1".into(),
        }
    }

    fn authority_child() -> bool {
        let Ok(path) = std::env::var("UBM_TERMINAL_AUTHORITY_CHILD_PATH") else {
            return false;
        };
        let ready = std::path::PathBuf::from(
            std::env::var("UBM_TERMINAL_AUTHORITY_CHILD_READY").expect("child ready path"),
        );
        let release = std::path::PathBuf::from(
            std::env::var("UBM_TERMINAL_AUTHORITY_CHILD_RELEASE").expect("child release path"),
        );
        let ledger = TerminalEvidenceLedger::open(Path::new(&path), 64).expect("child open");
        let reservation = ledger.reserve(&scope(), 32).expect("child reserve");
        ledger
            .stage(&reservation.reservation_id, "native-owner", b"terminal")
            .expect("child stage");
        std::fs::write(&ready, b"ready").expect("child ready");
        for _ in 0..500 {
            if release.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("parent did not release terminal authority child");
    }

    #[test]
    fn terminal_evidence_authority_child() {
        if authority_child() {
            return;
        }
    }

    #[test]
    fn staged_record_authority_blocks_live_recovery_but_allows_other_records_and_cold_recovery() {
        let path = path();
        let ready = path.with_extension("child-ready");
        let release = path.with_extension("child-release");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("terminal_evidence_ledger::tests::terminal_evidence_authority_child")
            .arg("--nocapture")
            .env("UBM_TERMINAL_AUTHORITY_CHILD_PATH", &path)
            .env("UBM_TERMINAL_AUTHORITY_CHILD_READY", &ready)
            .env("UBM_TERMINAL_AUTHORITY_CHILD_RELEASE", &release)
            .spawn()
            .expect("spawn authority child");
        for _ in 0..500 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ready.exists(), "child did not stage evidence");

        let recovery = TerminalEvidenceLedger::open(&path, 64).unwrap();
        assert!(matches!(
            recovery.recoverable("scan", "native-owner"),
            Err(TerminalEvidenceError::Busy)
        ));
        assert!(matches!(
            recovery.abandon(&terminal_id(&scope())),
            Err(TerminalEvidenceError::Busy)
        ));
        let independent_scope = TerminalEvidenceScope {
            operation_id: "independent".into(),
            ..scope()
        };
        let independent = recovery.reserve(&independent_scope, 8).unwrap();
        recovery
            .stage(&independent.reservation_id, "other-owner", b"other")
            .unwrap();
        let independent = recovery
            .commit(
                &independent.reservation_id,
                b"other",
                Sha256::digest(b"other").into(),
            )
            .unwrap();
        recovery
            .acknowledge(&independent.reference, "scan", "other-owner")
            .unwrap();

        std::fs::write(&release, b"release").unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
        // `recoverable` claims the legacy recovery lease. Promotion and
        // retirement must remain under that same lease rather than attempting
        // to impersonate the live source after cold recovery.
        let recovered = recovery
            .recoverable_page_for_lease(
                "scan",
                "native-owner",
                LEGACY_RECOVERY_LEASE,
                u32::MAX,
                u64::MAX,
            )
            .unwrap()
            .entries;
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].state, TerminalEvidenceRecoveryState::Staged);
        let committed = recovery
            .commit_recovered(
                LEGACY_RECOVERY_LEASE,
                &recovered[0].reservation_id,
                &recovered[0].canonical_bytes,
                Sha256::digest(&recovered[0].canonical_bytes).into(),
            )
            .unwrap();
        recovery
            .acknowledge_recovered(
                LEGACY_RECOVERY_LEASE,
                &recovered[0].reservation_id,
                &committed.reference,
                "scan",
                "native-owner",
            )
            .unwrap();
        let gate = recovery.authority_gate_path();
        drop(recovery);
        let _ = std::fs::remove_file(ready);
        let _ = std::fs::remove_file(release);
        let _ = std::fs::remove_file(gate);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn recoverable_page_enforces_item_and_byte_bounds_before_payload_selection() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 64).unwrap();
        for (operation_id, bytes) in [
            ("first", b"abcd".as_slice()),
            ("second", b"efgh".as_slice()),
        ] {
            let reservation = ledger
                .reserve(
                    &TerminalEvidenceScope {
                        operation_id: operation_id.into(),
                        ..scope()
                    },
                    8,
                )
                .unwrap();
            ledger
                .stage(&reservation.reservation_id, "native-owner", bytes)
                .unwrap();
        }
        let oversized = ledger
            .recoverable_page("scan", "native-owner", 1, 3)
            .unwrap();
        assert!(oversized.entries.is_empty());
        assert!(oversized.more);
        let one = ledger
            .recoverable_page("scan", "native-owner", 1, 4)
            .unwrap();
        assert_eq!(one.entries.len(), 1);
        assert_eq!(one.entries[0].canonical_bytes, b"abcd");
        assert!(one.more);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn recovery_lease_never_shares_live_source_or_another_recovery_lease() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 64).unwrap();
        let reservation = ledger.reserve(&scope(), 32).unwrap();
        ledger
            .stage(&reservation.reservation_id, "native-owner", b"terminal")
            .unwrap();

        // A source still owns the staged terminal, even though this recovery
        // controller is in the same process and uses the same ledger object.
        assert!(matches!(
            ledger.recoverable_page_for_lease("scan", "native-owner", "recovery-a", 1, 32),
            Err(TerminalEvidenceError::Busy)
        ));
        assert_eq!(
            ledger.abandon(&reservation.reservation_id).unwrap(),
            TerminalEvidenceAbandonment::RetainedForRecovery
        );

        let page = ledger
            .recoverable_page_for_lease("scan", "native-owner", "recovery-a", 1, 32)
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert!(matches!(
            ledger.recoverable_page_for_lease("scan", "native-owner", "recovery-b", 1, 32),
            Err(TerminalEvidenceError::Busy)
        ));
        let entry = &page.entries[0];
        let committed = ledger
            .commit_recovered(
                "recovery-a",
                &entry.reservation_id,
                &entry.canonical_bytes,
                Sha256::digest(&entry.canonical_bytes).into(),
            )
            .unwrap();
        ledger
            .acknowledge_recovered(
                "recovery-a",
                &entry.reservation_id,
                &committed.reference,
                "scan",
                "native-owner",
            )
            .unwrap();
        assert!(
            ledger
                .recoverable_page_for_lease("scan", "native-owner", "recovery-a", 1, 32)
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn recovery_lease_release_keeps_rows_for_a_later_controller() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 64).unwrap();
        let reservation = ledger.reserve(&scope(), 32).unwrap();
        ledger
            .stage(&reservation.reservation_id, "native-owner", b"terminal")
            .unwrap();
        assert_eq!(
            ledger.abandon(&reservation.reservation_id).unwrap(),
            TerminalEvidenceAbandonment::RetainedForRecovery
        );
        assert_eq!(
            ledger
                .recoverable_page_for_lease("scan", "native-owner", "recovery-a", 1, 32)
                .unwrap()
                .entries
                .len(),
            1
        );
        ledger.release_recovery_lease("recovery-a").unwrap();
        assert_eq!(
            ledger
                .recoverable_page_for_lease("scan", "native-owner", "recovery-b", 1, 32)
                .unwrap()
                .entries
                .len(),
            1
        );
        // A delayed second close from the former controller must only match
        // its own lease. It cannot remove recovery-b's replacement lock.
        ledger.release_recovery_lease("recovery-a").unwrap();
        assert!(matches!(
            ledger.recoverable_page_for_lease("scan", "native-owner", "recovery-c", 1, 32),
            Err(TerminalEvidenceError::Busy)
        ));
    }

    #[test]
    fn contention_page_returns_cold_rows_and_reports_live_source_then_retries() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 64).unwrap();
        let live = ledger.reserve(&scope(), 32).unwrap();
        ledger
            .stage(&live.reservation_id, "native-owner", b"live")
            .unwrap();
        let mut cold_scope = scope();
        cold_scope.operation_id = "cold".into();
        let cold = ledger.reserve(&cold_scope, 32).unwrap();
        ledger
            .stage(&cold.reservation_id, "native-owner", b"cold")
            .unwrap();
        assert_eq!(
            ledger.abandon(&cold.reservation_id).unwrap(),
            TerminalEvidenceAbandonment::RetainedForRecovery
        );

        // Strict callers preserve their old all-or-nothing behavior.
        assert!(matches!(
            ledger.recoverable_page_for_lease("scan", "native-owner", "recovery", 8, 64),
            Err(TerminalEvidenceError::Busy)
        ));
        let page = ledger
            .recoverable_page_for_lease_with_contention("scan", "native-owner", "recovery", 8, 64)
            .unwrap();
        assert!(page.busy);
        assert!(!page.more);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].reservation_id, cold.reservation_id);

        assert_eq!(
            ledger.abandon(&live.reservation_id).unwrap(),
            TerminalEvidenceAbandonment::RetainedForRecovery
        );
        let retry = ledger
            .recoverable_page_for_lease_with_contention("scan", "native-owner", "recovery", 8, 64)
            .unwrap();
        assert!(!retry.busy);
        assert!(!retry.more);
        assert_eq!(retry.entries.len(), 2);
    }

    #[test]
    fn scope_identity_is_length_delimited_and_includes_producer() {
        let left = TerminalEvidenceScope {
            producer: "scan".into(),
            manager_id: "a\0b".into(),
            operation_id: "c".into(),
            incarnation: "d".into(),
        };
        let right = TerminalEvidenceScope {
            producer: "scan".into(),
            manager_id: "a".into(),
            operation_id: "b\0c".into(),
            incarnation: "d".into(),
        };
        let other_producer = TerminalEvidenceScope {
            producer: "subscription".into(),
            ..left.clone()
        };
        assert_ne!(terminal_id(&left), terminal_id(&right));
        assert_ne!(terminal_id(&left), terminal_id(&other_producer));
    }
    #[test]
    fn commit_reopens_and_digest_conflicts_fence() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 1024).unwrap();
        let reservation = ledger.reserve(&scope(), 32).unwrap();
        let bytes = b"terminal";
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        ledger
            .stage(&reservation.reservation_id, "test-owner", bytes)
            .unwrap();
        let committed = ledger
            .commit(&reservation.reservation_id, bytes, digest)
            .unwrap();
        drop(ledger);
        let reopened = TerminalEvidenceLedger::open(&path, 1024).unwrap();
        assert_eq!(reopened.read(&committed.reference, 32).unwrap().0, bytes);
        assert!(matches!(
            reopened.commit(
                &reservation.reservation_id,
                b"other",
                Sha256::digest(b"other").into()
            ),
            Err(TerminalEvidenceError::Conflict)
        ));
        reopened
            .acknowledge(&committed.reference, "scan", "test-owner")
            .unwrap();
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn abandoning_a_committed_runtime_lease_retains_bytes_and_quota_until_recovery_ack() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 8).unwrap();
        let reservation = ledger.reserve(&scope(), 8).unwrap();
        let bytes = b"terminal";
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        ledger
            .stage(&reservation.reservation_id, "test-owner", bytes)
            .unwrap();
        let committed = ledger
            .commit(&reservation.reservation_id, bytes, digest)
            .unwrap();

        assert_eq!(
            ledger.abandon(&reservation.reservation_id).unwrap(),
            TerminalEvidenceAbandonment::RetainedForRecovery
        );
        drop(ledger);

        let reopened = TerminalEvidenceLedger::open(&path, 8).unwrap();
        assert_eq!(reopened.read(&committed.reference, 8).unwrap().0, bytes);
        let second = TerminalEvidenceScope {
            operation_id: "subscription".into(),
            ..scope()
        };
        assert!(matches!(
            reopened.reserve(&second, 1),
            Err(TerminalEvidenceError::Quota)
        ));

        reopened
            .acknowledge(&committed.reference, "scan", "test-owner")
            .unwrap();
        reopened.reserve(&second, 1).unwrap();
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn reservation_consumes_quota_across_restart() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 8).unwrap();
        ledger.reserve(&scope(), 8).unwrap();
        drop(ledger);
        let reopened = TerminalEvidenceLedger::open(&path, 8).unwrap();
        let second = TerminalEvidenceScope {
            operation_id: "subscription".into(),
            ..scope()
        };
        assert!(matches!(
            reopened.reserve(&second, 1),
            Err(TerminalEvidenceError::Quota)
        ));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn reservation_bound_is_available_before_commit_and_not_after_release() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 64).unwrap();
        let reservation = ledger.reserve(&scope(), 17).unwrap();
        assert_eq!(
            ledger
                .reservation_maximum_bytes(&reservation.reservation_id)
                .unwrap(),
            17
        );
        assert_eq!(
            ledger.abandon(&reservation.reservation_id).unwrap(),
            TerminalEvidenceAbandonment::ReleasedUnusedReservation
        );
        assert!(matches!(
            ledger.reservation_maximum_bytes(&reservation.reservation_id),
            Err(TerminalEvidenceError::Missing)
        ));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn recoverable_rows_are_bound_to_the_staged_recipient_before_and_after_commit() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 64).unwrap();
        let reservation = ledger.reserve(&scope(), 32).unwrap();
        ledger
            .stage(&reservation.reservation_id, "native-owner", b"terminal")
            .unwrap();
        assert_eq!(ledger.recoverable("scan", "other-owner").unwrap().len(), 0);
        let staged = ledger.recoverable("scan", "native-owner").unwrap();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].state, TerminalEvidenceRecoveryState::Staged);
        assert_eq!(staged[0].canonical_bytes, b"terminal");
        let digest: [u8; 32] = Sha256::digest(b"terminal").into();
        ledger
            .commit(&reservation.reservation_id, b"terminal", digest)
            .unwrap();
        let committed = ledger.recoverable("scan", "native-owner").unwrap();
        assert_eq!(committed[0].state, TerminalEvidenceRecoveryState::Committed);
        assert_eq!(committed[0].digest, Some(digest));
        ledger
            .acknowledge(
                &committed[0].reference.clone().unwrap(),
                "scan",
                "native-owner",
            )
            .unwrap();
        assert!(
            ledger
                .recoverable("scan", "native-owner")
                .unwrap()
                .is_empty()
        );
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn commit_fences_staged_bytes_and_retains_one_canonical_copy() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 8).unwrap();
        let reservation = ledger.reserve(&scope(), 8).unwrap();
        ledger
            .stage(&reservation.reservation_id, "native-owner", b"terminal")
            .unwrap();
        assert!(matches!(
            ledger.commit(
                &reservation.reservation_id,
                b"conflict",
                Sha256::digest(b"conflict").into()
            ),
            Err(TerminalEvidenceError::Conflict)
        ));
        let committed = ledger
            .commit(
                &reservation.reservation_id,
                b"terminal",
                Sha256::digest(b"terminal").into(),
            )
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        let copies: (i64, i64) = connection.query_row(
            "SELECT bytes IS NULL, length(staged_bytes) FROM terminal_evidence_reservations WHERE reference=?1",
            [&committed.reference],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(copies, (1, 8));
        let other = TerminalEvidenceScope {
            operation_id: "other".into(),
            ..scope()
        };
        assert!(matches!(
            ledger.reserve(&other, 1),
            Err(TerminalEvidenceError::Quota)
        ));
        ledger
            .acknowledge(&committed.reference, "scan", "native-owner")
            .unwrap();
        ledger.reserve(&other, 1).unwrap();
        drop(connection);
        drop(ledger);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn authenticated_acknowledgement_is_idempotent_after_pruning_and_restart() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 64).unwrap();
        let recipient = "native-owner";
        let first_scope = scope();
        let first = ledger.reserve(&first_scope, 1).unwrap();
        ledger
            .stage(&first.reservation_id, recipient, b"x")
            .unwrap();
        let first = ledger
            .commit(&first.reservation_id, b"x", Sha256::digest(b"x").into())
            .unwrap();
        ledger
            .acknowledge(&first.reference, "scan", recipient)
            .unwrap();
        for index in 0..300 {
            let next_scope = TerminalEvidenceScope {
                operation_id: format!("later-{index}"),
                ..scope()
            };
            let reservation = ledger.reserve(&next_scope, 1).unwrap();
            ledger
                .stage(&reservation.reservation_id, recipient, b"x")
                .unwrap();
            let committed = ledger
                .commit(
                    &reservation.reservation_id,
                    b"x",
                    Sha256::digest(b"x").into(),
                )
                .unwrap();
            ledger
                .acknowledge(&committed.reference, "scan", recipient)
                .unwrap();
        }
        drop(ledger);
        let reopened = TerminalEvidenceLedger::open(&path, 64).unwrap();
        reopened
            .acknowledge(&first.reference, "scan", recipient)
            .unwrap();
        assert!(matches!(
            reopened.read(&first.reference, 1),
            Err(TerminalEvidenceError::Missing)
        ));
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn acknowledgement_is_noop_for_missing_ref_but_preserves_live_other_owner() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 8).unwrap();
        let reservation = ledger.reserve(&scope(), 1).unwrap();
        ledger
            .stage(&reservation.reservation_id, "native-owner", b"x")
            .unwrap();
        let committed = ledger
            .commit(
                &reservation.reservation_id,
                b"x",
                Sha256::digest(b"x").into(),
            )
            .unwrap();
        let mut forged = committed.reference.clone();
        let replacement = if forged.ends_with('0') { "1" } else { "0" };
        forged.replace_range(forged.len() - 1.., replacement);
        ledger.acknowledge(&forged, "scan", "native-owner").unwrap();
        assert!(matches!(
            ledger.acknowledge(&committed.reference, "scan", "other-owner"),
            Err(TerminalEvidenceError::Missing)
        ));
        assert_eq!(ledger.read(&committed.reference, 1).unwrap().0, b"x");
        ledger
            .acknowledge(&committed.reference, "scan", "native-owner")
            .unwrap();
        ledger
            .acknowledge(&committed.reference, "subscription", "native-owner")
            .unwrap();
        drop(ledger);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn acknowledgement_rejects_malformed_reference_and_empty_producer_without_panicking() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 8).unwrap();
        let unicode = format!("te1-{}", "é".repeat(24));
        assert!(matches!(
            ledger.acknowledge(&unicode, "scan", "owner"),
            Err(TerminalEvidenceError::Invalid(_))
        ));
        assert!(matches!(
            ledger.acknowledge(
                "te1-000000000000000000000000000000000000000000000000",
                "",
                "owner"
            ),
            Err(TerminalEvidenceError::Invalid(_))
        ));
        drop(ledger);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn recovery_rejects_corrupt_format_version_and_retained_byte_bound() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 8).unwrap();
        let reservation = ledger.reserve(&scope(), 1).unwrap();
        ledger
            .stage(&reservation.reservation_id, "native-owner", b"x")
            .unwrap();
        let committed = ledger
            .commit(
                &reservation.reservation_id,
                b"x",
                Sha256::digest(b"x").into(),
            )
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE terminal_evidence_reservations SET format_version=2 WHERE reference=?1",
                [&committed.reference],
            )
            .unwrap();
        assert!(matches!(
            ledger.recoverable("scan", "native-owner"),
            Err(TerminalEvidenceError::Corrupt)
        ));
        connection
            .execute(
                "UPDATE terminal_evidence_reservations SET format_version=1,staged_bytes=x'78',reference=NULL WHERE id=?1",
                [&reservation.reservation_id],
            )
            .unwrap();
        assert!(matches!(
            ledger.recoverable("scan", "native-owner"),
            Err(TerminalEvidenceError::Corrupt)
        ));
        connection.execute("UPDATE terminal_evidence_reservations SET reference=?1,staged_bytes=zeroblob(2) WHERE id=?2", params![committed.reference, reservation.reservation_id]).unwrap();
        assert!(matches!(
            ledger.recoverable("scan", "native-owner"),
            Err(TerminalEvidenceError::Corrupt)
        ));
        drop(connection);
        drop(ledger);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn reopening_with_a_different_aggregate_quota_is_fenced() {
        let path = path();
        let ledger = TerminalEvidenceLedger::open(&path, 8).unwrap();
        drop(ledger);
        assert!(matches!(
            TerminalEvidenceLedger::open(&path, 9),
            Err(TerminalEvidenceError::Conflict)
        ));
        std::fs::remove_file(path).unwrap();
    }
}
