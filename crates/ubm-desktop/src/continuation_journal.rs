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
//! References: sqlite.org/fileformat2.html#the_rollback_journal, pragma.html.

use std::collections::HashMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use serde_json::{Value, json};

const PAGE: u64 = 4096;
const OVERHEAD: u64 = 65536;
const RESERVED_PAGES: u64 = 16;
const APPLICATION_ID: i64 = 0x55424d4a;
const SCHEMA_VERSION: i64 = 1;

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

#[derive(Default)]
struct RegistryState {
    directory: Option<PathBuf>,
    journals: HashMap<String, Arc<ContinuationJournal>>,
}

/// Host-private path authority and durable owners, independent of radio sessions.
/// Public callers supply only safe recording IDs; no path crosses that boundary.
#[derive(Default)]
pub struct JournalRegistry {
    state: Mutex<RegistryState>,
}
impl JournalRegistry {
    pub fn configure_directory(&self, path: &Path) -> Result<Value> {
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
        state.directory = Some(canonical);
        Ok(json!({"state":"configured","encrypted":false}))
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
        Ok(directory.join(format!("{id}.sqlite")))
    }
    pub fn open(
        &self,
        id: &str,
        declaration: &Value,
        quota: JournalQuota,
    ) -> Result<Arc<ContinuationJournal>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("storage.io", "recording registry interrupted"))?;
        let path = Self::location(&state, id)?;
        if let Some(journal) = state.journals.get(id) {
            if journal.quota != quota || journal.declaration != encoded(declaration, 65536)? {
                return Err(error(
                    "storage.identity",
                    "recording identity or quota differs; file retained",
                ));
            }
            return Ok(journal.clone());
        }
        let journal = Arc::new(ContinuationJournal::open(&path, id, declaration, quota)?);
        state.journals.insert(id.to_owned(), journal.clone());
        Ok(journal)
    }
    pub fn get(&self, id: &str) -> Result<Arc<ContinuationJournal>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("storage.io", "recording registry interrupted"))?;
        let path = Self::location(&state, id)?;
        if let Some(journal) = state.journals.get(id) {
            return Ok(journal.clone());
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
        let journal = Arc::new(ContinuationJournal::open(
            &path,
            id,
            &declaration,
            JournalQuota {
                max_bytes,
                max_records,
            },
        )?);
        state.journals.insert(id.to_owned(), journal.clone());
        Ok(journal)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalQuota {
    pub max_bytes: u64,
    pub max_records: u64,
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

/// One handle to one durable recording and its single shared delivery cursor.
/// Multiple authorized handles are serialized by SQLite IMMEDIATE transactions;
/// they are not independent owners. The host authorizes access and retains the
/// authoritative recording owner. Tokens protect cursor identity, not access.
pub struct ContinuationJournal {
    connection: Mutex<Connection>,
    quota: JournalQuota,
    page_limit: u64,
    recording_id: String,
    declaration: String,
    runtime_failure: Mutex<Option<JournalError>>,
    collection_failure: Mutex<Option<Value>>,
}

impl ContinuationJournal {
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
        Self::open_inner(path, recording_id, declaration, quota)
            .map_err(|failure| failure.at("open"))
    }
    fn open_inner(
        path: &Path,
        recording_id: &str,
        declaration: &Value,
        quota: JournalQuota,
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
            let application: i64 =
                connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
            let page_size =
                connection.pragma_query_value(None, "page_size", |row| unsigned(row, 0))?;
            if schema != SCHEMA_VERSION || application != APPLICATION_ID || page_size != PAGE {
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
            tx.execute_batch("CREATE TABLE journal(id INTEGER PRIMARY KEY CHECK(id=1), recording_id TEXT NOT NULL, declaration TEXT NOT NULL, max_bytes INTEGER NOT NULL, max_records INTEGER NOT NULL, phase TEXT NOT NULL, next_ordinal INTEGER NOT NULL, next_token INTEGER NOT NULL, lost INTEGER NOT NULL, pending_token TEXT, pending_last INTEGER, pending_more INTEGER, last_ack_token TEXT, last_ack_receipt TEXT, retained_records INTEGER NOT NULL, retained_bytes INTEGER NOT NULL, collection_failure TEXT); CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL);")?;
            tx.execute("INSERT INTO journal VALUES(1,?1,?2,?3,?4,'recording',1,1,0,NULL,NULL,NULL,NULL,NULL,0,0,NULL)", params![recording_id,declaration,sql_integer(quota.max_bytes)?,sql_integer(quota.max_records)?])?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.commit()?;
        }
        let retained_failure: Option<String> = connection.query_row(
            "SELECT collection_failure FROM journal WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        let collection_failure = retained_failure.map(|value| decode(&value)).transpose()?;
        Ok(Self {
            connection: Mutex::new(connection),
            quota,
            page_limit,
            recording_id: recording_id.to_owned(),
            declaration,
            runtime_failure: Mutex::new(None),
            collection_failure: Mutex::new(collection_failure),
        })
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| error("storage.io", "journal writer was interrupted"))
    }

    /// The collection owner declares a terminal admission failure. The local
    /// marker survives native session release through the registry-owned Arc.
    /// Persisting the stop is best effort, with its failure explicitly retained;
    /// inability to write never becomes a false claim of durable failure history.
    pub fn mark_collection_failure(&self, failure: &JournalError) {
        let connection = self.connection();
        let mut state = self
            .collection_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.is_some() {
            return;
        }
        let mut diagnostic = diagnostic(failure);
        diagnostic["persisted"] = json!(true);
        let result = connection.and_then(|connection| {
            connection
                .execute(
                    "UPDATE journal SET phase='stopped',collection_failure=?1 WHERE id=1 AND collection_failure IS NULL",
                    [diagnostic.to_string()],
                )
                .map_err(JournalError::from)?;
            let first:String=connection.query_row("SELECT collection_failure FROM journal WHERE id=1",[],|row|row.get(0))?;
            decode(&first)
        });
        match result {
            Ok(first) => diagnostic = first,
            Err(error) => {
                diagnostic["persisted"] = json!(false);
                diagnostic["persistenceFailure"] =
                    self::diagnostic(&error.at("mark-collection-failure"));
            }
        }
        *state = Some(diagnostic);
    }

    /// Persists context and value together before returning acceptance. Context
    /// must identify the process/session epoch, peer and generation-bound selector.
    pub fn append(&self, metadata: &Value, record: &Value) -> Result<Value> {
        let outcome = self
            .append_inner(metadata, record)
            .map_err(|failure| failure.at("append"));
        if let Err(failure) = &outcome
            && let Ok(mut stored) = self.runtime_failure.lock()
        {
            *stored = Some(failure.clone());
        }
        outcome
    }
    fn append_inner(&self, metadata: &Value, record: &Value) -> Result<Value> {
        if !metadata.is_object() || !record.is_object() {
            return Err(error(
                "argument.invalid",
                "record and context must be objects",
            ));
        }
        encoded(metadata, 16384)?;
        encoded(record, 65536)?;
        let mut connection = self.connection()?;
        if self
            .collection_failure
            .lock()
            .map_err(|_| error("storage.io", "collection failure state interrupted"))?
            .is_some()
        {
            return Err(error(
                "storage.collection-failed",
                "collection terminated; retained records remain readable",
            ));
        }
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (phase, ordinal, count): (String, i64, u64) = tx.query_row(
            "SELECT phase,next_ordinal,retained_records FROM journal WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, unsigned(row, 2)?)),
        )?;
        if phase == "capacity-reached" {
            tx.execute("UPDATE journal SET lost=CASE WHEN lost<9223372036854775807 THEN lost+1 ELSE lost END WHERE id=1",[])?;
            tx.commit()?;
            return Err(error(
                "storage.full",
                "recording capacity reached; unacknowledged records retained",
            ));
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
        let body = encoded(
            &json!({"ordinal":ordinal,"metadata":metadata,"record":record}),
            82944,
        )?;
        let pages = tx.pragma_query_value(None, "page_count", |row| unsigned(row, 0))?;
        let free = tx.pragma_query_value(None, "freelist_count", |row| unsigned(row, 0))?;
        // Reserve schema/cursor/loss update space; conservatively cover a record's
        // overflow pages and B-tree split path before admitting its transaction.
        let needed = (body.len() as u64).div_ceil(PAGE) + 12;
        if count >= self.quota.max_records
            || pages.saturating_sub(free) + needed + RESERVED_PAGES > self.page_limit
        {
            tx.execute(
                "UPDATE journal SET phase='capacity-reached',lost=lost+1 WHERE id=1",
                [],
            )?;
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
            "INSERT INTO records VALUES(?1,?2,?3)",
            params![ordinal, body, sql_integer(body.len() as u64)?],
        )?;
        tx.execute("UPDATE journal SET next_ordinal=?1,retained_records=retained_records+1,retained_bytes=retained_bytes+?2 WHERE id=1", params![next,sql_integer(body.len() as u64)?])?;
        tx.commit()?;
        Ok(json!({"accepted":true,"ordinal":ordinal}))
    }

    /// A prepared prefix is stable until explicit acknowledgement, including
    /// across process death. `more` is the snapshot at preparation, not a live count.
    pub fn prepare(&self, max_items: u32, max_bytes: u32) -> Result<Value> {
        self.prepare_inner(max_items, max_bytes)
            .map_err(|failure| failure.at("prepare"))
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
            let answer = prepared(
                &tx,
                &token,
                last.ok_or_else(|| error("storage.corrupt", "prepared cursor is incomplete"))?,
                more.unwrap_or(false),
            )?;
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
        {
            let mut statement =
                tx.prepare("SELECT ordinal,bytes FROM records ORDER BY ordinal LIMIT 2049")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let size = unsigned(row, 1)?;
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
        tx.commit()?;
        Ok(answer)
    }

    pub fn acknowledge(&self, token: &str) -> Result<Value> {
        self.acknowledge_inner(token)
            .map_err(|failure| failure.at("acknowledge"))
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
        let removed_bytes: i64 = tx.query_row(
            "SELECT coalesce(sum(bytes),0) FROM records WHERE ordinal<=?1",
            [last],
            |row| row.get(0),
        )?;
        let removed = tx.execute("DELETE FROM records WHERE ordinal<=?1", [last])?;
        tx.execute("UPDATE journal SET retained_records=retained_records-?1,retained_bytes=retained_bytes-?2 WHERE id=1",params![sql_integer(removed as u64)?,removed_bytes])?;
        let receipt = json!({"token":token,"acknowledged":true,"records":removed});
        tx.execute("UPDATE journal SET pending_token=NULL,pending_last=NULL,pending_more=NULL,last_ack_token=?1,last_ack_receipt=?2 WHERE id=1",params![token,receipt.to_string()])?;
        tx.commit()?;
        Ok(receipt)
    }

    pub fn stop(&self) -> Result<Value> {
        self.stop_inner().map_err(|failure| failure.at("stop"))
    }
    fn stop_inner(&self) -> Result<Value> {
        self.connection()?
            .execute("UPDATE journal SET phase='stopped' WHERE id=1", [])?;
        self.status()
    }

    /// Explicit destructive operation, never invoked by radio release or Drop.
    /// Sequence counters survive clear so old tokens cannot alias future batches.
    pub fn clear(&self) -> Result<Value> {
        self.clear_inner().map_err(|failure| failure.at("clear"))
    }
    fn clear_inner(&self) -> Result<Value> {
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
        let removed = tx.execute("DELETE FROM records", [])?;
        tx.execute("UPDATE journal SET pending_token=NULL,pending_last=NULL,pending_more=NULL,last_ack_token=NULL,last_ack_receipt=NULL,lost=0,retained_records=0,retained_bytes=0 WHERE id=1",[])?;
        tx.commit()?;
        Ok(json!({"cleared":true,"records":removed}))
    }

    pub fn status(&self) -> Result<Value> {
        self.status_inner().map_err(|failure| failure.at("status"))
    }
    fn status_inner(&self) -> Result<Value> {
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
        let runtime_failure = self
            .runtime_failure
            .lock()
            .map_err(|_| error("storage.io", "journal diagnostic lock interrupted"))?;
        let collection_failure = self
            .collection_failure
            .lock()
            .map_err(|_| error("storage.io", "collection failure state interrupted"))?;
        let collection_failure = collection_failure
            .clone()
            .or(persisted_failure.map(|value| decode(&value)).transpose()?);
        Ok(
            json!({"recordingId":self.recording_id,"phase":phase,"accepting":phase=="recording" && collection_failure.is_none(),"records":records,"bytes":bytes,"lostRecords":lost,"maxBytes":self.quota.max_bytes,"maxRecords":self.quota.max_records,"encrypted":false,"collectionFailure":collection_failure,"runtimeFailure":runtime_failure.as_ref().map(|failure|{let mut result=diagnostic(failure);result["persisted"]=json!(false);result})}),
        )
    }
}

fn diagnostic(failure: &JournalError) -> Value {
    json!({"kind":failure.kind,"detail":failure.detail,"operation":failure.operation,"sqliteExtendedCode":failure.sqlite_extended_code,"sqliteCode":failure.sqlite_code})
}

fn prepared(connection: &Connection, token: &str, last: i64, more: bool) -> Result<Value> {
    let mut statement =
        connection.prepare("SELECT body,bytes FROM records WHERE ordinal<=?1 ORDER BY ordinal")?;
    let mut rows = statement.query([last])?;
    let mut records = Vec::new();
    let mut bytes = 0u64;
    while let Some(row) = rows.next()? {
        let body: String = row.get(0)?;
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
