use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use ubm_desktop::continuation::{
    ContinuationFuture, ContinuationHost, ContinuationSession, NativeContinuation, Result, envelope,
};

#[derive(Default)]
struct Session {
    batches: Mutex<VecDeque<String>>,
    calls: Mutex<Vec<String>>,
}
struct Host(Arc<Session>);
impl ContinuationHost for Host {
    fn open_session(&self) -> Result<Arc<dyn ContinuationSession>> {
        Ok(self.0.clone())
    }
}
impl ContinuationSession for Session {
    fn call<'a>(&'a self, op: &'a str, _: &'a str) -> ContinuationFuture<'a> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(op.to_owned());
            envelope(Ok(match op {
                "session.quiesce" => {
                    json!({"state":"sealed","afterCutoffItems":2,"afterCutoffBytes":8})
                }
                "session.continuation-dispose" => {
                    json!({"state":"released","afterCutoffItems":3,"afterCutoffBytes":12})
                }
                _ => json!({}),
            }))
        })
    }
    fn drain(&self, _: u32, _: u32) -> ContinuationFuture<'_> {
        Box::pin(async move {
            self.batches
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| batch(false))
        })
    }
}
fn batch(more: bool) -> String {
    json!({"more":more,"records":[],"controlLost":0}).to_string()
}
async fn started(batches: Vec<String>) -> (NativeContinuation, Arc<Session>) {
    let session = Arc::new(Session {
        batches: Mutex::new(batches.into()),
        calls: Mutex::default(),
    });
    let engine = NativeContinuation::new(Arc::new(Host(session.clone())));
    engine
        .execute("peer", r#"{"onAppearance":"native"}"#)
        .await
        .unwrap();
    (engine, session)
}

#[tokio::test]
async fn capped_prefix_is_replayable_and_ack_does_not_dispose_tail() {
    let (engine, session) = started((0..40).map(|i| batch(i < 39)).collect()).await;
    let prefix = engine.prepare_claim(1, 1024).await.unwrap();
    assert_eq!(prefix["batches"].as_array().unwrap().len(), 32);
    assert!(prefix["disposeFailure"].is_string());
    assert_eq!(engine.prepare_claim(1, 1024).await.unwrap(), prefix);
    let token = prefix["claimToken"].as_str().unwrap();
    assert_eq!(
        engine.acknowledge_claim(token).await.unwrap()["disposed"],
        false
    );
    assert!(
        !session
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|op| op == "session.continuation-dispose")
    );
    assert!(
        engine
            .execute("peer", r#"{"onAppearance":"native"}"#)
            .await
            .is_err(),
        "acknowledged prefix must not reopen sealed intake"
    );
    let tail = engine.prepare_claim(1, 1024).await.unwrap();
    assert_eq!(tail["batches"].as_array().unwrap().len(), 8);
    assert_ne!(tail["claimToken"], prefix["claimToken"]);
    let ack = engine
        .acknowledge_claim(tail["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(ack["disposed"], true);
    assert_eq!(ack["afterCutoffLoss"], json!({"items":3,"bytes":12}));
}

#[tokio::test]
async fn malformed_drain_retains_valid_prefix_behind_acknowledgement() {
    for malformed in [
        "not-json",
        r#"{"more":false,"records":[]}"#,
        r#"{"more":false,"records":[],"controlLost":0,"unexpected":1}"#,
    ] {
        let (engine, session) =
            started(vec![batch(true), malformed.to_owned(), batch(false)]).await;
        let claim = engine.prepare_claim(1, 1024).await.unwrap();
        assert_eq!(claim["batches"], json!([batch(true)]));
        assert!(
            claim["disposeFailure"]
                .as_str()
                .unwrap()
                .contains("invalid continuation drain")
        );
        assert_eq!(engine.prepare_claim(1, 1024).await.unwrap(), claim);
        assert_eq!(
            engine
                .acknowledge_claim(claim["claimToken"].as_str().unwrap())
                .await
                .unwrap()["disposed"],
            false
        );
        assert!(
            !session
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|op| op == "session.continuation-dispose")
        );
        let tail = engine.prepare_claim(1, 1024).await.unwrap();
        assert_eq!(tail["disposeFailure"], Value::Null);
        assert_eq!(
            engine
                .acknowledge_claim(tail["claimToken"].as_str().unwrap())
                .await
                .unwrap()["disposed"],
            true
        );
    }
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
