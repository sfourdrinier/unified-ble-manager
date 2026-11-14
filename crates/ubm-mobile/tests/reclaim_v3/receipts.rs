use crate::reclaim_v3_receipts::*;
use std::sync::{Arc, Barrier};
use std::thread;

fn graph(revision: u64) -> Graph {
    Graph {
        connection: 1,
        database: 2,
        owner: "owner".into(),
        attachment: "attachment".into(),
        revision,
    }
}
fn runtime(name: &str) -> Runtime {
    Runtime {
        host: "host".into(),
        session: "session".into(),
        runtime: name.into(),
        incarnation: if name == "runtime" { 1 } else { 2 },
    }
}
fn recorder(peer: &str) -> Recorder {
    Recorder {
        host: "host".into(),
        peer: peer.into(),
        group: format!("group-{peer}"),
        recording: "recording".into(),
        capability: format!("cap-{peer}"),
        incarnation: if peer == "a" { 1 } else { 2 },
    }
}
fn request(peer: &str, seq: u64) -> Request {
    Request {
        runtime: runtime("runtime"),
        sequence: seq,
        request_id: format!("request-{seq}"),
        recorder: recorder(peer),
        lease: "lease".into(),
        expected_owner: "old".into(),
        expected_mapping: 0,
        expected_graph: graph(1),
        expected_authority: 1,
    }
}
fn permit(req: &Request) -> String {
    format!(
        "permit-{}-{}-{}",
        req.runtime.incarnation, req.recorder.incarnation, req.sequence
    )
}
fn ready(core: &Core, req: &Request) {
    core.begin_acquisition(req, "attempt").unwrap();
    core.acquisition_ready(req, "attempt", &permit(req))
        .unwrap();
}
fn setup(slots: usize) -> Core {
    let core = Core::new("host".into(), 4, 4, slots).unwrap();
    core.register_runtime(runtime("runtime"), "lease".into())
        .unwrap();
    for peer in ["a", "b"] {
        core.register_recorder(recorder(peer), graph(1), 1, "old".into())
            .unwrap();
    }
    core
}

#[test]
fn commit_requires_acquired_permit() {
    let core = setup(2);
    let req = request("a", 0);
    core.start(&req).unwrap();
    assert_eq!(core.commit(&req, &permit(&req)), Err(Error::NotPending));
    assert_eq!(core.mapping(&req.recorder).unwrap(), "old");
}

#[test]
fn duplicate_graph_event_is_idempotent_but_conflict_is_not() {
    let core = setup(1);
    assert_eq!(core.update_graph(&recorder("a"), graph(1)), Ok(()));
    let mut conflict = graph(1);
    conflict.attachment.push('x');
    assert_eq!(
        core.update_graph(&recorder("a"), conflict),
        Err(Error::Sequence)
    );
}

#[test]
fn fence_overtakes_unseen_dispatch_and_survives_concurrent_late_submission() {
    let core = Arc::new(setup(4));
    let req = request("a", 0);
    assert_eq!(core.fence(&req).unwrap().outcome, Outcome::FencedNoEffect);
    let barrier = Arc::new(Barrier::new(9));
    let mut threads = Vec::new();
    for _ in 0..8 {
        let (core, req, barrier) = (core.clone(), req.clone(), barrier.clone());
        threads.push(thread::spawn(move || {
            barrier.wait();
            core.start(&req).unwrap()
        }));
    }
    barrier.wait();
    for handle in threads {
        assert_eq!(handle.join().unwrap().outcome, Outcome::FencedNoEffect);
    }
    assert_eq!(core.mapping(&recorder("a")).unwrap(), "old");
}

#[test]
fn lost_reply_and_waiter_drop_leave_independently_queryable_truth_and_cleanup() {
    let core = Arc::new(setup(4));
    let req = request("a", 0);
    core.start(&req).unwrap();
    ready(&core, &req);
    let worker = {
        let core = core.clone();
        let req = req.clone();
        thread::spawn(move || {
            core.commit(&req, &permit(&req)).unwrap();
        })
    };
    worker.join().unwrap(); // pretend the reply was lost
    let receipt = core.status(&req).unwrap();
    assert_eq!(
        receipt.outcome,
        Outcome::Committed {
            graph: graph(1),
            authority: 1
        }
    );
    assert_eq!(receipt.cleanup, Cleanup::Pending);
    assert_eq!(core.fence(&req).unwrap(), receipt);
    core.cleanup_failed(&req, &permit(&req)).unwrap();
    assert_eq!(core.status(&req).unwrap().cleanup, Cleanup::Failed);
    core.cleanup_released(&req, &permit(&req)).unwrap();
    assert_eq!(core.status(&req).unwrap().outcome, receipt.outcome);
}

#[test]
fn stop_fences_pending_without_rewriting_prior_commit_and_other_peer_progresses() {
    let core = Arc::new(setup(4));
    let a = request("a", 0);
    core.start(&a).unwrap();
    ready(&core, &a);
    let b = request("b", 0);
    let barrier = Arc::new(Barrier::new(2));
    let thread = {
        let core = core.clone();
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            core.stop(&recorder("a")).unwrap();
        })
    };
    barrier.wait();
    core.start(&b).unwrap();
    ready(&core, &b);
    assert!(matches!(
        core.commit(&b, &permit(&b)).unwrap().outcome,
        Outcome::Committed { .. }
    ));
    thread.join().unwrap();
    assert_eq!(
        core.commit(&a, &permit(&a)).unwrap().outcome,
        Outcome::FencedNoEffect
    );
    assert_eq!(core.status(&a).unwrap().cleanup, Cleanup::Pending);
    let late = request("a", 1);
    assert_eq!(
        core.start(&late).unwrap().outcome,
        Outcome::Rejected(Reject::Authority)
    );
    assert_eq!(core.mapping(&recorder("a")).unwrap(), "old");
}

#[test]
fn stale_graph_and_authority_are_no_effect_and_committed_history_is_immutable() {
    let core = setup(5);
    let a = request("a", 0);
    core.start(&a).unwrap();
    ready(&core, &a);
    core.update_graph(&recorder("a"), graph(2)).unwrap();
    assert_eq!(
        core.commit(&a, &permit(&a)).unwrap().outcome,
        Outcome::Rejected(Reject::Graph)
    );
    let b = request("b", 0);
    core.start(&b).unwrap();
    ready(&core, &b);
    let committed = core.commit(&b, &permit(&b)).unwrap();
    core.stop(&recorder("b")).unwrap();
    assert_eq!(core.status(&b).unwrap(), committed);
    let mut stale = request("a", 1);
    stale.expected_graph = graph(2);
    core.advance_authority(&recorder("a")).unwrap();
    assert_eq!(
        core.start(&stale).unwrap().outcome,
        Outcome::Rejected(Reject::Authority)
    );
}

#[test]
fn exact_identity_rejects_duplicates_and_runtime_incarnations_do_not_collide() {
    let core = setup(4);
    let req = request("a", 0);
    core.start(&req).unwrap();
    let mut mismatch = req.clone();
    mismatch.request_id = "different".into();
    assert_eq!(core.fence(&mismatch), Err(Error::IdentityMismatch));
    mismatch = req.clone();
    mismatch.recorder.capability = "wrong".into();
    assert_eq!(core.status(&mismatch), Err(Error::IdentityMismatch));
    mismatch = req.clone();
    mismatch.expected_graph.attachment = "other".into();
    assert_eq!(core.start(&mismatch), Err(Error::IdentityMismatch));
    core.register_runtime(runtime("another-runtime"), "lease".into())
        .unwrap();
    let mut other = req.clone();
    other.runtime = runtime("another-runtime");
    assert_eq!(core.fence(&other).unwrap().outcome, Outcome::FencedNoEffect);
    assert_eq!(core.status(&req).unwrap().outcome, Outcome::Pending);
}

#[test]
fn authenticated_retirement_is_bounded_reusable_and_cannot_resurrect_late_dispatch() {
    let core = setup(1);
    let req = request("a", 0);
    core.start(&req).unwrap();
    ready(&core, &req);
    core.fence(&req).unwrap();
    assert_eq!(core.retire(&req), Err(Error::CleanupPending));
    core.cleanup_released(&req, &permit(&req)).unwrap();
    core.retire(&req).unwrap();
    core.ack_retire(&req).unwrap();
    core.forget_retire(&req).unwrap();
    let mut forged = req.clone();
    forged.lease = "wrong".into();
    assert_eq!(core.ack_forget(&forged), Err(Error::IdentityMismatch));
    assert_eq!(core.start(&request("a", 1)), Err(Error::Capacity));
    core.ack_forget(&req).unwrap();
    assert_eq!(core.start(&req), Err(Error::Retired));
    assert_eq!(core.status(&req), Err(Error::Retired));
    assert_eq!(
        core.start(&request("a", 1)).unwrap().outcome,
        Outcome::Pending
    );
}

#[test]
fn concurrent_fence_and_commit_have_one_truth() {
    for _ in 0..64 {
        let core = Arc::new(setup(2));
        let req = request("a", 0);
        core.start(&req).unwrap();
        ready(&core, &req);
        let barrier = Arc::new(Barrier::new(3));
        let commit = {
            let (core, req, barrier) = (core.clone(), req.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                core.commit(&req, &permit(&req)).unwrap()
            })
        };
        let fence = {
            let (core, req, barrier) = (core.clone(), req.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                core.fence(&req).unwrap()
            })
        };
        barrier.wait();
        let committed = commit.join().unwrap();
        let fenced = fence.join().unwrap();
        assert_eq!(committed, fenced);
        match committed.outcome {
            Outcome::Committed { .. } => assert_eq!(core.mapping(&recorder("a")).unwrap(), "lease"),
            Outcome::FencedNoEffect => assert_eq!(core.mapping(&recorder("a")).unwrap(), "old"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn retirement_order_retries_and_exact_fields_are_enforced() {
    let core = setup(2);
    let req = request("a", 0);
    core.fence(&req).unwrap();
    assert_eq!(core.ack_retire(&req), Err(Error::RetirementOrder));
    assert_eq!(core.forget_retire(&req), Err(Error::RetirementOrder));
    assert_eq!(core.ack_forget(&req), Err(Error::RetirementOrder));
    let mut variants = Vec::new();
    let mut v = req.clone();
    v.recorder.peer.push('x');
    variants.push(v);
    let mut v = req.clone();
    v.recorder.group.push('x');
    variants.push(v);
    let mut v = req.clone();
    v.recorder.recording.push('x');
    variants.push(v);
    let mut v = req.clone();
    v.lease.push('x');
    variants.push(v);
    let mut v = req.clone();
    v.expected_owner.push('x');
    variants.push(v);
    let mut v = req.clone();
    v.expected_authority += 1;
    variants.push(v);
    let mut v = req.clone();
    v.expected_mapping += 1;
    variants.push(v);
    let mut v = req.clone();
    v.expected_graph.connection += 1;
    variants.push(v);
    let mut v = req.clone();
    v.expected_graph.database += 1;
    variants.push(v);
    let mut v = req.clone();
    v.expected_graph.owner.push('x');
    variants.push(v);
    let mut v = req.clone();
    v.expected_graph.revision += 1;
    variants.push(v);
    for variant in variants {
        assert_eq!(core.status(&variant), Err(Error::IdentityMismatch));
    }
    for _ in 0..2 {
        core.retire(&req).unwrap();
        core.ack_retire(&req).unwrap();
        core.forget_retire(&req).unwrap();
    }
    core.ack_forget(&req).unwrap();
    assert_eq!(core.ack_forget(&req), Err(Error::Retired));
}

#[test]
fn terminal_revision_is_reserved_and_counter_never_wraps() {
    let core = Core::new("host".into(), 1, 2, 1).unwrap();
    assert_eq!(
        core.register_recorder(recorder("a"), graph(1), u64::MAX, "old".into()),
        Err(Error::Exhausted)
    );
    core.register_recorder(recorder("a"), graph(1), u64::MAX - 1, "old".into())
        .unwrap();
    assert_eq!(
        core.advance_authority(&recorder("a")),
        Err(Error::Exhausted)
    );
    core.stop(&recorder("a")).unwrap();
    core.stop(&recorder("a")).unwrap();
    assert_eq!(
        core.advance_authority(&recorder("a")),
        Err(Error::Exhausted)
    );
}

#[test]
fn unknown_or_gapped_sequence_cannot_consume_a_slot() {
    let core = setup(1);
    assert_eq!(core.fence(&request("a", 1)), Err(Error::Sequence));
    let mut forged = request("a", 0);
    forged.recorder.capability = "forged".into();
    assert_eq!(core.fence(&forged), Err(Error::UnknownRecorder));
    let mut forged = request("a", 0);
    forged.lease = "forged".into();
    assert_eq!(core.fence(&forged), Err(Error::UnknownLease));
    let mut forged = request("a", 0);
    forged.runtime.session = "foreign".into();
    assert_eq!(core.fence(&forged), Err(Error::UnknownRuntime));
    assert_eq!(core.status(&request("a", 0)), Err(Error::UnknownRequest));
    assert_eq!(
        core.fence(&request("a", 0)).unwrap().outcome,
        Outcome::FencedNoEffect
    );
}

#[test]
fn independent_peer_window_is_not_blocked_by_unseen_gap() {
    let core = setup(2);
    let a = request("a", 1);
    core.fence(&a).unwrap();
    core.fence(&request("a", 0)).unwrap();
    for seq in 0..6 {
        let b = request("b", seq);
        core.fence(&b).unwrap();
        core.retire(&b).unwrap();
        core.ack_retire(&b).unwrap();
        core.forget_retire(&b).unwrap();
        core.ack_forget(&b).unwrap();
    }
    assert_eq!(core.status(&a).unwrap().outcome, Outcome::FencedNoEffect);
}

#[test]
fn late_permit_remains_cleanup_debt_after_fence() {
    let core = setup(2);
    let req = request("a", 0);
    core.start(&req).unwrap();
    core.begin_acquisition(&req, "attempt").unwrap();
    core.fence(&req).unwrap();
    assert_eq!(core.retire(&req), Err(Error::CleanupPending));
    core.acquisition_ready(&req, "attempt", "permit").unwrap();
    core.acquisition_ready(&req, "attempt", "permit").unwrap();
    assert_eq!(
        core.acquisition_ready(&req, "attempt", "other"),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(
        core.cleanup_released(&req, "other"),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(core.retire(&req), Err(Error::CleanupPending));
    core.cleanup_released(&req, "permit").unwrap();
    core.retire(&req).unwrap();
}

#[test]
fn peers_can_have_equal_collection_identifiers() {
    let core = Core::new("host".into(), 1, 2, 2).unwrap();
    core.register_runtime(runtime("runtime"), "lease".into())
        .unwrap();
    let a = recorder("a");
    let mut b = recorder("b");
    b.group = a.group.clone();
    core.register_recorder(a.clone(), graph(1), 1, "old".into())
        .unwrap();
    core.register_recorder(b.clone(), graph(1), 1, "old".into())
        .unwrap();
    let mut req = request("b", 0);
    req.recorder = b.clone();
    core.start(&req).unwrap();
    ready(&core, &req);
    core.commit(&req, &permit(&req)).unwrap();
    assert_eq!(core.mapping(&a).unwrap(), "old");
    assert_eq!(core.mapping(&b).unwrap(), "lease");
}

#[test]
fn stop_and_commit_race_preserves_single_terminal_truth() {
    for _ in 0..64 {
        let core = Arc::new(setup(2));
        let req = request("a", 0);
        core.start(&req).unwrap();
        ready(&core, &req);
        let barrier = Arc::new(Barrier::new(3));
        let commit = {
            let (core, req, barrier) = (core.clone(), req.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                core.commit(&req, &permit(&req)).unwrap()
            })
        };
        let stop = {
            let (core, barrier) = (core.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                core.stop(&recorder("a")).unwrap();
            })
        };
        barrier.wait();
        let receipt = commit.join().unwrap();
        stop.join().unwrap();
        assert_eq!(core.status(&req).unwrap(), receipt);
        match receipt.outcome {
            Outcome::Committed { .. } => assert_eq!(core.mapping(&recorder("a")).unwrap(), "lease"),
            Outcome::FencedNoEffect => assert_eq!(core.mapping(&recorder("a")).unwrap(), "old"),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn competing_same_owner_and_aba_require_mapping_generation() {
    let core = Core::new("host".into(), 2, 1, 4).unwrap();
    core.register_runtime(runtime("runtime"), "lease".into())
        .unwrap();
    core.register_runtime(runtime("another-runtime"), "old".into())
        .unwrap();
    core.register_recorder(recorder("a"), graph(1), 1, "lease".into())
        .unwrap();
    let mut first = request("a", 0);
    first.expected_owner = "lease".into();
    let mut competitor = request("a", 1);
    competitor.expected_owner = "lease".into();
    core.start(&first).unwrap();
    core.start(&competitor).unwrap();
    ready(&core, &first);
    ready(&core, &competitor);
    let committed = core.commit(&first, &permit(&first)).unwrap();
    assert!(matches!(committed.outcome, Outcome::Committed { .. }));
    assert_eq!(core.commit(&first, &permit(&first)).unwrap(), committed);
    assert_eq!(
        core.commit(&competitor, &permit(&competitor))
            .unwrap()
            .outcome,
        Outcome::Rejected(Reject::Mapping)
    );
    assert_eq!(core.mapping_generation(&recorder("a")).unwrap(), 1);

    let mut back = request("a", 0);
    back.runtime = runtime("another-runtime");
    back.lease = "old".into();
    back.expected_owner = "lease".into();
    back.expected_mapping = 1;
    core.start(&back).unwrap();
    ready(&core, &back);
    core.commit(&back, &permit(&back)).unwrap();
    let mut forward = request("a", 2);
    forward.expected_owner = "old".into();
    forward.expected_mapping = 2;
    core.start(&forward).unwrap();
    ready(&core, &forward);
    core.commit(&forward, &permit(&forward)).unwrap();
    assert_eq!(core.mapping(&recorder("a")).unwrap(), "lease");
    let mut stale = request("a", 3);
    stale.expected_owner = "lease".into();
    assert_eq!(
        core.start(&stale).unwrap().outcome,
        Outcome::Rejected(Reject::Mapping)
    );
    assert_eq!(core.mapping_generation(&recorder("a")).unwrap(), 3);
}

#[test]
fn acquisition_dispatch_is_once_and_failed_result_settles_uncertainty() {
    let core = setup(1);
    let req = request("a", 0);
    core.start(&req).unwrap();
    assert_eq!(
        core.begin_acquisition(&req, "attempt"),
        Ok(AcquisitionDispatch::Dispatch)
    );
    assert_eq!(
        core.begin_acquisition(&req, "attempt"),
        Ok(AcquisitionDispatch::AlreadyRegistered)
    );
    assert_eq!(
        core.begin_acquisition(&req, "other"),
        Err(Error::IdentityMismatch)
    );
    core.stop(&req.recorder).unwrap();
    assert_eq!(core.retire(&req), Err(Error::CleanupPending));
    assert_eq!(
        core.acquisition_failed(&req, "other"),
        Err(Error::IdentityMismatch)
    );
    core.acquisition_failed(&req, "attempt").unwrap();
    core.acquisition_failed(&req, "attempt").unwrap();
    assert_eq!(
        core.begin_acquisition(&req, "attempt"),
        Ok(AcquisitionDispatch::AlreadyRegistered)
    );
    assert_eq!(
        core.acquisition_ready(&req, "attempt", "permit"),
        Err(Error::IdentityMismatch)
    );
    core.retire(&req).unwrap();
}

#[test]
fn lifecycle_reclaims_capacity_only_after_authenticated_safe_teardown() {
    let core = Core::new("host".into(), 1, 1, 2).unwrap();
    let rt = runtime("runtime");
    let rec = recorder("a");
    core.register_runtime(rt.clone(), "lease".into()).unwrap();
    core.register_recorder(rec.clone(), graph(1), 1, "old".into())
        .unwrap();
    let req = request("a", 0);
    core.start(&req).unwrap();
    ready(&core, &req);
    core.stop(&rec).unwrap();
    assert_eq!(core.retire_runtime(&rt, "wrong"), Err(Error::UnknownLease));
    assert_eq!(
        core.retire_runtime(&rt, "lease"),
        Err(Error::CleanupPending)
    );
    assert_eq!(core.retire_recorder(&rec), Err(Error::CleanupPending));
    core.cleanup_released(&req, &permit(&req)).unwrap();
    core.retire(&req).unwrap();
    core.ack_retire(&req).unwrap();
    core.forget_retire(&req).unwrap();
    core.ack_forget(&req).unwrap();
    core.retire_recorder(&rec).unwrap();
    core.retire_runtime(&rt, "lease").unwrap();
    assert_eq!(
        core.register_runtime(rt.clone(), "lease".into()),
        Err(Error::Retired)
    );
    assert_eq!(
        core.register_recorder(rec.clone(), graph(1), 1, "old".into()),
        Err(Error::Retired)
    );
    assert_eq!(core.start(&req), Err(Error::UnknownRuntime));
    let mut rt2 = rt.clone();
    rt2.incarnation += 1;
    let mut rec2 = rec.clone();
    rec2.incarnation += 1;
    core.register_runtime(rt2, "lease".into()).unwrap();
    core.register_recorder(rec2, graph(1), 1, "old".into())
        .unwrap();
    assert_eq!(core.start(&req), Err(Error::UnknownRuntime));
}

#[test]
fn out_of_order_final_ack_replays_exactly_until_compacted() {
    let core = setup(2);
    let later = request("a", 1);
    core.fence(&later).unwrap();
    core.retire(&later).unwrap();
    core.ack_retire(&later).unwrap();
    core.forget_retire(&later).unwrap();
    core.ack_forget(&later).unwrap();
    assert_eq!(core.ack_forget(&later), Ok(()));
    let mut forged = later.clone();
    forged.request_id.push('x');
    assert_eq!(core.ack_forget(&forged), Err(Error::IdentityMismatch));
    let first = request("a", 0);
    core.fence(&first).unwrap();
    core.retire(&first).unwrap();
    core.ack_retire(&first).unwrap();
    core.forget_retire(&first).unwrap();
    core.ack_forget(&first).unwrap();
    assert_eq!(core.ack_forget(&later), Err(Error::Retired));
    assert_eq!(core.ack_forget(&forged), Err(Error::Retired));
}

#[test]
fn permit_cannot_be_owned_by_two_requests_and_conflict_stays_uncertain() {
    let core = setup(2);
    let a = request("a", 0);
    let b = request("b", 0);
    core.start(&a).unwrap();
    core.start(&b).unwrap();
    ready(&core, &a);
    core.begin_acquisition(&b, "attempt").unwrap();
    assert_eq!(
        core.acquisition_ready(&b, "attempt", &permit(&a)),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(core.commit(&b, &permit(&a)), Err(Error::NotPending));
    core.fence(&b).unwrap();
    assert_eq!(core.retire(&b), Err(Error::CleanupPending));
}

#[test]
fn fenced_unseen_request_never_dispatches_acquisition() {
    let core = setup(1);
    let req = request("a", 0);
    core.fence(&req).unwrap();
    assert_eq!(
        core.begin_acquisition(&req, "attempt"),
        Err(Error::NotPending)
    );
    assert_eq!(
        core.acquisition_ready(&req, "attempt", "permit"),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(core.status(&req).unwrap().cleanup, Cleanup::NotRequired);
}

#[test]
fn wrong_permit_cannot_commit_or_release() {
    let core = setup(1);
    let req = request("a", 0);
    core.start(&req).unwrap();
    ready(&core, &req);
    assert_eq!(core.commit(&req, "wrong"), Err(Error::IdentityMismatch));
    assert_eq!(core.mapping(&req.recorder).unwrap(), "old");
    core.fence(&req).unwrap();
    assert_eq!(
        core.cleanup_released(&req, "wrong"),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(core.retire(&req), Err(Error::CleanupPending));
}

#[test]
fn lifecycle_capacity_recycles_under_repeated_incarnations() {
    let core = Core::new("host".into(), 1, 1, 1).unwrap();
    for incarnation in 1..10 {
        let mut rt = runtime("runtime");
        rt.incarnation = incarnation;
        let mut rec = recorder("a");
        rec.incarnation = incarnation;
        core.register_runtime(rt.clone(), "lease".into()).unwrap();
        core.register_recorder(rec.clone(), graph(1), 1, "old".into())
            .unwrap();
        core.stop(&rec).unwrap();
        core.retire_recorder(&rec).unwrap();
        core.retire_runtime(&rt, "lease").unwrap();
        assert_eq!(
            core.register_runtime(rt, "lease".into()),
            Err(Error::Retired)
        );
        assert_eq!(
            core.register_recorder(rec, graph(1), 1, "old".into()),
            Err(Error::Retired)
        );
    }
}

#[test]
fn same_owner_competing_commits_have_one_winner() {
    for _ in 0..32 {
        let core = Arc::new(Core::new("host".into(), 1, 1, 2).unwrap());
        core.register_runtime(runtime("runtime"), "lease".into())
            .unwrap();
        core.register_recorder(recorder("a"), graph(1), 1, "lease".into())
            .unwrap();
        let mut a = request("a", 0);
        let mut b = request("a", 1);
        a.expected_owner = "lease".into();
        b.expected_owner = "lease".into();
        for req in [&a, &b] {
            core.start(req).unwrap();
            ready(&core, req);
        }
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = [a, b]
            .into_iter()
            .map(|req| {
                let (core, barrier) = (core.clone(), barrier.clone());
                thread::spawn(move || {
                    barrier.wait();
                    core.commit(&req, &permit(&req)).unwrap().outcome
                })
            })
            .collect();
        barrier.wait();
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| matches!(o, Outcome::Committed { .. }))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| **o == Outcome::Rejected(Reject::Mapping))
                .count(),
            1
        );
    }
}
