#![cfg(test)]

use super::*;
use ubm_desktop::errors::PlatformMetadataEntry;

#[test]
fn napi_error_preserves_platform_entries_without_unsafe_map_keys() {
    let mut platform = PlatformDetail::new("native", "failed")
        .with_metadata("same", PlatformValue::Text("last".into()))
        .with_metadata("__proto__", PlatformValue::Text("unsafe".into()));
    platform.metadata_entries = Some(vec![
        PlatformMetadataEntry {
            key: "same".into(),
            value: PlatformValue::Int(i64::MIN),
        },
        PlatformMetadataEntry {
            key: "same".into(),
            value: PlatformValue::Text("|é".into()),
        },
        PlatformMetadataEntry {
            key: "__proto__".into(),
            value: PlatformValue::Bool(true),
        },
        PlatformMetadataEntry {
            key: "same".into(),
            value: PlatformValue::Int(i64::MAX),
        },
    ]);
    let error = DesktopError::new(BleErrorCode::GattReadFailed, BleErrorDomain::Gatt, "read")
        .with_platform(platform);
    let wire = to_napi(DispatchError::from(error)).reason;
    let fields: Vec<_> = wire.split('|').collect();
    assert_eq!(fields.len(), 7);
    let value: serde_json::Value = serde_json::from_str(fields[5]).unwrap();
    assert_eq!(value["metadata"]["same"], "last");
    assert!(value["metadata"].get("__proto__").is_none());
    assert_eq!(
        value["metadataEntries"],
        serde_json::json!([
            {"key":"same","value":{"kind":"int","value":"-9223372036854775808"}},
            {"key":"same","value":{"kind":"text","value":"|é"}},
            {"key":"__proto__","value":{"kind":"bool","value":true}},
            {"key":"same","value":{"kind":"int","value":"9223372036854775807"}}
        ])
    );
}

#[test]
fn napi_error_distinguishes_empty_entries_from_absent() {
    let mut platform = PlatformDetail::new("native", "failed");
    let absent: serde_json::Value = serde_json::from_str(&platform_wire(&platform)).unwrap();
    assert!(absent.get("metadataEntries").is_none());
    platform.metadata_entries = Some(vec![]);
    let empty: serde_json::Value = serde_json::from_str(&platform_wire(&platform)).unwrap();
    assert_eq!(empty["metadataEntries"], serde_json::json!([]));
}
