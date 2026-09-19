//! Deterministic synthetic ECG waveform.
//!
//! A PQRST complex built from Gaussian bumps, phase-locked to the simulated
//! heart rate. Deterministic by construction (no noise source) so unit tests
//! and the TS cross-check pin exact samples.

/// R-peak amplitude in microvolts (type-0 frames carry signed 24-bit µV).
pub const R_PEAK_UV: i32 = 1100;

/// Microvolts of one ECG sample at `t_s` seconds for a heart beating at
/// `bpm`. The complex repeats every beat starting at `t_s = 0` with the R
/// peak at 22% of the cycle.
pub fn ecg_sample_uv(t_s: f64, bpm: f64) -> i32 {
    if bpm <= 0.0 {
        return 0;
    }
    let period = 60.0 / bpm;
    let phase = (t_s / period) % 1.0;
    let phase = if phase < 0.0 { phase + 1.0 } else { phase };
    let spike = |center: f64, half_width: f64, amplitude: f64| {
        let distance = (phase - center).abs();
        if distance >= half_width {
            0.0
        } else {
            amplitude * (1.0 - distance / half_width)
        }
    };
    let bump = |center: f64, sigma: f64, amplitude: f64| {
        let distance = (phase - center).abs();
        amplitude * (-distance * distance / (2.0 * sigma * sigma)).exp()
    };
    let value = bump(0.12, 0.025, 120.0)
        + spike(0.205, 0.008, -120.0)
        + spike(0.22, 0.02, f64::from(R_PEAK_UV))
        + spike(0.238, 0.008, -220.0)
        + bump(0.38, 0.04, 280.0);
    value.round() as i32
}

/// Fills `out` with `count` consecutive samples at 130 Hz starting at sample
/// index `start_index` (sample `i` sits at `t = i / 130`).
pub fn ecg_frame_samples(start_index: u64, count: usize, bpm: f64, out: &mut Vec<i32>) {
    out.reserve(count);
    for offset in 0..count {
        let index = start_index.saturating_add(offset as u64);
        out.push(ecg_sample_uv(index as f64 / 130.0, bpm));
    }
}

/// The real strap recording, compiled in so the default ECG source works
/// from any directory: `fixtures/h10-raw/ecg-E9B93D29-2026-09-19-130hz.txt`
/// (one integer µV per line @130 Hz).
const RECORDED_ECG_TXT: &str =
    include_str!("../fixtures/h10-raw/ecg-E9B93D29-2026-09-19-130hz.txt");

/// Parses the compiled-in strap recording. A corrupt fixture is a loud
/// error naming the fixture — never silent synthetic fallback.
pub fn recorded_samples() -> Result<Vec<i32>, String> {
    parse_replay_text(
        RECORDED_ECG_TXT,
        "fixtures/h10-raw/ecg-E9B93D29-2026-09-19-130hz.txt",
    )
}

/// Loads a recorded ECG replay file: text, one integer µV per line, sampled
/// at 130 Hz. Blank lines are skipped; any other unparseable line fails
/// loudly with its line number. An empty file is an error, not silence.
pub fn load_replay_file(path: &str) -> Result<Vec<i32>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read ECG file {path}: {error}"))?;
    parse_replay_text(&text, path)
}

fn parse_replay_text(text: &str, origin: &str) -> Result<Vec<i32>, String> {
    let mut samples = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: i32 = line.parse().map_err(|_| {
            format!(
                "ECG file {origin} line {}: {line:?} is not an integer µV value",
                number + 1
            )
        })?;
        samples.push(value);
    }
    if samples.is_empty() {
        return Err(format!("ECG file {origin} has no samples"));
    }
    Ok(samples)
}

/// Fills `out` with `count` samples replayed from `samples`, cycling forever
/// from `start_index`.
pub fn replay_samples(samples: &[i32], start_index: u64, count: usize, out: &mut Vec<i32>) {
    out.reserve(count);
    for offset in 0..count {
        let at = start_index.saturating_add(offset as u64) % samples.len() as u64;
        out.push(samples[at as usize]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn compiled_in_strap_recording_parses() {
        let samples = recorded_samples().expect("committed strap recording must parse");
        assert_eq!(samples.len(), 3285, "one integer µV per fixture line");
        assert_eq!(
            samples[..5].to_vec(),
            vec![65, 75, 73, 68, 82],
            "first samples match the fixture head"
        );
        assert!(
            samples.iter().any(|sample| *sample > 500),
            "recording carries real QRS complexes, not baseline"
        );
    }

    #[test]
    fn replay_file_cycles_samples_and_rejects_garbage() {
        let path = std::env::temp_dir().join("h10-sim-ecg-replay-test.txt");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "0\n1000\n-1000\n").unwrap();
        drop(file);
        let samples = load_replay_file(path.to_str().unwrap()).unwrap();
        assert_eq!(samples, vec![0, 1000, -1000]);
        let mut out = Vec::new();
        replay_samples(&samples, 2, 4, &mut out);
        assert_eq!(out, vec![-1000, 0, 1000, -1000]);
        let _ = std::fs::remove_file(&path);
        assert!(load_replay_file(path.to_str().unwrap()).is_err());
        let bad = std::env::temp_dir().join("h10-sim-ecg-replay-bad.txt");
        std::fs::write(&bad, "12\nnope\n").unwrap();
        assert!(load_replay_file(bad.to_str().unwrap()).is_err());
        let _ = std::fs::remove_file(&bad);
    }

    #[test]
    fn r_peak_matches_amplitude_constant() {
        let bpm = 60.0;
        let period = 60.0 / bpm;
        assert_eq!(ecg_sample_uv(0.22 * period, bpm), R_PEAK_UV);
    }

    #[test]
    fn baseline_between_beats_is_near_zero() {
        let baseline = ecg_sample_uv(0.7 * (60.0 / 72.0), 72.0);
        assert!(baseline.abs() <= 5, "baseline was {baseline}");
    }

    #[test]
    fn waveform_repeats_every_beat() {
        let bpm = 80.0;
        let period = 60.0 / bpm;
        for probe in [0.05, 0.21, 0.4] {
            assert_eq!(
                ecg_sample_uv(probe, bpm),
                ecg_sample_uv(probe + period, bpm)
            );
        }
    }

    #[test]
    fn frame_samples_advance_at_130hz() {
        let mut first = Vec::new();
        let mut second = Vec::new();
        ecg_frame_samples(0, 3, 60.0, &mut first);
        ecg_frame_samples(3, 3, 60.0, &mut second);
        assert_eq!(first.len(), 3);
        assert_eq!(second[0], ecg_sample_uv(3.0 / 130.0, 60.0));
        assert_eq!(second[2], ecg_sample_uv(5.0 / 130.0, 60.0));
    }

    #[test]
    fn samples_stay_inside_24bit_range() {
        let mut out = Vec::new();
        for bpm_bits in [40.0, 72.0, 180.0] {
            out.clear();
            ecg_frame_samples(0, 260, bpm_bits, &mut out);
            for sample in &out {
                assert!(
                    (-8_388_608..=8_388_607).contains(sample),
                    "sample {sample} out of range"
                );
            }
        }
    }

    #[test]
    fn higher_bpm_gives_more_r_peaks_per_second() {
        /// Rising-edge count: beats detected, independent of sample alignment.
        fn count_peaks(bpm: f64) -> usize {
            let mut out = Vec::new();
            ecg_frame_samples(0, 520, bpm, &mut out);
            let mut peaks = 0;
            let mut above = false;
            for sample in out {
                if sample > R_PEAK_UV / 2 {
                    if !above {
                        peaks += 1;
                    }
                    above = true;
                } else {
                    above = false;
                }
            }
            peaks
        }
        assert_eq!(count_peaks(60.0), 4);
        assert_eq!(count_peaks(120.0), 8);
    }
}
