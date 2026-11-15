//! Ordered wire vectors from an independent scripted session.

use super::*;
use std::sync::{Arc, Mutex};
use ubm_core::ordered_notifications::{LossScope, LossUnit};
use ubm_mobile::{OrderedRecord, OrderedRegistration, OrderedSourceGeneration};

async fn value(
    r: &mut Recorder,
    session: &ubm_mobile::MobileSession,
    name: &str,
    op: &str,
    args: Value,
) -> Value {
    let envelope = r.invoke(session, name, op, args).await;
    assert_eq!(envelope["ok"], true, "{name}: {envelope}");
    envelope["value"].clone()
}

async fn unsupported(
    r: &mut Recorder,
    session: &ubm_mobile::MobileSession,
    name: &str,
    op: &str,
    args: Value,
) {
    let envelope = r.invoke(session, name, op, args).await;
    assert_eq!(envelope["ok"], false, "{name}: {envelope}");
    assert_eq!(
        envelope["error"]["code"], "capability.unsupported",
        "{name}: {envelope}"
    );
}

pub(super) async fn generate(r: &mut Recorder) {
    let member = OrderedRegistration {
        member_id: 0,
        source_member: 11,
        epoch: 2,
    };
    let records = Arc::new(Mutex::new(vec![
        OrderedRecord::Loss {
            scope: LossScope::Unknown,
            unit: LossUnit::Values,
            count: Some(2),
            byte_count: None,
        },
        OrderedRecord::Loss {
            scope: LossScope::Unknown,
            unit: LossUnit::Values,
            count: Some(3),
            byte_count: None,
        },
    ]));
    let source = Arc::clone(&records);
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => Reply::Now(RadioCompletion::DiscoveredScoped {
            services: writable_services(),
            identity: ubm_desktop::boundary::GattSnapshotIdentity {
                owner: "golden-graph".into(),
                attachment: 1,
                revision: 1,
            },
        }),
        RadioRequest::OrderedOpen { .. } => Reply::Now(RadioCompletion::OrderedOpened {
            generation: OrderedSourceGeneration::Counter(7),
            members: vec![member],
        }),
        RadioRequest::OrderedRetainClaim {
            generation, member, ..
        } => Reply::Now(RadioCompletion::OrderedClaim {
            generation: generation.clone(),
            member: *member,
        }),
        RadioRequest::OrderedEnableNotifications { .. } => Reply::Now(
            RadioCompletion::NotifyEnabled(ubm_desktop::ObservedDelivery::Notification),
        ),
        RadioRequest::OrderedDrain { .. } => {
            let records = std::mem::take(&mut *source.lock().unwrap());
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: (!records.is_empty()).then_some(1),
                records,
            })
        }
        _ => responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("golden-ordered").unwrap();
    let connected = ok(&call(
        &session,
        "connection.connect",
        &json!({"peerId":POLAR,"lease":"lease-1","operationId":"ordered-connect"}).to_string(),
    )
    .await);
    let discovered = ok(&call(
        &session,
        "gatt.discover",
        &json!({"peerId":POLAR,"lease":"lease-1","operationId":"ordered-discover"}).to_string(),
    )
    .await);

    value(
        r,
        &session,
        "ordered version",
        "gatt.ordered.version",
        json!({}),
    )
    .await;
    let metadata_version = value(
        r,
        &session,
        "ordered failure metadata version",
        "gatt.ordered.failure-metadata.version",
        json!({}),
    )
    .await;
    assert_eq!(metadata_version, json!({"versions": [1]}));
    let capacity_version = value(
        r,
        &session,
        "ordered capacity version",
        "gatt.ordered.capacity.version",
        json!({}),
    )
    .await;
    assert_eq!(
        capacity_version,
        json!({"wireVersion": 1, "maximumMaxBytes": 4_194_304})
    );
    value(
        r,
        &session,
        "ordered marker version",
        "gatt.ordered.marker.version",
        json!({}),
    )
    .await;
    value(
        r,
        &session,
        "ordered recording reclaim version",
        "gatt.ordered.recording-reclaim.version",
        json!({}),
    )
    .await;
    let reserved = value(
        r,
        &session,
        "ordered reserve",
        "gatt.ordered.reserve",
        json!({"peerId":POLAR}),
    )
    .await;
    let id = reserved["collectionId"].as_str().unwrap();
    let capability = reserved["reclaimCapability"].as_str().unwrap();
    let group = json!({"peerId":POLAR,"collectionId":id});
    let reclaim = json!({"peerId":POLAR,"collectionId":id,"reclaimCapability":capability});
    let reclaimed = value(
        r,
        &session,
        "ordered reclaim while owned",
        "gatt.ordered.reclaim",
        reclaim.clone(),
    )
    .await;
    assert!(reclaimed.is_null());
    value(r, &session, "ordered open", "gatt.ordered.open", json!({
        "wireVersion":2,"peerId":POLAR,"collectionId":id,
        "connectionGeneration":connected["connectionGeneration"],"databaseGeneration":discovered["databaseGeneration"],
        "publicConnectionGeneration":"public-connection","publicDatabaseGeneration":"public-database",
        "members":[{"memberId":"0","publicMemberId":"hr","selector":selector(),"requestedDelivery":"prefer-notification"}],
        "maxItems":8,"maxBytes":128,"maxControls":8,"operationId":"ordered-open"
    })).await;
    let status = value(
        r,
        &session,
        "ordered status",
        "gatt.ordered.status",
        group.clone(),
    )
    .await;
    let generation = status["sourceGeneration"]
        .as_object()
        .expect("opened ordered group has a source generation")
        .clone();
    let capacity = value(
        r,
        &session,
        "ordered increase capacity",
        "gatt.ordered.increase-capacity",
        json!({
            "peerId": POLAR,
            "collectionId": id,
            "sourceGeneration": generation,
            "maxBytes": 256,
            "wireVersion": 1,
        }),
    )
    .await;
    assert_eq!(capacity["previousMaxBytes"], json!(128));
    assert_eq!(capacity["maxBytes"], json!(256));
    assert_eq!(capacity["maximumMaxBytes"], json!(4_194_304));
    host.ingest(RadioIngress::OrderedWake {
        generation: OrderedSourceGeneration::Counter(7),
        group_id: id.parse().unwrap(),
    });
    let delivery = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let poll = ok(&call(&session, "gatt.ordered.poll", &group.to_string()).await);
            if !poll["deliveryId"].is_null() {
                break poll;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("ordered source delivered");
    let poll = value(
        r,
        &session,
        "ordered poll",
        "gatt.ordered.poll",
        group.clone(),
    )
    .await;
    assert_eq!(poll, delivery);
    let delivery_args = json!({"peerId":POLAR,"collectionId":id,"deliveryId":poll["deliveryId"]});
    let refusal = r
        .invoke(
            &session,
            "ordered discard while live",
            "gatt.ordered.discard",
            delivery_args.clone(),
        )
        .await;
    assert_eq!(refusal["error"]["code"], "lifecycle.invalid-state");
    let polled_receipt = value(
        r,
        &session,
        "ordered poll discard",
        "gatt.ordered.poll-discard",
        group.clone(),
    )
    .await;
    assert!(polled_receipt.is_null());
    r.invoke(
        &session,
        "ordered ack absent discard",
        "gatt.ordered.ack-discard",
        delivery_args.clone(),
    )
    .await;
    value(
        r,
        &session,
        "ordered ack delivery",
        "gatt.ordered.ack-delivery",
        delivery_args,
    )
    .await;

    let marker = json!({"peerId":POLAR,"collectionId":id,"handle":"1"});
    unsupported(
        r,
        &session,
        "ordered marker reserve unsupported",
        "gatt.ordered.marker.reserve",
        json!({"peerId":POLAR,"collectionId":id,"reservationId":"golden-marker"}),
    )
    .await;
    unsupported(r, &session, "ordered marker start unsupported", "gatt.ordered.marker.start", json!({"peerId":POLAR,"collectionId":id,"handle":"1","selector":selector(),"valueB64":"AQ==","mode":"with-response"})).await;
    for action in ["status", "cancel", "forget"] {
        unsupported(
            r,
            &session,
            &format!("ordered marker {action} unsupported"),
            &format!("gatt.ordered.marker.{action}"),
            marker.clone(),
        )
        .await;
    }
    let reclaim_next = json!({"peerId":POLAR,"collectionId":id,"recordingId":"golden-recording","reclaimCapability":capability});
    unsupported(
        r,
        &session,
        "ordered recording reclaim next unsupported",
        "gatt.ordered.recording-reclaim.next",
        reclaim_next.clone(),
    )
    .await;
    let mut reclaim_request = reclaim_next;
    reclaim_request["requestId"] = json!("1");
    reclaim_request["callerKey"] = json!("golden-caller");
    for action in [
        "reserve",
        "start",
        "status",
        "commit",
        "cancel",
        "release",
        "retire",
        "ack-retire",
        "forget-retire",
        "ack-forget",
    ] {
        unsupported(
            r,
            &session,
            &format!("ordered recording reclaim {action} unsupported"),
            &format!("gatt.ordered.recording-reclaim.{action}"),
            reclaim_request.clone(),
        )
        .await;
    }

    // A second, never-opened reservation has a synchronous release path.
    let cleanup = value(
        r,
        &session,
        "ordered cleanup reserve",
        "gatt.ordered.reserve",
        json!({"peerId":POLAR}),
    )
    .await;
    let cleanup_id = cleanup["collectionId"].as_str().unwrap();
    let cleanup_group = json!({"peerId":POLAR,"collectionId":cleanup_id});
    value(
        r,
        &session,
        "ordered close",
        "gatt.ordered.close",
        json!({"peerId":POLAR,"collectionId":cleanup_id,"operationId":"ordered-close"}),
    )
    .await;
    value(
        r,
        &session,
        "ordered ack group",
        "gatt.ordered.ack-group",
        cleanup_group,
    )
    .await;
    value(r, &session, "ordered forget group", "gatt.ordered.forget-group", json!({"peerId":POLAR,"collectionId":cleanup_id,"reclaimCapability":cleanup["reclaimCapability"]})).await;
}
