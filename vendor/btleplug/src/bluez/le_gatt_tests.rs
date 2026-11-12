use super::*;
use crate::api::Peripheral as _;
use dbus::arg::{PropMap, Variant};
use dbus::channel::{MatchingReceiver, Sender};
use dbus::message::MatchRule;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

const DEVICE: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
const SERVICE: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010";
const CHAR: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010/char0020";
const DESC: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010/char0020/desc0030";
const CHAR_NEXT: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010/char0040";

#[test]
#[ignore = "private session bus only; no radio"]
fn ordinary_writes_remain_owned_after_waiter_cancellation() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        peripheral.discover_services().await.unwrap();
        let characteristic = peripheral.characteristics().into_iter().next().unwrap();
        let descriptor = characteristic.descriptors.iter().next().unwrap().clone();
        for descriptor_write in [false, true] {
            let entered = fixture.hold_write();
            let peer = peripheral.clone();
            let characteristic = characteristic.clone();
            let descriptor = descriptor.clone();
            let waiter = tokio::spawn(async move {
                if descriptor_write { peer.write_descriptor(&descriptor, &[9]).await }
                else { peer.write(&characteristic, &[9], WriteType::WithResponse).await }
            });
            tokio::time::timeout(Duration::from_secs(5), entered.notified()).await.unwrap();
            assert_eq!(peripheral.peer.marker_reclaim_snapshot().0, 1);
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
            assert_eq!(peripheral.peer.marker_reclaim_snapshot().0, 1);
            fixture.finish_write(None);
            tokio::time::timeout(Duration::from_secs(5), async {
                while peripheral.peer.marker_reclaim_snapshot().0 != 0 { tokio::task::yield_now().await; }
            }).await.unwrap();
            assert!(!peripheral.peer.marker_reclaim_snapshot().1);
        }
    });
}

#[derive(Default)]
struct State {
    snapshots: usize,
    graph_reads: usize,
    revision: u64,
    failed: bool,
    missing: bool,
    invalidate_at_descriptor: bool,
    candidate_uuid: bool,
    discovering_left: usize,
    fail_at_snapshot: Option<usize>,
    hold_snapshot: bool,
    hold_snapshot_at: Option<usize>,
    held_snapshot_entered: Arc<tokio::sync::Notify>,
    pending_snapshot: Option<dbus::Message>,
    snapshot_entered: Arc<tokio::sync::Notify>,
    version: u32,
    attachment: u64,
    bearer: Option<String>,
    stage: Option<String>,
    extra_field: bool,
    wrong_type: bool,
    missing_field: bool,
    next_characteristic: bool,
    stopped_notify_paths: Vec<String>,
    started_notify: Vec<(String, String)>,
    retired_notify_senders: Vec<String>,
    hold_notify: bool,
    pending_notify: Option<dbus::Message>,
    notify_entered: Arc<tokio::sync::Notify>,
    write_entered: Arc<tokio::sync::Notify>,
    hold_write: bool,
    pending_write: Option<dbus::Message>,
    writes: Vec<(String, Vec<u8>)>,
    disconnected_left: usize,
    snapshot_sequence: std::collections::VecDeque<(u64, bool)>,
    disconnect_at_snapshot: Option<usize>,
    next_attachment: Option<u64>,
    le_connected: bool,
    le_connects: usize,
}

pub(in crate::bluez) struct Fixture {
    server: Arc<dbus::blocking::SyncConnection>,
    replies: std::sync::mpsc::Sender<dbus::Message>,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

#[test]
#[ignore = "private session bus only; no radio"]
fn write_is_sent_before_reply_future_is_polled() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let fixture = Fixture::new();
            let peripheral = fixture.peripheral().await;
            peripheral.discover_services().await.unwrap();
            let characteristic = peripheral.characteristics().into_iter().next().unwrap();
            let (_, _, daemon, id) = peripheral
                .peer
                .activation_snapshot(&characteristic)
                .unwrap();
            let entered = fixture.state.lock().unwrap().write_entered.clone();
            let reply = peripheral.session.start_characteristic_write(
                &daemon,
                &id,
                vec![1, 2],
                WriteOptions::default(),
            );
            tokio::time::timeout(Duration::from_secs(5), entered.notified())
                .await
                .unwrap();
            assert_eq!(
                fixture.state.lock().unwrap().writes,
                vec![(CHAR.to_owned(), vec![1, 2])]
            );
            reply.await.unwrap();
        });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn source_marker_precedes_synchronous_write_dispatch() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use crate::common::util::{
                SourceGroupError, SourceUnresolvedWriteRequest, SourceWriteRequest,
                SourceWriteStatus,
            };
            use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute, Poll};
            let fixture = Fixture::new();
            let peripheral = fixture.peripheral().await;
            peripheral.discover_services().await.unwrap();
            let characteristic = peripheral.characteristics().into_iter().next().unwrap();
            let token = peripheral.accepted_le_gatt_ready_token().unwrap().unwrap();
            let expected = (token.daemon_owner, token.attachment, token.revision);
            peripheral
                .start_notification_source_generation(1)
                .await
                .unwrap();
            let (group, consumer) = peripheral
                .open_notification_source_group(1, 8, 128)
                .unwrap();
            let graph = crate::common::util::marker_graph_identity(&expected);
            let request = SourceWriteRequest {
                selector: Default::default(),
                id: 0,
                graph,
                value: vec![3],
                write_type: WriteType::WithResponse,
                route: NotificationRoute {
                    service_uuid: characteristic.service_uuid,
                    service_instance: characteristic.service_instance,
                    characteristic_uuid: characteristic.uuid,
                    characteristic_instance: characteristic.instance,
                },
            };
            let unresolved = SourceUnresolvedWriteRequest {
                id: request.id,
                graph: request.graph.clone(),
                selector: request.selector.clone(),
                value: request.value.clone(),
                write_type: request.write_type,
            };
            let entered = fixture.state.lock().unwrap().write_entered.clone();
            let receipt = peripheral
                .start_ordered_marker_write(&group, &expected, &characteristic, request.clone())
                .await
                .unwrap();
            assert!(matches!(
                consumer.poll(),
                Poll::Record(IngressRecord::Marker { id: 0, ordinal: 0 })
            ));
            tokio::time::timeout(Duration::from_secs(5), entered.notified())
                .await
                .unwrap();
            let replay = peripheral
                .start_ordered_marker_write(&group, &expected, &characteristic, request.clone())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if matches!(receipt.status(), SourceWriteStatus::Settled { .. }) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(receipt.status(), replay.status());
            assert!(matches!(
                group
                    .cancel_unresolved_marker_write(&unresolved)
                    .unwrap()
                    .status(),
                SourceWriteStatus::Settled { .. }
            ));
            assert_eq!(fixture.state.lock().unwrap().writes.len(), 1);
            group.forget_marker_write(0).unwrap();
            let mut next = request;
            next.id = 1;
            let cancelled = SourceUnresolvedWriteRequest {
                id: next.id,
                graph: next.graph.clone(),
                selector: next.selector.clone(),
                value: next.value.clone(),
                write_type: next.write_type,
            };
            assert_eq!(
                group
                    .cancel_unresolved_marker_write(&cancelled)
                    .unwrap()
                    .status(),
                SourceWriteStatus::NotAdmitted(SourceGroupError::Cancelled)
            );
            let replay = peripheral
                .start_ordered_marker_write(&group, &expected, &characteristic, next)
                .await
                .unwrap();
            assert_eq!(
                replay.status(),
                SourceWriteStatus::NotAdmitted(SourceGroupError::Cancelled)
            );
            assert_eq!(fixture.state.lock().unwrap().writes.len(), 1);
        });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn marker_write_reply_retains_reclaim_owner_after_caller_cancellation() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        use crate::common::util::{SourceUnresolvedWriteRequest, SourceWriteRequest, SourceWriteStatus};
        use crate::ubm_peer_notification_ingress::NotificationRoute;
        use crate::ubm_reclaim_authority::GraphKey;
        use crate::ubm_reclaim_graph_gate::GateError;
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        peripheral.discover_services().await.unwrap();
        let characteristic = peripheral.characteristics().into_iter().next().unwrap();
        let token = peripheral.accepted_le_gatt_ready_token().unwrap().unwrap();
        let expected = (token.daemon_owner, token.attachment, token.revision);
        let binding = peripheral.capture_reclaim_graph_binding("peer", "connection", &expected).unwrap();
        let key = GraphKey {
            authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(),
            graph: expected.clone(), permit_id: 1,
        };
        peripheral.start_notification_source_generation(1).await.unwrap();
        let (group, _) = peripheral.open_notification_source_group(1, 8, 128).unwrap();
        let request = SourceWriteRequest {
            selector: Default::default(), id: 0,
            graph: crate::common::util::marker_graph_identity(&expected), value: vec![3],
            write_type: WriteType::WithResponse,
            route: NotificationRoute {
                service_uuid: characteristic.service_uuid,
                service_instance: characteristic.service_instance,
                characteristic_uuid: characteristic.uuid,
                characteristic_instance: characteristic.instance,
            },
        };
        let unresolved = SourceUnresolvedWriteRequest {
            id: request.id, graph: request.graph.clone(), selector: request.selector.clone(),
            value: request.value.clone(), write_type: request.write_type,
        };
        let entered = fixture.hold_write();
        let receipt = peripheral.start_ordered_marker_write(&group, &expected, &characteristic, request.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), entered.notified()).await.unwrap();
        assert_eq!(receipt.status(), SourceWriteStatus::Started { ordinal: 0 });
        assert_eq!(peripheral.peer.marker_reclaim_snapshot().0, 1);
        assert_eq!(peripheral.peer.acquire_reclaim_gate(&binding, key.clone()), Err(GateError::Busy));
        assert_eq!(group.cancel_unresolved_marker_write(&unresolved).unwrap().status(), SourceWriteStatus::Started { ordinal: 0 });
        drop(receipt);
        assert_eq!(peripheral.peer.marker_reclaim_snapshot().0, 1);
        assert_eq!(peripheral.peer.acquire_reclaim_gate(&binding, key.clone()), Err(GateError::Busy));
        fixture.finish_write(None);
        tokio::time::timeout(Duration::from_secs(5), async {
            while peripheral.peer.marker_reclaim_snapshot().0 != 0 { tokio::task::yield_now().await; }
        }).await.unwrap();
        assert!(!peripheral.peer.marker_reclaim_snapshot().1);
        group.forget_marker_write(0).unwrap();
        let mut failed = request;
        failed.id = 1;
        let receipt = peripheral.start_ordered_marker_write(&group, &expected, &characteristic, failed).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), entered.notified()).await.unwrap();
        assert_eq!(receipt.status(), SourceWriteStatus::Started { ordinal: 1 });
        assert_eq!(peripheral.peer.marker_reclaim_snapshot().0, 1);
        assert_eq!(peripheral.peer.acquire_reclaim_gate(&binding, key), Err(GateError::Busy));
        fixture.finish_write(Some("org.bluez.Error.Failed"));
        tokio::time::timeout(Duration::from_secs(5), async {
            while peripheral.peer.marker_reclaim_snapshot().0 != 0 { tokio::task::yield_now().await; }
        }).await.unwrap();
        assert!(peripheral.peer.marker_reclaim_snapshot().1);
        assert!(matches!(receipt.status(), SourceWriteStatus::Settled { ordinal: 1, result: Err(_) }));
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn marker_runtime_shutdown_settles_receipt_and_releases_operation() {
    use crate::common::util::{SourceWriteRequest, SourceWriteStatus};
    use crate::ubm_peer_notification_ingress::NotificationRoute;
    for poll_reply in [false, true] {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (fixture, peripheral, group, request, receipt) = runtime.block_on(async {
            let fixture = Fixture::new();
            let peripheral = fixture.peripheral().await;
            peripheral.discover_services().await.unwrap();
            let characteristic = peripheral.characteristics().into_iter().next().unwrap();
            let token = peripheral.accepted_le_gatt_ready_token().unwrap().unwrap();
            let expected = (token.daemon_owner, token.attachment, token.revision);
            peripheral.start_notification_source_generation(1).await.unwrap();
            let (group, _) = peripheral.open_notification_source_group(1, 8, 128).unwrap();
            let request = SourceWriteRequest {
                selector: Default::default(), id: 0,
                graph: crate::common::util::marker_graph_identity(&expected), value: vec![3],
                write_type: WriteType::WithResponse,
                route: NotificationRoute {
                    service_uuid: characteristic.service_uuid, service_instance: characteristic.service_instance,
                    characteristic_uuid: characteristic.uuid, characteristic_instance: characteristic.instance,
                },
            };
            let entered = fixture.hold_write();
            let receipt = peripheral.start_ordered_marker_write(&group, &expected, &characteristic, request.clone()).await.unwrap();
            if poll_reply {
                tokio::time::timeout(Duration::from_secs(5), entered.notified()).await.unwrap();
                tokio::task::yield_now().await;
            }
            assert_eq!(receipt.status(), SourceWriteStatus::Started { ordinal: 0 });
            assert_eq!(peripheral.peer.marker_reclaim_snapshot().0, 1);
            (fixture, peripheral, group, request, receipt)
        });
        drop(runtime);
        assert_eq!(peripheral.peer.marker_reclaim_snapshot(), (0, true));
        assert!(matches!(receipt.status(), SourceWriteStatus::Settled {
            ordinal: 0, result: Err(error),
        } if error.code == "write-reply-abandoned"));
        assert_eq!(group.replay_marker_write(&request).unwrap().unwrap().status(), receipt.status());
        group.forget_marker_write(0).unwrap();
        drop(fixture);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

impl Fixture {
    pub(in crate::bluez) fn new() -> Self {
        assert_eq!(
            std::env::var("UBM_BLUEZ_PRIVATE_BUS_TEST").as_deref(),
            Ok("1")
        );
        let server = Arc::new(dbus::blocking::SyncConnection::new_session().unwrap());
        server
            .request_name("org.bluez", true, false, false)
            .unwrap();
        let state = Arc::new(Mutex::new(State {
            version: 1,
            attachment: 7,
            revision: 1,
            ..State::default()
        }));
        server
            .add_match_no_cb(
                "type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged'",
            )
            .unwrap();
        let observed_signals = state.clone();
        server.start_receive(
            MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged"),
            Box::new(move |message, _| {
                if let Ok((name, old_owner, new_owner)) = message.read3::<String, String, String>()
                {
                    if name == old_owner && !old_owner.is_empty() && new_owner.is_empty() {
                        let mut state = observed_signals.lock().unwrap();
                        if state
                            .started_notify
                            .iter()
                            .any(|(_, sender)| sender == &name)
                        {
                            state.retired_notify_senders.push(name);
                        }
                    }
                }
                true
            }),
        );
        let observed = state.clone();
        server.start_receive(
            MatchRule::new_method_call(),
            Box::new(move |message, connection| {
                let mut state = observed.lock().unwrap();
                let object = message.path().unwrap().to_string();
                let reply = match (message.interface().as_deref(), message.member().as_deref()) {
                    (Some("org.freedesktop.DBus.ObjectManager"), Some("GetManagedObjects")) => {
                        let mut properties: PropMap = HashMap::new();
                        properties.insert(
                            "Address".into(),
                            Variant(Box::new("AA:BB:CC:DD:EE:FF".to_owned())),
                        );
                        properties
                            .insert("AddressType".into(), Variant(Box::new("public".to_owned())));
                        for name in ["Paired", "Trusted", "Blocked", "LegacyPairing"] {
                            properties.insert(name.into(), Variant(Box::new(false)));
                        }
                        for name in ["Connected", "ServicesResolved"] {
                            properties.insert(name.into(), Variant(Box::new(true)));
                        }
                        let interfaces =
                            HashMap::from([("org.bluez.Device1".to_owned(), properties)]);
                        message.method_return().append1(HashMap::from([(
                            dbus::Path::new(DEVICE).unwrap(),
                            interfaces,
                        )]))
                    }
                    (Some("org.unifiedblemanager.LEGatt1"), Some("GetSnapshot")) => {
                        state.snapshots += 1;
                        state.snapshot_entered.notify_one();
                        if state.hold_snapshot || state.hold_snapshot_at == Some(state.snapshots) {
                            state.held_snapshot_entered.notify_one();
                            assert!(state.pending_snapshot.is_none());
                            state.pending_snapshot = Some(message);
                            return true;
                        }
                        if state.missing {
                            message.error(
                                &dbus::strings::ErrorName::new(
                                    "org.freedesktop.DBus.Error.UnknownMethod",
                                )
                                .unwrap(),
                                &std::ffi::CString::new("extension absent").unwrap(),
                            )
                        } else {
                            let failed = state.failed
                                || state
                                    .fail_at_snapshot
                                    .is_some_and(|at| state.snapshots >= at);
                            let discovering = state.discovering_left > 0;
                            let mut disconnected = state.disconnected_left > 0
                                || state
                                    .disconnect_at_snapshot
                                    .is_some_and(|at| state.snapshots >= at);
                            state.disconnected_left = state.disconnected_left.saturating_sub(1);
                            if !disconnected {
                                state.discovering_left = state.discovering_left.saturating_sub(1);
                                if let Some(attachment) = state.next_attachment.take() {
                                    state.attachment = attachment;
                                }
                            }
                            if let Some((attachment, retired)) = state.snapshot_sequence.pop_front()
                            {
                                state.attachment = attachment;
                                disconnected = retired;
                            }
                            let mut reply = message.method_return();
                            if state.wrong_type {
                                reply = reply.append1(u64::from(state.version));
                            } else {
                                reply = reply.append1(state.version);
                            }
                            reply = reply.append3(
                                state.attachment,
                                state.revision,
                                state.bearer.as_deref().unwrap_or(if disconnected {
                                    "none"
                                } else {
                                    "le"
                                }),
                            );
                            reply = reply.append3(
                                if disconnected {
                                    "disconnected"
                                } else if failed {
                                    "failed"
                                } else if discovering {
                                    "discovering"
                                } else {
                                    "ready"
                                },
                                state.stage.as_deref().unwrap_or(if disconnected {
                                    "transport"
                                } else if failed {
                                    "discovery"
                                } else {
                                    "none"
                                }),
                                if disconnected {
                                    107i32
                                } else if failed {
                                    5i32
                                } else {
                                    0i32
                                },
                            );
                            if !state.missing_field {
                                reply =
                                    reply.append1(if failed && !disconnected { 10u8 } else { 0u8 });
                            }
                            if state.extra_field {
                                reply = reply.append1(1u32);
                            }
                            reply
                        }
                    }
                    (Some("org.freedesktop.DBus.Introspectable"), Some("Introspect")) => {
                        state.graph_reads += 1;
                        let child = match object.as_str() {
                            DEVICE => "service0010",
                            SERVICE if state.next_characteristic => "char0040",
                            SERVICE => "char0020",
                            CHAR | CHAR_NEXT => "desc0030",
                            _ => panic!("unexpected graph path {object}"),
                        };
                        message
                            .method_return()
                            .append1(format!("<node><node name=\"{child}\"/></node>"))
                    }
                    (Some("org.freedesktop.DBus.Properties"), Some("Get")) => {
                        let (interface, property): (String, String) = message.read2().unwrap();
                        if interface == "org.bluez.Bearer.LE1" && property == "Connected" {
                            return connection
                                .send(message.method_return().append1(Variant(state.le_connected)))
                                .is_ok();
                        }
                        state.graph_reads += 1;
                        if object == DESC && state.invalidate_at_descriptor {
                            state.revision += 1;
                        }
                        if property == "Primary" {
                            message.method_return().append1(Variant(true))
                        } else {
                            message.method_return().append1(Variant(
                                if object.ends_with("/desc0030") {
                                    "00002902-0000-1000-8000-00805f9b34fb"
                                } else if state.candidate_uuid {
                                    "0000180f-0000-1000-8000-00805f9b34fb"
                                } else {
                                    "0000180d-0000-1000-8000-00805f9b34fb"
                                }
                                .to_owned(),
                            ))
                        }
                    }
                    (Some("org.freedesktop.DBus.Properties"), Some("GetAll"))
                        if object == DEVICE =>
                    {
                        let mut properties: PropMap = HashMap::new();
                        properties.insert(
                            "Address".into(),
                            Variant(Box::new("AA:BB:CC:DD:EE:FF".to_owned())),
                        );
                        properties
                            .insert("AddressType".into(), Variant(Box::new("public".to_owned())));
                        for name in ["Paired", "Trusted", "Blocked", "LegacyPairing"] {
                            properties.insert(name.into(), Variant(Box::new(false)));
                        }
                        for name in ["Connected", "ServicesResolved"] {
                            properties.insert(name.into(), Variant(Box::new(true)));
                        }
                        message.method_return().append1(properties)
                    }
                    (Some("org.freedesktop.DBus.Properties"), Some("GetAll")) => {
                        state.graph_reads += 1;
                        let mut properties: PropMap = HashMap::new();
                        properties.insert(
                            "UUID".into(),
                            Variant(Box::new("00002a37-0000-1000-8000-00805f9b34fb".to_owned())),
                        );
                        properties
                            .insert("Flags".into(), Variant(Box::new(vec!["notify".to_owned()])));
                        message.method_return().append1(properties)
                    }
                    (Some("org.freedesktop.DBus.Peer"), Some("Ping")) => message.method_return(),
                    (Some("org.bluez.GattCharacteristic1"), Some("StartNotify")) => {
                        state
                            .started_notify
                            .push((object, message.sender().unwrap().to_string()));
                        if state.hold_notify {
                            state.pending_notify = Some(message);
                            state.notify_entered.notify_one();
                            return true;
                        }
                        message.method_return()
                    }
                    (Some("org.bluez.GattCharacteristic1"), Some("StopNotify")) => {
                        state.stopped_notify_paths.push(object);
                        message.method_return()
                    }
                    (Some("org.bluez.GattCharacteristic1" | "org.bluez.GattDescriptor1"), Some("WriteValue")) => {
                        let (value, _options): (Vec<u8>, PropMap) = message.read2().unwrap();
                        state.writes.push((object, value));
                        state.write_entered.notify_one();
                        if state.hold_write {
                            state.pending_write = Some(message);
                            return true;
                        }
                        message.method_return()
                    }
                    (Some("org.bluez.Bearer.LE1"), Some("Connect")) => {
                        state.le_connected = true;
                        state.le_connects += 1;
                        message.method_return()
                    }
                    other => panic!("unexpected method {other:?} on {object}"),
                };
                connection.send(reply).is_ok()
            }),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let running = stop.clone();
        let bus = server.clone();
        let (replies, queued_replies) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            while !running.load(Ordering::SeqCst) {
                for reply in queued_replies.try_iter() {
                    bus.send(reply).unwrap();
                }
                bus.process(Duration::from_millis(10)).unwrap();
            }
        });
        Self {
            server,
            replies,
            state,
            stop,
            worker: Some(worker),
        }
    }

    pub(in crate::bluez) fn hold_notify(&self) -> Arc<tokio::sync::Notify> {
        let mut state = self.state.lock().unwrap();
        state.hold_notify = true;
        state.notify_entered.clone()
    }
    fn hold_write(&self) -> Arc<tokio::sync::Notify> {
        let mut state = self.state.lock().unwrap();
        state.hold_write = true;
        state.write_entered.clone()
    }
    fn finish_write(&self, error: Option<&str>) {
        let message = self.state.lock().unwrap().pending_write.take().unwrap();
        let reply = match error {
            Some(name) => message.error(
                &dbus::strings::ErrorName::new(name).unwrap(),
                &std::ffi::CString::new("write refused").unwrap(),
            ),
            None => message.method_return(),
        };
        self.replies.send(reply).unwrap();
    }
    pub(in crate::bluez) fn finish_notify(&self) {
        let message = self.state.lock().unwrap().pending_notify.take().unwrap();
        self.replies.send(message.method_return()).unwrap();
    }
    pub(in crate::bluez) fn refresh_same_path(&self) {
        self.state.lock().unwrap().revision += 1;
    }
    pub(in crate::bluez) fn retired_senders(&self) -> Vec<String> {
        self.state.lock().unwrap().retired_notify_senders.clone()
    }
    pub(in crate::bluez) fn notify_calls(&self) -> (Vec<(String, String)>, Vec<String>) {
        let state = self.state.lock().unwrap();
        (
            state.started_notify.clone(),
            state.stopped_notify_paths.clone(),
        )
    }
    pub(in crate::bluez) fn sender_present(&self, sender: &str) -> bool {
        // The fixture server is concurrently driven by its processing thread.
        // Use an independent connection so that thread cannot consume this reply.
        let observer = dbus::blocking::SyncConnection::new_session().unwrap();
        let (present,): (bool,) = observer
            .with_proxy(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                Duration::from_secs(1),
            )
            .method_call("org.freedesktop.DBus", "NameHasOwner", (sender,))
            .unwrap();
        present
    }

    pub(in crate::bluez) async fn peripheral(&self) -> Peripheral {
        let (_, session) = BluetoothSession::new_session_bus().await.unwrap();
        let device = session.get_devices().await.unwrap().remove(0);
        Peripheral::new(session, device)
            .with_le_owner(&self.server.unique_name())
            .await
            .unwrap()
    }
}

fn run(test: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test);
}

#[test]
fn shared_publication_rejects_old_bracket_after_new_clone_commit() {
    for (
        current_attachment,
        current_revision,
        candidate_owner,
        candidate_attachment,
        candidate_revision,
    ) in [
        (7, 2, ":1.5", 7, 1),
        (8, 1, ":1.5", 7, 9),
        (7, 2, ":1.6", 7, 3),
    ] {
        let publication = Arc::new(Mutex::new(PublishedGatt::default()));
        let held_clone = publication.clone();
        let current = LeGattReadyToken {
            daemon_owner: ":1.5".into(),
            attachment: current_attachment,
            revision: current_revision,
        };
        let candidate = LeGattReadyToken {
            daemon_owner: candidate_owner.into(),
            attachment: candidate_attachment,
            revision: candidate_revision,
        };
        publish_gatt(
            &publication,
            PublishedGatt {
                services: vec![],
                accepted_token: Some(current.clone()),
            },
        )
        .unwrap();
        let error = publish_gatt(
            &held_clone,
            PublishedGatt {
                services: vec![],
                accepted_token: Some(candidate),
            },
        )
        .expect_err("older completed bracket must not replace a later clone publication");
        assert!(matches!(error, Error::Platform(detail) if detail.code == "snapshot-changed"));
        assert_eq!(publication.lock().unwrap().accepted_token, Some(current));
    }
}

#[test]
fn shared_publication_keeps_attested_owner_and_accepts_forward_progress() {
    let publication = Mutex::new(PublishedGatt::default());
    publish_gatt(&publication, PublishedGatt::default()).unwrap();
    for (attachment, revision) in [(7, 1), (7, 2), (7, 2), (8, 1)] {
        let token = LeGattReadyToken {
            daemon_owner: ":1.5".into(),
            attachment,
            revision,
        };
        publish_gatt(
            &publication,
            PublishedGatt {
                services: vec![],
                accepted_token: Some(token.clone()),
            },
        )
        .unwrap();
        assert_eq!(publication.lock().unwrap().accepted_token, Some(token));
    }
    assert!(
        publish_gatt(&publication, PublishedGatt::default()).is_err(),
        "unattested clone cannot replace an attested owner publication"
    );
    assert_eq!(
        publication
            .lock()
            .unwrap()
            .accepted_token
            .as_ref()
            .unwrap()
            .attachment,
        8
    );
}

#[test]
fn public_ready_token_revalidates_constructed_snapshot_not_only_parsed_snapshot() {
    let valid = LeGattSnapshot {
        daemon_owner: ":1.5".into(),
        version: 1,
        attachment: 7,
        revision: 9,
        bearer: LeGattBearer::Le,
        status: LeGattStatus::Ready,
        error_stage: LeGattErrorStage::None,
        errno: 0,
        att_error: 0,
    };
    assert!(valid.ready_token().is_ok());
    for mutant in 0..9 {
        let mut snapshot = valid.clone();
        match mutant {
            0 => snapshot.version = 2,
            1 => snapshot.attachment = 0,
            2 => snapshot.revision = 0,
            3 => snapshot.bearer = LeGattBearer::Mixed,
            4 => snapshot.error_stage = LeGattErrorStage::Discovery,
            5 => snapshot.errno = 5,
            6 => snapshot.att_error = 10,
            7 => snapshot.daemon_owner = "org.bluez".into(),
            _ => snapshot.daemon_owner = ":1.5\n".into(),
        };
        assert!(
            snapshot.ready_token().is_err(),
            "public constructed snapshot mutant {mutant} minted token"
        );
    }
}

#[test]
#[ignore = "private session bus only; no radio"]
fn stale_completed_graph_cannot_overwrite_newer_clone_publication() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        peripheral.discover_services().await.unwrap();
        let older_candidate = peripheral.services.lock().unwrap().clone();
        let held_clone = peripheral.clone();
        {
            let mut state = fixture.state.lock().unwrap();
            state.next_characteristic = true;
            state.revision += 1;
        }
        peripheral.discover_services().await.unwrap();
        let newer_graph = peripheral.services();
        let newer_token = peripheral.accepted_le_gatt_ready_token().unwrap();
        assert_eq!(
            newer_graph
                .iter()
                .next()
                .unwrap()
                .characteristics
                .iter()
                .next()
                .unwrap()
                .instance,
            0x40
        );
        let error = publish_gatt(&held_clone.services, older_candidate).unwrap_err();
        assert!(matches!(error, Error::Platform(detail) if detail.code == "snapshot-changed"));
        assert_eq!(peripheral.services(), newer_graph);
        assert_eq!(
            held_clone.accepted_le_gatt_ready_token().unwrap(),
            newer_token
        );
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn retained_notification_cleanup_targets_original_characteristic_after_graph_replacement() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        peripheral.discover_services().await.unwrap();
        let characteristic = peripheral.characteristics().into_iter().next().unwrap();
        let retained = peripheral
            .notification_cleanup_peripheral(&characteristic)
            .unwrap();
        {
            let mut state = fixture.state.lock().unwrap();
            state.next_characteristic = true;
            state.revision += 1;
        }
        peripheral.discover_services().await.unwrap();
        assert_eq!(
            peripheral.characteristics().iter().next().unwrap().instance,
            0x40
        );
        retained.unsubscribe(&characteristic).await.unwrap();
        assert_eq!(
            fixture.state.lock().unwrap().stopped_notify_paths,
            vec![CHAR]
        );
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn failed_snapshot_refuses_publication_and_preserves_prior_graph() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        peripheral.discover_services().await.unwrap();
        let prior = peripheral.services();
        let prior_token = peripheral.accepted_le_gatt_ready_token().unwrap();
        assert_eq!(
            peripheral.clone().accepted_le_gatt_ready_token().unwrap(),
            prior_token
        );
        {
            let mut state = fixture.state.lock().unwrap();
            state.failed = true;
            state.candidate_uuid = true;
        }
        let error = peripheral.discover_services().await.unwrap_err();
        let Error::Platform(platform) = error else {
            panic!("lost typed native failure {error:?}")
        };
        assert_eq!(platform.domain, "bluez-le-gatt");
        assert_eq!(platform.code, "failed");
        for pair in [
            ("errno", "5"),
            ("attError", "10"),
            ("errorStage", "discovery"),
        ] {
            assert!(
                platform
                    .metadata
                    .iter()
                    .any(|(key, value)| (*key, value.as_str()) == pair)
            );
        }
        assert_eq!(peripheral.services(), prior);
        assert_eq!(
            peripheral.accepted_le_gatt_ready_token().unwrap(),
            prior_token
        );
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn missing_extension_refuses_before_any_graph_read() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        fixture.state.lock().unwrap().missing = true;
        assert!(
            peripheral.discover_services().await.is_err(),
            "missing extension must not fall back to cached objects or ServicesResolved"
        );
        assert_eq!(fixture.state.lock().unwrap().graph_reads, 0);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn revision_changed_during_full_descriptor_graph_refuses_publication() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        peripheral.discover_services().await.unwrap();
        let prior = peripheral.services();
        {
            let mut state = fixture.state.lock().unwrap();
            state.invalidate_at_descriptor = true;
            state.candidate_uuid = true;
        }
        assert!(
            peripheral.discover_services().await.is_err(),
            "same ready token must bracket full descriptor graph"
        );
        assert_eq!(peripheral.services(), prior);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn strict_snapshot_wire_and_ready_state_fail_closed() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        assert_eq!(peripheral.accepted_le_gatt_ready_token().unwrap(), None);
        for mutant in 0..8 {
            {
                let mut state = fixture.state.lock().unwrap();
                state.version = 1;
                state.attachment = 7;
                state.revision = 1;
                state.bearer = None;
                state.stage = None;
                state.extra_field = false;
                state.wrong_type = false;
                state.missing_field = false;
                match mutant {
                    0 => state.version = 2,
                    1 => state.attachment = 0,
                    2 => state.revision = 0,
                    3 => state.bearer = Some("mixed".into()),
                    4 => state.stage = Some("future".into()),
                    5 => state.extra_field = true,
                    6 => state.wrong_type = true,
                    _ => state.missing_field = true,
                }
            }
            assert!(
                peripheral.le_gatt_snapshot().await.is_err(),
                "wire mutant {mutant} accepted"
            );
        }
        assert_eq!(fixture.state.lock().unwrap().graph_reads, 0);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn discovering_polls_authoritative_snapshot_then_publishes_matching_token() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        fixture.state.lock().unwrap().discovering_left = 1;
        peripheral.discover_services().await.unwrap();
        assert_eq!(fixture.state.lock().unwrap().snapshots, 3);
        let token = peripheral.accepted_le_gatt_ready_token().unwrap().unwrap();
        assert_eq!((token.attachment, token.revision), (7, 1));
        let services = peripheral.services();
        assert_eq!(services.len(), 1);
        assert_eq!(
            services
                .first()
                .unwrap()
                .characteristics
                .first()
                .unwrap()
                .descriptors
                .len(),
            1
        );
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn accepted_le_connect_waits_for_new_att_attachment_after_retired_snapshot() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        {
            let mut state = fixture.state.lock().unwrap();
            state.attachment = 13;
            state.disconnected_left = 1;
            state.next_attachment = Some(14);
            state.discovering_left = 1;
        }
        peripheral
            .connect_le(&fixture.server.unique_name())
            .await
            .unwrap();
        peripheral
            .discover_services()
            .await
            .expect("accepted LE link precedes new primary ATT initialization");
        let token = peripheral.accepted_le_gatt_ready_token().unwrap().unwrap();
        assert_eq!(token.attachment, 14);
        let state = fixture.state.lock().unwrap();
        assert_eq!(state.le_connects, 1);
        assert_eq!(state.snapshots, 4);
        assert!(state.graph_reads > 0);
        assert_eq!(peripheral.characteristics().len(), 1);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn retired_snapshot_without_live_le_link_remains_actual_transport_failure() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        fixture.state.lock().unwrap().disconnected_left = 1;
        let error = peripheral.discover_services().await.unwrap_err();
        assert!(
            matches!(error, Error::Platform(ref detail) if detail.code == "disconnected" && detail.metadata.iter().any(|(key,value)| *key == "errno" && value == "107"))
        );
        assert_eq!(fixture.state.lock().unwrap().snapshots, 1);
        assert_eq!(fixture.state.lock().unwrap().graph_reads, 0);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn new_attachment_failure_is_not_retried_as_old_disconnect() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        {
            let mut state = fixture.state.lock().unwrap();
            state.disconnected_left = 1;
            state.next_attachment = Some(8);
            state.fail_at_snapshot = Some(2);
        }
        peripheral
            .connect_le(&fixture.server.unique_name())
            .await
            .unwrap();
        let error = peripheral.discover_services().await.unwrap_err();
        assert!(
            matches!(error, Error::Platform(ref detail) if detail.code == "failed" && detail.metadata.iter().any(|(key,value)| *key == "attError" && value == "10"))
        );
        assert_eq!(fixture.state.lock().unwrap().snapshots, 2);
        assert_eq!(fixture.state.lock().unwrap().graph_reads, 0);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn old_attachment_cannot_become_ready_after_disconnected_snapshot() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        fixture.state.lock().unwrap().disconnected_left = 1;
        peripheral
            .connect_le(&fixture.server.unique_name())
            .await
            .unwrap();
        assert!(peripheral.discover_services().await.is_err());
        assert_eq!(fixture.state.lock().unwrap().snapshots, 2);
        assert_eq!(fixture.state.lock().unwrap().graph_reads, 0);
        assert_eq!(peripheral.accepted_le_gatt_ready_token().unwrap(), None);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn current_attachment_disconnect_is_terminal_even_while_le_bearer_connected() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        {
            let mut state = fixture.state.lock().unwrap();
            state.disconnected_left = 1;
            state.next_attachment = Some(8);
            state.discovering_left = 1;
            state.disconnect_at_snapshot = Some(3);
        }
        peripheral
            .connect_le(&fixture.server.unique_name())
            .await
            .unwrap();
        let error = peripheral.discover_services().await.unwrap_err();
        assert!(matches!(error, Error::Platform(ref detail) if detail.code == "disconnected"));
        let state = fixture.state.lock().unwrap();
        assert!(state.le_connected);
        assert_eq!(state.snapshots, 3);
        assert_eq!(state.graph_reads, 0);
        assert_eq!(peripheral.accepted_le_gatt_ready_token().unwrap(), None);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn regressing_retired_attachment_cannot_readmit_original_attachment() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        fixture.state.lock().unwrap().snapshot_sequence =
            [(13, true), (12, true), (13, false)].into();
        peripheral
            .connect_le(&fixture.server.unique_name())
            .await
            .unwrap();
        assert!(peripheral.discover_services().await.is_err());
        assert_eq!(fixture.state.lock().unwrap().graph_reads, 0);
        assert_eq!(peripheral.accepted_le_gatt_ready_token().unwrap(), None);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn newer_disconnected_attachment_is_actual_current_transport_failure() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        fixture.state.lock().unwrap().snapshot_sequence =
            [(13, true), (14, true), (15, false)].into();
        peripheral
            .connect_le(&fixture.server.unique_name())
            .await
            .unwrap();
        let error = peripheral.discover_services().await.unwrap_err();
        assert!(
            matches!(error, Error::Platform(ref detail) if detail.code == "disconnected" && detail.metadata.iter().any(|(key,value)| *key == "attachment" && value == "14"))
        );
        assert_eq!(fixture.state.lock().unwrap().snapshots, 2);
        assert_eq!(fixture.state.lock().unwrap().graph_reads, 0);
        assert_eq!(peripheral.accepted_le_gatt_ready_token().unwrap(), None);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn connected_le_without_new_att_attachment_keeps_total_admission_deadline() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        fixture.state.lock().unwrap().disconnected_left = usize::MAX;
        peripheral
            .connect_le(&fixture.server.unique_name())
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(6), peripheral.discover_services())
            .await
            .expect("retired ATT wait stays inside original five-second budget")
            .unwrap_err();
        assert!(matches!(error, Error::TimedOut(bound) if bound == Duration::from_secs(5)));
        let state = fixture.state.lock().unwrap();
        assert!(state.snapshots > 1);
        assert_eq!(state.graph_reads, 0);
        assert_eq!(peripheral.accepted_le_gatt_ready_token().unwrap(), None);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn cancelled_new_attachment_wait_never_publishes_late_ready_reply() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        {
            let mut state = fixture.state.lock().unwrap();
            state.disconnected_left = 1;
            state.hold_snapshot_at = Some(2);
        }
        peripheral
            .connect_le(&fixture.server.unique_name())
            .await
            .unwrap();
        let owned = peripheral.clone();
        let task = tokio::spawn(async move { owned.discover_services().await });
        let entered = fixture.state.lock().unwrap().held_snapshot_entered.clone();
        entered.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let pending = {
            let mut state = fixture.state.lock().unwrap();
            state.hold_snapshot_at = None;
            state.pending_snapshot.take().unwrap()
        };
        let mut reply = pending.method_return();
        reply.append_all((1u32, 8u64, 1u64, "le", "ready", "none", 0i32, 0u8));
        fixture.replies.send(reply).unwrap();
        peripheral.le_gatt_snapshot().await.unwrap();
        assert_eq!(fixture.state.lock().unwrap().graph_reads, 0);
        assert_eq!(peripheral.accepted_le_gatt_ready_token().unwrap(), None);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn discovering_then_failed_never_reads_graph() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        {
            let mut state = fixture.state.lock().unwrap();
            state.discovering_left = 1;
            state.fail_at_snapshot = Some(2);
        }
        assert!(peripheral.discover_services().await.is_err());
        assert_eq!(fixture.state.lock().unwrap().snapshots, 2);
        assert_eq!(fixture.state.lock().unwrap().graph_reads, 0);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn held_snapshot_rpc_cannot_escape_total_admission_deadline() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        fixture.state.lock().unwrap().hold_snapshot = true;
        let task = tokio::spawn(async move { peripheral.discover_services().await });
        let entered = fixture.state.lock().unwrap().snapshot_entered.clone();
        entered.notified().await;
        let error = tokio::time::timeout(Duration::from_secs(6), task)
            .await
            .expect("entire5s wait must bound held30s RPC")
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(error, Error::TimedOut(bound) if bound == Duration::from_secs(5)),
            "actual deadline must remain timeout not assumed nativefailed"
        );
        assert_eq!(fixture.state.lock().unwrap().graph_reads, 0);
        assert_eq!(fixture.state.lock().unwrap().snapshots, 1);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn cancelled_initial_snapshot_never_publishes_after_late_reply() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        fixture.state.lock().unwrap().hold_snapshot = true;
        let owned = peripheral.clone();
        let task = tokio::spawn(async move { owned.discover_services().await });
        let entered = fixture.state.lock().unwrap().snapshot_entered.clone();
        entered.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let pending = {
            let mut state = fixture.state.lock().unwrap();
            state.hold_snapshot = false;
            state.pending_snapshot.take().unwrap()
        };
        let mut reply = pending.method_return();
        reply.append_all((1u32, 7u64, 1u64, "le", "ready", "none", 0i32, 0u8));
        fixture.replies.send(reply).unwrap();
        peripheral.le_gatt_snapshot().await.unwrap();
        assert_eq!(fixture.state.lock().unwrap().graph_reads, 0);
        assert_eq!(peripheral.accepted_le_gatt_ready_token().unwrap(), None);
    });
}

#[test]
#[ignore = "private session bus only; no radio"]
fn daemon_owner_replacement_during_held_snapshot_refuses_stale_ready() {
    run(async {
        let fixture = Fixture::new();
        let peripheral = fixture.peripheral().await;
        fixture.state.lock().unwrap().hold_snapshot = true;
        let task = tokio::spawn(async move { peripheral.discover_services().await });
        let entered = fixture.state.lock().unwrap().snapshot_entered.clone();
        entered.notified().await;
        // SyncConnection::process must not race a blocking call on the same
        // connection: the processing worker can consume its method reply.
        // Replace from an independent connection, retaining the old unique
        // owner so its deliberately late response still reaches the caller.
        let replacement = dbus::blocking::SyncConnection::new_session().unwrap();
        assert_eq!(
            replacement
                .request_name("org.bluez", false, true, true)
                .unwrap(),
            dbus::blocking::stdintf::org_freedesktop_dbus::RequestNameReply::PrimaryOwner
        );
        let (current_owner,): (String,) = replacement
            .with_proxy(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                Duration::from_secs(1),
            )
            .method_call("org.freedesktop.DBus", "GetNameOwner", ("org.bluez",))
            .unwrap();
        assert_eq!(current_owner, replacement.unique_name().to_string());
        assert_ne!(current_owner, fixture.server.unique_name().to_string());
        let pending = fixture
            .state
            .lock()
            .unwrap()
            .pending_snapshot
            .take()
            .unwrap();
        let mut reply = pending.method_return();
        reply.append_all((1u32, 7u64, 1u64, "le", "ready", "none", 0i32, 0u8));
        fixture.replies.send(reply).unwrap();
        let error = task.await.unwrap().unwrap_err();
        assert!(
            matches!(error, Error::Platform(ref detail)
                if detail.domain == "bluez-dbus"
                    && detail.code == "org.bluez.Error.NotSupported"
                    && detail.message.contains("daemon owner changed")),
            "a stale owner must be refused explicitly, not pass through an unrelated timeout: {error:?}"
        );
        assert_eq!(fixture.state.lock().unwrap().graph_reads, 0);
    });
}
