//! Semi-random timing model for the H10 simulator.
//!
//! The simulator must behave like a real strap: notification intervals,
//! PMD command latencies and frame jitter vary from beat to beat. This
//! module loads those variations from a measured capture (the `timings`
//! section of an `h10-capture` fingerprint, see
//! `examples-shared/driver/scenarios/h10-capture.ts`) and samples
//! semi-random delays from them. Sampling is seeded ([`SeededRng`]) so a
//! capture plus a seed reproduces a run exactly.
//!
//! The live defaults are measured, not placeholders: `main.rs` starts from
//! `profiles/timing-h10-measured.json` (fitted from the Tauri strap capture
//! in `fixtures/h10-fingerprints/`). [`TimingProfile::default_unconfirmed`]
//! keeps the original documented placeholders marked [`UNCONFIRMED`] for
//! explicit opt-in via `--timing-profile
//! profiles/timing-default-unconfirmed.json`; those sample deterministically
//! (spread 0).
//!
//! Sources:
//! - Polar BLE SDK (`polarofficial/polar-ble-sdk`, Android `BlePMDClient`):
//!   ECG streams at 130 Hz; PMD control-point commands are answered with an
//!   indicate (`PmdControlPointResponse`). The SDK documents no typical
//!   latency; the measured default comes from capture `pmdResponseMs`.
//! - Bluetooth SIG, Heart Rate Service 1.0 §3.3: the H10 notifies the Heart
//!   Rate Measurement about once per second; the measured default comes from
//!   capture `hrNotificationIntervalMs`.
//! - Bluetooth SIG, Heart Rate Service 1.0 §3.4: body sensor location chest.
//!   (Structural, not timing; pinned in `gatt_spec.rs`.)
//! - ECG frame cadence (73 samples at 130 Hz ≈ 561.6 ms, `ecg_frames_per_sec`
//!   × `ecg_frame_samples`) matches the captures' `ecgSamplesPerFrame`; the
//!   residual jitter comes from capture `ecgFrameIntervalMs`, recentered on
//!   zero (the fingerprint measures the full period).
//! - The advertising interval is the platform's answer (BlueZ/Apple own the
//!   radio); the sim never steers it. The measured default documents capture
//!   `advertisementIntervalMs`.

use serde::{Deserialize, Serialize};

/// One measured distribution, milliseconds. Field names mirror the
/// fingerprint `timings` distributions (`n, min, p10, p50, p90, max, mean,
/// stdev`) so a capture deserializes without a translation layer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TimingDistribution {
    pub n: u64,
    pub min: Option<f64>,
    pub p10: Option<f64>,
    pub p50: Option<f64>,
    pub p90: Option<f64>,
    pub max: Option<f64>,
    pub mean: Option<f64>,
    pub stdev: Option<f64>,
}

/// A sampled delay: the median of a measured distribution plus Gaussian
/// jitter scaled by its spread, floored at `min_ms`. `confirmed` is false
/// while the model is a documented placeholder; `source` always cites where
/// the numbers came from (a capture field, or `UNCONFIRMED: …`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelayModel {
    pub median_ms: f64,
    pub spread_ms: f64,
    pub min_ms: f64,
    pub confirmed: bool,
    pub source: String,
}

impl DelayModel {
    /// A documented placeholder: deterministic (spread 0) until a capture
    /// confirms it. The source must start with `UNCONFIRMED: `.
    pub fn unconfirmed(median_ms: f64, min_ms: f64, source: &str) -> Self {
        Self {
            median_ms,
            spread_ms: 0.0,
            min_ms,
            confirmed: false,
            source: source.to_string(),
        }
    }

    /// A model fitted to a measured distribution: median p50 (falling back
    /// to mean), spread stdev, floor the measured min.
    pub fn measured(distribution: &TimingDistribution, field: &str) -> Self {
        let median = distribution.p50.or(distribution.mean).unwrap_or(0.0);
        Self {
            median_ms: median,
            spread_ms: distribution.stdev.unwrap_or(0.0).max(0.0),
            min_ms: distribution.min.unwrap_or(0.0).max(0.0),
            confirmed: true,
            source: format!("capture timings.{field}"),
        }
    }

    /// Samples one delay in milliseconds. Deterministic for a placeholder
    /// (spread 0); semi-random around the median once confirmed.
    pub fn sample_ms(&self, rng: &mut SeededRng) -> f64 {
        if self.spread_ms <= 0.0 {
            return self.median_ms.max(self.min_ms);
        }
        (self.median_ms + rng.next_gaussian() * self.spread_ms).max(self.min_ms)
    }
}

/// Runtime sampler: a profile plus its seeded RNG. Delays the sim controls
/// (HR cadence, ECG frame jitter, PMD response latency) are sampled here;
/// central-side latencies (connect, discovery, MTU) are the central's own
/// answers and are never synthesized.
#[derive(Debug)]
pub struct TimingRuntime {
    pub profile: TimingProfile,
    pub rng: SeededRng,
}

impl TimingRuntime {
    pub fn new(profile: TimingProfile) -> Self {
        let rng = SeededRng::new(profile.seed);
        Self { profile, rng }
    }

    /// Seconds until the next HR notification: the measured interval once a
    /// capture confirmed it, otherwise the configured `hr_hz` rate.
    pub fn hr_interval_s(&mut self, fallback_hz: f64) -> f64 {
        if self.profile.hr_interval.confirmed {
            (self.profile.hr_interval.sample_ms(&mut self.rng) / 1000.0).max(0.05)
        } else {
            1.0 / fallback_hz.max(0.1)
        }
    }

    /// Seconds until the next ECG frame: the nominal frame cadence plus the
    /// measured jitter once a capture confirmed it.
    pub fn ecg_interval_s(&mut self, fallback_frames_per_sec: f64) -> f64 {
        let base = 1.0 / fallback_frames_per_sec.max(0.5);
        if self.profile.ecg_frame_jitter.confirmed {
            (base + self.profile.ecg_frame_jitter.sample_ms(&mut self.rng) / 1000.0).max(0.01)
        } else {
            base
        }
    }

    /// Samples the PMD response latency in milliseconds: `None` means answer
    /// inline (unconfirmed profile or a zero sample — default behaviour is
    /// unchanged); `Some(ms)` means the indication is due that far in the
    /// future. The caller schedules it on the tick loop; nothing here sleeps,
    /// so a measured latency never blocks the event loop.
    pub fn sample_pmd_response_ms(&mut self) -> Option<f64> {
        if !self.profile.pmd_response.confirmed {
            return None;
        }
        let ms = self.profile.pmd_response.sample_ms(&mut self.rng).max(0.0);
        if ms > 0.0 {
            Some(ms)
        } else {
            None
        }
    }
}

/// SplitMix64: a tiny seeded PRNG with no dependencies. The same seed
/// replays the same delay sequence, so a capture plus a seed reproduces a
/// run exactly. (Reference: Steele, Lea & O'Neill 2014, "Fast Splittable
/// Pseudorandom Number Generators".)
#[derive(Debug, Clone)]
pub struct SeededRng(u64);

impl SeededRng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    /// Uniform in [0, 1).
    pub fn next_unit(&mut self) -> f64 {
        const SCALE: f64 = (1u64 << 53) as f64;
        ((self.next_u64() >> 11) as f64) / SCALE
    }

    /// Standard normal (mean 0, sigma 1) via Box–Muller. A zero uniform is
    /// re-drawn so the logarithm never sees 0 — never a silent NaN.
    pub fn next_gaussian(&mut self) -> f64 {
        let mut first = self.next_unit();
        while first <= 0.0 {
            first = self.next_unit();
        }
        let second = self.next_unit();
        (-2.0 * first.ln()).sqrt() * (2.0 * std::f64::consts::PI * second).cos()
    }
}

/// The simulator's timing behaviour: one delay model per modulated delay.
/// Serializes to `profiles/timing-default-unconfirmed.json` (pinned by test).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimingProfile {
    pub seed: u64,
    pub hr_interval: DelayModel,
    pub pmd_response: DelayModel,
    pub ecg_frame_jitter: DelayModel,
    pub advertising_interval: DelayModel,
}

impl TimingProfile {
    /// Documented Polar values until real captures exist. Every model is an
    /// [`UNCONFIRMED`] placeholder with spread 0, so sampling is
    /// deterministic and default over-the-air behaviour does not change.
    pub fn default_unconfirmed(seed: u64) -> Self {
        Self {
            seed,
            hr_interval: DelayModel::unconfirmed(
                1000.0,
                900.0,
                "UNCONFIRMED: H10 notifies HR ~1 Hz (SIG HRS 1.0 \u{00a7}3.3; sim hr_hz=1.0); confirm from capture timings.hrNotificationIntervalMs",
            ),
            pmd_response: DelayModel::unconfirmed(
                0.0,
                0.0,
                "UNCONFIRMED: sim answers PMD locally with no measured latency (Polar SDK BlePMDClient documents no typical latency); confirm from capture timings.pmdResponseMs",
            ),
            ecg_frame_jitter: DelayModel::unconfirmed(
                0.0,
                0.0,
                "UNCONFIRMED: no measured frame jitter around the nominal cadence (legacy 2 frames/s x 65 samples reference; the strap runs 73 samples at 130 Hz); confirm from capture timings.ecgFrameIntervalMs",
            ),
            advertising_interval: DelayModel::unconfirmed(
                100.0,
                0.0,
                "UNCONFIRMED: advertising interval is the platform radio's answer, never the sim's; confirm from capture timings.advertisementIntervalMs",
            ),
        }
    }

    /// True only when every model came from a measurement.
    pub fn fully_confirmed(&self) -> bool {
        self.hr_interval.confirmed
            && self.pmd_response.confirmed
            && self.ecg_frame_jitter.confirmed
            && self.advertising_interval.confirmed
    }

    /// Names of the models still waiting on a capture.
    pub fn unconfirmed_fields(&self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if !self.hr_interval.confirmed {
            fields.push("hr_interval");
        }
        if !self.pmd_response.confirmed {
            fields.push("pmd_response");
        }
        if !self.ecg_frame_jitter.confirmed {
            fields.push("ecg_frame_jitter");
        }
        if !self.advertising_interval.confirmed {
            fields.push("advertising_interval");
        }
        fields
    }

    /// Fits the models the fingerprint measured, keeping UNCONFIRMED
    /// placeholders for the rest. Any structural problem names the JSON
    /// path — never a silent partial profile. Accepts either a full
    /// `h10-capture` fingerprint (`{version, timings, …}`) or a raw
    /// [`TimingProfile`] JSON document.
    pub fn from_fingerprint_json(text: &str, seed: u64) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|error| format!("timing profile is not valid JSON: {error}"))?;
        if value.get("timings").is_some() {
            Self::from_fingerprint(&value, seed)
        } else {
            let mut profile: Self = serde_json::from_value(value).map_err(|error| {
                format!("timing profile is neither a fingerprint nor a TimingProfile: {error}")
            })?;
            profile.seed = seed;
            Ok(profile)
        }
    }

    /// Fits one profile to a parsed `h10-capture` fingerprint. Fields the
    /// fingerprint did not measure (for example `advertisement` on a
    /// chooser-only host whose scan reported `ok: false`) keep their
    /// UNCONFIRMED placeholder and stay listed in [`Self::unconfirmed_fields`].
    pub fn from_fingerprint(fingerprint: &serde_json::Value, seed: u64) -> Result<Self, String> {
        let version = fingerprint
            .get("version")
            .and_then(|version| version.as_u64())
            .ok_or_else(|| "fingerprint.timings: missing numeric \"version\"".to_string())?;
        if version != 1 {
            return Err(format!(
                "fingerprint.timings: unsupported fingerprint version {version} (expected 1)"
            ));
        }
        let timings = fingerprint
            .get("timings")
            .and_then(|timings| timings.as_object())
            .ok_or_else(|| "fingerprint.timings: missing object \"timings\"".to_string())?;
        let mut profile = Self::default_unconfirmed(seed);
        if let Some(distribution) = optional_distribution(timings, "hrNotificationIntervalMs")? {
            profile.hr_interval = DelayModel::measured(&distribution, "hrNotificationIntervalMs");
        }
        if let Some(distribution) = optional_distribution(timings, "pmdResponseMs")? {
            profile.pmd_response = DelayModel::measured(&distribution, "pmdResponseMs");
        }
        if let Some(distribution) = optional_distribution(timings, "ecgFrameIntervalMs")? {
            profile.ecg_frame_jitter = DelayModel::measured(&distribution, "ecgFrameIntervalMs");
            // The fingerprint measures the full frame period; the sim adds
            // this as jitter around its nominal frame cadence, so recenter on
            // zero while keeping the measured spread. The floor recenters too:
            // keeping the measured ~561 ms minimum would add a whole period
            // to every frame. Jitter below zero clamps at zero (a frame never
            // goes backwards); the measured spread is ~0.01 ms, so the clamp
            // is nearly never felt.
            profile.ecg_frame_jitter.median_ms = 0.0;
            profile.ecg_frame_jitter.min_ms = 0.0;
        }
        let advertisement = fingerprint.get("advertisement");
        let scanned = advertisement
            .and_then(|advertisement| advertisement.get("ok"))
            .and_then(|ok| ok.as_bool())
            .unwrap_or(false);
        if scanned {
            let intervals = advertisement
                .and_then(|advertisement| advertisement.get("advertisementIntervalMs"))
                .ok_or_else(|| {
                    "fingerprint.advertisement: scanned but missing \"advertisementIntervalMs\""
                        .to_string()
                })?;
            let distribution: TimingDistribution = serde_json::from_value(intervals.clone())
                .map_err(|error| {
                    format!("fingerprint.advertisement.advertisementIntervalMs: {error}")
                })?;
            if distribution.n > 0 {
                profile.advertising_interval =
                    DelayModel::measured(&distribution, "advertisement.advertisementIntervalMs");
            }
        }
        Ok(profile)
    }
}

/// Reads an optional timing distribution: absent or null keeps the
/// placeholder; present but malformed is a loud error naming the field.
fn optional_distribution(
    timings: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Option<TimingDistribution>, String> {
    match timings.get(field) {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(None),
        Some(value) => {
            let distribution: TimingDistribution = serde_json::from_value(value.clone())
                .map_err(|error| format!("fingerprint.timings.{field}: {error}"))?;
            if distribution.n == 0 {
                return Ok(None);
            }
            Ok(Some(distribution))
        }
    }
}

/// Marker for the doc links above: a model is a placeholder until a capture
/// confirms it.
#[allow(dead_code)]
const UNCONFIRMED: &str = "UNCONFIRMED";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_replays_the_same_sequence_for_one_seed() {
        let mut first = SeededRng::new(42);
        let mut second = SeededRng::new(42);
        let mut other = SeededRng::new(43);
        let a: Vec<u64> = (0..10).map(|_| first.next_u64()).collect();
        let b: Vec<u64> = (0..10).map(|_| second.next_u64()).collect();
        assert_eq!(a, b, "same seed replays the same sequence");
        assert_ne!(a[0], other.next_u64(), "different seeds diverge");
    }

    #[test]
    fn gaussian_samples_are_finite_and_centered() {
        let mut rng = SeededRng::new(7);
        let samples: Vec<f64> = (0..1000).map(|_| rng.next_gaussian()).collect();
        assert!(samples.iter().all(|value| value.is_finite()));
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        assert!(mean.abs() < 0.15, "mean near 0, got {mean}");
    }

    #[test]
    fn unconfirmed_defaults_are_deterministic_placeholders() {
        let profile = TimingProfile::default_unconfirmed(1);
        assert!(!profile.fully_confirmed());
        assert_eq!(profile.unconfirmed_fields().len(), 4);
        let mut rng = SeededRng::new(1);
        assert_eq!(profile.hr_interval.sample_ms(&mut rng), 1000.0);
        assert_eq!(profile.pmd_response.sample_ms(&mut rng), 0.0);
        for model in [
            &profile.hr_interval,
            &profile.pmd_response,
            &profile.ecg_frame_jitter,
            &profile.advertising_interval,
        ] {
            assert!(!model.confirmed);
            assert!(
                model.source.starts_with("UNCONFIRMED: "),
                "placeholder must say UNCONFIRMED, got {:?}",
                model.source
            );
            assert_eq!(model.spread_ms, 0.0, "placeholder has no spread");
        }
    }

    #[test]
    fn pmd_latency_samples_a_delay_without_sleeping() {
        // Unconfirmed: answer inline, exactly like before.
        let mut runtime = TimingRuntime::new(TimingProfile::default_unconfirmed(1));
        assert_eq!(runtime.sample_pmd_response_ms(), None);
        // Confirmed with a positive median: a due-in duration, never a
        // sleep — the caller schedules it on the tick loop.
        let mut profile = TimingProfile::default_unconfirmed(1);
        profile.pmd_response = DelayModel {
            median_ms: 50.0,
            spread_ms: 0.0,
            min_ms: 0.0,
            confirmed: true,
            source: "test".to_string(),
        };
        let mut runtime = TimingRuntime::new(profile);
        assert_eq!(runtime.sample_pmd_response_ms(), Some(50.0));
        // Confirmed but zero: still inline.
        let mut profile = TimingProfile::default_unconfirmed(1);
        profile.pmd_response = DelayModel {
            median_ms: 0.0,
            spread_ms: 0.0,
            min_ms: 0.0,
            confirmed: true,
            source: "test".to_string(),
        };
        let mut runtime = TimingRuntime::new(profile);
        assert_eq!(runtime.sample_pmd_response_ms(), None);
    }

    #[test]
    fn fingerprint_loader_fits_measured_fields_and_keeps_placeholders() {
        let fingerprint = serde_json::json!({
            "version": 1,
            "advertisement": { "ok": false },
            "timings": {
                "hrNotificationIntervalMs": {
                    "n": 60, "min": 950.0, "p10": 970.0, "p50": 1000.0,
                    "p90": 1030.0, "max": 1060.0, "mean": 1000.0, "stdev": 25.0
                },
                "pmdResponseMs": {
                    "n": 3, "min": 8.0, "p10": 8.0, "p50": 10.0,
                    "p90": 12.0, "max": 12.0, "mean": 10.0, "stdev": 2.0
                }
            }
        });
        let profile = TimingProfile::from_fingerprint(&fingerprint, 9).unwrap();
        assert_eq!(profile.seed, 9);
        assert!(profile.hr_interval.confirmed);
        assert_eq!(profile.hr_interval.median_ms, 1000.0);
        assert!(profile.pmd_response.confirmed);
        assert!(
            !profile.ecg_frame_jitter.confirmed,
            "unmeasured stays UNCONFIRMED"
        );
        assert!(
            !profile.advertising_interval.confirmed,
            "failed scan stays UNCONFIRMED"
        );
        assert_eq!(
            profile.unconfirmed_fields(),
            vec!["ecg_frame_jitter", "advertising_interval"]
        );
    }

    #[test]
    fn fingerprint_loader_fails_loudly_on_wrong_version_or_bad_shape() {
        let no_version = serde_json::json!({"timings": {}});
        assert!(TimingProfile::from_fingerprint(&no_version, 0)
            .unwrap_err()
            .contains("version"));
        let bad_version = serde_json::json!({"version": 2, "timings": {}});
        assert!(TimingProfile::from_fingerprint(&bad_version, 0)
            .unwrap_err()
            .contains("version 2"));
        let bad_dist = serde_json::json!({
            "version": 1,
            "timings": {"hrNotificationIntervalMs": {"n": "sixty"}}
        });
        assert!(TimingProfile::from_fingerprint(&bad_dist, 0)
            .unwrap_err()
            .contains("timings.hrNotificationIntervalMs"));
    }

    #[test]
    fn runtime_falls_back_to_configured_rates_until_confirmed() {
        let mut runtime = TimingRuntime::new(TimingProfile::default_unconfirmed(5));
        assert_eq!(runtime.hr_interval_s(2.0), 0.5);
        assert_eq!(runtime.ecg_interval_s(4.0), 0.25);
    }

    #[test]
    fn ecg_jitter_recentered_around_zero_not_the_full_period() {
        // The fingerprint measures the full frame period (~561.6 ms); the
        // sim adds this model as jitter around its nominal cadence, so the
        // recentered model must sample near zero — never near the period.
        let fingerprint = serde_json::json!({
            "version": 1,
            "advertisement": {"ok": false},
            "timings": {
                "ecgFrameIntervalMs": {
                    "n": 29, "min": 561.561716, "p10": 561.561718, "p50": 561.566804,
                    "p90": 561.587148, "max": 561.58715,
                    "mean": 561.5710137241381, "stdev": 0.00916367664086846
                }
            }
        });
        let profile = TimingProfile::from_fingerprint(&fingerprint, 7).unwrap();
        assert!(profile.ecg_frame_jitter.confirmed);
        assert_eq!(profile.ecg_frame_jitter.median_ms, 0.0);
        let mut runtime = TimingRuntime::new(profile);
        for _ in 0..50 {
            let interval = runtime.ecg_interval_s(130.0 / 73.0);
            assert!(
                (interval - 73.0 / 130.0).abs() < 0.05,
                "frame interval must stay near the nominal 561.5 ms cadence, got {interval}"
            );
        }
    }

    #[test]
    fn runtime_samples_confirmed_intervals_reproducibly() {
        let fingerprint = serde_json::json!({
            "version": 1,
            "advertisement": {"ok": false},
            "timings": {
                "hrNotificationIntervalMs": {
                    "n": 60, "min": 950.0, "p10": 970.0, "p50": 1000.0,
                    "p90": 1030.0, "max": 1060.0, "mean": 1000.0, "stdev": 25.0
                }
            }
        });
        let first = TimingRuntime::new(TimingProfile::from_fingerprint(&fingerprint, 11).unwrap());
        let second = TimingRuntime::new(TimingProfile::from_fingerprint(&fingerprint, 11).unwrap());
        let (mut first, mut second) = (first, second);
        let a: Vec<f64> = (0..10).map(|_| first.hr_interval_s(1.0)).collect();
        let b: Vec<f64> = (0..10).map(|_| second.hr_interval_s(1.0)).collect();
        assert_eq!(a, b, "same seed replays the same HR cadence");
        assert!(
            a.iter().any(|value| (*value - 1.0).abs() > 0.001),
            "confirmed intervals vary"
        );
        assert!(a.iter().all(|value| *value >= 0.05), "floor holds");
    }

    #[test]
    fn checked_in_default_profile_matches_code() {
        let text = std::fs::read_to_string("profiles/timing-default-unconfirmed.json")
            .expect("checked-in default timing profile must exist");
        let file: TimingProfile =
            serde_json::from_str(&text).expect("default timing profile must parse");
        assert_eq!(file, TimingProfile::default_unconfirmed(0));
    }

    #[test]
    fn checked_in_measured_profile_matches_the_tauri_capture() {
        // The file is generated from the Tauri fingerprint; the loader is
        // the independent oracle — any transcription drift fails here.
        let profile_text = std::fs::read_to_string("profiles/timing-h10-measured.json")
            .expect("checked-in measured timing profile must exist");
        let file: TimingProfile =
            serde_json::from_str(&profile_text).expect("measured timing profile must parse");
        assert!(file.fully_confirmed(), "every model must be CONFIRMED");
        assert!(file.unconfirmed_fields().is_empty());
        let capture_text = std::fs::read_to_string(
            "fixtures/h10-fingerprints/tauri-macos-unknown-engine-E9B93D29-2026-09-19.json",
        )
        .expect("committed Tauri fingerprint must exist");
        let capture: serde_json::Value =
            serde_json::from_str(&capture_text).expect("Tauri fingerprint must parse");
        assert_eq!(
            file,
            TimingProfile::from_fingerprint(&capture, 0).expect("Tauri fingerprint must fit")
        );
    }

    #[test]
    fn measured_model_samples_around_the_median() {
        let distribution = TimingDistribution {
            n: 60,
            min: Some(900.0),
            p10: Some(950.0),
            p50: Some(1000.0),
            p90: Some(1050.0),
            max: Some(1100.0),
            mean: Some(1001.0),
            stdev: Some(40.0),
        };
        let model = DelayModel::measured(&distribution, "hrNotificationIntervalMs");
        assert!(model.confirmed);
        assert_eq!(model.median_ms, 1000.0);
        let mut rng = SeededRng::new(3);
        let samples: Vec<f64> = (0..500).map(|_| model.sample_ms(&mut rng)).collect();
        assert!(samples.iter().all(|value| *value >= 900.0), "floor holds");
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        assert!((mean - 1000.0).abs() < 15.0, "mean near median, got {mean}");
    }
}
