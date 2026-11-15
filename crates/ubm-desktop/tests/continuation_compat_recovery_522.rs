use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use ubm_desktop::continuation::{
    ContinuationFuture, ContinuationHost, ContinuationSession, NativeContinuation, Result,
    continuation_drain_error, envelope,
};
use ubm_desktop::continuation_outbox::{DrainClaim, Outbox, WakeSink};
struct Wake;
impl WakeSink for Wake {
    fn wake(&self, _: u64) {}
}
struct Session {
    outbox: Outbox,
    quiesced: AtomicBool,
    legacy: bool,
}
struct Host(Arc<Session>);
impl ContinuationHost for Host {
    fn open_session(&self) -> Result<Arc<dyn ContinuationSession>> {
        Ok(self.0.clone())
    }
}
impl ContinuationSession for Session {
    fn legacy_claim_compatible(&self) -> bool {
        self.legacy
    }
    fn call<'a>(&'a self, op: &'a str, _: &'a str) -> ContinuationFuture<'a> {
        Box::pin(async move {
            envelope(Ok(match op {
                "session.quiesce" => {
                    self.quiesced.store(true, Ordering::SeqCst);
                    json!({"state":"sealed","afterCutoffItems":0,"afterCutoffBytes":0})
                }
                "session.continuation-dispose" => {
                    json!({"state":"released","afterCutoffItems":0,"afterCutoffBytes":0})
                }
                _ => json!({}),
            }))
        })
    }
    fn legacy_drain(
        &self,
        evidence: &ubm_desktop::continuation_outbox::LegacyEvidenceOwner,
        items: u32,
        bytes: u32,
    ) -> Option<Result<Value>> {
        self.legacy.then(|| {
            self.outbox
                .drain_legacy_preserving(
                    evidence,
                    items as usize,
                    bytes as usize,
                    ubm_desktop::continuation::project_521_record,
                )
                .map_err(ubm_desktop::continuation::legacy_evidence_error)
        })
    }
    fn claim_drain_v1(
        &self,
        evidence: &ubm_desktop::continuation_outbox::LegacyEvidenceOwner,
        items: u32,
        bytes: u32,
    ) -> Option<Result<Option<DrainClaim>>> {
        Some(
            self.outbox
                .claim_drain_projected(
                    evidence,
                    items as usize,
                    bytes as usize,
                    ubm_desktop::continuation::project_521_record,
                )
                .map_err(ubm_desktop::continuation::legacy_evidence_error),
        )
    }
    fn claim_drain(&self, items: u32, bytes: u32) -> Result<Option<DrainClaim>> {
        self.outbox
            .claim_drain(items as usize, bytes as usize)
            .map_err(continuation_drain_error)
    }
    fn acknowledge_drain(&self, id: u64) -> Result<()> {
        if self.outbox.acknowledge_drain(id) {
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
async fn started() -> (NativeContinuation, Arc<Session>) {
    let session = Arc::new(Session {
        outbox: Outbox::new(1, Arc::new(Wake)),
        quiesced: AtomicBool::new(false),
        legacy: false,
    });
    let owner = NativeContinuation::new(Arc::new(Host(session.clone())));
    owner
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    (owner, session)
}

#[tokio::test]
async fn production_style_legacy_projection_keeps_unrepresentable_originals_after_disposal() {
    let session = Arc::new(Session {
        outbox: Outbox::new(2, Arc::new(Wake)),
        quiesced: AtomicBool::new(false),
        legacy: true,
    });
    let owner = NativeContinuation::new(Arc::new(Host(session.clone())));
    owner
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    session.outbox.push_data(json!({"t":"value","consumer":"ubm-continuation-0","valueB64":"AQ==","delivery":"notification","provenance":"unknown","sourceGeneration":"1","sourceOrdinal":"1"})).unwrap();
    session.outbox.push_data(json!({"t":"peer-source-loss","consumer":"ubm-continuation-0","peerId":"peer","generation":"1","skippedRecords":null})).unwrap();
    let claim = owner.prepare_claim(10, 4096).await.unwrap();
    let token = claim["claimToken"].as_str().unwrap();
    let batch: Value = serde_json::from_str(claim["batches"][0].as_str().unwrap()).unwrap();
    assert_eq!(batch["records"].as_array().unwrap().len(), 1);
    assert!(batch["records"][0].get("provenance").is_none());
    owner.acknowledge_claim(token).await.unwrap();
    let page = owner
        .legacy_evidence_page(token, 0, 10, 4096)
        .await
        .unwrap();
    assert_eq!(page["records"].as_array().unwrap().len(), 2);
    assert_eq!(page["records"][0]["provenance"], "unknown");
    assert_eq!(page["records"][1]["t"], "peer-source-loss");
    owner.legacy_evidence_retire(token, 1).await.unwrap();
    owner.legacy_evidence_retire(token, 2).await.unwrap();
    assert!(owner.legacy_evidence_retire(token, 2).await.is_err());
}
#[tokio::test]
async fn legacy_incompatible_source_is_rejected_before_cutoff_and_v2_can_recover_raw() {
    let (owner, session) = started().await;
    let record = json!({"t":"value","consumer":"ubm-continuation-0","valueB64":"AQ==","delivery":"notification","provenance":"unknown","sourceGeneration":"1","sourceOrdinal":"1"});
    session.outbox.push_data(record.clone()).unwrap();
    assert_eq!(
        owner.prepare_claim(10, 4096).await.unwrap_err()["code"],
        "protocol.incompatible"
    );
    assert!(!session.quiesced.load(Ordering::SeqCst));
    let claim = owner.prepare_claim_versioned(2, 10, 4096).await.unwrap();
    let batch: Value = serde_json::from_str(claim["batches"][0].as_str().unwrap()).unwrap();
    assert_eq!(batch["records"][0]["provenance"], record["provenance"]);
    assert_eq!(
        batch["records"][0]["sourceOrdinal"],
        record["sourceOrdinal"]
    );
}

#[tokio::test]
async fn v1_projection_keeps_raw_prefix_until_ack_and_pages_omitted_descriptor() {
    let session = Arc::new(Session {
        outbox: Outbox::new(3, Arc::new(Wake)),
        quiesced: AtomicBool::new(false),
        legacy: true,
    });
    let owner = NativeContinuation::new(Arc::new(Host(session.clone())));
    owner
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    session.outbox.push_data(json!({"t":"peer-source-failure-recovery","v":2,"consumer":"ubm-continuation-0","descriptor":{"protocol":1}})).unwrap();
    let claim = owner.prepare_claim_versioned(1, 10, 4096).await.unwrap();
    let token = claim["claimToken"].as_str().unwrap();
    let batch: Value = serde_json::from_str(claim["batches"][0].as_str().unwrap()).unwrap();
    assert!(batch["records"].as_array().unwrap().is_empty());
    // A second protocol cannot take the owned raw prefix before ACK.
    assert!(owner.prepare_claim_versioned(2, 10, 4096).await.is_err());
    let page = owner
        .legacy_evidence_page(token, 0, 10, 4096)
        .await
        .unwrap();
    assert_eq!(page["records"][0]["t"], "peer-source-failure-recovery");
    owner.acknowledge_claim_v1(token).await.unwrap();
}

#[tokio::test]
async fn v1_successive_prefixes_keep_recovery_evidence_isolated_by_claim_token() {
    let session = Arc::new(Session {
        outbox: Outbox::new(4, Arc::new(Wake)),
        quiesced: AtomicBool::new(false),
        legacy: true,
    });
    let owner = NativeContinuation::new(Arc::new(Host(session.clone())));
    owner
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    session
        .outbox
        .push_data(json!({"t":"peer-source-failure-recovery","v":2,"consumer":"ubm-continuation-0","descriptor":{"prefix":1}}))
        .unwrap();
    let first = owner.prepare_claim_versioned(1, 1, 4096).await.unwrap();
    let first_token = first["claimToken"].as_str().unwrap().to_owned();
    // The later prefix is already source-owned before the first ACK. This
    // keeps the native session alive for the next retained-prefix claim.
    session
        .outbox
        .push_data(json!({"t":"peer-source-failure-recovery","v":2,"consumer":"ubm-continuation-0","descriptor":{"prefix":2}}))
        .unwrap();
    owner.acknowledge_claim_v1(&first_token).await.unwrap();
    let second = owner.prepare_claim_versioned(1, 1, 4096).await.unwrap();
    let second_token = second["claimToken"].as_str().unwrap().to_owned();
    assert_ne!(first_token, second_token);
    owner.acknowledge_claim_v1(&second_token).await.unwrap();

    let first_page = owner
        .legacy_evidence_page(&first_token, 0, 10, 4096)
        .await
        .unwrap();
    let second_page = owner
        .legacy_evidence_page(&second_token, 0, 10, 4096)
        .await
        .unwrap();
    assert_eq!(first_page["records"].as_array().unwrap().len(), 1);
    assert_eq!(second_page["records"].as_array().unwrap().len(), 1);
    assert_eq!(first_page["records"][0]["descriptor"]["prefix"], 1);
    assert_eq!(second_page["records"][0]["descriptor"]["prefix"], 2);

    let first_ordinal = first_page["records"][0]["ordinal"].as_u64().unwrap();
    owner
        .legacy_evidence_retire(&first_token, first_ordinal)
        .await
        .unwrap();
    // A lost page reply after the exact retirement remains an empty page for
    // this token, rather than becoming an unknown-token result or exposing
    // a later claim's evidence.
    assert!(
        owner
            .legacy_evidence_page(&first_token, 0, 10, 4096)
            .await
            .unwrap()["records"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        owner
            .legacy_evidence_retire(&first_token, first_ordinal)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn authenticated_v2_or_active_v1_tokens_never_fall_back_to_retired_v1_evidence() {
    let session = Arc::new(Session {
        outbox: Outbox::new(41, Arc::new(Wake)),
        quiesced: AtomicBool::new(false),
        legacy: true,
    });
    let owner = NativeContinuation::new(Arc::new(Host(session.clone())));
    owner
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();

    // A signed V2 token is valid for its own protocol, but was never issued
    // as a V1 evidence owner. It must not gain the V1 empty-page fallback,
    // either while active or after its receipt is retired.
    let v2 = owner.prepare_claim_versioned(2, 1, 4096).await.unwrap();
    let v2_token = v2["claimToken"].as_str().unwrap().to_owned();
    assert!(
        owner
            .legacy_evidence_page(&v2_token, 0, 1, 4096)
            .await
            .is_err()
    );
    owner.acknowledge_claim_v2(&v2_token).await.unwrap();
    owner.retire_claim_receipt(&v2_token).await.unwrap();
    assert!(
        owner
            .legacy_evidence_page(&v2_token, 0, 1, 4096)
            .await
            .is_err()
    );

    owner
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    session
        .outbox
        .push_data(json!({"t":"peer-source-failure-recovery","v":2,"consumer":"ubm-continuation-0","descriptor":{"active":true}}))
        .unwrap();
    let v1 = owner.prepare_claim_versioned(1, 1, 4096).await.unwrap();
    let v1_token = v1["claimToken"].as_str().unwrap();
    let active = owner
        .legacy_evidence_page(v1_token, 0, 1, 4096)
        .await
        .unwrap();
    assert_eq!(active["records"][0]["descriptor"]["active"], true);
}
#[tokio::test]
async fn v1_bounds_full_escaped_envelope_and_retries_identical_claim() {
    let (owner, session) = started().await;
    let payload = "\\\"\n".repeat(1500);
    for n in 0..100 {
        session
            .outbox
            // Use a record that strict 5.2.1 can actually represent. Unknown
            // vocabulary is covered separately by the raw-evidence test.
            .push_control(json!({"t":"adapter","state":payload,"n":n}));
    }
    let first = owner
        .prepare_claim_versioned(1, 100, 0x7fffffff)
        .await
        .unwrap();
    assert!(envelope(Ok(first.clone())).len() <= 1048576);
    assert_eq!(
        owner.prepare_claim_versioned(1, 1, 1024).await.unwrap(),
        first
    );
    let batch: Value = serde_json::from_str(first["batches"][0].as_str().unwrap()).unwrap();
    assert!(batch["records"].as_array().unwrap().len() < 100);
    assert_eq!(batch["records"][0]["state"], payload);
}
#[tokio::test]
async fn retirement_retry_after_lost_reply_is_idempotent() {
    let (owner, _) = started().await;
    let claim = owner.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    let token = claim["claimToken"].as_str().unwrap();
    owner.acknowledge_claim_v1(token).await.unwrap();
    owner.retire_claim_receipt_v1(token).await.unwrap();
    owner.retire_claim_receipt_v1(token).await.unwrap();
}

#[tokio::test]
async fn v1_retirement_refuses_a_v2_receipt_without_releasing_it() {
    let (owner, _) = started().await;
    let claim = owner.prepare_claim_versioned(2, 1, 1024).await.unwrap();
    let token = claim["claimToken"].as_str().unwrap();
    owner.acknowledge_claim_v2(token).await.unwrap();

    assert!(owner.retire_claim_receipt_v1(token).await.is_err());
    owner.retire_claim_receipt(token).await.unwrap();
}

#[tokio::test]
async fn retired_token_retries_survive_later_retirements_but_wrong_tokens_fail() {
    let (owner, session) = started().await;
    session.outbox.push_control(json!({"t":"first"}));
    session.outbox.push_control(json!({"t":"second"}));
    let first = owner.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    let first = first["claimToken"].as_str().unwrap().to_owned();
    let receipt = owner.acknowledge_claim_v1(&first).await.unwrap();
    assert_eq!(receipt["disposed"], false);
    owner.retire_claim_receipt(&first).await.unwrap();
    let second = owner.prepare_claim_versioned(1, 1, 1024).await.unwrap();
    let second = second["claimToken"].as_str().unwrap();
    assert!(owner.retire_claim_receipt(second).await.is_err());
    owner.acknowledge_claim_v1(second).await.unwrap();
    owner.retire_claim_receipt(second).await.unwrap();
    owner.retire_claim_receipt(&first).await.unwrap();
    assert!(owner.acknowledge_claim_v1(&first).await.is_err());
    let mut wrong = first;
    wrong.push('0');
    assert!(owner.retire_claim_receipt(&wrong).await.is_err());
}

#[tokio::test]
async fn retired_prefixes_do_not_exhaust_receipt_capacity() {
    let (owner, session) = started().await;
    session.outbox.push_control(json!({"t":"first"}));
    let mut first = String::new();
    for index in 0..4100 {
        let claim = owner.prepare_claim_versioned(1, 1, 1024).await.unwrap();
        let token = claim["claimToken"].as_str().unwrap();
        if index == 0 {
            first = token.to_owned();
        }
        session.outbox.push_control(json!({"t":"next", "n":index}));
        assert_eq!(
            owner.acknowledge_claim_v1(token).await.unwrap()["disposed"],
            false
        );
        let evidence = owner.legacy_evidence_page(token, 0, 1, 1024).await.unwrap();
        let ordinal = evidence["records"][0]["ordinal"].as_u64().unwrap();
        owner.legacy_evidence_retire(token, ordinal).await.unwrap();
        owner.retire_claim_receipt(token).await.unwrap();
    }
    owner.retire_claim_receipt(&first).await.unwrap();
    // The first owner's empty entry was reclaimed; authenticated stale page
    // retry remains an empty result without retaining thousands of owners.
    assert!(
        owner
            .legacy_evidence_page(&first, 0, 1, 1024)
            .await
            .unwrap()["records"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn oversized_single_record_remains_in_the_source_after_smaller_retries() {
    let (owner, session) = started().await;
    let payload = "\\\"é".repeat(120_000);
    session
        .outbox
        .push_control(json!({"t":"oversized", "payload":payload}));
    let error = owner
        .prepare_claim_versioned(1, 1, 0x7fffffff)
        .await
        .unwrap_err();
    assert_eq!(error["code"], "bytes.too-large");
    let required = match session.outbox.claim_drain(1, 1) {
        Err(ubm_desktop::continuation_outbox::DrainClaimError::RecordTooLarge {
            required_bytes,
        }) => required_bytes,
        _ => panic!("oversized record was not retained"),
    };
    assert!(required > 262_144);
    assert!(owner.prepare_claim_versioned(1, 1, 1024).await.is_err());
    // Direct source inspection establishes retention, not a packaged reader.
    let source = session.outbox.claim_drain(1, required).unwrap().unwrap();
    assert_eq!(source.records[0]["payload"], payload);
}
