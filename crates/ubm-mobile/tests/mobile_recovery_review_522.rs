//! Public-session regressions for v2 mobile recovery review 522.
mod common;

use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};
use ubm_desktop::ObservedDelivery;
use ubm_mobile::{
    Advertisement, HostOptions, MobileHost, MobilePlatform, MobileSession,
    OrderedAdmissionNegotiation, OrderedAdmissionOwner, OrderedEnd, OrderedOpenCompletionV2 as OC,
    OrderedOpenOutcomeV2 as OO, OrderedOpenRequestV2 as OR, OrderedOpenResultV2,
    OrderedOpenRetired, OrderedProvenance, OrderedRecord, OrderedRegistration,
    OrderedSourceGeneration as Gen, PlatformRadio, RadioCompletion as C, RadioIngress,
    RadioRequest as R, WakeSink,
};

#[derive(Clone, Copy)]
enum Mode {
    Normal,
    LostBarrier,
    LostAllBarriers,
    NoEffectCleanup,
    UnansweredBarrier,
    UnansweredClose,
}
#[derive(Default)]
struct State {
    requests: Vec<R>,
    queue: HashMap<u64, Vec<OrderedRecord>>,
    batches: HashMap<u64, (u64, Vec<OrderedRecord>)>,
    next_batch: u64,
    barriers: HashSet<(u64, u64)>,
    no_effect_ready: bool,
    disable_attempts: usize,
    release_barrier: bool,
    release_close: bool,
}
struct Radio {
    owner: OrderedAdmissionOwner,
    host: OnceLock<Weak<MobileHost>>,
    state: Mutex<State>,
    mode: Mode,
}
impl Radio {
    fn new(owner: OrderedAdmissionOwner, mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            owner,
            host: OnceLock::new(),
            state: Mutex::new(State::default()),
            mode,
        })
    }
    fn requests(&self) -> Vec<R> {
        self.state.lock().unwrap().requests.clone()
    }
    fn enqueue(&self, group: u64, record: OrderedRecord) {
        self.state
            .lock()
            .unwrap()
            .queue
            .entry(group)
            .or_default()
            .push(record);
    }
}
impl PlatformRadio for Radio {
    fn ordered_admission_owner(&self) -> Option<OrderedAdmissionOwner> {
        Some(self.owner)
    }
    fn ordered_admission_negotiation(&self) -> Option<OrderedAdmissionNegotiation> {
        Some(OrderedAdmissionNegotiation {
            version: 2,
            owner: self.owner,
        })
    }
    fn cancel(&self, _: u64) {}
    fn submit(&self, request: R) {
        let mut s = self.state.lock().unwrap();
        s.requests.push(request.clone());
        let response = match &request {
            R::Discover { .. } => C::DiscoveredScoped {
                services: common::polar_services(),
                identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "review-522-graph".into(),
                    attachment: 1,
                    revision: 1,
                },
            },
            R::OrderedOpenV2(OR::Open { identity, .. })
                if matches!(
                    self.mode,
                    Mode::Normal | Mode::UnansweredBarrier | Mode::UnansweredClose
                ) =>
            {
                C::OrderedOpenV2(OC::Result(OrderedOpenResultV2 {
                    slot_identity: identity.slot_identity(),
                    outcome: OO::Opened {
                        source_generation: Gen::Counter(7),
                        members: registrations(identity),
                    },
                }))
            }
            R::OrderedOpenV2(OR::Open { .. }) => return,
            R::OrderedOpenV2(OR::Recover { identity, .. }) => {
                C::OrderedOpenV2(OC::Result(OrderedOpenResultV2 {
                    slot_identity: identity.slot_identity(),
                    outcome: if matches!(self.mode, Mode::NoEffectCleanup) {
                        if s.no_effect_ready {
                            OO::Failed {
                                original_failure: ubm_mobile::PlatformFailure::new(
                                    ubm_mobile::FailureKind::Platform,
                                    "open had no effect",
                                ),
                                ownership: ubm_mobile::OrderedOpenOwnershipV2::NotCreated,
                            }
                        } else {
                            OO::Pending
                        }
                    } else {
                        OO::Opened {
                            source_generation: Gen::Counter(7),
                            members: registrations(identity),
                        }
                    },
                }))
            }
            R::OrderedOpenV2(OR::Acknowledge { identity, .. }) => {
                C::OrderedOpenV2(OC::Retired(OrderedOpenRetired {
                    slot_identity: identity.slot_identity(),
                }))
            }
            R::OrderedRetainClaim {
                generation, member, ..
            } => C::OrderedClaim {
                generation: generation.clone(),
                member: *member,
            },
            R::OrderedEnableNotifications { .. } => {
                C::NotifyEnabled(ObservedDelivery::Notification)
            }
            R::OrderedEnableSettlement { .. } => C::OrderedEnableStatus(
                ubm_mobile::OrderedEnableStatus::Enabled(ObservedDelivery::Notification),
            ),
            R::DisableNotifications { .. } if matches!(self.mode, Mode::NoEffectCleanup) => {
                s.disable_attempts += 1;
                if s.disable_attempts == 1 {
                    C::Failed(ubm_mobile::PlatformFailure::new(
                        ubm_mobile::FailureKind::Platform,
                        "transient cleanup failure",
                    ))
                } else {
                    C::Unit
                }
            }
            R::OrderedBarrier {
                group_id,
                barrier_id,
                ..
            } => {
                if matches!(self.mode, Mode::UnansweredBarrier) && !s.release_barrier {
                    return;
                }
                if s.barriers.insert((*group_id, *barrier_id)) {
                    s.queue
                        .entry(*group_id)
                        .or_default()
                        .push(OrderedRecord::Barrier {
                            barrier_id: *barrier_id,
                            uncertain_tail: false,
                        });
                    if matches!(self.mode, Mode::LostBarrier | Mode::LostAllBarriers) {
                        return;
                    }
                }
                if matches!(self.mode, Mode::LostAllBarriers) {
                    return;
                }
                C::Unit
            }
            R::OrderedRetire {
                group_id, member, ..
            } => {
                s.queue
                    .entry(*group_id)
                    .or_default()
                    .push(OrderedRecord::Fence {
                        member: *member,
                        uncertain_tail: false,
                    });
                C::Unit
            }
            R::OrderedClose { group_id, .. } => {
                if matches!(self.mode, Mode::UnansweredClose) && !s.release_close {
                    return;
                }
                s.queue
                    .entry(*group_id)
                    .or_default()
                    .push(OrderedRecord::Terminal {
                        reason: OrderedEnd::Closed,
                        uncertain_tail: false,
                        native_domain: None,
                        native_code: None,
                        failure_member: None,
                        rejected: None,
                    });
                C::Unit
            }
            R::OrderedDrain { group_id, .. } => {
                if !s.batches.contains_key(group_id) {
                    let records = std::mem::take(s.queue.entry(*group_id).or_default());
                    if !records.is_empty() {
                        s.next_batch += 1;
                        let id = s.next_batch;
                        s.batches.insert(*group_id, (id, records));
                    }
                }
                match s.batches.get(group_id) {
                    Some((id, records)) => C::OrderedRecords {
                        batch_id: Some(*id),
                        records: records.clone(),
                    },
                    None => C::OrderedRecords {
                        batch_id: None,
                        records: vec![],
                    },
                }
            }
            R::OrderedAcknowledgeBatch {
                group_id, batch_id, ..
            } => {
                assert_eq!(s.batches.get(group_id).map(|v| v.0), Some(*batch_id));
                s.batches.remove(group_id);
                C::Unit
            }
            other => match common::polar_responder(other) {
                common::Reply::Now(c) => c,
                common::Reply::Hold => return,
            },
        };
        drop(s);
        self.host
            .get()
            .unwrap()
            .upgrade()
            .unwrap()
            .complete(request.id(), response);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unanswered_close_releases_gate_and_retains_cleanup() {
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005228").unwrap();
    let radio = Radio::new(owner, Mode::UnansweredClose);
    let host = host(&radio).await;
    let (session, c, d, reserved) = setup(&host, "unanswered-close").await;
    call(&session, "gatt.ordered.open", open_args(&c, &d, &reserved)).await;
    let args = json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"]});
    let close = |id: String| json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"operationId":id});
    assert_eq!(
        call(&session, "gatt.ordered.close", close("first-close".into())).await,
        "pending-drain"
    );
    let poll = call(&session, "gatt.ordered.poll", args.clone()).await;
    assert!(poll["deliveryId"].is_null());
    radio.state.lock().unwrap().release_close = true;
    tokio::time::timeout(Duration::from_secs(5), async {
        for n in 0..20 {
            let result = call(&session, "gatt.ordered.close", close(format!("retry-close-{n}"))).await;
            if result == "released" { return; }
            let delivery = call(&session, "gatt.ordered.poll", args.clone()).await;
            if let Some(id) = delivery["deliveryId"].as_str() {
                let record: Value = serde_json::from_str(delivery["record"].as_str().unwrap()).unwrap();
                assert_eq!(record["reason"]["kind"], "closed");
                call(&session, "gatt.ordered.ack-delivery", json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"deliveryId":id})).await;
            }
        }
        panic!("close debt did not retire");
    }).await.unwrap();
}
fn registrations(identity: &ubm_mobile::OrderedOpenIdentity) -> Vec<OrderedRegistration> {
    identity
        .spec
        .members
        .iter()
        .map(|m| OrderedRegistration {
            member_id: m.member_id,
            source_member: m.member_id + 11,
            epoch: 2,
        })
        .collect()
}
struct Wake;
impl WakeSink for Wake {
    fn wake(&self, _: u64) {}
}
async fn host(radio: &Arc<Radio>) -> Arc<MobileHost> {
    let host = Arc::new(
        MobileHost::open(
            radio.clone(),
            Arc::new(Wake),
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "review-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    host
}
async fn call(session: &MobileSession, op: &str, args: Value) -> Value {
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        common::call(session, op, &args.to_string()),
    )
    .await
    .unwrap_or_else(|_| panic!("{op} timed out"));
    let envelope = common::parse(&result);
    assert_eq!(envelope["ok"], true, "{op} failed: {result}");
    envelope["value"].clone()
}
async fn setup(host: &Arc<MobileHost>, label: &str) -> (MobileSession, Value, Value, Value) {
    let session = host.open_session(label).unwrap();
    call(
        &session,
        "scan.start",
        json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}),
    )
    .await;
    host.ingest(RadioIngress::Advertisement(Advertisement {
        peer_id: common::POLAR.into(),
        service_uuids: vec!["180D".into()],
        ..Default::default()
    }));
    common::drain_until(&session, |r| !common::of_type(r, "adv").is_empty()).await;
    let connection = call(
        &session,
        "connection.connect",
        json!({"peerId":common::POLAR,"lease":"lease","operationId":"connect"}),
    )
    .await;
    let database = call(
        &session,
        "gatt.discover",
        json!({"peerId":common::POLAR,"lease":"lease","operationId":"discover"}),
    )
    .await;
    let reserved = call(
        &session,
        "gatt.ordered.reserve",
        json!({"peerId":common::POLAR}),
    )
    .await;
    (session, connection, database, reserved)
}
fn open_args(connection: &Value, database: &Value, reserved: &Value) -> Value {
    json!({"wireVersion":2,"peerId":common::POLAR,"collectionId":reserved["collectionId"],"connectionGeneration":connection["connectionGeneration"],"databaseGeneration":database["databaseGeneration"],"publicConnectionGeneration":"c","publicDatabaseGeneration":"d","members":[{"memberId":"0","publicMemberId":"a","selector":common::selector(),"requestedDelivery":"prefer-notification"}],"maxItems":8,"maxBytes":128,"maxControls":8,"operationId":"open"})
}
async fn wait_for(label: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label}"));
}
fn native_group(radio: &Radio) -> u64 {
    radio
        .requests()
        .iter()
        .find_map(|r| {
            if let R::OrderedOpenV2(OR::Open { identity, .. }) = r {
                Some(identity.native_group_id)
            } else {
                None
            }
        })
        .expect("native open")
}
async fn cancelled_open(session: &MobileSession, radio: &Radio, args: Value) {
    let s = session.clone();
    let open =
        tokio::spawn(async move { common::call(&s, "gatt.ordered.open", &args.to_string()).await });
    wait_for("native open was not dispatched", || {
        radio
            .requests()
            .iter()
            .any(|r| matches!(r, R::OrderedOpenV2(OR::Open { .. })))
    })
    .await;
    call(session, "op.cancel", json!({"operationId":"open"})).await;
    let result = tokio::time::timeout(Duration::from_secs(3), open)
        .await
        .expect("cancelled open hung")
        .unwrap();
    common::failure(&result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_distinct_native_ids_deliver_values_to_both_hosts() {
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005221").unwrap();
    let a = Radio::new(owner, Mode::Normal);
    let b = Radio::new(owner, Mode::Normal);
    let ha = host(&a).await;
    let hb = host(&b).await;
    let (sa, ca, da, ra) = setup(&ha, "a").await;
    let (sb, cb, db, rb) = setup(&hb, "b").await;
    call(&sa, "gatt.ordered.open", open_args(&ca, &da, &ra)).await;
    call(&sb, "gatt.ordered.open", open_args(&cb, &db, &rb)).await;
    let ga = native_group(&a);
    let gb = native_group(&b);
    assert_ne!(ga, gb);
    assert_ne!(ga.to_string(), ra["collectionId"].as_str().unwrap());
    assert_ne!(gb.to_string(), rb["collectionId"].as_str().unwrap());
    for (host, radio, group, value) in [(&ha, &a, ga, 41), (&hb, &b, gb, 42)] {
        radio.enqueue(
            group,
            OrderedRecord::Value {
                member: OrderedRegistration {
                    member_id: 0,
                    source_member: 11,
                    epoch: 2,
                },
                ordinal: 1,
                value: vec![value],
                delivery: ObservedDelivery::Notification,
                provenance: OrderedProvenance::Notification,
            },
        );
        host.ingest(RadioIngress::OrderedWake {
            generation: Gen::Counter(7),
            group_id: group,
        });
    }
    async fn value(
        session: &MobileSession,
        collection: &Value,
    ) -> Result<Value, tokio::time::error::Elapsed> {
        let args = json!({"peerId":common::POLAR,"collectionId":collection});
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let delivery = call(session, "gatt.ordered.poll", args.clone()).await;
                if !delivery["deliveryId"].is_null() {
                    return delivery;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    }
    let (first, second) = tokio::join!(
        value(&sa, &ra["collectionId"]),
        value(&sb, &rb["collectionId"])
    );
    assert!(
        first.is_ok() && second.is_ok(),
        "native values did not reach both central collections: host a={first:?}, host b={second:?}"
    );
    for (session, reserved, expected, delivery) in [
        (&sa, &ra, 41, first.unwrap()),
        (&sb, &rb, 42, second.unwrap()),
    ] {
        let record: Value = serde_json::from_str(delivery["record"].as_str().unwrap()).unwrap();
        assert_eq!(record["bytes"], json!([expected]));
        call(session, "gatt.ordered.ack-delivery", json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"deliveryId":delivery["deliveryId"]})).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_first_barrier_reply_retries_same_receipt_and_disposes() {
    lost_barrier_replies_recover(Mode::LostBarrier, "12345678-1234-4abc-8def-000000005222").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unanswered_barrier_remains_retryable_without_fabricating_source_failure() {
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005227").unwrap();
    let radio = Radio::new(owner, Mode::UnansweredBarrier);
    let host = host(&radio).await;
    let (session, c, d, reserved) = setup(&host, "unanswered-barrier").await;
    call(&session, "gatt.ordered.open", open_args(&c, &d, &reserved)).await;
    let args = json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"]});
    let close = |id: String| json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"operationId":id});
    assert_eq!(
        call(
            &session,
            "gatt.ordered.close",
            close("unanswered-close".into())
        )
        .await,
        "pending-source"
    );
    let poll = call(&session, "gatt.ordered.poll", args.clone()).await;
    assert!(
        poll["deliveryId"].is_null(),
        "unanswered barrier is not source failure: {poll}"
    );
    assert!(
        !radio
            .requests()
            .iter()
            .any(|r| matches!(r, R::OrderedClose { .. }))
    );
    let first: Vec<_> = radio
        .requests()
        .iter()
        .filter_map(|r| match r {
            R::OrderedBarrier { barrier_id, .. } => Some(*barrier_id),
            _ => None,
        })
        .collect();
    assert!(first.len() >= 2 && first.iter().all(|id| *id == first[0]));
    radio.state.lock().unwrap().release_barrier = true;
    tokio::time::timeout(Duration::from_secs(5), async {
        for n in 0..20 {
            let result = call(&session, "gatt.ordered.close", close(format!("retry-{n}"))).await;
            if result == "released" { return; }
            let delivery = call(&session, "gatt.ordered.poll", args.clone()).await;
            if let Some(id) = delivery["deliveryId"].as_str() {
                let record: Value = serde_json::from_str(delivery["record"].as_str().unwrap()).unwrap();
                assert_eq!(record["reason"]["kind"], "closed");
                call(&session, "gatt.ordered.ack-delivery", json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"deliveryId":id})).await;
            }
        }
        panic!("barrier debt did not retire");
    }).await.unwrap();
    let ids: Vec<_> = radio
        .requests()
        .iter()
        .filter_map(|r| match r {
            R::OrderedBarrier { barrier_id, .. } => Some(*barrier_id),
            _ => None,
        })
        .collect();
    assert!(ids.iter().all(|id| *id == first[0]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeatedly_lost_barrier_replies_use_the_acknowledged_native_record() {
    lost_barrier_replies_recover(
        Mode::LostAllBarriers,
        "12345678-1234-4abc-8def-000000005226",
    )
    .await;
}

async fn lost_barrier_replies_recover(mode: Mode, owner: &str) {
    let owner = OrderedAdmissionOwner::negotiate_v2(owner).unwrap();
    let radio = Radio::new(owner, mode);
    let host = host(&radio).await;
    let (session, c, d, reserved) = setup(&host, "lost-barrier").await;
    cancelled_open(&session, &radio, open_args(&c, &d, &reserved)).await;
    wait_for("first barrier was never dispatched", || {
        radio
            .requests()
            .iter()
            .any(|r| matches!(r, R::OrderedBarrier { .. }))
    })
    .await;
    let dispose_session = session.clone();
    let disposal =
        tokio::spawn(async move { common::call(&dispose_session, "session.dispose", "{}").await });
    wait_for("lost barrier was never retried", || {
        radio
            .requests()
            .iter()
            .filter(|r| matches!(r, R::OrderedBarrier { .. }))
            .count()
            >= 2
    })
    .await;
    let barriers: Vec<_> = radio
        .requests()
        .into_iter()
        .filter_map(|r| {
            if let R::OrderedBarrier {
                id,
                generation,
                group_id,
                barrier_id,
            } = r
            {
                Some((id, generation, group_id, barrier_id))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        (&barriers[0].1, barriers[0].2, barriers[0].3),
        (&barriers[1].1, barriers[1].2, barriers[1].3),
        "retry must preserve native barrier identity"
    );
    assert_ne!(
        barriers[0].0, barriers[1].0,
        "each transport attempt has its own request ID"
    );
    assert_eq!(
        host.complete(barriers[0].0, C::Unit),
        ubm_mobile::CompletionStatus::Late
    );
    assert_eq!(
        host.complete(barriers[0].0, C::Unit),
        ubm_mobile::CompletionStatus::Late
    );
    let result = tokio::time::timeout(Duration::from_secs(5), disposal)
        .await
        .expect("disposal remained blocked after retry")
        .unwrap();
    assert_eq!(
        host.complete(barriers[1].0, C::Unit),
        ubm_mobile::CompletionStatus::Late
    );
    let receipt = common::ok(&result);
    assert_eq!(
        receipt["state"], "release-failed",
        "disposal must preserve the detached ordered receipt: {receipt}"
    );
    let observer = host.open_session("barrier-observer").unwrap();
    call(&observer, "gatt.ordered.reclaim", json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"reclaimCapability":reserved["reclaimCapability"]})).await;
    let args = json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"]});
    tokio::time::timeout(Duration::from_secs(5), async {
        for n in 0..100 {
            let close = call(&observer, "gatt.ordered.close", json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"operationId":format!("recovery-close-{n}")})).await;
            if close == "released" { return; }
            let delivery = call(&observer, "gatt.ordered.poll", args.clone()).await;
            if !delivery["deliveryId"].is_null() {
                call(&observer, "gatt.ordered.ack-delivery", json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"deliveryId":delivery["deliveryId"]})).await;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("recovered source did not release");
    }).await.expect("recovered source cleanup timed out");
    assert!(
        radio
            .requests()
            .iter()
            .any(|r| matches!(r, R::OrderedRemove { .. })),
        "native source was not removed"
    );
    call(&observer, "gatt.ordered.ack-group", args).await;
    call(&observer, "gatt.ordered.forget-group", json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"reclaimCapability":reserved["reclaimCapability"]})).await;
    let final_disposal = call(&session, "session.dispose", json!({})).await;
    assert_eq!(final_disposal["state"], "released", "{final_disposal}");
}

#[tokio::test(start_paused = true)]
async fn disposal_retires_no_effect_open_before_worker_and_retries_real_claim_cleanup() {
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005225").unwrap();
    let radio = Radio::new(owner, Mode::NoEffectCleanup);
    let started = tokio::time::Instant::now();
    let host = host(&radio).await;
    let (session, c, d, reserved) = setup(&host, "no-effect-cleanup").await;
    cancelled_open(&session, &radio, open_args(&c, &d, &reserved)).await;
    radio.state.lock().unwrap().no_effect_ready = true;

    // No clock advance occurs between abandonment and disposal: the worker's
    // initial 500 ms scan cannot be the caller that retires the native open.
    let disposal = call(&session, "session.dispose", json!({})).await;
    assert_eq!(disposal["state"], "release-failed");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "worker scan may have run before disposal"
    );
    {
        let state = radio.state.lock().unwrap();
        let retired = state
            .requests
            .iter()
            .position(|request| matches!(request, R::OrderedOpenV2(OR::Acknowledge { .. })))
            .expect("disposal did not retire native open");
        let disable = state
            .requests
            .iter()
            .position(|request| matches!(request, R::DisableNotifications { .. }))
            .expect("central claim cleanup did not run");
        assert!(
            retired < disable,
            "native retirement must precede failed claim cleanup"
        );
        assert_eq!(
            state.disable_attempts, 1,
            "first central cleanup must fail once"
        );
    }

    for _ in 0..20 {
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        if radio.state.lock().unwrap().disable_attempts >= 2 {
            break;
        }
    }
    assert!(
        radio.state.lock().unwrap().disable_attempts >= 2,
        "worker forgot central cleanup after native receipt retirement"
    );
    let observer = host.open_session("no-effect-observer").unwrap();
    call(&observer, "gatt.ordered.reclaim", json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"reclaimCapability":reserved["reclaimCapability"]})).await;
    let args = json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"]});
    let close = call(&observer, "gatt.ordered.close", json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"operationId":"no-effect-close"})).await;
    assert_eq!(close, "released");
    call(&observer, "gatt.ordered.ack-group", args).await;
    call(&observer, "gatt.ordered.forget-group", json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"reclaimCapability":reserved["reclaimCapability"]})).await;
    let final_disposal = call(&session, "session.dispose", json!({})).await;
    assert_eq!(final_disposal["state"], "released", "{final_disposal}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_distinct_ids_route_barrier_and_complete_cleanup_on_both_hosts() {
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005224").unwrap();
    let a = Radio::new(owner, Mode::Normal);
    let b = Radio::new(owner, Mode::Normal);
    let ha = host(&a).await;
    let hb = host(&b).await;
    let (sa, ca, da, ra) = setup(&ha, "barrier-a").await;
    let (sb, cb, db, rb) = setup(&hb, "barrier-b").await;
    call(&sa, "gatt.ordered.open", open_args(&ca, &da, &ra)).await;
    call(&sb, "gatt.ordered.open", open_args(&cb, &db, &rb)).await;
    let ga = native_group(&a);
    let gb = native_group(&b);
    assert_ne!(ga, gb);
    assert_ne!(ga.to_string(), ra["collectionId"].as_str().unwrap());
    assert_ne!(gb.to_string(), rb["collectionId"].as_str().unwrap());
    for (session, radio, group, reserved) in [(&sa, &a, ga, &ra), (&sb, &b, gb, &rb)] {
        let args = json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"]});
        tokio::time::timeout(Duration::from_secs(3),async {
            for n in 0..100 {
                let close=call(session,"gatt.ordered.close",json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"operationId":format!("close-{n}")})).await;
                if close=="released" { return; }
                let delivery=call(session,"gatt.ordered.poll",args.clone()).await;
                if !delivery["deliveryId"].is_null() {
                    call(session,"gatt.ordered.ack-delivery",json!({"peerId":common::POLAR,"collectionId":reserved["collectionId"],"deliveryId":delivery["deliveryId"]})).await;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("central cleanup did not release");
        }).await.expect("barrier and cleanup timed out");
        let requests = radio.requests();
        assert!(
            requests
                .iter()
                .any(|r| matches!(r,R::OrderedBarrier {group_id,..} if *group_id==group)),
            "barrier must use the native group ID"
        );
        assert!(
            requests
                .iter()
                .any(|r| matches!(r,R::OrderedAcknowledgeBatch {group_id,..} if *group_id==group)),
            "native barrier batch must be acknowledged"
        );
        assert!(
            requests
                .iter()
                .any(|r| matches!(r,R::OrderedRemove {group_id,..} if *group_id==group)),
            "native source must be removed"
        );
        call(session, "gatt.ordered.ack-group", args).await;
    }
}
