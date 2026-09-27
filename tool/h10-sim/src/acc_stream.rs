//! Deterministic synthetic H10 ACC source and sample-clock batching.
//! Packet cadence is simulator policy, not a claim about H10 firmware batching.

use crate::acc;
use std::time::Duration;

pub struct Stream {
    settings: acc::Settings,
    clock: crate::sample_clock::SampleClock,
}

pub struct Batch {
    pub samples: Vec<[i16; 3]>,
    pub last_sample_ns: u64,
    pub dropped_samples: u64,
}

impl Stream {
    pub fn new(settings: acc::Settings) -> Self {
        Self {
            settings,
            clock: crate::sample_clock::SampleClock::new(settings.sample_rate_hz),
        }
    }

    /// Up to 100 ms per packet, constrained by the current transport's value
    /// capacity. Keep at most one second of backlog after a stalled host loop;
    /// return the exact discarded sample count to the caller for reporting.
    pub fn next(&mut self, elapsed: Duration, capacity: usize) -> Result<Option<Batch>, String> {
        let fit = capacity.saturating_sub(acc::HEADER_BYTES) / acc::SAMPLE_BYTES;
        if fit == 0 {
            return Err(format!(
                "ACC requires at least 16 notification bytes; transport permits {capacity}"
            ));
        }
        let rate = u64::from(self.settings.sample_rate_hz);
        let count = fit.min(usize::from(self.settings.sample_rate_hz / 10).max(1)) as u64;
        let Some(batch) = self.clock.next(elapsed, count)? else {
            return Ok(None);
        };
        // Synthetic, repeatable three-axis movement, not a captured H10 trace.
        // One gravity component plus bounded motion remains within every range.
        let amplitude = f64::from(self.settings.range_g) * 400.0;
        let samples = (batch.first..batch.end)
            .map(|index| {
                let phase = (index % (rate * 4)) as f64 / rate as f64 * std::f64::consts::TAU;
                [
                    (amplitude * phase.sin()).round() as i16,
                    (amplitude * (phase * 0.5).cos()).round() as i16,
                    (1000.0 + amplitude * 0.5 * (phase * 0.25).sin()).round() as i16,
                ]
            })
            .collect();
        Ok(Some(Batch {
            samples,
            last_sample_ns: batch.last_sample_ns,
            dropped_samples: batch.dropped_samples,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acc;
    use std::time::Duration;

    #[test]
    fn every_setting_delivers_exact_sample_clock_without_tick_drift() {
        for rate in acc::SAMPLE_RATES_HZ {
            for range in acc::RANGES_G {
                let settings = acc::Settings {
                    sample_rate_hz: rate,
                    range_g: range,
                };
                let mut stream = Stream::new(settings);
                let mut samples = 0;
                let mut previous = None;
                for millis in (0..=2000).step_by(5) {
                    while let Some(batch) = stream.next(Duration::from_millis(millis), 244).unwrap()
                    {
                        assert_eq!(batch.dropped_samples, 0);
                        assert!(batch.samples.len() * acc::SAMPLE_BYTES + acc::HEADER_BYTES <= 244);
                        if let Some(last) = previous {
                            assert_eq!(
                                batch.last_sample_ns - last,
                                batch.samples.len() as u64 * 1_000_000_000 / u64::from(rate)
                            );
                        }
                        previous = Some(batch.last_sample_ns);
                        for xyz in &batch.samples {
                            assert!(xyz
                                .iter()
                                .all(|axis| i32::from(*axis).abs() <= i32::from(range) * 1000));
                        }
                        samples += batch.samples.len();
                    }
                }
                // A final partial packet waits for the next batch, never invents samples.
                assert!(usize::from(rate) * 2 - samples < usize::from(rate / 10).max(1));
            }
        }
    }

    #[test]
    fn capacity_changes_are_applied_without_resetting_sample_identity() {
        let mut stream = Stream::new(acc::Settings {
            sample_rate_hz: 200,
            range_g: 8,
        });
        assert!(stream.next(Duration::from_secs(1), 15).is_err());
        let first = stream.next(Duration::from_millis(5), 16).unwrap().unwrap();
        assert_eq!(first.samples.len(), 1);
        assert_eq!(first.last_sample_ns, 0);
        let second = stream
            .next(Duration::from_millis(105), 244)
            .unwrap()
            .unwrap();
        assert_eq!(second.samples.len(), 20);
        assert_eq!(second.last_sample_ns, 100_000_000);
    }

    #[test]
    fn delayed_loop_bounds_backlog_and_reports_every_skipped_sample() {
        let mut stream = Stream::new(acc::Settings {
            sample_rate_hz: 200,
            range_g: 2,
        });
        let batch = stream.next(Duration::from_secs(10), 244).unwrap().unwrap();
        assert_eq!(batch.dropped_samples, 1800);
        assert_eq!(batch.last_sample_ns, 9_095_000_000);
        let mut delivered = batch.samples.len();
        while let Some(batch) = stream.next(Duration::from_secs(10), 244).unwrap() {
            assert_eq!(batch.dropped_samples, 0);
            delivered += batch.samples.len();
        }
        assert_eq!(delivered, 200);
    }
}
