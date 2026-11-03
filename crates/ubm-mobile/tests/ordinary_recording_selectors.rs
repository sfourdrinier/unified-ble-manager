//! Multiple selectors share one peer-bound journal and retain their identity.
mod common;
use common::*;
use serde_json::json;
use ubm_mobile::{MobilePlatform, RadioCompletion, RadioIngress, RadioRequest};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_recording_contains_independent_selectors() {
    const SECOND: &str = "00002a38-0000-1000-8000-00805f9b34fb";
    let directory =
        std::env::temp_dir().join(format!("ubm-recorded-selectors-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let radio = Scripted::new(Box::new(|request| match request {
        RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            let mut second = services[0].characteristics[0].clone();
            second.uuid = SECOND.into();
            services[0].characteristics.push(second);
            Reply::Now(RadioCompletion::Discovered(services))
        }
        request => polar_responder(request),
    }));
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    host.continuation()
        .configure_recording_directory(&directory)
        .unwrap();
    let session = host.open_session("selectors").unwrap();
    for (op, id) in [
        ("connection.connect", "connect"),
        ("gatt.discover", "discover"),
    ] {
        ok(&call(
            &session,
            op,
            &json!({"peerId":POLAR,"lease":"lease","operationId":id}).to_string(),
        )
        .await);
    }
    for (id, characteristic) in [("first", "2A37"), ("second", "2A38")] {
        let mut path = selector();
        path["characteristicUuid"] = json!(characteristic);
        ok(&call(&session, "gatt.subscribe-recorded", &json!({"peerId":POLAR,"selector":path,"consumer":id,
            "operationId":format!("subscribe-{id}"),"recording":{"id":"selectors","maxBytes":1048576,"maxRecords":100}}).to_string()).await);
    }
    let routes: Vec<_> = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| match request {
            RadioRequest::EnableNotifications {
                instance, epoch, ..
            } => Some((instance.clone(), *epoch)),
            _ => None,
        })
        .collect();
    for (characteristic, value) in [(HR_MEASUREMENT, 1), (SECOND, 2)] {
        let (instance, epoch) = routes
            .iter()
            .find(|(instance, _)| instance.characteristic_uuid == characteristic)
            .unwrap();
        host.ingest(RadioIngress::Notification {
            instance: instance.clone(),
            epoch: *epoch,
            value: vec![value],
        });
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while ok(&host.continuation_recording_status("selectors"))["records"]
            .as_u64()
            .unwrap()
            < 4
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let batch = ok(&host.continuation_recording_prepare("selectors", 100, 65536));
    let rows = batch["records"].as_array().unwrap();
    for (characteristic, value) in [(HR_MEASUREMENT, "AQ=="), (SECOND, "Ag==")] {
        assert!(rows.iter().any(
            |row| row["metadata"]["consumer"]["selector"]["characteristicUuid"] == characteristic
                && row["record"]["valueB64"] == value
        ));
    }
    ok(&call(&session, "session.dispose", "{}").await);
    host.shutdown().await;
    std::fs::remove_dir_all(directory).unwrap();
}
