//! Public revision negotiation must fail before a source can be admitted.
mod common;
use common::{self as test_common, Scripted};
use ubm_mobile::{MobilePlatform, RequestKind};

#[tokio::test]
async fn metadata_revision_offer_rejects_skew_before_native_admission() {
    let radio = Scripted::polar();
    let (host, _) = test_common::open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("metadata-revision").unwrap();
    let version = test_common::ok(&test_common::call(&session, "gatt.ordered.version", "{}").await);
    assert_eq!(version, serde_json::json!({"wireVersion": 2}));
    let metadata = test_common::ok(
        &test_common::call(&session, "gatt.ordered.failure-metadata.version", "{}").await,
    );
    assert_eq!(metadata["versions"], serde_json::json!([1]));
    let open = serde_json::json!({
        "wireVersion": 2, "failureMetadataVersion": 2,
        "peerId": test_common::POLAR, "collectionId": "1",
        "connectionGeneration": "c", "databaseGeneration": "d",
        "publicConnectionGeneration": "c", "publicDatabaseGeneration": "d",
        "members": [], "maxItems": 8, "maxBytes": 128, "maxControls": 8,
        "operationId": "open"
    });
    let (failure, _) = test_common::failure(
        &test_common::call(&session, "gatt.ordered.open", &open.to_string()).await,
    );
    assert_eq!(failure["code"], "protocol.incompatible");
    assert_eq!(radio.count(RequestKind::OrderedOpen), 0);
    assert_eq!(radio.count(RequestKind::OrderedAcknowledgeBatch), 0);
}
