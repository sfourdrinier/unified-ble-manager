//! Shared monotonic acquisition accounting; notification scheduling is not sensor time.
use std::time::Duration;

pub struct SampleClock {
    rate: u64,
    next_sample: u64,
}

pub struct Batch {
    pub first: u64,
    pub end: u64,
    pub last_sample_ns: u64,
    pub dropped_samples: u64,
}

impl SampleClock {
    pub fn new(rate: u16) -> Self {
        assert!(rate > 0, "sample rate must be positive");
        Self {
            rate: u64::from(rate),
            next_sample: 0,
        }
    }

    /// Admit only acquired samples. Bound retained backlog to one second or
    /// two selected frames, whichever is larger, reporting every skipped index.
    /// Two frames permit draining a whole frame plus the partial frame already
    /// acquired at a slightly late dispatch, without steady-state clock loss.
    pub fn next(&mut self, elapsed: Duration, count: u64) -> Result<Option<Batch>, String> {
        if count == 0 {
            return Err("sample batch must be nonempty".into());
        }
        let due = elapsed
            .as_nanos()
            .checked_mul(u128::from(self.rate))
            .and_then(|n| u64::try_from(n / 1_000_000_000).ok())
            .ok_or_else(|| "sample clock exhausted".to_owned())?;
        let available = due.saturating_sub(self.next_sample);
        if available < count {
            return Ok(None);
        }
        let dropped_samples = available.saturating_sub(self.rate.max(count.saturating_mul(2)));
        let first = self
            .next_sample
            .checked_add(dropped_samples)
            .ok_or("sample index exhausted")?;
        let end = first.checked_add(count).ok_or("sample index exhausted")?;
        let last_sample_ns =
            u64::try_from(u128::from(end - 1) * 1_000_000_000 / u128::from(self.rate))
                .map_err(|_| "sample timestamp exhausted".to_owned())?;
        self.next_sample = end;
        Ok(Some(Batch {
            first,
            end,
            last_sample_ns,
            dropped_samples,
        }))
    }
}

pub fn sample_timestamp_ns(origin: Duration, last_sample_ns: u64) -> Result<u64, String> {
    u64::try_from(origin.as_nanos() + u128::from(last_sample_ns))
        .map_err(|_| "device elapsed clock exhausted".to_owned())
}

/// Preserve the scheduled phase after a delayed loop without floating-point
/// multiplication or replaying an unbounded number of missed timer slots.
pub fn next_dispatch_delay(overdue: Duration, interval: Duration) -> Duration {
    assert!(!interval.is_zero(), "dispatch interval must be positive");
    let nanos = interval.as_nanos() - overdue.as_nanos() % interval.as_nanos();
    Duration::new(
        (nanos / 1_000_000_000) as u64,
        (nanos % 1_000_000_000) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stalled_ecg_loop_reports_skips_instead_of_compressing_sensor_time() {
        let mut clock = SampleClock::new(130);
        let first = clock.next(Duration::from_secs(1), 73).unwrap().unwrap();
        assert_eq!(first.first, 0);
        let late = clock.next(Duration::from_secs(11), 73).unwrap().unwrap();
        assert_eq!(late.dropped_samples, 1211);
        assert_eq!(late.first, 1284);
        assert_eq!(late.last_sample_ns, 1356 * 1_000_000_000 / 130);
        let final_batch = clock.next(Duration::from_secs(11), 73).unwrap().unwrap();
        assert_eq!(final_batch.first, late.end);
        assert_eq!(final_batch.dropped_samples, 0);
        assert!(clock.next(Duration::from_secs(11), 73).unwrap().is_none());
    }

    #[test]
    fn idle_start_and_restart_use_common_ecg_acc_boot_clock() {
        for start in [Duration::from_secs(116), Duration::from_secs(721)] {
            let mut ecg = SampleClock::new(130);
            let mut acc = SampleClock::new(200);
            let e = ecg.next(Duration::from_millis(600), 73).unwrap().unwrap();
            let a = acc.next(Duration::from_millis(600), 20).unwrap().unwrap();
            assert_eq!(
                sample_timestamp_ns(start, e.last_sample_ns).unwrap(),
                start.as_nanos() as u64 + 72 * 1_000_000_000 / 130
            );
            assert_eq!(
                sample_timestamp_ns(start, a.last_sample_ns).unwrap(),
                start.as_nanos() as u64 + 95_000_000
            );
            assert_eq!(e.first, 0);
            assert_eq!(a.first, 0);
        }
    }

    #[test]
    fn ecg_clock_tracks_ten_minutes_despite_late_dispatch_and_preserves_frame_size() {
        let mut clock = SampleClock::new(130);
        let mut delivered = 0;
        let mut skipped = 0;
        let mut last_end = 0;
        // About 73/130 seconds plus varying loop delay, not a perfect timer.
        for millis in (563..=600_000).step_by(563).chain(std::iter::once(600_000)) {
            while let Some(batch) = clock.next(Duration::from_millis(millis), 73).unwrap() {
                assert_eq!(batch.end - batch.first, 73);
                assert_eq!(batch.first, last_end + batch.dropped_samples);
                last_end = batch.end;
                delivered += 73;
                skipped += batch.dropped_samples;
            }
        }
        assert_eq!(skipped, 0);
        assert!(78_000 - delivered < 73);
        assert_eq!(last_end, delivered);
    }

    #[test]
    fn dispatch_phase_and_low_high_cadence_do_not_change_acquisition_rate() {
        for interval in [Duration::from_millis(100), Duration::from_secs(2)] {
            let overdue = Duration::from_secs(116) + Duration::from_millis(31);
            let delay = next_dispatch_delay(overdue, interval);
            assert!(delay > Duration::ZERO && delay <= interval);
            assert_eq!((overdue + delay).as_nanos() % interval.as_nanos(), 0);
        }
        let mut fast = SampleClock::new(130);
        assert!(fast.next(Duration::from_millis(100), 73).unwrap().is_none());
        let mut slow = SampleClock::new(130);
        let first = slow.next(Duration::from_secs(2), 73).unwrap().unwrap();
        let second = slow.next(Duration::from_secs(2), 73).unwrap().unwrap();
        assert_eq!(first.dropped_samples, 114);
        assert_eq!(second.dropped_samples, 0);
        assert_eq!(second.end, 260);
        assert!(slow.next(Duration::from_secs(2), 73).unwrap().is_none());
    }
}
