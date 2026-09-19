//! Simulator state: configuration, fault injection and PMD command handling.
//!
//! All GATT behaviour is decided here against pure data; `radio.rs` only
//! transports bytes. That keeps every behaviour below unit-testable without
//! Bluetooth hardware.

use serde::Deserialize;

use crate::gatt_spec;

/// Pairing/bonding policy of the simulated strap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PairPolicy {
    /// Bonding works when the central requests it; nothing is gated on a
    /// bond — this matches the H10, whose PMD streams without pairing.
    JustWorks,
    /// Pairing is refused at the policy level (recorded in state; BlueZ-side
    /// enforcement is documented in the README).
    Disabled,
}

impl PairPolicy {
    /// Parses a CLI value case-insensitively; anything else is a loud error.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.to_ascii_lowercase().as_str() {
            "just-works" => Ok(Self::JustWorks),
            "disabled" => Ok(Self::Disabled),
            _ => Err(format!(
                "pair policy must be just-works or disabled, got {text:?}"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::JustWorks => "just-works",
            Self::Disabled => "disabled",
        }
    }
}

/// ECG waveform source: `"synthetic"` or `{"file": "path"}` in profiles.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub enum EcgSource {
    /// Deterministic synthetic PQRST waveform.
    #[serde(rename = "synthetic")]
    Synthetic,
    /// Replay a text file (one integer µV per line @130 Hz), cycling forever.
    File { file: String },
}

/// One step of a scripted heart-rate curve: hold `bpm` from `at_s` on.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct BpmStep {
    pub at_s: f64,
    pub bpm: u8,
}

/// Runtime configuration of the simulated strap.
#[derive(Debug, Clone)]
pub struct SimConfig {
    pub name: String,
    pub bpm: u8,
    pub battery_percent: u8,
    pub manufacturer: String,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub hardware: String,
    pub software: String,
    /// 40-bit manufacturer identifier of the System ID (0x2A23).
    pub system_id_manufacturer: u64,
    /// 24-bit OUI of the System ID (0x2A23).
    pub system_id_oui: [u8; 3],
    /// Sensor-contact state reported in the HR flags.
    pub contact_detected: bool,
    /// Battery drain in percent per minute (0 = fixed level).
    pub drain_per_min: f64,
    /// RR-interval jitter in milliseconds (deterministic sine, Task 6).
    pub rr_jitter_ms: f64,
    /// Manufacturer data company id for the advertisement.
    pub mfr_company: u16,
    /// Manufacturer data payload bytes for the advertisement.
    pub mfr_payload: Vec<u8>,
    /// Pairing/bonding policy.
    pub pair_policy: PairPolicy,
    /// ECG waveform source.
    pub ecg_source: EcgSource,
    /// Scripted heart-rate curve (empty = fixed bpm).
    pub bpm_curve: Vec<BpmStep>,
    /// Profile file this config was loaded from, if any.
    pub profile_path: Option<String>,
    /// Heart-rate notification rate in Hz.
    pub hr_hz: f64,
    /// ECG samples per data frame.
    pub ecg_frame_samples: usize,
    /// ECG frames per second (samples/s ≈ frames × samples).
    pub ecg_frames_per_sec: f64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            name: gatt_spec::DEFAULT_ADV_NAME.to_string(),
            bpm: 72,
            battery_percent: 85,
            manufacturer: "Polar Electro Oy".to_string(),
            model: "H10".to_string(),
            serial: "SIM000001".to_string(),
            firmware: "3.2.1".to_string(),
            hardware: "9".to_string(),
            software: "3.2.1".to_string(),
            system_id_manufacturer: 1,
            system_id_oui: [0x6B, 0x00, 0x00],
            contact_detected: true,
            drain_per_min: 0.0,
            rr_jitter_ms: 0.0,
            mfr_company: gatt_spec::POLAR_COMPANY_ID,
            mfr_payload: Vec::new(),
            pair_policy: PairPolicy::JustWorks,
            ecg_source: EcgSource::Synthetic,
            bpm_curve: Vec::new(),
            profile_path: None,
            hr_hz: 1.0,
            ecg_frame_samples: 65,
            ecg_frames_per_sec: 2.0,
        }
    }
}

/// What handling a PMD write asks the transport to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PmdAction {
    None,
    StartEcg,
    StopEcg,
}

/// The answer to a PMD control-point write: bytes to indicate back, plus the
/// streaming action. `indicate: None` means the write was malformed and must
/// be logged without answering (never silently dropped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmdWriteOutcome {
    pub indicate: Option<Vec<u8>>,
    pub action: PmdAction,
}

/// Mutable simulator state: config plus faults.
#[derive(Debug)]
pub struct SimState {
    pub config: SimConfig,
    /// When set, notifications stop but the link stays up.
    pub silent: bool,
    /// Whether the ECG stream is currently running.
    pub ecg_streaming: bool,
    /// Status code forced onto the next PMD command response, then cleared.
    pub reject_next_status: Option<u8>,
    /// Running ECG sample index (130 Hz clock).
    pub ecg_sample_index: u64,
    /// Running heart-rate beat index (drives the deterministic RR jitter).
    pub hr_beat_index: u64,
    /// Fractional battery drain accumulator (percent, Task 5).
    pub battery_carry: f64,
    /// Recorded ECG replay samples (None = synthetic waveform).
    pub ecg_replay: Option<Vec<i32>>,
}

impl SimState {
    pub fn new(config: SimConfig) -> Self {
        Self {
            config,
            silent: false,
            ecg_streaming: false,
            reject_next_status: None,
            ecg_sample_index: 0,
            hr_beat_index: 0,
            battery_carry: 0.0,
            ecg_replay: None,
        }
    }

    /// RR interval for one beat: the base `60/bpm` plus the configured
    /// jitter as a deterministic sine of the beat index (period 10 beats).
    /// Pure and allocation-free, so tests pin exact bytes without a radio.
    pub fn rr_interval_s(bpm: u8, jitter_ms: f64, beat: u64) -> f64 {
        let base = if bpm == 0 { 0.0 } else { 60.0 / f64::from(bpm) };
        if jitter_ms <= 0.0 || base <= 0.0 {
            return base;
        }
        base + (jitter_ms / 1000.0) * (2.0 * std::f64::consts::PI * beat as f64 / 10.0).sin()
    }

    /// BPM selected by the scripted curve at `elapsed_s` (last step at or
    /// before now wins; order-independent). Empty curve keeps the fixed bpm.
    pub fn curve_bpm(&self, elapsed_s: f64) -> u8 {
        let mut bpm = self.config.bpm;
        let mut best_at = f64::NEG_INFINITY;
        for step in &self.config.bpm_curve {
            if elapsed_s >= step.at_s && step.at_s >= best_at {
                bpm = step.bpm;
                best_at = step.at_s;
            }
        }
        bpm
    }

    /// Current Heart Rate Measurement payload (one RR interval from the bpm).
    /// Contact flags follow the H10: supported + detected (`0x06`), or
    /// supported but lost (`0x04`). Advances the beat index, so each call is
    /// the next beat.
    pub fn hr_payload(&mut self) -> Vec<u8> {
        let beat = self.hr_beat_index;
        self.hr_beat_index = beat.saturating_add(1);
        let rr_s = Self::rr_interval_s(self.config.bpm, self.config.rr_jitter_ms, beat);
        gatt_spec::encode_hr_measurement_with_contact(
            self.config.bpm,
            &[rr_s],
            self.config.contact_detected,
        )
    }

    /// Bytes returned by a PMD control-point read: the feature set.
    pub fn pmd_features(&self) -> Vec<u8> {
        gatt_spec::encode_pmd_features()
    }

    /// Device Information / Battery read handler. `None` means the
    /// characteristic does not exist on an H10 (notably PnP ID 0x2A50).
    pub fn static_read(&self, char_uuid16: u16) -> Option<Vec<u8>> {
        match char_uuid16 {
            x if x == gatt_spec::uuid16::MANUFACTURER_NAME => {
                Some(self.config.manufacturer.as_bytes().to_vec())
            }
            x if x == gatt_spec::uuid16::MODEL_NUMBER => {
                Some(self.config.model.as_bytes().to_vec())
            }
            x if x == gatt_spec::uuid16::SERIAL_NUMBER => {
                Some(self.config.serial.as_bytes().to_vec())
            }
            x if x == gatt_spec::uuid16::FIRMWARE_REVISION => {
                Some(self.config.firmware.as_bytes().to_vec())
            }
            x if x == gatt_spec::uuid16::HARDWARE_REVISION => {
                Some(self.config.hardware.as_bytes().to_vec())
            }
            x if x == gatt_spec::uuid16::SOFTWARE_REVISION => {
                Some(self.config.software.as_bytes().to_vec())
            }
            x if x == gatt_spec::uuid16::SYSTEM_ID => Some(gatt_spec::encode_system_id(
                self.config.system_id_manufacturer,
                self.config.system_id_oui,
            )),
            x if x == gatt_spec::uuid16::BODY_SENSOR_LOCATION => {
                Some(gatt_spec::encode_body_sensor_location())
            }
            x if x == gatt_spec::uuid16::BATTERY_LEVEL => {
                Some(gatt_spec::encode_battery_level(self.config.battery_percent))
            }
            _ => None,
        }
    }

    /// Applies battery drain for `elapsed_s` seconds. Returns true when the
    /// reported level changed (fractional drain accumulates in
    /// `battery_carry`); the level saturates at zero.
    pub fn tick_battery(&mut self, elapsed_s: f64) -> bool {
        if self.config.drain_per_min <= 0.0 || elapsed_s <= 0.0 {
            return false;
        }
        if self.config.battery_percent == 0 {
            return false;
        }
        self.battery_carry += self.config.drain_per_min * elapsed_s / 60.0;
        let whole = self.battery_carry.floor() as u8;
        if whole == 0 {
            return false;
        }
        self.battery_carry -= f64::from(whole);
        let next = self.config.battery_percent.saturating_sub(whole);
        let changed = next != self.config.battery_percent;
        self.config.battery_percent = next;
        if next == 0 {
            self.battery_carry = 0.0;
        }
        changed
    }

    /// JSON snapshot of the live state for `get-state`.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.config.name,
            "bpm": self.config.bpm,
            "batteryPercent": self.config.battery_percent,
            "contactDetected": self.config.contact_detected,
            "pairPolicy": self.config.pair_policy.as_str(),
            "profile": self.config.profile_path,
            "silent": self.silent,
            "ecgStreaming": self.ecg_streaming,
            "rejectNextPmd": self.reject_next_status,
            "hrHz": self.config.hr_hz,
            "ecgFramesPerSec": self.config.ecg_frames_per_sec,
            "ecgFrameSamples": self.config.ecg_frame_samples,
            "ecgSampleIndex": self.ecg_sample_index,
        })
    }

    /// Handles a PMD control-point write, returning the indicate payload.
    pub fn handle_pmd_write(&mut self, bytes: &[u8]) -> PmdWriteOutcome {
        let Some(&op) = bytes.first() else {
            return PmdWriteOutcome {
                indicate: None,
                action: PmdAction::None,
            };
        };
        let measurement_type = bytes
            .get(1)
            .copied()
            .unwrap_or(gatt_spec::PMD_MEASUREMENT_ECG);
        let answer = |status: u8, params: &[u8], action: PmdAction| PmdWriteOutcome {
            indicate: Some(gatt_spec::encode_pmd_response(
                op,
                measurement_type,
                status,
                false,
                params,
            )),
            action,
        };
        if let Some(status) = self.reject_next_status.take() {
            return answer(status, &[], PmdAction::None);
        }
        if !matches!(
            op,
            gatt_spec::PMD_OP_GET_SETTINGS | gatt_spec::PMD_OP_START | gatt_spec::PMD_OP_STOP
        ) {
            return answer(gatt_spec::PMD_STATUS_INVALID_OP, &[], PmdAction::None);
        }
        // Known-but-unsupported types (PPG/ACC/PPI are valid Polar types but not
        // on an H10) report NOT_SUPPORTED; anything else is not a measurement.
        match measurement_type & 0x3F {
            x if x == gatt_spec::PMD_MEASUREMENT_ECG => {}
            0x01..=0x03 => {
                return answer(gatt_spec::PMD_STATUS_NOT_SUPPORTED, &[], PmdAction::None);
            }
            _ => {
                return answer(
                    gatt_spec::PMD_STATUS_INVALID_MEASUREMENT_TYPE,
                    &[],
                    PmdAction::None,
                );
            }
        }
        match op {
            gatt_spec::PMD_OP_GET_SETTINGS => answer(
                gatt_spec::PMD_STATUS_SUCCESS,
                &gatt_spec::encode_ecg_settings(),
                PmdAction::None,
            ),
            gatt_spec::PMD_OP_START => match validate_start_settings(&bytes[2..]) {
                Ok(()) => {
                    self.ecg_streaming = true;
                    answer(gatt_spec::PMD_STATUS_SUCCESS, &[], PmdAction::StartEcg)
                }
                Err(status) => answer(status, &[], PmdAction::None),
            },
            gatt_spec::PMD_OP_STOP => {
                self.ecg_streaming = false;
                answer(gatt_spec::PMD_STATUS_SUCCESS, &[], PmdAction::StopEcg)
            }
            _ => unreachable!("op validity is checked above"),
        }
    }
}

/// Validates the settings TLV of a PMD start command against the H10's fixed
/// ECG configuration (130 Hz / 14 bit). Returns the status code to report.
fn validate_start_settings(tlv: &[u8]) -> Result<(), u8> {
    let mut offset = 0;
    while offset < tlv.len() {
        let setting = tlv[offset];
        let Some(&count) = tlv.get(offset + 1) else {
            return Err(gatt_spec::PMD_STATUS_INVALID_LENGTH);
        };
        let value_at = |index: usize| -> Option<u16> {
            let base = offset + 2 + index * 2;
            Some(u16::from_le_bytes([*tlv.get(base)?, *tlv.get(base + 1)?]))
        };
        match setting {
            0x00 => {
                for index in 0..usize::from(count) {
                    let Some(rate) = value_at(index) else {
                        return Err(gatt_spec::PMD_STATUS_INVALID_LENGTH);
                    };
                    if rate != gatt_spec::H10_ECG_SAMPLE_RATE_HZ {
                        return Err(gatt_spec::PMD_STATUS_INVALID_SAMPLE_RATE);
                    }
                }
                offset += 2 + usize::from(count) * 2;
            }
            0x01 => {
                for index in 0..usize::from(count) {
                    let Some(resolution) = value_at(index) else {
                        return Err(gatt_spec::PMD_STATUS_INVALID_LENGTH);
                    };
                    if resolution != gatt_spec::H10_ECG_RESOLUTION_BITS {
                        return Err(gatt_spec::PMD_STATUS_INVALID_RESOLUTION);
                    }
                }
                offset += 2 + usize::from(count) * 2;
            }
            _ => {
                // Other setting kinds are length-prefixed but opaque here: skip
                // only what the count byte promises for known 2-byte fields is
                // unsafe, so an unknown setting ends validation as malformed.
                return Err(gatt_spec::PMD_STATUS_INVALID_LENGTH);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> SimState {
        SimState::new(SimConfig::default())
    }

    #[test]
    fn hr_payload_carries_configured_bpm() {
        let mut sim = state();
        sim.config.bpm = 96;
        let payload = sim.hr_payload();
        assert_eq!(payload[1], 96);
        assert_eq!(
            payload[0] & gatt_spec::HR_FLAG_RR_PRESENT,
            gatt_spec::HR_FLAG_RR_PRESENT
        );
    }

    #[test]
    fn drain_applies_per_minute_and_floors_at_zero() {
        let mut sim = state();
        sim.config.battery_percent = 10;
        sim.config.drain_per_min = 6.0;
        assert!(sim.tick_battery(30.0), "3 points drained: changed");
        assert_eq!(sim.config.battery_percent, 7);
        assert!(!sim.tick_battery(0.0), "no time passes: unchanged");
        assert!(sim.tick_battery(600.0), "drains to zero: changed");
        assert_eq!(sim.config.battery_percent, 0);
        assert!(!sim.tick_battery(60.0), "already zero: unchanged");
    }

    #[test]
    fn rr_jitter_varies_intervals_deterministically() {
        let with_jitter = |beat: u64| SimState::rr_interval_s(60, 50.0, beat);
        assert_eq!(with_jitter(0), with_jitter(10), "sine period is 10 beats");
        assert_ne!(with_jitter(0), with_jitter(3), "adjacent beats differ");
        assert_eq!(
            SimState::rr_interval_s(60, 0.0, 0),
            1.0,
            "no jitter: exact base interval"
        );
        let mut a = state();
        a.config.bpm = 60;
        a.config.rr_jitter_ms = 50.0;
        let first = a.hr_payload();
        let mut b = state();
        b.config.bpm = 60;
        b.config.rr_jitter_ms = 50.0;
        assert_eq!(first, b.hr_payload(), "same beat index, same bytes");
        assert_ne!(first, a.hr_payload(), "next beat differs under jitter");
    }

    #[test]
    fn bpm_curve_advances_with_elapsed_time() {
        let mut sim = state();
        sim.config.bpm = 72;
        sim.config.bpm_curve = vec![
            BpmStep { at_s: 0.0, bpm: 60 },
            BpmStep {
                at_s: 30.0,
                bpm: 120,
            },
        ];
        assert_eq!(sim.curve_bpm(0.0), 60);
        assert_eq!(sim.curve_bpm(29.9), 60);
        assert_eq!(sim.curve_bpm(30.0), 120);
        assert_eq!(sim.curve_bpm(3600.0), 120);
        assert_eq!(state().curve_bpm(999.0), 72, "empty curve: fixed bpm");
    }

    #[test]
    fn contact_lost_clears_detected_bit_but_keeps_supported() {
        let mut sim = state();
        sim.config.contact_detected = true;
        assert_eq!(sim.hr_payload()[0] & 0x06, 0x06);
        sim.config.contact_detected = false;
        assert_eq!(sim.hr_payload()[0] & 0x06, 0x04);
    }

    #[test]
    fn pmd_read_reports_features() {
        assert_eq!(state().pmd_features(), gatt_spec::encode_pmd_features());
    }

    #[test]
    fn dis_reads_match_h10_strings_and_omit_pnp_id() {
        let sim = state();
        let text = |uuid: u16| String::from_utf8(sim.static_read(uuid).unwrap()).unwrap();
        assert_eq!(
            text(gatt_spec::uuid16::MANUFACTURER_NAME),
            "Polar Electro Oy"
        );
        assert_eq!(text(gatt_spec::uuid16::MODEL_NUMBER), "H10");
        assert!(sim.static_read(gatt_spec::uuid16::SERIAL_NUMBER).is_some());
        assert!(sim
            .static_read(gatt_spec::uuid16::FIRMWARE_REVISION)
            .is_some());
        assert!(sim
            .static_read(gatt_spec::uuid16::HARDWARE_REVISION)
            .is_some());
        assert!(sim
            .static_read(gatt_spec::uuid16::SOFTWARE_REVISION)
            .is_some());
        assert_eq!(
            sim.static_read(gatt_spec::uuid16::SYSTEM_ID).unwrap().len(),
            8
        );
        assert_eq!(
            sim.static_read(gatt_spec::uuid16::BATTERY_LEVEL).unwrap(),
            vec![85]
        );
        assert_eq!(sim.static_read(0x2A50), None, "H10 omits PnP ID");
    }

    #[test]
    fn get_settings_returns_ecg_settings() {
        let mut sim = state();
        let outcome = sim.handle_pmd_write(&[0x01, 0x00]);
        assert_eq!(outcome.action, PmdAction::None);
        let expected = gatt_spec::encode_pmd_response(
            0x01,
            0x00,
            gatt_spec::PMD_STATUS_SUCCESS,
            false,
            &gatt_spec::encode_ecg_settings(),
        );
        assert_eq!(outcome.indicate, Some(expected));
    }

    #[test]
    fn start_ecg_with_sdk_bytes_streams() {
        let mut sim = state();
        let outcome =
            sim.handle_pmd_write(&[0x02, 0x00, 0x00, 0x01, 0x82, 0x00, 0x01, 0x01, 0x0E, 0x00]);
        assert_eq!(outcome.action, PmdAction::StartEcg);
        assert_eq!(
            outcome.indicate,
            Some(gatt_spec::encode_pmd_response(
                0x02,
                0x00,
                gatt_spec::PMD_STATUS_SUCCESS,
                false,
                &[]
            ))
        );
        assert!(sim.ecg_streaming);
    }

    #[test]
    fn start_ecg_rejects_wrong_sample_rate_and_resolution() {
        let mut sim = state();
        let rate = sim.handle_pmd_write(&[0x02, 0x00, 0x00, 0x01, 0x64, 0x00]);
        assert_eq!(rate.action, PmdAction::None);
        assert_eq!(
            rate.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_INVALID_SAMPLE_RATE
        );
        assert!(!sim.ecg_streaming);
        let resolution = sim.handle_pmd_write(&[0x02, 0x00, 0x01, 0x01, 0x08, 0x00]);
        assert_eq!(
            resolution.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_INVALID_RESOLUTION
        );
    }

    #[test]
    fn stop_ecg_halts_the_stream() {
        let mut sim = state();
        sim.ecg_streaming = true;
        let outcome = sim.handle_pmd_write(&[0x03, 0x00]);
        assert_eq!(outcome.action, PmdAction::StopEcg);
        assert_eq!(
            outcome.indicate,
            Some(gatt_spec::encode_pmd_response(
                0x03,
                0x00,
                gatt_spec::PMD_STATUS_SUCCESS,
                false,
                &[]
            ))
        );
    }

    #[test]
    fn reject_next_fault_fires_once_with_given_status() {
        let mut sim = state();
        sim.reject_next_status = Some(gatt_spec::PMD_STATUS_NOT_SUPPORTED);
        let first = sim.handle_pmd_write(&[0x01, 0x00]);
        assert_eq!(
            first.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_NOT_SUPPORTED
        );
        assert_eq!(first.action, PmdAction::None);
        assert_eq!(
            sim.reject_next_status, None,
            "fault must clear after firing"
        );
        let second = sim.handle_pmd_write(&[0x01, 0x00]);
        assert_eq!(
            second.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_SUCCESS
        );
    }

    #[test]
    fn unknown_op_and_type_are_reported_not_dropped() {
        let mut sim = state();
        let unknown_op = sim.handle_pmd_write(&[0x09, 0x00]);
        assert_eq!(
            unknown_op.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_INVALID_OP
        );
        let unsupported = sim.handle_pmd_write(&[0x01, 0x01]);
        assert_eq!(
            unsupported.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_NOT_SUPPORTED,
            "PPG is valid but not on an H10"
        );
        let invalid = sim.handle_pmd_write(&[0x01, 0x3F]);
        assert_eq!(
            invalid.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_INVALID_MEASUREMENT_TYPE
        );
        assert_eq!(
            sim.handle_pmd_write(&[]).indicate,
            None,
            "empty write has nothing to echo"
        );
    }
}
