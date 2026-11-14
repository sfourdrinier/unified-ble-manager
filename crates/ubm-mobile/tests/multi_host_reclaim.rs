// crates/ubm-mobile/tests/multi_host_reclaim.rs
#![cfg(all(feature = "synthetic-marker-harness", debug_assertions))]
mod common;

use common::{Reply, Scripted};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use ubm_desktop::{
    DesktopCentral, ObservedDelivery, OpControl,
    continuation_journal::JournalQuota,
    ordered_recording::{OrderedRecordingDeclaration, OrderedRecordingMember},
};
use ubm_mobile::{
    MobileHost, MobilePlatform, MobileSession, OrderedEnd,
    OrderedGraphAuthorityStatus as Authority, OrderedGraphPermitState as Permit,
    OrderedGraphReserveStatus as Reserve, OrderedRecord, OrderedRegistration,
    OrderedSourceGeneration as Generation, RadioCompletion as C, RadioRequest as R,
};

#[derive(Default)]
struct SharedSource {
    next: u64,
    reservations: HashMap<u64, u64>,
    // Receipt ID and authority ID are intentionally distinct source-issued IDs.
    authorities: HashMap<u64, (u64, bool)>,
    permit_receipts: Vec<(ubm_desktop::boundary::OrderedReclaimPermitRequest, u64)>,
    open_dispatches: usize,
    lost_open: bool,
    close_dispatches: usize,
    forget_dispatches: usize,
    lost_close: bool,
    lost_forget: bool,
    opened: Vec<u64>,
    closed: Vec<u64>,
    forgotten: Vec<u64>,
}
impl SharedSource {
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next + 100
    }
}

fn adapter(
    shared: Arc<Mutex<SharedSource>>,
    queue: Arc<Mutex<Vec<OrderedRecord>>>,
) -> Arc<Scripted> {
    let batch = AtomicU64::new(1);
    Scripted::new(Box::new(move |request| {
        let mut s = shared.lock().unwrap();
        let result = match request {
            R::Discover { .. } => C::DiscoveredScoped {
                services: common::polar_services(),
                identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "shared-graph".into(),
                    attachment: 1,
                    revision: 1,
                },
            },
            R::OrderedOpen { members, .. } => C::OrderedOpened {
                generation: Generation::Counter(7),
                members: members
                    .iter()
                    .map(|m| OrderedRegistration {
                        member_id: m.member_id,
                        source_member: m.member_id + 1,
                        epoch: 1,
                    })
                    .collect(),
            },
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
            R::OrderedRetire { member, .. } => {
                queue.lock().unwrap().push(OrderedRecord::Fence {
                    member: *member,
                    uncertain_tail: false,
                });
                C::Unit
            }
            R::OrderedBarrier { barrier_id, .. } => {
                queue.lock().unwrap().push(OrderedRecord::Barrier {
                    barrier_id: *barrier_id,
                    uncertain_tail: false,
                });
                C::Unit
            }
            R::OrderedClose { .. } => {
                queue.lock().unwrap().push(OrderedRecord::Terminal {
                    reason: OrderedEnd::Closed,
                    uncertain_tail: false,
                    native_domain: None,
                    native_code: None,
                    failure_member: None,
                    rejected: None,
                });
                C::Unit
            }
            R::OrderedDrain { .. } => {
                let records = std::mem::take(&mut *queue.lock().unwrap());
                C::OrderedRecords {
                    batch_id: (!records.is_empty()).then(|| batch.fetch_add(1, Ordering::SeqCst)),
                    records,
                }
            }
            R::OrderedGraphAuthorityReserve { request, .. } => {
                let receipt = if let Some(id) = s.reservations.get(request) {
                    *id
                } else {
                    let id = s.id();
                    s.reservations.insert(*request, id);
                    id
                };
                C::OrderedGraphAuthorityReserved(receipt)
            }
            R::OrderedGraphAuthorityReserveFence { request, .. }
            | R::OrderedGraphAuthorityReserveStatus { request, .. } => {
                C::OrderedGraphReserveStatus(
                    s.reservations
                        .get(request)
                        .copied()
                        .map(Reserve::Issued)
                        .unwrap_or(Reserve::NotCreated),
                )
            }
            R::OrderedGraphAuthorityReserveForget { request, .. } => {
                s.reservations.remove(request);
                C::Unit
            }
            R::OrderedGraphAuthorityOpen { request, .. } => {
                s.open_dispatches += 1;
                let id = if let Some((id, _)) = s.authorities.get(request) {
                    *id
                } else {
                    let id = s.id();
                    s.authorities.insert(*request, (id, false));
                    s.opened.push(id);
                    id
                };
                if !s.lost_open {
                    s.lost_open = true;
                    return Reply::Hold;
                }
                C::OrderedGraphAuthorityOpened(id)
            }
            R::OrderedGraphAuthorityFenceOpen { request, .. }
            | R::OrderedGraphAuthorityStatus { request, .. } => {
                C::OrderedGraphAuthorityStatus(match s.authorities.get(request) {
                    Some((id, true)) => Authority::Closed(*id),
                    Some((id, false)) => Authority::Opened(*id),
                    None => Authority::NotCreated,
                })
            }
            R::OrderedGraphAuthorityForgetOpen { request, .. } => {
                s.authorities.remove(request);
                C::Unit
            }
            R::OrderedGraphAuthorityClose { authority_id, .. } => {
                s.close_dispatches += 1;
                if let Some((_, closed)) = s
                    .authorities
                    .values_mut()
                    .find(|(id, _)| id == authority_id)
                {
                    *closed = true;
                } else {
                    panic!("unknown authority close");
                }
                if !s.closed.contains(authority_id) {
                    s.closed.push(*authority_id);
                }
                if !s.lost_close {
                    s.lost_close = true;
                    return Reply::Hold;
                }
                C::Unit
            }
            R::OrderedGraphAuthorityForget { authority_id, .. } => {
                s.forget_dispatches += 1;
                assert!(
                    s.authorities
                        .values()
                        .any(|(id, closed)| id == authority_id && *closed)
                );
                if !s.forgotten.contains(authority_id) {
                    s.forgotten.push(*authority_id);
                }
                if !s.lost_forget {
                    s.lost_forget = true;
                    return Reply::Hold;
                }
                C::Unit
            }
            R::OrderedGraphPermitReserve { key, .. } => {
                let id = if let Some((_, id)) = s.permit_receipts.iter().find(|(k, _)| k == key) {
                    *id
                } else {
                    let id = s.id();
                    s.permit_receipts.push((key.clone(), id));
                    id
                };
                C::OrderedGraphPermitReserved(id)
            }
            R::OrderedGraphPermitReserveFence { key, .. }
            | R::OrderedGraphPermitReserveStatus { key, .. } => C::OrderedGraphReserveStatus(
                s.permit_receipts
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, id)| Reserve::Issued(*id))
                    .unwrap_or(Reserve::NotCreated),
            ),
            R::OrderedGraphPermitForgetRequest { key, .. } => {
                s.permit_receipts.retain(|(k, _)| k != key);
                C::Unit
            }
            R::OrderedGraphPermitAcquire { key, .. } => {
                assert!(
                    s.authorities
                        .values()
                        .any(|(id, closed)| *id == key.authority_id && !closed)
                );
                assert!(
                    s.permit_receipts
                        .iter()
                        .any(|(request, id)| *id == key.permit_id
                            && request.authority_id == key.authority_id
                            && request.peer_id == key.peer_id
                            && request.connection_generation == key.connection_generation
                            && request.identity == key.identity)
                );
                C::OrderedGraphPermit(Permit::Acquired)
            }
            R::OrderedGraphPermitCommit { .. } => C::OrderedGraphPermit(Permit::Committed),
            R::OrderedGraphPermitRelease { .. } => C::OrderedGraphPermit(Permit::Released),
            R::OrderedGraphPermitForget { .. } => C::OrderedGraphPermit(Permit::Forgotten),
            _ => return common::polar_responder(request),
        };
        Reply::Now(result)
    }))
}

async fn call(session: &MobileSession, op: &str, args: serde_json::Value) -> serde_json::Value {
    common::ok(&common::call(session, op, &args.to_string()).await)
}
async fn connect(host: &Arc<MobileHost>, session: &MobileSession, label: &str) -> (String, String) {
    call(session,"scan.start",serde_json::json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":format!("scan-{label}")})).await;
    host.ingest(ubm_mobile::RadioIngress::Advertisement(
        ubm_mobile::Advertisement {
            peer_id: common::POLAR.into(),
            service_uuids: vec!["180D".into()],
            ..Default::default()
        },
    ));
    common::drain_until(session, |records| {
        !common::of_type(records, "adv").is_empty()
    })
    .await;
    let connection=call(session,"connection.connect",serde_json::json!({"peerId":common::POLAR,"lease":format!("lease-{label}"),"operationId":format!("connect-{label}")})).await;
    let database=call(session,"gatt.discover",serde_json::json!({"peerId":common::POLAR,"lease":format!("lease-{label}"),"operationId":format!("discover-{label}")})).await;
    (
        connection["connectionGeneration"].as_str().unwrap().into(),
        database["databaseGeneration"].as_str().unwrap().into(),
    )
}

async fn recorder(
    host: &Arc<MobileHost>,
    name: &str,
) -> (Arc<MobileSession>, String, String, String) {
    let first = host.open_session(&format!("first-{name}")).unwrap();
    let (connection, database) = connect(host, &first, name).await;
    let declaration = OrderedRecordingDeclaration {
        peer_id: common::POLAR.into(),
        connection_generation: connection,
        database_generation: database,
        members: vec![OrderedRecordingMember {
            member_id: 0,
            public_member_id: "hr".into(),
            selector: DesktopCentral::<ubm_mobile::ForeignRadio>::selector(
                "180D",
                Some(0),
                Some("2A37"),
                Some(0),
                None,
                None,
            )
            .unwrap(),
            delivery: None,
            preference: None,
        }],
        max_items: 8,
        max_bytes: 128,
        max_controls: 8,
    };
    let reservation = serde_json::json!([name, format!("multi-host-{name}")]).to_string();
    let handle = first.reserve_ordered_recording_start(&reservation).unwrap()["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = first
        .start_ordered_recording(
            handle.clone(),
            name.into(),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 100,
            },
            declaration,
            OpControl::budget_ms(5000),
        )
        .await
        .unwrap();
    assert_eq!(result["kind"], "started");
    let collection = result["receipt"]["collectionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let capability = result["reclaimCapability"].as_str().unwrap().to_owned();
    first.retire_ordered_recording_start(&handle).await.unwrap();
    let disposed = call(&first, "session.dispose", serde_json::json!({})).await;
    assert_eq!(disposed["state"], "released");
    let replacement = host.open_session(&format!("replacement-{name}")).unwrap();
    connect(host, &replacement, &format!("replacement-{name}")).await;
    (Arc::new(replacement), collection, capability, name.into())
}

async fn start_reclaim(
    session: &MobileSession,
    collection: &str,
    capability: &str,
    recording: &str,
    reserved: Option<serde_json::Value>,
) -> serde_json::Value {
    let args = if let Some(args) = reserved {
        args
    } else {
        let mut args = serde_json::json!({"peerId":common::POLAR,"collectionId":collection,"recordingId":recording,"reclaimCapability":capability});
        let next = call(session, "gatt.ordered.recording-reclaim.next", args.clone()).await;
        args["requestId"] = next["requestId"].clone();
        args["callerKey"] = serde_json::json!(format!("key-{recording}"));
        call(
            session,
            "gatt.ordered.recording-reclaim.reserve",
            args.clone(),
        )
        .await;
        args
    };
    call(
        session,
        "gatt.ordered.recording-reclaim.start",
        args.clone(),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let status = call(
                session,
                "gatt.ordered.recording-reclaim.status",
                args.clone(),
            )
            .await;
            if status["state"] != "pending" {
                assert_eq!(status["state"], "ready", "{status}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    call(
        session,
        "gatt.ordered.recording-reclaim.commit",
        args.clone(),
    )
    .await;
    call(
        session,
        "gatt.ordered.recording-reclaim.release",
        args.clone(),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if call(
                session,
                "gatt.ordered.recording-reclaim.status",
                args.clone(),
            )
            .await["state"]
                == "released"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    for op in ["retire", "ack-retire", "forget-retire"] {
        call(
            session,
            &format!("gatt.ordered.recording-reclaim.{op}"),
            args.clone(),
        )
        .await;
    }
    args
}

async fn recording_control(session: &MobileSession, id: &str, context: bool) -> serde_json::Value {
    let (tx, rx) = tokio::sync::oneshot::channel();
    session.ordered_recording_control_wire(
        id.into(),
        context,
        Box::new(move |value| {
            let _ = tx.send(value);
        }),
    );
    common::parse(&rx.await.unwrap())
}
async fn stop_recording(session: &MobileSession, id: &str) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = recording_control(session, id, false).await;
            assert_eq!(result["ok"], true, "{result}");
            if result["value"]["recording"]["state"] == "finished" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn shared_native_source_isolates_two_real_central_owners() {
    let shared = Arc::new(Mutex::new(SharedSource::default()));
    let (radio1, radio2) = (
        adapter(shared.clone(), Arc::default()),
        adapter(shared.clone(), Arc::default()),
    );
    let (host1, _) = common::open(&radio1, MobilePlatform::Android).await;
    let (host2, _) = common::open(&radio2, MobilePlatform::Android).await;
    host1.enable_synthetic_marker_for_test();
    host2.enable_synthetic_marker_for_test();
    let root = std::env::temp_dir().join(format!("ubm-multi-host-{}", std::process::id()));
    std::fs::create_dir_all(root.join("one")).unwrap();
    std::fs::create_dir_all(root.join("two")).unwrap();
    common::ok(&host1.continuation_configure_recording_directory(&root.join("one")));
    common::ok(&host2.continuation_configure_recording_directory(&root.join("two")));
    let (session1, group1, cap1, id1) = recorder(&host1, "one").await;
    let (session2, group2, cap2, id2) = recorder(&host2, "two").await;
    let wake1 = {
        let host = host1.clone();
        let group = group1.parse().unwrap();
        tokio::spawn(async move {
            loop {
                host.ingest(ubm_mobile::RadioIngress::OrderedWake {
                    generation: Generation::Counter(7),
                    group_id: group,
                });
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    let wake2 = {
        let host = host2.clone();
        let group = group2.parse().unwrap();
        tokio::spawn(async move {
            loop {
                host.ingest(ubm_mobile::RadioIngress::OrderedWake {
                    generation: Generation::Counter(7),
                    group_id: group,
                });
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    let contender = host1.open_session("contender-one").unwrap();
    call(&contender, "connection.connect", serde_json::json!({"peerId":common::POLAR,"lease":"lease-contender","operationId":"connect-contender"})).await;
    let base = serde_json::json!({"peerId":common::POLAR,"collectionId":group1,"recordingId":id1,"reclaimCapability":cap1});
    let first_next = call(
        &session1,
        "gatt.ordered.recording-reclaim.next",
        base.clone(),
    )
    .await;
    let second_next = call(
        &contender,
        "gatt.ordered.recording-reclaim.next",
        base.clone(),
    )
    .await;
    assert_eq!(first_next["requestId"], second_next["requestId"]);
    let mut winner = base.clone();
    winner["requestId"] = first_next["requestId"].clone();
    winner["callerKey"] = serde_json::json!("winner");
    let mut loser = base.clone();
    loser["requestId"] = second_next["requestId"].clone();
    loser["callerKey"] = serde_json::json!("loser");
    let (first_reserve, duplicate_reserve) = tokio::join!(
        call(
            &session1,
            "gatt.ordered.recording-reclaim.reserve",
            winner.clone()
        ),
        call(
            &session1,
            "gatt.ordered.recording-reclaim.reserve",
            winner.clone()
        ),
    );
    assert_eq!(first_reserve["state"], "reserved");
    assert_eq!(duplicate_reserve["state"], "reserved");
    let retry_next = call(&session1, "gatt.ordered.recording-reclaim.next", serde_json::json!({"peerId":common::POLAR,"collectionId":group1,"recordingId":id1,"reclaimCapability":cap1,"callerKey":"winner"})).await;
    assert_eq!(retry_next["requestId"], first_next["requestId"]);
    let distinct_next = call(&session1, "gatt.ordered.recording-reclaim.next", serde_json::json!({"peerId":common::POLAR,"collectionId":group1,"recordingId":id1,"reclaimCapability":cap1,"callerKey":"different"})).await;
    assert_eq!(distinct_next["state"], "not-reserved");
    let invalid_next = common::parse(&common::call(&session1, "gatt.ordered.recording-reclaim.next", &serde_json::json!({"peerId":common::POLAR,"collectionId":group1,"recordingId":id1,"reclaimCapability":"wrong","callerKey":"different"}).to_string()).await);
    assert_eq!(invalid_next["ok"], false);
    assert_eq!(invalid_next["error"]["code"], "ownership.denied");
    let refusal = call(&contender, "gatt.ordered.recording-reclaim.reserve", loser).await;
    assert_eq!(refusal["state"], "not-reserved");
    // The first reserve reply could have been lost. Reconnecting the same
    // session must not replace the identity that owns the retained gate.
    call(&session1, "connection.connect", serde_json::json!({"peerId":common::POLAR,"lease":"lease-winner-replacement","operationId":"connect-winner-replacement"})).await;
    assert_eq!(
        call(
            &session1,
            "gatt.ordered.recording-reclaim.reserve",
            winner.clone()
        )
        .await["state"],
        "reserved",
        "winner's lost reply can be replayed"
    );
    let args1 = start_reclaim(&session1, &group1, &cap1, &id1, Some(winner)).await;
    // The first forget took effect but its reply may have been lost. A new
    // reservation must not replace the exact receipt needed by the old caller.
    let next = call(
        &session1,
        "gatt.ordered.recording-reclaim.next",
        base.clone(),
    )
    .await;
    let mut newer = base.clone();
    newer["requestId"] = next["requestId"].clone();
    newer["callerKey"] = serde_json::json!("newer");
    assert_eq!(
        call(
            &session1,
            "gatt.ordered.recording-reclaim.reserve",
            newer.clone()
        )
        .await["state"],
        "reserved"
    );
    assert_eq!(
        call(
            &session1,
            "gatt.ordered.recording-reclaim.forget-retire",
            args1.clone()
        )
        .await["state"],
        "forgotten"
    );
    let mut forged = args1.clone();
    forged["callerKey"] = serde_json::json!("forged");
    let rejected = common::parse(
        &common::call(
            &session1,
            "gatt.ordered.recording-reclaim.ack-forget",
            &forged.to_string(),
        )
        .await,
    );
    assert_eq!(rejected["ok"], false);
    assert_eq!(
        call(
            &session1,
            "gatt.ordered.recording-reclaim.ack-forget",
            args1.clone()
        )
        .await["state"],
        "acknowledged"
    );
    assert_eq!(
        call(
            &session1,
            "gatt.ordered.recording-reclaim.ack-forget",
            args1.clone()
        )
        .await["state"],
        "absent"
    );
    assert_eq!(
        call(
            &session1,
            "gatt.ordered.recording-reclaim.status",
            newer.clone()
        )
        .await["state"],
        "reserved"
    );
    call(
        &session1,
        "gatt.ordered.recording-reclaim.cancel",
        newer.clone(),
    )
    .await;
    for op in ["retire", "ack-retire", "forget-retire", "ack-forget"] {
        call(
            &session1,
            &format!("gatt.ordered.recording-reclaim.{op}"),
            newer.clone(),
        )
        .await;
    }
    // Receipts cannot be evicted to make room: only the exact caller can
    // acknowledge one, after which a new reservation can be admitted.
    let mut receipts = Vec::new();
    for index in 0..128 {
        let next = call(
            &session1,
            "gatt.ordered.recording-reclaim.next",
            base.clone(),
        )
        .await;
        let mut args = base.clone();
        args["requestId"] = next["requestId"].clone();
        args["callerKey"] = serde_json::json!(format!("receipt-{index}"));
        assert_eq!(
            call(
                &session1,
                "gatt.ordered.recording-reclaim.reserve",
                args.clone()
            )
            .await["state"],
            "reserved"
        );
        for op in ["cancel", "retire", "ack-retire", "forget-retire"] {
            call(
                &session1,
                &format!("gatt.ordered.recording-reclaim.{op}"),
                args.clone(),
            )
            .await;
        }
        receipts.push(args);
    }
    let next = call(
        &session1,
        "gatt.ordered.recording-reclaim.next",
        base.clone(),
    )
    .await;
    let mut overflow = base.clone();
    overflow["requestId"] = next["requestId"].clone();
    overflow["callerKey"] = serde_json::json!("overflow");
    assert_eq!(
        call(
            &session1,
            "gatt.ordered.recording-reclaim.reserve",
            overflow.clone()
        )
        .await["state"],
        "not-reserved"
    );
    assert_eq!(
        call(
            &session1,
            "gatt.ordered.recording-reclaim.forget-retire",
            receipts[0].clone()
        )
        .await["state"],
        "forgotten"
    );
    let mut wrong_capability = receipts[0].clone();
    wrong_capability["reclaimCapability"] = serde_json::json!("wrong");
    let rejected = common::parse(
        &common::call(
            &session1,
            "gatt.ordered.recording-reclaim.ack-forget",
            &wrong_capability.to_string(),
        )
        .await,
    );
    assert_eq!(rejected["ok"], false);
    assert_eq!(
        call(
            &session1,
            "gatt.ordered.recording-reclaim.ack-forget",
            receipts[0].clone()
        )
        .await["state"],
        "acknowledged"
    );
    assert_eq!(
        call(
            &session1,
            "gatt.ordered.recording-reclaim.reserve",
            overflow.clone()
        )
        .await["state"],
        "reserved"
    );
    for op in [
        "cancel",
        "retire",
        "ack-retire",
        "forget-retire",
        "ack-forget",
    ] {
        call(
            &session1,
            &format!("gatt.ordered.recording-reclaim.{op}"),
            overflow.clone(),
        )
        .await;
    }
    for receipt in receipts.into_iter().skip(1) {
        assert_eq!(
            call(
                &session1,
                "gatt.ordered.recording-reclaim.ack-forget",
                receipt
            )
            .await["state"],
            "acknowledged"
        );
    }
    // This caller saw the ID before the winner committed. Its own reservation
    // was refused, and it must still be able to retire after ownership moves.
    let late_refusal = call(&contender, "gatt.ordered.recording-reclaim.reserve", serde_json::json!({"peerId":common::POLAR,"collectionId":group1,"recordingId":id1,"reclaimCapability":cap1,"requestId":second_next["requestId"],"callerKey":"loser"})).await;
    assert_eq!(late_refusal["state"], "not-reserved", "{late_refusal}");
    let _args2 = start_reclaim(&session2, &group2, &cap2, &id2, None).await;
    let ids = shared.lock().unwrap().opened.clone();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    assert_eq!(
        shared.lock().unwrap().open_dispatches,
        2,
        "one native open per owner, including the lost reply"
    );
    let denied=call(&session2,"gatt.ordered.recording-reclaim.next",serde_json::json!({"peerId":common::POLAR,"collectionId":group1,"recordingId":id1,"reclaimCapability":cap1})).await;
    assert_eq!(denied["state"], "not-reserved");
    assert_eq!(denied["error"]["code"], "ownership.denied");
    stop_recording(&session1, &id1).await;
    let first_shutdown = tokio::time::timeout(Duration::from_secs(30), host1.shutdown())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&first_shutdown).unwrap()["state"],
        "released"
    );
    assert_eq!(
        recording_control(&session1, &id1, true).await["ok"],
        false,
        "retired session cannot access its recorder"
    );
    assert_eq!(
        recording_control(&session2, &id2, true).await["ok"],
        true,
        "retiring first owner must not revoke second recorder"
    );
    {
        let source = shared.lock().unwrap();
        assert!(source.closed.contains(&ids[0]));
        assert!(!source.closed.contains(&ids[1]));
    }
    stop_recording(&session2, &id2).await;
    let second_shutdown = tokio::time::timeout(Duration::from_secs(30), host2.shutdown())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&second_shutdown).unwrap()["state"],
        "released"
    );
    wake1.abort();
    wake2.abort();
    let s = shared.lock().unwrap();
    assert!(s.lost_open && s.lost_close && s.lost_forget);
    assert_eq!(s.closed.len(), 2);
    assert_eq!(s.forgotten.len(), 2);
    assert!(s.close_dispatches >= 3, "lost close must replay");
    assert!(s.forget_dispatches >= 3, "lost forget must replay");
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}
