// crates/ubm-desktop/tests/recorded_loss.rs
use serde_json::json;
use ubm_desktop::continuation_journal::{ContinuationJournal, JournalQuota, RecordedLoss};

#[test]
fn partial_batch_loss_is_atomic_and_survives_reopen() {
    let directory = std::env::temp_dir().join(format!("ubm-recorded-loss-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("journal.sqlite");
    let quota = JournalQuota {
        max_bytes: 1 << 20,
        max_records: 2,
    };
    let journal = ContinuationJournal::open(&path, "loss", &json!({}), quota).unwrap();
    let metadata = json!({});
    let records = [json!({"v":1}), json!({"v":2}), json!({"v":3})];
    let entries: Vec<_> = records.iter().map(|record| (&metadata, record)).collect();
    let impacts = [
        RecordedLoss {
            items: 1,
            bytes: Some(2),
        },
        RecordedLoss {
            items: 1,
            bytes: Some(3),
        },
        RecordedLoss {
            items: 1,
            bytes: Some(4),
        },
    ];
    let batch = journal
        .append_recorded_batch(&entries, &impacts, RecordedLoss::default())
        .unwrap();
    assert_eq!(batch.accepted, 2);
    assert!(batch.rejected.is_some());
    let later = json!({"v":4});
    assert!(
        journal
            .append_recorded_batch(
                &[(&metadata, &later)],
                &[RecordedLoss {
                    items: 1,
                    bytes: Some(5)
                }],
                RecordedLoss::default()
            )
            .is_err()
    );
    drop(journal);
    let journal = ContinuationJournal::open(&path, "loss", &json!({}), quota).unwrap();
    let status = journal.status().unwrap();
    assert_eq!(status["lostRecords"], 2);
    assert_eq!(
        status["collectionFailure"]["knownRecordedLoss"],
        json!({"items":2,"bytes":9})
    );
    assert_eq!(status["collectionFailure"]["persisted"], true);
    drop(journal);
    std::fs::remove_dir_all(directory).unwrap();
}
