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
    availability_missing: bool,
    availability_version: Option<u32>,
    hold_availability: bool,
    availability_reply: Option<dbus::Message>,
    availability_read: Arc<tokio::sync::Notify>,
    discovery_starters: Vec<String>,
    discovery_stoppers: Vec<String>,
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
                Some("GetLeAvailability") if state.availability_missing => {
                    error("org.freedesktop.DBus.Error.UnknownMethod", "old daemon")
                }
                Some("GetLeAvailability") if state.hold_availability => {
                    state.availability_read.notify_one();
                    state.availability_reply = Some(message);
                    return true;
                }
                Some("GetLeAvailability") => message
                    .method_return()
                    .append2(state.availability_version.unwrap_or(1), 40u64),
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
                Some("SetDiscoveryFilter") => {
                    let filter: PropMap = message.read1().unwrap();
                    assert_eq!(filter.get("Transport").unwrap().0.as_str(), Some("le"));
                    message.method_return()
                }
                Some("ConnectDevice") => {
                    state.generic_connects += 1;
                    error(
                        "org.freedesktop.DBus.Error.UnknownMethod",
                        "unsafe connect forbidden",
                    )
                }
                Some("StartDiscovery") => {
                    state.starts += 1;
                    state
                        .discovery_starters
                        .push(message.sender().unwrap().to_string());
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
                    state
                        .discovery_stoppers
                        .push(message.sender().unwrap().to_string());
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
async fn private_bus_le_availability_never_accepts_cached_or_stale_or_other_peer() {
    let (bluez, state, server, worker) = fixture().await;
    let started = state.lock().unwrap().started.clone();
    let waiting = tokio::spawn({
        let bluez = bluez.clone();
        async move { bluez.wait_le_available("hci0/dev_AA_BB_CC_DD_EE_FF").await }
    });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    let signal = |peer: &str, sequence| {
        dbus::Message::new_signal(
            "/org/bluez/hci0",
            "org.unifiedblemanager.LinuxAuthority1",
            "LeAdvertisement",
        )
        .unwrap()
        .append2(dbus::Path::new(peer).unwrap(), sequence)
    };
    server
        .send(signal("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF", 40u64))
        .unwrap();
    server
        .send(signal("/org/bluez/hci0/dev_11_22_33_44_55_66", 41u64))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished());
    server
        .send(signal("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF", 42u64))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(state.lock().unwrap().stops, 1);
    assert_eq!(state.lock().unwrap().generic_connects, 0);
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_old_daemon_refuses_le_availability_before_scanning() {
    let (bluez, state, _server, worker) = fixture().await;
    state.lock().unwrap().availability_missing = true;
    let error = bluez
        .wait_le_available("hci0/dev_AA_BB_CC_DD_EE_FF")
        .await
        .unwrap_err();
    assert_eq!(error.code_str(), "capability.unsupported");
    assert_eq!(state.lock().unwrap().starts, 0);
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_le_availability_cancellation_settles_original_start() {
    let (bluez, state, server, worker) = fixture().await;
    let started = {
        let mut state = state.lock().unwrap();
        state.hold_start = true;
        state.started.clone()
    };
    let waiting = tokio::spawn({
        let bluez = bluez.clone();
        async move { bluez.wait_le_available("hci0/dev_AA_BB_CC_DD_EE_FF").await }
    });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    let finishing = tokio::spawn({
        let bluez = bluez.clone();
        async move {
            bluez
                .finish_availability("hci0/dev_AA_BB_CC_DD_EE_FF")
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!finishing.is_finished());
    let reply = state
        .lock()
        .unwrap()
        .start_reply
        .take()
        .unwrap()
        .method_return();
    server.send(reply).unwrap();
    tokio::time::timeout(Duration::from_secs(3), finishing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(state.lock().unwrap().stops, 1);
    assert_eq!(state.lock().unwrap().generic_connects, 0);
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_le_availability_other_peer_does_not_wait_for_absent_peer() {
    let (bluez, state, server, worker) = fixture().await;
    let started = state.lock().unwrap().started.clone();
    let absent = tokio::spawn({
        let bluez = bluez.clone();
        async move { bluez.wait_le_available("hci0/dev_AA_BB_CC_DD_EE_FF").await }
    });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    let available = tokio::spawn({
        let bluez = bluez.clone();
        async move { bluez.wait_le_available("hci0/dev_11_22_33_44_55_66").await }
    });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    server
        .send(
            dbus::Message::new_signal(
                "/org/bluez/hci0",
                "org.unifiedblemanager.LinuxAuthority1",
                "LeAdvertisement",
            )
            .unwrap()
            .append2(
                dbus::Path::new("/org/bluez/hci0/dev_11_22_33_44_55_66").unwrap(),
                41u64,
            ),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), available)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!absent.is_finished());
    assert_eq!(state.lock().unwrap().stops, 1);
    absent.abort();
    assert!(absent.await.unwrap_err().is_cancelled());
    bluez
        .finish_availability("hci0/dev_AA_BB_CC_DD_EE_FF")
        .await
        .unwrap();
    assert_eq!(state.lock().unwrap().stops, 2);
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_le_availability_cleanup_refusal_remains_owned() {
    let (bluez, state, server, worker) = fixture().await;
    let started = {
        let mut state = state.lock().unwrap();
        state.refuse_stop = true;
        state.started.clone()
    };
    let waiting = tokio::spawn({
        let bluez = bluez.clone();
        async move { bluez.wait_le_available("hci0/dev_AA_BB_CC_DD_EE_FF").await }
    });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    server
        .send(
            dbus::Message::new_signal(
                "/org/bluez/hci0",
                "org.unifiedblemanager.LinuxAuthority1",
                "LeAdvertisement",
            )
            .unwrap()
            .append2(
                dbus::Path::new("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").unwrap(),
                41u64,
            ),
        )
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.platform().unwrap().code, "org.bluez.Error.Failed");
    bluez
        .finish_availability("hci0/dev_AA_BB_CC_DD_EE_FF")
        .await
        .unwrap();
    assert!(state.lock().unwrap().stops >= 2);
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_le_availability_budget_ends_without_connect_or_unowned_stop() {
    let (bluez, state, _server, worker) = fixture().await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(40),
            bluez.wait_le_available("hci0/dev_AA_BB_CC_DD_EE_FF")
        )
        .await
        .is_err()
    );
    bluez
        .finish_availability("hci0/dev_AA_BB_CC_DD_EE_FF")
        .await
        .unwrap();
    // The caller's real budget may expire before dispatch under load. In
    // that case there is no accepted discovery effect to compensate.
    let observed = state.lock().unwrap();
    assert!(observed.starts <= 1);
    assert_eq!(observed.stops, observed.starts);
    drop(observed);
    assert_eq!(state.lock().unwrap().generic_connects, 0);
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_le_availability_malformed_signal_preserves_primary_and_cleanup_failures() {
    let (bluez, state, server, worker) = fixture().await;
    let started = {
        let mut state = state.lock().unwrap();
        state.refuse_stop = true;
        state.started.clone()
    };
    let waiting = tokio::spawn({
        let bluez = bluez.clone();
        async move { bluez.wait_le_available("hci0/dev_AA_BB_CC_DD_EE_FF").await }
    });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    server
        .send(
            dbus::Message::new_signal(
                "/org/bluez/hci0",
                "org.unifiedblemanager.LinuxAuthority1",
                "LeAdvertisement",
            )
            .unwrap()
            .append1("not-the-versioned-native-body"),
        )
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code_str(), "platform.failure");
    assert!(
        error
            .platform()
            .unwrap()
            .metadata
            .contains_key("cleanupDetail")
    );
    bluez
        .finish_availability("hci0/dev_AA_BB_CC_DD_EE_FF")
        .await
        .unwrap();
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_le_availability_daemon_replacement_does_not_retarget_wait() {
    let (bluez, state, server, worker) = fixture().await;
    let started = state.lock().unwrap().started.clone();
    let waiting = tokio::spawn({
        let bluez = bluez.clone();
        async move { bluez.wait_le_available("hci0/dev_AA_BB_CC_DD_EE_FF").await }
    });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    server.release_name("org.bluez").await.unwrap();
    let replacement = zbus::Connection::session().await.unwrap();
    replacement.request_name("org.bluez").await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code_str(), "capability.unsupported");
    assert_eq!(state.lock().unwrap().stops, 1);
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_le_availability_event_queued_before_baseline_is_not_fresh() {
    let (bluez, state, server, worker) = fixture().await;
    let (baseline_read, started) = {
        let mut state = state.lock().unwrap();
        state.hold_availability = true;
        (state.availability_read.clone(), state.started.clone())
    };
    let waiting = tokio::spawn({
        let bluez = bluez.clone();
        async move { bluez.wait_le_available("hci0/dev_AA_BB_CC_DD_EE_FF").await }
    });
    tokio::time::timeout(Duration::from_secs(3), baseline_read.notified())
        .await
        .unwrap();
    let signal = |sequence| {
        dbus::Message::new_signal(
            "/org/bluez/hci0",
            "org.unifiedblemanager.LinuxAuthority1",
            "LeAdvertisement",
        )
        .unwrap()
        .append2(
            dbus::Path::new("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").unwrap(),
            sequence,
        )
    };
    server.send(signal(41u64)).unwrap();
    let reply = state
        .lock()
        .unwrap()
        .availability_reply
        .take()
        .unwrap()
        .method_return()
        .append2(1u32, 41u64);
    server.send(reply).unwrap();
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!waiting.is_finished());
    server.send(signal(42u64)).unwrap();
    tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_le_availability_unknown_revision_refuses_before_effect() {
    let (bluez, state, _server, worker) = fixture().await;
    state.lock().unwrap().availability_version = Some(2);
    let error = bluez
        .wait_le_available("hci0/dev_AA_BB_CC_DD_EE_FF")
        .await
        .unwrap_err();
    assert_eq!(error.code_str(), "capability.unsupported");
    assert_eq!(state.lock().unwrap().starts, 0);
    assert_eq!(state.lock().unwrap().stops, 0);
    worker.abort();
}

#[tokio::test]
#[ignore = "requires a dedicated dbus-run-session"]
async fn private_bus_le_availability_releases_only_its_sender_not_public_scan() {
    let (bluez, state, server, worker) = fixture().await;
    let owner = server.unique_name().to_string();
    let public_scan = zbus::Connection::session().await.unwrap();
    public_scan
        .call_method(
            Some(owner.as_str()),
            "/org/bluez/hci0",
            Some("org.bluez.Adapter1"),
            "StartDiscovery",
            &(),
        )
        .await
        .unwrap();
    let started = state.lock().unwrap().started.clone();
    started.notified().await;
    let waiting = tokio::spawn({
        let bluez = bluez.clone();
        async move { bluez.wait_le_available("hci0/dev_AA_BB_CC_DD_EE_FF").await }
    });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    server
        .send(
            dbus::Message::new_signal(
                "/org/bluez/hci0",
                "org.unifiedblemanager.LinuxAuthority1",
                "LeAdvertisement",
            )
            .unwrap()
            .append2(
                dbus::Path::new("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").unwrap(),
                41u64,
            ),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    {
        let state = state.lock().unwrap();
        assert_ne!(state.discovery_starters[0], state.discovery_starters[1]);
        assert_eq!(
            state.discovery_stoppers,
            vec![state.discovery_starters[1].clone()]
        );
    }
    public_scan
        .call_method(
            Some(owner.as_str()),
            "/org/bluez/hci0",
            Some("org.bluez.Adapter1"),
            "StopDiscovery",
            &(),
        )
        .await
        .unwrap();
    worker.abort();
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
