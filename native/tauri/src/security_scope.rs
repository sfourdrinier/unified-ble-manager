use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Trusted Tauri permission atoms for security-sensitive operations.
/// The webview cannot supply this value; Tauri injects it from its capability.
/// Scope objects use the shipped TOML shape `{ operation: "state" }`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
pub(crate) enum SecurityPermission {
    State,
    Pair,
    CancelPairing,
    Unpair,
    CustomCeremony,
}

impl SecurityPermission {
    #[allow(dead_code)]
    pub(crate) fn operation_name(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Pair => "pair",
            Self::CancelPairing => "cancel-pairing",
            Self::Unpair => "unpair",
            Self::CustomCeremony => "custom-ceremony",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_tauri_permissions_and_reference_capability_deserialize_the_real_scope() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let output = std::env::temp_dir().join(format!(
            "ubm-security-scope-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&output).unwrap();
        let permissions = tauri::utils::acl::build::define_permissions(
            root.join("permissions/security.toml").to_str().unwrap(),
            "ubm-test-security",
            &output,
            |_| true,
        )
        .unwrap();
        std::fs::remove_dir_all(&output).unwrap();
        let expected = [
            SecurityPermission::State,
            SecurityPermission::Pair,
            SecurityPermission::CancelPairing,
            SecurityPermission::Unpair,
            SecurityPermission::CustomCeremony,
        ];
        let parsed = permissions
            .into_iter()
            .flat_map(|file| file.permission)
            .map(|permission| {
                let values = permission.scope.allow.unwrap();
                assert_eq!(values.len(), 1);
                serde_json::from_value::<SecurityPermission>(
                    serde_json::to_value(&values[0]).unwrap(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(parsed, expected);
        let capability: tauri::utils::acl::capability::Capability = serde_json::from_str(
            include_str!("../../../example-tauri/src-tauri/capabilities/main.json"),
        )
        .unwrap();
        for permission in &expected[..4] {
            assert!(capability
                .permissions
                .iter()
                .any(|entry| entry.identifier().as_ref()
                    == format!(
                        "unified-ble-manager:allow-security-{}",
                        permission.operation_name()
                    )));
        }
        assert!(serde_json::from_value::<SecurityPermission>(
            serde_json::json!({"operation":"unknown"})
        )
        .is_err());
    }
}
