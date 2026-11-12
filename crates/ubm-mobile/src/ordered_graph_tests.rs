// crates/ubm-mobile/src/ordered_graph_tests.rs
use super::*;
use crate::test_common::{self, Reply, Scripted};
use ubm_desktop::RadioBoundary;

#[tokio::test]
async fn graph_cache_is_invalidated_at_host_ingress() {
    let identity = ubm_desktop::boundary::GattSnapshotIdentity {
        owner: "native-owner".into(),
        attachment: 3,
        revision: 7,
    };
    let expected = identity.clone();
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => Reply::Now(RadioCompletion::DiscoveredScoped {
            services: vec![],
            identity: expected.clone(),
        }),
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    host.inner.radio.discover_scoped("peer").await.unwrap();
    assert_eq!(
        host.inner.radio.gatt_snapshot_identity("peer").unwrap(),
        Some(identity.clone())
    );
    host.ingest(crate::RadioIngress::ServicesChanged {
        peer_id: "peer".into(),
    });
    assert_eq!(
        host.inner.radio.gatt_snapshot_identity("peer").unwrap(),
        None
    );
    host.inner.radio.discover_scoped("peer").await.unwrap();
    host.ingest(crate::RadioIngress::ConnectionScoped {
        peer_id: "peer".into(),
        expected_generation: "stale".into(),
        connected: false,
        status: None,
    });
    assert_eq!(
        host.inner.radio.gatt_snapshot_identity("peer").unwrap(),
        Some(identity)
    );
    host.ingest(crate::RadioIngress::Connection {
        peer_id: "peer".into(),
        connected: false,
        status: None,
    });
    assert_eq!(
        host.inner.radio.gatt_snapshot_identity("peer").unwrap(),
        None
    );
    host.inner.radio.discover_scoped("peer").await.unwrap();
    let mut off = test_common::adapter_on();
    off.power = crate::AdapterPower::Off;
    host.ingest(crate::RadioIngress::AdapterState(off));
    assert_eq!(
        host.inner.radio.gatt_snapshot_identity("peer").unwrap(),
        None
    );
}
