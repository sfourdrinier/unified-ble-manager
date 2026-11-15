#[tokio::test]
async fn reclaim_status_and_unrelated_collection_bypass_stalled_lookup() {
    use crate::test_common::{self, Scripted};
    let radio = Scripted::polar();
    let (host, _) = test_common::open(&radio, MobilePlatform::Android).await;
    host.inner.radio.enable_marker_for_test();
    let session = host.open_session("reclaim-concurrency").unwrap();
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    lock(&session.state.reclaim_guards).insert(1, Arc::downgrade(&gate));
    let _stalled = gate.lock().await;
    let status = session.recording_reclaim(
        "status",
        "peer",
        1,
        "missing",
        "secret",
        Some(1),
        Some("caller"),
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), status)
            .await
            .is_ok(),
        "status must remain reachable while an unrelated submission is stalled"
    );
    let next =
        session.recording_reclaim("next", "peer", 2, "missing", "secret", None, Some("caller"));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), next)
            .await
            .is_ok(),
        "another collection must make progress"
    );
}
