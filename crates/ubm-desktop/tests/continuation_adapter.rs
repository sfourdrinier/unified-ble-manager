//! Deterministic boundary evidence, not physical-radio qualification.
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use ubm_desktop::continuation::{ContinuationHost, ContinuationSession, NativeContinuation};
use ubm_desktop::continuation_adapter::DesktopContinuationHost;
use ubm_desktop::{
    CharacteristicSnapshot, DesktopCentral, FakeRadio, OpControl, PropertyFlags, RadioEvent,
    ServiceSnapshot,
};
const PEER: &str = "AA:BB:CC:DD:EE:FF";
const SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const CHARACTERISTIC: &str = "00002a37-0000-1000-8000-00805f9b34fb";
const SECOND_CHARACTERISTIC: &str = "00002a38-0000-1000-8000-00805f9b34fb";
fn declaration() -> String {
    json!({"onAppearance":"native","peerId":PEER,"resubscribe":[{"serviceUuid":SERVICE,"characteristicUuid":CHARACTERISTIC}]}).to_string()
}
async fn fixture() -> (DesktopCentral<FakeRadio>, NativeContinuation) {
    let radio = FakeRadio::new();
    radio.set_services(
        PEER,
        vec![ServiceSnapshot {
            uuid: SERVICE.into(),
            occurrence: 0,
            characteristics: [CHARACTERISTIC, SECOND_CHARACTERISTIC]
                .into_iter()
                .map(|uuid| CharacteristicSnapshot {
                    uuid: uuid.into(),
                    occurrence: 0,
                    properties: PropertyFlags {
                        notify: true,
                        indicate: false,
                        read: true,
                        write: false,
                        write_without_response: false,
                    },
                    descriptors: vec![],
                })
                .collect(),
        }],
    );
    let central = DesktopCentral::open(radio, "continuation-test")
        .await
        .unwrap();
    let executor = NativeContinuation::new(Arc::new(DesktopContinuationHost::new(central.clone())));
    (central, executor)
}

async fn invoke(session: &Arc<dyn ContinuationSession>, operation: &str, args: Value) -> Value {
    serde_json::from_str(&session.call(operation, &args.to_string()).await).unwrap()
}

async fn wait_for_notification_call(central: &DesktopCentral<FakeRadio>, count: usize) {
    tokio::time::timeout(
        Duration::from_secs(10),
        central
            .boundary()
            .wait_for_calls("set_notifications", count),
    )
    .await
    .expect("notification operation did not enter its native boundary");
}

async fn inject_value(central: &DesktopCentral<FakeRadio>, bpm: u8) {
    let mut wake = central.native_wakes();
    central.boundary().push_event(RadioEvent::Notification {
        peer_id: PEER.into(),
        service_uuid: SERVICE.into(),
        service_occurrence: 0,
        characteristic_uuid: CHARACTERISTIC.into(),
        characteristic_occurrence: 0,
        value: vec![0, bpm],
        epoch: central.routing_epoch(PEER).await,
    });
    tokio::time::timeout(Duration::from_secs(2), wake.recv())
        .await
        .unwrap()
        .unwrap();
}

async fn subscribed_session(central: &DesktopCentral<FakeRadio>) -> Arc<dyn ContinuationSession> {
    let session = DesktopContinuationHost::new(central.clone())
        .open_session()
        .unwrap();
    assert_eq!(
        invoke(&session, "connection.connect", json!({"peerId":PEER})).await["ok"],
        true
    );
    assert_eq!(
        invoke(&session, "gatt.discover", json!({"peerId":PEER})).await["ok"],
        true
    );
    assert_eq!(invoke(&session,"gatt.subscribe",json!({"peerId":PEER,"consumer":"bounded","selector":{"serviceUuid":SERVICE,"characteristicUuid":CHARACTERISTIC}})).await["ok"],true);
    session
}

async fn drain_records(session: &Arc<dyn ContinuationSession>) -> Vec<Value> {
    let mut records = Vec::new();
    loop {
        let batch: Value = serde_json::from_str(&session.drain(256, 1 << 20).await).unwrap();
        let entries = batch["records"].as_array().unwrap();
        if entries.is_empty() {
            return records;
        }
        records.extend(entries.iter().cloned());
    }
}

#[tokio::test]
async fn sealed_full_backlog_counts_disposal_tail_once_without_overflow_terminal() {
    let (central, _) = fixture().await;
    let session = subscribed_session(&central).await;
    for _ in 0..ubm_desktop::continuation_outbox::DATA_RECORD_CAP {
        inject_value(&central, 72).await;
        assert_eq!(
            invoke(&session, "session.reconcile", json!({})).await["ok"],
            true
        );
    }
    assert_eq!(
        invoke(&session, "counters.describe", json!({})).await["value"]["queuedData"],
        ubm_desktop::continuation_outbox::DATA_RECORD_CAP
    );
    assert_eq!(
        invoke(&session, "session.quiesce", json!({})).await["value"]["state"],
        "sealed"
    );
    central
        .boundary()
        .block_op(ubm_desktop::FaultOp::Unsubscribe);
    let pending_session = session.clone();
    let pending = tokio::spawn(async move {
        invoke(&pending_session, "session.continuation-dispose", json!({})).await
    });
    wait_for_notification_call(&central, 2).await;
    for bpm in 73..76 {
        inject_value(&central, bpm).await;
    }
    central
        .boundary()
        .unblock_op(ubm_desktop::FaultOp::Unsubscribe);
    let receipt = pending.await.unwrap();
    assert_eq!(receipt["value"]["state"], "released");
    assert_eq!(receipt["value"]["afterCutoffItems"], 3, "{receipt}");
    assert!(receipt["value"]["afterCutoffBytes"].as_u64().unwrap() > 0);
    let records = drain_records(&session).await;
    assert_eq!(
        records
            .iter()
            .filter(|record| record["t"] == "value")
            .count(),
        ubm_desktop::continuation_outbox::DATA_RECORD_CAP
    );
    assert!(
        !records.iter().any(|record| record["reason"] == "overflow"),
        "sealed loss is not overflow: {records:?}"
    );
    central.shutdown().await;
}

#[tokio::test]
async fn refused_connect_does_not_invent_a_session_release_obligation() {
    // Open first, then close the central, to exercise admission refusal on an
    // existing session rather than the host's open-session guard.
    let (central, _) = fixture().await;
    let session = DesktopContinuationHost::new(central.clone())
        .open_session()
        .unwrap();
    central.shutdown().await;
    let result = invoke(&session, "connection.connect", json!({"peerId":PEER})).await;
    assert_eq!(result["ok"], false);
    assert_eq!(result["error"]["code"], "adapter.unavailable");
    let disposed = invoke(&session, "session.continuation-dispose", json!({})).await;
    assert_eq!(disposed["value"]["state"], "released", "{disposed}");
    assert_eq!(disposed["value"]["failures"], json!([]));
    assert!(
        !central
            .boundary()
            .calls()
            .iter()
            .any(|call| call == "connect")
    );
}

#[tokio::test]
async fn held_second_subscribe_does_not_block_existing_route_collection() {
    let (central, _) = fixture().await;
    let session = DesktopContinuationHost::new(central.clone())
        .open_session()
        .unwrap();
    assert_eq!(
        invoke(&session, "connection.connect", json!({"peerId":PEER})).await["ok"],
        true
    );
    assert_eq!(
        invoke(&session, "gatt.discover", json!({"peerId":PEER})).await["ok"],
        true
    );
    let subscribe = |uuid, consumer| json!({"peerId":PEER,"consumer":consumer,"selector":{"serviceUuid":SERVICE,"characteristicUuid":uuid}});
    assert_eq!(
        invoke(
            &session,
            "gatt.subscribe",
            subscribe(CHARACTERISTIC, "first")
        )
        .await["ok"],
        true
    );
    central.boundary().block_op(ubm_desktop::FaultOp::Subscribe);
    let pending_session = session.clone();
    let pending_args = subscribe(SECOND_CHARACTERISTIC, "second");
    let pending =
        tokio::spawn(async move { invoke(&pending_session, "gatt.subscribe", pending_args).await });
    wait_for_notification_call(&central, 2).await;
    inject_value(&central, 72).await;
    let collection = tokio::time::timeout(
        Duration::from_secs(2),
        invoke(&session, "session.reconcile", json!({})),
    )
    .await;
    central
        .boundary()
        .unblock_op(ubm_desktop::FaultOp::Subscribe);
    assert_eq!(pending.await.unwrap()["ok"], true);
    assert_eq!(
        collection.expect("collection barrier blocked by unrelated held enable")["ok"],
        true
    );
    assert_eq!(
        invoke(&session, "counters.describe", json!({})).await["value"]["queuedData"],
        1
    );
    assert_eq!(
        invoke(&session, "session.continuation-dispose", json!({})).await["value"]["state"],
        "released"
    );
    central.shutdown().await;
}

#[tokio::test]
async fn failed_native_connect_is_compensated_by_central_not_disposed_as_acquired() {
    let (central, _) = fixture().await;
    let session = DesktopContinuationHost::new(central.clone())
        .open_session()
        .unwrap();
    central
        .boundary()
        .fail_next(ubm_desktop::FaultOp::Connect, "radio refused");
    assert_eq!(
        invoke(&session, "connection.connect", json!({"peerId":PEER})).await["ok"],
        false
    );
    assert_eq!(
        central
            .boundary()
            .calls()
            .iter()
            .filter(|call| call.as_str() == "disconnect")
            .count(),
        1
    );
    assert_eq!(
        invoke(&session, "session.continuation-dispose", json!({})).await["value"]["state"],
        "released"
    );
    assert_eq!(
        central
            .boundary()
            .calls()
            .iter()
            .filter(|call| call.as_str() == "disconnect")
            .count(),
        1
    );
    // The same session can acquire a later successful lease and release it.
    assert_eq!(
        invoke(&session, "connection.connect", json!({"peerId":PEER})).await["ok"],
        true
    );
    assert_eq!(
        invoke(&session, "session.continuation-dispose", json!({})).await["value"]["state"],
        "released"
    );
    assert!(!central.boundary().link_connected(PEER));
    central.shutdown().await;
}

#[tokio::test]
async fn refused_subscribe_keeps_provisional_route_available_for_cleanup() {
    let (central, _) = fixture().await;
    let session = DesktopContinuationHost::new(central.clone())
        .open_session()
        .unwrap();
    assert_eq!(
        invoke(&session, "connection.connect", json!({"peerId":PEER})).await["ok"],
        true
    );
    assert_eq!(
        invoke(&session, "gatt.discover", json!({"peerId":PEER})).await["ok"],
        true
    );
    central
        .boundary()
        .fail_next(ubm_desktop::FaultOp::Subscribe, "CCCD refused");
    let result = invoke(&session,"gatt.subscribe",json!({"peerId":PEER,"consumer":"refused","selector":{"serviceUuid":SERVICE,"characteristicUuid":CHARACTERISTIC}})).await;
    assert_eq!(result["ok"], false);
    assert_eq!(result["error"]["code"], "gatt.subscribe-failed");
    let reconciled = invoke(&session, "session.reconcile", json!({})).await;
    assert_eq!(
        reconciled["value"]["subscriptions"],
        json!([{"consumer":"refused","state":"closed"}])
    );
    assert_eq!(
        invoke(&session, "session.continuation-dispose", json!({})).await["value"]["state"],
        "released"
    );
    assert!(!central.boundary().link_connected(PEER));
    central.shutdown().await;
}
#[tokio::test]
async fn native_collection_and_replayable_claim_need_no_ui() {
    let (central, executor) = fixture().await;
    assert_eq!(
        executor.execute(PEER, &declaration()).await.unwrap()["resubscribed"],
        1
    );
    let epoch = central.routing_epoch(PEER).await;
    let mut wake = central.native_wakes();
    central.boundary().push_event(RadioEvent::Notification {
        peer_id: PEER.into(),
        service_uuid: SERVICE.into(),
        service_occurrence: 0,
        characteristic_uuid: CHARACTERISTIC.into(),
        characteristic_occurrence: 0,
        value: vec![0, 72],
        epoch,
    });
    tokio::time::timeout(Duration::from_secs(2), wake.recv())
        .await
        .unwrap()
        .unwrap();
    let claim = executor.prepare_claim(256, 1 << 20).await.unwrap();
    let records: Vec<Value> = claim["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|batch| {
            let batch: Value = serde_json::from_str(batch.as_str().unwrap()).unwrap();
            batch["records"].as_array().unwrap().clone()
        })
        .collect();
    assert!(
        records
            .iter()
            .any(|r| r["t"] == "value" && r["valueB64"] == "AEg="),
        "{records:?}"
    );
    assert_eq!(executor.prepare_claim(256, 1 << 20).await.unwrap(), claim);
    let receipt = executor
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(receipt["disposed"], true);
    assert!(!central.boundary().link_connected(PEER));
    assert_eq!(
        executor
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap(),
        receipt
    );
    central.shutdown().await;
}
#[tokio::test]
async fn repeat_is_idempotent_but_link_loss_starts_a_new_generation() {
    let (central, executor) = fixture().await;
    executor.execute(PEER, &declaration()).await.unwrap();
    executor.execute(PEER, &declaration()).await.unwrap();
    central.remote_peer_loss(PEER).await.unwrap();
    executor.execute(PEER, &declaration()).await.unwrap();
    assert!(central.boundary().link_connected(PEER));
    let claim = executor.prepare_claim(256, 1 << 20).await.unwrap();
    assert_eq!(claim["consumerCount"], 2);
    assert_eq!(
        executor
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
    central.shutdown().await;
}
#[tokio::test]
async fn claim_preserves_another_clients_connection_lease() {
    let (central, executor) = fixture().await;
    central
        .connect(PEER, "other-client", OpControl::budget_ms(1000))
        .await
        .unwrap();
    executor.execute(PEER, &declaration()).await.unwrap();
    let claim = executor.prepare_claim(256, 1 << 20).await.unwrap();
    assert_eq!(
        executor
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
    assert!(
        central.boundary().link_connected(PEER),
        "must not drop another client's link"
    );
    central
        .discover(PEER, "other-client", OpControl::budget_ms(1000))
        .await
        .unwrap();
    central.shutdown().await;
}

#[tokio::test]
async fn claim_counts_values_arriving_while_native_unsubscribe_is_held() {
    let (central, executor) = fixture().await;
    executor.execute(PEER, &declaration()).await.unwrap();
    let claim = executor.prepare_claim(256, 1 << 20).await.unwrap();
    let token = claim["claimToken"].as_str().unwrap().to_owned();
    central
        .boundary()
        .block_op(ubm_desktop::FaultOp::Unsubscribe);
    let pending = tokio::spawn(async move { executor.acknowledge_claim(&token).await });
    wait_for_notification_call(&central, 2).await;
    let mut wake = central.native_wakes();
    central.boundary().push_event(RadioEvent::Notification {
        peer_id: PEER.into(),
        service_uuid: SERVICE.into(),
        service_occurrence: 0,
        characteristic_uuid: CHARACTERISTIC.into(),
        characteristic_occurrence: 0,
        value: vec![0, 73],
        epoch: central.routing_epoch(PEER).await,
    });
    tokio::time::timeout(Duration::from_secs(2), wake.recv())
        .await
        .unwrap()
        .unwrap();
    // The wake confirms central admission. Whether the native collector or
    // atomic unsubscribe tail observes it first must give the same count.
    assert!(!pending.is_finished());
    central
        .boundary()
        .unblock_op(ubm_desktop::FaultOp::Unsubscribe);
    let receipt = pending.await.unwrap().unwrap();
    assert_eq!(receipt["disposed"], true);
    assert_eq!(receipt["afterCutoffLoss"]["items"], 1, "{receipt}");
    assert!(receipt["afterCutoffLoss"]["bytes"].as_u64().unwrap() > 0);
    central.shutdown().await;
}

#[tokio::test]
async fn failed_parent_release_keeps_claim_retryable() {
    let (central, executor) = fixture().await;
    executor.execute(PEER, &declaration()).await.unwrap();
    let claim = executor.prepare_claim(256, 1 << 20).await.unwrap();
    central
        .boundary()
        .fail_next(ubm_desktop::FaultOp::Unsubscribe, "held CCCD");
    central
        .boundary()
        .fail_next(ubm_desktop::FaultOp::Disconnect, "held link");
    let token = claim["claimToken"].as_str().unwrap();
    assert_eq!(
        executor.acknowledge_claim(token).await.unwrap()["disposed"],
        false
    );
    assert_eq!(
        executor.acknowledge_claim(token).await.unwrap()["disposed"],
        true
    );
    assert!(!central.boundary().link_connected(PEER));
    central.shutdown().await;
}

#[tokio::test]
async fn native_supervisor_recovers_a_dropped_link_without_ui_requests() {
    let (central, _) = fixture().await;
    let owner = ubm_desktop::continuation_adapter::DesktopContinuation::new(
        central.clone(),
        tokio::runtime::Handle::current(),
    );
    owner.engine.execute(PEER, &declaration()).await.unwrap();
    central.remote_peer_loss(PEER).await.unwrap();
    wait_for_notification_call(&central, 2).await;
    assert!(central.boundary().link_connected(PEER));
    // The second enable entry proves supervision ran. Claim admission takes
    // the engine lock after that execution and is the final completion barrier.
    let claim = owner.engine.prepare_claim(256, 1 << 20).await.unwrap();
    assert_eq!(claim["consumerCount"], 2);
    assert_eq!(
        owner
            .engine
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
    central.shutdown().await;
}

#[tokio::test]
async fn retained_data_is_bounded_and_overflow_terminal_is_once_only() {
    let (central, _) = fixture().await;
    let session = subscribed_session(&central).await;
    for _ in 0..=ubm_desktop::continuation_outbox::DATA_RECORD_CAP {
        inject_value(&central, 72).await;
        // Central admission followed by the native reconcile barrier:
        // no scheduling delay is evidence that collection completed.
        assert_eq!(
            invoke(&session, "session.reconcile", json!({})).await["ok"],
            true
        );
    }
    assert_eq!(central.boundary().dropped_notification_count(), 0);
    let records = drain_records(&session).await;
    assert_eq!(
        records.iter().filter(|r| r["t"] == "value").count(),
        ubm_desktop::continuation_outbox::DATA_RECORD_CAP
    );
    let terminals: Vec<_> = records.iter().filter(|r| r["t"] == "stream-end").collect();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0]["reason"], "overflow");
    assert_eq!(terminals[0]["droppedItems"], 1);
    assert_eq!(
        invoke(&session, "session.continuation-dispose", json!({})).await["value"]["state"],
        "released"
    );
    central.shutdown().await;
}

#[tokio::test]
async fn shutdown_refuses_native_restart_on_the_same_radio_owner() {
    let (central, executor) = fixture().await;
    central.shutdown().await;
    assert_eq!(
        executor.execute(PEER, &declaration()).await.unwrap_err()["code"],
        "lifecycle.destroyed"
    );
}
