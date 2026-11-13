//! Opt-in durable continuation journal. Plaintext: the host owns private-path
//! permissions, backup exclusion and OS file protection; this is NOT encryption.
//! Every accepted record and prepared/acknowledged cursor is transactionally
//! committed. Radio release and process shutdown never acknowledge records.
//!
//! SQLite DELETE rollback + synchronous EXTRA syncs the directory on commit.
//! cache_spill=OFF prevents extra rollback headers during cache spills. We reserve
//! a full second database image (8 bytes/page rollback overhead) plus 64KiB for
//! the journal header/sector padding; temp_store=MEMORY prevents disk sort files.
//! max_page_count bounds the DB, including freelist pages. The quota describes
//! logical file lengths, not filesystem allocation blocks or external backups.
//! Evidence membership, evidence claim bytes and recording identity digests use that quota;
//! snapshot start or preparation can fail with `storage.full` atomically, and
//! smaller claim pages can be retried. Membership size grows with snapshot rows.
//! Recording claim digests verify content and membership; they are not a body backup.
//! Pre-v5 active cursors lack v5 identity. Migration refuses them before
//! changing the schema; the compatibility reader can finish a pending recording
//! page when no legacy evidence cursor also owns its rows.
//! References: sqlite.org/fileformat2.html#the_rollback_journal, pragma.html.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::central::{
    DurablePeerFailureRecoveryOccurrence, PeerFailureRecoveryLedger,
    durable_peer_failure_recovery_occurrence_decode,
    durable_peer_failure_recovery_occurrence_encode,
};
use crate::errors::{DesktopError, PlatformDetail};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

const PAGE: u64 = 4096;
const OVERHEAD: u64 = 65536;
const RESERVED_PAGES: u64 = 16;
// Maximum pending ordered payload plus SQLite B-tree overhead.
const ORDERED_RESERVED_PAGES: u64 = 48;
const APPLICATION_ID: i64 = 0x55424d4a;
// V8 preserves the first summary before projection updates. Older readers reject it. Active
// pre-v5 cursors still require their original reader before migration.
const SCHEMA_VERSION: i64 = 11;
const SUMMARY_SNAPSHOT_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS diagnostic_summary_original(id INTEGER PRIMARY KEY CHECK(id=1), payload BLOB NOT NULL);";
const SEPARATE_LOSS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS diagnostic_separate_loss(id INTEGER PRIMARY KEY CHECK(id=1), items INTEGER NOT NULL, bytes INTEGER, failure TEXT NOT NULL);";
const IDENTITY_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS evidence_members(ordinal INTEGER PRIMARY KEY); CREATE TABLE IF NOT EXISTS evidence_members_header(id INTEGER PRIMARY KEY CHECK(id=1), transfer_id TEXT NOT NULL, cutoff INTEGER NOT NULL, progressed INTEGER NOT NULL, count INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS recording_claim_records(ordinal INTEGER PRIMARY KEY, digest BLOB NOT NULL, bytes INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS recording_claim_header(id INTEGER PRIMARY KEY CHECK(id=1), token TEXT NOT NULL, last INTEGER NOT NULL, more INTEGER NOT NULL, count INTEGER NOT NULL);";
const EVIDENCE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS evidence_cursor(id INTEGER PRIMARY KEY CHECK(id=1), transfer_id TEXT, cutoff INTEGER NOT NULL, progressed INTEGER NOT NULL, pending_token TEXT, pending_last INTEGER, pending_more INTEGER, next_token INTEGER NOT NULL, last_ack_token TEXT, last_ack_receipt TEXT, last_rollback_token TEXT); INSERT OR IGNORE INTO evidence_cursor(id,transfer_id,cutoff,progressed,pending_token,pending_last,pending_more,next_token,last_ack_token,last_ack_receipt,last_rollback_token) VALUES(1,NULL,0,0,NULL,NULL,NULL,1,NULL,NULL,NULL);";
const EVIDENCE_CLAIM_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS evidence_claim_records(ordinal INTEGER PRIMARY KEY, body BLOB NOT NULL, bytes INTEGER NOT NULL);";
const EVIDENCE_CLAIM_HEADER: &str = "CREATE TABLE IF NOT EXISTS evidence_claim_header(id INTEGER PRIMARY KEY CHECK(id=1), transfer_id TEXT NOT NULL, token TEXT NOT NULL, progressed INTEGER NOT NULL, cutoff INTEGER NOT NULL, last INTEGER NOT NULL, more INTEGER NOT NULL, count INTEGER NOT NULL);";
const ORDERED_PENDING_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS ordered_pending(id INTEGER PRIMARY KEY CHECK(id=1), producer_id TEXT NOT NULL, collection_id TEXT NOT NULL, generation_kind TEXT NOT NULL, generation_value TEXT NOT NULL, delivery_id TEXT NOT NULL, payload TEXT NOT NULL);";
const ORDERED_OWNER_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS ordered_owner(id INTEGER PRIMARY KEY CHECK(id=1), producer_id TEXT NOT NULL, collection_id TEXT NOT NULL, generation_kind TEXT, generation_value TEXT, state TEXT NOT NULL CHECK(state IN ('live','sealed','interrupted')));";
const ORDERED_HOST_CLEANUP_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS ordered_host_cleanup(id INTEGER PRIMARY KEY CHECK(id=1), producer_id TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('uncertain','released','not-applicable')));";
const ORDERED_IDENTITIES_SCHEMA: &str = "CREATE TABLE ordered_identities(producer_id TEXT NOT NULL, collection_id TEXT NOT NULL, generation_kind TEXT NOT NULL, generation_value TEXT NOT NULL, delivery_id TEXT NOT NULL, payload TEXT NOT NULL, ordinal INTEGER NOT NULL, PRIMARY KEY(producer_id,collection_id,generation_kind,generation_value,delivery_id)) WITHOUT ROWID;";
// Central owns the public descriptor and recipient-lease semantics. This
// table owns only the crash-safe handoff of its canonical envelope together
// with exact producer evidence and the irreversible source-ACK phase.
const PEER_FAILURE_RECOVERY_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS peer_failure_recoveries(identity BLOB PRIMARY KEY, occurrence TEXT NOT NULL, evidence BLOB NOT NULL, digest BLOB NOT NULL CHECK(octet_length(digest)=32), state TEXT NOT NULL CHECK(state IN ('staged','source-ack-inflight','source-ack-committed')));";

// This sidecar is deliberately independent of a recording's primary schema.
// Released v1 readers use positional INSERTs into `records`, so putting a
// recovery cursor or a chunk table in that database would make a mixed-version
// handoff unsafe. The recording authority lock is still held by the owning
// `ContinuationJournal`; this sidecar is only opened through that authority.
const PEER_FAILURE_SIDECAR_APPLICATION_ID: i64 = 0x55425046;
const PEER_FAILURE_SIDECAR_SCHEMA_VERSION: i64 = 1;
const PEER_FAILURE_SIDECAR_SCHEMA: &str = "
CREATE TABLE occurrences(
  identity BLOB PRIMARY KEY CHECK(octet_length(identity)=96),
  occurrence TEXT NOT NULL,
  expected_bytes INTEGER NOT NULL CHECK(expected_bytes>0),
  staged_bytes INTEGER NOT NULL CHECK(staged_bytes>=0),
  digest BLOB CHECK(digest IS NULL OR octet_length(digest)=32),
  state TEXT NOT NULL CHECK(state IN ('staging','staged','source-ack-inflight','source-ack-committed')),
  source_ack_id TEXT
);
CREATE TABLE chunks(
  identity BLOB NOT NULL REFERENCES occurrences(identity) ON DELETE CASCADE,
  offset INTEGER NOT NULL CHECK(offset>=0),
  bytes BLOB NOT NULL CHECK(octet_length(bytes)>0),
  PRIMARY KEY(identity,offset)
) WITHOUT ROWID;";
// A scan failure is captured before the outbox consumes its source-time cut.
// This sidecar deliberately keeps raw tokens and recipient identity separate
// from the released primary recording schema.
const SCAN_FAILURE_SIDECAR_APPLICATION_ID: i64 = 0x55425346;
// Version two keeps the v1 raw source tables byte-for-byte readable, and adds
// an opaque, versioned owner envelope.  A v1 database is deliberately not
// migrated in place: it has no ordinal, limits, capability, or receipt state,
// so inventing those values would turn an exact recovery boundary into a
// plausible-looking new occurrence.
const SCAN_FAILURE_SIDECAR_SCHEMA_VERSION: i64 = 2;
const SCAN_FAILURE_CLEANUP_RECEIPT_INDEX_NAME: &str = "scan_failure_cleanup_receipt_by_envelope";
const SCAN_FAILURE_CLEANUP_RECEIPT_INDEX_SCHEMA: &str = "CREATE UNIQUE INDEX scan_failure_cleanup_receipt_by_envelope ON occurrences(envelope) WHERE envelope IS NOT NULL;";
const SCAN_FAILURE_SIDECAR_SCHEMA: &str = "
CREATE TABLE occurrences(identity BLOB PRIMARY KEY CHECK(octet_length(identity)=80), envelope BLOB);
CREATE UNIQUE INDEX scan_failure_cleanup_receipt_by_envelope ON occurrences(envelope) WHERE envelope IS NOT NULL;
CREATE TABLE members(identity BLOB NOT NULL REFERENCES occurrences(identity) ON DELETE CASCADE, position INTEGER NOT NULL, recipient BLOB NOT NULL CHECK(octet_length(recipient)=16), delivery BLOB NOT NULL CHECK(octet_length(delivery)=16), acknowledged INTEGER NOT NULL DEFAULT 0 CHECK(acknowledged IN (0,1)), PRIMARY KEY(identity,position)) WITHOUT ROWID;
CREATE TABLE tokens(identity BLOB NOT NULL REFERENCES occurrences(identity) ON DELETE CASCADE, position INTEGER NOT NULL, tag INTEGER NOT NULL, payload BLOB NOT NULL, PRIMARY KEY(identity,position)) WITHOUT ROWID;
-- The envelope is written only after the source-time cut has committed.  It
-- is an outbox-owned wire object, rather than a reconstructed JSON summary.
CREATE TABLE owner_envelopes(identity BLOB PRIMARY KEY REFERENCES occurrences(identity) ON DELETE CASCADE, version INTEGER NOT NULL CHECK(version=2), body BLOB NOT NULL) WITHOUT ROWID;";
// This recording-level authority remains after the last owner envelope moves
// into a cleanup receipt.  It is separate from the cleanup MAC key: it names
// the authenticated scan-owner domain which is needed to reopen and retire
// that receipt, and is removed once neither owners nor cleanup receipts remain.
const SCAN_FAILURE_RECORDING_AUTHORITY_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS recording_recovery_authority(
  id INTEGER PRIMARY KEY CHECK(id=1),
  session_id BLOB NOT NULL CHECK(octet_length(session_id)=8),
  authority BLOB NOT NULL CHECK(octet_length(authority)=32)
);";
// This key signs only opaque cleanup-retirement capabilities.  It is never
// returned by the journal and remains in the owner-private sidecar, so a
// caller that lost a successful retirement reply can prove that exact prior
// cleanup without retaining every retired receipt forever.
const SCAN_FAILURE_CLEANUP_AUTHORITY_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS cleanup_authority(
  id INTEGER PRIMARY KEY CHECK(id=1),
  key BLOB NOT NULL CHECK(octet_length(key)=32)
);";

fn ensure_scan_failure_cleanup_receipt_index(connection: &mut Connection) -> Result<()> {
    let existing: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name=?1",
            [SCAN_FAILURE_CLEANUP_RECEIPT_INDEX_NAME],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if normalized_schema_sql(&existing)
            != normalized_schema_sql(SCAN_FAILURE_CLEANUP_RECEIPT_INDEX_SCHEMA)
        {
            return Err(error(
                "storage.schema",
                "scan failure cleanup receipt index schema is not recognized",
            ));
        }
        return Ok(());
    }

    // This is a strictly additive v2 repair.  A duplicate pre-existing
    // envelope means the prior sidecar cannot state which occurrence owns an
    // opaque cleanup receipt, so it is corrupt rather than safe to rewrite.
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCAN_FAILURE_CLEANUP_RECEIPT_INDEX_SCHEMA)
        .map_err(|failure| match failure.sqlite_error_code() {
            Some(rusqlite::ErrorCode::ConstraintViolation) => error(
                "storage.corrupt",
                "scan failure cleanup receipt index conflicts with existing envelopes",
            ),
            _ => failure.into(),
        })?;
    tx.commit()?;
    Ok(())
}

// Apple notification-source failure overflow is a distinct, explicitly
// configured handoff.  It is deliberately not a primary-recording row: the
// source callback has no recording declaration and released readers must keep
// opening their primary journal unchanged.
const APPLE_FAILURE_SPOOL_APPLICATION_ID: i64 = 0x55424146;
const APPLE_FAILURE_SPOOL_SCHEMA_VERSION: i64 = 10;
const APPLE_FAILURE_SPOOL_SCHEMA: &str = "
CREATE TABLE occurrences(
  identity BLOB PRIMARY KEY CHECK(octet_length(identity)=32),
  route TEXT NOT NULL,
  epoch INTEGER NOT NULL,
  sequence INTEGER NOT NULL,
  occurrence TEXT NOT NULL,
  bytes INTEGER NOT NULL CHECK(bytes>0),
  UNIQUE(route, epoch, sequence)
) WITHOUT ROWID;";
const APPLE_FAILURE_SPOOL_WATERMARK_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS watermarks(
  route TEXT NOT NULL,
  epoch INTEGER NOT NULL,
  highest_sequence INTEGER NOT NULL CHECK(highest_sequence>=0),
  PRIMARY KEY(route, epoch)
) WITHOUT ROWID;";
const APPLE_FAILURE_SPOOL_RESERVATION_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS reservations(
  route TEXT NOT NULL,
  epoch INTEGER NOT NULL,
  sequence INTEGER NOT NULL,
  identity BLOB NOT NULL CHECK(octet_length(identity)=32),
  occurrence TEXT NOT NULL,
  bytes INTEGER NOT NULL CHECK(bytes>0),
  PRIMARY KEY(route, epoch)
) WITHOUT ROWID;";
// Versions 2 and 3 predate the complete reservation envelope.  Do not use
// the current DDL to validate them: the migration below is the only owner of
// adding these two columns.
const APPLE_FAILURE_SPOOL_RESERVATION_V2_SCHEMA: &str = "
CREATE TABLE reservations(
  route TEXT NOT NULL,
  epoch INTEGER NOT NULL,
  sequence INTEGER NOT NULL,
  identity BLOB NOT NULL CHECK(octet_length(identity)=32),
  PRIMARY KEY(route, epoch)
) WITHOUT ROWID;";
const APPLE_FAILURE_SPOOL_SETTINGS_SCHEMA: &str = "
CREATE TABLE settings(
  id INTEGER PRIMARY KEY CHECK(id=1),
  max_bytes INTEGER NOT NULL CHECK(max_bytes>0),
  max_records INTEGER NOT NULL CHECK(max_records>0),
  retirement_secret BLOB NOT NULL CHECK(octet_length(retirement_secret)=32)
);";
const APPLE_FAILURE_SPOOL_SETTINGS_V3_SCHEMA: &str = "
CREATE TABLE settings(
  id INTEGER PRIMARY KEY CHECK(id=1),
  max_bytes INTEGER NOT NULL CHECK(max_bytes>0),
  max_records INTEGER NOT NULL CHECK(max_records>0)
);";
const APPLE_FAILURE_SPOOL_GLOBAL_STREAM_SCHEMA: &str = "
CREATE TABLE capture_stream(
  capture_ordinal INTEGER PRIMARY KEY AUTOINCREMENT,
  identity BLOB NOT NULL UNIQUE CHECK(octet_length(identity)=32)
);";
const APPLE_FAILURE_SPOOL_CAPTURE_WATERMARK_SCHEMA: &str = "
CREATE TABLE capture_watermark(
  id INTEGER PRIMARY KEY CHECK(id=1),
  highest_ordinal INTEGER NOT NULL CHECK(highest_ordinal>=0)
);
INSERT INTO capture_watermark(id,highest_ordinal) VALUES(1,0);";
// ACK replies can be lost independently. Receipts are never overwritten by a
// later ACK. SQLite's configured max page count bounds the durable receipt
// set; a full database refuses the next ACK atomically rather than evicting
// proof.
const APPLE_FAILURE_SPOOL_ACKNOWLEDGEMENTS_SCHEMA: &str = "
CREATE TABLE acknowledgements(
  capture_ordinal INTEGER PRIMARY KEY CHECK(capture_ordinal>0),
  identity BLOB NOT NULL CHECK(octet_length(identity)=32),
  retired INTEGER NOT NULL DEFAULT 0 CHECK(retired IN (0,1)),
  occurrence TEXT,
  v4_capability_issued INTEGER NOT NULL DEFAULT 0 CHECK(v4_capability_issued IN (0,1))
);";
const APPLE_FAILURE_SPOOL_ACKNOWLEDGEMENTS_V9_SCHEMA: &str = "
CREATE TABLE acknowledgements(
  capture_ordinal INTEGER PRIMARY KEY CHECK(capture_ordinal>0),
  identity BLOB NOT NULL CHECK(octet_length(identity)=32),
  retired INTEGER NOT NULL DEFAULT 0 CHECK(retired IN (0,1))
);";
const APPLE_FAILURE_SPOOL_ACKNOWLEDGEMENTS_V7_SCHEMA: &str = "
CREATE TABLE acknowledgements(
  capture_ordinal INTEGER PRIMARY KEY CHECK(capture_ordinal>0),
  identity BLOB NOT NULL CHECK(octet_length(identity)=32)
);";

// Isolated building block only: this schema is deliberately NOT installed by
// open/migration. Enabling it needs a new schema and negotiated protocol plus
// an ingress sequencer, recovery, cursor/ACK, stop and clear integration. A v5
// reader must never be allowed to open a database carrying these tables.
#[allow(dead_code)]
const EXPERIMENTAL_SCAN_FENCE_SCHEMA: &str = "
    CREATE TABLE scan_fences(identity BLOB PRIMARY KEY, ordinal INTEGER NOT NULL UNIQUE, member_count INTEGER NOT NULL, token_count INTEGER NOT NULL);
    CREATE TABLE scan_fence_members(identity BLOB NOT NULL, position INTEGER NOT NULL, recipient BLOB NOT NULL, delivery BLOB NOT NULL, PRIMARY KEY(identity,position));
    CREATE TABLE scan_fence_tokens(identity BLOB NOT NULL, position INTEGER NOT NULL, tag INTEGER NOT NULL, payload BLOB NOT NULL, PRIMARY KEY(identity,position));";

// This is not an admission API. It is one component of a future IMMEDIATE
// transaction, after the producer has acquired a shared ingress cut. The
// caller MUST roll back the enclosing transaction on every error and retain
// the original occurrence until commit is confirmed or an exact retry succeeds.
// The row set stores even malformed flat sources, without whole-source copying.
#[allow(dead_code)]
fn capture_scan_fence(
    tx: &mut rusqlite::Transaction<'_>,
    occurrence: &ubm_core::oversized_evidence::ScanFailureOccurrence,
) -> Result<i64> {
    let savepoint = tx.savepoint()?;
    let ordinal = capture_scan_fence_inner(&savepoint, occurrence)?;
    savepoint.commit()?;
    Ok(ordinal)
}

#[allow(dead_code)]
fn capture_scan_fence_inner(
    tx: &Connection,
    occurrence: &ubm_core::oversized_evidence::ScanFailureOccurrence,
) -> Result<i64> {
    use std::borrow::Cow;
    use ubm_core::oversized_evidence::EvidenceToken;
    let source = occurrence
        .source_tokens()
        .ok_or_else(|| error("storage.identity", "failure source is no longer retained"))?;
    let id = occurrence.identity();
    let mut identity = Vec::with_capacity(80);
    for part in [
        &id.host,
        &id.scan,
        &id.start,
        &id.source_generation,
        &id.occurrence,
    ] {
        identity.extend_from_slice(part);
    }
    let members = occurrence.members();
    let member_count = i64::try_from(members.len())
        .map_err(|_| error("storage.full", "membership cannot be represented"))?;
    let token_count = i64::try_from(source.len())
        .map_err(|_| error("storage.full", "source cannot be represented"))?;
    fn token_parts(token: &EvidenceToken) -> (i64, Cow<'_, [u8]>) {
        match token {
            EvidenceToken::Null => (0, Cow::Borrowed(&[])),
            EvidenceToken::Bool(value) => (1, Cow::Owned(vec![u8::from(*value)])),
            EvidenceToken::I64(value) => (2, Cow::Owned(value.to_be_bytes().to_vec())),
            EvidenceToken::U64(value) => (3, Cow::Owned(value.to_be_bytes().to_vec())),
            EvidenceToken::F64Bits(value) => (4, Cow::Owned(value.to_be_bytes().to_vec())),
            EvidenceToken::Text(value) => (5, Cow::Borrowed(value.as_bytes())),
            EvidenceToken::Bytes(value) => (6, Cow::Borrowed(value)),
            EvidenceToken::Map(value) => (7, Cow::Owned(value.to_be_bytes().to_vec())),
            EvidenceToken::Array(value) => (8, Cow::Owned(value.to_be_bytes().to_vec())),
        }
    }
    let existing: Option<(i64, i64, i64)> = tx
        .query_row(
            "SELECT ordinal,member_count,token_count FROM scan_fences WHERE identity=?1",
            [&identity],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((ordinal, old_members, old_tokens)) = existing {
        if (old_members, old_tokens) != (member_count, token_count) {
            return Err(error(
                "storage.identity",
                "failure identity reused with different evidence or membership",
            ));
        }
        let mut rows = tx.prepare("SELECT position,recipient,delivery FROM scan_fence_members WHERE identity=?1 ORDER BY position")?;
        let mut rows = rows.query([&identity])?;
        for (position, member) in members.iter().enumerate() {
            let row = rows
                .next()?
                .ok_or_else(|| error("storage.corrupt", "failure membership is incomplete"))?;
            if row.get::<_, i64>(0)? != position as i64
                || row
                    .get_ref(1)?
                    .as_blob()
                    .map_err(|_| error("storage.corrupt", "invalid failure recipient"))?
                    != member.recipient
                || row
                    .get_ref(2)?
                    .as_blob()
                    .map_err(|_| error("storage.corrupt", "invalid failure delivery"))?
                    != member.delivery
            {
                return Err(error(
                    "storage.identity",
                    "failure identity reused with different membership",
                ));
            }
        }
        if rows.next()?.is_some() {
            return Err(error(
                "storage.corrupt",
                "failure membership has extra rows",
            ));
        }
        let mut rows = tx.prepare("SELECT position,tag,payload FROM scan_fence_tokens WHERE identity=?1 ORDER BY position")?;
        let mut rows = rows.query([&identity])?;
        for (position, token) in source.iter().enumerate() {
            let row = rows
                .next()?
                .ok_or_else(|| error("storage.corrupt", "failure source is incomplete"))?;
            let (tag, payload) = token_parts(token);
            if row.get::<_, i64>(0)? != position as i64
                || row.get::<_, i64>(1)? != tag
                || row
                    .get_ref(2)?
                    .as_blob()
                    .map_err(|_| error("storage.corrupt", "invalid failure token"))?
                    != payload.as_ref()
            {
                return Err(error(
                    "storage.identity",
                    "failure identity reused with different source",
                ));
            }
        }
        if rows.next()?.is_some() {
            return Err(error("storage.corrupt", "failure source has extra rows"));
        }
        return Ok(ordinal);
    }
    let (phase, ordinal): (String, i64) = tx.query_row(
        "SELECT phase,next_ordinal FROM journal WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if phase != "recording" {
        return Err(error(
            "storage.stopped",
            "journal is not accepting failure fences",
        ));
    }
    let next = ordinal
        .checked_add(1)
        .ok_or_else(|| error("storage.full", "journal ordinal exhausted"))?;
    tx.execute(
        "INSERT INTO scan_fences VALUES(?1,?2,?3,?4)",
        params![identity, ordinal, member_count, token_count],
    )?;
    for (position, member) in members.iter().enumerate() {
        tx.execute(
            "INSERT INTO scan_fence_members VALUES(?1,?2,?3,?4)",
            params![
                identity,
                position as i64,
                member.recipient.as_slice(),
                member.delivery.as_slice()
            ],
        )?;
    }
    for (position, token) in source.iter().enumerate() {
        let (tag, payload) = token_parts(token);
        tx.execute(
            "INSERT INTO scan_fence_tokens VALUES(?1,?2,?3,?4)",
            params![identity, position as i64, tag, payload.as_ref()],
        )?;
    }
    tx.execute("UPDATE journal SET next_ordinal=?1 WHERE id=1", [next])?;
    Ok(ordinal)
}
/// Most records one synced commit may carry. The pump never waits to fill a
/// batch: it groups only values it already holds, so this bounds how long one
/// commit can delay queued control work, not latency.
pub const APPEND_BATCH_MAX: usize = 32;

/// Trusted host controls share one blocking-pool boundary. Join failures expose
/// only a stable runtime identity, never a panic's potentially sensitive payload.
pub async fn run_blocking<F>(operation: F) -> crate::continuation::Result<Value>
where
    F: FnOnce() -> crate::continuation::Result<Value> + Send + 'static,
{
    run_blocking_result(operation).await
}

pub async fn run_blocking_result<T, F>(operation: F) -> crate::continuation::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> crate::continuation::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation).await.unwrap_or_else(|_|Err(json!({"code":"platform.failure","domain":"platform","operation":"continuation.recording","detail":"recording worker did not complete","platform":{"domain":"runtime","code":"task.join-failed","message":"recording worker did not complete","metadata":{}}})))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn scan_failure_identity_bytes(
    identity: ubm_core::oversized_evidence::ScanFailureIdentity,
) -> [u8; 80] {
    let mut bytes = [0_u8; 80];
    for (index, part) in [
        identity.host,
        identity.scan,
        identity.start,
        identity.source_generation,
        identity.occurrence,
    ]
    .iter()
    .enumerate()
    {
        let offset = index * 16;
        bytes[offset..offset + 16].copy_from_slice(part);
    }
    bytes
}

fn scan_failure_identity_from_bytes(
    bytes: &[u8],
) -> Result<ubm_core::oversized_evidence::ScanFailureIdentity> {
    if bytes.len() != 80 {
        return Err(error(
            "storage.corrupt",
            "scan failure identity has invalid length",
        ));
    }
    let part = |index: usize| {
        let mut value = [0_u8; 16];
        value.copy_from_slice(&bytes[index * 16..(index + 1) * 16]);
        value
    };
    Ok(ubm_core::oversized_evidence::ScanFailureIdentity {
        host: part(0),
        scan: part(1),
        start: part(2),
        source_generation: part(3),
        occurrence: part(4),
    })
}

/// One restartable v2 scan owner. The envelope remains opaque to the journal;
/// the identity is exposed separately so callers can request the bounded raw
/// evidence pages without decoding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DurableScanFailureOwnerEnvelope {
    pub(crate) identity: ubm_core::oversized_evidence::ScanFailureIdentity,
    pub(crate) body: Vec<u8>,
}

/// Exact raw cut used to rebuild a v2 owner.  This is intentionally internal:
/// public hosts page it through bounded methods instead of receiving an
/// unbounded allocation from a recovery sidecar.
#[derive(Debug)]
pub(crate) struct DurableScanFailureRawCut {
    pub(crate) identity: ubm_core::oversized_evidence::ScanFailureIdentity,
    pub(crate) members: Vec<ubm_core::oversized_evidence::ScanFailureMember>,
    pub(crate) tokens: Vec<ubm_core::oversized_evidence::EvidenceToken>,
}

#[derive(Default)]
struct RegistryState {
    directory: Option<PathBuf>,
    directory_identity: Option<(u64, u64)>,
    // This is installed only by a trusted native host before it publishes a
    // registry.  It deliberately is not representable by any IPC/FFI value:
    // a renderer must never be able to nominate the ACL authority protecting
    // bearer recovery material.
    scan_recovery_storage: Option<Arc<dyn ScanRecoveryStorageProtector>>,
}

/// Trusted-host protection for the secret-bearing v2 scan recovery sidecar.
///
/// The protected directory is the security boundary, not just the database
/// file: SQLite creates `-journal` beside the database before it can apply a
/// file ACL.  Hosts therefore protect and validate both the configured journal
/// directory and its dedicated scan-recovery child before SQLite sees a path.
/// Implementations must reject reparse points and validate the exact object.
pub trait ScanRecoveryStorageProtector: Send + Sync {
    /// Provision protection for a newly created, empty v2 sidecar.
    fn provision(&self, path: &Path) -> Result<()>;

    /// Verify protection remains present for the exact sidecar being opened.
    fn validate(&self, path: &Path) -> Result<()>;

    /// Provision a directory that will contain bearer material and SQLite
    /// auxiliary files.  The default preserves existing host implementations;
    /// a Windows host must override this to apply a directory DACL.
    fn provision_directory(&self, path: &Path) -> Result<()> {
        self.provision(path)
    }

    /// Validate a protected directory before it is used to resolve a SQLite
    /// database or one of its auxiliary files.
    fn validate_directory(&self, path: &Path) -> Result<()> {
        self.validate(path)
    }

    /// Stable host-owned capability label reported by configuration status.
    /// It is diagnostic only and never carries a key, ACL, or filesystem path.
    fn capability(&self) -> &'static str {
        "host-protected"
    }
}

/// Windows implementation for a v2 scan-recovery sidecar owned by the
/// current process user.
///
/// This protector never elevates: it reads the current process token, creates
/// one explicit full-control ACE for that user, marks that DACL protected, and
/// then reopens the final component with `FILE_FLAG_OPEN_REPARSE_POINT` to
/// verify the held file handle's identity and security descriptor. A default
/// DACL, inherited ACE, owner mismatch, reparse point, or extra ACE is a
/// failure. Hosts install it directly; it is intentionally not an IPC/FFI
/// configuration value.
#[cfg(windows)]
#[derive(Debug, Default)]
pub struct WindowsCurrentUserScanRecoveryStorageProtector;

#[cfg(windows)]
impl WindowsCurrentUserScanRecoveryStorageProtector {
    /// Construct the current-user, no-elevation Windows protector.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[cfg(windows)]
impl ScanRecoveryStorageProtector for WindowsCurrentUserScanRecoveryStorageProtector {
    fn provision(&self, path: &Path) -> Result<()> {
        windows_provision_scan_recovery_sidecar(path)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        windows_validate_scan_recovery_sidecar(path)
    }

    fn provision_directory(&self, path: &Path) -> Result<()> {
        windows_provision_scan_recovery_directory(path)
    }

    fn validate_directory(&self, path: &Path) -> Result<()> {
        windows_validate_scan_recovery_directory(path)
    }

    fn capability(&self) -> &'static str {
        "windows-current-user-protected-dacl"
    }
}

#[cfg(windows)]
struct WindowsHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for WindowsHandle {
    fn drop(&mut self) {
        // This only owns handles returned by OpenProcessToken, never the
        // pseudo-handle returned by GetCurrentProcess.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
struct WindowsLocalAllocation(*mut core::ffi::c_void);

#[cfg(windows)]
impl Drop for WindowsLocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.0);
            }
        }
    }
}

#[cfg(windows)]
struct WindowsSid(Vec<u8>);

#[cfg(windows)]
impl WindowsSid {
    fn as_psid(&self) -> windows_sys::Win32::Security::PSID {
        self.0.as_ptr().cast_mut().cast()
    }
}

#[cfg(windows)]
fn windows_storage_error(message: &'static str) -> JournalError {
    error("storage.permission", message)
}

#[cfg(windows)]
fn windows_current_user_sid() -> Result<WindowsSid> {
    use core::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, IsValidSid, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut raw_token: HANDLE = core::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0
        || raw_token.is_null()
    {
        return Err(windows_storage_error(
            "current Windows user identity cannot be inspected",
        ));
    }
    let token = WindowsHandle(raw_token);
    let mut required = 0_u32;
    // The sizing query is expected to return false. Its only usable output is
    // a non-zero required size; do not accept a zero-sized token record.
    unsafe {
        GetTokenInformation(token.0, TokenUser, core::ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(windows_storage_error(
            "current Windows user identity cannot be sized",
        ));
    }
    let required = usize::try_from(required).map_err(|_| {
        windows_storage_error("current Windows user identity exceeds addressable memory")
    })?;
    let words = required.div_ceil(core::mem::size_of::<usize>());
    let mut bytes = vec![MaybeUninit::<usize>::uninit(); words];
    let mut returned = 0_u32;
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            bytes.as_mut_ptr().cast(),
            u32::try_from(required).map_err(|_| {
                windows_storage_error("current Windows user identity exceeds API bounds")
            })?,
            &mut returned,
        )
    } == 0
        || usize::try_from(returned).ok() != Some(required)
    {
        return Err(windows_storage_error(
            "current Windows user identity cannot be read",
        ));
    }
    let user = unsafe { &*bytes.as_ptr().cast::<TOKEN_USER>() };
    if user.User.Sid.is_null() || unsafe { IsValidSid(user.User.Sid) } == 0 {
        return Err(windows_storage_error("current Windows user SID is invalid"));
    }
    let length = usize::try_from(unsafe { GetLengthSid(user.User.Sid) })
        .map_err(|_| windows_storage_error("current Windows user SID is too large"))?;
    if length == 0 {
        return Err(windows_storage_error("current Windows user SID is empty"));
    }
    let mut sid = vec![0_u8; length];
    unsafe {
        core::ptr::copy_nonoverlapping(user.User.Sid.cast::<u8>(), sid.as_mut_ptr(), length);
    }
    Ok(WindowsSid(sid))
}

#[cfg(windows)]
fn windows_open_exact_scan_recovery_sidecar(path: &Path) -> Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{WRITE_DAC, WRITE_OWNER};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        // Provisioning replaces the DACL after opening the exact final
        // component. Generic read/write does not request the security-descriptor
        // rights needed to install the private DACL and current-user owner.
        .access_mode(GENERIC_READ | GENERIC_WRITE | WRITE_DAC | WRITE_OWNER)
        // This is intentionally not a directory policy. The final component
        // itself is opened without following a reparse point before its ACL is
        // inspected.
        .custom_flags(0x0020_0000)
        .open(path)
        .map_err(|_| windows_storage_error("scan recovery sidecar cannot be opened securely"))?;
    let metadata = file
        .metadata()
        .map_err(|_| windows_storage_error("scan recovery sidecar cannot be inspected"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(windows_storage_error(
            "scan recovery sidecar is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn windows_open_exact_scan_recovery_directory(
    path: &Path,
    provisioning: bool,
) -> Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{WRITE_DAC, WRITE_OWNER};
    use windows_sys::Win32::System::SystemServices::MAXIMUM_ALLOWED;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        // See the sidecar opener: changing this object's DACL and owner
        // requires explicit security-descriptor rights on the held handle.
        // SetSecurityInfo does not propagate a directory DACL to existing
        // children when the handle used for provisioning requests
        // MAXIMUM_ALLOWED. The configured root can therefore be protected
        // without changing pre-existing, unrelated files beside a journal.
        .access_mode(if provisioning {
            MAXIMUM_ALLOWED
        } else {
            GENERIC_READ | GENERIC_WRITE | WRITE_DAC | WRITE_OWNER
        })
        // BACKUP_SEMANTICS is required for a directory handle; OPEN_REPARSE_POINT
        // makes a junction/symlink fail validation rather than redirecting it.
        .custom_flags(0x0220_0000)
        .open(path)
        .map_err(|_| windows_storage_error("scan recovery directory cannot be opened securely"))?;
    let metadata = file
        .metadata()
        .map_err(|_| windows_storage_error("scan recovery directory cannot be inspected"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(windows_storage_error(
            "scan recovery directory is not a real directory",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn windows_validate_held_scan_recovery_sidecar(
    file: &std::fs::File,
    expected_sid: &WindowsSid,
) -> Result<()> {
    windows_validate_held_scan_recovery_object(file, expected_sid, false)
}

#[cfg(windows)]
fn windows_validate_held_scan_recovery_object(
    file: &std::fs::File,
    expected_sid: &WindowsSid,
    expected_directory: bool,
) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
        GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
        IsValidSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PRESENT,
        SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, GetFileInformationByHandle,
    };
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    let handle = file.as_raw_handle().cast();
    let mut identity = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle, &mut identity) } == 0 {
        return Err(windows_storage_error(
            "scan recovery sidecar identity cannot be inspected",
        ));
    }
    if identity.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(windows_storage_error(
            "scan recovery sidecar must not be a reparse point",
        ));
    }
    let is_directory = identity.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if is_directory != expected_directory {
        return Err(windows_storage_error(
            "scan recovery object type changed while validating protection",
        ));
    }

    let mut owner = core::ptr::null_mut();
    let mut dacl: *mut ACL = core::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = core::ptr::null_mut();
    let result = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            core::ptr::null_mut(),
            &mut dacl,
            core::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 || descriptor.is_null() {
        return Err(windows_storage_error(
            "scan recovery sidecar ACL cannot be inspected",
        ));
    }
    let _descriptor = WindowsLocalAllocation(descriptor.cast());
    let mut descriptor_owner = core::ptr::null_mut();
    let mut owner_defaulted = 0;
    if unsafe {
        GetSecurityDescriptorOwner(descriptor, &mut descriptor_owner, &mut owner_defaulted)
    } == 0
        || descriptor_owner.is_null()
        || unsafe { IsValidSid(descriptor_owner) } == 0
        || unsafe { EqualSid(descriptor_owner, expected_sid.as_psid()) } == 0
    {
        return Err(windows_storage_error(
            "scan recovery sidecar owner is not the current Windows user",
        ));
    }
    // GetSecurityInfo returns the DACL separately, but this call proves the
    // descriptor actually contains one rather than treating a null DACL as a
    // private default (a null DACL grants everyone full access).
    let mut dacl_present = 0;
    let mut descriptor_dacl: *mut ACL = core::ptr::null_mut();
    let mut dacl_defaulted = 0;
    if unsafe {
        GetSecurityDescriptorDacl(
            descriptor,
            &mut dacl_present,
            &mut descriptor_dacl,
            &mut dacl_defaulted,
        )
    } == 0
        || dacl_present == 0
        || descriptor_dacl.is_null()
        || descriptor_dacl != dacl
    {
        return Err(windows_storage_error(
            "scan recovery sidecar does not have an explicit DACL",
        ));
    }
    let mut control = 0_u16;
    let mut revision = 0_u32;
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
        || control & (SE_DACL_PRESENT | SE_DACL_PROTECTED) != SE_DACL_PRESENT | SE_DACL_PROTECTED
    {
        return Err(windows_storage_error(
            "scan recovery sidecar DACL is not protected",
        ));
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    if unsafe {
        GetAclInformation(
            dacl,
            core::ptr::addr_of_mut!(size).cast(),
            u32::try_from(core::mem::size_of::<ACL_SIZE_INFORMATION>()).map_err(|_| {
                windows_storage_error("scan recovery sidecar ACL size exceeds API bounds")
            })?,
            AclSizeInformation,
        )
    } == 0
        || size.AceCount != 1
    {
        return Err(windows_storage_error(
            "scan recovery sidecar DACL must contain exactly one owner ACE",
        ));
    }
    let mut raw_ace = core::ptr::null_mut();
    if unsafe { GetAce(dacl, 0, &mut raw_ace) } == 0 || raw_ace.is_null() {
        return Err(windows_storage_error(
            "scan recovery sidecar owner ACE cannot be inspected",
        ));
    }
    let ace = raw_ace.cast::<ACCESS_ALLOWED_ACE>();
    let minimum_ace_size = core::mem::size_of::<windows_sys::Win32::Security::ACE_HEADER>()
        + core::mem::size_of::<u32>();
    if unsafe { (*ace).Header.AceType } != ACCESS_ALLOWED_ACE_TYPE as u8
        || unsafe { (*ace).Header.AceFlags } != 0
        || usize::from(unsafe { (*ace).Header.AceSize }) < minimum_ace_size
        || unsafe { (*ace).Mask } != FILE_ALL_ACCESS
    {
        return Err(windows_storage_error(
            "scan recovery sidecar owner ACE is not the required full-control ACE",
        ));
    }
    let ace_sid = unsafe { core::ptr::addr_of!((*ace).SidStart).cast_mut().cast() };
    if unsafe { IsValidSid(ace_sid) } == 0
        || unsafe { EqualSid(ace_sid, expected_sid.as_psid()) } == 0
    {
        return Err(windows_storage_error(
            "scan recovery sidecar owner ACE is not the current Windows user",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn windows_validate_held_scan_recovery_directory(
    directory: &std::fs::File,
    expected_sid: &WindowsSid,
) -> Result<()> {
    // ACL and owner requirements are identical for a directory.  The common
    // verifier works from a handle but also asserts a regular file, so retain
    // this narrowly duplicated directory verifier instead of weakening it.
    windows_validate_held_scan_recovery_object(directory, expected_sid, true)
}

#[cfg(windows)]
fn windows_provision_scan_recovery_object(path: &Path, directory: bool) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW, SetSecurityInfo,
        TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, NO_INHERITANCE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    let sid = windows_current_user_sid()?;
    let file = if directory {
        windows_open_exact_scan_recovery_directory(path, true)?
    } else {
        windows_open_exact_scan_recovery_sidecar(path)?
    };
    let trustee = TRUSTEE_W {
        pMultipleTrustee: core::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_USER,
        ptstrName: sid.as_psid().cast(),
    };
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: trustee,
    };
    let mut dacl = core::ptr::null_mut();
    if unsafe { SetEntriesInAclW(1, &entry, core::ptr::null(), &mut dacl) } != 0 || dacl.is_null() {
        return Err(windows_storage_error(
            "scan recovery sidecar DACL cannot be created",
        ));
    }
    let _dacl = WindowsLocalAllocation(dacl.cast());
    if unsafe {
        SetSecurityInfo(
            file.as_raw_handle().cast(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            sid.as_psid(),
            core::ptr::null_mut(),
            dacl,
            core::ptr::null(),
        )
    } != 0
    {
        return Err(windows_storage_error(
            "scan recovery sidecar ACL cannot be installed for the current Windows user",
        ));
    }
    windows_validate_held_scan_recovery_object(&file, &sid, directory)
}

#[cfg(windows)]
fn windows_provision_scan_recovery_sidecar(path: &Path) -> Result<()> {
    windows_provision_scan_recovery_object(path, false)
}

#[cfg(windows)]
fn windows_provision_scan_recovery_directory(path: &Path) -> Result<()> {
    windows_provision_scan_recovery_object(path, true)
}

#[cfg(windows)]
fn windows_validate_scan_recovery_sidecar(path: &Path) -> Result<()> {
    let sid = windows_current_user_sid()?;
    let file = windows_open_exact_scan_recovery_sidecar(path)?;
    windows_validate_held_scan_recovery_sidecar(&file, &sid)
}

#[cfg(windows)]
fn windows_validate_scan_recovery_directory(path: &Path) -> Result<()> {
    let sid = windows_current_user_sid()?;
    let directory = windows_open_exact_scan_recovery_directory(path, false)?;
    windows_validate_held_scan_recovery_directory(&directory, &sid)
}

#[cfg(unix)]
const fn default_scan_recovery_storage_capability() -> &'static str {
    "platform-owner-private"
}

#[cfg(not(unix))]
const fn default_scan_recovery_storage_capability() -> &'static str {
    "capability.unsupported"
}

const INACTIVE_HANDLES: usize = 16;
const AUTHORITY_HANDLES: usize = 256;

/// Process-wide authority: separate trusted stores must not create competing
/// SQLite writers or independent runtime-only failure histories for one path.
#[derive(Default)]
struct JournalAuthorities {
    journals: HashMap<PathBuf, Arc<ContinuationJournal>>,
    ordered_owners: HashMap<PathBuf, SupervisedOrderedOwner>,
    legacy_owners: HashMap<PathBuf, Vec<LegacyRecordingOwner>>,
    // An exact v1 recovery owns the released `.authority` lock from its first
    // prepared page through final receipt confirmation. That lock is the only
    // cross-process exclusion understood by the released reader, so do not
    // replace it with a process-local marker.
    legacy_recovery_authorities: HashMap<PathBuf, std::fs::File>,
}
struct LegacyRecordingOwner {
    session: std::sync::Weak<dyn crate::continuation::ContinuationSession>,
    recovery_stopped: std::sync::Weak<std::sync::atomic::AtomicBool>,
    recovery_wake: std::sync::Weak<tokio::sync::Notify>,
}
struct SupervisedOrderedOwner {
    producer: String,
    owner: Option<Arc<dyn std::any::Any + Send + Sync>>,
    stop: Arc<dyn OrderedSupervisor>,
}
pub(crate) trait OrderedSupervisor: Send + Sync {
    fn host_cleanup(
        self: Arc<Self>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>;
    fn shutdown(
        self: Arc<Self>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = std::result::Result<
                        crate::ordered_recording::RecordingDriverStatus,
                        crate::ordered_recording::RecordingError,
                    >,
                > + Send,
        >,
    >;
}
#[allow(
    clippy::collapsible_if,
    clippy::result_large_err,
    clippy::type_complexity,
    reason = "Authority recovery retains exact legacy receipt and owner representations."
)]
impl JournalAuthorities {
    fn maintain(&mut self, reserved: usize) {
        self.legacy_owners.retain(|_, owners| {
            owners.retain(|owner| owner.session.strong_count() != 0);
            !owners.is_empty()
        });
        // Serialize eviction with direct-handle registration: a peer cannot
        // join after the live-owner check but before the process lock drops.
        let Ok(_shared) = shared_journals().lock() else {
            return;
        };
        let removable: Vec<PathBuf> = self
            .journals
            .iter()
            .filter(|(path, journal)| {
                !self.legacy_owners.contains_key(*path)
                    && Arc::strong_count(journal) == 1
                    && journal.can_close()
            })
            .map(|(path, _)| path.clone())
            .collect();
        // Live readers/writers and uncommitted failure truth are never evicted.
        // The total authority cap also bounds pinned failure resources; exhaustion
        // refuses a new recording rather than forgetting an earlier failure.
        let remove = removable.len().saturating_sub(INACTIVE_HANDLES - reserved);
        for path in removable.into_iter().take(remove) {
            self.journals.remove(&path);
        }
    }
    fn admission(&mut self) -> Result<()> {
        self.maintain(1);
        if self.journals.len() >= AUTHORITY_HANDLES {
            return Err(error(
                "storage.busy",
                "recording authority capacity reached; existing owners retained",
            ));
        }
        Ok(())
    }
}
fn authorities() -> Result<MutexGuard<'static, JournalAuthorities>> {
    static AUTHORITIES: OnceLock<Mutex<JournalAuthorities>> = OnceLock::new();
    AUTHORITIES
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| error("storage.io", "recording authority interrupted"))
}

/// Host-private path authority and durable owners, independent of radio sessions.
/// Public callers supply only safe recording IDs; no path crosses that boundary.
#[derive(Default)]
pub struct JournalRegistry {
    state: Mutex<RegistryState>,
}
#[allow(
    clippy::collapsible_if,
    clippy::result_large_err,
    clippy::type_complexity,
    reason = "Registry operations retain fixed legacy receipt and ordered-owner result shapes."
)]
impl JournalRegistry {
    /// Release only this host's idle journal handles after its central has
    /// completed shutdown. Durable rows remain on disk; a journal with a live
    /// owner, diagnostic, or unpersisted failure is deliberately retained.
    pub fn release_idle_after_host_shutdown(&self) -> Result<()> {
        let directory = self
            .state
            .lock()
            .map_err(|_| error("storage.io", "recording registry interrupted"))?
            .directory
            .clone();
        let Some(directory) = directory else {
            return Ok(());
        };
        let mut owners = authorities()?;
        let paths: Vec<_> = owners
            .journals
            .iter()
            .filter(|(path, journal)| {
                path.parent() == Some(directory.as_path())
                    && Arc::strong_count(journal) == 1
                    && journal.can_close()
            })
            .map(|(path, _)| path.clone())
            .collect();
        let released: Vec<_> = paths
            .into_iter()
            .filter_map(|path| owners.journals.remove(&path))
            .collect();
        drop(owners);
        let mut shared = shared_journals()
            .lock()
            .map_err(|_| error("storage.io", "journal ownership interrupted"))?;
        for journal in &released {
            if shared.get(&journal.file_identity).is_some_and(|entry| {
                Arc::ptr_eq(entry, &journal.shared) && Arc::strong_count(entry) == 2
            }) {
                shared.remove(&journal.file_identity);
            }
        }
        Ok(())
    }

    /// Configure a private recording directory together with a trusted native
    /// protection provider for v2 scan-recovery sidecars. This API belongs to
    /// a main/native host; do not forward the provider across IPC or FFI.
    pub fn configure_directory_with_scan_recovery_storage(
        &self,
        path: &Path,
        protector: Arc<dyn ScanRecoveryStorageProtector>,
    ) -> Result<Value> {
        self.configure_directory_inner(path, Some(protector))
    }
    fn retain_legacy_recovery_authority(&self, id: &str) -> Result<PathBuf> {
        let state = self
            .state
            .lock()
            .map_err(|_| error("storage.io", "recording registry interrupted"))?;
        let path = Self::location(&state, id)?;
        drop(state);
        let mut owners = authorities()?;
        if owners.journals.contains_key(&path) || owners.ordered_owners.contains_key(&path) {
            return Err(error(
                "storage.owner-conflict",
                "recording is owned by another reader",
            ));
        }
        if !owners.legacy_recovery_authorities.contains_key(&path) {
            owners
                .legacy_recovery_authorities
                .insert(path.clone(), lock_authority(&path)?);
        }
        Ok(path)
    }

    fn release_legacy_recovery_authority(&self, id: &str) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| error("storage.io", "recording registry interrupted"))?;
        let path = Self::location(&state, id)?;
        drop(state);
        authorities()?.legacy_recovery_authorities.remove(&path);
        Ok(())
    }

    /// Replay a pre-v5 pending recording page using its original cursor, without
    /// adding v5 identity or changing the schema. Only an existing page can be
    /// read this way; the caller must durably retain it before acknowledgement.
    pub fn legacy_recording_prepare(
        &self,
        id: &str,
        max_items: u32,
        max_bytes: u32,
    ) -> Result<Value> {
        self.legacy_recording_prepare_inner(id, max_items, max_bytes, false)
    }

    /// Version 1 recovery observes current bytes; it cannot prove the original preparation.
    pub fn legacy_recording_prepare_exact(
        &self,
        id: &str,
        max_items: u32,
        max_bytes: u32,
    ) -> Result<Value> {
        self.legacy_recording_prepare_inner(id, max_items, max_bytes, true)
    }

    fn legacy_recording_prepare_inner(
        &self,
        id: &str,
        max_items: u32,
        max_bytes: u32,
        exact: bool,
    ) -> Result<Value> {
        if !(1..=2048).contains(&max_items) || !(1..=4_194_304).contains(&max_bytes) {
            return Err(error("argument.invalid", "invalid journal read bounds"));
        }
        let recovery_path = if exact {
            Some(self.retain_legacy_recovery_authority(id)?)
        } else {
            None
        };
        let result = self.with_legacy_recording(id, |tx, version| {
            let (token, last, more): (Option<String>, Option<i64>, Option<bool>) = tx.query_row(
                "SELECT pending_token,pending_last,pending_more FROM journal WHERE id=1", [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let token = token.ok_or_else(|| error("argument.invalid", "no legacy page is pending"))?;
            let last = last.ok_or_else(|| error("storage.corrupt", "prepared cursor is incomplete"))?;
            let more = more.ok_or_else(|| error("storage.corrupt", "prepared cursor is incomplete"))?;
            let filter = if version <= 2 { "ordinal<=?1" } else { "recording_acked=0 AND ordinal<=?1" };
            validate_legacy_recording_page(tx, filter, last)?;
            let sql = format!("SELECT body,bytes,ordinal FROM records WHERE {filter} ORDER BY ordinal LIMIT 2049");
            let mut statement = tx.prepare(&sql)?;
            let mut rows = statement.query([last])?;
            let mut records = Vec::new();
            let mut raw_records = Vec::new();
            let mut bytes = 0u64;
            while let Some(row) = rows.next()? {
                let body: String = row.get(0)?;
                let size = unsigned(row, 1)?;
                bytes = bytes.checked_add(size).ok_or_else(|| error("storage.corrupt", "prepared size overflow"))?;
                if size != body.len() as u64 || records.len() >= 2048 || bytes > 4_194_304 {
                    return Err(error("storage.corrupt", "prepared records violate journal bounds"));
                }
                records.push(decode(&body)?);
                let ordinal: i64 = row.get(2)?;
                raw_records.push(if exact { json!({"ordinal":ordinal.to_string(),"body":body.as_bytes()}) } else { json!({"ordinal":ordinal,"body":body.as_bytes()}) });
            }
            if records.is_empty() { return Err(error("storage.corrupt", "prepared prefix is missing")); }
            if records.len() > max_items as usize || bytes > u64::from(max_bytes) {
                return Err(error("argument.invalid", "requested bounds cannot fit the already prepared prefix"));
            }
            if exact {
                let page = json!({"version":1,"token":token,"cursorLast":last.to_string(),"rawRecords":raw_records,"bytes":bytes,"more":more});
                let (_, digest) = legacy_exact_claim(&page)?;
                ensure_legacy_exact_marker(tx)?;
                match legacy_exact_state(tx)? {
                    Some((stored, None, false)) if stored == digest => {},
                    Some((_, None, false)) => return Err(error("storage.corrupt", "prepared exact recovery page changed")),
                    Some(_) => return Err(error("storage.schema", "exact recovery receipt must be confirmed first")),
                    None => {
                        tx.execute(
                            "INSERT INTO legacy_exact_receipt(id,digest,receipt,state,claim) VALUES(1,?1,NULL,'outstanding',?2)",
                            params![&digest, page.to_string()],
                        )?;
                    }
                }
                Ok(page)
            } else {
                Ok(json!({"token":token,"records":records,"rawRecords":raw_records,"bytes":bytes,"more":more}))
            }
        });
        if result.is_err() {
            if let Some(path) = recovery_path {
                authorities()?.legacy_recovery_authorities.remove(&path);
            }
        }
        result
    }

    /// Restore the exact durable v1 page after a process restart. The retained
    /// claim is independent of the mutable v1 rows, so a later old-reader
    /// mutation cannot make the original evidence disappear or appear valid.
    pub fn legacy_recording_recover_exact(&self, id: &str) -> Result<Option<Value>> {
        let path = self.retain_legacy_recovery_authority(id)?;
        let result = self.with_legacy_recording(id, |tx, _| {
            ensure_legacy_exact_marker(tx)?;
            let Some((_, _, _)) = legacy_exact_state(tx)? else {
                return Ok(json!({"page":null}));
            };
            let claim: String = tx.query_row(
                "SELECT claim FROM legacy_exact_receipt WHERE id=1",
                [],
                |row| row.get(0),
            )?;
            Ok(json!({"page":decode(&claim)?}))
        });
        let result: Result<Option<Value>> = result.and_then(|value| match value.get("page") {
            Some(Value::Null) => Ok(None),
            Some(page) => Ok(Some(page.clone())),
            None => Err(error(
                "storage.corrupt",
                "exact recovery response is incomplete",
            )),
        });
        if !matches!(result, Ok(Some(_))) {
            authorities()?.legacy_recovery_authorities.remove(&path);
        }
        result
    }

    /// Acknowledge the original pre-v5 token, with the original durable receipt.
    pub fn legacy_recording_acknowledge(&self, id: &str, token: &str) -> Result<Value> {
        if token.is_empty() || token.len() > 96 {
            return Err(error("argument.invalid", "invalid journal token"));
        }
        self.with_legacy_recording(id, |tx, version| {
            if legacy_exact_state(tx)?.is_some() { return Err(error("storage.schema", "exact recovery receipt must be retired first")); }
            let (pending, last, more, ack, receipt): (Option<String>, Option<i64>, Option<bool>, Option<String>, Option<String>) = tx.query_row(
                "SELECT pending_token,pending_last,pending_more,last_ack_token,last_ack_receipt FROM journal WHERE id=1", [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )?;
            if ack.as_deref() == Some(token) {
                return decode(&receipt.ok_or_else(|| error("storage.corrupt", "ack receipt is missing"))?);
            }
            if pending.as_deref() != Some(token) { return Err(error("argument.invalid", "journal token is unknown or stale")); }
            let last = last.ok_or_else(|| error("storage.corrupt", "prepared cursor is incomplete"))?;
            more.ok_or_else(|| error("storage.corrupt", "prepared cursor is incomplete"))?;
            let filter = if version <= 2 { "ordinal<=?1" } else { "recording_acked=0 AND ordinal<=?1" };
            validate_legacy_recording_page(tx, filter, last)?;
            let bytes: i64 = tx.query_row(&format!("SELECT coalesce(sum(bytes),0) FROM records WHERE {filter}"), [last], |row| row.get(0))?;
            let removed = tx.execute(&format!("DELETE FROM records WHERE {filter}"), [last])?;
            tx.execute("UPDATE journal SET retained_records=retained_records-?1,retained_bytes=retained_bytes-?2 WHERE id=1", params![sql_integer(removed as u64)?, bytes])?;
            let receipt = json!({"token":token,"acknowledged":true,"records":removed});
            tx.execute("UPDATE journal SET pending_token=NULL,pending_last=NULL,pending_more=NULL,last_ack_token=?1,last_ack_receipt=?2 WHERE id=1", params![token,receipt.to_string()])?;
            Ok(receipt)
        })
    }

    /// Compare the accepted page and cursor inside the deletion transaction.
    pub fn legacy_recording_acknowledge_exact(&self, id: &str, page: &Value) -> Result<Value> {
        let (claim, digest) = legacy_exact_claim(page)?;
        self.with_legacy_recording(id, |tx, version| {
            ensure_legacy_exact_marker(tx)?;
            let (stored_digest, receipt, retired) = legacy_exact_state(tx)?.ok_or_else(|| error("argument.invalid", "exact recovery page was not prepared"))?;
            if retired { return Err(error("storage.schema", "exact recovery receipt is retired")); }
            if stored_digest != digest { return Err(error("storage.schema", "another exact recovery page remains outstanding")); }
            ensure_legacy_exact_marker(tx)?;
            if let Some(receipt) = receipt {
                if receipt["token"] != claim["token"] || receipt["records"].as_u64() != claim["rawRecords"].as_array().map(|rows| rows.len() as u64) { return Err(error("storage.corrupt", "exact recovery receipt differs from claim")); }
                return Ok(receipt);
            }
            let (pending, last, more): (Option<String>, Option<i64>, Option<bool>) = tx.query_row(
                "SELECT pending_token,pending_last,pending_more FROM journal WHERE id=1", [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let last = last.ok_or_else(|| error("storage.corrupt", "prepared cursor is incomplete"))?;
            let more = more.ok_or_else(|| error("storage.corrupt", "prepared cursor is incomplete"))?;
            let token = pending.ok_or_else(|| error("argument.invalid", "no legacy page is pending"))?;
            let filter = if version <= 2 { "ordinal<=?1" } else { "recording_acked=0 AND ordinal<=?1" };
            validate_legacy_recording_page(tx, filter, last)?;
            let mut stmt = tx.prepare(&format!("SELECT ordinal,CAST(body AS BLOB) FROM records WHERE {filter} ORDER BY ordinal"))?;
            let rows = stmt.query_map([last], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
            let mut raw = Vec::new();
            for row in rows { let (ordinal, body) = row?; raw.push(json!({"ordinal":ordinal.to_string(),"body":body})); }
            if claim != json!({"version":1,"token":token,"cursorLast":last.to_string(),"more":more,"rawRecords":raw}) {
                return Err(error("storage.corrupt", "accepted legacy page differs from current cursor or rows"));
            }
            let bytes: i64 = tx.query_row(&format!("SELECT coalesce(sum(bytes),0) FROM records WHERE {filter}"), [last], |r| r.get(0))?;
            let removed = tx.execute(&format!("DELETE FROM records WHERE {filter}"), [last])?;
            tx.execute("UPDATE journal SET retained_records=retained_records-?1,retained_bytes=retained_bytes-?2,pending_token=NULL,pending_last=NULL,pending_more=NULL WHERE id=1", params![sql_integer(removed as u64)?, bytes])?;
            let receipt = json!({"version":1,"token":token,"acknowledged":true,"records":removed});
            tx.execute("UPDATE legacy_exact_receipt SET receipt=?1 WHERE id=1", [receipt.to_string()])?;
            Ok(receipt)
        })
    }

    /// Retire only the receipt for the same accepted page after its result is durable.
    pub fn legacy_recording_retire_exact(&self, id: &str, page: &Value) -> Result<Value> {
        let (claim, digest) = legacy_exact_claim(page)?;
        // A migrated journal may already be held by this process. Its connection
        // is the authority and can answer a retry without reopening the file.
        {
            let state = self
                .state
                .lock()
                .map_err(|_| error("storage.io", "recording registry interrupted"))?;
            let path = Self::location(&state, id)?;
            let authorities = authorities()?;
            if let Some(journal) = authorities.journals.get(&path) {
                let db = journal
                    .shared
                    .connection
                    .lock()
                    .map_err(|_| error("storage.io", "recording journal interrupted"))?;
                return retired_exact_result(&db, &claim, &digest);
            }
        }
        let result = self.with_legacy_recording_version(id, true, |tx, version| {
            let receipt_table_exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='legacy_exact_receipt')",
                [],
                |row| row.get(0),
            )?;
            if !receipt_table_exists {
                return Err(error(
                    "storage.corrupt",
                    "exact recovery receipt is missing",
                ));
            }
            ensure_legacy_exact_marker(tx)?;
            let (stored, receipt, retired) = legacy_exact_state(tx)?
                .ok_or_else(|| error("storage.corrupt", "exact recovery receipt is absent"))?;
            let receipt = receipt.ok_or_else(|| {
                error(
                    "argument.invalid",
                    "exact recovery has not been acknowledged",
                )
            })?;
            if stored != digest {
                return Err(error("argument.invalid", "exact recovery receipt differs"));
            }
            if receipt["token"] != claim["token"]
                || receipt["records"].as_u64()
                    != claim["rawRecords"].as_array().map(|rows| rows.len() as u64)
            {
                return Err(error(
                    "storage.corrupt",
                    "exact recovery receipt differs from claim",
                ));
            }
            if !retired {
                if version >= 5 {
                    return Err(error(
                        "storage.corrupt",
                        "unretired receipt in migrated journal",
                    ));
                }
                tx.execute(
                    "UPDATE legacy_exact_receipt SET state='retired' WHERE id=1",
                    [],
                )?;
            }
            Ok(json!({"version":1,"token":receipt["token"],"retired":true}))
        });
        match result {
            // A retired result is immutable until explicitly confirmed. A read
            // transaction can replay it while another process holds authority.
            Err(ref failure) if failure.kind == "storage.busy" => {
                match self.read_retired_exact(id, &claim, &digest)? {
                    Some(receipt) => Ok(receipt),
                    None => result,
                }
            }
            _ => result,
        }
    }

    fn read_retired_exact(&self, id: &str, claim: &Value, digest: &[u8]) -> Result<Option<Value>> {
        let state = self
            .state
            .lock()
            .map_err(|_| error("storage.io", "recording registry interrupted"))?;
        let path = Self::location(&state, id)?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|_| error("storage.io", "recording file cannot be inspected"))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(error(
                "storage.permission",
                "recording path is not a regular file",
            ));
        }
        let mut db = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.busy_timeout(std::time::Duration::ZERO)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let application: i64 = tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let page: u64 = tx.pragma_query_value(None, "page_size", |row| unsigned(row, 0))?;
        let mode: String = tx.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if !(1..=SCHEMA_VERSION).contains(&version)
            || application != APPLICATION_ID
            || page != PAGE
            || mode != "delete"
        {
            return Err(error(
                "storage.schema",
                "not a supported journal; file retained",
            ));
        }
        let stored_id: String =
            tx.query_row("SELECT recording_id FROM journal WHERE id=1", [], |row| {
                row.get(0)
            })?;
        if stored_id != id {
            return Err(error(
                "storage.identity",
                "recording identity differs; file retained",
            ));
        }
        let result = if legacy_exact_state(&tx)?.is_some_and(|(_, _, retired)| retired) {
            Some(retired_exact_result(&tx, claim, digest)?)
        } else {
            None
        };
        tx.commit()?;
        Ok(result)
    }

    /// The caller has received retirement and permits replacing its bounded
    /// replay slot when another legacy page must be processed.
    pub fn legacy_recording_confirm_exact_retirement(
        &self,
        id: &str,
        page: &Value,
    ) -> Result<Value> {
        let (claim, digest) = legacy_exact_claim(page)?;
        let result = self.with_legacy_recording(id, |tx, _| {
            ensure_legacy_exact_marker(tx)?;
            let result = retired_exact_result(tx, &claim, &digest)?;
            tx.execute("DELETE FROM legacy_exact_receipt WHERE id=1", [])?;
            Ok(result)
        })?;
        self.release_legacy_recovery_authority(id)?;
        Ok(result)
    }

    fn with_legacy_recording(
        &self,
        id: &str,
        operation: impl FnOnce(&Connection, i64) -> Result<Value>,
    ) -> Result<Value> {
        self.with_legacy_recording_version(id, false, operation)
    }

    fn with_legacy_recording_version(
        &self,
        id: &str,
        allow_migrated: bool,
        operation: impl FnOnce(&Connection, i64) -> Result<Value>,
    ) -> Result<Value> {
        let state = self
            .state
            .lock()
            .map_err(|_| error("storage.io", "recording registry interrupted"))?;
        let path = Self::location(&state, id)?;
        let authorities = authorities()?;
        if authorities.journals.contains_key(&path)
            || authorities.ordered_owners.contains_key(&path)
        {
            return Err(error(
                "storage.owner-conflict",
                "recording is owned by another reader",
            ));
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|_| error("storage.io", "recording file cannot be inspected"))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > (1 << 29) {
            return Err(error(
                "storage.permission",
                "recording path is not an admissible regular file",
            ));
        }
        let _temporary_authority = if authorities.legacy_recovery_authorities.contains_key(&path) {
            None
        } else {
            Some(lock_authority(&path)?)
        };
        let mut db = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.busy_timeout(std::time::Duration::ZERO)?;
        db.execute_batch("PRAGMA synchronous=EXTRA; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF; PRAGMA secure_delete=ON;")?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let application: i64 = tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let page: u64 = tx.pragma_query_value(None, "page_size", |row| unsigned(row, 0))?;
        let mode: String = tx.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if !(1..=if allow_migrated { SCHEMA_VERSION } else { 4 }).contains(&version)
            || application != APPLICATION_ID
            || page != PAGE
            || mode != "delete"
        {
            return Err(error(
                "storage.schema",
                "not a supported pre-v5 journal; file retained",
            ));
        }
        let check: String = tx.pragma_query_value(None, "quick_check", |row| row.get(0))?;
        if check != "ok" {
            return Err(error(
                "storage.corrupt",
                "journal integrity check failed; file retained",
            ));
        }
        let (stored_id, max_bytes, max_records, consistent): (String,u64,u64,bool) = tx.query_row(
            "SELECT recording_id,max_bytes,max_records,retained_records=(SELECT count(*) FROM records) AND retained_bytes=(SELECT coalesce(sum(bytes),0) FROM records) FROM journal WHERE id=1", [],
            |row| Ok((row.get(0)?, unsigned(row,1)?, unsigned(row,2)?, row.get(3)?)),
        )?;
        if stored_id != id {
            return Err(error(
                "storage.identity",
                "recording identity differs; file retained",
            ));
        }
        if !consistent {
            return Err(error(
                "storage.corrupt",
                "journal retained counters are inconsistent; file retained",
            ));
        }
        if !(1 << 20..=1 << 30).contains(&max_bytes) || !(1..=1_000_000).contains(&max_records) {
            return Err(error("storage.corrupt", "stored quota is invalid"));
        }
        let page_limit = (max_bytes - OVERHEAD) / (2 * PAGE + 8);
        let pages: u64 = tx.pragma_query_value(None, "page_count", |row| unsigned(row, 0))?;
        if pages > page_limit {
            return Err(error(
                "storage.full",
                "existing database exceeds quota; file retained",
            ));
        }
        let limit: u64 =
            tx.query_row(&format!("PRAGMA max_page_count={page_limit}"), [], |row| {
                unsigned(row, 0)
            })?;
        if limit != page_limit {
            return Err(error(
                "storage.full",
                "existing database exceeds quota; file retained",
            ));
        }
        if (3..=4).contains(&version) {
            let evidence: bool = tx.query_row("SELECT transfer_id IS NOT NULL OR pending_token IS NOT NULL OR pending_last IS NOT NULL OR pending_more IS NOT NULL OR cutoff!=0 OR progressed!=0 OR EXISTS(SELECT 1 FROM records WHERE recording_acked!=0) FROM evidence_cursor WHERE id=1", [], |row| row.get(0))?;
            if evidence {
                return Err(error(
                    "storage.schema",
                    "legacy evidence reader remains active; file retained",
                ));
            }
        }
        if version == 4 {
            let saved: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM evidence_claim_header) OR EXISTS(SELECT 1 FROM evidence_claim_records)", [], |row| row.get(0))?;
            if saved {
                return Err(error(
                    "storage.corrupt",
                    "legacy evidence claim remains retained",
                ));
            }
        }
        let result = operation(&tx, version)?;
        tx.commit()?;
        Ok(result)
    }
    /// Snapshot the bounded process-owned recorders on this exact central.
    /// The host must fence new recorder admission while using this snapshot for
    /// final central shutdown; a manager close must not stop these owners.
    pub fn supervised_ordered_for_central<B: crate::boundary::RadioBoundary>(
        central: &crate::central::DesktopCentral<B>,
    ) -> Result<Vec<Arc<crate::ordered_recording::OrderedRecording<B>>>> {
        let state = authorities()?;
        let mut owners = Vec::new();
        for slot in state.ordered_owners.values() {
            if let Some(owner) = &slot.owner {
                if let Ok(owner) =
                    Arc::clone(owner).downcast::<crate::ordered_recording::OrderedRecording<B>>()
                {
                    if owner.belongs_to(central) {
                        owners.push(owner);
                    }
                }
            }
        }
        Ok(owners)
    }
    pub fn configure_directory(&self, path: &Path) -> Result<Value> {
        self.configure_directory_inner(path, None)
    }

    fn configure_directory_inner(
        &self,
        path: &Path,
        protector: Option<Arc<dyn ScanRecoveryStorageProtector>>,
    ) -> Result<Value> {
        let canonical = path.canonicalize().map_err(|_| {
            error("storage.io", "recording directory cannot be resolved").at("configure")
        })?;
        if !canonical.is_dir() {
            return Err(error(
                "storage.permission",
                "recording directory is not a directory",
            )
            .at("configure"));
        }
        if let Some(protector) = protector.as_deref() {
            // The configured root is part of the security boundary: without
            // it, a hostile principal could replace the protected child
            // directory after validation. This trusted-host action happens
            // before the registry publishes the location to any journal.
            protector.provision_directory(&canonical)?;
            protector.validate_directory(&canonical)?;
        }
        let identity = path_identity(&canonical)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("storage.io", "recording registry interrupted"))?;
        if state
            .directory
            .as_ref()
            .is_some_and(|current| current != &canonical)
        {
            return Err(error(
                "storage.identity",
                "recording directory is already configured",
            )
            .at("configure"));
        }
        if state
            .directory_identity
            .is_some_and(|current| current != identity)
        {
            return Err(error(
                "storage.identity",
                "configured recording directory was replaced",
            ));
        }
        if let Some(protector) = protector {
            if state.scan_recovery_storage.is_some() {
                return Err(error(
                    "storage.identity",
                    "scan recovery storage protector is already configured",
                )
                .at("configure"));
            }
            state.scan_recovery_storage = Some(protector);
        }
        let scan_recovery_storage = state
            .scan_recovery_storage
            .as_ref()
            .map_or_else(default_scan_recovery_storage_capability, |value| {
                value.capability()
            });
        state.directory = Some(canonical);
        state.directory_identity = Some(identity);
        Ok(
            json!({"state":"configured","encrypted":false,"scanRecoveryStorage":scan_recovery_storage}),
        )
    }
    fn location(state: &RegistryState, id: &str) -> Result<PathBuf> {
        if !valid_id(id) {
            return Err(error("argument.invalid", "invalid recording identity"));
        }
        let directory = state.directory.as_ref().ok_or_else(|| {
            error(
                "storage.unconfigured",
                "host has not configured a private recording directory",
            )
        })?;
        if directory.canonicalize().ok().as_ref() != Some(directory)
            || Some(path_identity(directory)?) != state.directory_identity
        {
            return Err(error(
                "storage.permission",
                "configured recording directory changed",
            ));
        }
        Ok(directory.join(format!("{id}.sqlite")))
    }

    /// Opens the registry-wide host-private terminal-evidence ledger. The
    /// trusted configured directory chooses the path; callers never do.
    pub fn terminal_evidence_ledger(
        &self,
        maximum_bytes: u64,
    ) -> std::result::Result<
        crate::terminal_evidence_ledger::TerminalEvidenceLedger,
        crate::terminal_evidence_ledger::TerminalEvidenceError,
    > {
        let state = self
            .state
            .lock()
            .map_err(|_| crate::terminal_evidence_ledger::TerminalEvidenceError::Unresolved)?;
        let directory = state
            .directory
            .as_ref()
            .ok_or(crate::terminal_evidence_ledger::TerminalEvidenceError::Unresolved)?;
        crate::terminal_evidence_ledger::TerminalEvidenceLedger::open(
            &directory.join("terminal-evidence.sqlite"),
            maximum_bytes,
        )
    }
    /// Read a bounded raw chunk from a configured recording, even when its
    /// journal cannot be opened. This does not validate historical identity or
    /// provide a consistent snapshot across calls; quiesce writers first.
    pub fn inspect_raw_record(
        &self,
        id: &str,
        source: &str,
        ordinal: Option<&str>,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Value> {
        let state = self
            .state
            .lock()
            .map_err(|_| error("storage.io", "recording registry interrupted"))?;
        let path = Self::location(&state, id)?;
        drop(state);
        ContinuationJournal::inspect_raw_record(&path, source, ordinal, offset, max_bytes)
    }
    pub fn open(
        &self,
        id: &str,
        declaration: &Value,
        quota: JournalQuota,
    ) -> Result<Arc<ContinuationJournal>> {
        let state = self
            .state
            .lock()
            .map_err(|_| error("storage.io", "recording registry interrupted"))?;
        let path = Self::location(&state, id)?;
        let scan_recovery_storage = state.scan_recovery_storage.clone();
        drop(state);
        let mut authorities = authorities()?;
        if let Some(journal) = authorities.journals.get(&path) {
            journal.validate_path(&path)?;
            if journal.quota != quota || journal.declaration != encoded(declaration, 65536)? {
                return Err(error(
                    "storage.identity",
                    "recording identity or quota differs; file retained",
                ));
            }
            let journal = journal.clone();
            authorities.maintain(0);
            return Ok(journal);
        }
        authorities.admission()?;
        if authorities.ordered_owners.contains_key(&path) {
            return Err(error(
                "storage.owner-conflict",
                "ordered producer remains supervised",
            ));
        }
        if authorities.legacy_recovery_authorities.contains_key(&path) {
            return Err(error(
                "storage.owner-conflict",
                "exact legacy recovery retains this recording",
            ));
        }
        let authority = lock_authority(&path)?;
        let mut journal = ContinuationJournal::open_with_scan_recovery_storage(
            &path,
            id,
            declaration,
            quota,
            scan_recovery_storage,
        )?;
        journal.authority = Some(authority);
        journal.recover_cold_ordered_owner()?;
        let journal = Arc::new(journal);
        authorities.journals.insert(path, journal.clone());
        Ok(journal)
    }
    /// Request the supervised ordered owner to drain and release its source.
    /// None means no in-process ordered owner exists for this recording.
    pub async fn stop_ordered(
        &self,
        id: &str,
    ) -> std::result::Result<
        Option<crate::ordered_recording::RecordingDriverStatus>,
        crate::ordered_recording::RecordingError,
    > {
        let stop = {
            let registry = self
                .state
                .lock()
                .map_err(|_| error("storage.io", "recording registry interrupted"))?;
            let path = Self::location(&registry, id)?;
            let state = authorities()?;
            state
                .ordered_owners
                .get(&path)
                .map(|slot| Arc::clone(&slot.stop))
        };
        match stop {
            Some(owner) => owner.shutdown().await.map(Some),
            None => Ok(None),
        }
    }

    /// Versioned host-facing stop receipt; pending outcomes retain the owner.
    pub async fn stop_ordered_host_receipt(
        self: &Arc<Self>,
        id: &str,
    ) -> crate::continuation::Result<Value> {
        let owner = {
            let registry = self.state.lock().map_err(|_| {
                crate::continuation::recording_failure(error(
                    "storage.io",
                    "recording registry interrupted",
                ))
            })?;
            let path =
                Self::location(&registry, id).map_err(crate::continuation::recording_failure)?;
            let state = authorities().map_err(crate::continuation::recording_failure)?;
            state
                .ordered_owners
                .get(&path)
                .map(|slot| Arc::clone(&slot.stop))
        };
        if let Some(owner) = owner {
            let recording =
                crate::ordered_recording::stop_receipt(Arc::clone(&owner).shutdown().await);
            let host_cleanup = owner.host_cleanup().await;
            Ok(json!({"recording":recording,"hostCleanup":host_cleanup}))
        } else {
            let registry = Arc::clone(self);
            let id = id.to_owned();
            run_blocking_result(move || {
                registry
                    .stop_ordered_host_offline(&id)
                    .map_err(crate::continuation::recording_failure)
            })
            .await
        }
    }

    /// Versioned host-facing stop receipt; pending outcomes retain the owner.
    pub async fn stop_ordered_receipt(
        self: &Arc<Self>,
        id: &str,
    ) -> crate::continuation::Result<Value> {
        if !valid_id(id) {
            return Err(crate::continuation::recording_failure(error(
                "argument.invalid",
                "invalid recording identity",
            )));
        }
        match self.stop_ordered(id).await {
            Ok(Some(status)) => return Ok(crate::ordered_recording::stop_receipt(Ok(status))),
            Err(error) => return Ok(crate::ordered_recording::stop_receipt(Err(error))),
            Ok(None) => {}
        }
        let registry = Arc::clone(self);
        let id = id.to_owned();
        let status = run_blocking_result(move || {
            registry
                .get(&id)
                .and_then(|journal| journal.status())
                .map_err(crate::continuation::recording_failure)
        })
        .await?;
        match status["orderedOwner"]["state"].as_str() {
            Some("sealed") => Ok(json!({"version":2,"state":"finished","error":null})),
            Some("interrupted") => Ok(json!({"version":2,"state":"interrupted","error":null})),
            _ => Err(
                json!({"code":"lifecycle.invalid-state","domain":"restoration","operation":"continuation.recording.stop-ordered","detail":"no settled or supervised ordered recording exists"}),
            ),
        }
    }

    /// Read the immutable versioned ordered context, independently of radio.
    pub fn ordered_context(&self, id: &str) -> Result<Value> {
        let journal = self.get(id)?;
        let value = decode(&journal.declaration)?;
        crate::ordered_recording::OrderedRecordingDeclaration::from_context(&value)
            .map_err(|_| error("storage.schema", "ordered recording context is invalid"))?;
        Ok(value)
    }

    // Registry admission and offline access share one authority lock: callers
    // never observe or control an owner created between check and use.
    fn with_offline_ordered<T>(
        &self,
        id: &str,
        operation: impl FnOnce(&ContinuationJournal, Value) -> Result<T>,
    ) -> Result<T> {
        let journal = self.get(id)?;
        let state = authorities()?;
        let path = journal_path(&state, &journal)?;
        if state.ordered_owners.contains_key(&path) {
            return Err(error(
                "storage.owner-conflict",
                "ordered recording has a live owner",
            ));
        }
        let status = journal.status()?;
        match status["orderedOwner"]["state"].as_str() {
            Some("sealed" | "interrupted") => operation(&journal, status),
            Some("live") => Err(error(
                "storage.owner-conflict",
                "ordered producer remains live",
            )),
            None if status.get("orderedOwner").is_none() => {
                let value = decode(&journal.declaration)?;
                crate::ordered_recording::OrderedRecordingDeclaration::from_context(&value)
                    .map_err(|_| {
                        error("storage.identity", "recording has no ordered declaration")
                    })?;
                operation(&journal, status)
            }
            _ => Err(error(
                "storage.identity",
                "recording has no ordered lifecycle",
            )),
        }
    }

    /// Trusted offline context access requires no live ordered owner.
    pub fn ordered_context_offline(&self, id: &str) -> Result<Value> {
        self.with_offline_ordered(id, |journal, _| {
            let value = decode(&journal.declaration)?;
            crate::ordered_recording::OrderedRecordingDeclaration::from_context(&value)
                .map_err(|_| error("storage.schema", "ordered recording context is invalid"))?;
            Ok(value)
        })
    }
    /// A versioned wrapper distinguishes a persisted not-started declaration
    /// from a sealed or crash-interrupted ordered producer.
    pub fn ordered_status_offline(&self, id: &str) -> Result<Value> {
        self.with_offline_ordered(id, |_, status| {
            let state = match status["orderedOwner"]["state"].as_str() {
                Some("sealed") => "sealed",
                Some("interrupted") => "interrupted",
                None => "not-started",
                _ => {
                    return Err(error(
                        "storage.identity",
                        "recording has no settled ordered lifecycle",
                    ));
                }
            };
            Ok(json!({"version":1,"state":state,"journal":status}))
        })
    }
    /// An offline stop observes the already-settled state and never mutates it.
    pub fn stop_ordered_host_offline(&self, id: &str) -> Result<Value> {
        self.with_offline_ordered(id, |journal, status| {
            let state = match status["orderedOwner"]["state"].as_str() {
                Some("sealed") => "finished",
                Some("interrupted") => "interrupted",
                _ => return Err(error("storage.identity", "recording has no settled ordered lifecycle")),
            };
            Ok(json!({"recording":{"version":2,"state":state,"error":null},"hostCleanup":journal.ordered_host_cleanup()?}))
        })
    }

    /// An offline stop observes the already-settled state and never mutates it.
    pub fn stop_ordered_offline(&self, id: &str) -> Result<Value> {
        self.with_offline_ordered(id, |_, status| {
            match status["orderedOwner"]["state"].as_str() {
                Some("sealed") => Ok(json!({"version":2,"state":"finished","error":null})),
                Some("interrupted") => Ok(json!({"version":2,"state":"interrupted","error":null})),
                _ => Err(error(
                    "storage.identity",
                    "recording has no settled ordered lifecycle",
                )),
            }
        })
    }

    /// Look up only an owner bound to this exact host central and recording.
    pub fn ordered_owner<B: crate::boundary::RadioBoundary>(
        &self,
        id: &str,
        central: &crate::central::DesktopCentral<B>,
    ) -> Result<Option<Arc<crate::ordered_recording::OrderedRecording<B>>>> {
        let registry = self
            .state
            .lock()
            .map_err(|_| error("storage.io", "recording registry interrupted"))?;
        let path = Self::location(&registry, id)?;
        let state = authorities()?;
        let Some(slot) = state.ordered_owners.get(&path) else {
            return Ok(None);
        };
        let Some(owner) = &slot.owner else {
            return Err(error(
                "storage.owner-conflict",
                "ordered owner is not installed",
            ));
        };
        let owner = Arc::clone(owner)
            .downcast::<crate::ordered_recording::OrderedRecording<B>>()
            .map_err(|_| {
                error(
                    "storage.owner-conflict",
                    "ordered owner belongs to another host",
                )
            })?;
        if !owner.belongs_to(central) {
            return Err(error(
                "storage.owner-conflict",
                "ordered owner belongs to another host",
            ));
        }
        Ok(Some(owner))
    }

    pub fn get(&self, id: &str) -> Result<Arc<ContinuationJournal>> {
        let state = self
            .state
            .lock()
            .map_err(|_| error("storage.io", "recording registry interrupted"))?;
        let path = Self::location(&state, id)?;
        let scan_recovery_storage = state.scan_recovery_storage.clone();
        drop(state);
        let mut authorities = authorities()?;
        if let Some(journal) = authorities.journals.get(&path) {
            journal.validate_path(&path)?;
            let journal = journal.clone();
            authorities.maintain(0);
            return Ok(journal);
        }
        authorities.admission()?;
        if authorities.ordered_owners.contains_key(&path) {
            return Err(error(
                "storage.owner-conflict",
                "ordered producer remains supervised",
            ));
        }
        if authorities.legacy_recovery_authorities.contains_key(&path) {
            return Err(error(
                "storage.owner-conflict",
                "exact legacy recovery retains this recording",
            ));
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|_| error("storage.io", "recording file cannot be inspected").at("lookup"))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > (1 << 29) {
            return Err(error(
                "storage.permission",
                "recording path is not an admissible regular file",
            )
            .at("lookup"));
        }
        let authority = lock_authority(&path)?;
        // Read only bounded identity before the full quota/schema/integrity open.
        // READ_ONLY without CREATE prevents missing IDs creating empty journals.
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(JournalError::from)
        .map_err(|failure| failure.at("lookup"))?;
        let size: i64 = connection.query_row(
            "SELECT length(CAST(declaration AS BLOB)) FROM journal WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        if !(2..=65536).contains(&size) {
            return Err(error(
                "storage.corrupt",
                "stored declaration exceeds identity bounds",
            ));
        }
        let (declaration, max_bytes, max_records): (String, u64, u64) = connection.query_row(
            "SELECT declaration,max_bytes,max_records FROM journal WHERE id=1",
            [],
            |row| Ok((row.get(0)?, unsigned(row, 1)?, unsigned(row, 2)?)),
        )?;
        drop(connection);
        let declaration = decode(&declaration)?;
        let mut journal = ContinuationJournal::open_with_scan_recovery_storage(
            &path,
            id,
            &declaration,
            JournalQuota {
                max_bytes,
                max_records,
            },
            scan_recovery_storage,
        )?;
        journal.authority = Some(authority);
        journal.recover_cold_ordered_owner()?;
        let journal = Arc::new(journal);
        authorities.journals.insert(path, journal.clone());
        Ok(journal)
    }
}

// The receipt contains a SHA-256 commitment to the ordered raw bytes and cursor.
// Only one can be outstanding, bounding its storage independently of client count.
fn legacy_exact_claim(page: &Value) -> Result<(Value, Vec<u8>)> {
    let token = page
        .get("token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 96)
        .ok_or_else(|| error("argument.invalid", "invalid exact recovery token"))?;
    let last = page
        .get("cursorLast")
        .and_then(Value::as_str)
        .ok_or_else(|| error("argument.invalid", "invalid exact recovery cursor"))?;
    if last
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0 && n.to_string() == last)
        .is_none()
        || page.get("version") != Some(&json!(1))
        || !page.get("more").is_some_and(Value::is_boolean)
    {
        return Err(error("argument.invalid", "invalid exact recovery cursor"));
    }
    let rows = page
        .get("rawRecords")
        .and_then(Value::as_array)
        .filter(|rows| !rows.is_empty() && rows.len() <= 2048)
        .ok_or_else(|| error("argument.invalid", "invalid exact recovery rows"))?;
    let mut total = 0usize;
    let mut previous = 0i64;
    for row in rows {
        let obj = row
            .as_object()
            .filter(|o| o.len() == 2)
            .ok_or_else(|| error("argument.invalid", "invalid exact recovery row"))?;
        let ordinal = obj
            .get("ordinal")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<i64>().ok().filter(|n| n.to_string() == s))
            .filter(|n| *n > previous)
            .ok_or_else(|| error("argument.invalid", "invalid exact recovery row ordinal"))?;
        previous = ordinal;
        let body = obj
            .get("body")
            .and_then(Value::as_array)
            .ok_or_else(|| error("argument.invalid", "invalid exact recovery body"))?;
        total = total
            .checked_add(body.len())
            .ok_or_else(|| error("argument.invalid", "exact recovery page too large"))?;
        if total > 4_194_304 || body.iter().any(|v| v.as_u64().is_none_or(|b| b > 255)) {
            return Err(error("argument.invalid", "invalid exact recovery body"));
        }
    }
    let claim =
        json!({"version":1,"token":token,"cursorLast":last,"more":page["more"],"rawRecords":rows});
    let digest = Sha256::digest(claim.to_string().as_bytes()).to_vec();
    Ok((claim, digest))
}

fn ensure_legacy_exact_marker(connection: &Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS legacy_exact_receipt(id INTEGER PRIMARY KEY CHECK(id=1), digest BLOB NOT NULL, receipt TEXT, state TEXT NOT NULL CHECK(state IN ('outstanding','retired')), claim TEXT);")?;
    let has_state: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('legacy_exact_receipt') WHERE name='state')",
        [],
        |r| r.get(0),
    )?;
    if !has_state {
        // An earlier unreleased implementation wrote only the auxiliary table.
        // Upgrading that table preserves its outstanding exact receipt without
        // changing either v1 core table.
        connection.execute_batch("ALTER TABLE legacy_exact_receipt ADD COLUMN state TEXT NOT NULL DEFAULT 'outstanding' CHECK(state IN ('outstanding','retired'));")?;
    }
    let has_claim: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('legacy_exact_receipt') WHERE name='claim')",
        [],
        |r| r.get(0),
    )?;
    if !has_claim {
        connection.execute_batch("ALTER TABLE legacy_exact_receipt ADD COLUMN claim TEXT;")?;
    }
    Ok(())
}

fn retired_exact_result(connection: &Connection, claim: &Value, digest: &[u8]) -> Result<Value> {
    let (stored, receipt, retired) = legacy_exact_state(connection)?
        .ok_or_else(|| error("argument.invalid", "exact recovery receipt is absent"))?;
    let receipt = receipt.ok_or_else(|| {
        error(
            "argument.invalid",
            "exact recovery has not been acknowledged",
        )
    })?;
    if !retired || stored != digest {
        return Err(error(
            "argument.invalid",
            "exact recovery receipt differs or is not retired",
        ));
    }
    if receipt["token"] != claim["token"]
        || receipt["records"].as_u64()
            != claim["rawRecords"].as_array().map(|rows| rows.len() as u64)
    {
        return Err(error(
            "storage.corrupt",
            "exact recovery receipt differs from claim",
        ));
    }
    Ok(json!({"version":1,"token":receipt["token"],"retired":true}))
}

#[allow(
    clippy::type_complexity,
    reason = "The exact legacy receipt tuple mirrors the fixed SQLite row shape."
)]
fn legacy_exact_state(connection: &Connection) -> Result<Option<(Vec<u8>, Option<Value>, bool)>> {
    let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='legacy_exact_receipt')", [], |r| r.get(0))?;
    if !exists {
        return Ok(None);
    }
    let has_state: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('legacy_exact_receipt') WHERE name='state')",
        [],
        |r| r.get(0),
    )?;
    if !has_state {
        return Err(error(
            "storage.schema",
            "exact recovery receipt needs auxiliary-state upgrade",
        ));
    }
    let count: i64 =
        connection.query_row("SELECT count(*) FROM legacy_exact_receipt", [], |r| {
            r.get(0)
        })?;
    if count > 1 {
        return Err(error(
            "storage.corrupt",
            "invalid exact recovery receipt count",
        ));
    }
    if count == 0 {
        return Ok(None);
    }
    if count != 1 {
        return Err(error(
            "storage.corrupt",
            "exact recovery receipt state is missing or invalid",
        ));
    }
    let entry: Option<(i64, Vec<u8>, Option<String>, String, Option<String>)> = connection
        .query_row(
            "SELECT id,digest,receipt,state,claim FROM legacy_exact_receipt",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    if let Some((id, digest, text, state, claim)) = entry {
        if id != 1 || digest.len() != 32 {
            return Err(error(
                "storage.corrupt",
                "invalid exact recovery commitment",
            ));
        }
        let receipt = if let Some(text) = text {
            let receipt = decode(&text)?;
            if receipt.as_object().is_none_or(|o| o.len() != 4)
                || receipt["version"] != 1
                || receipt["acknowledged"] != true
                || receipt["token"]
                    .as_str()
                    .is_none_or(|s| s.is_empty() || s.len() > 96)
                || receipt["records"]
                    .as_u64()
                    .is_none_or(|n| !(1..=2048).contains(&n))
            {
                return Err(error("storage.corrupt", "invalid exact recovery receipt"));
            }
            Some(receipt)
        } else {
            None
        };
        if state != "outstanding" && state != "retired" {
            return Err(error(
                "storage.corrupt",
                "invalid exact recovery receipt state",
            ));
        }
        let claim = claim.ok_or_else(|| {
            error(
                "storage.schema",
                "exact recovery claim predates durable raw evidence",
            )
        })?;
        let claim: Value = decode(&claim)?;
        let (_, stored_digest) = legacy_exact_claim(&claim)?;
        if stored_digest != digest {
            return Err(error(
                "storage.corrupt",
                "exact recovery commitment differs from durable claim",
            ));
        }
        let retired = state == "retired";
        if retired && receipt.is_none() {
            return Err(error("storage.corrupt", "retired receipt is missing"));
        }
        Ok(Some((digest, receipt, retired)))
    } else {
        Err(error(
            "storage.corrupt",
            "exact recovery receipt is missing",
        ))
    }
}

fn validate_legacy_recording_page(connection: &Connection, filter: &str, last: i64) -> Result<()> {
    let sql = format!(
        "SELECT count(*) BETWEEN 1 AND 2048 AND coalesce(sum(octet_length(body)),0)<=4194304 AND coalesce(sum(CASE WHEN bytes!=octet_length(body) OR NOT json_valid(body) THEN 1 ELSE 0 END),0)=0 FROM records WHERE {filter}"
    );
    let valid: bool = connection.query_row(&sql, [last], |row| row.get(0))?;
    if !valid {
        return Err(error(
            "storage.corrupt",
            "prepared prefix is invalid or missing",
        ));
    }
    Ok(())
}

fn journal_path(
    authorities: &JournalAuthorities,
    journal: &Arc<ContinuationJournal>,
) -> Result<PathBuf> {
    authorities
        .journals
        .iter()
        .find_map(|(path, current)| Arc::ptr_eq(current, journal).then(|| path.clone()))
        .ok_or_else(|| {
            error(
                "storage.owner-conflict",
                "ordered recording requires registry authority",
            )
        })
}

/// Reserve central ownership and its strong process supervisor atomically.
pub(crate) fn register_ordered_supervisor<T, E>(
    journal: &Arc<ContinuationJournal>,
    producer: &str,
    create: impl FnOnce() -> std::result::Result<Arc<T>, E>,
) -> std::result::Result<Arc<T>, E>
where
    T: OrderedSupervisor + std::any::Any + Send + Sync + 'static,
    E: From<JournalError>,
{
    let mut state = authorities().map_err(E::from)?;
    let path = journal_path(&state, journal).map_err(E::from)?;
    if state.ordered_owners.contains_key(&path)
        || state.legacy_owners.get(&path).is_some_and(|owners| {
            owners
                .iter()
                .filter_map(|owner| owner.session.upgrade())
                .any(|session| !session.collection_sealed())
        })
    {
        return Err(E::from(error(
            "storage.owner-conflict",
            "ordered producer is already supervised",
        )));
    }
    let owner = create()?;
    state.ordered_owners.insert(
        path,
        SupervisedOrderedOwner {
            producer: producer.to_owned(),
            owner: Some(owner.clone()),
            stop: owner.clone(),
        },
    );
    Ok(owner)
}
pub(crate) fn begin_supervised_ordered(
    journal: &Arc<ContinuationJournal>,
    producer: &str,
    collection: &str,
) -> Result<()> {
    let state = authorities()?;
    let path = journal_path(&state, journal)?;
    if !state
        .ordered_owners
        .get(&path)
        .is_some_and(|slot| slot.producer == producer && slot.owner.is_some())
    {
        return Err(error(
            "storage.owner-conflict",
            "ordered producer is not supervised",
        ));
    }
    journal.begin_ordered(producer, collection)
}
pub(crate) fn release_ordered_supervisor(
    journal: &Arc<ContinuationJournal>,
    producer: &str,
) -> Result<()> {
    let mut state = authorities()?;
    let path = journal_path(&state, journal)?;
    if !state
        .ordered_owners
        .get(&path)
        .is_some_and(|slot| slot.producer == producer)
    {
        return Err(error(
            "storage.owner-conflict",
            "ordered supervisor slot differs",
        ));
    }
    state.ordered_owners.remove(&path);
    Ok(())
}

/// Attach and publish the exact collection owner under the journal authority.
pub(crate) fn attach_legacy_recording_owner(
    journal: &Arc<ContinuationJournal>,
    session: &Arc<dyn crate::continuation::ContinuationSession>,
    recovery_stopped: &Arc<std::sync::atomic::AtomicBool>,
    recovery_wake: &Arc<tokio::sync::Notify>,
    context: Value,
) -> crate::continuation::Result<()> {
    let mut state = authorities().map_err(crate::continuation::recording_failure)?;
    let path = journal_path(&state, journal).map_err(crate::continuation::recording_failure)?;
    if state.ordered_owners.contains_key(&path) {
        return Err(crate::continuation::recording_failure(error(
            "storage.owner-conflict",
            "another producer owns this journal",
        )));
    }
    if journal
        .status()
        .map_err(crate::continuation::recording_failure)?["accepting"]
        != true
    {
        return Err(
            json!({"code":"lifecycle.invalid-state","domain":"restoration","operation":"continuation","detail":"recording is not accepting native collection"}),
        );
    }
    session.attach_journal(Arc::clone(journal), context)?;
    let owners = state.legacy_owners.entry(path).or_default();
    owners.retain(|owner| owner.session.strong_count() != 0);
    owners.push(LegacyRecordingOwner {
        session: Arc::downgrade(session),
        recovery_stopped: Arc::downgrade(recovery_stopped),
        recovery_wake: Arc::downgrade(recovery_wake),
    });
    Ok(())
}

/// Seal the live collection, whichever host owns it, before stopping storage.
pub fn stop_legacy_recording(
    journal: &Arc<ContinuationJournal>,
) -> crate::continuation::Result<Value> {
    let mut state = authorities().map_err(crate::continuation::recording_failure)?;
    let path = journal_path(&state, journal).map_err(crate::continuation::recording_failure)?;
    if state.ordered_owners.contains_key(&path) {
        return Err(crate::continuation::recording_failure(error(
            "storage.owner-conflict",
            "ordered producer owns this journal",
        )));
    }
    if let Some(owners) = state.legacy_owners.get(&path) {
        for owner in owners {
            if let Some(session) = owner.session.upgrade() {
                session.seal_collection()?;
                if let Some(stopped) = owner.recovery_stopped.upgrade() {
                    stopped.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                if let Some(wake) = owner.recovery_wake.upgrade() {
                    wake.notify_one();
                }
            }
        }
    }
    let mut receipt = journal
        .stop()
        .map_err(crate::continuation::recording_failure)?;
    state.legacy_owners.remove(&path);
    receipt["radioRelease"] = json!("not-requested");
    Ok(receipt)
}

/// Nonblocking process admission happens before opening the recording SQLite
/// connection. The zero-byte sidecar is not deleted: unlinking a held OS lock
/// would permit a second authority. Crash/close releases the OS lock itself.
fn lock_authority(path: &Path) -> Result<std::fs::File> {
    let lock_path = path.with_extension("authority");
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
    let file = options.open(&lock_path).map_err(|_| {
        error(
            "storage.permission",
            "recording authority path cannot be opened",
        )
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| error("storage.io", "recording authority cannot be inspected"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != 0 {
        return Err(error(
            "storage.permission",
            "recording authority is not an empty regular file",
        ));
    }
    file.try_lock().map_err(|failure| match failure {
        std::fs::TryLockError::WouldBlock => {
            error("storage.busy", "recording is owned by another process")
        }
        std::fs::TryLockError::Error(_) => {
            error("storage.io", "recording process authority cannot be locked")
        }
    })?;
    Ok(file)
}

#[cfg(unix)]
fn path_identity(path: &Path) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| error("storage.io", "recording path cannot be inspected"))?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn handle_identity(file: &std::fs::File) -> Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the File retains a valid handle throughout this call and `info`
    // supplies an initialized, correctly sized writable output structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(error(
            "storage.io",
            "recording path identity cannot be inspected",
        ));
    }
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

#[cfg(windows)]
fn path_identity(path: &Path) -> Result<(u64, u64)> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x02200000) // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
        .open(path)
        .map_err(|_| error("storage.io", "recording path cannot be inspected"))?;
    handle_identity(&file)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalQuota {
    pub max_bytes: u64,
    pub max_records: u64,
}

#[cfg(test)]
pub(crate) fn private_fixture_directory(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalError {
    pub kind: &'static str,
    pub detail: &'static str,
    pub operation: &'static str,
    pub sqlite_extended_code: Option<i32>,
    pub sqlite_code: Option<String>,
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.detail)
    }
}
impl std::error::Error for JournalError {}
type Result<T> = std::result::Result<T, JournalError>;
fn error(kind: &'static str, detail: &'static str) -> JournalError {
    JournalError {
        kind,
        detail,
        operation: "journal",
        sqlite_extended_code: None,
        sqlite_code: None,
    }
}
impl JournalError {
    pub(crate) fn invalid(detail: &'static str) -> Self {
        error("argument.invalid", detail)
    }
    fn at(mut self, operation: &'static str) -> Self {
        self.operation = operation;
        self
    }
}
impl From<rusqlite::Error> for JournalError {
    fn from(value: rusqlite::Error) -> Self {
        use rusqlite::ErrorCode;
        let kind = match value.sqlite_error_code() {
            Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => "storage.corrupt",
            Some(ErrorCode::DiskFull) => "storage.full",
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => "storage.busy",
            Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly) => "storage.permission",
            _ => "storage.io",
        };
        let mut failure = error(
            kind,
            "SQLite operation failed; no successful persistence is claimed",
        );
        if let Some(native) = value.sqlite_error() {
            failure.sqlite_extended_code = Some(native.extended_code);
            failure.sqlite_code = Some(format!("{:?}", native.code));
        }
        failure
    }
}

struct LimitedBytes {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for LimitedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("journal JSON limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encoded(value: &Value, limit: usize) -> Result<String> {
    fn depth(value: &Value, level: usize, remaining: &mut usize) -> Result<()> {
        if level > 64 || *remaining == 0 {
            return Err(error(
                "argument.invalid",
                "journal JSON structural bound exceeded",
            ));
        }
        *remaining -= 1;
        match value {
            Value::Array(values) => {
                for value in values {
                    depth(value, level + 1, remaining)?;
                }
            }
            Value::Object(values) => {
                for value in values.values() {
                    depth(value, level + 1, remaining)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    depth(value, 0, &mut 65536)?;
    let mut output = LimitedBytes {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut output, value)
        .map_err(|_| error("argument.invalid", "journal JSON exceeds its byte bound"))?;
    String::from_utf8(output.bytes)
        .map_err(|_| error("argument.invalid", "journal JSON is invalid"))
}
fn decode(value: &str) -> Result<Value> {
    serde_json::from_str(value)
        .map_err(|_| error("storage.corrupt", "stored journal JSON is invalid"))
}
fn unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}
fn sql_integer(value: u64) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| error("argument.invalid", "journal integer exceeds SQLite range"))
}

/// Result of one committed [`ContinuationJournal::append_batch`] transaction.
/// The `accepted` leading records hold consecutive ordinals starting at
/// `first_ordinal`, the ordinal the batch's first record was offered at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendBatch {
    pub accepted: usize,
    pub first_ordinal: i64,
    pub rejected: Option<AppendRejection>,
}

/// Source observations and raw bytes that could not enter a recorded journal.
/// These are distinct from serialized journal bytes and from unknown historic loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedLoss {
    pub items: u64,
    /// None means the source did not report a raw-byte count.
    pub bytes: Option<u64>,
}

/// Durable receipt for a producer-supplied loss occurrence identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedLossReceipt {
    pub occurrence: [u8; 40],
}

impl Default for RecordedLoss {
    fn default() -> Self {
        Self {
            items: 0,
            bytes: Some(0),
        }
    }
}

impl RecordedLoss {
    fn add(self, other: Self) -> Self {
        Self {
            items: self.items.saturating_add(other.items),
            bytes: self
                .bytes
                .zip(other.bytes)
                .map(|(left, right)| left.saturating_add(right)),
        }
    }
    fn from_report(report: Option<&Value>) -> Self {
        let loss = report.and_then(|entry| entry.get("knownRecordedLoss"));
        Self {
            items: loss.and_then(|value| value["items"].as_u64()).unwrap_or(0),
            bytes: loss.map_or(Some(0), |value| value["bytes"].as_u64()),
        }
    }
    fn value(self) -> Value {
        let mut result = json!({"items":self.items});
        if let Some(bytes) = self.bytes {
            result["bytes"] = json!(bytes);
        }
        result
    }
}

/// Capacity was reached at the first record after the accepted prefix. That
/// record carries the committed loss marker; `not_attempted` later records of
/// the batch were never admitted and are the caller's to account for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendRejection {
    pub error: JournalError,
    pub not_attempted: usize,
}

// Directly opened handles share the SQLite connection as well as the volatile
// admission fence. This makes checking a retained failure and committing an
// append one serialized operation, even when persistence of the fence failed.
#[derive(Default)]
struct LocalJournalFailures {
    runtime_failure: Mutex<Option<JournalError>>,
    collection_failure: Mutex<Option<Value>>,
    pending_recorded_loss: Mutex<RecordedLoss>,
    // Exact attempts survive a failed or ambiguous commit until reconciliation.
    separate_occurrences: Mutex<Vec<DiagnosticEntry>>,
    #[cfg(test)]
    failed_append_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl LocalJournalFailures {
    fn can_discard(&self) -> bool {
        let Ok(runtime) = self.runtime_failure.lock() else {
            return false;
        };
        let Ok(collection) = self.collection_failure.lock() else {
            return false;
        };
        let Ok(pending) = self.pending_recorded_loss.lock() else {
            return false;
        };
        let Ok(occurrences) = self.separate_occurrences.lock() else {
            return false;
        };
        runtime.is_none()
            && *pending == RecordedLoss::default()
            && occurrences.is_empty()
            && collection
                .as_ref()
                .is_none_or(|report| report["persisted"] == true)
    }
}

// The zero identity was reserved for an obsolete synthetic v5 summary row.
const DIAGNOSTIC_LEDGER_SCHEMA: &str = "CREATE TABLE diagnostic_ledger(sequence INTEGER PRIMARY KEY AUTOINCREMENT, occurrence BLOB NOT NULL UNIQUE, payload TEXT NOT NULL, acknowledged INTEGER NOT NULL DEFAULT 0 CHECK(acknowledged IN (0,1)), retired INTEGER NOT NULL DEFAULT 0 CHECK(retired IN (0,1)));";
const DIAGNOSTIC_OWNER_SCHEMA: &str = "CREATE TABLE diagnostic_owners(occurrence BLOB PRIMARY KEY, payload_limit INTEGER NOT NULL, state TEXT NOT NULL CHECK(state IN ('reserved','committed','reclaimed')), payload TEXT);";
// A transition compares one bounded host-supplied chunk per transaction. The
// durable offset makes a stopped host resume exactly, without retaining an
// unbounded IPC request or trusting a digest in place of raw bytes.
// A v5 journal can already carry this later, independent side table after an
// interrupted downgrade/repair fixture. Its absence is what v10 repairs; its
// presence must not rewrite the v5 primary rows or authority.
const DIAGNOSTIC_TRANSITION_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS diagnostic_transitions(occurrence BLOB PRIMARY KEY, action TEXT NOT NULL CHECK(action IN ('check','acknowledge','retire','confirm','release')), total INTEGER NOT NULL, progressed INTEGER NOT NULL);";
const DIAGNOSTIC_OWNER_LIMIT: i64 = 64;
const DIAGNOSTIC_PAYLOAD_LIMIT: usize = 64 * 1024;
const DIAGNOSTIC_RECEIPT_LIMIT: usize = DIAGNOSTIC_PAYLOAD_LIMIT * 6 + 4096;

fn normalized_schema_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(';')
        .replace("CREATE TABLE IF NOT EXISTS ", "CREATE TABLE ")
}

/// A stale user-version alone must not make an unrelated or malformed table
/// look like an owned later migration. We accept the exact table produced by
/// this migration only; constraints live in SQLite's stored table SQL, and no
/// named indexes belong to this table.
fn ensure_diagnostic_transition_schema(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='diagnostic_transitions'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if normalized_schema_sql(&existing) != normalized_schema_sql(DIAGNOSTIC_TRANSITION_SCHEMA) {
            return Err(error(
                "storage.schema",
                "diagnostic transition schema is incompatible; file retained",
            ));
        }
        let has_named_index: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND tbl_name='diagnostic_transitions' AND sql IS NOT NULL)",
            [],
            |row| row.get(0),
        )?;
        if has_named_index {
            return Err(error(
                "storage.schema",
                "diagnostic transition indexes are incompatible; file retained",
            ));
        }
    }
    tx.execute_batch(DIAGNOSTIC_TRANSITION_SCHEMA)?;
    Ok(())
}
const DIAGNOSTIC_PAGE_LIMIT: u32 = 64 * 1024;
const DIAGNOSTIC_OCCURRENCE_PAGE_LIMIT: usize = 128;

pub(crate) type DiagnosticId = [u8; 40];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticOwnerState {
    Reserved,
    Committed,
    Reclaimed,
}

#[allow(
    clippy::collapsible_if,
    clippy::result_large_err,
    clippy::too_many_arguments,
    clippy::type_complexity,
    reason = "The durable journal implementation preserves fixed private receipt, recovery, and callback shapes; restructuring them would change its retained ownership boundaries."
)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Diagnostic ownership records are retained for durable journal recovery tests."
    )
)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DiagnosticOwner {
    pub(crate) id: DiagnosticId,
    pub(crate) payload_limit: usize,
    pub(crate) state: DiagnosticOwnerState,
    pub(crate) bound: bool,
}

fn diagnostic_owner_state(state: &str) -> Result<DiagnosticOwnerState> {
    match state {
        "reserved" => Ok(DiagnosticOwnerState::Reserved),
        "committed" => Ok(DiagnosticOwnerState::Committed),
        "reclaimed" => Ok(DiagnosticOwnerState::Reclaimed),
        _ => Err(error("storage.corrupt", "invalid diagnostic owner state")),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct DiagnosticPage {
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub reclaimed: bool,
    pub durable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticCategory {
    Collection,
    Runtime,
}

#[derive(Clone, Debug)]
pub(crate) struct DiagnosticEntry {
    pub(crate) id: DiagnosticId,
    pub(crate) category: DiagnosticCategory,
    pub(crate) failure: JournalError,
    pub(crate) loss: RecordedLoss,
    pub(crate) durable: bool,
    pub(crate) persistence_failure: Option<JournalError>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Diagnostic slot capacity fields are retained for the durable diagnostic protocol."
    )
)]
#[derive(Debug)]
struct DiagnosticSlot {
    id: DiagnosticId,
    charge: usize,
    payload_limit: usize,
    entry: Option<DiagnosticEntry>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Diagnostic ledger identity fields are exercised by durable recovery tests."
    )
)]
#[derive(Debug, Default)]
struct DiagnosticLedger {
    incarnation: Option<[u8; 16]>,
    next_sequence: u64,
    charged: usize,
    slots: Vec<DiagnosticSlot>,
}

/// Pins the SQLite read transaction and the in-process slots until dropped.
pub(crate) struct DiagnosticOccurrenceScan<'a> {
    connection: MutexGuard<'a, Connection>,
    ledger: MutexGuard<'a, DiagnosticLedger>,
    pending: Vec<DiagnosticEntry>,
    after: Option<DiagnosticId>,
}

impl DiagnosticOccurrenceScan<'_> {
    /// Read through the same cut as discovery; no other journal call is needed.
    pub(crate) fn diagnostic_page(
        &self,
        id: DiagnosticId,
        offset: u64,
        max_bytes: u32,
    ) -> Result<DiagnosticPage> {
        let local = self
            .ledger
            .slots
            .iter()
            .find_map(|slot| (slot.id == id).then_some(slot.entry.as_ref()).flatten())
            .or_else(|| self.pending.iter().find(|entry| entry.id == id));
        let stored = diagnostic_db_page(&self.connection, id, offset, max_bytes);
        match (stored, local) {
            (Ok(page), Some(entry)) => {
                // An ambiguous commit can leave both forms. Validate the whole
                // identity in bounded chunks before returning even one page.
                let mut at = 0;
                loop {
                    let local = diagnostic_local_page(entry, at, DIAGNOSTIC_PAGE_LIMIT)?;
                    if page.total_bytes != local.total_bytes {
                        return Err(error(
                            "storage.identity",
                            "diagnostic occurrence payload differs",
                        ));
                    }
                    let saved =
                        diagnostic_db_page(&self.connection, id, at, DIAGNOSTIC_PAGE_LIMIT)?;
                    if saved.bytes != local.bytes {
                        return Err(error(
                            "storage.identity",
                            "diagnostic occurrence payload differs",
                        ));
                    }
                    at += local.bytes.len() as u64;
                    if at == local.total_bytes {
                        break;
                    }
                }
                Ok(page)
            }
            (Ok(page), None) => Ok(page),
            (Err(failure), Some(entry)) if failure.kind == "storage.identity" => {
                diagnostic_local_page(entry, offset, max_bytes)
            }
            (Err(failure), _) => Err(failure),
        }
    }

    pub(crate) fn next_page(&mut self, limit: usize) -> Result<Vec<DiagnosticId>> {
        if limit == 0 || limit > DIAGNOSTIC_OCCURRENCE_PAGE_LIMIT {
            return Err(error(
                "argument.invalid",
                "invalid diagnostic occurrence page limit",
            ));
        }
        let mut ids = Vec::new();
        ids.try_reserve_exact(limit)
            .map_err(|_| error("storage.full", "diagnostic page allocation failed"))?;
        let sql = if self.after.is_some() {
            "SELECT occurrence FROM diagnostic_ledger WHERE occurrence>?1 ORDER BY occurrence LIMIT ?2"
        } else {
            "SELECT occurrence FROM diagnostic_ledger WHERE ?1 IS NULL ORDER BY occurrence LIMIT ?2"
        };
        let mut statement = self.connection.prepare(sql)?;
        let mut rows = statement.query(params![
            self.after.as_ref().map(|id| id.as_slice()),
            limit as i64
        ])?;
        while let Some(row) = rows.next()? {
            let raw = row
                .get_ref(0)?
                .as_blob()
                .map_err(|_| error("storage.corrupt", "invalid diagnostic occurrence identity"))?;
            ids.push(
                raw.try_into().map_err(|_| {
                    error("storage.corrupt", "invalid diagnostic occurrence identity")
                })?,
            );
        }
        for slot in self
            .ledger
            .slots
            .iter()
            .filter(|slot| slot.entry.is_some() && self.after.is_none_or(|id| slot.id > id))
        {
            match ids.binary_search(&slot.id) {
                Ok(_) => {}
                Err(index) if index < limit => {
                    if ids.len() == limit {
                        ids.pop();
                    }
                    ids.insert(index, slot.id);
                }
                Err(_) => {}
            }
        }
        for entry in self
            .pending
            .iter()
            .filter(|entry| self.after.is_none_or(|id| entry.id > id))
        {
            match ids.binary_search(&entry.id) {
                Ok(_) => {}
                Err(index) if index < limit => {
                    if ids.len() == limit {
                        ids.pop();
                    }
                    ids.insert(index, entry.id);
                }
                Err(_) => {}
            }
        }
        if let Some(id) = ids.last() {
            self.after = Some(*id);
        }
        Ok(ids)
    }
}

impl Drop for DiagnosticOccurrenceScan<'_> {
    fn drop(&mut self) {
        if let Err(failure) = self.connection.execute_batch("ROLLBACK") {
            eprintln!("diagnostic occurrence snapshot rollback failed: {failure}");
        }
    }
}

#[derive(Clone, Copy)]
pub enum DiagnosticExactAction {
    CheckReplay,
    Acknowledge,
    Retire,
    Confirm,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagnosticTransitionProgress {
    pub offset: u64,
    pub total: u64,
    pub complete: bool,
}

fn diagnostic_action_name(action: DiagnosticExactAction) -> &'static str {
    match action {
        DiagnosticExactAction::CheckReplay => "check",
        DiagnosticExactAction::Acknowledge => "acknowledge",
        DiagnosticExactAction::Retire => "retire",
        DiagnosticExactAction::Confirm => "confirm",
        DiagnosticExactAction::Release => "release",
    }
}

pub(crate) struct DiagnosticExact<'a> {
    connection: MutexGuard<'a, Connection>,
    ledger: MutexGuard<'a, DiagnosticLedger>,
    id: DiagnosticId,
    action: DiagnosticExactAction,
    row: Option<(i64, bool, bool)>,
    owner: Option<(i64, String, Option<u64>)>,
    total: u64,
    offset: u64,
    local: Option<String>,
    failed: bool,
    open: bool,
}

impl DiagnosticExact<'_> {
    pub(crate) fn compare(&mut self, bytes: &[u8]) -> Result<()> {
        if self.failed
            || bytes.is_empty()
            || bytes.len() > DIAGNOSTIC_PAGE_LIMIT as usize
            || self
                .offset
                .checked_add(bytes.len() as u64)
                .is_none_or(|end| end > self.total)
        {
            self.failed = true;
            return Err(error(
                "storage.identity",
                "invalid diagnostic comparison chunk",
            ));
        }
        let offset = usize::try_from(self.offset).map_err(|_| {
            error(
                "storage.corrupt",
                "diagnostic offset exceeds platform range",
            )
        })?;
        let mut buffer = vec![0; bytes.len()];
        let mut matches = true;
        for (table, rowid) in self
            .row
            .map(|row| ("diagnostic_ledger", row.0))
            .into_iter()
            .chain(
                self.owner
                    .as_ref()
                    .and_then(|owner| owner.2.map(|_| ("diagnostic_owners", owner.0))),
            )
        {
            let blob = self
                .connection
                .blob_open("main", table, "payload", rowid, true)?;
            blob.read_at_exact(&mut buffer, offset)?;
            blob.close()?;
            matches &= buffer == bytes;
        }
        if let Some(local) = &self.local {
            matches &= local.as_bytes().get(offset..offset + bytes.len()) == Some(bytes);
        }
        if !matches {
            self.failed = true;
            return Err(error(
                "storage.identity",
                "diagnostic occurrence payload differs",
            ));
        }
        self.offset += bytes.len() as u64;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        if self.failed || self.offset != self.total {
            return Err(error(
                "storage.identity",
                "diagnostic comparison is incomplete",
            ));
        }
        let id = self.id.as_slice();
        match self.action {
            DiagnosticExactAction::CheckReplay => {
                if self
                    .owner
                    .as_ref()
                    .is_some_and(|(_, state, _)| state == "reclaimed")
                {
                    return Err(error(
                        "storage.reclaimed",
                        "diagnostic occurrence was reclaimed",
                    ));
                }
                if self.row.is_none() {
                    return Err(error(
                        "storage.identity",
                        "diagnostic occurrence is not retained",
                    ));
                }
            }
            DiagnosticExactAction::Acknowledge | DiagnosticExactAction::Retire => {
                if self.row.is_none() {
                    return Err(error(
                        "storage.identity",
                        "diagnostic occurrence is not retained",
                    ));
                }
                let sql = if matches!(self.action, DiagnosticExactAction::Acknowledge) {
                    "UPDATE diagnostic_ledger SET acknowledged=1 WHERE occurrence=?1"
                } else {
                    "UPDATE diagnostic_ledger SET retired=1 WHERE occurrence=?1"
                };
                self.connection.execute(sql, [id])?;
            }
            DiagnosticExactAction::Confirm => {
                if let Some((_, acknowledged, retired)) = self.row {
                    if !acknowledged || !retired {
                        return Err(error(
                            "storage.busy",
                            "diagnostic retirement is not complete",
                        ));
                    }
                    match &self.owner {
                        Some((_, state, None))
                            if state == "reclaimed"
                                && self.total > DIAGNOSTIC_RECEIPT_LIMIT as u64 => {}
                        Some((_, state, Some(_))) if state == "committed" => {
                            self.connection.execute("UPDATE diagnostic_owners SET state='reclaimed' WHERE occurrence=?1", [id])?;
                            self.connection.execute(
                                "DELETE FROM diagnostic_ledger WHERE occurrence=?1",
                                [id],
                            )?;
                        }
                        Some(_) => {
                            return Err(error("storage.identity", "diagnostic owner differs"));
                        }
                        None => {
                            let count: i64 = self.connection.query_row(
                                "SELECT count(*) FROM diagnostic_owners",
                                [],
                                |row| row.get(0),
                            )?;
                            if count >= DIAGNOSTIC_OWNER_LIMIT {
                                return Err(error(
                                    "storage.full",
                                    "diagnostic confirmation receipts are full",
                                ));
                            }
                            if self.total > DIAGNOSTIC_RECEIPT_LIMIT as u64 {
                                self.connection.execute(
                                    "INSERT INTO diagnostic_owners VALUES(?1,0,'reclaimed',NULL)",
                                    [id],
                                )?;
                            } else {
                                self.connection.execute("INSERT INTO diagnostic_owners SELECT occurrence,0,'reclaimed',payload FROM diagnostic_ledger WHERE occurrence=?1", [id])?;
                                self.connection.execute(
                                    "DELETE FROM diagnostic_ledger WHERE occurrence=?1",
                                    [id],
                                )?;
                            }
                        }
                    }
                } else if !self
                    .owner
                    .as_ref()
                    .is_some_and(|(_, state, size)| state == "reclaimed" && size.is_some())
                {
                    return Err(error(
                        "storage.identity",
                        "diagnostic confirmation is not retained",
                    ));
                }
                if let Some(index) = self.ledger.slots.iter().position(|slot| slot.id == self.id) {
                    let slot = self.ledger.slots.remove(index);
                    self.ledger.charged -= slot.charge;
                }
            }
            DiagnosticExactAction::Release => {
                let (_, state, size) = self.owner.as_ref().ok_or_else(|| {
                    error(
                        "storage.identity",
                        "diagnostic confirmation is not retained",
                    )
                })?;
                if state != "reclaimed" {
                    return Err(error(
                        "storage.identity",
                        "diagnostic confirmation is not retained",
                    ));
                }
                if size.is_none() {
                    if self.total <= DIAGNOSTIC_RECEIPT_LIMIT as u64
                        || !self.row.is_some_and(|(_, ack, retired)| ack && retired)
                    {
                        return Err(error(
                            "storage.identity",
                            "oversized diagnostic confirmation is not retained",
                        ));
                    }
                    self.connection
                        .execute("DELETE FROM diagnostic_ledger WHERE occurrence=?1", [id])?;
                } else if self.row.is_some() {
                    return Err(error(
                        "storage.identity",
                        "diagnostic confirmation has an unexpected row",
                    ));
                }
                self.connection
                    .execute("DELETE FROM diagnostic_owners WHERE occurrence=?1", [id])?;
            }
        }
        self.connection.execute_batch("COMMIT")?;
        self.open = false;
        Ok(())
    }
}

#[allow(
    clippy::collapsible_if,
    reason = "Rust 2021 cannot express the guarded rollback with Clippy's let-chain suggestion."
)]
impl Drop for DiagnosticExact<'_> {
    fn drop(&mut self) {
        if self.open {
            if let Err(failure) = self.connection.execute_batch("ROLLBACK") {
                eprintln!("diagnostic comparison rollback failed: {failure}");
            }
        }
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Diagnostic permits retain ownership until the durable diagnostic path consumes them."
    )
)]
#[derive(Debug)]
pub(crate) struct DiagnosticPermit {
    ledger: Arc<Mutex<DiagnosticLedger>>,
    id: DiagnosticId,
    consumed: bool,
}

#[allow(
    clippy::collapsible_if,
    reason = "Rust 2021 cannot express the lock-and-slot cleanup with Clippy's let-chain suggestion."
)]
impl Drop for DiagnosticPermit {
    fn drop(&mut self) {
        if self.consumed {
            return;
        }
        // A poisoned ledger remains owned and cannot be reused or discarded.
        if let Ok(mut ledger) = self.ledger.lock() {
            if let Some(index) = ledger
                .slots
                .iter()
                .position(|slot| slot.id == self.id && slot.entry.is_none())
            {
                let slot = ledger.slots.remove(index);
                ledger.charged -= slot.charge;
            }
        }
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Diagnostic recording errors retain the permit for retry semantics."
    )
)]
#[derive(Debug)]
pub(crate) struct DiagnosticRecordError {
    pub(crate) error: JournalError,
    pub(crate) permit: DiagnosticPermit,
    pub(crate) category: DiagnosticCategory,
    pub(crate) failure: JournalError,
    pub(crate) loss: RecordedLoss,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Diagnostic receipts are consumed by the retained retry protocol."
    )
)]
#[derive(Debug)]
pub(crate) struct DiagnosticReceipt {
    pub(crate) id: DiagnosticId,
    pub(crate) durable: bool,
    pub(crate) persistence_failure: Option<JournalError>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Prepared diagnostics remain part of the explicit durable retry protocol."
    )
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedDiagnostic {
    pub(crate) payload: String,
    pub(crate) durable: bool,
}

#[derive(Clone, Copy)]
enum DiagnosticTransition {
    Acknowledge,
    Retire,
    ConfirmRetirement,
}

struct SharedJournal {
    connection: Mutex<Connection>,
    apple_failure_spool_gate: Mutex<()>,
    diagnostic_ledger: Arc<Mutex<DiagnosticLedger>>,
    failures: Mutex<Vec<Arc<LocalJournalFailures>>>,
    reader_space: Arc<tokio::sync::Notify>,
}

type SharedJournalMap = HashMap<(u64, u64), Arc<SharedJournal>>;

fn shared_journals() -> &'static Mutex<SharedJournalMap> {
    static SHARED: OnceLock<Mutex<SharedJournalMap>> = OnceLock::new();
    SHARED.get_or_init(Mutex::default)
}

fn shared_journal(
    identity: (u64, u64),
    connection: Connection,
) -> Result<(Arc<SharedJournal>, Arc<LocalJournalFailures>)> {
    let mut entries = shared_journals()
        .lock()
        .map_err(|_| error("storage.io", "journal ownership interrupted"))?;
    entries.retain(|_, shared| {
        if Arc::strong_count(shared) != 1 {
            return true;
        }
        let Ok(failures) = shared.failures.lock() else {
            return true;
        };
        let ledger_owned = shared
            .diagnostic_ledger
            .lock()
            .map_or(true, |ledger| !ledger.slots.is_empty());
        ledger_owned || failures.iter().any(|failure| !failure.can_discard())
    });
    if !entries.contains_key(&identity) && entries.len() >= AUTHORITY_HANDLES {
        return Err(error(
            "storage.busy",
            "journal ownership capacity reached; failures retained",
        ));
    }
    entries.entry(identity).or_insert_with(|| {
        Arc::new(SharedJournal {
            connection: Mutex::new(connection),
            apple_failure_spool_gate: Mutex::new(()),
            diagnostic_ledger: Arc::new(Mutex::new(DiagnosticLedger::default())),
            failures: Mutex::new(Vec::new()),
            reader_space: Arc::new(tokio::sync::Notify::new()),
        })
    });
    let shared = entries
        .get(&identity)
        .expect("journal just inserted")
        .clone();
    let local = {
        let mut failures = shared
            .failures
            .lock()
            .map_err(|_| error("storage.io", "journal failure ownership interrupted"))?;
        // An abandoned unpersisted failure remains reachable and can be retried
        // by a later handle. Live handles retain their separate retry deltas.
        let orphan = failures
            .iter()
            .find(|failure| Arc::strong_count(failure) == 1 && !failure.can_discard())
            .cloned();
        failures.retain(|failure| Arc::strong_count(failure) > 1 || !failure.can_discard());
        match orphan {
            Some(failure) => failure,
            None => {
                if failures.len() >= AUTHORITY_HANDLES {
                    return Err(error(
                        "storage.busy",
                        "journal failure owner capacity reached; failures retained",
                    ));
                }
                let failure = Arc::new(LocalJournalFailures::default());
                failures.push(failure.clone());
                failure
            }
        }
    };
    Ok((shared, local))
}

/// One handle to a durable recording with a shared delivery cursor and an
/// independent, bounded evidence-transfer snapshot cursor.
/// Authorized handles in one process share serialization and volatile failure
/// ownership, including failures SQLite could not persist. The host authorizes
/// access and enforces exclusive process ownership (as `JournalRegistry` does).
/// Tokens protect cursor identity, not access.
pub struct ContinuationJournal {
    authority: Option<std::fs::File>,
    file_identity: (u64, u64),
    journal_path: PathBuf,
    peer_failure_sidecar_path: PathBuf,
    peer_failure_ledger: Mutex<Weak<JournalPeerFailureRecoveryLedger>>,
    scan_failure_sidecar_path: PathBuf,
    // Released v1 kept raw scan evidence next to the primary journal.  This
    // remains an export-only input; it is never a v2 recovery authority.
    legacy_scan_failure_sidecar_path: PathBuf,
    scan_recovery_storage: Option<Arc<dyn ScanRecoveryStorageProtector>>,
    apple_failure_spool_path: PathBuf,
    shared: Arc<SharedJournal>,
    failures: Arc<LocalJournalFailures>,
    quota: JournalQuota,
    page_limit: u64,
    schema_version: i64,
    recording_id: String,
    declaration: String,
}

#[allow(
    clippy::collapsible_if,
    clippy::result_large_err,
    clippy::type_complexity,
    reason = "The established journal API preserves durable receipt and SQLite row representations."
)]
impl ContinuationJournal {
    /// Opens the host-private terminal-evidence sidecar while this journal's
    /// authority lock remains held. The primary recording schema is untouched,
    /// preserving released readers; the derived sidecar inherits the trusted
    /// directory selected for this recording and never accepts a caller path.
    pub fn terminal_evidence_ledger(
        &self,
        maximum_bytes: u64,
    ) -> std::result::Result<
        crate::terminal_evidence_ledger::TerminalEvidenceLedger,
        crate::terminal_evidence_ledger::TerminalEvidenceError,
    > {
        crate::terminal_evidence_ledger::TerminalEvidenceLedger::open(
            &self.journal_path.with_extension("terminal-evidence.sqlite"),
            maximum_bytes,
        )
    }
}

/// Exact native source-failure occurrence retained before the host tells a
/// platform callback that its overflow was accepted. `route` is host-defined
/// but stable for the source stream; `(route, epoch, sequence)` is the
/// ordering and idempotency identity.
#[derive(Debug, Clone, PartialEq)]
pub struct AppleNotificationFailureOccurrence {
    pub route: String,
    pub epoch: u64,
    pub sequence: u64,
    pub failure: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppleNotificationFailureCapture {
    pub capture_ordinal: u64,
    pub occurrence: AppleNotificationFailureOccurrence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppleNotificationFailureAppend {
    Committed,
    Full,
}

/// Bounded durable stream for Apple notification-source failure overflow.
/// The primary journal authority remains retained for this handle's lifetime.
pub struct JournalAppleNotificationFailureSpool {
    _journal: Arc<ContinuationJournal>,
    connection: Mutex<Connection>,
    quota: JournalQuota,
}

/// Host-configured, streaming durable handoff for one journal authority.
///
/// It intentionally owns a separate SQLite connection and file: the primary
/// journal can remain byte-for-byte compatible with a released v1 reader,
/// while the occurrence envelope, chunks and source-ACK receipt still commit
/// atomically within this sidecar. A caller obtains this only from an already
/// open [`ContinuationJournal`], so the journal's process authority lock stays
/// held for its lifetime.
pub struct JournalPeerFailureRecoveryLedger {
    // Retains the primary journal's authority lock for the whole sidecar
    // lifetime. The underscore is intentional: the lock, not a direct call,
    // is the ownership relationship.
    _journal: Arc<ContinuationJournal>,
    connection: Mutex<Connection>,
    recovered: Mutex<HashSet<[u8; 96]>>,
    startup_recovery: Mutex<bool>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Sidecar occurrences are retained for exact peer-failure recovery coverage."
    )
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DurablePeerFailureSidecarOccurrence {
    pub(crate) occurrence: DurablePeerFailureRecoveryOccurrence,
    pub(crate) source_ack_id: Option<String>,
    pub(crate) state: PeerFailureRecoveryState,
    pub(crate) staged_bytes: u64,
}

fn open_peer_failure_sidecar(path: &Path, quota: JournalQuota) -> Result<Connection> {
    open_exact_failure_sidecar(
        path,
        quota,
        PEER_FAILURE_SIDECAR_APPLICATION_ID,
        PEER_FAILURE_SIDECAR_SCHEMA_VERSION,
        PEER_FAILURE_SIDECAR_SCHEMA,
        "peer failure recovery",
    )
}

const SCAN_RECOVERY_DIRECTORY: &str = ".ubm-scan-recovery";

/// Resolve the only directory SQLite may use for v2 recovery.  Its parent is
/// verified first, so an untrusted principal cannot replace the child between
/// validation and SQLite creating `-journal`.
fn secure_scan_failure_sidecar_path(
    journal_path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<PathBuf> {
    let parent = journal_path.parent().ok_or_else(|| {
        error(
            "storage.permission",
            "journal path does not have a parent directory",
        )
    })?;
    validate_private_scan_recovery_parent(parent, protector)?;
    let directory = parent.join(SCAN_RECOVERY_DIRECTORY);
    let created = match std::fs::symlink_metadata(&directory) {
        Ok(_) => false,
        Err(value) if value.kind() == std::io::ErrorKind::NotFound => {
            create_private_scan_recovery_directory(&directory, protector)?;
            true
        }
        Err(_) => {
            return Err(error(
                "storage.io",
                "scan recovery directory cannot be inspected",
            ));
        }
    };
    if let Err(failure) = validate_private_scan_recovery_directory(&directory, protector) {
        if created {
            // The parent remains verified and no foreign principal can replace
            // this name. Remove only the empty directory we just created.
            let _ = std::fs::remove_dir(&directory);
        }
        return Err(failure);
    }
    let name = journal_path
        .file_name()
        .ok_or_else(|| error("storage.permission", "journal path needs a filename"))?;
    Ok(directory
        .join(name)
        .with_extension("scan-failure-recovery.sqlite"))
}

/// This is deliberately only a name computation.  Opening a continuation
/// journal must not provision the v2 recovery directory: a released v1
/// sibling can be exported without changing the recording directory at all.
/// Call `secure_scan_failure_sidecar_path` immediately before an operation
/// that is permitted to create or mutate v2 recovery state.
fn scan_failure_sidecar_candidate_path(journal_path: &Path) -> Result<PathBuf> {
    let parent = journal_path.parent().ok_or_else(|| {
        error(
            "storage.permission",
            "journal path does not have a parent directory",
        )
    })?;
    let name = journal_path
        .file_name()
        .ok_or_else(|| error("storage.permission", "journal path needs a filename"))?;
    Ok(parent
        .join(SCAN_RECOVERY_DIRECTORY)
        .join(name)
        .with_extension("scan-failure-recovery.sqlite"))
}

#[cfg(unix)]
fn validate_private_scan_recovery_parent(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| error("storage.io", "scan recovery parent cannot be inspected"))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(error(
            "storage.permission",
            "scan recovery parent is not owned and non-writable by other users",
        ));
    }
    if let Some(protector) = protector {
        protector.validate_directory(path)?;
    }
    Ok(())
}

#[cfg(windows)]
fn validate_private_scan_recovery_parent(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    protector
        .ok_or_else(|| {
            error(
                "capability.unsupported",
                "scan recovery requires trusted host-protected Windows storage",
            )
        })?
        .validate_directory(path)
}

#[cfg(not(any(unix, windows)))]
fn validate_private_scan_recovery_parent(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    protector
        .ok_or_else(|| {
            error(
                "capability.unsupported",
                "scan recovery requires protected storage",
            )
        })?
        .validate_directory(path)
}

#[cfg(unix)]
fn create_private_scan_recovery_directory(
    path: &Path,
    _protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(path).map_err(|value| {
        if value.kind() == std::io::ErrorKind::AlreadyExists {
            error(
                "storage.busy",
                "scan recovery directory was created concurrently",
            )
        } else {
            error(
                "storage.permission",
                "scan recovery directory cannot be created privately",
            )
        }
    })
}

#[cfg(windows)]
fn create_private_scan_recovery_directory(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    let protector = protector.ok_or_else(|| {
        error(
            "capability.unsupported",
            "scan recovery requires trusted host-protected Windows storage",
        )
    })?;
    std::fs::create_dir(path).map_err(|value| {
        if value.kind() == std::io::ErrorKind::AlreadyExists {
            error(
                "storage.busy",
                "scan recovery directory was created concurrently",
            )
        } else {
            error(
                "storage.permission",
                "scan recovery directory cannot be created privately",
            )
        }
    })?;
    if let Err(failure) = protector.provision_directory(path) {
        // `create_dir` created this empty final component. The protected parent
        // was validated before this call, so a best-effort remove cannot erase
        // an attacker replacement.
        let _ = std::fs::remove_dir(path);
        return Err(failure);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn create_private_scan_recovery_directory(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    let protector = protector.ok_or_else(|| {
        error(
            "capability.unsupported",
            "scan recovery requires protected storage",
        )
    })?;
    std::fs::create_dir(path).map_err(|_| {
        error(
            "storage.permission",
            "scan recovery directory cannot be created privately",
        )
    })?;
    if let Err(failure) = protector.provision_directory(path) {
        let _ = std::fs::remove_dir(path);
        return Err(failure);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_scan_recovery_directory(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| error("storage.io", "scan recovery directory cannot be inspected"))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(error(
            "storage.permission",
            "scan recovery directory is not owner-private",
        ));
    }
    if let Some(protector) = protector {
        protector.validate_directory(path)?;
    }
    Ok(())
}

#[cfg(windows)]
fn validate_private_scan_recovery_directory(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    protector
        .ok_or_else(|| {
            error(
                "capability.unsupported",
                "scan recovery requires trusted host-protected Windows storage",
            )
        })?
        .validate_directory(path)
}

#[cfg(not(any(unix, windows)))]
fn validate_private_scan_recovery_directory(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    protector
        .ok_or_else(|| {
            error(
                "capability.unsupported",
                "scan recovery requires protected storage",
            )
        })?
        .validate_directory(path)
}

fn open_scan_failure_sidecar(
    path: &Path,
    quota: JournalQuota,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<Connection> {
    let directory = path.parent().ok_or_else(|| {
        error(
            "storage.permission",
            "scan recovery sidecar does not have a parent directory",
        )
    })?;
    // This precedes existence checks, SQLite open, and every PRAGMA. A v2
    // database is never trusted merely because its final component looked safe.
    validate_private_scan_recovery_directory(directory, protector)?;
    let mut connection = open_exact_failure_sidecar_allowing_previous(
        path,
        quota,
        protector,
        SCAN_FAILURE_SIDECAR_APPLICATION_ID,
        SCAN_FAILURE_SIDECAR_SCHEMA_VERSION,
        SCAN_FAILURE_SIDECAR_SCHEMA,
        1,
    )?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == SCAN_FAILURE_SIDECAR_SCHEMA_VERSION {
        ensure_scan_failure_cleanup_receipt_index(&mut connection)?;
    }
    // v2 sidecars predate bounded cleanup retirement. The authority table is
    // additive and contains no occurrence state, so initializing it cannot
    // manufacture a recovery owner or weaken the existing v2 envelope.
    let authority_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='cleanup_authority')",
        [],
        |row| row.get(0),
    )?;
    if version == SCAN_FAILURE_SIDECAR_SCHEMA_VERSION && !authority_exists {
        let mut key = [0_u8; 32];
        getrandom::fill(&mut key).map_err(|_| {
            error(
                "storage.io",
                "scan failure cleanup retirement authority is unavailable",
            )
        })?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(SCAN_FAILURE_CLEANUP_AUTHORITY_SCHEMA)?;
        tx.execute(
            "INSERT INTO cleanup_authority(id,key) VALUES(1,?1)",
            [&key[..]],
        )?;
        tx.commit()?;
    }
    // This is another additive v2 repair.  Existing owner envelopes remain
    // the source of truth until an authenticated outbox transition records
    // the same domain here; no migration parses an opaque envelope or invents
    // an authority.
    if version == SCAN_FAILURE_SIDECAR_SCHEMA_VERSION {
        connection.execute_batch(SCAN_FAILURE_RECORDING_AUTHORITY_SCHEMA)?;
    }
    // Version one retained raw evidence but no bearer authority.  It remains
    // export-only even if it predates private-sidecar enforcement.  A v2
    // envelope contains the durable recovery capability, so accepting it from
    // a group/world-readable or differently owned file would turn the
    // recording path into an ambient credential.
    if version == SCAN_FAILURE_SIDECAR_SCHEMA_VERSION {
        validate_private_scan_failure_sidecar(path, protector)?;
    }
    Ok(connection)
}

/// Open a released v1 raw-evidence sidecar strictly for export.  In
/// particular this path never creates a database, enables a rollback journal,
/// or writes an acknowledgement: v1 has no owner envelope and therefore no
/// restart authority to recover.
fn open_legacy_scan_failure_sidecar_for_export(path: &Path) -> Result<Option<Connection>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(value) if value.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(error(
                "storage.io",
                "legacy scan recovery sidecar cannot be inspected",
            ));
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(error(
            "storage.permission",
            "legacy scan recovery sidecar is not a regular file",
        ));
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let application: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if application != SCAN_FAILURE_SIDECAR_APPLICATION_ID || version != 1 {
        return Err(error(
            "storage.schema",
            "legacy scan recovery sidecar schema is not recognized",
        ));
    }
    let check: String = connection.pragma_query_value(None, "quick_check", |row| row.get(0))?;
    if check != "ok" {
        return Err(error(
            "storage.corrupt",
            "legacy scan recovery sidecar integrity check failed",
        ));
    }
    Ok(Some(connection))
}

fn scan_failure_sidecar_file_exists(path: &Path, label: &'static str) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(error("storage.permission", label)),
        Err(value) if value.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(error(
            "storage.io",
            "scan recovery sidecar cannot be inspected",
        )),
    }
}

fn scan_failure_cleanup_retirement_mac(
    authority: &[u8],
    cleanup_receipt: &[u8; 32],
) -> Result<[u8; 32]> {
    use hmac::Mac;

    if authority.len() != 32 {
        return Err(error(
            "storage.corrupt",
            "scan failure cleanup retirement authority is invalid",
        ));
    }
    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(authority).map_err(|_| {
        error(
            "storage.corrupt",
            "scan failure cleanup retirement authority is invalid",
        )
    })?;
    mac.update(b"ubm-scan-cleanup-retirement/1");
    mac.update(cleanup_receipt);
    Ok(mac.finalize().into_bytes().into())
}

#[cfg(unix)]
fn validate_private_scan_failure_sidecar(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| error("storage.io", "scan recovery sidecar cannot be inspected"))?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(error(
            "storage.permission",
            "scan recovery sidecar is not an owner-private regular file",
        ));
    }
    if let Some(protector) = protector {
        protector.validate(path)?;
    }
    Ok(())
}

#[cfg(windows)]
fn validate_private_scan_failure_sidecar(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    // std does not expose a way to prove that a file's ACL excludes every
    // other principal.  Do not claim that a default DACL is private: a host
    // must provide protected storage before v2 scan recovery can be used on
    // Windows.
    protector
        .ok_or_else(|| {
            error(
                "capability.unsupported",
                "scan recovery requires trusted host-protected Windows storage",
            )
        })?
        .validate(path)
}

#[cfg(not(any(unix, windows)))]
fn validate_private_scan_failure_sidecar(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    protector
        .ok_or_else(|| {
            error(
                "capability.unsupported",
                "scan recovery requires trusted host-protected storage",
            )
        })?
        .validate(path)
}

/// Scan evidence was first released as a raw v1 sidecar.  It remains
/// inspectable/exportable, but cannot be promoted into a v2 owner because the
/// missing ownership fields are security and ordering facts, not defaults.
/// New databases are always v2.
fn open_exact_failure_sidecar_allowing_previous(
    path: &Path,
    quota: JournalQuota,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
    application_id: i64,
    schema_version: i64,
    schema: &str,
    previous_version: i64,
) -> Result<Connection> {
    let existed = std::fs::symlink_metadata(path)
        .map(|metadata| {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                Err(error(
                    "storage.permission",
                    "failure recovery sidecar is not a regular file",
                ))
            } else {
                Ok(true)
            }
        })
        .unwrap_or_else(|value| {
            if value.kind() == std::io::ErrorKind::NotFound {
                Ok(false)
            } else {
                Err(error(
                    "storage.io",
                    "failure recovery sidecar cannot be inspected",
                ))
            }
        })?;
    if !existed {
        create_private_scan_failure_sidecar(path, protector)?;
    } else {
        // Reject a tampered existing sidecar before opening SQLite. In
        // particular, never run a PRAGMA against a file merely selected by an
        // attacker-controlled final component.
        validate_private_scan_failure_sidecar(path, protector)?;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut connection = Connection::open_with_flags(path, flags)?;
    connection.busy_timeout(std::time::Duration::ZERO)?;
    connection.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF; PRAGMA secure_delete=ON; PRAGMA foreign_keys=ON;",
    )?;
    connection.pragma_update(None, "page_size", sql_integer(PAGE)?)?;
    let page_limit = quota
        .max_bytes
        .checked_add(OVERHEAD)
        .ok_or_else(|| error("storage.full", "scan failure recovery quota overflows"))?
        .div_ceil(PAGE)
        .checked_add(RESERVED_PAGES)
        .ok_or_else(|| error("storage.full", "scan failure recovery quota overflows"))?;
    let actual_limit =
        connection.query_row(&format!("PRAGMA max_page_count={page_limit}"), [], |row| {
            unsigned(row, 0)
        })?;
    if actual_limit != page_limit {
        return Err(error(
            "storage.full",
            "scan failure recovery sidecar quota cannot be enforced",
        ));
    }
    if !existed {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(schema)?;
        tx.pragma_update(None, "application_id", application_id)?;
        tx.pragma_update(None, "user_version", schema_version)?;
        tx.commit()?;
        return Ok(connection);
    }
    let application: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if application != application_id || (version != schema_version && version != previous_version) {
        return Err(error(
            "storage.schema",
            "failure recovery sidecar schema is not recognized",
        ));
    }
    let check: String = connection.pragma_query_value(None, "quick_check", |row| row.get(0))?;
    if check != "ok" {
        return Err(error(
            "storage.corrupt",
            "failure recovery sidecar integrity check failed",
        ));
    }
    Ok(connection)
}

#[cfg(unix)]
#[allow(
    clippy::collapsible_if,
    reason = "The protected create cleanup keeps the explicit Rust 2021 error boundary."
)]
fn create_private_scan_failure_sidecar(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600);
    let file = options.open(path).map_err(|failure| {
        if failure.kind() == std::io::ErrorKind::AlreadyExists {
            error(
                "storage.busy",
                "scan recovery sidecar was created concurrently",
            )
        } else {
            error(
                "storage.permission",
                "scan recovery sidecar cannot be created privately",
            )
        }
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| error("storage.io", "scan recovery sidecar cannot be inspected"))?;
    if !metadata.is_file() {
        return Err(error(
            "storage.permission",
            "scan recovery sidecar is not a regular file",
        ));
    }
    drop(file);
    if let Some(protector) = protector {
        if let Err(failure) = protector.provision(path) {
            // The containing directory was verified owner-private before this
            // create, so this removes only our still-unpublished empty file.
            let _ = std::fs::remove_file(path);
            return Err(failure);
        }
    }
    // The mandatory validation happens after SQLite has reopened this exact
    // final component with NOFOLLOW.  Calling host validation before that
    // boundary would leave a replace-between-check-and-open window.
    Ok(())
}

#[cfg(windows)]
fn create_private_scan_failure_sidecar(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    let protector = protector.ok_or_else(|| {
        error(
            "capability.unsupported",
            "scan recovery requires trusted host-protected Windows storage",
        )
    })?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        // FILE_FLAG_OPEN_REPARSE_POINT: do not follow a final-component
        // reparse point between the preflight and trusted-host ACL work.
        .custom_flags(0x0020_0000)
        .open(path)
        .map_err(|failure| {
            if failure.kind() == std::io::ErrorKind::AlreadyExists {
                error(
                    "storage.busy",
                    "scan recovery sidecar was created concurrently",
                )
            } else {
                error(
                    "storage.permission",
                    "scan recovery sidecar cannot be created privately",
                )
            }
        })?;
    let metadata = file
        .metadata()
        .map_err(|_| error("storage.io", "scan recovery sidecar cannot be inspected"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(error(
            "storage.permission",
            "scan recovery sidecar is not a regular file",
        ));
    }
    drop(file);
    if let Err(failure) = protector.provision(path) {
        let _ = std::fs::remove_file(path);
        return Err(failure);
    }
    if let Err(failure) = validate_private_scan_failure_sidecar(path, Some(protector)) {
        let _ = std::fs::remove_file(path);
        return Err(failure);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn create_private_scan_failure_sidecar(
    path: &Path,
    protector: Option<&dyn ScanRecoveryStorageProtector>,
) -> Result<()> {
    let protector = protector.ok_or_else(|| {
        error(
            "capability.unsupported",
            "scan recovery requires trusted host-protected storage",
        )
    })?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| {
            error(
                "storage.permission",
                "scan recovery sidecar cannot be created privately",
            )
        })?;
    if !file
        .metadata()
        .map_err(|_| error("storage.io", "scan recovery sidecar cannot be inspected"))?
        .is_file()
    {
        return Err(error(
            "storage.permission",
            "scan recovery sidecar is not a regular file",
        ));
    }
    drop(file);
    protector.provision(path)?;
    validate_private_scan_failure_sidecar(path, Some(protector))
}

fn open_exact_failure_sidecar(
    path: &Path,
    quota: JournalQuota,
    application_id: i64,
    schema_version: i64,
    schema: &str,
    _name: &str,
) -> Result<Connection> {
    let existed = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(error(
                    "storage.permission",
                    "failure recovery sidecar is not a regular file",
                ));
            }
            true
        }
        Err(value) if value.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => {
            return Err(error(
                "storage.io",
                "failure recovery sidecar cannot be inspected",
            ));
        }
    };
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut connection = Connection::open_with_flags(
        path,
        if existed {
            flags
        } else {
            flags | OpenFlags::SQLITE_OPEN_CREATE
        },
    )?;
    connection.busy_timeout(std::time::Duration::ZERO)?;
    connection.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF; PRAGMA secure_delete=ON; PRAGMA foreign_keys=ON;",
    )?;
    connection.pragma_update(None, "page_size", sql_integer(PAGE)?)?;
    let page_limit = quota
        .max_bytes
        .checked_add(OVERHEAD)
        .ok_or_else(|| error("storage.full", "peer failure recovery quota overflows"))?
        .div_ceil(PAGE)
        .checked_add(RESERVED_PAGES)
        .ok_or_else(|| error("storage.full", "peer failure recovery quota overflows"))?;
    let actual_limit =
        connection.query_row(&format!("PRAGMA max_page_count={page_limit}"), [], |row| {
            unsigned(row, 0)
        })?;
    if actual_limit != page_limit {
        return Err(error(
            "storage.full",
            "peer failure recovery sidecar quota cannot be enforced",
        ));
    }
    if !existed {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(schema)?;
        tx.pragma_update(None, "application_id", application_id)?;
        tx.pragma_update(None, "user_version", schema_version)?;
        tx.commit()?;
    } else {
        let application: i64 =
            connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if application != application_id || version != schema_version {
            return Err(error(
                "storage.schema",
                "failure recovery sidecar schema is not recognized",
            ));
        }
        let check: String = connection.pragma_query_value(None, "quick_check", |row| row.get(0))?;
        if check != "ok" {
            return Err(error(
                "storage.corrupt",
                "failure recovery sidecar integrity check failed",
            ));
        }
    }
    Ok(connection)
}

fn apple_failure_spool_table_is_exact(
    connection: &Connection,
    name: &str,
    schema: &str,
) -> Result<bool> {
    let actual: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |row| row.get(0),
        )
        .optional()?;
    let Some(actual) = actual else {
        return Ok(false);
    };
    let table_schema = schema.split(';').next().unwrap_or(schema);
    // SQLite quotes a table's new name when ALTER TABLE ... RENAME TO is used
    // during an owned migration. The migration's compact DDL also omits spaces
    // around punctuation. The identifier and punctuation formatting are
    // normalized; every schema token and constraint remains exact.
    let actual = normalized_schema_sql(&actual)
        .replace(
            &format!("CREATE TABLE \"{name}\""),
            &format!("CREATE TABLE {name}"),
        )
        .replace(", ", ",")
        .replace("( ", "(")
        .replace(" )", ")");
    let expected = normalized_schema_sql(table_schema)
        .replace(", ", ",")
        .replace("( ", "(")
        .replace(" )", ")");
    if actual != expected {
        return Ok(false);
    }
    let has_named_index: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND tbl_name=?1 AND sql IS NOT NULL)",
        [name],
        |row| row.get(0),
    )?;
    Ok(!has_named_index)
}

fn validate_apple_failure_spool_shape(connection: &Connection, version: i64) -> Result<()> {
    let mut expected = vec![
        ("occurrences", APPLE_FAILURE_SPOOL_SCHEMA, true),
        (
            "watermarks",
            APPLE_FAILURE_SPOOL_WATERMARK_SCHEMA,
            version >= 2,
        ),
        (
            "reservations",
            if (2..=3).contains(&version) {
                APPLE_FAILURE_SPOOL_RESERVATION_V2_SCHEMA
            } else {
                APPLE_FAILURE_SPOOL_RESERVATION_SCHEMA
            },
            version >= 2,
        ),
        (
            "settings",
            if (3..=8).contains(&version) {
                APPLE_FAILURE_SPOOL_SETTINGS_V3_SCHEMA
            } else {
                APPLE_FAILURE_SPOOL_SETTINGS_SCHEMA
            },
            version >= 3,
        ),
        (
            "capture_stream",
            APPLE_FAILURE_SPOOL_GLOBAL_STREAM_SCHEMA,
            version >= 5,
        ),
        (
            "capture_watermark",
            APPLE_FAILURE_SPOOL_CAPTURE_WATERMARK_SCHEMA,
            version >= 6,
        ),
        (
            "acknowledgements",
            if version == 7 {
                APPLE_FAILURE_SPOOL_ACKNOWLEDGEMENTS_V7_SCHEMA
            } else if (8..=9).contains(&version) {
                APPLE_FAILURE_SPOOL_ACKNOWLEDGEMENTS_V9_SCHEMA
            } else {
                APPLE_FAILURE_SPOOL_ACKNOWLEDGEMENTS_SCHEMA
            },
            version >= 7,
        ),
    ];
    // A known interrupted v1 migration can have created the first two v2
    // tables before it advanced user_version. No other v1 extension is owned.
    if version == 1 {
        expected[1].2 = false;
        expected[2].2 = false;
    }
    for (name, schema, required) in &expected {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [*name],
            |row| row.get(0),
        )?;
        if (*required && !exists)
            || (exists && !apple_failure_spool_table_is_exact(connection, name, schema)?)
        {
            return Err(error(
                "storage.schema",
                "apple notification failure spool schema is incompatible; file retained",
            ));
        }
    }
    let allowed = expected
        .iter()
        .filter_map(|(name, _, required)| {
            if *required || version == 1 && (*name == "watermarks" || *name == "reservations") {
                Some(*name)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if names
        .iter()
        .any(|name| !allowed.iter().any(|allowed| *allowed == name))
    {
        return Err(error(
            "storage.schema",
            "apple notification failure spool has unrecognized tables; file retained",
        ));
    }
    let unexpected_schema_object: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type IN ('trigger','view') AND name NOT LIKE 'sqlite_%')",
        [],
        |row| row.get(0),
    )?;
    if unexpected_schema_object {
        return Err(error(
            "storage.schema",
            "apple notification failure spool has unrecognized schema objects; file retained",
        ));
    }
    Ok(())
}

fn open_apple_failure_spool(path: &Path, quota: JournalQuota) -> Result<Connection> {
    let existed = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(error(
                    "storage.permission",
                    "apple notification failure spool is not a regular file",
                ));
            }
            true
        }
        Err(value) if value.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => {
            return Err(error(
                "storage.io",
                "apple notification failure spool cannot be inspected",
            ));
        }
    };
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut connection = Connection::open_with_flags(
        path,
        if existed {
            flags
        } else {
            flags | OpenFlags::SQLITE_OPEN_CREATE
        },
    )?;
    connection.busy_timeout(std::time::Duration::ZERO)?;
    connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF; PRAGMA secure_delete=ON; PRAGMA foreign_keys=ON;")?;
    connection.pragma_update(None, "page_size", sql_integer(PAGE)?)?;
    if !existed {
        let mut retirement_secret = [0_u8; 32];
        getrandom::fill(&mut retirement_secret).map_err(|_| {
            error(
                "storage.io",
                "apple notification failure retirement secret is unavailable",
            )
        })?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(APPLE_FAILURE_SPOOL_SCHEMA)?;
        tx.execute_batch(APPLE_FAILURE_SPOOL_WATERMARK_SCHEMA)?;
        tx.execute_batch(APPLE_FAILURE_SPOOL_RESERVATION_SCHEMA)?;
        tx.execute_batch(APPLE_FAILURE_SPOOL_SETTINGS_SCHEMA)?;
        tx.execute_batch(APPLE_FAILURE_SPOOL_GLOBAL_STREAM_SCHEMA)?;
        tx.execute_batch(APPLE_FAILURE_SPOOL_CAPTURE_WATERMARK_SCHEMA)?;
        tx.execute_batch(APPLE_FAILURE_SPOOL_ACKNOWLEDGEMENTS_SCHEMA)?;
        tx.execute(
            "INSERT INTO settings(id,max_bytes,max_records,retirement_secret) VALUES(1,?1,?2,?3)",
            params![
                sql_integer(quota.max_bytes)?,
                sql_integer(quota.max_records)?,
                &retirement_secret[..]
            ],
        )?;
        tx.pragma_update(None, "application_id", APPLE_FAILURE_SPOOL_APPLICATION_ID)?;
        tx.pragma_update(None, "user_version", APPLE_FAILURE_SPOOL_SCHEMA_VERSION)?;
        tx.commit()?;
    } else {
        let application: i64 =
            connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if application != APPLE_FAILURE_SPOOL_APPLICATION_ID
            || !(1..=APPLE_FAILURE_SPOOL_SCHEMA_VERSION).contains(&version)
        {
            return Err(error(
                "storage.schema",
                "apple notification failure spool schema is not recognized",
            ));
        }
        let check: String = connection.pragma_query_value(None, "quick_check", |row| row.get(0))?;
        if check != "ok" {
            return Err(error(
                "storage.corrupt",
                "apple notification failure spool integrity check failed",
            ));
        }
        validate_apple_failure_spool_shape(&connection, version)?;
        // Migrate the sidecar under one transaction.  In particular, do not
        // commit the v1 watermark/reservation tables while its user_version
        // still says v1: a crash at that point made the next open replay a
        // non-idempotent CREATE TABLE and strand the retained occurrences.
        if version < APPLE_FAILURE_SPOOL_SCHEMA_VERSION {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if version == 1 {
                tx.execute_batch(APPLE_FAILURE_SPOOL_WATERMARK_SCHEMA)?;
                tx.execute_batch(APPLE_FAILURE_SPOOL_RESERVATION_SCHEMA)?;
                tx.execute(
                "INSERT INTO watermarks(route,epoch,highest_sequence) SELECT route,epoch,max(sequence) FROM occurrences WHERE 1 GROUP BY route,epoch ON CONFLICT(route,epoch) DO UPDATE SET highest_sequence=max(watermarks.highest_sequence,excluded.highest_sequence)",
                [],
            )?;
            }
            if version <= 2 {
                tx.execute_batch(APPLE_FAILURE_SPOOL_SETTINGS_V3_SCHEMA)?;
                tx.execute(
                    "INSERT INTO settings(id,max_bytes,max_records) VALUES(1,?1,?2)",
                    params![
                        sql_integer(quota.max_bytes)?,
                        sql_integer(quota.max_records)?
                    ],
                )?;
            }
            if version <= 8 {
                let mut retirement_secret = [0_u8; 32];
                getrandom::fill(&mut retirement_secret).map_err(|_| {
                    error(
                        "storage.io",
                        "apple notification failure retirement secret is unavailable",
                    )
                })?;
                tx.execute_batch(
                    "CREATE TABLE settings_v9(\
                       id INTEGER PRIMARY KEY CHECK(id=1),\
                       max_bytes INTEGER NOT NULL CHECK(max_bytes>0),\
                       max_records INTEGER NOT NULL CHECK(max_records>0),\
                       retirement_secret BLOB NOT NULL CHECK(octet_length(retirement_secret)=32)\
                     );",
                )?;
                tx.execute(
                    "INSERT INTO settings_v9(id,max_bytes,max_records,retirement_secret) \
                     SELECT id,max_bytes,max_records,?1 FROM settings",
                    [&retirement_secret[..]],
                )?;
                tx.execute_batch(
                    "DROP TABLE settings; ALTER TABLE settings_v9 RENAME TO settings;",
                )?;
            }
            if version <= 3 {
                // SQLite cannot add the required NOT NULL envelope columns
                // to a populated legacy table. Rebuild from the occurrence
                // that owns every legacy reservation; a reservation without
                // that exact durable occurrence was never replayable, so
                // reject rather than inventing a payload.
                let reservations: i64 =
                    tx.query_row("SELECT count(*) FROM reservations", [], |row| row.get(0))?;
                let recoverable: i64 = tx.query_row(
                    "SELECT count(*) FROM reservations r JOIN occurrences o ON o.identity=r.identity",
                    [],
                    |row| row.get(0),
                )?;
                if recoverable != reservations {
                    return Err(error(
                        "storage.corrupt",
                        "apple notification failure legacy reservation lacks its occurrence",
                    ));
                }
                tx.execute_batch(
                    "CREATE TABLE reservations_v8(\
                       route TEXT NOT NULL, epoch INTEGER NOT NULL, sequence INTEGER NOT NULL,\
                       identity BLOB NOT NULL CHECK(octet_length(identity)=32),\
                       occurrence TEXT NOT NULL, bytes INTEGER NOT NULL CHECK(bytes>0),\
                       PRIMARY KEY(route,epoch)\
                     ) WITHOUT ROWID;\
                     INSERT INTO reservations_v8(route,epoch,sequence,identity,occurrence,bytes)\
                       SELECT r.route,r.epoch,r.sequence,r.identity,o.occurrence,o.bytes \
                       FROM reservations r JOIN occurrences o ON o.identity=r.identity;\
                     DROP TABLE reservations;\
                     ALTER TABLE reservations_v8 RENAME TO reservations;",
                )?;
            }
            if version <= 4 {
                tx.execute_batch(APPLE_FAILURE_SPOOL_GLOBAL_STREAM_SCHEMA)?;
                tx.execute(
                "INSERT INTO capture_stream(identity) SELECT identity FROM occurrences ORDER BY sequence,route,epoch",
                [],
            )?;
                tx.execute(
                "INSERT INTO capture_stream(identity) \
                 SELECT identity FROM reservations \
                 WHERE NOT EXISTS(SELECT 1 FROM capture_stream WHERE capture_stream.identity=reservations.identity) \
                 ORDER BY sequence,route,epoch",
                [],
            )?;
            }
            if version <= 5 {
                tx.execute_batch(APPLE_FAILURE_SPOOL_CAPTURE_WATERMARK_SCHEMA)?;
                tx.execute(
                "UPDATE capture_watermark SET highest_ordinal=coalesce((SELECT max(capture_ordinal) FROM capture_stream),0) WHERE id=1",
                [],
            )?;
            }
            if version <= 6 {
                tx.execute_batch(APPLE_FAILURE_SPOOL_ACKNOWLEDGEMENTS_V7_SCHEMA)?;
            }
            if version <= 7 {
                tx.execute_batch(
                    "ALTER TABLE acknowledgements ADD COLUMN retired INTEGER NOT NULL DEFAULT 0 CHECK(retired IN (0,1));",
                )?;
            }
            if version <= 9 {
                tx.execute_batch(
                    "CREATE TABLE acknowledgements_v10(\
                       capture_ordinal INTEGER PRIMARY KEY CHECK(capture_ordinal>0),\
                       identity BLOB NOT NULL CHECK(octet_length(identity)=32),\
                       retired INTEGER NOT NULL DEFAULT 0 CHECK(retired IN (0,1)),\
                       occurrence TEXT,\
                       v4_capability_issued INTEGER NOT NULL DEFAULT 0 CHECK(v4_capability_issued IN (0,1)\
                     ));\
                     INSERT INTO acknowledgements_v10(capture_ordinal,identity,retired)\
                       SELECT capture_ordinal,identity,retired FROM acknowledgements;\
                     DROP TABLE acknowledgements;\
                     ALTER TABLE acknowledgements_v10 RENAME TO acknowledgements;",
                )?;
            }
            tx.pragma_update(None, "user_version", APPLE_FAILURE_SPOOL_SCHEMA_VERSION)?;
            tx.commit()?;
        }
        validate_apple_failure_spool_shape(&connection, APPLE_FAILURE_SPOOL_SCHEMA_VERSION)?;
    }
    let configured: (u64, u64) = connection.query_row(
        "SELECT max_bytes,max_records FROM settings WHERE id=1",
        [],
        |row| Ok((unsigned(row, 0)?, unsigned(row, 1)?)),
    )?;
    if configured != (quota.max_bytes, quota.max_records) {
        return Err(error(
            "storage.identity",
            "apple notification failure spool quota differs from its configured authority",
        ));
    }
    let limit = configured
        .0
        .checked_add(OVERHEAD)
        .ok_or_else(|| {
            error(
                "storage.full",
                "apple notification failure spool quota overflows",
            )
        })?
        .div_ceil(PAGE)
        .checked_add(RESERVED_PAGES)
        .ok_or_else(|| {
            error(
                "storage.full",
                "apple notification failure spool quota overflows",
            )
        })?;
    if connection.query_row(&format!("PRAGMA max_page_count={limit}"), [], |row| {
        unsigned(row, 0)
    })? != limit
    {
        return Err(error(
            "storage.full",
            "apple notification failure spool quota cannot be enforced",
        ));
    }
    Ok(connection)
}

#[allow(
    clippy::collapsible_if,
    reason = "The sidecar sequence admission uses explicit Rust 2021 nesting to preserve each retention check."
)]
impl JournalAppleNotificationFailureSpool {
    fn retirement_capability(&self, capture_ordinal: u64, identity: &[u8; 32]) -> Result<String> {
        let connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        let secret: Vec<u8> = connection.query_row(
            "SELECT retirement_secret FROM settings WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        if secret.len() != 32 {
            return Err(error(
                "storage.corrupt",
                "apple notification failure retirement secret is invalid",
            ));
        }
        let mut digest = Sha256::new();
        digest.update(&secret);
        digest.update(capture_ordinal.to_be_bytes());
        digest.update(identity);
        let mac = digest.finalize();
        Ok(format!("{}:{mac:x}", capture_ordinal))
    }

    fn verify_retirement_capability(
        &self,
        capture_ordinal: u64,
        identity: &[u8; 32],
        capability: &str,
    ) -> Result<()> {
        let expected = self.retirement_capability(capture_ordinal, identity)?;
        if expected != capability {
            return Err(error(
                "storage.identity",
                "apple notification failure retirement capability is invalid",
            ));
        }
        Ok(())
    }

    pub fn recording_id(&self) -> &str {
        self._journal.recording_id()
    }

    pub fn has_retained_occurrences(&self) -> Result<bool> {
        let connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM occurrences UNION ALL SELECT 1 FROM reservations)",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    fn encoded(occurrence: &AppleNotificationFailureOccurrence) -> Result<(Vec<u8>, [u8; 32])> {
        if occurrence.route.is_empty() || occurrence.route.len() > 1024 || occurrence.sequence == 0
        {
            return Err(error(
                "argument.invalid",
                "apple notification failure spool identity is invalid",
            ));
        }
        let value = json!({"version":1,"route":occurrence.route,"epoch":occurrence.epoch.to_string(),"sequence":occurrence.sequence.to_string(),"failure":occurrence.failure});
        let bytes = serde_json::to_vec(&value).map_err(|_| {
            error(
                "argument.invalid",
                "apple notification failure occurrence cannot be encoded",
            )
        })?;
        if bytes.len() > 64 * 1024 {
            return Err(error(
                "argument.invalid",
                "apple notification failure occurrence is too large",
            ));
        }
        let mut identity = [0; 32];
        identity.copy_from_slice(&Sha256::digest(&bytes));
        Ok((bytes, identity))
    }

    pub fn append(
        &self,
        occurrence: &AppleNotificationFailureOccurrence,
    ) -> Result<AppleNotificationFailureAppend> {
        let (bytes, identity) = Self::encoded(occurrence)?;
        let text = String::from_utf8(bytes.clone()).map_err(|_| {
            error(
                "storage.corrupt",
                "apple notification failure encoding is not UTF-8",
            )
        })?;
        let _gate = self
            ._journal
            .shared
            .apple_failure_spool_gate
            .lock()
            .map_err(|_| {
                error(
                    "storage.io",
                    "apple notification failure spool was interrupted",
                )
            })?;
        let phase: String = self._journal.connection()?.query_row(
            "SELECT phase FROM journal WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        if phase != "recording" {
            return Err(error(
                "storage.busy",
                "apple notification failure spool is not accepting while recording is stopped",
            ));
        }
        let mut connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT occurrence FROM occurrences WHERE route=?1 AND epoch=?2 AND sequence=?3",
                params![
                    occurrence.route,
                    sql_integer(occurrence.epoch)?,
                    sql_integer(occurrence.sequence)?
                ],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing != text {
                return Err(error(
                    "storage.identity",
                    "apple notification failure retry differs",
                ));
            }
            tx.commit()?;
            return Ok(AppleNotificationFailureAppend::Committed);
        }
        let reservation: Option<(u64, Vec<u8>)> = tx
            .query_row(
                "SELECT sequence,identity FROM reservations WHERE route=?1 AND epoch=?2",
                params![occurrence.route, sql_integer(occurrence.epoch)?],
                |row| Ok((unsigned(row, 0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((reserved_sequence, reserved_identity)) = &reservation {
            if *reserved_sequence != occurrence.sequence {
                return Err(error(
                    "storage.sequence",
                    "apple notification failure sequence is blocked by an earlier full append",
                ));
            }
            if reserved_identity.as_slice() != identity {
                return Err(error(
                    "storage.identity",
                    "apple notification failure retry differs from blocked append",
                ));
            }
        }
        let blocked_elsewhere: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM reservations WHERE NOT (route=?1 AND epoch=?2))",
            params![occurrence.route, sql_integer(occurrence.epoch)?],
            |row| row.get(0),
        )?;
        if blocked_elsewhere {
            return Err(error(
                "storage.sequence",
                "apple notification failure sequence is blocked by an earlier full append",
            ));
        }
        // A source can start spilling only after an earlier contiguous prefix
        // was accepted inline. Its first sidecar record is therefore a trusted
        // producer base, not necessarily sequence one. Once a base commits or
        // a full append reserves its next sequence, every later append is the
        // exact durable successor.
        let highest: Option<u64> = tx
            .query_row(
                "SELECT highest_sequence FROM watermarks WHERE route=?1 AND epoch=?2",
                params![occurrence.route, sql_integer(occurrence.epoch)?],
                |row| unsigned(row, 0),
            )
            .optional()?;
        if reservation.is_none() {
            if let Some(highest) = highest {
                if occurrence.sequence <= highest {
                    return Err(error(
                        "storage.sequence",
                        "apple notification failure sequence is already retired",
                    ));
                }
            }
        }
        let count: u64 = tx.query_row("SELECT count(*) FROM occurrences", [], |row| {
            unsigned(row, 0)
        })?;
        let total: u64 = tx.query_row(
            "SELECT coalesce(sum(bytes),0) FROM occurrences",
            [],
            |row| unsigned(row, 0),
        )?;
        let next = total.checked_add(bytes.len() as u64).ok_or_else(|| {
            error(
                "storage.full",
                "apple notification failure spool bytes overflow",
            )
        })?;
        if count >= self.quota.max_records || next > self.quota.max_bytes {
            if reservation.is_none() {
                tx.execute(
                    "INSERT INTO reservations(route,epoch,sequence,identity,occurrence,bytes) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![occurrence.route, sql_integer(occurrence.epoch)?, sql_integer(occurrence.sequence)?, &identity[..], text, sql_integer(bytes.len() as u64)?],
                )?;
                tx.execute(
                    "INSERT INTO capture_stream(identity) VALUES(?1)",
                    [&identity[..]],
                )?;
                tx.execute(
                    "UPDATE capture_watermark SET highest_ordinal=last_insert_rowid() WHERE id=1",
                    [],
                )?;
            }
            tx.commit()?;
            return Ok(AppleNotificationFailureAppend::Full);
        }
        tx.execute("INSERT INTO occurrences(identity,route,epoch,sequence,occurrence,bytes) VALUES(?1,?2,?3,?4,?5,?6)", params![&identity[..], occurrence.route, sql_integer(occurrence.epoch)?, sql_integer(occurrence.sequence)?, text, sql_integer(bytes.len() as u64)?])?;
        if reservation.is_none() {
            tx.execute(
                "INSERT INTO capture_stream(identity) VALUES(?1)",
                [&identity[..]],
            )?;
            tx.execute(
                "UPDATE capture_watermark SET highest_ordinal=last_insert_rowid() WHERE id=1",
                [],
            )?;
        }
        tx.execute(
            "INSERT INTO watermarks(route,epoch,highest_sequence) VALUES(?1,?2,?3) ON CONFLICT(route,epoch) DO UPDATE SET highest_sequence=excluded.highest_sequence",
            params![occurrence.route, sql_integer(occurrence.epoch)?, sql_integer(occurrence.sequence)?],
        )?;
        tx.execute(
            "DELETE FROM reservations WHERE identity=?1",
            [&identity[..]],
        )?;
        tx.commit()?;
        Ok(AppleNotificationFailureAppend::Committed)
    }

    pub fn page(
        &self,
        route: &str,
        epoch: u64,
        after_sequence: u64,
        max_items: u32,
    ) -> Result<Vec<AppleNotificationFailureOccurrence>> {
        if max_items == 0 || max_items > 2048 {
            return Err(error(
                "argument.invalid",
                "apple notification failure spool page bound is invalid",
            ));
        }
        let connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        let mut statement = connection.prepare("SELECT occurrence FROM (SELECT occurrence,sequence FROM occurrences WHERE route=?1 AND epoch=?2 AND sequence>?3 UNION ALL SELECT occurrence,sequence FROM reservations WHERE route=?1 AND epoch=?2 AND sequence>?3) ORDER BY sequence LIMIT ?4")?;
        let rows = statement.query_map(
            params![
                route,
                sql_integer(epoch)?,
                sql_integer(after_sequence)?,
                i64::from(max_items)
            ],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| {
            let v: Value = serde_json::from_str(&row?).map_err(|_| {
                error(
                    "storage.corrupt",
                    "apple notification failure occurrence cannot be decoded",
                )
            })?;
            Ok(AppleNotificationFailureOccurrence {
                route: v["route"]
                    .as_str()
                    .ok_or_else(|| {
                        error(
                            "storage.corrupt",
                            "apple notification failure route is invalid",
                        )
                    })?
                    .to_owned(),
                epoch: v["epoch"]
                    .as_str()
                    .ok_or_else(|| {
                        error(
                            "storage.corrupt",
                            "apple notification failure epoch is invalid",
                        )
                    })?
                    .parse()
                    .map_err(|_| {
                        error(
                            "storage.corrupt",
                            "apple notification failure epoch is invalid",
                        )
                    })?,
                sequence: v["sequence"]
                    .as_str()
                    .ok_or_else(|| {
                        error(
                            "storage.corrupt",
                            "apple notification failure sequence is invalid",
                        )
                    })?
                    .parse()
                    .map_err(|_| {
                        error(
                            "storage.corrupt",
                            "apple notification failure sequence is invalid",
                        )
                    })?,
                failure: v["failure"].clone(),
            })
        })
        .collect()
    }

    /// Returns the one durable FIFO stream shared by every callback route.
    /// `capture_ordinal` is immutable source-capture identity, not a new
    /// process-local V1 delivery receipt sequence.
    pub fn page_global(
        &self,
        after_capture_ordinal: u64,
        max_items: u32,
    ) -> Result<Vec<AppleNotificationFailureCapture>> {
        if max_items == 0 || max_items > 2048 {
            return Err(error(
                "argument.invalid",
                "apple notification failure spool page bound is invalid",
            ));
        }
        let connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        let mut statement = connection.prepare(
            "SELECT capture_stream.capture_ordinal,coalesce(occurrences.occurrence,reservations.occurrence) \
             FROM capture_stream \
             LEFT JOIN occurrences ON occurrences.identity=capture_stream.identity \
             LEFT JOIN reservations ON reservations.identity=capture_stream.identity \
             WHERE capture_stream.capture_ordinal>?1 \
             ORDER BY capture_stream.capture_ordinal LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![sql_integer(after_capture_ordinal)?, i64::from(max_items)],
            |row| Ok((unsigned(row, 0)?, row.get::<_, Option<String>>(1)?)),
        )?;
        rows.map(|row| {
            let (capture_ordinal, occurrence) = row?;
            let occurrence = occurrence.ok_or_else(|| {
                error(
                    "storage.corrupt",
                    "apple notification failure capture has no retained occurrence",
                )
            })?;
            let value: Value = serde_json::from_str(&occurrence).map_err(|_| {
                error(
                    "storage.corrupt",
                    "apple notification failure occurrence cannot be decoded",
                )
            })?;
            let route = value["route"].as_str().ok_or_else(|| {
                error(
                    "storage.corrupt",
                    "apple notification failure route is invalid",
                )
            })?;
            let epoch = value["epoch"]
                .as_str()
                .ok_or_else(|| {
                    error(
                        "storage.corrupt",
                        "apple notification failure epoch is invalid",
                    )
                })?
                .parse()
                .map_err(|_| {
                    error(
                        "storage.corrupt",
                        "apple notification failure epoch is invalid",
                    )
                })?;
            let sequence = value["sequence"]
                .as_str()
                .ok_or_else(|| {
                    error(
                        "storage.corrupt",
                        "apple notification failure sequence is invalid",
                    )
                })?
                .parse()
                .map_err(|_| {
                    error(
                        "storage.corrupt",
                        "apple notification failure sequence is invalid",
                    )
                })?;
            Ok(AppleNotificationFailureCapture {
                capture_ordinal,
                occurrence: AppleNotificationFailureOccurrence {
                    route: route.to_owned(),
                    epoch,
                    sequence,
                    failure: value["failure"].clone(),
                },
            })
        })
        .collect()
    }

    /// Retires the current global FIFO head only. An out-of-order ACK would
    /// let a later route overtake an earlier retained callback after a retry.
    pub fn acknowledge_capture(&self, capture_ordinal: u64) -> Result<()> {
        let mut connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt: Option<u64> = transaction
            .query_row(
                "SELECT capture_ordinal FROM acknowledgements WHERE capture_ordinal=?1",
                [sql_integer(capture_ordinal)?],
                |row| unsigned(row, 0),
            )
            .optional()?;
        if receipt == Some(capture_ordinal) {
            transaction.commit()?;
            return Ok(());
        }
        let head: Option<(u64, Vec<u8>)> = transaction.query_row(
            "SELECT capture_ordinal,identity FROM capture_stream ORDER BY capture_ordinal LIMIT 1",
            [],
            |row| Ok((unsigned(row, 0)?, row.get(1)?)),
        ).optional()?;
        let Some((head_ordinal, identity)) = head else {
            return Err(error(
                "storage.identity",
                "apple notification failure acknowledgement is absent",
            ));
        };
        if head_ordinal != capture_ordinal {
            return Err(error(
                "storage.sequence",
                "apple notification failure acknowledgement is not the global FIFO head",
            ));
        }
        let changed = transaction
            .execute("DELETE FROM occurrences WHERE identity=?1", [&identity[..]])?
            + transaction.execute(
                "DELETE FROM reservations WHERE identity=?1",
                [&identity[..]],
            )?;
        if changed != 1 {
            return Err(error(
                "storage.corrupt",
                "apple notification failure capture has no retained occurrence",
            ));
        }
        transaction.execute(
            "DELETE FROM capture_stream WHERE capture_ordinal=?1",
            [sql_integer(capture_ordinal)?],
        )?;
        transaction.execute(
            "INSERT INTO acknowledgements(capture_ordinal,identity) VALUES(?1,?2)",
            params![sql_integer(capture_ordinal)?, &identity[..]],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Proof-bearing counterpart to the ordinal-only compatibility ACK.
    pub fn acknowledge_capture_exact(
        &self,
        capture_ordinal: u64,
        occurrence: &AppleNotificationFailureOccurrence,
    ) -> Result<()> {
        let (_, expected_identity) = Self::encoded(occurrence)?;
        let mut connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT identity FROM acknowledgements WHERE capture_ordinal=?1",
                [sql_integer(capture_ordinal)?],
                |row| row.get(0),
            )
            .optional()?;
        if receipt.as_deref() == Some(expected_identity.as_slice()) {
            transaction.commit()?;
            return Ok(());
        }
        let head: Option<(u64, Vec<u8>)> = transaction
            .query_row(
                "SELECT capture_ordinal,identity FROM capture_stream ORDER BY capture_ordinal LIMIT 1",
                [],
                |row| Ok((unsigned(row, 0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((head_ordinal, identity)) = head else {
            return Err(error(
                "storage.identity",
                "apple notification failure acknowledgement is absent",
            ));
        };
        if head_ordinal != capture_ordinal || identity.as_slice() != expected_identity {
            return Err(error(
                "storage.sequence",
                "apple notification failure acknowledgement is not the exact global FIFO head",
            ));
        }
        let changed = transaction
            .execute("DELETE FROM occurrences WHERE identity=?1", [&identity[..]])?
            + transaction.execute(
                "DELETE FROM reservations WHERE identity=?1",
                [&identity[..]],
            )?;
        if changed != 1 {
            return Err(error(
                "storage.corrupt",
                "apple notification failure capture has no retained occurrence",
            ));
        }
        transaction.execute(
            "DELETE FROM capture_stream WHERE capture_ordinal=?1",
            [sql_integer(capture_ordinal)?],
        )?;
        transaction.execute(
            "INSERT INTO acknowledgements(capture_ordinal,identity) VALUES(?1,?2)",
            params![sql_integer(capture_ordinal)?, &identity[..]],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Additive V4 ACK lifecycle. The opaque capability is returned only after
    /// the exact ACK commits; callers persist it before asking V4 final
    /// confirmation to reclaim the receipt.
    pub fn acknowledge_capture_exact_v4(
        &self,
        capture_ordinal: u64,
        occurrence: &AppleNotificationFailureOccurrence,
    ) -> Result<String> {
        let (encoded, identity) = Self::encoded(occurrence)?;
        let occurrence = String::from_utf8(encoded).map_err(|_| {
            error(
                "storage.corrupt",
                "apple notification failure occurrence is not UTF-8",
            )
        })?;
        let mut connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior_receipt: Option<(Vec<u8>, Option<String>, bool)> = transaction
            .query_row(
                "SELECT identity,occurrence,v4_capability_issued FROM acknowledgements WHERE capture_ordinal=?1",
                [sql_integer(capture_ordinal)?],
                |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0)),
            )
            .optional()?;
        match prior_receipt {
            Some((receipt, Some(recovered), true))
                if receipt.as_slice() == identity && recovered == occurrence => {}
            Some((receipt, _, true)) if receipt.as_slice() == identity => {
                return Err(error(
                    "storage.corrupt",
                    "apple notification failure V4 acknowledgement recovery differs",
                ));
            }
            Some((receipt, _, false)) if receipt.as_slice() == identity => {
                return Err(error(
                    "storage.identity",
                    "apple notification failure acknowledgement predates V4 retirement proof",
                ));
            }
            Some(_) => {
                return Err(error(
                    "storage.identity",
                    "apple notification failure acknowledgement receipt differs",
                ));
            }
            None => {
                let head: Option<(u64, Vec<u8>)> = transaction
                    .query_row(
                        "SELECT capture_ordinal,identity FROM capture_stream ORDER BY capture_ordinal LIMIT 1",
                        [],
                        |row| Ok((unsigned(row, 0)?, row.get(1)?)),
                    )
                    .optional()?;
                let Some((head_ordinal, head_identity)) = head else {
                    return Err(error(
                        "storage.identity",
                        "apple notification failure acknowledgement is absent",
                    ));
                };
                if head_ordinal != capture_ordinal || head_identity.as_slice() != identity {
                    return Err(error(
                        "storage.sequence",
                        "apple notification failure acknowledgement is not the exact global FIFO head",
                    ));
                }
                let changed = transaction.execute(
                    "DELETE FROM occurrences WHERE identity=?1",
                    [&head_identity[..]],
                )? + transaction.execute(
                    "DELETE FROM reservations WHERE identity=?1",
                    [&head_identity[..]],
                )?;
                if changed != 1 {
                    return Err(error(
                        "storage.corrupt",
                        "apple notification failure capture has no retained occurrence",
                    ));
                }
                transaction.execute(
                    "DELETE FROM capture_stream WHERE capture_ordinal=?1",
                    [sql_integer(capture_ordinal)?],
                )?;
                // The ACK, exact recovery payload, and the fact that its V4
                // capability was issued are one durable state transition. A
                // process death cannot leave a V3-shaped receipt which the
                // V4 recovery page is unable to finish.
                transaction.execute(
                    "INSERT INTO acknowledgements(capture_ordinal,identity,occurrence,v4_capability_issued) VALUES(?1,?2,?3,1)",
                    params![sql_integer(capture_ordinal)?, &identity[..], occurrence],
                )?;
            }
        }
        let secret: Vec<u8> = transaction.query_row(
            "SELECT retirement_secret FROM settings WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        if secret.len() != 32 {
            return Err(error(
                "storage.corrupt",
                "apple notification failure retirement secret is invalid",
            ));
        }
        let mut digest = Sha256::new();
        digest.update(&secret);
        digest.update(capture_ordinal.to_be_bytes());
        digest.update(identity);
        let capability = format!("{}:{:x}", capture_ordinal, digest.finalize());
        transaction.commit()?;
        Ok(capability)
    }

    /// Final V4 confirmation reclaims the receipt only when the caller
    /// presents the capability delivered by the committed ACK. A lost final
    /// reply remains idempotent because the capability MAC can be verified
    /// after the row has been deleted.
    pub fn confirm_acknowledgement_retirement_v4(
        &self,
        capture_ordinal: u64,
        occurrence: &AppleNotificationFailureOccurrence,
        capability: &str,
    ) -> Result<()> {
        let (_, identity) = Self::encoded(occurrence)?;
        self.verify_retirement_capability(capture_ordinal, &identity, capability)?;
        let mut connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT identity FROM acknowledgements WHERE capture_ordinal=?1",
                [sql_integer(capture_ordinal)?],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(receipt) = receipt {
            if receipt.as_slice() != identity {
                return Err(error(
                    "storage.identity",
                    "apple notification failure acknowledgement receipt differs",
                ));
            }
            transaction.execute(
                "DELETE FROM acknowledgements WHERE capture_ordinal=?1 AND identity=?2",
                params![sql_integer(capture_ordinal)?, &identity[..]],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Enumerates only V4 receipts whose ACK committed but whose final
    /// confirmation has not reclaimed the receipt. This makes a crash between
    /// those two operations recoverable without inventing an adapter cursor.
    pub fn pending_acknowledgement_retirements_v4(
        &self,
        max_items: u32,
    ) -> Result<Vec<(u64, AppleNotificationFailureOccurrence, String)>> {
        if max_items == 0 || max_items > 2048 {
            return Err(error(
                "argument.invalid",
                "apple notification failure retirement page bound is invalid",
            ));
        }
        let connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        let rows = connection.prepare(
            "SELECT capture_ordinal,identity,occurrence FROM acknowledgements WHERE v4_capability_issued=1 AND occurrence IS NOT NULL ORDER BY capture_ordinal LIMIT ?1",
        )?.query_map([i64::from(max_items)], |row| {
            Ok((unsigned(row, 0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, String>(2)?))
        })?.collect::<std::result::Result<Vec<_>, _>>()?;
        drop(connection);
        rows.into_iter()
            .map(|(ordinal, identity, body)| {
                let value: Value = serde_json::from_str(&body).map_err(|_| {
                    error(
                        "storage.corrupt",
                        "apple notification failure retirement occurrence is invalid",
                    )
                })?;
                if identity.len() != 32 {
                    return Err(error(
                        "storage.corrupt",
                        "apple notification failure retirement identity is invalid",
                    ));
                }
                let mut exact = [0_u8; 32];
                exact.copy_from_slice(&identity);
                let occurrence = AppleNotificationFailureOccurrence {
                    route: value["route"]
                        .as_str()
                        .ok_or_else(|| {
                            error(
                                "storage.corrupt",
                                "apple notification failure retirement route is invalid",
                            )
                        })?
                        .to_owned(),
                    epoch: value["epoch"]
                        .as_str()
                        .ok_or_else(|| {
                            error(
                                "storage.corrupt",
                                "apple notification failure retirement epoch is invalid",
                            )
                        })?
                        .parse()
                        .map_err(|_| {
                            error(
                                "storage.corrupt",
                                "apple notification failure retirement epoch is invalid",
                            )
                        })?,
                    sequence: value["sequence"]
                        .as_str()
                        .ok_or_else(|| {
                            error(
                                "storage.corrupt",
                                "apple notification failure retirement sequence is invalid",
                            )
                        })?
                        .parse()
                        .map_err(|_| {
                            error(
                                "storage.corrupt",
                                "apple notification failure retirement sequence is invalid",
                            )
                        })?,
                    failure: value["failure"].clone(),
                };
                let (_, recomputed) = Self::encoded(&occurrence)?;
                if recomputed != exact {
                    return Err(error(
                        "storage.corrupt",
                        "apple notification failure retirement identity differs",
                    ));
                }
                Ok((
                    ordinal,
                    occurrence,
                    self.retirement_capability(ordinal, &exact)?,
                ))
            })
            .collect()
    }

    /// The adapter restores its capture counter from this value before it
    /// admits another callback for this route. Otherwise a process restart
    /// could reuse a valid `(route, epoch, sequence)` idempotency key.
    pub fn highest_sequence(&self, route: &str, epoch: u64) -> Result<u64> {
        let connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        let value: i64 = connection.query_row(
            "SELECT coalesce((SELECT highest_sequence FROM watermarks WHERE route=?1 AND epoch=?2),0)",
            params![route, sql_integer(epoch)?],
            |row| row.get(0),
        )?;
        u64::try_from(value).map_err(|_| {
            error(
                "storage.corrupt",
                "apple notification failure sequence is invalid",
            )
        })
    }

    /// Nondecreasing capture order across every route. It remains after ACK so
    /// a restored adapter can allocate a new V1 delivery stream without
    /// confusing it with the original source identity.
    pub fn highest_capture_ordinal(&self) -> Result<u64> {
        let connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        Ok(connection.query_row(
            "SELECT highest_ordinal FROM capture_watermark WHERE id=1",
            [],
            |row| unsigned(row, 0),
        )?)
    }

    pub fn acknowledge(&self, occurrence: &AppleNotificationFailureOccurrence) -> Result<()> {
        let (_, identity) = Self::encoded(occurrence)?;
        let mut connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT identity FROM acknowledgements WHERE identity=?1 LIMIT 1",
                [&identity[..]],
                |row| row.get(0),
            )
            .optional()?;
        if receipt.as_deref() == Some(identity.as_slice()) {
            transaction.commit()?;
            return Ok(());
        }
        let head: Option<(u64, Vec<u8>)> = transaction.query_row(
            "SELECT capture_ordinal,identity FROM capture_stream ORDER BY capture_ordinal LIMIT 1",
            [],
            |row| Ok((unsigned(row, 0)?, row.get(1)?)),
        ).optional()?;
        let Some((capture_ordinal, head_identity)) = head else {
            return Err(error(
                "storage.identity",
                "apple notification failure acknowledgement is absent",
            ));
        };
        if head_identity != identity {
            return Err(error(
                "storage.sequence",
                "apple notification failure acknowledgement is not the global FIFO head",
            ));
        }
        let changed = transaction
            .execute("DELETE FROM occurrences WHERE identity=?1", [&identity[..]])?
            + transaction.execute(
                "DELETE FROM reservations WHERE identity=?1",
                [&identity[..]],
            )?;
        if changed != 1 {
            return Err(error(
                "storage.identity",
                "apple notification failure acknowledgement is absent",
            ));
        }
        transaction.execute(
            "DELETE FROM capture_stream WHERE capture_ordinal=?1",
            [sql_integer(capture_ordinal)?],
        )?;
        transaction.execute(
            "INSERT INTO acknowledgements(capture_ordinal,identity) VALUES(?1,?2)",
            params![sql_integer(capture_ordinal)?, &identity[..]],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Marks one durable lost-reply proof retired after the caller has durably
    /// accepted its ACK. The exact, retired receipt is intentionally retained:
    /// if this success reply is lost, retrying this operation must be safe and
    /// must not make a later capture look acknowledged. The sidecar page quota
    /// bounds retained receipts; it refuses further writes rather than evicting
    /// an identity proof.
    pub fn retire_acknowledgement(
        &self,
        capture_ordinal: u64,
        occurrence: &AppleNotificationFailureOccurrence,
    ) -> Result<()> {
        let (_, identity) = Self::encoded(occurrence)?;
        let mut connection = self.connection.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE acknowledgements SET retired=1 WHERE capture_ordinal=?1 AND identity=?2 AND retired=0",
            params![sql_integer(capture_ordinal)?, &identity[..]],
        )?;
        if changed == 0 {
            let exact_retired: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM acknowledgements WHERE capture_ordinal=?1 AND identity=?2 AND retired=1)",
                params![sql_integer(capture_ordinal)?, &identity[..]],
                |row| row.get(0),
            )?;
            if exact_retired {
                transaction.commit()?;
                return Ok(());
            }
            return Err(error(
                "storage.identity",
                "apple notification failure acknowledgement receipt is absent",
            ));
        }
        transaction.commit()?;
        Ok(())
    }
}

#[allow(
    clippy::type_complexity,
    reason = "The private recovery row exactly mirrors its retained SQLite occurrence schema."
)]
impl JournalPeerFailureRecoveryLedger {
    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| error("storage.io", "peer failure recovery ledger was interrupted"))
    }

    fn stage_begin(&self, occurrence: &DurablePeerFailureRecoveryOccurrence) -> Result<()> {
        let identity = peer_failure_identity(&occurrence.descriptor);
        let expected = sidecar_expected_bytes(occurrence)?;
        let encoded = encoded(
            &durable_peer_failure_recovery_occurrence_encode(occurrence),
            1 << 20,
        )?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, i64, String)> = tx
            .query_row(
                "SELECT occurrence,expected_bytes,state FROM occurrences WHERE identity=?1",
                [&identity[..]],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        match existing {
            Some((stored, bytes, state)) => {
                if stored != encoded || bytes != sql_integer(expected)? {
                    return Err(error(
                        "storage.identity",
                        "peer failure recovery occurrence differs for identity",
                    ));
                }
                if !matches!(
                    state.as_str(),
                    "staging" | "staged" | "source-ack-inflight" | "source-ack-committed"
                ) {
                    return Err(error(
                        "storage.corrupt",
                        "peer failure recovery state is invalid",
                    ));
                }
            }
            None => {
                tx.execute(
                    "INSERT INTO occurrences(identity,occurrence,expected_bytes,staged_bytes,digest,state,source_ack_id) VALUES(?1,?2,?3,0,NULL,'staging',NULL)",
                    params![&identity[..], encoded, sql_integer(expected)?],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Persist the entire recipient cut as one sidecar transaction.  The
    /// descriptor and recipient leases are immutable; only contiguous read
    /// prefixes and acknowledgement bits may advance.  This is intentionally
    /// a complete snapshot so a restart cannot combine separate recipients'
    /// different persistence generations.
    fn update_recipients(&self, occurrence: &DurablePeerFailureRecoveryOccurrence) -> Result<()> {
        let identity = peer_failure_identity(&occurrence.descriptor);
        let expected = sidecar_expected_bytes(occurrence)?;
        let encoded = encoded(
            &durable_peer_failure_recovery_occurrence_encode(occurrence),
            1 << 20,
        )?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (stored, stored_expected, state): (String, i64, String) = tx
            .query_row(
                "SELECT occurrence,expected_bytes,state FROM occurrences WHERE identity=?1",
                [&identity[..]],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| error("storage.identity", "peer failure recovery stage is absent"))?;
        if stored_expected != sql_integer(expected)?
            || !matches!(
                state.as_str(),
                "staging" | "staged" | "source-ack-inflight" | "source-ack-committed"
            )
        {
            return Err(error(
                "storage.corrupt",
                "peer failure recovery stage metadata is invalid",
            ));
        }
        let previous =
            durable_peer_failure_recovery_occurrence_decode(&decode(&stored)?).map_err(|_| {
                error(
                    "storage.corrupt",
                    "peer failure recovery occurrence cannot be decoded",
                )
            })?;
        if previous.descriptor != occurrence.descriptor
            || previous.source_ack_id != occurrence.source_ack_id
            || previous.producer_ack != occurrence.producer_ack
            || previous.recipients.len() != occurrence.recipients.len()
        {
            return Err(error(
                "storage.identity",
                "peer failure recovery occurrence identity differs",
            ));
        }
        for prior in &previous.recipients {
            let next = occurrence
                .recipients
                .iter()
                .find(|candidate| {
                    candidate.scope == prior.scope
                        && candidate.epoch == prior.epoch
                        && candidate.recipient_lease == prior.recipient_lease
                })
                .ok_or_else(|| {
                    error(
                        "storage.identity",
                        "peer failure recovery recipient identity differs",
                    )
                })?;
            if next.contiguous_read > occurrence.descriptor.encoded_length
                || (next.acknowledged
                    && next.contiguous_read != occurrence.descriptor.encoded_length)
                || next.contiguous_read < prior.contiguous_read
                || (prior.acknowledged && !next.acknowledged)
            {
                return Err(error(
                    "storage.identity",
                    "peer failure recovery recipient progress regressed",
                ));
            }
        }
        tx.execute(
            "UPDATE occurrences SET occurrence=?2 WHERE identity=?1",
            params![&identity[..], encoded],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn append_chunk(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        offset: u64,
        bytes: &[u8],
        done: bool,
    ) -> Result<()> {
        let identity = peer_failure_identity(descriptor);
        if bytes.is_empty() || bytes.len() > descriptor.chunk_bytes as usize {
            return Err(error(
                "argument.invalid",
                "peer failure recovery chunk bounds are invalid",
            ));
        }
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| error("argument.invalid", "peer failure recovery chunk overflows"))?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (expected, staged, state): (i64, i64, String) = tx
            .query_row(
                "SELECT expected_bytes,staged_bytes,state FROM occurrences WHERE identity=?1",
                [&identity[..]],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| error("storage.identity", "peer failure recovery stage is absent"))?;
        let expected = u64::try_from(expected).map_err(|_| {
            error(
                "storage.corrupt",
                "peer failure recovery expected byte count is invalid",
            )
        })?;
        let staged = u64::try_from(staged).map_err(|_| {
            error(
                "storage.corrupt",
                "peer failure recovery staged byte count is invalid",
            )
        })?;
        if state != "staging" {
            return Err(error(
                "storage.identity",
                "peer failure recovery stage is not writable",
            ));
        }
        if end > expected || done != (end == expected) {
            return Err(error(
                "argument.invalid",
                "peer failure recovery chunk completion differs",
            ));
        }
        if offset < staged {
            let saved: Vec<u8> = tx
                .query_row(
                    "SELECT CAST(bytes AS BLOB) FROM chunks WHERE identity=?1 AND offset=?2",
                    params![&identity[..], sql_integer(offset)?],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| {
                    error(
                        "storage.corrupt",
                        "peer failure recovery chunk gap is retained",
                    )
                })?;
            if saved != bytes {
                return Err(error(
                    "storage.identity",
                    "peer failure recovery chunk differs on retry",
                ));
            }
        } else if offset == staged {
            tx.execute(
                "INSERT INTO chunks(identity,offset,bytes) VALUES(?1,?2,?3)",
                params![&identity[..], sql_integer(offset)?, bytes],
            )?;
            tx.execute(
                "UPDATE occurrences SET staged_bytes=?2 WHERE identity=?1",
                params![&identity[..], sql_integer(end)?],
            )?;
        } else {
            return Err(error(
                "storage.identity",
                "peer failure recovery chunk is not contiguous",
            ));
        }
        tx.commit()?;
        Ok(())
    }

    fn stage_commit(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    ) -> Result<()> {
        let identity = peer_failure_identity(descriptor);
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (expected, staged, state): (i64, i64, String) = tx
            .query_row(
                "SELECT expected_bytes,staged_bytes,state FROM occurrences WHERE identity=?1",
                [&identity[..]],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| error("storage.identity", "peer failure recovery stage is absent"))?;
        if state == "staged" || state == "source-ack-inflight" || state == "source-ack-committed" {
            tx.commit()?;
            return Ok(());
        }
        if state != "staging"
            || expected != staged
            || expected != sql_integer(descriptor.encoded_length)?
        {
            return Err(error(
                "storage.identity",
                "peer failure recovery stage is incomplete",
            ));
        }
        let mut digest = Sha256::new();
        let mut next = 0_u64;
        {
            let mut statement = tx.prepare(
                "SELECT offset,CAST(bytes AS BLOB) FROM chunks WHERE identity=?1 ORDER BY offset",
            )?;
            let mut rows = statement.query([&identity[..]])?;
            while let Some(row) = rows.next()? {
                let offset = unsigned(row, 0)?;
                let bytes: Vec<u8> = row.get(1)?;
                if offset != next || bytes.is_empty() {
                    return Err(error(
                        "storage.corrupt",
                        "peer failure recovery chunks are not contiguous",
                    ));
                }
                next = next.checked_add(bytes.len() as u64).ok_or_else(|| {
                    error("storage.corrupt", "peer failure recovery bytes overflow")
                })?;
                digest.update(&bytes);
            }
        }
        if next != descriptor.encoded_length {
            return Err(error(
                "storage.corrupt",
                "peer failure recovery chunk length differs",
            ));
        }
        tx.execute(
            "UPDATE occurrences SET digest=?2,state='staged' WHERE identity=?1",
            params![&identity[..], digest.finalize().as_slice()],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn mark_inflight(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        source_ack_id: &str,
    ) -> Result<()> {
        if source_ack_id.is_empty() || source_ack_id.len() > 256 {
            return Err(error(
                "argument.invalid",
                "invalid peer failure source acknowledgement",
            ));
        }
        self.transition_source_ack(descriptor, source_ack_id, false)
    }

    fn confirm_ack(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        source_ack_id: &str,
    ) -> Result<()> {
        self.transition_source_ack(descriptor, source_ack_id, true)
    }

    fn transition_source_ack(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        source_ack_id: &str,
        confirm: bool,
    ) -> Result<()> {
        let identity = peer_failure_identity(descriptor);
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (stored, state, receipt): (String, String, Option<String>) = tx
            .query_row(
                "SELECT occurrence,state,source_ack_id FROM occurrences WHERE identity=?1",
                [&identity[..]],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| error("storage.identity", "peer failure recovery stage is absent"))?;
        if receipt
            .as_deref()
            .is_some_and(|value| value != source_ack_id)
        {
            return Err(error(
                "storage.identity",
                "peer failure source acknowledgement differs",
            ));
        }
        let next = match (confirm, state.as_str()) {
            (false, "staged") => "source-ack-inflight",
            (false, "source-ack-inflight") => "source-ack-inflight",
            (true, "source-ack-inflight") => "source-ack-committed",
            (true, "source-ack-committed") => "source-ack-committed",
            _ => {
                return Err(error(
                    "storage.identity",
                    "peer failure source acknowledgement state is invalid",
                ));
            }
        };
        let mut occurrence = durable_peer_failure_recovery_occurrence_decode(&decode(&stored)?)
            .map_err(|_| {
                error(
                    "storage.corrupt",
                    "peer failure recovery occurrence cannot be decoded",
                )
            })?;
        if occurrence.descriptor != *descriptor {
            return Err(error(
                "storage.corrupt",
                "peer failure recovery occurrence descriptor differs",
            ));
        }
        if occurrence
            .source_ack_id
            .as_deref()
            .is_some_and(|value| value != source_ack_id)
        {
            return Err(error(
                "storage.corrupt",
                "peer failure recovery occurrence source acknowledgement differs",
            ));
        }
        occurrence.source_ack_id = Some(source_ack_id.to_owned());
        occurrence.producer_ack = if confirm {
            crate::central::DurablePeerFailureProducerAck::Committed
        } else {
            crate::central::DurablePeerFailureProducerAck::InFlight
        };
        let occurrence = encoded(
            &durable_peer_failure_recovery_occurrence_encode(&occurrence),
            1 << 20,
        )?;
        tx.execute(
            "UPDATE occurrences SET occurrence=?2,state=?3,source_ack_id=?4 WHERE identity=?1",
            params![&identity[..], occurrence, next, source_ack_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn release_stage(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    ) -> Result<()> {
        let identity = peer_failure_identity(descriptor);
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM occurrences WHERE identity=?1",
                [&identity[..]],
                |row| row.get(0),
            )
            .optional()?;
        match state.as_deref() {
            None => {}
            Some("staging") | Some("staged") | Some("source-ack-committed") => {
                tx.execute("DELETE FROM occurrences WHERE identity=?1", [&identity[..]])?;
            }
            Some("source-ack-inflight") => {
                return Err(error(
                    "storage.identity",
                    "peer failure source acknowledgement remains in flight",
                ));
            }
            Some(_) => {
                return Err(error(
                    "storage.corrupt",
                    "peer failure recovery state is invalid",
                ));
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Recover every fully staged sidecar occurrence after a process restart.
    /// Staging rows never reached the durable byte/digest fence and therefore
    /// cannot authorize a source ACK. The first recovery pass of an
    /// OS-authorized, singleton ledger reclaims them; other calls exclude
    /// them rather than mistaking an active writer for crash residue. This
    /// verifies chunks incrementally before handing central a canonical
    /// occurrence; it never materializes a whole evidence body.
    pub fn recover_all(&self) -> Result<Vec<DurablePeerFailureRecoveryOccurrence>> {
        let connection = self.connection()?;
        let reclaim_startup_staging = {
            let started = self.startup_recovery.lock().map_err(|_| {
                error(
                    "storage.io",
                    "peer failure startup recovery was interrupted",
                )
            })?;
            if *started {
                false
            } else {
                // `JournalRegistry` holds this OS lock for the entire
                // journal lifetime. Direct test/offline opens intentionally
                // lack that proof and therefore never reclaim hidden rows.
                self._journal.authority.is_some()
            }
        };
        if reclaim_startup_staging {
            connection.execute("DELETE FROM occurrences WHERE state='staging'", [])?;
        }
        // Do not consume the one cold-reclaim attempt until the delete has
        // succeeded. A transient SQLite failure must leave the next explicit
        // recovery call able to retry safely under the same authority lock.
        if reclaim_startup_staging || self._journal.authority.is_none() {
            *self.startup_recovery.lock().map_err(|_| {
                error(
                    "storage.io",
                    "peer failure startup recovery was interrupted",
                )
            })? = true;
        }
        let rows = {
            let mut statement = connection.prepare(
                "SELECT identity,occurrence,expected_bytes,staged_bytes,source_ack_id,state,digest FROM occurrences WHERE state!='staging' ORDER BY identity",
            )?;
            let mut rows = statement.query([])?;
            let mut retained = Vec::new();
            while let Some(row) = rows.next()? {
                retained.push((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<Vec<u8>>>(6)?,
                ));
            }
            retained
        };
        let mut recovered = Vec::with_capacity(rows.len());
        for (identity, occurrence, expected, staged, source_ack_id, state, digest) in rows {
            let occurrence = durable_peer_failure_recovery_occurrence_decode(&decode(&occurrence)?)
                .map_err(|_| {
                    error(
                        "storage.corrupt",
                        "peer failure recovery occurrence cannot be decoded",
                    )
                })?;
            if identity != peer_failure_identity(&occurrence.descriptor)
                || expected != sql_integer(occurrence.descriptor.encoded_length)?
                || staged != expected
            {
                return Err(error(
                    "storage.corrupt",
                    "peer failure recovery occurrence differs",
                ));
            }
            let expected_producer_ack = match state.as_str() {
                "staged" => crate::central::DurablePeerFailureProducerAck::NotStarted,
                "source-ack-inflight" => crate::central::DurablePeerFailureProducerAck::InFlight,
                "source-ack-committed" => crate::central::DurablePeerFailureProducerAck::Committed,
                _ => {
                    return Err(error(
                        "storage.corrupt",
                        "peer failure recovery state is invalid",
                    ));
                }
            };
            if occurrence.source_ack_id != source_ack_id
                || occurrence.producer_ack != expected_producer_ack
            {
                return Err(error(
                    "storage.corrupt",
                    "peer failure recovery acknowledgement envelope differs",
                ));
            }
            let digest = digest.ok_or_else(|| {
                error(
                    "storage.corrupt",
                    "peer failure recovery digest is missing after stage",
                )
            })?;
            let mut actual = Sha256::new();
            let mut next = 0_u64;
            let mut statement = connection.prepare(
                "SELECT offset,CAST(bytes AS BLOB) FROM chunks WHERE identity=?1 ORDER BY offset",
            )?;
            let mut chunks = statement.query([&identity[..]])?;
            while let Some(row) = chunks.next()? {
                let offset = unsigned(row, 0)?;
                let bytes: Vec<u8> = row.get(1)?;
                if offset != next || bytes.is_empty() {
                    return Err(error(
                        "storage.corrupt",
                        "peer failure recovery chunks are not contiguous",
                    ));
                }
                next = next.checked_add(bytes.len() as u64).ok_or_else(|| {
                    error("storage.corrupt", "peer failure recovery bytes overflow")
                })?;
                actual.update(&bytes);
            }
            if next != occurrence.descriptor.encoded_length
                || actual.finalize().as_slice() != digest.as_slice()
            {
                return Err(error(
                    "storage.corrupt",
                    "peer failure recovery durable bytes differ",
                ));
            }
            self.recovered
                .lock()
                .map_err(|_| error("storage.io", "peer failure recovery ledger was interrupted"))?
                .insert(peer_failure_identity(&occurrence.descriptor));
            recovered.push(occurrence);
        }
        Ok(recovered)
    }

    fn read_recovered_chunk(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        offset: u64,
    ) -> Result<ubm_core::oversized_evidence::PeerSourceFailureChunk> {
        let identity = peer_failure_identity(descriptor);
        if !self
            .recovered
            .lock()
            .map_err(|_| error("storage.io", "peer failure recovery ledger was interrupted"))?
            .contains(&identity)
        {
            return Err(error(
                "storage.identity",
                "peer failure recovery has not passed restart integrity validation",
            ));
        }
        let connection = self.connection()?;
        let (expected, state): (i64, String) = connection
            .query_row(
                "SELECT expected_bytes,state FROM occurrences WHERE identity=?1",
                [&identity[..]],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| error("storage.identity", "peer failure recovery stage is absent"))?;
        if expected != sql_integer(descriptor.encoded_length)?
            || !matches!(
                state.as_str(),
                "staged" | "source-ack-inflight" | "source-ack-committed"
            )
            || offset >= descriptor.encoded_length
        {
            return Err(error(
                "storage.identity",
                "peer failure recovery read is outside the retained evidence",
            ));
        }
        let bytes: Vec<u8> = connection
            .query_row(
                "SELECT CAST(bytes AS BLOB) FROM chunks WHERE identity=?1 AND offset=?2",
                params![&identity[..], sql_integer(offset)?],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| error("storage.corrupt", "peer failure recovery chunk is missing"))?;
        let next_offset = offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| error("storage.corrupt", "peer failure recovery bytes overflow"))?;
        if bytes.is_empty()
            || bytes.len() > descriptor.chunk_bytes as usize
            || next_offset > descriptor.encoded_length
        {
            return Err(error(
                "storage.corrupt",
                "peer failure recovery chunk bounds are invalid",
            ));
        }
        Ok(ubm_core::oversized_evidence::PeerSourceFailureChunk {
            offset,
            next_offset,
            done: next_offset == descriptor.encoded_length,
            bytes,
        })
    }

    #[cfg(test)]
    fn recover(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    ) -> Result<Option<DurablePeerFailureSidecarOccurrence>> {
        let identity = peer_failure_identity(descriptor);
        let connection = self.connection()?;
        let row: Option<(String, i64, i64, Option<String>, String, Option<Vec<u8>>)> = connection
            .query_row(
                "SELECT occurrence,expected_bytes,staged_bytes,source_ack_id,state,digest FROM occurrences WHERE identity=?1",
                [&identity[..]],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .optional()?;
        row.map(|(occurrence, expected, staged, source_ack_id, state, digest)| {
            let occurrence = durable_peer_failure_recovery_occurrence_decode(&decode(&occurrence)?)
                .map_err(|_| {
                    error(
                        "storage.corrupt",
                        "peer failure recovery occurrence cannot be decoded",
                    )
                })?;
            if occurrence.descriptor != *descriptor
                || expected != sql_integer(descriptor.encoded_length)?
            {
                return Err(error(
                    "storage.corrupt",
                    "peer failure recovery occurrence differs",
                ));
            }
            let expected_producer_ack = match state.as_str() {
                "staging" | "staged" => crate::central::DurablePeerFailureProducerAck::NotStarted,
                "source-ack-inflight" => crate::central::DurablePeerFailureProducerAck::InFlight,
                "source-ack-committed" => crate::central::DurablePeerFailureProducerAck::Committed,
                _ => {
                    return Err(error(
                        "storage.corrupt",
                        "peer failure recovery state is invalid",
                    ));
                }
            };
            if occurrence.source_ack_id != source_ack_id || occurrence.producer_ack != expected_producer_ack {
                return Err(error(
                    "storage.corrupt",
                    "peer failure recovery acknowledgement envelope differs",
                ));
            }
            let durable_complete = state != "staging";
            let state = match state.as_str() {
                "staging" | "staged" => PeerFailureRecoveryState::Staged,
                "source-ack-inflight" => PeerFailureRecoveryState::SourceAckInFlight,
                "source-ack-committed" => PeerFailureRecoveryState::SourceAckCommitted,
                _ => {
                    return Err(error(
                        "storage.corrupt",
                        "peer failure recovery state is invalid",
                    ));
                }
            };
            if durable_complete {
                let digest = digest.ok_or_else(|| {
                    error(
                        "storage.corrupt",
                        "peer failure recovery digest is missing after stage",
                    )
                })?;
                let mut actual = Sha256::new();
                let mut next = 0_u64;
                let mut statement = connection.prepare(
                    "SELECT offset,CAST(bytes AS BLOB) FROM chunks WHERE identity=?1 ORDER BY offset",
                )?;
                let mut rows = statement.query([&identity[..]])?;
                while let Some(row) = rows.next()? {
                    let offset = unsigned(row, 0)?;
                    let bytes: Vec<u8> = row.get(1)?;
                    if offset != next || bytes.is_empty() {
                        return Err(error(
                            "storage.corrupt",
                            "peer failure recovery chunks are not contiguous",
                        ));
                    }
                    next = next.checked_add(bytes.len() as u64).ok_or_else(|| {
                        error("storage.corrupt", "peer failure recovery bytes overflow")
                    })?;
                    actual.update(bytes);
                }
                if next != descriptor.encoded_length || actual.finalize().as_slice() != digest.as_slice() {
                    return Err(error(
                        "storage.corrupt",
                        "peer failure recovery durable bytes differ",
                    ));
                }
            }
            Ok(DurablePeerFailureSidecarOccurrence {
                occurrence,
                source_ack_id,
                state,
                staged_bytes: u64::try_from(staged).map_err(|_| {
                    error(
                        "storage.corrupt",
                        "peer failure recovery staged byte count is invalid",
                    )
                })?,
            })
        })
        .transpose()
    }
}

impl PeerFailureRecoveryLedger for JournalPeerFailureRecoveryLedger {
    fn read_recovered_chunk<'a>(
        &'a self,
        descriptor: &'a ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        offset: u64,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = std::result::Result<
                        ubm_core::oversized_evidence::PeerSourceFailureChunk,
                        DesktopError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.read_recovered_chunk(descriptor, offset)
                .map_err(peer_failure_sidecar_error)
        })
    }

    fn recover_all<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = std::result::Result<
                        Vec<DurablePeerFailureRecoveryOccurrence>,
                        DesktopError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.recover_all().map_err(peer_failure_sidecar_error) })
    }

    fn begin_stage<'a>(
        &'a self,
        occurrence: &'a DurablePeerFailureRecoveryOccurrence,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), DesktopError>> + Send + 'a>> {
        Box::pin(async move {
            self.stage_begin(occurrence)
                .map_err(peer_failure_sidecar_error)
        })
    }

    fn append_stage_chunk<'a>(
        &'a self,
        descriptor: &'a ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        offset: u64,
        bytes: &'a [u8],
        done: bool,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), DesktopError>> + Send + 'a>> {
        Box::pin(async move {
            self.append_chunk(descriptor, offset, bytes, done)
                .map_err(peer_failure_sidecar_error)
        })
    }

    fn commit_stage<'a>(
        &'a self,
        descriptor: &'a ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), DesktopError>> + Send + 'a>> {
        Box::pin(async move {
            self.stage_commit(descriptor)
                .map_err(peer_failure_sidecar_error)
        })
    }

    fn update_recipients<'a>(
        &'a self,
        occurrence: &'a DurablePeerFailureRecoveryOccurrence,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), DesktopError>> + Send + 'a>> {
        Box::pin(async move {
            self.update_recipients(occurrence)
                .map_err(peer_failure_sidecar_error)
        })
    }

    fn mark_source_ack_inflight<'a>(
        &'a self,
        descriptor: &'a ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        source_ack_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), DesktopError>> + Send + 'a>> {
        Box::pin(async move {
            self.mark_inflight(descriptor, source_ack_id)
                .map_err(peer_failure_sidecar_error)
        })
    }

    fn confirm_source_ack<'a>(
        &'a self,
        descriptor: &'a ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        source_ack_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), DesktopError>> + Send + 'a>> {
        Box::pin(async move {
            self.confirm_ack(descriptor, source_ack_id)
                .map_err(peer_failure_sidecar_error)
        })
    }

    fn release<'a>(
        &'a self,
        descriptor: &'a ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), DesktopError>> + Send + 'a>> {
        Box::pin(async move {
            self.release_stage(descriptor)
                .map_err(peer_failure_sidecar_error)
        })
    }
}

/// Durable state of the native producer acknowledgement. The occurrence stays
/// retained through `SourceAckCommitted` until central has reconciled the
/// producer receipt and explicitly releases it.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Recovery state is retained for durable peer-failure replay coverage."
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerFailureRecoveryState {
    Staged,
    SourceAckInFlight,
    SourceAckCommitted,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Storage conversion is retained for peer-failure recovery replay."
    )
)]
impl PeerFailureRecoveryState {
    fn from_storage(value: &str) -> Result<Self> {
        match value {
            "staged" => Ok(Self::Staged),
            "source-ack-inflight" => Ok(Self::SourceAckInFlight),
            "source-ack-committed" => Ok(Self::SourceAckCommitted),
            _ => Err(error(
                "storage.corrupt",
                "invalid peer failure recovery state",
            )),
        }
    }

    const fn storage(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::SourceAckInFlight => "source-ack-inflight",
            Self::SourceAckCommitted => "source-ack-committed",
        }
    }
}

fn peer_failure_sidecar_error(failure: JournalError) -> DesktopError {
    let mut platform = PlatformDetail::new("journal", failure.kind);
    platform.metadata.insert(
        "operation".into(),
        crate::errors::PlatformValue::Text(failure.operation.into()),
    );
    if let Some(code) = failure.sqlite_code {
        platform.metadata.insert(
            "sqliteCode".into(),
            crate::errors::PlatformValue::Text(code),
        );
    }
    if let Some(code) = failure.sqlite_extended_code {
        platform.metadata.insert(
            "sqliteExtendedCode".into(),
            crate::errors::PlatformValue::Int(i64::from(code)),
        );
    }
    DesktopError::new(
        BleErrorCode::PlatformFailure,
        BleErrorDomain::Restoration,
        "gatt.notification.source-failure-recovery.ledger",
    )
    .with_detail(failure.detail)
    .with_platform(platform)
}

fn peer_failure_identity(
    descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
) -> [u8; 96] {
    let mut identity = [0_u8; 96];
    identity[..16].copy_from_slice(&descriptor.identity.host);
    identity[16..32].copy_from_slice(&descriptor.identity.peer);
    identity[32..48].copy_from_slice(&descriptor.identity.source_generation);
    identity[48..64].copy_from_slice(&descriptor.identity.native_group);
    identity[64..80].copy_from_slice(&descriptor.identity.transfer);
    identity[80..96].copy_from_slice(&descriptor.identity.recovery);
    identity
}

fn sidecar_expected_bytes(occurrence: &DurablePeerFailureRecoveryOccurrence) -> Result<u64> {
    if occurrence.descriptor.encoded_length == 0 {
        return Err(error(
            "argument.invalid",
            "peer failure recovery evidence cannot be empty",
        ));
    }
    Ok(occurrence.descriptor.encoded_length)
}

/// Transactionally retained opaque central occurrence envelope plus the exact
/// native bytes. The JSON envelope is canonicalized by central; journal never
/// synthesizes route, lease, error, or source-ACK identities.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Peer-failure recovery envelopes are retained for the durable sidecar protocol."
    )
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerFailureRecovery {
    pub(crate) occurrence: Value,
    pub(crate) evidence: Vec<u8>,
    pub(crate) state: PeerFailureRecoveryState,
}

/// The authenticated scan-owner domain for a private recording.  This is
/// retained independently of owner envelopes while a finalized cleanup
/// receipt still needs recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScanFailureRecordingRecoveryAuthority {
    pub(crate) session_id: u64,
    pub(crate) authority: [u8; 32],
}

#[allow(
    clippy::collapsible_if,
    clippy::result_large_err,
    clippy::too_many_arguments,
    clippy::type_complexity,
    reason = "The durable journal implementation preserves fixed private receipt, recovery, and callback shapes; restructuring them would change its retained ownership boundaries."
)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "The durable recovery API is covered by journal integration tests while this module also compiles in the library."
    )
)]
impl ContinuationJournal {
    /// Select one exact raw-evidence source.  A release may have either the
    /// old sibling v1 file or the private v2 file, but never silently merges
    /// them: equal identities from two files would make the export ambiguous.
    fn open_scan_failure_evidence_for_export(&self) -> Result<Connection> {
        let current = scan_failure_sidecar_file_exists(
            &self.scan_failure_sidecar_path,
            "scan recovery sidecar is not a regular file",
        )?;
        let legacy = scan_failure_sidecar_file_exists(
            &self.legacy_scan_failure_sidecar_path,
            "legacy scan recovery sidecar is not a regular file",
        )?;
        if current && legacy {
            return Err(error(
                "storage.identity",
                "legacy and private scan recovery sidecars both exist; export paths collide",
            ));
        }
        if legacy {
            return open_legacy_scan_failure_sidecar_for_export(
                &self.legacy_scan_failure_sidecar_path,
            )?
            .ok_or_else(|| error("storage.io", "legacy scan recovery sidecar disappeared"));
        }
        if !current {
            // No export-only legacy input exists. This is the first v2
            // operation, so establish the protected directory before SQLite
            // is permitted to create the sidecar or its rollback journal.
            return self.open_private_scan_failure_sidecar();
        }
        open_scan_failure_sidecar(
            &self.scan_failure_sidecar_path,
            self.quota,
            self.scan_recovery_storage.as_deref(),
        )
    }

    fn open_private_scan_failure_sidecar(&self) -> Result<Connection> {
        if scan_failure_sidecar_file_exists(
            &self.legacy_scan_failure_sidecar_path,
            "legacy scan recovery sidecar is not a regular file",
        )? {
            return Err(error(
                "storage.identity",
                "legacy scan recovery sidecar must be exported before creating private recovery",
            ));
        }
        // Provision the protected directory only after ruling out a released
        // v1 sibling.  In particular, read-only v1 export must leave no
        // `.ubm-scan-recovery` directory behind.
        let path = secure_scan_failure_sidecar_path(
            &self.journal_path,
            self.scan_recovery_storage.as_deref(),
        )?;
        open_scan_failure_sidecar(&path, self.quota, self.scan_recovery_storage.as_deref())
    }

    fn uses_released_v1_schema(&self) -> bool {
        self.schema_version == 1
    }

    /// Bind this already-authorized recording to a streaming peer-failure
    /// ledger. This is explicit host configuration: the central never derives
    /// a directory or creates this sidecar by itself.
    pub fn peer_failure_recovery_ledger(
        self: &Arc<Self>,
    ) -> Result<Arc<JournalPeerFailureRecoveryLedger>> {
        let mut cached = self.peer_failure_ledger.lock().map_err(|_| {
            error(
                "storage.io",
                "peer failure recovery ledger cache was interrupted",
            )
        })?;
        if let Some(ledger) = cached.upgrade() {
            return Ok(ledger);
        }
        let connection = open_peer_failure_sidecar(&self.peer_failure_sidecar_path, self.quota)?;
        let ledger = Arc::new(JournalPeerFailureRecoveryLedger {
            _journal: Arc::clone(self),
            connection: Mutex::new(connection),
            recovered: Mutex::new(HashSet::new()),
            startup_recovery: Mutex::new(false),
        });
        *cached = Arc::downgrade(&ledger);
        Ok(ledger)
    }

    /// Atomically retains an unprepared scan failure before an outbox consumes
    /// its source-time occurrence.  This is idempotent only for byte-for-byte
    /// identical identity, members, and raw tokens; callers retain ownership
    /// on any error and therefore never replace the source with a summary.
    pub(crate) fn capture_scan_failure_occurrence(
        &self,
        occurrence: &ubm_core::oversized_evidence::ScanFailureOccurrence,
    ) -> Result<()> {
        self.capture_scan_failure_occurrence_inner(occurrence, None, None)
    }

    /// Atomically captures an exact source-time cut and the opaque owner that
    /// makes that cut restartable. Unlike the raw-only export API above, this
    /// never leaves a v2 source cut without its authority envelope.
    pub(crate) fn capture_scan_failure_occurrence_with_owner(
        &self,
        occurrence: &ubm_core::oversized_evidence::ScanFailureOccurrence,
        owner_envelope: &[u8],
        recovery_authority: Option<ScanFailureRecordingRecoveryAuthority>,
    ) -> Result<()> {
        self.capture_scan_failure_occurrence_inner(
            occurrence,
            Some(owner_envelope),
            recovery_authority,
        )
    }

    fn capture_scan_failure_occurrence_inner(
        &self,
        occurrence: &ubm_core::oversized_evidence::ScanFailureOccurrence,
        owner_envelope: Option<&[u8]>,
        recovery_authority: Option<ScanFailureRecordingRecoveryAuthority>,
    ) -> Result<()> {
        use std::borrow::Cow;
        use ubm_core::oversized_evidence::EvidenceToken;
        fn token_parts(token: &EvidenceToken) -> (i64, Cow<'_, [u8]>) {
            match token {
                EvidenceToken::Null => (0, Cow::Borrowed(&[])),
                EvidenceToken::Bool(value) => (1, Cow::Owned(vec![u8::from(*value)])),
                EvidenceToken::I64(value) => (2, Cow::Owned(value.to_be_bytes().to_vec())),
                EvidenceToken::U64(value) => (3, Cow::Owned(value.to_be_bytes().to_vec())),
                EvidenceToken::F64Bits(value) => (4, Cow::Owned(value.to_be_bytes().to_vec())),
                EvidenceToken::Text(value) => (5, Cow::Borrowed(value.as_bytes())),
                EvidenceToken::Bytes(value) => (6, Cow::Borrowed(value)),
                EvidenceToken::Map(value) => (7, Cow::Owned(value.to_be_bytes().to_vec())),
                EvidenceToken::Array(value) => (8, Cow::Owned(value.to_be_bytes().to_vec())),
            }
        }
        let source = occurrence.source_tokens().ok_or_else(|| {
            error(
                "storage.identity",
                "scan failure source is no longer retained",
            )
        })?;
        if owner_envelope.is_some_and(|body| body.is_empty()) {
            return Err(error(
                "argument.invalid",
                "scan failure owner envelope is empty",
            ));
        }
        let identity = scan_failure_identity_bytes(occurrence.identity());
        let connection = self.open_private_scan_failure_sidecar()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCAN_FAILURE_SIDECAR_SCHEMA_VERSION {
            return Err(error(
                "storage.schema",
                "scan failure sidecar is v1 and cannot own a restartable occurrence",
            ));
        }
        let tx = connection.unchecked_transaction()?;
        if let Some(recovery_authority) = recovery_authority {
            let existing: Option<(Vec<u8>, Vec<u8>)> = tx
                .query_row(
                    "SELECT session_id,authority FROM recording_recovery_authority WHERE id=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            match existing {
                None => {
                    tx.execute(
                        "INSERT INTO recording_recovery_authority(id,session_id,authority) VALUES(1,?1,?2)",
                        params![recovery_authority.session_id.to_be_bytes().as_slice(), recovery_authority.authority.as_slice()],
                    )?;
                }
                Some((session_id, authority))
                    if session_id.as_slice() == recovery_authority.session_id.to_be_bytes()
                        && authority.as_slice() == recovery_authority.authority => {}
                Some(_) => {
                    return Err(error(
                        "storage.identity",
                        "scan failure recording recovery authority differs",
                    ));
                }
            }
        }
        let existing: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM occurrences WHERE identity=?1",
                [&identity],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_none() {
            tx.execute("INSERT INTO occurrences(identity) VALUES(?1)", [&identity])?;
            for (position, member) in occurrence.members().iter().enumerate() {
                tx.execute(
                    "INSERT INTO members(identity,position,recipient,delivery) VALUES(?1,?2,?3,?4)",
                    params![
                        identity,
                        position as i64,
                        member.recipient.as_slice(),
                        member.delivery.as_slice()
                    ],
                )?;
            }
            for (position, token) in source.iter().enumerate() {
                let (tag, payload) = token_parts(token);
                tx.execute(
                    "INSERT INTO tokens(identity,position,tag,payload) VALUES(?1,?2,?3,?4)",
                    params![identity, position as i64, tag, payload.as_ref()],
                )?;
            }
        } else {
            let member_count: i64 = tx.query_row(
                "SELECT count(*) FROM members WHERE identity=?1",
                [&identity],
                |row| row.get(0),
            )?;
            let token_count: i64 = tx.query_row(
                "SELECT count(*) FROM tokens WHERE identity=?1",
                [&identity],
                |row| row.get(0),
            )?;
            if member_count != occurrence.members().len() as i64
                || token_count != source.len() as i64
            {
                return Err(error("storage.identity", "scan failure identity differs"));
            }
            // A retry after the caller lost our success reply is valid only
            // when it carries the same source-time cut.  Cardinality alone
            // would let a reused identity silently replace what a recipient
            // must later recover, so compare every member and token at its
            // original position before treating this as idempotent.
            for (position, member) in occurrence.members().iter().enumerate() {
                let retained: (Vec<u8>, Vec<u8>) = tx.query_row(
                    "SELECT recipient,delivery FROM members WHERE identity=?1 AND position=?2",
                    params![identity, position as i64],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if retained.0.as_slice() != member.recipient
                    || retained.1.as_slice() != member.delivery
                {
                    return Err(error("storage.identity", "scan failure identity differs"));
                }
            }
            for (position, token) in source.iter().enumerate() {
                let (expected_tag, expected_payload) = token_parts(token);
                let retained: (i64, Vec<u8>) = tx.query_row(
                    "SELECT tag,CAST(payload AS BLOB) FROM tokens WHERE identity=?1 AND position=?2",
                    params![identity, position as i64],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if retained.0 != expected_tag || retained.1.as_slice() != expected_payload.as_ref()
                {
                    return Err(error("storage.identity", "scan failure identity differs"));
                }
            }
        }
        if let Some(body) = owner_envelope {
            let existing_owner: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT body FROM owner_envelopes WHERE identity=?1 AND version=2",
                    [&identity],
                    |row| row.get(0),
                )
                .optional()?;
            match (existing.is_some(), existing_owner) {
                (false, None) => {
                    tx.execute(
                        "INSERT INTO owner_envelopes(identity,version,body) VALUES(?1,2,?2)",
                        params![identity, body],
                    )?;
                }
                (true, Some(existing)) if existing == body => {}
                (true, Some(_)) => {
                    return Err(error(
                        "storage.identity",
                        "scan failure owner envelope differs",
                    ));
                }
                // A legacy two-step capture may have crashed after the raw
                // cut committed. Do not promote that orphan into a new
                // restartable owner: it has no proved ordinal or authority.
                (true, None) => {
                    return Err(error(
                        "storage.identity",
                        "scan failure source has no owner envelope",
                    ));
                }
                (false, Some(_)) => unreachable!("owner envelope references raw cut"),
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Store the outbox-owned v2 occurrence envelope after the raw source cut
    /// is durable.  The journal treats it as opaque bytes: parsing, defaults,
    /// and reconstruction belong to the outbox so this layer cannot fabricate
    /// a capability, receipt, limit, or source budget during recovery.
    pub(crate) fn store_scan_failure_owner_envelope(
        &self,
        identity: ubm_core::oversized_evidence::ScanFailureIdentity,
        body: &[u8],
    ) -> Result<()> {
        if body.is_empty() {
            return Err(error(
                "argument.invalid",
                "scan failure owner envelope is empty",
            ));
        }
        let identity = scan_failure_identity_bytes(identity);
        let connection = self.open_private_scan_failure_sidecar()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCAN_FAILURE_SIDECAR_SCHEMA_VERSION {
            return Err(error(
                "storage.schema",
                "scan failure sidecar is incomplete v1 evidence",
            ));
        }
        let tx = connection.unchecked_transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM occurrences WHERE identity=?1)",
            [&identity],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(error(
                "storage.identity",
                "scan failure source was not captured",
            ));
        }
        let existing: Option<Vec<u8>> = tx
            .query_row(
                "SELECT body FROM owner_envelopes WHERE identity=?1",
                [&identity],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing != body {
                return Err(error(
                    "storage.identity",
                    "scan failure owner envelope differs",
                ));
            }
        } else {
            tx.execute(
                "INSERT INTO owner_envelopes(identity,version,body) VALUES(?1,2,?2)",
                params![identity, body],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Compare-and-swap a v2 envelope at one linearized outbox transition.
    /// The caller records the exact previously durable bytes and writes the
    /// successor before exposing the corresponding transition result.  This
    /// prevents a stale retry from replacing a newer receipt bitmap or release
    /// tombstone after a crash/reopen.
    pub(crate) fn advance_scan_failure_owner_envelope(
        &self,
        identity: ubm_core::oversized_evidence::ScanFailureIdentity,
        previous: &[u8],
        next: &[u8],
    ) -> Result<()> {
        if previous.is_empty() || next.is_empty() {
            return Err(error(
                "argument.invalid",
                "scan failure owner envelope is empty",
            ));
        }
        let identity = scan_failure_identity_bytes(identity);
        let connection = self.open_private_scan_failure_sidecar()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCAN_FAILURE_SIDECAR_SCHEMA_VERSION {
            return Err(error(
                "storage.schema",
                "scan failure sidecar is incomplete v1 evidence",
            ));
        }
        let changed = connection.execute(
            "UPDATE owner_envelopes SET body=?3 WHERE identity=?1 AND version=2 AND body=?2",
            params![identity, previous, next],
        )?;
        if changed != 1 {
            // SQLite can report a failed caller after its commit reached disk
            // (for example a transport-side interruption).  Treat the exact
            // intended successor as an idempotent ambiguous-commit recovery;
            // any other body remains a real stale-writer conflict.
            let current: Option<Vec<u8>> = connection
                .query_row(
                    "SELECT body FROM owner_envelopes WHERE identity=?1 AND version=2",
                    [&identity],
                    |row| row.get(0),
                )
                .optional()?;
            if current.as_deref() != Some(next) {
                return Err(error(
                    "storage.conflict",
                    "scan failure owner envelope no longer matches durable state",
                ));
            }
        }
        Ok(())
    }

    /// Remove one fully acknowledged v2 owner and its exact raw cut only after
    /// the authenticated release confirmation has been observed. The compact,
    /// opaque receipt remains in the otherwise empty occurrence row so a
    /// caller that lost our successful reply can prove that this exact cleanup
    /// committed after reopening; no owner capability or source bytes remain.
    pub(crate) fn remove_scan_failure_owner(
        &self,
        identity: ubm_core::oversized_evidence::ScanFailureIdentity,
        cleanup_receipt: &[u8; 32],
        recovery_authority: Option<ScanFailureRecordingRecoveryAuthority>,
    ) -> Result<()> {
        let identity = scan_failure_identity_bytes(identity);
        let connection = self.open_private_scan_failure_sidecar()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCAN_FAILURE_SIDECAR_SCHEMA_VERSION {
            return Err(error(
                "storage.schema",
                "scan failure sidecar is incomplete v1 evidence",
            ));
        }
        let tx = connection.unchecked_transaction()?;
        if let Some(recovery_authority) = recovery_authority {
            let existing: Option<(Vec<u8>, Vec<u8>)> = tx
                .query_row(
                    "SELECT session_id,authority FROM recording_recovery_authority WHERE id=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            match existing {
                None => {
                    tx.execute(
                        "INSERT INTO recording_recovery_authority(id,session_id,authority) VALUES(1,?1,?2)",
                        params![recovery_authority.session_id.to_be_bytes().as_slice(), recovery_authority.authority.as_slice()],
                    )?;
                }
                Some((session_id, authority))
                    if session_id.as_slice() == recovery_authority.session_id.to_be_bytes()
                        && authority.as_slice() == recovery_authority.authority => {}
                Some(_) => {
                    return Err(error(
                        "storage.identity",
                        "scan failure recording recovery authority differs",
                    ));
                }
            }
        }
        let retained_receipt: Option<Option<Vec<u8>>> = tx
            .query_row(
                "SELECT envelope FROM occurrences WHERE identity=?1",
                [&identity],
                |row| row.get(0),
            )
            .optional()?;
        match retained_receipt {
            Some(Some(existing)) if existing.as_slice() == cleanup_receipt => {}
            Some(None) => {
                let deleted_owner = tx.execute(
                    "DELETE FROM owner_envelopes WHERE identity=?1 AND version=2",
                    [&identity],
                )?;
                if deleted_owner != 1 {
                    return Err(error(
                        "storage.identity",
                        "scan failure owner is not retained",
                    ));
                }
                tx.execute("DELETE FROM members WHERE identity=?1", [&identity])?;
                tx.execute("DELETE FROM tokens WHERE identity=?1", [&identity])?;
                tx.execute(
                    "UPDATE occurrences SET envelope=?2 WHERE identity=?1 AND envelope IS NULL",
                    params![identity, cleanup_receipt.as_slice()],
                )?;
            }
            _ => {
                return Err(error(
                    "storage.identity",
                    "scan failure cleanup receipt differs",
                ));
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Checks only whether an opaque authenticated cleanup receipt committed.
    /// It never returns the receipt or any owner/source material, and is used
    /// solely to turn a lost finalization response into an exact idempotent
    /// success after process restart.
    pub(crate) fn has_scan_failure_cleanup_receipt(
        &self,
        cleanup_receipt: &[u8; 32],
    ) -> Result<bool> {
        let connection = self.open_private_scan_failure_sidecar()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCAN_FAILURE_SIDECAR_SCHEMA_VERSION {
            return Err(error(
                "storage.schema",
                "scan failure sidecar is incomplete v1 evidence",
            ));
        }
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM occurrences WHERE envelope=?1)",
                [cleanup_receipt.as_slice()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Read only the recording-level authenticated scan recovery domain. This
    /// remains available after the last owner envelope was finalized, until
    /// every resulting cleanup receipt is retired.
    pub(crate) fn scan_failure_recording_recovery_authority(
        &self,
    ) -> Result<Option<ScanFailureRecordingRecoveryAuthority>> {
        if !scan_failure_sidecar_file_exists(
            &self.scan_failure_sidecar_path,
            "scan recovery sidecar is not a regular file",
        )? {
            return Ok(None);
        }
        let connection = self.open_private_scan_failure_sidecar()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 1 {
            return Ok(None);
        }
        let row: Option<(Vec<u8>, Vec<u8>)> = connection
            .query_row(
                "SELECT session_id,authority FROM recording_recovery_authority WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((session_id, authority)) = row else {
            return Ok(None);
        };
        let session_id: [u8; 8] = session_id.try_into().map_err(|_| {
            error(
                "storage.corrupt",
                "scan failure recording recovery session has invalid width",
            )
        })?;
        let authority: [u8; 32] = authority.try_into().map_err(|_| {
            error(
                "storage.corrupt",
                "scan failure recording recovery authority has invalid width",
            )
        })?;
        let session_id = u64::from_be_bytes(session_id);
        if session_id == 0 {
            return Err(error(
                "storage.corrupt",
                "scan failure recording recovery session is invalid",
            ));
        }
        Ok(Some(ScanFailureRecordingRecoveryAuthority {
            session_id,
            authority,
        }))
    }

    /// Whether this recording has no resumable v2 scan owner and no retained
    /// finalized cleanup receipt. Raw-only/v1 evidence is intentionally not
    /// treated as resumable authority.
    pub(crate) fn scan_failure_recovery_is_empty(&self) -> Result<bool> {
        if !scan_failure_sidecar_file_exists(
            &self.scan_failure_sidecar_path,
            "scan recovery sidecar is not a regular file",
        )? {
            return Ok(true);
        }
        let connection = self.open_private_scan_failure_sidecar()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 1 {
            return Ok(true);
        }
        connection
            .query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM owner_envelopes)
                AND NOT EXISTS(SELECT 1 FROM occurrences WHERE envelope IS NOT NULL)",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Issue an opaque, host-held retirement capability for one committed
    /// cleanup receipt. The capability is a MAC, never the durable authority
    /// key, so it remains valid after this receipt row is reclaimed and can
    /// make a lost retirement reply exactly idempotent after reopen.
    pub(crate) fn scan_failure_cleanup_retirement(
        &self,
        cleanup_receipt: &[u8; 32],
    ) -> Result<[u8; 32]> {
        let connection = self.open_private_scan_failure_sidecar()?;
        let retained: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM occurrences WHERE envelope=?1)",
            [cleanup_receipt.as_slice()],
            |row| row.get(0),
        )?;
        if !retained {
            return Err(error(
                "storage.identity",
                "scan failure cleanup receipt is not retained",
            ));
        }
        let authority: Vec<u8> =
            connection.query_row("SELECT key FROM cleanup_authority WHERE id=1", [], |row| {
                row.get(0)
            })?;
        scan_failure_cleanup_retirement_mac(&authority, cleanup_receipt)
    }

    /// Enumerate only active finalized cleanup receipts as authenticated
    /// opaque capabilities. Callers never receive the durable authority key.
    pub(crate) fn scan_failure_cleanup_retirements(&self) -> Result<Vec<([u8; 32], [u8; 32])>> {
        let connection = self.open_private_scan_failure_sidecar()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 1 {
            return Ok(Vec::new());
        }
        let authority: Vec<u8> =
            connection.query_row("SELECT key FROM cleanup_authority WHERE id=1", [], |row| {
                row.get(0)
            })?;
        let mut statement = connection.prepare(
            "SELECT envelope FROM occurrences WHERE envelope IS NOT NULL ORDER BY identity",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        let mut retirements = Vec::new();
        for row in rows {
            let receipt = row?;
            let receipt: [u8; 32] = receipt.try_into().map_err(|_| {
                error(
                    "storage.corrupt",
                    "scan failure cleanup receipt has invalid width",
                )
            })?;
            let authenticator = scan_failure_cleanup_retirement_mac(&authority, &receipt)?;
            retirements.push((receipt, authenticator));
        }
        Ok(retirements)
    }

    /// Retire a committed cleanup receipt. A valid client-held capability
    /// remains verifiable after deletion, which is what makes this operation
    /// safe for a lost response without retaining an unbounded tombstone.
    pub(crate) fn retire_scan_failure_cleanup_receipt(
        &self,
        cleanup_receipt: &[u8; 32],
        authenticator: &[u8; 32],
    ) -> Result<()> {
        use hmac::Mac;

        let mut connection = self.open_private_scan_failure_sidecar()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authority: Vec<u8> =
            tx.query_row("SELECT key FROM cleanup_authority WHERE id=1", [], |row| {
                row.get(0)
            })?;
        if authority.len() != 32 {
            return Err(error(
                "storage.corrupt",
                "scan failure cleanup retirement authority is invalid",
            ));
        }
        let mut mac = hmac::Hmac::<Sha256>::new_from_slice(&authority).map_err(|_| {
            error(
                "storage.corrupt",
                "scan failure cleanup retirement authority is invalid",
            )
        })?;
        mac.update(b"ubm-scan-cleanup-retirement/1");
        mac.update(cleanup_receipt);
        mac.verify_slice(authenticator).map_err(|_| {
            error(
                "storage.identity",
                "scan failure cleanup retirement differs",
            )
        })?;
        tx.execute(
            "DELETE FROM occurrences WHERE envelope=?1",
            [cleanup_receipt.as_slice()],
        )?;
        // The recording-level domain is needed to reopen a retained cleanup
        // receipt after the last owner envelope has gone. Once both kinds of
        // durable recovery state are gone it would only be an unnecessary
        // retained credential. The cleanup MAC authority remains separately
        // so an exact lost-reply retirement stays idempotent.
        tx.execute(
            "DELETE FROM recording_recovery_authority
             WHERE id=1
               AND NOT EXISTS(SELECT 1 FROM owner_envelopes)
               AND NOT EXISTS(SELECT 1 FROM occurrences WHERE envelope IS NOT NULL)",
            [],
        )?;
        // `DELETE 0` is the exact lost-reply replay: a valid capability could
        // only have been issued while this receipt was retained, and its MAC
        // remains independently verifiable after the first committed delete.
        tx.commit()?;
        Ok(())
    }

    /// Return the exact stored v2 owner envelope. `None` distinguishes an
    /// already-captured source still awaiting owner commit from a v1 sidecar;
    /// callers must inspect/export v1 raw evidence rather than resume it.
    pub(crate) fn scan_failure_owner_envelope(
        &self,
        identity: ubm_core::oversized_evidence::ScanFailureIdentity,
    ) -> Result<Option<Vec<u8>>> {
        let identity = scan_failure_identity_bytes(identity);
        let connection = self.open_private_scan_failure_sidecar()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 1 {
            return Ok(None);
        }
        if version != SCAN_FAILURE_SIDECAR_SCHEMA_VERSION {
            return Err(error(
                "storage.schema",
                "scan failure sidecar schema is not recognized",
            ));
        }
        Ok(connection
            .query_row(
                "SELECT body FROM owner_envelopes WHERE identity=?1",
                [&identity],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Enumerate only complete v2 owners. Raw v1 sidecars and source cuts that
    /// crashed before their owner envelope are intentionally omitted: exposing
    /// either as resumable would require manufacturing the missing authority
    /// state. Their raw pages remain available through the evidence API.
    pub(crate) fn scan_failure_owner_envelopes(
        &self,
    ) -> Result<Vec<DurableScanFailureOwnerEnvelope>> {
        if scan_failure_sidecar_file_exists(
            &self.legacy_scan_failure_sidecar_path,
            "legacy scan recovery sidecar is not a regular file",
        )? {
            if scan_failure_sidecar_file_exists(
                &self.scan_failure_sidecar_path,
                "scan recovery sidecar is not a regular file",
            )? {
                return Err(error(
                    "storage.identity",
                    "legacy and private scan recovery sidecars both exist; recovery paths collide",
                ));
            }
            // Validate the retained bytes before reporting the required empty
            // owner set. A v1 file is exportable, never rehydratable.
            let _ = open_legacy_scan_failure_sidecar_for_export(
                &self.legacy_scan_failure_sidecar_path,
            )?;
            return Ok(Vec::new());
        }
        let connection = self.open_private_scan_failure_sidecar()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 1 {
            return Ok(Vec::new());
        }
        if version != SCAN_FAILURE_SIDECAR_SCHEMA_VERSION {
            return Err(error(
                "storage.schema",
                "scan failure sidecar schema is not recognized",
            ));
        }
        let mut statement = connection.prepare(
            "SELECT identity,body FROM owner_envelopes WHERE version=2 ORDER BY identity",
        )?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .map(|row| {
                let (identity, body) = row?;
                Ok(DurableScanFailureOwnerEnvelope {
                    identity: scan_failure_identity_from_bytes(&identity)?,
                    body,
                })
            })
            .collect()
    }

    /// Load the exact raw v2 cut for an authenticated owner rehydration. This
    /// performs strict tag/width validation; corrupt bytes are never coerced
    /// into a similar token and therefore cannot change a later descriptor.
    pub(crate) fn load_scan_failure_raw_cut(
        &self,
        identity: ubm_core::oversized_evidence::ScanFailureIdentity,
    ) -> Result<DurableScanFailureRawCut> {
        use ubm_core::oversized_evidence::{EvidenceToken, ScanFailureMember};
        let bytes = scan_failure_identity_bytes(identity);
        let connection = self.open_scan_failure_evidence_for_export()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let present: bool = match version {
            1 => connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM occurrences WHERE identity=?1)",
                [&bytes],
                |row| row.get(0),
            )?,
            SCAN_FAILURE_SIDECAR_SCHEMA_VERSION => connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM occurrences WHERE identity=?1 AND envelope IS NULL)",
                [&bytes],
                |row| row.get(0),
            )?,
            _ => {
                return Err(error(
                    "storage.schema",
                    "scan failure sidecar schema is not recognized",
                ));
            }
        };
        if !present {
            return Err(error(
                "storage.identity",
                "scan failure occurrence is not retained",
            ));
        }
        let mut member_statement = connection.prepare(
            "SELECT position,recipient,delivery FROM members WHERE identity=?1 ORDER BY position",
        )?;
        let mut members = Vec::new();
        for row in member_statement.query_map([&bytes], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })? {
            let (position, recipient, delivery) = row?;
            if position != members.len() as i64 || recipient.len() != 16 || delivery.len() != 16 {
                return Err(error(
                    "storage.corrupt",
                    "scan failure membership is malformed",
                ));
            }
            let mut recipient_id = [0_u8; 16];
            let mut delivery_id = [0_u8; 16];
            recipient_id.copy_from_slice(&recipient);
            delivery_id.copy_from_slice(&delivery);
            members.push(ScanFailureMember {
                recipient: recipient_id,
                delivery: delivery_id,
            });
        }
        let mut token_statement = connection.prepare(
            "SELECT position,tag,CAST(payload AS BLOB) FROM tokens WHERE identity=?1 ORDER BY position",
        )?;
        let mut tokens = Vec::new();
        for row in token_statement.query_map([&bytes], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })? {
            let (position, tag, payload) = row?;
            if position != tokens.len() as i64 {
                return Err(error(
                    "storage.corrupt",
                    "scan failure token positions are malformed",
                ));
            }
            let fixed = |width: usize| -> Result<[u8; 8]> {
                if payload.len() != width || width > 8 {
                    return Err(error(
                        "storage.corrupt",
                        "scan failure token payload has invalid width",
                    ));
                }
                let mut value = [0_u8; 8];
                value[8 - width..].copy_from_slice(&payload);
                Ok(value)
            };
            let token = match tag {
                0 if payload.is_empty() => EvidenceToken::Null,
                1 if payload.len() == 1 && payload[0] <= 1 => EvidenceToken::Bool(payload[0] != 0),
                2 => EvidenceToken::I64(i64::from_be_bytes(fixed(8)?)),
                3 => EvidenceToken::U64(u64::from_be_bytes(fixed(8)?)),
                4 => EvidenceToken::F64Bits(u64::from_be_bytes(fixed(8)?)),
                5 => EvidenceToken::Text(String::from_utf8(payload).map_err(|_| {
                    error("storage.corrupt", "scan failure text token is not UTF-8")
                })?),
                6 => EvidenceToken::Bytes(payload),
                7 => EvidenceToken::Map(u64::from_be_bytes(fixed(8)?)),
                8 => EvidenceToken::Array(u64::from_be_bytes(fixed(8)?)),
                _ => {
                    return Err(error(
                        "storage.corrupt",
                        "scan failure token tag is invalid",
                    ));
                }
            };
            tokens.push(token);
        }
        Ok(DurableScanFailureRawCut {
            identity,
            members,
            tokens,
        })
    }

    /// Page the exact token representation retained for one source-time scan
    /// occurrence. Payload bytes are deliberately returned as JSON byte arrays
    /// rather than decoded text, preserving malformed and opaque evidence.
    pub(crate) fn scan_failure_occurrence_page(
        &self,
        identity: ubm_core::oversized_evidence::ScanFailureIdentity,
        offset: u64,
        max_items: u32,
    ) -> Result<Value> {
        if max_items == 0 {
            return Err(error(
                "argument.invalid",
                "scan failure page must request items",
            ));
        }
        let identity = scan_failure_identity_bytes(identity);
        let connection = self.open_scan_failure_evidence_for_export()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let present: bool = match version {
            // Released v1 has no `envelope` column. It is already validated
            // by the no-follow, read-only legacy opener above.
            1 => connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM occurrences WHERE identity=?1)",
                [&identity],
                |row| row.get(0),
            )?,
            SCAN_FAILURE_SIDECAR_SCHEMA_VERSION => connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM occurrences WHERE identity=?1 AND envelope IS NULL)",
                [&identity],
                |row| row.get(0),
            )?,
            _ => {
                return Err(error(
                    "storage.schema",
                    "scan failure sidecar schema is not recognized",
                ));
            }
        };
        if !present {
            return Err(error(
                "storage.identity",
                "scan failure occurrence is not retained",
            ));
        }
        let mut statement = connection.prepare(
            "SELECT position,tag,CAST(payload AS BLOB) FROM tokens WHERE identity=?1 AND position>=?2 ORDER BY position LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![identity, sql_integer(offset)?, i64::from(max_items)],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )?;
        let tokens = rows
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(position, tag, payload)| json!({"position": position, "tag": tag, "payload": payload}))
            .collect::<Vec<_>>();
        Ok(json!({"identity": identity.to_vec(), "offset": offset, "tokens": tokens}))
    }

    /// Persist one exact recipient acknowledgement. The occurrence remains
    /// until every source-time recipient has acknowledged; no new observer can
    /// acknowledge by merely matching its bytes or route.
    pub(crate) fn acknowledge_scan_failure_recipient(
        &self,
        identity: ubm_core::oversized_evidence::ScanFailureIdentity,
        recipient: [u8; 16],
        delivery: [u8; 16],
    ) -> Result<bool> {
        let identity = scan_failure_identity_bytes(identity);
        let connection = self.open_private_scan_failure_sidecar()?;
        let tx = connection.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE members SET acknowledged=1 WHERE identity=?1 AND recipient=?2 AND delivery=?3",
            params![identity, recipient.as_slice(), delivery.as_slice()],
        )?;
        if changed != 1 {
            return Err(error(
                "storage.identity",
                "scan failure recipient is not retained",
            ));
        }
        let remaining: i64 = tx.query_row(
            "SELECT count(*) FROM members WHERE identity=?1 AND acknowledged=0",
            [&identity],
            |row| row.get(0),
        )?;
        if remaining == 0 {
            tx.execute("DELETE FROM occurrences WHERE identity=?1", [&identity])?;
        }
        tx.commit()?;
        Ok(remaining == 0)
    }

    /// Opens the explicit Apple notification failure overflow spool associated
    /// with this already-authorized journal. No directory is inferred and the
    /// spool cannot outlive this journal authority.
    pub fn apple_notification_failure_spool(
        self: &Arc<Self>,
    ) -> Result<Arc<JournalAppleNotificationFailureSpool>> {
        self.apple_notification_failure_spool_with_quota(self.quota)
    }

    /// Opens the explicit Apple notification-failure spool with its own host
    /// supplied quota. This budget is separate from the primary recording;
    /// callers must surface both rather than describing it as shared capacity.
    pub fn apple_notification_failure_spool_with_quota(
        self: &Arc<Self>,
        quota: JournalQuota,
    ) -> Result<Arc<JournalAppleNotificationFailureSpool>> {
        if !(1..=1 << 30).contains(&quota.max_bytes)
            || !(1..=1_000_000).contains(&quota.max_records)
        {
            return Err(error(
                "argument.invalid",
                "apple notification failure spool quota is invalid",
            ));
        }
        let connection = open_apple_failure_spool(&self.apple_failure_spool_path, quota)?;
        Ok(Arc::new(JournalAppleNotificationFailureSpool {
            _journal: Arc::clone(self),
            connection: Mutex::new(connection),
            quota,
        }))
    }

    fn reject_unacknowledged_apple_failure_spool(&self) -> Result<()> {
        match std::fs::symlink_metadata(&self.apple_failure_spool_path) {
            Err(value) if value.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => {
                return Err(error(
                    "storage.io",
                    "apple notification failure spool cannot be inspected",
                ));
            }
            Ok(_) => {}
        }
        let inspection = Connection::open_with_flags(
            &self.apple_failure_spool_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let version: i64 = inspection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let quota = if version >= 3 {
            inspection.query_row(
                "SELECT max_bytes,max_records FROM settings WHERE id=1",
                [],
                |row| {
                    Ok(JournalQuota {
                        max_bytes: unsigned(row, 0)?,
                        max_records: unsigned(row, 1)?,
                    })
                },
            )?
        } else {
            self.quota
        };
        drop(inspection);
        let connection = open_apple_failure_spool(&self.apple_failure_spool_path, quota)?;
        let retained: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM occurrences UNION ALL SELECT 1 FROM reservations)",
            [],
            |row| row.get(0),
        )?;
        if retained {
            return Err(error(
                "storage.busy",
                "acknowledge apple notification failure spool before clearing recording",
            ));
        }
        Ok(())
    }

    pub(crate) fn matches_declaration(
        &self,
        declaration: &Value,
        quota: JournalQuota,
    ) -> Result<bool> {
        Ok(self.quota == quota && self.declaration == encoded(declaration, 65536)?)
    }

    /// Identity bound to this journal authority at open.
    pub fn recording_id(&self) -> &str {
        &self.recording_id
    }

    fn validate_path(&self, path: &Path) -> Result<()> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| error("storage.permission", "recording authority path changed"))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(error(
                "storage.permission",
                "recording authority path changed",
            ));
        }
        {
            if path_identity(path)? != self.file_identity {
                return Err(error(
                    "storage.identity",
                    "recording file was replaced; existing owner retained",
                ));
            }
            if let Some(authority) = &self.authority {
                let authority_path = path.with_extension("authority");
                let current = std::fs::symlink_metadata(&authority_path)
                    .map_err(|_| error("storage.permission", "recording authority path changed"))?;
                #[cfg(unix)]
                let held_identity = {
                    use std::os::unix::fs::MetadataExt;
                    let held = authority.metadata().map_err(|_| {
                        error("storage.io", "recording authority cannot be inspected")
                    })?;
                    (held.dev(), held.ino())
                };
                #[cfg(windows)]
                let held_identity = handle_identity(authority)?;
                if !current.is_file()
                    || current.file_type().is_symlink()
                    || held_identity != path_identity(&authority_path)?
                {
                    return Err(error(
                        "storage.identity",
                        "recording authority file was replaced; existing owner retained",
                    ));
                }
            }
        }
        Ok(())
    }
    fn can_close(&self) -> bool {
        // The registry's process lock protects every handle sharing this file,
        // including a peer's volatile failure after that peer is dropped.
        // One reference belongs to the process map and one to this handle.
        if Arc::strong_count(&self.shared) > 2 {
            return false;
        }
        let Ok(failures) = self.shared.failures.lock() else {
            return false;
        };
        failures.iter().all(|failure| failure.can_discard())
            && self
                .shared
                .diagnostic_ledger
                .lock()
                .is_ok_and(|ledger| ledger.slots.is_empty())
    }
    pub fn validate_metadata(metadata: &Value) -> Result<()> {
        if !metadata.is_object() {
            return Err(error(
                "argument.invalid",
                "journal metadata must be an object",
            ));
        }
        encoded(metadata, 16384).map(|_| ())
    }
    pub fn open(
        path: &Path,
        recording_id: &str,
        declaration: &Value,
        quota: JournalQuota,
    ) -> Result<Self> {
        Self::open_with_scan_recovery_storage(path, recording_id, declaration, quota, None)
    }

    /// Open a journal with native-host storage protection for the v2 scan
    /// recovery sidecar. This is intentionally a Rust host boundary; foreign
    /// callers never receive the provider or its protected-storage authority.
    pub fn open_with_scan_recovery_storage(
        path: &Path,
        recording_id: &str,
        declaration: &Value,
        quota: JournalQuota,
        scan_recovery_storage: Option<Arc<dyn ScanRecoveryStorageProtector>>,
    ) -> Result<Self> {
        Self::open_inner(
            path,
            recording_id,
            declaration,
            quota,
            scan_recovery_storage,
        )
        .map_err(|failure| failure.at("open"))
    }
    fn open_inner(
        path: &Path,
        recording_id: &str,
        declaration: &Value,
        quota: JournalQuota,
        scan_recovery_storage: Option<Arc<dyn ScanRecoveryStorageProtector>>,
    ) -> Result<Self> {
        if !valid_id(recording_id)
            || !(1 << 20..=1 << 30).contains(&quota.max_bytes)
            || !(1..=1_000_000).contains(&quota.max_records)
            || !declaration.is_object()
        {
            return Err(error(
                "argument.invalid",
                "invalid recording identity or quota",
            ));
        }
        let declaration = encoded(declaration, 65536)?;
        // Host directory aliases (notably /tmp -> /private/tmp on macOS) are
        // legitimate. Resolve that trusted directory, never the target file;
        // SQLite NOFOLLOW still closes a final-component replacement race.
        let name = path
            .file_name()
            .ok_or_else(|| error("argument.invalid", "journal path needs a filename"))?;
        let parent = path
            .parent()
            .filter(|value| !value.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let path = parent
            .canonicalize()
            .map_err(|_| error("storage.io", "journal directory cannot be resolved"))?
            .join(name);
        let page_limit = (quota.max_bytes - OVERHEAD) / (2 * PAGE + 8);
        let existing = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                if metadata.len() > page_limit * PAGE {
                    return Err(error(
                        "storage.full",
                        "existing database exceeds quota; file retained",
                    ));
                }
                true
            }
            Ok(_) => {
                return Err(error(
                    "storage.permission",
                    "journal path is not a regular file",
                ));
            }
            Err(value) if value.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return Err(error("storage.io", "journal path cannot be inspected")),
        };
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut connection = Connection::open_with_flags(
            &path,
            if existing {
                flags
            } else {
                flags | OpenFlags::SQLITE_OPEN_CREATE
            },
        )?;
        connection.busy_timeout(std::time::Duration::ZERO)?;
        let mut opened_schema = SCHEMA_VERSION;
        if existing {
            let pages =
                connection.pragma_query_value(None, "page_count", |row| unsigned(row, 0))?;
            if pages > page_limit {
                return Err(error(
                    "storage.full",
                    "existing database exceeds quota; file retained",
                ));
            }
            let mode: String =
                connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
            if mode != "delete" {
                return Err(error(
                    "storage.schema",
                    "unsupported journal mode; file retained",
                ));
            }
            let schema: i64 =
                connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
            opened_schema = schema;
            let application: i64 =
                connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
            let page_size =
                connection.pragma_query_value(None, "page_size", |row| unsigned(row, 0))?;
            if !(1..=SCHEMA_VERSION).contains(&schema)
                || application != APPLICATION_ID
                || page_size != PAGE
            {
                return Err(error(
                    "storage.schema",
                    "unknown journal schema or page format; file retained",
                ));
            }
            let check: String =
                connection.pragma_query_value(None, "quick_check", |row| row.get(0))?;
            if check != "ok" {
                return Err(error(
                    "storage.corrupt",
                    "journal integrity check failed; file retained",
                ));
            }
            let consistent: bool = connection.query_row("SELECT retained_records=(SELECT count(*) FROM records) AND retained_bytes=(SELECT coalesce(sum(bytes),0) FROM records) FROM journal WHERE id=1", [], |row|row.get(0))?;
            if !consistent {
                return Err(error(
                    "storage.corrupt",
                    "journal retained counters are inconsistent; file retained",
                ));
            }
            let identity: (String, String, u64, u64) = connection.query_row(
                "SELECT recording_id,declaration,max_bytes,max_records FROM journal WHERE id=1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        unsigned(row, 2)?,
                        unsigned(row, 3)?,
                    ))
                },
            )?;
            if identity
                != (
                    recording_id.to_owned(),
                    declaration.clone(),
                    quota.max_bytes,
                    quota.max_records,
                )
            {
                return Err(error(
                    "storage.identity",
                    "recording identity or quota differs; file retained",
                ));
            }
            // A released v1 journal remains on its released core schema. Its
            // pending cursor is readable by the v1 adapter below, rather than
            // being a reason to reject an otherwise resumable recording. A
            // newer journal synthetically labelled v1 is not a released v1
            // shape: `recording_acked` proves its primary table already passed
            // the v2 migration and its missing side tables must be repaired.
            let released_v1_core = schema == 1
                && !connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('records') WHERE name='recording_acked')",
                    [],
                    |row| row.get::<_, bool>(0),
                )?;
            if !released_v1_core {
                reject_legacy_cursors(&connection, schema)?;
            }
        }
        connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF; PRAGMA secure_delete=ON;")?;
        connection.pragma_update(None, "page_size", sql_integer(PAGE)?)?;
        let actual_limit =
            connection.query_row(&format!("PRAGMA max_page_count={page_limit}"), [], |row| {
                unsigned(row, 0)
            })?;
        if actual_limit != page_limit {
            return Err(error(
                "storage.full",
                "existing database exceeds quota; file retained",
            ));
        }
        if !existing {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch("CREATE TABLE journal(id INTEGER PRIMARY KEY CHECK(id=1), recording_id TEXT NOT NULL, declaration TEXT NOT NULL, max_bytes INTEGER NOT NULL, max_records INTEGER NOT NULL, phase TEXT NOT NULL, next_ordinal INTEGER NOT NULL, next_token INTEGER NOT NULL, lost INTEGER NOT NULL, pending_token TEXT, pending_last INTEGER, pending_more INTEGER, last_ack_token TEXT, last_ack_receipt TEXT, retained_records INTEGER NOT NULL, retained_bytes INTEGER NOT NULL, collection_failure TEXT); CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL, recording_acked INTEGER NOT NULL DEFAULT 0 CHECK(recording_acked IN (0,1)));")?;
            tx.execute("INSERT INTO journal VALUES(1,?1,?2,?3,?4,'recording',1,1,0,NULL,NULL,NULL,NULL,NULL,0,0,NULL)", params![recording_id,declaration,sql_integer(quota.max_bytes)?,sql_integer(quota.max_records)?])?;
            tx.execute_batch(ORDERED_IDENTITIES_SCHEMA)?;
            tx.execute_batch(EVIDENCE_SCHEMA)?;
            tx.execute_batch("ALTER TABLE evidence_cursor ADD COLUMN pending_count INTEGER;")?;
            tx.execute_batch(EVIDENCE_CLAIM_SCHEMA)?;
            tx.execute_batch(EVIDENCE_CLAIM_HEADER)?;
            tx.execute_batch(IDENTITY_SCHEMA)?;
            tx.execute_batch(ORDERED_OWNER_SCHEMA)?;
            tx.execute_batch(ORDERED_HOST_CLEANUP_SCHEMA)?;
            tx.execute_batch(ORDERED_PENDING_SCHEMA)?;
            tx.execute_batch(DIAGNOSTIC_LEDGER_SCHEMA)?;
            tx.execute_batch(DIAGNOSTIC_OWNER_SCHEMA)?;
            tx.execute_batch(SUMMARY_SNAPSHOT_SCHEMA)?;
            tx.execute_batch(SEPARATE_LOSS_SCHEMA)?;
            tx.execute_batch(DIAGNOSTIC_TRANSITION_SCHEMA)?;
            tx.execute_batch(PEER_FAILURE_RECOVERY_SCHEMA)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.commit()?;
        }
        let released_v1_core = existing
            && opened_schema == 1
            && !connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('records') WHERE name='recording_acked')",
                [],
                |row| row.get::<_, bool>(0),
            )?;
        if existing && !released_v1_core {
            // Keep the cursor check and all schema steps under one write lock.
            // A quota or interruption failure leaves the original version intact.
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
            reject_legacy_cursors(&tx, version)?;
            if version == 1 {
                tx.execute_batch(ORDERED_IDENTITIES_SCHEMA)?;
                tx.execute_batch(ORDERED_OWNER_SCHEMA)?;
                tx.execute_batch(ORDERED_HOST_CLEANUP_SCHEMA)?;
                tx.execute_batch(ORDERED_PENDING_SCHEMA)?;
            }
            if version <= 2 {
                let has_column: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('records') WHERE name='recording_acked')", [], |row| row.get(0))?;
                if !has_column {
                    tx.execute_batch("ALTER TABLE records ADD COLUMN recording_acked INTEGER NOT NULL DEFAULT 0 CHECK(recording_acked IN (0,1));")?;
                }
                tx.execute_batch(EVIDENCE_SCHEMA)?;
            }
            if version <= 3 {
                let has_count: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('evidence_cursor') WHERE name='pending_count')", [], |row| row.get(0))?;
                if !has_count {
                    tx.execute_batch(
                        "ALTER TABLE evidence_cursor ADD COLUMN pending_count INTEGER;",
                    )?;
                }
                tx.execute_batch(EVIDENCE_CLAIM_SCHEMA)?;
                tx.execute_batch(EVIDENCE_CLAIM_HEADER)?;
            }
            if version <= 4 {
                tx.execute_batch(IDENTITY_SCHEMA)?;
            }
            if version <= 5 {
                tx.execute_batch(DIAGNOSTIC_LEDGER_SCHEMA)?;
            }
            if version <= 6 {
                tx.execute_batch(DIAGNOSTIC_OWNER_SCHEMA)?;
            }
            if version <= 7 {
                tx.execute_batch(SUMMARY_SNAPSHOT_SCHEMA)?;
            }
            if version <= 8 {
                tx.execute_batch(SEPARATE_LOSS_SCHEMA)?;
            }
            if version <= 9 {
                ensure_diagnostic_transition_schema(&tx)?;
            }
            if version <= 10 {
                tx.execute_batch(PEER_FAILURE_RECOVERY_SCHEMA)?;
            }
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            // Repair only the exact, untouched synthetic row made by the
            // earlier v6 migration. The original summary stays byte-for-byte
            // in the legacy column, including after explicit public clear.
            if synthetic_summary_matches(&tx)? {
                tx.execute("DELETE FROM diagnostic_ledger WHERE occurrence=zeroblob(40) AND acknowledged=0 AND retired=0", [])?;
            }
            // Earlier unreleased v2 journals can lack lifecycle rows.
            tx.execute_batch(ORDERED_OWNER_SCHEMA)?;
            tx.execute_batch(ORDERED_HOST_CLEANUP_SCHEMA)?;
            tx.execute_batch(ORDERED_PENDING_SCHEMA)?;
            tx.commit()?;
            // The migrated primary schema is current. Keeping its pre-migration
            // version on this handle would route new ordered lifecycle calls
            // through the released-v1 adapter even though their side tables
            // are now durable.
            opened_schema = SCHEMA_VERSION;
        }
        // Do not create the private v2 directory while merely opening a
        // recording.  A released v1 sibling remains export-only and opening
        // it must be observational.  The v2 opener provisions this directory
        // immediately before SQLite is allowed to touch it.
        let scan_failure_sidecar_path = scan_failure_sidecar_candidate_path(&path)?;
        let file_identity = path_identity(&path)?;
        let (shared, failures) = shared_journal(file_identity, connection)?;
        Ok(Self {
            authority: None,
            file_identity,
            journal_path: path.clone(),
            peer_failure_sidecar_path: path.with_extension("peer-failure-recovery.sqlite"),
            peer_failure_ledger: Mutex::new(Weak::new()),
            scan_failure_sidecar_path,
            legacy_scan_failure_sidecar_path: path.with_extension("scan-failure-recovery.sqlite"),
            scan_recovery_storage,
            apple_failure_spool_path: path.with_extension("apple-notification-failure.sqlite"),
            shared,
            failures,
            quota,
            page_limit,
            schema_version: opened_schema,
            recording_id: recording_id.to_owned(),
            declaration,
        })
    }

    // Called while holding the shared connection, so no peer can change its
    // failure state between this check and the transaction's commit.
    fn has_collection_failure(&self) -> Result<bool> {
        let failures = self
            .shared
            .failures
            .lock()
            .map_err(|_| error("storage.io", "journal failure ownership interrupted"))?;
        let ledger = self
            .shared
            .diagnostic_ledger
            .lock()
            .map_err(|_| error("storage.io", "diagnostic ledger interrupted"))?;
        if ledger
            .slots
            .iter()
            .filter_map(|slot| slot.entry.as_ref())
            .any(|entry| entry.category == DiagnosticCategory::Collection)
        {
            return Ok(true);
        }
        for failure in failures.iter() {
            if failure
                .collection_failure
                .lock()
                .map_err(|_| error("storage.io", "collection failure state interrupted"))?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.shared
            .connection
            .lock()
            .map_err(|_| error("storage.io", "journal writer was interrupted"))
    }

    /// Commit the central-produced recipient-cut envelope and exact producer
    /// bytes before the boundary may begin its irreversible source ACK. A
    /// retry must name the exact same immutable occurrence and bytes.
    pub(crate) fn stage_peer_failure_recovery(
        &self,
        identity: &[u8],
        occurrence: &Value,
        evidence: &[u8],
    ) -> Result<()> {
        if identity.len() != 96 || evidence.len() as u64 > self.quota.max_bytes {
            return Err(error(
                "argument.invalid",
                "invalid peer failure recovery identity or evidence length",
            ));
        }
        let occurrence = encoded(occurrence, 1 << 20)?;
        let digest = Sha256::digest(evidence).to_vec();
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(error(
                "storage.schema",
                "peer failure recovery requires current journal schema",
            ));
        }
        let existing: Option<(String, Vec<u8>, Vec<u8>)> = tx
            .query_row(
                "SELECT occurrence,CAST(evidence AS BLOB),digest FROM peer_failure_recoveries WHERE identity=?1",
                [identity],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((stored_occurrence, stored_evidence, stored_digest)) = existing {
            if stored_occurrence != occurrence
                || stored_evidence != evidence
                || stored_digest != digest
            {
                return Err(error(
                    "storage.identity",
                    "peer failure recovery identity differs",
                ));
            }
        } else {
            tx.execute(
                "INSERT INTO peer_failure_recoveries(identity,occurrence,evidence,digest,state) VALUES(?1,?2,?3,?4,'staged')",
                params![identity, occurrence, evidence, digest],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read the exact durable handoff. A digest mismatch is corruption, never
    /// a reason to substitute a freshly observed source failure.
    pub(crate) fn peer_failure_recovery(
        &self,
        identity: &[u8],
    ) -> Result<Option<PeerFailureRecovery>> {
        if identity.len() != 96 {
            return Err(error(
                "argument.invalid",
                "invalid peer failure recovery identity",
            ));
        }
        let connection = self.connection()?;
        let row: Option<(String, Vec<u8>, Vec<u8>, String)> = connection
            .query_row(
                "SELECT occurrence,CAST(evidence AS BLOB),digest,state FROM peer_failure_recoveries WHERE identity=?1",
                [identity],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        row.map(|(occurrence, evidence, digest, state)| {
            if Sha256::digest(&evidence).as_slice() != digest.as_slice() {
                return Err(error(
                    "storage.corrupt",
                    "peer failure recovery evidence digest differs",
                ));
            }
            Ok(PeerFailureRecovery {
                occurrence: decode(&occurrence)?,
                evidence,
                state: PeerFailureRecoveryState::from_storage(&state)?,
            })
        })
        .transpose()
    }

    /// Persist intent before the native boundary is called. Replaying an
    /// interrupted request returns the same durable in-flight state so callers
    /// reconcile the same source-ACK receipt rather than issuing another one.
    pub(crate) fn begin_peer_failure_source_ack(
        &self,
        identity: &[u8],
    ) -> Result<PeerFailureRecoveryState> {
        self.transition_peer_failure_source_ack(identity, false)
    }

    /// Mark the one acknowledged producer boundary. Central retains the row
    /// until it has reconciled recipients and source teardown, then explicitly
    /// calls [`Self::release_peer_failure_recovery`].
    pub(crate) fn confirm_peer_failure_source_ack(
        &self,
        identity: &[u8],
    ) -> Result<PeerFailureRecoveryState> {
        self.transition_peer_failure_source_ack(identity, true)
    }

    fn transition_peer_failure_source_ack(
        &self,
        identity: &[u8],
        confirm: bool,
    ) -> Result<PeerFailureRecoveryState> {
        if identity.len() != 96 {
            return Err(error(
                "argument.invalid",
                "invalid peer failure recovery identity",
            ));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored: String = tx
            .query_row(
                "SELECT state FROM peer_failure_recoveries WHERE identity=?1",
                [identity],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| error("storage.identity", "peer failure recovery is not retained"))?;
        let state = PeerFailureRecoveryState::from_storage(&stored)?;
        let next = match (confirm, state) {
            (false, PeerFailureRecoveryState::Staged) => {
                PeerFailureRecoveryState::SourceAckInFlight
            }
            (false, state) => state,
            (true, PeerFailureRecoveryState::SourceAckInFlight) => {
                PeerFailureRecoveryState::SourceAckCommitted
            }
            (true, PeerFailureRecoveryState::SourceAckCommitted) => state,
            (true, PeerFailureRecoveryState::Staged) => {
                return Err(error(
                    "storage.identity",
                    "peer failure source ACK was not begun",
                ));
            }
        };
        if next != state {
            tx.execute(
                "UPDATE peer_failure_recoveries SET state=?2 WHERE identity=?1",
                params![identity, next.storage()],
            )?;
        }
        tx.commit()?;
        Ok(next)
    }

    /// Release the durable occurrence only after the source boundary and every
    /// recipient obligation have been reconciled by central.
    pub(crate) fn release_peer_failure_recovery(&self, identity: &[u8]) -> Result<()> {
        if identity.len() != 96 {
            return Err(error(
                "argument.invalid",
                "invalid peer failure recovery identity",
            ));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: String = tx
            .query_row(
                "SELECT state FROM peer_failure_recoveries WHERE identity=?1",
                [identity],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| error("storage.identity", "peer failure recovery is not retained"))?;
        if PeerFailureRecoveryState::from_storage(&state)?
            != PeerFailureRecoveryState::SourceAckCommitted
        {
            return Err(error(
                "storage.identity",
                "peer failure source ACK is not committed",
            ));
        }
        tx.execute(
            "DELETE FROM peer_failure_recoveries WHERE identity=?1",
            [identity],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The host retains a unique, never reused id and exact occurrence. The
    /// reservation commits that payload before any insertion can be attempted.
    /// It does not reserve filesystem or SQLite space.
    pub(crate) fn reserve_durable_diagnostic(
        &self,
        id: DiagnosticId,
        payload_limit: usize,
        payload: &str,
    ) -> Result<DiagnosticOwnerState> {
        if id == [0; 40] || payload_limit > DIAGNOSTIC_PAYLOAD_LIMIT {
            return Err(error(
                "storage.identity",
                "invalid diagnostic owner or payload limit",
            ));
        }
        let max = payload_limit
            .checked_mul(6)
            .and_then(|n| n.checked_add(4096))
            .ok_or_else(|| error("storage.full", "diagnostic reservation exceeds quota"))?;
        if payload.len() > max {
            return Err(error(
                "storage.full",
                "diagnostic exceeds durable owner limit",
            ));
        }
        let (_, failure, _) = validate_diagnostic_payload(payload)?;
        let size = diagnostic_field_bytes([
            failure["kind"].as_str().unwrap(),
            failure["detail"].as_str().unwrap(),
            failure["operation"].as_str().unwrap(),
            failure["sqliteCode"].as_str().unwrap_or(""),
        ]);
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(i64, String, Option<String>)> = tx
            .query_row(
                "SELECT payload_limit,state,payload FROM diagnostic_owners WHERE occurrence=?1",
                [id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let state = if let Some((limit, state, stored)) = existing {
            if limit != payload_limit as i64 || stored.as_deref() != Some(payload) {
                return Err(error(
                    "storage.identity",
                    "diagnostic owner already used or unbound",
                ));
            }
            diagnostic_owner_state(&state)?
        } else {
            if size.is_none_or(|size| size > payload_limit) {
                return Err(error("storage.full", "diagnostic exceeds reserved payload"));
            }
            let count: i64 = tx.query_row("SELECT count(*) FROM diagnostic_owners", [], |row| {
                row.get(0)
            })?;
            if count >= DIAGNOSTIC_OWNER_LIMIT {
                return Err(error("storage.full", "diagnostic owner receipts are full"));
            }
            let retained: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM diagnostic_ledger WHERE occurrence=?1)",
                [id.as_slice()],
                |row| row.get(0),
            )?;
            if retained {
                return Err(error(
                    "storage.identity",
                    "diagnostic occurrence already exists",
                ));
            }
            tx.execute(
                "INSERT INTO diagnostic_owners VALUES(?1,?2,'reserved',?3)",
                params![id.as_slice(), payload_limit as i64, payload],
            )?;
            DiagnosticOwnerState::Reserved
        };
        tx.commit()?;
        Ok(state)
    }

    /// Abandon an unused reservation only after the producer has excluded all
    /// future attempts. As with receipt release, absence is not a replay receipt.
    pub(crate) fn release_unused_diagnostic_owner(&self, id: DiagnosticId) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = tx.execute("DELETE FROM diagnostic_owners WHERE occurrence=?1 AND state='reserved' AND payload IS NULL AND NOT EXISTS(SELECT 1 FROM diagnostic_ledger WHERE occurrence=?1)", [id.as_slice()])?;
        if removed != 1 {
            return Err(error(
                "storage.identity",
                "unused diagnostic owner is not retained",
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// Release a bound, unused reservation only after all producers are fenced.
    pub(crate) fn release_bound_diagnostic_owner(
        &self,
        id: DiagnosticId,
        payload: &str,
    ) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = tx.execute("DELETE FROM diagnostic_owners WHERE occurrence=?1 AND state='reserved' AND payload=?2 AND NOT EXISTS(SELECT 1 FROM diagnostic_ledger WHERE occurrence=?1)", params![id.as_slice(), payload])?;
        if removed != 1 {
            return Err(error(
                "storage.identity",
                "matching unused diagnostic owner is not retained",
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// The producer supplies its retained exact occurrence after an uncertain
    /// insert. A durable reservation is required; unknown identities fail.
    pub(crate) fn replay_owned_diagnostic(
        &self,
        id: DiagnosticId,
        payload: &str,
    ) -> Result<DiagnosticReceipt> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owner: Option<(i64, String, Option<String>)> = tx
            .query_row(
                "SELECT payload_limit,state,payload FROM diagnostic_owners WHERE occurrence=?1",
                [id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (limit, state, stored) =
            owner.ok_or_else(|| error("storage.identity", "diagnostic owner is not retained"))?;
        if state == "reclaimed" {
            if stored.is_none() && payload.len() > DIAGNOSTIC_RECEIPT_LIMIT {
                let matches: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM diagnostic_ledger WHERE occurrence=?1 AND payload=?2 AND acknowledged=1 AND retired=1)", params![id.as_slice(), payload], |row| row.get(0))?;
                if matches {
                    return Err(error(
                        "storage.reclaimed",
                        "diagnostic occurrence was reclaimed",
                    ));
                }
            }
            return if stored.as_deref() == Some(payload) {
                Err(error(
                    "storage.reclaimed",
                    "diagnostic occurrence was reclaimed",
                ))
            } else {
                Err(error(
                    "storage.identity",
                    "diagnostic occurrence payload differs",
                ))
            };
        }
        let max = usize::try_from(limit)
            .ok()
            .and_then(|n| n.checked_mul(6))
            .and_then(|n| n.checked_add(4096))
            .ok_or_else(|| error("storage.corrupt", "invalid diagnostic owner limit"))?;
        if payload.len() > max {
            return Err(error(
                "storage.full",
                "diagnostic exceeds durable owner limit",
            ));
        }
        if stored.as_deref() != Some(payload) {
            return Err(error(
                "storage.identity",
                "diagnostic occurrence payload differs or is unbound",
            ));
        }
        if state != "reserved" && state != "committed" {
            return Err(error("storage.corrupt", "invalid diagnostic owner state"));
        }
        if state == "committed" {
            let retained: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM diagnostic_ledger WHERE occurrence=?1)",
                [id.as_slice()],
                |row| row.get(0),
            )?;
            if !retained {
                return Err(error(
                    "storage.corrupt",
                    "committed diagnostic occurrence missing",
                ));
            }
        }
        persist_diagnostic_tx(&tx, id, payload)?;
        tx.execute(
            "UPDATE diagnostic_owners SET state='committed',payload=?2 WHERE occurrence=?1",
            params![id.as_slice(), payload],
        )?;
        tx.commit()?;
        Ok(DiagnosticReceipt {
            id,
            durable: true,
            persistence_failure: None,
        })
    }

    /// Ends confirmation replay. The host must first durably record the
    /// confirmation and exclude all stale producers and consumers.
    pub fn release_diagnostic_confirmation(&self, id: DiagnosticId, payload: &str) -> Result<()> {
        self.compare_diagnostic_exact(id, payload, DiagnosticExactAction::Release)
    }

    // This bounds diagnostic accounting within this process only. It does not
    // reserve SQLite pages against other writes or direct cross-process openers,
    // and cannot promise persistence after admission. Production callers have
    // not migrated to this internal interface.
    pub(crate) fn reserve_diagnostic(
        &self,
        producer: [u8; 16],
        payload_limit: usize,
    ) -> Result<DiagnosticPermit> {
        if payload_limit > DIAGNOSTIC_PAYLOAD_LIMIT {
            return Err(error(
                "storage.full",
                "diagnostic exceeds reclaimable payload limit",
            ));
        }
        let connection = self.connection()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(error(
                "storage.schema",
                "diagnostic ledger requires current schema",
            ));
        }
        let mut ledger = self
            .shared
            .diagnostic_ledger
            .lock()
            .map_err(|_| error("storage.io", "diagnostic ledger interrupted"))?;
        // JSON can expand a single input byte to a six-byte escape. The fixed
        // page also accounts for identity, loss, receipt and row overhead.
        let charge = payload_limit
            .checked_mul(6)
            .and_then(|n| n.checked_add(4096))
            .ok_or_else(|| error("storage.full", "diagnostic reservation exceeds quota"))?;
        let budget = usize::try_from(self.quota.max_bytes.saturating_sub(OVERHEAD) / 2)
            .unwrap_or(usize::MAX);
        let mut retained_charge = 0usize;
        let mut statement = connection
            .prepare("SELECT occurrence,length(CAST(payload AS BLOB)) FROM diagnostic_ledger")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: Vec<u8> = row.get(0)?;
            if id.len() != 40 {
                return Err(error(
                    "storage.corrupt",
                    "invalid diagnostic occurrence identity",
                ));
            }
            let bytes: i64 = row.get(1)?;
            let bytes = usize::try_from(bytes)
                .map_err(|_| error("storage.corrupt", "invalid diagnostic payload size"))?;
            let persisted = bytes
                .checked_add(4096)
                .ok_or_else(|| error("storage.full", "diagnostic reservation exceeds quota"))?;
            let already_charged = ledger
                .slots
                .iter()
                .find(|slot| slot.id.as_slice() == id && slot.entry.is_some())
                .map_or(0, |slot| slot.charge);
            retained_charge = retained_charge
                .checked_add(persisted.saturating_sub(already_charged))
                .ok_or_else(|| error("storage.full", "diagnostic reservation exceeds quota"))?;
        }
        if ledger
            .charged
            .checked_add(retained_charge)
            .and_then(|n| n.checked_add(charge))
            .is_none_or(|total| total > budget)
        {
            return Err(error(
                "storage.full",
                "diagnostic reservation exceeds quota",
            ));
        }
        if ledger.incarnation.is_none() {
            let mut incarnation = [0; 16];
            getrandom::fill(&mut incarnation)
                .map_err(|_| error("storage.io", "diagnostic identity entropy unavailable"))?;
            ledger.incarnation = Some(incarnation);
        }
        let sequence = ledger
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| error("storage.full", "diagnostic sequence exhausted"))?;
        ledger
            .slots
            .try_reserve(1)
            .map_err(|_| error("storage.full", "diagnostic reservation allocation failed"))?;
        let mut id = [0; 40];
        id[..16].copy_from_slice(&ledger.incarnation.expect("identity initialized"));
        id[16..32].copy_from_slice(&producer);
        id[32..].copy_from_slice(&sequence.to_be_bytes());
        ledger.slots.push(DiagnosticSlot {
            id,
            charge,
            payload_limit,
            entry: None,
        });
        ledger.charged += charge;
        ledger.next_sequence = sequence;
        Ok(DiagnosticPermit {
            ledger: self.shared.diagnostic_ledger.clone(),
            id,
            consumed: false,
        })
    }

    // Failure to install returns the permit and the original payload. Once
    // installed, persistence failure stays on the entry, without recursion.
    pub(crate) fn record_diagnostic(
        &self,
        permit: DiagnosticPermit,
        category: DiagnosticCategory,
        failure: JournalError,
        loss: RecordedLoss,
    ) -> std::result::Result<DiagnosticReceipt, DiagnosticRecordError> {
        let mut connection = self.connection();
        self.record_diagnostic_locked(
            connection
                .as_mut()
                .map(|guard| &mut **guard)
                .map_err(|error| error.clone()),
            permit,
            category,
            failure,
            loss,
        )
    }

    // Holding the append connection lock establishes a shared failure cut.
    // Production failure paths have not yet adopted this internal API.
    fn record_diagnostic_locked(
        &self,
        connection: Result<&mut Connection>,
        mut permit: DiagnosticPermit,
        category: DiagnosticCategory,
        failure: JournalError,
        loss: RecordedLoss,
    ) -> std::result::Result<DiagnosticReceipt, DiagnosticRecordError> {
        let reject = |error, permit, category, failure, loss| DiagnosticRecordError {
            error,
            permit,
            category,
            failure,
            loss,
        };
        if !Arc::ptr_eq(&permit.ledger, &self.shared.diagnostic_ledger) {
            return Err(reject(
                error(
                    "storage.identity",
                    "diagnostic permit belongs to another journal",
                ),
                permit,
                category,
                failure,
                loss,
            ));
        }
        let mut ledger = match self.shared.diagnostic_ledger.lock() {
            Ok(ledger) => ledger,
            Err(_) => {
                return Err(reject(
                    error("storage.io", "diagnostic ledger interrupted"),
                    permit,
                    category,
                    failure,
                    loss,
                ));
            }
        };
        let Some(slot) = ledger
            .slots
            .iter_mut()
            .find(|slot| slot.id == permit.id && slot.entry.is_none())
        else {
            drop(ledger);
            return Err(reject(
                error(
                    "storage.identity",
                    "diagnostic permit is no longer reserved",
                ),
                permit,
                category,
                failure,
                loss,
            ));
        };
        let size = diagnostic_field_bytes([
            failure.kind,
            failure.detail,
            failure.operation,
            failure.sqlite_code.as_deref().unwrap_or(""),
        ]);
        if size.is_none_or(|size| size > slot.payload_limit) {
            drop(ledger);
            return Err(reject(
                error("storage.full", "diagnostic exceeds reserved payload"),
                permit,
                category,
                failure,
                loss,
            ));
        }
        let id = permit.id;
        let entry = DiagnosticEntry {
            id,
            category,
            failure: failure.clone(),
            loss,
            durable: false,
            persistence_failure: None,
        };
        if diagnostic_payload(&entry).len() > DIAGNOSTIC_RECEIPT_LIMIT {
            drop(ledger);
            return Err(reject(
                error(
                    "storage.full",
                    "diagnostic exceeds reclaimable receipt limit",
                ),
                permit,
                category,
                failure,
                loss,
            ));
        }
        slot.entry = Some(entry);
        permit.consumed = true;
        let entry = slot.entry.as_mut().expect("diagnostic just installed");
        let durable = match connection.and_then(|connection| persist_diagnostic(connection, entry))
        {
            Ok(()) => {
                entry.durable = true;
                true
            }
            Err(failure) => {
                entry.persistence_failure = Some(failure);
                false
            }
        };
        let persistence_failure = entry.persistence_failure.clone();
        drop(ledger);
        drop(permit);
        Ok(DiagnosticReceipt {
            id,
            durable,
            persistence_failure,
        })
    }

    pub(crate) fn retry_diagnostic(&self, id: DiagnosticId) -> Result<DiagnosticReceipt> {
        self.retry_diagnostic_inner(id, None)
    }

    /// A cold retry must provide the exact payload retained by its producer.
    pub(crate) fn retry_diagnostic_exact(
        &self,
        id: DiagnosticId,
        payload: &str,
    ) -> Result<DiagnosticReceipt> {
        self.retry_diagnostic_inner(id, Some(payload))
    }

    fn retry_diagnostic_inner(
        &self,
        id: DiagnosticId,
        payload: Option<&str>,
    ) -> Result<DiagnosticReceipt> {
        // Connection before ledger is the same cut used by append admission.
        let mut connection =
            match self.connection() {
                Ok(connection) => connection,
                Err(failure) => {
                    if let Ok(mut ledger) = self.shared.diagnostic_ledger.lock() {
                        if let Some(entry) = ledger.slots.iter_mut().find_map(|slot| {
                            (slot.id == id).then_some(slot.entry.as_mut()).flatten()
                        }) {
                            entry.durable = false;
                            entry.persistence_failure = Some(failure.clone());
                        }
                    }
                    return Err(failure);
                }
            };
        let mut ledger = self
            .shared
            .diagnostic_ledger
            .lock()
            .map_err(|_| error("storage.io", "diagnostic ledger interrupted"))?;
        let Some(entry) = ledger
            .slots
            .iter_mut()
            .find_map(|slot| (slot.id == id).then_some(slot.entry.as_mut()).flatten())
        else {
            let payload = payload.ok_or_else(|| {
                error(
                    "storage.identity",
                    "cold diagnostic retry requires its exact payload",
                )
            })?;
            let stored: Option<String> = connection
                .query_row(
                    "SELECT payload FROM diagnostic_ledger WHERE occurrence=?1",
                    [id.as_slice()],
                    |row| row.get(0),
                )
                .optional()?;
            return match stored {
                Some(stored) if stored == payload => {
                    let reclaimed: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM diagnostic_owners WHERE occurrence=?1 AND state='reclaimed' AND payload IS NULL)", [id.as_slice()], |row| row.get(0))?;
                    if reclaimed {
                        Err(error(
                            "storage.reclaimed",
                            "diagnostic occurrence was reclaimed",
                        ))
                    } else {
                        Ok(DiagnosticReceipt {
                            id,
                            durable: true,
                            persistence_failure: None,
                        })
                    }
                }
                Some(_) => Err(error(
                    "storage.identity",
                    "diagnostic occurrence payload differs",
                )),
                None => {
                    let receipt: Option<bool> = connection.query_row(
                        "SELECT payload=?2 FROM diagnostic_owners WHERE occurrence=?1 AND state='reclaimed' AND payload IS NOT NULL",
                        params![id.as_slice(), payload], |row| row.get(0)).optional()?;
                    if receipt == Some(true) {
                        Err(error(
                            "storage.reclaimed",
                            "diagnostic occurrence was reclaimed",
                        ))
                    } else {
                        Err(error(
                            "storage.identity",
                            "diagnostic occurrence is not retained or payload differs",
                        ))
                    }
                }
            };
        };
        if payload.is_some_and(|payload| payload != diagnostic_payload(entry)) {
            return Err(error(
                "storage.identity",
                "diagnostic occurrence payload differs",
            ));
        }
        let outcome = persist_diagnostic(&mut connection, entry);
        match outcome {
            Ok(()) => {
                entry.durable = true;
                entry.persistence_failure = None;
                Ok(DiagnosticReceipt {
                    id,
                    durable: true,
                    persistence_failure: None,
                })
            }
            Err(failure) => {
                entry.durable = false;
                entry.persistence_failure = Some(failure.clone());
                Err(failure)
            }
        }
    }

    // In-process slots only; use begin_diagnostic_occurrences and diagnostic_page
    // to inspect durable entries from earlier processes as well.
    pub(crate) fn diagnostic_entries(&self) -> Result<Vec<DiagnosticEntry>> {
        let ledger = self
            .shared
            .diagnostic_ledger
            .lock()
            .map_err(|_| error("storage.io", "diagnostic ledger interrupted"))?;
        Ok(ledger
            .slots
            .iter()
            .filter_map(|slot| slot.entry.clone())
            .collect())
    }

    /// The scan holds the journal cut and a SQLite snapshot until it is dropped.
    pub(crate) fn begin_diagnostic_occurrences(&self) -> Result<DiagnosticOccurrenceScan<'_>> {
        let connection = self.connection()?;
        let ledger = self
            .shared
            .diagnostic_ledger
            .lock()
            .map_err(|_| error("storage.io", "diagnostic ledger interrupted"))?;
        let mut pending = Vec::new();
        {
            let failures = self
                .shared
                .failures
                .lock()
                .map_err(|_| error("storage.io", "journal failure ownership interrupted"))?;
            for failure in failures.iter() {
                let entries = failure
                    .separate_occurrences
                    .lock()
                    .map_err(|_| error("storage.io", "loss occurrence ownership interrupted"))?;
                pending.extend(entries.iter().cloned());
            }
        }
        connection.execute_batch("BEGIN DEFERRED")?;
        if let Err(failure) = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM diagnostic_ledger)",
            [],
            |row| row.get::<_, bool>(0),
        ) {
            if let Err(rollback) = connection.execute_batch("ROLLBACK") {
                return Err(rollback.into());
            }
            return Err(failure.into());
        }
        Ok(DiagnosticOccurrenceScan {
            connection,
            ledger,
            pending,
            after: None,
        })
    }

    pub fn diagnostic_occurrences_page(
        &self,
        after: Option<DiagnosticId>,
        limit: usize,
    ) -> Result<Vec<DiagnosticId>> {
        let mut scan = self.begin_diagnostic_occurrences()?;
        scan.after = after;
        scan.next_page(limit)
    }

    pub fn diagnostic_occurrence_page(
        &self,
        id: DiagnosticId,
        offset: u64,
        limit: u32,
    ) -> Result<DiagnosticPage> {
        self.begin_diagnostic_occurrences()?
            .diagnostic_page(id, offset, limit)
    }

    /// Retry the exact process-owned loss attempt without inventing a new identity.
    /// An error, including an uncertain commit, leaves the attempt available.
    pub fn retry_production_loss(&self, id: DiagnosticId) -> Result<()> {
        let mut connection = self.connection()?;
        let failures = self
            .shared
            .failures
            .lock()
            .map_err(|_| error("storage.io", "journal failure ownership interrupted"))?;
        for failure in failures.iter() {
            let mut report = failure
                .collection_failure
                .lock()
                .map_err(|_| error("storage.io", "collection failure state interrupted"))?;
            let pending = failure
                .pending_recorded_loss
                .lock()
                .map_err(|_| error("storage.io", "recorded loss state interrupted"))?;
            let mut attempts = failure
                .separate_occurrences
                .lock()
                .map_err(|_| error("storage.io", "recorded loss occurrences interrupted"))?;
            let Some(index) = attempts.iter().position(|entry| entry.id == id) else {
                continue;
            };
            let entry = &attempts[index];
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            persist_diagnostic_tx(&tx, id, &diagnostic_payload(entry))?;
            tx.commit()?;
            attempts[index].durable = true;
            attempts[index].persistence_failure = None;
            // Keep the committed members until the entire local group settles:
            // the first reported failure need not be the last one retried.
            if attempts.iter().all(|entry| entry.durable) && *pending == RecordedLoss::default() {
                if let Some(value) = report.as_mut().filter(|value| {
                    attempts
                        .iter()
                        .any(|entry| same_failure(value, &diagnostic(&entry.failure)))
                }) {
                    value["persisted"] = json!(true);
                    if let Some(object) = value.as_object_mut() {
                        object.remove("persistenceFailure");
                    }
                }
                attempts.clear();
            }
            return Ok(());
        }
        let reclaimed: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM diagnostic_owners WHERE occurrence=?1 AND state='reclaimed')", [id.as_slice()], |row| row.get(0))?;
        if reclaimed {
            return Err(error(
                "storage.reclaimed",
                "diagnostic occurrence was reclaimed",
            ));
        }
        let retained: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM diagnostic_ledger WHERE occurrence=?1)",
            [id.as_slice()],
            |row| row.get(0),
        )?;
        if retained {
            Ok(())
        } else {
            Err(error(
                "storage.identity",
                "diagnostic occurrence is not retained",
            ))
        }
    }

    #[cfg(test)]
    fn diagnostic_occurrences(&self) -> Result<Vec<DiagnosticId>> {
        let mut scan = self.begin_diagnostic_occurrences()?;
        let mut all = Vec::new();
        loop {
            let page = scan.next_page(DIAGNOSTIC_OCCURRENCE_PAGE_LIMIT)?;
            if page.is_empty() {
                return Ok(all);
            }
            all.extend(page);
        }
    }

    /// Bounded owner snapshot, independent of the potentially larger ledger.
    /// Payload bytes are inspected separately and release must be coordinated.
    pub(crate) fn diagnostic_owners(&self) -> Result<Vec<DiagnosticOwner>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT occurrence,payload_limit,state,payload IS NOT NULL FROM diagnostic_owners ORDER BY occurrence LIMIT ?1")?;
        let mut rows = statement.query([DIAGNOSTIC_OWNER_LIMIT + 1])?;
        let mut owners = Vec::new();
        while let Some(row) = rows.next()? {
            if owners.len() >= DIAGNOSTIC_OWNER_LIMIT as usize {
                return Err(error(
                    "storage.corrupt",
                    "diagnostic owner count exceeds limit",
                ));
            }
            let raw = row
                .get_ref(0)?
                .as_blob()
                .map_err(|_| error("storage.corrupt", "invalid diagnostic owner identity"))?;
            let id = raw
                .try_into()
                .map_err(|_| error("storage.corrupt", "invalid diagnostic owner identity"))?;
            let limit: i64 = row.get(1)?;
            let payload_limit = usize::try_from(limit)
                .map_err(|_| error("storage.corrupt", "invalid diagnostic owner limit"))?;
            let state: String = row.get(2)?;
            owners.push(DiagnosticOwner {
                id,
                payload_limit,
                state: diagnostic_owner_state(&state)?,
                bound: row.get(3)?,
            });
        }
        Ok(owners)
    }

    /// Compare the caller's retained bytes sequentially while the transaction
    /// pins both the retained row and its lifecycle state.
    pub(crate) fn begin_diagnostic_exact(
        &self,
        id: DiagnosticId,
        action: DiagnosticExactAction,
    ) -> Result<DiagnosticExact<'_>> {
        let connection = self.connection()?;
        let ledger = self
            .shared
            .diagnostic_ledger
            .lock()
            .map_err(|_| error("storage.io", "diagnostic ledger interrupted"))?;
        // A producer may still own an ambiguous commit even when the durable
        // row exists. Reconcile that attempt before any lifecycle transition;
        // otherwise confirmation can reclaim the row beneath its retry.
        {
            let failures = self
                .shared
                .failures
                .lock()
                .map_err(|_| error("storage.io", "journal failure ownership interrupted"))?;
            for failure in failures.iter() {
                let attempts = failure
                    .separate_occurrences
                    .lock()
                    .map_err(|_| error("storage.io", "loss occurrence ownership interrupted"))?;
                if attempts.iter().any(|entry| entry.id == id) {
                    return Err(error(
                        "storage.busy",
                        "loss occurrence must be reconciled before lifecycle transition",
                    ));
                }
            }
        }
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let inspected = (|| -> Result<_> {
            let row: Option<(i64, u64, bool, bool)> = connection.query_row(
                "SELECT sequence,octet_length(payload),acknowledged,retired FROM diagnostic_ledger WHERE occurrence=?1",
                [id.as_slice()], |row| Ok((row.get(0)?, unsigned(row, 1)?, row.get(2)?, row.get(3)?))).optional()?;
            let owner: Option<(i64, String, Option<i64>)> = connection.query_row(
                "SELECT rowid,state,octet_length(payload) FROM diagnostic_owners WHERE occurrence=?1",
                [id.as_slice()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
            let owner = owner
                .map(|(rowid, state, size)| -> Result<_> {
                    diagnostic_owner_state(&state)?;
                    Ok((
                        rowid,
                        state,
                        size.map(|value| {
                            u64::try_from(value).map_err(|_| {
                                error("storage.corrupt", "invalid diagnostic payload length")
                            })
                        })
                        .transpose()?,
                    ))
                })
                .transpose()?;
            let total = row
                .as_ref()
                .map(|row| row.1)
                .or_else(|| owner.as_ref().and_then(|owner| owner.2))
                .ok_or_else(|| {
                    if ledger
                        .slots
                        .iter()
                        .any(|slot| slot.id == id && slot.entry.is_some())
                    {
                        error(
                            "storage.busy",
                            "volatile diagnostic cannot be acknowledged or retired",
                        )
                    } else {
                        error("storage.identity", "diagnostic occurrence is not retained")
                    }
                })?;
            if owner
                .as_ref()
                .and_then(|owner| owner.2)
                .is_some_and(|size| size != total)
            {
                return Err(error("storage.identity", "diagnostic owner differs"));
            }
            let local = ledger
                .slots
                .iter()
                .find_map(|slot| (slot.id == id).then_some(slot.entry.as_ref()).flatten())
                .map(diagnostic_payload);
            if local
                .as_ref()
                .is_some_and(|value| value.len() as u64 != total)
            {
                return Err(error(
                    "storage.identity",
                    "diagnostic occurrence payload differs",
                ));
            }
            Ok((
                row.map(|(rowid, _, ack, retired)| (rowid, ack, retired)),
                owner,
                total,
                local,
            ))
        })();
        match inspected {
            Ok((row, owner, total, local)) => Ok(DiagnosticExact {
                connection,
                ledger,
                id,
                action,
                row,
                owner,
                total,
                offset: 0,
                local,
                failed: false,
                open: true,
            }),
            Err(failure) => {
                if let Err(rollback) = connection.execute_batch("ROLLBACK") {
                    return Err(rollback.into());
                }
                Err(failure)
            }
        }
    }

    /// Inspect retained occurrence or confirmation bytes without copying the
    /// whole payload. Callers coordinate release and retain every byte before
    /// acknowledgement; offsets are bytes, including within UTF-8 sequences.
    pub(crate) fn diagnostic_page(
        &self,
        id: DiagnosticId,
        offset: u64,
        max_bytes: u32,
    ) -> Result<DiagnosticPage> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        diagnostic_db_page(&tx, id, offset, max_bytes)
    }

    /// Export the legacy collection summary, which may predate occurrence IDs.
    /// Coordinate concurrent summary updates while paging.
    pub fn diagnostic_summary_page(&self, offset: u64, max_bytes: u32) -> Result<DiagnosticPage> {
        if !(1..=DIAGNOSTIC_PAGE_LIMIT).contains(&max_bytes) {
            return Err(error("argument.invalid", "invalid diagnostic page bounds"));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let snapshot: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM diagnostic_summary_original WHERE id=1)",
            [],
            |row| row.get(0),
        )?;
        let (table, column) = if snapshot {
            ("diagnostic_summary_original", "payload")
        } else {
            ("journal", "collection_failure")
        };
        let size: Option<i64> = tx.query_row(
            &format!("SELECT octet_length({column}) FROM {table} WHERE id=1"),
            [],
            |row| row.get(0),
        )?;
        let total_bytes = u64::try_from(
            size.ok_or_else(|| error("storage.identity", "diagnostic summary is not retained"))?,
        )
        .map_err(|_| error("storage.corrupt", "invalid diagnostic summary length"))?;
        if offset > total_bytes {
            return Err(error(
                "argument.invalid",
                "diagnostic offset exceeds payload",
            ));
        }
        let start = usize::try_from(offset).map_err(|_| {
            error(
                "argument.invalid",
                "diagnostic offset exceeds platform range",
            )
        })?;
        let blob = tx.blob_open("main", table, column, 1, true)?;
        if blob.len() as u64 != total_bytes {
            return Err(error(
                "storage.corrupt",
                "diagnostic summary length changed",
            ));
        }
        let mut bytes = vec![0; (total_bytes - offset).min(u64::from(max_bytes)) as usize];
        blob.read_at_exact(&mut bytes, start)?;
        blob.close()?;
        Ok(DiagnosticPage {
            bytes,
            total_bytes,
            reclaimed: false,
            durable: true,
        })
    }

    /// Read one immutable occurrence. The caller retains these exact bytes
    /// before acknowledging them. Volatile entries cannot be acknowledged.
    pub(crate) fn prepare_diagnostic(&self, id: DiagnosticId) -> Result<PreparedDiagnostic> {
        let connection = self.connection()?;
        let mut ledger = self
            .shared
            .diagnostic_ledger
            .lock()
            .map_err(|_| error("storage.io", "diagnostic ledger interrupted"))?;
        let stored: Option<String> = connection
            .query_row(
                "SELECT payload FROM diagnostic_ledger WHERE occurrence=?1",
                [id.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        let local = ledger
            .slots
            .iter_mut()
            .find_map(|slot| (slot.id == id).then_some(slot.entry.as_mut()).flatten());
        match (stored, local) {
            (Some(payload), Some(entry)) => {
                if payload != diagnostic_payload(entry) {
                    return Err(error(
                        "storage.identity",
                        "diagnostic occurrence payload differs",
                    ));
                }
                entry.durable = true;
                entry.persistence_failure = None;
                Ok(PreparedDiagnostic {
                    payload,
                    durable: true,
                })
            }
            (Some(payload), None) => Ok(PreparedDiagnostic {
                payload,
                durable: true,
            }),
            (None, Some(entry)) => Ok(PreparedDiagnostic {
                payload: diagnostic_payload(entry),
                durable: false,
            }),
            (None, None) => Err(error(
                "storage.identity",
                "diagnostic occurrence is not retained",
            )),
        }
    }

    pub fn acknowledge_diagnostic(&self, id: DiagnosticId, payload: &str) -> Result<DiagnosticId> {
        self.transition_diagnostic(id, payload, DiagnosticTransition::Acknowledge)?;
        Ok(id)
    }

    /// The producer must be definitively unable to retry before retirement.
    pub fn retire_diagnostic(&self, id: DiagnosticId, payload: &str) -> Result<DiagnosticId> {
        self.transition_diagnostic(id, payload, DiagnosticTransition::Retire)?;
        Ok(id)
    }

    /// The caller has retained the retirement receipt and permits reclamation.
    pub fn confirm_diagnostic_retirement(&self, id: DiagnosticId, payload: &str) -> Result<()> {
        self.transition_diagnostic(id, payload, DiagnosticTransition::ConfirmRetirement)
    }

    fn transition_diagnostic(
        &self,
        id: DiagnosticId,
        payload: &str,
        transition: DiagnosticTransition,
    ) -> Result<()> {
        let action = match transition {
            DiagnosticTransition::Acknowledge => DiagnosticExactAction::Acknowledge,
            DiagnosticTransition::Retire => DiagnosticExactAction::Retire,
            DiagnosticTransition::ConfirmRetirement => DiagnosticExactAction::Confirm,
        };
        self.compare_diagnostic_exact(id, payload, action)
    }

    fn compare_diagnostic_exact(
        &self,
        id: DiagnosticId,
        payload: &str,
        action: DiagnosticExactAction,
    ) -> Result<()> {
        let mut comparison = self.begin_diagnostic_exact(id, action)?;
        for chunk in payload.as_bytes().chunks(DIAGNOSTIC_PAGE_LIMIT as usize) {
            comparison.compare(chunk)?;
        }
        comparison.finish()
    }

    /// Compare one bounded slice of a diagnostic lifecycle transition. The
    /// durable cursor is intentionally keyed by occurrence and action: a host
    /// restart may continue its exact comparison, while a stale or foreign
    /// transition cannot reuse a partial comparison for a different action.
    pub fn diagnostic_transition_chunk(
        &self,
        id: DiagnosticId,
        action: DiagnosticExactAction,
        offset: u64,
        bytes: &[u8],
    ) -> Result<DiagnosticTransitionProgress> {
        if bytes.is_empty() || bytes.len() > DIAGNOSTIC_PAGE_LIMIT as usize {
            return Err(error(
                "argument.invalid",
                "invalid diagnostic comparison chunk",
            ));
        }
        let connection = self.connection()?;
        let ledger = self
            .shared
            .diagnostic_ledger
            .lock()
            .map_err(|_| error("storage.io", "diagnostic ledger interrupted"))?;
        if ledger
            .slots
            .iter()
            .any(|slot| slot.id == id && slot.entry.is_some())
        {
            return Err(error(
                "storage.busy",
                "volatile diagnostic cannot be transitioned",
            ));
        }
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<DiagnosticTransitionProgress> {
            let row: Option<(i64, u64, bool, bool)> = connection.query_row(
                "SELECT sequence,octet_length(payload),acknowledged,retired FROM diagnostic_ledger WHERE occurrence=?1",
                [id.as_slice()],
                |row| Ok((row.get(0)?, unsigned(row, 1)?, row.get(2)?, row.get(3)?)),
            ).optional()?;
            let owner_raw: Option<(i64, String, Option<i64>)> = connection.query_row(
                "SELECT rowid,state,octet_length(payload) FROM diagnostic_owners WHERE occurrence=?1",
                [id.as_slice()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional()?;
            let owner = owner_raw
                .map(
                    |(rowid, state, size)| -> Result<(i64, String, Option<u64>)> {
                        Ok((
                            rowid,
                            state,
                            size.map(|value| {
                                u64::try_from(value).map_err(|_| {
                                    error("storage.corrupt", "invalid diagnostic payload length")
                                })
                            })
                            .transpose()?,
                        ))
                    },
                )
                .transpose()?;
            if let Some((_, state, _)) = &owner {
                diagnostic_owner_state(state)?;
            }
            let total = row
                .as_ref()
                .map(|row| row.1)
                .or_else(|| owner.as_ref().and_then(|owner| owner.2))
                .ok_or_else(|| {
                    error("storage.identity", "diagnostic occurrence is not retained")
                })?;
            if owner
                .as_ref()
                .and_then(|owner| owner.2)
                .is_some_and(|size| size != total)
            {
                return Err(error("storage.identity", "diagnostic owner differs"));
            }
            let stored: Option<(String, i64, i64)> = connection.query_row(
                "SELECT action,total,progressed FROM diagnostic_transitions WHERE occurrence=?1", [id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional()?;
            let progressed = match stored {
                Some((stored_action, stored_total, progressed)) => {
                    if stored_action != diagnostic_action_name(action)
                        || stored_total < 0
                        || progressed < 0
                        || stored_total as u64 != total
                        || progressed > stored_total
                    {
                        return Err(error(
                            "storage.identity",
                            "diagnostic transition differs from retained cursor",
                        ));
                    }
                    progressed as u64
                }
                None => {
                    connection.execute(
                        "INSERT INTO diagnostic_transitions VALUES(?1,?2,?3,0)",
                        params![
                            id.as_slice(),
                            diagnostic_action_name(action),
                            sql_integer(total)?
                        ],
                    )?;
                    0
                }
            };
            if offset != progressed
                || offset
                    .checked_add(bytes.len() as u64)
                    .is_none_or(|end| end > total)
            {
                return Err(error(
                    "storage.identity",
                    "diagnostic comparison offset differs",
                ));
            }
            let byte_offset = usize::try_from(offset).map_err(|_| {
                error(
                    "storage.corrupt",
                    "diagnostic offset exceeds platform range",
                )
            })?;
            let mut expected = vec![0; bytes.len()];
            for (table, rowid) in row
                .map(|row| ("diagnostic_ledger", row.0))
                .into_iter()
                .chain(
                    owner
                        .as_ref()
                        .and_then(|owner| owner.2.map(|_| ("diagnostic_owners", owner.0))),
                )
            {
                let blob = connection.blob_open("main", table, "payload", rowid, true)?;
                blob.read_at_exact(&mut expected, byte_offset)?;
                blob.close()?;
                if expected != bytes {
                    return Err(error(
                        "storage.identity",
                        "diagnostic occurrence payload differs",
                    ));
                }
            }
            let next = offset + bytes.len() as u64;
            if next != total {
                connection.execute(
                    "UPDATE diagnostic_transitions SET progressed=?2 WHERE occurrence=?1",
                    params![id.as_slice(), sql_integer(next)?],
                )?;
                return Ok(DiagnosticTransitionProgress {
                    offset: next,
                    total,
                    complete: false,
                });
            }
            match action {
                DiagnosticExactAction::CheckReplay => {
                    if owner
                        .as_ref()
                        .is_some_and(|(_, state, _)| state == "reclaimed")
                    {
                        return Err(error(
                            "storage.reclaimed",
                            "diagnostic occurrence was reclaimed",
                        ));
                    }
                    if row.is_none() {
                        return Err(error(
                            "storage.identity",
                            "diagnostic occurrence is not retained",
                        ));
                    }
                }
                DiagnosticExactAction::Acknowledge | DiagnosticExactAction::Retire => {
                    if row.is_none() {
                        return Err(error(
                            "storage.identity",
                            "diagnostic occurrence is not retained",
                        ));
                    }
                    let sql = if matches!(action, DiagnosticExactAction::Acknowledge) {
                        "UPDATE diagnostic_ledger SET acknowledged=1 WHERE occurrence=?1"
                    } else {
                        "UPDATE diagnostic_ledger SET retired=1 WHERE occurrence=?1"
                    };
                    connection.execute(sql, [id.as_slice()])?;
                }
                DiagnosticExactAction::Confirm => {
                    if let Some((_, _, acknowledged, retired)) = row {
                        if !acknowledged || !retired {
                            return Err(error(
                                "storage.busy",
                                "diagnostic retirement is not complete",
                            ));
                        }
                        match &owner {
                            Some((_, state, None))
                                if state == "reclaimed"
                                    && total > DIAGNOSTIC_RECEIPT_LIMIT as u64 => {}
                            Some((_, state, Some(_))) if state == "committed" => {
                                connection.execute("UPDATE diagnostic_owners SET state='reclaimed' WHERE occurrence=?1", [id.as_slice()])?;
                                connection.execute(
                                    "DELETE FROM diagnostic_ledger WHERE occurrence=?1",
                                    [id.as_slice()],
                                )?;
                            }
                            Some(_) => {
                                return Err(error("storage.identity", "diagnostic owner differs"));
                            }
                            None => {
                                let count: i64 = connection.query_row(
                                    "SELECT count(*) FROM diagnostic_owners",
                                    [],
                                    |row| row.get(0),
                                )?;
                                if count >= DIAGNOSTIC_OWNER_LIMIT {
                                    return Err(error(
                                        "storage.full",
                                        "diagnostic confirmation receipts are full",
                                    ));
                                }
                                if total > DIAGNOSTIC_RECEIPT_LIMIT as u64 {
                                    connection.execute("INSERT INTO diagnostic_owners VALUES(?1,0,'reclaimed',NULL)", [id.as_slice()])?;
                                } else {
                                    connection.execute("INSERT INTO diagnostic_owners SELECT occurrence,0,'reclaimed',payload FROM diagnostic_ledger WHERE occurrence=?1", [id.as_slice()])?;
                                    connection.execute(
                                        "DELETE FROM diagnostic_ledger WHERE occurrence=?1",
                                        [id.as_slice()],
                                    )?;
                                }
                            }
                        }
                    } else if !owner
                        .as_ref()
                        .is_some_and(|(_, state, size)| state == "reclaimed" && size.is_some())
                    {
                        return Err(error(
                            "storage.identity",
                            "diagnostic confirmation is not retained",
                        ));
                    }
                }
                DiagnosticExactAction::Release => {
                    let (_, state, size) = owner.as_ref().ok_or_else(|| {
                        error(
                            "storage.identity",
                            "diagnostic confirmation is not retained",
                        )
                    })?;
                    if state != "reclaimed" {
                        return Err(error(
                            "storage.identity",
                            "diagnostic confirmation is not retained",
                        ));
                    }
                    if size.is_none() {
                        if total <= DIAGNOSTIC_RECEIPT_LIMIT as u64
                            || !row.is_some_and(|(_, _, ack, retired)| ack && retired)
                        {
                            return Err(error(
                                "storage.identity",
                                "oversized diagnostic confirmation is not retained",
                            ));
                        }
                        connection.execute(
                            "DELETE FROM diagnostic_ledger WHERE occurrence=?1",
                            [id.as_slice()],
                        )?;
                    } else if row.is_some() {
                        return Err(error(
                            "storage.identity",
                            "diagnostic confirmation has an unexpected row",
                        ));
                    }
                    connection.execute(
                        "DELETE FROM diagnostic_owners WHERE occurrence=?1",
                        [id.as_slice()],
                    )?;
                }
            }
            connection.execute(
                "DELETE FROM diagnostic_transitions WHERE occurrence=?1",
                [id.as_slice()],
            )?;
            Ok(DiagnosticTransitionProgress {
                offset: next,
                total,
                complete: true,
            })
        })();
        match result {
            Ok(progress) => {
                connection.execute_batch("COMMIT")?;
                Ok(progress)
            }
            Err(failure) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(failure)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn test_lock_connection(&self) -> MutexGuard<'_, Connection> {
        self.shared
            .connection
            .lock()
            .expect("test journal connection")
    }

    #[cfg(test)]
    pub(crate) fn test_failed_append_hook(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.failures.failed_append_hook.lock().unwrap() = Some(Box::new(hook));
    }

    #[cfg(test)]
    pub(crate) fn test_shared_connection_busy(&self) -> bool {
        matches!(
            self.shared.connection.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }

    // Called only on a cold registry open after exclusive sidecar acquisition,
    // while the process-global authorities mutex excludes live registration.
    fn recover_cold_ordered_owner(&self) -> Result<()> {
        let retained = {
            let connection = self.connection()?;
            connection.query_row(
                "SELECT producer_id,state FROM ordered_owner WHERE id=1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
        };
        match retained {
            Ok((producer, state)) if state == "live" => {
                self.recover_interrupted_ordered(&producer)?;
                self.retire_interrupted_ordered_identities(&producer)?;
            }
            Ok((producer, state)) if state == "interrupted" => {
                self.retire_interrupted_ordered_identities(&producer)?
            }
            Ok(_) | Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(failure) => return Err(failure.into()),
        }
        Ok(())
    }

    /// Reserve the journal for a new producer before source admission. A live or
    /// finished lifecycle cannot be overwritten by another producer.
    pub(crate) fn begin_ordered(&self, producer: &str, collection: &str) -> Result<()> {
        if [producer, collection]
            .iter()
            .any(|part| part.is_empty() || part.len() > 256)
        {
            return Err(error("argument.invalid", "invalid ordered owner identity"));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let phase: String =
            tx.query_row("SELECT phase FROM journal WHERE id=1", [], |row| row.get(0))?;
        if phase != "recording" {
            return Err(error("storage.stopped", "journal is not accepting records"));
        }
        let pages = tx.pragma_query_value(None, "page_count", |row| unsigned(row, 0))?;
        let free = tx.pragma_query_value(None, "freelist_count", |row| unsigned(row, 0))?;
        if pages.saturating_sub(free) + ORDERED_RESERVED_PAGES > self.page_limit {
            return Err(error(
                "storage.full",
                "insufficient journal space for ordered pending admission",
            ));
        }
        let changed = tx.execute(
            "INSERT OR IGNORE INTO ordered_owner VALUES(1,?1,?2,NULL,NULL,'live')",
            params![producer, collection],
        )?;
        if changed != 1 {
            return Err(error(
                "storage.busy",
                "ordered owner lifecycle already exists",
            ));
        }
        tx.execute(
            "INSERT INTO ordered_host_cleanup VALUES(1,?1,'uncertain')",
            [producer],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Bind the exact source generation once registration has returned it.
    /// Repeating the same binding is safe; a different binding is refused.
    pub fn bind_ordered_generation(
        &self,
        producer: &str,
        kind: &str,
        generation: &str,
    ) -> Result<()> {
        if !matches!(kind, "counter" | "opaque") || generation.is_empty() || generation.len() > 256
        {
            return Err(error(
                "argument.invalid",
                "invalid ordered source generation",
            ));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute("UPDATE ordered_owner SET generation_kind=?1,generation_value=?2 WHERE id=1 AND producer_id=?3 AND state='live' AND (generation_kind IS NULL OR (generation_kind=?1 AND generation_value=?2))", params![kind,generation,producer])?;
        if changed != 1 {
            return Err(error(
                "storage.identity",
                "ordered source generation conflicts with live owner",
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// Settle a known live producer after its terminal and cleanup have completed.
    pub fn seal_ordered(&self, producer: &str) -> Result<()> {
        self.finish_ordered(producer, "sealed")
    }

    pub(crate) fn settle_ordered_host_cleanup(&self, producer: &str, released: bool) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = if released {
            "released"
        } else {
            "not-applicable"
        };
        let changed = tx.execute("UPDATE ordered_host_cleanup SET state=?1 WHERE id=1 AND producer_id=?2 AND (state='uncertain' OR state=?1)", params![state, producer])?;
        if changed != 1 {
            return Err(error(
                "storage.identity",
                "ordered host cleanup identity differs",
            ));
        }
        tx.commit()?;
        Ok(())
    }

    fn ordered_host_cleanup(&self) -> Result<Value> {
        let connection = self.connection()?;
        let owner: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM ordered_owner WHERE id=1)",
            [],
            |row| row.get(0),
        )?;
        let state = match connection.query_row(
            "SELECT state FROM ordered_host_cleanup WHERE id=1",
            [],
            |row| row.get::<_, String>(0),
        ) {
            Ok(state) => state,
            Err(rusqlite::Error::QueryReturnedNoRows) if owner => "uncertain".to_owned(),
            Err(rusqlite::Error::QueryReturnedNoRows) => "not-applicable".to_owned(),
            Err(error) => return Err(error.into()),
        };
        Ok(if state == "released" {
            json!({"state":"released","failures":[]})
        } else {
            json!({"state":state})
        })
    }

    /// The host may invoke this only after proving the producer is gone. This
    /// preserves the uncertain tail and does not resume the physical source.
    pub(crate) fn recover_interrupted_ordered(&self, producer: &str) -> Result<()> {
        self.finish_ordered(producer, "interrupted")
    }

    fn finish_ordered(&self, producer: &str, state: &str) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if state == "sealed" {
            let pending: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM ordered_pending WHERE id=1)",
                [],
                |row| row.get(0),
            )?;
            if pending {
                return Err(error(
                    "storage.owner-conflict",
                    "ordered delivery remains pending",
                ));
            }
        }
        let changed = tx.execute("UPDATE ordered_owner SET state=?1 WHERE id=1 AND producer_id=?2 AND (state='live' OR state=?1)", params![state,producer])?;
        if changed != 1 {
            return Err(error(
                "storage.identity",
                "ordered owner is not the expected live producer",
            ));
        }
        tx.execute("UPDATE journal SET phase='stopped' WHERE id=1", [])?;
        tx.commit()?;
        Ok(())
    }

    /// Forget only the deduplication entry after the host has authoritatively
    /// confirmed the exact native delivery ACK. Recorded bodies remain intact.
    pub fn retire_ordered_identity(
        &self,
        producer: &str,
        collection: &str,
        kind: &str,
        generation: &str,
        delivery: &str,
    ) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owns: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ordered_owner WHERE id=1 AND producer_id=?1 AND collection_id=?2 AND generation_kind=?3 AND generation_value=?4 AND state='live')", params![producer,collection,kind,generation], |row| row.get(0))?;
        if !owns {
            return Err(error(
                "storage.owner-conflict",
                "ordered producer does not own identity",
            ));
        }
        tx.execute("DELETE FROM ordered_identities WHERE producer_id=?1 AND collection_id=?2 AND generation_kind=?3 AND generation_value=?4 AND delivery_id=?5", params![producer,collection,kind,generation,delivery])?;
        tx.commit()?;
        Ok(())
    }

    /// After the host proves an interrupted producer is gone, its source
    /// deliveries cannot replay and its deduplication entries can be retired.
    pub(crate) fn retire_interrupted_ordered_identities(&self, producer: &str) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let interrupted: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ordered_owner WHERE id=1 AND producer_id=?1 AND state='interrupted')", [producer], |row| row.get(0))?;
        if !interrupted {
            return Err(error(
                "storage.owner-conflict",
                "ordered producer is not interrupted",
            ));
        }
        tx.execute(
            "DELETE FROM ordered_identities WHERE producer_id=?1",
            [producer],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The collection owner declares a terminal admission failure. The local
    /// marker survives native session release through the registry-owned Arc.
    /// Persisting the stop is best effort, with its failure explicitly retained;
    /// inability to write never becomes a false claim of durable failure history.
    /// This compatibility method can coalesce distinct failures and is not a
    /// lossless occurrence API. The admitted diagnostic permit interface is
    /// currently internal and has not been wired to production producers.
    pub fn mark_collection_failure(&self, failure: &JournalError) {
        let mut connection = self.connection();
        self.mark_collection_failure_locked(
            connection
                .as_mut()
                .map(|guard| &mut **guard)
                .map_err(|e| e.clone()),
            failure,
        );
    }

    fn mark_collection_failure_locked(
        &self,
        connection: Result<&mut Connection>,
        failure: &JournalError,
    ) {
        let mut state = self
            .failures
            .collection_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .as_ref()
            .is_some_and(|report| report["persisted"] == true)
        {
            return;
        }
        // Loss deltas need the transactional loss retry path, not this update.
        if *self
            .failures
            .pending_recorded_loss
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            != RecordedLoss::default()
        {
            return;
        }
        if !self
            .failures
            .separate_occurrences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
        {
            return;
        }
        let original = state.clone();
        let mut diagnostic = original.clone().unwrap_or_else(|| diagnostic(failure));
        diagnostic["persisted"] = json!(true);
        if let Some(object) = diagnostic.as_object_mut() {
            object.remove("persistenceFailure");
        }
        let result = connection.and_then(|connection| {
            connection
                .execute(
                    "UPDATE journal SET phase='stopped',collection_failure=?1 WHERE id=1 AND collection_failure IS NULL",
                    [diagnostic.to_string()],
                )
                .map_err(JournalError::from)?;
            summary_for_update(connection)?.ok_or_else(|| error("storage.corrupt", "diagnostic summary is missing after failure update"))
        });
        match result {
            Ok(first) => {
                diagnostic = match original {
                    Some(local) if !same_failure(&local, &first) => local,
                    _ => first,
                };
            }
            Err(error) => {
                diagnostic["persisted"] = json!(false);
                diagnostic["persistenceFailure"] =
                    self::diagnostic(&error.at("mark-collection-failure"));
            }
        }
        *state = Some(diagnostic.clone());
        drop(state);
        self.retire_persisted_runtime_failure(&diagnostic);
    }

    // A runtime diagnostic can be released only when that exact error is
    // already present in the durable collection report. An unrelated durable
    // failure must never retire the sole copy of runtime evidence.
    fn retire_persisted_runtime_failure(&self, report: &Value) {
        if report["persisted"] != true {
            return;
        }
        let mut runtime = self
            .failures
            .runtime_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if runtime
            .as_ref()
            .is_some_and(|failure| same_failure(&diagnostic(failure), report))
        {
            *runtime = None;
        }
    }

    /// Persists context and value together before returning acceptance. Context
    /// must identify the process/session epoch, peer and generation-bound selector.
    /// One record is the one-element case of [`Self::append_batch`], so single and
    /// grouped admission cannot drift apart.
    pub fn append(&self, metadata: &Value, record: &Value) -> Result<Value> {
        let batch = self.append_batch(&[(metadata, record)])?;
        match batch.rejected {
            Some(rejected) => Err(rejected.error),
            None => Ok(json!({"accepted":true,"ordinal":batch.first_ordinal})),
        }
    }

    /// Atomically commits one ordered delivery and its retry identity. Identities
    /// remain after reader acknowledgement until the exact native delivery ACK
    /// is confirmed and the owner explicitly retires them. A retry before then
    /// returns its original ordinal even after its record was read. Identity
    /// storage consumes the same bounded SQLite page quota as records. The owner
    /// supplies a collision-resistant producer incarnation and retains it for
    /// retries, including after reopening the journal.
    pub fn append_ordered(
        &self,
        producer_id: &str,
        collection_id: &str,
        generation_kind: &str,
        generation_value: &str,
        delivery_id: &str,
        metadata: &Value,
        record: &Value,
    ) -> Result<Value> {
        let outcome = self
            .append_ordered_inner(
                producer_id,
                collection_id,
                generation_kind,
                generation_value,
                delivery_id,
                metadata,
                record,
            )
            .map_err(|failure| failure.at("append-ordered"));
        if let Err(failure) = &outcome {
            if failure.kind.starts_with("storage.")
                && failure.kind != "storage.stopped"
                && failure.kind != "storage.identity-conflict"
                && failure.kind != "storage.owner-conflict"
                && failure.kind != "storage.collection-failed"
            {
                if let Ok(mut stored) = self.failures.runtime_failure.lock() {
                    *stored = Some(failure.clone());
                }
            }
        }
        outcome
    }

    fn append_ordered_inner(
        &self,
        producer_id: &str,
        collection_id: &str,
        generation_kind: &str,
        generation_value: &str,
        delivery_id: &str,
        metadata: &Value,
        record: &Value,
    ) -> Result<Value> {
        if [
            producer_id,
            collection_id,
            generation_kind,
            generation_value,
            delivery_id,
        ]
        .iter()
        .any(|part| part.is_empty() || part.len() > 256)
            || !metadata.is_object()
            || !record.is_object()
        {
            return Err(error(
                "argument.invalid",
                "invalid ordered delivery identity or payload",
            ));
        }
        encoded(metadata, 16384)?;
        encoded(record, 65536)?;
        let payload = encoded(&json!({"metadata":metadata,"record":record}), 82944)?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = tx.query_row(
            "SELECT payload,ordinal FROM ordered_identities WHERE producer_id=?1 AND collection_id=?2 AND generation_kind=?3 AND generation_value=?4 AND delivery_id=?5",
            params![producer_id,collection_id,generation_kind,generation_value,delivery_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        );
        match existing {
            Ok((previous, ordinal)) if previous == payload => {
                tx.commit()?;
                return Ok(json!({"accepted":true,"ordinal":ordinal}));
            }
            Ok(_) => {
                return Err(error(
                    "storage.identity-conflict",
                    "ordered delivery identity has a different payload",
                ));
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(failure) => return Err(failure.into()),
        }
        let lifecycle = tx.query_row("SELECT producer_id,collection_id,generation_kind,generation_value,state FROM ordered_owner WHERE id=1", [], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,String>(4)?)));
        match lifecycle {
            Ok((producer, collection, kind, generation, state))
                if producer == producer_id
                    && collection == collection_id
                    && kind.as_deref() == Some(generation_kind)
                    && generation.as_deref() == Some(generation_value)
                    && state == "live" => {}
            Ok((producer, collection, None, None, state))
                if producer == producer_id
                    && collection == collection_id
                    && generation_kind == "no-source"
                    && generation_value == "not-created"
                    && delivery_id == "setup-terminal"
                    && state == "live" => {}
            Ok(_) => {
                return Err(error(
                    "storage.identity",
                    "delivery does not match bound live ordered owner",
                ));
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(error(
                    "storage.owner-conflict",
                    "no ordered producer owns this journal",
                ));
            }
            Err(failure) => return Err(failure.into()),
        }
        let pending = tx.query_row("SELECT producer_id,collection_id,generation_kind,generation_value,delivery_id,payload FROM ordered_pending WHERE id=1", [], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,String>(5)?)));
        let retrying = match pending {
            Ok((producer, collection, kind, generation, delivery, previous)) => {
                if producer != producer_id
                    || collection != collection_id
                    || kind != generation_kind
                    || generation != generation_value
                    || delivery != delivery_id
                {
                    return Err(error(
                        "storage.owner-conflict",
                        "a prior ordered delivery remains pending",
                    ));
                }
                if previous != payload {
                    return Err(error(
                        "storage.identity-conflict",
                        "pending ordered delivery has a different payload",
                    ));
                }
                true
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => false,
            Err(failure) => return Err(failure.into()),
        };
        if self.has_collection_failure()? {
            return Err(error(
                "storage.collection-failed",
                "collection terminated; retained records remain readable",
            ));
        }
        let (phase, ordinal, count): (String, i64, u64) = tx.query_row(
            "SELECT phase,next_ordinal,retained_records FROM journal WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, unsigned(row, 2)?)),
        )?;
        if phase != "recording" && !(phase == "capacity-reached" && retrying) {
            return Err(if phase == "capacity-reached" {
                error(
                    "storage.full",
                    "recording capacity reached; unacknowledged records retained",
                )
            } else {
                error("storage.stopped", "journal is not accepting records")
            });
        }
        let body = encoded(
            &json!({"ordinal":ordinal,"metadata":metadata,"record":record}),
            82944,
        )?;
        // Delete within this transaction; rollback preserves a still-full retry.
        if retrying {
            tx.execute("DELETE FROM ordered_pending WHERE id=1", [])?;
        }
        let pages = tx.pragma_query_value(None, "page_count", |row| unsigned(row, 0))?;
        let free = tx.pragma_query_value(None, "freelist_count", |row| unsigned(row, 0))?;
        let identity_bytes = payload.len()
            + producer_id.len()
            + collection_id.len()
            + generation_kind.len()
            + generation_value.len()
            + delivery_id.len();
        let needed =
            (body.len() as u64).div_ceil(PAGE) + (identity_bytes as u64).div_ceil(PAGE) + 24;
        if count >= self.quota.max_records
            || pages.saturating_sub(free) + needed + ORDERED_RESERVED_PAGES > self.page_limit
        {
            if retrying {
                return Err(error(
                    "storage.full",
                    "recording capacity reached; unacknowledged records retained",
                ));
            }
            tx.execute(
                "INSERT INTO ordered_pending VALUES(1,?1,?2,?3,?4,?5,?6)",
                params![
                    producer_id,
                    collection_id,
                    generation_kind,
                    generation_value,
                    delivery_id,
                    payload
                ],
            )?;
            // The source still owns this delivery, so refusal is not evidence
            // of loss. Seal admission durably without incrementing lostRecords.
            tx.execute("UPDATE journal SET phase='capacity-reached' WHERE id=1", [])?;
            tx.commit()?;
            return Err(error(
                "storage.full",
                "recording capacity reached; unacknowledged records retained",
            ));
        }
        let next = ordinal
            .checked_add(1)
            .ok_or_else(|| error("storage.full", "journal ordinal exhausted"))?;
        tx.execute(
            "INSERT INTO records(ordinal,body,bytes) VALUES(?1,?2,?3)",
            params![ordinal, body, sql_integer(body.len() as u64)?],
        )?;
        tx.execute(
            "INSERT INTO ordered_identities VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                producer_id,
                collection_id,
                generation_kind,
                generation_value,
                delivery_id,
                payload,
                ordinal
            ],
        )?;
        tx.execute("UPDATE journal SET phase='recording',next_ordinal=?1,retained_records=retained_records+1,retained_bytes=retained_bytes+?2 WHERE id=1", params![next,sql_integer(body.len() as u64)?])?;
        tx.commit()?;
        Ok(json!({"accepted":true,"ordinal":ordinal}))
    }

    /// Persists up to [`APPEND_BATCH_MAX`] already-held records in ONE immediate
    /// transaction (one synced commit) with exactly the per-record admission
    /// rules and consecutive ordinals of sequential [`Self::append`] calls.
    ///
    /// * `Err`: validation, storage, stop or commit failure. The transaction was
    ///   rolled back, no record of this batch is retained and none was accepted.
    /// * `Ok` with `rejected: None`: every record committed.
    /// * `Ok` with `rejected: Some`: capacity was reached. The accepted prefix and
    ///   the loss marker for the first rejected record committed atomically;
    ///   `not_attempted` later records were neither retained nor marked lost
    ///   here, exactly as sequential admission stops at its first refusal. The
    ///   caller must account for them.
    pub fn append_batch(&self, items: &[(&Value, &Value)]) -> Result<AppendBatch> {
        self.append_batch_checked(items, |_| Ok(()))
    }

    /// Apply the caller's per-record admission check within the same transaction
    /// and in the same order as journal validation and capacity checks.
    pub(crate) fn append_batch_checked(
        &self,
        items: &[(&Value, &Value)],
        check: impl Fn(usize) -> Result<()>,
    ) -> Result<AppendBatch> {
        self.append_batch_with_policy(items, None, check, false)
    }

    pub(crate) fn append_batch_terminal_checked(
        &self,
        items: &[(&Value, &Value)],
        check: impl Fn(usize) -> Result<()>,
    ) -> Result<AppendBatch> {
        self.append_batch_with_policy(items, None, check, true)
    }

    fn append_batch_with_policy(
        &self,
        items: &[(&Value, &Value)],
        accounted: Option<(&[RecordedLoss], RecordedLoss)>,
        check: impl Fn(usize) -> Result<()>,
        terminal: bool,
    ) -> Result<AppendBatch> {
        if self.uses_released_v1_schema() {
            // The released primary schema has no ordered owner, evidence
            // cursor, claim-copy, or `recording_acked` column. Keep ordinary
            // recording available through its original transactional contract.
            return self.append_batch_v1(items, check);
        }
        let mut connection = match self.connection() {
            Ok(connection) => connection,
            Err(failure) => {
                let failure = failure.at("append");
                if let Some((impacts, trailing)) = accounted {
                    let loss = impacts.iter().copied().fold(trailing, RecordedLoss::add);
                    self.mark_recorded_collection_loss_locked(Err(failure.clone()), &failure, loss);
                } else if terminal {
                    self.mark_collection_failure_locked(Err(failure.clone()), &failure);
                }
                return Err(failure);
            }
        };
        let outcome = self
            .append_batch_inner(&mut connection, items, accounted, &check)
            .map_err(|failure| failure.at("append"));
        #[cfg(test)]
        if outcome.is_err() {
            if let Some(hook) = self.failures.failed_append_hook.lock().unwrap().as_ref() {
                hook();
            }
        }
        if accounted.is_none() {
            if let Err(failure) = &outcome {
                if failure.kind.starts_with("storage.")
                    && failure.kind != "storage.stopped"
                    && failure.kind != "storage.owner-conflict"
                    && failure.kind != "storage.collection-failed"
                {
                    if let Ok(mut stored) = self.failures.runtime_failure.lock() {
                        *stored = Some(failure.clone());
                    }
                }
            }
        }
        if let Some((impacts, trailing)) = accounted {
            if let Err(failure) = &outcome {
                if failure.kind != "storage.owner-conflict" {
                    let loss = impacts.iter().copied().fold(trailing, RecordedLoss::add);
                    self.mark_recorded_collection_loss_locked(Ok(&mut connection), failure, loss);
                }
            }
        } else if terminal {
            let failure = match &outcome {
                Err(failure)
                    if failure.kind != "storage.stopped"
                        && failure.kind != "storage.owner-conflict"
                        && failure.kind != "storage.collection-failed" =>
                {
                    Some(failure)
                }
                Ok(batch) => batch.rejected.as_ref().map(|rejected| &rejected.error),
                _ => None,
            };
            if let Some(failure) = failure {
                self.mark_collection_failure_locked(Ok(&mut connection), failure);
            }
        }
        outcome
    }

    fn append_batch_v1(
        &self,
        items: &[(&Value, &Value)],
        check: impl Fn(usize) -> Result<()>,
    ) -> Result<AppendBatch> {
        if items.is_empty() || items.len() > APPEND_BATCH_MAX {
            return Err(error(
                "argument.invalid",
                "journal batch must hold between one and its maximum of records",
            ));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (phase, first, retained): (String, i64, u64) = tx.query_row(
            "SELECT phase,next_ordinal,retained_records FROM journal WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, unsigned(row, 2)?)),
        )?;
        if phase != "recording" {
            return Err(if phase == "capacity-reached" {
                error(
                    "storage.full",
                    "recording capacity reached; unacknowledged records retained",
                )
            } else {
                error("storage.stopped", "journal is not accepting records")
            });
        }
        let mut ordinal = first;
        let mut accepted = 0usize;
        let mut added_bytes = 0u64;
        for (index, (metadata, record)) in items.iter().enumerate() {
            check(index)?;
            if !metadata.is_object() || !record.is_object() {
                return Err(error(
                    "argument.invalid",
                    "record and context must be objects",
                ));
            }
            encoded(metadata, 16384)?;
            encoded(record, 65536)?;
            if retained + accepted as u64 >= self.quota.max_records {
                tx.execute("UPDATE journal SET phase='capacity-reached' WHERE id=1", [])?;
                tx.commit()?;
                return Ok(AppendBatch {
                    accepted,
                    first_ordinal: first,
                    rejected: Some(AppendRejection {
                        error: error(
                            "storage.full",
                            "recording capacity reached; unacknowledged records retained",
                        ),
                        not_attempted: items.len() - index - 1,
                    }),
                });
            }
            let body = encoded(
                &json!({"ordinal":ordinal,"metadata":metadata,"record":record}),
                82944,
            )?;
            tx.execute(
                "INSERT INTO records VALUES(?1,?2,?3)",
                params![ordinal, body, sql_integer(body.len() as u64)?],
            )?;
            ordinal = ordinal
                .checked_add(1)
                .ok_or_else(|| error("storage.full", "journal ordinal exhausted"))?;
            accepted += 1;
            added_bytes = added_bytes
                .checked_add(body.len() as u64)
                .ok_or_else(|| error("storage.full", "journal byte counter overflow"))?;
        }
        tx.execute(
            "UPDATE journal SET next_ordinal=?1,retained_records=retained_records+?2,retained_bytes=retained_bytes+?3",
            params![ordinal, sql_integer(accepted as u64)?, sql_integer(added_bytes)?],
        )?;
        tx.commit()?;
        Ok(AppendBatch {
            accepted,
            first_ordinal: first,
            rejected: None,
        })
    }
    /// Admits a bounded recorded group and atomically records the entire refused
    /// suffix and its raw-byte loss when journal capacity is reached. A storage
    /// error reports whether subsequent loss accounting itself persisted.
    pub fn append_recorded_batch(
        &self,
        items: &[(&Value, &Value)],
        impacts: &[RecordedLoss],
        trailing: RecordedLoss,
    ) -> Result<AppendBatch> {
        if items.len() != impacts.len() || items.is_empty() || items.len() > APPEND_BATCH_MAX {
            return Err(error(
                "argument.invalid",
                "recorded batch impacts do not match records",
            ));
        }
        self.append_batch_with_policy(items, Some((impacts, trailing)), |_| Ok(()), false)
    }

    /// Persist a producer-owned occurrence. The producer must retain the same
    /// unique identity and exact failure and loss across retries and restarts.
    /// An error may follow a committed write; retry with those same inputs.
    /// The returned receipt identifies the durable occurrence, not a total for
    /// any unreadable historical summary. Storage capacity is not reserved.
    pub fn record_loss_occurrence(
        &self,
        id: [u8; 40],
        failure: &JournalError,
        loss: RecordedLoss,
    ) -> Result<RecordedLossReceipt> {
        let limit = diagnostic_field_bytes([
            failure.kind,
            failure.detail,
            failure.operation,
            failure.sqlite_code.as_deref().unwrap_or(""),
        ])
        .ok_or_else(|| error("storage.full", "diagnostic payload exceeds limit"))?;
        let entry = DiagnosticEntry {
            id,
            category: DiagnosticCategory::Collection,
            failure: failure.clone(),
            loss,
            durable: false,
            persistence_failure: None,
        };
        let payload = diagnostic_payload(&entry);
        self.reserve_durable_diagnostic(id, limit, &payload)?;
        self.replay_owned_diagnostic(id, &payload)?;
        Ok(RecordedLossReceipt { occurrence: id })
    }

    /// Records known raw loss even after collection has stopped. If storage
    /// refuses this update, the same aggregate is retained as unpersisted.
    pub fn mark_recorded_collection_loss(&self, failure: &JournalError, loss: RecordedLoss) {
        let mut connection = self.connection();
        self.mark_recorded_collection_loss_locked(
            connection
                .as_mut()
                .map(|guard| &mut **guard)
                .map_err(|e| e.clone()),
            failure,
            loss,
        );
    }

    fn mark_recorded_collection_loss_locked(
        &self,
        connection: Result<&mut Connection>,
        failure: &JournalError,
        loss: RecordedLoss,
    ) {
        let mut state = self
            .failures
            .collection_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut pending = self
            .failures
            .pending_recorded_loss
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let new_pending = pending.add(loss);
        let mut occurrences = self
            .failures
            .separate_occurrences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut transferred = false;
        let original = state.clone();
        // On a failed transaction retain the local aggregate and the exact
        // uncommitted delta independently of changes by other handles.
        let prior = RecordedLoss::from_report(state.as_ref());
        let mut report = state.clone().unwrap_or_else(|| diagnostic(failure));
        report["knownRecordedLoss"] = prior.add(loss).value();
        let result = (|| -> Result<()> {
            // Allocate the identity before the transaction, and retain the exact
            // delta until a retry confirms the commit. The same transaction
            // inserts the occurrence and applies its aggregate exactly once.
            if new_pending != RecordedLoss::default() {
                occurrences
                    .try_reserve(1)
                    .map_err(|_| error("storage.full", "loss occurrence allocation failed"))?;
                let mut id = [0; 40];
                getrandom::fill(&mut id).map_err(|_| {
                    error("storage.io", "loss occurrence identity entropy unavailable")
                })?;
                if id == [0; 40] {
                    return Err(error(
                        "storage.identity",
                        "invalid loss occurrence identity",
                    ));
                }
                occurrences.push(DiagnosticEntry {
                    id,
                    category: DiagnosticCategory::Collection,
                    failure: failure.clone(),
                    loss: new_pending,
                    durable: false,
                    persistence_failure: None,
                });
                *pending = RecordedLoss::default();
                transferred = true;
            }
            let connection = connection?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for entry in occurrences.iter() {
                persist_diagnostic_tx(&tx, entry.id, &diagnostic_payload(entry))?;
            }
            if !summary_needs_separate_loss(&tx)? {
                if let Some(durable) = summary_for_update(&tx)? {
                    report = durable;
                }
            }
            tx.commit()?;
            occurrences.clear();
            report["persisted"] = json!(true);
            if let Some(object) = report.as_object_mut() {
                object.remove("persistenceFailure");
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                *pending = RecordedLoss::default();
                if let Some(local) = original.as_ref() {
                    if local["persisted"] == false && !same_failure(local, &report) {
                        report = local.clone();
                    }
                }
            }
            Err(error) => {
                if !transferred {
                    *pending = new_pending;
                }
                if let Some(local) = original
                    .as_ref()
                    .filter(|value| value["persisted"] == false)
                {
                    report = local.clone();
                }
                report["persisted"] = json!(false);
                report["persistenceFailure"] =
                    diagnostic(&error.at("mark-recorded-collection-loss"));
            }
        }
        *state = Some(report.clone());
        drop(occurrences);
        drop(pending);
        drop(state);
        self.retire_persisted_runtime_failure(&report);
    }

    fn append_batch_inner(
        &self,
        connection: &mut Connection,
        items: &[(&Value, &Value)],
        accounted: Option<(&[RecordedLoss], RecordedLoss)>,
        check: &dyn Fn(usize) -> Result<()>,
    ) -> Result<AppendBatch> {
        if items.is_empty() || items.len() > APPEND_BATCH_MAX {
            return Err(error(
                "argument.invalid",
                "journal batch must hold between one and its maximum of records",
            ));
        }
        // Dropping the transaction on any early return rolls the whole batch back.
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ordered_live: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ordered_owner WHERE id=1 AND state='live')",
            [],
            |row| row.get(0),
        )?;
        if ordered_live {
            return Err(error(
                "storage.owner-conflict",
                "ordered producer owns this journal",
            ));
        }
        if self.has_collection_failure()? {
            return Err(error(
                "storage.collection-failed",
                "collection terminated; retained records remain readable",
            ));
        }
        let validate = |index: usize| -> Result<()> {
            check(index)?;
            let (metadata, record) = items[index];
            if !metadata.is_object() || !record.is_object() {
                return Err(error(
                    "argument.invalid",
                    "record and context must be objects",
                ));
            }
            encoded(metadata, 16384)?;
            encoded(record, 65536)?;
            Ok(())
        };
        // A single append validates its own record even if capacity was already
        // reached. Later records have no say once an earlier admission refuses.
        validate(0)?;
        let (phase, first, count): (String, i64, u64) = tx.query_row(
            "SELECT phase,next_ordinal,retained_records FROM journal WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, unsigned(row, 2)?)),
        )?;
        let full = || {
            error(
                "storage.full",
                "recording capacity reached; unacknowledged records retained",
            )
        };
        if phase == "capacity-reached" {
            if let Some((impacts, trailing)) = accounted {
                let loss = impacts.iter().copied().fold(trailing, RecordedLoss::add);
                let mut report = diagnostic(&full().at("append"));
                report["persisted"] = json!(true);
                report["knownRecordedLoss"] = loss.value();
                let increment = sql_integer(loss.items)?;
                preserve_summary_before_update(&tx)?;
                tx.execute("UPDATE journal SET phase='stopped',lost=CASE WHEN lost>?1 THEN 9223372036854775807 ELSE lost+?2 END,collection_failure=?3 WHERE id=1",params![i64::MAX-increment,increment,report.to_string()])?;
                tx.commit()?;
                *self
                    .failures
                    .collection_failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
            } else {
                tx.execute("UPDATE journal SET lost=CASE WHEN lost<9223372036854775807 THEN lost+1 ELSE lost END WHERE id=1",[])?;
                tx.commit()?;
            }
            return Ok(AppendBatch {
                accepted: 0,
                first_ordinal: first,
                rejected: Some(AppendRejection {
                    error: full().at("append"),
                    not_attempted: items.len() - 1,
                }),
            });
        }
        if phase != "recording" {
            let failed: bool = tx.query_row(
                "SELECT collection_failure IS NOT NULL FROM journal WHERE id=1",
                [],
                |row| row.get(0),
            )?;
            if failed {
                return Err(error(
                    "storage.collection-failed",
                    "collection terminated; retained records remain readable",
                ));
            }
            return Err(error("storage.stopped", "journal is not accepting records"));
        }
        let mut next = first;
        let mut accepted = 0usize;
        let mut added_bytes = 0u64;
        let mut rejected = None;
        for (index, (metadata, record)) in items.iter().enumerate() {
            if index > 0 {
                validate(index)?;
            }
            let body = encoded(
                &json!({"ordinal":next,"metadata":metadata,"record":record}),
                82944,
            )?;
            let pages = tx.pragma_query_value(None, "page_count", |row| unsigned(row, 0))?;
            let free = tx.pragma_query_value(None, "freelist_count", |row| unsigned(row, 0))?;
            // Reserve schema/cursor/loss update space; conservatively cover a record's
            // overflow pages and B-tree split path before admitting its transaction.
            let needed = (body.len() as u64).div_ceil(PAGE) + 12;
            if count + index as u64 >= self.quota.max_records
                || pages.saturating_sub(free) + needed + RESERVED_PAGES > self.page_limit
            {
                rejected = Some(AppendRejection {
                    error: full().at("append"),
                    not_attempted: items.len() - index - 1,
                });
                break;
            }
            let after = next
                .checked_add(1)
                .ok_or_else(|| error("storage.full", "journal ordinal exhausted"))?;
            tx.execute(
                "INSERT INTO records(ordinal,body,bytes) VALUES(?1,?2,?3)",
                params![next, body, sql_integer(body.len() as u64)?],
            )?;
            next = after;
            accepted = index + 1;
            added_bytes += body.len() as u64;
        }
        if accepted > 0 {
            tx.execute("UPDATE journal SET next_ordinal=?1,retained_records=retained_records+?2,retained_bytes=retained_bytes+?3 WHERE id=1", params![next,sql_integer(accepted as u64)?,sql_integer(added_bytes)?])?;
        }
        let mut recorded_failure = None;
        if let Some(ref rejection) = rejected {
            if let Some((impacts, trailing)) = accounted {
                let loss = impacts[accepted..]
                    .iter()
                    .copied()
                    .fold(trailing, RecordedLoss::add);
                let mut report = diagnostic(&rejection.error);
                report["persisted"] = json!(true);
                report["knownRecordedLoss"] = loss.value();
                let increment = sql_integer(loss.items)?;
                preserve_summary_before_update(&tx)?;
                tx.execute("UPDATE journal SET phase='stopped',lost=CASE WHEN lost>?1 THEN 9223372036854775807 ELSE lost+?2 END,collection_failure=?3 WHERE id=1",params![i64::MAX-increment,increment,report.to_string()])?;
                recorded_failure = Some(report);
            } else {
                tx.execute(
                    "UPDATE journal SET phase='capacity-reached',lost=lost+1 WHERE id=1",
                    [],
                )?;
            }
        }
        tx.commit()?;
        if let Some(report) = recorded_failure {
            *self
                .failures
                .collection_failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
        }
        Ok(AppendBatch {
            accepted,
            first_ordinal: first,
            rejected,
        })
    }

    /// A prepared prefix is stable until explicit acknowledgement, including
    /// across process death. `more` is the snapshot at preparation, not a live count.
    pub fn prepare(&self, max_items: u32, max_bytes: u32) -> Result<Value> {
        if self.uses_released_v1_schema() {
            return self
                .prepare_v1(max_items, max_bytes)
                .map_err(|failure| failure.at("prepare"));
        }
        self.prepare_inner(max_items, max_bytes)
            .map_err(|failure| failure.at("prepare"))
    }

    fn prepare_v1(&self, max_items: u32, max_bytes: u32) -> Result<Value> {
        if !(1..=2048).contains(&max_items) || !(1..=4_194_304).contains(&max_bytes) {
            return Err(error("argument.invalid", "invalid journal read bounds"));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (pending, last, more, sequence): (Option<String>, Option<i64>, Option<bool>, i64) = tx
            .query_row(
                "SELECT pending_token,pending_last,pending_more,next_token FROM journal WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        if let Some(token) = pending {
            let last =
                last.ok_or_else(|| error("storage.corrupt", "prepared cursor is incomplete"))?;
            let answer = prepared_v1(&tx, &token, last, more.unwrap_or(false))?;
            if answer["bytes"]
                .as_u64()
                .is_none_or(|bytes| bytes > u64::from(max_bytes))
                || answer["records"]
                    .as_array()
                    .is_none_or(|records| records.len() > max_items as usize)
            {
                return Err(error(
                    "argument.invalid",
                    "requested bounds cannot fit the already prepared prefix",
                ));
            }
            tx.commit()?;
            return Ok(answer);
        }
        let mut bytes = 0u64;
        let mut count = 0u32;
        let mut last = 0i64;
        let mut more = false;
        let mut statement = tx.prepare(
            "SELECT ordinal,bytes,octet_length(body) FROM records ORDER BY ordinal LIMIT 2049",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let size = unsigned(row, 1)?;
            if size != unsigned(row, 2)? {
                return Err(error(
                    "storage.corrupt",
                    "stored record length differs from its bytes",
                ));
            }
            if count >= max_items || size > u64::from(max_bytes).saturating_sub(bytes) {
                more = true;
                break;
            }
            last = row.get(0)?;
            bytes += size;
            count += 1;
        }
        drop(rows);
        drop(statement);
        if count == 0 {
            if more {
                return Err(error(
                    "argument.invalid",
                    "read byte bound cannot fit the oldest record",
                ));
            }
            tx.commit()?;
            return Ok(json!({"token":null,"records":[],"bytes":0,"more":false}));
        }
        let next = sequence
            .checked_add(1)
            .ok_or_else(|| error("storage.full", "journal token sequence exhausted"))?;
        let token = format!("{}:{sequence}", self.recording_id);
        tx.execute(
            "UPDATE journal SET pending_token=?1,pending_last=?2,pending_more=?3,next_token=?4 WHERE id=1",
            params![token, last, more, next],
        )?;
        let answer = prepared_v1(&tx, &token, last, more)?;
        tx.commit()?;
        Ok(answer)
    }
    fn prepare_inner(&self, max_items: u32, max_bytes: u32) -> Result<Value> {
        if !(1..=2048).contains(&max_items) || !(1..=4_194_304).contains(&max_bytes) {
            return Err(error("argument.invalid", "invalid journal read bounds"));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (pending, last, more, sequence): (Option<String>, Option<i64>, Option<bool>, i64) = tx
            .query_row(
                "SELECT pending_token,pending_last,pending_more,next_token FROM journal WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        if let Some(token) = pending {
            let last =
                last.ok_or_else(|| error("storage.corrupt", "prepared cursor is incomplete"))?;
            verify_recording_claim(&tx)?;
            check_record_lengths(&tx, 0, last, true)?;
            check_requested_bounds(&tx, 0, last, true, max_items, max_bytes)?;
            let answer = prepared(&tx, &token, last, more.unwrap_or(false))?;
            if answer["bytes"]
                .as_u64()
                .is_none_or(|bytes| bytes > u64::from(max_bytes))
                || answer["records"]
                    .as_array()
                    .is_none_or(|records| records.len() > max_items as usize)
            {
                return Err(error(
                    "argument.invalid",
                    "requested bounds cannot fit the already prepared prefix",
                ));
            }
            tx.commit()?;
            return Ok(answer);
        }
        let orphan: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM recording_claim_header) OR EXISTS(SELECT 1 FROM recording_claim_records)", [], |r| r.get(0))?;
        if orphan {
            return Err(error(
                "storage.corrupt",
                "orphaned recording claim identity retained",
            ));
        }
        let mut bytes = 0u64;
        let mut count = 0u32;
        let mut last = 0i64;
        let mut more = false;
        {
            let mut statement =
                tx.prepare("SELECT ordinal,bytes,octet_length(body) FROM records WHERE recording_acked=0 ORDER BY ordinal LIMIT 2049")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let size = unsigned(row, 1)?;
                if size != unsigned(row, 2)? {
                    return Err(error(
                        "storage.corrupt",
                        "stored record length differs from its bytes",
                    ));
                }
                if count >= max_items || bytes + size > u64::from(max_bytes) {
                    more = true;
                    break;
                }
                last = row.get(0)?;
                bytes += size;
                count += 1;
            }
        }
        if count == 0 {
            if more {
                return Err(error(
                    "argument.invalid",
                    "read byte bound cannot fit the oldest record",
                ));
            }
            tx.commit()?;
            return Ok(json!({"token":null,"records":[],"bytes":0,"more":false}));
        }
        let next = sequence
            .checked_add(1)
            .ok_or_else(|| error("storage.full", "journal token sequence exhausted"))?;
        let token = format!("{}:{sequence}", self.recording_id);
        tx.execute("UPDATE journal SET pending_token=?1,pending_last=?2,pending_more=?3,next_token=?4 WHERE id=1",params![token,last,more,next])?;
        let answer = prepared(&tx, &token, last, more)?;
        tx.execute(
            "INSERT INTO recording_claim_header VALUES(1,?1,?2,?3,?4)",
            params![token, last, more, count],
        )?;
        {
            let mut statement = tx.prepare("SELECT ordinal,CAST(body AS BLOB),bytes FROM records WHERE recording_acked=0 AND ordinal<=?1 ORDER BY ordinal")?;
            let mut rows = statement.query([last])?;
            while let Some(row) = rows.next()? {
                let ordinal: i64 = row.get(0)?;
                let body: Vec<u8> = row.get(1)?;
                let size: i64 = row.get(2)?;
                let digest = Sha256::digest(&body);
                tx.execute(
                    "INSERT INTO recording_claim_records VALUES(?1,?2,?3)",
                    params![ordinal, digest.as_slice(), size],
                )?;
            }
        }
        verify_recording_claim(&tx)?;
        tx.commit()?;
        Ok(answer)
    }

    pub fn acknowledge(&self, token: &str) -> Result<Value> {
        if self.uses_released_v1_schema() {
            return self
                .acknowledge_v1(token)
                .map_err(|failure| failure.at("acknowledge"));
        }
        self.acknowledge_inner(token)
            .map_err(|failure| failure.at("acknowledge"))
    }

    fn acknowledge_v1(&self, token: &str) -> Result<Value> {
        if token.is_empty() || token.len() > 96 {
            return Err(error("argument.invalid", "invalid journal token"));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (pending, last, acknowledged, receipt): (Option<String>, Option<i64>, Option<String>, Option<String>) = tx
            .query_row(
                "SELECT pending_token,pending_last,last_ack_token,last_ack_receipt FROM journal WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        if acknowledged.as_deref() == Some(token) {
            let receipt = decode(
                &receipt.ok_or_else(|| error("storage.corrupt", "ack receipt is missing"))?,
            )?;
            tx.commit()?;
            return Ok(receipt);
        }
        if pending.as_deref() != Some(token) {
            return Err(error(
                "argument.invalid",
                "journal token is unknown or stale",
            ));
        }
        let last = last.ok_or_else(|| error("storage.corrupt", "prepared cursor is incomplete"))?;
        prepared_v1(&tx, token, last, false)?;
        let removed_bytes: i64 = tx.query_row(
            "SELECT coalesce(sum(bytes),0) FROM records WHERE ordinal<=?1",
            [last],
            |row| row.get(0),
        )?;
        let removed = tx.execute("DELETE FROM records WHERE ordinal<=?1", [last])?;
        let receipt = json!({"token":token,"acknowledged":true,"records":removed});
        tx.execute(
            "UPDATE journal SET retained_records=retained_records-?1,retained_bytes=retained_bytes-?2,pending_token=NULL,pending_last=NULL,pending_more=NULL,last_ack_token=?3,last_ack_receipt=?4",
            params![sql_integer(removed as u64)?, removed_bytes, token, receipt.to_string()],
        )?;
        tx.commit()?;
        self.shared.reader_space.notify_one();
        Ok(receipt)
    }
    fn acknowledge_inner(&self, token: &str) -> Result<Value> {
        if token.is_empty() || token.len() > 96 {
            return Err(error("argument.invalid", "invalid journal token"));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (pending,last,ack,receipt):(Option<String>,Option<i64>,Option<String>,Option<String>)=tx.query_row("SELECT pending_token,pending_last,last_ack_token,last_ack_receipt FROM journal WHERE id=1",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
        if ack.as_deref() == Some(token) {
            let receipt = decode(
                &receipt.ok_or_else(|| error("storage.corrupt", "ack receipt is missing"))?,
            )?;
            tx.commit()?;
            return Ok(receipt);
        }
        if pending.as_deref() != Some(token) {
            return Err(error(
                "argument.invalid",
                "journal token is unknown or stale",
            ));
        }
        let last = last.ok_or_else(|| error("storage.corrupt", "prepared cursor is incomplete"))?;
        // Identity must match the exact committed prefix, not merely valid JSON.
        verify_recording_claim(&tx)?;
        let evidence_active: bool = tx.query_row(
            "SELECT transfer_id IS NOT NULL FROM evidence_cursor WHERE id=1",
            [],
            |r| r.get(0),
        )?;
        if evidence_active {
            verify_evidence_members(&tx)?;
        } else {
            reject_orphaned_evidence(&tx)?;
        }
        prepared(&tx, token, last, false)?;
        let acknowledged: i64 = tx.query_row(
            "SELECT count(*) FROM records WHERE recording_acked=0 AND ordinal<=?1",
            [last],
            |row| row.get(0),
        )?;
        tx.execute("UPDATE records SET recording_acked=1 WHERE recording_acked=0 AND ordinal<=?1 AND ordinal>(SELECT progressed FROM evidence_cursor WHERE id=1) AND ordinal<=(SELECT cutoff FROM evidence_cursor WHERE id=1)", [last])?;
        let removed_bytes: i64 = tx.query_row(
            "SELECT coalesce(sum(bytes),0) FROM records WHERE recording_acked=0 AND ordinal<=?1",
            [last],
            |row| row.get(0),
        )?;
        let removed = tx.execute(
            "DELETE FROM records WHERE recording_acked=0 AND ordinal<=?1",
            [last],
        )?;
        tx.execute("UPDATE journal SET retained_records=retained_records-?1,retained_bytes=retained_bytes-?2 WHERE id=1",params![sql_integer(removed as u64)?,removed_bytes])?;
        tx.execute("DELETE FROM recording_claim_records", [])?;
        tx.execute("DELETE FROM recording_claim_header", [])?;
        let receipt = json!({"token":token,"acknowledged":true,"records":acknowledged});
        tx.execute("UPDATE journal SET pending_token=NULL,pending_last=NULL,pending_more=NULL,last_ack_token=?1,last_ack_receipt=?2 WHERE id=1",params![token,receipt.to_string()])?;
        tx.commit()?;
        self.shared.reader_space.notify_one();
        Ok(receipt)
    }

    /// Discover the committed transfer and any pending claim after a restart.
    /// This reports journal state only; the destination may be volatile.
    pub fn evidence_transfer_status(&self) -> Result<Value> {
        self.evidence_transfer_status_inner()
            .map_err(|e| e.at("evidence_transfer_status"))
    }
    fn evidence_transfer_status_inner(&self) -> Result<Value> {
        let connection = self.connection()?;
        let (transfer, cutoff, progressed, pending): (Option<String>, i64, i64, Option<String>) = connection.query_row(
            "SELECT transfer_id,cutoff,progressed,pending_token FROM evidence_cursor WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let membership_valid = if transfer.is_some() {
            match verify_evidence_members(&connection) {
                Ok(()) => Some(true),
                Err(failure) if failure.kind == "storage.corrupt" => Some(false),
                Err(failure) => return Err(failure),
            }
        } else {
            None
        };
        Ok(
            json!({"transferId":transfer,"cutoff":cutoff.to_string(),"progressed":progressed.to_string(),"pendingToken":pending,"membershipValid":membership_valid,"complete":progressed >= cutoff && membership_valid != Some(false),"durable":true}),
        )
    }

    /// Start or resume one durable snapshot of the records retained at this
    /// transaction's linearization point. Records acknowledged before this
    /// point are not recoverable. Use a fresh transfer ID for each snapshot.
    /// Only one transfer can be active; snapshot rows count against the normal
    /// quota until both readers have acknowledged them. Later appends need a
    /// subsequent snapshot. A caller must safely store each page before ACK.
    /// `durable` describes journal state, not destination evidence durability.
    pub fn begin_evidence_transfer(&self, transfer_id: &str) -> Result<Value> {
        self.begin_evidence_transfer_inner(transfer_id)
            .map_err(|e| e.at("begin_evidence_transfer"))
    }
    fn begin_evidence_transfer_inner(&self, transfer_id: &str) -> Result<Value> {
        if !valid_id(transfer_id) {
            return Err(error(
                "argument.invalid",
                "invalid evidence transfer identity",
            ));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, cutoff, progressed): (Option<String>, i64, i64) = tx.query_row(
            "SELECT transfer_id,cutoff,progressed FROM evidence_cursor WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let cutoff = if current.as_deref() == Some(transfer_id) {
            verify_evidence_members(&tx)?;
            cutoff
        } else {
            if current.is_some() {
                verify_evidence_members(&tx)?;
            } else {
                reject_orphaned_evidence(&tx)?;
            }
            let claim: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM evidence_claim_header) OR EXISTS(SELECT 1 FROM evidence_claim_records) OR pending_token IS NOT NULL OR pending_last IS NOT NULL OR pending_more IS NOT NULL OR pending_count IS NOT NULL FROM evidence_cursor WHERE id=1", [], |r| r.get(0))?;
            if claim {
                return Err(error("storage.corrupt", "evidence claim state retained"));
            }
            if current.is_some() && progressed < cutoff {
                return Err(error(
                    "storage.owner-conflict",
                    "another evidence transfer is active",
                ));
            }
            let cutoff: i64 =
                tx.query_row("SELECT coalesce(max(ordinal),0) FROM records", [], |r| {
                    r.get(0)
                })?;
            tx.execute("DELETE FROM evidence_members", [])?;
            tx.execute("DELETE FROM evidence_members_header", [])?;
            tx.execute(
                "INSERT INTO evidence_members SELECT ordinal FROM records WHERE ordinal<=?1",
                [cutoff],
            )?;
            tx.execute("INSERT INTO evidence_members_header SELECT 1,?1,?2,0,count(*) FROM evidence_members", params![transfer_id,cutoff])?;
            tx.execute("UPDATE evidence_cursor SET transfer_id=?1,cutoff=?2,progressed=0,pending_token=NULL,pending_last=NULL,pending_more=NULL,last_ack_token=NULL,last_ack_receipt=NULL,last_rollback_token=NULL WHERE id=1", params![transfer_id,cutoff])?;
            cutoff
        };
        tx.commit()?;
        Ok(json!({"transferId":transfer_id,"cutoff":cutoff.to_string(),"durable":true}))
    }

    /// Prepare a stable bounded page, independent of the recording cursor.
    /// The page and its token survive process death; shrinking bounds is refused.
    /// `ordinals` and `recordBodies` carry exact identities and stored JSON bytes
    /// as strings in record order, including across JavaScript number limits.
    pub fn prepare_evidence_transfer(
        &self,
        transfer_id: &str,
        max_items: u32,
        max_bytes: u32,
    ) -> Result<Value> {
        self.prepare_evidence_transfer_inner(transfer_id, max_items, max_bytes)
            .map_err(|e| e.at("prepare_evidence_transfer"))
    }
    fn prepare_evidence_transfer_inner(
        &self,
        transfer_id: &str,
        max_items: u32,
        max_bytes: u32,
    ) -> Result<Value> {
        if !valid_id(transfer_id)
            || !(1..=2048).contains(&max_items)
            || !(1..=4_194_304).contains(&max_bytes)
        {
            return Err(error(
                "argument.invalid",
                "invalid evidence read identity or bounds",
            ));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, cutoff, progressed, pending, last, more, sequence): (Option<String>,i64,i64,Option<String>,Option<i64>,Option<bool>,i64) = tx.query_row("SELECT transfer_id,cutoff,progressed,pending_token,pending_last,pending_more,next_token FROM evidence_cursor WHERE id=1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
        if current.as_deref() != Some(transfer_id) {
            return Err(error("argument.invalid", "unknown evidence transfer"));
        }
        verify_evidence_members(&tx)?;
        if let Some(token) = pending {
            let last =
                last.ok_or_else(|| error("storage.corrupt", "evidence cursor is incomplete"))?;
            check_record_lengths(&tx, progressed, last, false)?;
            check_requested_bounds(&tx, progressed, last, false, max_items, max_bytes)?;
            verify_evidence_claim(&tx, progressed, last)?;
            let answer = evidence_page(
                &tx,
                transfer_id,
                &token,
                progressed,
                last,
                more.ok_or_else(|| error("storage.corrupt", "evidence cursor is incomplete"))?,
            )?;
            if answer["bytes"]
                .as_u64()
                .is_none_or(|n| n > u64::from(max_bytes))
                || answer["records"]
                    .as_array()
                    .is_none_or(|r| r.len() > max_items as usize)
            {
                return Err(error(
                    "argument.invalid",
                    "requested bounds cannot fit the prepared evidence page",
                ));
            }
            tx.commit()?;
            return Ok(answer);
        }
        let orphan: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM evidence_claim_header) OR EXISTS(SELECT 1 FROM evidence_claim_records)", [], |r| r.get(0))?;
        if orphan {
            return Err(error(
                "storage.corrupt",
                "orphaned evidence claim copy retained",
            ));
        }
        let (mut count, mut bytes, mut last, mut more) = (0u32, 0u64, progressed, false);
        {
            let mut stmt = tx.prepare("SELECT ordinal,bytes,octet_length(body) FROM records WHERE ordinal>?1 AND ordinal<=?2 ORDER BY ordinal LIMIT 2049")?;
            let mut rows = stmt.query(params![progressed, cutoff])?;
            while let Some(row) = rows.next()? {
                let size = unsigned(row, 1)?;
                if size != unsigned(row, 2)? {
                    return Err(error(
                        "storage.corrupt",
                        "stored record length differs from its bytes",
                    ));
                }
                if count >= max_items || size > u64::from(max_bytes).saturating_sub(bytes) {
                    more = true;
                    break;
                }
                last = row.get(0)?;
                bytes += size;
                count += 1;
            }
        }
        if count == 0 {
            if more {
                return Err(error(
                    "argument.invalid",
                    "read byte bound cannot fit the oldest evidence record",
                ));
            }
            if progressed < cutoff {
                return Err(error(
                    "storage.corrupt",
                    "evidence snapshot records are missing",
                ));
            }
            tx.commit()?;
            return Ok(
                json!({"transferId":transfer_id,"token":null,"records":[],"ordinals":[],"recordBodies":[],"bytes":0,"more":false,"durable":true}),
            );
        }
        let next = sequence
            .checked_add(1)
            .ok_or_else(|| error("storage.full", "evidence token sequence exhausted"))?;
        let token = format!("{}:evidence:{sequence}", self.recording_id);
        tx.execute(
            "INSERT INTO evidence_claim_header VALUES(1,?1,?2,?3,?4,?5,?6,?7)",
            params![transfer_id, token, progressed, cutoff, last, more, count],
        )?;
        tx.execute("INSERT INTO evidence_claim_records SELECT ordinal,CAST(body AS BLOB),bytes FROM records WHERE ordinal>?1 AND ordinal<=?2", params![progressed,last])?;
        tx.execute("UPDATE evidence_cursor SET pending_token=?1,pending_last=?2,pending_more=?3,next_token=?4,pending_count=?5 WHERE id=1", params![token,last,more,next,count])?;
        verify_evidence_claim(&tx, progressed, last)?;
        let answer = evidence_page(&tx, transfer_id, &token, progressed, last, more)?;
        tx.commit()?;
        Ok(answer)
    }

    /// Read one bounded chunk directly from a retained journal, without opening a
    /// writer, trusting counters, repairing state, or requiring an active cursor.
    /// Sources are `records` and `evidenceClaim`.
    /// `countersValid` and `counterFault` are null: this reader does not scan the
    /// journal to validate counters. Only the returned chunk and allocation are
    /// bounded; SQLite open, WAL handling and I/O are not bounded by `max_bytes`.
    /// This is a
    /// diagnostic file reader, not authorization; the host must restrict its path.
    /// Older schemas lack newer claim tables and cannot supply missing identities.
    /// SQLite may refuse a read-only open when recovery of a hot rollback journal
    /// requires writing; that failure is returned without modifying the file.
    pub fn inspect_raw_record(
        path: &Path,
        source: &str,
        ordinal: Option<&str>,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Value> {
        Self::inspect_raw_record_inner(path, source, ordinal, offset, max_bytes)
            .map_err(|e| e.at("inspect_raw_record"))
    }
    fn inspect_raw_record_inner(
        path: &Path,
        source: &str,
        ordinal: Option<&str>,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Value> {
        let table = match source {
            "records" => "records",
            "evidenceClaim" => "evidence_claim_records",
            _ => return Err(error("argument.invalid", "unknown raw recovery source")),
        };
        if !(1..=4_194_304).contains(&max_bytes) {
            return Err(error("argument.invalid", "invalid raw recovery bound"));
        }
        let offset = sql_integer(offset)?;
        let selected = ordinal
            .map(|v| {
                v.parse::<i64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| error("argument.invalid", "invalid recovery ordinal"))
            })
            .transpose()?;
        let meta = std::fs::symlink_metadata(path)
            .map_err(|_| error("storage.io", "journal file cannot be inspected"))?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(error(
                "storage.permission",
                "journal path is not a regular file",
            ));
        }
        let name = path
            .file_name()
            .ok_or_else(|| error("argument.invalid", "journal path needs a filename"))?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let resolved = parent
            .canonicalize()
            .map_err(|_| error("storage.io", "journal directory cannot be resolved"))?
            .join(name);
        let mut connection = Connection::open_with_flags(
            resolved,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(std::time::Duration::ZERO)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let schema: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let application: i64 = tx.pragma_query_value(None, "application_id", |r| r.get(0))?;
        if !(1..=SCHEMA_VERSION).contains(&schema) || application != APPLICATION_ID {
            return Err(error(
                "storage.schema",
                "unknown journal schema; file retained",
            ));
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(error(
                "storage.schema",
                "recovery source absent from this journal schema",
            ));
        }
        let id: i64 = tx.query_row(&format!("SELECT ordinal FROM {table} WHERE (?1 IS NULL OR ordinal=?1) ORDER BY ordinal LIMIT 1"), [selected], |r| r.get(0)).map_err(|e| match e { rusqlite::Error::QueryReturnedNoRows => error("argument.invalid", "recovery record not retained"), other => other.into() })?;
        let (size, stored): (u64, Option<i64>) = tx.query_row(
            &format!("SELECT octet_length(body),bytes FROM {table} WHERE ordinal=?1"),
            [id],
            |r| {
                Ok((
                    unsigned(r, 0)?,
                    match r.get_ref(1)? {
                        rusqlite::types::ValueRef::Integer(n) => Some(n),
                        _ => None,
                    },
                ))
            },
        )?;
        if offset as u64 > size {
            return Err(error(
                "argument.invalid",
                "recovery offset exceeds record length",
            ));
        }
        let start = usize::try_from(offset)
            .map_err(|_| error("argument.invalid", "recovery offset exceeds platform range"))?;
        let blob = tx.blob_open("main", table, "body", id, true)?;
        if blob.len() as u64 != size {
            return Err(error(
                "storage.corrupt",
                "record length changed during inspection",
            ));
        }
        let length = (size - offset as u64).min(u64::from(max_bytes)) as usize;
        let mut part = vec![0; length];
        blob.read_at_exact(&mut part, start)?;
        blob.close()?;
        let next: Option<i64> = tx.query_row(
            &format!("SELECT min(ordinal) FROM {table} WHERE ordinal>?1"),
            [id],
            |r| r.get(0),
        )?;
        let full = offset == 0 && part.len() as u64 == size;
        let json_valid = full.then(|| serde_json::from_slice::<Value>(&part).is_ok());
        let length_valid = stored.and_then(|n| u64::try_from(n).ok()) == Some(size);
        let length_fault = (!length_valid).then(|| {
            diagnostic(&error(
                "storage.corrupt",
                "stored record length is invalid or differs from its bytes",
            ))
        });
        let answer = json!({"source":source,"schemaVersion":schema,"ordinal":id.to_string(),"nextOrdinal":next.map(|n| n.to_string()),"offset":offset.to_string(),"length":size.to_string(),"storedLength":stored.map(|n| n.to_string()),"bytes":part,"complete":offset as u64 + length as u64 == size,"lengthValid":length_valid,"lengthFault":length_fault,"jsonValidated":full,"jsonValid":json_valid,"countersValid":null,"counterFault":null});
        tx.commit()?;
        Ok(answer)
    }

    /// Inspect raw bytes of a retained snapshot row without changing either cursor.
    /// `None` starts at the first unprogressed row; `nextOrdinal` enumerates rows.
    /// Offsets and lengths count bytes, not Unicode characters. Each call copies at
    /// most `max_bytes`; `fault` is null when no fault was detected, or identifies
    /// a length mismatch or, when the whole record fits in this call, malformed JSON.
    /// `lengthValid` reports the length check independently. `jsonValid` is null
    /// unless the full body was parsed. A partial chunk reports `jsonValidated:
    /// false`; callers can preserve every chunk without full-body validation.
    pub fn inspect_evidence_record(
        &self,
        transfer_id: &str,
        ordinal: Option<&str>,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Value> {
        self.inspect_evidence_record_inner(transfer_id, ordinal, offset, max_bytes)
            .map_err(|e| e.at("inspect_evidence_record"))
    }
    fn inspect_evidence_record_inner(
        &self,
        transfer_id: &str,
        ordinal: Option<&str>,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Value> {
        if !valid_id(transfer_id) || !(1..=4_194_304).contains(&max_bytes) {
            return Err(error(
                "argument.invalid",
                "invalid evidence inspection bounds",
            ));
        }
        let offset = sql_integer(offset)?;
        let selected = ordinal
            .map(|value| {
                value
                    .parse::<i64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| error("argument.invalid", "invalid evidence ordinal"))
            })
            .transpose()?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let (current, progressed, cutoff): (Option<String>, i64, i64) = tx.query_row(
            "SELECT transfer_id,progressed,cutoff FROM evidence_cursor WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if current.as_deref() != Some(transfer_id) {
            return Err(error("argument.invalid", "unknown evidence transfer"));
        }
        let id: i64 = tx.query_row("SELECT ordinal FROM records WHERE ordinal>?1 AND ordinal<=?2 AND (?3 IS NULL OR ordinal=?3) ORDER BY ordinal LIMIT 1", params![progressed,cutoff,selected], |row| row.get(0)).map_err(|e| match e { rusqlite::Error::QueryReturnedNoRows => error("argument.invalid", "evidence record not retained"), other => other.into() })?;
        let (size, stored): (u64, Option<i64>) = tx.query_row(
            "SELECT octet_length(body),bytes FROM records WHERE ordinal=?1",
            [id],
            |row| {
                Ok((
                    unsigned(row, 0)?,
                    match row.get_ref(1)? {
                        rusqlite::types::ValueRef::Integer(n) => Some(n),
                        _ => None,
                    },
                ))
            },
        )?;
        if offset as u64 > size {
            return Err(error(
                "argument.invalid",
                "evidence offset exceeds record length",
            ));
        }
        let start = usize::try_from(offset)
            .map_err(|_| error("argument.invalid", "evidence offset exceeds platform range"))?;
        let blob = tx.blob_open("main", "records", "body", id, true)?;
        if blob.len() as u64 != size {
            return Err(error(
                "storage.corrupt",
                "record length changed during inspection",
            ));
        }
        let length = (size - offset as u64).min(u64::from(max_bytes)) as usize;
        let mut part = vec![0; length];
        blob.read_at_exact(&mut part, start)?;
        blob.close()?;
        let next: Option<i64> = tx.query_row(
            "SELECT min(ordinal) FROM records WHERE ordinal>?1 AND ordinal<=?2",
            params![id, cutoff],
            |row| row.get(0),
        )?;
        let fully_inspected = offset == 0 && part.len() as u64 == size;
        let length_valid = stored.and_then(|n| u64::try_from(n).ok()) == Some(size);
        let json_valid = fully_inspected.then(|| serde_json::from_slice::<Value>(&part).is_ok());
        let fault = if !length_valid {
            Some("stored record length differs from its bytes")
        } else if json_valid == Some(false) {
            Some("stored journal JSON is invalid")
        } else {
            None
        };
        let answer = json!({"transferId":transfer_id,"ordinal":id.to_string(),"nextOrdinal":next.map(|n| n.to_string()),"offset":offset.to_string(),"length":size.to_string(),"storedLength":stored.map(|n| n.to_string()),"bytes":part,"complete":(offset as u64)+(part.len() as u64)==size,"lengthValid":length_valid,"jsonValidated":fully_inspected,"jsonValid":json_valid,"fault":fault.map(|detail| json!({"kind":"storage.corrupt","detail":detail}))});
        tx.commit()?;
        Ok(answer)
    }

    /// Retire exactly the claimed page after the caller has safely accepted it.
    /// The durable receipt describes only journal state, not destination storage.
    pub fn acknowledge_evidence_transfer(&self, transfer_id: &str, token: &str) -> Result<Value> {
        self.finish_evidence_transfer(transfer_id, token, true)
            .map_err(|e| e.at("acknowledge_evidence_transfer"))
    }
    /// Release a claim without advancing or releasing the snapshot's pinned rows.
    pub fn rollback_evidence_transfer(&self, transfer_id: &str, token: &str) -> Result<Value> {
        self.finish_evidence_transfer(transfer_id, token, false)
            .map_err(|e| e.at("rollback_evidence_transfer"))
    }
    fn finish_evidence_transfer(&self, transfer_id: &str, token: &str, ack: bool) -> Result<Value> {
        if !valid_id(transfer_id) || token.is_empty() || token.len() > 96 {
            return Err(error("argument.invalid", "invalid evidence claim identity"));
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, cutoff, pending, last, more, last_ack, receipt, rollback): (Option<String>,i64,Option<String>,Option<i64>,Option<bool>,Option<String>,Option<String>,Option<String>) = tx.query_row("SELECT transfer_id,cutoff,pending_token,pending_last,pending_more,last_ack_token,last_ack_receipt,last_rollback_token FROM evidence_cursor WHERE id=1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))?;
        if current.as_deref() != Some(transfer_id) {
            return Err(error("argument.invalid", "unknown evidence transfer"));
        }
        if ack && last_ack.as_deref() == Some(token) {
            let answer = decode(
                &receipt.ok_or_else(|| error("storage.corrupt", "evidence receipt missing"))?,
            )?;
            tx.commit()?;
            return Ok(answer);
        }
        if !ack && rollback.as_deref() == Some(token) {
            tx.commit()?;
            return Ok(
                json!({"transferId":transfer_id,"token":token,"rolledBack":true,"durable":true}),
            );
        }
        if pending.as_deref() != Some(token) {
            return Err(error(
                "argument.invalid",
                "evidence token is unknown or stale",
            ));
        }
        if !ack {
            // Legacy claims without a copy can be released, but never delete a
            // saved copy unless its live counterpart and identity still match.
            let has_copy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM evidence_claim_header) OR EXISTS(SELECT 1 FROM evidence_claim_records)", [], |r| r.get(0))?;
            if has_copy {
                let progressed: i64 = tx.query_row(
                    "SELECT progressed FROM evidence_cursor WHERE id=1",
                    [],
                    |r| r.get(0),
                )?;
                verify_evidence_claim(
                    &tx,
                    progressed,
                    last.ok_or_else(|| error("storage.corrupt", "evidence cursor is incomplete"))?,
                )?;
            }
            tx.execute("UPDATE evidence_cursor SET pending_token=NULL,pending_last=NULL,pending_more=NULL,pending_count=NULL,last_rollback_token=?1 WHERE id=1", [token])?;
            tx.execute("DELETE FROM evidence_claim_records", [])?;
            tx.execute("DELETE FROM evidence_claim_header", [])?;
            tx.commit()?;
            return Ok(
                json!({"transferId":transfer_id,"token":token,"rolledBack":true,"durable":true}),
            );
        }
        let last = last.ok_or_else(|| error("storage.corrupt", "evidence cursor is incomplete"))?;
        let more = more.ok_or_else(|| error("storage.corrupt", "evidence cursor is incomplete"))?;
        let progressed: i64 = tx.query_row(
            "SELECT progressed FROM evidence_cursor WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        verify_evidence_members(&tx)?;
        verify_evidence_claim(&tx, progressed, last)?;
        let removed_bytes: i64 = tx.query_row(
            "SELECT coalesce(sum(bytes),0) FROM records WHERE recording_acked=1 AND ordinal<=?1",
            [last],
            |r| r.get(0),
        )?;
        let removed = tx.execute(
            "DELETE FROM records WHERE recording_acked=1 AND ordinal<=?1",
            [last],
        )?;
        tx.execute("UPDATE journal SET retained_records=retained_records-?1,retained_bytes=retained_bytes-?2 WHERE id=1", params![sql_integer(removed as u64)?,removed_bytes])?;
        let answer =
            json!({"transferId":transfer_id,"token":token,"acknowledged":true,"durable":true});
        tx.execute("DELETE FROM evidence_members WHERE ordinal<=?1", [last])?;
        tx.execute("UPDATE evidence_members_header SET progressed=?1,count=(SELECT count(*) FROM evidence_members) WHERE id=1", [if more {last} else {cutoff}])?;
        tx.execute("UPDATE evidence_cursor SET progressed=?1,pending_token=NULL,pending_last=NULL,pending_more=NULL,pending_count=NULL,last_ack_token=?2,last_ack_receipt=?3 WHERE id=1", params![if more {last} else {cutoff},token,answer.to_string()])?;
        tx.execute("DELETE FROM evidence_claim_records", [])?;
        tx.execute("DELETE FROM evidence_claim_header", [])?;
        tx.commit()?;
        if removed > 0 {
            self.shared.reader_space.notify_one();
        }
        Ok(answer)
    }

    pub(crate) fn reader_space_wake(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.shared.reader_space)
    }

    pub fn stop(&self) -> Result<Value> {
        if self.uses_released_v1_schema() {
            return self.stop_v1().map_err(|failure| failure.at("stop"));
        }
        self.stop_inner().map_err(|failure| failure.at("stop"))
    }

    fn stop_v1(&self) -> Result<Value> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE journal SET phase='stopped' WHERE id=1", [])?;
        tx.commit()?;
        self.status_v1()
    }
    fn stop_inner(&self) -> Result<Value> {
        {
            let mut connection = self.connection()?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            reject_live_ordered_owner(&tx)?;
            tx.execute("UPDATE journal SET phase='stopped' WHERE id=1", [])?;
            tx.commit()?;
        }
        self.status()
    }

    /// Explicit destructive operation, never invoked by radio release or Drop.
    /// Sequence counters survive clear so old tokens cannot alias future batches.
    pub fn clear(&self) -> Result<Value> {
        if self.uses_released_v1_schema() {
            return self.clear_v1().map_err(|failure| failure.at("clear"));
        }
        self.clear_inner().map_err(|failure| failure.at("clear"))
    }

    fn clear_v1(&self) -> Result<Value> {
        let _spool_gate = self.shared.apple_failure_spool_gate.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        self.reject_unacknowledged_apple_failure_spool()?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let phase: String =
            tx.query_row("SELECT phase FROM journal WHERE id=1", [], |row| row.get(0))?;
        if phase != "stopped" {
            return Err(error(
                "storage.busy",
                "stop recording before clearing retained records",
            ));
        }
        let pending: bool = tx.query_row(
            "SELECT pending_token IS NOT NULL OR pending_last IS NOT NULL OR pending_more IS NOT NULL FROM journal WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        if pending {
            return Err(error(
                "storage.busy",
                "prepared records require acknowledgement before clearing",
            ));
        }
        let removed = tx.execute("DELETE FROM records", [])?;
        tx.execute("UPDATE journal SET last_ack_token=NULL,last_ack_receipt=NULL,lost=0,retained_records=0,retained_bytes=0 WHERE id=1", [])?;
        tx.commit()?;
        Ok(json!({"cleared":true,"records":removed}))
    }
    fn clear_inner(&self) -> Result<Value> {
        let _spool_gate = self.shared.apple_failure_spool_gate.lock().map_err(|_| {
            error(
                "storage.io",
                "apple notification failure spool was interrupted",
            )
        })?;
        self.reject_unacknowledged_apple_failure_spool()?;
        let mut connection = self.connection()?;
        let ledger = self
            .shared
            .diagnostic_ledger
            .lock()
            .map_err(|_| error("storage.io", "diagnostic ledger interrupted"))?;
        if !ledger.slots.is_empty() {
            return Err(error("storage.busy", "diagnostic identities remain owned"));
        }
        let failures = self
            .shared
            .failures
            .lock()
            .map_err(|_| error("storage.io", "journal failure ownership interrupted"))?;
        for failure in failures.iter() {
            let pending = failure
                .pending_recorded_loss
                .lock()
                .map_err(|_| error("storage.io", "recorded loss state interrupted"))?;
            let occurrences = failure
                .separate_occurrences
                .lock()
                .map_err(|_| error("storage.io", "recorded loss occurrences interrupted"))?;
            if *pending != RecordedLoss::default() || !occurrences.is_empty() {
                return Err(error(
                    "storage.busy",
                    "uncommitted diagnostic loss remains retained",
                ));
            }
        }
        drop(failures);
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let retained: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM diagnostic_ledger)",
            [],
            |row| row.get(0),
        )?;
        if retained {
            return Err(error(
                "storage.busy",
                "durable diagnostic identities remain retained",
            ));
        }
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM diagnostic_owners WHERE state != 'reclaimed')",
            [],
            |row| row.get(0),
        )?;
        if pending {
            return Err(error(
                "storage.busy",
                "durable diagnostic owner remains unresolved",
            ));
        }
        reject_live_ordered_owner(&tx)?;
        let phase: String =
            tx.query_row("SELECT phase FROM journal WHERE id=1", [], |row| row.get(0))?;
        if phase != "stopped" {
            return Err(error(
                "storage.busy",
                "stop recording before clearing retained records",
            ));
        }
        let transfer: Option<String> = tx.query_row(
            "SELECT transfer_id FROM evidence_cursor WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        if transfer.is_some() {
            verify_evidence_members(&tx)?;
        } else {
            reject_orphaned_evidence(&tx)?;
        }
        let orphan_claim: bool = tx.query_row("SELECT pending_token IS NULL AND (pending_last IS NOT NULL OR pending_more IS NOT NULL OR pending_count IS NOT NULL OR EXISTS(SELECT 1 FROM evidence_claim_header) OR EXISTS(SELECT 1 FROM evidence_claim_records)) FROM evidence_cursor WHERE id=1", [], |row| row.get(0))?;
        if orphan_claim {
            return Err(error("storage.corrupt", "orphaned evidence claim retained"));
        }
        let active: bool = tx.query_row(
            "SELECT progressed<cutoff OR pending_token IS NOT NULL FROM evidence_cursor WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        if active {
            return Err(error("storage.busy", "evidence transfer retains records"));
        }
        let removed = tx.execute("DELETE FROM records", [])?;
        tx.execute("DELETE FROM ordered_identities", [])?;
        tx.execute("DELETE FROM ordered_pending", [])?;
        tx.execute("DELETE FROM recording_claim_records", [])?;
        tx.execute("DELETE FROM recording_claim_header", [])?;
        tx.execute("UPDATE journal SET pending_token=NULL,pending_last=NULL,pending_more=NULL,last_ack_token=NULL,last_ack_receipt=NULL,lost=0,retained_records=0,retained_bytes=0 WHERE id=1",[])?;
        tx.commit()?;
        Ok(json!({"cleared":true,"records":removed}))
    }

    pub fn status(&self) -> Result<Value> {
        if self.uses_released_v1_schema() {
            return self.status_v1().map_err(|failure| failure.at("status"));
        }
        self.status_inner().map_err(|failure| failure.at("status"))
    }

    fn status_v1(&self) -> Result<Value> {
        let connection = self.connection()?;
        let (phase, lost, persisted_failure): (String, u64, Option<String>) = connection
            .query_row(
                "SELECT phase,lost,collection_failure FROM journal WHERE id=1",
                [],
                |row| Ok((row.get(0)?, unsigned(row, 1)?, row.get(2)?)),
            )?;
        let (records, bytes): (u64, u64) = connection.query_row(
            "SELECT retained_records,retained_bytes FROM journal WHERE id=1",
            [],
            |row| Ok((unsigned(row, 0)?, unsigned(row, 1)?)),
        )?;
        let collection_failure = persisted_failure
            .as_deref()
            .map(decode)
            .transpose()?
            .filter(valid_summary);
        Ok(json!({
            "recordingId":self.recording_id,
            "phase":phase,
            "accepting":phase=="recording" && collection_failure.is_none(),
            "records":records,
            "bytes":bytes,
            "lostRecords":lost,
            "maxBytes":self.quota.max_bytes,
            "maxRecords":self.quota.max_records,
            "encrypted":false,
            "collectionFailure":collection_failure,
            "runtimeFailure":Value::Null,
        }))
    }
    fn status_inner(&self) -> Result<Value> {
        let connection = self.connection()?;
        let (phase, lost, persisted_failure, summary_bytes): (String, u64, Option<Vec<u8>>, Option<i64>) = connection
            .query_row(
                "SELECT phase,lost,CASE WHEN octet_length(collection_failure)<=?1 THEN CAST(collection_failure AS BLOB) END,octet_length(collection_failure) FROM journal WHERE id=1",
                [DIAGNOSTIC_PAGE_LIMIT],
                |row| Ok((row.get(0)?, unsigned(row, 1)?, row.get(2)?, row.get(3)?)),
            )?;
        let (records, bytes): (u64, u64) = connection.query_row(
            "SELECT retained_records,retained_bytes FROM journal WHERE id=1",
            [],
            |row| Ok((unsigned(row, 0)?, unsigned(row, 1)?)),
        )?;
        let ordered_owner = match connection.query_row("SELECT producer_id,collection_id,generation_kind,generation_value,state FROM ordered_owner WHERE id=1", [], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,String>(4)?))) {
            Ok((producer, collection, kind, generation, state)) => Some(json!({"version":1,"producerIncarnation":producer,"collectionId":collection,"sourceGeneration":kind.zip(generation).map(|(kind,value)|json!({"kind":kind,"value":value})),"uncertainTail":if state == "interrupted" { Some(true) } else { None },"state":state})),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(failure) => return Err(failure.into()),
        };
        let runtime_failure = self
            .failures
            .runtime_failure
            .lock()
            .map_err(|_| error("storage.io", "journal diagnostic lock interrupted"))?
            .clone();
        let failures = self
            .shared
            .failures
            .lock()
            .map_err(|_| error("storage.io", "journal failure ownership interrupted"))?;
        let mut local_report = None;
        let mut peer_report = None;
        let mut pending = RecordedLoss::default();
        for failure in failures.iter() {
            let report = failure
                .collection_failure
                .lock()
                .map_err(|_| error("storage.io", "collection failure state interrupted"))?;
            if report
                .as_ref()
                .is_some_and(|value| value["persisted"] == false)
            {
                if Arc::ptr_eq(failure, &self.failures) {
                    local_report = report.clone();
                } else if peer_report.is_none() {
                    peer_report = report.clone();
                }
            }
            pending = pending.add(
                *failure
                    .pending_recorded_loss
                    .lock()
                    .map_err(|_| error("storage.io", "recorded loss state interrupted"))?,
            );
            let occurrences = failure
                .separate_occurrences
                .lock()
                .map_err(|_| error("storage.io", "recorded loss occurrences interrupted"))?;
            // An error returned by COMMIT does not prove rollback. Compare the
            // exact receipt before counting the retained producer attempt. Keep
            // its owner until retry also resolves the producer's failure state.
            for entry in occurrences.iter() {
                let payload = diagnostic_payload(entry);
                let matches: Option<bool> = connection.query_row(
                    "SELECT CAST(payload AS BLOB)=?2 FROM diagnostic_ledger WHERE occurrence=?1",
                    params![entry.id.as_slice(), payload.as_bytes()], |row| row.get(0)).optional()?;
                match matches {
                    Some(true) => {}
                    Some(false) => {
                        return Err(error("storage.identity", "loss occurrence payload differs"));
                    }
                    None => {
                        pending = pending.add(entry.loss);
                    }
                }
            }
        }
        let mut ledger = self
            .shared
            .diagnostic_ledger
            .lock()
            .map_err(|_| error("storage.io", "diagnostic ledger interrupted"))?;
        // A failed COMMIT can be ambiguous. Resolve the exact occurrence before
        // combining durable and pending loss, and reject conflicting identity.
        for entry in ledger
            .slots
            .iter_mut()
            .filter_map(|slot| slot.entry.as_mut())
            .filter(|entry| !entry.durable)
        {
            let stored: Option<(u64, Option<String>)> = connection.query_row("SELECT octet_length(payload),CASE WHEN octet_length(payload)<=?2 THEN payload END FROM diagnostic_ledger WHERE occurrence=?1", params![entry.id.as_slice(), DIAGNOSTIC_RECEIPT_LIMIT as i64], |row| Ok((unsigned(row, 0)?, row.get(1)?))).optional()?;
            if let Some((_, stored)) = stored {
                if stored.as_deref() != Some(diagnostic_payload(entry).as_str()) {
                    return Err(error(
                        "storage.identity",
                        "diagnostic occurrence payload differs",
                    ));
                }
                entry.durable = true;
                entry.persistence_failure = None;
            }
        }
        let ledger_runtime_report = ledger
            .slots
            .iter()
            .filter_map(|slot| slot.entry.as_ref())
            .find(|entry| entry.category == DiagnosticCategory::Runtime && !entry.durable)
            .map(|entry| {
                let mut report = diagnostic(&entry.failure);
                report["persisted"] = json!(false);
                report
            });
        let mut durable_runtime_report = None;
        {
            let mut statement = connection.prepare("SELECT octet_length(payload),CASE WHEN octet_length(payload)<=?1 THEN CAST(payload AS BLOB) END FROM diagnostic_ledger WHERE acknowledged=0 ORDER BY sequence")?;
            let mut rows = statement.query([DIAGNOSTIC_PAGE_LIMIT])?;
            while let Some(row) = rows.next()? {
                let size = unsigned(row, 0)?;
                let Some(payload): Option<Vec<u8>> = row.get(1)? else {
                    // The category itself is inside the oversized payload and is
                    // deliberately not parsed or guessed by ordinary status.
                    durable_runtime_report = Some(oversized_diagnostic_report(size, "occurrence"));
                    break;
                };
                let value = std::str::from_utf8(&payload)
                    .ok()
                    .and_then(|text| decode(text).ok());
                let Some(value) = value else {
                    durable_runtime_report = Some(unreadable_diagnostic_report(size, "occurrence"));
                    break;
                };
                match value.get("category").and_then(Value::as_str) {
                    Some("runtime") if valid_occurrence(&value, &payload) => {
                        let Some(mut report) = value
                            .get("failure")
                            .filter(|failure| valid_failure(failure))
                            .cloned()
                        else {
                            durable_runtime_report =
                                Some(unreadable_diagnostic_report(size, "occurrence"));
                            break;
                        };
                        report["persisted"] = json!(true);
                        durable_runtime_report = Some(report);
                        break;
                    }
                    Some("collection") if valid_occurrence(&value, &payload) => {}
                    Some("legacy")
                        if value.as_object().is_some_and(|object| {
                            object.len() == 2 && object.get("report").is_some_and(Value::is_string)
                        }) => {}
                    _ => {
                        durable_runtime_report =
                            Some(unreadable_diagnostic_report(size, "occurrence"));
                        break;
                    }
                }
            }
        }
        let mut ledger_report = None;
        for entry in ledger
            .slots
            .iter()
            .filter_map(|slot| slot.entry.as_ref())
            .filter(|entry| entry.category == DiagnosticCategory::Collection && !entry.durable)
        {
            pending = pending.add(entry.loss);
            if ledger_report.is_none() {
                let mut report = diagnostic(&entry.failure);
                report["persisted"] = json!(false);
                if let Some(failure) = &entry.persistence_failure {
                    report["persistenceFailure"] = diagnostic(failure);
                }
                ledger_report = Some(report);
            }
        }
        let summary_bytes = summary_bytes
            .map(|size| {
                u64::try_from(size)
                    .map_err(|_| error("storage.corrupt", "invalid diagnostic summary length"))
            })
            .transpose()?;
        let summary_oversized =
            summary_bytes.is_some_and(|size| size > u64::from(DIAGNOSTIC_PAGE_LIMIT));
        let persisted_failure = if summary_oversized {
            summary_bytes.map(|size| oversized_diagnostic_report(size, "collectionSummary"))
        } else if let Some(bytes) = persisted_failure {
            match std::str::from_utf8(&bytes)
                .ok()
                .and_then(|value| decode(value).ok())
                .filter(valid_summary)
            {
                Some(value) => Some(value),
                None => Some(unreadable_summary_report(bytes.len() as u64)),
            }
        } else {
            None
        };
        let summary_unavailable = summary_oversized
            || persisted_failure
                .as_ref()
                .is_some_and(|value| value["kind"] == "storage.diagnostic-unreadable");
        let separate: Option<(u64, Option<i64>, String)> = connection
            .query_row(
                "SELECT items,bytes,failure FROM diagnostic_separate_loss WHERE id=1",
                [],
                |row| Ok((unsigned(row, 0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let separate = separate
            .map(|(items, bytes, failure)| -> Result<_> {
                let bytes = bytes
                    .map(|value| {
                        u64::try_from(value)
                            .map_err(|_| error("storage.corrupt", "invalid separate loss bytes"))
                    })
                    .transpose()?;
                let failure = decode(&failure)?;
                if !valid_failure(&failure) {
                    return Err(error("storage.corrupt", "invalid separate loss failure"));
                }
                Ok((RecordedLoss { items, bytes }, failure))
            })
            .transpose()?;
        let volatile_pending = pending;
        if let Some((loss, _)) = &separate {
            pending = pending.add(*loss);
        }
        if separate.is_some() && !summary_unavailable {
            return Err(error(
                "storage.corrupt",
                "separate loss requires its retained historical summary",
            ));
        }
        let separate_report = separate.as_ref().map(|(_, failure)| {
            let mut report = failure.clone();
            report["persisted"] = json!(true);
            report
        });
        let collection_failure = if let Some(mut report) = local_report
            .or(peer_report)
            .or(ledger_report)
            .or(separate_report)
        {
            let durable_loss = RecordedLoss::from_report(persisted_failure.as_ref());
            if summary_unavailable {
                if let Some(object) = report.as_object_mut() {
                    object.remove("knownRecordedLoss");
                }
                report["retainedSummary"] = persisted_failure.clone().unwrap_or(Value::Null);
                if pending != RecordedLoss::default() {
                    let mut loss = pending.value();
                    loss["persisted"] =
                        json!(volatile_pending == RecordedLoss::default() && separate.is_some());
                    report["pendingRecordedLoss"] = loss;
                }
            } else if pending != RecordedLoss::default()
                || persisted_failure
                    .as_ref()
                    .is_some_and(|value| value.get("knownRecordedLoss").is_some())
            {
                report["knownRecordedLoss"] = durable_loss.add(pending).value();
            }
            Some(report)
        } else {
            persisted_failure
        };
        let mut result = json!({"recordingId":self.recording_id,"phase":phase,"accepting":phase=="recording" && collection_failure.is_none(),"records":records,"bytes":bytes,"lostRecords":lost,"maxBytes":self.quota.max_bytes,"maxRecords":self.quota.max_records,"encrypted":false,"collectionFailure":collection_failure,"runtimeFailure":runtime_failure.as_ref().map(|failure|{let mut result=diagnostic(failure);result["persisted"]=json!(false);result}).or(ledger_runtime_report).or(durable_runtime_report)});
        if let Some(owner) = ordered_owner {
            result["orderedOwner"] = owner;
        }
        Ok(result)
    }
}

fn oversized_diagnostic_report(size: u64, source: &str) -> Value {
    json!({"kind":"storage.diagnostic-oversized", "detail":"retained diagnostic exceeds status projection limit", "operation":"status", "persisted":true, "payloadBytes":size, "source":source})
}

fn unreadable_summary_report(size: u64) -> Value {
    unreadable_diagnostic_report(size, "collectionSummary")
}

fn unreadable_diagnostic_report(size: u64, source: &str) -> Value {
    json!({"kind":"storage.diagnostic-unreadable", "detail":"retained diagnostic is unreadable", "operation":"status", "persisted":true, "payloadBytes":size, "source":source})
}

const MAX_PUBLIC_INTEGER: u64 = 9_007_199_254_740_991;

#[allow(
    clippy::collapsible_if,
    reason = "Rust 2021 cannot express this validated optional field check as Clippy's suggested let-chain."
)]
fn valid_failure(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let fields = [
        "kind",
        "detail",
        "operation",
        "sqliteExtendedCode",
        "sqliteCode",
    ];
    if object.len() != fields.len() || fields.iter().any(|key| !object.contains_key(*key)) {
        return false;
    }
    let mut bytes = 0usize;
    for key in ["kind", "detail", "operation"] {
        let Some(text) = object
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        else {
            return false;
        };
        bytes = bytes.saturating_add(text.len());
    }
    match object.get("sqliteCode") {
        Some(Value::Null) => {}
        Some(Value::String(text)) if !text.is_empty() => bytes = bytes.saturating_add(text.len()),
        _ => return false,
    }
    bytes <= DIAGNOSTIC_PAGE_LIMIT as usize
        && object.get("sqliteExtendedCode").is_some_and(|value| {
            value.is_null()
                || value
                    .as_i64()
                    .is_some_and(|number| i32::try_from(number).is_ok())
        })
}

fn valid_occurrence(value: &Value, payload: &[u8]) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == 3
            && object.contains_key("category")
            && object.get("failure").is_some_and(valid_failure)
            && object
                .get("loss")
                .and_then(Value::as_object)
                .is_some_and(|loss| {
                    loss.len() == 2 && loss.contains_key("items") && loss.contains_key("bytes")
                })
    }) && std::str::from_utf8(payload)
        .ok()
        .is_some_and(|text| validate_diagnostic_payload(text).is_ok())
}

fn valid_summary(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let allowed = [
        "kind",
        "detail",
        "operation",
        "sqliteExtendedCode",
        "sqliteCode",
        "persisted",
        "persistenceFailure",
        "knownRecordedLoss",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return false;
    }
    let mut failure = object.clone();
    for key in ["persisted", "persistenceFailure", "knownRecordedLoss"] {
        failure.remove(key);
    }
    if !valid_failure(&Value::Object(failure))
        || matches!(
            object.get("kind").and_then(Value::as_str),
            Some("storage.diagnostic-unreadable" | "storage.diagnostic-oversized")
        )
    {
        return false;
    }
    let Some(persisted) = object.get("persisted").and_then(Value::as_bool) else {
        return false;
    };
    if object
        .get("persistenceFailure")
        .is_some_and(|nested| persisted || !valid_failure(nested))
    {
        return false;
    }
    if let Some(loss) = object.get("knownRecordedLoss") {
        let Some(loss) = loss.as_object() else {
            return false;
        };
        if loss.keys().any(|key| key != "items" && key != "bytes")
            || !loss
                .get("items")
                .and_then(Value::as_u64)
                .is_some_and(|value| value <= MAX_PUBLIC_INTEGER)
            || loss.get("bytes").is_some_and(|value| {
                !value
                    .as_u64()
                    .is_some_and(|value| value <= MAX_PUBLIC_INTEGER)
            })
        {
            return false;
        }
    }
    true
}

// Copy inside the same transaction as the mutation. SQLite copies the value
// without interpreting its TEXT encoding; failure rolls back the loss update.
fn preserve_summary_before_update(tx: &Connection) -> Result<()> {
    tx.execute("INSERT OR IGNORE INTO diagnostic_summary_original SELECT 1,CAST(collection_failure AS BLOB) FROM journal WHERE id=1 AND collection_failure IS NOT NULL AND NOT EXISTS(SELECT 1 FROM diagnostic_summary_original WHERE id=1)", [])?;
    Ok(())
}

fn summary_for_update(tx: &Connection) -> Result<Option<Value>> {
    let (size, bytes): (Option<i64>, Option<Vec<u8>>) = tx.query_row(
        "SELECT octet_length(collection_failure),CASE WHEN octet_length(collection_failure)<=?1 THEN CAST(collection_failure AS BLOB) END FROM journal WHERE id=1",
        [DIAGNOSTIC_PAGE_LIMIT], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    match (size, bytes) {
        (None, _) => Ok(None),
        (Some(_), None) => Err(error(
            "storage.full",
            "diagnostic summary exceeds loss update limit; retained",
        )),
        (Some(_), Some(bytes)) => {
            let text = std::str::from_utf8(&bytes).map_err(|_| {
                error(
                    "storage.corrupt",
                    "diagnostic summary is not UTF-8; retained",
                )
            })?;
            let value = decode(text)?;
            if !valid_summary(&value) {
                return Err(error(
                    "storage.corrupt",
                    "diagnostic summary has an invalid shape; retained",
                ));
            }
            Ok(Some(value))
        }
    }
}

// This test does not decode or rewrite the historical value for persistence.
// Invalid and oversized summaries remain available through the byte reader.
fn summary_needs_separate_loss(tx: &Connection) -> Result<bool> {
    let (size, bytes): (Option<i64>, Option<Vec<u8>>) = tx.query_row(
        "SELECT octet_length(collection_failure),CASE WHEN octet_length(collection_failure)<=?1 THEN CAST(collection_failure AS BLOB) END FROM journal WHERE id=1",
        [DIAGNOSTIC_PAGE_LIMIT], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(match (size, bytes) {
        (None, _) => false,
        (Some(_), Some(bytes)) => !std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| decode(text).ok())
            .is_some_and(|value| valid_summary(&value)),
        (Some(_), None) => true,
    })
}

fn add_separate_loss(tx: &Connection, failure: &Value, loss: RecordedLoss) -> Result<()> {
    let previous: Option<(u64, Option<u64>)> = tx
        .query_row(
            "SELECT items,bytes FROM diagnostic_separate_loss WHERE id=1",
            [],
            |row| {
                Ok((
                    unsigned(row, 0)?,
                    row.get::<_, Option<i64>>(1)?
                        .map(|value| {
                            u64::try_from(value)
                                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, value))
                        })
                        .transpose()?,
                ))
            },
        )
        .optional()?;
    let total = previous.map_or(loss, |(items, bytes)| {
        RecordedLoss { items, bytes }.add(loss)
    });
    let items = sql_integer(total.items)?;
    let bytes = total.bytes.map(sql_integer).transpose()?;
    tx.execute("INSERT INTO diagnostic_separate_loss VALUES(1,?1,?2,?3) ON CONFLICT(id) DO UPDATE SET items=excluded.items,bytes=excluded.bytes", params![items, bytes, failure.to_string()])?;
    tx.execute("UPDATE journal SET phase='stopped' WHERE id=1", [])?;
    Ok(())
}

// Only an exact untouched synthetic row is removed. Read both SQLite values
// in fixed windows and serialize each valid UTF-8 segment with serde's JSON
// string escaping, avoiding a complete copy of either historical value.
fn synthetic_summary_matches(tx: &Connection) -> Result<bool> {
    let row: Option<i64> = tx.query_row("SELECT sequence FROM diagnostic_ledger WHERE occurrence=zeroblob(40) AND acknowledged=0 AND retired=0 AND (SELECT collection_failure IS NOT NULL FROM journal WHERE id=1)", [], |row| row.get(0)).optional()?;
    let Some(row) = row else {
        return Ok(false);
    };
    let original = tx.blob_open("main", "journal", "collection_failure", 1, true)?;
    let candidate = tx.blob_open("main", "diagnostic_ledger", "payload", row, true)?;
    let mut position = 0usize;
    let mut compare = |bytes: &[u8]| -> Result<bool> {
        let Some(end) = position.checked_add(bytes.len()) else {
            return Ok(false);
        };
        if end > candidate.len() {
            return Ok(false);
        }
        let mut actual = vec![0; bytes.len()];
        candidate.read_at_exact(&mut actual, position)?;
        position = end;
        Ok(actual == bytes)
    };
    if !compare(b"{\"category\":\"legacy\",\"report\":\"")? {
        return Ok(false);
    }
    let mut offset = 0usize;
    let mut carry = Vec::new();
    while offset < original.len() {
        let count = (original.len() - offset).min(8192);
        let start = carry.len();
        carry.resize(start + count, 0);
        original.read_at_exact(&mut carry[start..], offset)?;
        offset += count;
        let valid = match std::str::from_utf8(&carry) {
            Ok(_) => carry.len(),
            Err(failure) if failure.error_len().is_none() && offset < original.len() => {
                failure.valid_up_to()
            }
            Err(_) => return Ok(false),
        };
        let text = std::str::from_utf8(&carry[..valid])
            .map_err(|_| error("storage.corrupt", "invalid summary segment"))?;
        let escaped = serde_json::to_vec(text)
            .map_err(|_| error("storage.corrupt", "cannot compare summary"))?;
        if !compare(&escaped[1..escaped.len() - 1])? {
            return Ok(false);
        }
        carry.drain(..valid);
    }
    if !carry.is_empty() || !compare(b"\"}")? {
        return Ok(false);
    }
    Ok(position == candidate.len())
}

fn diagnostic_db_page(
    connection: &Connection,
    id: DiagnosticId,
    offset: u64,
    max_bytes: u32,
) -> Result<DiagnosticPage> {
    if !(1..=DIAGNOSTIC_PAGE_LIMIT).contains(&max_bytes) {
        return Err(error("argument.invalid", "invalid diagnostic page bounds"));
    }
    let row: Option<(i64, u64, bool)> = connection.query_row(
            "SELECT sequence,octet_length(payload),EXISTS(SELECT 1 FROM diagnostic_owners WHERE occurrence=?1 AND state='reclaimed' AND payload IS NULL) FROM diagnostic_ledger WHERE occurrence=?1",
            [id.as_slice()], |row| Ok((row.get(0)?, unsigned(row, 1)?, row.get(2)?))).optional()?;
    let (table, rowid, total_bytes, reclaimed) = if let Some((rowid, size, reclaimed)) = row {
        ("diagnostic_ledger", rowid, size, reclaimed)
    } else {
        let receipt: Option<(i64, u64, bool)> = connection.query_row("SELECT rowid,octet_length(payload),state='reclaimed' FROM diagnostic_owners WHERE occurrence=?1 AND payload IS NOT NULL", [id.as_slice()], |row| Ok((row.get(0)?, unsigned(row, 1)?, row.get(2)?))).optional()?;
        let (rowid, size, reclaimed) = receipt
            .ok_or_else(|| error("storage.identity", "diagnostic occurrence is not retained"))?;
        ("diagnostic_owners", rowid, size, reclaimed)
    };
    if offset > total_bytes {
        return Err(error(
            "argument.invalid",
            "diagnostic offset exceeds payload",
        ));
    }
    let start = usize::try_from(offset).map_err(|_| {
        error(
            "argument.invalid",
            "diagnostic offset exceeds platform range",
        )
    })?;
    let blob = connection.blob_open("main", table, "payload", rowid, true)?;
    if blob.len() as u64 != total_bytes {
        return Err(error(
            "storage.corrupt",
            "diagnostic payload length changed",
        ));
    }
    let mut bytes = vec![0; (total_bytes - offset).min(u64::from(max_bytes)) as usize];
    blob.read_at_exact(&mut bytes, start)?;
    blob.close()?;
    Ok(DiagnosticPage {
        bytes,
        total_bytes,
        reclaimed,
        durable: true,
    })
}

// Serialize directly into a bounded window, without building a JSON Value or
// full occurrence. The key order matches diagnostic_payload's canonical JSON.
struct DiagnosticWindow {
    offset: u64,
    limit: usize,
    position: u64,
    bytes: Vec<u8>,
}
impl Write for DiagnosticWindow {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let end = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("diagnostic length overflow"))?;
        let start = self
            .offset
            .saturating_sub(self.position)
            .min(bytes.len() as u64) as usize;
        let count = (bytes.len() - start).min(self.limit - self.bytes.len());
        self.bytes.extend_from_slice(&bytes[start..start + count]);
        self.position = end;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn diagnostic_local_page(
    entry: &DiagnosticEntry,
    offset: u64,
    max_bytes: u32,
) -> Result<DiagnosticPage> {
    if !(1..=DIAGNOSTIC_PAGE_LIMIT).contains(&max_bytes) {
        return Err(error("argument.invalid", "invalid diagnostic page bounds"));
    }
    let mut out = DiagnosticWindow {
        offset,
        limit: max_bytes as usize,
        position: 0,
        bytes: Vec::new(),
    };
    let write = (|| -> std::io::Result<()> {
        out.write_all(b"{\"category\":")?;
        serde_json::to_writer(
            &mut out,
            match entry.category {
                DiagnosticCategory::Collection => "collection",
                DiagnosticCategory::Runtime => "runtime",
            },
        )?;
        out.write_all(b",\"failure\":{\"detail\":")?;
        serde_json::to_writer(&mut out, entry.failure.detail)?;
        out.write_all(b",\"kind\":")?;
        serde_json::to_writer(&mut out, entry.failure.kind)?;
        out.write_all(b",\"operation\":")?;
        serde_json::to_writer(&mut out, entry.failure.operation)?;
        out.write_all(b",\"sqliteCode\":")?;
        serde_json::to_writer(&mut out, &entry.failure.sqlite_code)?;
        out.write_all(b",\"sqliteExtendedCode\":")?;
        serde_json::to_writer(&mut out, &entry.failure.sqlite_extended_code)?;
        out.write_all(b"},\"loss\":{\"bytes\":")?;
        serde_json::to_writer(&mut out, &entry.loss.bytes)?;
        out.write_all(b",\"items\":")?;
        serde_json::to_writer(&mut out, &entry.loss.items)?;
        out.write_all(b"}}")
    })();
    write.map_err(|_| error("storage.io", "diagnostic serialization failed"))?;
    if offset > out.position {
        return Err(error(
            "argument.invalid",
            "diagnostic offset exceeds payload",
        ));
    }
    Ok(DiagnosticPage {
        bytes: out.bytes,
        total_bytes: out.position,
        reclaimed: false,
        durable: false,
    })
}

// A retry compares the complete immutable payload before applying loss. Large
// integers remain exact JSON integers and null byte counts stay distinct.
pub(crate) fn diagnostic_payload(entry: &DiagnosticEntry) -> String {
    json!({"category": match entry.category { DiagnosticCategory::Collection => "collection", DiagnosticCategory::Runtime => "runtime" }, "failure": diagnostic(&entry.failure), "loss": {"items": entry.loss.items, "bytes": entry.loss.bytes} }).to_string()
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Diagnostic persistence remains retained for exact journal replay tests."
    )
)]
fn persist_diagnostic(connection: &mut Connection, entry: &DiagnosticEntry) -> Result<()> {
    let payload = diagnostic_payload(entry);
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(error(
            "storage.schema",
            "diagnostic ledger requires current schema",
        ));
    }
    let owner: Option<String> = tx
        .query_row(
            "SELECT state FROM diagnostic_owners WHERE occurrence=?1",
            [entry.id.as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    if owner.is_some() {
        return Err(error(
            "storage.identity",
            "durable diagnostic owner requires exact replay",
        ));
    }
    persist_diagnostic_tx(&tx, entry.id, &payload)?;
    tx.commit()?;
    Ok(())
}

fn diagnostic_field_bytes<'a>(fields: impl IntoIterator<Item = &'a str>) -> Option<usize> {
    fields
        .into_iter()
        .try_fold(0usize, |total, field| total.checked_add(field.len()))
}

fn validate_diagnostic_payload(
    payload: &str,
) -> Result<(&str, serde_json::Map<String, Value>, RecordedLoss)> {
    let value: Value = serde_json::from_str(payload)
        .map_err(|_| error("storage.identity", "invalid diagnostic payload"))?;
    let category = value
        .get("category")
        .and_then(Value::as_str)
        .filter(|category| *category == "collection" || *category == "runtime")
        .ok_or_else(|| error("storage.identity", "invalid diagnostic category"))?;
    let failure = value
        .get("failure")
        .and_then(Value::as_object)
        .ok_or_else(|| error("storage.identity", "invalid diagnostic failure"))?;
    for field in ["kind", "detail", "operation"] {
        if !failure.get(field).is_some_and(Value::is_string) {
            return Err(error("storage.identity", "invalid diagnostic failure"));
        }
    }
    if !failure
        .get("sqliteExtendedCode")
        .is_some_and(|v| v.is_null() || v.as_i64().is_some_and(|n| i32::try_from(n).is_ok()))
        || !failure
            .get("sqliteCode")
            .is_some_and(|v| v.is_null() || v.is_string())
    {
        return Err(error("storage.identity", "invalid diagnostic failure"));
    }
    let loss = value
        .get("loss")
        .and_then(Value::as_object)
        .ok_or_else(|| error("storage.identity", "invalid diagnostic loss"))?;
    let items = loss
        .get("items")
        .and_then(Value::as_u64)
        .ok_or_else(|| error("storage.identity", "invalid diagnostic loss"))?;
    let bytes = loss
        .get("bytes")
        .ok_or_else(|| error("storage.identity", "invalid diagnostic loss"))?;
    let bytes = if bytes.is_null() {
        None
    } else {
        Some(
            bytes
                .as_u64()
                .ok_or_else(|| error("storage.identity", "invalid diagnostic loss"))?,
        )
    };
    // Own the category because the parsed JSON is local to this function.
    Ok((
        if category == "collection" {
            "collection"
        } else {
            "runtime"
        },
        failure.clone(),
        RecordedLoss { items, bytes },
    ))
}

fn persist_diagnostic_tx(
    tx: &rusqlite::Transaction<'_>,
    id: DiagnosticId,
    payload: &str,
) -> Result<()> {
    let (category, failure, recorded_loss) = validate_diagnostic_payload(payload)?;
    let items = recorded_loss.items;
    let reclaimed: Option<Option<bool>> = tx.query_row(
        "SELECT CASE WHEN payload IS NULL THEN NULL ELSE CAST(payload AS BLOB)=?2 END FROM diagnostic_owners WHERE occurrence=?1 AND state='reclaimed'",
        params![id.as_slice(), payload.as_bytes()], |row| row.get(0)).optional()?;
    if let Some(receipt_matches) = reclaimed {
        let matches = match receipt_matches {
            Some(matches) => matches,
            None => tx
                .query_row(
                    "SELECT CAST(payload AS BLOB)=?2 FROM diagnostic_ledger WHERE occurrence=?1",
                    params![id.as_slice(), payload.as_bytes()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| {
                    error("storage.corrupt", "reclaimed diagnostic payload is missing")
                })?,
        };
        return if matches {
            Err(error(
                "storage.reclaimed",
                "diagnostic occurrence was reclaimed",
            ))
        } else {
            Err(error(
                "storage.identity",
                "diagnostic occurrence payload differs",
            ))
        };
    }
    let existing: Option<bool> = tx
        .query_row(
            "SELECT CAST(payload AS BLOB)=?2 FROM diagnostic_ledger WHERE occurrence=?1",
            params![id.as_slice(), payload.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(matches) = existing {
        if !matches {
            return Err(error(
                "storage.identity",
                "diagnostic occurrence payload differs",
            ));
        }
    } else {
        tx.execute(
            "INSERT INTO diagnostic_ledger(occurrence,payload) VALUES(?1,?2)",
            params![id.as_slice(), payload],
        )?;
        if category == "collection" {
            let separate: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM diagnostic_separate_loss WHERE id=1)",
                [],
                |row| row.get(0),
            )?;
            if separate || summary_needs_separate_loss(tx)? {
                add_separate_loss(tx, &Value::Object(failure), recorded_loss)?;
                return Ok(());
            }
            let previous = summary_for_update(tx)?;
            let prior_loss = RecordedLoss::from_report(previous.as_ref());
            let mut report = previous.unwrap_or(Value::Object(failure));
            report["persisted"] = json!(true);
            report["knownRecordedLoss"] = prior_loss.add(recorded_loss).value();
            preserve_summary_before_update(tx)?;
            tx.execute(
                "UPDATE journal SET phase='stopped',collection_failure=?1 WHERE id=1",
                [report.to_string()],
            )?;
        }
        let increment = i64::try_from(items).unwrap_or(i64::MAX);
        tx.execute("UPDATE journal SET lost=CASE WHEN lost>?1 THEN 9223372036854775807 ELSE lost+?2 END WHERE id=1", params![i64::MAX - increment, increment])?;
    }
    Ok(())
}

fn reject_live_ordered_owner(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let live: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM ordered_owner WHERE id=1 AND state='live')",
        [],
        |row| row.get(0),
    )?;
    if live {
        return Err(error(
            "storage.owner-conflict",
            "ordered producer owns this journal",
        ));
    }
    Ok(())
}

// Loss counters and persistence metadata are independent of the originating
// diagnostic. Never call a distinct local failure durable just because another
// handle persisted its own terminal report.
fn same_failure(left: &Value, right: &Value) -> bool {
    [
        "kind",
        "detail",
        "operation",
        "sqliteExtendedCode",
        "sqliteCode",
    ]
    .iter()
    .all(|key| left.get(key) == right.get(key))
}

fn diagnostic(failure: &JournalError) -> Value {
    json!({"kind":failure.kind,"detail":failure.detail,"operation":failure.operation,"sqliteExtendedCode":failure.sqlite_extended_code,"sqliteCode":failure.sqlite_code})
}

fn reject_legacy_cursors(connection: &Connection, schema: i64) -> Result<()> {
    if schema >= 5 {
        return Ok(());
    }
    let pending: bool = connection.query_row("SELECT pending_token IS NOT NULL OR pending_last IS NOT NULL OR pending_more IS NOT NULL FROM journal WHERE id=1", [], |row| row.get(0))?;
    let evidence: bool = if schema >= 3 {
        connection.query_row("SELECT transfer_id IS NOT NULL OR pending_token IS NOT NULL OR pending_last IS NOT NULL OR pending_more IS NOT NULL OR cutoff!=0 OR progressed!=0 OR EXISTS(SELECT 1 FROM records WHERE recording_acked!=0) FROM evidence_cursor WHERE id=1", [], |row| row.get(0))?
    } else {
        false
    };
    let saved_claim: bool = if schema >= 4 {
        connection.query_row("SELECT EXISTS(SELECT 1 FROM evidence_claim_header) OR EXISTS(SELECT 1 FROM evidence_claim_records)", [], |row| row.get(0))?
    } else {
        false
    };
    if pending
        || evidence
        || saved_claim
        || legacy_exact_state(connection)?.is_some_and(|(_, _, retired)| !retired)
    {
        return Err(error(
            "storage.schema",
            "pre-v5 cursor needs its original reader before migration; file retained",
        ));
    }
    Ok(())
}

fn reject_orphaned_evidence(connection: &Connection) -> Result<()> {
    let orphan: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM evidence_members_header) OR EXISTS(SELECT 1 FROM evidence_members) OR EXISTS(SELECT 1 FROM evidence_claim_header) OR EXISTS(SELECT 1 FROM evidence_claim_records) OR pending_token IS NOT NULL OR pending_last IS NOT NULL OR pending_more IS NOT NULL OR pending_count IS NOT NULL OR cutoff != 0 OR progressed != 0 FROM evidence_cursor WHERE id=1", [], |r| r.get(0))?;
    if orphan {
        return Err(error("storage.corrupt", "orphaned evidence state retained"));
    }
    Ok(())
}

fn verify_recording_claim(connection: &Connection) -> Result<()> {
    let valid: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM recording_claim_header h WHERE h.id=1 AND h.token=j.pending_token AND h.last=j.pending_last AND h.more=j.pending_more AND h.count BETWEEN 1 AND 2048 AND h.count=(SELECT count(*) FROM recording_claim_records) AND h.count=(SELECT count(*) FROM records WHERE recording_acked=0 AND ordinal<=h.last) AND NOT EXISTS(SELECT 1 FROM recording_claim_records c LEFT JOIN records r ON r.ordinal=c.ordinal WHERE r.ordinal IS NULL OR r.recording_acked!=0 OR c.ordinal>h.last OR r.bytes!=c.bytes OR length(c.digest)!=32)) FROM journal j WHERE id=1", [], |r| r.get(0))?;
    if !valid {
        return Err(error(
            "storage.corrupt",
            "recording claim changed or lacks historical identity",
        ));
    }
    let last: i64 = connection.query_row(
        "SELECT last FROM recording_claim_header WHERE id=1",
        [],
        |r| r.get(0),
    )?;
    check_record_lengths(connection, 0, last, true)?;
    let mut statement = connection.prepare("SELECT CAST(r.body AS BLOB),c.digest FROM recording_claim_records c JOIN records r ON r.ordinal=c.ordinal ORDER BY c.ordinal")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let body: Vec<u8> = row.get(0)?;
        let digest: Vec<u8> = row.get(1)?;
        if Sha256::digest(&body).as_slice() != digest.as_slice() {
            return Err(error("storage.corrupt", "recording claim body changed"));
        }
    }
    Ok(())
}

fn verify_evidence_members(connection: &Connection) -> Result<()> {
    let valid: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM evidence_members_header h WHERE h.id=1 AND h.transfer_id=c.transfer_id AND h.cutoff=c.cutoff AND h.progressed=c.progressed AND h.count=(SELECT count(*) FROM evidence_members) AND h.count=(SELECT count(*) FROM records WHERE ordinal>c.progressed AND ordinal<=c.cutoff) AND NOT EXISTS(SELECT 1 FROM evidence_members m LEFT JOIN records r ON r.ordinal=m.ordinal WHERE r.ordinal IS NULL OR m.ordinal<=c.progressed OR m.ordinal>c.cutoff)) FROM evidence_cursor c WHERE id=1", [], |r| r.get(0))?;
    if !valid {
        return Err(error(
            "storage.corrupt",
            "evidence snapshot membership changed or lacks historical identity",
        ));
    }
    Ok(())
}

fn verify_evidence_claim(connection: &Connection, after: i64, last: i64) -> Result<()> {
    check_record_lengths(connection, after, last, false)?;
    let valid: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM evidence_claim_header h WHERE h.id=1 AND h.transfer_id=evidence_cursor.transfer_id AND h.token=evidence_cursor.pending_token AND h.progressed=evidence_cursor.progressed AND h.cutoff=evidence_cursor.cutoff AND h.last=evidence_cursor.pending_last AND h.more=evidence_cursor.pending_more AND h.count=evidence_cursor.pending_count) AND pending_count IS NOT NULL AND pending_count > 0 AND pending_count=(SELECT count(*) FROM evidence_claim_records) AND pending_count=(SELECT count(*) FROM records WHERE ordinal>?1 AND ordinal<=?2) AND NOT EXISTS(SELECT 1 FROM evidence_claim_records c LEFT JOIN records r ON r.ordinal=c.ordinal WHERE r.ordinal IS NULL OR c.ordinal<=?1 OR c.ordinal>?2 OR r.bytes != c.bytes OR CAST(r.body AS BLOB) != c.body) FROM evidence_cursor WHERE id=1",
        params![after,last], |row| row.get(0))?;
    if !valid {
        return Err(error(
            "storage.corrupt",
            "prepared evidence page changed or has no verifiable identity",
        ));
    }
    Ok(())
}

fn check_record_lengths(
    connection: &Connection,
    after: i64,
    last: i64,
    recording: bool,
) -> Result<()> {
    let mut statement = connection.prepare("SELECT bytes,octet_length(body) FROM records WHERE ordinal>?1 AND ordinal<=?2 AND (?3=0 OR recording_acked=0) ORDER BY ordinal LIMIT 2049")?;
    let mut rows = statement.query(params![after, last, recording])?;
    let (mut count, mut total) = (0usize, 0u64);
    while let Some(row) = rows.next()? {
        let stored = unsigned(row, 0)?;
        let actual = unsigned(row, 1)?;
        count += 1;
        total = total
            .checked_add(actual)
            .ok_or_else(|| error("storage.corrupt", "record size overflow"))?;
        if stored != actual || count > 2048 || total > 4_194_304 {
            return Err(error(
                "storage.corrupt",
                "record page violates actual byte bounds",
            ));
        }
    }
    Ok(())
}

fn check_requested_bounds(
    connection: &Connection,
    after: i64,
    last: i64,
    recording: bool,
    max_items: u32,
    max_bytes: u32,
) -> Result<()> {
    let (count, bytes): (u64,u64) = connection.query_row("SELECT count(*),coalesce(sum(octet_length(body)),0) FROM records WHERE ordinal>?1 AND ordinal<=?2 AND (?3=0 OR recording_acked=0)", params![after,last,recording], |row| Ok((unsigned(row,0)?,unsigned(row,1)?)))?;
    if count > u64::from(max_items) || bytes > u64::from(max_bytes) {
        return Err(error(
            "argument.invalid",
            "requested bounds cannot fit the prepared page",
        ));
    }
    Ok(())
}

fn evidence_page(
    connection: &Connection,
    transfer_id: &str,
    token: &str,
    after: i64,
    last: i64,
    more: bool,
) -> Result<Value> {
    check_record_lengths(connection, after, last, false)?;
    let mut statement = connection.prepare(
        "SELECT ordinal,CAST(body AS BLOB),bytes FROM records WHERE ordinal>?1 AND ordinal<=?2 ORDER BY ordinal",
    )?;
    let mut rows = statement.query(params![after, last])?;
    let mut records = Vec::new();
    let mut ordinals = Vec::new();
    let mut bodies = Vec::new();
    let mut bytes = 0u64;
    while let Some(row) = rows.next()? {
        let ordinal: i64 = row.get(0)?;
        let raw: Vec<u8> = row.get(1)?;
        let body = String::from_utf8(raw)
            .map_err(|_| error("storage.corrupt", "stored journal JSON is not UTF-8"))?;
        let size = unsigned(row, 2)?;
        bytes = bytes
            .checked_add(size)
            .ok_or_else(|| error("storage.corrupt", "evidence page size overflow"))?;
        if records.len() >= 2048 || bytes > 4_194_304 || size != body.len() as u64 {
            return Err(error(
                "storage.corrupt",
                "evidence page violates journal bounds",
            ));
        }
        records.push(decode(&body)?);
        ordinals.push(ordinal.to_string());
        bodies.push(body);
    }
    if records.is_empty() {
        return Err(error("storage.corrupt", "evidence page is missing"));
    }
    Ok(
        json!({"transferId":transfer_id,"token":token,"records":records,"ordinals":ordinals,"recordBodies":bodies,"bytes":bytes,"more":more,"durable":true}),
    )
}

fn prepared(connection: &Connection, token: &str, last: i64, more: bool) -> Result<Value> {
    check_record_lengths(connection, 0, last, true)?;
    let mut statement =
        connection.prepare("SELECT CAST(body AS BLOB),bytes FROM records WHERE recording_acked=0 AND ordinal<=?1 ORDER BY ordinal")?;
    let mut rows = statement.query([last])?;
    let mut records = Vec::new();
    let mut bytes = 0u64;
    while let Some(row) = rows.next()? {
        let raw: Vec<u8> = row.get(0)?;
        let body = String::from_utf8(raw)
            .map_err(|_| error("storage.corrupt", "stored journal JSON is not UTF-8"))?;
        let size = unsigned(row, 1)?;
        bytes = bytes
            .checked_add(size)
            .ok_or_else(|| error("storage.corrupt", "prepared size overflow"))?;
        if records.len() >= 2048 || bytes > 4_194_304 || size != body.len() as u64 {
            return Err(error(
                "storage.corrupt",
                "prepared records violate journal bounds",
            ));
        }
        records.push(decode(&body)?);
    }
    if records.is_empty() {
        return Err(error("storage.corrupt", "prepared prefix is missing"));
    }
    Ok(json!({"token":token,"records":records,"bytes":bytes,"more":more}))
}

// The v1 table has no recording_acked marker or claim tables.  Its original
// cursor owns the prefix until acknowledgement, so keep this reader deliberately
// limited to that released shape instead of adding an extension column.
fn prepared_v1(connection: &Connection, token: &str, last: i64, more: bool) -> Result<Value> {
    let mut statement = connection.prepare(
        "SELECT CAST(body AS BLOB),bytes FROM records WHERE ordinal<=?1 ORDER BY ordinal",
    )?;
    let mut rows = statement.query([last])?;
    let mut records = Vec::new();
    let mut bytes = 0u64;
    while let Some(row) = rows.next()? {
        let raw: Vec<u8> = row.get(0)?;
        let body = String::from_utf8(raw)
            .map_err(|_| error("storage.corrupt", "stored journal JSON is not UTF-8"))?;
        let size = unsigned(row, 1)?;
        bytes = bytes
            .checked_add(size)
            .ok_or_else(|| error("storage.corrupt", "prepared size overflow"))?;
        if records.len() >= 2048 || bytes > 4_194_304 || size != body.len() as u64 {
            return Err(error(
                "storage.corrupt",
                "prepared records violate journal bounds",
            ));
        }
        records.push(decode(&body)?);
    }
    if records.is_empty() {
        return Err(error("storage.corrupt", "prepared prefix is missing"));
    }
    Ok(json!({"token":token,"records":records,"bytes":bytes,"more":more}))
}

#[cfg(test)]
mod authority_tests {
    use super::*;

    fn open_current_v2_scan_fixture(
        path: &Path,
        recording_id: &str,
        quota: JournalQuota,
    ) -> ContinuationJournal {
        #[cfg(windows)]
        {
            let protector: Arc<dyn ScanRecoveryStorageProtector> =
                Arc::new(WindowsCurrentUserScanRecoveryStorageProtector::new());
            protector
                .provision_directory(path.parent().expect("scan fixture parent"))
                .expect("trusted Windows scan fixture root");
            ContinuationJournal::open_with_scan_recovery_storage(
                path,
                recording_id,
                &json!({}),
                quota,
                Some(protector),
            )
            .expect("open trusted Windows scan fixture")
        }

        #[cfg(not(windows))]
        ContinuationJournal::open(path, recording_id, &json!({}), quota).expect("open scan fixture")
    }

    #[test]
    fn host_shutdown_releases_only_idle_registry_journals() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-registry-host-shutdown-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let other_directory = directory.with_extension("other");
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(&other_directory).unwrap();
        let registry = JournalRegistry::default();
        registry.configure_directory(&directory).unwrap();
        let other_registry = JournalRegistry::default();
        other_registry
            .configure_directory(&other_directory)
            .unwrap();
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 8,
        };
        let idle = registry.open("idle-recording", &json!({}), quota).unwrap();
        idle.append(&json!({}), &json!({"value":"persisted"}))
            .unwrap();
        let idle_identity = idle.file_identity;
        drop(idle);
        let live = registry.open("live-recording", &json!({}), quota).unwrap();
        live.append(&json!({}), &json!({"value":"live"})).unwrap();
        let other = other_registry
            .open("other-recording", &json!({}), quota)
            .unwrap();
        other.append(&json!({}), &json!({"value":"other"})).unwrap();
        let other_identity = other.file_identity;
        drop(other);
        let canonical_directory = directory.canonicalize().unwrap();
        let canonical_other_directory = other_directory.canonicalize().unwrap();

        assert_eq!(Arc::strong_count(&live), 2);
        assert!(
            authorities()
                .unwrap()
                .journals
                .contains_key(&canonical_directory.join("live-recording.sqlite"))
        );
        assert!(
            authorities()
                .unwrap()
                .journals
                .contains_key(&canonical_other_directory.join("other-recording.sqlite"))
        );

        registry.release_idle_after_host_shutdown().unwrap();

        assert!(
            !authorities()
                .unwrap()
                .journals
                .contains_key(&canonical_directory.join("idle-recording.sqlite"))
        );
        assert!(
            !shared_journals()
                .lock()
                .unwrap()
                .contains_key(&idle_identity)
        );
        assert!(
            authorities()
                .unwrap()
                .journals
                .contains_key(&canonical_directory.join("live-recording.sqlite"))
        );
        assert_eq!(
            live.prepare(8, 1024).unwrap()["records"][0]["record"],
            json!({"value":"live"})
        );
        assert!(
            authorities()
                .unwrap()
                .journals
                .contains_key(&canonical_other_directory.join("other-recording.sqlite"))
        );
        assert!(
            shared_journals()
                .lock()
                .unwrap()
                .contains_key(&other_identity)
        );

        let reopened_registry = JournalRegistry::default();
        reopened_registry.configure_directory(&directory).unwrap();
        let reopened = reopened_registry
            .open("idle-recording", &json!({}), quota)
            .unwrap();
        assert_eq!(
            reopened.prepare(8, 1024).unwrap()["records"][0]["record"],
            json!({"value":"persisted"})
        );
        drop(reopened);
        reopened_registry
            .release_idle_after_host_shutdown()
            .unwrap();
        drop(live);
        registry.release_idle_after_host_shutdown().unwrap();
        other_registry.release_idle_after_host_shutdown().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(other_directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_reopens_idempotently_and_enforces_ordered_identity() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-spool-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 2,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = journal.apple_notification_failure_spool().unwrap();
        let one = AppleNotificationFailureOccurrence {
            route: "peer|service|0|characteristic|0".into(),
            epoch: 9,
            sequence: 1,
            failure: json!({"kind":"platform","detail":"one"}),
        };
        assert_eq!(
            spool.append(&one).unwrap(),
            AppleNotificationFailureAppend::Committed
        );
        assert_eq!(
            spool.append(&one).unwrap(),
            AppleNotificationFailureAppend::Committed
        );
        assert_eq!(spool.highest_sequence(&one.route, one.epoch).unwrap(), 1);
        drop(spool);
        drop(journal);
        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = reopened.apple_notification_failure_spool().unwrap();
        assert_eq!(
            spool.page(&one.route, one.epoch, 0, 8).unwrap(),
            vec![one.clone()]
        );
        let conflicting = AppleNotificationFailureOccurrence {
            failure: json!({"kind":"platform","detail":"different"}),
            ..one.clone()
        };
        assert_eq!(
            spool.append(&conflicting).unwrap_err().kind,
            "storage.identity"
        );
        spool.acknowledge(&one).unwrap();
        assert!(spool.page(&one.route, one.epoch, 0, 8).unwrap().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_keeps_sequence_watermark_after_acknowledgement_and_reopen() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-watermark-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 2,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = journal.apple_notification_failure_spool().unwrap();
        let occurrence = AppleNotificationFailureOccurrence {
            route: "peer|service|0|characteristic|0".into(),
            epoch: 9,
            sequence: 1,
            failure: json!({"kind":"platform","detail":"one"}),
        };
        spool.append(&occurrence).unwrap();
        spool.acknowledge(&occurrence).unwrap();
        assert_eq!(
            spool
                .highest_sequence(&occurrence.route, occurrence.epoch)
                .unwrap(),
            1
        );
        drop(spool);
        drop(journal);

        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = reopened.apple_notification_failure_spool().unwrap();
        assert_eq!(
            spool
                .highest_sequence(&occurrence.route, occurrence.epoch)
                .unwrap(),
            1
        );
        let reused = AppleNotificationFailureOccurrence {
            failure: json!({"kind":"platform","detail":"different"}),
            ..occurrence
        };
        assert_eq!(spool.append(&reused).unwrap_err().kind, "storage.sequence");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_ack_receipt_survives_reopen_and_keeps_fifo_proof_exact() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-ack-receipt-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 4,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = journal.apple_notification_failure_spool().unwrap();
        let first = AppleNotificationFailureOccurrence {
            route: "peer|service|0|characteristic|0".into(),
            epoch: 9,
            sequence: 1,
            failure: json!({"kind":"platform","detail":"first"}),
        };
        let second = AppleNotificationFailureOccurrence {
            sequence: 2,
            failure: json!({"kind":"platform","detail":"second"}),
            ..first.clone()
        };
        spool.append(&first).unwrap();
        spool.append(&second).unwrap();
        assert_eq!(
            spool
                .acknowledge_capture_exact(1, &second)
                .unwrap_err()
                .kind,
            "storage.sequence"
        );
        spool.acknowledge_capture_exact(1, &first).unwrap();
        drop(spool);
        drop(journal);

        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = reopened.apple_notification_failure_spool().unwrap();
        // This is the retry after SQLite committed but the caller lost its
        // success reply. It must not delete the current second capture.
        spool.acknowledge_capture(1).unwrap();
        assert_eq!(spool.page_global(0, 8).unwrap()[0].occurrence, second);
        assert_eq!(
            spool.acknowledge_capture(3).unwrap_err().kind,
            "storage.sequence"
        );
        assert_eq!(
            spool
                .acknowledge(&AppleNotificationFailureOccurrence {
                    failure: json!({"kind":"platform","detail":"different"}),
                    ..first.clone()
                })
                .unwrap_err()
                .kind,
            "storage.sequence"
        );
        spool.acknowledge_capture(2).unwrap();
        // An independent later ACK cannot prove the first caller received its
        // reply, so both exact receipts survive until explicit retirement.
        spool.acknowledge_capture(1).unwrap();
        spool.acknowledge(&first).unwrap();
        assert_eq!(
            spool
                .retire_acknowledgement(
                    1,
                    &AppleNotificationFailureOccurrence {
                        failure: json!({"kind":"platform","detail":"different"}),
                        ..first.clone()
                    }
                )
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        spool.retire_acknowledgement(1, &first).unwrap();
        // Lost retirement replies and concurrent late retries are both exact,
        // idempotent operations. They cannot delete the second capture.
        spool.retire_acknowledgement(1, &first).unwrap();
        spool.acknowledge_capture(1).unwrap();
        drop(spool);
        drop(reopened);
        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = reopened.apple_notification_failure_spool().unwrap();
        // Receipt 2 survived both a later ACK and the explicit retirement of
        // receipt 1; this is the recovery path after a process crash.
        spool.acknowledge_capture(2).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_v4_capability_reclaims_bounded_receipt_and_retries_after_reopen() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-v4-retirement-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 2,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = journal.apple_notification_failure_spool().unwrap();
        let occurrence = AppleNotificationFailureOccurrence {
            route: "peer|service|0|characteristic|0".into(),
            epoch: 9,
            sequence: 1,
            failure: json!({"kind":"platform","detail":"v4"}),
        };
        spool.append(&occurrence).unwrap();
        let capability = spool.acknowledge_capture_exact_v4(1, &occurrence).unwrap();
        assert_eq!(
            spool
                .confirm_acknowledgement_retirement_v4(1, &occurrence, "forged")
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        drop(spool);
        drop(journal);
        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = reopened.apple_notification_failure_spool().unwrap();
        // Crash after ACK but before final confirmation: the sidecar alone
        // retains exact occurrence and capability recovery material.
        let pending = spool.pending_acknowledgement_retirements_v4(1).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, 1);
        assert_eq!(pending[0].1, occurrence);
        assert_eq!(pending[0].2, capability);
        spool
            .confirm_acknowledgement_retirement_v4(1, &occurrence, &capability)
            .unwrap();
        // This is the retry after a committed final confirmation lost its reply.
        spool
            .confirm_acknowledgement_retirement_v4(1, &occurrence, &capability)
            .unwrap();
        assert_eq!(
            spool
                .acknowledge_capture_exact(1, &occurrence)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_rejects_malformed_named_index_without_rewriting_file() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-schema-index-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 2,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = journal.apple_notification_failure_spool().unwrap();
        drop(spool);
        drop(journal);
        let spool_path = path.with_extension("apple-notification-failure.sqlite");
        let db = Connection::open(&spool_path).unwrap();
        db.execute_batch("CREATE INDEX malformed_occurrence_route ON occurrences(route);")
            .unwrap();
        drop(db);
        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let error = match reopened.apple_notification_failure_spool() {
            Ok(_) => panic!("malformed sidecar unexpectedly opened"),
            Err(error) => error,
        };
        assert_eq!(error.kind, "storage.schema");
        let db = Connection::open(&spool_path).unwrap();
        let retained: String = db
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name='malformed_occurrence_route'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(retained.contains("malformed_occurrence_route"));
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_rejects_malformed_table_without_rewriting_file() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-schema-table-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 2,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = journal.apple_notification_failure_spool().unwrap();
        drop(spool);
        drop(journal);
        let spool_path = path.with_extension("apple-notification-failure.sqlite");
        let db = Connection::open(&spool_path).unwrap();
        db.execute_batch(
            "DROP TABLE acknowledgements; CREATE TABLE acknowledgements(capture_ordinal INTEGER PRIMARY KEY, identity BLOB);",
        )
        .unwrap();
        drop(db);
        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let error = match reopened.apple_notification_failure_spool() {
            Ok(_) => panic!("malformed sidecar unexpectedly opened"),
            Err(error) => error,
        };
        assert_eq!(error.kind, "storage.schema");
        let db = Connection::open(&spool_path).unwrap();
        let retained: String = db
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='acknowledgements'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            normalized_schema_sql(&retained),
            normalized_schema_sql(
                "CREATE TABLE acknowledgements(capture_ordinal INTEGER PRIMARY KEY, identity BLOB)"
            )
        );
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_rejects_unexpected_view_or_trigger_without_rewriting_file() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-schema-object-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 2,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        drop(journal.apple_notification_failure_spool().unwrap());
        drop(journal);
        let spool_path = path.with_extension("apple-notification-failure.sqlite");
        let db = Connection::open(&spool_path).unwrap();
        db.execute_batch("CREATE VIEW unexpected_apple_view AS SELECT identity FROM occurrences;")
            .unwrap();
        drop(db);
        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let failure = match reopened.apple_notification_failure_spool() {
            Ok(_) => panic!("unexpected Apple failure sidecar schema object was accepted"),
            Err(failure) => failure,
        };
        assert_eq!(failure.kind, "storage.schema");
        let db = Connection::open(&spool_path).unwrap();
        assert!(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='view' AND name='unexpected_apple_view')", [], |row| row.get::<_, bool>(0)).unwrap());
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_migrates_genuine_v3_reservations_without_losing_rows() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-v3-migration-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        private_fixture_directory(&directory);
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 2,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        drop(journal);
        let spool_path = path.with_extension("apple-notification-failure.sqlite");
        let occurrence = AppleNotificationFailureOccurrence {
            route: "peer|service|0|characteristic|0".into(),
            epoch: 7,
            sequence: 3,
            failure: json!({"kind":"platform","detail":"v3"}),
        };
        let (encoded, identity) =
            JournalAppleNotificationFailureSpool::encoded(&occurrence).unwrap();
        let db = Connection::open(&spool_path).unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_SCHEMA).unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_WATERMARK_SCHEMA)
            .unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_RESERVATION_V2_SCHEMA)
            .unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_SETTINGS_V3_SCHEMA)
            .unwrap();
        db.execute("INSERT INTO occurrences(identity,route,epoch,sequence,occurrence,bytes) VALUES(?1,?2,?3,?4,?5,?6)", params![&identity[..], occurrence.route, sql_integer(occurrence.epoch).unwrap(), sql_integer(occurrence.sequence).unwrap(), String::from_utf8(encoded).unwrap(), 123_i64]).unwrap();
        db.execute(
            "INSERT INTO reservations(route,epoch,sequence,identity) VALUES(?1,?2,?3,?4)",
            params![
                occurrence.route,
                sql_integer(occurrence.epoch).unwrap(),
                sql_integer(occurrence.sequence).unwrap(),
                &identity[..]
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO watermarks(route,epoch,highest_sequence) VALUES(?1,?2,?3)",
            params![
                occurrence.route,
                sql_integer(occurrence.epoch).unwrap(),
                sql_integer(occurrence.sequence).unwrap()
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO settings(id,max_bytes,max_records) VALUES(1,?1,?2)",
            params![
                sql_integer(quota.max_bytes).unwrap(),
                sql_integer(quota.max_records).unwrap()
            ],
        )
        .unwrap();
        db.pragma_update(None, "application_id", APPLE_FAILURE_SPOOL_APPLICATION_ID)
            .unwrap();
        db.pragma_update(None, "user_version", 3_i64).unwrap();
        drop(db);
        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = reopened.apple_notification_failure_spool().unwrap();
        assert_eq!(spool.page_global(0, 8).unwrap()[0].occurrence, occurrence);
        drop(spool);
        drop(reopened);
        let db = Connection::open(&spool_path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            APPLE_FAILURE_SPOOL_SCHEMA_VERSION
        );
        assert!(
            apple_failure_spool_table_is_exact(
                &db,
                "reservations",
                APPLE_FAILURE_SPOOL_RESERVATION_SCHEMA
            )
            .unwrap()
        );
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_migrates_genuine_v8_acknowledgements_without_losing_rows() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-v8-migration-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        private_fixture_directory(&directory);
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 2,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        drop(journal);
        let spool_path = path.with_extension("apple-notification-failure.sqlite");
        let occurrence = AppleNotificationFailureOccurrence {
            route: "peer|service|0|characteristic|0".into(),
            epoch: 11,
            sequence: 4,
            failure: json!({"kind":"platform","detail":"v8"}),
        };
        let (encoded, identity) =
            JournalAppleNotificationFailureSpool::encoded(&occurrence).unwrap();
        let bytes = sql_integer(u64::try_from(encoded.len()).unwrap()).unwrap();
        let db = Connection::open(&spool_path).unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_SCHEMA).unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_WATERMARK_SCHEMA)
            .unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_RESERVATION_SCHEMA)
            .unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_SETTINGS_V3_SCHEMA)
            .unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_GLOBAL_STREAM_SCHEMA)
            .unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_CAPTURE_WATERMARK_SCHEMA)
            .unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_ACKNOWLEDGEMENTS_V9_SCHEMA)
            .unwrap();
        db.execute(
            "INSERT INTO occurrences(identity,route,epoch,sequence,occurrence,bytes) VALUES(?1,?2,?3,?4,?5,?6)",
            params![&identity[..], occurrence.route, sql_integer(occurrence.epoch).unwrap(), sql_integer(occurrence.sequence).unwrap(), String::from_utf8(encoded).unwrap(), bytes],
        ).unwrap();
        db.execute(
            "INSERT INTO settings(id,max_bytes,max_records) VALUES(1,?1,?2)",
            params![
                sql_integer(quota.max_bytes).unwrap(),
                sql_integer(quota.max_records).unwrap()
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO capture_stream(identity) VALUES(?1)",
            [&identity[..]],
        )
        .unwrap();
        db.execute(
            "UPDATE capture_watermark SET highest_ordinal=1 WHERE id=1",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO acknowledgements(capture_ordinal,identity,retired) VALUES(1,?1,1)",
            [&identity[..]],
        )
        .unwrap();
        db.pragma_update(None, "application_id", APPLE_FAILURE_SPOOL_APPLICATION_ID)
            .unwrap();
        db.pragma_update(None, "user_version", 8_i64).unwrap();
        drop(db);

        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = reopened.apple_notification_failure_spool().unwrap();
        assert_eq!(spool.page_global(0, 8).unwrap()[0].occurrence, occurrence);
        drop(spool);
        drop(reopened);
        let db = Connection::open(&spool_path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            APPLE_FAILURE_SPOOL_SCHEMA_VERSION
        );
        assert_eq!(
            db.query_row(
                "SELECT retired FROM acknowledgements WHERE capture_ordinal=1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_repairs_interrupted_v1_migration_without_losing_rows() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-v1-interrupted-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        private_fixture_directory(&directory);
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 2,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        drop(journal);
        let spool_path = path.with_extension("apple-notification-failure.sqlite");
        let occurrence = AppleNotificationFailureOccurrence {
            route: "peer|service|0|characteristic|0".into(),
            epoch: 9,
            sequence: 6,
            failure: json!({"kind":"platform","detail":"retained"}),
        };
        let (encoded, identity) =
            JournalAppleNotificationFailureSpool::encoded(&occurrence).unwrap();
        let encoded = String::from_utf8(encoded).unwrap();
        let db = Connection::open(&spool_path).unwrap();
        // This is the exact durable shape left by the pre-5.2.2 bug: its v1
        // migration committed these two tables, then the process stopped
        // before it advanced user_version.
        db.execute_batch(APPLE_FAILURE_SPOOL_SCHEMA).unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_WATERMARK_SCHEMA)
            .unwrap();
        db.execute_batch(APPLE_FAILURE_SPOOL_RESERVATION_SCHEMA)
            .unwrap();
        db.execute(
            "INSERT INTO occurrences(identity,route,epoch,sequence,occurrence,bytes) VALUES(?1,?2,?3,?4,?5,?6)",
            params![&identity[..], occurrence.route, sql_integer(occurrence.epoch).unwrap(), sql_integer(occurrence.sequence).unwrap(), encoded, 1_i64],
        ).unwrap();
        db.execute(
            "INSERT INTO watermarks(route,epoch,highest_sequence) VALUES(?1,?2,?3)",
            params![
                occurrence.route,
                sql_integer(occurrence.epoch).unwrap(),
                sql_integer(occurrence.sequence).unwrap()
            ],
        )
        .unwrap();
        db.pragma_update(None, "application_id", APPLE_FAILURE_SPOOL_APPLICATION_ID)
            .unwrap();
        db.pragma_update(None, "user_version", 1_i64).unwrap();
        drop(db);

        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = reopened.apple_notification_failure_spool().unwrap();
        assert_eq!(spool.page_global(0, 8).unwrap()[0].occurrence, occurrence);
        let db = Connection::open(&spool_path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            APPLE_FAILURE_SPOOL_SCHEMA_VERSION
        );
        drop(db);
        drop(spool);
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn scan_failure_v1_sidecar_is_exportable_but_never_promoted_to_a_v2_owner() {
        use ubm_core::oversized_evidence::{
            EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureMember,
            ScanFailureOccurrence,
        };
        let directory = std::env::temp_dir().join(format!(
            "ubm-scan-failure-v1-sidecar-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 1,
        };
        let identity = ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [5; 16],
        };
        if std::env::var_os("UBM_SCAN_V1_EXPORT_CHILD").is_some() {
            let path = std::env::var_os("UBM_SCAN_V1_EXPORT_PATH")
                .map(PathBuf::from)
                .expect("child fixture path");
            let reopened = ContinuationJournal::open(&path, "scan-v1", &json!({}), quota).unwrap();
            assert!(
                !path
                    .parent()
                    .unwrap()
                    .join(SCAN_RECOVERY_DIRECTORY)
                    .exists()
            );
            let raw = reopened.load_scan_failure_raw_cut(identity).unwrap();
            assert_eq!(raw.members.len(), 2);
            assert_eq!(raw.members[0].recipient, [6; 16]);
            assert_eq!(raw.members[1].delivery, [9; 16]);
            assert!(
                matches!(raw.tokens.as_slice(), [EvidenceToken::Null, EvidenceToken::Bytes(value), EvidenceToken::I64(-42)] if value == &[0, 255, 3, 0])
            );
            return;
        }
        let journal = ContinuationJournal::open(&path, "scan-v1", &json!({}), quota).unwrap();
        let private_directory = directory.join(SCAN_RECOVERY_DIRECTORY);
        assert!(
            !private_directory.exists(),
            "opening a primary journal must not provision v2 scan recovery"
        );
        let sidecar = path.with_extension("scan-failure-recovery.sqlite");
        let database = Connection::open(&sidecar).unwrap();
        database.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE occurrences(identity BLOB PRIMARY KEY CHECK(octet_length(identity)=80)); CREATE TABLE members(identity BLOB NOT NULL REFERENCES occurrences(identity) ON DELETE CASCADE, position INTEGER NOT NULL, recipient BLOB NOT NULL CHECK(octet_length(recipient)=16), delivery BLOB NOT NULL CHECK(octet_length(delivery)=16), acknowledged INTEGER NOT NULL DEFAULT 0 CHECK(acknowledged IN (0,1)), PRIMARY KEY(identity,position)) WITHOUT ROWID; CREATE TABLE tokens(identity BLOB NOT NULL REFERENCES occurrences(identity) ON DELETE CASCADE, position INTEGER NOT NULL, tag INTEGER NOT NULL, payload BLOB NOT NULL, PRIMARY KEY(identity,position)) WITHOUT ROWID;").unwrap();
        database
            .pragma_update(None, "application_id", SCAN_FAILURE_SIDECAR_APPLICATION_ID)
            .unwrap();
        database.pragma_update(None, "user_version", 1_i64).unwrap();
        let key = scan_failure_identity_bytes(identity);
        database
            .execute("INSERT INTO occurrences(identity) VALUES(?1)", [&key])
            .unwrap();
        database
            .execute(
                "INSERT INTO members(identity,position,recipient,delivery) VALUES(?1,0,?2,?3)",
                params![&key, [6_u8; 16].as_slice(), [7_u8; 16].as_slice()],
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO members(identity,position,recipient,delivery) VALUES(?1,1,?2,?3)",
                params![&key, [8_u8; 16].as_slice(), [9_u8; 16].as_slice()],
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO tokens(identity,position,tag,payload) VALUES(?1,0,0,?2)",
                params![&key, Vec::<u8>::new()],
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO tokens(identity,position,tag,payload) VALUES(?1,1,6,?2)",
                params![&key, vec![0_u8, 255, 3, 0]],
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO tokens(identity,position,tag,payload) VALUES(?1,2,2,?2)",
                params![&key, (-42_i64).to_be_bytes().to_vec()],
            )
            .unwrap();
        drop(database);
        // Reopen through the public journal path: the old sibling remains
        // discoverable for exact export, but never gains a v2 owner.
        drop(journal);
        let journal = ContinuationJournal::open(&path, "scan-v1", &json!({}), quota).unwrap();
        assert!(
            !private_directory.exists(),
            "exporting a v1 sibling must not leave a private recovery directory"
        );
        assert!(journal.scan_failure_owner_envelopes().unwrap().is_empty());
        assert_eq!(
            journal
                .scan_failure_occurrence_page(identity, 0, 1)
                .unwrap()["tokens"][0]["tag"],
            0
        );
        let raw = journal.load_scan_failure_raw_cut(identity).unwrap();
        assert_eq!(
            raw.members,
            vec![
                ScanFailureMember {
                    recipient: [6; 16],
                    delivery: [7; 16]
                },
                ScanFailureMember {
                    recipient: [8; 16],
                    delivery: [9; 16]
                },
            ]
        );
        assert!(
            matches!(raw.tokens.as_slice(), [EvidenceToken::Null, EvidenceToken::Bytes(value), EvidenceToken::I64(-42)] if value == &[0, 255, 3, 0])
        );
        let occurrence = ScanFailureOccurrence::new(
            identity,
            vec![ScanFailureMember {
                recipient: [6; 16],
                delivery: [7; 16],
            }],
            Arc::new(EvidenceSource::new(vec![EvidenceToken::Null])),
        );
        assert_eq!(
            journal
                .capture_scan_failure_occurrence(&occurrence)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("continuation_journal::authority_tests::scan_failure_v1_sidecar_is_exportable_but_never_promoted_to_a_v2_owner")
            .arg("--nocapture")
            .env("UBM_SCAN_V1_EXPORT_CHILD", "1")
            .env("UBM_SCAN_V1_EXPORT_PATH", &path)
            .status()
            .unwrap();
        assert!(status.success(), "fresh-process v1 export failed: {status}");
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_legacy_acknowledge_is_global_fifo_and_atomic() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-legacy-global-ack-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 8,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = journal.apple_notification_failure_spool().unwrap();
        let a6 = AppleNotificationFailureOccurrence {
            route: "a|service|0|characteristic|0".into(),
            epoch: 9,
            sequence: 6,
            failure: json!({"kind":"platform","detail":"a6"}),
        };
        let b7 = AppleNotificationFailureOccurrence {
            route: "b|service|0|characteristic|0".into(),
            epoch: 9,
            sequence: 7,
            failure: json!({"kind":"platform","detail":"b7"}),
        };
        let a8 = AppleNotificationFailureOccurrence {
            sequence: 8,
            failure: json!({"kind":"platform","detail":"a8"}),
            ..a6.clone()
        };
        spool.append(&a6).unwrap();
        spool.append(&b7).unwrap();
        spool.append(&a8).unwrap();
        assert_eq!(spool.acknowledge(&b7).unwrap_err().kind, "storage.sequence");
        // The rejected transaction leaves the occurrence and its capture row
        // together; reopening proves a callback cannot be orphaned mid-ACK.
        drop(spool);
        drop(journal);
        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = reopened.apple_notification_failure_spool().unwrap();
        assert_eq!(
            spool
                .page_global(0, 8)
                .unwrap()
                .into_iter()
                .map(|capture| capture.occurrence)
                .collect::<Vec<_>>(),
            vec![a6.clone(), b7.clone(), a8.clone()]
        );
        spool.acknowledge(&a6).unwrap();
        spool.acknowledge(&b7).unwrap();
        spool.acknowledge(&a8).unwrap();
        assert!(spool.page_global(0, 8).unwrap().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_allows_first_overflow_base_then_rejects_a_hole_after_full() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-sequence-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 1,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = journal.apple_notification_failure_spool().unwrap();
        let six = AppleNotificationFailureOccurrence {
            route: "peer|service|0|characteristic|0".into(),
            epoch: 9,
            sequence: 6,
            failure: json!({"kind":"platform","detail":"six"}),
        };
        assert_eq!(
            spool.append(&six).unwrap(),
            AppleNotificationFailureAppend::Committed
        );
        let seven = AppleNotificationFailureOccurrence {
            sequence: 7,
            failure: json!({"kind":"platform","detail":"seven"}),
            ..six.clone()
        };
        assert_eq!(
            spool.append(&seven).unwrap(),
            AppleNotificationFailureAppend::Full
        );
        let eight = AppleNotificationFailureOccurrence {
            sequence: 8,
            failure: json!({"kind":"platform","detail":"eight"}),
            ..six.clone()
        };
        assert_eq!(spool.append(&eight).unwrap_err().kind, "storage.sequence");
        spool.acknowledge(&six).unwrap();
        assert_eq!(
            spool.append(&seven).unwrap(),
            AppleNotificationFailureAppend::Committed
        );
        assert_eq!(spool.highest_sequence(&six.route, six.epoch).unwrap(), 7);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_full_first_append_reserves_its_sequence() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-first-full-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 2,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = journal
            .apple_notification_failure_spool_with_quota(JournalQuota {
                max_bytes: 1,
                max_records: 2,
            })
            .unwrap();
        let six = AppleNotificationFailureOccurrence {
            route: "peer|service|0|characteristic|0".into(),
            epoch: 9,
            sequence: 6,
            failure: json!({"kind":"platform","detail":"six"}),
        };
        assert_eq!(
            spool.append(&six).unwrap(),
            AppleNotificationFailureAppend::Full
        );
        let seven = AppleNotificationFailureOccurrence {
            sequence: 7,
            failure: json!({"kind":"platform","detail":"seven"}),
            ..six.clone()
        };
        assert_eq!(spool.append(&seven).unwrap_err().kind, "storage.sequence");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apple_failure_spool_global_page_preserves_interleaved_capture_order_after_reopen() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-global-page-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 8,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = journal.apple_notification_failure_spool().unwrap();
        let a = AppleNotificationFailureOccurrence {
            route: "a|service|0|characteristic|0".into(),
            epoch: 9,
            sequence: 6,
            failure: json!({"kind":"platform","detail":"a6"}),
        };
        let b = AppleNotificationFailureOccurrence {
            route: "b|service|0|characteristic|0".into(),
            epoch: 9,
            sequence: 7,
            failure: json!({"kind":"platform","detail":"b7"}),
        };
        let a_next = AppleNotificationFailureOccurrence {
            sequence: 8,
            failure: json!({"kind":"platform","detail":"a8"}),
            ..a.clone()
        };
        spool.append(&a).unwrap();
        spool.append(&b).unwrap();
        spool.append(&a_next).unwrap();
        assert_eq!(
            spool.page_global(0, 8).unwrap(),
            vec![
                AppleNotificationFailureCapture {
                    capture_ordinal: 1,
                    occurrence: a.clone()
                },
                AppleNotificationFailureCapture {
                    capture_ordinal: 2,
                    occurrence: b.clone()
                },
                AppleNotificationFailureCapture {
                    capture_ordinal: 3,
                    occurrence: a_next.clone()
                },
            ]
        );
        drop(spool);
        drop(journal);

        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = reopened.apple_notification_failure_spool().unwrap();
        assert_eq!(spool.page_global(0, 1).unwrap()[0].occurrence, a);
        assert_eq!(
            spool.acknowledge_capture(2).unwrap_err().kind,
            "storage.sequence"
        );
        spool.acknowledge_capture(1).unwrap();
        assert_eq!(spool.page_global(0, 8).unwrap()[0].occurrence, b);
        spool.acknowledge_capture(2).unwrap();
        assert_eq!(spool.page_global(0, 8).unwrap()[0].occurrence, a_next);
        spool.acknowledge_capture(3).unwrap();
        assert!(spool.page_global(0, 8).unwrap().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn clear_refuses_unacknowledged_apple_failure_spool_and_preserves_reopen_page() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-apple-failure-clear-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 2,
        };
        let journal =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = journal.apple_notification_failure_spool().unwrap();
        let occurrence = AppleNotificationFailureOccurrence {
            route: "peer|service|0|characteristic|0".into(),
            epoch: 9,
            sequence: 6,
            failure: json!({"kind":"platform","detail":"six"}),
        };
        spool.append(&occurrence).unwrap();
        journal.stop().unwrap();
        assert_eq!(journal.clear().unwrap_err().kind, "storage.busy");
        drop(spool);
        drop(journal);

        let reopened =
            Arc::new(ContinuationJournal::open(&path, "apple", &json!({}), quota).unwrap());
        let spool = reopened.apple_notification_failure_spool().unwrap();
        assert_eq!(
            spool
                .page(&occurrence.route, occurrence.epoch, 0, 8)
                .unwrap(),
            vec![occurrence]
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn peer_failure_occurrence(encoded_length: u64) -> DurablePeerFailureRecoveryOccurrence {
        let descriptor = ubm_core::oversized_evidence::PeerSourceFailureDescriptor {
            protocol: 1,
            representation: ubm_core::oversized_evidence::PEER_SOURCE_FAILURE_REPRESENTATION,
            identity: ubm_core::oversized_evidence::PeerSourceFailureIdentity {
                host: [1; 16],
                peer: [2; 16],
                source_generation: [3; 16],
                native_group: [4; 16],
                transfer: [5; 16],
                recovery: [6; 16],
            },
            encoded_length,
            chunk_bytes: 64 * 1024,
        };
        DurablePeerFailureRecoveryOccurrence {
            descriptor,
            source_ack_id: None,
            recipients: vec![crate::central::DurablePeerFailureRecoveryRecipient {
                scope: (
                    "peer".into(),
                    "service".into(),
                    0,
                    "characteristic".into(),
                    0,
                ),
                consumer: "test-consumer".to_owned(),
                epoch: 7,
                recipient_lease: "recipient-lease".into(),
                contiguous_read: 0,
                acknowledged: false,
            }],
            producer_ack: crate::central::DurablePeerFailureProducerAck::NotStarted,
            teardown: Some(crate::errors::DesktopErrorEvidence {
                code: BleErrorCode::PlatformFailure,
                domain: BleErrorDomain::Platform,
                operation: "gatt.notification.source-failure".into(),
                detail: Some("producer bytes: \u{ff}\0".into()),
                platform: Some(Box::new(
                    crate::errors::PlatformDetail::new("winrt", "0x80004005")
                        .with_message("native producer \u{ff}\0")
                        .with_metadata(
                            "hresult",
                            crate::errors::PlatformValue::Text("0x80004005".into()),
                        )
                        .with_metadata(
                            "gattStatus",
                            crate::errors::PlatformValue::Text("protocol-error".into()),
                        ),
                )),
                commit: None,
                retryability: crate::errors::Retryability::CallerDecides,
            }),
        }
    }

    #[test]
    fn peer_failure_v1_occurrence_decodes_without_inventing_teardown() {
        let occurrence = peer_failure_occurrence(4);
        let mut v1 = crate::central::durable_peer_failure_recovery_occurrence_encode(&occurrence);
        v1["version"] = json!(1);
        v1.as_object_mut()
            .expect("encoded occurrence")
            .remove("teardown");
        let decoded = crate::central::durable_peer_failure_recovery_occurrence_decode(&v1).unwrap();
        assert_eq!(decoded.teardown, None);
        assert_eq!(decoded.descriptor, occurrence.descriptor);
    }

    #[tokio::test]
    async fn peer_failure_sidecar_streams_chunks_reopens_and_requires_explicit_retirement() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-peer-failure-sidecar-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let journal = Arc::new(
            ContinuationJournal::open(
                &path,
                "peer-failure-sidecar",
                &json!({}),
                JournalQuota {
                    max_bytes: 8 << 20,
                    max_records: 1,
                },
            )
            .unwrap(),
        );
        let ledger = journal.peer_failure_recovery_ledger().unwrap();
        let occurrence = peer_failure_occurrence((4 << 20) + 17);
        let descriptor = occurrence.descriptor;
        ledger.begin_stage(&occurrence).await.unwrap();
        let mut offset = 0_u64;
        while offset < descriptor.encoded_length {
            let size = usize::try_from(
                (descriptor.encoded_length - offset).min(u64::from(descriptor.chunk_bytes)),
            )
            .unwrap();
            let bytes = vec![(offset as u8).wrapping_mul(31); size];
            let done = offset + size as u64 == descriptor.encoded_length;
            ledger
                .append_stage_chunk(&descriptor, offset, &bytes, done)
                .await
                .unwrap();
            // The first retry verifies exact durable chunk identity without
            // consuming a second copy or advancing the durable cursor.
            if offset == 0 {
                ledger
                    .append_stage_chunk(&descriptor, offset, &bytes, done)
                    .await
                    .unwrap();
            }
            offset += size as u64;
        }
        ledger.commit_stage(&descriptor).await.unwrap();
        let mut recipient_progress = occurrence.clone();
        recipient_progress.recipients[0].contiguous_read = descriptor.encoded_length;
        recipient_progress.recipients[0].acknowledged = true;
        PeerFailureRecoveryLedger::update_recipients(ledger.as_ref(), &recipient_progress)
            .await
            .unwrap();
        ledger
            .mark_source_ack_inflight(&descriptor, "source-ack")
            .await
            .unwrap();
        drop(ledger);

        let reopened = journal.peer_failure_recovery_ledger().unwrap();
        let retained = reopened.recover(&descriptor).unwrap().unwrap();
        let mut expected_occurrence = recipient_progress;
        expected_occurrence.source_ack_id = Some("source-ack".into());
        expected_occurrence.producer_ack = crate::central::DurablePeerFailureProducerAck::InFlight;
        assert_eq!(retained.occurrence, expected_occurrence);
        assert_eq!(retained.staged_bytes, descriptor.encoded_length);
        assert_eq!(retained.source_ack_id.as_deref(), Some("source-ack"));
        assert_eq!(retained.state, PeerFailureRecoveryState::SourceAckInFlight);
        let recovered = reopened.recover_all().unwrap();
        assert_eq!(recovered, vec![expected_occurrence.clone()]);
        let first =
            PeerFailureRecoveryLedger::read_recovered_chunk(reopened.as_ref(), &descriptor, 0)
                .await
                .unwrap();
        assert_eq!(first.offset, 0);
        assert_eq!(first.bytes.len(), descriptor.chunk_bytes as usize);
        assert!(!first.done);
        // A crash/reopen never promotes an in-flight producer ACK.
        reopened
            .confirm_source_ack(&descriptor, "source-ack")
            .await
            .unwrap();
        reopened.release(&descriptor).await.unwrap();
        assert!(reopened.recover(&descriptor).unwrap().is_none());
        drop(reopened);
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn peer_failure_cold_reclaim_requires_authority_and_never_reclaims_after_live_recovery() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-peer-failure-cold-reclaim-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 8,
        };
        let registry = JournalRegistry::default();
        registry.configure_directory(&directory).unwrap();
        let journal = registry.open("cold", &json!({}), quota).unwrap();
        let first = journal.peer_failure_recovery_ledger().unwrap();
        let second = journal.peer_failure_recovery_ledger().unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "same journal shares one sidecar owner"
        );
        first.recover_all().unwrap();
        let occurrence = peer_failure_occurrence(8);
        let descriptor = occurrence.descriptor;
        first.begin_stage(&occurrence).await.unwrap();
        first
            .append_stage_chunk(&descriptor, 0, &[1, 2, 3, 4], false)
            .await
            .unwrap();
        // The cached second consumer is a live operation, not a restart: its
        // recovery pass must preserve the active partial stage.
        assert!(second.recover_all().unwrap().is_empty());
        assert!(second.recover(&descriptor).unwrap().is_some());
        drop(second);
        drop(first);
        drop(journal);
        authorities()
            .unwrap()
            .journals
            .remove(&directory.join("cold.sqlite"));

        // With the prior process authority gone, a new registry holds the
        // OS lock and its first singleton recovery pass reclaims only the
        // never-acknowledgeable partial stage.
        let reopened_registry = JournalRegistry::default();
        reopened_registry.configure_directory(&directory).unwrap();
        let reopened_journal = reopened_registry.open("cold", &json!({}), quota).unwrap();
        let reopened = reopened_journal.peer_failure_recovery_ledger().unwrap();
        assert!(reopened.recover_all().unwrap().is_empty());
        assert!(reopened.recover(&descriptor).unwrap().is_none());
        drop(reopened);
        drop(reopened_journal);
        authorities()
            .unwrap()
            .journals
            .remove(&directory.join("cold.sqlite"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn peer_failure_direct_open_never_reclaims_an_unowned_live_stage() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-peer-failure-direct-stage-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let journal = Arc::new(
            ContinuationJournal::open(
                &directory.join("recording.sqlite"),
                "direct",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 8,
                },
            )
            .unwrap(),
        );
        let ledger = journal.peer_failure_recovery_ledger().unwrap();
        let occurrence = peer_failure_occurrence(8);
        let descriptor = occurrence.descriptor;
        ledger.begin_stage(&occurrence).await.unwrap();
        ledger
            .append_stage_chunk(&descriptor, 0, &[1, 2, 3, 4], false)
            .await
            .unwrap();
        assert!(ledger.recover_all().unwrap().is_empty());
        assert!(ledger.recover(&descriptor).unwrap().is_some());
        drop(ledger);
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn peer_failure_sidecar_refuses_gaps_mismatched_retries_and_inflight_release() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-peer-failure-sidecar-refusal-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let journal = Arc::new(
            ContinuationJournal::open(
                &path,
                "peer-failure-sidecar-refusal",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 1,
                },
            )
            .unwrap(),
        );
        let ledger = journal.peer_failure_recovery_ledger().unwrap();
        let occurrence = peer_failure_occurrence(4);
        let descriptor = occurrence.descriptor;
        ledger.begin_stage(&occurrence).await.unwrap();
        let gap = ledger
            .append_stage_chunk(&descriptor, 1, b"abc", true)
            .await
            .unwrap_err();
        assert_eq!(gap.code(), BleErrorCode::PlatformFailure);
        ledger
            .append_stage_chunk(&descriptor, 0, b"abcd", true)
            .await
            .unwrap();
        let different = ledger
            .append_stage_chunk(&descriptor, 0, b"abce", true)
            .await
            .unwrap_err();
        assert_eq!(different.code(), BleErrorCode::PlatformFailure);
        ledger.commit_stage(&descriptor).await.unwrap();
        let mut advanced = occurrence.clone();
        advanced.recipients[0].contiguous_read = descriptor.encoded_length;
        advanced.recipients[0].acknowledged = true;
        PeerFailureRecoveryLedger::update_recipients(ledger.as_ref(), &advanced)
            .await
            .unwrap();
        let regressed = PeerFailureRecoveryLedger::update_recipients(ledger.as_ref(), &occurrence)
            .await
            .unwrap_err();
        assert_eq!(regressed.code(), BleErrorCode::PlatformFailure);
        ledger
            .mark_source_ack_inflight(&descriptor, "source-ack")
            .await
            .unwrap();
        let release = ledger.release(&descriptor).await.unwrap_err();
        assert_eq!(release.code(), BleErrorCode::PlatformFailure);
        ledger
            .confirm_source_ack(&descriptor, "source-ack")
            .await
            .unwrap();
        ledger.release(&descriptor).await.unwrap();
        drop(ledger);
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn peer_failure_sidecar_reopen_refuses_tampered_durable_bytes() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-peer-failure-sidecar-corrupt-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let journal = Arc::new(
            ContinuationJournal::open(
                &path,
                "peer-failure-sidecar-corrupt",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 1,
                },
            )
            .unwrap(),
        );
        let ledger = journal.peer_failure_recovery_ledger().unwrap();
        let occurrence = peer_failure_occurrence(4);
        let descriptor = occurrence.descriptor;
        ledger.begin_stage(&occurrence).await.unwrap();
        ledger
            .append_stage_chunk(&descriptor, 0, b"abcd", true)
            .await
            .unwrap();
        ledger.commit_stage(&descriptor).await.unwrap();

        // Simulate a post-commit disk mutation before the next process opens
        // the trusted sidecar. The persisted digest is the restart fence.
        let identity = peer_failure_identity(&descriptor);
        ledger
            .connection()
            .unwrap()
            .execute(
                "UPDATE chunks SET bytes=?3 WHERE identity=?1 AND offset=?2",
                params![&identity[..], 0_i64, b"abce"],
            )
            .unwrap();
        drop(ledger);

        let reopened = journal.peer_failure_recovery_ledger().unwrap();
        let error = reopened.recover(&descriptor).unwrap_err();
        assert_eq!(error.kind, "storage.corrupt");
        drop(reopened);
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn scan_failure_sidecar_reopens_exact_tokens_requires_all_recipients_and_refuses_changed_retry()
    {
        use ubm_core::oversized_evidence::{
            EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureMember,
            ScanFailureOccurrence,
        };
        let directory = std::env::temp_dir().join(format!(
            "ubm-scan-failure-sidecar-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        private_fixture_directory(&directory);
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 1,
        };
        let identity = ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [5; 16],
        };
        let members = vec![
            ScanFailureMember {
                recipient: [6; 16],
                delivery: [7; 16],
            },
            ScanFailureMember {
                recipient: [8; 16],
                delivery: [9; 16],
            },
        ];
        let occurrence = ScanFailureOccurrence::new(
            identity,
            members.clone(),
            Arc::new(EvidenceSource::new(vec![
                EvidenceToken::Text("raw\0token".into()),
                EvidenceToken::Bytes(vec![0, 255, 3]),
                EvidenceToken::F64Bits(u64::MAX),
            ])),
        );
        let journal = open_current_v2_scan_fixture(&path, "scan-sidecar", quota);
        journal
            .capture_scan_failure_occurrence(&occurrence)
            .unwrap();
        // A lost success reply may repeat the original cut without a second
        // durable copy. A same-id retry with even one altered raw byte must
        // fail instead of overwriting the evidence recipients will recover.
        journal
            .capture_scan_failure_occurrence(&occurrence)
            .unwrap();
        let altered = ScanFailureOccurrence::new(
            identity,
            members.clone(),
            Arc::new(EvidenceSource::new(vec![
                EvidenceToken::Text("raw\0token".into()),
                EvidenceToken::Bytes(vec![0, 254, 3]),
                EvidenceToken::F64Bits(u64::MAX),
            ])),
        );
        assert_eq!(
            journal
                .capture_scan_failure_occurrence(&altered)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        let altered_member = ScanFailureOccurrence::new(
            identity,
            vec![
                ScanFailureMember {
                    recipient: [6; 16],
                    delivery: [7; 16],
                },
                ScanFailureMember {
                    recipient: [8; 16],
                    delivery: [10; 16],
                },
            ],
            Arc::new(EvidenceSource::new(vec![
                EvidenceToken::Text("raw\0token".into()),
                EvidenceToken::Bytes(vec![0, 255, 3]),
                EvidenceToken::F64Bits(u64::MAX),
            ])),
        );
        assert_eq!(
            journal
                .capture_scan_failure_occurrence(&altered_member)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        drop(journal);

        let reopened = open_current_v2_scan_fixture(&path, "scan-sidecar", quota);
        let page = reopened
            .scan_failure_occurrence_page(identity, 0, 8)
            .unwrap();
        assert_eq!(
            page["tokens"],
            json!([
                {"position": 0, "tag": 5, "payload": b"raw\0token".to_vec()},
                {"position": 1, "tag": 6, "payload": vec![0, 255, 3]},
                {"position": 2, "tag": 4, "payload": u64::MAX.to_be_bytes().to_vec()},
            ])
        );
        assert!(
            !reopened
                .acknowledge_scan_failure_recipient(
                    identity,
                    members[0].recipient,
                    members[0].delivery
                )
                .unwrap()
        );
        assert!(
            reopened
                .scan_failure_occurrence_page(identity, 0, 8)
                .is_ok()
        );
        assert!(
            reopened
                .acknowledge_scan_failure_recipient(
                    identity,
                    members[1].recipient,
                    members[1].delivery
                )
                .unwrap()
        );
        assert_eq!(
            reopened
                .scan_failure_occurrence_page(identity, 0, 8)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_failure_sidecar_is_created_owner_private() {
        use std::os::unix::fs::MetadataExt;
        use ubm_core::oversized_evidence::{
            EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureOccurrence,
        };

        let directory = std::env::temp_dir().join(format!(
            "ubm-scan-failure-sidecar-private-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        private_fixture_directory(&directory);
        let path = directory.join("recording.sqlite");
        let journal = ContinuationJournal::open(
            &path,
            "scan-sidecar-private",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 1,
            },
        )
        .unwrap();
        let identity = ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [5; 16],
        };
        journal
            .capture_scan_failure_occurrence(&ScanFailureOccurrence::new(
                identity,
                Vec::new(),
                Arc::new(EvidenceSource::new(vec![EvidenceToken::Null])),
            ))
            .unwrap();
        let sidecar = directory
            .join(SCAN_RECOVERY_DIRECTORY)
            .join("recording.scan-failure-recovery.sqlite");
        let metadata = std::fs::symlink_metadata(&sidecar).unwrap();
        assert!(metadata.is_file());
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o777, 0o600);
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_recovery_storage_protector_is_provisioned_once_and_validated_on_reopen() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Protector {
            provisioned: AtomicUsize,
            validated: AtomicUsize,
            directories_provisioned: AtomicUsize,
            directories_validated: AtomicUsize,
        }
        impl ScanRecoveryStorageProtector for Protector {
            fn provision(&self, path: &Path) -> Result<()> {
                assert!(
                    path.to_string_lossy()
                        .ends_with(".scan-failure-recovery.sqlite")
                );
                self.provisioned.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn validate(&self, path: &Path) -> Result<()> {
                assert!(
                    path.to_string_lossy()
                        .ends_with(".scan-failure-recovery.sqlite")
                );
                self.validated.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn capability(&self) -> &'static str {
                "test-host-acl"
            }
            fn provision_directory(&self, path: &Path) -> Result<()> {
                assert!(path.is_dir());
                self.directories_provisioned.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn validate_directory(&self, path: &Path) -> Result<()> {
                assert!(path.is_dir());
                self.directories_validated.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        use ubm_core::oversized_evidence::{
            EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureOccurrence,
        };

        let directory = std::env::temp_dir().join(format!(
            "ubm-scan-sidecar-protector-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        private_fixture_directory(&directory);
        let protector = Arc::new(Protector {
            provisioned: AtomicUsize::new(0),
            validated: AtomicUsize::new(0),
            directories_provisioned: AtomicUsize::new(0),
            directories_validated: AtomicUsize::new(0),
        });
        let registry = JournalRegistry::default();
        assert_eq!(
            registry
                .configure_directory_with_scan_recovery_storage(&directory, protector.clone())
                .unwrap()["scanRecoveryStorage"],
            "test-host-acl"
        );
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 1,
        };
        let path = directory.join("scan-sidecar-protector.sqlite");
        let journal = ContinuationJournal::open_with_scan_recovery_storage(
            &path,
            "scan-sidecar-protector",
            &json!({}),
            quota,
            Some(protector.clone()),
        )
        .unwrap();
        journal
            .capture_scan_failure_occurrence(&ScanFailureOccurrence::new(
                ScanFailureIdentity {
                    host: [1; 16],
                    scan: [2; 16],
                    start: [3; 16],
                    source_generation: [4; 16],
                    occurrence: [5; 16],
                },
                Vec::new(),
                Arc::new(EvidenceSource::new(vec![EvidenceToken::Null])),
            ))
            .unwrap();
        assert_eq!(protector.provisioned.load(Ordering::SeqCst), 1);
        assert_eq!(protector.validated.load(Ordering::SeqCst), 1);
        // Unix establishes the child with `0700` itself; the host protector
        // owns the configured root boundary and therefore provisions once.
        assert_eq!(protector.directories_provisioned.load(Ordering::SeqCst), 1);
        assert!(protector.directories_validated.load(Ordering::SeqCst) >= 3);
        drop(journal);
        let reopened = ContinuationJournal::open_with_scan_recovery_storage(
            &path,
            "scan-sidecar-protector",
            &json!({}),
            quota,
            Some(protector.clone()),
        )
        .unwrap();
        let identity = ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [5; 16],
        };
        reopened
            .scan_failure_occurrence_page(identity, 0, 1)
            .unwrap();
        assert_eq!(protector.provisioned.load(Ordering::SeqCst), 1);
        assert!(protector.validated.load(Ordering::SeqCst) >= 2);
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_recovery_refuses_an_other_writable_parent_before_sidecar_creation() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "ubm-insecure-scan-parent-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        let journal = ContinuationJournal::open(
            &directory.join("recording.sqlite"),
            "insecure-scan-parent",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 1,
            },
        )
        .unwrap();
        let failure = journal
            .scan_failure_cleanup_retirements()
            .expect_err("insecure parent must be refused before scan recovery is created");
        assert_eq!(failure.kind, "storage.permission");
        assert!(!directory.join(SCAN_RECOVERY_DIRECTORY).exists());
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_recovery_rejects_a_tampered_child_before_sqlite_reopens_sidecar() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "ubm-tampered-scan-child-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        private_fixture_directory(&directory);
        let journal = ContinuationJournal::open(
            &directory.join("recording.sqlite"),
            "tampered-scan-child",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 1,
            },
        )
        .unwrap();
        journal.scan_failure_cleanup_retirements().unwrap();

        let child = directory.join(SCAN_RECOVERY_DIRECTORY);
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755)).unwrap();
        let failure = journal.scan_failure_cleanup_retirements().unwrap_err();
        assert_eq!(failure.kind, "storage.permission");
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unreadable_summary_occurrence_retry_is_exact_and_durable() {
        let (journal, db) = diagnostic_test_journal("unreadable-occurrence-retry");
        let original = b"bad\xffsummary";
        db.execute(
            "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped',lost=4",
            [original.as_slice()],
        )
        .unwrap();
        let permit = journal.reserve_diagnostic([73; 16], 128).unwrap();
        let receipt = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Collection,
                error("storage.io", "later"),
                RecordedLoss {
                    items: 2,
                    bytes: Some(7),
                },
            )
            .unwrap();
        assert!(receipt.durable);
        assert!(journal.retry_diagnostic(receipt.id).unwrap().durable);
        let retained: Vec<u8> = db
            .query_row(
                "SELECT CAST(collection_failure AS BLOB) FROM journal WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, original);
        let status = journal.status().unwrap();
        assert_eq!(status["lostRecords"], 4);
        assert_eq!(
            status["collectionFailure"]["pendingRecordedLoss"],
            json!({"items":2,"bytes":7,"persisted":true})
        );
        assert!(
            status["collectionFailure"]
                .get("knownRecordedLoss")
                .is_none()
        );
    }

    #[test]
    fn unreadable_summary_distinct_occurrences_survive_cold_exact_retry() {
        let (journal, db) = diagnostic_test_journal("unreadable-distinct-cold-retry");
        let path = PathBuf::from(db.path().unwrap());
        let original = b"bad\xffsummary";
        db.execute(
            "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped',lost=4",
            [original.as_slice()],
        )
        .unwrap();
        let first = journal.reserve_diagnostic([74; 16], 128).unwrap();
        let second = journal.reserve_diagnostic([74; 16], 128).unwrap();
        let first = journal
            .record_diagnostic(
                first,
                DiagnosticCategory::Collection,
                error("storage.io", "first"),
                RecordedLoss {
                    items: 2,
                    bytes: Some(7),
                },
            )
            .unwrap();
        let second = journal
            .record_diagnostic(
                second,
                DiagnosticCategory::Collection,
                error("storage.io", "second"),
                RecordedLoss {
                    items: 3,
                    bytes: None,
                },
            )
            .unwrap();
        assert!(first.durable && second.durable);
        let payload = journal.prepare_diagnostic(first.id).unwrap().payload;
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert!(
            reopened
                .retry_diagnostic_exact(first.id, &payload)
                .unwrap()
                .durable
        );
        assert_eq!(
            reopened
                .retry_diagnostic_exact(first.id, "different")
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        let status = reopened.status().unwrap();
        assert_eq!(status["lostRecords"], 4);
        assert_eq!(
            status["collectionFailure"]["pendingRecordedLoss"],
            json!({"items":5,"persisted":true})
        );
        assert!(
            status["collectionFailure"]
                .get("knownRecordedLoss")
                .is_none()
        );
        assert_eq!(
            reopened.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
        assert_eq!(
            reopened.diagnostic_page(first.id, 0, 4096).unwrap().bytes,
            payload.as_bytes()
        );
    }

    #[test]
    fn production_loss_retry_after_ambiguous_commit_is_idempotent() {
        let (journal, db) = diagnostic_test_journal("production-loss-ambiguous");
        let original = b"bad\xffsummary";
        db.execute(
            "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped',lost=4",
            [original.as_slice()],
        )
        .unwrap();
        let failure = error("storage.io", "later");
        journal.mark_recorded_collection_loss(
            &failure,
            RecordedLoss {
                items: 2,
                bytes: Some(7),
            },
        );
        let (id, payload): (Vec<u8>, String) = db
            .query_row(
                "SELECT occurrence,payload FROM diagnostic_ledger",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let id: DiagnosticId = id.try_into().unwrap();
        // Model a successful commit whose response was lost: the producer
        // retains exactly the same attempt and must resolve it before retry.
        journal
            .failures
            .separate_occurrences
            .lock()
            .unwrap()
            .push(DiagnosticEntry {
                id,
                category: DiagnosticCategory::Collection,
                failure: failure.clone(),
                loss: RecordedLoss {
                    items: 2,
                    bytes: Some(7),
                },
                durable: false,
                persistence_failure: Some(error("storage.io", "ambiguous commit")),
            });
        assert_eq!(
            journal.status().unwrap()["collectionFailure"]["pendingRecordedLoss"],
            json!({"items":2,"bytes":7,"persisted":true})
        );
        journal.mark_recorded_collection_loss(
            &failure,
            RecordedLoss {
                items: 3,
                bytes: None,
            },
        );
        journal.mark_recorded_collection_loss(&failure, RecordedLoss::default());
        assert_eq!(
            journal.status().unwrap()["collectionFailure"]["pendingRecordedLoss"],
            json!({"items":5,"persisted":true})
        );
        assert_eq!(
            db.query_row(
                "SELECT payload FROM diagnostic_ledger WHERE occurrence=?1",
                [id.as_slice()],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            payload
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM diagnostic_ledger", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            journal.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
    }

    #[test]
    fn production_loss_rolls_back_and_retries_the_same_occurrence() {
        let (journal, db) = diagnostic_test_journal("production-loss-rollback");
        db.execute(
            "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped',lost=4",
            [b"bad\xffsummary".as_slice()],
        )
        .unwrap();
        db.execute_batch("CREATE TRIGGER reject_loss BEFORE INSERT ON diagnostic_separate_loss BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
        let failure = error("storage.io", "later");
        journal.mark_recorded_collection_loss(
            &failure,
            RecordedLoss {
                items: 2,
                bytes: Some(7),
            },
        );
        let attempt = journal.failures.separate_occurrences.lock().unwrap()[0].clone();
        assert_eq!(
            db.query_row("SELECT count(*) FROM diagnostic_ledger", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            journal.status().unwrap()["collectionFailure"]["pendingRecordedLoss"],
            json!({"items":2,"bytes":7,"persisted":false})
        );
        db.execute_batch("DROP TRIGGER reject_loss").unwrap();
        journal.mark_recorded_collection_loss(&failure, RecordedLoss::default());
        assert_eq!(
            journal.status().unwrap()["collectionFailure"]["pendingRecordedLoss"],
            json!({"items":2,"bytes":7,"persisted":true})
        );
        let payload: String = db
            .query_row(
                "SELECT payload FROM diagnostic_ledger WHERE occurrence=?1",
                [attempt.id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(payload, diagnostic_payload(&attempt));
    }

    #[test]
    fn refused_loss_remains_owned_and_blocks_clear_and_summary_retry() {
        let (journal, db) = diagnostic_test_journal("refused-loss-owned");
        db.execute_batch("CREATE TRIGGER reject_occurrence BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'refused'); END;").unwrap();
        let failure = error("storage.io", "lost source value");
        journal.mark_recorded_collection_loss(
            &failure,
            RecordedLoss {
                items: 2,
                bytes: Some(7),
            },
        );
        let id = journal.failures.separate_occurrences.lock().unwrap()[0].id;
        journal.mark_collection_failure(&failure);
        assert!(!journal.failures.can_discard());
        assert_eq!(journal.clear().unwrap_err().kind, "storage.busy");
        assert_eq!(
            journal.diagnostic_occurrences_page(None, 8).unwrap(),
            vec![id]
        );
        assert!(
            !journal
                .diagnostic_occurrence_page(id, 0, 4096)
                .unwrap()
                .durable
        );
    }

    #[test]
    fn readable_loss_ambiguous_retry_preserves_identity_and_counts_once() {
        let (journal, db) = diagnostic_test_journal("readable-ambiguous-retry");
        let failure = error("storage.io", "source loss");
        let loss = RecordedLoss {
            items: 2,
            bytes: Some(7),
        };
        journal.mark_recorded_collection_loss(&failure, loss);
        let (id, payload): (Vec<u8>, String) = db
            .query_row(
                "SELECT occurrence,payload FROM diagnostic_ledger",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let id: DiagnosticId = id.try_into().unwrap();
        journal
            .failures
            .separate_occurrences
            .lock()
            .unwrap()
            .push(DiagnosticEntry {
                id,
                category: DiagnosticCategory::Collection,
                failure,
                loss,
                durable: false,
                persistence_failure: Some(error("storage.io", "ambiguous commit")),
            });
        let page = journal.diagnostic_occurrence_page(id, 0, 4096).unwrap();
        assert!(page.durable);
        assert_eq!(page.bytes, payload.as_bytes());
        journal.retry_production_loss(id).unwrap();
        journal.retry_production_loss(id).unwrap();
        assert_eq!(
            journal.status().unwrap()["collectionFailure"]["knownRecordedLoss"],
            json!({"items":2,"bytes":7})
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM diagnostic_ledger", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn delayed_loss_retry_cannot_reinsert_a_reclaimed_occurrence() {
        let (journal, db) = diagnostic_test_journal("loss-retry-reclaimed");
        let failure = error("storage.io", "source loss");
        let loss = RecordedLoss {
            items: 2,
            bytes: Some(7),
        };
        journal.mark_recorded_collection_loss(&failure, loss);
        let (id, payload): (Vec<u8>, String) = db
            .query_row(
                "SELECT occurrence,payload FROM diagnostic_ledger",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        db.execute(
            "INSERT INTO diagnostic_owners VALUES(?1,0,'reclaimed',?2)",
            params![&id, &payload],
        )
        .unwrap();
        db.execute("DELETE FROM diagnostic_ledger WHERE occurrence=?1", [&id])
            .unwrap();
        let id: DiagnosticId = id.try_into().unwrap();
        journal
            .failures
            .separate_occurrences
            .lock()
            .unwrap()
            .push(DiagnosticEntry {
                id,
                category: DiagnosticCategory::Collection,
                failure,
                loss,
                durable: false,
                persistence_failure: None,
            });
        assert_eq!(
            journal.retry_production_loss(id).unwrap_err().kind,
            "storage.reclaimed"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM diagnostic_ledger", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            journal.failures.separate_occurrences.lock().unwrap().len(),
            1
        );
    }

    #[test]
    fn ambiguous_loss_retry_rejects_a_conflicting_identity_without_changing_evidence() {
        let (journal, db) = diagnostic_test_journal("loss-retry-conflict");
        let original = b"bad\xffsummary";
        db.execute(
            "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped'",
            [original.as_slice()],
        )
        .unwrap();
        let failure = error("storage.io", "source loss");
        let loss = RecordedLoss {
            items: 2,
            bytes: Some(7),
        };
        journal.mark_recorded_collection_loss(&failure, loss);
        let (id, payload): (Vec<u8>, String) = db
            .query_row(
                "SELECT occurrence,payload FROM diagnostic_ledger",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let id: DiagnosticId = id.try_into().unwrap();
        journal
            .failures
            .separate_occurrences
            .lock()
            .unwrap()
            .push(DiagnosticEntry {
                id,
                category: DiagnosticCategory::Collection,
                failure,
                loss: RecordedLoss {
                    items: 3,
                    bytes: Some(7),
                },
                durable: false,
                persistence_failure: None,
            });
        assert_eq!(
            journal.retry_production_loss(id).unwrap_err().kind,
            "storage.identity"
        );
        assert_eq!(
            db.query_row(
                "SELECT payload FROM diagnostic_ledger WHERE occurrence=?1",
                [id.as_slice()],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            payload
        );
        assert_eq!(
            journal.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
        assert_eq!(
            journal.failures.separate_occurrences.lock().unwrap().len(),
            1
        );
    }

    #[test]
    fn diagnostic_ack_waits_for_ambiguous_production_loss_reconciliation() {
        let (journal, db) = diagnostic_test_journal("ack-pending-production-loss");
        let failure = error("storage.io", "source loss");
        let loss = RecordedLoss {
            items: 2,
            bytes: Some(7),
        };
        journal.mark_recorded_collection_loss(&failure, loss);
        let (id, payload): (Vec<u8>, String) = db
            .query_row(
                "SELECT occurrence,payload FROM diagnostic_ledger",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let id: DiagnosticId = id.try_into().unwrap();
        journal
            .failures
            .separate_occurrences
            .lock()
            .unwrap()
            .push(DiagnosticEntry {
                id,
                category: DiagnosticCategory::Collection,
                failure,
                loss,
                durable: false,
                persistence_failure: Some(error("storage.io", "ambiguous commit")),
            });
        assert_eq!(
            journal
                .acknowledge_diagnostic(id, &payload)
                .unwrap_err()
                .kind,
            "storage.busy"
        );
        assert!(
            !db.query_row(
                "SELECT acknowledged FROM diagnostic_ledger WHERE occurrence=?1",
                [id.as_slice()],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
        );
        journal.retry_production_loss(id).unwrap();
        journal.acknowledge_diagnostic(id, &payload).unwrap();
    }

    #[test]
    fn status_does_not_discard_ambiguous_attempt_before_retry_resolves_report() {
        let (journal, db) = diagnostic_test_journal("status-ambiguous-loss");
        let failure = error("storage.io", "source loss");
        let loss = RecordedLoss {
            items: 2,
            bytes: Some(7),
        };
        journal.mark_recorded_collection_loss(&failure, loss);
        let id: Vec<u8> = db
            .query_row("SELECT occurrence FROM diagnostic_ledger", [], |row| {
                row.get(0)
            })
            .unwrap();
        let id: DiagnosticId = id.try_into().unwrap();
        journal
            .failures
            .separate_occurrences
            .lock()
            .unwrap()
            .push(DiagnosticEntry {
                id,
                category: DiagnosticCategory::Collection,
                failure,
                loss,
                durable: false,
                persistence_failure: Some(error("storage.io", "ambiguous commit")),
            });
        {
            let mut report = journal.failures.collection_failure.lock().unwrap();
            let report = report.as_mut().unwrap();
            report["persisted"] = json!(false);
            report["persistenceFailure"] = diagnostic(&error("storage.io", "ambiguous commit"));
        }
        assert_eq!(
            journal.status().unwrap()["collectionFailure"]["knownRecordedLoss"],
            json!({"items":2,"bytes":7})
        );
        journal.retry_production_loss(id).unwrap();
        let status = journal.status().unwrap();
        assert_eq!(status["collectionFailure"]["persisted"], true);
        assert!(
            status["collectionFailure"]
                .get("persistenceFailure")
                .is_none()
        );
        assert_eq!(
            status["collectionFailure"]["knownRecordedLoss"],
            json!({"items":2,"bytes":7})
        );
    }

    #[test]
    fn retrying_distinct_pending_losses_resolves_first_failure_report() {
        let (journal, db) = diagnostic_test_journal("retry-distinct-loss-reports");
        db.execute_batch("CREATE TRIGGER reject_loss_occurrence BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'refused'); END;").unwrap();
        journal.mark_recorded_collection_loss(
            &error("storage.io", "first"),
            RecordedLoss {
                items: 2,
                bytes: Some(7),
            },
        );
        journal.mark_recorded_collection_loss(
            &error("storage.io", "second"),
            RecordedLoss {
                items: 3,
                bytes: Some(8),
            },
        );
        let ids = journal.diagnostic_occurrences_page(None, 8).unwrap();
        assert_eq!(ids.len(), 2);
        let first = journal.failures.separate_occurrences.lock().unwrap()[0].id;
        let second = journal.failures.separate_occurrences.lock().unwrap()[1].id;
        db.execute_batch("DROP TRIGGER reject_loss_occurrence")
            .unwrap();
        journal.retry_production_loss(first).unwrap();
        journal.retry_production_loss(second).unwrap();
        let status = journal.status().unwrap();
        assert_eq!(status["collectionFailure"]["detail"], "first");
        assert_eq!(status["collectionFailure"]["persisted"], true);
        assert_eq!(
            status["collectionFailure"]["knownRecordedLoss"],
            json!({"items":5,"bytes":15})
        );
    }

    #[test]
    fn unreadable_summary_later_loss_survives_cold_reopen() {
        let (journal, db) = diagnostic_test_journal("unreadable-loss-cold-reopen");
        let path = PathBuf::from(db.path().unwrap());
        let original = b"bad\xffsummary";
        db.execute(
            "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped',lost=4",
            [original.as_slice()],
        )
        .unwrap();
        journal.mark_recorded_collection_loss(
            &error("storage.io", "later"),
            RecordedLoss {
                items: 2,
                bytes: Some(7),
            },
        );
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        let status = reopened.status().unwrap();
        assert_eq!(status["lostRecords"], 4);
        assert_eq!(
            status["collectionFailure"]["pendingRecordedLoss"],
            json!({"items":2,"bytes":7,"persisted":true})
        );
        assert!(
            status["collectionFailure"]
                .get("knownRecordedLoss")
                .is_none()
        );
        assert_eq!(
            reopened.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
    }

    #[test]
    fn migrated_summary_bytes_survive_loss_update_and_reopen() {
        let (journal, db) = diagnostic_test_journal("summary-loss-reopen");
        let path = PathBuf::from(db.path().unwrap());
        let identity = journal.file_identity;
        let original = b"{ \"kind\":\"storage.io\", \"detail\":\"legacy\\u0020text\", \"operation\":\"append\", \"sqliteExtendedCode\":null, \"sqliteCode\":null, \"persisted\":true, \"knownRecordedLoss\":{\"items\":2} }";
        db.execute(
            "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped',lost=2",
            [original.as_slice()],
        )
        .unwrap();
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        reopened.mark_recorded_collection_loss(
            &error("storage.io", "later"),
            RecordedLoss {
                items: 3,
                bytes: None,
            },
        );
        assert_eq!(reopened.status().unwrap()["lostRecords"], 5);
        assert_eq!(
            reopened.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
        let identity = reopened.file_identity;
        drop(reopened);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert_eq!(
            reopened.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
    }

    #[test]
    fn invalid_utf8_summary_does_not_prevent_cold_open_or_raw_export() {
        let (journal, db) = diagnostic_test_journal("summary-invalid-utf8");
        let path = PathBuf::from(db.path().unwrap());
        let identity = journal.file_identity;
        let original = b"{invalid\xffsummary}";
        db.execute(
            "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped'",
            [original.as_slice()],
        )
        .unwrap();
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert_eq!(
            reopened.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
        assert_eq!(
            reopened.status().unwrap()["collectionFailure"]["kind"],
            "storage.diagnostic-unreadable"
        );
        reopened.mark_recorded_collection_loss(
            &error("storage.io", "later"),
            RecordedLoss {
                items: 1,
                bytes: None,
            },
        );
        assert_eq!(
            reopened.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
        let status = reopened.status().unwrap();
        assert_eq!(status["lostRecords"], 0);
        assert_eq!(status["collectionFailure"]["persisted"], true);
        assert_eq!(
            status["collectionFailure"]["retainedSummary"]["kind"],
            "storage.diagnostic-unreadable"
        );
    }

    #[test]
    fn occurrence_loss_preserves_original_summary_bytes() {
        let (journal, db) = diagnostic_test_journal("summary-occurrence-loss");
        let original = b"{ \"kind\":\"storage.io\", \"detail\":\"legacy\\u0020text\", \"operation\":\"append\", \"sqliteExtendedCode\":null, \"sqliteCode\":null, \"persisted\":true, \"knownRecordedLoss\":{\"items\":2} }";
        db.execute(
            "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped',lost=2",
            [original.as_slice()],
        )
        .unwrap();
        let permit = journal.reserve_diagnostic([82; 16], 256).unwrap();
        let receipt = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Collection,
                error("storage.io", "later"),
                RecordedLoss {
                    items: 3,
                    bytes: None,
                },
            )
            .unwrap();
        assert!(receipt.durable);
        assert_eq!(journal.status().unwrap()["lostRecords"], 5);
        assert_eq!(
            journal.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
    }

    #[test]
    fn failed_summary_snapshot_rolls_back_loss_update() {
        let (journal, db) = diagnostic_test_journal("summary-snapshot-failure");
        let original = b"{ \"kind\":\"storage.io\", \"detail\":\"legacy\", \"operation\":\"append\", \"sqliteExtendedCode\":null, \"sqliteCode\":null, \"persisted\":true, \"knownRecordedLoss\":{\"items\":2} }";
        db.execute(
            "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped',lost=2",
            [original.as_slice()],
        )
        .unwrap();
        db.execute_batch("CREATE TRIGGER reject_summary BEFORE INSERT ON diagnostic_summary_original BEGIN SELECT RAISE(ABORT, 'no snapshot'); END;").unwrap();
        journal.mark_recorded_collection_loss(
            &error("storage.io", "later"),
            RecordedLoss {
                items: 3,
                bytes: None,
            },
        );
        assert_eq!(
            journal.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
        let status = journal.status().unwrap();
        assert_eq!(status["lostRecords"], 2);
        assert_eq!(status["collectionFailure"]["persisted"], false);
        db.execute_batch("DROP TRIGGER reject_summary").unwrap();
        journal
            .mark_recorded_collection_loss(&error("storage.io", "later"), RecordedLoss::default());
        assert_eq!(journal.status().unwrap()["lostRecords"], 5);
        assert_eq!(
            journal.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
    }

    #[test]
    fn synthetic_repair_retains_invalid_utf8_candidate_and_summary() {
        let (journal, db) = diagnostic_test_journal("summary-invalid-synthetic");
        let path = PathBuf::from(db.path().unwrap());
        let identity = journal.file_identity;
        let original = b"invalid\xffsummary";
        let candidate = b"{\"category\":\"legacy\",\"report\":\"invalid\xffsummary\"}";
        db.execute(
            "UPDATE journal SET collection_failure=CAST(?1 AS TEXT)",
            [original.as_slice()],
        )
        .unwrap();
        db.execute("INSERT INTO diagnostic_ledger(occurrence,payload) VALUES(zeroblob(40),CAST(?1 AS TEXT))", [candidate.as_slice()]).unwrap();
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert_eq!(
            reopened.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
        assert_eq!(
            reopened.diagnostic_page([0; 40], 0, 4096).unwrap().bytes,
            candidate
        );
        for status in [reopened.status().unwrap(), reopened.stop().unwrap()] {
            assert_eq!(
                status["runtimeFailure"]["kind"],
                "storage.diagnostic-unreadable"
            );
            assert_eq!(status["runtimeFailure"]["source"], "occurrence");
            assert_eq!(status["runtimeFailure"]["payloadBytes"], candidate.len());
            assert_eq!(
                status["collectionFailure"]["kind"],
                "storage.diagnostic-unreadable"
            );
        }
        assert_eq!(
            reopened.diagnostic_page([0; 40], 0, 4096).unwrap().bytes,
            candidate
        );
    }

    #[test]
    fn malformed_summaries_preserve_bytes_and_report_pending_loss_separately() {
        let malformed = [
            br#"{"kind":"storage.io"}"#.as_slice(),
            br#"{"kind":"storage.io","detail":"old","operation":"append","sqliteExtendedCode":null,"sqliteCode":null,"persisted":true,"knownRecordedLoss":{"items":"unknown"}}"#.as_slice(),
            b"{invalid\xffsummary}".as_slice(),
        ];
        for (index, original) in malformed.into_iter().enumerate() {
            let (journal, db) = diagnostic_test_journal(&format!("malformed-projection-{index}"));
            db.execute(
                "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped',lost=4",
                [original],
            )
            .unwrap();
            assert_eq!(
                journal.status().unwrap()["collectionFailure"]["kind"],
                "storage.diagnostic-unreadable"
            );
            journal.mark_recorded_collection_loss(
                &error("storage.io", "later"),
                RecordedLoss {
                    items: 2,
                    bytes: Some(7),
                },
            );
            for status in [journal.status().unwrap(), journal.stop().unwrap()] {
                assert_eq!(status["lostRecords"], 4);
                assert_eq!(
                    status["collectionFailure"]["retainedSummary"]["kind"],
                    "storage.diagnostic-unreadable"
                );
                assert_eq!(
                    status["collectionFailure"]["pendingRecordedLoss"],
                    json!({"items":2,"bytes":7,"persisted":true})
                );
                assert!(
                    status["collectionFailure"]
                        .get("knownRecordedLoss")
                        .is_none()
                );
            }
            let retained: Vec<u8> = db
                .query_row(
                    "SELECT CAST(collection_failure AS BLOB) FROM journal WHERE id=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(retained, original);
        }
    }

    #[test]
    fn unreadable_summary_reports_pending_loss_without_claiming_a_total() {
        let (journal, db) = diagnostic_test_journal("pending-unreadable-summary");
        let original = b"bad\xffsummary";
        db.execute(
            "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped',lost=4",
            [original.as_slice()],
        )
        .unwrap();
        journal.mark_recorded_collection_loss(
            &error("storage.io", "later"),
            RecordedLoss {
                items: 2,
                bytes: Some(7),
            },
        );
        for status in [journal.status().unwrap(), journal.stop().unwrap()] {
            assert_eq!(status["lostRecords"], 4);
            assert_eq!(
                status["collectionFailure"]["pendingRecordedLoss"],
                json!({"items":2,"bytes":7,"persisted":true})
            );
            assert!(
                status["collectionFailure"]
                    .get("knownRecordedLoss")
                    .is_none()
            );
        }
        assert_eq!(
            journal.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
        journal.mark_recorded_collection_loss(
            &error("storage.io", "later"),
            RecordedLoss {
                items: 3,
                bytes: None,
            },
        );
        journal
            .mark_recorded_collection_loss(&error("storage.io", "retry"), RecordedLoss::default());
        assert_eq!(
            journal.status().unwrap()["collectionFailure"]["pendingRecordedLoss"],
            json!({"items":5,"persisted":true})
        );
        assert_eq!(
            journal.diagnostic_summary_page(0, 4096).unwrap().bytes,
            original
        );
    }

    #[test]
    fn malformed_occurrence_json_and_shape_remain_readable_as_raw_bytes() {
        for (index, payload) in [
            br#"{not json}"#.as_slice(),
            br#"{"category":"runtime","failure":{"kind":"storage.io"}}"#.as_slice(),
            br#"{"category":"collection","failure":{"kind":"storage.io","detail":"","operation":"append","sqliteExtendedCode":null,"sqliteCode":null},"loss":{"items":0,"bytes":0}}"#.as_slice(),
        ].into_iter().enumerate() {
            let (journal, db) = diagnostic_test_journal(&format!("malformed-occurrence-{index}"));
            let id = [index as u8 + 81; 40];
            db.execute("INSERT INTO diagnostic_ledger(occurrence,payload) VALUES(?1,CAST(?2 AS TEXT))", params![id.as_slice(), payload]).unwrap();
            for status in [journal.status().unwrap(), journal.stop().unwrap()] {
                assert_eq!(status["runtimeFailure"]["kind"], "storage.diagnostic-unreadable");
                assert_eq!(status["runtimeFailure"]["payloadBytes"], payload.len());
            }
            assert_eq!(journal.diagnostic_page(id, 0, 4096).unwrap().bytes, payload);
        }
    }

    #[test]
    fn collection_failure_does_not_decode_or_copy_unreadable_historical_summary() {
        for (index, original) in [b"bad\xffbytes".to_vec(), vec![b'x'; 1024 * 1024]]
            .into_iter()
            .enumerate()
        {
            let (journal, db) =
                diagnostic_test_journal_with_quota(&format!("failure-summary-{index}"), 8 << 20);
            db.execute(
                "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped'",
                [original.as_slice()],
            )
            .unwrap();
            journal.mark_collection_failure(&error("storage.io", "new failure"));
            let status = journal.status().unwrap();
            assert_eq!(status["collectionFailure"]["persisted"], false);
            assert_eq!(
                status["collectionFailure"]["persistenceFailure"]["kind"],
                if index == 0 {
                    "storage.corrupt"
                } else {
                    "storage.full"
                }
            );
            assert_eq!(
                status["collectionFailure"]["retainedSummary"]["payloadBytes"],
                original.len()
            );
            assert_eq!(
                journal.diagnostic_summary_page(0, 4096).unwrap().bytes,
                original[..original.len().min(4096)]
            );
        }
    }

    #[test]
    fn synthetic_repair_compares_exact_escaped_bytes_and_keeps_other_rows() {
        for (label, matches) in [("exact", true), ("different", false)] {
            let (journal, db) = diagnostic_test_journal_with_quota(label, 8 << 20);
            let path = PathBuf::from(db.path().unwrap());
            let identity = journal.file_identity;
            let summary = format!("{}é\\\"\n\0{}", "x".repeat(8191), "y".repeat(70000));
            let synthetic = json!({"category":"legacy", "report": if matches { summary.clone() } else { format!("{summary}changed") }}).to_string();
            db.execute("UPDATE journal SET collection_failure=?1", [&summary])
                .unwrap();
            db.execute(
                "INSERT INTO diagnostic_ledger(occurrence,payload) VALUES(zeroblob(40),?1)",
                [&synthetic],
            )
            .unwrap();
            drop(journal);
            drop(db);
            shared_journals().lock().unwrap().remove(&identity);
            let reopened = ContinuationJournal::open(
                &path,
                "ledger",
                &json!({}),
                JournalQuota {
                    max_bytes: 8 << 20,
                    max_records: 10,
                },
            )
            .unwrap();
            let retained: i64 = reopened
                .connection()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM diagnostic_ledger WHERE occurrence=zeroblob(40)",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(retained, i64::from(!matches));
            assert_eq!(
                reopened.diagnostic_summary_page(0, 100).unwrap().bytes,
                summary.as_bytes()[..100]
            );
        }
    }

    #[test]
    fn v5_upgrade_preserves_pending_claim_and_legacy_report_bytes() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-v6-migration-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal = ContinuationJournal::open(&path, "migration", &json!({}), quota).unwrap();
        journal
            .append(&json!({}), &json!({"previous":"receipt"}))
            .unwrap();
        let previous = journal.prepare(1, 4096).unwrap();
        let previous_token = previous["token"].as_str().unwrap().to_owned();
        let receipt = journal.acknowledge(&previous_token).unwrap();
        journal
            .append(&json!({"raw":"é"}), &json!({"payload":"\\u0000"}))
            .unwrap();
        journal
            .begin_evidence_transfer("migration-evidence")
            .unwrap();
        let evidence = journal
            .prepare_evidence_transfer("migration-evidence", 1, 4096)
            .unwrap();
        let claim = journal.prepare(1, 4096).unwrap();
        drop(journal);
        let db = Connection::open(&path).unwrap();
        db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE IF EXISTS diagnostic_ledger; PRAGMA user_version=5;").unwrap();
        let report = "{ \"kind\":\"storage.io\", \"detail\":\"legacy\\u0020text\", \"knownRecordedLoss\":{\"items\":2} }";
        db.execute(
            "UPDATE journal SET collection_failure=?1,phase='stopped',lost=2",
            [report],
        )
        .unwrap();
        let before: (String, String) = db
            .query_row(
                "SELECT (SELECT body FROM records LIMIT 1),collection_failure FROM journal",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(db);
        let reopened = ContinuationJournal::open(&path, "migration", &json!({}), quota).unwrap();
        assert_eq!(reopened.prepare(1, 4096).unwrap(), claim);
        assert_eq!(
            reopened
                .prepare_evidence_transfer("migration-evidence", 1, 4096)
                .unwrap(),
            evidence
        );
        assert_eq!(reopened.acknowledge(&previous_token).unwrap(), receipt);
        let db = Connection::open(&path).unwrap();
        let version: i64 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let after: (String, String) = db
            .query_row(
                "SELECT (SELECT body FROM records LIMIT 1),collection_failure FROM journal",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(
            db.query_row("SELECT count(*) FROM diagnostic_ledger", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn v5_upgrade_schema_conflict_rolls_back_without_touching_claim() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-v6-rollback-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal = ContinuationJournal::open(&path, "rollback", &json!({}), quota).unwrap();
        journal
            .append(&json!({}), &json!({"value":"retained"}))
            .unwrap();
        let claim = journal.prepare(1, 4096).unwrap();
        drop(journal);
        let db = Connection::open(&path).unwrap();
        db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE IF EXISTS diagnostic_ledger; CREATE TABLE diagnostic_ledger(unexpected BLOB); INSERT INTO diagnostic_ledger VALUES(x'00ff'); PRAGMA user_version=5;").unwrap();
        let original: Vec<u8> = db
            .query_row("SELECT unexpected FROM diagnostic_ledger", [], |row| {
                row.get(0)
            })
            .unwrap();
        drop(db);
        assert!(ContinuationJournal::open(&path, "rollback", &json!({}), quota).is_err());
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            5
        );
        assert_eq!(
            db.query_row("SELECT unexpected FROM diagnostic_ledger", [], |row| row
                .get::<_, Vec<u8>>(0))
                .unwrap(),
            original
        );
        let token: String = db
            .query_row("SELECT pending_token FROM journal", [], |row| row.get(0))
            .unwrap();
        assert_eq!(token, claim["token"].as_str().unwrap());
    }

    #[test]
    fn stale_version_rejects_incompatible_diagnostic_transition_schema_without_rewriting_data() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-transition-schema-conflict-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal =
            ContinuationJournal::open(&path, "transition-schema", &json!({}), quota).unwrap();
        journal
            .append(&json!({}), &json!({"value":"retained"}))
            .unwrap();
        let claim = journal.prepare(1, 4096).unwrap();
        drop(journal);
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "DROP TABLE diagnostic_transitions; CREATE TABLE diagnostic_transitions(occurrence BLOB PRIMARY KEY, action TEXT NOT NULL); PRAGMA user_version=9;",
        )
        .unwrap();
        let original: String = db
            .query_row("SELECT body FROM records WHERE ordinal=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        drop(db);

        let failure = match ContinuationJournal::open(&path, "transition-schema", &json!({}), quota)
        {
            Ok(_) => panic!("incompatible transition schema was accepted"),
            Err(failure) => failure,
        };
        assert_eq!(failure.kind, "storage.schema");
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            9
        );
        assert_eq!(
            db.query_row("SELECT body FROM records WHERE ordinal=1", [], |row| row
                .get::<_, String>(
                0
            ))
            .unwrap(),
            original
        );
        assert_eq!(
            db.query_row("SELECT pending_token FROM journal", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            claim["token"].as_str().unwrap()
        );
    }

    #[test]
    fn stale_version_rejects_named_diagnostic_transition_index_without_rewriting_data() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-transition-index-conflict-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal =
            ContinuationJournal::open(&path, "transition-index", &json!({}), quota).unwrap();
        journal
            .append(&json!({}), &json!({"value":"retained"}))
            .unwrap();
        let claim = journal.prepare(1, 4096).unwrap();
        drop(journal);
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE INDEX diagnostic_transitions_action_index ON diagnostic_transitions(action); PRAGMA user_version=9;",
        ).unwrap();
        let original: String = db
            .query_row("SELECT body FROM records WHERE ordinal=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        drop(db);

        let failure = match ContinuationJournal::open(&path, "transition-index", &json!({}), quota)
        {
            Ok(_) => panic!("named transition index was accepted"),
            Err(failure) => failure,
        };
        assert_eq!(failure.kind, "storage.schema");
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            9
        );
        assert_eq!(
            db.query_row("SELECT body FROM records WHERE ordinal=1", [], |row| row
                .get::<_, String>(
                0
            ))
            .unwrap(),
            original
        );
        assert_eq!(
            db.query_row("SELECT pending_token FROM journal", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            claim["token"].as_str().unwrap()
        );
    }

    #[test]
    fn v5_upgrade_preserves_ordered_pending_and_retry_identity_bytes() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-v6-ordered-migration-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 4 << 20,
            max_records: 1,
        };
        let journal =
            ContinuationJournal::open(&path, "ordered-migration", &json!({}), quota).unwrap();
        journal.begin_ordered("producer", "collection").unwrap();
        journal
            .bind_ordered_generation("producer", "counter", "1")
            .unwrap();
        journal
            .append_ordered(
                "producer",
                "collection",
                "counter",
                "1",
                "first",
                &json!({}),
                &json!({"value":"é"}),
            )
            .unwrap();
        assert_eq!(
            journal
                .append_ordered(
                    "producer",
                    "collection",
                    "counter",
                    "1",
                    "second",
                    &json!({}),
                    &json!({"value":"pending"})
                )
                .unwrap_err()
                .kind,
            "storage.full"
        );
        drop(journal);
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; PRAGMA user_version=5;",
        )
        .unwrap();
        let before: (Vec<u8>, Vec<u8>) = db.query_row("SELECT (SELECT CAST(payload AS BLOB) FROM ordered_pending),(SELECT CAST(payload AS BLOB) FROM ordered_identities LIMIT 1)", [], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        drop(db);
        let journal =
            ContinuationJournal::open(&path, "ordered-migration", &json!({}), quota).unwrap();
        let db = Connection::open(&path).unwrap();
        let after: (Vec<u8>, Vec<u8>) = db.query_row("SELECT (SELECT CAST(payload AS BLOB) FROM ordered_pending),(SELECT CAST(payload AS BLOB) FROM ordered_identities LIMIT 1)", [], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(after, before);
        assert_eq!(
            journal
                .append_ordered(
                    "producer",
                    "collection",
                    "counter",
                    "1",
                    "first",
                    &json!({}),
                    &json!({"value":"é"})
                )
                .unwrap()["ordinal"],
            1
        );
    }

    #[test]
    fn durable_diagnostic_retry_and_clear_after_cold_reopen() {
        let (journal, db) = diagnostic_test_journal("cold-retry");
        let path = PathBuf::from(db.path().unwrap());
        let id = journal
            .record_diagnostic(
                journal.reserve_diagnostic([22; 16], 128).unwrap(),
                DiagnosticCategory::Collection,
                error("storage.io", "cold retained"),
                RecordedLoss {
                    items: 3,
                    bytes: None,
                },
            )
            .unwrap()
            .id;
        let prepared = journal.prepare_diagnostic(id).unwrap();
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert_eq!(
            reopened.retry_diagnostic(id).unwrap_err().kind,
            "storage.identity"
        );
        assert_eq!(
            reopened
                .retry_diagnostic_exact(id, "different")
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert!(
            reopened
                .retry_diagnostic_exact(id, &prepared.payload)
                .unwrap()
                .durable
        );
        assert_eq!(reopened.clear().unwrap_err().kind, "storage.busy");
        assert_eq!(reopened.status().unwrap()["lostRecords"], 3);
    }

    #[test]
    fn diagnostic_ack_and_retirement_pin_exact_payload_across_reopen() {
        let (journal, db) = diagnostic_test_journal("receipt");
        let path = PathBuf::from(db.path().unwrap());
        let id = journal
            .record_diagnostic(
                journal.reserve_diagnostic([23; 16], 128).unwrap(),
                DiagnosticCategory::Runtime,
                error("storage.io", "retain exactly"),
                RecordedLoss {
                    items: 7,
                    bytes: None,
                },
            )
            .unwrap()
            .id;
        let prepared = journal.prepare_diagnostic(id).unwrap();
        assert!(prepared.durable);
        assert_eq!(
            journal
                .acknowledge_diagnostic(id, "different")
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert_eq!(
            journal
                .acknowledge_diagnostic(id, &prepared.payload)
                .unwrap(),
            id
        );
        assert_eq!(
            journal
                .acknowledge_diagnostic(id, &prepared.payload)
                .unwrap(),
            id
        );
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert_eq!(reopened.prepare_diagnostic(id).unwrap(), prepared);
        assert_eq!(
            reopened
                .acknowledge_diagnostic(id, &prepared.payload)
                .unwrap(),
            id
        );
        assert_eq!(
            reopened.retire_diagnostic(id, &prepared.payload).unwrap(),
            id
        );
        assert_eq!(
            reopened.retire_diagnostic(id, &prepared.payload).unwrap(),
            id
        );
        assert_eq!(
            reopened
                .acknowledge_diagnostic(id, &prepared.payload)
                .unwrap(),
            id
        );
        reopened
            .confirm_diagnostic_retirement(id, &prepared.payload)
            .unwrap();
        assert_eq!(
            reopened.prepare_diagnostic(id).unwrap_err().kind,
            "storage.identity"
        );
    }

    #[test]
    fn volatile_diagnostic_cannot_be_acknowledged_or_retired() {
        let (journal, db) = diagnostic_test_journal("volatile-receipt");
        db.execute_batch("CREATE TRIGGER reject_diagnostic BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
        let id = journal
            .record_diagnostic(
                journal.reserve_diagnostic([24; 16], 128).unwrap(),
                DiagnosticCategory::Runtime,
                error("storage.io", "volatile"),
                RecordedLoss::default(),
            )
            .unwrap()
            .id;
        let prepared = journal.prepare_diagnostic(id).unwrap();
        assert!(!prepared.durable);
        assert_eq!(
            journal
                .acknowledge_diagnostic(id, &prepared.payload)
                .unwrap_err()
                .kind,
            "storage.busy"
        );
        assert_eq!(
            journal
                .retire_diagnostic(id, &prepared.payload)
                .unwrap_err()
                .kind,
            "storage.busy"
        );
        db.execute_batch("DROP TRIGGER reject_diagnostic").unwrap();
        assert!(journal.retry_diagnostic(id).unwrap().durable);
        assert_eq!(
            journal
                .acknowledge_diagnostic(id, &prepared.payload)
                .unwrap(),
            id
        );
    }

    #[test]
    fn diagnostic_ack_write_failure_keeps_exact_occurrence_for_retry() {
        let (journal, db) = diagnostic_test_journal("ack-failure");
        let id = journal
            .record_diagnostic(
                journal.reserve_diagnostic([27; 16], 128).unwrap(),
                DiagnosticCategory::Runtime,
                error("storage.io", "retained"),
                RecordedLoss::default(),
            )
            .unwrap()
            .id;
        let prepared = journal.prepare_diagnostic(id).unwrap();
        db.execute_batch("CREATE TRIGGER reject_ack BEFORE UPDATE OF acknowledged ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
        assert!(
            journal
                .acknowledge_diagnostic(id, &prepared.payload)
                .is_err()
        );
        assert_eq!(
            db.query_row(
                "SELECT acknowledged FROM diagnostic_ledger WHERE occurrence=?1",
                [id.as_slice()],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(journal.prepare_diagnostic(id).unwrap(), prepared);
        db.execute_batch("DROP TRIGGER reject_ack").unwrap();
        assert_eq!(
            journal
                .acknowledge_diagnostic(id, &prepared.payload)
                .unwrap(),
            id
        );
    }

    #[test]
    fn producer_retirement_can_precede_consumer_acknowledgement() {
        let (journal, db) = diagnostic_test_journal("retire-before-ack");
        let path = PathBuf::from(db.path().unwrap());
        let id = journal
            .record_diagnostic(
                journal.reserve_diagnostic([28; 16], 128).unwrap(),
                DiagnosticCategory::Runtime,
                error("storage.io", "independent"),
                RecordedLoss::default(),
            )
            .unwrap()
            .id;
        let prepared = journal.prepare_diagnostic(id).unwrap();
        assert_eq!(
            journal.retire_diagnostic(id, &prepared.payload).unwrap(),
            id
        );
        assert_eq!(
            journal.retire_diagnostic(id, &prepared.payload).unwrap(),
            id
        );
        assert_eq!(
            journal
                .confirm_diagnostic_retirement(id, &prepared.payload)
                .unwrap_err()
                .kind,
            "storage.busy"
        );
        assert_eq!(journal.prepare_diagnostic(id).unwrap(), prepared);
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert_eq!(
            reopened.retire_diagnostic(id, &prepared.payload).unwrap(),
            id
        );
        assert_eq!(
            reopened
                .acknowledge_diagnostic(id, &prepared.payload)
                .unwrap(),
            id
        );
        reopened
            .confirm_diagnostic_retirement(id, &prepared.payload)
            .unwrap();
    }

    #[test]
    fn uncertain_retirement_confirmation_cannot_reinsert_and_recount() {
        let (journal, db) = diagnostic_test_journal("ambiguous-retirement");
        let id = journal
            .record_diagnostic(
                journal.reserve_diagnostic([29; 16], 128).unwrap(),
                DiagnosticCategory::Runtime,
                error("storage.io", "retire once"),
                RecordedLoss {
                    items: 3,
                    bytes: Some(5),
                },
            )
            .unwrap()
            .id;
        let prepared = journal.prepare_diagnostic(id).unwrap();
        journal
            .acknowledge_diagnostic(id, &prepared.payload)
            .unwrap();
        journal.retire_diagnostic(id, &prepared.payload).unwrap();
        db.execute_batch("BEGIN; SELECT count(*) FROM diagnostic_ledger;")
            .unwrap();
        assert!(
            journal
                .confirm_diagnostic_retirement(id, &prepared.payload)
                .is_err()
        );
        db.execute_batch("COMMIT").unwrap();
        assert_eq!(journal.prepare_diagnostic(id).unwrap(), prepared);
        assert!(
            journal
                .retry_diagnostic_exact(id, &prepared.payload)
                .unwrap()
                .durable
        );
        // Model the other possible observation after an ambiguous commit: the
        // deletion reached disk even though the caller received an error.
        db.execute(
            "DELETE FROM diagnostic_ledger WHERE occurrence=?1",
            [id.as_slice()],
        )
        .unwrap();
        assert_eq!(
            journal.retry_diagnostic(id).unwrap_err().kind,
            "storage.identity"
        );
        assert_eq!(journal.status().unwrap()["lostRecords"], 3);
    }

    #[test]
    fn legacy_failure_method_does_not_claim_an_occurrence_history() {
        let (journal, _) = diagnostic_test_journal("legacy-summary");
        journal.mark_collection_failure(&error("storage.io", "first"));
        journal.mark_collection_failure(&error("storage.full", "second"));
        assert_eq!(
            journal.status().unwrap()["collectionFailure"]["detail"],
            "first"
        );
        assert!(journal.diagnostic_occurrences().unwrap().is_empty());
    }

    #[test]
    fn diagnostic_quota_counts_retained_rows_after_cold_reopen() {
        let (journal, db) = diagnostic_test_journal("cold-quota");
        let path = PathBuf::from(db.path().unwrap());
        let id = journal
            .record_diagnostic(
                journal.reserve_diagnostic([25; 16], 128).unwrap(),
                DiagnosticCategory::Runtime,
                error("storage.io", "quota"),
                RecordedLoss::default(),
            )
            .unwrap()
            .id;
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let mut reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        let payload_limit = DIAGNOSTIC_PAYLOAD_LIMIT;
        let retained_charge = reopened.prepare_diagnostic(id).unwrap().payload.len() + 4096;
        let charge = payload_limit * 6 + 4096;
        reopened.quota.max_bytes = OVERHEAD + 2 * (charge + retained_charge - 1) as u64;
        assert_eq!(
            reopened
                .reserve_diagnostic([25; 16], payload_limit)
                .unwrap_err()
                .kind,
            "storage.full"
        );
        let prepared = reopened.prepare_diagnostic(id).unwrap();
        reopened
            .acknowledge_diagnostic(id, &prepared.payload)
            .unwrap();
        reopened.retire_diagnostic(id, &prepared.payload).unwrap();
        reopened
            .confirm_diagnostic_retirement(id, &prepared.payload)
            .unwrap();
        assert!(reopened.reserve_diagnostic([25; 16], payload_limit).is_ok());
    }

    #[test]
    fn diagnostic_snapshot_includes_durable_and_volatile_occurrences_across_reopen() {
        let (journal, db) = diagnostic_test_journal("snapshot");
        let path = PathBuf::from(db.path().unwrap());
        let one = journal
            .record_diagnostic(
                journal.reserve_diagnostic([26; 16], 128).unwrap(),
                DiagnosticCategory::Runtime,
                error("storage.io", "one"),
                RecordedLoss::default(),
            )
            .unwrap()
            .id;
        db.execute_batch("CREATE TRIGGER reject_diagnostic BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
        let two = journal
            .record_diagnostic(
                journal.reserve_diagnostic([26; 16], 128).unwrap(),
                DiagnosticCategory::Runtime,
                error("storage.io", "two"),
                RecordedLoss::default(),
            )
            .unwrap()
            .id;
        let snapshot = journal.diagnostic_occurrences().unwrap();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.contains(&one) && snapshot.contains(&two));
        db.execute_batch("DROP TRIGGER reject_diagnostic").unwrap();
        assert!(journal.retry_diagnostic(two).unwrap().durable);
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert_eq!(reopened.diagnostic_occurrences().unwrap(), snapshot);
        assert!(reopened.prepare_diagnostic(one).unwrap().durable);
        assert!(reopened.prepare_diagnostic(two).unwrap().durable);
    }

    #[test]
    fn cold_status_reports_earliest_durable_runtime_occurrence() {
        let (journal, db) = diagnostic_test_journal("runtime-cold-status");
        let path = PathBuf::from(db.path().unwrap());
        let first = journal.reserve_diagnostic([255; 16], 128).unwrap();
        let second = journal.reserve_diagnostic([0; 16], 128).unwrap();
        journal
            .record_diagnostic(
                first,
                DiagnosticCategory::Runtime,
                error("storage.io", "first runtime"),
                RecordedLoss::default(),
            )
            .unwrap();
        journal
            .record_diagnostic(
                second,
                DiagnosticCategory::Runtime,
                error("storage.io", "second runtime"),
                RecordedLoss::default(),
            )
            .unwrap();
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        let status = reopened.status().unwrap();
        assert_eq!(status["runtimeFailure"]["detail"], "first runtime");
        assert_eq!(status["runtimeFailure"]["persisted"], true);
    }

    #[test]
    fn recorded_admission_retains_loss_when_connection_lock_is_poisoned() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-poisoned-loss-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let journal = ContinuationJournal::open(
            &directory.join("recording.sqlite"),
            "poison",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = journal.test_lock_connection();
            panic!("inject interrupted journal writer");
        }));
        let failure = journal
            .append_recorded_batch(
                &[(&json!({}), &json!({"value":1}))],
                &[RecordedLoss {
                    items: 1,
                    bytes: Some(7),
                }],
                RecordedLoss::default(),
            )
            .unwrap_err();
        assert_eq!(failure.operation, "append");
        let retained = journal.failures.separate_occurrences.lock().unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0].loss,
            RecordedLoss {
                items: 1,
                bytes: Some(7)
            }
        );
        let report = journal
            .failures
            .collection_failure
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        assert_eq!(report["detail"], failure.detail);
        assert_eq!(report["persisted"], false);
    }

    fn diagnostic_test_journal(label: &str) -> (ContinuationJournal, Connection) {
        diagnostic_test_journal_with_quota(label, 1 << 20)
    }

    fn diagnostic_test_journal_with_quota(
        label: &str,
        max_bytes: u64,
    ) -> (ContinuationJournal, Connection) {
        let directory = std::env::temp_dir().join(format!(
            "ubm-ledger-{label}-{}-{:?}-{:?}",
            std::process::id(),
            std::thread::current().id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let journal = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes,
                max_records: 10,
            },
        )
        .unwrap();
        let injector = Connection::open(&path).unwrap();
        (journal, injector)
    }

    #[test]
    fn peer_failure_recovery_handoff_is_atomic_and_requires_explicit_release() {
        let (journal, _) = diagnostic_test_journal("peer-failure-recovery");
        let identity = [41_u8; 96];
        let occurrence = json!({
            "version": 1,
            "descriptor": {"source": "opaque"},
            "recipients": [{"lease": "opaque", "acknowledged": false}],
            "producerAck": "not-started"
        });
        let evidence = b"exact native evidence\x00\xff";

        journal
            .stage_peer_failure_recovery(&identity, &occurrence, evidence)
            .unwrap();
        // An identical retry is safe, but a second producer cannot overwrite
        // the source-time recipient cut or bytes.
        journal
            .stage_peer_failure_recovery(&identity, &occurrence, evidence)
            .unwrap();
        assert_eq!(
            journal
                .stage_peer_failure_recovery(&identity, &occurrence, b"different")
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        let staged = journal.peer_failure_recovery(&identity).unwrap().unwrap();
        assert_eq!(staged.occurrence, occurrence);
        assert_eq!(staged.evidence, evidence);
        assert_eq!(staged.state, PeerFailureRecoveryState::Staged);

        assert_eq!(
            journal.begin_peer_failure_source_ack(&identity).unwrap(),
            PeerFailureRecoveryState::SourceAckInFlight
        );
        // Lost responses replay the same durable in-flight state; they do not
        // create a second native source ACK.
        assert_eq!(
            journal.begin_peer_failure_source_ack(&identity).unwrap(),
            PeerFailureRecoveryState::SourceAckInFlight
        );
        assert_eq!(
            journal.confirm_peer_failure_source_ack(&identity).unwrap(),
            PeerFailureRecoveryState::SourceAckCommitted
        );
        assert!(journal.peer_failure_recovery(&identity).unwrap().is_some());
        journal.release_peer_failure_recovery(&identity).unwrap();
        assert!(journal.peer_failure_recovery(&identity).unwrap().is_none());
    }

    #[test]
    fn peer_failure_recovery_survives_reopen_without_promoting_source_ack() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-peer-failure-reopen-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let identity = [17_u8; 96];
        let occurrence = json!({"version":1,"recipientCut":["opaque"]});
        let evidence = b"exact evidence after process loss";
        let journal = ContinuationJournal::open(&path, "reopen", &json!({}), quota).unwrap();
        journal
            .stage_peer_failure_recovery(&identity, &occurrence, evidence)
            .unwrap();
        assert_eq!(
            journal.begin_peer_failure_source_ack(&identity).unwrap(),
            PeerFailureRecoveryState::SourceAckInFlight
        );
        drop(journal);

        let reopened = ContinuationJournal::open(&path, "reopen", &json!({}), quota).unwrap();
        let retained = reopened.peer_failure_recovery(&identity).unwrap().unwrap();
        assert_eq!(retained.occurrence, occurrence);
        assert_eq!(retained.evidence, evidence);
        assert_eq!(retained.state, PeerFailureRecoveryState::SourceAckInFlight);
        assert_eq!(
            reopened.confirm_peer_failure_source_ack(&identity).unwrap(),
            PeerFailureRecoveryState::SourceAckCommitted
        );
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn diagnostic_admission_respects_reclaimable_receipt_boundary() {
        let (journal, _) = diagnostic_test_journal_with_quota("receipt-boundary", 4 << 20);
        assert_eq!(
            journal
                .reserve_diagnostic([42; 16], DIAGNOSTIC_PAYLOAD_LIMIT + 1)
                .unwrap_err()
                .kind,
            "storage.full"
        );
        let permit = journal
            .reserve_diagnostic([42; 16], DIAGNOSTIC_PAYLOAD_LIMIT)
            .unwrap();
        let mut failure = error("", "");
        failure.sqlite_code = Some("\0".repeat(DIAGNOSTIC_PAYLOAD_LIMIT - failure.operation.len()));
        let id = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Runtime,
                failure,
                RecordedLoss::default(),
            )
            .unwrap()
            .id;
        let payload = journal.prepare_diagnostic(id).unwrap().payload;
        journal.acknowledge_diagnostic(id, &payload).unwrap();
        journal.retire_diagnostic(id, &payload).unwrap();
        journal.confirm_diagnostic_retirement(id, &payload).unwrap();
        journal.confirm_diagnostic_retirement(id, &payload).unwrap();
        assert!(journal.diagnostic_page(id, 0, 1024).unwrap().reclaimed);
    }

    #[test]
    fn old_oversized_diagnostic_confirmation_preserves_exact_retry() {
        // The injected historical payload must fit the journal's actual page
        // limit; quota-constrained migration requires independent coverage.
        let (journal, db) = diagnostic_test_journal_with_quota("oversized-receipt", 4 << 20);
        let path = PathBuf::from(db.path().unwrap());
        let id = [43; 40];
        let mut failure = error("storage.io", "");
        failure.sqlite_code = Some(format!(
            "{}🫀",
            "x".repeat(DIAGNOSTIC_PAYLOAD_LIMIT * 6 + 4096)
        ));
        let payload = diagnostic_payload(&DiagnosticEntry {
            id,
            category: DiagnosticCategory::Runtime,
            failure,
            loss: RecordedLoss::default(),
            durable: true,
            persistence_failure: None,
        });
        db.execute(
            "INSERT INTO diagnostic_ledger(occurrence,payload) VALUES(?1,?2)",
            params![id.as_slice(), &payload],
        )
        .unwrap();
        let export = |journal: &ContinuationJournal, reclaimed: bool| {
            let mut bytes = Vec::new();
            loop {
                let page = journal
                    .diagnostic_page(id, bytes.len() as u64, 16_381)
                    .unwrap();
                assert_eq!(page.total_bytes, payload.len() as u64);
                assert_eq!(page.reclaimed, reclaimed);
                bytes.extend(page.bytes);
                if bytes.len() == payload.len() {
                    break;
                }
            }
            assert_eq!(bytes, payload.as_bytes());
        };
        export(&journal, false);
        let unicode_start = payload.find('🫀').unwrap();
        let part = journal
            .diagnostic_page(id, unicode_start as u64 + 1, 1)
            .unwrap();
        assert_eq!(
            part.bytes,
            payload.as_bytes()[unicode_start + 1..unicode_start + 2]
        );
        assert_eq!(
            journal.diagnostic_page(id, 0, 0).unwrap_err().kind,
            "argument.invalid"
        );
        assert_eq!(
            journal
                .diagnostic_page(id, 0, DIAGNOSTIC_PAGE_LIMIT + 1)
                .unwrap_err()
                .kind,
            "argument.invalid"
        );
        assert_eq!(
            journal
                .diagnostic_page(id, payload.len() as u64 + 1, 1)
                .unwrap_err()
                .kind,
            "argument.invalid"
        );
        journal.acknowledge_diagnostic(id, &payload).unwrap();
        journal.retire_diagnostic(id, &payload).unwrap();
        db.execute_batch("CREATE TRIGGER reject_confirmation BEFORE INSERT ON diagnostic_owners BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
        assert!(journal.confirm_diagnostic_retirement(id, &payload).is_err());
        export(&journal, false);
        db.execute_batch("DROP TRIGGER reject_confirmation")
            .unwrap();
        journal.confirm_diagnostic_retirement(id, &payload).unwrap();
        journal.confirm_diagnostic_retirement(id, &payload).unwrap();
        assert_eq!(
            journal
                .confirm_diagnostic_retirement(id, "different")
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        export(&journal, true);
        let owners = journal.diagnostic_owners().unwrap();
        assert_eq!(
            owners,
            vec![DiagnosticOwner {
                id,
                payload_limit: 0,
                state: DiagnosticOwnerState::Reclaimed,
                bound: false
            }]
        );
        assert_eq!(
            journal
                .retry_diagnostic_exact(id, &payload)
                .unwrap_err()
                .kind,
            "storage.reclaimed"
        );
        assert_eq!(
            journal
                .replay_owned_diagnostic(id, &payload)
                .unwrap_err()
                .kind,
            "storage.reclaimed"
        );
        assert_eq!(
            journal
                .retry_diagnostic_exact(id, "different")
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert_eq!(journal.clear().unwrap_err().kind, "storage.busy");
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 4 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        export(&reopened, true);
        reopened
            .confirm_diagnostic_retirement(id, &payload)
            .unwrap();
        assert_eq!(
            reopened
                .release_diagnostic_confirmation(id, "different")
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        export(&reopened, true);
        reopened
            .release_diagnostic_confirmation(id, &payload)
            .unwrap();
        assert_eq!(
            reopened.diagnostic_page(id, 0, 1).unwrap_err().kind,
            "storage.identity"
        );
        reopened.stop().unwrap();
        assert_eq!(reopened.clear().unwrap()["cleared"], true);
    }

    #[test]
    fn durable_diagnostic_owner_survives_process_and_confirmation_retry() {
        const CHILD: &str = "UBM_DIAGNOSTIC_OWNER_CHILD";
        const PHASE: &str = "UBM_DIAGNOSTIC_OWNER_PHASE";
        let entry = DiagnosticEntry {
            id: [91; 40],
            category: DiagnosticCategory::Runtime,
            failure: error("storage.io", "retained by producer"),
            loss: RecordedLoss {
                items: 3,
                bytes: Some(5),
            },
            durable: false,
            persistence_failure: None,
        };
        if let Some(path) = std::env::var_os(CHILD) {
            let journal = ContinuationJournal::open(
                Path::new(&path),
                "ledger",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 10,
                },
            )
            .unwrap();
            if std::env::var_os(PHASE).is_none() {
                journal
                    .reserve_durable_diagnostic(entry.id, 128, &diagnostic_payload(&entry))
                    .unwrap();
                let injector = Connection::open(Path::new(&path)).unwrap();
                injector.execute_batch("CREATE TRIGGER reject_owned BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
                assert!(
                    journal
                        .replay_owned_diagnostic(entry.id, &diagnostic_payload(&entry))
                        .is_err()
                );
                injector
                    .execute_batch("DROP TRIGGER reject_owned;")
                    .unwrap();
            } else if std::env::var_os(PHASE).as_deref() == Some(std::ffi::OsStr::new("replay")) {
                assert!(
                    journal
                        .replay_owned_diagnostic(entry.id, &diagnostic_payload(&entry))
                        .unwrap()
                        .durable
                );
                assert_eq!(journal.status().unwrap()["lostRecords"], 3);
            } else {
                journal
                    .confirm_diagnostic_retirement(entry.id, &diagnostic_payload(&entry))
                    .unwrap();
            }
            return;
        }
        let (journal, db) = diagnostic_test_journal("durable-owner-process");
        let path = PathBuf::from(db.path().unwrap());
        drop(journal);
        drop(db);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact").arg("continuation_journal::authority_tests::durable_diagnostic_owner_survives_process_and_confirmation_retry")
            .env(CHILD, &path).status().unwrap();
        assert!(child.success());
        let journal = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert_eq!(journal.clear().unwrap_err().kind, "storage.busy");
        let mut different = entry.clone();
        different.failure.detail = "different valid payload";
        assert_eq!(
            journal
                .replay_owned_diagnostic(different.id, &diagnostic_payload(&different))
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert!(
            journal
                .replay_owned_diagnostic(entry.id, &diagnostic_payload(&entry))
                .unwrap()
                .durable
        );
        assert_eq!(
            journal
                .replay_owned_diagnostic(different.id, &diagnostic_payload(&different))
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert!(
            journal
                .replay_owned_diagnostic(entry.id, &diagnostic_payload(&entry))
                .unwrap()
                .durable
        );
        assert_eq!(journal.status().unwrap()["lostRecords"], 3);
        drop(journal);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact").arg("continuation_journal::authority_tests::durable_diagnostic_owner_survives_process_and_confirmation_retry")
            .env(CHILD, &path).env(PHASE, "replay").status().unwrap();
        assert!(child.success());
        let journal = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        let payload = diagnostic_payload(&entry);
        journal.acknowledge_diagnostic(entry.id, &payload).unwrap();
        journal.retire_diagnostic(entry.id, &payload).unwrap();
        drop(journal);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact").arg("continuation_journal::authority_tests::durable_diagnostic_owner_survives_process_and_confirmation_retry")
            .env(CHILD, &path).env(PHASE, "confirm").status().unwrap();
        assert!(child.success());
        let journal = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        journal
            .confirm_diagnostic_retirement(entry.id, &payload)
            .unwrap();
        assert_eq!(
            journal
                .replay_owned_diagnostic(entry.id, &diagnostic_payload(&entry))
                .unwrap_err()
                .kind,
            "storage.reclaimed"
        );
        assert_eq!(
            journal
                .confirm_diagnostic_retirement([92; 40], &payload)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        journal
            .release_diagnostic_confirmation(entry.id, &payload)
            .unwrap();
        assert_eq!(
            journal
                .confirm_diagnostic_retirement(entry.id, &payload)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
    }

    #[test]
    fn durable_diagnostic_owner_capacity_and_conflicts_fail_closed() {
        let (journal, _) = diagnostic_test_journal("durable-owner-bounds");
        let payload = diagnostic_payload(&DiagnosticEntry {
            id: [1; 40],
            category: DiagnosticCategory::Runtime,
            failure: error("storage.io", "bound"),
            loss: RecordedLoss::default(),
            durable: false,
            persistence_failure: None,
        });
        for value in 1..=64 {
            journal
                .reserve_durable_diagnostic([value; 40], 128, &payload)
                .unwrap();
        }
        journal
            .reserve_durable_diagnostic([1; 40], 128, &payload)
            .unwrap();
        assert_eq!(
            journal
                .reserve_durable_diagnostic([1; 40], 129, &payload)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert_eq!(
            journal
                .reserve_durable_diagnostic([65; 40], 128, &payload)
                .unwrap_err()
                .kind,
            "storage.full"
        );
        let unknown = DiagnosticEntry {
            id: [66; 40],
            category: DiagnosticCategory::Runtime,
            failure: error("storage.io", "unknown"),
            loss: RecordedLoss::default(),
            durable: false,
            persistence_failure: None,
        };
        assert_eq!(
            journal
                .replay_owned_diagnostic(unknown.id, &diagnostic_payload(&unknown))
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert_eq!(
            journal
                .release_bound_diagnostic_owner([1; 40], "{}")
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        journal
            .release_bound_diagnostic_owner([1; 40], &payload)
            .unwrap();
        assert_eq!(
            journal
                .release_bound_diagnostic_owner([1; 40], &payload)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert_eq!(
            journal
                .replay_owned_diagnostic([1; 40], &payload)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        journal
            .reserve_durable_diagnostic([65; 40], 128, &payload)
            .unwrap();
    }

    #[test]
    fn review_owner_discovery_across_process_and_quota() {
        const CHILD: &str = "UBM_REVIEW_OWNER_DISCOVERY_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let journal = ContinuationJournal::open(
                Path::new(&path),
                "ledger",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 10,
                },
            )
            .unwrap();
            let owners = journal.diagnostic_owners().unwrap();
            assert_eq!(owners.len(), 64);
            assert_eq!(
                owners
                    .iter()
                    .filter(|o| o.state == DiagnosticOwnerState::Reserved && !o.bound)
                    .count(),
                1
            );
            assert_eq!(
                owners
                    .iter()
                    .filter(|o| o.state == DiagnosticOwnerState::Committed)
                    .count(),
                1
            );
            assert_eq!(
                owners
                    .iter()
                    .filter(|o| o.state == DiagnosticOwnerState::Reclaimed)
                    .count(),
                1
            );
            assert_eq!(journal.clear().unwrap_err().kind, "storage.busy");
            for owner in owners {
                if !owner.bound {
                    journal.release_unused_diagnostic_owner(owner.id).unwrap();
                    continue;
                }
                let page = journal
                    .diagnostic_page(owner.id, 0, DIAGNOSTIC_PAGE_LIMIT)
                    .unwrap();
                assert_eq!(page.bytes.len() as u64, page.total_bytes);
                let payload = String::from_utf8(page.bytes).unwrap();
                match owner.state {
                    DiagnosticOwnerState::Reserved => journal
                        .release_bound_diagnostic_owner(owner.id, &payload)
                        .unwrap(),
                    DiagnosticOwnerState::Committed => {
                        journal.acknowledge_diagnostic(owner.id, &payload).unwrap();
                        journal.retire_diagnostic(owner.id, &payload).unwrap();
                        journal
                            .confirm_diagnostic_retirement(owner.id, &payload)
                            .unwrap();
                        journal
                            .release_diagnostic_confirmation(owner.id, &payload)
                            .unwrap();
                    }
                    DiagnosticOwnerState::Reclaimed => journal
                        .release_diagnostic_confirmation(owner.id, &payload)
                        .unwrap(),
                }
            }
            assert!(journal.diagnostic_owners().unwrap().is_empty());
            journal.stop().unwrap();
            assert_eq!(journal.clear().unwrap()["cleared"], true);
            return;
        }
        let (journal, db) = diagnostic_test_journal("review-owner-discovery");
        let path = PathBuf::from(db.path().unwrap());
        let entry = DiagnosticEntry {
            id: [1; 40],
            category: DiagnosticCategory::Runtime,
            failure: error("storage.io", "preserve"),
            loss: RecordedLoss::default(),
            durable: false,
            persistence_failure: None,
        };
        let payload = diagnostic_payload(&entry);
        for n in 1..=63 {
            journal
                .reserve_durable_diagnostic([n; 40], 128, &payload)
                .unwrap();
        }
        journal.replay_owned_diagnostic([1; 40], &payload).unwrap();
        journal.replay_owned_diagnostic([2; 40], &payload).unwrap();
        journal.acknowledge_diagnostic([2; 40], &payload).unwrap();
        journal.retire_diagnostic([2; 40], &payload).unwrap();
        journal
            .confirm_diagnostic_retirement([2; 40], &payload)
            .unwrap();
        db.execute(
            "INSERT INTO diagnostic_owners VALUES(?1,128,'reserved',NULL)",
            [[64_u8; 40].as_slice()],
        )
        .unwrap();
        assert_eq!(
            journal
                .reserve_durable_diagnostic([65; 40], 128, &payload)
                .unwrap_err()
                .kind,
            "storage.full"
        );
        drop(journal);
        drop(db);
        let child = std::process::Command::new(std::env::current_exe().unwrap()).arg("--exact").arg("continuation_journal::authority_tests::review_owner_discovery_across_process_and_quota").env(CHILD, &path).status().unwrap();
        assert!(child.success());
    }

    #[test]
    fn review_exact_retry_of_normal_receipt_after_reopen() {
        let (journal, db) = diagnostic_test_journal("review-normal-receipt");
        let path = PathBuf::from(db.path().unwrap());
        let id = journal
            .record_diagnostic(
                journal.reserve_diagnostic([7; 16], 128).unwrap(),
                DiagnosticCategory::Runtime,
                error("storage.io", "original"),
                RecordedLoss::default(),
            )
            .unwrap()
            .id;
        let payload = journal.prepare_diagnostic(id).unwrap().payload;
        let mut value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        value["failure"]["detail"] = json!("different");
        let mismatch = value.to_string();
        assert!(validate_diagnostic_payload(&mismatch).is_ok());
        assert_ne!(payload, mismatch);
        journal.acknowledge_diagnostic(id, &payload).unwrap();
        journal.retire_diagnostic(id, &payload).unwrap();
        journal.confirm_diagnostic_retirement(id, &payload).unwrap();
        assert_eq!(
            journal
                .retry_diagnostic_exact(id, &payload)
                .unwrap_err()
                .kind,
            "storage.reclaimed"
        );
        assert_eq!(
            journal
                .retry_diagnostic_exact(id, &mismatch)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let journal = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert_eq!(
            journal
                .retry_diagnostic_exact(id, &payload)
                .unwrap_err()
                .kind,
            "storage.reclaimed"
        );
        assert_eq!(
            journal
                .retry_diagnostic_exact(id, &mismatch)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
    }

    #[test]
    fn review_exact_reservation_retry_after_interleaved_replay_and_reopen() {
        let (journal, db) = diagnostic_test_journal("review-reservation-retry");
        let path = PathBuf::from(db.path().unwrap());
        let id = [76; 40];
        let entry = DiagnosticEntry {
            id,
            category: DiagnosticCategory::Runtime,
            failure: error("storage.io", "original"),
            loss: RecordedLoss::default(),
            durable: false,
            persistence_failure: None,
        };
        let payload = diagnostic_payload(&entry);
        let mut other = entry.clone();
        other.failure.detail = "different";
        let mismatch = diagnostic_payload(&other);
        assert!(validate_diagnostic_payload(&mismatch).is_ok());
        assert_eq!(
            journal
                .reserve_durable_diagnostic(id, 128, &payload)
                .unwrap(),
            DiagnosticOwnerState::Reserved
        );
        journal.replay_owned_diagnostic(id, &payload).unwrap();
        assert_eq!(
            journal
                .reserve_durable_diagnostic(id, 128, &payload)
                .unwrap(),
            DiagnosticOwnerState::Committed
        );
        assert_eq!(
            journal
                .reserve_durable_diagnostic(id, 128, &mismatch)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert_eq!(
            journal
                .reserve_durable_diagnostic(id, 129, &payload)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let journal = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert_eq!(
            journal
                .reserve_durable_diagnostic(id, 128, &payload)
                .unwrap(),
            DiagnosticOwnerState::Committed
        );
        journal.acknowledge_diagnostic(id, &payload).unwrap();
        journal.retire_diagnostic(id, &payload).unwrap();
        journal.confirm_diagnostic_retirement(id, &payload).unwrap();
        assert_eq!(
            journal
                .reserve_durable_diagnostic(id, 128, &payload)
                .unwrap(),
            DiagnosticOwnerState::Reclaimed
        );
        assert_eq!(
            journal
                .reserve_durable_diagnostic(id, 128, &mismatch)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert_eq!(
            journal
                .reserve_durable_diagnostic(id, 129, &payload)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
    }

    #[test]
    fn review_owner_discovery_fails_closed_over_capacity_without_deleting_evidence() {
        let (journal, db) = diagnostic_test_journal("review-owner-corrupt-count");
        for n in 1..=65 {
            db.execute(
                "INSERT INTO diagnostic_owners VALUES(?1,128,'reserved',NULL)",
                [[n; 40].as_slice()],
            )
            .unwrap();
        }
        assert_eq!(
            journal.diagnostic_owners().unwrap_err().kind,
            "storage.corrupt"
        );
        let count: i64 = db
            .query_row("SELECT count(*) FROM diagnostic_owners", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 65);
        assert_eq!(journal.clear().unwrap_err().kind, "storage.busy");
    }

    #[test]
    fn huge_legacy_diagnostic_has_bounded_exact_lifecycle() {
        let (journal, db) = diagnostic_test_journal_with_quota("huge-exact-lifecycle", 16 << 20);
        let path = PathBuf::from(db.path().unwrap());
        let id = [112; 40];
        let payload = format!("{}🫀end", "x".repeat(2_000_000));
        db.execute(
            "INSERT INTO diagnostic_ledger(occurrence,payload) VALUES(?1,?2)",
            params![id.as_slice(), &payload],
        )
        .unwrap();
        let apply = |journal: &ContinuationJournal, action| {
            let mut exact = journal.begin_diagnostic_exact(id, action).unwrap();
            for chunk in payload.as_bytes().chunks(13_007) {
                exact.compare(chunk).unwrap();
            }
            exact.finish().unwrap();
        };
        {
            let mut exact = journal
                .begin_diagnostic_exact(id, DiagnosticExactAction::Acknowledge)
                .unwrap();
            assert_eq!(
                exact.compare(b"wrong").unwrap_err().kind,
                "storage.identity"
            );
        }
        {
            let mut exact = journal
                .begin_diagnostic_exact(id, DiagnosticExactAction::Acknowledge)
                .unwrap();
            exact.compare(b"x").unwrap();
            assert_eq!(exact.finish().unwrap_err().kind, "storage.identity");
        }
        apply(&journal, DiagnosticExactAction::Acknowledge);
        apply(&journal, DiagnosticExactAction::Retire);
        apply(&journal, DiagnosticExactAction::CheckReplay);
        apply(&journal, DiagnosticExactAction::Confirm);
        assert!(journal.diagnostic_page(id, 0, 1).unwrap().reclaimed);
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 16 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        apply(&reopened, DiagnosticExactAction::Confirm);
        let mut replay = reopened
            .begin_diagnostic_exact(id, DiagnosticExactAction::CheckReplay)
            .unwrap();
        for chunk in payload.as_bytes().chunks(13_007) {
            replay.compare(chunk).unwrap();
        }
        assert_eq!(replay.finish().unwrap_err().kind, "storage.reclaimed");
        {
            let mut wrong = reopened
                .begin_diagnostic_exact(id, DiagnosticExactAction::Release)
                .unwrap();
            for chunk in payload.as_bytes()[..payload.len() - 1].chunks(13_007) {
                wrong.compare(chunk).unwrap();
            }
            assert_eq!(wrong.compare(b"!").unwrap_err().kind, "storage.identity");
            assert_eq!(wrong.finish().unwrap_err().kind, "storage.identity");
        }
        assert!(reopened.diagnostic_page(id, 0, 1).unwrap().reclaimed);
        apply(&reopened, DiagnosticExactAction::Release);
        assert_eq!(
            reopened.diagnostic_page(id, 0, 1).unwrap_err().kind,
            "storage.identity"
        );
    }

    #[test]
    fn diagnostic_chunk_transition_resumes_more_than_eighteen_megabytes_without_replaying_a_full_payload()
     {
        let (journal, db) = diagnostic_test_journal_with_quota("chunked-huge-lifecycle", 128 << 20);
        let id = [113; 40];
        let payload = format!("{}🫀end", "x".repeat(18 * 1024 * 1024 + 1));
        db.execute(
            "INSERT INTO diagnostic_ledger(occurrence,payload) VALUES(?1,?2)",
            params![id.as_slice(), &payload],
        )
        .unwrap();
        let apply = |action, mut offset| {
            while offset < payload.len() as u64 {
                let end_u64 = std::cmp::min(offset + 13_007u64, payload.len() as u64);
                let end = usize::try_from(end_u64).unwrap();
                let result = journal
                    .diagnostic_transition_chunk(
                        id,
                        action,
                        offset,
                        &payload.as_bytes()[offset as usize..end],
                    )
                    .unwrap();
                offset = result.offset;
            }
        };
        let first = journal
            .diagnostic_transition_chunk(
                id,
                DiagnosticExactAction::Acknowledge,
                0,
                &payload.as_bytes()[..13_007],
            )
            .unwrap();
        assert_eq!(first.offset, 13_007);
        assert_eq!(
            journal
                .diagnostic_transition_chunk(
                    id,
                    DiagnosticExactAction::Acknowledge,
                    0,
                    &payload.as_bytes()[..13_007]
                )
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        apply(DiagnosticExactAction::Acknowledge, first.offset);
        apply(DiagnosticExactAction::Retire, 0);
        apply(DiagnosticExactAction::Confirm, 0);
        apply(DiagnosticExactAction::Release, 0);
        assert_eq!(
            journal.diagnostic_page(id, 0, 1).unwrap_err().kind,
            "storage.identity"
        );
    }

    #[test]
    fn volatile_discovery_pages_export_exact_bytes_and_pin_retry() {
        let (journal, db) = diagnostic_test_journal("volatile-scan-export");
        db.execute_batch("CREATE TRIGGER reject_export BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        let permit = journal.reserve_diagnostic([9; 16], 8192).unwrap();
        let mut failure = error("storage.io", "volatile");
        failure.sqlite_code = Some("é\0\\\"".repeat(500));
        let receipt = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Runtime,
                failure,
                RecordedLoss::default(),
            )
            .unwrap();
        assert!(!receipt.durable);
        let expected = diagnostic_payload(&journal.diagnostic_entries().unwrap()[0]).into_bytes();
        std::thread::scope(|scope| {
            let mut scan = journal.begin_diagnostic_occurrences().unwrap();
            assert_eq!(scan.next_page(1).unwrap(), vec![receipt.id]);
            let mut bytes = Vec::new();
            loop {
                let page = scan
                    .diagnostic_page(receipt.id, bytes.len() as u64, 17)
                    .unwrap();
                assert_eq!(page.total_bytes, expected.len() as u64);
                assert!(!page.reclaimed);
                assert!(!page.durable);
                if page.bytes.is_empty() {
                    break;
                }
                bytes.extend(page.bytes);
            }
            assert_eq!(bytes, expected);
            assert_eq!(
                scan.diagnostic_page([1; 40], 0, 1).unwrap_err().kind,
                "storage.identity"
            );
            let (tx, rx) = std::sync::mpsc::channel();
            let journal_ref = &journal;
            scope.spawn(move || {
                tx.send(journal_ref.retry_diagnostic(receipt.id)).unwrap();
            });
            assert!(
                rx.recv_timeout(std::time::Duration::from_millis(30))
                    .is_err()
            );
            drop(scan);
            assert!(
                rx.recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .is_err()
            );
        });
        db.execute_batch("DROP TRIGGER reject_export").unwrap();
        assert!(journal.retry_diagnostic(receipt.id).unwrap().durable);
        let mut scan = journal.begin_diagnostic_occurrences().unwrap();
        assert_eq!(scan.next_page(1).unwrap(), vec![receipt.id]);
        let page = scan.diagnostic_page(receipt.id, 0, 17).unwrap();
        assert!(page.durable);
        assert_eq!(page.bytes, expected[..17]);
        drop(scan);
        let payload = std::str::from_utf8(&expected).unwrap();
        let mut conflicting = expected.clone();
        let last = conflicting.len() - 1;
        conflicting[last] ^= 1;
        db.execute(
            "UPDATE diagnostic_ledger SET payload=?1 WHERE occurrence=?2",
            params![
                std::str::from_utf8(&conflicting).unwrap(),
                receipt.id.as_slice()
            ],
        )
        .unwrap();
        let scan = journal.begin_diagnostic_occurrences().unwrap();
        assert_eq!(
            scan.diagnostic_page(receipt.id, 0, 1).unwrap_err().kind,
            "storage.identity"
        );
        drop(scan);
        db.execute(
            "UPDATE diagnostic_ledger SET payload=?1 WHERE occurrence=?2",
            params![payload, receipt.id.as_slice()],
        )
        .unwrap();
        journal.acknowledge_diagnostic(receipt.id, payload).unwrap();
        journal.retire_diagnostic(receipt.id, payload).unwrap();
        journal
            .confirm_diagnostic_retirement(receipt.id, payload)
            .unwrap();
        assert!(
            journal
                .begin_diagnostic_occurrences()
                .unwrap()
                .next_page(1)
                .unwrap()
                .is_empty()
        );
        let path = PathBuf::from(db.path().unwrap());
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert!(
            reopened
                .begin_diagnostic_occurrences()
                .unwrap()
                .next_page(1)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reopened.diagnostic_page(receipt.id, 0, 17).unwrap().bytes,
            expected[..17]
        );
    }

    #[test]
    fn oversized_unacknowledged_status_and_stop_remain_bounded_and_recoverable() {
        let (journal, db) = diagnostic_test_journal_with_quota("oversized-status", 8 << 20);
        let id = [71; 40];
        let payload = json!({"category":"runtime", "failure":{"kind":"storage.io", "detail":"large".repeat(300_000), "operation":"record", "sqliteCode":null, "sqliteExtendedCode":null}, "loss":{"items":0,"bytes":0}}).to_string();
        db.execute(
            "INSERT INTO diagnostic_ledger(occurrence,payload) VALUES(?1,?2)",
            params![id.as_slice(), payload],
        )
        .unwrap();
        for status in [journal.status().unwrap(), journal.stop().unwrap()] {
            assert_eq!(
                status["runtimeFailure"]["kind"],
                "storage.diagnostic-oversized"
            );
            assert_eq!(status["runtimeFailure"]["payloadBytes"], payload.len());
            assert!(status.to_string().len() < 4096);
        }
        let mut scan = journal.begin_diagnostic_occurrences().unwrap();
        assert_eq!(scan.next_page(1).unwrap(), vec![id]);
        let mut bytes = Vec::new();
        while bytes.len() < payload.len() {
            let page = scan
                .diagnostic_page(id, bytes.len() as u64, 16_384)
                .unwrap();
            bytes.extend(page.bytes);
        }
        assert_eq!(bytes, payload.as_bytes());
        drop(scan);
        let path = PathBuf::from(db.path().unwrap());
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 8 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        assert_eq!(
            reopened.status().unwrap()["runtimeFailure"]["payloadBytes"],
            payload.len()
        );
        for action in [
            DiagnosticExactAction::Acknowledge,
            DiagnosticExactAction::Retire,
            DiagnosticExactAction::Confirm,
        ] {
            let mut exact = reopened.begin_diagnostic_exact(id, action).unwrap();
            for chunk in bytes.chunks(16_384) {
                exact.compare(chunk).unwrap();
            }
            exact.finish().unwrap();
        }
        assert!(reopened.status().unwrap()["runtimeFailure"].is_null());
        assert!(reopened.diagnostic_page(id, 0, 1).unwrap().reclaimed);
        let mut release = reopened
            .begin_diagnostic_exact(id, DiagnosticExactAction::Release)
            .unwrap();
        for chunk in bytes.chunks(16_384) {
            release.compare(chunk).unwrap();
        }
        release.finish().unwrap();
    }

    #[test]
    fn oversized_legacy_summary_is_bounded_and_exportable() {
        let (journal, db) = diagnostic_test_journal_with_quota("oversized-summary", 8 << 20);
        let summary = json!({"kind":"storage.io", "detail":"é".repeat(100_000), "operation":"append", "persisted":true, "knownRecordedLoss":{"items":5,"bytes":10}}).to_string();
        db.execute(
            "UPDATE journal SET collection_failure=?1,phase='stopped',lost=5 WHERE id=1",
            [&summary],
        )
        .unwrap();
        for status in [journal.status().unwrap(), journal.stop().unwrap()] {
            assert_eq!(
                status["collectionFailure"]["kind"],
                "storage.diagnostic-oversized"
            );
            assert_eq!(status["collectionFailure"]["payloadBytes"], summary.len());
            assert_eq!(status["lostRecords"], 5);
            assert!(status.to_string().len() < 4096);
        }
        journal.mark_recorded_collection_loss(
            &error("storage.io", "later"),
            RecordedLoss {
                items: 2,
                bytes: Some(11),
            },
        );
        for status in [journal.status().unwrap(), journal.stop().unwrap()] {
            assert_eq!(status["lostRecords"], 5);
            assert_eq!(
                status["collectionFailure"]["retainedSummary"]["kind"],
                "storage.diagnostic-oversized"
            );
            assert_eq!(
                status["collectionFailure"]["pendingRecordedLoss"],
                json!({"items":2,"bytes":11,"persisted":true})
            );
            assert!(
                status["collectionFailure"]
                    .get("knownRecordedLoss")
                    .is_none()
            );
        }
        let mut bytes = Vec::new();
        while bytes.len() < summary.len() {
            let page = journal
                .diagnostic_summary_page(bytes.len() as u64, 8191)
                .unwrap();
            assert_eq!(page.total_bytes, summary.len() as u64);
            bytes.extend(page.bytes);
        }
        assert_eq!(bytes, summary.as_bytes());
    }

    #[test]
    fn occurrence_scan_fences_retirement_and_restarts_from_current_state() {
        let (journal, mut db) = diagnostic_test_journal_with_quota("page-retirement", 4 << 20);
        let path = PathBuf::from(db.path().unwrap());
        let tx = db.transaction().unwrap();
        for n in 1u32..=300 {
            let mut id = [0u8; 40];
            id[36..].copy_from_slice(&n.to_be_bytes());
            tx.execute(
                "INSERT INTO diagnostic_ledger(occurrence,payload) VALUES(?1,'retained')",
                [id.as_slice()],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let mut first = [0u8; 40];
        first[39] = 1;
        journal.acknowledge_diagnostic(first, "retained").unwrap();
        journal.retire_diagnostic(first, "retained").unwrap();
        std::thread::scope(|scope| {
            let mut scan = journal.begin_diagnostic_occurrences().unwrap();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let journal_ref = &journal;
            scope.spawn(move || {
                started_tx.send(()).unwrap();
                done_tx
                    .send(journal_ref.confirm_diagnostic_retirement(first, "retained"))
                    .unwrap();
            });
            started_rx.recv().unwrap();
            let mut ids = Vec::new();
            loop {
                let page = scan.next_page(19).unwrap();
                if page.is_empty() {
                    break;
                }
                ids.extend(page);
            }
            assert_eq!(ids.len(), 300);
            assert_eq!(ids[0], first);
            assert!(
                done_rx
                    .recv_timeout(std::time::Duration::from_millis(30))
                    .is_err()
            );
            drop(scan);
            done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
        });
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let reopened = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 4 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        let mut scan = reopened.begin_diagnostic_occurrences().unwrap();
        let mut count = 0;
        loop {
            let page = scan.next_page(23).unwrap();
            if page.is_empty() {
                break;
            }
            assert!(!page.contains(&first));
            count += page.len();
        }
        assert_eq!(count, 299);
    }

    #[test]
    fn diagnostic_occurrence_pages_pin_ordered_membership_until_closed() {
        let (journal, mut db) = diagnostic_test_journal_with_quota("occurrence-pages", 8 << 20);
        let tx = db.transaction().unwrap();
        for n in 0u32..1000 {
            let mut id = [0u8; 40];
            id[36..].copy_from_slice(&n.to_be_bytes());
            tx.execute(
                "INSERT INTO diagnostic_ledger(occurrence,payload) VALUES(?1,'{}')",
                [id.as_slice()],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        db.execute_batch("CREATE TRIGGER reject_page_slot BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        let permit = journal.reserve_diagnostic([7; 16], 128).unwrap();
        let volatile = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Runtime,
                error("storage.io", "volatile"),
                RecordedLoss::default(),
            )
            .unwrap();
        assert!(!volatile.durable);
        let mut scan = journal.begin_diagnostic_occurrences().unwrap();
        let mut ids = Vec::new();
        loop {
            let page = scan.next_page(37).unwrap();
            assert!(page.len() <= 37);
            if page.is_empty() {
                break;
            }
            ids.extend(page);
        }
        assert_eq!(ids.len(), 1001);
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(ids.contains(&volatile.id));
        db.busy_timeout(std::time::Duration::ZERO).unwrap();
        assert!(
            db.execute(
                "DELETE FROM diagnostic_ledger WHERE occurrence=?1",
                [ids[0].as_slice()]
            )
            .is_err()
        );
        drop(scan);
        db.execute(
            "DELETE FROM diagnostic_ledger WHERE occurrence=?1",
            [ids[0].as_slice()],
        )
        .unwrap();
        let mut next = journal.begin_diagnostic_occurrences().unwrap();
        assert_ne!(next.next_page(1).unwrap()[0], ids[0]);
    }

    #[test]
    fn durable_reservation_counts_decoded_utf8_failure_bytes() {
        let (journal, _) = diagnostic_test_journal("durable-field-bytes");
        for (n, detail) in ["plain text".repeat(100), "🫀".repeat(200)]
            .into_iter()
            .enumerate()
        {
            let id = [n as u8 + 101; 40];
            let failure = error("storage.io", "");
            let size = failure.kind.len() + detail.len() + failure.operation.len();
            let payload =
                json!({"category":"runtime", "failure": {"kind":failure.kind, "detail":detail,
                "operation":failure.operation, "sqliteCode":null, "sqliteExtendedCode":null},
                "loss":{"items":0,"bytes":0}})
                .to_string();
            assert_eq!(
                journal
                    .reserve_durable_diagnostic(id, size - 1, &payload)
                    .unwrap_err()
                    .kind,
                "storage.full"
            );
            assert_eq!(
                journal
                    .reserve_durable_diagnostic(id, size, &payload)
                    .unwrap(),
                DiagnosticOwnerState::Reserved
            );
        }
        let payload = json!({"category":"runtime", "failure":{"kind":"storage.io", "detail":"x".repeat(70_000),
            "operation":"journal", "sqliteCode":null, "sqliteExtendedCode":null}, "loss":{"items":0,"bytes":0}}).to_string();
        assert_eq!(
            journal
                .reserve_durable_diagnostic([103; 40], DIAGNOSTIC_PAYLOAD_LIMIT, &payload)
                .unwrap_err()
                .kind,
            "storage.full"
        );
        let id = [104; 40];
        let connection = journal.connection().unwrap();
        connection
            .execute(
                "INSERT INTO diagnostic_owners VALUES(?1,?2,'committed',?3)",
                params![id.as_slice(), DIAGNOSTIC_PAYLOAD_LIMIT as i64, &payload],
            )
            .unwrap();
        drop(connection);
        assert_eq!(
            journal
                .reserve_durable_diagnostic(id, DIAGNOSTIC_PAYLOAD_LIMIT, &payload)
                .unwrap(),
            DiagnosticOwnerState::Committed
        );
    }

    #[test]
    fn review_diagnostic_admission_counts_utf8_bytes_not_capacity() {
        let (journal, _) = diagnostic_test_journal("review-allocation");
        let mut failure = error("", "");
        let mut code = String::with_capacity(200_000);
        code.push_str("é\0");
        failure.sqlite_code = Some(code);
        let bytes = failure.kind.len()
            + failure.detail.len()
            + failure.operation.len()
            + failure.sqlite_code.as_ref().unwrap().len();
        let permit = journal.reserve_diagnostic([78; 16], bytes).unwrap();
        let id = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Runtime,
                failure,
                RecordedLoss::default(),
            )
            .unwrap()
            .id;
        assert!(
            journal
                .prepare_diagnostic(id)
                .unwrap()
                .payload
                .contains("\\u0000")
        );
        let permit = journal.reserve_diagnostic([78; 16], bytes - 1).unwrap();
        let mut failure = error("", "");
        failure.sqlite_code = Some("é\0".into());
        let rejected = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Runtime,
                failure,
                RecordedLoss::default(),
            )
            .unwrap_err();
        assert_eq!(rejected.error.kind, "storage.full");
    }

    #[test]
    fn legacy_unbound_diagnostic_owner_cannot_acquire_guessed_payload() {
        let (journal, db) = diagnostic_test_journal("legacy-unbound");
        let id = [77; 40];
        db.execute(
            "INSERT INTO diagnostic_owners VALUES(?1,128,'reserved',NULL)",
            [id.as_slice()],
        )
        .unwrap();
        let path = PathBuf::from(db.path().unwrap());
        let identity = journal.file_identity;
        drop(journal);
        drop(db);
        shared_journals().lock().unwrap().remove(&identity);
        let journal = ContinuationJournal::open(
            &path,
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        let payload = diagnostic_payload(&DiagnosticEntry {
            id,
            category: DiagnosticCategory::Runtime,
            failure: error("storage.io", "unverifiable"),
            loss: RecordedLoss::default(),
            durable: false,
            persistence_failure: None,
        });
        assert_eq!(
            journal
                .replay_owned_diagnostic(id, &payload)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert_eq!(
            journal
                .reserve_durable_diagnostic(id, 128, &payload)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert_eq!(journal.clear().unwrap_err().kind, "storage.busy");
        journal.release_unused_diagnostic_owner(id).unwrap();
        assert_eq!(
            journal
                .replay_owned_diagnostic(id, &payload)
                .unwrap_err()
                .kind,
            "storage.identity"
        );
    }

    #[test]
    fn distinct_failed_loss_diagnostics_remain_owned_when_persistence_fails() {
        let (journal, injector) = diagnostic_test_journal("two-volatile");
        injector.execute_batch("CREATE TRIGGER reject_diagnostic BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
        let first = error("storage.io", "first distinct failure").at("first");
        let second = error("storage.full", "second distinct failure").at("second");
        let first_permit = journal.reserve_diagnostic([1; 16], 128).unwrap();
        let second_permit = journal.reserve_diagnostic([1; 16], 128).unwrap();
        let one = journal
            .record_diagnostic(
                first_permit,
                DiagnosticCategory::Collection,
                first.clone(),
                RecordedLoss {
                    items: 1,
                    bytes: Some(4),
                },
            )
            .unwrap();
        let two = journal
            .record_diagnostic(
                second_permit,
                DiagnosticCategory::Collection,
                second.clone(),
                RecordedLoss {
                    items: 2,
                    bytes: Some(8),
                },
            )
            .unwrap();
        assert!(!one.durable && !two.durable);
        assert_ne!(one.id, two.id);
        let entries = journal.diagnostic_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].failure, first);
        assert_eq!(entries[1].failure, second);
        assert_eq!(
            entries[0].loss,
            RecordedLoss {
                items: 1,
                bytes: Some(4)
            }
        );
        assert_eq!(
            entries[1].loss,
            RecordedLoss {
                items: 2,
                bytes: Some(8)
            }
        );
        injector
            .execute_batch("DROP TRIGGER reject_diagnostic")
            .unwrap();
        assert!(journal.retry_diagnostic(one.id).unwrap().durable);
        assert!(journal.retry_diagnostic(two.id).unwrap().durable);
        assert!(journal.retry_diagnostic(two.id).unwrap().durable);
        assert_eq!(journal.status().unwrap()["lostRecords"], 3);
        assert!(
            journal
                .diagnostic_entries()
                .unwrap()
                .iter()
                .all(|entry| entry.durable)
        );
    }

    #[test]
    fn distinct_pending_loss_does_not_replace_an_existing_durable_diagnostic() {
        let (journal, injector) = diagnostic_test_journal("partial");
        let first = error("storage.io", "durable first failure").at("first");
        let mut second = error("storage.full", "pending second failure").at("second");
        second.sqlite_extended_code = Some(13);
        second.sqlite_code = Some("DiskFull".to_owned());
        let first_permit = journal.reserve_diagnostic([2; 16], 128).unwrap();
        let second_permit = journal.reserve_diagnostic([2; 16], 128).unwrap();
        let one = journal
            .record_diagnostic(
                first_permit,
                DiagnosticCategory::Collection,
                first.clone(),
                RecordedLoss {
                    items: 1,
                    bytes: Some(4),
                },
            )
            .unwrap();
        assert!(one.durable);
        let before: String = injector
            .query_row(
                "SELECT collection_failure FROM journal WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        injector.execute_batch("CREATE TRIGGER reject_second BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
        let two = journal
            .record_diagnostic(
                second_permit,
                DiagnosticCategory::Collection,
                second.clone(),
                RecordedLoss {
                    items: 2,
                    bytes: None,
                },
            )
            .unwrap();
        assert!(!two.durable);
        let after: String = injector
            .query_row(
                "SELECT collection_failure FROM journal WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, after);
        let entries = journal.diagnostic_entries().unwrap();
        assert_eq!(entries[0].failure, first);
        assert_eq!(entries[1].failure, second);
        assert_eq!(entries[1].loss.bytes, None);
        injector
            .execute_batch("DROP TRIGGER reject_second")
            .unwrap();
        assert!(journal.retry_diagnostic(two.id).unwrap().durable);
        assert_eq!(journal.status().unwrap()["lostRecords"], 3);
    }

    #[test]
    fn diagnostic_identity_conflict_and_capacity_refusal_preserve_owned_entries() {
        let (journal, injector) = diagnostic_test_journal("identity");
        let permit = journal.reserve_diagnostic([3; 16], 128).unwrap();
        let receipt = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Runtime,
                error("storage.io", "same error"),
                RecordedLoss::default(),
            )
            .unwrap();
        assert!(receipt.durable);
        let original: String = injector
            .query_row(
                "SELECT payload FROM diagnostic_ledger WHERE occurrence=?1",
                [receipt.id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let mut conflicting: Value = serde_json::from_str(&original).unwrap();
        conflicting["loss"]["bytes"] = json!(null);
        injector
            .execute(
                "UPDATE diagnostic_ledger SET payload=?1 WHERE occurrence=?2",
                params![conflicting.to_string(), receipt.id.as_slice()],
            )
            .unwrap();
        assert_eq!(
            journal.retry_diagnostic(receipt.id).unwrap_err().kind,
            "storage.identity"
        );
        assert_eq!(
            journal.diagnostic_entries().unwrap()[0].failure.detail,
            "same error"
        );
        let oversized = usize::try_from(journal.quota.max_bytes).unwrap();
        assert_eq!(
            journal
                .reserve_diagnostic([3; 16], oversized)
                .unwrap_err()
                .kind,
            "storage.full"
        );
    }

    #[test]
    fn diagnostic_permits_bound_payload_and_release_unused_capacity() {
        let (mut journal, _) = diagnostic_test_journal("capacity");
        let payload_limit = DIAGNOSTIC_PAYLOAD_LIMIT;
        journal.quota.max_bytes = OVERHEAD + 2 * (payload_limit * 6 + 4096 + 4096) as u64;
        let permit = journal.reserve_diagnostic([4; 16], payload_limit).unwrap();
        assert_eq!(
            journal.reserve_diagnostic([4; 16], 1).unwrap_err().kind,
            "storage.full"
        );
        drop(permit);
        let permit = journal.reserve_diagnostic([4; 16], 1).unwrap();
        let rejected = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Runtime,
                error("storage.io", "payload"),
                RecordedLoss::default(),
            )
            .unwrap_err();
        assert_eq!(rejected.error.kind, "storage.full");
        assert_eq!(rejected.failure.detail, "payload");
        assert_eq!(rejected.category, DiagnosticCategory::Runtime);
        assert_eq!(rejected.loss, RecordedLoss::default());
        drop(rejected.permit);
        assert!(journal.reserve_diagnostic([4; 16], payload_limit).is_ok());
    }

    #[test]
    fn identical_diagnostics_from_peers_keep_distinct_identity_and_retry() {
        let (journal, injector) = diagnostic_test_journal("peers");
        let peer = ContinuationJournal::open(
            Path::new(injector.path().unwrap()),
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        injector.execute_batch("CREATE TRIGGER reject_all BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
        let a = journal.reserve_diagnostic([5; 16], 128).unwrap();
        let b = peer.reserve_diagnostic([5; 16], 128).unwrap();
        let first = journal
            .record_diagnostic(
                a,
                DiagnosticCategory::Runtime,
                error("storage.io", "identical"),
                RecordedLoss::default(),
            )
            .unwrap();
        let second = peer
            .record_diagnostic(
                b,
                DiagnosticCategory::Runtime,
                error("storage.io", "identical"),
                RecordedLoss::default(),
            )
            .unwrap();
        assert_ne!(first.id, second.id);
        drop(peer);
        assert_eq!(journal.diagnostic_entries().unwrap().len(), 2);
        assert!(journal.retry_diagnostic(first.id).is_err());
        assert!(
            journal.diagnostic_entries().unwrap()[0]
                .persistence_failure
                .is_some()
        );
        assert!(!journal.can_close());
        injector.execute_batch("DROP TRIGGER reject_all").unwrap();
        assert!(journal.retry_diagnostic(first.id).unwrap().durable);
        assert!(journal.retry_diagnostic(second.id).unwrap().durable);
        assert_eq!(
            injector
                .query_row("SELECT count(*) FROM diagnostic_ledger", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn volatile_collection_entry_fences_append_and_remains_visible() {
        let (journal, injector) = diagnostic_test_journal("fence");
        injector.execute_batch("CREATE TRIGGER reject_diagnostic BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
        let permit = journal.reserve_diagnostic([6; 16], 128).unwrap();
        let receipt = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Collection,
                error("storage.io", "volatile fence"),
                RecordedLoss {
                    items: 2,
                    bytes: Some(7),
                },
            )
            .unwrap();
        assert!(!receipt.durable);
        assert_eq!(
            journal.append(&json!({}), &json!({})).unwrap_err().kind,
            "storage.collection-failed"
        );
        let status = journal.status().unwrap();
        assert_eq!(status["collectionFailure"]["detail"], "volatile fence");
        assert_eq!(
            status["collectionFailure"]["knownRecordedLoss"],
            json!({"items": 2, "bytes": 7})
        );
        assert_eq!(status["accepting"], false);
    }

    #[test]
    fn diagnostic_runtime_failure_is_visible_while_volatile() {
        let (journal, injector) = diagnostic_test_journal("runtime-status");
        injector.execute_batch("CREATE TRIGGER reject_diagnostic BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
        let permit = journal.reserve_diagnostic([10; 16], 128).unwrap();
        let receipt = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Runtime,
                error("storage.io", "runtime volatile"),
                RecordedLoss::default(),
            )
            .unwrap();
        assert!(!receipt.durable);
        let status = journal.status().unwrap();
        assert_eq!(status["runtimeFailure"]["detail"], "runtime volatile");
        assert_eq!(status["runtimeFailure"]["persisted"], false);
    }

    #[test]
    fn diagnostic_failure_publication_holds_shared_append_cut() {
        use std::sync::mpsc;
        let (journal, injector) = diagnostic_test_journal("locked-cut");
        let peer = ContinuationJournal::open(
            Path::new(injector.path().unwrap()),
            "ledger",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        injector.execute_batch("CREATE TRIGGER reject_diagnostic BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
        let permit = journal.reserve_diagnostic([12; 16], 128).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            let mut connection = journal.test_lock_connection();
            let receipt = journal
                .record_diagnostic_locked(
                    Ok(&mut connection),
                    permit,
                    DiagnosticCategory::Collection,
                    error("storage.io", "cut"),
                    RecordedLoss::default(),
                )
                .unwrap();
            ready_tx.send(receipt).unwrap();
            release_rx.recv().unwrap();
        });
        let receipt = ready_rx.recv().unwrap();
        assert!(!receipt.durable);
        assert!(peer.test_shared_connection_busy());
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        assert_eq!(
            peer.append(&json!({}), &json!({})).unwrap_err().kind,
            "storage.collection-failed"
        );
    }

    #[test]
    fn diagnostic_retained_when_connection_lock_is_poisoned() {
        let (journal, _) = diagnostic_test_journal("poison");
        let permit = journal.reserve_diagnostic([11; 16], 128).unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = journal.test_lock_connection();
            panic!("inject interrupted journal writer");
        }));
        let receipt = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Collection,
                error("storage.io", "after poison"),
                RecordedLoss {
                    items: 1,
                    bytes: None,
                },
            )
            .unwrap();
        assert!(!receipt.durable);
        assert!(receipt.persistence_failure.is_some());
        let entry = journal.diagnostic_entries().unwrap().remove(0);
        assert_eq!(entry.failure.detail, "after poison");
        assert_eq!(entry.loss.bytes, None);
        assert!(entry.persistence_failure.is_some());
        assert!(!journal.can_close());
    }

    #[test]
    fn diagnostic_ambiguous_commit_does_not_double_count_status_or_retry() {
        let (journal, _) = diagnostic_test_journal("ambiguous");
        let permit = journal.reserve_diagnostic([7; 16], 128).unwrap();
        let receipt = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Collection,
                error("storage.io", "ambiguous"),
                RecordedLoss {
                    items: 2,
                    bytes: Some(7),
                },
            )
            .unwrap();
        assert!(receipt.durable);
        {
            let mut ledger = journal.shared.diagnostic_ledger.lock().unwrap();
            let entry = ledger.slots[0].entry.as_mut().unwrap();
            entry.durable = false;
            entry.persistence_failure = Some(error("storage.io", "ambiguous commit"));
        }
        let status = journal.status().unwrap();
        assert_eq!(
            status["collectionFailure"]["knownRecordedLoss"],
            json!({"items": 2, "bytes": 7})
        );
        assert_eq!(status["collectionFailure"]["persisted"], true);
        assert!(journal.retry_diagnostic(receipt.id).unwrap().durable);
        assert_eq!(journal.status().unwrap()["lostRecords"], 2);
    }

    #[test]
    fn diagnostic_runtime_loss_is_counted_once() {
        let (journal, _) = diagnostic_test_journal("runtime-loss");
        let permit = journal.reserve_diagnostic([8; 16], 128).unwrap();
        let receipt = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Runtime,
                error("storage.io", "runtime"),
                RecordedLoss {
                    items: 3,
                    bytes: None,
                },
            )
            .unwrap();
        assert!(receipt.durable);
        assert!(journal.retry_diagnostic(receipt.id).unwrap().durable);
        assert_eq!(journal.status().unwrap()["lostRecords"], 3);
        assert_eq!(journal.diagnostic_entries().unwrap()[0].loss.bytes, None);
    }

    #[test]
    fn diagnostic_clear_refuses_owned_identity_and_loss_stays_exact() {
        let (journal, injector) = diagnostic_test_journal("clear");
        let permit = journal.reserve_diagnostic([9; 16], 128).unwrap();
        journal.stop().unwrap();
        assert_eq!(journal.clear().unwrap_err().kind, "storage.busy");
        drop(permit);
        assert_eq!(journal.clear().unwrap()["cleared"], true);
        let permit = journal.reserve_diagnostic([9; 16], 128).unwrap();
        let receipt = journal
            .record_diagnostic(
                permit,
                DiagnosticCategory::Collection,
                error("storage.io", "large loss"),
                RecordedLoss {
                    items: u64::MAX,
                    bytes: None,
                },
            )
            .unwrap();
        assert!(receipt.durable);
        let payload: String = injector
            .query_row(
                "SELECT payload FROM diagnostic_ledger WHERE occurrence=?1",
                [receipt.id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["loss"]["items"].as_u64(), Some(u64::MAX));
        assert_eq!(payload["loss"]["bytes"], Value::Null);
        assert_eq!(journal.clear().unwrap_err().kind, "storage.busy");
    }

    #[test]
    fn v5_rejects_diagnostic_reservation_without_schema_change() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-ledger-v5-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let journal = ContinuationJournal::open(
            &path,
            "v5",
            &json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            },
        )
        .unwrap();
        let injector = Connection::open(path).unwrap();
        injector.pragma_update(None, "user_version", 5).unwrap();
        assert_eq!(
            journal.reserve_diagnostic([0; 16], 128).unwrap_err().kind,
            "storage.schema"
        );
        assert!(
            journal
                .shared
                .diagnostic_ledger
                .lock()
                .unwrap()
                .slots
                .is_empty()
        );
    }

    #[test]
    fn failed_recorded_insert_keeps_peer_out_until_fence_is_published() {
        use std::sync::mpsc;
        for reject_persistence in [false, true] {
            let directory = std::env::temp_dir().join(format!(
                "ubm-failed-append-{}-{:?}-{reject_persistence}-{:?}",
                std::process::id(),
                std::thread::current().id(),
                std::time::SystemTime::now()
            ));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("recording.sqlite");
            let quota = JournalQuota {
                max_bytes: 1 << 20,
                max_records: 10,
            };
            let writer =
                Arc::new(ContinuationJournal::open(&path, "race", &json!({}), quota).unwrap());
            let peer = ContinuationJournal::open(&path, "race", &json!({}), quota).unwrap();
            let injector = Connection::open(&path).unwrap();
            injector.execute_batch("CREATE TRIGGER reject_record BEFORE INSERT ON records WHEN new.body LIKE '%reject-me%' BEGIN SELECT RAISE(ABORT, 'precise insert failure'); END;").unwrap();
            if reject_persistence {
                injector.execute_batch("CREATE TRIGGER reject_failure BEFORE UPDATE OF collection_failure ON journal BEGIN SELECT RAISE(ABORT, 'precise persistence failure'); END;").unwrap();
            }
            let (entered_tx, entered_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let resume_rx = Mutex::new(resume_rx);
            *writer.failures.failed_append_hook.lock().unwrap() = Some(Box::new(move || {
                entered_tx.send(()).unwrap();
                resume_rx.lock().unwrap().recv().unwrap();
            }));
            let worker = writer.clone();
            let thread = std::thread::spawn(move || {
                worker.append_recorded_batch(
                    &[(&json!({}), &json!({"value":"reject-me"}))],
                    &[RecordedLoss {
                        items: 1,
                        bytes: Some(9),
                    }],
                    RecordedLoss::default(),
                )
            });
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let locked = matches!(
                peer.shared.connection.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            );
            let overtook = if locked {
                None
            } else {
                Some(peer.append(&json!({}), &json!({"value":"permitted"})))
            };
            resume_tx.send(()).unwrap();
            let failure = thread.join().unwrap().unwrap_err();
            assert!(locked, "peer overtook failed insert: {overtook:?}");
            assert_eq!(failure.operation, "append");
            assert!(failure.sqlite_extended_code.is_some());
            assert_eq!(
                peer.append(&json!({}), &json!({"value":"later"}))
                    .unwrap_err()
                    .kind,
                "storage.collection-failed"
            );
            let status = peer.status().unwrap();
            assert_eq!(status["records"], 0);
            assert_eq!(status["collectionFailure"]["knownRecordedLoss"]["items"], 1);
            assert_eq!(
                status["collectionFailure"]["persisted"],
                !reject_persistence
            );
            if reject_persistence {
                assert_eq!(
                    status["collectionFailure"]["persistenceFailure"]["operation"],
                    "mark-recorded-collection-loss"
                );
                assert!(
                    status["collectionFailure"]["persistenceFailure"]["sqliteExtendedCode"]
                        .is_number()
                );
                injector
                    .execute_batch("DROP TRIGGER reject_failure")
                    .unwrap();
                writer.mark_recorded_collection_loss(&failure, RecordedLoss::default());
                writer.mark_recorded_collection_loss(&failure, RecordedLoss::default());
                let retried = peer.status().unwrap();
                assert_eq!(retried["lostRecords"], 1);
                assert_eq!(
                    retried["collectionFailure"]["knownRecordedLoss"],
                    json!({"items":1,"bytes":9})
                );
                assert_eq!(retried["collectionFailure"]["persisted"], true);
                assert_eq!(
                    retried["collectionFailure"]["sqliteExtendedCode"],
                    failure.sqlite_extended_code.unwrap()
                );
            }
        }
    }

    #[tokio::test]
    async fn acknowledgement_wakes_independently_opened_reader() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-reader-wake-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let writer = ContinuationJournal::open(&path, "wake", &json!({}), quota).unwrap();
        let reader = ContinuationJournal::open(&path, "wake", &json!({}), quota).unwrap();
        writer.append(&json!({}), &json!({"value":1})).unwrap();
        let wake = writer.reader_space_wake();
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let prepared = reader.prepare(1, 4096).unwrap();
        reader
            .acknowledge(prepared["token"].as_str().unwrap())
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(100), notified)
            .await
            .expect("peer acknowledgement must wake the writer");
    }

    #[test]
    fn persisted_runtime_failure_does_not_pin_journal() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-runtime-retire-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal = ContinuationJournal::open(&path, "runtime", &json!({}), quota).unwrap();
        let injector = Connection::open(&path).unwrap();
        injector.execute_batch("CREATE TRIGGER reject_record BEFORE INSERT ON records BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
        let failure = journal.append(&json!({}), &json!({"value":1})).unwrap_err();
        assert_eq!(
            journal.status().unwrap()["runtimeFailure"]["kind"],
            failure.kind
        );
        injector
            .execute_batch("DROP TRIGGER reject_record")
            .unwrap();
        journal.mark_collection_failure(&failure);
        let status = journal.status().unwrap();
        assert_eq!(status["collectionFailure"]["kind"], failure.kind);
        assert_eq!(status["collectionFailure"]["operation"], failure.operation);
        assert_eq!(status["collectionFailure"]["persisted"], true);
        assert_eq!(status["runtimeFailure"], Value::Null);
        assert!(journal.can_close());
    }

    #[test]
    fn unrelated_persisted_failure_does_not_retire_runtime_evidence() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-runtime-distinct-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal = ContinuationJournal::open(&path, "runtime", &json!({}), quota).unwrap();
        let injector = Connection::open(&path).unwrap();
        injector.execute_batch("CREATE TRIGGER reject_record BEFORE INSERT ON records BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
        let failure = journal.append(&json!({}), &json!({"value":1})).unwrap_err();
        injector
            .execute_batch("DROP TRIGGER reject_record")
            .unwrap();
        journal.mark_collection_failure(&error("storage.full", "different failure"));
        let status = journal.status().unwrap();
        assert_eq!(status["collectionFailure"]["kind"], "storage.full");
        assert_eq!(status["runtimeFailure"]["kind"], failure.kind);
        assert!(!journal.can_close());
    }

    #[test]
    fn peer_unpersisted_failure_pins_registry_authority() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-peer-failure-pin-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let registry_handle = ContinuationJournal::open(&path, "pin", &json!({}), quota).unwrap();
        let peer = ContinuationJournal::open(&path, "pin", &json!({}), quota).unwrap();
        assert!(
            !registry_handle.can_close(),
            "a live peer must retain the process lock"
        );
        let failure = peer.append_batch(&[]).unwrap_err();
        let injector = Connection::open(&path).unwrap();
        injector.execute_batch("CREATE TRIGGER reject_failure BEFORE UPDATE OF collection_failure ON journal BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
        peer.mark_collection_failure(&failure);
        drop(peer);
        assert!(!registry_handle.can_close());
        drop(injector);
    }

    #[test]
    fn experimental_fence_capture_is_atomic_and_retries_exact_source_across_handles() {
        use ubm_core::oversized_evidence::{
            EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureMember,
            ScanFailureOccurrence,
        };
        let directory = std::env::temp_dir().join(format!(
            "ubm-fence-primitive-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        let journal = ContinuationJournal::open(&path, "fence", &json!({}), quota).unwrap();
        journal.append(&json!({}), &json!({"before":true})).unwrap();
        {
            let connection = journal.connection().unwrap();
            connection
                .execute_batch(EXPERIMENTAL_SCAN_FENCE_SCHEMA)
                .unwrap();
        }
        let make = |byte, recipient| {
            ScanFailureOccurrence::new(
                ScanFailureIdentity {
                    host: [1; 16],
                    scan: [2; 16],
                    start: [3; 16],
                    source_generation: [4; 16],
                    occurrence: [5; 16],
                },
                vec![ScanFailureMember {
                    recipient: [recipient; 16],
                    delivery: [7; 16],
                }],
                Arc::new(EvidenceSource::new(vec![
                    EvidenceToken::Array(3),
                    EvidenceToken::Bytes(vec![byte; 70_000]),
                    EvidenceToken::F64Bits(u64::MAX),
                    EvidenceToken::Null,
                ])),
            )
        };
        let source = make(42, 6);
        {
            let mut connection = journal.connection().unwrap();
            let mut tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            tx.execute_batch("CREATE TRIGGER reject_fence_token BEFORE INSERT ON scan_fence_tokens WHEN new.position=1 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
            assert!(capture_scan_fence(&mut tx, &source).is_err());
            assert_eq!(
                tx.query_row("SELECT count(*) FROM scan_fences", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
            drop(tx);
        }
        {
            let mut connection = journal.connection().unwrap();
            let mut tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            assert_eq!(capture_scan_fence(&mut tx, &source).unwrap(), 2);
            drop(tx);
        }
        let second = ContinuationJournal::open(&path, "fence", &json!({}), quota).unwrap();
        {
            let mut connection = second.connection().unwrap();
            let mut tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            assert_eq!(capture_scan_fence(&mut tx, &source).unwrap(), 2);
            tx.commit().unwrap();
        }
        drop(journal);
        drop(second);
        let reopened = ContinuationJournal::open(&path, "fence", &json!({}), quota).unwrap();
        {
            let mut connection = reopened.connection().unwrap();
            let mut tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            assert_eq!(capture_scan_fence(&mut tx, &source).unwrap(), 2);
            assert_eq!(
                capture_scan_fence(&mut tx, &make(43, 6)).unwrap_err().kind,
                "storage.identity"
            );
            assert_eq!(
                capture_scan_fence(&mut tx, &make(42, 8)).unwrap_err().kind,
                "storage.identity"
            );
            assert_eq!(
                tx.query_row("SELECT next_ordinal FROM journal", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                3
            );
            assert_eq!(
                tx.query_row("SELECT count(*) FROM scan_fences", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert_eq!(
                tx.query_row(
                    "SELECT length(payload) FROM scan_fence_tokens WHERE position=1",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                70_000
            );
            assert_eq!(
                tx.query_row(
                    "SELECT payload FROM scan_fence_tokens WHERE position=2",
                    [],
                    |row| row.get::<_, Vec<u8>>(0)
                )
                .unwrap(),
                u64::MAX.to_be_bytes()
            );
            tx.commit().unwrap();
        }
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_stop_retains_all_owners_when_one_seal_fails() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Session {
            fail: AtomicBool,
            sealed: AtomicBool,
        }
        impl crate::continuation::ContinuationSession for Session {
            fn attach_journal(
                &self,
                _: Arc<ContinuationJournal>,
                _: Value,
            ) -> crate::continuation::Result<()> {
                Ok(())
            }
            fn seal_collection(&self) -> crate::continuation::Result<()> {
                if self.fail.load(Ordering::SeqCst) {
                    return Err(json!({"code":"lifecycle.invalid-state"}));
                }
                self.sealed.store(true, Ordering::SeqCst);
                Ok(())
            }
            fn collection_sealed(&self) -> bool {
                self.sealed.load(Ordering::SeqCst)
            }
            fn call<'a>(
                &'a self,
                _: &'a str,
                _: &'a str,
            ) -> crate::continuation::ContinuationFuture<'a> {
                Box::pin(async { String::new() })
            }
            fn claim_drain(
                &self,
                _: u32,
                _: u32,
            ) -> crate::continuation::Result<Option<crate::continuation_outbox::DrainClaim>>
            {
                Ok(None)
            }
            fn acknowledge_drain(&self, _: u64) -> crate::continuation::Result<()> {
                Ok(())
            }
        }
        let directory = std::env::temp_dir().join(format!(
            "ubm-legacy-owner-retry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let registry = JournalRegistry::default();
        registry.configure_directory(&directory).unwrap();
        let journal = registry
            .open(
                "retry",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 10,
                },
            )
            .unwrap();
        let first = Arc::new(Session {
            fail: AtomicBool::new(false),
            sealed: AtomicBool::new(false),
        });
        let second = Arc::new(Session {
            fail: AtomicBool::new(true),
            sealed: AtomicBool::new(false),
        });
        let first_session: Arc<dyn crate::continuation::ContinuationSession> = first.clone();
        let second_session: Arc<dyn crate::continuation::ContinuationSession> = second.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(tokio::sync::Notify::new());
        attach_legacy_recording_owner(&journal, &first_session, &stopped, &wake, json!({}))
            .unwrap();
        attach_legacy_recording_owner(&journal, &second_session, &stopped, &wake, json!({}))
            .unwrap();
        assert!(stop_legacy_recording(&journal).is_err());
        assert_eq!(journal.status().unwrap()["accepting"], true);
        assert!(first.sealed.load(Ordering::SeqCst));
        assert!(!second.sealed.load(Ordering::SeqCst));
        second.fail.store(false, Ordering::SeqCst);
        stop_legacy_recording(&journal).unwrap();
        assert!(second.sealed.load(Ordering::SeqCst));
        assert_eq!(journal.status().unwrap()["accepting"], false);
        drop(journal);
        authorities()
            .unwrap()
            .journals
            .remove(&directory.join("retry.sqlite"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    impl OrderedSupervisor for () {
        fn host_cleanup(
            self: Arc<Self>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>> {
            Box::pin(async { json!({"state":"not-applicable"}) })
        }
        fn shutdown(
            self: Arc<Self>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = std::result::Result<
                            crate::ordered_recording::RecordingDriverStatus,
                            crate::ordered_recording::RecordingError,
                        >,
                    > + Send,
            >,
        > {
            Box::pin(async { Ok(crate::ordered_recording::RecordingDriverStatus::Finished) })
        }
    }

    #[test]
    fn ordered_supervisor_hot_handles_do_not_interrupt_live_owner() {
        let directory = std::env::temp_dir().join(format!("ubm-hot-owner-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let registry = JournalRegistry::default();
        registry.configure_directory(&directory).unwrap();
        let journal = registry
            .open(
                "hot",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 10,
                },
            )
            .unwrap();
        let _owner = register_ordered_supervisor(&journal, "producer", || {
            Ok::<_, JournalError>(Arc::new(()))
        })
        .unwrap();
        begin_supervised_ordered(&journal, "producer", "1").unwrap();
        let other = JournalRegistry::default();
        other.configure_directory(&directory).unwrap();
        let same = other.get("hot").unwrap();
        let guarded = stop_legacy_recording(&same);
        assert!(guarded.is_err());
        assert!(Arc::ptr_eq(&journal, &same));
        assert_eq!(
            other.ordered_context_offline("hot").unwrap_err().kind,
            "storage.owner-conflict"
        );
        assert_eq!(
            other.ordered_status_offline("hot").unwrap_err().kind,
            "storage.owner-conflict"
        );
        assert_eq!(
            other.stop_ordered_offline("hot").unwrap_err().kind,
            "storage.owner-conflict"
        );
        assert_eq!(same.status().unwrap()["orderedOwner"]["state"], "live");
        assert!(
            register_ordered_supervisor(&same, "other", || Ok::<_, JournalError>(Arc::new(())))
                .is_err()
        );
        journal.seal_ordered("producer").unwrap();
        release_ordered_supervisor(&journal, "producer").unwrap();
        drop(same);
        drop(journal);
    }

    #[tokio::test]
    async fn ordered_stop_receipt_distinguishes_settled_interrupted_and_ordinary() {
        let directory =
            std::env::temp_dir().join(format!("ubm-stop-receipt-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let registry = Arc::new(JournalRegistry::default());
        registry.configure_directory(&directory).unwrap();
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let sealed = registry.open("sealed", &json!({}), quota).unwrap();
        sealed.begin_ordered("producer", "1").unwrap();
        sealed.seal_ordered("producer").unwrap();
        assert_eq!(sealed.ordered_host_cleanup().unwrap()["state"], "uncertain");
        sealed
            .settle_ordered_host_cleanup("producer", true)
            .unwrap();
        let reopened = ContinuationJournal::open(
            &directory.join("sealed.sqlite"),
            "sealed",
            &json!({}),
            quota,
        )
        .unwrap();
        assert_eq!(
            reopened.ordered_host_cleanup().unwrap(),
            json!({"state":"released","failures":[]})
        );
        assert_eq!(
            registry.stop_ordered_host_receipt("sealed").await.unwrap()["hostCleanup"]["state"],
            "released"
        );
        assert_eq!(
            registry.stop_ordered_receipt("sealed").await.unwrap(),
            json!({"version":2,"state":"finished","error":null})
        );
        let interrupted = registry.open("interrupted", &json!({}), quota).unwrap();
        interrupted.begin_ordered("producer", "1").unwrap();
        interrupted.recover_interrupted_ordered("producer").unwrap();
        assert_eq!(
            registry.stop_ordered_receipt("interrupted").await.unwrap(),
            json!({"version":2,"state":"interrupted","error":null})
        );
        registry.open("ordinary", &json!({}), quota).unwrap();
        assert!(registry.stop_ordered_receipt("ordinary").await.is_err());
        assert_eq!(
            registry
                .stop_ordered_receipt("../invalid")
                .await
                .unwrap_err()["code"],
            "argument.invalid"
        );
    }

    #[test]
    fn ordered_prepared_before_begin_survives_process_exit_without_invented_owner() {
        const CHILD: &str = "UBM_ORDERED_PREPARE_CRASH_DIR";
        let context = json!({"kind":"ordered-recording","version":1,"peerId":"peer","connectionGeneration":"connection","databaseGeneration":"database","maxItems":8,"maxBytes":4096,"maxControls":4,"members":[{"memberId":"1","publicMemberId":"heart","selector":{"serviceUuid":"service","serviceOccurrence":"0","characteristicUuid":"characteristic","characteristicOccurrence":"2"},"delivery":"notification","preference":null}]});
        if let Ok(directory) = std::env::var(CHILD) {
            let registry = JournalRegistry::default();
            registry.configure_directory(Path::new(&directory)).unwrap();
            registry
                .open(
                    "prepared",
                    &context,
                    JournalQuota {
                        max_bytes: 1 << 20,
                        max_records: 10,
                    },
                )
                .unwrap();
            std::process::exit(0);
        }
        let directory =
            std::env::temp_dir().join(format!("ubm-prepared-owner-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap()).arg("--exact").arg("continuation_journal::authority_tests::ordered_prepared_before_begin_survives_process_exit_without_invented_owner").env(CHILD,&directory).status().unwrap();
        assert!(result.success());
        let registry = JournalRegistry::default();
        registry.configure_directory(&directory).unwrap();
        assert_eq!(
            registry.ordered_context_offline("prepared").unwrap(),
            context
        );
        let status = registry.ordered_status_offline("prepared").unwrap();
        assert_eq!(status["state"], "not-started");
        assert!(status["journal"].get("orderedOwner").is_none());
        assert!(registry.stop_ordered_offline("prepared").is_err());
    }

    #[test]
    fn ordered_supervisor_crash_recovery() {
        const CHILD: &str = "UBM_ORDERED_CRASH_TEST_DIR";
        let context = json!({"kind":"ordered-recording","version":1,"peerId":"peer","connectionGeneration":"connection","databaseGeneration":"database","maxItems":8,"maxBytes":4096,"maxControls":4,"members":[{"memberId":"1","publicMemberId":"heart","selector":{"serviceUuid":"service","serviceOccurrence":"0","characteristicUuid":"characteristic","characteristicOccurrence":"2"},"delivery":"notification","preference":null}]});
        if let Ok(directory) = std::env::var(CHILD) {
            let registry = JournalRegistry::default();
            registry.configure_directory(Path::new(&directory)).unwrap();
            let journal = registry
                .open(
                    "crash",
                    &context,
                    JournalQuota {
                        max_bytes: 1 << 20,
                        max_records: 10,
                    },
                )
                .unwrap();
            let _owner = register_ordered_supervisor(&journal, "producer", || {
                Ok::<_, JournalError>(Arc::new(()))
            })
            .unwrap();
            begin_supervised_ordered(&journal, "producer", "1").unwrap();
            journal
                .bind_ordered_generation("producer", "counter", "1")
                .unwrap();
            journal
                .append_ordered(
                    "producer",
                    "1",
                    "counter",
                    "1",
                    "1",
                    &json!({}),
                    &json!({"kind":"value"}),
                )
                .unwrap();
            std::process::exit(0);
        }
        let directory =
            std::env::temp_dir().join(format!("ubm-crashed-owner-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("continuation_journal::authority_tests::ordered_supervisor_crash_recovery")
            .env(CHILD, &directory)
            .status()
            .unwrap();
        assert!(result.success());
        let registry = JournalRegistry::default();
        registry.configure_directory(&directory).unwrap();
        let journal = registry.get("crash").unwrap();
        assert_eq!(registry.ordered_context("crash").unwrap(), context);
        assert_eq!(registry.ordered_context_offline("crash").unwrap(), context);
        assert_eq!(
            registry.ordered_status_offline("crash").unwrap()["state"],
            "interrupted"
        );
        assert_eq!(
            registry.stop_ordered_offline("crash").unwrap()["state"],
            "interrupted"
        );
        let status = journal.status().unwrap();
        assert_eq!(
            journal.ordered_host_cleanup().unwrap()["state"],
            "uncertain"
        );
        assert_eq!(status["orderedOwner"]["state"], "interrupted");
        assert_eq!(status["orderedOwner"]["uncertainTail"], true);
        assert_eq!(status["records"], 1);
        let connection = journal.connection().unwrap();
        let count: u64 = connection
            .query_row("SELECT count(*) FROM ordered_identities", [], |row| {
                unsigned(row, 0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn ordered_identity_retirement_bounds_history_and_preserves_restart_replay() {
        let directory =
            std::env::temp_dir().join(format!("ubm-ordered-retirement-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("retirement.sqlite");
        let _ = std::fs::remove_file(&path);
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 2,
        };
        let journal = ContinuationJournal::open(&path, "retirement", &json!({}), quota).unwrap();
        journal.begin_ordered("producer", "1").unwrap();
        journal
            .bind_ordered_generation("producer", "counter", "2")
            .unwrap();
        let fault_body = json!({"value":"retirement-fault"});
        let committed = journal
            .append_ordered(
                "producer",
                "1",
                "counter",
                "2",
                "fault",
                &json!({}),
                &fault_body,
            )
            .unwrap();
        {
            let connection = journal.connection().unwrap();
            connection.execute_batch("CREATE TEMP TRIGGER fail_retirement BEFORE DELETE ON ordered_identities BEGIN SELECT RAISE(FAIL, 'injected'); END;").unwrap();
        }
        assert!(
            journal
                .retire_ordered_identity("producer", "1", "counter", "2", "fault")
                .is_err()
        );
        assert_eq!(
            journal
                .append_ordered(
                    "producer",
                    "1",
                    "counter",
                    "2",
                    "fault",
                    &json!({}),
                    &fault_body
                )
                .unwrap(),
            committed
        );
        {
            let connection = journal.connection().unwrap();
            connection
                .execute_batch("DROP TRIGGER fail_retirement;")
                .unwrap();
        }
        journal
            .retire_ordered_identity("producer", "1", "counter", "2", "fault")
            .unwrap();
        let batch = journal.prepare(1, 4096).unwrap();
        journal
            .acknowledge(batch["token"].as_str().unwrap())
            .unwrap();
        for id in 1..=100 {
            let delivery = id.to_string();
            let body = json!({"value":id});
            let result = journal
                .append_ordered(
                    "producer",
                    "1",
                    "counter",
                    "2",
                    &delivery,
                    &json!({}),
                    &body,
                )
                .unwrap();
            // Before confirmed source ACK the identity still makes retries idempotent.
            assert_eq!(
                journal
                    .append_ordered(
                        "producer",
                        "1",
                        "counter",
                        "2",
                        &delivery,
                        &json!({}),
                        &body
                    )
                    .unwrap(),
                result
            );
            journal
                .retire_ordered_identity("producer", "1", "counter", "2", &delivery)
                .unwrap();
            journal
                .retire_ordered_identity("producer", "1", "counter", "2", &delivery)
                .unwrap();
            let batch = journal.prepare(1, 4096).unwrap();
            journal
                .acknowledge(batch["token"].as_str().unwrap())
                .unwrap();
        }
        let connection = journal.connection().unwrap();
        let count: u64 = connection
            .query_row("SELECT count(*) FROM ordered_identities", [], |row| {
                unsigned(row, 0)
            })
            .unwrap();
        assert_eq!(count, 0);
        drop(connection);
        let body = json!({"value":"restart"});
        let committed = journal
            .append_ordered("producer", "1", "counter", "2", "101", &json!({}), &body)
            .unwrap();
        assert_eq!(
            journal
                .retire_interrupted_ordered_identities("producer")
                .unwrap_err()
                .kind,
            "storage.owner-conflict"
        );
        drop(journal);
        let journal = ContinuationJournal::open(&path, "retirement", &json!({}), quota).unwrap();
        assert_eq!(
            journal
                .append_ordered("producer", "1", "counter", "2", "101", &json!({}), &body)
                .unwrap(),
            committed
        );
        journal.recover_interrupted_ordered("producer").unwrap();
        journal
            .retire_interrupted_ordered_identities("producer")
            .unwrap();
        journal
            .retire_interrupted_ordered_identities("producer")
            .unwrap();
        assert_eq!(journal.status().unwrap()["records"], 1);
        let connection = journal.connection().unwrap();
        let count: u64 = connection
            .query_row("SELECT count(*) FROM ordered_identities", [], |row| {
                unsigned(row, 0)
            })
            .unwrap();
        assert_eq!(count, 0);
        drop(connection);
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ordered_maximum_pending_and_terminal_resume_after_reader_ack() {
        let directory =
            std::env::temp_dir().join(format!("ubm-ordered-pending-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("pending.sqlite");
        let _ = std::fs::remove_file(&path);
        let quota = JournalQuota {
            max_bytes: 4 << 20,
            max_records: 1,
        };
        let journal = ContinuationJournal::open(&path, "pending", &json!({}), quota).unwrap();
        journal.begin_ordered("producer", "1").unwrap();
        journal
            .bind_ordered_generation("producer", "counter", "1")
            .unwrap();
        let metadata = json!({"m":"x".repeat(16370)});
        let record = json!({"r":"x".repeat(65520)});
        journal
            .append_ordered(
                "producer",
                "1",
                "counter",
                "1",
                "1",
                &json!({}),
                &json!({"kind":"value"}),
            )
            .unwrap();
        assert_eq!(
            journal
                .append_ordered("producer", "1", "counter", "1", "2", &metadata, &record)
                .unwrap_err()
                .kind,
            "storage.full"
        );
        drop(journal);
        let journal = ContinuationJournal::open(&path, "pending", &json!({}), quota).unwrap();
        assert_eq!(journal.status().unwrap()["phase"], "capacity-reached");
        let first = journal.prepare(1, 4096).unwrap();
        journal
            .acknowledge(first["token"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            journal
                .append_ordered("producer", "1", "counter", "1", "2", &metadata, &record)
                .unwrap()["ordinal"],
            2
        );
        let terminal = json!({"kind":"terminal","tailUncertain":true});
        assert_eq!(
            journal
                .append_ordered("producer", "1", "counter", "1", "3", &json!({}), &terminal)
                .unwrap_err()
                .kind,
            "storage.full"
        );
        let second = journal.prepare(1, 131072).unwrap();
        journal
            .acknowledge(second["token"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            journal
                .append_ordered("producer", "1", "counter", "1", "3", &json!({}), &terminal)
                .unwrap()["ordinal"],
            3
        );
        journal.seal_ordered("producer").unwrap();
        assert_eq!(journal.status().unwrap()["lostRecords"], 0);
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ordered_owner_rejects_legacy_writers_without_loss_or_ordinal_change() {
        let directory =
            std::env::temp_dir().join(format!("ubm-ordered-exclusive-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("exclusive.sqlite");
        let _ = std::fs::remove_file(&path);
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal = ContinuationJournal::open(&path, "exclusive", &json!({}), quota).unwrap();
        assert_eq!(
            journal
                .append_ordered("p", "1", "counter", "1", "1", &json!({}), &json!({}))
                .unwrap_err()
                .kind,
            "storage.owner-conflict"
        );
        journal.begin_ordered("p", "1").unwrap();
        journal
            .bind_ordered_generation("p", "counter", "1")
            .unwrap();
        drop(journal);
        let journal = ContinuationJournal::open(&path, "exclusive", &json!({}), quota).unwrap();
        let metadata = json!({});
        let record = json!({"value":1});
        assert_eq!(
            journal.append(&metadata, &record).unwrap_err().kind,
            "storage.owner-conflict"
        );
        assert_eq!(
            journal
                .append_recorded_batch(
                    &[(&metadata, &record)],
                    &[RecordedLoss {
                        items: 1,
                        bytes: Some(4)
                    }],
                    RecordedLoss::default()
                )
                .unwrap_err()
                .kind,
            "storage.owner-conflict"
        );
        assert_eq!(
            journal
                .append_recorded_batch(
                    &[(&metadata, &json!(null))],
                    &[RecordedLoss {
                        items: 1,
                        bytes: None
                    }],
                    RecordedLoss::default()
                )
                .unwrap_err()
                .kind,
            "storage.owner-conflict"
        );
        assert_eq!(journal.stop().unwrap_err().kind, "storage.owner-conflict");
        assert_eq!(journal.clear().unwrap_err().kind, "storage.owner-conflict");
        let status = journal.status().unwrap();
        assert_eq!(status["lostRecords"], 0);
        assert_eq!(status["records"], 0);
        assert!(status["collectionFailure"].is_null());
        assert!(status["runtimeFailure"].is_null());
        assert_eq!(
            journal
                .append_ordered("p", "1", "counter", "1", "1", &metadata, &record)
                .unwrap()["ordinal"],
            1
        );
        assert_eq!(journal.stop().unwrap_err().kind, "storage.owner-conflict");
        assert_eq!(journal.clear().unwrap_err().kind, "storage.owner-conflict");
        assert_eq!(
            journal
                .append_ordered("p", "1", "counter", "1", "1", &metadata, &record)
                .unwrap()["ordinal"],
            1
        );
        assert_eq!(journal.status().unwrap()["records"], 1);
        journal.seal_ordered("p").unwrap();
        assert_eq!(journal.stop().unwrap()["phase"], "stopped");
        assert_eq!(journal.clear().unwrap()["records"], 1);
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ordered_lifecycle_migrates_v1_without_losing_records() {
        let directory = std::env::temp_dir().join(format!("ubm-ordered-v1-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("ordered.sqlite");
        let _ = std::fs::remove_file(&path);
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal = ContinuationJournal::open(&path, "migration", &json!({}), quota).unwrap();
        journal.append(&json!({}), &json!({"value":1})).unwrap();
        drop(journal);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TABLE ordered_owner; DROP TABLE ordered_identities; DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; PRAGMA user_version=1;",
            )
            .unwrap();
        drop(connection);
        let journal = ContinuationJournal::open(&path, "migration", &json!({}), quota).unwrap();
        assert_eq!(journal.status().unwrap()["records"], 1);
        journal.begin_ordered("producer", "4").unwrap();
        assert_eq!(journal.status().unwrap()["orderedOwner"]["state"], "live");
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ordered_unbound_lifecycle_cannot_accept_delivery_and_reopens_uncertain() {
        let directory =
            std::env::temp_dir().join(format!("ubm-ordered-unbound-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("ordered.sqlite");
        let _ = std::fs::remove_file(&path);
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal = ContinuationJournal::open(&path, "unbound", &json!({}), quota).unwrap();
        journal.begin_ordered("producer", "4").unwrap();
        assert_eq!(
            journal
                .append_ordered("producer", "4", "counter", "1", "1", &json!({}), &json!({}))
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        drop(journal);
        let journal = ContinuationJournal::open(&path, "unbound", &json!({}), quota).unwrap();
        assert!(journal.status().unwrap()["orderedOwner"]["sourceGeneration"].is_null());
        journal.recover_interrupted_ordered("producer").unwrap();
        assert_eq!(
            journal.status().unwrap()["orderedOwner"]["uncertainTail"],
            true
        );
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ordered_no_source_terminal_is_idempotent_and_seals_without_generation() {
        let directory =
            std::env::temp_dir().join(format!("ubm-ordered-no-source-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("ordered.sqlite");
        let _ = std::fs::remove_file(&path);
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal = ContinuationJournal::open(&path, "no-source", &json!({}), quota).unwrap();
        journal.begin_ordered("producer", "4").unwrap();
        let append = || {
            journal.append_ordered(
                "producer",
                "4",
                "no-source",
                "not-created",
                "setup-terminal",
                &json!({"sourceGeneration":null}),
                &json!({"kind":"setup-terminal"}),
            )
        };
        assert_eq!(append().unwrap(), append().unwrap());
        assert_eq!(journal.status().unwrap()["records"], 1);
        journal.seal_ordered("producer").unwrap();
        let status = journal.status().unwrap();
        assert_eq!(status["orderedOwner"]["state"], "sealed");
        assert!(status["orderedOwner"]["sourceGeneration"].is_null());
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ordered_lifecycle_retains_live_identity_and_explicit_interrupted_tail() {
        let directory =
            std::env::temp_dir().join(format!("ubm-ordered-lifecycle-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("ordered.sqlite");
        let _ = std::fs::remove_file(&path);
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal = ContinuationJournal::open(&path, "lifecycle", &json!({}), quota).unwrap();
        assert!(journal.status().unwrap().get("orderedOwner").is_none());
        journal.begin_ordered("producer", "collection").unwrap();
        journal
            .bind_ordered_generation("producer", "counter", "9")
            .unwrap();
        journal
            .bind_ordered_generation("producer", "counter", "9")
            .unwrap();
        assert_eq!(
            journal
                .bind_ordered_generation("producer", "counter", "10")
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        assert_eq!(
            journal
                .begin_ordered("other", "collection")
                .unwrap_err()
                .kind,
            "storage.busy"
        );
        assert_eq!(
            journal
                .recover_interrupted_ordered("other")
                .unwrap_err()
                .kind,
            "storage.identity"
        );
        drop(journal);
        let journal = ContinuationJournal::open(&path, "lifecycle", &json!({}), quota).unwrap();
        assert_eq!(journal.status().unwrap()["orderedOwner"]["state"], "live");
        assert_eq!(journal.status().unwrap()["orderedOwner"]["version"], 1);
        journal.recover_interrupted_ordered("producer").unwrap();
        drop(journal);
        let journal = ContinuationJournal::open(&path, "lifecycle", &json!({}), quota).unwrap();
        let status = journal.status().unwrap();
        assert_eq!(status["orderedOwner"]["state"], "interrupted");
        assert_eq!(status["orderedOwner"]["uncertainTail"], true);
        assert_eq!(status["phase"], "stopped");
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ordered_retry_conflict_reopen_and_quota_are_atomic() {
        let directory =
            std::env::temp_dir().join(format!("ubm-ordered-journal-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("ordered.sqlite");
        let _ = std::fs::remove_file(&path);
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 1,
        };
        let journal = ContinuationJournal::open(&path, "ordered", &json!({}), quota).unwrap();
        journal.begin_ordered("producer-a", "collection").unwrap();
        journal
            .bind_ordered_generation("producer-a", "counter", "1")
            .unwrap();
        let append =
            |journal: &ContinuationJournal, delivery: &str, metadata: Value, record: Value| {
                journal.append_ordered(
                    "producer-a",
                    "collection",
                    "counter",
                    "1",
                    delivery,
                    &metadata,
                    &record,
                )
            };
        let first = append(&journal, "1", json!({"peer":"a"}), json!({"value":1})).unwrap();
        assert_eq!(first["ordinal"], 1);
        assert_eq!(
            append(&journal, "1", json!({"peer":"a"}), json!({"value":1})).unwrap(),
            first
        );
        assert_eq!(
            append(&journal, "1", json!({"peer":"b"}), json!({"value":1}))
                .unwrap_err()
                .kind,
            "storage.identity-conflict"
        );
        assert_eq!(
            append(&journal, "1", json!({"peer":"a"}), json!({"value":2}))
                .unwrap_err()
                .kind,
            "storage.identity-conflict"
        );
        assert_eq!(
            append(&journal, "2", json!({}), json!({"value":2}))
                .unwrap_err()
                .kind,
            "storage.full"
        );
        assert_eq!(
            append(&journal, "3", json!({}), json!({"value":3}))
                .unwrap_err()
                .kind,
            "storage.owner-conflict"
        );
        assert_eq!(
            append(&journal, "2", json!({}), json!({"value":3}))
                .unwrap_err()
                .kind,
            "storage.identity-conflict"
        );
        assert_eq!(
            journal.seal_ordered("producer-a").unwrap_err().kind,
            "storage.owner-conflict"
        );
        drop(journal);
        let journal = ContinuationJournal::open(&path, "ordered", &json!({}), quota).unwrap();
        assert_eq!(
            append(&journal, "2", json!({}), json!({"value":2}))
                .unwrap_err()
                .kind,
            "storage.full"
        );
        let prepared = journal.prepare(10, 4096).unwrap();
        assert_eq!(prepared["records"].as_array().unwrap().len(), 1);
        journal
            .acknowledge(prepared["token"].as_str().unwrap())
            .unwrap();
        drop(journal);
        let journal = ContinuationJournal::open(&path, "ordered", &json!({}), quota).unwrap();
        assert_eq!(
            append(&journal, "1", json!({"peer":"a"}), json!({"value":1})).unwrap(),
            first
        );
        assert_eq!(journal.status().unwrap()["phase"], "capacity-reached");
        assert_eq!(journal.status().unwrap()["accepting"], false);
        assert_eq!(journal.status().unwrap()["lostRecords"], 0);
        assert_eq!(
            append(&journal, "2", json!({}), json!({"value":2})).unwrap()["ordinal"],
            2
        );
        assert_eq!(journal.status().unwrap()["phase"], "recording");
        assert_eq!(journal.status().unwrap()["records"], 1);
        let connection = journal.connection().unwrap();
        let (next, identities): (i64, i64) = connection.query_row(
            "SELECT next_ordinal,(SELECT count(*) FROM ordered_identities) FROM journal WHERE id=1",
            [], |row| Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        assert_eq!((next, identities), (3, 2));
        drop(connection);
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ordered_producer_incarnation_distinguishes_new_observations() {
        let directory =
            std::env::temp_dir().join(format!("ubm-ordered-producer-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("producer.sqlite");
        let _ = std::fs::remove_file(&path);
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal = ContinuationJournal::open(&path, "producer", &json!({}), quota).unwrap();
        journal.begin_ordered("producer-a", "collection").unwrap();
        journal
            .bind_ordered_generation("producer-a", "counter", "1")
            .unwrap();
        let append = |journal: &ContinuationJournal, producer: &str, value: i32| {
            journal.append_ordered(
                producer,
                "collection",
                "counter",
                "1",
                "1",
                &json!({}),
                &json!({"value":value}),
            )
        };
        assert_eq!(append(&journal, "producer-a", 1).unwrap()["ordinal"], 1);
        drop(journal);
        let journal = ContinuationJournal::open(&path, "producer", &json!({}), quota).unwrap();
        assert_eq!(append(&journal, "producer-a", 1).unwrap()["ordinal"], 1);
        assert_eq!(
            append(&journal, "producer-b", 1).unwrap_err().kind,
            "storage.identity"
        );
        journal.seal_ordered("producer-a").unwrap();
        assert_eq!(append(&journal, "producer-a", 1).unwrap()["ordinal"], 1);
        assert_eq!(
            append(&journal, "producer-c", 2).unwrap_err().kind,
            "storage.identity"
        );
        assert_eq!(
            append(&journal, "producer-a", 2).unwrap_err().kind,
            "storage.identity-conflict"
        );
        assert_eq!(journal.status().unwrap()["records"], 1);
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn normal_v1_adapter_preserves_released_core_schema() {
        let directory =
            std::env::temp_dir().join(format!("ubm-ordered-upgrade-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("upgrade.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let database = Connection::open(&path).unwrap();
        database.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=DELETE; PRAGMA application_id=1430408522; PRAGMA user_version=1; CREATE TABLE journal(id INTEGER PRIMARY KEY CHECK(id=1), recording_id TEXT NOT NULL, declaration TEXT NOT NULL, max_bytes INTEGER NOT NULL, max_records INTEGER NOT NULL, phase TEXT NOT NULL, next_ordinal INTEGER NOT NULL, next_token INTEGER NOT NULL, lost INTEGER NOT NULL, pending_token TEXT, pending_last INTEGER, pending_more INTEGER, last_ack_token TEXT, last_ack_receipt TEXT, retained_records INTEGER NOT NULL, retained_bytes INTEGER NOT NULL, collection_failure TEXT); CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL); INSERT INTO journal VALUES(1,'upgrade','{}',1048576,10,'recording',2,2,0,NULL,NULL,NULL,NULL,NULL,1,15,NULL); INSERT INTO records VALUES(1,'{\"legacy\":true}',15);").unwrap();
        drop(database);
        let journal = ContinuationJournal::open(&path, "upgrade", &json!({}), quota).unwrap();
        let prepared = journal.prepare(10, 4096).unwrap();
        assert_eq!(prepared["records"][0]["legacy"], true);
        journal
            .acknowledge(prepared["token"].as_str().unwrap())
            .unwrap();
        journal.append(&json!({}), &json!({"newer":true})).unwrap();
        drop(journal);
        let database = Connection::open(&path).unwrap();
        assert_eq!(
            database
                .query_row(
                    "SELECT count(*) FROM pragma_table_info('records')",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            3
        );
        // This is the released v1 positional writer shape. Newer normal use
        // must leave it executable, rather than silently adding a column.
        database
            .execute(
                "INSERT INTO records VALUES(?1,?2,?3)",
                params![3, "{\"legacy\":false}", 16],
            )
            .unwrap();
        drop(database);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn normal_v1_adapter_resumes_a_released_pending_page_without_migration() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-v1-pending-adapter-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("pending.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let database = Connection::open(&path).unwrap();
        database.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=DELETE; PRAGMA application_id=1430408522; PRAGMA user_version=1; CREATE TABLE journal(id INTEGER PRIMARY KEY CHECK(id=1), recording_id TEXT NOT NULL, declaration TEXT NOT NULL, max_bytes INTEGER NOT NULL, max_records INTEGER NOT NULL, phase TEXT NOT NULL, next_ordinal INTEGER NOT NULL, next_token INTEGER NOT NULL, lost INTEGER NOT NULL, pending_token TEXT, pending_last INTEGER, pending_more INTEGER, last_ack_token TEXT, last_ack_receipt TEXT, retained_records INTEGER NOT NULL, retained_bytes INTEGER NOT NULL, collection_failure TEXT); CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL); INSERT INTO journal VALUES(1,'pending','{}',1048576,10,'recording',2,2,0,'pending:1',1,0,NULL,NULL,1,7,NULL); INSERT INTO records VALUES(1,'{\"v\":1}',7);").unwrap();
        drop(database);
        let journal = ContinuationJournal::open(&path, "pending", &json!({}), quota).unwrap();
        let page = journal.prepare(10, 4096).unwrap();
        assert_eq!(page["token"], "pending:1");
        assert_eq!(page["records"], json!([{"v":1}]));
        assert_eq!(journal.acknowledge("pending:1").unwrap()["records"], 1);
        drop(journal);
        let database = Connection::open(&path).unwrap();
        assert_eq!(
            database
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            database
                .query_row(
                    "SELECT count(*) FROM pragma_table_info('records')",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            3
        );
        drop(database);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exact_v1_recovery_preserves_v1_core_schema_and_holds_authority_until_confirmed() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-v1-exact-handoff-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("legacy.sqlite");
        let database = Connection::open(&path).unwrap();
        database.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=DELETE; PRAGMA application_id=1430408522; PRAGMA user_version=1; CREATE TABLE journal(id INTEGER PRIMARY KEY CHECK(id=1), recording_id TEXT NOT NULL, declaration TEXT NOT NULL, max_bytes INTEGER NOT NULL, max_records INTEGER NOT NULL, phase TEXT NOT NULL, next_ordinal INTEGER NOT NULL, next_token INTEGER NOT NULL, lost INTEGER NOT NULL, pending_token TEXT, pending_last INTEGER, pending_more INTEGER, last_ack_token TEXT, last_ack_receipt TEXT, retained_records INTEGER NOT NULL, retained_bytes INTEGER NOT NULL, collection_failure TEXT); CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL); INSERT INTO journal VALUES(1,'legacy','{}',1048576,10,'recording',2,2,0,'1',1,0,NULL,NULL,1,7,NULL); INSERT INTO records VALUES(1,'{\"v\":1}',7);").unwrap();
        drop(database);

        let registry = JournalRegistry::default();
        registry.configure_directory(&directory).unwrap();
        let page = registry
            .legacy_recording_prepare_exact("legacy", 10, 4096)
            .unwrap();
        assert_eq!(page["version"], 1);

        let database = Connection::open(&path).unwrap();
        let columns: Vec<String> = database
            .prepare("SELECT name FROM pragma_table_info('journal') ORDER BY cid")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(columns.len(), 17);
        assert_eq!(
            database
                .query_row(
                    "SELECT count(*) FROM pragma_table_info('records')",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            3
        );
        drop(database);
        assert_eq!(lock_authority(&path).unwrap_err().kind, "storage.busy");

        registry
            .legacy_recording_acknowledge_exact("legacy", &page)
            .unwrap();
        registry
            .legacy_recording_retire_exact("legacy", &page)
            .unwrap();
        registry
            .legacy_recording_confirm_exact_retirement("legacy", &page)
            .unwrap();
        drop(lock_authority(&path).unwrap());
        let database = Connection::open(&path).unwrap();
        // This is the released v1 positional statement. It proves the exact
        // recovery handoff did not add a core-table column that would make an
        // old process fail before it can resume the recording.
        database
            .execute(
                "INSERT INTO records VALUES(?1,?2,?3)",
                params![2, "{\"v\":2}", 7],
            )
            .unwrap();
        database
            .execute(
                "UPDATE journal SET next_ordinal=3,retained_records=1,retained_bytes=7,pending_token=NULL,pending_last=NULL,pending_more=NULL,last_ack_token='1',last_ack_receipt='{\"token\":\"1\",\"acknowledged\":true,\"records\":1}' WHERE id=1",
                [],
            )
            .unwrap();
        assert_eq!(
            database
                .query_row("SELECT count(*) FROM records", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(database);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exact_v1_recovery_keeps_durable_raw_claim_after_old_reader_mutates_primary_rows() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-v1-exact-crash-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("legacy.sqlite");
        let database = Connection::open(&path).unwrap();
        database.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=DELETE; PRAGMA application_id=1430408522; PRAGMA user_version=1; CREATE TABLE journal(id INTEGER PRIMARY KEY CHECK(id=1), recording_id TEXT NOT NULL, declaration TEXT NOT NULL, max_bytes INTEGER NOT NULL, max_records INTEGER NOT NULL, phase TEXT NOT NULL, next_ordinal INTEGER NOT NULL, next_token INTEGER NOT NULL, lost INTEGER NOT NULL, pending_token TEXT, pending_last INTEGER, pending_more INTEGER, last_ack_token TEXT, last_ack_receipt TEXT, retained_records INTEGER NOT NULL, retained_bytes INTEGER NOT NULL, collection_failure TEXT); CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL); INSERT INTO journal VALUES(1,'legacy','{}',1048576,10,'recording',2,2,0,'1',1,0,NULL,NULL,1,7,NULL); INSERT INTO records VALUES(1,'{\"v\":1}',7);").unwrap();
        drop(database);

        let registry = JournalRegistry::default();
        registry.configure_directory(&directory).unwrap();
        let page = registry
            .legacy_recording_prepare_exact("legacy", 10, 4096)
            .unwrap();
        // Model a crashed newer process: the operating-system lock goes away,
        // while its exact receipt remains on disk. A released reader can now
        // acknowledge and delete the primary row before recovery resumes.
        authorities()
            .unwrap()
            .legacy_recovery_authorities
            .remove(&path);
        let database = Connection::open(&path).unwrap();
        database
            .execute("DELETE FROM records WHERE ordinal=1", [])
            .unwrap();
        database
            .execute("UPDATE journal SET retained_records=0,retained_bytes=0,pending_token=NULL,pending_last=NULL,pending_more=NULL,last_ack_token='1',last_ack_receipt='{\"token\":\"1\",\"acknowledged\":true,\"records\":1}' WHERE id=1", [])
            .unwrap();
        drop(database);

        let restored = registry
            .legacy_recording_recover_exact("legacy")
            .unwrap()
            .unwrap();
        assert_eq!(restored, page);
        assert_eq!(
            registry
                .legacy_recording_acknowledge_exact("legacy", &restored)
                .unwrap_err()
                .kind,
            "storage.corrupt"
        );
        let database = Connection::open(&path).unwrap();
        let claim: String = database
            .query_row(
                "SELECT claim FROM legacy_exact_receipt WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(decode(&claim).unwrap(), page);
        drop(database);
        authorities()
            .unwrap()
            .legacy_recovery_authorities
            .remove(&path);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn receipt_timestamp_storage_round_trip_preserves_float_bits() {
        let base = 1_760_662_567_890.0_f64.to_bits();
        for offset in 0..4096 {
            let timestamp = f64::from_bits(base + offset);
            let encoded = serde_json::to_string(&timestamp).unwrap();
            let decoded: f64 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(
                timestamp.to_bits(),
                decoded.to_bits(),
                "timestamp {encoded}"
            );
        }
    }

    #[test]
    fn held_export_transaction_blocks_shared_writer_without_sqlite_busy() {
        let directory =
            std::env::temp_dir().join(format!("ubm-held-export-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let writer_registry = JournalRegistry::default();
        let export_registry = JournalRegistry::default();
        writer_registry.configure_directory(&directory).unwrap();
        export_registry.configure_directory(&directory).unwrap();
        let writer = writer_registry
            .open(
                "held",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 100,
                },
            )
            .unwrap();
        let export = export_registry.get("held").unwrap();
        let mut connection = export.connection().unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let (ready, started) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            // Deterministic assertion at the admission boundary: the writer
            // encounters this exact held export mutex, not a second DB handle.
            assert!(matches!(
                writer.shared.connection.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            ready.send(()).unwrap();
            writer.append(&json!({}), &json!({"value":1})).unwrap();
            assert_eq!(writer.status().unwrap()["collectionFailure"], Value::Null);
        });
        started
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        tx.commit().unwrap();
        drop(connection);
        worker.join().unwrap();
        assert_eq!(export.status().unwrap()["records"], 1);
    }

    #[cfg(windows)]
    #[test]
    fn windows_scan_recovery_protector_provisions_the_real_directory_and_sidecar_route() {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        };
        use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;

        let directory = std::env::temp_dir().join(format!(
            "ubm-windows-scan-recovery-acl-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let ordinary_file = directory.join("ordinary-preexisting-file");
        std::fs::write(&ordinary_file, b"before-root-protection").unwrap();
        let protector = WindowsCurrentUserScanRecoveryStorageProtector::new();
        assert_eq!(
            protector.capability(),
            "windows-current-user-protected-dacl"
        );
        // This is the same sequence as a trusted Windows host: protect the
        // configured root, then the dedicated recovery directory, before
        // creating and protecting SQLite's sidecar.
        protector.provision_directory(&directory).unwrap();
        protector.validate_directory(&directory).unwrap();
        assert_eq!(
            std::fs::read(&ordinary_file).unwrap(),
            b"before-root-protection"
        );
        let mut ordinary_file_handle = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&ordinary_file)
            .unwrap();
        ordinary_file_handle
            .write_all(b"after-root-protection")
            .unwrap();
        drop(ordinary_file_handle);
        assert_eq!(
            std::fs::read(&ordinary_file).unwrap(),
            b"after-root-protection"
        );
        let recovery_directory = directory.join(SCAN_RECOVERY_DIRECTORY);
        std::fs::create_dir(&recovery_directory).unwrap();
        protector.provision_directory(&recovery_directory).unwrap();
        protector.validate_directory(&recovery_directory).unwrap();
        let path = recovery_directory.join("scan-failure-recovery.sqlite");
        std::fs::File::create(&path).unwrap();
        protector.provision(&path).unwrap();
        // This is a second, independent open of the final component.
        protector.validate(&path).unwrap();

        // A null DACL means full access to everyone. The validator must never
        // reinterpret that as a private default.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .access_mode(WRITE_DAC)
            .open(&path)
            .unwrap();
        assert_eq!(
            unsafe {
                SetSecurityInfo(
                    file.as_raw_handle().cast(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null(),
                    core::ptr::null(),
                )
            },
            0
        );
        drop(file);
        assert_eq!(
            protector.validate(&path).unwrap_err().kind,
            "storage.permission"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn scan_cleanup_retirement_reclaims_thousands_of_distinct_receipts() {
        const RECEIPTS: u64 = 1_024;

        let directory = std::env::temp_dir().join(format!(
            "ubm-scan-cleanup-reclaim-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        private_fixture_directory(&directory);
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: RECEIPTS,
        };
        let journal = open_current_v2_scan_fixture(&path, "scan", quota);

        journal.scan_failure_cleanup_retirements().unwrap();

        fn insert_receipts(journal: &ContinuationJournal, quota: JournalQuota, start: u64) {
            let mut connection = open_scan_failure_sidecar(
                &journal.scan_failure_sidecar_path,
                quota,
                journal.scan_recovery_storage.as_deref(),
            )
            .unwrap();
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            for index in start..start + RECEIPTS {
                let mut identity = [0_u8; 80];
                identity[..8].copy_from_slice(&index.to_be_bytes());
                let mut receipt = [0_u8; 32];
                receipt[..8].copy_from_slice(&index.to_be_bytes());
                tx.execute(
                    "INSERT INTO occurrences(identity,envelope) VALUES(?1,?2)",
                    params![identity.as_slice(), receipt.as_slice()],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        insert_receipts(&journal, quota, 0);
        let retirements = journal.scan_failure_cleanup_retirements().unwrap();
        assert_eq!(retirements.len(), RECEIPTS as usize);
        // A forged capability must leave the genuine receipt retained.
        let mut forged_authenticator = retirements[0].1;
        forged_authenticator[0] ^= 1;
        assert!(
            journal
                .retire_scan_failure_cleanup_receipt(&retirements[0].0, &forged_authenticator)
                .is_err()
        );
        assert!(
            journal
                .has_scan_failure_cleanup_receipt(&retirements[0].0)
                .unwrap()
        );
        // A valid capability is bound to one receipt: crossing independent
        // identities is rejected rather than reclaiming either proof.
        assert!(
            journal
                .retire_scan_failure_cleanup_receipt(&retirements[1].0, &retirements[0].1)
                .is_err()
        );
        for (receipt, authenticator) in &retirements {
            journal
                .retire_scan_failure_cleanup_receipt(receipt, authenticator)
                .unwrap();
        }
        assert!(
            journal
                .scan_failure_cleanup_retirements()
                .unwrap()
                .is_empty()
        );
        // The same quota accepts an equally large next generation after every
        // confirmed receipt is reclaimed; no suffix receipt was evicted.
        insert_receipts(&journal, quota, RECEIPTS);
        assert_eq!(
            journal.scan_failure_cleanup_retirements().unwrap().len(),
            RECEIPTS as usize
        );
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn scan_cleanup_receipt_index_migrates_v2_and_rejects_conflicting_legacy_rows() {
        let directory = std::env::temp_dir().join(format!(
            "ubm-scan-cleanup-index-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        private_fixture_directory(&directory);
        let path = directory.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 16,
        };
        let journal = open_current_v2_scan_fixture(&path, "scan", quota);
        let sidecar = journal.scan_failure_sidecar_path.clone();
        // Establish a current v2 sidecar, then emulate an unindexed v2
        // database from before this additive repair.
        assert!(
            journal
                .scan_failure_cleanup_retirements()
                .unwrap()
                .is_empty()
        );

        let legacy_receipt = [7_u8; 32];
        {
            let connection = Connection::open(&sidecar).unwrap();
            connection
                .execute_batch(
                    "DROP INDEX scan_failure_cleanup_receipt_by_envelope; \
                     INSERT INTO occurrences(identity,envelope) VALUES(zeroblob(80),NULL);",
                )
                .unwrap();
            // Seed the legacy receipt while the deliberately older v2
            // sidecar lacks its later additive index.
            connection
                .execute(
                    "UPDATE occurrences SET envelope=?1 WHERE identity=zeroblob(80)",
                    [legacy_receipt.as_slice()],
                )
                .unwrap();
        }
        let retirements = journal.scan_failure_cleanup_retirements().unwrap();
        assert_eq!(retirements.len(), 1);
        assert_eq!(retirements[0].0, legacy_receipt);
        let connection = Connection::open(&sidecar).unwrap();
        let schema: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name=?1",
                [SCAN_FAILURE_CLEANUP_RECEIPT_INDEX_NAME],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            normalized_schema_sql(&schema),
            normalized_schema_sql(SCAN_FAILURE_CLEANUP_RECEIPT_INDEX_SCHEMA)
        );
        connection
            .execute_batch("DROP INDEX scan_failure_cleanup_receipt_by_envelope;")
            .unwrap();
        let first_identity = [1_u8; 80];
        let second_identity = [2_u8; 80];
        let conflicting_receipt = [9_u8; 32];
        connection
            .execute(
                "INSERT INTO occurrences(identity,envelope) VALUES(?1,?2)",
                params![first_identity.as_slice(), conflicting_receipt.as_slice()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO occurrences(identity,envelope) VALUES(?1,?2)",
                params![second_identity.as_slice(), conflicting_receipt.as_slice()],
            )
            .unwrap();
        drop(connection);

        let failure = journal.scan_failure_cleanup_retirements().unwrap_err();
        assert_eq!(failure.kind, "storage.corrupt");
        drop(journal);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
