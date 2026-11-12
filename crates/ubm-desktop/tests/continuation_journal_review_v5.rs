use serde_json::json;
use ubm_desktop::continuation_journal::{ContinuationJournal, JournalQuota};

fn fixture() -> (std::path::PathBuf, JournalQuota) {
    let dir = std::env::temp_dir().join(format!(
        "ubm-v5-review-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    (
        dir.join("journal.sqlite"),
        JournalQuota {
            max_bytes: 1 << 20,
            max_records: 100,
        },
    )
}
fn open(path: &std::path::Path, quota: JournalQuota) -> ContinuationJournal {
    ContinuationJournal::open(path, "recording", &json!({}), quota).unwrap()
}
#[test]
fn legacy_pending_recording_is_left_for_its_original_reader() {
    let (path, quota) = fixture();
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    let page = journal.prepare(1, 4096).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; DROP TABLE evidence_members; DROP TABLE evidence_members_header; DROP TABLE recording_claim_records; DROP TABLE recording_claim_header; PRAGMA user_version=4;").unwrap();
    drop(db);
    let failure = ContinuationJournal::open(&path, "recording", &json!({}), quota)
        .err()
        .expect("migration must refuse an unverifiable pending page");
    assert_eq!(failure.kind, "storage.schema");
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
        db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn lost_transfer_id_cannot_replace_membership_or_retire_recording() {
    let (path, quota) = fixture();
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("original").unwrap();
    let evidence = journal
        .prepare_evidence_transfer("original", 1, 4096)
        .unwrap();
    let recording = journal.prepare(1, 4096).unwrap();
    journal.append(&json!({}), &json!({"n":2})).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE evidence_cursor SET transfer_id=NULL", [])
        .unwrap();
    drop(db);
    assert_eq!(
        journal
            .begin_evidence_transfer("replacement")
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    assert_eq!(
        journal
            .acknowledge(recording["token"].as_str().unwrap())
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT transfer_id FROM evidence_members_header", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "original"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    let raw = ContinuationJournal::inspect_raw_record(&path, "evidenceClaim", Some("1"), 0, 4096)
        .unwrap();
    assert_eq!(
        raw["bytes"],
        json!(evidence["recordBodies"][0].as_str().unwrap().as_bytes())
    );
}
#[test]
fn raw_recovery_reads_bodies_with_non_integer_lengths() {
    let (path, quota) = fixture();
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("original").unwrap();
    let page = journal
        .prepare_evidence_transfer("original", 1, 4096)
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "UPDATE records SET bytes='bad'; UPDATE evidence_claim_records SET bytes=x'01';",
    )
    .unwrap();
    drop(db);
    for source in ["records", "evidenceClaim"] {
        let raw =
            ContinuationJournal::inspect_raw_record(&path, source, Some("1"), 0, 4096).unwrap();
        assert_eq!(
            raw["bytes"],
            json!(page["recordBodies"][0].as_str().unwrap().as_bytes())
        );
        assert_eq!(raw["storedLength"], serde_json::Value::Null);
        assert_eq!(raw["lengthValid"], false);
        assert_eq!(raw["lengthFault"]["kind"], "storage.corrupt");
        assert_eq!(raw["countersValid"], serde_json::Value::Null);
    }
    let raw = journal
        .inspect_evidence_record("original", Some("1"), 0, 4096)
        .unwrap();
    assert_eq!(
        raw["bytes"],
        json!(page["recordBodies"][0].as_str().unwrap().as_bytes())
    );
    assert_eq!(raw["storedLength"], serde_json::Value::Null);
    assert_eq!(raw["lengthValid"], false);
}

#[test]
fn quiescent_v2_rows_migrate_and_prepare() {
    let (path, quota) = fixture();
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":42})).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("BEGIN; DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; DROP TABLE evidence_cursor; DROP TABLE evidence_claim_records; DROP TABLE evidence_claim_header; DROP TABLE evidence_members; DROP TABLE evidence_members_header; DROP TABLE recording_claim_records; DROP TABLE recording_claim_header; ALTER TABLE records RENAME TO records_v3; CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL); INSERT INTO records SELECT ordinal,body,bytes FROM records_v3; DROP TABLE records_v3; PRAGMA user_version=2; COMMIT;").unwrap();
    drop(db);
    let journal = open(&path, quota);
    let page = journal.prepare(1, 4096).unwrap();
    assert_eq!(page["records"][0]["record"]["n"], 42);
    journal
        .acknowledge(page["token"].as_str().unwrap())
        .unwrap();
}

#[test]
fn chunk_reads_from_large_live_wal_with_corrupt_counters() {
    let (path, quota) = fixture();
    drop(open(&path, quota));
    let mut writer = rusqlite::Connection::open(&path).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    let body = format!("\"{}\"", "x".repeat(2048));
    let tx = writer.transaction().unwrap();
    {
        let mut stmt = tx
            .prepare("INSERT INTO records(ordinal,body,bytes,recording_acked) VALUES(?1,?2,?3,0)")
            .unwrap();
        for n in 1..=4096 {
            stmt.execute(rusqlite::params![n, body, 2050]).unwrap();
        }
    }
    tx.execute("UPDATE journal SET retained_records=999999", [])
        .unwrap();
    tx.commit().unwrap();
    let wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
    assert!(std::fs::metadata(&wal).unwrap().len() > 4 * 1024 * 1024);
    let mut recovered = Vec::new();
    let mut offset = 0;
    loop {
        let part =
            ContinuationJournal::inspect_raw_record(&path, "records", Some("4096"), offset, 17)
                .unwrap();
        assert!(part["countersValid"].is_null());
        let bytes = part["bytes"].as_array().unwrap();
        assert!(bytes.len() <= 17);
        recovered.extend(bytes.iter().map(|v| v.as_u64().unwrap() as u8));
        offset += bytes.len() as u64;
        if part["complete"] == true {
            break;
        }
    }
    assert_eq!(recovered, body.as_bytes());
    assert_eq!(
        writer
            .query_row("SELECT retained_records FROM journal", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        999999
    );
}

#[test]
fn clear_refuses_lost_identity_of_even_an_empty_snapshot() {
    let (path, quota) = fixture();
    let journal = open(&path, quota);
    journal.begin_evidence_transfer("empty").unwrap();
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.stop().unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE evidence_cursor SET transfer_id=NULL", [])
        .unwrap();
    drop(db);
    assert_eq!(journal.clear().unwrap_err().kind, "storage.corrupt");
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[test]
fn full_claim_copy_fails_atomically_and_retries_after_space_is_freed() {
    let (path, quota) = fixture();
    let journal = open(&path, quota);
    journal
        .append(&json!({}), &json!({"payload":"x".repeat(60_000)}))
        .unwrap();
    journal.begin_evidence_transfer("full").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    let page_limit = (quota.max_bytes - 65536) / (2 * 4096 + 8);
    db.pragma_update(None, "max_page_count", i64::try_from(page_limit).unwrap())
        .unwrap();
    db.execute("CREATE TABLE test_padding(body BLOB)", [])
        .unwrap();
    loop {
        match db.execute("INSERT INTO test_padding VALUES(zeroblob(4096))", []) {
            Ok(_) => (),
            Err(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::DiskFull =>
            {
                break;
            }
            Err(e) => panic!("unexpected padding failure: {e}"),
        }
    }
    assert_eq!(
        journal
            .prepare_evidence_transfer("full", 1, 100_000)
            .unwrap_err()
            .kind,
        "storage.full"
    );
    assert!(journal.evidence_transfer_status().unwrap()["pendingToken"].is_null());
    assert_eq!(journal.status().unwrap()["records"], 1);
    assert_eq!(
        db.query_row("SELECT count(*) FROM evidence_claim_records", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    db.execute("DROP TABLE test_padding", []).unwrap();
    drop(db);
    let page = journal
        .prepare_evidence_transfer("full", 1, 100_000)
        .unwrap();
    assert_eq!(
        page["records"][0]["record"]["payload"]
            .as_str()
            .unwrap()
            .len(),
        60_000
    );
}

#[test]
fn already_migrated_pending_recording_never_acquires_guessed_identity() {
    let (path, quota) = fixture();
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":7})).unwrap();
    let page = journal.prepare(1, 4096).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DELETE FROM recording_claim_header; DELETE FROM recording_claim_records;")
        .unwrap();
    drop(db);
    assert_eq!(
        journal.prepare(1, 4096).unwrap_err().kind,
        "storage.corrupt"
    );
    assert_eq!(
        journal
            .acknowledge(page["token"].as_str().unwrap())
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    let raw =
        ContinuationJournal::inspect_raw_record(&path, "records", Some("1"), 0, 4096).unwrap();
    assert!(raw["jsonValid"] == true);
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[test]
fn configured_host_can_inspect_raw_v5_record_when_open_fails() {
    use ubm_desktop::continuation_journal::JournalRegistry;
    let (path, quota) = fixture();
    let journal = ContinuationJournal::open(&path, "journal", &json!({}), quota).unwrap();
    journal.append(&json!({}), &json!({"n":42})).unwrap();
    let page = journal.prepare(1, 4096).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DELETE FROM recording_claim_records; DELETE FROM recording_claim_header;")
        .unwrap();
    drop(db);
    let reopened = ContinuationJournal::open(&path, "journal", &json!({}), quota).unwrap();
    assert_eq!(
        reopened.prepare(1, 4096).unwrap_err().kind,
        "storage.corrupt"
    );
    assert_eq!(
        reopened
            .acknowledge(page["token"].as_str().unwrap())
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    drop(reopened);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE journal SET retained_records=99", [])
        .unwrap();
    drop(db);
    let registry = JournalRegistry::default();
    registry
        .configure_directory(path.parent().unwrap())
        .unwrap();
    let mut bytes = Vec::new();
    let mut offset = 0;
    loop {
        let chunk = registry
            .inspect_raw_record("journal", "records", Some("1"), offset, 7)
            .unwrap();
        let part = chunk["bytes"].as_array().unwrap();
        bytes.extend(part.iter().map(|b| b.as_u64().unwrap() as u8));
        offset += part.len() as u64;
        if chunk["complete"] == true {
            break;
        }
    }
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        page["records"][0]
    );
    assert!(
        registry
            .inspect_raw_record("../journal", "records", None, 0, 7)
            .is_err()
    );
}
