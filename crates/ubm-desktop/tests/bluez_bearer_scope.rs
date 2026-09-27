//! Actual vendored D-Bus calls against a private dual-bearer fixture.
//! The fixture distinguishes generic Device1 from implemented LE1 methods;
//! this is protocol evidence, not a radio or daemon-version attestation.
#![cfg(target_os = "linux")]

use bluez_async::{BluetoothSession, DeviceId};
use dbus::arg::{PropMap, Variant};
use dbus::channel::{MatchingReceiver, Sender};
use dbus::message::MatchRule;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";

#[derive(Debug)]
struct Bearers {
    le: bool,
    classic: bool,
    connects: usize,
    disconnects: usize,
    le_connects: usize,
    le_disconnects: usize,
    owner: String,
    hold_connect: bool,
    pending_connect: Option<dbus::Message>,
    connect_entered: Arc<tokio::sync::Notify>,
    refuse_disconnect: bool,
    destinations: Vec<String>,
    hold_disconnect: bool,
    pending_disconnect: Option<dbus::Message>,
    disconnect_entered: Arc<tokio::sync::Notify>,
    unknown_connect: bool,
    timeout_disconnect: bool,
    unconfirmed_connect: bool,
    refuse_connect_once: Option<bool>,
}

async fn fixture(
    classic: bool,
) -> (
    BluetoothSession,
    DeviceId,
    Arc<Mutex<Bearers>>,
    tokio::task::JoinHandle<dbus_tokio::connection::IOResourceError>,
    Arc<dbus::nonblock::SyncConnection>,
) {
    assert_eq!(
        std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
        Ok("1")
    );
    let (resource, server) = dbus_tokio::connection::new_session_sync().unwrap();
    let worker = tokio::spawn(resource);
    server
        .request_name("org.bluez", false, false, false)
        .await
        .unwrap();
    let state = Arc::new(Mutex::new(Bearers {
        le: true,
        classic,
        connects: 0,
        disconnects: 0,
        le_connects: 0,
        le_disconnects: 0,
        owner: server.unique_name().to_string(),
        hold_connect: false,
        pending_connect: None,
        connect_entered: Arc::new(tokio::sync::Notify::new()),
        refuse_disconnect: false,
        destinations: Vec::new(),
        hold_disconnect: false,
        pending_disconnect: None,
        disconnect_entered: Arc::new(tokio::sync::Notify::new()),
        unknown_connect: false,
        timeout_disconnect: false,
        unconfirmed_connect: false,
        refuse_connect_once: None,
    }));
    let observed = state.clone();
    server.start_receive(
        MatchRule::new_method_call(),
        Box::new(move |message, connection| {
            assert_eq!(message.path().as_deref(), Some(PATH));
            let mut state = observed.lock().unwrap();
            state
                .destinations
                .push(message.destination().unwrap().to_string());
            let response = match (message.interface().as_deref(), message.member().as_deref()) {
                (Some("org.bluez.Bearer.LE1"), Some("Connect")) => {
                    state.le_connects += 1;
                    state.connect_entered.notify_one();
                    if let Some(partially_connected) = state.refuse_connect_once.take() {
                        state.le = partially_connected;
                        return connection
                            .send(message.error(
                                &dbus::strings::ErrorName::new("org.bluez.Error.Failed").unwrap(),
                                &std::ffi::CString::new("connect refused after admission").unwrap(),
                            ))
                            .is_ok();
                    }
                    if state.unknown_connect {
                        return connection
                            .send(
                                message.error(
                                    &dbus::strings::ErrorName::new(
                                        "org.freedesktop.DBus.Error.UnknownMethod",
                                    )
                                    .unwrap(),
                                    &std::ffi::CString::new("LE method absent").unwrap(),
                                ),
                            )
                            .is_ok();
                    }
                    if state.hold_connect {
                        state.pending_connect = Some(message);
                        return true;
                    }
                    state.le = !state.unconfirmed_connect;
                    message.method_return()
                }
                (Some("org.bluez.Bearer.LE1"), Some("Disconnect")) => {
                    state.le_disconnects += 1;
                    state.disconnect_entered.notify_one();
                    if state.timeout_disconnect {
                        return connection
                            .send(
                                message.error(
                                    &dbus::strings::ErrorName::new(
                                        "org.freedesktop.DBus.Error.NoReply",
                                    )
                                    .unwrap(),
                                    &std::ffi::CString::new("release outcome unknown").unwrap(),
                                ),
                            )
                            .is_ok();
                    }
                    if state.hold_disconnect {
                        state.pending_disconnect = Some(message);
                        return true;
                    }
                    if state.refuse_disconnect {
                        state.refuse_disconnect = false;
                        return connection
                            .send(message.error(
                                &dbus::strings::ErrorName::new("org.bluez.Error.Failed").unwrap(),
                                &std::ffi::CString::new("scoped release refused").unwrap(),
                            ))
                            .is_ok();
                    }
                    state.le = false;
                    message.method_return()
                }
                (Some("org.bluez.Device1"), Some("Connect")) => {
                    state.connects += 1;
                    // BlueZ Device1.Connect adds the other bearer when LE is
                    // already connected; the LE scan filter does not constrain it.
                    state.classic = true;
                    message.method_return()
                }
                (Some("org.bluez.Device1"), Some("Disconnect")) => {
                    state.disconnects += 1;
                    state.le = false;
                    state.classic = false;
                    message.method_return()
                }
                (Some("org.freedesktop.DBus.Properties"), Some("Get")) => {
                    let (interface, property): (String, String) = message.read2().unwrap();
                    assert!(
                        (interface == "org.bluez.Device1" && property == "ServicesResolved")
                            || (interface == "org.bluez.Bearer.LE1" && property == "Connected")
                    );
                    message.method_return().append1(Variant(state.le))
                }
                (Some("org.freedesktop.DBus.Properties"), Some("GetAll")) => {
                    let mut properties = PropMap::new();
                    properties.insert(
                        "Address".into(),
                        Variant(Box::new("AA:BB:CC:DD:EE:FF".to_owned())),
                    );
                    properties.insert("AddressType".into(), Variant(Box::new("public".to_owned())));
                    for (name, value) in [
                        ("Connected", state.le || state.classic),
                        ("ServicesResolved", state.le),
                        ("Paired", false),
                        ("Trusted", false),
                        ("Blocked", false),
                        ("LegacyPairing", false),
                    ] {
                        properties.insert(name.into(), Variant(Box::new(value)));
                    }
                    message.method_return().append1(properties)
                }
                other => panic!("unexpected private-bus call: {other:?}"),
            };
            connection.send(response).unwrap();
            true
        }),
    );
    let (_, session) = BluetoothSession::new_session_bus().await.unwrap();
    let device = serde_json::from_value(serde_json::json!({"object_path": PATH})).unwrap();
    (session, device, state, worker, server)
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_le_connect_does_not_add_classic_bearer() {
    let (session, device, state, worker, _server) = fixture(false).await;
    let owner = state.lock().unwrap().owner.clone();
    tokio::time::timeout(Duration::from_secs(3), session.connect_le(&device, &owner))
        .await
        .unwrap()
        .unwrap();
    session.drain_match_cleanup().await.unwrap();
    worker.abort();
    let state = state.lock().unwrap();
    assert!(state.le, "positive LE connection must remain available");
    assert!(
        !state.classic,
        "LE acquisition called generic Connect and created an unrelated Classic bearer: {state:?}"
    );
    assert_eq!(
        state.connects, 0,
        "no generic fallback may hide a scoped admission failure"
    );
    assert_eq!(state.le_connects, 1);
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_le_release_preserves_other_apps_classic_bearer() {
    let (session, device, state, worker, _server) = fixture(true).await;
    let owner = state.lock().unwrap().owner.clone();
    session.connect_le(&device, &owner).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        session.disconnect_le(&device, &owner),
    )
    .await
    .unwrap()
    .unwrap();
    worker.abort();
    let state = state.lock().unwrap();
    assert!(!state.le, "the owned LE release must actually happen");
    assert!(
        state.classic,
        "LE release called generic Disconnect and destroyed another application's Classic bearer: {state:?}"
    );
    assert_eq!(
        state.disconnects, 0,
        "no device-wide disconnect is a scoped LE release"
    );
    assert_eq!(state.le_disconnects, 1);
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_cancelled_wait_retains_late_le_acquisition_for_cleanup() {
    let (session, device, state, worker, server) = fixture(true).await;
    let (owner, entered) = {
        let mut state = state.lock().unwrap();
        state.le = false;
        state.hold_connect = true;
        (state.owner.clone(), state.connect_entered.clone())
    };
    let connecting = tokio::spawn({
        let session = session.clone();
        let device = device.clone();
        let owner = owner.clone();
        async move { session.connect_le(&device, &owner).await }
    });
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    connecting.abort();
    assert!(connecting.await.unwrap_err().is_cancelled());
    let cleanup = tokio::spawn({
        let session = session.clone();
        let device = device.clone();
        let owner = owner.clone();
        async move { session.disconnect_le(&device, &owner).await }
    });
    // The actual accepted reply is delivered after its original waiter died.
    let response = {
        let mut state = state.lock().unwrap();
        state.le = true;
        state.pending_connect.take().unwrap().method_return()
    };
    server.send(response).unwrap();
    tokio::time::timeout(Duration::from_secs(3), cleanup)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let state = state.lock().unwrap();
    assert!(!state.le);
    assert!(state.classic);
    assert_eq!((state.le_connects, state.le_disconnects), (1, 1));
    assert_eq!((state.connects, state.disconnects), (0, 0));
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_scoped_release_refusal_is_retryable_without_classic_effects() {
    let (session, device, state, worker, _server) = fixture(true).await;
    let owner = state.lock().unwrap().owner.clone();
    session.connect_le(&device, &owner).await.unwrap();
    state.lock().unwrap().refuse_disconnect = true;
    let error = session.disconnect_le(&device, &owner).await.unwrap_err();
    assert!(error.to_string().contains("scoped release refused"));
    assert!(state.lock().unwrap().le);
    session.disconnect_le(&device, &owner).await.unwrap();
    let state = state.lock().unwrap();
    assert!(!state.le);
    assert!(state.classic);
    assert_eq!(state.le_disconnects, 2);
    assert_eq!(state.disconnects, 0);
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_wrong_or_well_known_owner_refuses_before_effects() {
    let (session, device, state, worker, _server) = fixture(true).await;
    for owner in ["org.bluez", ":999999.999999", ":1.2\n", ":1.2\0"] {
        assert!(session.connect_le(&device, owner).await.is_err());
    }
    let state = state.lock().unwrap();
    assert_eq!(
        (state.le_connects, state.connects, state.disconnects),
        (0, 0, 0)
    );
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_owner_pin_applies_to_gatt_directory_and_replacement_refuses() {
    let (session, device, state, worker, server) = fixture(true).await;
    let owner = state.lock().unwrap().owner.clone();
    let pinned = session.with_le_owner(&owner).await.unwrap();
    pinned.connect_le(&device, &owner).await.unwrap();
    pinned.get_device_info(&device).await.unwrap();
    assert!(
        state
            .lock()
            .unwrap()
            .destinations
            .iter()
            .all(|actual| actual == &owner)
    );
    server.release_name("org.bluez").await.unwrap();
    let (new_resource, replacement) = dbus_tokio::connection::new_session_sync().unwrap();
    let new_worker = tokio::spawn(new_resource);
    replacement
        .request_name("org.bluez", false, false, false)
        .await
        .unwrap();
    assert!(
        pinned
            .connect_le(&device, &owner)
            .await
            .unwrap_err()
            .to_string()
            .contains("owner changed")
    );
    assert!(
        pinned
            .disconnect_le(&device, &owner)
            .await
            .unwrap_err()
            .to_string()
            .contains("owner changed")
    );
    let state = state.lock().unwrap();
    assert_eq!(
        (
            state.le_connects,
            state.le_disconnects,
            state.connects,
            state.disconnects
        ),
        (1, 0, 0, 0)
    );
    worker.abort();
    new_worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_cancelled_release_reuses_accepted_reply_before_retry() {
    let (session, device, state, worker, server) = fixture(true).await;
    let (owner, entered) = {
        let mut state = state.lock().unwrap();
        state.hold_disconnect = true;
        (state.owner.clone(), state.disconnect_entered.clone())
    };
    session.connect_le(&device, &owner).await.unwrap();
    let releasing = tokio::spawn({
        let session = session.clone();
        let device = device.clone();
        let owner = owner.clone();
        async move { session.disconnect_le(&device, &owner).await }
    });
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    releasing.abort();
    assert!(releasing.await.unwrap_err().is_cancelled());
    let response = {
        let mut state = state.lock().unwrap();
        state.le = false;
        state.pending_disconnect.take().unwrap().method_return()
    };
    server.send(response).unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        session.disconnect_le(&device, &owner),
    )
    .await
    .unwrap()
    .unwrap();
    // A later external link is not ours: repeated cleanup is a no-op.
    state.lock().unwrap().le = true;
    session.disconnect_le(&device, &owner).await.unwrap();
    let state = state.lock().unwrap();
    assert!(state.le && state.classic);
    assert_eq!(
        (state.le_connects, state.le_disconnects, state.disconnects),
        (1, 1, 0)
    );
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_abandoned_queued_cleanup_cannot_release_a_later_external_link() {
    let (session, device, state, worker, server) = fixture(true).await;
    let (owner, entered) = {
        let mut state = state.lock().unwrap();
        state.hold_disconnect = true;
        (state.owner.clone(), state.disconnect_entered.clone())
    };
    session.connect_le(&device, &owner).await.unwrap();
    let releasing = tokio::spawn({
        let session = session.clone();
        let device = device.clone();
        let owner = owner.clone();
        async move { session.disconnect_le(&device, &owner).await }
    });
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    let mut queued = Box::pin(session.disconnect_le(&device, &owner));
    assert!(
        futures::poll!(queued.as_mut()).is_pending(),
        "the first release owns the peer gate"
    );
    let response = {
        let mut state = state.lock().unwrap();
        state.le = false;
        state.pending_disconnect.take().unwrap().method_return()
    };
    server.send(response).unwrap();
    releasing.await.unwrap().unwrap();
    // The queued waiter still holds the old entry but never acquired its gate.
    drop(queued);
    state.lock().unwrap().le = true;
    session.disconnect_le(&device, &owner).await.unwrap();
    let state = state.lock().unwrap();
    assert!(state.le && state.classic);
    assert_eq!((state.le_connects, state.le_disconnects), (1, 1));
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_absent_le_method_has_no_fabricated_cleanup_debt() {
    let (session, device, state, worker, _server) = fixture(true).await;
    let owner = {
        let mut state = state.lock().unwrap();
        state.unknown_connect = true;
        state.owner.clone()
    };
    assert!(
        session
            .connect_le(&device, &owner)
            .await
            .unwrap_err()
            .to_string()
            .contains("LE method absent")
    );
    session.disconnect_le(&device, &owner).await.unwrap();
    let state = state.lock().unwrap();
    assert!(
        state.le && state.classic,
        "pre-effect refusal cannot release somebody else's bearer"
    );
    assert_eq!(state.le_disconnects, 0);
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_absent_le_method_stays_released_after_owner_departure() {
    let (session, device, state, worker, server) = fixture(true).await;
    let owner = {
        let mut state = state.lock().unwrap();
        state.unknown_connect = true;
        state.owner.clone()
    };
    assert!(session.connect_le(&device, &owner).await.is_err());
    server.release_name("org.bluez").await.unwrap();
    session.disconnect_le(&device, &owner).await.unwrap();
    session.disconnect_le(&device, &owner).await.unwrap();
    let state = state.lock().unwrap();
    assert!(state.le && state.classic);
    assert_eq!((state.le_connects, state.le_disconnects), (1, 0));
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_le_confirmation_timeout_is_not_reported_as_service_discovery() {
    let (session, device, state, worker, _server) = fixture(true).await;
    let owner = {
        let mut state = state.lock().unwrap();
        state.le = false;
        state.unconfirmed_connect = true;
        state.owner.clone()
    };
    let error = tokio::time::timeout(Duration::from_secs(7), session.connect_le(&device, &owner))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "LE connection was not confirmed within 5 s"
    );
    session.disconnect_le(&device, &owner).await.unwrap();
    assert!(state.lock().unwrap().classic);
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_indeterminate_release_never_resends_or_admits_new_connect() {
    let (session, device, state, worker, _server) = fixture(true).await;
    let owner = state.lock().unwrap().owner.clone();
    session.connect_le(&device, &owner).await.unwrap();
    state.lock().unwrap().timeout_disconnect = true;
    for _ in 0..2 {
        assert!(
            session
                .disconnect_le(&device, &owner)
                .await
                .unwrap_err()
                .to_string()
                .contains("release outcome unknown")
        );
    }
    assert!(
        session
            .connect_le(&device, &owner)
            .await
            .unwrap_err()
            .to_string()
            .contains("release outcome unknown")
    );
    let state = state.lock().unwrap();
    assert!(state.le && state.classic);
    assert_eq!(
        (state.le_connects, state.le_disconnects, state.disconnects),
        (1, 1, 0)
    );
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_refused_connect_retries_after_exact_cleanup_with_or_without_partial_link() {
    for partially_connected in [false, true] {
        let (session, device, state, worker, server) = fixture(true).await;
        let owner = {
            let mut state = state.lock().unwrap();
            state.le = false;
            state.refuse_connect_once = Some(partially_connected);
            state.owner.clone()
        };
        let first = session
            .connect_le(&device, &owner)
            .await
            .unwrap_err()
            .to_string();
        assert!(first.contains("connect refused after admission"));
        // Before the shared central's compensation boundary, a second caller
        // cannot overwrite the original uncertain physical obligation.
        assert_eq!(
            session
                .connect_le(&device, &owner)
                .await
                .unwrap_err()
                .to_string(),
            first
        );
        assert_eq!(state.lock().unwrap().le_connects, 1);
        session.disconnect_le(&device, &owner).await.unwrap();
        assert_eq!(
            state.lock().unwrap().le_disconnects,
            usize::from(partially_connected)
        );
        session.connect_le(&device, &owner).await.unwrap();
        assert!(state.lock().unwrap().le);
        assert!(state.lock().unwrap().classic);
        assert_eq!(state.lock().unwrap().le_connects, 2);
        session.disconnect_le(&device, &owner).await.unwrap();
        assert!(!state.lock().unwrap().le);
        assert!(state.lock().unwrap().classic);
        server.release_name("org.bluez").await.unwrap();
        worker.abort();
    }
}
