//! H10 accelerometer settings and raw type-1 PMD wire encoding.
//!
//! Primary evidence (SDK pinned to a693e9e944c9bc925addbdd8cf07fb9b28748bf7):
//! * `documentation/products/PolarH10.md`: 25/50/100/200 Hz, +/-2/4/8 G, mG.
//! * Polar SDK contributor's H10-specific settings recipe (16 bits only;
//!   missing resolution gives INVALID_PARAMETER):
//!   https://github.com/polarofficial/polar-ble-sdk/issues/124#issuecomment-772310984
//! * `technical_documentation/online_measurement.pdf`, tables 6 and 8;
//!   Android `pmd/model/AccData.kt`, `PmdDataFrame.kt`, and `AccDataTest.kt`:
//!   measurement 2, u64 LE timestamp of the last sample, raw frame type 1,
//!   signed little-endian XYZ in milli-g (six bytes per sample).
//!
//! These are protocol-backed encodings, not new physical-device evidence.
//! Generic SDK decoding of raw 8/24-bit and compressed ACC does not establish
//! those as H10-selectable modes. Three axes are fixed, not a CHANNELS option.
//! The status vocabulary is SDK-defined. Exact real-firmware error precedence
//! for duplicate/unknown/multiple-selected/truncated TLVs remains unmeasured;
//! the simulator deterministically refuses malformed structure before values.

use crate::gatt_spec;

pub const SAMPLE_RATES_HZ: [u16; 4] = [25, 50, 100, 200];
pub const RESOLUTION_BITS: u16 = 16;
pub const RANGES_G: [u16; 3] = [2, 4, 8];
pub const CHANNELS: usize = 3;
pub const HEADER_BYTES: usize = 10;
pub const SAMPLE_BYTES: usize = CHANNELS * 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub sample_rate_hz: u16,
    pub range_g: u16,
}

/// Get-settings response parameters, not the surrounding control envelope.
pub fn settings_payload() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(22);
    for (kind, values) in [
        (0, SAMPLE_RATES_HZ.as_slice()),
        (1, [RESOLUTION_BITS].as_slice()),
        (2, RANGES_G.as_slice()),
    ] {
        bytes.extend_from_slice(&[kind, values.len() as u8]);
        for value in values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}

/// Validate exactly one selected value for each of H10's three required TLVs.
/// Call with the settings bytes after the command and measurement-type bytes.
pub fn validate_start(mut tlv: &[u8]) -> Result<Settings, u8> {
    let mut selected = [None; 3];
    while !tlv.is_empty() {
        let [kind, count, tail @ ..] = tlv else {
            return Err(gatt_spec::PMD_STATUS_INVALID_LENGTH);
        };
        let index = usize::from(*kind);
        if index >= selected.len() || *count != 1 {
            return Err(gatt_spec::PMD_STATUS_INVALID_PARAMETER);
        }
        let [low, high, rest @ ..] = tail else {
            return Err(gatt_spec::PMD_STATUS_INVALID_LENGTH);
        };
        if selected[index]
            .replace(u16::from_le_bytes([*low, *high]))
            .is_some()
        {
            return Err(gatt_spec::PMD_STATUS_INVALID_PARAMETER);
        }
        tlv = rest;
    }
    let [Some(sample_rate_hz), Some(resolution), Some(range_g)] = selected else {
        return Err(gatt_spec::PMD_STATUS_INVALID_PARAMETER);
    };
    if !SAMPLE_RATES_HZ.contains(&sample_rate_hz) {
        return Err(gatt_spec::PMD_STATUS_INVALID_SAMPLE_RATE);
    }
    if resolution != RESOLUTION_BITS {
        return Err(gatt_spec::PMD_STATUS_INVALID_RESOLUTION);
    }
    if !RANGES_G.contains(&range_g) {
        return Err(gatt_spec::PMD_STATUS_INVALID_RANGE);
    }
    Ok(Settings {
        sample_rate_hz,
        range_g,
    })
}

/// Encode mG values without scaling or clipping. Range validation belongs to
/// the selected source; transport batching belongs to the radio's real budget.
/// Timestamp is the sensor-clock time of the final sample, in nanoseconds.
pub fn frame(timestamp_ns: u64, samples_mg: &[[i16; 3]]) -> Result<Vec<u8>, String> {
    if samples_mg.is_empty() {
        return Err("ACC frame must contain at least one XYZ sample".to_owned());
    }
    let size = samples_mg
        .len()
        .checked_mul(SAMPLE_BYTES)
        .and_then(|bytes| bytes.checked_add(HEADER_BYTES))
        .ok_or_else(|| "ACC frame size overflows addressable memory".to_owned())?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|error| format!("ACC frame allocation failed: {error}"))?;
    bytes.push(gatt_spec::PMD_MEASUREMENT_ACC);
    bytes.extend_from_slice(&timestamp_ns.to_le_bytes());
    bytes.push(1); // Uncompressed signed 16-bit XYZ, already in milli-g.
    for sample in samples_mg {
        for axis in sample {
            bytes.extend_from_slice(&axis.to_le_bytes());
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gatt_spec;

    fn selected(rate: u16, resolution: u16, range: u16) -> Vec<u8> {
        [(0_u8, rate), (1, resolution), (2, range)]
            .into_iter()
            .flat_map(|(kind, value)| [kind, 1, value as u8, (value >> 8) as u8])
            .collect()
    }

    #[test]
    fn catalog_advertises_only_h10_options_and_all_twelve_combinations_work() {
        assert_eq!(
            settings_payload(),
            vec![0, 4, 25, 0, 50, 0, 100, 0, 200, 0, 1, 1, 16, 0, 2, 3, 2, 0, 4, 0, 8, 0,]
        );
        assert_eq!(CHANNELS, 3);
        for rate in SAMPLE_RATES_HZ {
            for range in RANGES_G {
                assert_eq!(
                    validate_start(&selected(rate, 16, range)),
                    Ok(Settings {
                        sample_rate_hz: rate,
                        range_g: range
                    })
                );
            }
        }
        assert_eq!(
            validate_start(&[2, 1, 8, 0, 0, 1, 200, 0, 1, 1, 16, 0]),
            Ok(Settings {
                sample_rate_hz: 200,
                range_g: 8
            })
        );
    }

    #[test]
    fn rejects_non_h10_values_with_specific_pmd_status() {
        for rate in [0, 12, 26, 52, 104, 208, 400, u16::MAX] {
            assert_eq!(
                validate_start(&selected(rate, 16, 8)),
                Err(gatt_spec::PMD_STATUS_INVALID_SAMPLE_RATE)
            );
        }
        for resolution in [0, 8, 14, 24, u16::MAX] {
            assert_eq!(
                validate_start(&selected(200, resolution, 8)),
                Err(gatt_spec::PMD_STATUS_INVALID_RESOLUTION)
            );
        }
        for range in [0, 1, 3, 16, u16::MAX] {
            assert_eq!(
                validate_start(&selected(200, 16, range)),
                Err(gatt_spec::PMD_STATUS_INVALID_RANGE)
            );
        }
    }

    #[test]
    fn malformed_selection_never_reads_past_input_or_guesses_missing_values() {
        let valid = selected(25, 16, 2);
        for end in 0..valid.len() {
            let expected = if end % 4 == 0 {
                gatt_spec::PMD_STATUS_INVALID_PARAMETER
            } else {
                gatt_spec::PMD_STATUS_INVALID_LENGTH
            };
            assert_eq!(
                validate_start(&valid[..end]),
                Err(expected),
                "prefix length {end}"
            );
        }
        for bad in [
            vec![0, 0],
            vec![0, 2, 25, 0, 50, 0],
            [valid.as_slice(), &[0, 1, 25, 0]].concat(),
            [valid.as_slice(), &[4, 1, 3]].concat(),
            [valid.as_slice(), &[5, 1, 0, 0, 128, 63]].concat(),
            [valid.as_slice(), &[255, 1, 0, 0]].concat(),
        ] {
            assert_eq!(
                validate_start(&bad),
                Err(gatt_spec::PMD_STATUS_INVALID_PARAMETER),
                "{bad:?}"
            );
        }
        // The H10 requires every advertised setting, including resolution.
        assert_eq!(
            validate_start(&[0, 1, 25, 0, 2, 1, 4, 0]),
            Err(gatt_spec::PMD_STATUS_INVALID_PARAMETER)
        );
    }

    #[test]
    fn raw_type_one_frame_matches_official_sdk_vector_and_signed_extremes() {
        assert_eq!(
            frame(2_000_000_000, &[[-9, -1, 999], [-8, -2, 997]]).unwrap(),
            vec![
                2, 0, 0x94, 0x35, 0x77, 0, 0, 0, 0, 1, 0xf7, 0xff, 0xff, 0xff, 0xe7, 3, 0xf8, 0xff,
                0xfe, 0xff, 0xe5, 3,
            ]
        );
        let extrema = frame(u64::MAX, &[[i16::MIN, 0, i16::MAX]]).unwrap();
        assert_eq!(&extrema[1..9], &[255; 8]);
        assert_eq!(&extrema[10..], &[0, 128, 0, 0, 255, 127]);
        assert_eq!(extrema.len(), HEADER_BYTES + SAMPLE_BYTES);
        assert!(frame(1, &[]).is_err());
    }
}
