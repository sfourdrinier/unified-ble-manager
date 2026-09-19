//! Advertisement layout and the legacy 31-byte payload budget.
//!
//! BlueZ 5.72 (`src/advertising.c`) lays a `LEAdvertisement1` object with
//! `Type: "peripheral"`, `ServiceUUIDs: [180D, FEEE]`, `LocalName` and an
//! empty `Includes` list out as two legacy payloads:
//!
//! * advertisement data: Flags (`02 01 06`, 3 bytes) plus the complete list
//!   of 16-bit service UUIDs (`05 03 0D 18 EE FE`, 6 bytes) — 9 bytes total;
//! * scan response: the complete local name (`len + 0x09 + name`) —
//!   `2 + name.len()` bytes.
//!
//! The name therefore never competes with the flags and UUIDs for the 31
//! bytes of the advertisement data: it lives in the scan response, exactly
//! like a real Polar H10. `Includes: ["local-name"]` must stay empty —
//! setting it alongside the `LocalName` property makes BlueZ reject the
//! object outright (`parse_local_name`: "Local name already included"), and
//! the 128-bit Polar PMD service UUID must never be added to the advertised
//! set (16 bytes where 4 suffice).
//!
//! `--name` is user-controlled and unbounded, so [`fit_name`] truncates it to
//! the scan-response budget before it reaches the radio. An over-long name
//! is the one payload case that provably fails registration with the exact
//! symptom seen on Linux (`Failed to register advertisement`, kernel
//! `Invalid Parameters (0x0d)` for an oversized scan response), hence the
//! truncation plus the tests below.

use std::borrow::Cow;

use crate::gatt_spec;

/// Legacy advertisement / scan-response payload limit (Core v5.x, Vol 3 C §11).
pub const MAX_LEGACY_PAYLOAD: usize = 31;

/// Bytes of overhead around the service-UUID list in the advertisement data:
/// Flags AD structure (3) + complete-16-bit-UUID list header (2).
pub const ADV_FIXED_LEN: usize = 3 + 2 + 2 * 2;

/// Bytes of overhead around the name in the scan response: length + AD type.
pub const SCAN_NAME_OVERHEAD: usize = 2;

/// Maximum name length that still fits the scan response.
pub const MAX_NAME_LEN: usize = MAX_LEGACY_PAYLOAD - SCAN_NAME_OVERHEAD;

/// Service UUIDs carried in the advertisement: Heart Rate plus Polar (FEEE).
/// This set is pinned: adding the 128-bit PMD service UUID (16 bytes) or
/// manufacturer data here spends budget the name needs.
pub const ADVERTISED_SERVICES: [u16; 2] = [
    gatt_spec::uuid16::HEART_RATE_SERVICE,
    gatt_spec::uuid16::POLAR_ADV_SERVICE,
];

/// Bytes of overhead of the manufacturer-data AD structure: length + AD type
/// (2) plus the company id (2). The payload follows.
pub const ADV_MFR_OVERHEAD: usize = 2 + 2;

/// Legacy advertisement-data length with a manufacturer payload of `len`
/// bytes: flags (3) + UUID list (6) + mfr structure.
pub fn adv_len_with_mfr(payload_len: usize) -> usize {
    ADV_FIXED_LEN + ADV_MFR_OVERHEAD + payload_len
}

/// True when the advertisement data still fits with a manufacturer payload of
/// `len` bytes. The name lives in the scan response and is unaffected.
pub fn mfr_fits(payload_len: usize) -> bool {
    adv_len_with_mfr(payload_len) <= MAX_LEGACY_PAYLOAD
}

/// Refuses an oversized manufacturer payload loudly (names the budget).
pub fn check_mfr_budget(payload_len: usize) -> Result<(), String> {
    if mfr_fits(payload_len) {
        Ok(())
    } else {
        Err(format!(
            "manufacturer payload of {payload_len} bytes does not fit: \
             advertisement data would be {} bytes, budget is {MAX_LEGACY_PAYLOAD}",
            adv_len_with_mfr(payload_len)
        ))
    }
}

/// Legacy payload sizes for `name`: advertisement data and scan response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvertisementSizes {
    pub adv_len: usize,
    pub scan_rsp_len: usize,
}

/// Computes the legacy payload sizes for `name` under the layout above.
pub fn advertisement_sizes(name: &str) -> AdvertisementSizes {
    AdvertisementSizes {
        adv_len: ADV_FIXED_LEN,
        scan_rsp_len: SCAN_NAME_OVERHEAD + name.len(),
    }
}

/// True when both legacy payloads fit for `name`.
pub fn fits_budget(name: &str) -> bool {
    let sizes = advertisement_sizes(name);
    sizes.adv_len <= MAX_LEGACY_PAYLOAD && sizes.scan_rsp_len <= MAX_LEGACY_PAYLOAD
}

/// Truncates `name` to [`MAX_NAME_LEN`] bytes on a UTF-8 boundary.
/// Returns a borrow when the name already fits (the common path is free).
pub fn fit_name(name: &str) -> Cow<'_, str> {
    if name.len() <= MAX_NAME_LEN {
        return Cow::Borrowed(name);
    }
    let mut end = MAX_NAME_LEN;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(name[..end].to_string())
}

/// Builds the full 128-bit UUID for a 16-bit short UUID
/// (`0000XXXX-0000-1000-8000-00805F9B34FB`), bit-identical to
/// `ble_peripheral_rust::uuid::ShortUuid::from_short` so both radio backends
/// route the same characteristic UUIDs.
pub fn short_uuid(short: u16) -> uuid::Uuid {
    uuid::Uuid::from_fields(
        u32::from(short),
        0,
        0x1000,
        b"\x80\x00\x00\x80\x5F\x9B\x34\xFB",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gatt_spec;

    #[test]
    fn manufacturer_data_spends_adv_budget() {
        // Flags (3) + UUID list (6) + mfr AD structure (2 header + 2 company + N).
        assert_eq!(adv_len_with_mfr(0), 9 + 4);
        assert_eq!(adv_len_with_mfr(5), 9 + 4 + 5);
        assert!(mfr_fits(18), "9 + 4 + 18 = 31 fills the payload");
        assert!(!mfr_fits(19));
    }

    #[test]
    fn default_name_fits_both_payloads() {
        let sizes = advertisement_sizes(gatt_spec::DEFAULT_ADV_NAME);
        assert_eq!(gatt_spec::DEFAULT_ADV_NAME.len(), 17);
        assert_eq!(sizes.adv_len, 9, "flags (3) + UUID list (6)");
        assert_eq!(sizes.scan_rsp_len, 19, "header (2) + name (17)");
        assert!(fits_budget(gatt_spec::DEFAULT_ADV_NAME));
        assert!(matches!(
            fit_name(gatt_spec::DEFAULT_ADV_NAME),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn reported_linux_name_fits_both_payloads() {
        // "Polar H10 SIMLNX1" (17 chars) is the name from the failing Linux
        // run: adv 9 + scan 19 both fit, so payload size did not cause that
        // failure. This test pins that finding.
        let name = "Polar H10 SIMLNX1";
        let sizes = advertisement_sizes(name);
        assert_eq!(
            sizes,
            AdvertisementSizes {
                adv_len: 9,
                scan_rsp_len: 19
            }
        );
        assert!(fits_budget(name));
    }

    #[test]
    fn max_length_name_fills_scan_response_exactly() {
        let name = "P".repeat(MAX_NAME_LEN);
        assert_eq!(MAX_NAME_LEN, 29);
        assert_eq!(advertisement_sizes(&name).scan_rsp_len, MAX_LEGACY_PAYLOAD);
        assert!(fits_budget(&name));
        let one_more = format!("{name}P");
        assert!(!fits_budget(&one_more));
    }

    #[test]
    fn overlong_name_truncates_to_budget() {
        let long = "Polar H10 SIM0001-with-a-very-long-suffix";
        let fitted = fit_name(long);
        assert!(matches!(fitted, Cow::Owned(_)));
        assert_eq!(fitted.len(), MAX_NAME_LEN);
        assert!(fits_budget(&fitted));
        assert_eq!(
            advertisement_sizes(&fitted).scan_rsp_len,
            MAX_LEGACY_PAYLOAD
        );
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        // 28 ASCII bytes plus a 2-byte 'é' would end at 30: must back off to 28.
        let name = format!("{}é", "P".repeat(28));
        assert_eq!(name.len(), 30);
        let fitted = fit_name(&name);
        assert_eq!(fitted.len(), 28);
        assert!(fitted.is_char_boundary(fitted.len()));
    }

    #[test]
    fn advertised_set_is_exactly_hr_plus_polar() {
        assert_eq!(ADVERTISED_SERVICES, [0x180D, 0xFEEE]);
        // The 128-bit PMD service UUID is served over GATT, never advertised.
        let pmd = uuid::Uuid::parse_str(gatt_spec::pmd::SERVICE).unwrap();
        for short in ADVERTISED_SERVICES {
            assert_ne!(short_uuid(short), pmd);
        }
        assert_eq!(
            short_uuid(0x180D).to_string(),
            "0000180d-0000-1000-8000-00805f9b34fb"
        );
    }
}
