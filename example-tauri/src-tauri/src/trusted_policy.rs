use tauri_plugin_unified_ble_manager::BluezConnectionPolicy;

/// Preserve the trusted exact identity; native admission decides whether it exists.
pub fn adapter_id(value: Option<String>) -> Result<Option<String>, &'static str> {
    if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
        return Err("UBM_TAURI_ADAPTER requires a nonempty exact adapter identity");
    }
    Ok(value)
}

/// Trusted process environment only. Native admission validates the daemon
/// unique name; this reference never probes or substitutes an owner.
pub fn connection_policy(
    owner: Option<String>,
    linux: bool,
) -> Result<Option<BluezConnectionPolicy>, &'static str> {
    if owner.is_some() && !linux {
        return Err("UBM_BLUEZ_DAEMON_OWNER is only valid with the Linux BlueZ backend");
    }
    Ok(
        owner.map(|daemon_unique_owner| BluezConnectionPolicy::LeBearer {
            daemon_unique_owner,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_adapter_selection_is_optional_and_exact() {
        assert_eq!(adapter_id(None).unwrap(), None);
        assert_eq!(
            adapter_id(Some("hci0".into())).unwrap(),
            Some("hci0".into())
        );
        assert_eq!(
            adapter_id(Some(" exact adapter ".into())).unwrap(),
            Some(" exact adapter ".into())
        );
        assert!(adapter_id(Some(String::new())).is_err());
        assert!(adapter_id(Some(" \t\n".into())).is_err());
    }

    #[test]
    fn trusted_owner_is_explicit_and_linux_only() {
        assert!(connection_policy(None, true).unwrap().is_none());
        assert!(connection_policy(Some(":1.42".into()), false).is_err());
        assert_eq!(
            connection_policy(Some(":1.42".into()), true).unwrap(),
            Some(
                tauri_plugin_unified_ble_manager::BluezConnectionPolicy::LeBearer {
                    daemon_unique_owner: ":1.42".into(),
                }
            )
        );
    }

    #[test]
    fn native_admission_owns_unique_name_validation() {
        assert_eq!(
            connection_policy(Some("invalid".into()), true).unwrap(),
            Some(
                tauri_plugin_unified_ble_manager::BluezConnectionPolicy::LeBearer {
                    daemon_unique_owner: "invalid".into(),
                }
            )
        );
    }
}
