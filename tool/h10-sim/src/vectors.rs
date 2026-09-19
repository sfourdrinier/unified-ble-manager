//! Golden vectors for the TypeScript cross-check.
//!
//! `vectors::test_vectors` serializes encoder outputs; the Node script
//! `tests/xcheck/run-xcheck.cjs` decodes them with the repository's own
//! parsers. Timestamps that exceed the JSON safe-integer range travel as
//! strings.

use serde::Serialize;

use crate::{ecg, gatt_spec, sim::SimState};

#[derive(Debug, Serialize)]
pub struct HrVector {
    pub bpm: u8,
    pub rr_s: Vec<f64>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Serialize)]
pub struct PmdResponseVector {
    pub op: u8,
    pub status: u8,
    pub status_name: &'static str,
    pub settings: bool,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Serialize)]
pub struct EcgFrameVector {
    pub timestamp_ns: String,
    pub samples_uv: Vec<i32>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Serialize)]
pub struct TestVectors {
    pub hr_measurements: Vec<HrVector>,
    pub hr_no_contact: HrVector,
    pub hr_no_rr: HrVector,
    pub body_location: Vec<u8>,
    pub pmd_features: Vec<u8>,
    pub pmd_responses: Vec<PmdResponseVector>,
    pub ecg_frames: Vec<EcgFrameVector>,
}

pub fn test_vectors(state: &SimState) -> TestVectors {
    // Contact-detected vectors keep exercising the parser's contact path;
    // the strap default (no contact bits) rides in `hr_no_contact`.
    let hr_case = |bpm: u8| {
        let rr_s = vec![60.0 / f64::from(bpm)];
        let bytes = gatt_spec::encode_hr_measurement_with_contact(bpm, &rr_s, true);
        HrVector { bpm, rr_s, bytes }
    };
    let ecg_case = |timestamp_ns: u64, start_index: u64, count: usize| {
        let mut samples_uv = Vec::with_capacity(count);
        ecg::ecg_frame_samples(
            start_index,
            count,
            f64::from(state.config.bpm),
            &mut samples_uv,
        );
        let bytes = gatt_spec::encode_ecg_frame(timestamp_ns, &samples_uv);
        EcgFrameVector {
            timestamp_ns: timestamp_ns.to_string(),
            samples_uv,
            bytes,
        }
    };
    TestVectors {
        hr_measurements: vec![hr_case(48), hr_case(72), hr_case(150)],
        hr_no_contact: HrVector {
            // Byte-identical to recorded packet 1 (1058bd02): 88 bpm,
            // one RR interval of 701/1024 s, contact not supported.
            bpm: 88,
            rr_s: vec![701.0 / 1024.0],
            bytes: gatt_spec::encode_hr_measurement_no_contact(88, &[701.0 / 1024.0]),
        },
        hr_no_rr: HrVector {
            bpm: 90,
            rr_s: Vec::new(),
            bytes: gatt_spec::encode_hr_measurement(90, &[]),
        },
        body_location: gatt_spec::encode_body_sensor_location(),
        pmd_features: state.pmd_features(),
        pmd_responses: vec![
            PmdResponseVector {
                op: gatt_spec::PMD_OP_GET_SETTINGS,
                status: gatt_spec::PMD_STATUS_SUCCESS,
                status_name: "SUCCESS",
                settings: true,
                bytes: gatt_spec::encode_pmd_response(
                    gatt_spec::PMD_OP_GET_SETTINGS,
                    gatt_spec::PMD_MEASUREMENT_ECG,
                    gatt_spec::PMD_STATUS_SUCCESS,
                    false,
                    &gatt_spec::encode_ecg_settings(),
                ),
            },
            PmdResponseVector {
                op: gatt_spec::PMD_OP_START,
                status: gatt_spec::PMD_STATUS_SUCCESS,
                status_name: "SUCCESS",
                settings: false,
                bytes: gatt_spec::encode_pmd_response(
                    gatt_spec::PMD_OP_START,
                    gatt_spec::PMD_MEASUREMENT_ECG,
                    gatt_spec::PMD_STATUS_SUCCESS,
                    false,
                    &[],
                ),
            },
            PmdResponseVector {
                op: gatt_spec::PMD_OP_START,
                status: gatt_spec::PMD_STATUS_INVALID_SAMPLE_RATE,
                status_name: "ERROR_INVALID_SAMPLE_RATE",
                settings: false,
                bytes: gatt_spec::encode_pmd_response(
                    gatt_spec::PMD_OP_START,
                    gatt_spec::PMD_MEASUREMENT_ECG,
                    gatt_spec::PMD_STATUS_INVALID_SAMPLE_RATE,
                    false,
                    &[],
                ),
            },
        ],
        ecg_frames: vec![
            ecg_case(1_000_000_000, 0, 13),
            ecg_case(9_999_999_999_999_999_999, 1300, 65),
        ],
    }
}
