//! One name per physical event (owner decision, 5.0): Android's and Apple's
//! own answers for each event, driven through the mobile owner over the
//! scripted radio, report on the wire exactly the error code and
//! retryability the shared table names
//! (`src/backend-contract/event-vocabulary.ts`, copied to
//! `crates/ubm-desktop/tests/fixtures/event-vocabulary.json` and pinned by
//! `__tests__/event-vocabulary.test.js`).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use ubm_mobile::{
    AdapterAuthorization, AdapterAvailability, AdapterPower, AdapterSnapshot, FailureKind,
    MobilePlatform, MobileSession, PlatformFailure, RadioCompletion, RadioIngress, RadioRequest,
    RequestKind,
};

fn backend(platform: MobilePlatform) -> &'static str {
    match platform {
        MobilePlatform::Android => "react-native-android",
        MobilePlatform::Apple => "react-native-ios",
    }
}

fn expected(event: &str, platform: MobilePlatform) -> (Value, Value) {
    let table: Value = serde_json::from_str(include_str!(
        "../../ubm-desktop/tests/fixtures/event-vocabulary.json"
    ))
    .expect("fixture");
    let row = &table[event][backend(platform)];
    (row["error"].clone(), row["retryability"].clone())
}

fn observed(text: &str) -> (Value, Value) {
    let envelope = parse(text);
    assert_eq!(envelope["ok"], false, "{text}");
    (
        envelope["error"]["code"].clone(),
        envelope["retryability"].clone(),
    )
}

/// The platform's own failure for each event, as its radio adapter reports it.
fn link_gone(platform: MobilePlatform) -> PlatformFailure {
    match platform {
        MobilePlatform::Android => PlatformFailure {
            gatt_status: Some(8),
            ..PlatformFailure::new(FailureKind::NotConnected, "link lost")
        },
        MobilePlatform::Apple => PlatformFailure {
            native_domain: Some("com.sfourdrinier.unifiedblemanager.corebluetooth".to_owned()),
            native_code: Some(1020),
            ..PlatformFailure::new(FailureKind::NotConnected, "CoreBluetooth disconnected")
        },
    }
}

fn not_established(platform: MobilePlatform) -> PlatformFailure {
    match platform {
        MobilePlatform::Android => PlatformFailure {
            gatt_status: Some(133),
            ..PlatformFailure::new(FailureKind::GattStatus, "status 133")
        },
        MobilePlatform::Apple => PlatformFailure {
            native_domain: Some("CBErrorDomain".to_owned()),
            native_code: Some(10),
            ..PlatformFailure::new(FailureKind::Platform, "Connection failed")
        },
    }
}

fn security(platform: MobilePlatform) -> PlatformFailure {
    match platform {
        MobilePlatform::Android => PlatformFailure {
            gatt_status: Some(5),
            ..PlatformFailure::new(FailureKind::GattStatus, "GATT_INSUFFICIENT_AUTHENTICATION")
        },
        MobilePlatform::Apple => PlatformFailure {
            gatt_status: Some(5),
            native_domain: Some("CBATTErrorDomain".to_owned()),
            native_code: Some(5),
            ..PlatformFailure::new(FailureKind::GattStatus, "Authentication is insufficient.")
        },
    }
}

fn read_args(id: &str, budget_ms: Option<u64>) -> String {
    let mut args = json!({"peerId": POLAR, "selector": selector(), "operationId": id});
    if let Some(budget) = budget_ms {
        args["budgetMs"] = json!(budget);
    }
    args.to_string()
}

async fn connected(
    radio: &Arc<Scripted>,
    platform: MobilePlatform,
) -> (std::sync::Arc<ubm_mobile::MobileHost>, MobileSession) {
    let (host, _) = open(radio, platform).await;
    let session = host.open_session("rn").unwrap();
    ok(&call(
        &session,
        "connection.connect",
        &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "c"}).to_string(),
    )
    .await);
    ok(&call(
        &session,
        "gatt.discover",
        &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "d"}).to_string(),
    )
    .await);
    (host, session)
}

fn failing_with(kind: RequestKind, failure: PlatformFailure) -> Arc<Scripted> {
    Scripted::new(Box::new(move |request| {
        if request.kind() == kind {
            Reply::Now(RadioCompletion::Failed(failure.clone()))
        } else if matches!(request, RadioRequest::Discover { .. }) {
            let mut services = polar_services();
            services[0].characteristics[0].properties.read = true;
            Reply::Now(RadioCompletion::Discovered(services))
        } else {
            polar_responder(request)
        }
    }))
}

fn holding(kinds: &'static [RequestKind]) -> Box<dyn FnMut(&RadioRequest) -> Reply + Send> {
    Box::new(move |request| {
        if kinds.contains(&request.kind()) {
            Reply::Hold
        } else {
            polar_responder(request)
        }
    })
}

async fn until_held(radio: &Scripted, kind: RequestKind) {
    for _ in 0..3000 {
        if !radio.held_of(kind).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("{kind:?} never reached the radio");
}

/// A read the radio holds, ended by `end`.
async fn held_read_ended_by(
    radio: &Arc<Scripted>,
    session: &MobileSession,
    budget_ms: Option<u64>,
    end: impl AsyncFnOnce(),
) -> String {
    radio.set_responder(holding(&[RequestKind::Read, RequestKind::Disconnect]));
    let reader = session.clone();
    let pending =
        tokio::spawn(async move { call(&reader, "gatt.read", &read_args("r", budget_ms)).await });
    until_held(radio, RequestKind::Read).await;
    end().await;
    tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("the read ends")
        .expect("task")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_mobile_platforms_name_each_event_as_the_table_does() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        // link-lost-during-operation: the platform fails the operation...
        let radio = failing_with(RequestKind::Read, link_gone(platform));
        let (_host, session) = connected(&radio, platform).await;
        let text = call(&session, "gatt.read", &read_args("r", None)).await;
        assert_eq!(
            observed(&text),
            expected("link-lost-during-operation", platform),
            "{platform:?} answered"
        );
        // ...or the OS reports the link gone while the operation waits.
        let radio = Scripted::polar();
        let (host, session) = connected(&radio, platform).await;
        let text = held_read_ended_by(&radio, &session, None, async || {
            host.ingest(RadioIngress::Connection {
                peer_id: POLAR.to_owned(),
                connected: false,
                status: Some(8),
            });
        })
        .await;
        assert_eq!(
            observed(&text),
            expected("link-lost-during-operation", platform),
            "{platform:?} unanswered"
        );

        // requested-disconnect-during-operation
        let radio = Scripted::polar();
        let (_host, session) = connected(&radio, platform).await;
        let releasing = session.clone();
        let text = held_read_ended_by(&radio, &session, None, async || {
            tokio::spawn(async move {
                call(
                    &releasing,
                    "connection.disconnect",
                    &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "x"}).to_string(),
                )
                .await
            });
        })
        .await;
        assert_eq!(
            observed(&text),
            expected("requested-disconnect-during-operation", platform),
            "{platform:?}"
        );

        // adapter-loss-during-operation
        let radio = Scripted::polar();
        let (host, session) = connected(&radio, platform).await;
        let text = held_read_ended_by(&radio, &session, None, async || {
            host.ingest(RadioIngress::AdapterState(AdapterSnapshot {
                availability: AdapterAvailability::Available,
                authorization: AdapterAuthorization::Granted,
                power: AdapterPower::Off,
                safe_reason: None,
            }));
        })
        .await;
        assert_eq!(
            observed(&text),
            expected("adapter-loss-during-operation", platform),
            "{platform:?}"
        );

        // connect-not-established
        let radio = failing_with(RequestKind::Connect, not_established(platform));
        let (host, _) = open(&radio, platform).await;
        let session = host.open_session("rn").unwrap();
        let text = call(
            &session,
            "connection.connect",
            &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "c"}).to_string(),
        )
        .await;
        assert_eq!(
            observed(&text),
            expected("connect-not-established", platform),
            "{platform:?}"
        );

        // peer-not-found
        let radio = failing_with(
            RequestKind::Connect,
            PlatformFailure::not_dispatched(FailureKind::PeerUnknown, "never observed"),
        );
        let (host, _) = open(&radio, platform).await;
        let session = host.open_session("rn").unwrap();
        let text = call(
            &session,
            "connection.connect",
            &json!({"peerId": POLAR, "lease": "lease-1", "operationId": "c"}).to_string(),
        )
        .await;
        assert_eq!(
            observed(&text).0,
            expected("peer-not-found", platform).0,
            "{platform:?}"
        );

        // security-refused
        let radio = failing_with(RequestKind::Read, security(platform));
        let (_host, session) = connected(&radio, platform).await;
        let text = call(&session, "gatt.read", &read_args("r", None)).await;
        assert_eq!(
            observed(&text),
            expected("security-refused", platform),
            "{platform:?}"
        );

        // operation-timed-out
        let radio = Scripted::polar();
        let (_host, session) = connected(&radio, platform).await;
        let text = held_read_ended_by(&radio, &session, Some(50), async || {}).await;
        assert_eq!(
            observed(&text),
            expected("operation-timed-out", platform),
            "{platform:?}"
        );

        // operation-cancelled
        let radio = Scripted::polar();
        let (_host, session) = connected(&radio, platform).await;
        let cancelling = session.clone();
        let text = held_read_ended_by(&radio, &session, None, async || {
            ok(&call(
                &cancelling,
                "op.cancel",
                &json!({"operationId": "r"}).to_string(),
            )
            .await);
        })
        .await;
        assert_eq!(
            observed(&text),
            expected("operation-cancelled", platform),
            "{platform:?}"
        );
    }
}
