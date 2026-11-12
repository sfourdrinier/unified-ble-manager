use serde_json::json;
use std::sync::{Arc, Barrier};
use ubm_desktop::continuation_journal::{ContinuationJournal, JournalQuota};

fn fixture(max_records: u64) -> (std::path::PathBuf, JournalQuota) {
    let dir = std::env::temp_dir().join(format!(
        "ubm-evidence-cursor-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    (
        dir.join("journal.sqlite"),
        JournalQuota {
            max_bytes: 1 << 20,
            max_records,
        },
    )
}
fn open(path: &std::path::Path, quota: JournalQuota) -> ContinuationJournal {
    ContinuationJournal::open(path, "recording", &json!({}), quota).unwrap()
}
fn token(v: &serde_json::Value) -> &str {
    v["token"].as_str().unwrap()
}

#[test]
fn evidence_snapshot_survives_recording_ack_rollback_and_reopen() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    for n in 1..=3 {
        journal
            .append(&json!({"source":"a"}), &json!({"n":n}))
            .unwrap();
    }
    let recording = journal.prepare(2, 4096).unwrap();
    let start = journal.begin_evidence_transfer("transfer-a").unwrap();
    assert_eq!(start["durable"], true);
    let page = journal
        .prepare_evidence_transfer("transfer-a", 1, 4096)
        .unwrap();
    assert_eq!(page["records"][0]["record"]["n"], 1);
    assert_eq!(journal.prepare(2, 4096).unwrap(), recording);
    assert!(
        journal
            .acknowledge_evidence_transfer("other", token(&page))
            .is_err()
    );
    journal.acknowledge(token(&recording)).unwrap();
    assert_eq!(
        journal.prepare(2, 4096).unwrap()["records"][0]["record"]["n"],
        3
    );
    journal
        .rollback_evidence_transfer("transfer-a", token(&page))
        .unwrap();
    assert!(
        journal
            .acknowledge_evidence_transfer("transfer-a", token(&page))
            .is_err()
    );
    drop(journal);
    let journal = open(&path, quota);
    let retry = journal
        .prepare_evidence_transfer("transfer-a", 2, 4096)
        .unwrap();
    assert_eq!(retry["records"].as_array().unwrap().len(), 2);
    assert_eq!(retry["records"][0]["record"]["n"], 1);
    assert_ne!(token(&retry), token(&page));
    let ack = journal
        .acknowledge_evidence_transfer("transfer-a", token(&retry))
        .unwrap();
    assert_eq!(ack["durable"], true);
    drop(journal);
    let journal = open(&path, quota);
    assert_eq!(
        journal
            .acknowledge_evidence_transfer("transfer-a", token(&retry))
            .unwrap(),
        ack
    );
    let tail = journal
        .prepare_evidence_transfer("transfer-a", 2, 4096)
        .unwrap();
    assert_eq!(tail["records"][0]["record"]["n"], 3);
    journal
        .acknowledge_evidence_transfer("transfer-a", token(&tail))
        .unwrap();
    assert_eq!(journal.status().unwrap()["records"], 1);
    assert_eq!(
        journal
            .prepare_evidence_transfer("transfer-a", 1, 4096)
            .unwrap()["records"],
        json!([])
    );
}

#[test]
fn concurrent_recording_and_evidence_claims_serialize_without_loss() {
    let (path, quota) = fixture(100);
    let journal = Arc::new(open(&path, quota));
    for n in 0..30 {
        journal.append(&json!({}), &json!({"n":n})).unwrap();
    }
    journal.begin_evidence_transfer("concurrent").unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let other = Arc::clone(&journal);
    let b = Arc::clone(&barrier);
    let thread = std::thread::spawn(move || {
        b.wait();
        loop {
            let page = other.prepare(3, 4096).unwrap();
            if page["token"].is_null() {
                break;
            }
            other.acknowledge(token(&page)).unwrap();
        }
    });
    barrier.wait();
    let mut found = Vec::new();
    loop {
        let page = journal
            .prepare_evidence_transfer("concurrent", 2, 4096)
            .unwrap();
        if page["token"].is_null() {
            break;
        }
        for row in page["records"].as_array().unwrap() {
            found.push(row["record"]["n"].as_u64().unwrap());
        }
        journal
            .acknowledge_evidence_transfer("concurrent", token(&page))
            .unwrap();
    }
    thread.join().unwrap();
    assert_eq!(found, (0..30).collect::<Vec<_>>());
    assert_eq!(journal.status().unwrap()["records"], 0);
}

#[test]
fn pinned_records_count_against_quota_and_bounds_fail_without_advancing() {
    let (path, quota) = fixture(1);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("quota").unwrap();
    assert!(journal.begin_evidence_transfer("different").is_err());
    assert!(journal.prepare_evidence_transfer("quota", 1, 1).is_err());
    let recording = journal.prepare(1, 4096).unwrap();
    journal.acknowledge(token(&recording)).unwrap();
    assert_eq!(journal.status().unwrap()["records"], 1);
    assert!(journal.append(&json!({}), &json!({"n":2})).is_err());
    let page = journal.prepare_evidence_transfer("quota", 1, 4096).unwrap();
    assert_eq!(page["records"][0]["record"]["n"], 1);
    journal
        .acknowledge_evidence_transfer("quota", token(&page))
        .unwrap();
    assert_eq!(journal.status().unwrap()["records"], 0);
}

#[test]
fn version_two_pending_page_refuses_migration_without_inventing_identity() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":7})).unwrap();
    let recording = journal.prepare(1, 4096).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("BEGIN; DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; DROP TABLE evidence_cursor; DROP TABLE evidence_claim_records; DROP TABLE evidence_claim_header; DROP TABLE evidence_members; DROP TABLE evidence_members_header; DROP TABLE recording_claim_records; DROP TABLE recording_claim_header; ALTER TABLE records RENAME TO records_v3; CREATE TABLE records(ordinal INTEGER PRIMARY KEY, body TEXT NOT NULL, bytes INTEGER NOT NULL); INSERT INTO records SELECT ordinal,body,bytes FROM records_v3; DROP TABLE records_v3; PRAGMA user_version=2; COMMIT;").unwrap();
    drop(db);
    assert_eq!(
        ContinuationJournal::open(&path, "recording", &json!({}), quota)
            .err()
            .unwrap()
            .kind,
        "storage.schema"
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row("SELECT pending_token FROM journal", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        token(&recording)
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn evidence_ack_failure_rolls_back_deletion_and_never_claims_success() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":7})).unwrap();
    journal.begin_evidence_transfer("failure").unwrap();
    let page = journal
        .prepare_evidence_transfer("failure", 1, 4096)
        .unwrap();
    let recording = journal.prepare(1, 4096).unwrap();
    journal.acknowledge(token(&recording)).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_evidence_ack BEFORE UPDATE ON evidence_cursor WHEN NEW.last_ack_token IS NOT NULL BEGIN SELECT RAISE(ABORT,'forced failure'); END;").unwrap();
    assert!(
        journal
            .acknowledge_evidence_transfer("failure", token(&page))
            .is_err()
    );
    assert_eq!(journal.status().unwrap()["records"], 1);
    assert_eq!(
        journal
            .prepare_evidence_transfer("failure", 1, 4096)
            .unwrap(),
        page
    );
    db.execute_batch("DROP TRIGGER fail_evidence_ack;").unwrap();
    drop(db);
    journal
        .acknowledge_evidence_transfer("failure", token(&page))
        .unwrap();
    assert_eq!(journal.status().unwrap()["records"], 0);
}

#[cfg(unix)]
#[test]
fn abrupt_process_death_retains_claim_and_pinned_record() {
    const CHILD: &str = "UBM_EVIDENCE_CRASH_CHILD";
    if let Some(path) = std::env::var_os(CHILD) {
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let journal = open(std::path::Path::new(&path), quota);
        journal.append(&json!({}), &json!({"n":17})).unwrap();
        journal.begin_evidence_transfer("crash").unwrap();
        let evidence = journal.prepare_evidence_transfer("crash", 1, 4096).unwrap();
        let recording = journal.prepare(1, 4096).unwrap();
        journal.acknowledge(token(&recording)).unwrap();
        std::fs::write(
            std::path::Path::new(&path).with_extension("token"),
            token(&evidence),
        )
        .unwrap();
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
        unreachable!();
    }
    let (path, quota) = fixture(10);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "abrupt_process_death_retains_claim_and_pinned_record",
        ])
        .env(CHILD, &path)
        .status()
        .unwrap();
    assert!(!status.success());
    let expected = std::fs::read_to_string(path.with_extension("token")).unwrap();
    let journal = open(&path, quota);
    let page = journal.prepare_evidence_transfer("crash", 1, 4096).unwrap();
    assert_eq!(token(&page), expected);
    assert_eq!(page["records"][0]["record"]["n"], 17);
    journal
        .acknowledge_evidence_transfer("crash", token(&page))
        .unwrap();
    assert_eq!(journal.status().unwrap()["records"], 0);
}

#[test]
fn snapshot_excludes_later_appends_and_clear_cannot_discard_pins() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("first").unwrap();
    journal.append(&json!({}), &json!({"n":2})).unwrap();
    let page = journal.prepare_evidence_transfer("first", 2, 4096).unwrap();
    assert_eq!(page["records"].as_array().unwrap().len(), 1);
    assert_eq!(page["records"][0]["record"]["n"], 1);
    assert!(journal.prepare_evidence_transfer("first", 1, 1).is_err());
    assert!(journal.acknowledge(token(&page)).is_err());
    journal.stop().unwrap();
    assert!(journal.clear().is_err());
    journal
        .acknowledge_evidence_transfer("first", token(&page))
        .unwrap();
    assert_eq!(
        journal.prepare_evidence_transfer("first", 2, 4096).unwrap()["records"],
        json!([])
    );
    journal.begin_evidence_transfer("second").unwrap();
    assert!(
        journal
            .acknowledge_evidence_transfer("first", token(&page))
            .is_err()
    );
    let second = journal
        .prepare_evidence_transfer("second", 2, 4096)
        .unwrap();
    assert_eq!(second["records"].as_array().unwrap().len(), 2);
}

#[test]
fn recovery_discovers_transfer_and_pending_identity_after_reopen() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    assert!(journal.evidence_transfer_status().unwrap()["transferId"].is_null());
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("discover").unwrap();
    let page = journal
        .prepare_evidence_transfer("discover", 1, 4096)
        .unwrap();
    drop(journal);
    let journal = open(&path, quota);
    let status = journal.evidence_transfer_status().unwrap();
    assert_eq!(status["transferId"], "discover");
    assert_eq!(status["pendingToken"], page["token"]);
    assert_eq!(status["complete"], false);
    assert_eq!(status["durable"], true);
    journal
        .acknowledge_evidence_transfer("discover", token(&page))
        .unwrap();
    assert_eq!(
        journal.evidence_transfer_status().unwrap()["complete"],
        true
    );
}

#[test]
fn evidence_identity_keeps_ordinals_exact_above_javascript_integer_range() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE journal SET next_ordinal=9007199254740993 WHERE id=1",
        [],
    )
    .unwrap();
    drop(db);
    journal.append(&json!({}), &json!({"n": 1})).unwrap();
    let start = journal.begin_evidence_transfer("large-ordinal").unwrap();
    assert_eq!(start["cutoff"], "9007199254740993");
    assert_eq!(
        journal.evidence_transfer_status().unwrap()["cutoff"],
        "9007199254740993"
    );
    let page = journal
        .prepare_evidence_transfer("large-ordinal", 1, 4096)
        .unwrap();
    assert_eq!(page["ordinals"], json!(["9007199254740993"]));
    assert!(
        page["recordBodies"][0]
            .as_str()
            .unwrap()
            .contains("9007199254740993")
    );
}

#[test]
fn changed_prepared_page_cannot_be_retried_or_acknowledged_after_reopen() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    for n in 1..=2 {
        journal.append(&json!({}), &json!({"n":n})).unwrap();
    }
    journal.begin_evidence_transfer("changed").unwrap();
    let page = journal
        .prepare_evidence_transfer("changed", 2, 4096)
        .unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DELETE FROM records WHERE ordinal=2; UPDATE journal SET retained_records=(SELECT count(*) FROM records),retained_bytes=(SELECT coalesce(sum(bytes),0) FROM records);").unwrap();
    drop(db);
    let journal = open(&path, quota);
    assert!(
        journal
            .prepare_evidence_transfer("changed", 2, 4096)
            .is_err()
    );
    assert!(
        journal
            .acknowledge_evidence_transfer("changed", token(&page))
            .is_err()
    );
    assert_eq!(
        journal.evidence_transfer_status().unwrap()["complete"],
        false
    );
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[test]
fn malformed_body_is_recoverable_in_bounded_chunks_without_advancing_after_reopen() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.append(&json!({}), &json!({"n":2})).unwrap();
    journal.begin_evidence_transfer("malformed").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE records SET body=substr(body,1,bytes-1)||'!' WHERE ordinal=2",
        [],
    )
    .unwrap();
    let original: Vec<u8> = db
        .query_row(
            "SELECT CAST(body AS BLOB) FROM records WHERE ordinal=2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(db);
    assert_eq!(
        journal
            .prepare_evidence_transfer("malformed", 2, 4096)
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    drop(journal);
    let journal = open(&path, quota);
    let first = journal
        .inspect_evidence_record("malformed", None, 0, 8)
        .unwrap();
    assert_eq!(first["ordinal"], "1");
    assert_eq!(first["nextOrdinal"], "2");
    let diagnosis = journal
        .inspect_evidence_record("malformed", Some("2"), 0, 4096)
        .unwrap();
    assert_eq!(diagnosis["fault"]["kind"], "storage.corrupt");
    let mut got = Vec::new();
    let mut offset = 0;
    loop {
        let part = journal
            .inspect_evidence_record("malformed", Some("2"), offset, 8)
            .unwrap();
        got.extend(
            part["bytes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b.as_u64().unwrap() as u8),
        );
        offset += part["bytes"].as_array().unwrap().len() as u64;
        if part["complete"] == true {
            break;
        }
    }
    assert_eq!(got, original);
    assert_eq!(
        journal.evidence_transfer_status().unwrap()["complete"],
        false
    );
    assert_eq!(journal.status().unwrap()["records"], 2);
}

#[test]
fn oversized_corrupt_body_is_read_only_in_bounded_chunks() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("oversized").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE records SET body=replace(hex(zeroblob(100000)), '0', 'x') WHERE ordinal=1",
        [],
    )
    .unwrap();
    drop(db);
    assert_eq!(
        journal
            .prepare_evidence_transfer("oversized", 1, 100)
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    assert_eq!(journal.prepare(1, 100).unwrap_err().kind, "storage.corrupt");
    drop(journal);
    let journal = open(&path, quota);
    let part = journal
        .inspect_evidence_record("oversized", None, 199_990, 16)
        .unwrap();
    assert_eq!(part["length"], "200000");
    assert_eq!(
        part["bytes"],
        json!([120, 120, 120, 120, 120, 120, 120, 120, 120, 120])
    );
    assert_eq!(part["complete"], true);
    assert_eq!(part["fault"]["kind"], "storage.corrupt");
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[test]
fn changed_equal_length_body_cannot_be_acknowledged_even_without_retry() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("changed-body").unwrap();
    let page = journal
        .prepare_evidence_transfer("changed-body", 1, 4096)
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE records SET body=replace(body, '\"n\":1', '\"n\":2') WHERE ordinal=1",
        [],
    )
    .unwrap();
    drop(db);
    assert_eq!(
        journal
            .acknowledge_evidence_transfer("changed-body", token(&page))
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[test]
fn v3_pending_claim_refuses_migration_and_remains_unchanged() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    let recording = journal.prepare(1, 4096).unwrap();
    journal.begin_evidence_transfer("v3").unwrap();
    let page = journal.prepare_evidence_transfer("v3", 1, 4096).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; DROP TABLE evidence_claim_records; DROP TABLE evidence_claim_header; DROP TABLE evidence_members; DROP TABLE evidence_members_header; DROP TABLE recording_claim_records; DROP TABLE recording_claim_header; ALTER TABLE evidence_cursor DROP COLUMN pending_count; PRAGMA user_version=3;").unwrap();
    drop(db);
    assert_eq!(
        ContinuationJournal::open(&path, "recording", &json!({}), quota)
            .err()
            .unwrap()
            .kind,
        "storage.schema"
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        db.query_row("SELECT pending_token FROM journal", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        token(&recording)
    );
    assert_eq!(
        db.query_row("SELECT pending_token FROM evidence_cursor", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        token(&page)
    );
}

#[test]
fn changed_more_flag_cannot_skip_unclaimed_rows() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    for n in 1..=2 {
        journal.append(&json!({}), &json!({"n":n})).unwrap();
    }
    journal.begin_evidence_transfer("changed-more").unwrap();
    let page = journal
        .prepare_evidence_transfer("changed-more", 1, 4096)
        .unwrap();
    assert_eq!(page["more"], true);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE evidence_cursor SET pending_more=0 WHERE id=1", [])
        .unwrap();
    drop(db);
    assert_eq!(
        journal
            .acknowledge_evidence_transfer("changed-more", token(&page))
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    assert_eq!(
        journal.evidence_transfer_status().unwrap()["complete"],
        false
    );
}

#[test]
fn invalid_utf8_bytes_are_preserved_for_inspection() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("invalid-utf8").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    let len: u32 = db
        .query_row("SELECT bytes FROM records WHERE ordinal=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let raw = vec![0xff; len as usize];
    db.execute("UPDATE records SET body=?1 WHERE ordinal=1", [&raw])
        .unwrap();
    drop(db);
    assert_eq!(
        journal
            .prepare_evidence_transfer("invalid-utf8", 1, 4096)
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    assert_eq!(
        journal.prepare(1, 4096).unwrap_err().kind,
        "storage.corrupt"
    );
    let data = journal
        .inspect_evidence_record("invalid-utf8", None, 0, 4096)
        .unwrap();
    assert_eq!(data["fault"]["kind"], "storage.corrupt");
    assert_eq!(data["bytes"], json!(raw));
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[test]
fn oversized_change_to_prepared_record_is_detected_before_retry_and_ack() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("changed-large").unwrap();
    let page = journal
        .prepare_evidence_transfer("changed-large", 1, 4096)
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE records SET body=replace(hex(zeroblob(100000)), '0', 'x') WHERE ordinal=1",
        [],
    )
    .unwrap();
    drop(db);
    assert_eq!(
        journal
            .prepare_evidence_transfer("changed-large", 1, 4096)
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    assert_eq!(
        journal
            .acknowledge_evidence_transfer("changed-large", token(&page))
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    let part = journal
        .inspect_evidence_record("changed-large", None, 199_998, 8)
        .unwrap();
    assert_eq!(part["bytes"], json!([120, 120]));
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[test]
fn recording_ack_does_not_delete_a_record_corrupted_after_prepare() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    let page = journal.prepare(1, 4096).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE records SET body=substr(body,1,bytes-1)||'!' WHERE ordinal=1",
        [],
    )
    .unwrap();
    drop(db);
    assert_eq!(
        journal.acknowledge(token(&page)).unwrap_err().kind,
        "storage.corrupt"
    );
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[cfg(unix)]
#[test]
fn corrupted_prepared_page_cannot_be_acknowledged_after_process_death() {
    const CHILD: &str = "UBM_EVIDENCE_CORRUPT_CRASH_CHILD";
    if let Some(path) = std::env::var_os(CHILD) {
        let quota = JournalQuota {
            max_bytes: 1 << 20,
            max_records: 10,
        };
        let path = std::path::Path::new(&path);
        let journal = open(path, quota);
        for n in 1..=2 {
            journal.append(&json!({}), &json!({"n":n})).unwrap();
        }
        journal.begin_evidence_transfer("corrupt-crash").unwrap();
        let page = journal
            .prepare_evidence_transfer("corrupt-crash", 2, 4096)
            .unwrap();
        std::fs::write(path.with_extension("token"), token(&page)).unwrap();
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch("DELETE FROM records WHERE ordinal=2; UPDATE journal SET retained_records=(SELECT count(*) FROM records),retained_bytes=(SELECT coalesce(sum(bytes),0) FROM records);").unwrap();
        drop(db);
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
        unreachable!();
    }
    let (path, quota) = fixture(10);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "corrupted_prepared_page_cannot_be_acknowledged_after_process_death",
        ])
        .env(CHILD, &path)
        .status()
        .unwrap();
    assert!(!status.success());
    let expected = std::fs::read_to_string(path.with_extension("token")).unwrap();
    let journal = open(&path, quota);
    assert_eq!(
        journal
            .acknowledge_evidence_transfer("corrupt-crash", &expected)
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    assert_eq!(
        journal
            .rollback_evidence_transfer("corrupt-crash", &expected)
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    let saved = ContinuationJournal::inspect_raw_record(&path, "evidenceClaim", Some("2"), 0, 4096)
        .unwrap();
    let bytes: Vec<u8> = saved["bytes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_u64().unwrap() as u8)
        .collect();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["record"]["n"],
        2
    );
    assert_eq!(
        journal.evidence_transfer_status().unwrap()["complete"],
        false
    );
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[test]
fn recording_ack_rejects_changed_valid_body_and_missing_membership_after_reopen() {
    for missing in [false, true] {
        let (path, quota) = fixture(10);
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        let journal = open(&path, quota);
        for n in 1..=2 {
            journal.append(&json!({}), &json!({"n":n})).unwrap();
        }
        let page = journal.prepare(2, 4096).unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        if missing {
            db.execute("DELETE FROM records WHERE ordinal=1", [])
                .unwrap();
            db.execute("UPDATE journal SET retained_records=retained_records-1,retained_bytes=(SELECT sum(bytes) FROM records)", []).unwrap();
        } else {
            db.execute(
                "UPDATE records SET body=replace(body, '\"n\":1', '\"n\":9') WHERE ordinal=1",
                [],
            )
            .unwrap();
        }
        drop(db);
        drop(journal);
        let journal = open(&path, quota);
        assert_eq!(
            journal.acknowledge(token(&page)).unwrap_err().kind,
            "storage.corrupt"
        );
        drop(journal);
    }
}

#[test]
fn evidence_rejects_missing_unprepared_snapshot_member() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    for n in 1..=2 {
        journal.append(&json!({}), &json!({"n":n})).unwrap();
    }
    journal
        .begin_evidence_transfer("missing-before-page")
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("DELETE FROM records WHERE ordinal=1", [])
        .unwrap();
    drop(db);
    assert_eq!(
        journal
            .prepare_evidence_transfer("missing-before-page", 2, 4096)
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    assert_eq!(
        journal.evidence_transfer_status().unwrap()["complete"],
        false
    );
}

#[test]
fn rollback_refuses_to_destroy_changed_claim_copy() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("original").unwrap();
    let page = journal
        .prepare_evidence_transfer("original", 1, 4096)
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE records SET body=replace(body, '\"n\":1', '\"n\":9')",
        [],
    )
    .unwrap();
    drop(db);
    assert_eq!(
        journal
            .rollback_evidence_transfer("original", token(&page))
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    let saved: String = db
        .query_row(
            "SELECT CAST(body AS TEXT) FROM evidence_claim_records WHERE ordinal=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(saved, page["recordBodies"][0].as_str().unwrap());
}

#[test]
fn inspection_reports_independent_length_and_json_faults() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("two-faults").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE records SET body='!' WHERE ordinal=1", [])
        .unwrap();
    drop(db);
    let inspected = journal
        .inspect_evidence_record("two-faults", None, 0, 100)
        .unwrap();
    assert_eq!(inspected["lengthValid"], false);
    assert_eq!(inspected["jsonValidated"], true);
    assert_eq!(inspected["jsonValid"], false);
}

#[test]
fn bounded_read_only_recovery_survives_bad_counters_and_exposes_saved_claim() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("raw").unwrap();
    let page = journal.prepare_evidence_transfer("raw", 1, 4096).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE records SET body='!'", []).unwrap();
    db.execute("UPDATE journal SET retained_records=99", [])
        .unwrap();
    drop(db);
    assert_eq!(
        ContinuationJournal::open(&path, "recording", &json!({}), quota)
            .err()
            .unwrap()
            .kind,
        "storage.corrupt"
    );
    let live = ContinuationJournal::inspect_raw_record(&path, "records", Some("1"), 0, 10).unwrap();
    assert!(live["countersValid"].is_null());
    assert!(live["counterFault"].is_null());
    assert_eq!(live["lengthValid"], false);
    assert_eq!(live["jsonValid"], false);
    let mut recovered = Vec::new();
    let mut offset = 0;
    loop {
        let part =
            ContinuationJournal::inspect_raw_record(&path, "evidenceClaim", Some("1"), offset, 7)
                .unwrap();
        let bytes = part["bytes"].as_array().unwrap();
        recovered.extend(bytes.iter().map(|v| v.as_u64().unwrap() as u8));
        offset += bytes.len() as u64;
        if part["complete"] == true {
            break;
        }
    }
    assert_eq!(
        recovered,
        page["recordBodies"][0].as_str().unwrap().as_bytes()
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT retained_records FROM journal", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        99
    );
}

#[test]
fn recording_ack_fails_closed_when_active_snapshot_membership_is_damaged() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    for n in 1..=2 {
        journal.append(&json!({}), &json!({"n":n})).unwrap();
    }
    journal.begin_evidence_transfer("damaged-snapshot").unwrap();
    let recording = journal.prepare(1, 4096).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("DELETE FROM records WHERE ordinal=2", [])
        .unwrap();
    drop(db);
    assert_eq!(
        journal.acknowledge(token(&recording)).unwrap_err().kind,
        "storage.corrupt"
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT recording_acked FROM records WHERE ordinal=1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn v4_migration_refusal_preserves_claim_and_original_copy() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("v4").unwrap();
    let page = journal.prepare_evidence_transfer("v4", 1, 4096).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; DROP TABLE evidence_members; DROP TABLE evidence_members_header; DROP TABLE recording_claim_records; DROP TABLE recording_claim_header; PRAGMA user_version=4;").unwrap();
    db.execute(
        "UPDATE records SET body=replace(body, '\"n\":1', '\"n\":9')",
        [],
    )
    .unwrap();
    drop(db);
    assert_eq!(
        ContinuationJournal::open(&path, "recording", &json!({}), quota)
            .err()
            .unwrap()
            .kind,
        "storage.schema"
    );
    let saved = ContinuationJournal::inspect_raw_record(&path, "evidenceClaim", Some("1"), 0, 4096)
        .unwrap();
    assert_eq!(
        saved["bytes"],
        json!(page["recordBodies"][0].as_str().unwrap().as_bytes())
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
}

#[test]
fn recording_identity_fits_under_quota_without_copying_all_bodies() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    for n in 0..4 {
        journal
            .append(&json!({}), &json!({"n":n,"payload":"x".repeat(60_000)}))
            .unwrap();
    }
    let before = std::fs::metadata(&path).unwrap().len();
    let page = journal.prepare(4, 400_000).unwrap();
    assert_eq!(page["records"].as_array().unwrap().len(), 4);
    assert!(std::fs::metadata(&path).unwrap().len() - before < 40_000);
    journal.acknowledge(token(&page)).unwrap();
}

#[test]
fn evidence_ack_detects_missing_unprepared_tail() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    for n in 1..=2 {
        journal.append(&json!({}), &json!({"n":n})).unwrap();
    }
    journal.begin_evidence_transfer("tail").unwrap();
    let page = journal.prepare_evidence_transfer("tail", 1, 4096).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("DELETE FROM records WHERE ordinal=2", [])
        .unwrap();
    drop(db);
    assert_eq!(
        journal
            .acknowledge_evidence_transfer("tail", token(&page))
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    let status = journal.evidence_transfer_status().unwrap();
    assert_eq!(status["membershipValid"], false);
    assert_eq!(status["complete"], false);
    assert_eq!(status["progressed"], "0");
}

#[test]
fn orphaned_evidence_claim_is_not_overwritten_when_cursor_loses_token() {
    let (path, quota) = fixture(10);
    let journal = open(&path, quota);
    journal.append(&json!({}), &json!({"n":1})).unwrap();
    journal.begin_evidence_transfer("orphan").unwrap();
    let page = journal
        .prepare_evidence_transfer("orphan", 1, 4096)
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE evidence_cursor SET pending_token=NULL WHERE id=1",
        [],
    )
    .unwrap();
    drop(db);
    assert_eq!(
        journal
            .prepare_evidence_transfer("orphan", 1, 4096)
            .unwrap_err()
            .kind,
        "storage.corrupt"
    );
    let raw = ContinuationJournal::inspect_raw_record(&path, "evidenceClaim", Some("1"), 0, 4096)
        .unwrap();
    assert_eq!(
        raw["bytes"],
        json!(page["recordBodies"][0].as_str().unwrap().as_bytes())
    );
}
