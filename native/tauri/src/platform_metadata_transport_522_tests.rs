use super::*;
use ubm_core::contracts::BleErrorDomain;
use ubm_desktop::errors::PlatformMetadataEntry;

#[test]
fn tauri_dispatch_error_preserves_ordered_entries_and_safe_map() {
    let mut platform = PlatformDetail::new("native", "failed")
        .with_metadata("same", PlatformValue::Text("last".into()))
        .with_metadata("constructor", PlatformValue::Bool(true));
    platform.metadata_entries = Some(vec![
        PlatformMetadataEntry {
            key: "same".into(),
            value: PlatformValue::Int(i64::MIN),
        },
        PlatformMetadataEntry {
            key: "same".into(),
            value: PlatformValue::Text("é".into()),
        },
        PlatformMetadataEntry {
            key: "constructor".into(),
            value: PlatformValue::Bool(true),
        },
        PlatformMetadataEntry {
            key: "same".into(),
            value: PlatformValue::Int(i64::MAX),
        },
    ]);
    let error =
        ubm_desktop::DesktopError::new(BleErrorCode::GattReadFailed, BleErrorDomain::Gatt, "read")
            .with_platform(platform);
    let value = DispatchError::from_core(&error).into_response().into_wire();
    let detail = &value["error"]["platform"];
    assert_eq!(detail["metadata"]["same"], "last");
    assert!(detail["metadata"].get("constructor").is_none());
    assert_eq!(
        detail["metadataEntries"],
        serde_json::json!([
            {"key":"same","value":{"kind":"int","value":"-9223372036854775808"}},
            {"key":"same","value":{"kind":"text","value":"é"}},
            {"key":"constructor","value":{"kind":"bool","value":true}},
            {"key":"same","value":{"kind":"int","value":"9223372036854775807"}}
        ])
    );
}

#[test]
fn tauri_distinguishes_empty_entries_from_absent() {
    let mut platform = PlatformDetail::new("native", "failed");
    let absent = native_platform_wire(&platform, None).into_wire();
    assert!(absent.get("metadataEntries").is_none());
    platform.metadata_entries = Some(vec![]);
    let empty = native_platform_wire(&platform, None).into_wire();
    assert_eq!(empty["metadataEntries"], serde_json::json!([]));
}

#[test]
fn ordered_status_preserves_nested_cleanup_failures() {
    let nested = ubm_desktop::DesktopError::new(
        BleErrorCode::PlatformFailure,
        BleErrorDomain::Gatt,
        "cleanup.nested",
    )
    .with_platform(PlatformDetail::new("nested", "N"));
    let mut platform = PlatformDetail::new("native", "E");
    platform.cleanup.push(PlatformDetail::new("cleanup", "C"));
    platform.cleanup_failures.push(nested);
    let source = ubm_desktop::DesktopError::new(
        BleErrorCode::PlatformFailure,
        BleErrorDomain::Gatt,
        "source",
    )
    .with_platform(platform);
    let value = ordered_groups::normalized_error(&source).unwrap();
    assert_eq!(value["orderedFailure"]["platformCleanup"][0]["code"], "C");
    assert_eq!(
        value["orderedFailure"]["platformCleanupFailures"][0]["operation"],
        "cleanup.nested"
    );
}
