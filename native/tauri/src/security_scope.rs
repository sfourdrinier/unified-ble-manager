use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Trusted Tauri permission atoms for security-sensitive operations.
/// The webview cannot supply this value; Tauri injects it from its capability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
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
