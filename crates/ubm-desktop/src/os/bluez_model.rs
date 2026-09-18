//! BlueZ facts as pure data (no D-Bus): object paths, adapter ids, bond
//! state, pairing error classes and characteristic flags. Compiled on every
//! host so the Linux adapter's translation rules are tested everywhere; the
//! D-Bus calls themselves live in `os::linux`.
//!
//! Sources: BlueZ `doc/org.bluez.Device.rst`, `doc/org.bluez.Adapter.rst`,
//! `doc/org.bluez.GattCharacteristic.rst`; `src/device.c` (`pair_device`,
//! `new_authentication_return`, `cancel_pairing`); the legacy TypeScript
//! backend (`src/backends/bluez/bluez-security.ts`,
//! `bluez-backend-handles.ts`).

use std::collections::HashMap;

use crate::boundary::{AdapterLossCause, BondState, CharacteristicAccess, InstanceKey};

/// Root of every BlueZ object path.
pub const BLUEZ_ROOT: &str = "/org/bluez";

/// What one D-Bus signal says about the selected BlueZ adapter (finding
/// 57; legacy `bluez-dbus-next-boundary.ts` `NameOwnerChanged` and
/// `bluez-backend-runtime.ts` `interfacesRemoved`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterSignal {
    /// Live work on the adapter is lost.
    Lost(AdapterLossCause),
    /// The adapter object is back.
    Restored,
}

/// `org.freedesktop.DBus.NameOwnerChanged(name, old, new)`: `org.bluez`
/// losing its owner (bluetoothd exited) or changing owner (bluetoothd
/// replaced) loses the adapter. A new owner alone restores nothing: the
/// adapter object's own `InterfacesAdded` does.
#[must_use]
pub fn name_owner_signal(name: &str, old_owner: &str, new_owner: &str) -> Option<AdapterSignal> {
    (name == "org.bluez" && !old_owner.is_empty() && old_owner != new_owner)
        .then_some(AdapterSignal::Lost(AdapterLossCause::DaemonRestarted))
}

/// `ObjectManager.InterfacesRemoved(path, interfaces)`: the selected
/// adapter's `Adapter1` going away loses the adapter.
#[must_use]
pub fn interfaces_removed_signal<S: AsRef<str>>(
    adapter_path: &str,
    path: &str,
    interfaces: &[S],
) -> Option<AdapterSignal> {
    (path == adapter_path
        && interfaces
            .iter()
            .any(|interface| interface.as_ref() == "org.bluez.Adapter1"))
    .then_some(AdapterSignal::Lost(AdapterLossCause::Removed))
}

/// `ObjectManager.InterfacesAdded(path, interfaces)`: the selected
/// adapter's `Adapter1` appearing restores it.
#[must_use]
pub fn interfaces_added_signal<S: AsRef<str>>(
    adapter_path: &str,
    path: &str,
    interfaces: &[S],
) -> Option<AdapterSignal> {
    (path == adapter_path
        && interfaces
            .iter()
            .any(|interface| interface.as_ref() == "org.bluez.Adapter1"))
    .then_some(AdapterSignal::Restored)
}

/// The adapter id BlueZ uses in object paths (`hci0`) from btleplug's
/// `adapter_info` label (`"hci0 (usb:v1D6Bp0246d0540)"`): its first token.
#[must_use]
pub fn adapter_id_from_info(info: &str) -> &str {
    info.split_whitespace().next().unwrap_or(info)
}

/// `/org/bluez/<adapter>` for an adapter id.
#[must_use]
pub fn adapter_path(adapter_id: &str) -> String {
    format!("{BLUEZ_ROOT}/{adapter_id}")
}

/// The D-Bus object path of a btleplug BlueZ peer id (`hci0/dev_AA_..`).
#[must_use]
pub fn device_path(peer_id: &str) -> String {
    format!("{BLUEZ_ROOT}/{peer_id}")
}

/// The btleplug BlueZ peer id of a device object path, when it is one.
#[must_use]
pub fn peer_id_for_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(BLUEZ_ROOT)?.strip_prefix('/')?;
    let mut parts = rest.split('/');
    let adapter = parts.next()?;
    let device = parts.next()?;
    (parts.next().is_none() && !adapter.is_empty() && device.starts_with("dev_")).then_some(rest)
}

/// The adapter object path a peer id lives under.
#[must_use]
pub fn adapter_path_of_peer(peer_id: &str) -> Option<String> {
    let adapter = peer_id.split('/').next()?;
    (!adapter.is_empty() && peer_id.contains("/dev_")).then(|| adapter_path(adapter))
}

/// The device object path BlueZ gives `address` (`AA:BB:..`) under an
/// adapter: `<adapter>/dev_AA_BB_..`.
#[must_use]
pub fn device_path_for_address(adapter_path: &str, address: &str) -> String {
    format!(
        "{adapter_path}/dev_{}",
        address.to_ascii_uppercase().replace(':', "_")
    )
}

/// Whether pairing is possible, as `security.state()` reports it (B-R1).
/// BlueZ has no "can pair" fact; the legacy backend's constant `true` is
/// the contract, so the Rust path reports it too — never `null`.
pub const PAIRING_POSSIBLE: bool = true;

/// Bond state from `Device1.Paired` / `Device1.Bonded`. `Bonded` (BlueZ
/// 5.66+) is the bond fact itself; older daemons expose only `Paired`,
/// which the legacy backend reported as the bond. Neither present is the
/// daemon withholding the fact.
#[must_use]
pub fn bond_state(paired: Option<bool>, bonded: Option<bool>) -> BondState {
    match (bonded, paired) {
        (Some(true), _) => BondState::Bonded,
        (Some(false), _) => BondState::NotBonded,
        (None, Some(true)) => BondState::Bonded,
        (None, Some(false)) => BondState::NotBonded,
        (None, None) => BondState::Unknown,
    }
}

/// How one failed `Device1.Pair` ended, by D-Bus error name
/// (`src/device.c` `new_authentication_return` / `pair_device`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairFailure {
    /// `AlreadyExists`: the peer is already bonded.
    AlreadyPaired,
    /// `AuthenticationCanceled`: a cancel (ours or the peer's) ended it.
    Cancelled,
    /// The peer or the daemon refused the ceremony.
    Rejected(String),
    /// `InProgress`: another pairing or connection owns the device.
    InProgress,
    /// Anything else is a platform failure, not a pairing outcome.
    Failure,
}

/// Classify a `Device1.Pair` error name.
#[must_use]
pub fn classify_pair_error(name: &str, message: &str) -> PairFailure {
    match name {
        "org.bluez.Error.AlreadyExists" => PairFailure::AlreadyPaired,
        "org.bluez.Error.AuthenticationCanceled" => PairFailure::Cancelled,
        "org.bluez.Error.AuthenticationRejected"
        | "org.bluez.Error.AuthenticationFailed"
        | "org.bluez.Error.AuthenticationTimeout"
        | "org.bluez.Error.ConnectionAttemptFailed" => {
            PairFailure::Rejected(format!("{name}: {message}"))
        }
        "org.bluez.Error.InProgress" => PairFailure::InProgress,
        _ => PairFailure::Failure,
    }
}

/// Whether a rejected `Device1.CancelPairing` still proves no pairing is in
/// flight (legacy `cancelProvesPairingAlreadyTerminal`): nothing to cancel,
/// or the device object is gone.
#[must_use]
pub fn cancel_error_proves_terminal(name: &str) -> bool {
    matches!(
        name,
        "org.bluez.Error.DoesNotExist" | "org.freedesktop.DBus.Error.UnknownObject"
    )
}

/// Whether a D-Bus error says the method does not exist (BlueZ without
/// `--experimental` for `Adapter1.ConnectDevice`).
#[must_use]
pub fn is_unknown_method(name: &str) -> bool {
    name == "org.freedesktop.DBus.Error.UnknownMethod"
}

/// Characteristic facts from `GattCharacteristic1.Flags`. BlueZ reports
/// the complete flag set, so every fact is known (`Some`).
#[must_use]
pub fn access_from_flags<S: AsRef<str>>(flags: &[S]) -> CharacteristicAccess {
    let has = |wanted: &str| Some(flags.iter().any(|flag| flag.as_ref() == wanted));
    CharacteristicAccess {
        broadcast: has("broadcast"),
        authenticated_signed_writes: has("authenticated-signed-writes"),
        extended_properties: has("extended-properties"),
        reliable_write: has("reliable-write"),
        writable_auxiliaries: has("writable-auxiliaries"),
        encrypt_read: has("encrypt-read"),
        encrypt_write: has("encrypt-write"),
        encrypt_authenticated_read: has("encrypt-authenticated-read"),
        encrypt_authenticated_write: has("encrypt-authenticated-write"),
        secure_read: has("secure-read"),
        secure_write: has("secure-write"),
        authorize: has("authorize"),
    }
}

/// One `GattCharacteristic1` object of a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluezCharacteristic {
    /// The characteristic's own object path (`.../service0010/char0012`).
    pub path: String,
    /// Owning `GattService1` object path.
    pub service_path: String,
    /// Characteristic UUID (canonical lower-case).
    pub uuid: String,
    /// `Flags` as reported.
    pub flags: Vec<String>,
    /// `MTU` when the daemon exposes it (BlueZ 5.62+).
    pub mtu: Option<u16>,
}

/// The ATT handle BlueZ encodes in a GATT object path's last segment
/// (`service%04x`, `char%04x`, `desc%04x`, BlueZ `src/gatt-client.c`): the
/// attribute instance the vendored btleplug numbers occurrences by
/// (UBM_PATCHES.md #6).
#[must_use]
pub fn gatt_handle(path: &str) -> Option<u64> {
    let segment = path.rsplit('/').next()?;
    let digits = segment.trim_start_matches(|c: char| c.is_ascii_lowercase());
    if digits.is_empty() || digits.len() == segment.len() {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

/// Rank of `handle` among `handles` (ascending): the per-UUID occurrence
/// btleplug assigns in (UUID, instance) order.
fn rank(handles: &[u64], handle: u64) -> u64 {
    handles.iter().filter(|other| **other < handle).count() as u64
}

/// Characteristic facts per instance key. The vendored btleplug numbers
/// same-UUID services and characteristics by ATT handle (UBM_PATCHES.md
/// #6), and BlueZ object paths carry those handles, so every instance —
/// duplicated UUIDs included — maps to exactly its own facts. An object
/// whose path carries no handle is left out, never attributed by guess.
#[must_use]
pub fn access_for_instances(
    peer_id: &str,
    services: &HashMap<String, String>,
    characteristics: &[BluezCharacteristic],
) -> HashMap<InstanceKey, CharacteristicAccess> {
    let mut service_handles: HashMap<&str, Vec<u64>> = HashMap::new();
    for (path, uuid) in services {
        if let Some(handle) = gatt_handle(path) {
            service_handles
                .entry(uuid.as_str())
                .or_default()
                .push(handle);
        }
    }
    let mut characteristic_handles: HashMap<(&str, &str), Vec<u64>> = HashMap::new();
    for characteristic in characteristics {
        if let Some(handle) = gatt_handle(&characteristic.path) {
            characteristic_handles
                .entry((
                    characteristic.service_path.as_str(),
                    characteristic.uuid.as_str(),
                ))
                .or_default()
                .push(handle);
        }
    }
    let mut out = HashMap::new();
    for characteristic in characteristics {
        let Some(service_uuid) = services.get(&characteristic.service_path) else {
            continue;
        };
        let (Some(service_handle), Some(handle)) = (
            gatt_handle(&characteristic.service_path),
            gatt_handle(&characteristic.path),
        ) else {
            continue;
        };
        let service_occurrence = service_handles
            .get(service_uuid.as_str())
            .map_or(0, |handles| rank(handles, service_handle));
        let characteristic_occurrence = characteristic_handles
            .get(&(
                characteristic.service_path.as_str(),
                characteristic.uuid.as_str(),
            ))
            .map_or(0, |handles| rank(handles, handle));
        out.insert(
            (
                peer_id.to_owned(),
                service_uuid.clone(),
                service_occurrence,
                characteristic.uuid.clone(),
                characteristic_occurrence,
            ),
            access_from_flags(&characteristic.flags),
        );
    }
    out
}

/// The link's ATT MTU from the characteristics' `MTU` properties: BlueZ
/// reports the same link MTU on every characteristic. `None` when the
/// daemon exposes none (then the caller keeps its own fallback).
#[must_use]
pub fn link_mtu(characteristics: &[BluezCharacteristic]) -> Option<u16> {
    characteristics.iter().filter_map(|c| c.mtu).max()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        BluezCharacteristic, PAIRING_POSSIBLE, PairFailure, access_for_instances,
        access_from_flags, adapter_id_from_info, adapter_path_of_peer, bond_state,
        cancel_error_proves_terminal, classify_pair_error, device_path, device_path_for_address,
        is_unknown_method, link_mtu, peer_id_for_path,
    };
    use crate::boundary::BondState;

    /// B-R1: `security.state().pairingPossible` is the legacy constant
    /// `true` — BlueZ has no "can pair" fact to read. The assertion is on
    /// a constant by design: it pins the legacy literal.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn pairing_is_possible_as_legacy_reported_it() {
        assert!(PAIRING_POSSIBLE);
    }

    #[test]
    fn adapter_signals_follow_the_legacy_reset_sources() {
        use super::{
            AdapterSignal, interfaces_added_signal, interfaces_removed_signal, name_owner_signal,
        };
        use crate::boundary::AdapterLossCause;
        let lost = |cause| Some(AdapterSignal::Lost(cause));
        assert_eq!(
            name_owner_signal("org.bluez", ":1.7", ""),
            lost(AdapterLossCause::DaemonRestarted),
            "bluetoothd exited"
        );
        assert_eq!(
            name_owner_signal("org.bluez", ":1.7", ":1.9"),
            lost(AdapterLossCause::DaemonRestarted),
            "bluetoothd replaced"
        );
        assert_eq!(
            name_owner_signal("org.bluez", "", ":1.9"),
            None,
            "first owner"
        );
        assert_eq!(name_owner_signal("org.other", ":1.7", ""), None);
        let adapter = "/org/bluez/hci0";
        assert_eq!(
            interfaces_removed_signal(adapter, adapter, &["org.bluez.Adapter1"]),
            lost(AdapterLossCause::Removed)
        );
        assert_eq!(
            interfaces_removed_signal(adapter, "/org/bluez/hci1", &["org.bluez.Adapter1"]),
            None,
            "another adapter"
        );
        assert_eq!(
            interfaces_removed_signal(adapter, adapter, &["org.bluez.Media1"]),
            None,
            "another interface on the adapter"
        );
        assert_eq!(
            interfaces_added_signal(adapter, adapter, &["org.bluez.Adapter1"]),
            Some(AdapterSignal::Restored)
        );
        assert_eq!(
            interfaces_added_signal(adapter, "/org/bluez/hci0/dev_AA", &["org.bluez.Device1"]),
            None
        );
    }

    #[test]
    fn paths_round_trip_between_peer_ids_and_objects() {
        assert_eq!(adapter_id_from_info("hci1 (usb:v1D6Bp0246d0540)"), "hci1");
        assert_eq!(adapter_id_from_info("hci0"), "hci0");
        assert_eq!(
            device_path("hci0/dev_AA_BB_CC_DD_EE_FF"),
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"
        );
        assert_eq!(
            peer_id_for_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"),
            Some("hci0/dev_AA_BB_CC_DD_EE_FF")
        );
        assert_eq!(peer_id_for_path("/org/bluez/hci0"), None, "an adapter");
        assert_eq!(
            peer_id_for_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service000a"),
            None,
            "a GATT object below the device"
        );
        assert_eq!(
            adapter_path_of_peer("hci1/dev_AA_BB_CC_DD_EE_FF").as_deref(),
            Some("/org/bluez/hci1")
        );
        assert_eq!(
            device_path_for_address("/org/bluez/hci0", "aa:bb:cc:dd:ee:0f"),
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_0F"
        );
    }

    #[test]
    fn bonded_wins_over_paired_and_absence_is_unknown() {
        assert_eq!(bond_state(Some(true), Some(false)), BondState::NotBonded);
        assert_eq!(bond_state(Some(true), None), BondState::Bonded);
        assert_eq!(bond_state(Some(false), None), BondState::NotBonded);
        assert_eq!(bond_state(None, Some(true)), BondState::Bonded);
        assert_eq!(bond_state(None, None), BondState::Unknown);
    }

    #[test]
    fn pair_errors_map_to_outcomes_or_failures() {
        assert_eq!(
            classify_pair_error("org.bluez.Error.AlreadyExists", "Already Paired"),
            PairFailure::AlreadyPaired
        );
        assert_eq!(
            classify_pair_error("org.bluez.Error.AuthenticationCanceled", ""),
            PairFailure::Cancelled
        );
        assert!(matches!(
            classify_pair_error("org.bluez.Error.AuthenticationRejected", "Authentication Rejected"),
            PairFailure::Rejected(reason) if reason.contains("AuthenticationRejected")
        ));
        assert_eq!(
            classify_pair_error("org.bluez.Error.InProgress", ""),
            PairFailure::InProgress
        );
        assert_eq!(
            classify_pair_error("org.freedesktop.DBus.Error.NoReply", ""),
            PairFailure::Failure
        );
        assert!(cancel_error_proves_terminal("org.bluez.Error.DoesNotExist"));
        assert!(!cancel_error_proves_terminal("org.bluez.Error.Failed"));
        assert!(is_unknown_method(
            "org.freedesktop.DBus.Error.UnknownMethod"
        ));
    }

    #[test]
    fn flags_cover_every_bluez_fact_the_legacy_backend_read() {
        let access = access_from_flags(&[
            "read",
            "broadcast",
            "reliable-write",
            "writable-auxiliaries",
            "encrypt-authenticated-write",
            "authorize",
        ]);
        assert_eq!(access.broadcast, Some(true));
        assert_eq!(access.reliable_write, Some(true));
        assert_eq!(access.writable_auxiliaries, Some(true));
        assert_eq!(access.encrypt_authenticated_write, Some(true));
        assert_eq!(access.authorize, Some(true));
        assert_eq!(
            access.authenticated_signed_writes,
            Some(false),
            "BlueZ reports the full set: absent means false, never unknown"
        );
        assert_eq!(access.encrypt_read, Some(false));
    }

    fn characteristic(
        service: &str,
        handle: u16,
        uuid: &str,
        flag: &str,
        mtu: Option<u16>,
    ) -> BluezCharacteristic {
        BluezCharacteristic {
            path: format!("{service}/char{handle:04x}"),
            service_path: service.to_owned(),
            uuid: uuid.to_owned(),
            flags: vec![flag.to_owned()],
            mtu,
        }
    }

    #[test]
    fn every_instance_receives_its_own_facts_by_handle() {
        let dev = "/org/bluez/hci0/dev_AA";
        let services = HashMap::from([
            (format!("{dev}/service0010"), "svc-a".to_owned()),
            (format!("{dev}/service0040"), "svc-b".to_owned()),
            (format!("{dev}/service0020"), "svc-b".to_owned()),
        ]);
        let characteristics = vec![
            characteristic(
                &format!("{dev}/service0010"),
                0x12,
                "char-1",
                "read",
                Some(247),
            ),
            characteristic(
                &format!("{dev}/service0010"),
                0x18,
                "char-2",
                "notify",
                None,
            ),
            characteristic(&format!("{dev}/service0010"), 0x15, "char-2", "write", None),
            characteristic(
                &format!("{dev}/service0040"),
                0x42,
                "char-3",
                "indicate",
                None,
            ),
            characteristic(
                &format!("{dev}/service0020"),
                0x22,
                "char-3",
                "broadcast",
                None,
            ),
        ];
        let access = access_for_instances("peer", &services, &characteristics);
        assert_eq!(access.len(), 5, "duplicated UUIDs included");
        let key = |service: &str, service_occurrence, uuid: &str, occurrence| {
            (
                "peer".to_owned(),
                service.to_owned(),
                service_occurrence,
                uuid.to_owned(),
                occurrence,
            )
        };
        // Same-UUID characteristics rank by handle: 0x15 before 0x18.
        assert_eq!(
            access[&key("svc-a", 0, "char-2", 0)].reliable_write,
            Some(false)
        );
        assert!(
            access[&key("svc-a", 0, "char-2", 1)] == access_from_flags(&["notify"]),
            "occurrence 1 is handle 0x18"
        );
        // Same-UUID services rank by handle: 0x20 before 0x40.
        assert_eq!(
            access[&key("svc-b", 0, "char-3", 0)].broadcast,
            Some(true),
            "service occurrence 0 is handle 0x20"
        );
        assert_eq!(access[&key("svc-b", 1, "char-3", 0)].broadcast, Some(false));
        assert_eq!(link_mtu(&characteristics), Some(247));
        assert_eq!(link_mtu(&characteristics[1..]), None);
    }

    #[test]
    fn a_path_without_a_handle_is_left_out() {
        let services = HashMap::from([("/odd/service".to_owned(), "svc".to_owned())]);
        let characteristics = vec![BluezCharacteristic {
            path: "/odd/service/char".to_owned(),
            service_path: "/odd/service".to_owned(),
            uuid: "char".to_owned(),
            flags: Vec::new(),
            mtu: None,
        }];
        assert!(access_for_instances("peer", &services, &characteristics).is_empty());
        assert_eq!(
            super::gatt_handle("/org/bluez/hci0/dev_AA/service0010/char002a"),
            Some(0x2a)
        );
        assert_eq!(
            super::gatt_handle("/org/bluez/hci0/dev_AA/service0010"),
            Some(0x10)
        );
        assert_eq!(super::gatt_handle("/odd/service"), None);
    }
}
