//! Real D-Bus dispatch regression, run only by the Linux private-bus CI step.
#![cfg(target_os = "linux")]
use bluez_async::{BluetoothEvent, BluetoothSession, DeviceEvent};
use dbus::arg::{RefArg, Variant};
use dbus::channel::Sender;
use dbus::nonblock::{Proxy, SyncConnection};
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

async fn server_matches(connection: &SyncConnection) -> u64 {
    let proxy = Proxy::new(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        Duration::from_secs(2),
        connection,
    );
    let (stats,): (HashMap<String, Variant<Box<dyn RefArg>>>,) = proxy
        .method_call("org.freedesktop.DBus.Debug.Stats", "GetStats", ())
        .await
        .unwrap();
    stats["MatchRules"].0.as_u64().unwrap()
}

fn emit(connection: &SyncConnection, rssi: i16) {
    let message = dbus::Message::new_signal(
        "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
    )
    .unwrap()
    .append3(
        "org.bluez.Device1",
        HashMap::from([("RSSI", Variant(rssi))]),
        Vec::<String>::new(),
    );
    connection.send(message).unwrap();
}

#[tokio::test]
#[ignore = "run on a fresh private bus with dbus-run-session; never the system bus"]
async fn private_bus_stream_drop_keeps_server_rule_and_other_scope_ownership_consistent() {
    assert_eq!(
        std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
        Ok("1"),
        "run through the dedicated dbus-run-session CI command, not an existing user bus"
    );
    let panics = Arc::new(AtomicUsize::new(0));
    let previous = std::panic::take_hook();
    let observed = panics.clone();
    std::panic::set_hook(Box::new(move |info| {
        observed.fetch_add(1, Ordering::SeqCst);
        eprintln!("private D-Bus regression observed panic: {info}");
    }));
    let (resource, publisher) = dbus_tokio::connection::new_session_sync().unwrap();
    let publisher_worker = tokio::spawn(resource);
    publisher
        .request_name("org.bluez", false, false, false)
        .await
        .unwrap();
    let baseline = server_matches(&publisher).await;
    let (_, session) = BluetoothSession::new_session_bus().await.unwrap();
    let first = session.scoped_match_cleanup();
    let second = session.scoped_match_cleanup();
    let mut survivor = Box::pin(second.event_stream().await.unwrap());
    assert_eq!(server_matches(&publisher).await, baseline + 2);
    for index in 0..50 {
        let transient = first.event_stream().await.unwrap();
        assert_eq!(
            server_matches(&publisher).await,
            baseline + 2,
            "identical server rules are leased, not installed twice"
        );
        emit(&publisher, index);
        drop(transient);
        first.drain_match_cleanup().await.unwrap();
        let delivered = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = survivor.next().await {
                if matches!(event, BluetoothEvent::Device { event: DeviceEvent::Rssi { rssi }, .. } if rssi == index) { return true }
            }
            false
        }).await.unwrap();
        assert!(
            delivered,
            "another adapter scope still receives positive events"
        );
    }
    drop(survivor);
    second.drain_match_cleanup().await.unwrap();
    assert_eq!(
        server_matches(&publisher).await,
        baseline,
        "the final server rules were actually removed"
    );
    assert_eq!(panics.load(Ordering::SeqCst), 0);
    std::panic::set_hook(previous);
    publisher_worker.abort();
}
