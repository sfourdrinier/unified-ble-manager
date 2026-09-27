use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use ubm_desktop::continuation_journal::{ContinuationJournal, JournalQuota, JournalRegistry};
use ubm_desktop::continuation_outbox::{DATA_RECORD_CAP, Outbox, WakeSink};
struct NoWake;
impl WakeSink for NoWake {
    fn wake(&self, _: u64) {}
}

#[tokio::test(flavor = "current_thread")]
async fn recording_task_uses_blocking_pool_and_redacts_join_failure_payload() {
    let runtime_thread = std::thread::current().id();
    let answer = ubm_desktop::continuation_journal::run_blocking(move || {
        assert_ne!(std::thread::current().id(), runtime_thread);
        Ok(json!({"ran":true}))
    })
    .await
    .unwrap();
    assert_eq!(answer["ran"], true);
    let error =
        ubm_desktop::continuation_journal::run_blocking(|| panic!("private sensor payload"))
            .await
            .unwrap_err();
    assert_eq!(error["code"], "platform.failure");
    assert!(error["platform"]["message"].is_string());
    assert!(error["platform"]["metadata"].is_object());
    assert!(!error.to_string().contains("private sensor payload"));
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
fn path() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ubm-journal-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).unwrap();
    path.join("recording.sqlite")
}
fn quota() -> JournalQuota {
    JournalQuota {
        max_bytes: 1 << 20,
        max_records: 100,
    }
}
fn open(path: &std::path::Path) -> ContinuationJournal {
    ContinuationJournal::open(
        path,
        "recording-1",
        &json!({"onAppearance":"native"}),
        quota(),
    )
    .unwrap()
}

#[test]
fn registry_retains_owner_and_reopens_persisted_identity_without_path_input() {
    let path = path();
    let directory = path.parent().unwrap();
    let registry = JournalRegistry::default();
    assert!(registry.open("r", &json!({}), quota()).is_err());
    registry.configure_directory(directory).unwrap();
    registry.configure_directory(directory).unwrap();
    assert!(registry.configure_directory(&std::env::temp_dir()).is_err());
    let journal = registry.open("r", &json!({"recipe":1}), quota()).unwrap();
    journal
        .append(&json!({"epoch":"one"}), &json!({"value":1}))
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(
        &journal,
        &registry.get("r").unwrap()
    ));
    assert!(registry.open("r", &json!({"recipe":2}), quota()).is_err());
    assert!(registry.get("../r").is_err());
    drop(journal);
    drop(registry);
    let registry = JournalRegistry::default();
    registry.configure_directory(directory).unwrap();
    let journal = registry.get("r").unwrap();
    assert_eq!(
        journal.prepare(1, 4096).unwrap()["records"][0]["record"]["value"],
        1
    );
    assert!(registry.get("missing").is_err());
    assert!(!directory.join("missing.sqlite").exists());
}

#[test]
fn terminal_collection_failure_stops_admission_and_survives_restart() {
    let path = path();
    let journal = open(&path);
    let other = open(&path);
    journal.append(&json!({}), &json!({"value":1})).unwrap();
    let lock = rusqlite::Connection::open(&path).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let failure = journal.append(&json!({}), &json!({"value":2})).unwrap_err();
    lock.execute_batch("ROLLBACK").unwrap();
    // Ordinary operation failure remains retryable until the collection owner marks it terminal.
    journal.append(&json!({}), &json!({"value":2})).unwrap();
    journal.mark_collection_failure(&failure);
    let status = journal.status().unwrap();
    assert_eq!(status["accepting"], false);
    assert_eq!(status["collectionFailure"]["kind"], "storage.busy");
    assert_eq!(status["collectionFailure"]["persisted"], true);
    assert_eq!(
        other.status().unwrap()["collectionFailure"],
        status["collectionFailure"]
    );
    let later = other.append(&json!({}), &json!({"value":3})).unwrap_err();
    other.mark_collection_failure(&later);
    assert_eq!(
        other.status().unwrap()["collectionFailure"],
        status["collectionFailure"]
    );
    assert!(journal.append(&json!({}), &json!({"value":3})).is_err());
    assert_eq!(status["records"], 2);
    drop(journal);
    let journal = open(&path);
    assert_eq!(
        journal.status().unwrap()["collectionFailure"],
        status["collectionFailure"]
    );
    assert_eq!(journal.status().unwrap()["accepting"], false);
    assert_eq!(
        journal.prepare(10, 4096).unwrap()["records"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn unpersistable_collection_failure_remains_owned_and_honestly_reported() {
    let path = path();
    let journal = std::sync::Arc::new(open(&path));
    let outbox = Outbox::new(1, std::sync::Arc::new(NoWake));
    outbox
        .attach_journal(journal.clone(), json!({"epoch":"a"}))
        .unwrap();
    outbox
        .register_journal_consumer("c", json!({"generation":"a"}))
        .unwrap();
    let lock = rusqlite::Connection::open(&path).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(
        outbox
            .push_data(json!({"t":"value","consumer":"c","valueB64":"AQ=="}))
            .is_err()
    );
    outbox.seal();
    drop(outbox);
    let status = journal.status().unwrap();
    assert_eq!(status["accepting"], false);
    assert_eq!(status["phase"], "recording"); // Last durable fact, not invented persistence.
    assert_eq!(status["collectionFailure"]["persisted"], false);
    assert_eq!(
        status["collectionFailure"]["persistenceFailure"]["sqliteExtendedCode"],
        5
    );
    lock.execute_batch("ROLLBACK").unwrap();
    assert!(journal.append(&json!({}), &json!({"value":3})).is_err());
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[test]
fn sqlite_failures_preserve_safe_native_code_and_operation() {
    let path = path();
    let journal = open(&path);
    let competing = rusqlite::Connection::open(&path).unwrap();
    competing.execute_batch("BEGIN IMMEDIATE").unwrap();
    let failure = journal
        .append(&json!({}), &json!({"secret":"not diagnostic"}))
        .unwrap_err();
    assert_eq!(failure.operation, "append");
    assert_eq!(failure.sqlite_extended_code, Some(5));
    assert_eq!(failure.sqlite_code.as_deref(), Some("DatabaseBusy"));
    let diagnostic = journal.status().unwrap()["runtimeFailure"].clone();
    assert_eq!(diagnostic["sqliteExtendedCode"], 5);
    assert_eq!(diagnostic["operation"], "append");
    assert!(!format!("{failure:?}").contains("secret"));
    competing.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn independent_handles_share_one_cursor_and_transactional_counters() {
    let path = path();
    let first = open(&path);
    let second = open(&path);
    first
        .append(&json!({"owner":1}), &json!({"value":1}))
        .unwrap();
    let prepared = second.prepare(1, 4096).unwrap();
    second
        .append(&json!({"owner":2}), &json!({"value":2}))
        .unwrap();
    assert_eq!(first.prepare(2, 8192).unwrap(), prepared);
    assert!(first.acknowledge("unknown-owner-token").is_err());
    let token = prepared["token"].as_str().unwrap();
    assert_eq!(
        first.acknowledge(token).unwrap(),
        second.acknowledge(token).unwrap()
    );
    assert_eq!(first.status().unwrap()["records"], 1);
    assert_eq!(
        second.prepare(1, 4096).unwrap()["records"][0]["record"]["value"],
        2
    );
    let connection = rusqlite::Connection::open(&path).unwrap();
    let (count, bytes): (i64, i64) = connection
        .query_row(
            "SELECT retained_records,retained_bytes FROM journal",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(first.status().unwrap()["bytes"], bytes);
}

#[test]
fn simultaneous_handles_serialize_or_report_busy_without_losing_records() {
    let path = path();
    let first = std::sync::Arc::new(open(&path));
    let second = std::sync::Arc::new(open(&path));
    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = [first.clone(), second.clone()]
        .into_iter()
        .enumerate()
        .map(|(index, journal)| {
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                (
                    index,
                    journal.append(&json!({"owner":index}), &json!({"value":index})),
                )
            })
        })
        .collect();
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    for (index, outcome) in outcomes {
        if let Err(failure) = outcome {
            assert_eq!(failure.kind, "storage.busy");
            first
                .append(&json!({"owner":index}), &json!({"value":index}))
                .unwrap();
        }
    }
    assert_eq!(first.status().unwrap()["records"], 2);
    let batch = second.prepare(2, 8192).unwrap();
    assert_eq!(batch["records"].as_array().unwrap().len(), 2);
    first.acknowledge(batch["token"].as_str().unwrap()).unwrap();
    assert_eq!(second.status().unwrap()["records"], 0);
    drop(first);
    drop(second);
    assert_eq!(open(&path).status().unwrap()["records"], 0);
}

#[test]
fn durable_outbox_bypasses_volatile_cap_and_native_seal_never_acknowledges() {
    let path = path();
    let journal = std::sync::Arc::new(
        ContinuationJournal::open(
            &path,
            "recording-1",
            &json!({}),
            JournalQuota {
                max_bytes: 8 << 20,
                max_records: 10000,
            },
        )
        .unwrap(),
    );
    let outbox = Outbox::new(1, std::sync::Arc::new(NoWake));
    outbox
        .attach_journal(journal.clone(), json!({"epoch":"a","peer":"p"}))
        .unwrap();
    outbox
        .register_journal_consumer(
            "c",
            json!({"databaseGeneration":"g","selector":{"characteristic":"x"}}),
        )
        .unwrap();
    for index in 0..DATA_RECORD_CAP + 1 {
        outbox
            .push_data(json!({"t":"value","consumer":"c","valueB64":"AQ==","index":index}))
            .unwrap();
    }
    assert!(
        outbox.drain(2048, 4 << 20)["records"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(journal.status().unwrap()["records"], DATA_RECORD_CAP + 2);
    outbox.seal();
    assert_eq!(journal.status().unwrap()["records"], DATA_RECORD_CAP + 2);
    let batch = journal.prepare(2, 8192).unwrap();
    assert_eq!(batch["records"][0]["record"]["t"], "consumer-registration");
    assert_eq!(
        batch["records"][1]["metadata"]["consumer"]["databaseGeneration"],
        "g"
    );
}

#[test]
fn durable_failure_prevents_ack_observation_and_preserves_storage_cause() {
    let path = path();
    let journal = std::sync::Arc::new(open(&path));
    let outbox = Outbox::new(1, std::sync::Arc::new(NoWake));
    outbox
        .attach_journal(journal.clone(), json!({"epoch":"a","peer":"p"}))
        .unwrap();
    outbox
        .register_journal_consumer("c", json!({"generation":"g"}))
        .unwrap();
    let mut observation = outbox.observe("c", std::sync::Arc::new(|_| true)).unwrap();
    let lock = rusqlite::Connection::open(&path).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(
        outbox
            .push_data(json!({"t":"value","consumer":"c","valueB64":"AQ=="}))
            .is_err()
    );
    let terminal = observation.receiver.try_recv().unwrap();
    assert_eq!(terminal["t"], "stream-end");
    assert_eq!(terminal["reason"], "source-failed");
    assert_eq!(outbox.journal_failure().unwrap().kind, "storage.busy");
    lock.execute_batch("ROLLBACK").unwrap();
    assert_eq!(journal.status().unwrap()["records"], 1);
    assert!(
        outbox
            .push_data(json!({"t":"value","consumer":"c","valueB64":"Ag=="}))
            .is_err()
    );
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[test]
fn offline_handle_stop_seals_live_outbox_without_fabricating_collection_failure() {
    let path = path();
    let journal = std::sync::Arc::new(open(&path));
    let outbox = Outbox::new(1, std::sync::Arc::new(NoWake));
    outbox
        .attach_journal(journal.clone(), json!({"epoch":"a"}))
        .unwrap();
    outbox
        .register_journal_consumer("c", json!({"generation":"a"}))
        .unwrap();
    let offline = open(&path);
    offline.stop().unwrap();
    assert!(
        outbox
            .push_data(json!({"t":"value","consumer":"c","valueB64":"AQ=="}))
            .is_err()
    );
    assert!(outbox.is_sealed());
    assert!(outbox.journal_failure().is_none());
    assert!(journal.status().unwrap()["collectionFailure"].is_null());
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[test]
fn durable_registration_is_immutable_and_control_records_have_one_durable_copy() {
    let path = path();
    let journal = std::sync::Arc::new(open(&path));
    let outbox = Outbox::new(1, std::sync::Arc::new(NoWake));
    outbox
        .attach_journal(journal.clone(), json!({"epoch":"a","peer":"p"}))
        .unwrap();
    outbox
        .register_journal_consumer("c", json!({"generation":"a"}))
        .unwrap();
    outbox
        .register_journal_consumer("c", json!({"generation":"a"}))
        .unwrap();
    assert!(
        outbox
            .register_journal_consumer("c", json!({"generation":"b"}))
            .is_err()
    );
    outbox.push_control(json!({"t":"stream-end","consumer":"c","reason":"closed"}));
    assert_eq!(
        outbox.drain(10, 4096)["records"].as_array().unwrap().len(),
        1
    );
    assert_eq!(journal.status().unwrap()["records"], 2);
    assert_eq!(
        journal.prepare(10, 4096).unwrap()["records"][1]["record"]["reason"],
        "closed"
    );
    outbox.seal();
    journal.stop().unwrap();
    outbox.push_control(json!({"t":"stream-end","consumer":"c","reason":"closed"}));
    assert_eq!(
        outbox.drain(10, 4096)["records"].as_array().unwrap().len(),
        1
    );
    assert!(journal.status().unwrap()["collectionFailure"].is_null());
    assert!(outbox.journal_failure().is_none());
    assert!(
        outbox
            .register_journal_consumer("late", json!({"generation":"b"}))
            .is_err()
    );
    assert_eq!(journal.status().unwrap()["records"], 2);
}

#[test]
fn oversized_existing_file_is_rejected_before_integrity_scan() {
    let path = path();
    drop(open(&path));
    // A sparse oversized file must be rejected without reading its large tail.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(2 << 20)
        .unwrap();
    let failure = ContinuationJournal::open(
        &path,
        "recording-1",
        &json!({"onAppearance":"native"}),
        quota(),
    )
    .err()
    .unwrap();
    assert_eq!(failure.kind, "storage.full");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 2 << 20);
}

#[test]
fn prepared_prefix_and_ack_receipt_survive_restart_without_losing_later_data() {
    let path = path();
    let journal = open(&path);
    journal
        .append(
            &json!({"epoch":"a","peer":"p","selector":0}),
            &json!({"value":[1,2]}),
        )
        .unwrap();
    let first = journal.prepare(1, 4096).unwrap();
    journal
        .append(
            &json!({"epoch":"a","peer":"p","selector":0}),
            &json!({"value":[3]}),
        )
        .unwrap();
    assert_eq!(journal.prepare(2, 8192).unwrap(), first);
    drop(journal);
    let journal = open(&path);
    assert_eq!(journal.prepare(2, 8192).unwrap(), first);
    let token = first["token"].as_str().unwrap();
    let receipt = journal.acknowledge(token).unwrap();
    drop(journal);
    let journal = open(&path);
    assert_eq!(journal.acknowledge(token).unwrap(), receipt);
    let second = journal.prepare(2, 8192).unwrap();
    assert_eq!(second["records"].as_array().unwrap().len(), 1);
    assert_eq!(second["records"][0]["record"]["value"], json!([3]));
    assert_eq!(second["records"][0]["metadata"]["epoch"], "a");
    assert_eq!(journal.status().unwrap()["encrypted"], false);
}

#[test]
fn capacity_retains_owned_data_and_clear_requires_stop_and_invalidates_old_tokens() {
    let path = path();
    let journal = ContinuationJournal::open(
        &path,
        "recording-1",
        &json!({}),
        JournalQuota {
            max_records: 1,
            ..quota()
        },
    )
    .unwrap();
    journal.append(&json!({}), &json!({"value":1})).unwrap();
    let prepared = journal.prepare(1, 4096).unwrap();
    assert!(journal.clear().is_err());
    assert!(journal.append(&json!({}), &json!({"value":2})).is_err());
    assert_eq!(journal.status().unwrap()["phase"], "capacity-reached");
    assert!(journal.append(&json!({}), &json!({"value":3})).is_err());
    assert_eq!(journal.status().unwrap()["lostRecords"], 2);
    assert_eq!(journal.prepare(1, 4096).unwrap(), prepared);
    journal.stop().unwrap();
    journal.clear().unwrap();
    assert!(
        journal
            .acknowledge(prepared["token"].as_str().unwrap())
            .is_err()
    );
    assert_eq!(journal.status().unwrap()["records"], 0);
}

#[test]
fn corrupt_unknown_schema_and_identity_mismatch_fail_closed() {
    let path = path();
    std::fs::write(&path, b"not a database").unwrap();
    assert!(ContinuationJournal::open(&path, "recording-1", &json!({}), quota()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"not a database");
    let second = self::path();
    drop(open(&second));
    assert!(ContinuationJournal::open(&second, "different", &json!({}), quota()).is_err());
    let connection = rusqlite::Connection::open(&second).unwrap();
    connection.pragma_update(None, "user_version", 999).unwrap();
    drop(connection);
    assert!(
        ContinuationJournal::open(
            &second,
            "recording-1",
            &json!({"onAppearance":"native"}),
            quota()
        )
        .is_err()
    );
}

#[test]
fn crash_worker() {
    let Ok(path) = std::env::var("UBM_JOURNAL_CRASH_PATH") else {
        return;
    };
    let journal = open(std::path::Path::new(&path));
    match std::env::var("UBM_JOURNAL_CRASH_STAGE").unwrap().as_str() {
        "prepare" => {
            journal
                .append(&json!({"epoch":"crashed"}), &json!({"value":9}))
                .unwrap();
            journal.prepare(1, 4096).unwrap();
        }
        "ack" => {
            let batch = journal.prepare(1, 4096).unwrap();
            journal
                .acknowledge(batch["token"].as_str().unwrap())
                .unwrap();
        }
        _ => panic!("unknown crash stage"),
    }
    // No destructors: persisted commits, not close/drop, supply durability.
    std::process::exit(77);
}

#[test]
fn process_exit_before_ack_replays_and_exit_after_ack_does_not_resurrect() {
    let path = path();
    let run = |stage| {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_worker", "--nocapture"])
            .env("UBM_JOURNAL_CRASH_PATH", &path)
            .env("UBM_JOURNAL_CRASH_STAGE", stage)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(77));
    };
    run("prepare");
    let journal = open(&path);
    let first = journal.prepare(1, 4096).unwrap();
    assert_eq!(first["records"][0]["record"]["value"], 9);
    drop(journal);
    run("ack");
    let journal = open(&path);
    assert_eq!(journal.status().unwrap()["records"], 0);
    assert_eq!(
        journal
            .acknowledge(first["token"].as_str().unwrap())
            .unwrap()["acknowledged"],
        true
    );
}

#[test]
fn input_and_prepared_bounds_fail_without_mutating_the_owned_prefix() {
    let path = path();
    assert!(ContinuationJournal::open(&path, "../bad", &json!({}), quota()).is_err());
    assert!(!path.exists());
    let journal = open(&path);
    assert!(
        journal
            .append(&json!({"large":"x".repeat(16384)}), &json!({}))
            .is_err()
    );
    assert_eq!(journal.status().unwrap()["records"], 0);
    journal
        .append(&json!({}), &json!({"data":"x".repeat(1024)}))
        .unwrap();
    let prefix = journal.prepare(1, 4096).unwrap();
    assert!(journal.prepare(1, 1).is_err());
    assert_eq!(journal.prepare(1, 4096).unwrap(), prefix);
    assert!(journal.acknowledge("wrong").is_err());
    assert_eq!(journal.status().unwrap()["records"], 1);
}

#[test]
fn disk_quota_includes_database_and_live_rollback_journal() {
    let path = path();
    let journal = open(&path);
    let record = json!({"data":"x".repeat(60000)});
    while journal.append(&json!({"epoch":"large"}), &record).is_ok() {}
    assert_eq!(journal.status().unwrap()["phase"], "capacity-reached");
    let database = std::fs::metadata(&path).unwrap().len();
    assert!(database <= quota().max_bytes / 2);
    drop(journal);
    // Exercise SQLite's real rollback journal, including a worst-case rewrite
    // of every retained data page, not just the steady-state database length.
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA cache_spill=OFF; PRAGMA synchronous=EXTRA; BEGIN IMMEDIATE; UPDATE records SET body=replace(body,'x','y');").unwrap();
    let rollback = std::fs::metadata(path.with_extension("sqlite-journal"))
        .unwrap()
        .len();
    assert!(database + rollback <= quota().max_bytes);
    db.execute_batch("ROLLBACK").unwrap();
    drop(db);
    assert!(open(&path).status().unwrap()["records"].as_u64().unwrap() > 0);
}

#[test]
fn writer_lock_failure_is_explicit_and_never_claimed_as_persisted_loss() {
    let path = path();
    let journal = open(&path);
    let other = rusqlite::Connection::open(&path).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let failure = journal.append(&json!({}), &json!({"value":1})).unwrap_err();
    assert_eq!(failure.kind, "storage.busy");
    other.execute_batch("ROLLBACK").unwrap();
    assert_eq!(journal.status().unwrap()["lostRecords"], 0);
    assert_eq!(
        journal.status().unwrap()["runtimeFailure"]["persisted"],
        false
    );
}

#[test]
fn incompatible_wal_mode_and_deep_metadata_are_refused_without_rewriting() {
    let path = path();
    drop(open(&path));
    let other = rusqlite::Connection::open(&path).unwrap();
    other.execute_batch("PRAGMA journal_mode=WAL").unwrap();
    drop(other);
    assert!(
        ContinuationJournal::open(
            &path,
            "recording-1",
            &json!({"onAppearance":"native"}),
            quota()
        )
        .is_err()
    );
    let other = rusqlite::Connection::open(&path).unwrap();
    let mode: String = other
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let journal = open(&self::path());
    let mut nested = json!({});
    for _ in 0..70 {
        nested = json!({"nested":nested});
    }
    assert!(journal.append(&nested, &json!({})).is_err());
}

#[test]
fn stop_survives_restart_and_empty_or_unwritable_locations_never_become_fresh_recordings() {
    let path = path();
    let journal = open(&path);
    journal
        .append(&json!({"epoch":"stopped"}), &json!({"value":1}))
        .unwrap();
    journal.stop().unwrap();
    drop(journal);
    let journal = open(&path);
    assert_eq!(journal.status().unwrap()["accepting"], false);
    assert!(journal.append(&json!({}), &json!({})).is_err());
    assert_eq!(
        journal.prepare(1, 4096).unwrap()["records"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let empty = self::path();
    std::fs::write(&empty, []).unwrap();
    assert!(ContinuationJournal::open(&empty, "recording-1", &json!({}), quota()).is_err());
    assert_eq!(std::fs::metadata(&empty).unwrap().len(), 0);
    let missing_parent = self::path().join("absent").join("journal.sqlite");
    assert!(
        ContinuationJournal::open(&missing_parent, "recording-1", &json!({}), quota()).is_err()
    );
    assert!(!missing_parent.exists());
}

#[cfg(unix)]
#[test]
fn symlink_destination_is_refused_without_changing_target() {
    let target = path();
    std::fs::write(&target, b"private unrelated bytes").unwrap();
    let link = path();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(ContinuationJournal::open(&link, "recording-1", &json!({}), quota()).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"private unrelated bytes");
}
