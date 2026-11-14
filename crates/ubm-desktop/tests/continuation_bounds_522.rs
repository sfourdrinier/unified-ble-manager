use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use ubm_desktop::continuation::{
    ContinuationFuture, ContinuationHost, ContinuationSession, NativeContinuation, Result,
    continuation_drain_error, envelope, legacy_evidence_error,
};
use ubm_desktop::continuation_outbox::{DrainClaim, LegacyEvidenceOwner, Outbox, WakeSink};

struct Wake;
impl WakeSink for Wake {
    fn wake(&self, _: u64) {}
}
struct Session {
    outbox: Outbox,
    cleanup_tail: AtomicBool,
    quiesced: AtomicBool,
    lose_source_ack: AtomicBool,
    direct_legacy: bool,
}
struct Host(Arc<Session>);
impl ContinuationHost for Host {
    fn open_session(&self) -> Result<Arc<dyn ContinuationSession>> {
        Ok(self.0.clone())
    }
}
impl ContinuationSession for Session {
    fn legacy_claim_compatible(&self) -> bool {
        true
    }
    fn call<'a>(&'a self, op: &'a str, _: &'a str) -> ContinuationFuture<'a> {
        Box::pin(async move {
            envelope(Ok(match op {
                "session.quiesce" => {
                    self.quiesced.store(true, Ordering::SeqCst);
                    json!({"state":"sealed","afterCutoffItems":0,"afterCutoffBytes":0})
                }
                "session.continuation-dispose" => {
                    if self.cleanup_tail.swap(false, Ordering::SeqCst) {
                        for n in 0..3 {
                            self.outbox.push_control(json!({"t":"cleanup", "n":n, "payload":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}));
                        }
                    }
                    json!({"state":"released","afterCutoffItems":0,"afterCutoffBytes":0})
                }
                _ => json!({}),
            }))
        })
    }
    fn legacy_drain(
        &self,
        _: &ubm_desktop::continuation_outbox::LegacyEvidenceOwner,
        items: u32,
        bytes: u32,
    ) -> Option<Result<Value>> {
        self.direct_legacy
            .then(|| Ok(self.outbox.drain(items as usize, bytes as usize)))
    }
    fn claim_drain(&self, items: u32, bytes: u32) -> Result<Option<DrainClaim>> {
        self.outbox
            .claim_drain(items as usize, bytes as usize)
            .map_err(continuation_drain_error)
    }
    fn claim_drain_v1(
        &self,
        evidence: &LegacyEvidenceOwner,
        items: u32,
        bytes: u32,
    ) -> Option<Result<Option<DrainClaim>>> {
        Some(self.claim_drain(items, bytes).and_then(|claim| {
            if let Some(claim) = &claim {
                evidence
                    .stage_exact_records(&claim.records)
                    .map_err(legacy_evidence_error)?;
            }
            Ok(claim)
        }))
    }
    fn acknowledge_drain(&self, id: u64) -> Result<()> {
        if self.outbox.acknowledge_drain(id) {
            if self.lose_source_ack.swap(false, Ordering::SeqCst) {
                return Err(json!({"code":"lost-source-ack"}));
            }
            Ok(())
        } else {
            Err(json!({"code":"mismatch"}))
        }
    }
    fn has_retained_tail(&self) -> Result<bool> {
        self.outbox
            .has_retained_tail()
            .map_err(continuation_drain_error)
    }
}
async fn started(records: usize, cleanup_tail: bool) -> NativeContinuation {
    let session = Arc::new(Session {
        outbox: Outbox::new(1, Arc::new(Wake)),
        cleanup_tail: AtomicBool::new(cleanup_tail),
        quiesced: AtomicBool::new(false),
        lose_source_ack: AtomicBool::new(false),
        direct_legacy: true,
    });
    for n in 0..records {
        session.outbox.push_control(json!({"t":"value", "n":n, "payload":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}));
    }
    let engine = NativeContinuation::new(Arc::new(Host(session)));
    engine
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    engine
}
fn records(claim: &Value) -> Vec<Value> {
    let batch: Value = serde_json::from_str(claim["batches"][0].as_str().unwrap()).unwrap();
    batch["records"].as_array().unwrap().clone()
}
#[tokio::test]
async fn next_prepare_respects_smaller_item_bound() {
    let engine = started(5, false).await;
    let first = engine.prepare_claim_versioned(1, 2, 4096).await.unwrap();
    engine
        .acknowledge_claim_v1(first["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    let next = engine.prepare_claim_versioned(1, 1, 4096).await.unwrap();
    assert_eq!(records(&next).len(), 1);
}
#[tokio::test]
async fn next_prepare_respects_smaller_byte_bound() {
    let engine = started(5, false).await;
    let first = engine.prepare_claim_versioned(1, 2, 4096).await.unwrap();
    engine
        .acknowledge_claim_v1(first["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    let next = engine.prepare_claim_versioned(1, 10, 180).await.unwrap();
    let records = records(&next);
    assert_eq!(records.len(), 1);
    assert!(serde_json::to_string(&records).unwrap().len() <= 180);
}
#[tokio::test]
async fn cleanup_tail_obeys_new_prepare_bounds() {
    let engine = started(1, true).await;
    let first = engine.prepare_claim_versioned(1, 256, 4096).await.unwrap();
    assert_eq!(
        engine
            .acknowledge_claim_v1(first["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        false
    );
    let next = engine.prepare_claim_versioned(1, 1, 180).await.unwrap();
    let records = records(&next);
    assert_eq!(records.len(), 1);
    assert!(serde_json::to_string(&records).unwrap().len() <= 180);
}

#[test]
fn tail_inspection_retains_control_loss_without_selecting_a_claim() {
    let outbox = Outbox::new(2, Arc::new(Wake));
    assert!(!outbox.has_retained_tail().unwrap());
    outbox.note_control_loss(3);
    assert!(outbox.has_retained_tail().unwrap());
    assert!(outbox.has_retained_tail().unwrap());
    let claim = outbox.claim_drain(1, 4096).unwrap().unwrap();
    assert_eq!(claim.records[0]["count"], 3);
    assert!(outbox.acknowledge_drain(claim.id));
    assert!(!outbox.has_retained_tail().unwrap());

    let legacy = Outbox::new(4, Arc::new(Wake));
    legacy.push_control(json!({"t":"legacy"}));
    assert!(legacy.has_retained_tail().unwrap());
    assert_eq!(legacy.drain(1, 4096)["records"][0]["t"], "legacy");
}

#[tokio::test]
async fn reduced_bounds_deliver_every_ordered_record_and_cleanup_control() {
    let engine = started(5, true).await;
    let mut observed = Vec::new();
    for attempt in 0..16 {
        let claim = engine
            .prepare_claim_versioned(1, if attempt == 0 { 2 } else { 1 }, 180)
            .await
            .unwrap();
        for batch in claim["batches"].as_array().unwrap() {
            let batch: Value = serde_json::from_str(batch.as_str().unwrap()).unwrap();
            observed.extend(batch["records"].as_array().unwrap().iter().map(|record| {
                (
                    record["t"].as_str().unwrap().to_owned(),
                    record["n"].as_u64().unwrap(),
                )
            }));
        }
        let receipt = engine
            .acknowledge_claim_v1(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap();
        if receipt["disposed"] == true {
            assert_eq!(
                observed,
                [
                    ("value", 0),
                    ("value", 1),
                    ("value", 2),
                    ("value", 3),
                    ("value", 4),
                    ("cleanup", 0),
                    ("cleanup", 1),
                    ("cleanup", 2)
                ]
                .map(|(kind, n)| (kind.to_owned(), n))
            );
            return;
        }
    }
    panic!("cleanup did not complete");
}

#[tokio::test]
async fn unsupported_handoff_version_fails_before_quiescence() {
    let session = Arc::new(Session {
        outbox: Outbox::new(3, Arc::new(Wake)),
        cleanup_tail: AtomicBool::new(false),
        quiesced: AtomicBool::new(false),
        lose_source_ack: AtomicBool::new(false),
        direct_legacy: true,
    });
    session.outbox.push_control(json!({"t":"value"}));
    let engine = NativeContinuation::new(Arc::new(Host(session.clone())));
    engine
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    assert!(engine.prepare_claim_versioned(0, 1, 4096).await.is_err());
    assert!(!session.quiesced.load(Ordering::SeqCst));
    assert!(session.outbox.claim_drain(1, 4096).unwrap().is_some());
}

#[tokio::test]
async fn legacy_prepare_aggregates_batches_and_preserves_partial_tail() {
    let engine = started(35, false).await;
    let claim = engine.prepare_claim(1, 4096).await.unwrap();
    let batches = claim["batches"].as_array().unwrap();
    assert_eq!(batches.len(), 32);
    for (n, batch) in batches.iter().enumerate() {
        let batch: Value = serde_json::from_str(batch.as_str().unwrap()).unwrap();
        assert_eq!(batch["records"][0]["n"], n);
        assert_eq!(batch["more"], true);
    }
    assert!(claim["disposeFailure"].is_string());
    let token = claim["claimToken"].as_str().unwrap();
    let receipt = engine.acknowledge_claim(token).await.unwrap();
    assert_eq!(receipt["disposed"], false);
    assert_eq!(engine.acknowledge_claim(token).await.unwrap(), receipt);
    let tail = engine.prepare_claim(1, 4096).await.unwrap();
    assert_eq!(tail["batches"].as_array().unwrap().len(), 3);
    let last: Value = serde_json::from_str(tail["batches"][2].as_str().unwrap()).unwrap();
    assert_eq!(last["more"], false);
    assert_eq!(
        engine
            .acknowledge_claim(tail["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
}

#[tokio::test]
async fn legacy_stages_source_ack_before_retry_and_keeps_data_control_order() {
    let session = Arc::new(Session {
        outbox: Outbox::new(11, Arc::new(Wake)),
        cleanup_tail: AtomicBool::new(false),
        quiesced: AtomicBool::new(false),
        lose_source_ack: AtomicBool::new(true),
        direct_legacy: false,
    });
    session.outbox.push_data(json!({"t":"data","n":0})).unwrap();
    session.outbox.push_control(json!({"t":"control","n":1}));
    session.outbox.push_data(json!({"t":"data","n":2})).unwrap();
    let engine = NativeContinuation::new(Arc::new(Host(session)));
    engine
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    assert_eq!(
        engine.prepare_claim(1, 4096).await.unwrap_err()["code"],
        "lost-source-ack"
    );
    assert!(engine.prepare_claim_versioned(1, 1, 4096).await.is_err());
    let claim = engine.prepare_claim(9, 4096).await.unwrap();
    assert_eq!(engine.prepare_claim(1, 4096).await.unwrap(), claim);
    let batches = claim["batches"].as_array().unwrap();
    assert_eq!(batches.len(), 3, "retry preserves original batch bounds");
    for (n, batch) in batches.iter().enumerate() {
        let batch: Value = serde_json::from_str(batch.as_str().unwrap()).unwrap();
        assert_eq!(batch["records"][0]["n"], n);
        assert_eq!(batch["more"], n != 2);
    }
    let token = claim["claimToken"].as_str().unwrap();
    assert!(engine.acknowledge_claim_v1(token).await.is_err());
    let receipt = engine.acknowledge_claim(token).await.unwrap();
    assert_eq!(receipt["disposed"], true);
    assert_eq!(engine.acknowledge_claim(token).await.unwrap(), receipt);
}

#[tokio::test]
async fn v1_and_legacy_cannot_mix_prepare_or_acknowledgement() {
    let engine = started(2, false).await;
    let claim = engine.prepare_claim_versioned(1, 1, 4096).await.unwrap();
    let token = claim["claimToken"].as_str().unwrap();
    assert!(engine.prepare_claim(10, 4096).await.is_err());
    assert!(engine.acknowledge_claim(token).await.is_err());
    let receipt = engine.acknowledge_claim_v1(token).await.unwrap();
    assert_eq!(receipt["disposed"], false);
    assert!(engine.acknowledge_claim(token).await.is_err());
    assert_eq!(engine.acknowledge_claim_v1(token).await.unwrap(), receipt);
    engine.retire_claim_receipt(token).await.unwrap();
    assert!(engine.acknowledge_claim_v1(token).await.is_err());
}

#[tokio::test]
async fn legacy_small_byte_bound_still_delivers_first_oversized_record() {
    let engine = started(1, false).await;
    let claim = engine.prepare_claim(1, 1).await.unwrap();
    assert_eq!(records(&claim)[0]["n"], 0);
    assert_eq!(
        engine
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
}

#[tokio::test]
async fn legacy_byte_budget_matches_521_control_accounting() {
    let engine = started(5, false).await;
    let claim = engine.prepare_claim(5, 1).await.unwrap();
    assert_eq!(claim["batches"].as_array().unwrap().len(), 1);
    assert_eq!(records(&claim).len(), 5);
    assert_eq!(
        engine
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
}

#[tokio::test]
async fn legacy_direct_drain_preserves_data_and_control_order() {
    let session = Arc::new(Session {
        outbox: Outbox::new(12, Arc::new(Wake)),
        cleanup_tail: AtomicBool::new(true),
        quiesced: AtomicBool::new(false),
        lose_source_ack: AtomicBool::new(false),
        direct_legacy: true,
    });
    session.outbox.push_data(json!({"t":"data","n":0})).unwrap();
    session.outbox.push_control(json!({"t":"control","n":1}));
    session.outbox.push_data(json!({"t":"data","n":2})).unwrap();
    let engine = NativeContinuation::new(Arc::new(Host(session)));
    engine
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    let claim = engine.prepare_claim(1, 4096).await.unwrap();
    let values: Vec<Value> = claim["batches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|batch| {
            serde_json::from_str::<Value>(batch.as_str().unwrap()).unwrap()["records"][0]["n"]
                .clone()
        })
        .collect();
    assert_eq!(values, vec![json!(0), json!(1), json!(2)]);
    let receipt = engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(receipt["disposed"], false);
    let cleanup = engine.prepare_claim(1, 180).await.unwrap();
    assert_eq!(cleanup["batches"].as_array().unwrap().len(), 3);
    assert_eq!(
        engine
            .acknowledge_claim(cleanup["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
}
