//! WinRT facts as pure data (no WinRT calls): peer addresses, pairing and
//! unpairing results, adapter selection and presence, and the deployment
//! diagnostic. Compiled on every host so
//! the Windows adapter's translation rules are tested everywhere; the WinRT
//! calls themselves live in `os::windows`.
//!
//! Sources: `Windows.Devices.Enumeration.DevicePairingResultStatus` /
//! `DeviceUnpairingResultStatus`; the legacy addon
//! (`native/electron/winrt/src/addon.cpp` `PairWinRtPeer`,
//! `UnpairWinRtPeer`, `ReadWinRtSecurityState`, `ReadAdapters`).

/// The 48-bit address of a btleplug WinRT peer id (`AA:BB:CC:DD:EE:FF`,
/// btleplug's `BDAddr` display).
#[must_use]
pub fn address_of_peer(peer_id: &str) -> Option<u64> {
    let octets: Vec<&str> = peer_id.split(':').collect();
    if octets.len() != 6 {
        return None;
    }
    let mut value = 0u64;
    for octet in octets {
        if octet.len() != 2 {
            return None;
        }
        value = (value << 8) | u64::from(u8::from_str_radix(octet, 16).ok()?);
    }
    Some(value)
}

/// What one `PairAsync` ended as, by `DevicePairingResultStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingStatus {
    Paired,
    AlreadyPaired,
    Cancelled,
    /// Every other status: the OS or the peer refused, named verbatim.
    Rejected(String),
}

/// Classify a `DevicePairingResultStatus` by its raw value
/// (`Paired` 0, `AlreadyPaired` 3, `PairingCanceled` 14) — the legacy
/// addon's mapping.
#[must_use]
pub fn pairing_status(raw: i32, name: &str) -> PairingStatus {
    match raw {
        0 => PairingStatus::Paired,
        3 => PairingStatus::AlreadyPaired,
        14 => PairingStatus::Cancelled,
        _ => PairingStatus::Rejected(format!("Windows pairing status {name} ({raw})")),
    }
}

/// What one `UnpairAsync` ended as, by `DeviceUnpairingResultStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnpairingStatus {
    Unpaired,
    AlreadyUnpaired,
    /// Every other status is a refusal (the legacy addon threw).
    Refused(String),
}

/// Classify a `DeviceUnpairingResultStatus` (`Unpaired` 0,
/// `AlreadyUnpaired` 1).
#[must_use]
pub fn unpairing_status(raw: i32, name: &str) -> UnpairingStatus {
    match raw {
        0 => UnpairingStatus::Unpaired,
        1 => UnpairingStatus::AlreadyUnpaired,
        _ => UnpairingStatus::Refused(format!("Windows unpairing status {name} ({raw})")),
    }
}

/// Adapter authorization from `DeviceAccessInformation.CurrentStatus()`
/// (`DeviceAccessStatus`: `Unspecified` 0, `Allowed` 1, `DeniedByUser` 2,
/// `DeniedBySystem` 3) — the legacy addon's `AdapterAuthorization`
/// mapping. A value outside the enum is not forced into a word.
pub fn authorization_from_access_status(
    raw: i32,
) -> Result<crate::boundary::AdapterAuthorization, String> {
    use crate::boundary::AdapterAuthorization;
    match raw {
        1 => Ok(AdapterAuthorization::Granted),
        2 => Ok(AdapterAuthorization::Denied),
        3 => Ok(AdapterAuthorization::Restricted),
        0 => Ok(AdapterAuthorization::NotDetermined),
        other => Err(format!(
            "Windows reported an unrecognized device access status {other}"
        )),
    }
}

/// One listed Windows Bluetooth adapter as selection sees it: its native
/// device id and whether it is the OS default adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedAdapter {
    pub id: String,
    pub default: bool,
}

/// Choose the adapter `wanted` names among the listed ones (legacy
/// `SelectAdapter`, `winrt-boundary.inc`: the id must be one Windows
/// enumerates). No name selects the default adapter, or the sole adapter
/// when Windows marks none; several adapters without a default are
/// `adapter.ambiguous`. A non-default adapter is selectable: the legacy
/// addon selected it for adapter state and authorization while Windows
/// kept running Bluetooth LE through its own stack.
pub fn select_listed(
    adapters: &[ListedAdapter],
    wanted: Option<&str>,
) -> Result<usize, crate::errors::DesktopError> {
    use crate::errors::DesktopError;
    use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
    if adapters.is_empty() {
        return Err(DesktopError::adapter_unavailable("adapter.select")
            .with_detail("Windows lists no Bluetooth adapter"));
    }
    match wanted {
        Some(wanted) => adapters
            .iter()
            .position(|adapter| adapter.id == wanted)
            .ok_or_else(|| {
                DesktopError::new(
                    BleErrorCode::AdapterSelectionRequired,
                    BleErrorDomain::Adapter,
                    "adapter.select",
                )
                .with_detail(format!("no Windows Bluetooth adapter has id {wanted:?}"))
            }),
        None => {
            if let Some(index) = adapters.iter().position(|adapter| adapter.default) {
                return Ok(index);
            }
            if adapters.len() == 1 {
                return Ok(0);
            }
            Err(DesktopError::new(
                BleErrorCode::AdapterAmbiguous,
                BleErrorDomain::Adapter,
                "adapter.select",
            )
            .with_detail(format!(
                "{} adapters are present and Windows marks none as default; name one ({})",
                adapters.len(),
                adapters
                    .iter()
                    .map(|adapter| adapter.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    }
}

/// Whether the selected adapter's radio may be read, by its
/// authorization (legacy `SelectAdapter`/`ReadAdapter`: the radio is
/// opened only when access is granted; otherwise the adapter is selected
/// without one and its power is not known).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioAccess {
    Read,
    /// Access denied or restricted: state is `unauthorized`.
    WithheldUnauthorized,
    /// Access not yet determined: state is `unknown`.
    WithheldUndetermined,
}

#[must_use]
pub fn radio_access(authorization: crate::boundary::AdapterAuthorization) -> RadioAccess {
    use crate::boundary::AdapterAuthorization;
    match authorization {
        AdapterAuthorization::Granted => RadioAccess::Read,
        AdapterAuthorization::Denied | AdapterAuthorization::Restricted => {
            RadioAccess::WithheldUnauthorized
        }
        AdapterAuthorization::NotDetermined => RadioAccess::WithheldUndetermined,
    }
}

/// `ERROR_INSUFFICIENT_BUFFER`: `GetCurrentPackageFullName` found a
/// package name to copy.
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
/// `APPMODEL_ERROR_NO_PACKAGE`: the process has no package identity.
const APPMODEL_ERROR_NO_PACKAGE: u32 = 15700;

/// The deployment diagnostic from `GetCurrentPackageFullName` called with
/// an empty buffer (legacy `addon.cpp` `IsPackagedProcess`). The legacy
/// addon called every other result unpackaged; here any other result is
/// reported as what it is, never guessed.
pub fn deployment_from_status(raw: u32) -> Result<crate::boundary::HostDeployment, String> {
    use crate::boundary::HostDeployment;
    match raw {
        ERROR_INSUFFICIENT_BUFFER => Ok(HostDeployment::Packaged),
        APPMODEL_ERROR_NO_PACKAGE => Ok(HostDeployment::Unpackaged),
        other => Err(format!(
            "GetCurrentPackageFullName returned the unexpected status {other}"
        )),
    }
}

/// What a change in the selected adapter's presence means for the radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceChange {
    Lost,
    Restored,
}

/// One `DeviceWatcher` report on the Bluetooth adapter device selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceReport {
    Added(String),
    Removed(String),
    EnumerationCompleted,
}

/// The selected adapter's presence, from the `DeviceWatcher` over
/// `BluetoothAdapter::GetDeviceSelector()`. The watcher first enumerates
/// what exists (`Added` then `EnumerationCompleted`): an adapter missing
/// from that enumeration was lost between selection and the watch.
/// Afterwards `Removed` is a loss and `Added` a return; repeated reports
/// of the same fact change nothing, and other adapters are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterPresence {
    selected: String,
    present: bool,
    enumerated: bool,
}

impl AdapterPresence {
    #[must_use]
    pub fn new(selected: impl Into<String>) -> Self {
        Self {
            selected: selected.into(),
            present: false,
            enumerated: false,
        }
    }

    pub fn observe(&mut self, report: PresenceReport) -> Option<PresenceChange> {
        match report {
            PresenceReport::EnumerationCompleted => {
                if self.enumerated {
                    return None;
                }
                self.enumerated = true;
                (!self.present).then_some(PresenceChange::Lost)
            }
            PresenceReport::Added(id) if id == self.selected => {
                let was_present = std::mem::replace(&mut self.present, true);
                (self.enumerated && !was_present).then_some(PresenceChange::Restored)
            }
            PresenceReport::Removed(id) if id == self.selected => {
                let was_present = std::mem::replace(&mut self.present, false);
                (self.enumerated && was_present).then_some(PresenceChange::Lost)
            }
            PresenceReport::Added(_) | PresenceReport::Removed(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AdapterPresence, ListedAdapter, PairingStatus, PresenceChange, PresenceReport, RadioAccess,
        UnpairingStatus, address_of_peer, deployment_from_status, pairing_status, radio_access,
        select_listed, unpairing_status,
    };
    use crate::boundary::{AdapterAuthorization, HostDeployment};
    use ubm_core::contracts::BleErrorCode;

    #[test]
    fn peer_ids_parse_to_48_bit_addresses() {
        assert_eq!(address_of_peer("AA:BB:CC:DD:EE:FF"), Some(0xAABB_CCDD_EEFF));
        assert_eq!(address_of_peer("00:00:00:00:00:01"), Some(1));
        assert_eq!(address_of_peer("AA:BB:CC:DD:EE"), None);
        assert_eq!(address_of_peer("AA:BB:CC:DD:EE:GG"), None);
        assert_eq!(address_of_peer("hci0/dev_AA_BB"), None);
    }

    #[test]
    fn pairing_statuses_follow_the_legacy_addon() {
        assert_eq!(pairing_status(0, "Paired"), PairingStatus::Paired);
        assert_eq!(
            pairing_status(3, "AlreadyPaired"),
            PairingStatus::AlreadyPaired
        );
        assert_eq!(
            pairing_status(14, "PairingCanceled"),
            PairingStatus::Cancelled
        );
        assert!(matches!(
            pairing_status(8, "RejectedByHandler"),
            PairingStatus::Rejected(reason) if reason.contains("RejectedByHandler")
        ));
        assert_eq!(unpairing_status(0, "Unpaired"), UnpairingStatus::Unpaired);
        assert_eq!(
            unpairing_status(1, "AlreadyUnpaired"),
            UnpairingStatus::AlreadyUnpaired
        );
        assert!(matches!(
            unpairing_status(4, "Failed"),
            UnpairingStatus::Refused(_)
        ));
    }

    #[test]
    fn device_access_statuses_follow_the_legacy_addon() {
        use super::authorization_from_access_status;
        use crate::boundary::AdapterAuthorization;

        assert_eq!(
            authorization_from_access_status(1),
            Ok(AdapterAuthorization::Granted)
        );
        assert_eq!(
            authorization_from_access_status(2),
            Ok(AdapterAuthorization::Denied)
        );
        assert_eq!(
            authorization_from_access_status(3),
            Ok(AdapterAuthorization::Restricted)
        );
        assert_eq!(
            authorization_from_access_status(0),
            Ok(AdapterAuthorization::NotDetermined)
        );
        assert!(authorization_from_access_status(7).is_err());
    }

    fn listed(entries: &[(&str, bool)]) -> Vec<ListedAdapter> {
        entries
            .iter()
            .map(|(id, default)| ListedAdapter {
                id: (*id).to_owned(),
                default: *default,
            })
            .collect()
    }

    #[test]
    fn a_named_adapter_is_selected_even_when_it_is_not_the_default() {
        let adapters = listed(&[("usb-dongle", false), ("built-in", true)]);
        assert_eq!(select_listed(&adapters, Some("usb-dongle")).ok(), Some(0));
        assert_eq!(select_listed(&adapters, Some("built-in")).ok(), Some(1));
        let unknown = select_listed(&adapters, Some("gone")).unwrap_err();
        assert_eq!(unknown.code(), BleErrorCode::AdapterSelectionRequired);
    }

    #[test]
    fn no_name_selects_the_default_then_a_sole_adapter_and_never_guesses() {
        let adapters = listed(&[("usb-dongle", false), ("built-in", true)]);
        assert_eq!(select_listed(&adapters, None).ok(), Some(1));
        assert_eq!(
            select_listed(&listed(&[("only", false)]), None).ok(),
            Some(0)
        );
        let two = listed(&[("a", false), ("b", false)]);
        assert_eq!(
            select_listed(&two, None).unwrap_err().code(),
            BleErrorCode::AdapterAmbiguous
        );
        assert_eq!(
            select_listed(&[], None).unwrap_err().code(),
            BleErrorCode::AdapterUnavailable
        );
    }

    #[test]
    fn the_radio_is_read_only_with_granted_access() {
        assert_eq!(
            radio_access(AdapterAuthorization::Granted),
            RadioAccess::Read
        );
        assert_eq!(
            radio_access(AdapterAuthorization::Denied),
            RadioAccess::WithheldUnauthorized
        );
        assert_eq!(
            radio_access(AdapterAuthorization::Restricted),
            RadioAccess::WithheldUnauthorized
        );
        assert_eq!(
            radio_access(AdapterAuthorization::NotDetermined),
            RadioAccess::WithheldUndetermined
        );
    }

    #[test]
    fn deployment_follows_get_current_package_full_name() {
        assert_eq!(deployment_from_status(122), Ok(HostDeployment::Packaged));
        assert_eq!(
            deployment_from_status(15700),
            Ok(HostDeployment::Unpackaged)
        );
        assert!(deployment_from_status(0).is_err());
        assert!(deployment_from_status(87).is_err());
    }

    #[test]
    fn presence_reports_loss_and_return_once_after_enumeration() {
        let mut presence = AdapterPresence::new("built-in");
        assert_eq!(
            presence.observe(PresenceReport::Added("other".into())),
            None
        );
        assert_eq!(
            presence.observe(PresenceReport::Added("built-in".into())),
            None
        );
        assert_eq!(presence.observe(PresenceReport::EnumerationCompleted), None);
        assert_eq!(
            presence.observe(PresenceReport::Removed("other".into())),
            None
        );
        assert_eq!(
            presence.observe(PresenceReport::Removed("built-in".into())),
            Some(PresenceChange::Lost)
        );
        assert_eq!(
            presence.observe(PresenceReport::Removed("built-in".into())),
            None
        );
        assert_eq!(
            presence.observe(PresenceReport::Added("built-in".into())),
            Some(PresenceChange::Restored)
        );
        assert_eq!(
            presence.observe(PresenceReport::Added("built-in".into())),
            None
        );
    }

    #[test]
    fn an_adapter_missing_from_the_first_enumeration_is_lost() {
        let mut presence = AdapterPresence::new("built-in");
        assert_eq!(
            presence.observe(PresenceReport::Added("other".into())),
            None
        );
        assert_eq!(
            presence.observe(PresenceReport::EnumerationCompleted),
            Some(PresenceChange::Lost)
        );
        assert_eq!(presence.observe(PresenceReport::EnumerationCompleted), None);
        assert_eq!(
            presence.observe(PresenceReport::Added("built-in".into())),
            Some(PresenceChange::Restored)
        );
    }
}
