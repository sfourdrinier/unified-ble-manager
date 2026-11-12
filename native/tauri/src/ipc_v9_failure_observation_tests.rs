use super::*;
use crate::IpcDispatcher;
use std::sync::atomic::{AtomicUsize, Ordering};
use tauri::ipc::Channel;
use ubm_desktop::{DesktopCentral, FakeRadio};

fn offer(v: i64) -> IpcValue {
    let range = |axis: &str, n: i64| {
        let selected = object([
            ("axis", string(axis)),
            ("value", IpcValue::Number(n.into())),
        ]);
        object([
            ("axis", string(axis)),
            ("minimum", selected.clone()),
            ("maximum", selected),
        ])
    };
    object([
        ("backendContract", range("backend-contract", 1)),
        ("capabilitySchema", range("capability-schema", 1)),
        ("eventSchema", range("event-schema", 1)),
        ("traceFormat", range("trace-format", 1)),
        ("ipcProtocol", range("ipc-protocol", v)),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordered_routes_require_v9_but_preserve_v6_to_v10_bootstrap_compatibility() {
    let opens = Arc::new(AtomicUsize::new(0));
    let observed = opens.clone();
    let dispatcher = BtleplugDispatcher::with_profiled_opener(
        BtleplugDispatcherOptions::default(),
        Arc::new(move |profile, _policy| {
            observed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let central = DesktopCentral::open_with(FakeRadio::new(), profile)
                    .await
                    .map_err(|e| DispatchError::from_core(&e))?;
                Ok(Arc::new(central) as Arc<dyn CoreAuthority>)
            })
        }),
    );
    let caller = crate::AuthenticatedCaller::new("test".into(), "main".into());
    let route = |bootstrap: &serde_json::Value,
                 command: &str,
                 correlation: &str,
                 payload: serde_json::Value| {
        IpcValue::from_wire(serde_json::json!({"kind":"route", "envelope": {
            "versions": bootstrap["versions"], "attachment": bootstrap["attachment"],
            "attachmentId": bootstrap["attachmentId"], "renderer": bootstrap["renderer"],
            "rendererLease": bootstrap["rendererLease"], "correlation": correlation,
            "dispatchEpoch": "dispatch", "command": command, "payload": payload,
            "binaryPayload": null
        }}))
        .unwrap()
    };
    for v in [5] {
        let request = object([("kind", string("bootstrap")), ("offer", offer(v))]);
        let response = dispatcher
            .dispatch(
                caller.clone(),
                request,
                Some(IpcEventSink::new(Channel::new(|_| Ok(())))),
            )
            .await
            .into_wire();
        assert_eq!(response["error"]["code"], "protocol.incompatible");
        assert_eq!(opens.load(Ordering::SeqCst), 0);
    }
    for version in [6, 8] {
        let request = object([("kind", string("bootstrap")), ("offer", offer(version))]);
        let response = dispatcher
            .dispatch(
                caller.clone(),
                request,
                Some(IpcEventSink::new(Channel::new(|_| Ok(())))),
            )
            .await
            .into_wire();
        assert_eq!(response["kind"], "bootstrap");
        assert_eq!(
            response["bootstrap"]["versions"]["ipcProtocol"]["selected"]["value"],
            version
        );
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        let bootstrap = &response["bootstrap"];

        let ordinary = dispatcher
            .dispatch(
                caller.clone(),
                route(
                    bootstrap,
                    "adapter.state",
                    &format!("v{version}-ordinary"),
                    serde_json::json!({}),
                ),
                None,
            )
            .await
            .into_wire();
        assert_eq!(
            ordinary["kind"], "route",
            "v{version} ordinary BLE route: {ordinary}"
        );

        let before = {
            let state = dispatcher.inner.lock().await;
            let caller_state = state.callers.get(&caller_key(&caller)).unwrap();
            (
                caller_state.operations.len(),
                caller_state.completed_correlations.len(),
                caller_state.ordered_groups.len(),
            )
        };
        let opens_before = opens.load(Ordering::SeqCst);
        let rejected = dispatcher
            .dispatch(
                caller.clone(),
                route(
                    bootstrap,
                    "gatt.ordered.open",
                    &format!("v{version}-ordered"),
                    serde_json::json!({}),
                ),
                None,
            )
            .await
            .into_wire();
        assert_eq!(rejected["error"]["code"], "capability.unsupported");
        assert_eq!(
            rejected["error"]["operation"],
            "tauri.ordered.ipc-v9-required"
        );
        let after = {
            let state = dispatcher.inner.lock().await;
            let caller_state = state.callers.get(&caller_key(&caller)).unwrap();
            (
                caller_state.operations.len(),
                caller_state.completed_correlations.len(),
                caller_state.ordered_groups.len(),
            )
        };
        assert_eq!(
            after, before,
            "v{version} ordered rejection must not claim caller operation state"
        );
        assert_eq!(
            opens.load(Ordering::SeqCst),
            opens_before,
            "v{version} ordered rejection must not open authority"
        );
    }

    let request = object([("kind", string("bootstrap")), ("offer", offer(9))]);
    let response = dispatcher
        .dispatch(
            caller.clone(),
            request,
            Some(IpcEventSink::new(Channel::new(|_| Ok(())))),
        )
        .await
        .into_wire();
    assert_eq!(response["kind"], "bootstrap");
    assert_eq!(
        response["bootstrap"]["versions"]["ipcProtocol"]["selected"]["value"],
        9
    );
    let bootstrap = &response["bootstrap"];
    for command in [
        "gatt.ordered.open",
        "gatt.ordered-recording.reserve",
        "gatt.ordered-recording.start",
        "gatt.ordered-recording.reclaim.reserve",
        "gatt.ordered-recording.reclaim.start",
        "gatt.ordered-recording.reclaim.commit",
        "gatt.ordered.marker.reserve",
        "gatt.ordered.marker.start",
        "gatt.ordered-recording.marker.reserve",
        "gatt.ordered-recording.marker.start",
    ] {
        let reply = dispatcher
            .dispatch(
                caller.clone(),
                route(bootstrap, command, command, serde_json::json!({})),
                None,
            )
            .await
            .into_wire();
        assert_ne!(
            reply["error"]["operation"], "tauri.ordered.ipc-v9-required",
            "{command}: {reply}"
        );
    }
    let reply = dispatcher
        .dispatch(
            caller.clone(),
            route(
                bootstrap,
                "operation.cancel",
                "cancel",
                serde_json::json!({"targetCorrelation":"gatt.ordered.open"}),
            ),
            None,
        )
        .await
        .into_wire();
    assert_eq!(reply["payload"]["state"], "already-terminal");
    assert_eq!(opens.load(Ordering::SeqCst), 1);

    let v9_grammar = dispatcher
        .dispatch(
            caller.clone(),
            route(
                bootstrap,
                "gatt.ordered.status",
                "v9-real-grammar",
                serde_json::json!({"orderedHandle":"missing"}),
            ),
            None,
        )
        .await
        .into_wire();
    assert_ne!(
        v9_grammar["error"]["operation"],
        "tauri.ordered.ipc-v9-required"
    );

    let request = object([("kind", string("bootstrap")), ("offer", offer(10))]);
    let response = dispatcher
        .dispatch(
            caller.clone(),
            request,
            Some(IpcEventSink::new(Channel::new(|_| Ok(())))),
        )
        .await
        .into_wire();
    assert_eq!(response["kind"], "bootstrap");
    assert_eq!(
        response["bootstrap"]["versions"]["ipcProtocol"]["selected"]["value"],
        10
    );
    let bootstrap = &response["bootstrap"];
    let v10_grammar = dispatcher
        .dispatch(
            caller.clone(),
            route(
                bootstrap,
                "gatt.ordered.status",
                "v10-real-grammar",
                serde_json::json!({"orderedHandle":"missing"}),
            ),
            None,
        )
        .await
        .into_wire();
    assert_ne!(
        v10_grammar["error"]["operation"],
        "tauri.ordered.ipc-v9-required"
    );

    // Existing owners remain readable while new admission is guarded. A bad
    // or oversized record is replayed until a client explicitly acknowledges it.
    for (handle, wire) in [
        (
            "malformed",
            serde_json::json!({"kind":"failure-observation", "unexpected":true}),
        ),
        (
            "oversized",
            serde_json::json!({"kind":"value", "bytes":vec![0u8; 70_000]}),
        ),
    ] {
        let mut delivery = ordered_delivery::OrderedDelivery::default();
        delivery
            .retain_native(1, IpcValue::from_wire(wire.clone()).unwrap())
            .unwrap();
        let group = ordered_groups::OrderedGroup {
            peer_id: "peer".into(),
            collection_id: 1,
            connection_handle: "connection".into(),
            open_response: None,
            delivery,
            pending_native_id: Some(1),
            source_closed: false,
            released: false,
            core_acknowledged: false,
            core_forgotten: false,
            last_status: None,
            terminal_status: None,
            terminal_status_pending: false,
            member_ids: vec![],
            public_connection: "c".into(),
            public_database: "d".into(),
            orphan: Default::default(),
        };
        dispatcher
            .inner
            .lock()
            .await
            .callers
            .get_mut(&caller_key(&caller))
            .unwrap()
            .ordered_groups
            .insert(handle.into(), Arc::new(tokio::sync::Mutex::new(group)));
        for attempt in 0..2 {
            let reply = dispatcher
                .dispatch(
                    caller.clone(),
                    route(
                        bootstrap,
                        "gatt.ordered.next",
                        &format!("{handle}-{attempt}"),
                        serde_json::json!({"orderedHandle":handle}),
                    ),
                    None,
                )
                .await
                .into_wire();
            assert_eq!(reply["payload"]["deliveryId"], "1");
            assert_eq!(reply["payload"]["nativeWire"], wire);
        }
    }

    // Seed a sealed v2 receipt to exercise the real route while admission is
    // disabled.
    let observation = serde_json::json!({"kind":"failure-observation", "failure":null, "sourceRejection":{"kind":"fence", "membership":{"member":"1", "epoch":"2"}}, "sourceFailureMember":null, "rejectedLoss":null, "tailUncertain":true});
    let receipt = serde_json::json!({"format":"public", "receiptVersion":2, "receiptId":"sealed", "values":"0", "bytes":"0", "controls":"3", "uncertainTail":true, "terminal":null, "losses":[], "failureObservations":[observation.clone(), observation], "failureObservationsOmitted":[{"kind":"failure-observations-omitted", "count":null}]});
    dispatcher
        .inner
        .lock()
        .await
        .ordered_orphans
        .push(ordered_groups::OrphanSlot {
            owner: caller_key(&caller),
            id: "sealed".into(),
            receipt: Some(IpcValue::from_wire(receipt.clone()).unwrap()),
            page_start: 0,
            page_end: None,
            page_done: false,
        });
    let invoke = |command: &str, correlation: &str, payload: serde_json::Value| {
        route(bootstrap, command, correlation, payload)
    };
    let early = dispatcher
        .dispatch(
            caller.clone(),
            invoke(
                "gatt.ordered.orphan-ack",
                "early",
                serde_json::json!({"receiptId":"sealed"}),
            ),
            None,
        )
        .await
        .into_wire();
    assert_eq!(early["error"]["code"], "lifecycle.invalid-state");
    let page = dispatcher
        .dispatch(
            caller.clone(),
            invoke(
                "gatt.ordered.orphan-page",
                "page",
                serde_json::json!({"receiptId":"sealed", "offset":0}),
            ),
            None,
        )
        .await
        .into_wire();
    let chunk = page["payload"]["chunk"].as_str().unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(chunk).unwrap(),
        receipt
    );
    let offset = page["payload"]["nextOffset"].as_u64().unwrap();
    let done = dispatcher
        .dispatch(
            caller.clone(),
            invoke(
                "gatt.ordered.orphan-page",
                "done",
                serde_json::json!({"receiptId":"sealed", "offset":offset}),
            ),
            None,
        )
        .await
        .into_wire();
    assert_eq!(done["payload"]["done"], true);
    let replay = dispatcher
        .dispatch(
            caller.clone(),
            invoke(
                "gatt.ordered.orphan-page",
                "replay",
                serde_json::json!({"receiptId":"sealed", "offset":0}),
            ),
            None,
        )
        .await
        .into_wire();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(replay["payload"]["chunk"].as_str().unwrap())
            .unwrap(),
        receipt
    );
    let replay_done = dispatcher
        .dispatch(
            caller.clone(),
            invoke(
                "gatt.ordered.orphan-page",
                "replay-done",
                serde_json::json!({"receiptId":"sealed", "offset":offset}),
            ),
            None,
        )
        .await
        .into_wire();
    assert_eq!(replay_done["payload"]["done"], true);
    let ack = dispatcher
        .dispatch(
            caller.clone(),
            invoke(
                "gatt.ordered.orphan-ack",
                "ack",
                serde_json::json!({"receiptId":"sealed"}),
            ),
            None,
        )
        .await
        .into_wire();
    assert_eq!(ack["payload"]["state"], "forgotten");
    assert!(dispatcher.inspect_ordered_orphans(&caller).await.is_empty());
}
