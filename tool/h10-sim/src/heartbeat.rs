//! Acquisition-time physiology shared by synthetic ECG and HR/RR.
//! Analytic integration avoids per-beat storage and work proportional to pauses.
use crate::sim::{BpmStep, SimConfig};
use serde::Deserialize;
use std::collections::VecDeque;
use std::f64::consts::TAU;

pub const MAX_EPOCHS: usize = 32;
pub const MAX_CURVE_STEPS: usize = 128;
pub const MAX_RR_PER_PACKET: u64 = 8;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    #[default]
    Steady,
    Breathing,
    ExerciseRecovery,
    Abrupt,
}
impl Scenario {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "steady" => Ok(Self::Steady),
            "breathing" => Ok(Self::Breathing),
            "exercise-recovery" => Ok(Self::ExerciseRecovery),
            "abrupt" => Ok(Self::Abrupt),
            _ => Err(format!("unknown heartbeat scenario {text:?}")),
        }
    }
}
#[derive(Debug, Clone)]
struct Model {
    bpm: u8,
    jitter: f64,
    scenario: Scenario,
    curve: Vec<BpmStep>,
    original_curve: Vec<BpmStep>,
}
impl Model {
    fn new(config: &SimConfig) -> Self {
        let mut curve = config.bpm_curve.clone();
        curve.sort_by(|a, b| a.at_s.total_cmp(&b.at_s));
        Self {
            bpm: config.bpm,
            jitter: config.rr_jitter_ms,
            scenario: config.scenario,
            curve,
            original_curve: config.bpm_curve.clone(),
        }
    }
    fn matches(&self, config: &SimConfig) -> bool {
        self.bpm == config.bpm
            && self.jitter == config.rr_jitter_ms
            && self.scenario == config.scenario
            && self.original_curve == config.bpm_curve
    }
    fn integral(&self, t: f64) -> f64 {
        let base = f64::from(self.bpm);
        let high = (base + 68.0).min(255.0);
        let low = (base - 12.0).max(1.0);
        let mut area = 0.0;
        let mut at = 0.0;
        let mut rate = base;
        // Steps are bounded at configuration validation, including runtime profiles.
        for step in &self.curve {
            if step.at_s > t {
                break;
            }
            area += (step.at_s - at) * rate;
            at = step.at_s;
            rate = f64::from(step.bpm);
        }
        area += (t - at) * rate;
        if self.curve.is_empty() {
            match self.scenario {
                Scenario::ExerciseRecovery => {
                    let ramp = t.min(60.0);
                    let hold = (t - 60.0).clamp(0.0, 30.0);
                    let recovery = (t - 90.0).clamp(0.0, 120.0);
                    area += (high - base)
                        * (ramp * ramp / 120.0 + hold + recovery - recovery * recovery / 240.0);
                }
                Scenario::Abrupt => {
                    area += (high - base) * (t - 30.0).clamp(0.0, 30.0)
                        + (low - base) * (t - 60.0).clamp(0.0, 30.0);
                }
                _ => {}
            }
        }
        let minimum = self.curve.iter().map(|s| f64::from(s.bpm)).fold(
            if self.scenario == Scenario::Abrupt {
                low
            } else {
                base
            },
            f64::min,
        );
        let breathing = if self.scenario == Scenario::Breathing {
            6.0_f64.min(base * 0.25)
        } else {
            0.0
        };
        let maximum = self.curve.iter().map(|s| f64::from(s.bpm)).fold(
            if matches!(self.scenario, Scenario::ExerciseRecovery | Scenario::Abrupt) {
                high
            } else {
                base
            },
            f64::max,
        );
        let amplitude = (breathing + base * base * self.jitter / 60000.0)
            .min(minimum * 0.25)
            .min(minimum - 1.0)
            .min(255.0 - maximum);
        let omega = TAU * 0.1;
        (area + amplitude * (1.0 - (omega * t).cos()) / omega) / 60.0
    }
}
#[derive(Debug, Clone)]
struct Epoch {
    at: f64,
    phase: f64,
    model: Model,
}
#[derive(Debug, Clone)]
pub struct Timeline {
    epochs: VecDeque<Epoch>,
}
impl Timeline {
    pub fn new(config: &SimConfig) -> Self {
        Self {
            epochs: VecDeque::from([Epoch {
                at: 0.0,
                phase: 0.0,
                model: Model::new(config),
            }]),
        }
    }
    pub fn sync(&mut self, config: &SimConfig, at: f64) {
        if self
            .epochs
            .back()
            .expect("timeline epoch")
            .model
            .matches(config)
        {
            return;
        }
        let phase = self.phase(at).expect("current acquisition time");
        if self.epochs.len() == MAX_EPOCHS {
            self.epochs.pop_front();
        }
        self.epochs.push_back(Epoch {
            at,
            phase,
            model: Model::new(config),
        });
    }
    pub fn phase(&self, t: f64) -> Option<f64> {
        let epoch = self.epochs.iter().rev().find(|e| t >= e.at)?;
        Some(epoch.phase + epoch.model.integral(t - epoch.at))
    }
    pub fn ecg_sample(&self, at: f64) -> Option<i32> {
        self.phase(at)
            .map(|phase| crate::ecg::ecg_sample_uv((phase + 0.22).rem_euclid(1.0), 60.0))
    }

    fn beat_time(&self, beat: u64, now: f64) -> Option<f64> {
        let mut lo = self.epochs.front()?.at;
        if self.phase(lo)? > beat as f64 {
            return None;
        }
        let mut hi = now;
        for _ in 0..56 {
            let mid = (lo + hi) * 0.5;
            if self.phase(mid)? < beat as f64 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Some((lo + hi) * 0.5)
    }
    /// Returns at most eight newly acquired intervals, shedding older backlog explicitly.
    pub fn intervals(&self, now: f64, cursor: &mut u64) -> (Vec<f64>, u64) {
        let end = self.phase(now).unwrap_or(0.0).floor() as u64;
        let first = (*cursor).max(end.saturating_sub(MAX_RR_PER_PACKET));
        let mut dropped = first.saturating_sub(*cursor);
        let mut rr = Vec::with_capacity(MAX_RR_PER_PACKET as usize);
        for beat in first.saturating_add(1)..=end {
            match (self.beat_time(beat - 1, now), self.beat_time(beat, now)) {
                (Some(a), Some(b)) => rr.push(b - a),
                _ => dropped += 1,
            }
        }
        *cursor = end;
        (rr, dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extreme_rates_keep_rr_encodable_without_clipping() {
        for bpm in [1, 2, 254, 255] {
            for scenario in [
                Scenario::Steady,
                Scenario::Breathing,
                Scenario::ExerciseRecovery,
                Scenario::Abrupt,
            ] {
                let timeline = Timeline::new(&SimConfig {
                    bpm,
                    scenario,
                    rr_jitter_ms: 100_000.0,
                    ..SimConfig::default()
                });
                let mut cursor = 0;
                for t in [60.0, 90.0, 120.0, 300.0] {
                    let (rr, _) = timeline.intervals(t, &mut cursor);
                    assert!(rr
                        .iter()
                        .all(|r| *r >= 60.0 / 255.0 - 1e-9 && *r <= 60.0 + 1e-9));
                }
            }
        }
    }

    #[test]
    fn actual_ecg_r_peaks_and_encoded_rr_agree_across_all_scenarios() {
        for scenario in [
            Scenario::Steady,
            Scenario::Breathing,
            Scenario::ExerciseRecovery,
            Scenario::Abrupt,
        ] {
            let config = SimConfig {
                scenario,
                ..SimConfig::default()
            };
            let timeline = Timeline::new(&config);
            let end = timeline.phase(240.0).unwrap().floor() as u64;
            let mut previous_time = 0.0;
            let mut previous_sample = 0_u64;
            for beat in 1..=end {
                let time = timeline.beat_time(beat, 240.0).unwrap();
                let nearest = (time * 130.0).round() as u64;
                let peak = (nearest.saturating_sub(2)..=nearest + 2)
                    .max_by_key(|sample| timeline.ecg_sample(*sample as f64 / 130.0).unwrap())
                    .unwrap();
                assert!((peak as f64 / 130.0 - time).abs() <= 1.0 / 130.0);
                let rr = time - previous_time;
                let packet = crate::gatt_spec::encode_hr_measurement_no_contact(
                    (60.0 / rr).round() as u8,
                    &[rr],
                );
                let encoded = f64::from(u16::from_le_bytes([packet[2], packet[3]])) / 1024.0;
                let ecg_rr = (peak - previous_sample) as f64 / 130.0;
                assert!((encoded - ecg_rr).abs() <= 2.0 / 130.0 + 1.0 / 1024.0);
                previous_time = time;
                previous_sample = peak;
            }
        }
    }

    #[test]
    fn packet_partitions_delays_and_rate_changes_preserve_ecg_sample_identity() {
        let mut config = SimConfig::default();
        let mut timeline = Timeline::new(&config);
        let before = timeline.ecg_sample(10.237).unwrap();
        config.bpm = 137;
        timeline.sync(&config, 10.237);
        assert_eq!(timeline.ecg_sample(10.237), Some(before));
        let samples: Vec<_> = (0..5000)
            .map(|i| timeline.ecg_sample(f64::from(i) / 130.0).unwrap())
            .collect();
        for width in [1, 2, 73, 167] {
            let packets: Vec<_> = samples
                .chunks(width)
                .map(|c| crate::gatt_spec::encode_ecg_frame(0, c))
                .collect();
            let delivered: Vec<_> = packets
                .iter()
                .flat_map(|packet| {
                    packet[10..].as_chunks::<3>().0.iter().map(|b| {
                        (i32::from(b[0]) | (i32::from(b[1]) << 8) | (i32::from(b[2]) << 16)) << 8
                            >> 8
                    })
                })
                .collect();
            assert_eq!(samples, delivered);
        }
        let mut cursor = 0;
        let (rr, dropped) = timeline.intervals(300.0, &mut cursor);
        assert_eq!(rr.len(), 8);
        assert!(dropped > 500);
    }

    #[test]
    fn steady_intervals_are_acquired_once_not_per_notification() {
        let timeline = Timeline::new(&SimConfig::default());
        let mut cursor = 0;
        assert!(timeline.intervals(0.5, &mut cursor).0.is_empty());
        let rr = timeline.intervals(5.0, &mut cursor).0;
        assert_eq!(rr.len(), 6);
        assert!(rr.iter().all(|r| (r - 60.0 / 72.0).abs() < 1e-10));
        assert!(timeline.intervals(5.0, &mut cursor).0.is_empty());
    }
    #[test]
    fn changes_preserve_phase_and_history_is_bounded() {
        let mut config = SimConfig::default();
        let mut timeline = Timeline::new(&config);
        for i in 1..1000 {
            let at = f64::from(i) / 10.0;
            let before = timeline.phase(at).unwrap();
            config.bpm = if i % 2 == 0 { 72 } else { 120 };
            timeline.sync(&config, at);
            assert_eq!(before, timeline.phase(at).unwrap());
        }
        assert_eq!(timeline.epochs.len(), MAX_EPOCHS);
        let (rr, dropped) = timeline.intervals(1e6, &mut 0);
        assert_eq!(rr.len(), 8);
        assert!(dropped > 100_000);
    }
    #[test]
    fn scenarios_are_monotonic_repeatable_and_rr_matches_phase() {
        for scenario in [
            Scenario::Steady,
            Scenario::Breathing,
            Scenario::ExerciseRecovery,
            Scenario::Abrupt,
        ] {
            let config = SimConfig {
                scenario,
                ..SimConfig::default()
            };
            let timeline = Timeline::new(&config);
            let duplicate = Timeline::new(&config);
            let mut cursor = 0;
            let mut previous = 0.0;
            for i in 1..=3000 {
                let t = f64::from(i) / 10.0;
                let phase = timeline.phase(t).unwrap();
                assert!(phase > previous);
                assert_eq!(phase, duplicate.phase(t).unwrap());
                previous = phase;
                let (rr, dropped) = timeline.intervals(t, &mut cursor);
                assert_eq!(dropped, 0);
                assert!(rr.iter().all(|r| *r > 0.2 && *r < 2.0));
            }
        }
    }
}
