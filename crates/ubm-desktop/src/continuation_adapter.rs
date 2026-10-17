//! Continuation session adapter over the host's existing central. This owns
//! only a lease, consumer routes and their shared bounded outbox, never a radio.
use crate::continuation::{ContinuationFuture, ContinuationHost, ContinuationSession, Result};
use crate::continuation_outbox::{IngressClass, Outbox, WakeSink, encode_base64};
use crate::{
    ConnectionState, DatabaseState, DesktopCentral, DesktopError, NotificationPoll, OpControl,
    PathSelector, RadioBoundary,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use tokio::sync::Mutex;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
mod ingress_tests {
    use super::*;
    struct ThreadWake(std::sync::Mutex<Vec<std::thread::ThreadId>>);
    impl WakeSink for ThreadWake {
        fn wake(&self, _: u64) {
            self.0.lock().unwrap().push(std::thread::current().id());
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn journal_attachment_during_suspended_ingress_stays_off_runtime() {
        let runtime_thread = std::thread::current().id();
        let central = DesktopCentral::open(crate::FakeRadio::new(), "attachment-race")
            .await
            .unwrap();
        let wake = Arc::new(ThreadWake(std::sync::Mutex::new(Vec::new())));
        let session = Arc::new(Session {
            central,
            lease: "test".into(),
            peer: Mutex::new(None),
            routes: Mutex::new(vec![Route {
                peer: "absent".into(),
                consumer: "c".into(),
                live: true,
                delivery: "unknown",
                selector: PathSelector {
                    service_uuid: "180d".into(),
                    service_occurrence: None,
                    characteristic_uuid: Some("2a37".into()),
                    characteristic_occurrence: None,
                    descriptor_uuid: None,
                    descriptor_occurrence: None,
                },
            }]),
            cursor: AtomicUsize::new(0),
            outbox: Outbox::new(1, wake.clone()),
            last_error: Mutex::new(None),
        });
        let routes = session.routes.lock().await;
        let pass = session.collect_pass();
        tokio::pin!(pass);
        tokio::select! { biased;
            result = &mut pass => panic!("held routes unexpectedly completed: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        let directory = std::env::temp_dir().join(format!(
            "ubm-attachment-race-{}-{}",
            std::process::id(),
            NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let journal = Arc::new(
            crate::continuation_journal::ContinuationJournal::open(
                &directory.join("recording.sqlite"),
                "race",
                &json!({}),
                crate::continuation_journal::JournalQuota {
                    max_bytes: 1024 * 1024,
                    max_records: 10,
                },
            )
            .unwrap(),
        );
        session
            .outbox
            .attach_journal(journal, json!({"epoch":"test"}))
            .unwrap();
        drop(routes);
        pass.await.unwrap();
        let threads = wake.0.lock().unwrap();
        assert!(
            !threads.is_empty(),
            "terminal ingress must really be delivered"
        );
        assert!(
            threads.iter().all(|thread| *thread != runtime_thread),
            "a pass admitted before attachment committed on the runtime thread"
        );
    }
}

#[cfg(test)]
mod group_commit_tests {
    //! The production `Session` over a `FakeRadio` central, driven without the
    //! background collector so each backlog and every pass is deterministic.
    use super::*;
    use crate::continuation_journal::{ContinuationJournal, JournalQuota};
    use crate::{CharacteristicSnapshot, FakeRadio, PropertyFlags, RadioEvent, ServiceSnapshot};
    use std::path::PathBuf;
    use std::time::Duration;

    const PEER: &str = "AA:BB:CC:DD:EE:FF";
    const SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
    const FIRST: &str = "00002a37-0000-1000-8000-00805f9b34fb";
    const SECOND: &str = "00002a38-0000-1000-8000-00805f9b34fb";

    struct Harness {
        central: DesktopCentral<FakeRadio>,
        session: Arc<Session<FakeRadio>>,
        path: Option<PathBuf>,
    }

    impl Harness {
        async fn new(max_records: Option<u64>) -> Self {
            let radio = FakeRadio::new();
            radio.set_services(
                PEER,
                vec![ServiceSnapshot {
                    primary: None,
                    included_services: None,
                    uuid: SERVICE.into(),
                    occurrence: 0,
                    characteristics: [FIRST, SECOND]
                        .into_iter()
                        .map(|uuid| CharacteristicSnapshot {
                            uuid: uuid.into(),
                            occurrence: 0,
                            properties: PropertyFlags {
                                notify: true,
                                indicate: false,
                                read: true,
                                write: true,
                                write_without_response: false,
                            },
                            descriptors: vec![],
                        })
                        .collect(),
                    access: std::default::Default::default(),
                }],
            );
            let central = DesktopCentral::open(radio, "group-commit").await.unwrap();
            let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
            let session = Arc::new(Session {
                central: central.clone(),
                lease: format!("group-commit-{id}"),
                peer: Mutex::new(None),
                routes: Mutex::new(Vec::new()),
                cursor: AtomicUsize::new(0),
                outbox: Outbox::new(id, Arc::new(NoWake)),
                last_error: Mutex::new(None),
            });
            let path = match max_records {
                Some(max_records) => {
                    let directory = std::env::temp_dir()
                        .join(format!("ubm-adapter-group-{}-{id}", std::process::id()));
                    std::fs::create_dir(&directory).unwrap();
                    let path = directory.join("recording.sqlite");
                    let journal = Arc::new(
                        ContinuationJournal::open(
                            &path,
                            "group",
                            &json!({}),
                            JournalQuota {
                                max_bytes: 1 << 20,
                                max_records,
                            },
                        )
                        .unwrap(),
                    );
                    session
                        .outbox
                        .attach_journal(journal, json!({"epoch":"test"}))
                        .unwrap();
                    Some(path)
                }
                None => None,
            };
            for (operation, args) in [
                ("connection.connect", json!({"peerId":PEER})),
                ("gatt.discover", json!({"peerId":PEER})),
            ] {
                session.invoke(operation, args).await.unwrap();
            }
            Self {
                central,
                session,
                path,
            }
        }

        fn register(&self, consumer: &str) {
            self.session
                .outbox
                .register_journal_consumer(consumer, json!({"generation":"g"}))
                .unwrap();
        }

        async fn subscribe(&self, consumer: &str, uuid: &str) {
            self.register(consumer);
            self.session
                .invoke(
                    "gatt.subscribe",
                    json!({"peerId":PEER,"consumer":consumer,
                        "selector":{"serviceUuid":SERVICE,"characteristicUuid":uuid}}),
                )
                .await
                .unwrap();
        }

        /// A consumer whose core queue ends with an overflow terminal after
        /// `items` values, so a poll run can end in the core's own terminal.
        async fn subscribe_small(&self, consumer: &str, uuid: &str, items: u64) {
            self.register(consumer);
            let selector =
                DesktopCentral::<FakeRadio>::selector(SERVICE, None, Some(uuid), None, None, None)
                    .unwrap();
            let delivery = self
                .central
                .subscribe_buffered(
                    PEER,
                    &selector,
                    consumer,
                    None,
                    ubm_core::streams::OverflowPolicy::Error,
                    (items, 1 << 20),
                    OpControl::budget_ms(10_000).with_connection_lease(self.session.lease.clone()),
                )
                .await
                .unwrap();
            self.session.routes.lock().await.push(Route {
                peer: PEER.into(),
                selector,
                consumer: consumer.into(),
                live: true,
                delivery: delivery.as_str(),
            });
        }

        async fn inject(&self, uuid: &str, index: u16) {
            let mut wake = self.central.native_wakes();
            self.central
                .boundary()
                .push_event(RadioEvent::Notification {
                    peer_id: PEER.into(),
                    service_uuid: SERVICE.into(),
                    service_occurrence: 0,
                    characteristic_uuid: uuid.into(),
                    characteristic_occurrence: 0,
                    value: index.to_be_bytes().to_vec(),
                    epoch: self.central.routing_epoch(PEER).await,
                });
            tokio::time::timeout(Duration::from_secs(2), wake.recv())
                .await
                .expect("central admitted the notification")
                .unwrap();
        }

        async fn inject_many(&self, uuid: &str, count: u16) {
            for index in 0..count {
                self.inject(uuid, index).await;
            }
        }

        fn audit(&self) -> rusqlite::Connection {
            // One cursor update per commit: the trigger counts transactions.
            let audit = rusqlite::Connection::open(self.path.as_ref().unwrap()).unwrap();
            audit
                .execute_batch(
                    "CREATE TABLE commits(n INTEGER); \
                     CREATE TRIGGER counted AFTER UPDATE OF next_ordinal ON journal \
                     BEGIN INSERT INTO commits VALUES(NEW.next_ordinal); END;",
                )
                .unwrap();
            audit
        }

        fn fail_insert_of_ordinal(&self, ordinal: i64) -> rusqlite::Connection {
            let injector = rusqlite::Connection::open(self.path.as_ref().unwrap()).unwrap();
            injector
                .execute_batch(&format!(
                    "CREATE TRIGGER injected BEFORE INSERT ON records WHEN NEW.ordinal={ordinal} \
                     BEGIN SELECT RAISE(ABORT,'injected storage failure'); END;"
                ))
                .unwrap();
            injector
        }

        /// Durable value payloads in ordinal order, read independently.
        fn stored_values(&self) -> Vec<String> {
            let reader = rusqlite::Connection::open(self.path.as_ref().unwrap()).unwrap();
            let mut statement = reader
                .prepare("SELECT body FROM records ORDER BY ordinal")
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|body| serde_json::from_str::<Value>(&body.unwrap()).unwrap())
                .filter(|body| body["record"]["t"] == "value")
                .map(|body| body["record"]["valueB64"].as_str().unwrap().to_owned())
                .collect()
        }

        /// Every durable record's kind in ordinal order.
        fn stored_kinds(&self) -> Vec<String> {
            let reader = rusqlite::Connection::open(self.path.as_ref().unwrap()).unwrap();
            let mut statement = reader
                .prepare("SELECT body FROM records ORDER BY ordinal")
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|body| {
                    serde_json::from_str::<Value>(&body.unwrap()).unwrap()["record"]["t"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect()
        }

        fn stored_values_of(&self, consumer: &str) -> usize {
            let reader = rusqlite::Connection::open(self.path.as_ref().unwrap()).unwrap();
            let mut statement = reader.prepare("SELECT body FROM records").unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|body| serde_json::from_str::<Value>(&body.unwrap()).unwrap())
                .filter(|body| {
                    body["record"]["t"] == "value" && body["record"]["consumer"] == consumer
                })
                .count()
        }

        async fn settle(&self) {
            while self.session.collect().await {}
        }

        fn controls(&self) -> Vec<Value> {
            let mut records = Vec::new();
            loop {
                let batch: Value =
                    serde_json::from_str(&self.session.outbox.drain(256, 1 << 20).to_string())
                        .unwrap();
                let entries = batch["records"].as_array().unwrap();
                if entries.is_empty() {
                    return records;
                }
                records.extend(entries.iter().cloned());
            }
        }

        async fn live(&self, consumer: &str) -> bool {
            self.session
                .routes
                .lock()
                .await
                .iter()
                .find(|route| route.consumer == consumer)
                .unwrap()
                .live
        }
    }

    fn commits(audit: &rusqlite::Connection) -> i64 {
        audit
            .query_row("SELECT count(*) FROM commits", [], |row| row.get(0))
            .unwrap()
    }

    fn encoded(index: u16) -> String {
        encode_base64(&index.to_be_bytes())
    }

    fn value_bytes(consumer: &str, index: u16) -> u64 {
        json!({"t":"value","consumer":consumer,"valueB64":encoded(index),"delivery":"unknown"})
            .to_string()
            .len() as u64
    }

    fn stream_ends(records: &[Value], consumer: &str) -> Vec<Value> {
        records
            .iter()
            .filter(|record| record["t"] == "stream-end" && record["consumer"] == consumer)
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn backlog_commits_in_bounded_groups_without_waiting_to_fill_and_keeps_order() {
        let harness = Harness::new(Some(1000)).await;
        harness.subscribe("c", FIRST).await;
        harness.inject_many(FIRST, 70).await;
        let audit = harness.audit();
        harness.settle().await;
        assert_eq!(
            commits(&audit),
            3,
            "70 already-held values are 32 + 32 + 6, never one commit per value"
        );
        assert_eq!(
            harness.stored_values(),
            (0..70).map(encoded).collect::<Vec<_>>(),
            "every value lands once, in poll order"
        );
        // A single held value is committed at once, never held back to fill a group.
        harness.inject(FIRST, 500).await;
        assert!(!harness.session.collect().await);
        assert_eq!(commits(&audit), 4);
        assert_eq!(harness.stored_values().len(), 71);
    }

    #[tokio::test]
    async fn a_pass_commits_one_group_across_routes_and_rotates_to_the_peer() {
        let harness = Harness::new(Some(1000)).await;
        harness.subscribe("deep", FIRST).await;
        harness.subscribe("shallow", SECOND).await;
        harness.inject_many(FIRST, 70).await;
        harness.inject_many(SECOND, 5).await;
        let audit = harness.audit();
        assert!(harness.session.collect().await, "the deep route has more");
        assert_eq!(
            (
                harness.stored_values_of("deep"),
                harness.stored_values_of("shallow"),
                commits(&audit)
            ),
            (32, 0, 1),
            "one pass is one group of one route, never one group per route"
        );
        assert!(
            harness.session.routes.try_lock().is_ok(),
            "the routes lock is free between passes, so control operations progress"
        );
        assert!(harness.session.collect().await);
        assert_eq!(
            (
                harness.stored_values_of("deep"),
                harness.stored_values_of("shallow"),
                commits(&audit)
            ),
            (32, 5, 2),
            "the second consumer is served on the next pass, not after the deep backlog"
        );
        harness.settle().await;
        assert_eq!(harness.stored_values_of("deep"), 70);
        assert_eq!(commits(&audit), 4, "32, then 5, then 32, then 6");
    }

    #[tokio::test]
    async fn empty_routes_are_searched_without_commits_and_the_search_rotates() {
        let harness = Harness::new(Some(1000)).await;
        harness.subscribe("a", FIRST).await;
        harness.subscribe("b", SECOND).await;
        harness.inject_many(SECOND, 3).await;
        let audit = harness.audit();
        assert!(
            !harness.session.collect().await,
            "the empty first route was passed over and the one group left nothing behind"
        );
        assert_eq!(harness.stored_values_of("b"), 3);
        assert_eq!(
            commits(&audit),
            1,
            "only the group that held values committed"
        );
        assert!(!harness.session.collect().await);
        assert_eq!(commits(&audit), 1, "an idle pass commits nothing");
    }

    #[tokio::test]
    async fn operation_collection_is_bounded_in_groups_per_live_route_and_loses_nothing() {
        let harness = Harness::new(Some(1000)).await;
        harness.subscribe("deep", FIRST).await;
        harness.subscribe("shallow", SECOND).await;
        harness.inject_many(FIRST, 100).await;
        harness.inject_many(SECOND, 5).await;
        let audit = harness.audit();
        harness.session.collect_for_operation().await;
        assert_eq!(
            (
                harness.stored_values_of("deep"),
                harness.stored_values_of("shallow")
            ),
            (96, 5),
            "2 groups per live route in all, rotated: 32, 5, 32, then 32 past the empty route"
        );
        assert_eq!(
            commits(&audit),
            4,
            "the bound is 2 x 2 groups, each its own commit"
        );
        harness.settle().await;
        assert_eq!(harness.stored_values_of("deep"), 100, "nothing is lost");
    }

    #[tokio::test]
    async fn rolled_back_group_is_counted_once_and_its_storage_error_is_propagated_once() {
        let harness = Harness::new(Some(1000)).await;
        harness.subscribe("c", FIRST).await;
        harness.inject_many(FIRST, 40).await;
        // Registration is ordinal 1; the 40 values are 2..=41. The failing
        // ordinal sits in the second group, so only that group rolls back.
        let _injector = harness.fail_insert_of_ordinal(36);
        harness.settle().await;
        assert_eq!(
            harness.stored_values(),
            (0..32).map(encoded).collect::<Vec<_>>(),
            "the first group committed; the failed group left no row"
        );
        let controls = harness.controls();
        let ends = stream_ends(&controls, "c");
        assert_eq!(ends.len(), 1, "{controls:?}");
        assert_eq!(ends[0]["reason"], "closed");
        assert_eq!(
            ends[0]["droppedItems"], 8,
            "every value of the rolled-back group is counted, none twice"
        );
        assert_eq!(
            ends[0]["droppedBytes"],
            (32..40).map(|index| value_bytes("c", index)).sum::<u64>()
        );
        assert!(!harness.live("c").await);
        let error = harness.session.last_error.lock().await.clone().unwrap();
        assert_eq!(error["platform"]["code"], "storage.io", "{error}");
        assert_eq!(error["platform"]["metadata"]["operation"], "append");
        assert_eq!(
            harness
                .session
                .outbox
                .journal_failure()
                .map(|failure| failure.kind),
            Some("storage.io")
        );
    }

    #[tokio::test]
    async fn capacity_commits_the_prefix_and_counts_every_held_value_in_one_terminal() {
        // Registration + 9 values fill the journal.
        let harness = Harness::new(Some(10)).await;
        harness.subscribe("c", FIRST).await;
        harness.inject_many(FIRST, 40).await;
        harness.settle().await;
        assert_eq!(
            harness.stored_values(),
            (0..9).map(encoded).collect::<Vec<_>>(),
            "the sequential admission prefix is retained"
        );
        let controls = harness.controls();
        let ends = stream_ends(&controls, "c");
        assert_eq!(ends.len(), 1, "{controls:?}");
        assert_eq!(ends[0]["reason"], "closed");
        assert_eq!(
            ends[0]["droppedItems"], 23,
            "the first refused value and the 22 later polled values of its group"
        );
        assert_eq!(
            ends[0]["droppedBytes"],
            (9..32).map(|index| value_bytes("c", index)).sum::<u64>()
        );
        assert!(!harness.live("c").await);
    }

    #[tokio::test]
    async fn core_terminal_loss_joins_a_refusal_instead_of_being_dropped() {
        let harness = Harness::new(Some(10)).await;
        harness.subscribe_small("c", FIRST, 20).await;
        // 20 values fill the core queue; the 21st ends the stream with one
        // overflow terminal, so one poll run holds 20 values then that terminal.
        harness.inject_many(FIRST, 21).await;
        harness.settle().await;
        assert_eq!(harness.stored_values().len(), 9);
        let controls = harness.controls();
        let ends = stream_ends(&controls, "c");
        assert_eq!(ends.len(), 1, "one terminal per route: {controls:?}");
        assert_eq!(
            ends[0]["droppedItems"], 12,
            "11 refused polled values plus the core's own overflow item"
        );
        assert_eq!(
            ends[0]["droppedBytes"],
            (9..20).map(|index| value_bytes("c", index)).sum::<u64>() + 2
        );
    }

    #[tokio::test]
    async fn values_are_committed_before_the_core_terminal_that_ends_them() {
        let harness = Harness::new(Some(1000)).await;
        harness.subscribe_small("c", FIRST, 20).await;
        harness.inject_many(FIRST, 21).await;
        let audit = harness.audit();
        harness.settle().await;
        assert_eq!(
            commits(&audit),
            2,
            "20 held values are one group; the terminal is its own record"
        );
        let kinds = harness.stored_kinds();
        assert_eq!(kinds.len(), 22, "{kinds:?}");
        assert!(
            kinds[1..21].iter().all(|kind| kind == "value"),
            "registration, then the 20 values: {kinds:?}"
        );
        assert_eq!(
            kinds[21], "stream-end",
            "the terminal is durable after every value it ends"
        );
        let controls = harness.controls();
        let ends = stream_ends(&controls, "c");
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["reason"], "overflow");
        assert_eq!(ends[0]["droppedItems"], 1);
        assert_eq!(ends[0]["droppedBytes"], 2);
        assert!(!harness.live("c").await);
    }

    #[tokio::test]
    async fn volatile_queue_admits_its_prefix_then_ends_once_counting_the_held_rest() {
        let cap = crate::continuation_outbox::DATA_RECORD_CAP;
        let harness = Harness::new(None).await;
        harness.subscribe("c", FIRST).await;
        harness.inject_many(FIRST, (cap + 10) as u16).await;
        harness.settle().await;
        let controls = harness.controls();
        assert_eq!(
            controls
                .iter()
                .filter(|record| record["t"] == "value")
                .count(),
            cap,
            "the sequential prefix is retained"
        );
        let ends = stream_ends(&controls, "c");
        assert_eq!(ends.len(), 1, "one terminal per route");
        assert_eq!(ends[0]["reason"], "overflow");
        assert_eq!(ends[0]["droppedItems"], 10);
        assert_eq!(
            ends[0]["droppedBytes"],
            (cap as u16..cap as u16 + 10)
                .map(|index| value_bytes("c", index))
                .sum::<u64>()
        );
        assert_eq!(
            controls.last().unwrap()["t"],
            "stream-end",
            "values drain before the terminal that ends them"
        );
        assert!(!harness.live("c").await);
    }

    #[tokio::test]
    async fn sealed_group_is_counted_after_the_cutoff_once_without_a_value_terminal() {
        let harness = Harness::new(Some(1000)).await;
        harness.subscribe_small("c", FIRST, 20).await;
        harness.inject_many(FIRST, 21).await;
        harness.session.outbox.seal();
        harness.settle().await;
        assert!(harness.stored_values().is_empty());
        let loss = harness.session.outbox.after_cutoff_loss();
        assert_eq!(loss.items, 21, "20 values once each, plus the core's item");
        assert_eq!(
            loss.bytes,
            (0..20).map(|index| value_bytes("c", index)).sum::<u64>() + 2
        );
        let controls = harness.controls();
        let ends = stream_ends(&controls, "c");
        assert_eq!(ends.len(), 1, "only the core's own terminal: {controls:?}");
        assert_eq!(ends[0]["droppedItems"], 1);
    }

    #[tokio::test]
    async fn disposal_tail_commits_in_bounded_groups_and_loses_nothing() {
        let harness = Harness::new(Some(1000)).await;
        harness.subscribe("c", FIRST).await;
        harness.inject_many(FIRST, 100).await;
        let audit = harness.audit();
        let receipt = harness
            .session
            .invoke("session.continuation-dispose", json!({}))
            .await
            .unwrap();
        assert_eq!(receipt["state"], "released", "{receipt}");
        assert_eq!(
            harness.stored_values(),
            (0..100).map(encoded).collect::<Vec<_>>()
        );
        // The call's own collection is bounded to two groups per route; the
        // 36 values behind it are the disposal tail: 32 + 4.
        assert_eq!(commits(&audit), 4);
        assert!(stream_ends(&harness.controls(), "c").is_empty());
    }

    #[tokio::test]
    async fn refused_disposal_tail_is_one_terminal_not_a_swallowed_result() {
        // Registration + 64 collected values + 15 tail values fit.
        let harness = Harness::new(Some(80)).await;
        harness.subscribe("c", FIRST).await;
        harness.inject_many(FIRST, 100).await;
        let receipt = harness
            .session
            .invoke("session.continuation-dispose", json!({}))
            .await
            .unwrap();
        assert_eq!(receipt["state"], "released", "{receipt}");
        assert_eq!(
            harness.stored_values(),
            (0..79).map(encoded).collect::<Vec<_>>()
        );
        let controls = harness.controls();
        let ends = stream_ends(&controls, "c");
        assert_eq!(ends.len(), 1, "{controls:?}");
        assert_eq!(ends[0]["reason"], "closed");
        assert_eq!(ends[0]["droppedItems"], 21);
        assert_eq!(
            ends[0]["droppedBytes"],
            (79..100).map(|index| value_bytes("c", index)).sum::<u64>()
        );
    }

    #[tokio::test]
    async fn sealed_disposal_tail_is_counted_after_the_cutoff_not_committed() {
        let harness = Harness::new(Some(1000)).await;
        harness.subscribe("c", FIRST).await;
        harness.inject_many(FIRST, 100).await;
        harness
            .session
            .invoke("session.quiesce", json!({}))
            .await
            .unwrap();
        let before = harness.stored_values().len();
        let receipt = harness
            .session
            .invoke("session.continuation-dispose", json!({}))
            .await
            .unwrap();
        assert_eq!(receipt["state"], "released", "{receipt}");
        assert_eq!(harness.stored_values().len(), before);
        assert_eq!(
            receipt["afterCutoffItems"].as_u64().unwrap() as usize,
            100 - before,
            "{receipt}"
        );
        assert!(stream_ends(&harness.controls(), "c").is_empty());
    }

    fn notification_drops(records: &[Value]) -> Vec<u64> {
        records
            .iter()
            .filter(|record| record["t"] == "ingress-drop" && record["class"] == "notification")
            .map(|record| record["count"].as_u64().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn tail_values_refused_after_the_route_ended_are_an_explicit_ingress_drop() {
        // Registration + 9 values fill the journal. The core holds 41 values
        // behind a 40 item queue: one pass takes 32 and ends the route with
        // the capacity refusal, leaving 8 values and the core's overflow
        // terminal for the disposal tail.
        let harness = Harness::new(Some(10)).await;
        harness.subscribe_small("c", FIRST, 40).await;
        harness.inject_many(FIRST, 41).await;
        harness.session.collect().await;
        assert!(!harness.live("c").await);
        let first = harness.controls();
        let ends = stream_ends(&first, "c");
        assert_eq!(ends.len(), 1, "{first:?}");
        assert_eq!(ends[0]["droppedItems"], 23);
        let receipt = harness
            .session
            .invoke("session.continuation-dispose", json!({}))
            .await
            .unwrap();
        assert_eq!(receipt["state"], "released", "{receipt}");
        let tail = harness.controls();
        assert!(
            stream_ends(&tail, "c").is_empty(),
            "a route emits one terminal: {tail:?}"
        );
        assert_eq!(
            notification_drops(&tail),
            vec![9],
            "8 refused tail values plus the core's overflow item, once: {tail:?}"
        );
        assert_eq!(harness.stored_values().len(), 9);
        let error = harness.session.last_error.lock().await.clone().unwrap();
        assert_eq!(error["platform"]["code"], "storage.full", "{error}");
    }

    #[tokio::test]
    async fn core_terminal_loss_after_the_route_ended_is_an_explicit_ingress_drop() {
        let harness = Harness::new(Some(1000)).await;
        harness.subscribe_small("c", FIRST, 20).await;
        harness.inject_many(FIRST, 21).await;
        // Simulate a route an earlier poll answer already ended.
        harness.session.routes.lock().await[0].live = false;
        let receipt = harness
            .session
            .invoke("session.continuation-dispose", json!({}))
            .await
            .unwrap();
        assert_eq!(receipt["state"], "released", "{receipt}");
        assert_eq!(
            harness.stored_values(),
            (0..20).map(encoded).collect::<Vec<_>>(),
            "every held value is committed"
        );
        let tail = harness.controls();
        assert!(stream_ends(&tail, "c").is_empty(), "{tail:?}");
        assert_eq!(notification_drops(&tail), vec![1], "{tail:?}");
    }

    #[tokio::test]
    async fn sealed_tail_after_the_route_ended_is_counted_once_after_the_cutoff() {
        let harness = Harness::new(Some(10)).await;
        harness.subscribe_small("c", FIRST, 40).await;
        harness.inject_many(FIRST, 41).await;
        harness.session.collect().await;
        harness
            .session
            .invoke("session.quiesce", json!({}))
            .await
            .unwrap();
        let receipt = harness
            .session
            .invoke("session.continuation-dispose", json!({}))
            .await
            .unwrap();
        assert_eq!(receipt["afterCutoffItems"], 9, "{receipt}");
        let tail = harness.controls();
        assert!(
            notification_drops(&tail).is_empty(),
            "a cutoff-counted loss is not reported a second time: {tail:?}"
        );
    }

    #[tokio::test]
    async fn queue_overflow_is_reported_even_when_the_seal_lands_after_the_refusal() {
        use crate::continuation_outbox::DataIngressFailure as Failure;
        let harness = Harness::new(None).await;
        let outbox = &harness.session.outbox;
        let held = |count: u16| -> Vec<Value> {
            (0..count)
                .map(|index| value_record("c", "unknown", &index.to_be_bytes()))
                .collect()
        };
        let cap = crate::continuation_outbox::DATA_RECORD_CAP;
        assert!(
            outbox.push_data_batch(held(cap as u16)).rejected.is_none(),
            "fill the volatile queue"
        );
        let outcome = outbox.push_data_batch(held(5));
        let rejected = outcome.rejected.expect("a full queue refuses");
        assert!(matches!(rejected.failure, Failure::Overflow { .. }));
        // The seal arrives only after the queue already refused.
        outbox.seal();
        let admission = harness.session.settle(Some(rejected), None);
        let ending = admission.ending.expect("the overflow is still a terminal");
        assert_eq!((ending.reason, ending.items), ("overflow", 5));
        assert!(admission.refused);
        let loss = outbox.after_cutoff_loss();
        assert_eq!(
            (loss.items, loss.bytes),
            (0, 0),
            "a queue refusal is never also counted after the cutoff"
        );
    }

    #[tokio::test]
    async fn sealed_refusal_is_counted_once_and_adds_no_value_terminal() {
        let harness = Harness::new(None).await;
        harness.session.outbox.seal();
        let records: Vec<Value> = (0..5)
            .map(|index| value_record("c", "unknown", &(index as u16).to_be_bytes()))
            .collect();
        let bytes: u64 = records.iter().map(|r| r.to_string().len() as u64).sum();
        let admission = harness.session.admit(records, None);
        assert!(admission.ending.is_none() && !admission.refused);
        let loss = harness.session.outbox.after_cutoff_loss();
        assert_eq!((loss.items, loss.bytes), (5, bytes));
        // The same answer again settles no second time.
        let admission = harness.session.settle(None, None);
        assert!(admission.ending.is_none());
        assert_eq!(harness.session.outbox.after_cutoff_loss().items, 5);
    }
}

/// Process-host owner. Dropping a renderer does not drop this object; host
/// shutdown stops supervision and the central remains authoritative for release.
pub struct DesktopContinuation {
    pub engine: crate::continuation::NativeContinuation,
    monitor: tokio::task::JoinHandle<()>,
}

impl DesktopContinuation {
    pub fn new<B: RadioBoundary>(
        central: DesktopCentral<B>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self::new_with_recording_registry(central, runtime, Arc::default())
    }
    pub fn new_with_recording_registry<B: RadioBoundary>(
        central: DesktopCentral<B>,
        runtime: tokio::runtime::Handle,
        recordings: Arc<crate::continuation_journal::JournalRegistry>,
    ) -> Self {
        let mut lifecycle = central.lifecycle_events();
        let mut adapter = central.adapter_events();
        let engine = crate::continuation::NativeContinuation::new_with_recording_registry(
            Arc::new(DesktopContinuationHost::with_runtime(
                central,
                runtime.clone(),
            )),
            recordings,
        );
        let recovery = engine.clone();
        let monitor_runtime = runtime.clone();
        let monitor = runtime.spawn(async move {
            loop {
                let closed = tokio::select! {
                    result = lifecycle.recv() => matches!(result, Err(tokio::sync::broadcast::error::RecvError::Closed)),
                    result = adapter.recv() => matches!(result, Err(tokio::sync::broadcast::error::RecvError::Closed)),
                };
                if closed { break; }
                recovery.request_recovery(&monitor_runtime);
            }
        });
        Self { engine, monitor }
    }
}

impl Drop for DesktopContinuation {
    fn drop(&mut self) {
        self.engine.stop_recovery();
        self.monitor.abort();
    }
}

pub struct DesktopContinuationHost<B: RadioBoundary> {
    central: DesktopCentral<B>,
    runtime: tokio::runtime::Handle,
}
impl<B: RadioBoundary> DesktopContinuationHost<B> {
    pub fn new(central: DesktopCentral<B>) -> Self {
        Self::with_runtime(central, tokio::runtime::Handle::current())
    }
    pub fn with_runtime(central: DesktopCentral<B>, runtime: tokio::runtime::Handle) -> Self {
        Self { central, runtime }
    }
}

struct NoWake;
impl WakeSink for NoWake {
    fn wake(&self, _: u64) {}
}

/// Values one route turn polls before committing them together: the journal's
/// bounded group, so a backlog is a few commits instead of one per value. A
/// turn never waits to fill a group; it commits what the core already holds.
const VALUE_GROUP: usize = crate::continuation_journal::APPEND_BATCH_MAX;
/// An explicit operation's own collection is this many bounded groups per
/// live route, the same 64 values per route it was bounded to before values
/// were grouped.
const OPERATION_COLLECT_TURNS: usize = 2;

/// The stream-end a route emits, with the loss it reports.
#[derive(Clone, Copy)]
struct Ending {
    reason: &'static str,
    items: u64,
    bytes: u64,
}

impl Ending {
    const CLOSED: Self = Self {
        reason: "closed",
        items: 0,
        bytes: 0,
    };

    fn record(self, consumer: &str) -> Value {
        json!({"t":"stream-end","consumer":consumer,"reason":self.reason,"droppedItems":self.items,"droppedBytes":self.bytes})
    }

    fn with_loss(self, items: u64, bytes: u64) -> Self {
        Self {
            items: self.items.saturating_add(items),
            bytes: self.bytes.saturating_add(bytes),
            ..self
        }
    }
}

fn value_record(consumer: &str, delivery: &str, bytes: &[u8]) -> Value {
    json!({"t":"value","consumer":consumer,"valueB64":encode_base64(bytes),"delivery":delivery})
}

/// What committing one polled group decided for its route.
struct Admission {
    /// The stream-end the route emits, once: the refusal that ended it, with
    /// the core's own terminal loss folded in, or that terminal alone.
    ending: Option<Ending>,
    /// An unsealed refusal ended the route.
    refused: bool,
    /// The storage failure that refused the group.
    failure: Option<Value>,
    /// `ending`'s loss is already in the handoff cutoff count, so a route
    /// that can no longer emit a terminal has nothing left to report.
    cutoff_counted: bool,
}

/// One route's disposal tail: the values and terminal the central hands over
/// under its own lock while it retires the consumer. Values are committed in
/// the same bounded groups as collection and the route still ends once.
struct DisposalTail<'a, B: RadioBoundary> {
    session: &'a Session<B>,
    consumer: &'a str,
    delivery: &'static str,
    /// The route already emitted its terminal; it cannot emit another.
    ended: bool,
    state: std::sync::Mutex<TailState>,
}

#[derive(Default)]
struct TailState {
    held: Vec<Value>,
    ending: Option<Ending>,
    /// `ending`'s loss is already in the handoff cutoff count.
    cutoff_counted: bool,
    refused: bool,
    failure: Option<Value>,
    finished: bool,
}

struct TailOutcome {
    ended: bool,
    failure: Option<Value>,
}

#[derive(Clone)]
struct Route {
    peer: String,
    selector: PathSelector,
    consumer: String,
    live: bool,
    delivery: &'static str,
}
struct Session<B: RadioBoundary> {
    central: DesktopCentral<B>,
    lease: String,
    peer: Mutex<Option<String>>,
    routes: Mutex<Vec<Route>>,
    /// Where the next collection pass starts its search, so the route served
    /// last is the last one searched again.
    cursor: AtomicUsize,
    outbox: Outbox,
    last_error: Mutex<Option<Value>>,
}

impl<B: RadioBoundary> ContinuationHost for DesktopContinuationHost<B> {
    fn open_session(&self) -> Result<Arc<dyn ContinuationSession>> {
        if self.central.is_shut_down() {
            return Err(failure("lifecycle.destroyed", "central is shut down"));
        }
        let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        let session = Arc::new(Session {
            central: self.central.clone(),
            lease: format!("ubm-native-continuation-{id}"),
            peer: Mutex::new(None),
            routes: Mutex::new(Vec::new()),
            cursor: AtomicUsize::new(0),
            outbox: Outbox::new(id, Arc::new(NoWake)),
            last_error: Mutex::new(None),
        });
        let weak = Arc::downgrade(&session);
        let mut wakes = self.central.native_wakes();
        self.runtime.spawn(async move {
            loop {
                let Some(session) = weak.upgrade() else {
                    break;
                };
                if session.central.is_shut_down() {
                    break;
                }
                // One awaited pass per session: native bounded hubs retain
                // backpressure; no extra writer queue or detached disk work.
                let more = match session.collect_pass().await {
                    Ok(more) => more,
                    Err(error) => {
                        *session.last_error.lock().await = Some(error);
                        let owned = session.clone();
                        let result = crate::continuation_journal::run_blocking_result(move || {
                            owned.outbox.fail_collection_worker();
                            Ok(())
                        })
                        .await;
                        if let Err(error) = result {
                            *session.last_error.lock().await = Some(error);
                        }
                        break;
                    }
                };
                drop(session);
                if more {
                    tokio::task::yield_now().await;
                } else if matches!(
                    wakes.recv().await,
                    Err(tokio::sync::broadcast::error::RecvError::Closed)
                ) {
                    break;
                }
            }
        });
        Ok(session)
    }
}

fn failure(code: &str, detail: &str) -> Value {
    json!({"code":code,"domain":"restoration","operation":"continuation","detail":detail})
}

fn error(error: DesktopError) -> Value {
    let platform = error.platform().map(|platform| {
        let metadata:serde_json::Map<String, Value> = platform.metadata.iter().map(|(key,value)| {
            let value = match value { crate::PlatformValue::Int(value) => json!(value), crate::PlatformValue::Text(value) => json!(value), crate::PlatformValue::Bool(value) => json!(value) };
            (key.clone(), value)
        }).collect();
        json!({"domain":platform.domain,"code":platform.code,"message":platform.message.as_deref().or(error.detail()),"metadata":metadata})
    });
    json!({"code":error.code_str(),"domain":error.domain().as_str(),"operation":error.operation(),
        "detail":error.detail(),"retryability":error.retryability().as_str(),"platform":platform})
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| failure("argument.invalid", key))
}

impl<B: RadioBoundary> Session<B> {
    async fn collect_pass(self: &Arc<Self>) -> Result<bool> {
        // Attachment may occur while this pass awaits a route or native poll.
        // A pre-await has_journal snapshot cannot authorize inline execution.
        let owned = self.clone();
        let runtime = tokio::runtime::Handle::current();
        crate::continuation_journal::run_blocking_result(move || {
            Ok(runtime.block_on(owned.collect()))
        })
        .await
    }
    /// Commit one polled group, then decide its route's ending. Every value
    /// is committed before the answer that ended the poll run, in order.
    fn admit(&self, records: Vec<Value>, core: Option<Ending>) -> Admission {
        let rejected = if records.is_empty() {
            None
        } else {
            self.outbox.push_data_batch(records).rejected
        };
        self.settle(rejected, core)
    }

    /// Decide a route's ending from the refusal the push itself answered with.
    /// The cause is read from that answer, never from the outbox afterwards: a
    /// seal that lands after a queue or storage refusal cannot turn it into a
    /// cutoff refusal and hide it.
    ///
    /// A refused group is the accepted prefix plus one terminal that counts
    /// each uncommitted value once, with the core's own terminal loss folded
    /// in rather than dropped. A refusal by the handoff cutoff already counted
    /// the group after that cutoff, so no value terminal is added.
    fn settle(
        &self,
        rejected: Option<crate::continuation_outbox::DataBatchRejection>,
        core: Option<Ending>,
    ) -> Admission {
        use crate::continuation_outbox::DataIngressFailure;
        let mut failure = None;
        let refusal = rejected.and_then(|rejected| {
            let reason = match rejected.failure {
                DataIngressFailure::Stopped { .. } | DataIngressFailure::Sealed { .. } => {
                    return None;
                }
                DataIngressFailure::Overflow { .. } => "overflow",
                DataIngressFailure::Storage { error, .. } => {
                    failure = Some(crate::continuation::recording_failure(error));
                    "closed"
                }
            };
            Some(Ending {
                reason,
                items: rejected.items,
                bytes: rejected.bytes,
            })
        });
        let (ending, cutoff_counted) = match (refusal, core) {
            // The terminal carries the core's loss; counting it after a cutoff
            // as well would report it twice.
            (Some(refusal), Some(core)) => (Some(refusal.with_loss(core.items, core.bytes)), false),
            (Some(refusal), None) => (Some(refusal), false),
            (None, Some(core)) => {
                let counted = self.outbox.note_after_cutoff_loss(core.items, core.bytes);
                (Some(core), counted)
            }
            // A cutoff refusal counted its values after the cutoff itself.
            (None, None) => (None, true),
        };
        Admission {
            refused: refusal.is_some(),
            ending,
            failure,
            cutoff_counted,
        }
    }

    /// One normal pass commits at most ONE already-held group (<= `VALUE_GROUP`
    /// values) across all routes. Terminal controls retain separate commits,
    /// but a second consumer or control operation never waits behind every
    /// route's value group under one acquisition of the `routes` lock.
    /// The search starts after the route served last and passes over routes
    /// that hold nothing without committing, so a deep route cannot starve its
    /// peers. A terminal always follows the values it ends. Returns whether
    /// more is waiting: a full group, or routes this pass did not reach.
    async fn collect(&self) -> bool {
        let mut routes = self.routes.lock().await;
        let count = routes.len();
        let start = self.cursor.load(Ordering::Relaxed) % count.max(1);
        for step in 0..count {
            let index = (start + step) % count;
            let route = &mut routes[index];
            if !route.live {
                continue;
            }
            let mut records = Vec::with_capacity(VALUE_GROUP);
            let mut core = None;
            let mut poll_failure = None;
            let answered = loop {
                if records.len() == VALUE_GROUP {
                    break false;
                }
                match self
                    .central
                    .poll_notification(&route.peer, &route.selector, &route.consumer)
                    .await
                {
                    Ok(NotificationPoll::Value(bytes)) => {
                        records.push(value_record(&route.consumer, route.delivery, &bytes));
                    }
                    Ok(NotificationPoll::Empty) => break true,
                    Ok(NotificationPoll::Terminal(terminal)) => {
                        core = Some(Ending {
                            reason: terminal.reason(),
                            items: terminal.dropped_items(),
                            bytes: terminal.dropped_bytes(),
                        });
                        break true;
                    }
                    Ok(NotificationPoll::Invalidated(_)) => {
                        core = Some(Ending {
                            reason: "invalidated",
                            ..Ending::CLOSED
                        });
                        break true;
                    }
                    Ok(NotificationPoll::Closed) => {
                        core = Some(Ending::CLOSED);
                        break true;
                    }
                    Err(error) => {
                        poll_failure = Some(self::error(error));
                        core = Some(Ending::CLOSED);
                        break true;
                    }
                }
            };
            let committed = !records.is_empty();
            // A storage refusal precedes the poll answer that was held behind it.
            let admission = self.admit(records, core);
            if let Some(failure) = admission.failure.or(poll_failure) {
                *self.last_error.lock().await = Some(failure);
            }
            let ended = admission.ending.is_some();
            if let Some(ending) = admission.ending {
                self.outbox.push_control(ending.record(&route.consumer));
                route.live = false;
            }
            if !committed {
                // Nothing was committed, so this route cost no group: keep
                // searching for the one that holds values.
                continue;
            }
            self.cursor.store(index + 1, Ordering::Relaxed);
            let unvisited = (step + 1..count).any(|later| routes[(start + later) % count].live);
            return unvisited || (!answered && !ended);
        }
        false
    }

    /// An explicit operation's own collection: `OPERATION_COLLECT_TURNS`
    /// bounded groups per live route, each its own pass so the `routes` lock is
    /// released between them. Values it leaves behind are not lost: the
    /// background collector commits them, or a sealed outbox counts them after
    /// the cutoff, and the disposal tail hands over whatever the central still
    /// holds. A hot producer therefore cannot hold the operation past its
    /// deadline.
    async fn collect_for_operation(&self) {
        let live = self
            .routes
            .lock()
            .await
            .iter()
            .filter(|route| route.live)
            .count();
        for _ in 0..OPERATION_COLLECT_TURNS * live {
            if !self.collect().await {
                break;
            }
        }
    }

    async fn invoke(&self, op: &str, args: Value) -> Result<Value> {
        let ctl = || {
            OpControl::budget_ms(args["budgetMs"].as_u64().unwrap_or(10000))
                .with_connection_lease(self.lease.clone())
        };
        match op {
            "connection.request-mtu" => Err(failure(
                "capability.unsupported",
                "desktop native MTU negotiation is OS-managed",
            )),
            "connection.connect" => {
                let peer = text(&args, "peerId")?;
                self.central
                    .connect(peer, &self.lease, ctl())
                    .await
                    .map_err(error)?;
                // The central owns compensation for failed acquisition. Only
                // a successful result transfers a lease to this session.
                *self.peer.lock().await = Some(peer.to_owned());
                Ok(json!({"state":"connected"}))
            }
            "gatt.discover" => {
                self.central
                    .discover(text(&args, "peerId")?, &self.lease, ctl())
                    .await
                    .map_err(error)?;
                Ok(json!({"state":"current"}))
            }
            "gatt.subscribe" => {
                let peer = text(&args, "peerId")?;
                let consumer = text(&args, "consumer")?;
                let selector = &args["selector"];
                let selector = DesktopCentral::<B>::selector(
                    text(selector, "serviceUuid")?,
                    selector["serviceOccurrence"].as_u64(),
                    Some(text(selector, "characteristicUuid")?),
                    selector["characteristicOccurrence"].as_u64(),
                    None,
                    None,
                )
                .map_err(error)?;
                {
                    let mut routes = self.routes.lock().await;
                    if !routes.iter().any(|route| route.consumer == consumer) {
                        routes.push(Route {
                            peer: peer.into(),
                            selector: selector.clone(),
                            consumer: consumer.into(),
                            live: false,
                            delivery: "unknown",
                        });
                    }
                }
                // Keep provisional ownership, but never block collection of
                // existing routes behind an OS enable completion.
                let delivery = self
                    .central
                    .subscribe(peer, &selector, consumer, None, ctl())
                    .await
                    .map_err(error)?;
                {
                    let mut routes = self.routes.lock().await;
                    if let Some(route) = routes.iter_mut().find(|route| route.consumer == consumer)
                    {
                        route.live = true;
                        route.delivery = delivery.as_str();
                    }
                }
                // An early value wake may have been consumed while this route
                // was provisional; drain again after publishing its delivery.
                self.collect_for_operation().await;
                Ok(json!({"delivery":delivery.as_str()}))
            }
            "gatt.write" => {
                let selector = &args["selector"];
                let selector = DesktopCentral::<B>::selector(
                    text(selector, "serviceUuid")?,
                    selector["serviceOccurrence"].as_u64(),
                    Some(text(selector, "characteristicUuid")?),
                    selector["characteristicOccurrence"].as_u64(),
                    None,
                    None,
                )
                .map_err(error)?;
                let value =
                    crate::continuation_outbox::decode_base64(text(&args, "valueB64")?, 512)
                        .map_err(|_| failure("argument.invalid", "invalid setup write bytes"))?;
                self.central
                    .write(
                        text(&args, "peerId")?,
                        &selector,
                        value,
                        "with-response",
                        ctl(),
                    )
                    .await
                    .map_err(error)?;
                Ok(json!({"state":"written"}))
            }
            "session.reconcile" => {
                self.collect_for_operation().await;
                let peers = self.central.peer_records().await;
                let links:Vec<Value> = peers.iter().filter(|peer| peer.connection_state == Some(ConnectionState::Connected)).map(|peer| json!({"peerId":peer.peer_id,"state":"connected","connectionGeneration":peer.connection_generation,"databaseGeneration":peer.database_generation,"databaseState":if peer.database_state == Some(DatabaseState::Current) {"current"} else {"stale"}})).collect();
                let routes = self.routes.lock().await;
                let subscriptions:Vec<Value> = routes.iter().map(|route| json!({"consumer":route.consumer,"state":if route.live {"live"} else {"closed"}})).collect();
                Ok(json!({"links":links,"subscriptions":subscriptions}))
            }
            "session.quiesce" => {
                self.collect_for_operation().await;
                let loss = self.outbox.seal();
                Ok(
                    json!({"state":"sealed","afterCutoffItems":loss.items,"afterCutoffBytes":loss.bytes}),
                )
            }
            "session.continuation-dispose" => {
                self.collect_for_operation().await;
                let routes = self.routes.lock().await.clone();
                let mut failures = Vec::new();
                for route in routes {
                    let tail = DisposalTail::new(self, &route);
                    let drain = |observation| tail.observe(observation);
                    let released = self
                        .central
                        .unsubscribe_draining(
                            &route.peer,
                            &route.selector,
                            &route.consumer,
                            ctl(),
                            Some(&drain),
                        )
                        .await;
                    let outcome = tail.finish();
                    if let Some(failure) = outcome.failure {
                        *self.last_error.lock().await = Some(failure);
                    }
                    if outcome.ended
                        && let Some(owned) = self
                            .routes
                            .lock()
                            .await
                            .iter_mut()
                            .find(|owned| owned.consumer == route.consumer)
                    {
                        owned.live = false;
                    }
                    match released {
                        Ok(_) => self
                            .routes
                            .lock()
                            .await
                            .retain(|owned| owned.consumer != route.consumer),
                        Err(failure) => {
                            failures.push(error(failure));
                        }
                    }
                    self.collect_for_operation().await;
                }
                let mut peer = self.peer.lock().await;
                if let Some(peer_id) = peer.as_ref() {
                    match self
                        .central
                        .release_connection_lease(peer_id, &self.lease, ctl())
                        .await
                    {
                        Ok(physical_released) => {
                            if physical_released {
                                self.collect_for_operation().await;
                                self.routes.lock().await.clear();
                                failures.clear();
                            }
                            *peer = None;
                        }
                        Err(failure) => failures.push(error(failure)),
                    }
                }
                let loss = self.outbox.after_cutoff_loss();
                Ok(
                    json!({"state":if failures.is_empty() {"released"} else {"release-failed"},"failures":failures,"afterCutoffItems":loss.items,"afterCutoffBytes":loss.bytes}),
                )
            }
            "counters.describe" => Ok(
                json!({"queuedData":self.outbox.queued_data(),"lastError":*self.last_error.lock().await}),
            ),
            _ => Err(failure("capability.unsupported", op)),
        }
    }
}

impl<'a, B: RadioBoundary> DisposalTail<'a, B> {
    fn new(session: &'a Session<B>, route: &'a Route) -> Self {
        Self {
            session,
            consumer: &route.consumer,
            delivery: route.delivery,
            ended: !route.live,
            state: std::sync::Mutex::new(TailState::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, TailState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The central's per-observation hand-over. Held values are committed in
    /// bounded groups and always before the terminal that follows them.
    fn observe(&self, observation: NotificationPoll) {
        let mut state = self.state();
        match observation {
            NotificationPoll::Value(bytes) => {
                state
                    .held
                    .push(value_record(self.consumer, self.delivery, &bytes));
                if state.held.len() == VALUE_GROUP {
                    self.commit(&mut state, None);
                }
            }
            NotificationPoll::Terminal(terminal) => self.commit(
                &mut state,
                Some(Ending {
                    reason: terminal.reason(),
                    items: terminal.dropped_items(),
                    bytes: terminal.dropped_bytes(),
                }),
            ),
            _ => {}
        }
    }

    fn commit(&self, state: &mut TailState, core: Option<Ending>) {
        let records = std::mem::take(&mut state.held);
        if state.refused {
            // An unsealed refusal already ended this tail's admission: every
            // later value and the core's own terminal loss ride that one
            // terminal, counted once and not offered to the outbox again.
            let held_bytes: u64 = records
                .iter()
                .map(|record| record.to_string().len() as u64)
                .sum();
            let (core_items, core_bytes) = core.map_or((0, 0), |core| (core.items, core.bytes));
            state.ending = state.ending.map(|ending| {
                ending.with_loss(
                    (records.len() as u64).saturating_add(core_items),
                    held_bytes.saturating_add(core_bytes),
                )
            });
            return;
        }
        let admission = self.session.admit(records, core);
        if state.failure.is_none() {
            state.failure = admission.failure;
        }
        state.refused = admission.refused;
        if admission.ending.is_some() {
            state.ending = admission.ending;
            state.cutoff_counted = admission.cutoff_counted;
        }
    }

    /// Commit what is still held and emit the route's one terminal. Idempotent:
    /// a cancelled disposal still hands every drained value to the outbox.
    fn finish(&self) -> TailOutcome {
        let mut state = self.state();
        if state.finished {
            return TailOutcome {
                ended: false,
                failure: None,
            };
        }
        state.finished = true;
        self.commit(&mut state, None);
        let ended = match state.ending.take() {
            Some(ending) if !self.ended => {
                self.session
                    .outbox
                    .push_control(ending.record(self.consumer));
                true
            }
            // A route that already ended cannot emit a second terminal, and
            // its tail loss must not vanish behind the first one. Unless the
            // handoff cutoff already counted it, it is reported as the
            // notification ingress drop it is.
            Some(ending) => {
                if !state.cutoff_counted {
                    self.session
                        .outbox
                        .push_ingress_drop_count(IngressClass::Notification, ending.items);
                }
                false
            }
            None => false,
        };
        TailOutcome {
            ended,
            failure: state.failure.take(),
        }
    }
}

impl<B: RadioBoundary> Drop for DisposalTail<'_, B> {
    fn drop(&mut self) {
        let outcome = self.finish();
        if let Some(failure) = outcome.failure
            && let Ok(mut slot) = self.session.last_error.try_lock()
        {
            *slot = Some(failure);
        }
    }
}

impl<B: RadioBoundary> ContinuationSession for Session<B> {
    fn seal_collection(&self) -> Result<()> {
        self.outbox.seal();
        Ok(())
    }
    fn collection_sealed(&self) -> bool {
        self.outbox.is_sealed()
    }
    fn attach_journal(
        &self,
        journal: Arc<crate::continuation_journal::ContinuationJournal>,
        mut context: Value,
    ) -> Result<()> {
        context["sessionId"] = json!(self.lease);
        context["backendInstanceId"] =
            json!(self.central.attachment().backend_instance_id().as_str());
        context["sessionStartedAtUnixNs"] = json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| failure("platform.failure", "recording session clock unavailable"))?
                .as_nanos()
                .to_string()
        );
        context["sessionEpoch"] = json!(format!(
            "{}:{}:{}",
            context["backendInstanceId"]
                .as_str()
                .expect("backend instance"),
            self.lease,
            context["sessionStartedAtUnixNs"]
                .as_str()
                .expect("session clock")
        ));
        self.outbox
            .attach_journal(journal, context)
            .map_err(crate::continuation::recording_failure)
    }
    fn register_journal_consumer(&self, consumer: &str, metadata: Value) -> Result<()> {
        self.outbox
            .register_journal_consumer(consumer, metadata)
            .map_err(crate::continuation::recording_failure)
    }
    fn observe(
        &self,
        consumer: &str,
        matcher: crate::continuation_outbox::RecordMatcher,
    ) -> Result<crate::continuation_outbox::Observation> {
        self.outbox
            .observe(consumer, matcher)
            .map_err(|detail| failure("lifecycle.invalid-state", detail))
    }
    fn call<'a>(&'a self, op: &'a str, args: &'a str) -> ContinuationFuture<'a> {
        Box::pin(async move {
            let result = match serde_json::from_str(args) {
                Ok(args) => self.invoke(op, args).await,
                Err(_) => Err(failure("argument.invalid", "invalid operation JSON")),
            };
            match result { Ok(value) => json!({"ok":true,"value":value}), Err(error) => json!({"ok":false,"retryability":error.get("retryability").cloned().unwrap_or(json!("never")),"error":error}) }.to_string()
        })
    }
    fn drain(&self, max_items: u32, max_bytes: u32) -> ContinuationFuture<'_> {
        Box::pin(async move {
            self.outbox
                .drain(max_items as usize, max_bytes as usize)
                .to_string()
        })
    }
}
