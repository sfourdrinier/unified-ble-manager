//! The Tauri plugin names every identity as its 4.x dispatcher did (origin/main
//! `native/tauri/src/btleplug_dispatcher.rs:465-539,1503-1505,2160-2162,2383`):
//! one per-dispatcher counter numbers, in order, the attachment
//! (`tauri-attachment-{n}`), the backend instance (`tauri-btleplug-{n}`), the
//! backend and adapter generations (`tauri-backend-generation-{n}`,
//! `tauri-adapter-generation-{n}`), the renderer lease, then each resource
//! (`connection-{n}`, `connection-id-{n}`, `connection-generation-{n}`, …).
//! Names the plugin needs only internally (the core lease, a subscription's
//! core consumer) never take a number from that counter.
//!
//! The host recovers from an adapter loss: a link the loss ended releases as
//! released, a connect on the lost adapter is refused before any effect, and
//! the next connect after the adapter returns succeeds (after the renderer
//! re-attaches, finding 57).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tauri::ipc::Channel;
use ubm_desktop::{
    CharacteristicSnapshot, DesktopCentral, FakeRadio, OpControl, PeerSnapshot, PropertyFlags,
    RadioEvent, ServiceSnapshot,
};

use super::{object, string, BtleplugDispatcher, IpcEventSink, IpcValue};
use crate::desktop_core::CoreAuthority;
use crate::AuthenticatedCaller;

const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";

fn text(value: &IpcValue, key: &str) -> String {
    match value {
        IpcValue::Object(fields) => match fields.get(key) {
            Some(IpcValue::String(text)) => text.clone(),
            other => panic!("field {key} is not a string: {other:?}"),
        },
        other => panic!("expected an object, got {other:?}"),
    }
}

fn field<'a>(value: &'a IpcValue, key: &str) -> &'a IpcValue {
    match value {
        IpcValue::Object(fields) => fields
            .get(key)
            .unwrap_or_else(|| panic!("missing field {key}")),
        other => panic!("expected an object, got {other:?}"),
    }
}

fn version_offer() -> BTreeMap<String, IpcValue> {
    let range = |axis: &str, value: i64| {
        let version = object([
            ("axis", string(axis)),
            ("value", IpcValue::Number(serde_json::Number::from(value))),
        ]);
        object([
            ("axis", string(axis)),
            ("minimum", version.clone()),
            ("maximum", version),
        ])
    };
    let IpcValue::Object(offer) = object([
        ("backendContract", range("backend-contract", 1)),
        ("capabilitySchema", range("capability-schema", 1)),
        ("eventSchema", range("event-schema", 1)),
        ("traceFormat", range("trace-format", 1)),
        ("ipcProtocol", range("ipc-protocol", 4)),
    ]) else {
        panic!("the version offer is an object");
    };
    offer
}

type Opened = Arc<StdMutex<Option<DesktopCentral<FakeRadio>>>>;

/// A production-profiled dispatcher over `radio`; `opened` receives its central.
fn dispatcher_over(radio: FakeRadio) -> (BtleplugDispatcher, Opened) {
    let opened: Opened = Arc::new(StdMutex::new(None));
    let slot = Arc::clone(&opened);
    let radio = StdMutex::new(Some(radio));
    let dispatcher = BtleplugDispatcher::with_profiled_opener(
        None,
        Arc::new(move |profile| {
            let slot = Arc::clone(&slot);
            let radio = radio.lock().expect("radio").take().expect("opened once");
            Box::pin(async move {
                let central = ubm_desktop::executor::desktop_runtime()
                    .spawn(DesktopCentral::open_with(radio, profile))
                    .await
                    .expect("open joins")
                    .map_err(|error| super::DispatchError::from_core(&error))?;
                *slot.lock().expect("slot") = Some(central.clone());
                let authority: Arc<dyn CoreAuthority> = Arc::new(central);
                Ok(authority)
            })
        }),
    );
    (dispatcher, opened)
}

async fn advertise(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
    let radio = central.boundary();
    radio.set_services(
        peer_id,
        vec![ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![CharacteristicSnapshot {
                uuid: HRM_MEASUREMENT.to_owned(),
                occurrence: 0,
                properties: PropertyFlags {
                    read: true,
                    write: false,
                    write_without_response: false,
                    notify: true,
                    indicate: false,
                },
                descriptors: Vec::new(),
            }],
        }],
    );
    radio.push_event(RadioEvent::Advertisement(PeerSnapshot {
        id: peer_id.to_owned(),
        address: None,
        service_uuids: vec![HRM_SERVICE.to_owned()],
        rssi: Some(-60),
        local_name: None,
        manufacturer_data: Vec::new(),
        service_data: Vec::new(),
        tx_power_level: None,
        extras: ubm_desktop::AdvertisementExtras::default(),
    }));
    for _ in 0..5000 {
        if central.peer_key_for(peer_id).await.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("peer {peer_id} never resolved");
}

fn leased(lease: &(String, String), entries: Vec<(&str, IpcValue)>) -> BTreeMap<String, IpcValue> {
    let mut payload: BTreeMap<String, IpcValue> = entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
    payload.insert("__expectedLeaseId".to_owned(), string(lease.0.clone()));
    payload.insert(
        "__expectedLeaseGeneration".to_owned(),
        string(lease.1.clone()),
    );
    payload
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_identity_is_numbered_as_the_4x_dispatcher_numbered_it() {
    let (dispatcher, opened) = dispatcher_over(FakeRadio::new());
    let caller = AuthenticatedCaller::new("test-app".to_owned(), "main".to_owned());
    let response = dispatcher
        .bootstrap(
            caller.clone(),
            IpcEventSink::new(Channel::new(|_| Ok(()))),
            version_offer(),
        )
        .await
        .expect("bootstrap");
    let bootstrap = field(&response, "bootstrap");
    let attachment = field(bootstrap, "attachment");
    assert_eq!(text(attachment, "attachmentId"), "tauri-attachment-1");
    assert_eq!(text(attachment, "backendInstanceId"), "tauri-btleplug-2");
    assert_eq!(
        text(attachment, "backendGeneration"),
        "tauri-backend-generation-3"
    );
    assert_eq!(
        text(field(attachment, "adapter"), "adapterGeneration"),
        "tauri-adapter-generation-4"
    );
    let lease = field(bootstrap, "rendererLease");
    assert_eq!(text(lease, "leaseId"), "tauri-lease-5");
    assert_eq!(text(lease, "generation"), "tauri-lease-generation-6");

    let central = opened.lock().expect("slot").clone().expect("opened");
    advertise(&central, "peer-1").await;
    let lease = (
        "tauri-lease-5".to_owned(),
        "tauri-lease-generation-6".to_owned(),
    );
    let payload = |entries: Vec<(&str, IpcValue)>| leased(&lease, entries);
    let connected = dispatcher
        .execute(
            &caller,
            "connection.connect",
            payload(vec![("peerId", string("peer-1"))]),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("connect");
    assert_eq!(text(&connected, "handle"), "connection-7");
    assert_eq!(text(&connected, "connectionId"), "connection-id-8");
    assert_eq!(
        text(&connected, "connectionGeneration"),
        "connection-generation-9"
    );
}

/// One attached renderer: every event its channel received, and what a
/// route needs from its bootstrap.
struct Renderer {
    caller: AuthenticatedCaller,
    events: Arc<StdMutex<Vec<serde_json::Value>>>,
    lease: (String, String),
    versions: IpcValue,
    renderer: IpcValue,
}

async fn attach(dispatcher: &BtleplugDispatcher) -> (Renderer, IpcValue) {
    let caller = AuthenticatedCaller::new("test-app".to_owned(), "main".to_owned());
    let events = Arc::new(StdMutex::new(Vec::new()));
    let log = Arc::clone(&events);
    let sink = IpcEventSink::new(Channel::new(move |body| {
        if let tauri::ipc::InvokeResponseBody::Json(json) = body {
            log.lock()
                .expect("event log")
                .push(serde_json::from_str(&json).expect("event json"));
        }
        Ok(())
    }));
    let response = dispatcher
        .bootstrap(caller.clone(), sink, version_offer())
        .await
        .expect("bootstrap");
    let bootstrap = field(&response, "bootstrap");
    let lease = field(bootstrap, "rendererLease");
    (
        Renderer {
            caller,
            events,
            lease: (text(lease, "leaseId"), text(lease, "generation")),
            versions: field(bootstrap, "versions").clone(),
            renderer: field(bootstrap, "renderer").clone(),
        },
        field(bootstrap, "attachment").clone(),
    )
}

/// The full route request the webview sends under `attachment`.
async fn route(
    dispatcher: &BtleplugDispatcher,
    renderer: &Renderer,
    attachment: &IpcValue,
    command: &str,
    correlation: &str,
    entries: Vec<(&str, IpcValue)>,
) -> Result<IpcValue, super::DispatchError> {
    let payload = IpcValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    );
    let envelope = object([
        ("command", string(command)),
        ("correlation", string(correlation)),
        ("payload", payload),
        (
            "rendererLease",
            object([
                ("leaseId", string(renderer.lease.0.clone())),
                ("generation", string(renderer.lease.1.clone())),
            ]),
        ),
        ("attachmentId", string(text(attachment, "attachmentId"))),
        ("attachment", attachment.clone()),
        ("renderer", renderer.renderer.clone()),
        ("versions", renderer.versions.clone()),
        ("binaryPayload", IpcValue::Null),
    ]);
    dispatcher
        .route(
            renderer.caller.clone(),
            BTreeMap::from([
                ("kind".to_owned(), string("route")),
                ("envelope".to_owned(), envelope),
            ]),
        )
        .await
        .map(|response| field(&response, "payload").clone())
}

/// IPC protocol 4: after an adapter loss the dispatcher (never the webview)
/// rebinds the renderer to the central's new attachment and announces it on
/// the `attachment` stream; until then, and afterwards for the replaced
/// attachment, work is refused `backend.reset` (releases excepted).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_adapter_loss_rebinds_the_renderer_and_the_next_connect_succeeds() {
    let radio = FakeRadio::new();
    radio.set_os_policy(ubm_desktop::AdmissionPolicy::CoreBluetooth, true);
    radio.set_adapter_state(ubm_desktop::AdapterPowerState::PoweredOn);
    let (dispatcher, opened) = dispatcher_over(radio);
    let (renderer, first_attachment) = attach(&dispatcher).await;
    let central = opened.lock().expect("slot").clone().expect("opened");
    advertise(&central, "peer-1").await;
    let connect = |attachment: IpcValue, correlation: &'static str| {
        let dispatcher = &dispatcher;
        let renderer = &renderer;
        async move {
            route(
                dispatcher,
                renderer,
                &attachment,
                "connection.connect",
                correlation,
                vec![("peerId", string("peer-1"))],
            )
            .await
        }
    };
    let first = connect(first_attachment.clone(), "connect-1")
        .await
        .expect("connect");

    let mut resets = central.adapter_reset_events();
    central.boundary().push_event(RadioEvent::AdapterState(
        ubm_desktop::AdapterPowerState::PoweredOff,
    ));
    tokio::time::timeout(Duration::from_secs(5), resets.recv())
        .await
        .expect("a reset is published")
        .expect("reset event");
    let mut announced = None;
    for _ in 0..500 {
        announced = renderer
            .events
            .lock()
            .expect("event log")
            .iter()
            .find(|event| event["streamId"] == "attachment")
            .cloned();
        if announced.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let announced = announced.expect("the rebind is announced");
    let rebound = &announced["item"]["value"];
    assert_eq!(rebound["kind"], "backend-restarted");
    assert_eq!(rebound["schemaVersion"], 1);
    assert_eq!(
        rebound["previousAttachmentId"].as_str(),
        Some(text(&first_attachment, "attachmentId").as_str())
    );
    assert_eq!(
        rebound["attachmentId"],
        rebound["attachment"]["attachmentId"]
    );
    let current = central.attachment();
    assert_eq!(
        rebound["attachmentId"].as_str(),
        Some(current.attachment_id().as_str())
    );
    let next_attachment =
        IpcValue::from_wire(rebound["attachment"].clone()).expect("an IPC attachment");

    // The replaced attachment is refused; its links still release.
    let stale = connect(first_attachment.clone(), "connect-2")
        .await
        .expect_err("the replaced attachment");
    assert_eq!(stale.code.as_str(), "backend.reset");
    let released = route(
        &dispatcher,
        &renderer,
        &first_attachment,
        "connection.disconnect",
        "disconnect-1",
        vec![
            ("connectionHandle", string(text(&first, "handle"))),
            ("peerId", string("peer-1")),
            ("connectionId", string(text(&first, "connectionId"))),
            ("ownerLeaseId", string(text(&first, "ownerLeaseId"))),
            (
                "connectionGeneration",
                string(text(&first, "connectionGeneration")),
            ),
        ],
    )
    .await
    .expect("the ended link releases");
    assert_eq!(text(&released, "state"), "released");

    // A webview never picks an attachment: one the dispatcher did not give
    // it is a protocol violation, not a rebind.
    let mut forged_wire = rebound["attachment"].clone();
    forged_wire["attachmentId"] = serde_json::Value::from("tauri-attachment-999");
    let forged = IpcValue::from_wire(forged_wire).expect("an IPC attachment");
    let refused = connect(forged, "connect-forged")
        .await
        .expect_err("a forged attachment");
    assert_eq!(refused.code.as_str(), "protocol.violation");

    // Under the announced attachment: refused before any effect while off.
    let refused = connect(next_attachment.clone(), "connect-3")
        .await
        .expect_err("the adapter is off");
    assert_eq!(refused.code.as_str(), "adapter.powered-off");
    central.boundary().push_event(RadioEvent::AdapterState(
        ubm_desktop::AdapterPowerState::PoweredOn,
    ));
    let mut again = None;
    for attempt in 0..500 {
        let correlation: &'static str = Box::leak(format!("connect-on-{attempt}").into_boxed_str());
        if let Ok(connected) = connect(next_attachment.clone(), correlation).await {
            again = Some(connected);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let again = again.expect("the next connect after the adapter returns succeeds");
    assert_ne!(
        text(&again, "connectionGeneration"),
        text(&first, "connectionGeneration")
    );
}
