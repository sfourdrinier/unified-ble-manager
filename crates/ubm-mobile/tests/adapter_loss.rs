//! Adapter loss on the mobile owner, as the legacy React Native backends
//! handled it (origin/main `src/backends/corebluetooth/corebluetooth-backend.ts`
//! `handleAdapterState` :1146, `startAdapterLossCleanup` :1173,
//! `advanceGeneration` :1282, `corebluetooth-adapter-loss-cleanup.ts`).
//!
//! - The adapter is lost when it is not available, its authorization blocks
//!   (`denied`, `restricted`, `unavailable`), or its power reports a concrete
//!   loss. Unknown power is an observation, not a fabricated resetting state.
//! - The first lost state of an episode ends the scan (`source-failed`), the
//!   subscriptions and the links, then advances the backend and adapter
//!   generations by one. Further lost states of the same episode advance
//!   nothing; a usable adapter ends the episode.
//! - The state record of the change carries the generation it happened
//!   under; the advance is published as its own adapter record.

mod common;

use common::*;
use serde_json::{Value, json};
use ubm_mobile::{
    AdapterAuthorization, AdapterAvailability, AdapterPower, AdapterSnapshot, Advertisement,
    MobilePlatform, MobileSession, RadioIngress,
};

fn advertisement() -> RadioIngress {
    RadioIngress::Advertisement(Advertisement {
        capture_timestamp_ms: None,
        cached_name: None,
        peer_id: POLAR.to_owned(),
        address: Some(POLAR.to_owned()),
        service_uuids: vec!["180D".to_owned()],
        ..Advertisement::default()
    })
}

fn snapshot(
    availability: AdapterAvailability,
    authorization: AdapterAuthorization,
    power: AdapterPower,
) -> RadioIngress {
    RadioIngress::AdapterState(AdapterSnapshot {
        availability,
        authorization,
        power,
        safe_reason: None,
    })
}

fn off() -> RadioIngress {
    snapshot(
        AdapterAvailability::Available,
        AdapterAuthorization::Granted,
        AdapterPower::Off,
    )
}

fn on() -> RadioIngress {
    RadioIngress::AdapterState(adapter_on())
}

fn adapter_records(records: &[Value]) -> Vec<(String, String, String)> {
    of_type(records, "adapter")
        .iter()
        .map(|record| {
            let state = &record["state"];
            (
                state["power"].as_str().unwrap_or_default().to_owned(),
                state["backendGeneration"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                state["adapterGeneration"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect()
}

fn has_generation(records: &[Value], generation: &str) -> bool {
    adapter_records(records)
        .iter()
        .any(|(_, backend, _)| backend == generation)
}

async fn generation(session: &MobileSession) -> (Value, Value) {
    let state = ok(&call(session, "adapter.state", "{}").await);
    (
        state["backendGeneration"].clone(),
        state["adapterGeneration"].clone(),
    )
}

/// Drain for a while and answer everything that arrived.
async fn settle(session: &MobileSession) -> Vec<Value> {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    parse(&session.drain(256, 65536))["records"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_unknown_power_does_not_reset_a_connect_waiting_for_native_readiness() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::new(Box::new(|request| match request {
            ubm_mobile::RadioRequest::Connect { .. } => Reply::Hold,
            _ => polar_responder(request),
        }));
        let (host, _) = open(&radio, platform).await;
        let session = host.open_session("initial-state").expect("session");
        host.ingest(advertisement());
        let mut connect = Box::pin(call(
            &session,
            "connection.connect",
            r#"{"peerId":"A0:9E:1A:00:00:01","lease":"initial","operationId":"initial-connect"}"#,
        ));
        tokio::select! {
            result = &mut connect => panic!("connect did not reach held native preparation: {result}"),
            () = async {
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while radio.held_of(ubm_mobile::RequestKind::Connect).is_empty() { tokio::task::yield_now().await; }
                }).await.expect("native connect admission");
            } => {}
        }
        host.ingest(snapshot(
            AdapterAvailability::Available,
            AdapterAuthorization::NotDetermined,
            AdapterPower::Unknown,
        ));
        let records = settle(&session).await;
        assert!(
            adapter_records(&records)
                .iter()
                .all(|(_, backend, adapter)| backend == "1" && adapter == "1"),
            "unknown observation advanced attachment: {records:?}"
        );
        host.ingest(on());
        radio.answer(
            radio.held_of(ubm_mobile::RequestKind::Connect)[0],
            ubm_mobile::RadioCompletion::Unit,
        );
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), connect)
            .await
            .expect("connect settled");
        ok(&result);
        ok(&call(&session, "connection.disconnect", r#"{"peerId":"A0:9E:1A:00:00:01","lease":"initial","operationId":"initial-disconnect"}"#).await);
        ok(&call(&session, "session.dispose", "{}").await);
        assert_eq!(parse(&host.shutdown().await)["state"], "released");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_adapter_ends_live_work_and_advances_the_generations_once_per_episode() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let session = host.open_session("loss").expect("session");
        let scan = ok(&call(
            &session,
            "scan.start",
            &json!({"serviceUuids": ["180D"], "duplicatePolicy": "all", "operationId": "scan-1"})
                .to_string(),
        )
        .await);
        host.ingest(advertisement());
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "connect-1"}).to_string(),
        )
        .await);
        ok(&call(
            &session,
            "gatt.discover",
            &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "discover-1"}).to_string(),
        )
        .await);
        ok(&call(
            &session,
            "gatt.subscribe",
            &json!({"peerId": POLAR, "selector": selector(), "consumer": "hr",
                    "deliveryMode": "require-notification", "operationId": "sub-1"})
            .to_string(),
        )
        .await);
        settle(&session).await;

        host.ingest(off());
        let records = drain_until(&session, |r| has_generation(r, "2")).await;
        assert_eq!(
            adapter_records(&records),
            [
                ("off".to_owned(), "1".to_owned(), "1".to_owned()),
                ("off".to_owned(), "2".to_owned(), "2".to_owned()),
            ],
            "{platform:?}: the change under generation 1, then the advance: {records:#?}"
        );
        let scan_ends = of_type(&records, "scan-end");
        assert_eq!(scan_ends.len(), 1, "{platform:?}: {records:#?}");
        assert_eq!(scan_ends[0]["operationId"], scan["operationId"]);
        assert_eq!(scan_ends[0]["reason"], "source-failed");
        let links = of_type(&records, "link");
        assert_eq!(links.len(), 1, "{platform:?}: {records:#?}");
        assert_eq!(links[0]["reason"], "adapter");
        assert_eq!(of_type(&records, "stream-end").len(), 1, "{records:#?}");
        assert_eq!(generation(&session).await, (json!("2"), json!("2")));

        // The same episode: nothing advances.
        host.ingest(snapshot(
            AdapterAvailability::Available,
            AdapterAuthorization::Granted,
            AdapterPower::Unknown,
        ));
        let unknown_records = settle(&session).await;
        assert_eq!(
            adapter_records(&unknown_records),
            [("unknown".to_owned(), "2".to_owned(), "2".to_owned())]
        );
        host.ingest(off());
        host.ingest(snapshot(
            AdapterAvailability::Available,
            AdapterAuthorization::Denied,
            AdapterPower::Off,
        ));
        let records = settle(&session).await;
        assert!(!has_generation(&records, "3"), "{platform:?}: {records:#?}");
        assert_eq!(generation(&session).await, (json!("2"), json!("2")));

        // A usable adapter ends the episode without advancing.
        host.ingest(on());
        let records = settle(&session).await;
        assert_eq!(
            adapter_records(&records),
            [("on".to_owned(), "2".to_owned(), "2".to_owned())],
            "{platform:?}: {records:#?}"
        );

        // Every concrete loss condition opens a new episode.
        let mut expected = 2;
        for lost in [
            snapshot(
                AdapterAvailability::Available,
                AdapterAuthorization::Denied,
                AdapterPower::On,
            ),
            snapshot(
                AdapterAvailability::Available,
                AdapterAuthorization::Restricted,
                AdapterPower::On,
            ),
            snapshot(
                AdapterAvailability::Available,
                AdapterAuthorization::Unavailable,
                AdapterPower::On,
            ),
            snapshot(
                AdapterAvailability::Unavailable,
                AdapterAuthorization::Granted,
                AdapterPower::On,
            ),
            snapshot(
                AdapterAvailability::Available,
                AdapterAuthorization::Granted,
                AdapterPower::Resetting,
            ),
        ] {
            expected += 1;
            host.ingest(lost);
            let wanted = expected.to_string();
            drain_until(&session, |r| has_generation(r, &wanted)).await;
            host.ingest(on());
            settle(&session).await;
        }
        for unmeasured in [
            snapshot(
                AdapterAvailability::Unknown,
                AdapterAuthorization::Granted,
                AdapterPower::On,
            ),
            snapshot(
                AdapterAvailability::Available,
                AdapterAuthorization::Granted,
                AdapterPower::Unknown,
            ),
        ] {
            host.ingest(unmeasured);
            let records = settle(&session).await;
            assert!(
                !has_generation(&records, &(expected + 1).to_string()),
                "unmeasured facts fabricated reset: {records:?}"
            );
            assert_eq!(
                generation(&session).await,
                (json!(expected.to_string()), json!(expected.to_string()))
            );
            host.ingest(on());
            settle(&session).await;
        }
        // Blocking-free authorizations are not a loss (legacy
        // `isAuthorizationBlocking`).
        for usable in [
            AdapterAuthorization::NotDetermined,
            AdapterAuthorization::Unknown,
        ] {
            host.ingest(snapshot(
                AdapterAvailability::Available,
                usable,
                AdapterPower::On,
            ));
        }
        let records = settle(&session).await;
        assert!(
            !has_generation(&records, &(expected + 1).to_string()),
            "{platform:?}: {records:#?}"
        );
        ok(&call(&session, "session.dispose", "{}").await);
        let record = parse(&host.shutdown().await);
        assert_eq!(record["state"], "released", "{record}");
    }
}

/// The physical Samsung run: the OS reported the link lost while the adapter
/// went down, the app removed the subscription the loss had ended, then
/// destroyed the manager. Legacy answered `released` at every step (the
/// adapter-loss cleanup had already ended them); the owner must not try to
/// resolve a path whose database the loss removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn releases_after_a_loss_answer_released_as_legacy_did() {
    for (platform, adapter_first) in [
        (MobilePlatform::Android, true),
        (MobilePlatform::Android, false),
        (MobilePlatform::Apple, true),
        (MobilePlatform::Apple, false),
    ] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let session = host.open_session("loss").expect("session");
        host.ingest(advertisement());
        ok(&call(
            &session,
            "connection.connect",
            &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "connect-1"}).to_string(),
        )
        .await);
        ok(&call(
            &session,
            "gatt.discover",
            &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "discover-1"}).to_string(),
        )
        .await);
        ok(&call(
            &session,
            "gatt.subscribe",
            &json!({"peerId": POLAR, "selector": selector(), "consumer": "hr",
                    "deliveryMode": "require-notification", "operationId": "sub-1"})
            .to_string(),
        )
        .await);
        let link_lost = RadioIngress::Connection {
            peer_id: POLAR.to_owned(),
            connected: false,
            status: Some(8),
        };
        if adapter_first {
            host.ingest(off());
            host.ingest(link_lost);
        } else {
            host.ingest(link_lost);
            host.ingest(off());
        }
        drain_until(&session, |r| !of_type(r, "stream-end").is_empty()).await;
        settle(&session).await;
        let removed = ok(&call(
            &session,
            "gatt.unsubscribe",
            &json!({"peerId": POLAR, "selector": selector(), "consumer": "hr", "operationId": "unsub-1"})
                .to_string(),
        )
        .await);
        assert_eq!(
            removed["state"], "released",
            "{platform:?} {adapter_first}: {removed}"
        );
        let released = ok(&call(
            &session,
            "connection.disconnect",
            &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "disconnect-1"})
                .to_string(),
        )
        .await);
        assert_eq!(
            released["state"], "released",
            "{platform:?} {adapter_first}: {released}"
        );
        let disposed = ok(&call(&session, "session.dispose", "{}").await);
        assert_eq!(
            disposed["state"], "released",
            "{platform:?} {adapter_first}: {disposed}"
        );
        let record = parse(&host.shutdown().await);
        assert_eq!(record["state"], "released", "{record}");
    }
}
