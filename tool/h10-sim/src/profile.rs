//! Device profiles: one JSON file defines every identity and state field.
//!
//! [`load_profile`] reads the file and fails loudly (missing file or bad
//! JSON names the path — never a silent fallback); [`SimConfig::from_profile`]
//! copies every field into the runtime config. CLI flags override the profile
//! afterwards (see `main.rs`).

use serde::Deserialize;

use crate::sim::{BpmStep, EcgSource, HrSource, PairPolicy, SimConfig};

/// Device Information service strings + System ID parts.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceInformation {
    pub manufacturer: String,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub hardware: String,
    pub software: String,
    pub system_id_manufacturer: u64,
    pub system_id_oui: [u8; 3],
}

/// Battery level plus the optional drain simulation.
#[derive(Debug, Clone, Deserialize)]
pub struct BatteryProfile {
    pub level: u8,
    pub drain_per_min: f64,
}

/// Advertisement identity.
#[derive(Debug, Clone, Deserialize)]
pub struct AdvertisingProfile {
    pub name: String,
    pub manufacturer_company: u16,
    pub manufacturer_payload_hex: String,
}

/// Heart-rate state.
#[derive(Debug, Clone, Deserialize)]
pub struct HeartRateProfile {
    pub bpm: u8,
    /// Whether the HR flags carry contact bits at all (the strap reports
    /// contact not supported; older profiles without this field parse as
    /// strap-faithful `false`).
    #[serde(default)]
    pub contact_supported: bool,
    pub contact_detected: bool,
    pub rr_jitter_ms: f64,
    #[serde(default)]
    pub scenario: crate::heartbeat::Scenario,
    pub bpm_curve: Vec<BpmStep>,
    /// Recorded HR replay (`{"file": path}`); defaults to synthetic.
    #[serde(default)]
    pub hr_source: HrSource,
}

/// PMD behaviour.
#[derive(Debug, Clone, Deserialize)]
pub struct PmdProfile {
    pub pair_policy: PairPolicy,
    pub ecg_source: EcgSource,
}

/// A complete simulated device: every identity and state field.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceProfile {
    pub device_information: DeviceInformation,
    pub battery: BatteryProfile,
    pub advertising: AdvertisingProfile,
    pub heart_rate: HeartRateProfile,
    pub pmd: PmdProfile,
}

/// Reads a profile file. Any failure names the path.
pub fn load_profile(path: &str) -> Result<DeviceProfile, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read profile {path}: {error}"))?;
    parse_profile(&text, path)
}

/// Parses profile JSON from `origin` (a path or `<builtin>`). Shared by file
/// loads and the compiled-in stock default so both validate identically.
pub fn parse_profile(text: &str, origin: &str) -> Result<DeviceProfile, String> {
    let profile: DeviceProfile = serde_json::from_str(text)
        .map_err(|error| format!("cannot parse profile {origin}: {error}"))?;
    crate::advertisement::validate_simulator_name(&profile.advertising.name)
        .map_err(|error| format!("profile {origin}: {error}"))?;
    if profile.battery.level > 100 {
        return Err(format!(
            "profile {origin}: battery.level {} is out of range 0..=100",
            profile.battery.level
        ));
    }
    if profile.battery.drain_per_min < 0.0 {
        return Err(format!(
            "profile {origin}: battery.drain_per_min {} must be >= 0",
            profile.battery.drain_per_min
        ));
    }
    Ok(profile)
}

/// Decodes lowercase hex (empty = no payload). Loud on odd length/bad digit.
pub fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err(format!("hex {hex:?} has odd length"));
    }
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            let text =
                std::str::from_utf8(pair).map_err(|_| format!("hex {hex:?} is not ASCII"))?;
            u8::from_str_radix(text, 16)
                .map_err(|_| format!("hex {hex:?} has a non-hex digit in {text:?}"))
        })
        .collect()
}

impl SimConfig {
    /// Builds the runtime config from a profile: every field is copied, so a
    /// profile fully determines the simulated device. Bad hex is an Err —
    /// never a silent empty payload.
    pub fn from_profile(profile: &DeviceProfile) -> Result<Self, String> {
        if profile.heart_rate.bpm == 0
            || !profile.heart_rate.rr_jitter_ms.is_finite()
            || profile.heart_rate.rr_jitter_ms < 0.0
            || profile.heart_rate.bpm_curve.len() > crate::heartbeat::MAX_CURVE_STEPS
            || profile
                .heart_rate
                .bpm_curve
                .iter()
                .any(|step| !step.at_s.is_finite() || step.at_s < 0.0 || step.bpm == 0)
        {
            return Err("heartbeat requires positive BPM, finite nonnegative jitter/times and at most 128 curve steps".into());
        }
        if profile.heart_rate.scenario != crate::heartbeat::Scenario::Steady
            && !profile.heart_rate.bpm_curve.is_empty()
        {
            return Err("choose a heartbeat scenario or bpm_curve, not both".into());
        }
        crate::advertisement::validate_simulator_name(&profile.advertising.name)?;
        let info = &profile.device_information;
        let mfr_payload = decode_hex(&profile.advertising.manufacturer_payload_hex)?;
        Ok(Self {
            name: profile.advertising.name.clone(),
            bpm: profile.heart_rate.bpm,
            battery_percent: profile.battery.level,
            manufacturer: info.manufacturer.clone(),
            model: info.model.clone(),
            serial: info.serial.clone(),
            firmware: info.firmware.clone(),
            hardware: info.hardware.clone(),
            software: info.software.clone(),
            system_id_manufacturer: info.system_id_manufacturer,
            system_id_oui: info.system_id_oui,
            contact_supported: profile.heart_rate.contact_supported,
            contact_detected: profile.heart_rate.contact_detected,
            drain_per_min: profile.battery.drain_per_min,
            rr_jitter_ms: profile.heart_rate.rr_jitter_ms,
            scenario: profile.heart_rate.scenario,
            mfr_company: profile.advertising.manufacturer_company,
            mfr_payload,
            pair_policy: profile.pmd.pair_policy,
            ecg_source: profile.pmd.ecg_source.clone(),
            hr_source: profile.heart_rate.hr_source.clone(),
            bpm_curve: profile.heart_rate.bpm_curve.clone(),
            profile_path: None,
            ..Self::default()
        })
    }

    /// Applies a loaded profile onto the live config, recording its path.
    pub fn apply_profile(&mut self, path: &str, profile: &DeviceProfile) -> Result<(), String> {
        let mut config =
            Self::from_profile(profile).map_err(|error| format!("profile {path}: {error}"))?;
        config.profile_path = Some(path.to_string());
        *self = config;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synchronized_scenario_profiles_validate_and_use_both_synthetic_sources() {
        for (text, scenario) in [
            (
                include_str!("../profiles/synthetic-steady.json"),
                crate::heartbeat::Scenario::Steady,
            ),
            (
                include_str!("../profiles/synthetic-breathing.json"),
                crate::heartbeat::Scenario::Breathing,
            ),
            (
                include_str!("../profiles/synthetic-exercise-recovery.json"),
                crate::heartbeat::Scenario::ExerciseRecovery,
            ),
            (
                include_str!("../profiles/synthetic-abrupt.json"),
                crate::heartbeat::Scenario::Abrupt,
            ),
        ] {
            let profile = parse_profile(text, "scenario").unwrap();
            let config = SimConfig::from_profile(&profile).unwrap();
            assert_eq!(config.scenario, scenario);
            assert_eq!(config.ecg_source, EcgSource::Synthetic);
            assert_eq!(config.hr_source, HrSource::Synthetic);
        }
    }

    #[test]
    fn heartbeat_configuration_rejects_unbounded_and_invalid_curves() {
        let mut profile =
            parse_profile(include_str!("../profiles/stock-h10.json"), "stock").unwrap();
        profile.heart_rate.bpm = 0;
        assert!(SimConfig::from_profile(&profile).is_err());
        profile.heart_rate.bpm = 72;
        for jitter in [f64::NAN, f64::INFINITY, -1.0] {
            profile.heart_rate.rr_jitter_ms = jitter;
            assert!(SimConfig::from_profile(&profile).is_err());
        }
        profile.heart_rate.rr_jitter_ms = 0.0;
        profile.heart_rate.bpm_curve = vec![BpmStep { at_s: 0.0, bpm: 72 }; 129];
        assert!(SimConfig::from_profile(&profile).is_err());
        for at_s in [f64::NAN, f64::INFINITY, -1.0] {
            profile.heart_rate.bpm_curve = vec![BpmStep { at_s, bpm: 72 }];
            assert!(SimConfig::from_profile(&profile).is_err());
        }
        profile.heart_rate.bpm_curve = vec![BpmStep { at_s: 1.0, bpm: 72 }];
        profile.heart_rate.scenario = crate::heartbeat::Scenario::Breathing;
        assert!(SimConfig::from_profile(&profile).is_err());
    }

    #[test]
    fn default_and_bundled_profiles_start_with_sim() {
        for source in [
            include_str!("../profiles/stock-h10.json"),
            include_str!("../profiles/low-battery-legacy.json"),
        ] {
            let profile = parse_profile(source, "<bundled>").unwrap();
            assert!(profile.advertising.name.starts_with("SIM"));
        }
        assert!(crate::gatt_spec::DEFAULT_ADV_NAME.starts_with("SIM"));
    }

    #[test]
    fn non_sim_profile_names_are_rejected_before_mutation() {
        for name in [
            "Polar H10 E997042F",
            "Polar H10 SIM0001",
            "sim H10",
            "",
            " SIM H10",
        ] {
            let mut profile: serde_json::Value =
                serde_json::from_str(include_str!("../profiles/stock-h10.json")).unwrap();
            profile["advertising"]["name"] = serde_json::json!(name);
            let error = parse_profile(&profile.to_string(), "bad-name.json")
                .expect_err("unsafe simulator name accepted");
            assert!(error.contains("bad-name.json") && error.contains("SIM"));
        }
    }

    #[test]
    fn stock_profile_loads_and_builds_config() {
        let profile = load_profile("profiles/stock-h10.json").unwrap();
        assert_eq!(profile.device_information.model, "H10");
        assert_eq!(profile.device_information.firmware, "5.0.0");
        assert_eq!(profile.device_information.hardware, "00760690.03");
        assert_eq!(profile.device_information.software, "4.2.0");
        let config = SimConfig::from_profile(&profile).unwrap();
        assert_eq!(config.name, "SIM Polar H10 0001");
        assert_eq!(config.battery_percent, 90);
        assert!(!config.contact_supported);
    }

    #[test]
    fn alternate_profile_is_low_battery_legacy_unit() {
        let profile = load_profile("profiles/low-battery-legacy.json").unwrap();
        let config = SimConfig::from_profile(&profile).unwrap();
        assert_eq!(config.battery_percent, 15);
        assert_eq!(config.firmware, "1.5.9");
        assert_ne!(config.serial, SimConfig::default().serial);
    }

    #[test]
    fn bad_profile_fails_loudly_with_path() {
        assert!(load_profile("profiles/does-not-exist.json")
            .unwrap_err()
            .contains("profiles/does-not-exist.json"));
    }

    #[test]
    fn hex_decode_rejects_odd_length_and_bad_digits() {
        assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());
        assert_eq!(decode_hex("6b00").unwrap(), vec![0x6B, 0x00]);
        assert!(decode_hex("abc").is_err());
        assert!(decode_hex("zz").is_err());
    }
}
