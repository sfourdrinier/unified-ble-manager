use serde_json::json;
use ubm_desktop::continuation_journal::{ContinuationJournal, JournalQuota, JournalRegistry};

#[test]
fn legacy_page_replays_then_acknowledges_and_migrates_without_forging_identity() {
    let dir = std::env::temp_dir().join(format!(
        "ubm-legacy-recovery-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("recording.sqlite");
    let quota = JournalQuota {
        max_bytes: 1 << 20,
        max_records: 100,
    };
    let journal = ContinuationJournal::open(&path, "recording", &json!({}), quota).unwrap();
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.append(&json!({}), &json!({"n":2})).unwrap();
    let page = journal.prepare(1, 4096).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; DROP TABLE evidence_cursor; DROP TABLE evidence_claim_records; DROP TABLE evidence_claim_header; DROP TABLE evidence_members; DROP TABLE evidence_members_header; DROP TABLE recording_claim_records; DROP TABLE recording_claim_header; ALTER TABLE records RENAME TO new_records; CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL); INSERT INTO records SELECT ordinal,body,bytes FROM new_records; DROP TABLE new_records; PRAGMA user_version=2;").unwrap();
    drop(db);
    let registry = JournalRegistry::default();
    registry.configure_directory(&dir).unwrap();
    let recovered = registry
        .legacy_recording_prepare("recording", 1, 4096)
        .unwrap();
    let mut decoded = recovered.clone();
    decoded.as_object_mut().unwrap().remove("rawRecords");
    assert_eq!(decoded, page);
    assert!(
        registry
            .legacy_recording_prepare("recording", 1, 1)
            .is_err()
    );
    let token = page["token"].as_str().unwrap();
    let ack = registry
        .legacy_recording_acknowledge("recording", token)
        .unwrap();
    assert_eq!(ack["records"], 1);
    assert_eq!(
        registry
            .legacy_recording_acknowledge("recording", token)
            .unwrap(),
        ack
    );
    let reopened = registry.get("recording").unwrap();
    assert_eq!(reopened.acknowledge(token).unwrap(), ack);
    assert_eq!(
        reopened.prepare(1, 4096).unwrap()["records"][0]["record"]["n"],
        2
    );
}

#[test]
fn active_evidence_fences_recording_ack_and_preserves_both_claims() {
    let dir = std::env::temp_dir().join(format!(
        "ubm-legacy-evidence-fence-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("recording.sqlite");
    let quota = JournalQuota {
        max_bytes: 1 << 20,
        max_records: 100,
    };
    let journal = ContinuationJournal::open(&path, "recording", &json!({}), quota).unwrap();
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    let page = journal.prepare(1, 4096).unwrap();
    journal.begin_evidence_transfer("evidence").unwrap();
    let evidence = journal
        .prepare_evidence_transfer("evidence", 1, 4096)
        .unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; DROP TABLE evidence_members; DROP TABLE evidence_members_header; DROP TABLE recording_claim_records; DROP TABLE recording_claim_header; PRAGMA user_version=4;").unwrap();
    drop(db);
    let registry = JournalRegistry::default();
    registry.configure_directory(&dir).unwrap();
    assert_eq!(
        registry
            .legacy_recording_acknowledge("recording", page["token"].as_str().unwrap())
            .unwrap_err()
            .kind,
        "storage.schema"
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert_eq!(
        db.query_row("SELECT pending_token FROM journal", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        page["token"].as_str().unwrap()
    );
    assert_eq!(
        db.query_row("SELECT pending_token FROM evidence_cursor", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        evidence["token"].as_str().unwrap()
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn legacy_ack_with_concurrent_sqlite_reader_retries_without_partial_commit() {
    let dir = std::env::temp_dir().join(format!(
        "ubm-legacy-reader-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("recording.sqlite");
    let quota = JournalQuota {
        max_bytes: 1 << 20,
        max_records: 100,
    };
    let journal = ContinuationJournal::open(&path, "recording", &json!({}), quota).unwrap();
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    let page = journal.prepare(1, 4096).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; DROP TABLE evidence_cursor; DROP TABLE evidence_claim_records; DROP TABLE evidence_claim_header; DROP TABLE evidence_members; DROP TABLE evidence_members_header; DROP TABLE recording_claim_records; DROP TABLE recording_claim_header; ALTER TABLE records RENAME TO new_records; CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL); INSERT INTO records SELECT ordinal,body,bytes FROM new_records; DROP TABLE new_records; PRAGMA user_version=2; BEGIN;").unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let registry = JournalRegistry::default();
    registry.configure_directory(&dir).unwrap();
    let token = page["token"].as_str().unwrap();
    assert_eq!(
        registry
            .legacy_recording_acknowledge("recording", token)
            .unwrap_err()
            .kind,
        "storage.busy"
    );
    db.execute_batch("COMMIT").unwrap();
    let recovered = registry
        .legacy_recording_prepare("recording", 1, 4096)
        .unwrap();
    let mut decoded = recovered.clone();
    decoded.as_object_mut().unwrap().remove("rawRecords");
    assert_eq!(decoded, page);
    let receipt = registry
        .legacy_recording_acknowledge("recording", token)
        .unwrap();
    assert_eq!(receipt["records"], 1);
    assert_eq!(
        registry
            .legacy_recording_acknowledge("recording", token)
            .unwrap(),
        receipt
    );
}

#[test]
fn compatibility_reader_rejects_current_schema_instead_of_downgrading_it() {
    let dir = std::env::temp_dir().join(format!(
        "ubm-legacy-downgrade-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("recording.sqlite");
    let quota = JournalQuota {
        max_bytes: 1 << 20,
        max_records: 100,
    };
    let journal = ContinuationJournal::open(&path, "recording", &json!({}), quota).unwrap();
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    let page = journal.prepare(1, 4096).unwrap();
    drop(journal);
    let registry = JournalRegistry::default();
    registry.configure_directory(&dir).unwrap();
    assert_eq!(
        registry
            .legacy_recording_acknowledge("recording", page["token"].as_str().unwrap())
            .unwrap_err()
            .kind,
        "storage.schema"
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn orphaned_legacy_evidence_blocks_migration_and_preserves_rows() {
    let dir = std::env::temp_dir().join(format!(
        "ubm-legacy-orphan-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("recording.sqlite");
    let quota = JournalQuota {
        max_bytes: 1 << 20,
        max_records: 100,
    };
    let journal = ContinuationJournal::open(&path, "recording", &json!({}), quota).unwrap();
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; UPDATE records SET recording_acked=1; PRAGMA user_version=4;").unwrap();
    drop(db);
    let registry = JournalRegistry::default();
    registry.configure_directory(&dir).unwrap();
    assert!(registry.get("recording").is_err());
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM records WHERE recording_acked=1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn orphaned_legacy_evidence_progress_blocks_migration() {
    for (version, column) in [
        (3, "cutoff"),
        (3, "progressed"),
        (4, "cutoff"),
        (4, "progressed"),
    ] {
        let dir = std::env::temp_dir().join(format!(
            "ubm-legacy-orphan-{version}-{column}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recording.sqlite");
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        };
        let journal = ContinuationJournal::open(&path, "recording", &json!({}), quota).unwrap();
        journal.append(&json!({}), &json!({"n":1})).unwrap();
        drop(journal);
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(&format!("DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; UPDATE evidence_cursor SET {column}=1; PRAGMA user_version={version};")).unwrap();
        drop(db);
        let registry = JournalRegistry::default();
        registry.configure_directory(&dir).unwrap();
        assert_eq!(
            registry.get("recording").err().unwrap().kind,
            "storage.schema"
        );
        let db = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            version
        );
        assert_eq!(
            db.query_row(&format!("SELECT {column} FROM evidence_cursor"), [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}

#[test]
fn legacy_prepare_exposes_exact_stored_body_bytes() {
    let dir = std::env::temp_dir().join(format!(
        "ubm-legacy-bytes-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("recording.sqlite");
    let journal = ContinuationJournal::open(
        &path,
        "recording",
        &json!({}),
        JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        },
    )
    .unwrap();
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.prepare(1, 4096).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    let raw = r#"{ "ordinal":1, "metadata":{}, "record":{"n":1.000e+0} }"#;
    db.execute(
        "UPDATE records SET body=?1, bytes=?2",
        rusqlite::params![raw, raw.len() as i64],
    )
    .unwrap();
    db.execute("UPDATE journal SET retained_bytes=?1", [raw.len() as i64])
        .unwrap();
    db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; DROP TABLE evidence_cursor; DROP TABLE evidence_claim_records; DROP TABLE evidence_claim_header; DROP TABLE evidence_members; DROP TABLE evidence_members_header; DROP TABLE recording_claim_records; DROP TABLE recording_claim_header; ALTER TABLE records RENAME TO new_records; CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL); INSERT INTO records SELECT ordinal,body,bytes FROM new_records; DROP TABLE new_records; PRAGMA user_version=2;").unwrap();
    drop(db);
    let registry = JournalRegistry::default();
    registry.configure_directory(&dir).unwrap();
    let page = registry
        .legacy_recording_prepare("recording", 1, 4096)
        .unwrap();
    assert_eq!(
        page["rawRecords"][0],
        json!({"ordinal":1,"body":raw.as_bytes()})
    );
    assert_eq!(page["records"][0]["record"]["n"], json!(1.0));
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE records SET bytes=bytes+1", []).unwrap();
    db.execute("UPDATE journal SET retained_bytes=retained_bytes+1", [])
        .unwrap();
    drop(db);
    assert_eq!(
        registry
            .legacy_recording_prepare("recording", 1, 4096)
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT body FROM records", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        raw
    );
}

#[test]
fn exact_legacy_ack_rejects_changed_bytes_and_retains_receipt_until_retired() {
    let dir = std::env::temp_dir().join(format!(
        "ubm-legacy-exact-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("recording.sqlite");
    let journal = ContinuationJournal::open(
        &path,
        "recording",
        &json!({}),
        JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        },
    )
    .unwrap();
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.prepare(1, 4096).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; DROP TABLE evidence_cursor; DROP TABLE evidence_claim_records; DROP TABLE evidence_claim_header; DROP TABLE evidence_members; DROP TABLE evidence_members_header; DROP TABLE recording_claim_records; DROP TABLE recording_claim_header; ALTER TABLE records RENAME TO new_records; CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL); INSERT INTO records SELECT ordinal,body,bytes FROM new_records; DROP TABLE new_records; PRAGMA user_version=2;").unwrap();
    drop(db);
    let first = JournalRegistry::default();
    first.configure_directory(&dir).unwrap();
    let page = first
        .legacy_recording_prepare_exact("recording", 1, 4096)
        .unwrap();
    let mut wrong = page.clone();
    wrong["rawRecords"][0]["body"][0] = json!(32);
    assert!(
        first
            .legacy_recording_acknowledge_exact("recording", &wrong)
            .is_err()
    );
    let receipt = first
        .legacy_recording_acknowledge_exact("recording", &page)
        .unwrap();
    drop(first);
    let second = JournalRegistry::default();
    second.configure_directory(&dir).unwrap();
    assert_eq!(
        second
            .legacy_recording_acknowledge_exact("recording", &page)
            .unwrap(),
        receipt
    );
    assert!(
        second
            .legacy_recording_acknowledge_exact("recording", &wrong)
            .is_err()
    );
    assert!(second.get("recording").is_err());
    assert!(
        second
            .legacy_recording_acknowledge("recording", page["token"].as_str().unwrap())
            .is_err()
    );
    second
        .legacy_recording_retire_exact("recording", &page)
        .unwrap();
    assert!(second.get("recording").is_ok());
}

fn exact_fixture(name: &str) -> (std::path::PathBuf, JournalRegistry, serde_json::Value) {
    let dir = std::env::temp_dir().join(format!(
        "ubm-exact-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("recording.sqlite");
    let journal = ContinuationJournal::open(
        &path,
        "recording",
        &json!({}),
        JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        },
    )
    .unwrap();
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.prepare(1, 4096).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    let raw = r#"{ "ordinal":1, "metadata":{}, "record":{"n":1.000e+0} }"#;
    db.execute(
        "UPDATE records SET body=?1,bytes=?2",
        rusqlite::params![raw, raw.len() as i64],
    )
    .unwrap();
    db.execute("UPDATE journal SET retained_bytes=?1", [raw.len() as i64])
        .unwrap();
    db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; DROP TABLE evidence_cursor; DROP TABLE evidence_claim_records; DROP TABLE evidence_claim_header; DROP TABLE evidence_members; DROP TABLE evidence_members_header; DROP TABLE recording_claim_records; DROP TABLE recording_claim_header; ALTER TABLE records RENAME TO new_records; CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL); INSERT INTO records SELECT ordinal,body,bytes FROM new_records; DROP TABLE new_records; PRAGMA user_version=2;").unwrap();
    drop(db);
    let registry = JournalRegistry::default();
    registry.configure_directory(&dir).unwrap();
    let page = registry
        .legacy_recording_prepare_exact("recording", 1, 4096)
        .unwrap();
    (dir, registry, page)
}

#[test]
fn missing_exact_receipt_is_corruption_and_blocks_migration() {
    let (dir, registry, page) = exact_fixture("missing-receipt");
    registry
        .legacy_recording_acknowledge_exact("recording", &page)
        .unwrap();
    let db = rusqlite::Connection::open(dir.join("recording.sqlite")).unwrap();
    db.execute("DELETE FROM legacy_exact_receipt", []).unwrap();
    drop(db);
    assert_eq!(
        registry
            .legacy_recording_retire_exact("recording", &page)
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    assert!(registry.get("recording").is_err());
}

#[test]
fn retirement_reply_replays_after_another_client_migrates() {
    let (dir, registry, page) = exact_fixture("retire-migrate");
    registry
        .legacy_recording_acknowledge_exact("recording", &page)
        .unwrap();
    let receipt = registry
        .legacy_recording_retire_exact("recording", &page)
        .unwrap();
    let other = JournalRegistry::default();
    other.configure_directory(&dir).unwrap();
    let journal = other.get("recording").unwrap();
    assert_eq!(
        registry
            .legacy_recording_retire_exact("recording", &page)
            .unwrap(),
        receipt
    );
    let mut changed = page.clone();
    changed["rawRecords"][0]["body"][0] = json!(32);
    assert!(
        registry
            .legacy_recording_retire_exact("recording", &changed)
            .is_err()
    );
    drop(journal);
}

#[test]
fn concurrent_retirements_share_one_replayable_result() {
    let (dir, registry, page) = exact_fixture("concurrent-retire");
    registry
        .legacy_recording_acknowledge_exact("recording", &page)
        .unwrap();
    drop(registry);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let dir = dir.clone();
            let page = page.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let registry = JournalRegistry::default();
                registry.configure_directory(&dir).unwrap();
                barrier.wait();
                registry
                    .legacy_recording_retire_exact("recording", &page)
                    .unwrap()
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(results[0], results[1]);
    let next = JournalRegistry::default();
    next.configure_directory(&dir).unwrap();
    assert_eq!(
        next.legacy_recording_retire_exact("recording", &page)
            .unwrap(),
        results[0]
    );
}

#[test]
fn retirement_replays_while_another_process_owns_the_migrated_journal() {
    if let Ok(dir) = std::env::var("UBM_EXACT_MIGRATED_OWNER_TEST_DIR") {
        let registry = JournalRegistry::default();
        registry
            .configure_directory(std::path::Path::new(&dir))
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(
            &std::fs::read(std::path::Path::new(&dir).join("page.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            registry
                .legacy_recording_retire_exact("recording", &page)
                .unwrap()["retired"],
            true
        );
        return;
    }
    let (dir, registry, page) = exact_fixture("migrated-process-owner");
    registry
        .legacy_recording_acknowledge_exact("recording", &page)
        .unwrap();
    registry
        .legacy_recording_retire_exact("recording", &page)
        .unwrap();
    std::fs::write(dir.join("page.json"), serde_json::to_vec(&page).unwrap()).unwrap();
    let owner = registry.get("recording").unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "retirement_replays_while_another_process_owns_the_migrated_journal",
        ])
        .env("UBM_EXACT_MIGRATED_OWNER_TEST_DIR", &dir)
        .status()
        .unwrap();
    assert!(status.success());
    drop(owner);
}

#[test]
fn missing_receipt_table_or_unmarked_empty_table_cannot_authorize_migration() {
    for (name, sql) in [
        ("table-dropped", "DROP TABLE legacy_exact_receipt"),
        (
            "unmarked-empty",
            "DELETE FROM legacy_exact_receipt; ALTER TABLE journal DROP COLUMN legacy_exact_state",
        ),
    ] {
        let (dir, registry, page) = exact_fixture(name);
        registry
            .legacy_recording_acknowledge_exact("recording", &page)
            .unwrap();
        let db = rusqlite::Connection::open(dir.join("recording.sqlite")).unwrap();
        db.execute_batch(sql).unwrap();
        drop(db);
        assert_eq!(
            registry.get("recording").err().unwrap().kind,
            "storage.corrupt"
        );
    }
}

#[test]
fn exact_ack_compares_float_lexemes_and_cursor_under_the_write_lock() {
    let (dir, registry, page) = exact_fixture("lexemes");
    let path = dir.join("recording.sqlite");
    let raw = String::from_utf8(
        page["rawRecords"][0]["body"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u8)
            .collect(),
    )
    .unwrap();
    assert!(raw.contains("1.000e+0"));
    let changed = raw.replace("1.000e+0", "1.000e-0");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE records SET body=?1", [&changed])
        .unwrap();
    drop(db);
    assert_eq!(
        registry
            .legacy_recording_acknowledge_exact("recording", &page)
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT body FROM records", [], |r| r.get::<_, String>(0))
            .unwrap(),
        changed
    );
    db.execute("UPDATE records SET body=?1", [&raw]).unwrap();
    db.execute("UPDATE journal SET pending_more=1", []).unwrap();
    drop(db);
    assert!(
        registry
            .legacy_recording_acknowledge_exact("recording", &page)
            .is_err()
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE journal SET pending_more=0", []).unwrap();
    drop(db);
    assert_eq!(
        registry
            .legacy_recording_acknowledge_exact("recording", &page)
            .unwrap()["records"],
        1
    );
}

#[test]
fn exact_receipt_fences_subsequent_ack_and_corruption_fails_closed() {
    let (dir, registry, first) = exact_fixture("subsequent");
    let receipt = registry
        .legacy_recording_acknowledge_exact("recording", &first)
        .unwrap();
    let path = dir.join("recording.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    let body = r#"{"ordinal":2,"metadata":{},"record":{"n":2}}"#;
    db.execute(
        "INSERT INTO records(ordinal,body,bytes) VALUES(2,?1,?2)",
        rusqlite::params![body, body.len() as i64],
    )
    .unwrap();
    db.execute("UPDATE journal SET retained_records=1,retained_bytes=?1,pending_token='next',pending_last=2,pending_more=0", [body.len() as i64]).unwrap();
    drop(db);
    assert!(
        registry
            .legacy_recording_prepare_exact("recording", 1, 4096)
            .is_err()
    );
    let mut next = first.clone();
    next["token"] = json!("next");
    next["cursorLast"] = json!("2");
    next["rawRecords"] = json!([{"ordinal":"2", "body":body.as_bytes()}]);
    next["bytes"] = json!(body.len());
    assert!(
        registry
            .legacy_recording_acknowledge_exact("recording", &next)
            .is_err()
    );
    assert!(
        registry
            .legacy_recording_acknowledge("recording", "next")
            .is_err()
    );
    assert_eq!(
        registry
            .legacy_recording_acknowledge_exact("recording", &first)
            .unwrap(),
        receipt
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE legacy_exact_receipt SET receipt='{\"version\":1,\"token\":\"bad\",\"acknowledged\":true,\"records\":1}'", []).unwrap();
    drop(db);
    assert_eq!(
        registry
            .legacy_recording_acknowledge_exact("recording", &first)
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    assert!(
        registry
            .legacy_recording_retire_exact("recording", &first)
            .is_err()
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    db.execute(
        "UPDATE legacy_exact_receipt SET receipt=?1",
        [receipt.to_string()],
    )
    .unwrap();
    drop(db);
    registry
        .legacy_recording_retire_exact("recording", &first)
        .unwrap();
    assert!(
        registry
            .legacy_recording_prepare_exact("recording", 1, 4096)
            .is_err()
    );
    registry
        .legacy_recording_confirm_exact_retirement("recording", &first)
        .unwrap();
    assert_eq!(
        registry
            .legacy_recording_prepare_exact("recording", 1, 4096)
            .unwrap(),
        next
    );
    assert_eq!(
        registry
            .legacy_recording_acknowledge_exact("recording", &next)
            .unwrap()["records"],
        1
    );
}

#[test]
fn exact_receipt_survives_committed_child_exit_and_another_client() {
    if let Ok(dir) = std::env::var("UBM_EXACT_CRASH_TEST_DIR") {
        let registry = JournalRegistry::default();
        registry
            .configure_directory(std::path::Path::new(&dir))
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(
            &std::fs::read(std::path::Path::new(&dir).join("page.json")).unwrap(),
        )
        .unwrap();
        registry
            .legacy_recording_acknowledge_exact("recording", &page)
            .unwrap();
        std::process::exit(0);
    }
    let (dir, registry, page) = exact_fixture("exit");
    std::fs::write(dir.join("page.json"), serde_json::to_vec(&page).unwrap()).unwrap();
    drop(registry);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "exact_receipt_survives_committed_child_exit_and_another_client",
        ])
        .env("UBM_EXACT_CRASH_TEST_DIR", &dir)
        .status()
        .unwrap();
    assert!(status.success());
    let another = JournalRegistry::default();
    another.configure_directory(&dir).unwrap();
    assert_eq!(
        another
            .legacy_recording_acknowledge_exact("recording", &page)
            .unwrap()["records"],
        1
    );
    assert!(another.get("recording").is_err());
}

#[test]
fn exact_ack_from_competing_clients_is_idempotent() {
    let (dir, registry, page) = exact_fixture("concurrent");
    drop(registry);
    let gate = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let dir = dir.clone();
        let page = page.clone();
        let gate = gate.clone();
        threads.push(std::thread::spawn(move || {
            let registry = JournalRegistry::default();
            registry.configure_directory(&dir).unwrap();
            gate.wait();
            for _ in 0..100 {
                match registry.legacy_recording_acknowledge_exact("recording", &page) {
                    Ok(receipt) => return receipt,
                    Err(e) if e.kind == "storage.busy" || e.kind == "storage.owner-conflict" => {
                        std::thread::yield_now()
                    }
                    Err(e) => panic!("unexpected error: {e:?}"),
                }
            }
            panic!("clients did not settle")
        }));
    }
    gate.wait();
    let receipts: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(receipts[0], receipts[1]);
    assert_eq!(receipts[0]["records"], 1);
}

#[test]
fn exact_ack_reopens_after_process_dies_with_uncommitted_delete() {
    if let Ok(dir) = std::env::var("UBM_EXACT_ROLLBACK_TEST_DIR") {
        let db = rusqlite::Connection::open(std::path::Path::new(&dir).join("recording.sqlite"))
            .unwrap();
        db.execute_batch("PRAGMA synchronous=EXTRA; BEGIN IMMEDIATE; DELETE FROM records; UPDATE journal SET pending_token=NULL,pending_last=NULL,pending_more=NULL,retained_records=0,retained_bytes=0;").unwrap();
        std::process::exit(0);
    }
    let (dir, registry, page) = exact_fixture("rollback");
    drop(registry);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "exact_ack_reopens_after_process_dies_with_uncommitted_delete",
        ])
        .env("UBM_EXACT_ROLLBACK_TEST_DIR", &dir)
        .status()
        .unwrap();
    assert!(status.success());
    let reopened = JournalRegistry::default();
    reopened.configure_directory(&dir).unwrap();
    assert_eq!(
        reopened
            .legacy_recording_prepare_exact("recording", 1, 4096)
            .unwrap(),
        page
    );
    assert_eq!(
        reopened
            .legacy_recording_acknowledge_exact("recording", &page)
            .unwrap()["records"],
        1
    );
}

#[test]
fn exact_prepare_fences_token_only_before_any_ack() {
    let (dir, registry, page) = exact_fixture("prepare-fence");
    assert!(
        registry
            .legacy_recording_acknowledge("recording", page["token"].as_str().unwrap())
            .is_err()
    );
    let second = JournalRegistry::default();
    second.configure_directory(&dir).unwrap();
    assert_eq!(
        second
            .legacy_recording_prepare_exact("recording", 1, 4096)
            .unwrap(),
        page
    );
    assert_eq!(
        second
            .legacy_recording_acknowledge_exact("recording", &page)
            .unwrap()["records"],
        1
    );
}

#[test]
fn failed_receipt_write_rolls_back_deletion_and_cursor() {
    let (dir, registry, page) = exact_fixture("receipt-abort");
    let db = rusqlite::Connection::open(dir.join("recording.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_exact_receipt BEFORE UPDATE ON legacy_exact_receipt BEGIN SELECT RAISE(ABORT,'injected receipt write failure'); END;").unwrap();
    drop(db);
    assert!(
        registry
            .legacy_recording_acknowledge_exact("recording", &page)
            .is_err()
    );
    let db = rusqlite::Connection::open(dir.join("recording.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT pending_token FROM journal", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        page["token"].as_str().unwrap()
    );
    db.execute_batch("DROP TRIGGER fail_exact_receipt;")
        .unwrap();
    drop(db);
    assert_eq!(
        registry
            .legacy_recording_acknowledge_exact("recording", &page)
            .unwrap()["records"],
        1
    );
}
