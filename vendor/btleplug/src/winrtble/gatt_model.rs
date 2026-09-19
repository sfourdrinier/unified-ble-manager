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
pub fn gatt_status_name(raw: i32) -> Option<&'static str> {
    match raw {
        0 => Some("Success"),
        1 => Some("Unreachable"),
        2 => Some("ProtocolError"),
        3 => Some("AccessDenied"),
        _ => None,
    }
}

/// `Ok` for `Success`. Any other status is the failure of `stage`, with the
/// status named: WinRT discovery never turns a failed query into an empty
/// list.
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
        att_error_metadata, gatt_status_code, gatt_status_name, hresult_code, index_unique,
        require_gatt_success, service_data_section,
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
