mod common;
#[path = "../../test-support/recording_fixture.rs"]
mod recording_fixture;
use recording_fixture::{complete_fixture_process, isolated_fixture_process};
use serde_json::json;
use std::sync::Arc;
use ubm_desktop::continuation_journal::{JournalQuota, JournalRegistry};
use ubm_mobile::{HostOptions, MobileHost, MobilePlatform};

#[tokio::test]
async fn offline_recording_cursor_needs_no_radio_and_is_shared_by_later_host() {
    let Some(directory) = isolated_fixture_process(
        "offline_recording_cursor_needs_no_radio_and_is_shared_by_later_host",
    ) else {
        return;
    };
    let registry = Arc::new(JournalRegistry::default());
    registry.configure_directory(&directory).unwrap();
    registry
        .open(
            "test",
            &json!({}),
            JournalQuota {
                max_bytes: 1048576,
                max_records: 100,
            },
        )
        .unwrap()
        .append(
            &json!({}),
            &json!({"t":"value","consumer":"c","valueB64":"AQ==","delivery":"notification"}),
        )
        .unwrap();
    let prepared: serde_json::Value =
        serde_json::from_str(&ubm_mobile::continuation::recording_control(
            &registry, None, "prepare", "test", "", 10, 65536,
        ))
        .unwrap();
    assert_eq!(prepared["ok"], true);
    assert_eq!(prepared["value"]["records"].as_array().unwrap().len(), 1);
    let radio = common::Scripted::polar();
    assert!(radio.requests.lock().unwrap().is_empty());
    let host = Arc::new(
        MobileHost::open_with_recording_registry(
            radio.clone(),
            Arc::new(common::Wakes::default()),
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "late".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
            registry.clone(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    assert!(Arc::ptr_eq(
        &registry,
        &host.continuation().recording_registry()
    ));
    let token = prepared["value"]["token"].as_str().unwrap();
    let acknowledged: serde_json::Value =
        serde_json::from_str(&host.continuation_recording_acknowledge("test", token)).unwrap();
    assert_eq!(acknowledged["ok"], true);
    let status: serde_json::Value = serde_json::from_str(
        &ubm_mobile::continuation::recording_control(&registry, None, "status", "test", "", 0, 0),
    )
    .unwrap();
    assert_eq!(status["value"]["records"], 0);
    host.shutdown().await;
    drop(host);
    drop(registry);
    complete_fixture_process(&directory);
}
