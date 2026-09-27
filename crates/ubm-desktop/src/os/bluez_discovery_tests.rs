use super::*;
use dbus::arg::PropMap;
use dbus::channel::{MatchingReceiver, Sender};
use dbus::message::MatchRule;
use std::time::Duration;

#[derive(Default)]
struct Fixture {
    hold_start: bool,
    start_reply: Option<dbus::Message>,
    started: Arc<tokio::sync::Notify>,
    stopped: Arc<tokio::sync::Notify>,
    stops: usize,
    starts: usize,
    generic_connects: usize,
    discovered: bool,
    refuse_stop: bool,
    hold_stop: bool,
    stop_reply: Option<dbus::Message>,
    refuse_start: bool,
    timeout_stop: bool,
    hold_found: bool,
    found_reply: Option<dbus::Message>,
    found: Arc<tokio::sync::Notify>,
}

async fn fixture() -> (
    Arc<super::super::Bluez>,
    Arc<StdMutex<Fixture>>,
    Arc<dbus::nonblock::SyncConnection>,
    tokio::task::JoinHandle<dbus_tokio::connection::IOResourceError>,
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
    let state = Arc::new(StdMutex::new(Fixture::default()));
    let observed = state.clone();
    server.start_receive(
        MatchRule::new_method_call(),
        Box::new(move |message, connection| {
            let mut state = observed.lock().unwrap();
            let error = |name: &str, detail: &str| {
                message.error(
                    &dbus::strings::ErrorName::new(name).unwrap(),
                    &std::ffi::CString::new(detail).unwrap(),
                )
            };
            let response = match message.member().as_deref() {
                Some("GetAll") if state.discovered && state.hold_found => {
                    state.found.notify_one();
                    state.found_reply = Some(message);
                    return true;
                }
                Some("GetAll") if state.discovered => {
                    message.method_return().append1(PropMap::new())
                }
                Some("GetAll") => {
                    error("org.freedesktop.DBus.Error.UnknownObject", "not discovered")
                }
                Some("GetManagedObjects") => {
                    message.method_return().append1(std::collections::HashMap::<
                        dbus::Path<'static>,
                        std::collections::HashMap<String, PropMap>,
                    >::new())
                }
                Some("SetDiscoveryFilter") => message.method_return(),
                Some("ConnectDevice") => {
                    state.generic_connects += 1;
                    error(
                        "org.freedesktop.DBus.Error.UnknownMethod",
                        "unsafe connect forbidden",
                    )
                }
                Some("StartDiscovery") => {
                    state.starts += 1;
                    state.started.notify_one();
                    if state.refuse_start {
                        return connection
                            .send(error("org.bluez.Error.NotReady", "adapter not ready"))
                            .is_ok();
                    }
                    if state.hold_start {
                        state.start_reply = Some(message);
                        return true;
                    }
                    state.discovered = true;
                    message.method_return()
                }
                Some("StopDiscovery") => {
                    state.stops += 1;
                    state.stopped.notify_one();
                    if state.hold_stop {
                        state.stop_reply = Some(message);
                        return true;
                    }
                    if state.timeout_stop {
                        return connection
                            .send(error(
                                "org.freedesktop.DBus.Error.NoReply",
                                "stop outcome unknown",
                            ))
                            .is_ok();
                    }
                    if state.refuse_stop {
                        state.refuse_stop = false;
                        error("org.bluez.Error.Failed", "owned stop refused")
                    } else {
                        message.method_return()
                    }
                }
                other => panic!("unexpected fixture request {other:?}"),
            };
            connection.send(response).unwrap();
            true
        }),
    );
    let bluez = super::super::Bluez::open("hci0", crate::boundary::BluezBus::Session)
        .await
        .unwrap();
    (bluez, state, server, worker)
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_address_resolution_cancellation_retains_accepted_start() {
    let (bluez, state, server, worker) = fixture().await;
    let (started, stopped) = {
        let mut state = state.lock().unwrap();
        state.hold_start = true;
        (state.started.clone(), state.stopped.clone())
    };
    let resolving = tokio::spawn({
        let bluez = bluez.clone();
        async move {
            bluez
                .resolve_address("AA:BB:CC:DD:EE:FF", crate::boundary::AddressType::Public)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    resolving.abort();
    assert!(resolving.await.unwrap_err().is_cancelled());
    let reply = state
        .lock()
        .unwrap()
        .start_reply
        .take()
        .unwrap()
        .method_return();
    server.send(reply).unwrap();
    tokio::time::timeout(Duration::from_secs(3), stopped.notified())
        .await
        .unwrap();
    assert_eq!(state.lock().unwrap().stops, 1);
    assert_eq!(state.lock().unwrap().generic_connects, 0);
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_address_resolution_reports_stop_refusal_not_success() {
    let (bluez, state, _server, worker) = fixture().await;
    state.lock().unwrap().refuse_stop = true;
    let result = bluez
        .resolve_address("AA:BB:CC:DD:EE:FF", crate::boundary::AddressType::Public)
        .await;
    assert!(
        result
            .unwrap_err()
            .detail()
            .unwrap()
            .contains("owned stop refused")
    );
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_address_resolution_canceled_stop_settles_original_reply() {
    let (bluez, state, server, worker) = fixture().await;
    let stopped = {
        let mut state = state.lock().unwrap();
        state.hold_stop = true;
        state.stopped.clone()
    };
    let resolving = tokio::spawn({
        let bluez = bluez.clone();
        async move {
            bluez
                .resolve_address("AA:BB:CC:DD:EE:FF", crate::boundary::AddressType::Public)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), stopped.notified())
        .await
        .unwrap();
    resolving.abort();
    assert!(resolving.await.unwrap_err().is_cancelled());
    server
        .send(
            state
                .lock()
                .unwrap()
                .stop_reply
                .take()
                .unwrap()
                .method_return(),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), bluez.finish_discovery())
        .await
        .unwrap()
        .unwrap();
    bluez.finish_discovery().await.unwrap();
    let state = state.lock().unwrap();
    assert_eq!(
        (state.starts, state.stops, state.generic_connects),
        (1, 1, 0)
    );
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_address_resolution_without_discovery_never_stops_unowned_session() {
    let (bluez, state, _server, worker) = fixture().await;
    state.lock().unwrap().discovered = true;
    bluez
        .resolve_address("AA:BB:CC:DD:EE:FF", crate::boundary::AddressType::Public)
        .await
        .unwrap();
    bluez.finish_discovery().await.unwrap();
    let state = state.lock().unwrap();
    assert_eq!(
        (state.starts, state.stops, state.generic_connects),
        (0, 0, 0)
    );
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_address_resolution_refused_start_has_no_stop_debt() {
    let (bluez, state, _server, worker) = fixture().await;
    state.lock().unwrap().refuse_start = true;
    let error = bluez
        .resolve_address("AA:BB:CC:DD:EE:FF", crate::boundary::AddressType::Public)
        .await
        .unwrap_err();
    assert!(error.detail().unwrap().contains("adapter not ready"));
    bluez.finish_discovery().await.unwrap();
    let state = state.lock().unwrap();
    assert_eq!((state.starts, state.stops), (1, 0));
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_address_resolution_indeterminate_stop_remains_owned_without_resend() {
    let (bluez, state, _server, worker) = fixture().await;
    state.lock().unwrap().timeout_stop = true;
    let error = bluez
        .resolve_address("AA:BB:CC:DD:EE:FF", crate::boundary::AddressType::Public)
        .await
        .unwrap_err();
    assert!(error.detail().unwrap().contains("stop outcome unknown"));
    for _ in 0..2 {
        assert!(
            bluez
                .finish_discovery()
                .await
                .unwrap_err()
                .detail()
                .unwrap()
                .contains("stop outcome unknown")
        );
    }
    let state = state.lock().unwrap();
    assert_eq!((state.starts, state.stops), (1, 1));
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_address_resolution_rejects_old_owner_result_but_releases_its_discovery() {
    let (bluez, state, server, worker) = fixture().await;
    let found = {
        let mut state = state.lock().unwrap();
        state.hold_found = true;
        state.found.clone()
    };
    let resolving = tokio::spawn({
        let bluez = bluez.clone();
        async move {
            bluez
                .resolve_address("AA:BB:CC:DD:EE:FF", crate::boundary::AddressType::Public)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), found.notified())
        .await
        .unwrap();
    server.release_name("org.bluez").await.unwrap();
    let (resource, replacement) = dbus_tokio::connection::new_session_sync().unwrap();
    let replacement_worker = tokio::spawn(resource);
    replacement
        .request_name("org.bluez", false, false, false)
        .await
        .unwrap();
    server
        .send(
            state
                .lock()
                .unwrap()
                .found_reply
                .take()
                .unwrap()
                .method_return()
                .append1(PropMap::new()),
        )
        .unwrap();
    let error = resolving.await.unwrap().unwrap_err();
    assert!(error.detail().unwrap().contains("owner changed"));
    bluez.finish_discovery().await.unwrap();
    assert_eq!(state.lock().unwrap().stops, 1);
    worker.abort();
    replacement_worker.abort();
}
