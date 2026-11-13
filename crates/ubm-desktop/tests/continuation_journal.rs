use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use ubm_desktop::continuation_journal::{ContinuationJournal, JournalQuota, JournalRegistry};
use ubm_desktop::continuation_outbox::{DATA_RECORD_CAP, Outbox, WakeSink};
#[path = "../../test-support/recording_fixture.rs"]
mod recording_fixture;
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
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    path.join("recording.sqlite")
}
fn quota() -> JournalQuota {
    JournalQuota {
        max_bytes: 1 << 20,
        max_records: 100,
    }
}
fn open_with(
    path: &std::path::Path,
    declaration: &serde_json::Value,
    quota: JournalQuota,
) -> ContinuationJournal {
    #[cfg(windows)]
    let protector = std::sync::Arc::new(
        ubm_desktop::continuation_journal::WindowsCurrentUserScanRecoveryStorageProtector::new(),
    );
    #[cfg(windows)]
    ubm_desktop::continuation_journal::ScanRecoveryStorageProtector::provision_directory(
        protector.as_ref(),
        path.parent().unwrap(),
    )
    .unwrap();
    #[cfg(windows)]
    let journal = ContinuationJournal::open_with_scan_recovery_storage(
        path,
        "recording-1",
        declaration,
        quota,
        Some(protector),
    );
    #[cfg(not(windows))]
    let journal = ContinuationJournal::open(path, "recording-1", declaration, quota);
    journal.unwrap()
}
fn open(path: &std::path::Path) -> ContinuationJournal {
    open_with(path, &json!({"onAppearance":"native"}), quota())
}

fn configure_registry_directory(registry: &JournalRegistry, path: &std::path::Path) {
    #[cfg(windows)]
    registry
        .configure_directory_with_scan_recovery_storage(
            path,
            std::sync::Arc::new(
                ubm_desktop::continuation_journal::WindowsCurrentUserScanRecoveryStorageProtector::new(),
            ),
        )
        .unwrap();
    #[cfg(not(windows))]
    registry.configure_directory(path).unwrap();
}

#[test]
fn readable_production_loss_exposes_an_exact_occurrence_through_host() {
    use std::sync::Arc;
    use ubm_desktop::continuation::{ContinuationHost, ContinuationSession, NativeContinuation};
    use ubm_desktop::continuation_journal::{JournalError, RecordedLoss};
    struct NoHost;
    impl ContinuationHost for NoHost {
        fn open_session(&self) -> ubm_desktop::continuation::Result<Arc<dyn ContinuationSession>> {
            panic!("recording management does not open a radio session")
        }
    }
    let directory = path().parent().unwrap().to_path_buf();
    let journal = open(&directory.join("recording-1.sqlite"));
    let host = NativeContinuation::new(Arc::new(NoHost));
    host.configure_recording_directory(&directory).unwrap();
    let failure = JournalError {
        kind: "storage.io",
        detail: "source loss",
        operation: "test",
        sqlite_code: None,
        sqlite_extended_code: None,
    };
    journal.mark_recorded_collection_loss(
        &failure,
        RecordedLoss {
            items: 2,
            bytes: Some(7),
        },
    );
    let occurrences = host
        .recording_diagnostic_occurrences_page("recording-1", None, 8)
        .unwrap();
    assert_eq!(occurrences.len(), 1);
    let page = host
        .recording_diagnostic_occurrence_page("recording-1", occurrences[0], 0, 4096)
        .unwrap();
    assert!(page.durable);
    let payload: serde_json::Value = serde_json::from_slice(&page.bytes).unwrap();
    assert_eq!(payload["loss"], json!({"items":2,"bytes":7}));
}

#[test]
fn refused_production_loss_is_exportable_from_host_before_retry() {
    use std::sync::Arc;
    use ubm_desktop::continuation::{ContinuationHost, ContinuationSession, NativeContinuation};
    use ubm_desktop::continuation_journal::{JournalError, RecordedLoss};
    struct NoHost;
    impl ContinuationHost for NoHost {
        fn open_session(&self) -> ubm_desktop::continuation::Result<Arc<dyn ContinuationSession>> {
            panic!("recording management does not open a radio session")
        }
    }
    let directory = path().parent().unwrap().to_path_buf();
    let file = directory.join("recording-1.sqlite");
    let journal = open(&file);
    let db = rusqlite::Connection::open(&file).unwrap();
    db.execute(
        "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped'",
        [b"bad\xffsummary".as_slice()],
    )
    .unwrap();
    db.execute_batch("CREATE TRIGGER reject_occurrence BEFORE INSERT ON diagnostic_ledger BEGIN SELECT RAISE(ABORT, 'refused'); END;").unwrap();
    let host = NativeContinuation::new(Arc::new(NoHost));
    host.configure_recording_directory(&directory).unwrap();
    let failure = JournalError {
        kind: "storage.io",
        detail: "source loss",
        operation: "test",
        sqlite_code: None,
        sqlite_extended_code: None,
    };
    journal.mark_recorded_collection_loss(
        &failure,
        RecordedLoss {
            items: 2,
            bytes: Some(7),
        },
    );
    assert_eq!(
        journal.status().unwrap()["collectionFailure"]["persisted"],
        false
    );
    let ids = host
        .recording_diagnostic_occurrences_page("recording-1", None, 8)
        .unwrap();
    assert_eq!(ids.len(), 1);
    let page = host
        .recording_diagnostic_occurrence_page("recording-1", ids[0], 0, 4096)
        .unwrap();
    assert!(!page.durable);
    let payload: serde_json::Value = serde_json::from_slice(&page.bytes).unwrap();
    assert_eq!(payload["loss"], json!({"items":2,"bytes":7}));
    db.execute_batch("DROP TRIGGER reject_occurrence").unwrap();
    host.recording_retry_diagnostic_occurrence("recording-1", ids[0])
        .unwrap();
    let committed = host
        .recording_diagnostic_occurrence_page("recording-1", ids[0], 0, 4096)
        .unwrap();
    assert!(committed.durable);
    assert_eq!(committed.bytes, page.bytes);
    assert_eq!(
        host.recording_status("recording-1").unwrap()["collectionFailure"]["pendingRecordedLoss"],
        json!({"items":2,"bytes":7,"persisted":true})
    );
}

// The writer and reader are separate processes: clearing an in-process cache
// is not a substitute for a host restart. The reader uses the production
// continuation management route without opening a radio session.
#[test]
fn unreadable_summary_loss_survives_process_restart_at_host_status() {
    use std::sync::Arc;
    use ubm_desktop::continuation::{ContinuationHost, ContinuationSession, NativeContinuation};
    use ubm_desktop::continuation_journal::{JournalError, RecordedLoss};
    struct NoHost;
    impl ContinuationHost for NoHost {
        fn open_session(&self) -> ubm_desktop::continuation::Result<Arc<dyn ContinuationSession>> {
            panic!("recording management does not open a radio session")
        }
    }
    const CHILD: &str = "UBM_UNREADABLE_LOSS_CHILD";
    const DIRECTORY: &str = "UBM_UNREADABLE_LOSS_DIRECTORY";
    if let Some(mode) = std::env::var_os(CHILD) {
        let directory = PathBuf::from(std::env::var_os(DIRECTORY).unwrap());
        if mode == "write" {
            let journal = open(&directory.join("recording-1.sqlite"));
            journal.mark_recorded_collection_loss(
                &JournalError {
                    kind: "storage.io",
                    detail: "later",
                    operation: "test",
                    sqlite_code: None,
                    sqlite_extended_code: None,
                },
                RecordedLoss {
                    items: 2,
                    bytes: Some(7),
                },
            );
        } else {
            let engine = NativeContinuation::new(Arc::new(NoHost));
            engine.configure_recording_directory(&directory).unwrap();
            let status = engine.recording_status("recording-1").unwrap();
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
            let ids = engine
                .recording_diagnostic_occurrences_page("recording-1", None, 8)
                .unwrap();
            assert_eq!(ids.len(), 1);
            let page = engine
                .recording_diagnostic_occurrence_page("recording-1", ids[0], 0, 4096)
                .unwrap();
            let payload: serde_json::Value = serde_json::from_slice(&page.bytes).unwrap();
            assert_eq!(payload["loss"], json!({"items":2,"bytes":7}));
            assert!(page.durable);
        }
        return;
    }
    let directory = path().parent().unwrap().to_path_buf();
    let file = directory.join("recording-1.sqlite");
    drop(open(&file));
    let db = rusqlite::Connection::open(&file).unwrap();
    let original = b"bad\xffsummary";
    db.execute(
        "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped',lost=4",
        [original.as_slice()],
    )
    .unwrap();
    drop(db);
    for mode in ["write", "read"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unreadable_summary_loss_survives_process_restart_at_host_status",
                "--nocapture",
            ])
            .env(CHILD, mode)
            .env(DIRECTORY, &directory)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout).to_string()
                + &String::from_utf8_lossy(&output.stderr)
        );
    }
    let db = rusqlite::Connection::open(&file).unwrap();
    let retained: Vec<u8> = db
        .query_row(
            "SELECT CAST(collection_failure AS BLOB) FROM journal",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, original);
}

#[test]
fn producer_loss_identity_replays_across_reopen_without_double_counting() {
    use ubm_desktop::continuation_journal::{JournalError, RecordedLoss};
    const CHILD: &str = "UBM_LOSS_RECEIPT_CHILD";
    const FILE: &str = "UBM_LOSS_RECEIPT_FILE";
    if let Some(mode) = std::env::var_os(CHILD) {
        let file = PathBuf::from(std::env::var_os(FILE).unwrap());
        let journal = open(&file);
        let failure = JournalError {
            kind: "storage.io",
            detail: "source loss",
            operation: "test",
            sqlite_code: None,
            sqlite_extended_code: None,
        };
        let id = [0x42; 40];
        let receipt = journal
            .record_loss_occurrence(
                id,
                &failure,
                RecordedLoss {
                    items: 2,
                    bytes: Some(7),
                },
            )
            .unwrap();
        assert_eq!(receipt.occurrence, id);
        if mode == "retry" {
            assert_eq!(
                journal
                    .record_loss_occurrence(
                        id,
                        &failure,
                        RecordedLoss {
                            items: 3,
                            bytes: Some(7)
                        }
                    )
                    .unwrap_err()
                    .kind,
                "storage.identity"
            );
            let status = journal.status().unwrap();
            assert_eq!(status["lostRecords"], 4);
            assert_eq!(
                status["collectionFailure"]["pendingRecordedLoss"],
                json!({"items":2,"bytes":7,"persisted":true})
            );
        }
        return;
    }
    let file = path();
    drop(open(&file));
    let db = rusqlite::Connection::open(&file).unwrap();
    let original = b"bad\xffsummary";
    db.execute(
        "UPDATE journal SET collection_failure=CAST(?1 AS TEXT),phase='stopped',lost=4",
        [original.as_slice()],
    )
    .unwrap();
    drop(db);
    for mode in ["write", "retry"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "producer_loss_identity_replays_across_reopen_without_double_counting",
                "--nocapture",
            ])
            .env(CHILD, mode)
            .env(FILE, &file)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout).to_string()
                + &String::from_utf8_lossy(&output.stderr)
        );
    }
    let db = rusqlite::Connection::open(&file).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT CAST(collection_failure AS BLOB) FROM journal",
            [],
            |row| row.get::<_, Vec<u8>>(0)
        )
        .unwrap(),
        original
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM diagnostic_ledger", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn host_exports_unreadable_summary_as_bounded_exact_bytes_after_reopen() {
    use std::sync::Arc;
    use ubm_desktop::continuation::{ContinuationHost, ContinuationSession, NativeContinuation};
    struct NoHost;
    impl ContinuationHost for NoHost {
        fn open_session(&self) -> ubm_desktop::continuation::Result<Arc<dyn ContinuationSession>> {
            panic!("recording management does not open a radio session")
        }
    }
    let directory = path().parent().unwrap().to_path_buf();
    let file = directory.join("recording-1.sqlite");
    drop(open(&file));
    let original = b"bad\xffsummary";
    let db = rusqlite::Connection::open(&file).unwrap();
    db.execute(
        "UPDATE journal SET collection_failure=CAST(?1 AS TEXT) WHERE id=1",
        [original.as_slice()],
    )
    .unwrap();
    drop(db);
    let host = NativeContinuation::new(Arc::new(NoHost));
    host.configure_recording_directory(&directory).unwrap();
    let first = host
        .recording_diagnostic_summary_page("recording-1", 0, 4)
        .unwrap();
    let second = host
        .recording_diagnostic_summary_page("recording-1", 4, 64)
        .unwrap();
    assert_eq!(first["bytes"], json!(&original[..4]));
    assert_eq!(second["bytes"], json!(&original[4..]));
    assert_eq!(first["totalBytes"], original.len());
    assert_eq!(second["totalBytes"], original.len());
    assert!(
        host.recording_diagnostic_summary_page("recording-1", 0, 0)
            .is_err()
    );
    assert!(
        host.recording_diagnostic_summary_page("recording-1", original.len() as u64 + 1, 1)
            .is_err()
    );
}

// Exercise the existing host-facing clear route in a fresh process, including
// files already migrated by the earlier v6 implementation.
#[test]
fn legacy_failure_public_clear_across_restart() {
    use std::sync::Arc;
    use ubm_desktop::continuation::{ContinuationHost, ContinuationSession, NativeContinuation};
    struct NoHost;
    impl ContinuationHost for NoHost {
        fn open_session(&self) -> ubm_desktop::continuation::Result<Arc<dyn ContinuationSession>> {
            panic!("recording management does not open a radio session")
        }
    }
    const CHILD: &str = "UBM_JOURNAL_LEGACY_CLEAR_CHILD";
    if let Some(directory) = std::env::var_os(CHILD) {
        let engine = NativeContinuation::new(Arc::new(NoHost));
        engine
            .configure_recording_directory(std::path::Path::new(&directory))
            .unwrap();
        let status = engine.recording_status("recording-1").unwrap();
        assert_eq!(status["collectionFailure"]["detail"], "legacy detail");
        assert_eq!(status["records"], 1);
        assert_eq!(
            engine.recording_prepare("recording-1", 1, 4096).unwrap()["records"][0]["record"]["kept"],
            true
        );
        let cleared = engine.recording_clear("recording-1").unwrap();
        assert_eq!(cleared["cleared"], true);
        assert_eq!(cleared["records"], 1);
        assert_eq!(
            engine.recording_status("recording-1").unwrap()["collectionFailure"]["detail"],
            "legacy detail"
        );
        return;
    }
    for already_migrated in [false, true] {
        let directory = path().parent().unwrap().to_path_buf();
        let file = directory.join("recording-1.sqlite");
        let journal = open(&file);
        journal.append(&json!({}), &json!({"kept":true})).unwrap();
        journal.mark_collection_failure(&ubm_desktop::continuation_journal::JournalError {
            kind: "storage.io",
            detail: "legacy detail",
            operation: "test",
            sqlite_code: None,
            sqlite_extended_code: None,
        });
        drop(journal);
        let db = rusqlite::Connection::open(&file).unwrap();
        let original: String = db
            .query_row("SELECT collection_failure FROM journal", [], |row| {
                row.get(0)
            })
            .unwrap();
        if already_migrated {
            db.execute_batch("DROP TABLE diagnostic_owners; PRAGMA user_version=6;")
                .unwrap();
            let payload = json!({"category":"legacy", "report":original}).to_string();
            db.execute(
                "INSERT INTO diagnostic_ledger(occurrence,payload) VALUES(zeroblob(40),?1)",
                [payload],
            )
            .unwrap();
        } else {
            db.execute_batch("DROP TABLE diagnostic_owners; DROP TABLE diagnostic_ledger; PRAGMA user_version=5;").unwrap();
        }
        drop(db);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "legacy_failure_public_clear_across_restart",
                "--nocapture",
            ])
            .env(CHILD, &directory)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout).to_string()
                + &String::from_utf8_lossy(&output.stderr)
        );
        let db = rusqlite::Connection::open(&file).unwrap();
        assert_eq!(
            db.query_row("SELECT collection_failure FROM journal", [], |row| row
                .get::<_, String>(
                0
            ))
            .unwrap(),
            original
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM diagnostic_ledger", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn migration_repair_keeps_nonmatching_zero_identity() {
    let file = path();
    let journal = open(&file);
    journal.mark_collection_failure(&ubm_desktop::continuation_journal::JournalError {
        kind: "storage.io",
        detail: "original",
        operation: "test",
        sqlite_code: None,
        sqlite_extended_code: None,
    });
    drop(journal);
    let db = rusqlite::Connection::open(&file).unwrap();
    let payload = json!({"category":"legacy", "report":"different"}).to_string();
    db.execute(
        "INSERT INTO diagnostic_ledger(occurrence,payload) VALUES(zeroblob(40),?1)",
        [&payload],
    )
    .unwrap();
    drop(db);
    let reopened = open(&file);
    assert_eq!(reopened.clear().unwrap_err().kind, "storage.busy");
    let db = rusqlite::Connection::open(&file).unwrap();
    assert_eq!(
        db.query_row("SELECT payload FROM diagnostic_ledger", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        payload
    );
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
fn independent_registries_share_live_authority_and_serialize_export_with_ingress() {
    let path = path();
    let writer = JournalRegistry::default();
    let reader = JournalRegistry::default();
    writer.configure_directory(path.parent().unwrap()).unwrap();
    reader.configure_directory(path.parent().unwrap()).unwrap();
    let journal = writer.open("shared", &json!({}), quota()).unwrap();
    let export = reader.get("shared").unwrap();
    assert!(std::sync::Arc::ptr_eq(&journal, &export));
    journal.append(&json!({}), &json!({"value":1})).unwrap();
    let prefix = export.prepare(1, 4096).unwrap();
    journal.append(&json!({}), &json!({"value":2})).unwrap();
    assert_eq!(export.prepare(1, 4096).unwrap(), prefix);
    assert_eq!(journal.status().unwrap()["records"], 2);
}

#[test]
fn registry_bounds_inactive_handles_across_one_thousand_ids_and_preserves_active_owner() {
    let path = path();
    let registry = JournalRegistry::default();
    registry
        .configure_directory(path.parent().unwrap())
        .unwrap();
    let active = registry.open("active", &json!({}), quota()).unwrap();
    active.append(&json!({}), &json!({"value":7})).unwrap();
    let mut inactive = Vec::new();
    for index in 0..1000 {
        let journal = registry
            .open(&format!("inactive-{index}"), &json!({}), quota())
            .unwrap();
        journal.stop().unwrap();
        journal.clear().unwrap();
        inactive.push(std::sync::Arc::downgrade(&journal));
    }
    assert!(
        inactive
            .iter()
            .filter(|journal| journal.strong_count() > 0)
            .count()
            <= 16
    );
    assert!(std::sync::Arc::ptr_eq(
        &active,
        &registry.get("active").unwrap()
    ));
    assert_eq!(
        registry.get("inactive-0").unwrap().status().unwrap()["records"],
        0
    );
    assert_eq!(
        active.prepare(1, 4096).unwrap()["records"][0]["record"]["value"],
        7
    );
}

#[test]
fn independent_stop_then_late_ingress_retires_one_thousand_journals() {
    let Some(directory) = recording_fixture::isolated_fixture_process(
        "independent_stop_then_late_ingress_retires_one_thousand_journals",
    ) else {
        return;
    };
    let writer = JournalRegistry::default();
    let reader = JournalRegistry::default();
    configure_registry_directory(&writer, &directory);
    configure_registry_directory(&reader, &directory);
    let active = writer.open("active", &json!({}), quota()).unwrap();
    active.append(&json!({}), &json!({"value":7})).unwrap();
    let mut retired = Vec::new();
    for index in 0..1000 {
        let id = format!("retired-{index}");
        let journal = writer.open(&id, &json!({}), quota()).unwrap();
        let outbox = Outbox::new(1, std::sync::Arc::new(NoWake));
        outbox
            .attach_journal(journal.clone(), json!({"epoch":"a"}))
            .unwrap();
        outbox
            .register_journal_consumer("c", json!({"generation":"a"}))
            .unwrap();
        reader.get(&id).unwrap().stop().unwrap();
        assert!(!outbox.is_sealed());
        // Synchronous ingress occurs before producer disposal: sealing first
        // would bypass the stopped journal and falsely pass this regression.
        assert!(matches!(
            outbox.push_data(json!({"t":"value","consumer":"c","valueB64":"AQ=="})),
            Err(ubm_desktop::continuation_outbox::DataIngressFailure::Stopped { .. })
        ));
        assert!(outbox.is_sealed());
        assert_eq!(outbox.after_cutoff_loss().items, 1);
        assert!(outbox.journal_failure().is_none());
        let status = journal.status().unwrap();
        assert_eq!(status["phase"], "stopped");
        assert_eq!(status["records"], 1); // Registration only; late value refused.
        assert!(status["runtimeFailure"].is_null());
        assert!(status["collectionFailure"].is_null());
        drop(outbox);
        assert_eq!(journal.clear().unwrap()["cleared"], true);
        let status = journal.status().unwrap();
        assert_eq!(status["records"], 0);
        assert_eq!(status["bytes"], 0);
        retired.push(std::sync::Arc::downgrade(&journal));
        drop(journal);
        // Lookup drives normal cache maintenance at each completed checkpoint.
        assert!(std::sync::Arc::ptr_eq(
            &active,
            &writer.get("active").unwrap()
        ));
        assert!(
            retired
                .iter()
                .filter(|journal| journal.strong_count() > 0)
                .count()
                <= 16
        );
    }
    assert_eq!(active.status().unwrap()["records"], 1);
    assert_eq!(
        reader.get("retired-0").unwrap().status().unwrap()["records"],
        0
    );
    recording_fixture::complete_fixture_process(&directory);
}

#[test]
fn stopped_refusal_does_not_erase_or_unpin_real_uncommitted_failure() {
    let path = path();
    let directory = path.parent().unwrap();
    let registry = JournalRegistry::default();
    registry.configure_directory(directory).unwrap();
    let journal = registry
        .open("failed-before-stop", &json!({}), quota())
        .unwrap();
    let lock = rusqlite::Connection::open(directory.join("failed-before-stop.sqlite")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let failure = journal.append(&json!({}), &json!({"value":1})).unwrap_err();
    assert_eq!(failure.kind, "storage.busy");
    let diagnostic = journal.status().unwrap()["runtimeFailure"].clone();
    assert_eq!(diagnostic["persisted"], false);
    lock.execute_batch("ROLLBACK").unwrap();
    journal.stop().unwrap();
    assert_eq!(
        journal
            .append(&json!({}), &json!({"value":2}))
            .unwrap_err()
            .kind,
        "storage.stopped"
    );
    journal.clear().unwrap();
    assert_eq!(journal.status().unwrap()["runtimeFailure"], diagnostic);
    let weak = std::sync::Arc::downgrade(&journal);
    drop(journal);
    for index in 0..32 {
        registry
            .open(&format!("failure-pressure-{index}"), &json!({}), quota())
            .unwrap();
    }
    let retained = registry.get("failed-before-stop").unwrap();
    assert!(std::sync::Arc::ptr_eq(&weak.upgrade().unwrap(), &retained));
    assert_eq!(retained.status().unwrap()["runtimeFailure"], diagnostic);
    assert_eq!(retained.status().unwrap()["records"], 0);
}

#[test]
fn process_authority_worker() {
    let Some(directory) = std::env::var_os("UBM_TEST_AUTHORITY_DIRECTORY") else {
        return;
    };
    let registry = JournalRegistry::default();
    registry
        .configure_directory(std::path::Path::new(&directory))
        .unwrap();
    if std::env::var_os("UBM_TEST_AUTHORITY_CREATE").is_some() {
        registry
            .open("shared", &json!({}), quota())
            .unwrap()
            .append(&json!({}), &json!({"value":9}))
            .unwrap();
        return;
    }
    let failure = match registry.get("shared") {
        Err(failure) => failure,
        Ok(_) => panic!("foreign process acquired a live recording authority"),
    };
    assert_eq!(failure.kind, "storage.busy");
}

#[test]
fn process_exit_releases_authority_and_cold_reopen_preserves_rows() {
    let path = path();
    let outcome = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process_authority_worker"])
        .env("UBM_TEST_AUTHORITY_DIRECTORY", path.parent().unwrap())
        .env("UBM_TEST_AUTHORITY_CREATE", "1")
        .status()
        .unwrap();
    assert!(outcome.success());
    let registry = JournalRegistry::default();
    registry
        .configure_directory(path.parent().unwrap())
        .unwrap();
    assert_eq!(
        registry.get("shared").unwrap().prepare(1, 4096).unwrap()["records"][0]["record"]["value"],
        9
    );
}

#[test]
fn authority_capacity_worker() {
    let Some(directory) = std::env::var_os("UBM_TEST_AUTHORITY_CAPACITY") else {
        return;
    };
    let registry = JournalRegistry::default();
    registry
        .configure_directory(std::path::Path::new(&directory))
        .unwrap();
    let mut owners = Vec::new();
    for index in 0..256 {
        owners.push(
            registry
                .open(&format!("owner-{index}"), &json!({}), quota())
                .unwrap(),
        );
    }
    assert_eq!(
        registry
            .open("excess", &json!({}), quota())
            .err()
            .unwrap()
            .kind,
        "storage.busy"
    );
    assert!(std::sync::Arc::ptr_eq(
        &owners[0],
        &registry.get("owner-0").unwrap()
    ));
    assert!(
        !std::path::Path::new(&directory)
            .join("excess.sqlite")
            .exists()
    );
    let weak: Vec<_> = owners.iter().map(std::sync::Arc::downgrade).collect();
    drop(owners);
    registry.get("owner-0").unwrap();
    assert!(weak.iter().filter(|owner| owner.strong_count() > 0).count() <= 17);
}

#[test]
fn authority_capacity_refuses_only_new_ids_and_releases_unpinned_handles() {
    let path = path();
    let outcome = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "authority_capacity_worker"])
        .env("UBM_TEST_AUTHORITY_CAPACITY", path.parent().unwrap())
        .status()
        .unwrap();
    assert!(outcome.success());
}

#[test]
fn evicted_journals_reopen_the_same_prepared_token_without_acknowledging() {
    let path = path();
    let registry = JournalRegistry::default();
    registry
        .configure_directory(path.parent().unwrap())
        .unwrap();
    let mut prefixes = Vec::new();
    for index in 0..32 {
        let id = format!("prepared-{index}");
        let journal = registry.open(&id, &json!({}), quota()).unwrap();
        journal.append(&json!({}), &json!({"value":index})).unwrap();
        let prefix = journal.prepare(1, 4096).unwrap();
        journal.stop().unwrap();
        prefixes.push((id, std::sync::Arc::downgrade(&journal), prefix));
    }
    let (id, _, prefix) = prefixes
        .iter()
        .find(|(_, journal, _)| journal.strong_count() == 0)
        .unwrap();
    let reopened = registry.get(id).unwrap();
    assert_eq!(&reopened.prepare(1, 4096).unwrap(), prefix);
    assert_eq!(reopened.status().unwrap()["records"], 1);
}

#[test]
fn foreign_process_is_refused_before_export_can_contend_with_the_writer() {
    let path = path();
    let registry = JournalRegistry::default();
    registry
        .configure_directory(path.parent().unwrap())
        .unwrap();
    let journal = registry.open("shared", &json!({}), quota()).unwrap();
    journal.append(&json!({}), &json!({"value":1})).unwrap();
    let outcome = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process_authority_worker"])
        .env("UBM_TEST_AUTHORITY_DIRECTORY", path.parent().unwrap())
        .status()
        .unwrap();
    assert!(outcome.success());
    journal.append(&json!({}), &json!({"value":2})).unwrap();
    assert_eq!(journal.status().unwrap()["accepting"], true);
    assert_eq!(journal.status().unwrap()["records"], 2);
}

#[cfg(unix)]
#[test]
fn symlink_authority_is_refused_before_creating_the_recording() {
    let path = path();
    let directory = path.parent().unwrap();
    let target = directory.join("target");
    std::fs::write(&target, []).unwrap();
    std::os::unix::fs::symlink(&target, directory.join("shared.authority")).unwrap();
    let registry = JournalRegistry::default();
    registry.configure_directory(directory).unwrap();
    assert_eq!(
        registry
            .open("shared", &json!({}), quota())
            .err()
            .unwrap()
            .kind,
        "storage.permission"
    );
    assert!(!directory.join("shared.sqlite").exists());
    assert_eq!(std::fs::metadata(target).unwrap().len(), 0);
}

#[cfg(unix)]
#[test]
fn cached_authority_refuses_directory_replacement_at_the_same_path() {
    let path = path();
    let directory = path.parent().unwrap();
    let registry = JournalRegistry::default();
    registry.configure_directory(directory).unwrap();
    let journal = registry.open("shared", &json!({}), quota()).unwrap();
    std::fs::rename(directory, directory.with_extension("original")).unwrap();
    std::fs::create_dir(directory).unwrap();
    assert_eq!(
        registry.get("shared").err().unwrap().kind,
        "storage.permission"
    );
    assert_eq!(
        registry.configure_directory(directory).err().unwrap().kind,
        "storage.identity"
    );
    assert!(!directory.join("shared.sqlite").exists());
    assert_eq!(journal.status().unwrap()["records"], 0);
}

#[cfg(unix)]
#[test]
fn cached_authority_refuses_replaced_journal_and_keeps_original_owner() {
    let path = path();
    let directory = path.parent().unwrap();
    let registry = JournalRegistry::default();
    registry.configure_directory(directory).unwrap();
    let journal = registry.open("shared", &json!({}), quota()).unwrap();
    std::fs::rename(
        directory.join("shared.sqlite"),
        directory.join("original.sqlite"),
    )
    .unwrap();
    std::fs::write(directory.join("shared.sqlite"), []).unwrap();
    assert_eq!(
        registry.get("shared").err().unwrap().kind,
        "storage.identity"
    );
    assert_eq!(journal.status().unwrap()["records"], 0);
}

#[test]
fn eviction_pressure_preserves_unpersisted_failure_truth_across_controllers() {
    let path = path();
    let directory = path.parent().unwrap();
    let writer = JournalRegistry::default();
    writer.configure_directory(directory).unwrap();
    let journal = writer.open("failed", &json!({}), quota()).unwrap();
    let lock = rusqlite::Connection::open(directory.join("failed.sqlite")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let failure = journal.append(&json!({}), &json!({"value":1})).unwrap_err();
    journal.mark_collection_failure(&failure);
    assert_eq!(
        journal.status().unwrap()["collectionFailure"]["persisted"],
        false
    );
    let weak = std::sync::Arc::downgrade(&journal);
    drop(journal);
    lock.execute_batch("ROLLBACK").unwrap();
    for index in 0..32 {
        writer
            .open(&format!("pressure-{index}"), &json!({}), quota())
            .unwrap();
    }
    let reader = JournalRegistry::default();
    reader.configure_directory(directory).unwrap();
    let journal = reader.get("failed").unwrap();
    assert!(std::sync::Arc::ptr_eq(&weak.upgrade().unwrap(), &journal));
    assert_eq!(
        journal.status().unwrap()["collectionFailure"]["persisted"],
        false
    );
    assert_eq!(journal.status().unwrap()["accepting"], false);
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
    let journal = std::sync::Arc::new(open_with(
        &path,
        &json!({}),
        JournalQuota {
            max_bytes: 8 << 20,
            max_records: 10000,
        },
    ));
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
    let journal = open_with(
        &path,
        &json!({}),
        JournalQuota {
            max_records: 1,
            ..quota()
        },
    );
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

#[test]
fn ordinary_refusals_allow_authority_eviction() {
    let path = path();
    let registry = JournalRegistry::default();
    registry
        .configure_directory(path.parent().unwrap())
        .unwrap();
    let mut owners = Vec::new();
    for index in 0..40 {
        let journal = registry
            .open(
                &format!("refusal-{index}"),
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 1,
                },
            )
            .unwrap();
        if index % 2 == 0 {
            assert_eq!(
                journal.append_batch(&[]).unwrap_err().kind,
                "argument.invalid"
            );
        } else {
            journal.append(&json!({}), &json!({"value":1})).unwrap();
            assert_eq!(
                journal
                    .append(&json!({}), &json!({"value":2}))
                    .unwrap_err()
                    .kind,
                "storage.full"
            );
        }
        assert!(journal.status().unwrap()["runtimeFailure"].is_null());
        owners.push(std::sync::Arc::downgrade(&journal));
    }
    registry
        .open("after-refusals", &json!({}), quota())
        .unwrap();
    assert!(
        owners
            .iter()
            .filter(|owner| owner.strong_count() > 0)
            .count()
            <= 16
    );
}
