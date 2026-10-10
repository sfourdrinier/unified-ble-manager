//! Bounded group commit: values the pump already holds share one synced SQLite
//! transaction without changing a single journal row, counter, ordering or
//! failure/loss account relative to committing them one at a time.
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use ubm_desktop::continuation_journal::{
    APPEND_BATCH_MAX, AppendBatch, ContinuationJournal, JournalQuota,
};
use ubm_desktop::continuation_outbox::{
    AfterCutoffLoss, DATA_RECORD_BYTES, DATA_RECORD_CAP, DataIngressFailure, Outbox, WakeSink,
};

struct NoWake;
impl WakeSink for NoWake {
    fn wake(&self, _: u64) {}
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
fn path() -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "ubm-group-commit-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    directory.join("recording.sqlite")
}
fn quota(max_records: u64) -> JournalQuota {
    JournalQuota {
        max_bytes: 1 << 20,
        max_records,
    }
}
fn open(path: &std::path::Path, max_records: u64) -> ContinuationJournal {
    ContinuationJournal::open(
        path,
        "group",
        &json!({"onAppearance":"native"}),
        quota(max_records),
    )
    .unwrap()
}
/// A journal plus the file it owns, so identity checks read the stored rows
/// with an independent connection instead of the journal's own cursor API.
struct Fixture {
    path: PathBuf,
    journal: Arc<ContinuationJournal>,
}
impl std::ops::Deref for Fixture {
    type Target = ContinuationJournal;
    fn deref(&self) -> &ContinuationJournal {
        &self.journal
    }
}
fn fixture(max_records: u64) -> Fixture {
    let path = path();
    let journal = Arc::new(open(&path, max_records));
    Fixture { path, journal }
}
/// Every stored byte that is not a retained diagnostic: the journal row
/// (ordinals, tokens, loss, counters, phase) and each record row.
fn stored(fixture: &Fixture) -> Value {
    let reader = rusqlite::Connection::open(&fixture.path).unwrap();
    let cursor: Vec<Value> = reader
        .query_row(
            "SELECT phase,next_ordinal,next_token,lost,pending_token,pending_last,\
             pending_more,last_ack_token,last_ack_receipt,retained_records,retained_bytes,\
             collection_failure FROM journal WHERE id=1",
            [],
            |row| {
                (0..12)
                    .map(|index| match row.get_ref(index)? {
                        rusqlite::types::ValueRef::Null => Ok(Value::Null),
                        rusqlite::types::ValueRef::Integer(value) => Ok(json!(value)),
                        rusqlite::types::ValueRef::Text(value) => {
                            Ok(json!(String::from_utf8_lossy(value)))
                        }
                        _ => Err(rusqlite::Error::InvalidQuery),
                    })
                    .collect()
            },
        )
        .unwrap();
    let mut statement = reader
        .prepare("SELECT ordinal,body,bytes FROM records ORDER BY ordinal")
        .unwrap();
    let records: Vec<Value> = statement
        .query_map([], |row| {
            Ok(json!([
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?
            ]))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    json!({"cursor":cursor,"records":records})
}
/// What `status` reports, without the call-scoped failure diagnostic.
fn counters(journal: &ContinuationJournal) -> Value {
    let mut status = journal.status().unwrap();
    status["runtimeFailure"] = Value::Null;
    status
}
fn row_count(fixture: &Fixture) -> usize {
    stored(fixture)["records"].as_array().unwrap().len()
}
fn context(index: usize) -> Value {
    json!({"session":{"epoch":"a","peerId":"p"},"consumer":{"selector":index % 3}})
}
/// Mixed sizes so batch boundaries cut across pages and overflow chains.
fn record(index: usize) -> Value {
    let padding = "x".repeat(match index % 4 {
        0 => 0,
        1 => 700,
        2 => 3000,
        _ => 9000,
    });
    json!({"t":"value","consumer":"c","index":index,"valueB64":padding})
}

fn sequential(journal: &Fixture, from: usize, count: usize) -> Vec<Value> {
    (from..from + count)
        .map(|index| journal.append(&context(index), &record(index)).unwrap())
        .collect()
}
fn batched(journal: &Fixture, from: usize, count: usize, group: usize) -> Vec<AppendBatch> {
    (from..from + count)
        .collect::<Vec<_>>()
        .chunks(group)
        .map(|indexes| {
            let owned: Vec<(Value, Value)> = indexes
                .iter()
                .map(|index| (context(*index), record(*index)))
                .collect();
            let items: Vec<(&Value, &Value)> = owned.iter().map(|(c, r)| (c, r)).collect();
            journal.append_batch(&items).unwrap()
        })
        .collect()
}

#[test]
fn grouped_commit_is_row_for_row_identical_to_sequential_append() {
    let one = fixture(1000);
    let many = fixture(1000);
    let accepted = sequential(&one, 0, 75);
    let batches = batched(&many, 0, 75, APPEND_BATCH_MAX);
    assert_eq!(
        batches
            .iter()
            .map(|batch| batch.accepted)
            .collect::<Vec<_>>(),
        [32, 32, 11]
    );
    assert!(batches.iter().all(|batch| batch.rejected.is_none()));
    // Same ordinal the single-record API reported, consecutively.
    assert_eq!(
        batches
            .iter()
            .flat_map(|batch| batch.first_ordinal..batch.first_ordinal + batch.accepted as i64)
            .collect::<Vec<_>>(),
        accepted
            .iter()
            .map(|answer| answer["ordinal"].as_i64().unwrap())
            .collect::<Vec<_>>()
    );
    assert_eq!(stored(&one), stored(&many));
    assert_eq!(counters(&one), counters(&many));
    // Prepare tokens/receipts and the cursor they leave behind agree too.
    let (prepared_one, prepared_many) = (
        one.prepare(40, 4_194_304).unwrap(),
        many.prepare(40, 4_194_304).unwrap(),
    );
    assert_eq!(prepared_one, prepared_many);
    let token = prepared_one["token"].as_str().unwrap();
    assert_eq!(
        one.acknowledge(token).unwrap(),
        many.acknowledge(token).unwrap()
    );
    assert_eq!(stored(&one), stored(&many));
    assert_eq!(counters(&one), counters(&many));
    // Appends after an acknowledgement continue the same ordinals.
    sequential(&one, 75, 5);
    batched(&many, 75, 5, 3);
    assert_eq!(stored(&one), stored(&many));
    assert_eq!(counters(&one), counters(&many));
}

#[test]
fn append_is_the_one_element_case_of_a_batch() {
    let journal = fixture(10);
    let (metadata, value) = (context(0), record(0));
    let answer = journal.append(&metadata, &value).unwrap();
    assert_eq!(answer, json!({"accepted":true,"ordinal":1}));
    let batch = journal.append_batch(&[(&metadata, &value)]).unwrap();
    assert_eq!((batch.accepted, batch.first_ordinal), (1, 2));
    assert!(batch.rejected.is_none());
}

#[test]
fn record_capacity_mid_batch_commits_prefix_and_first_loss_marker_atomically() {
    let one = fixture(10);
    let many = fixture(10);
    for index in 0..4 {
        one.append(&context(index), &record(index)).unwrap();
        many.append(&context(index), &record(index)).unwrap();
    }
    // Sequential admission stops at its first refusal.
    let mut refused = None;
    for index in 4..20 {
        if let Err(failure) = one.append(&context(index), &record(index)) {
            refused = Some((index, failure));
            break;
        }
    }
    let (cut, failure) = refused.unwrap();
    assert_eq!((cut, failure.kind), (10, "storage.full"));
    let owned: Vec<(Value, Value)> = (4..20).map(|i| (context(i), record(i))).collect();
    let items: Vec<(&Value, &Value)> = owned.iter().map(|(c, r)| (c, r)).collect();
    let batch = many
        .append_batch(&items[..16.min(APPEND_BATCH_MAX)])
        .unwrap();
    assert_eq!(batch.accepted, 6);
    assert_eq!(batch.first_ordinal, 5);
    let rejected = batch.rejected.unwrap();
    assert_eq!(rejected.error.kind, "storage.full");
    assert_eq!(rejected.error.operation, "append");
    // Records after the first refused one were never admitted.
    assert_eq!(rejected.not_attempted, 16 - 6 - 1);
    assert_eq!(stored(&one), stored(&many));
    assert_eq!(counters(&one), counters(&many));
    let status = many.status().unwrap();
    assert_eq!(status["phase"], "capacity-reached");
    assert_eq!(status["records"], 10);
    assert_eq!(status["lostRecords"], 1);
    // A later group is refused whole with one more recorded loss, as for
    // a single append once capacity was reached.
    let later = many.append_batch(&items[..3]).unwrap();
    assert_eq!(later.accepted, 0);
    assert_eq!(later.rejected.unwrap().not_attempted, 2);
    assert_eq!(many.status().unwrap()["lostRecords"], 2);
    assert!(one.append(&context(0), &record(0)).is_err());
    assert_eq!(stored(&one), stored(&many));
    assert_eq!(counters(&one), counters(&many));
}

#[test]
fn byte_capacity_cuts_identically_to_sequential_admission() {
    // Large bodies make the page reservation, not the record count, decide.
    let big = |index: usize| json!({"t":"value","index":index,"blob":"y".repeat(60_000)});
    let one = fixture(1000);
    let many = fixture(1000);
    let mut sequential_accepted = 0;
    for index in 0..40 {
        if one.append(&json!({}), &big(index)).is_err() {
            break;
        }
        sequential_accepted += 1;
    }
    assert!(sequential_accepted > 4 && sequential_accepted < 32);
    let owned: Vec<(Value, Value)> = (0..32).map(|i| (json!({}), big(i))).collect();
    let items: Vec<(&Value, &Value)> = owned.iter().map(|(c, r)| (c, r)).collect();
    let batch = many.append_batch(&items).unwrap();
    assert_eq!(batch.accepted, sequential_accepted);
    assert_eq!(
        batch.rejected.unwrap().not_attempted,
        32 - sequential_accepted - 1
    );
    assert_eq!(stored(&one), stored(&many));
    assert_eq!(counters(&one), counters(&many));
    assert_eq!(many.status().unwrap()["lostRecords"], 1);
}

/// SQLite can fail in the middle of a transaction; a trigger makes that
/// failure deterministic without touching production code.
fn fail_insert_of_ordinal(path: &std::path::Path, ordinal: i64) -> rusqlite::Connection {
    let injector = rusqlite::Connection::open(path).unwrap();
    injector
        .execute_batch(&format!(
            "CREATE TRIGGER injected BEFORE INSERT ON records WHEN NEW.ordinal={ordinal} \
             BEGIN SELECT RAISE(ABORT,'injected storage failure'); END;"
        ))
        .unwrap();
    injector
}

#[test]
fn storage_failure_rolls_back_the_whole_group_and_keeps_the_precise_error() {
    let journal = fixture(100);
    let path = journal.path.clone();
    batched(&journal, 0, 2, 2);
    let before = stored(&journal);
    let injector = fail_insert_of_ordinal(&path, 5);
    let owned: Vec<(Value, Value)> = (2..8).map(|i| (context(i), record(i))).collect();
    let items: Vec<(&Value, &Value)> = owned.iter().map(|(c, r)| (c, r)).collect();
    let failure = journal.append_batch(&items).unwrap_err();
    assert_eq!(failure.kind, "storage.io");
    assert_eq!(failure.operation, "append");
    assert!(failure.sqlite_code.is_some());
    // Ordinals 3 and 4 were inserted before the abort: none may survive.
    assert_eq!(stored(&journal), before);
    let status = journal.status().unwrap();
    assert_eq!(status["records"], 2);
    assert_eq!(status["lostRecords"], 0);
    assert_eq!(status["phase"], "recording");
    // The precise cause is retained as uncommitted failure evidence.
    assert_eq!(status["runtimeFailure"]["kind"], "storage.io");
    assert_eq!(status["runtimeFailure"]["persisted"], false);
    injector.execute_batch("DROP TRIGGER injected").unwrap();
    // Nothing advanced: the next group is still ordinal 3, consecutive.
    let again = journal.append_batch(&items).unwrap();
    assert_eq!((again.accepted, again.first_ordinal), (6, 3));
}

#[test]
fn invalid_empty_oversized_and_stopped_groups_change_nothing() {
    let journal = fixture(100);
    sequential(&journal, 0, 2);
    let before = stored(&journal);
    assert_eq!(
        journal.append_batch(&[]).unwrap_err().kind,
        "argument.invalid"
    );
    let owned: Vec<(Value, Value)> = (0..=APPEND_BATCH_MAX)
        .map(|i| (context(i), record(i)))
        .collect();
    let items: Vec<(&Value, &Value)> = owned.iter().map(|(c, r)| (c, r)).collect();
    assert_eq!(
        journal.append_batch(&items).unwrap_err().kind,
        "argument.invalid"
    );
    // One bad record fails the group before any row is written.
    let (metadata, value) = (context(9), record(9));
    let invalid = json!("not an object");
    assert_eq!(
        journal
            .append_batch(&[(&metadata, &value), (&metadata, &invalid)])
            .unwrap_err()
            .kind,
        "argument.invalid"
    );
    assert_eq!(stored(&journal), before);
    journal.stop().unwrap();
    let stopped = journal.append_batch(&[(&metadata, &value)]).unwrap_err();
    assert_eq!(stopped.kind, "storage.stopped");
    let status = journal.status().unwrap();
    assert_eq!(status["records"], 2);
    assert_eq!(status["lostRecords"], 0);
}

// ---- outbox boundary: observation, loss accounting and bounded commits ----

fn durable(max_records: u64) -> (Fixture, Outbox) {
    let journal = fixture(max_records);
    let outbox = Outbox::new(1, Arc::new(NoWake));
    outbox
        .attach_journal(journal.journal.clone(), json!({"epoch":"a","peerId":"p"}))
        .unwrap();
    outbox
        .register_journal_consumer("c", json!({"generation":"g"}))
        .unwrap();
    (journal, outbox)
}
fn value(index: usize) -> Value {
    json!({"t":"value","consumer":"c","valueB64":format!("v{index}"),"delivery":"notification"})
}
fn bytes(index: usize) -> u64 {
    value(index).to_string().len() as u64
}

#[test]
fn outbox_group_equals_single_pushes_in_rows_counters_and_observation() {
    let (one, single) = durable(1000);
    let (many, grouped) = durable(1000);
    let mut observed_one = single
        .observe("c", Arc::new(|record| record["valueB64"] == "v50"))
        .unwrap();
    let mut observed_many = grouped
        .observe("c", Arc::new(|record| record["valueB64"] == "v50"))
        .unwrap();
    for index in 0..90 {
        single.push_data(value(index)).unwrap();
    }
    let outcome = grouped.push_data_batch((0..90).map(value).collect());
    assert_eq!((outcome.accepted, outcome.rejected), (90, None));
    assert_eq!(stored(&one), stored(&many));
    assert_eq!(counters(&one), counters(&many));
    assert_eq!(
        observed_one.receiver.try_recv().unwrap(),
        observed_many.receiver.try_recv().unwrap()
    );
    assert_eq!(single.after_cutoff_loss(), grouped.after_cutoff_loss());
    assert_eq!(single.drain(2048, 1 << 20), grouped.drain(2048, 1 << 20));
}

#[test]
fn longer_groups_commit_in_bounded_transactions() {
    let (journal, outbox) = durable(1000);
    // One BEFORE-UPDATE audit row per cursor update == one commit per group.
    let audit = rusqlite::Connection::open(&journal.path).unwrap();
    audit
        .execute_batch(
            "CREATE TABLE commits(n INTEGER); \
             CREATE TRIGGER counted AFTER UPDATE OF next_ordinal ON journal \
             BEGIN INSERT INTO commits VALUES(NEW.next_ordinal); END;",
        )
        .unwrap();
    let outcome = outbox.push_data_batch((0..70).map(value).collect());
    assert_eq!(outcome.accepted, 70);
    let commits: i64 = audit
        .query_row("SELECT count(*) FROM commits", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        commits, 3,
        "70 held values are 32 + 32 + 6, never 70 commits"
    );
    assert_eq!(journal.status().unwrap()["records"], 71);
    assert_eq!(row_count(&journal), 71);
}

#[test]
fn observation_follows_commit_in_order_and_sees_every_row_durable() {
    let (journal, outbox) = durable(1000);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let probe = journal.journal.clone();
    let record = seen.clone();
    let mut observation = outbox
        .observe(
            "c",
            Arc::new(move |candidate| {
                // The matcher is evaluated only after the whole group committed.
                record.lock().unwrap().push((
                    candidate["valueB64"].as_str().unwrap().to_owned(),
                    probe.status().unwrap()["records"].as_u64().unwrap(),
                ));
                candidate["valueB64"] == "v2"
            }),
        )
        .unwrap();
    let outcome = outbox.push_data_batch((0..5).map(value).collect());
    assert_eq!(outcome.accepted, 5);
    assert_eq!(observation.receiver.try_recv().unwrap()["valueB64"], "v2");
    // Registration + all 5 rows were durable before the first match call, and
    // matching stopped at the first hit.
    assert_eq!(
        *seen.lock().unwrap(),
        [
            ("v0".to_owned(), 6),
            ("v1".to_owned(), 6),
            ("v2".to_owned(), 6)
        ]
    );
}

#[test]
fn outbox_capacity_commits_prefix_observes_it_and_accounts_every_uncommitted_value() {
    // Registration + 4 values fit.
    let (one, single) = durable(5);
    let (many, grouped) = durable(5);
    for index in 0..7 {
        if single.push_data(value(index)).is_err() {
            break;
        }
    }
    let mut observation = grouped
        .observe("c", Arc::new(|record| record["valueB64"] == "v3"))
        .unwrap();
    let outcome = grouped.push_data_batch((0..7).map(value).collect());
    assert_eq!(outcome.accepted, 4);
    let rejected = outcome.rejected.unwrap();
    assert!(matches!(
        rejected.failure,
        DataIngressFailure::Storage { ref error, bytes: first }
            if error.kind == "storage.full" && first as u64 == bytes(4)
    ));
    // The refused value plus the two polled values behind it, once each.
    assert_eq!(rejected.items, 3);
    assert_eq!(rejected.bytes, bytes(4) + bytes(5) + bytes(6));
    // The admitted prefix was observed in order; the terminal found none left.
    assert_eq!(observation.receiver.try_recv().unwrap()["valueB64"], "v3");
    assert_eq!(stored(&one), stored(&many));
    assert_eq!(counters(&one), counters(&many));
    assert_eq!(many.status().unwrap()["lostRecords"], 1);
    assert_eq!(grouped.journal_failure().unwrap().kind, "storage.full");
    assert_eq!(single.journal_failure(), grouped.journal_failure());
    // Nothing further is admitted behind the retained failure.
    let after = grouped.push_data_batch(vec![value(9)]);
    assert_eq!(after.accepted, 0);
    assert_eq!(many.status().unwrap()["records"], 5);
}

#[test]
fn outbox_storage_failure_is_all_or_nothing_without_observation() {
    let (journal, outbox) = durable(1000);
    let before = stored(&journal);
    let mut observation = outbox
        .observe("c", Arc::new(|record| record["valueB64"] == "v0"))
        .unwrap();
    // Registration is ordinal 1; value k is ordinal k + 2. The third value's
    // insert aborts after the first two were written inside the transaction.
    let injector = fail_insert_of_ordinal(&journal.path, 4);
    let outcome = outbox.push_data_batch((0..6).map(value).collect());
    assert_eq!(
        outcome.accepted, 0,
        "a rolled-back prefix is never accepted"
    );
    let rejected = outcome.rejected.unwrap();
    let DataIngressFailure::Storage { error, .. } = rejected.failure else {
        panic!("expected a storage failure: {rejected:?}");
    };
    assert_eq!(error.kind, "storage.io");
    assert!(error.sqlite_code.is_some());
    assert_eq!(rejected.items, 6);
    assert_eq!(rejected.bytes, (0..6).map(bytes).sum::<u64>());
    // The observer got the storage terminal, never a rolled-back value.
    let terminal = observation.receiver.try_recv().unwrap();
    assert_eq!(terminal["t"], "stream-end");
    assert_eq!(terminal["reason"], "source-failed");
    assert_eq!(outbox.journal_failure().unwrap(), error);
    drop(injector);
    // No row of the rolled-back group exists and the cursor never advanced.
    let after = stored(&journal);
    assert_eq!(after["records"], before["records"]);
    assert_eq!(after["cursor"][1], before["cursor"][1]);
    let status = journal.status().unwrap();
    assert_eq!(status["records"], 1);
    assert_eq!(status["lostRecords"], 0);
    assert_eq!(status["collectionFailure"]["kind"], "storage.io");
}

#[test]
fn stopped_journal_counts_every_polled_value_after_the_cutoff() {
    let (journal, outbox) = durable(1000);
    let mut observation = outbox.observe("c", Arc::new(|_| true)).unwrap();
    // An offline handle stops the recording under this outbox.
    open(&journal.path, 1000).stop().unwrap();
    // Cross the commit boundary: every held value remains cutoff loss.
    let outcome = outbox.push_data_batch((0..70).map(value).collect());
    assert_eq!(outcome.accepted, 0);
    let rejected = outcome.rejected.unwrap();
    assert_eq!(
        rejected.failure,
        DataIngressFailure::Stopped {
            bytes: bytes(0) as usize
        }
    );
    let total = (0..70).map(bytes).sum::<u64>();
    assert_eq!((rejected.items, rejected.bytes), (70, total));
    assert_eq!(
        outbox.after_cutoff_loss(),
        AfterCutoffLoss {
            items: 70,
            bytes: total
        }
    );
    assert!(outbox.is_sealed());
    assert!(outbox.journal_failure().is_none());
    assert!(journal.status().unwrap()["collectionFailure"].is_null());
    assert_eq!(journal.status().unwrap()["records"], 1);
    // Sealing withdrew the observation without fabricating a terminal.
    assert!(observation.receiver.try_recv().is_err());
}

#[test]
fn sealed_outbox_counts_each_value_of_a_group_exactly_once() {
    let (journal, outbox) = durable(1000);
    let accepted = outbox.push_data_batch((0..3).map(value).collect());
    assert_eq!(accepted.accepted, 3);
    assert_eq!(outbox.seal(), AfterCutoffLoss { items: 0, bytes: 0 });
    let late = outbox.push_data_batch((3..8).map(value).collect());
    assert_eq!(late.accepted, 0);
    let rejected = late.rejected.unwrap();
    assert!(matches!(
        rejected.failure,
        DataIngressFailure::Sealed { .. }
    ));
    let total = (3..8).map(bytes).sum::<u64>();
    assert_eq!((rejected.items, rejected.bytes), (5, total));
    assert_eq!(
        outbox.after_cutoff_loss(),
        AfterCutoffLoss {
            items: 5,
            bytes: total
        }
    );
    // A single push after the cutoff adds exactly its own value.
    assert!(outbox.push_data(value(8)).is_err());
    assert_eq!(
        outbox.after_cutoff_loss(),
        AfterCutoffLoss {
            items: 6,
            bytes: total + bytes(8)
        }
    );
    assert_eq!(journal.status().unwrap()["records"], 4);
}

#[test]
fn volatile_queue_keeps_its_bounds_and_reports_the_accepted_prefix() {
    let outbox = Outbox::new(1, Arc::new(NoWake));
    let small = |index: usize| json!({"t":"value","consumer":"c","n":index});
    let outcome = outbox.push_data_batch((0..DATA_RECORD_CAP + 3).map(small).collect());
    assert_eq!(outcome.accepted, DATA_RECORD_CAP);
    let rejected = outcome.rejected.unwrap();
    assert_eq!(rejected.items, 3);
    assert!(matches!(
        rejected.failure,
        DataIngressFailure::Overflow { .. }
    ));
    assert_eq!(outbox.queued_data(), DATA_RECORD_CAP);
    let drained = outbox.drain(DATA_RECORD_CAP, DATA_RECORD_BYTES);
    let numbers: Vec<u64> = drained["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["n"].as_u64().unwrap())
        .collect();
    assert_eq!(numbers, (0..DATA_RECORD_CAP as u64).collect::<Vec<_>>());

    // The byte bound refuses from the first value that no longer fits.
    let huge = |index: usize| json!({"t":"value","consumer":"c","n":index,"pad":"z".repeat(DATA_RECORD_BYTES / 2)});
    let outcome = outbox.push_data_batch((0..3).map(huge).collect());
    assert_eq!(outcome.accepted, 1);
    assert_eq!(outcome.rejected.unwrap().items, 2);
}

#[test]
fn queue_overflow_stays_an_overflow_and_uncounted_when_a_seal_follows_the_refusal() {
    let outbox = Outbox::new(1, Arc::new(NoWake));
    let small = |index: usize| json!({"t":"value","consumer":"c","n":index});
    // 70 refused values span three bounded groups of the same batch.
    let outcome = outbox.push_data_batch((0..DATA_RECORD_CAP + 70).map(small).collect());
    let rejected = outcome.rejected.unwrap();
    assert!(matches!(
        rejected.failure,
        DataIngressFailure::Overflow { .. }
    ));
    assert!(!rejected.failure.after_cutoff());
    assert_eq!(rejected.items, 70);
    // The cutoff arrives after the refusal; it changes neither the answer
    // already given nor where those values are counted.
    assert_eq!(outbox.seal(), AfterCutoffLoss { items: 0, bytes: 0 });
    assert_eq!(
        outbox.after_cutoff_loss(),
        AfterCutoffLoss { items: 0, bytes: 0 }
    );
}

#[test]
fn sealed_refusal_names_the_cutoff_and_counts_a_long_batch_once() {
    let outbox = Outbox::new(1, Arc::new(NoWake));
    outbox.seal();
    let outcome = outbox.push_data_batch((0..70).map(value).collect());
    assert_eq!(outcome.accepted, 0);
    let rejected = outcome.rejected.unwrap();
    assert!(matches!(
        rejected.failure,
        DataIngressFailure::Sealed { .. }
    ));
    assert!(rejected.failure.after_cutoff());
    let total = (0..70).map(bytes).sum::<u64>();
    assert_eq!((rejected.items, rejected.bytes), (70, total));
    assert_eq!(
        outbox.after_cutoff_loss(),
        AfterCutoffLoss {
            items: 70,
            bytes: total
        },
        "every value of the batch, tail groups included, once"
    );
}
