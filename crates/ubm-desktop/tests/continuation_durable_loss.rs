use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use ubm_desktop::continuation_journal::{ContinuationJournal, JournalQuota};
use ubm_desktop::continuation_outbox::{CONTROL_RECORD_CAP, IngressClass, Outbox, WakeSink};

struct NoWake;
impl WakeSink for NoWake {
    fn wake(&self, _: u64) {}
}
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ubm-durable-loss-{}-{epoch}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn journal(&self, max_records: u64) -> Arc<ContinuationJournal> {
        Arc::new(
            ContinuationJournal::open(
                &self.0.join("r.sqlite"),
                "r",
                &json!({}),
                JournalQuota {
                    max_bytes: 16 * 1024 * 1024,
                    max_records,
                },
            )
            .unwrap(),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove isolated journal fixture");
    }
}
fn attach(journal: &Arc<ContinuationJournal>) -> Outbox {
    let outbox = Outbox::new(1, Arc::new(NoWake));
    outbox
        .attach_journal(
            journal.clone(),
            json!({"peerId":"peer-a", "sessionId":"review"}),
        )
        .unwrap();
    outbox
}
fn loss(batch: &Value, class: &str) -> u64 {
    batch["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row["record"])
        .filter(|record| record["t"] == "ingress-drop" && record["class"] == class)
        .map(|record| record["count"].as_u64().unwrap())
        .sum()
}

#[test]
fn durable_loss_matches_coalesced_memory_loss() {
    let fixture = Fixture::new();
    let journal = fixture.journal(10000);
    let outbox = attach(&journal);
    outbox.push_ingress_drop_count(IngressClass::Control, 3);
    outbox.push_ingress_drop_count(IngressClass::Control, 5);
    let memory = outbox.drain(2048, 4194304);
    assert_eq!(memory["records"][0]["count"], 8);
    assert_eq!(memory["records"].as_array().unwrap().len(), 1);
    assert_eq!(loss(&journal.prepare(2048, 4194304).unwrap(), "control"), 8);
}

#[test]
fn prepared_loss_prefix_is_immutable_and_later_delta_survives_reopen() {
    let fixture = Fixture::new();
    let journal = fixture.journal(10000);
    let outbox = attach(&journal);
    outbox.push_ingress_drop_count(IngressClass::Control, 3);
    let first = journal.prepare(2048, 4194304).unwrap();
    outbox.push_ingress_drop_count(IngressClass::Control, 5);
    assert_eq!(journal.prepare(2048, 4194304).unwrap(), first);
    drop(outbox);
    drop(journal);
    let journal = fixture.journal(10000);
    assert_eq!(journal.prepare(2048, 4194304).unwrap(), first);
    journal
        .acknowledge(first["token"].as_str().unwrap())
        .unwrap();
    assert_eq!(loss(&journal.prepare(2048, 4194304).unwrap(), "control"), 5);
}

#[test]
fn full_memory_control_queue_does_not_suppress_durable_loss() {
    let fixture = Fixture::new();
    let journal = fixture.journal(10000);
    let outbox = attach(&journal);
    for _ in 0..CONTROL_RECORD_CAP {
        outbox.push_control(json!({"t":"ingress-drop", "class":"advertisement", "count":1}));
    }
    outbox.push_ingress_drop_count(IngressClass::Control, 7);
    assert_eq!(loss(&journal.prepare(2048, 4194304).unwrap(), "control"), 7);
    assert_eq!(outbox.drain(2048, 4194304)["controlLost"], 1);
}

#[test]
fn single_and_mixed_loss_deltas_survive_reopen_and_seal() {
    let fixture = Fixture::new();
    let journal = fixture.journal(10000);
    let outbox = attach(&journal);
    outbox.push_ingress_drop(IngressClass::Control);
    outbox.push_ingress_drop(IngressClass::Control);
    outbox.push_ingress_drop_count(IngressClass::Advertisement, 9);
    outbox.push_ingress_drop(IngressClass::Advertisement);
    outbox.push_ingress_drop_count(IngressClass::Control, 0);
    outbox.seal();
    outbox.push_ingress_drop_count(IngressClass::Control, 12);
    drop(outbox);
    drop(journal);
    let batch = fixture.journal(10000).prepare(2048, 4194304).unwrap();
    assert_eq!(loss(&batch, "control"), 2);
    assert_eq!(loss(&batch, "advertisement"), 10);
}

#[test]
fn coalesced_loss_quota_failure_is_reported_and_retained() {
    let fixture = Fixture::new();
    let journal = fixture.journal(1);
    let outbox = attach(&journal);
    outbox.push_ingress_drop_count(IngressClass::Control, 3);
    outbox.push_ingress_drop_count(IngressClass::Control, 5);
    assert_eq!(outbox.journal_failure().unwrap().kind, "storage.full");
    assert_eq!(journal.status().unwrap()["accepting"], false);
    assert_eq!(loss(&journal.prepare(2048, 4194304).unwrap(), "control"), 3);
    assert_eq!(outbox.drain(2048, 4194304)["records"][0]["count"], 8);
}

#[test]
fn coalesced_loss_failed_append_exposes_storage_failure() {
    let fixture = Fixture::new();
    let journal = fixture.journal(10000);
    let outbox = attach(&journal);
    outbox.push_ingress_drop_count(IngressClass::Control, 3);
    let blocker = rusqlite::Connection::open(fixture.0.join("r.sqlite")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    outbox.push_ingress_drop_count(IngressClass::Control, 5);
    let failure = outbox.journal_failure();
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(failure.unwrap().kind, "storage.busy");
    assert_eq!(journal.status().unwrap()["accepting"], false);
    assert_eq!(loss(&journal.prepare(2048, 4194304).unwrap(), "control"), 3);
}

#[test]
fn attachment_and_loss_admission_have_one_order() {
    for _ in 0..16 {
        let fixture = Fixture::new();
        let journal = fixture.journal(10000);
        let outbox = Arc::new(Outbox::new(1, Arc::new(NoWake)));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let attaching = {
            let outbox = outbox.clone();
            let journal = journal.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                outbox.attach_journal(journal, json!({"peerId":"peer-a"}))
            })
        };
        let reporting = {
            let outbox = outbox.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                outbox.push_ingress_drop_count(IngressClass::Control, 3);
            })
        };
        barrier.wait();
        attaching
            .join()
            .unwrap()
            .expect("undelivered controls may precede attachment");
        reporting.join().unwrap();
        assert_eq!(outbox.drain(2048, 4194304)["records"][0]["count"], 3);
        assert_eq!(loss(&journal.prepare(2048, 4194304).unwrap(), "control"), 3);
    }
}

#[test]
fn attachment_replays_undelivered_loss_and_peer_scoped_controls_once() {
    let fixture = Fixture::new();
    let journal = fixture.journal(10000);
    let outbox = Outbox::new(1, Arc::new(NoWake));
    outbox.push_ingress_drop_count(IngressClass::Control, 3);
    outbox.push_ingress_drop_count(IngressClass::Control, 5);
    outbox.push_control(json!({"t":"link","peerId":"peer-b","reason":"remote"}));
    outbox.push_control(json!({"t":"link","peerId":"peer-a","reason":"remote"}));
    outbox
        .attach_journal(journal.clone(), json!({"peerId":"peer-a"}))
        .unwrap();
    let batch = journal.prepare(2048, 4194304).unwrap();
    assert_eq!(loss(&batch, "control"), 8);
    assert_eq!(batch["records"].as_array().unwrap().len(), 2);
    assert_eq!(batch["records"][1]["record"]["peerId"], "peer-a");
    assert_eq!(
        outbox.drain(2048, 4194304)["records"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn attachment_retains_full_queue_loss_without_fabricating_upstream_counts() {
    let fixture = Fixture::new();
    let journal = fixture.journal(10000);
    let outbox = Outbox::new(1, Arc::new(NoWake));
    for _ in 0..CONTROL_RECORD_CAP + 7 {
        outbox.push_control(json!({"t":"lifecycle","event":{"kind":"adapter-state"}}));
    }
    outbox
        .attach_journal(journal.clone(), json!({"peerId":"peer-a"}))
        .unwrap();
    let batch = journal.prepare(2048, 4194304).unwrap();
    assert_eq!(loss(&batch, "control"), 7);
    assert_eq!(
        batch["records"].as_array().unwrap().len(),
        CONTROL_RECORD_CAP + 1
    );
    assert_eq!(outbox.drain(2048, 4194304)["controlLost"], 7);
}

#[test]
fn attachment_refuses_data_delivered_controls_and_consumer_controls() {
    for case in ["data", "delivered", "consumer", "sealed"] {
        let fixture = Fixture::new();
        let journal = fixture.journal(10000);
        let outbox = Outbox::new(1, Arc::new(NoWake));
        match case {
            "data" => outbox
                .push_data(json!({"t":"adv","peerId":"peer-a"}))
                .unwrap(),
            "delivered" => {
                outbox.push_control(json!({"t":"lifecycle"}));
                assert_eq!(
                    outbox.drain(1, 4096)["records"].as_array().unwrap().len(),
                    1
                );
            }
            "consumer" => {
                outbox.push_control(json!({"t":"stream-end","consumer":"c","reason":"closed"}))
            }
            "sealed" => {
                outbox.seal();
            }
            _ => unreachable!(),
        }
        assert!(
            outbox
                .attach_journal(journal.clone(), json!({"peerId":"peer-a"}))
                .is_err(),
            "{case}"
        );
        assert!(!outbox.has_journal());
        assert_eq!(journal.status().unwrap()["records"], 0);
    }
}

#[test]
fn attachment_reports_queued_control_quota_failure_and_retains_committed_prefix() {
    let fixture = Fixture::new();
    let journal = fixture.journal(1);
    let outbox = Outbox::new(1, Arc::new(NoWake));
    outbox.push_ingress_drop_count(IngressClass::Control, 3);
    outbox.push_ingress_drop_count(IngressClass::Advertisement, 5);
    let failure = outbox
        .attach_journal(journal.clone(), json!({"peerId":"peer-a"}))
        .unwrap_err();
    assert_eq!(failure.kind, "storage.full");
    assert!(!outbox.has_journal());
    assert_eq!(journal.status().unwrap()["accepting"], false);
    assert_eq!(
        journal.status().unwrap()["collectionFailure"]["kind"],
        "storage.full"
    );
    assert_eq!(loss(&journal.prepare(2048, 4194304).unwrap(), "control"), 3);
    assert_eq!(
        outbox.drain(2048, 4194304)["records"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn durable_values_between_loss_reports_do_not_hide_a_delta() {
    let fixture = Fixture::new();
    let journal = fixture.journal(10000);
    let outbox = attach(&journal);
    outbox
        .register_journal_consumer("c", json!({"peerId":"peer-a", "generation":1}))
        .unwrap();
    outbox.push_ingress_drop_count(IngressClass::Control, 3);
    outbox
        .push_data(json!({"t":"value", "consumer":"c", "valueB64":"AQ=="}))
        .unwrap();
    outbox.push_ingress_drop_count(IngressClass::Control, 5);
    assert_eq!(loss(&journal.prepare(2048, 4194304).unwrap(), "control"), 8);
    assert_eq!(outbox.drain(2048, 4194304)["records"][0]["count"], 8);
}

#[test]
fn loss_exit_worker() {
    let Some(path) = std::env::var_os("UBM_DURABLE_LOSS_EXIT_PATH") else {
        return;
    };
    let fixture = Fixture(PathBuf::from(path));
    let journal = fixture.journal(10000);
    let outbox = attach(&journal);
    outbox.push_ingress_drop_count(IngressClass::Control, 3);
    outbox.push_ingress_drop_count(IngressClass::Control, 5);
    // No journal/outbox destructor can flush a memory-only tail on this exit.
    std::process::exit(0);
}

#[test]
fn coalesced_loss_survives_process_exit_without_destructor_flush() {
    let fixture = Fixture::new();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "loss_exit_worker"])
        .env("UBM_DURABLE_LOSS_EXIT_PATH", &fixture.0)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        loss(
            &fixture.journal(10000).prepare(2048, 4194304).unwrap(),
            "control"
        ),
        8
    );
}
