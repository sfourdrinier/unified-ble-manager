//! System-connected inventory is independent of this owner's discoveries/leases.
mod common;
use common::*;
use serde_json::json;
use ubm_mobile::{ConnectedPeer, MobilePlatform, RadioCompletion, RadioRequest, RequestKind};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_connected_inventory_reports_foreign_unbonded_peers_without_ownership() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::new(Box::new(|request| match request {
            RadioRequest::ConnectedPeers { services, .. } => {
                assert!(services.is_empty() || services == &[HR_SERVICE.to_owned()]);
                Reply::Now(RadioCompletion::ConnectedPeers(vec![ConnectedPeer {
                    peer_id: POLAR.into(),
                    name: Some("another application's link".into()),
                }]))
            }
            _ => polar_responder(request),
        }));
        let (host, _) = open(&radio, platform).await;
        let session = host.open_session("directory-only").unwrap();
        let services = if platform == MobilePlatform::Apple {
            vec![HR_SERVICE]
        } else {
            vec![]
        };
        let peers = ok(&call(
            &session,
            "peers.connected",
            &json!({"operationId":"inventory", "services":services, "budgetMs":1000}).to_string(),
        )
        .await);
        assert_eq!(peers[0]["peerId"], POLAR);
        assert_eq!(peers[0]["source"], "system-connected");
        assert_eq!(peers[0]["connection"], "connected");
        assert_eq!(radio.count(RequestKind::ConnectedPeers), 1);
        assert_eq!(radio.count(RequestKind::Connect), 0);
        assert_eq!(radio.count(RequestKind::StartScan), 0);
        assert!(
            ok(&call(&session, "peers.known", "{}").await)
                .as_array()
                .unwrap()
                .is_empty()
        );
        ok(&call(&session, "session.dispose", "{}").await);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connected_query_rejects_unanswerable_service_scope_before_native_effects() {
    for (platform, services) in [
        (MobilePlatform::Apple, vec![]),
        (MobilePlatform::Android, vec![HR_SERVICE]),
    ] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let session = host.open_session("directory-only").unwrap();
        let result = parse(
            &call(
                &session,
                "peers.connected",
                &json!({"operationId":"unsupported", "services":services}).to_string(),
            )
            .await,
        );
        assert_eq!(result["error"]["code"], "capability.unsupported");
        assert_eq!(radio.count(RequestKind::ConnectedPeers), 0);
        ok(&call(&session, "session.dispose", "{}").await);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_reference_resolution_queries_native_inventory_without_creating_owner_state() {
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::ResolvePeer { peer_id, .. } => Reply::Now(RadioCompletion::ResolvedPeer(
            (peer_id == POLAR).then(|| ubm_mobile::ResolvedPeer {
                peer_id: peer_id.clone(),
                name: Some("native lookup".into()),
            }),
        )),
        _ => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("directory-only").unwrap();
    let resolved = ok(&call(
        &session,
        "peers.resolve",
        &json!({"reference":{"opaqueId":POLAR},"operationId":"lookup","budgetMs":1000}).to_string(),
    )
    .await);
    assert_eq!(resolved["peerId"], POLAR);
    assert_eq!(resolved["name"], "native lookup");
    assert_eq!(resolved["connection"], "unknown");
    assert_eq!(radio.count(RequestKind::ResolvePeer), 1);
    assert_eq!(radio.count(RequestKind::Connect), 0);
    assert!(
        ok(&call(&session, "peers.known", "{}").await)
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        ok(&call(
            &session,
            "peers.resolve",
            &json!({"reference":{"opaqueId":"AA:BB:CC:DD:EE:00"},"operationId":"missing"})
                .to_string()
        )
        .await)
        .is_null()
    );
    ok(&call(&session, "session.dispose", "{}").await);
}
