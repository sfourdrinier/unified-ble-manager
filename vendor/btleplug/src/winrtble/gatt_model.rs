// UBM patch (UBM_PATCHES.md, `winrt-attribute-instances`): the WinRT GATT
// rules as pure data, with no WinRT calls. Self-contained (std only) so
// `crates/ubm-desktop/tests/winrt_gatt_model.rs` compiles this file with
// `#[path]` and runs its tests on every host, not only on Windows.

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

/// The name of a raw `GattCommunicationStatus` (`Success` 0, `Unreachable`
/// 1, `ProtocolError` 2, `AccessDenied` 3). A value outside the enum has
/// no name and is reported by number.
///
/// The WinRT I/O path uses the typed enum in `utils.rs`. This raw mapping
/// exists for the pure model, which the Windows library build does not call.
#[cfg(test)]
pub fn gatt_status_name(raw: i32) -> Option<&'static str> {
    match raw {
        0 => Some("Success"),
        1 => Some("Unreachable"),
        2 => Some("ProtocolError"),
        3 => Some("AccessDenied"),
        _ => None,
    }
}

/// What one characteristic-discovery status does to the rest of discovery.
///
/// `AccessDenied` (3) with no ATT error byte keeps the service in the
/// table with no characteristics. Missing ATT metadata does not mean
/// Windows permanently reserved the service: Heart Rate, Battery, Device
/// Information, and vendor services can answer this way. Known OS-reserved
/// UUIDs are decided by [`windows_reserves_service`] before this query.
/// A peer refusal is a `ProtocolError`, or an `AccessDenied` that still
/// carries an ATT byte, and that still fails discovery. `Unreachable` and
/// every other non-success fail it too.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CharacteristicDiscovery {
    Continue,
    /// AccessDenied with no ATT byte. The service stays, restricted.
    AccessDenied,
    Failed,
}

/// Why a service is present with no characteristics.
///
/// `Open` listed characteristics normally. `OsReserved` was not queried.
/// `AccessDenied` was queried and Windows refused it without an ATT byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceRestriction {
    Open,
    OsReserved,
    AccessDenied,
}

/// Connectable bit of one Windows advertisement type.
///
/// `0` and `1` are connectable undirected and directed. `2` and `3` are
/// scannable and non-connectable undirected. A scan response (`4`) does not
/// say, even when `is_connectable` is `Some(false)`. Extended (`5`) uses
/// that flag only when the OS returned it. Any other discriminant stays
/// unknown.
pub fn advertisement_connectable(
    advertisement_type: i32,
    is_connectable: Option<bool>,
) -> Option<bool> {
    match advertisement_type {
        0 | 1 => Some(true),
        2 | 3 => Some(false),
        4 => None,
        5 => is_connectable,
        _ => None,
    }
}

/// Windows keeps these 16-bit services for the system (HID, LE Audio, and
/// the Ranging service on this host). Asking `GetCharacteristics` for one
/// of them returns `AccessDenied` with no ATT request. Discovery leaves
/// them out before that call. Heart Rate, Battery, Device Information, and
/// vendor 128-bit services are not in this set.
pub fn windows_reserves_service(uuid: u128) -> bool {
    const BASE_LOW_96: u128 = 0x0000_1000_8000_0080_5f9b_34fb;
    if uuid & ((1_u128 << 96) - 1) != BASE_LOW_96 || (uuid >> 112) != 0 {
        return false;
    }
    matches!(
        ((uuid >> 96) & 0xffff) as u16,
        0x1812 | // Human Interface Device
        0x1843 | 0x1844 | 0x1845 | 0x1846 | // Audio Input, Volume, Volume Offset, Coordinated Set
        0x1848 | 0x1849 | // Media Control, Generic Media Control
        0x184D | 0x184E | 0x184F | // Microphone, Audio Stream, Broadcast Audio Scan
        0x1850 | 0x1851 | 0x1852 | 0x1853 | 0x1854 | 0x1855 | // Published Audio through Telephony and Media Audio
        0x1858 | // Gaming Audio
        0x185B | // Ranging
        0x185C // HID over ISO
    )
}

/// Decide one characteristic query. `status` is the raw
/// `GattCommunicationStatus`. `att_error` is the result's protocol byte,
/// when the result had one.
pub fn characteristic_discovery(status: i32, att_error: Option<u8>) -> CharacteristicDiscovery {
    match (status, att_error) {
        (0, _) => CharacteristicDiscovery::Continue,
        (3, None) => CharacteristicDiscovery::AccessDenied,
        _ => CharacteristicDiscovery::Failed,
    }
}

/// Outcome of one `GetDescriptors` enumeration.
///
/// Success lists every descriptor the peer returned, including an empty
/// list when the characteristic has none. Attribute Not Found is not
/// success. AccessDenied and ProtocolError fail the query and keep the
/// ATT byte when the result carried one. The caller cancels the owned
/// WinRT operation if the discovery budget ends first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptorEnumeration {
    Listed,
    Failed,
}

/// `Success` (0) is the only status that publishes a descriptor list.
pub fn descriptor_enumeration(status: i32) -> DescriptorEnumeration {
    if status == 0 {
        DescriptorEnumeration::Listed
    } else {
        DescriptorEnumeration::Failed
    }
}

/// `Ok` for `Success`. Any other status is the failure of `stage`, with the
/// status named: WinRT discovery never turns a failed query into an empty
/// list.
///
/// Production WinRT code reports the typed `GattCommunicationStatus`. The
/// pure model keeps this check for the tests that compile this file.
#[cfg(test)]
pub fn require_gatt_success(stage: &str, raw: i32) -> Result<(), String> {
    match gatt_status_name(raw) {
        Some("Success") => Ok(()),
        Some(name) => Err(format!(
            "{stage} failed with GattCommunicationStatus {name} ({raw})"
        )),
        None => Err(format!(
            "{stage} failed with an unrecognized GattCommunicationStatus ({raw})"
        )),
    }
}

/// UBM patch (UBM_PATCHES.md #15): a raw `GattCommunicationStatus` as the
/// legacy WinRT addon's `gattStatus` code (`GattCommunicationStatusCode` in
/// `native/electron/winrt/src/addon.cpp`).
pub fn gatt_status_code(raw: i32) -> &'static str {
    match raw {
        0 => "success",
        1 => "unreachable",
        2 => "protocol-error",
        3 => "access-denied",
        _ => "unknown",
    }
}

/// UBM patch (UBM_PATCHES.md #15): an HRESULT as the legacy WinRT addon's
/// `hresult` code: `0x` and eight upper-case hex digits (`HresultCode`).
pub fn hresult_code(hresult: i32) -> String {
    format!("0x{:08X}", hresult as u32)
}

/// UBM patch (UBM_PATCHES.md #20): the ATT error byte of a
/// `GattCommunicationStatus::ProtocolError` as platform metadata: the key
/// the host's security mapping reads, and the byte as decimal text (the
/// same base the host's ATT code table uses). The radio attaches it for
/// every protocol error it can read one for, security or not — deciding
/// is the host's job.
pub fn att_error_metadata(byte: u8) -> (&'static str, String) {
    ("attError", byte.to_string())
}

/// UBM patch (UBM_PATCHES.md #17): one advertisement data section as
/// service data: the service UUID as a 128-bit value (16- and 32-bit UUIDs
/// on the Bluetooth base UUID) and the payload. `None` for a section that
/// is not service data, or that is too short to hold its UUID (upstream
/// panicked on a short section).
pub fn service_data_section(data_type: u8, data: &[u8]) -> Option<(u128, Vec<u8>)> {
    const BASE: u128 = 0x0000_0000_0000_1000_8000_0080_5f9b_34fb;
    let (uuid, payload) = match data_type {
        0x16 => {
            let (short, rest) = data.split_first_chunk::<2>()?;
            (BASE | (u128::from(u16::from_le_bytes(*short)) << 96), rest)
        }
        0x20 => {
            let (short, rest) = data.split_first_chunk::<4>()?;
            (BASE | (u128::from(u32::from_le_bytes(*short)) << 96), rest)
        }
        0x21 => {
            let (long, rest) = data.split_first_chunk::<16>()?;
            (u128::from_be_bytes(*long), rest)
        }
        _ => return None,
    };
    Some((uuid, payload.to_vec()))
}

/// Index attributes by key, keeping every one. A repeated key is returned
/// as the error instead of one entry silently replacing another: the key
/// is (UUID, ATT handle), and a handle is unique in a GATT database.
pub fn index_unique<K, V>(entries: impl IntoIterator<Item = (K, V)>) -> Result<HashMap<K, V>, K>
where
    K: Eq + Hash + Clone + Debug,
{
    let mut index = HashMap::new();
    for (key, value) in entries {
        if index.contains_key(&key) {
            return Err(key);
        }
        index.insert(key, value);
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::{
        CharacteristicDiscovery, DescriptorEnumeration, ServiceRestriction, att_error_metadata,
        characteristic_discovery, descriptor_enumeration, gatt_status_code, gatt_status_name,
        hresult_code, index_unique, require_gatt_success, service_data_section,
        windows_reserves_service,
    };

    #[test]
    fn only_success_passes_and_every_failure_names_its_status() {
        assert_eq!(require_gatt_success("service discovery", 0), Ok(()));
        for (raw, name) in [
            (1, "Unreachable"),
            (2, "ProtocolError"),
            (3, "AccessDenied"),
        ] {
            let error = require_gatt_success("service discovery", raw).unwrap_err();
            assert!(error.contains("service discovery"), "{error}");
            assert!(error.contains(name), "{error}");
            assert_eq!(gatt_status_name(raw), Some(name));
        }
        let unknown = require_gatt_success("descriptor discovery", 9).unwrap_err();
        assert!(unknown.contains("unrecognized"), "{unknown}");
        assert!(unknown.contains('9'), "{unknown}");
    }

    #[test]
    fn repeated_uuids_stay_distinct_instances_by_handle() {
        let heart_rate = 0x2a37_u16;
        let index = index_unique([
            ((heart_rate, 0x000e_u64), "first"),
            ((heart_rate, 0x0012_u64), "second"),
            ((0x2a38_u16, 0x0010_u64), "location"),
        ])
        .expect("distinct handles");
        assert_eq!(index.len(), 3);
        assert_eq!(index[&(heart_rate, 0x000e)], "first");
        assert_eq!(index[&(heart_rate, 0x0012)], "second");
    }

    #[test]
    fn a_repeated_handle_is_refused_not_overwritten() {
        let repeated = index_unique([((0x2a37_u16, 0x000e_u64), 1), ((0x2a37_u16, 0x000e_u64), 2)]);
        assert_eq!(repeated, Err((0x2a37, 0x000e)));
    }

    #[test]
    fn advertisement_type_keeps_connectable_and_a_scan_response_stays_unknown() {
        assert_eq!(super::advertisement_connectable(0, None), Some(true));
        assert_eq!(super::advertisement_connectable(1, Some(false)), Some(true));
        assert_eq!(super::advertisement_connectable(2, None), Some(false));
        assert_eq!(super::advertisement_connectable(3, Some(true)), Some(false));
        assert_eq!(super::advertisement_connectable(4, Some(false)), None);
        assert_eq!(super::advertisement_connectable(4, None), None);
        assert_eq!(super::advertisement_connectable(5, Some(true)), Some(true));
        assert_eq!(
            super::advertisement_connectable(5, Some(false)),
            Some(false)
        );
        assert_eq!(super::advertisement_connectable(5, None), None);
        assert_eq!(super::advertisement_connectable(9, Some(true)), None);
    }

    /// Windows keeps HID and the LE Audio services. Heart Rate, Battery,
    /// and a 128-bit vendor service stay in discovery.
    #[test]
    fn windows_reserved_services_are_the_os_audio_and_hid_set() {
        let base = 0x0000_1000_8000_0080_5f9b_34fb_u128;
        let short = |id: u16| -> u128 { (u128::from(id) << 96) | base };
        assert!(windows_reserves_service(short(0x184D)));
        assert!(windows_reserves_service(short(0x1844)));
        assert!(windows_reserves_service(short(0x1812)));
        assert!(windows_reserves_service(short(0x185B)));
        assert!(!windows_reserves_service(short(0x180D)));
        assert!(!windows_reserves_service(short(0x180F)));
        assert!(!windows_reserves_service(short(0x180A)));
        assert!(!windows_reserves_service(
            0xfb005c80_02e7_f387_1cad_8acd2d8df0c8
        ));
    }

    /// AccessDenied without an ATT byte keeps an ordinary service. A
    /// protocol error, or an access denial that still carries an ATT byte,
    /// still fails discovery. Known reserved UUIDs are a separate decision.
    #[test]
    fn access_denied_without_an_att_byte_keeps_the_service() {
        assert_eq!(
            characteristic_discovery(0, None),
            CharacteristicDiscovery::Continue
        );
        assert_eq!(
            characteristic_discovery(3, None),
            CharacteristicDiscovery::AccessDenied
        );
        assert_eq!(
            characteristic_discovery(3, Some(5)),
            CharacteristicDiscovery::Failed
        );
        assert_eq!(
            characteristic_discovery(2, Some(5)),
            CharacteristicDiscovery::Failed
        );
        assert_eq!(
            characteristic_discovery(2, None),
            CharacteristicDiscovery::Failed
        );
        assert_eq!(
            characteristic_discovery(1, None),
            CharacteristicDiscovery::Failed
        );
        let base = 0x0000_1000_8000_0080_5f9b_34fb_u128;
        let battery = (u128::from(0x180Fu16) << 96) | base;
        assert!(!windows_reserves_service(battery));
        assert_eq!(
            if windows_reserves_service(battery) {
                ServiceRestriction::OsReserved
            } else if characteristic_discovery(3, None) == CharacteristicDiscovery::AccessDenied {
                ServiceRestriction::AccessDenied
            } else {
                ServiceRestriction::Open
            },
            ServiceRestriction::AccessDenied
        );
        let microphone = (u128::from(0x184Du16) << 96) | base;
        assert!(windows_reserves_service(microphone));
    }

    /// Success is the only status that publishes a descriptor list, and
    /// that list may be empty. Attribute Not Found, Insufficient
    /// Encryption, and AccessDenied are failures, not an empty success.
    #[test]
    fn descriptor_enumeration_lists_only_a_successful_result() {
        assert_eq!(descriptor_enumeration(0), DescriptorEnumeration::Listed);
        assert_eq!(
            descriptor_enumeration(2),
            DescriptorEnumeration::Failed,
            "Attribute Not Found is not an empty list"
        );
        assert_eq!(descriptor_enumeration(3), DescriptorEnumeration::Failed);
        assert_eq!(descriptor_enumeration(1), DescriptorEnumeration::Failed);
    }

    /// UBM patch #20: the ATT error byte rides the platform detail as
    /// decimal text under `attError`, so the host can tell a security
    /// refusal (5, 8, 12, 15) from any other protocol error.
    #[test]
    fn the_att_error_byte_rides_the_platform_detail_as_decimal_text() {
        assert_eq!(att_error_metadata(5), ("attError", "5".to_owned()));
        assert_eq!(att_error_metadata(8), ("attError", "8".to_owned()));
        assert_eq!(att_error_metadata(12), ("attError", "12".to_owned()));
        assert_eq!(att_error_metadata(15), ("attError", "15".to_owned()));
        assert_eq!(att_error_metadata(3), ("attError", "3".to_owned()));
    }

    /// UBM patch #15: the legacy WinRT addon's identities.
    #[test]
    fn platform_codes_follow_the_legacy_addon() {
        assert_eq!(
            [0, 1, 2, 3, 9].map(gatt_status_code),
            [
                "success",
                "unreachable",
                "protocol-error",
                "access-denied",
                "unknown"
            ]
        );
        assert_eq!(hresult_code(0x8065_0005_u32 as i32), "0x80650005");
        assert_eq!(hresult_code(0x0000_00FF), "0x000000FF");
        assert_eq!(hresult_code(-2_147_024_891), "0x80070005");
    }

    /// UBM patch #17: service data sections parse without panicking.
    #[test]
    fn service_data_sections_parse_by_uuid_width() {
        let heart_rate = 0x0000_180d_0000_1000_8000_0080_5f9b_34fb_u128;
        assert_eq!(
            service_data_section(0x16, &[0x0d, 0x18, 0xaa]),
            Some((heart_rate, vec![0xaa]))
        );
        assert_eq!(
            service_data_section(0x20, &[0x0d, 0x18, 0x00, 0x00]),
            Some((heart_rate, vec![]))
        );
        let long = (1u8..=16).collect::<Vec<_>>();
        let mut section = long.clone();
        section.push(0x7f);
        assert_eq!(
            service_data_section(0x21, &section),
            Some((u128::from_be_bytes(long.try_into().unwrap()), vec![0x7f]))
        );
        assert_eq!(service_data_section(0x16, &[0x0d]), None, "too short");
        assert_eq!(service_data_section(0x21, &[0; 15]), None, "too short");
        assert_eq!(
            service_data_section(0xff, &[1, 2, 3]),
            None,
            "not service data"
        );
    }
}
