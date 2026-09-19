//! Pure H10 GATT encoders.
//!
//! Every byte layout here is pinned by the unit tests below and cross-checked
//! against the repository's own TypeScript parsers
//! (`examples-shared/driver/polar-pmd.ts` and `src/profiles/heart-rate.ts`) by
//! `tests/xcheck/run-xcheck.mjs`. No Bluetooth hardware is involved.
//!
//! Sources:
//! - Bluetooth SIG, Heart Rate Service (HRS) 1.0: §3.3 Heart Rate Measurement
//!   (flags bit 0 = Heart Rate Value Format, bit 4 = RR-Interval present),
//!   §3.4 Body Sensor Location.
//! - Bluetooth SIG, Battery Service (BAS) 1.1: §3.2 Battery Level (uint8 percent).
//! - Bluetooth SIG, Device Information Service (DIS) 1.1: string characteristics
//!   are UTF-8; System ID (0x2A23) is 8 bytes.
//! - Polar BLE SDK source (polarofficial/polar-ble-sdk, Android
//!   `BlePMDClient` / `PmdControlPointResponse` / `PmdDataFrame` / `PmdSetting`
//!   / `PmdMeasurementType`): PMD control-point responses are
//!   `[0xF0, op, measurementType, status, more, params…]`, ECG data frames are
//!   `[0x00, timestampNs:u64 LE, frameType, samples…]` with type-0 samples as
//!   signed 24-bit little-endian microvolts at 130 Hz / 14 bit, and the feature
//!   bitmap's byte 1 carries ECG = 0x01.

/// Short UUIDs used by the simulated H10.
pub mod uuid16 {
    pub const HEART_RATE_SERVICE: u16 = 0x180D;
    pub const HEART_RATE_MEASUREMENT: u16 = 0x2A37;
    pub const BODY_SENSOR_LOCATION: u16 = 0x2A38;
    pub const BATTERY_SERVICE: u16 = 0x180F;
    pub const BATTERY_LEVEL: u16 = 0x2A19;
    pub const DEVICE_INFORMATION_SERVICE: u16 = 0x180A;
    pub const MANUFACTURER_NAME: u16 = 0x2A29;
    pub const MODEL_NUMBER: u16 = 0x2A24;
    pub const SERIAL_NUMBER: u16 = 0x2A25;
    pub const FIRMWARE_REVISION: u16 = 0x2A26;
    pub const HARDWARE_REVISION: u16 = 0x2A27;
    pub const SOFTWARE_REVISION: u16 = 0x2A28;
    pub const SYSTEM_ID: u16 = 0x2A23;
    /// Polar's custom advertisement service (present on a real H10).
    pub const POLAR_ADV_SERVICE: u16 = 0xFEEE;
}

/// Full 128-bit Polar PMD UUIDs.
pub mod pmd {
    pub const SERVICE: &str = "FB005C80-02E7-F387-1CAD-8ACD2D8DF0C8";
    pub const CONTROL_POINT: &str = "FB005C81-02E7-F387-1CAD-8ACD2D8DF0C8";
    pub const DATA: &str = "FB005C82-02E7-F387-1CAD-8ACD2D8DF0C8";
}

/// Full 128-bit UUIDs of the second Polar vendor service and its two
/// characteristics, pinned by the h10-capture fingerprints in
/// `fixtures/h10-fingerprints/` (each characteristic has its own base UUID).
pub mod vendor {
    pub const SERVICE: &str = "6217FF4B-FB31-1140-AD5A-A45545D7ECF3";
    pub const READ: &str = "6217FF4C-C8EC-B1FB-1380-3AD986708E2D";
    pub const WRITE_INDICATE: &str = "6217FF4D-91BB-91D0-7E2A-7CD3BDA8A1F3";
}

/// Full 128-bit UUIDs of the Polar advertisement service's GATT characteristics
/// (PMD base UUID), pinned by the same fingerprints. None is readable; their
/// values are UNCONFIRMED (see README).
pub mod feee {
    pub const CHAR_51: &str = "FB005C51-02E7-F387-1CAD-8ACD2D8DF0C8";
    pub const CHAR_52: &str = "FB005C52-02E7-F387-1CAD-8ACD2D8DF0C8";
    pub const CHAR_53: &str = "FB005C53-02E7-F387-1CAD-8ACD2D8DF0C8";
}

/// Polar's Bluetooth SIG company identifier (real H10 advertises manufacturer
/// data under it; the peripheral backends used here cannot emit manufacturer
/// data, so this is a documented fidelity gap — see README).
pub const POLAR_COMPANY_ID: u16 = 0x006B;

/// H10 ECG stream parameters (Polar preferred: 130 Hz, 14-bit resolution).
pub const H10_ECG_SAMPLE_RATE_HZ: u16 = 130;
pub const H10_ECG_RESOLUTION_BITS: u16 = 14;
/// H10 ECG frame geometry, pinned by the h10-capture fingerprints in
/// `fixtures/h10-fingerprints/` (`ecgSamplesPerFrame`: 73 samples, consistent,
/// on all three capture hosts; frame interval ~561.6 ms = 73/130 s).
pub const H10_ECG_SAMPLES_PER_FRAME: usize = 73;
pub const H10_ECG_FRAMES_PER_SEC: f64 = 130.0 / 73.0;

/// H10 body sensor location: chest (SIG Body Sensor Location §3.4, value 1).
pub const BODY_LOCATION_CHEST: u8 = 1;

/// PMD op codes (Polar SDK `PmdControlPointCommandClientToService`).
pub const PMD_OP_GET_SETTINGS: u8 = 0x01;
pub const PMD_OP_START: u8 = 0x02;
pub const PMD_OP_STOP: u8 = 0x03;
pub const PMD_MEASUREMENT_ECG: u8 = 0x00;
pub const PMD_RESPONSE_CODE: u8 = 0xF0;

/// PMD status codes (Polar SDK `PmdControlPointResponseCode`, same order as
/// `RESPONSE_STATUS_NAMES` in `examples-shared/driver/polar-pmd.ts`).
pub const PMD_STATUS_SUCCESS: u8 = 0x00;
pub const PMD_STATUS_INVALID_OP: u8 = 0x01;
pub const PMD_STATUS_INVALID_MEASUREMENT_TYPE: u8 = 0x02;
pub const PMD_STATUS_NOT_SUPPORTED: u8 = 0x03;
pub const PMD_STATUS_INVALID_LENGTH: u8 = 0x04;
pub const PMD_STATUS_ALREADY_IN_STATE: u8 = 0x06;
pub const PMD_STATUS_INVALID_RESOLUTION: u8 = 0x07;
pub const PMD_STATUS_INVALID_SAMPLE_RATE: u8 = 0x08;

/// Heart Rate Measurement flags (SIG HRS §3.3).
/// Bit 2: sensor contact supported; bit 1: contact detected.
pub const HR_FLAG_CONTACT_MASK: u8 = 0x06;
pub const HR_FLAG_CONTACT_SUPPORTED: u8 = 0x04;
pub const HR_FLAG_RR_PRESENT: u8 = 0x10;

/// Default advertised name; `<4 hex>` keeps the `device` exact-match argument working.
pub const DEFAULT_ADV_NAME: &str = "Polar H10 SIM0001";

/// Encodes a Heart Rate Measurement (SIG 0x2A37).
///
/// Flags match the strap: uint8 bpm, sensor contact not supported, and
/// RR-Interval present whenever intervals are supplied (`0x10` with RR,
/// `0x00` without — all 120 raw packets in `fixtures/h10-raw` agree).
/// Explicit contact simulation stays available through
/// [`encode_hr_measurement_with_contact`].
pub fn encode_hr_measurement(bpm: u8, rr_intervals_s: &[f64]) -> Vec<u8> {
    encode_hr_measurement_no_contact(bpm, rr_intervals_s)
}

/// Encodes a Heart Rate Measurement with no sensor-contact bits: `0x10`
/// when RR intervals are present, `0x00` otherwise (SIG HRS §3.3 bits 1–2
/// clear = contact not supported — what the strap sends).
pub fn encode_hr_measurement_no_contact(bpm: u8, rr_intervals_s: &[f64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rr_intervals_s.len() * 2);
    out.push(if rr_intervals_s.is_empty() {
        0x00
    } else {
        HR_FLAG_RR_PRESENT
    });
    out.push(bpm);
    for interval in rr_intervals_s {
        let units = (interval * 1024.0).round().clamp(0.0, 65_535.0) as u16;
        out.extend_from_slice(&units.to_le_bytes());
    }
    out
}

/// Encodes a Heart Rate Measurement with an explicit contact state: contact
/// detected reports supported + detected (`0x06`); contact lost reports
/// supported only (`0x04`, SIG HRS §3.3 bits 1–2).
pub fn encode_hr_measurement_with_contact(
    bpm: u8,
    rr_intervals_s: &[f64],
    contact_detected: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rr_intervals_s.len() * 2);
    let mut flags = if contact_detected {
        HR_FLAG_CONTACT_MASK
    } else {
        HR_FLAG_CONTACT_SUPPORTED
    };
    if !rr_intervals_s.is_empty() {
        flags |= HR_FLAG_RR_PRESENT;
    }
    out.push(flags);
    out.push(bpm);
    for interval in rr_intervals_s {
        let units = (interval * 1024.0).round().clamp(0.0, 65_535.0) as u16;
        out.extend_from_slice(&units.to_le_bytes());
    }
    out
}

/// Encodes a Device Information string (SIG DIS 1.1): UTF-8 with the strap's
/// trailing NUL (`fixtures/h10-fingerprints/*/values.*.raw` all end in 00).
pub fn encode_dis_string(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 1);
    out.extend_from_slice(text.as_bytes());
    out.push(0x00);
    out
}

/// Encodes Body Sensor Location (SIG 0x2A38): chest.
pub fn encode_body_sensor_location() -> Vec<u8> {
    vec![BODY_LOCATION_CHEST]
}

/// Encodes Battery Level (SIG 0x2A19): uint8 percent.
pub fn encode_battery_level(percent: u8) -> Vec<u8> {
    vec![percent.min(100)]
}

/// Encodes the PMD control-point feature read: the exact 17 bytes a real H10
/// answers, pinned by the h10-capture fingerprints in
/// `fixtures/h10-fingerprints/` (`0f050000…`, stable across all three capture
/// hosts). Byte 1 is the SDK feature bitmap (`PmdMeasurementType.fromByteArray`):
/// ECG = 0x01, ACC = 0x04 — the strap streams ECG; ACC streaming stays an
/// UNCONFIRMED gap (see README).
pub fn encode_pmd_features() -> Vec<u8> {
    vec![
        0x0F, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ]
}

/// Encodes a PMD control-point response:
/// `[0xF0, op, measurementType, status, more, params…]`.
pub fn encode_pmd_response(
    op: u8,
    measurement_type: u8,
    status: u8,
    more: bool,
    params: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + params.len());
    out.push(PMD_RESPONSE_CODE);
    out.push(op);
    out.push(measurement_type);
    out.push(status);
    if status == PMD_STATUS_SUCCESS {
        out.push(u8::from(more));
        out.extend_from_slice(params);
    }
    out
}

/// Encodes the ECG settings TLV carried in a get-settings success response:
/// sample rate (type 0) then resolution (type 1), each `[type][count=1][u16 LE]`.
pub fn encode_ecg_settings() -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&[0x00, 0x01]);
    out.extend_from_slice(&H10_ECG_SAMPLE_RATE_HZ.to_le_bytes());
    out.extend_from_slice(&[0x01, 0x01]);
    out.extend_from_slice(&H10_ECG_RESOLUTION_BITS.to_le_bytes());
    out
}

/// Encodes one PMD ECG data frame: `[0x00, timestampNs:u64 LE, 0x00,
/// samples…]` with type-0 samples as signed 24-bit little-endian microvolts.
/// The timestamp is the sensor time of the frame's last sample.
pub fn encode_ecg_frame(timestamp_ns: u64, samples_uv: &[i32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + samples_uv.len() * 3);
    out.push(PMD_MEASUREMENT_ECG);
    out.extend_from_slice(&timestamp_ns.to_le_bytes());
    out.push(0x00);
    for sample in samples_uv.iter().copied() {
        let clamped = sample.clamp(-8_388_608, 8_388_607);
        let unsigned = (clamped as i64 & 0xFF_FFFF) as u32;
        out.extend_from_slice(&unsigned.to_le_bytes()[..3]);
    }
    out
}

/// Encodes an 8-byte System ID (SIG 0x2A23): 5-byte manufacturer identifier
/// (little-endian) followed by the 3-byte OUI.
pub fn encode_system_id(manufacturer_id: u64, oui: [u8; 3]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&manufacturer_id.to_le_bytes()[..5]);
    out.extend_from_slice(&oui);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hr_flags_match_strap_rr_present_contact_not_supported() {
        // The 120 raw HR packets in fixtures/h10-raw all start with 0x10:
        // uint8 bpm, RR present, sensor contact not supported.
        let bytes = encode_hr_measurement(72, &[0.833]);
        assert_eq!(bytes[0], 0x10, "flags must be uint8 + RR, no contact bits");
        assert_eq!(bytes[1], 72);
    }

    #[test]
    fn hr_rr_intervals_use_1024ths_of_a_second() {
        let bytes = encode_hr_measurement(60, &[1.0, 0.5]);
        assert_eq!(bytes[0] & HR_FLAG_RR_PRESENT, HR_FLAG_RR_PRESENT);
        let rr0 = u16::from_le_bytes([bytes[2], bytes[3]]);
        let rr1 = u16::from_le_bytes([bytes[4], bytes[5]]);
        assert_eq!(rr0, 1024);
        assert_eq!(rr1, 512);
        assert_eq!(bytes.len(), 6);
    }

    #[test]
    fn hr_without_rr_clears_rr_flag_and_reports_no_contact() {
        let bytes = encode_hr_measurement(90, &[]);
        assert_eq!(bytes, vec![0x00, 90]);
    }

    #[test]
    fn dis_strings_carry_the_strap_trailing_nul() {
        // Every DIS string in fixtures/h10-fingerprints ends in 0x00.
        assert_eq!(encode_dis_string("Polar Electro Oy"), {
            let mut expected = b"Polar Electro Oy".to_vec();
            expected.push(0x00);
            expected
        });
        assert_eq!(encode_dis_string("H10"), vec![0x48, 0x31, 0x30, 0x00]);
    }

    #[test]
    fn ecg_geometry_matches_the_strap_frame() {
        // All three captures: 73 samples/frame, consistent, 130 Hz.
        assert_eq!(H10_ECG_SAMPLES_PER_FRAME, 73);
        assert!(
            (H10_ECG_FRAMES_PER_SEC - 130.0 / 73.0).abs() < 1e-12,
            "frame cadence must satisfy frames x samples = 130 Hz"
        );
    }

    #[test]
    fn body_sensor_location_is_chest() {
        assert_eq!(encode_body_sensor_location(), vec![BODY_LOCATION_CHEST]);
        assert_eq!(BODY_LOCATION_CHEST, 1);
    }

    #[test]
    fn battery_level_is_uint8_percent() {
        assert_eq!(encode_battery_level(85), vec![85]);
        assert_eq!(encode_battery_level(100), vec![100]);
    }

    #[test]
    fn pmd_features_match_the_real_strap_bytes() {
        assert_eq!(
            encode_pmd_features(),
            vec![
                0x0F, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00,
            ],
            "byte-identical to fixtures/h10-fingerprints/*/values.pmdFeatures.raw",
        );
        let features = encode_pmd_features();
        assert_eq!(features[1] & 0x01, 0x01, "ECG bit must be set");
        assert_eq!(features[1] & 0x04, 0x04, "ACC bit must be set");
    }

    #[test]
    fn pmd_response_layout_matches_sdk() {
        let response = encode_pmd_response(0x01, 0x00, 0x00, false, &[0xAA]);
        assert_eq!(response, vec![0xF0, 0x01, 0x00, 0x00, 0x00, 0xAA]);
    }

    #[test]
    fn pmd_error_response_carries_status_and_no_params() {
        let response = encode_pmd_response(0x02, 0x00, PMD_STATUS_INVALID_SAMPLE_RATE, false, &[]);
        assert_eq!(response, vec![0xF0, 0x02, 0x00, 0x08]);
    }

    #[test]
    fn ecg_settings_carry_130hz_14bit() {
        assert_eq!(
            encode_ecg_settings(),
            vec![0x00, 0x01, 0x82, 0x00, 0x01, 0x01, 0x0E, 0x00]
        );
    }

    #[test]
    fn ecg_frame_header_is_type_timestamp_frametype() {
        let frame = encode_ecg_frame(1_000_000_000, &[0]);
        assert_eq!(
            &frame[0..10],
            &[0x00, 0x00, 0xCA, 0x9A, 0x3B, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(frame.len(), 13);
    }

    #[test]
    fn ecg_frame_samples_are_signed_24bit_le_microvolts() {
        let frame = encode_ecg_frame(0, &[1, -1, 8_388_607, -8_388_608]);
        assert_eq!(&frame[10..13], &[0x01, 0x00, 0x00]);
        assert_eq!(&frame[13..16], &[0xFF, 0xFF, 0xFF]);
        assert_eq!(&frame[16..19], &[0xFF, 0xFF, 0x7F]);
        assert_eq!(&frame[19..22], &[0x00, 0x00, 0x80]);
    }

    #[test]
    fn system_id_is_8_bytes_manufacturer_then_oui() {
        let id = encode_system_id(0x0102030405, [0xAA, 0xBB, 0xCC]);
        assert_eq!(id, vec![0x05, 0x04, 0x03, 0x02, 0x01, 0xAA, 0xBB, 0xCC]);
    }
}
