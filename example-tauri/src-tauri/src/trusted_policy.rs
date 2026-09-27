use tauri_plugin_unified_ble_manager::BluezConnectionPolicy;

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
