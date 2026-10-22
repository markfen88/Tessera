//! Read-only clocks and conversion of kernel timestamps to unslewed media time.

use nix::time::{ClockId, clock_gettime};

fn ns_of(id: ClockId) -> u64 {
    match clock_gettime(id) {
        Ok(ts) => (ts.tv_sec() as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec() as u64),
        Err(_) => 0,
    }
}

/// The regular monotonic clock, used only at operating-system timer boundaries.
#[must_use]
pub fn mono_ns() -> u64 {
    ns_of(ClockId::CLOCK_MONOTONIC)
}

/// The unslewed local timebase shared by PTP, the media engine and audio clients.
#[must_use]
pub fn raw_ns() -> u64 {
    tessera_audio_ipc::raw_monotonic_ns()
}

/// `CLOCK_REALTIME` in nanoseconds (read only).
#[must_use]
pub fn realtime_ns() -> u64 {
    ns_of(ClockId::CLOCK_REALTIME)
}

/// A plausible epoch for a leader, read once when its media clock starts.
/// Subsequent media time advances exclusively against the raw oscillator.
#[must_use]
pub fn tai_ns() -> u64 {
    ns_of(ClockId::CLOCK_TAI)
}

/// Converts kernel software timestamps from realtime to `CLOCK_MONOTONIC_RAW`.
///
/// Refresh immediately before and after draining a receive/TX-timestamp batch.
/// Use timestamps only when both calibrations are valid. A fresh
/// bracket dates the recent timestamps in the raw domain; no long-lived NTP
/// rate estimate steers media time. Step detection deliberately compares
/// realtime with regular monotonic time: those clocks slew together, whereas
/// their offset changes on a wall-clock step or suspend/resume. Using the raw
/// offset for detection would mistake legitimate NTP slewing for clock jumps.
#[derive(Debug, Clone, Copy)]
pub struct RealtimeToRaw {
    offset: Option<i64>,
    monotonic_offset: Option<i64>,
    step_threshold_ns: u64,
    poisoned: bool,
    steps: u64,
    calibration_rejections: u64,
}

const MAX_BRACKET_NS: u64 = 50_000;

/// Timestamp validity for one socket queue. A discontinuity seen by any
/// calibration poisons this queue until it has drained, including batches
/// carried into later loop iterations. Separate queues need separate guards.
#[derive(Debug, Default)]
pub struct TimestampQueue {
    generation: u64,
    poisoned: bool,
}

impl TimestampQueue {
    /// Call after the calibration immediately before receiving.
    pub fn before_batch(&mut self, clock: &RealtimeToRaw) {
        let generation = clock.steps_detected() + clock.calibration_rejections();
        self.poisoned |= !clock.is_valid() || generation != self.generation;
        self.generation = generation;
    }

    /// Call after the post-receive calibration. `drained` means the receive
    /// returned short or empty, so older queued stamps cannot survive it.
    /// The batch that drains a poisoned queue is still invalid.
    pub fn finish_batch(&mut self, clock: &RealtimeToRaw, drained: bool) -> bool {
        let generation_before = self.generation;
        self.before_batch(clock);
        let valid = !self.poisoned;
        // A step detected after recv may have followed its empty-queue
        // observation. Drain once more in the new generation before reuse.
        if drained && clock.is_valid() && self.generation == generation_before {
            self.poisoned = false;
        }
        valid
    }
}

fn difference(a: u64, b: u64) -> i64 {
    (i128::from(a) - i128::from(b)).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

impl RealtimeToRaw {
    /// `step_threshold_ns`: a change in realtime minus regular monotonic
    /// larger than this discards the affected batch, regardless of NTP speed.
    #[must_use]
    pub fn new(step_threshold_ns: u64) -> Self {
        Self {
            offset: None,
            monotonic_offset: None,
            step_threshold_ns,
            poisoned: false,
            steps: 0,
            calibration_rejections: 0,
        }
    }

    fn update(&mut self, raw: u64, mono: u64, real: u64) -> bool {
        let off = difference(mono, real);
        let stepped = self
            .monotonic_offset
            .is_some_and(|prev| off.abs_diff(prev) > self.step_threshold_ns);
        self.steps += u64::from(stepped);
        self.offset = Some(difference(raw, real));
        self.monotonic_offset = Some(off);
        self.poisoned = stepped;
        !stepped
    }

    /// Refreshes the conversion using the narrowest of three clock brackets.
    /// `false` reports a wall-clock discontinuity. An entirely preempted
    /// calibration also discards the batch, without claiming a clock step.
    pub fn refresh(&mut self) -> bool {
        let mut best: Option<(u64, u64, u64, u64)> = None;
        for _ in 0..3 {
            let r1 = raw_ns();
            let m1 = mono_ns();
            let real = realtime_ns();
            let m2 = mono_ns();
            let r2 = raw_ns();
            let width = r2.saturating_sub(r1);
            let raw_mid = r1 + width / 2;
            let mono_mid = m1 + m2.saturating_sub(m1) / 2;
            if best.is_none_or(|b| width < b.0) {
                best = Some((width, raw_mid, mono_mid, real));
            }
        }
        let Some((width, raw, mono, real)) = best else {
            self.poisoned = true;
            self.calibration_rejections += 1;
            return true;
        };
        if width > MAX_BRACKET_NS {
            self.poisoned = true;
            self.calibration_rejections += 1;
            return true;
        }
        self.update(raw, mono, real)
    }

    /// A realtime timestamp as raw monotonic nanoseconds, or `None` for an
    /// uncalibrated, stepped or imprecisely calibrated batch.
    #[must_use]
    pub fn convert(&self, real_ns: u64) -> Option<u64> {
        if self.poisoned {
            return None;
        }
        u64::try_from(i128::from(real_ns) + i128::from(self.offset?)).ok()
    }

    /// Wall-clock discontinuities detected so far.
    #[must_use]
    pub fn steps_detected(&self) -> u64 {
        self.steps
    }

    /// Whether the current calibration can safely date a timestamp batch.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.offset.is_some() && !self.poisoned
    }

    /// Brackets rejected because all calibration attempts were preempted.
    #[must_use]
    pub fn calibration_rejections(&self) -> u64 {
        self.calibration_rejections
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_matches_the_raw_clock() {
        let mut c = RealtimeToRaw::new(2_000_000);
        assert!(c.convert(realtime_ns()).is_none());
        assert!(c.refresh());
        let converted = c.convert(realtime_ns()).unwrap();
        assert!(raw_ns().abs_diff(converted) < 50_000_000);
    }

    #[test]
    fn ntp_slew_never_becomes_a_media_clock_step() {
        let mut c = RealtimeToRaw::new(2_000_000);
        let epoch = 1_700_000_000_000_000_000u64;
        for ppm in [-10_000i64, -350, 0, 350, 10_000] {
            for sec in 0..100u64 {
                let raw = 10_000_000_000 + sec * 1_000_000_000;
                let slew = i128::from(sec) * i128::from(ppm) * 1_000;
                let mono = (i128::from(raw) + 500_000_000 + slew) as u64;
                let real = epoch + mono;
                assert!(c.update(raw, mono, real), "NTP {ppm} ppm became a step");
                assert_eq!(c.convert(real), Some(raw));
            }
        }
        assert_eq!(c.steps_detected(), 0);
    }

    #[test]
    fn a_wall_clock_step_discards_one_batch_and_recovers() {
        let mut c = RealtimeToRaw::new(2_000_000);
        assert!(c.update(1_000, 2_000, 10_000_000_000));
        assert!(!c.update(2_000, 3_000, 11_000_001_000));
        assert!(c.convert(11_000_001_000).is_none());
        assert_eq!(c.steps_detected(), 1);
        assert!(c.update(3_000, 4_000, 11_000_002_000));
        assert_eq!(c.convert(11_000_002_000), Some(3_000));
    }

    #[test]
    fn step_during_a_batch_invalidates_stamps_from_both_sides() {
        let mut c = RealtimeToRaw::new(2_000_000);
        c.update(1_000, 2_000, 10_000_000_000);
        let valid_before = c.is_valid();
        let before_step = 10_000_000_100;
        let after_step = 11_000_000_200;
        c.update(1_300, 2_300, 11_000_000_300);
        assert!(!(valid_before && c.is_valid()));
        assert_eq!(c.convert(before_step), None);
        assert_eq!(c.convert(after_step), None);
    }

    #[test]
    fn a_step_poisons_every_queue_until_its_backlog_has_drained() {
        let mut clock = RealtimeToRaw::new(2_000_000);
        let mut a = TimestampQueue::default();
        let mut b = TimestampQueue::default();
        clock.update(1_000, 2_000, 10_000_000_000);
        a.before_batch(&clock);
        b.before_batch(&clock);
        // Step during A's drain, while both queues have pre-step stamps.
        clock.update(2_000, 3_000, 11_000_001_000);
        assert!(!a.finish_batch(&clock, false));
        // New calibrations are valid, but both queues still contain backlog.
        clock.update(3_000, 4_000, 11_000_002_000);
        a.before_batch(&clock);
        b.before_batch(&clock);
        assert!(!a.finish_batch(&clock, false));
        assert!(!b.finish_batch(&clock, true));
        b.before_batch(&clock);
        assert!(b.finish_batch(&clock, true));
        assert!(!a.finish_batch(&clock, true));
        a.before_batch(&clock);
        assert!(a.finish_batch(&clock, true));
    }

    #[test]
    fn rejected_calibration_also_poisons_backlog_without_claiming_a_step() {
        let mut clock = RealtimeToRaw::new(2_000_000);
        let mut queue = TimestampQueue::default();
        clock.update(1_000, 2_000, 10_000_000_000);
        queue.before_batch(&clock);
        clock.calibration_rejections += 1;
        clock.poisoned = true;
        assert!(!queue.finish_batch(&clock, false));
        clock.update(2_000, 3_000, 10_000_001_000);
        queue.before_batch(&clock);
        assert!(!queue.finish_batch(&clock, true));
        queue.before_batch(&clock);
        assert!(queue.finish_batch(&clock, true));
        assert_eq!(clock.steps_detected(), 0);
    }

    #[test]
    fn a_step_after_a_short_receive_requires_another_drain() {
        let mut clock = RealtimeToRaw::new(2_000_000);
        let mut queue = TimestampQueue::default();
        clock.update(1_000, 2_000, 10_000_000_000);
        queue.before_batch(&clock);
        // recv returned short; a packet arrived just before a wall-clock
        // step that the post-receive calibration then detected.
        clock.update(2_000, 3_000, 11_000_001_000);
        assert!(!queue.finish_batch(&clock, true));
        clock.update(3_000, 4_000, 11_000_002_000);
        queue.before_batch(&clock);
        assert!(!queue.finish_batch(&clock, true));
        queue.before_batch(&clock);
        assert!(queue.finish_batch(&clock, true));
    }

    #[test]
    fn resume_does_not_add_sleep_time_to_the_raw_clock() {
        let mut c = RealtimeToRaw::new(2_000_000);
        assert!(c.update(1_000, 2_000, 10_000_000_000));
        // Raw and monotonic pause together, while realtime includes sleep.
        assert!(!c.update(2_000, 3_000, 70_000_001_000));
        assert!(c.convert(70_000_001_000).is_none());
        assert!(c.update(3_000, 4_000, 70_000_002_000));
        assert_eq!(c.convert(70_000_002_000), Some(3_000));
    }

    #[test]
    fn tai_is_at_least_realtime() {
        assert!(tai_ns() >= realtime_ns().saturating_sub(1_000_000_000));
    }
}
