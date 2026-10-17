//! Deterministic boundary evidence, not physical-radio qualification.
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use ubm_desktop::continuation::{ContinuationHost, ContinuationSession, NativeContinuation};
use ubm_desktop::continuation_adapter::DesktopContinuationHost;
use ubm_desktop::{
    CharacteristicSnapshot, DesktopCentral, FakeRadio, OpControl, PropertyFlags, RadioEvent,
    ServiceSnapshot,
};
#[path = "../../test-support/recording_fixture.rs"]
mod recording_fixture;
use recording_fixture::{complete_fixture_process, isolated_fixture_process};
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
            primary: None,
            included_services: None,
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
                        write: true,
                        write_without_response: false,
                    },
                    descriptors: vec![],
                })
                .collect(),
            access: std::default::Default::default(),
        }],
    );
    let central = DesktopCentral::open(radio, "continuation-test")
        .await
        .unwrap();
    let executor = NativeContinuation::new(Arc::new(DesktopContinuationHost::new(central.clone())));
    (central, executor)
}

#[tokio::test]
async fn failed_discovery_continuation_link_is_released_by_its_parent() {
    let (central, engine) = fixture().await;
    central
        .boundary()
        .fail_next(ubm_desktop::FaultOp::Discover, "projection failed");
    engine.execute(PEER, &declaration()).await.unwrap_err();
    assert!(
        central.boundary().link_connected(PEER),
        "setup failure must not invent physical release"
    );
    assert!(central.shutdown().await.is_released());
    assert!(!central.boundary().link_connected(PEER));
    let claim = engine.prepare_claim(256, 1 << 20).await.unwrap();
    assert_eq!(
        engine
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
}

#[tokio::test]
async fn process_shutdown_retry_then_claim_recognizes_confirmed_parent_release() {
    let (central, engine) = fixture().await;
    engine.execute(PEER, &declaration()).await.unwrap();
    central
        .boundary()
        .fail_next(ubm_desktop::FaultOp::Disconnect, "one release refusal");
    let first = central.shutdown().await;
    assert!(!first.is_released());
    assert!(central.boundary().link_connected(PEER));
    let second = central.shutdown().await;
    assert!(second.is_released(), "{second:?}");
    assert!(!central.boundary().link_connected(PEER));
    let claim = engine.prepare_claim(256, 1 << 20).await.unwrap();
    let receipt = engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(receipt["disposed"], true, "{receipt}");
}

#[tokio::test]
async fn process_shutdown_after_confirmed_link_loss_allows_child_retirement() {
    let (central, engine) = fixture().await;
    engine.execute(PEER, &declaration()).await.unwrap();
    engine.stop_recovery();
    central.remote_peer_loss(PEER).await.unwrap();
    assert!(central.shutdown().await.is_released());
    let claim = engine.prepare_claim(256, 1 << 20).await.unwrap();
    let receipt = engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(receipt["disposed"], true, "{receipt}");
}

#[tokio::test]
async fn process_shutdown_pending_is_not_child_release_authority() {
    let (central, engine) = fixture().await;
    engine.execute(PEER, &declaration()).await.unwrap();
    central
        .boundary()
        .block_op(ubm_desktop::FaultOp::Disconnect);
    let owner = central.clone();
    let shutdown = tokio::spawn(async move { owner.shutdown().await });
    tokio::time::timeout(
        Duration::from_secs(2),
        central.boundary().wait_for_calls("disconnect", 1),
    )
    .await
    .unwrap();
    let claim = engine.prepare_claim(256, 1 << 20).await.unwrap();
    let token = claim["claimToken"].as_str().unwrap();
    assert_eq!(
        engine.acknowledge_claim(token).await.unwrap()["disposed"],
        false
    );
    central
        .boundary()
        .unblock_op(ubm_desktop::FaultOp::Disconnect);
    assert!(shutdown.await.unwrap().is_released());
    assert_eq!(
        engine.acknowledge_claim(token).await.unwrap()["disposed"],
        true
    );
    assert!(
        central
            .release_connection_lease(PEER, "foreign-lease", ubm_desktop::OpControl::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn process_shutdown_success_allows_explicit_later_claim_disposal() {
    let (central, engine) = fixture().await;
    engine.execute(PEER, &declaration()).await.unwrap();
    assert!(central.shutdown().await.is_released());
    let claim = engine.prepare_claim(256, 1 << 20).await.unwrap();
    let receipt = engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(receipt["disposed"], true, "{receipt}");
}

async fn invoke(session: &Arc<dyn ContinuationSession>, operation: &str, args: Value) -> Value {
    serde_json::from_str(&session.call(operation, &args.to_string()).await).unwrap()
}

#[tokio::test]
async fn pinned_declaration_precheck_compares_full_normalized_identity() {
    let (central, engine) = fixture().await;
    engine.execute(PEER, &declaration()).await.unwrap();
    assert!(
        engine
            .declaration_replacement_failure(&declaration())
            .is_none()
    );
    for (field, value) in [
        (
            "setup",
            json!([{"selector":{"serviceUuid":SERVICE,"characteristicUuid":CHARACTERISTIC},"value":[1],"timeoutMs":1000}]),
        ),
        (
            "link",
            json!({"mtu":{"requested":247,"timeoutMs":1000,"onUnsupported":"continue"}}),
        ),
        (
            "recording",
            json!({"id":"changed","maxBytes":1048576,"maxRecords":1000}),
        ),
    ] {
        let mut changed: Value = serde_json::from_str(&declaration()).unwrap();
        changed[field] = value;
        assert!(
            engine
                .declaration_replacement_failure(&changed.to_string())
                .is_some(),
            "{field} replacement must require claim"
        );
    }
    let claim = engine.prepare_claim(256, 65536).await.unwrap();
    engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    central.shutdown().await;
}

#[tokio::test]
async fn unattached_recording_can_retry_after_directory_configuration() {
    let Some(directory) =
        isolated_fixture_process("unattached_recording_can_retry_after_directory_configuration")
    else {
        return;
    };
    let (central, engine) = fixture().await;
    let mut order: Value = serde_json::from_str(&declaration()).unwrap();
    order["recording"] = json!({"id":"late-directory","maxBytes":1048576,"maxRecords":1000});
    assert!(engine.execute(PEER, &order.to_string()).await.is_err());
    engine.configure_recording_directory(&directory).unwrap();
    assert_eq!(
        engine.execute(PEER, &order.to_string()).await.unwrap()["event"],
        "continuation.completed"
    );
    assert_eq!(
        engine.recording_status("late-directory").unwrap()["accepting"],
        true
    );
    let before = engine.recording_status("late-directory").unwrap()["records"]
        .as_u64()
        .unwrap();
    inject_value(&central, 73).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while engine.recording_status("late-directory").unwrap()["records"]
            .as_u64()
            .unwrap()
            <= before
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let prepared = engine
        .recording_prepare("late-directory", 100, 65536)
        .unwrap();
    assert!(
        prepared["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["record"]["valueB64"] == "AEk=")
    );
    engine.recording_stop("late-directory").unwrap();
    let claim = engine.prepare_claim(256, 65536).await.unwrap();
    engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    central.shutdown().await;
    drop(engine);
    complete_fixture_process(&directory);
}

#[tokio::test]
async fn desktop_native_setup_observes_early_reply_but_waits_for_att_and_retains_data() {
    let (central, executor) = fixture().await;
    central.boundary().set_mtu(PEER, 247);
    central.boundary().block_op(ubm_desktop::FaultOp::Write);
    let mut order: Value = serde_json::from_str(&declaration()).unwrap();
    order["link"] = json!({"mtu":{"requested":512,"timeoutMs":1000,"onUnsupported":"continue"}});
    order["setup"] = json!([{"selector":order["resubscribe"][0],"value":[2,0],"timeoutMs":2000,"response":{"subscriptionIndex":0,"prefix":[240,2,0],"minLength":4,"maxLength":4,"status":{"offset":3,"accepted":[0]}}}]);
    let running = executor.clone();
    let result = tokio::spawn(async move { running.execute(PEER, &order.to_string()).await });
    tokio::time::timeout(
        Duration::from_secs(2),
        central.boundary().wait_for_calls("write_characteristic", 1),
    )
    .await
    .unwrap();
    central.boundary().push_event(RadioEvent::Notification {
        peer_id: PEER.into(),
        service_uuid: SERVICE.into(),
        service_occurrence: 0,
        characteristic_uuid: CHARACTERISTIC.into(),
        characteristic_occurrence: 0,
        value: vec![240, 2, 0, 0],
        epoch: central.routing_epoch(PEER).await,
    });
    assert!(
        !result.is_finished(),
        "application acknowledgement does not bypass held ATT receipt"
    );
    central.boundary().unblock_op(ubm_desktop::FaultOp::Write);
    assert_eq!(
        result.await.unwrap().unwrap()["link"]["mtu"]["outcome"],
        "unsupported"
    );
    let claim = executor.prepare_claim(256, 65536).await.unwrap();
    let records: Vec<Value> = claim["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|batch| {
            serde_json::from_str::<Value>(batch.as_str().unwrap()).unwrap()["records"]
                .as_array()
                .unwrap()
                .clone()
        })
        .collect();
    assert!(
        records
            .iter()
            .any(|record| record["valueB64"] == "8AIAAA==")
    );
    executor
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    central.shutdown().await;
}

#[tokio::test]
async fn durable_native_values_survive_radio_claim_and_restart_without_double_delivery() {
    let Some(directory) = isolated_fixture_process(
        "durable_native_values_survive_radio_claim_and_restart_without_double_delivery",
    ) else {
        return;
    };
    let (central, engine) = fixture().await;
    engine.configure_recording_directory(&directory).unwrap();
    let mut order: Value = serde_json::from_str(&declaration()).unwrap();
    order["recording"] = json!({"id":"native-test","maxBytes":1048576,"maxRecords":1000});
    engine.execute(PEER, &order.to_string()).await.unwrap();
    inject_value(&central, 72).await;
    let prepared = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status = engine.recording_status("native-test").unwrap();
            if status["records"]
                .as_u64()
                .is_some_and(|records| records >= 2)
            {
                break engine.recording_prepare("native-test", 100, 65536).unwrap();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        prepared["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["record"]["valueB64"] == "AEg=")
    );
    engine.recording_stop("native-test").unwrap();
    inject_value(&central, 73).await;
    let claim = engine.prepare_claim(256, 65536).await.unwrap();
    assert!(
        engine.recording_status("native-test").unwrap()["collectionFailure"].is_null(),
        "explicit collection stop is not a storage fault"
    );
    assert_eq!(claim["recording"], json!({"id":"native-test"}));
    let ordinary: Vec<Value> = claim["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|batch| {
            serde_json::from_str::<Value>(batch.as_str().unwrap()).unwrap()["records"]
                .as_array()
                .unwrap()
                .clone()
        })
        .collect();
    assert!(!ordinary.iter().any(|record| record["t"] == "value"));
    engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        engine.recording_prepare("native-test", 100, 65536).unwrap(),
        prepared
    );
    // A released radio generation must not retain the stopped recording's
    // admission state. Its independent durable cursor still belongs to A.
    order["recording"]["id"] = json!("next-recording");
    engine.execute(PEER, &order.to_string()).await.unwrap();
    inject_value(&central, 74).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if engine.recording_status("next-recording").unwrap()["records"]
                .as_u64()
                .is_some_and(|records| records >= 2)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let next_records = engine
        .recording_prepare("next-recording", 100, 65536)
        .unwrap();
    assert!(
        next_records["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["record"]["valueB64"] == "AEo=")
    );
    assert_eq!(
        engine.recording_prepare("native-test", 100, 65536).unwrap(),
        prepared
    );
    engine.recording_stop("next-recording").unwrap();
    let next_claim = engine.prepare_claim(256, 65536).await.unwrap();
    assert_eq!(next_claim["recording"], json!({"id":"next-recording"}));
    engine
        .acknowledge_claim(next_claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    central.shutdown().await;
    drop(engine);
    let (reopened_central, reopened) = fixture().await;
    reopened.configure_recording_directory(&directory).unwrap();
    assert_eq!(
        reopened
            .recording_prepare("native-test", 100, 65536)
            .unwrap(),
        prepared
    );
    reopened
        .recording_acknowledge("native-test", prepared["token"].as_str().unwrap())
        .unwrap();
    reopened.recording_stop("native-test").unwrap();
    reopened.recording_clear("native-test").unwrap();
    reopened_central.shutdown().await;
    drop(reopened);
    complete_fixture_process(&directory);
}

#[tokio::test]
async fn independent_store_stop_is_observed_without_waiting_for_sensor_ingress() {
    let Some(directory) = isolated_fixture_process(
        "independent_store_stop_is_observed_without_waiting_for_sensor_ingress",
    ) else {
        return;
    };
    let (central, engine) = fixture().await;
    engine.configure_recording_directory(&directory).unwrap();
    let mut order: Value = serde_json::from_str(&declaration()).unwrap();
    order["recording"] = json!({"id":"external-stop","maxBytes":1048576,"maxRecords":1000});
    engine.execute(PEER, &order.to_string()).await.unwrap();
    let offline = ubm_desktop::continuation_journal::JournalRegistry::default();
    offline.configure_directory(&directory).unwrap();
    offline.get("external-stop").unwrap().stop().unwrap();
    assert_eq!(
        engine.execute(PEER, &order.to_string()).await.unwrap_err()["code"],
        "lifecycle.invalid-state"
    );
    assert!(engine.recording_status("external-stop").unwrap()["collectionFailure"].is_null());
    let claim = engine.prepare_claim(256, 65536).await.unwrap();
    assert_eq!(
        engine
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
    central.shutdown().await;
    drop(engine);
    drop(offline);
    complete_fixture_process(&directory);
}

#[tokio::test]
async fn recording_stop_during_held_setup_prevents_late_success_and_keeps_release_reachable() {
    let Some(directory) = isolated_fixture_process(
        "recording_stop_during_held_setup_prevents_late_success_and_keeps_release_reachable",
    ) else {
        return;
    };
    let (central, engine) = fixture().await;
    central.boundary().set_mtu(PEER, 247);
    central.boundary().block_op(ubm_desktop::FaultOp::Write);
    engine.configure_recording_directory(&directory).unwrap();
    let mut order: Value = serde_json::from_str(&declaration()).unwrap();
    order["recording"] = json!({"id":"stop-test","maxBytes":1048576,"maxRecords":1000});
    order["setup"] = json!([{"selector":order["resubscribe"][0],"value":[2,0],"timeoutMs":2000}]);
    let running = engine.clone();
    let execution = tokio::spawn(async move { running.execute(PEER, &order.to_string()).await });
    tokio::time::timeout(
        Duration::from_secs(2),
        central.boundary().wait_for_calls("write_characteristic", 1),
    )
    .await
    .unwrap();
    assert_eq!(
        engine.recording_stop("stop-test").unwrap()["radioRelease"],
        "not-requested"
    );
    assert!(
        central.boundary().link_connected(PEER),
        "stopping collection does not falsely report radio release"
    );
    central.boundary().unblock_op(ubm_desktop::FaultOp::Write);
    assert_eq!(
        execution.await.unwrap().unwrap_err()["code"],
        "lifecycle.invalid-state"
    );
    assert!(engine.recording_status("stop-test").unwrap()["collectionFailure"].is_null());
    let claim = engine.prepare_claim(256, 65536).await.unwrap();
    assert!(
        engine
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"]
            == true
    );
    central.shutdown().await;
    drop(engine);
    complete_fixture_process(&directory);
}

#[tokio::test]
async fn reopened_recording_uses_a_distinct_session_epoch_before_new_ingress() {
    let Some(directory) = isolated_fixture_process(
        "reopened_recording_uses_a_distinct_session_epoch_before_new_ingress",
    ) else {
        return;
    };
    let mut epochs = Vec::new();
    for _ in 0..2 {
        let (central, engine) = fixture().await;
        engine.configure_recording_directory(&directory).unwrap();
        let mut order: Value = serde_json::from_str(&declaration()).unwrap();
        order["recording"] = json!({"id":"epoch-test","maxBytes":1048576,"maxRecords":1000});
        engine.execute(PEER, &order.to_string()).await.unwrap();
        let prepared = engine.recording_prepare("epoch-test", 100, 65536).unwrap();
        let session = &prepared["records"][0]["metadata"]["session"];
        assert!(session["sessionId"].is_string());
        epochs.push(session["sessionEpoch"].as_str().unwrap().to_owned());
        engine
            .recording_acknowledge("epoch-test", prepared["token"].as_str().unwrap())
            .unwrap();
        let claim = engine.prepare_claim(256, 65536).await.unwrap();
        engine
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap();
        central.shutdown().await;
    }
    assert_ne!(epochs[0], epochs[1]);
    complete_fixture_process(&directory);
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

/// Gives every journal commit a fixed cost inside its own transaction, as a
/// slow synced filesystem does. Per-value commits then scale with the backlog
/// while a bounded group pays it once. SQLite fixes `now` for a statement, so
/// the cost is a row count calibrated against this machine.
fn make_every_commit_cost(path: &std::path::Path, millis: u64) {
    let connection = rusqlite::Connection::open(path).unwrap();
    let spin = |rows: u64| {
        let started = std::time::Instant::now();
        connection
            .query_row(
                &format!(
                    "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM s WHERE x<{rows}) \
                     SELECT count(*) FROM s"
                ),
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        started.elapsed()
    };
    let probe = 200_000u64;
    let per_probe = spin(probe).min(spin(probe)).as_secs_f64().max(1e-6);
    let mut rows = (probe as f64 * (millis as f64 / 1000.0) / per_probe)
        .ceil()
        .max(1.0) as u64;
    calibrate_commit_cost(&connection, &mut rows, millis);
}

fn calibrate_commit_cost(connection: &rusqlite::Connection, rows: &mut u64, millis: u64) {
    let floor_ms = millis
        .checked_mul(3)
        .and_then(|value| value.checked_div(4))
        .expect("commit-cost floor overflowed");
    assert!(
        floor_ms >= 30,
        "the commit-cost calibration requires a floor of at least 30 ms"
    );

    const MAX_ADJUSTMENTS: usize = 6;
    const MAX_ROWS: u64 = 50_000_000;
    for adjustment in 0..=MAX_ADJUSTMENTS {
        assert!(
            *rows <= MAX_ROWS,
            "commit-cost calibration exceeded row bound"
        );
        connection
            .execute_batch(&format!(
                "DROP TRIGGER IF EXISTS slow_commit_cursor; DROP VIEW IF EXISTS slow_commit; \
                 CREATE VIEW slow_commit AS WITH RECURSIVE s(x) AS (\
                   SELECT 1 UNION ALL SELECT x+1 FROM s WHERE x<{}) \
                 SELECT count(*) AS n FROM s; \
                 CREATE TRIGGER slow_commit_cursor AFTER UPDATE OF next_ordinal ON journal \
                 BEGIN SELECT n FROM slow_commit; END;",
                *rows
            ))
            .unwrap();
        let measure = || {
            // Measure the actual UPDATE-trigger path, while rolling back the
            // probe so calibration cannot persist journal state.
            connection
                .execute_batch("SAVEPOINT commit_cost_probe")
                .unwrap();
            let started = std::time::Instant::now();
            connection
                .execute("UPDATE journal SET next_ordinal=next_ordinal+1", [])
                .unwrap();
            let elapsed = started.elapsed();
            connection
                .execute_batch("ROLLBACK TO commit_cost_probe; RELEASE commit_cost_probe")
                .unwrap();
            elapsed
        };
        let observed = measure().min(measure());
        if observed >= Duration::from_millis(millis) {
            return;
        }
        assert!(
            adjustment < MAX_ADJUSTMENTS,
            "the fixture must really cost at least {millis} ms per commit; observed {observed:?}"
        );
        let actual_ms = observed.as_secs_f64() * 1000.0;
        let factor = (millis as f64 / actual_ms.max(0.001)) * 1.15;
        let next = ((*rows as f64) * factor).ceil();
        assert!(
            next.is_finite() && next >= *rows as f64 && next <= MAX_ROWS as f64,
            "invalid commit-cost adjustment"
        );
        *rows = next as u64;
    }
    unreachable!("bounded commit-cost calibration exhausted");
}

#[test]
fn commit_cost_calibration_adjusts_from_a_short_initial_probe() {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE journal(next_ordinal INTEGER); \
             INSERT INTO journal VALUES(0);",
        )
        .unwrap();
    let mut rows = 1;
    calibrate_commit_cost(&connection, &mut rows, 40);
    assert_eq!(
        connection
            .query_row("SELECT next_ordinal FROM journal", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0,
        "calibration probes must roll back journal state"
    );
    let started = std::time::Instant::now();
    connection
        .execute("UPDATE journal SET next_ordinal=next_ordinal+1", [])
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(
        connection
            .query_row("SELECT next_ordinal FROM journal", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1,
        "the real journal update must still increment the ordinal"
    );
    assert!(
        elapsed >= Duration::from_millis(30),
        "calibrated trigger must cost at least 30 ms: {elapsed:?} (rows={rows})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_response_behind_a_recorded_backlog_is_acknowledged_within_its_deadline() {
    let Some(directory) = isolated_fixture_process(
        "setup_response_behind_a_recorded_backlog_is_acknowledged_within_its_deadline",
    ) else {
        return;
    };
    let (central, engine) = fixture().await;
    central.boundary().set_mtu(PEER, 247);
    central.boundary().block_op(ubm_desktop::FaultOp::Write);
    engine.configure_recording_directory(&directory).unwrap();
    let mut order: Value = serde_json::from_str(&declaration()).unwrap();
    order["recording"] = json!({"id":"backlog","maxBytes":1048576,"maxRecords":1000});
    order["setup"] = json!([{"selector":order["resubscribe"][0],"value":[2,0],"timeoutMs":2000,"response":{"subscriptionIndex":0,"prefix":[240,2,0],"minLength":4,"maxLength":4,"status":{"offset":3,"accepted":[0]}}}]);
    let running = engine.clone();
    let mut result = tokio::spawn(async move { running.execute(PEER, &order.to_string()).await });
    tokio::select! {
        () = central.boundary().wait_for_calls("write_characteristic", 1) => {}
        finished = &mut result => panic!("setup ended before its write was held: {finished:?}"),
        () = tokio::time::sleep(Duration::from_secs(5)) => panic!("setup never reached its write"),
    }
    // The journal exists and its setup observer is registered. Every later
    // commit costs 40 ms: 121 per-value commits are ~4.8 s against the 2 s step
    // deadline, a few bounded groups are a fraction of it.
    make_every_commit_cost(&directory.join("backlog.sqlite"), 40);
    let epoch = central.routing_epoch(PEER).await;
    let notification = |value: Vec<u8>| RadioEvent::Notification {
        peer_id: PEER.into(),
        service_uuid: SERVICE.into(),
        service_occurrence: 0,
        characteristic_uuid: CHARACTERISTIC.into(),
        characteristic_occurrence: 0,
        value,
        epoch,
    };
    for index in 0..120u8 {
        central.boundary().push_event(notification(vec![0, index]));
    }
    central
        .boundary()
        .push_event(notification(vec![240, 2, 0, 0]));
    central.boundary().unblock_op(ubm_desktop::FaultOp::Write);
    let started = std::time::Instant::now();
    result
        .await
        .unwrap()
        .expect("the response behind the backlog is acknowledged inside its step deadline");
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "bounded groups, not {} per-value commits: {:?}",
        121,
        started.elapsed()
    );
    // Nothing was dropped to meet the deadline: every value is durable.
    let status = engine.recording_status("backlog").unwrap();
    assert!(
        status["records"].as_u64().unwrap() >= 121,
        "all 120 values and the response are retained: {status}"
    );
    assert_eq!(status["lostRecords"], 0);
    engine.recording_stop("backlog").unwrap();
    let claim = engine.prepare_claim(256, 65536).await.unwrap();
    engine
        .acknowledge_claim(claim["claimToken"].as_str().unwrap())
        .await
        .unwrap();
    central.shutdown().await;
    drop(engine);
    complete_fixture_process(&directory);
}
