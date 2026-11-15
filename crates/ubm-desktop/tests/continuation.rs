use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use ubm_desktop::continuation::{
    ContinuationFuture, ContinuationHost, ContinuationSession, NativeContinuation, Result,
    envelope, legacy_evidence_error,
};
use ubm_desktop::continuation_outbox::LegacyEvidenceOwner;

#[derive(Default)]
struct Session {
    batches: Mutex<VecDeque<String>>,
    calls: Mutex<Vec<String>>,
    disposal_tail: Mutex<Option<String>>,
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
            self.calls.lock().unwrap().push(op.to_owned());
            envelope(Ok(match op {
                "session.quiesce" => {
                    json!({"state":"sealed","afterCutoffItems":2,"afterCutoffBytes":8})
                }
                "session.continuation-dispose" => {
                    if let Some(tail) = self.disposal_tail.lock().unwrap().take() {
                        self.batches.lock().unwrap().push_back(tail);
                    }
                    json!({"state":"released","afterCutoffItems":3,"afterCutoffBytes":12})
                }
                _ => json!({}),
            }))
        })
    }
    fn claim_drain(
        &self,
        _: u32,
        _: u32,
    ) -> Result<Option<ubm_desktop::continuation_outbox::DrainClaim>> {
        let batches = self.batches.lock().unwrap();
        let Some(batch) = batches.front() else {
            return Ok(None);
        };
        let value: Value = serde_json::from_str(batch).map_err(|_| json!({"code":"malformed"}))?;
        let records = value["records"]
            .as_array()
            .ok_or_else(|| json!({"code":"malformed"}))?
            .clone();
        if !value["more"].is_boolean()
            || !value["controlLost"].is_u64()
            || value.as_object().unwrap().len() != 3
        {
            return Err(json!({"code":"malformed"}));
        }
        Ok(Some(ubm_desktop::continuation_outbox::DrainClaim {
            id: 1,
            records,
            control_lost: value["controlLost"].as_u64().unwrap(),
        }))
    }
    fn claim_drain_v1(
        &self,
        evidence: &LegacyEvidenceOwner,
        items: u32,
        bytes: u32,
    ) -> Option<Result<Option<ubm_desktop::continuation_outbox::DrainClaim>>> {
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
        assert_eq!(id, 1);
        self.batches.lock().unwrap().pop_front();
        Ok(())
    }
    fn has_retained_tail(&self) -> Result<bool> {
        Ok(!self.batches.lock().unwrap().is_empty())
    }
}
fn batch(more: bool) -> String {
    json!({"more":more,"records":[],"controlLost":0}).to_string()
}
async fn started(batches: Vec<String>) -> (NativeContinuation, Arc<Session>) {
    let session = Arc::new(Session {
        batches: Mutex::new(batches.into()),
        calls: Mutex::default(),
        disposal_tail: Mutex::default(),
    });
    let engine = NativeContinuation::new(Arc::new(Host(session.clone())));
    engine
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    (engine, session)
}

#[tokio::test]
async fn disposal_tail_remains_owned_until_a_second_handoff() {
    let (engine, session) = started(vec![batch(false)]).await;
    *session.disposal_tail.lock().unwrap() = Some(batch(false));
    let first = engine.prepare_claim(1, 1024).await.unwrap();
    let receipt = engine
        .acknowledge_claim(first["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        receipt["disposed"], false,
        "cleanup-created tail remains to transfer"
    );
    let tail = engine.prepare_claim(1, 1024).await.unwrap();
    assert_ne!(tail["claimToken"], first["claimToken"]);
    assert_eq!(
        engine
            .acknowledge_claim(tail["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
    assert_eq!(
        session
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|op| *op == "session.continuation-dispose")
            .count(),
        1
    );
}

#[tokio::test]
async fn preparation_does_not_remove_multiple_unacknowledged_prefixes() {
    let (engine, session) = started((0..3).map(|i| batch(i < 2)).collect()).await;
    let claim = engine.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    assert_eq!(claim["batches"].as_array().unwrap().len(), 1);
    assert_eq!(
        session.batches.lock().unwrap().len(),
        3,
        "the source must retain its prefix until the handoff is acknowledged"
    );
}

#[tokio::test]
async fn incomplete_receipt_replays_and_tail_remains_owned() {
    let (engine, session) = started(vec![batch(true), batch(false)]).await;
    let prefix = engine.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    assert_eq!(
        engine.prepare_claim_versioned(1, 1, 1024).await.unwrap(),
        prefix
    );
    let token = prefix["claimToken"].as_str().unwrap();
    let receipt = engine.acknowledge_claim_v1(token).await.unwrap();
    assert_eq!(receipt["disposed"], false);
    assert_eq!(engine.acknowledge_claim_v1(token).await.unwrap(), receipt);
    assert_eq!(session.batches.lock().unwrap().len(), 1);
    let tail = engine.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    assert_ne!(tail["claimToken"], prefix["claimToken"]);
    assert_eq!(engine.acknowledge_claim_v1(token).await.unwrap(), receipt);
    let ack = engine
        .acknowledge_claim_v1(tail["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(ack["disposed"], true);
    assert_eq!(ack["afterCutoffLoss"], json!({"items":3,"bytes":12}));
}

#[tokio::test]
async fn malformed_tail_stays_owned_and_reports_retryable_failure() {
    let (engine, session) = started(vec![batch(true), "not-json".to_owned()]).await;
    let prefix = engine.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    let receipt = engine
        .acknowledge_claim_v1(prefix["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(receipt["disposed"], false);
    assert!(engine.prepare_claim_versioned(1, 1, 1024).await.is_err());
    assert_eq!(session.batches.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn declaration_reservation_rejects_a_stale_wake_after_persistence_commits() {
    let session = Arc::new(Session::default());
    let engine = NativeContinuation::new(Arc::new(Host(session.clone())));
    let a = r#"{"onAppearance":"native","peerId":"peer-a"}"#;
    let b = r#"{"onAppearance":"native","peerId":"peer-b"}"#;
    engine.seed_declaration(a).unwrap();
    let reserved = engine.reserve_declaration(b).unwrap();
    let token = reserved["reservationToken"].as_str().unwrap();
    assert!(
        engine.execute("peer-a", a).await.is_err(),
        "read A before reservation cannot race native session publication"
    );
    assert!(
        engine.execute("peer-b", b).await.is_err(),
        "not yet persisted B cannot start either"
    );
    assert!(engine.commit_declaration("stale-token").is_err());
    engine.commit_declaration(token).unwrap();
    assert!(
        engine.execute("peer-a", a).await.is_err(),
        "captured A stays rejected after B commits"
    );
    assert!(
        engine.seed_declaration(a).is_err(),
        "cold-start seeding cannot overwrite live authority"
    );
    assert!(session.calls.lock().unwrap().is_empty());
    engine.execute("peer-b", b).await.unwrap();
    assert!(
        engine.reserve_declaration(a).is_err(),
        "live resources pin declaration B"
    );
}

#[tokio::test]
async fn cancelled_persistence_reservation_restores_previous_execution_authority() {
    let session = Arc::new(Session::default());
    let engine = NativeContinuation::new(Arc::new(Host(session)));
    let a = r#"{"onAppearance":"native","peerId":"peer-a"}"#;
    engine.seed_declaration(a).unwrap();
    let reserved = engine
        .reserve_declaration(r#"{"onAppearance":"record-only"}"#)
        .unwrap();
    engine
        .cancel_declaration(reserved["reservationToken"].as_str().unwrap())
        .unwrap();
    engine.execute("peer-a", a).await.unwrap();
}

#[tokio::test]
async fn opaque_desktop_peers_preserve_case_and_never_alias_declaration_authority() {
    let session = Arc::new(Session::default());
    let engine = NativeContinuation::new(Arc::new(Host(session.clone())));
    let peer = "hci1/dev_AA_BB_CC_DD_EE_FF";
    let other = "hci1/dev_aa_bb_cc_dd_ee_ff";
    let declaration = json!({"onAppearance":"native","peerId":peer}).to_string();
    let replacement = json!({"onAppearance":"native","peerId":other}).to_string();
    engine.seed_declaration(&declaration).unwrap();
    assert!(
        engine.seed_declaration(&replacement).is_err(),
        "case-distinct opaque identities must not seed the same authority"
    );
    assert!(
        engine.execute(other, &declaration).await.is_err(),
        "a declaration must not authorize a case-distinct host ID"
    );
    assert!(session.calls.lock().unwrap().is_empty());
    let completed = engine.execute(peer, &declaration).await.unwrap();
    assert_eq!(completed["peerAddress"], peer);
    assert!(engine.reserve_declaration(&replacement).is_err());
}

#[tokio::test]
async fn foreign_executor_claim_token_cannot_acknowledge_another_source() {
    let (first, _) = started(vec![batch(false)]).await;
    let (second, second_session) = started(vec![batch(false)]).await;
    let first_claim = first.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    let second_claim = second.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    assert_ne!(first_claim["claimToken"], second_claim["claimToken"]);
    assert!(
        second
            .acknowledge_claim_v1(first_claim["claimToken"].as_str().unwrap())
            .await
            .is_err()
    );
    assert_eq!(second_session.batches.lock().unwrap().len(), 1);
    assert!(second.acknowledge_claim_v1("invalid").await.is_err());
    assert_eq!(second_session.batches.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn lost_receipts_remain_exact_after_new_prefixes_and_new_execution() {
    let (engine, _) = started(vec![batch(true), batch(true), batch(false)]).await;
    let first = engine.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    let first_token = first["claimToken"].as_str().unwrap().to_owned();
    let first_receipt = engine.acknowledge_claim_v1(&first_token).await.unwrap();
    let second = engine.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    let second_token = second["claimToken"].as_str().unwrap().to_owned();
    let second_receipt = engine.acknowledge_claim_v1(&second_token).await.unwrap();
    let third = engine.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    let third_token = third["claimToken"].as_str().unwrap().to_owned();
    let final_receipt = engine.acknowledge_claim_v1(&third_token).await.unwrap();
    assert_eq!(final_receipt["disposed"], true);
    engine
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    let new_claim = engine.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    assert_ne!(new_claim["claimToken"], third["claimToken"]);
    assert_eq!(
        engine.acknowledge_claim_v1(&first_token).await.unwrap(),
        first_receipt
    );
    assert_eq!(
        engine.acknowledge_claim_v1(&second_token).await.unwrap(),
        second_receipt
    );
    assert_eq!(
        engine.acknowledge_claim_v1(&third_token).await.unwrap(),
        final_receipt
    );
}

#[tokio::test]
async fn explicit_retirement_forgets_only_the_selected_acknowledged_receipt() {
    let (engine, _) = started(vec![batch(true), batch(false)]).await;
    let first = engine.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    let first_token = first["claimToken"].as_str().unwrap();
    let receipt = engine.acknowledge_claim_v1(first_token).await.unwrap();
    let second = engine.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    let second_token = second["claimToken"].as_str().unwrap();
    assert!(engine.retire_claim_receipt(second_token).await.is_err());
    assert_eq!(
        engine.acknowledge_claim_v1(first_token).await.unwrap(),
        receipt
    );
    engine.retire_claim_receipt(first_token).await.unwrap();
    assert!(engine.acknowledge_claim_v1(first_token).await.is_err());
    assert!(engine.acknowledge_claim_v1(second_token).await.is_ok());
    engine.retire_claim_receipt(second_token).await.unwrap();
    assert!(engine.acknowledge_claim_v1(second_token).await.is_err());
}

#[tokio::test]
async fn v1_retirement_never_discards_a_legacy_receipt() {
    let (engine, _) = started(vec![batch(false)]).await;
    let claim = engine.prepare_claim(1, 1024).await.unwrap();
    let token = claim["claimToken"].as_str().unwrap();
    let receipt = engine.acknowledge_claim(token).await.unwrap();
    assert!(engine.retire_claim_receipt(token).await.is_err());
    assert_eq!(engine.acknowledge_claim(token).await.unwrap(), receipt);
    engine
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    assert!(engine.retire_claim_receipt(token).await.is_err());
    assert_eq!(engine.acknowledge_claim(token).await.unwrap(), receipt);
}
