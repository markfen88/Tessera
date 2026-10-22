//! Integer-only PI servo with outlier rejection and step handling.
//!
//! The servo is pure: the port feeds it `(t, e)` where `t` is the local
//! monotonic time of a Sync arrival and `e` is the media-clock error there
//! (`media_ns_at(t) - leader_time_at(t)`, ns; positive = our media clock is
//! ahead), and applies the returned [`ServoOutput`] to the
//! [`MediaClock`](crate::media_clock::MediaClock). All math is `i64`/`i128`
//! so results are identical on x86, armv7 and thumbv8m (no libm).
//!
//! Algorithm (design, `guess` -- PTPv1 does not mandate a servo):
//! 1. first sample: if |e| is at least the step threshold, a phase step so the
//!    error is zero (the epoch increments); otherwise no step at all;
//! 2. after at least `freq_init_min_span_ns`: initialise the frequency from
//!    the error accumulated since the first sample, step out the remaining
//!    error only if it is at least the step threshold, and continue with PI;
//! 3. PI: `rate = -(kp*e + integral)`, `integral += ki*e*dt`, clamped to
//!    `max_rate_ppb`;
//! 4. outliers (|e| above `outlier_k` times the running mean deviation, with
//!    a floor) are skipped, but after `max_consecutive_rejects` the sample is
//!    accepted, so a genuine jump re-locks instead of sticking;
//! 5. |e| above `step_threshold_ns` is stepped, otherwise slewed;
//! 6. lock = the median |e| of the last `lock_samples` accepted samples is at
//!    or below `lock_threshold_ns`; lock is lost when that median exceeds
//!    `unlock_threshold_ns`. (A median, not the raw sample, so timestamp
//!    jitter comparable to the threshold does not prevent lock.)

use crate::time::LocalNs;

/// Fixed-point scale of the gains: `1.0 == 1 << GAIN_SHIFT`.
pub const GAIN_SHIFT: u32 = 16;

/// Largest supported `lock_samples` (the window is a fixed array).
pub const MAX_LOCK_WINDOW: usize = 8;

/// Servo configuration. Gains are Q16 fixed point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServoConfig {
    /// Proportional gain in ppb per ns of error (Q16). Time constant is about
    /// `1/kp` seconds; keep `kp * sample_interval_s < 1`.
    pub kp_q16: i64,
    /// Integral gain in ppb per ns of error per second (Q16).
    pub ki_q16: i64,
    /// Rate clamp (ppb), both for the output and the integrator.
    pub max_rate_ppb: i64,
    /// Errors larger than this are stepped, not slewed (the reference uses
    /// 1 ms in its Kalman filter, spec:7).
    pub step_threshold_ns: i64,
    /// Minimum time between the first step and the frequency initialisation.
    pub freq_init_min_span_ns: u64,
    /// After the frequency initialisation the remaining error is slewed out
    /// with the proportional term only (the integrator is frozen, because
    /// the frequency is already right and integrating the transient would
    /// overshoot) until lock or this long, whichever comes first.
    pub settle_ns: u64,
    /// Proportional gain used while settling (Q16); higher than `kp_q16`
    /// because the integrator is frozen then.
    pub settle_kp_q16: i64,
    /// Outlier threshold as a multiple of the running mean |e|.
    pub outlier_k: i64,
    /// Outlier threshold floor (ns).
    pub outlier_floor_ns: i64,
    /// Outliers skipped in a row before a sample is accepted anyway.
    pub max_consecutive_rejects: u8,
    /// Median |e| at or below this (ns) means locked.
    pub lock_threshold_ns: i64,
    /// Window length of the lock median (1..=[`MAX_LOCK_WINDOW`]).
    pub lock_samples: u8,
    /// Median |e| above this (ns) loses lock.
    pub unlock_threshold_ns: i64,
}

impl Default for ServoConfig {
    fn default() -> Self {
        ServoConfig {
            kp_q16: 6_553,         // 0.1 ppb/ns
            ki_q16: 655,           // 0.01 ppb/(ns*s)
            max_rate_ppb: 400_000, // 400 ppm (spec:7 reference limit)
            step_threshold_ns: 1_000_000,
            freq_init_min_span_ns: 4_000_000_000,
            settle_ns: 8_000_000_000,
            settle_kp_q16: 19_660, // 0.3 ppb/ns
            outlier_k: 6,
            outlier_floor_ns: 100_000,
            max_consecutive_rejects: 3,
            lock_threshold_ns: 50_000,
            lock_samples: 5,
            unlock_threshold_ns: 250_000,
        }
    }
}

/// What the port must do with the clock after a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServoOutput {
    /// Sample discarded as an outlier; no change.
    Rejected,
    /// No change yet (waiting for the frequency-initialisation span).
    Hold,
    /// Set the rate (ppb); continuous.
    Rate {
        /// New absolute rate offset in ppb.
        rate_ppb: i64,
    },
    /// Step media time by `correction_ns` (epoch increments) and set the rate.
    Step {
        /// Amount to add to media time (negative of the error).
        correction_ns: i64,
        /// New absolute rate offset in ppb.
        rate_ppb: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    First,
    Rebaseline,
    Second {
        t0: LocalNs,
        e0: i64,
        last_t: LocalNs,
        last_e: i64,
    },
    Tracking {
        last_t: LocalNs,
        settle_until: LocalNs,
    },
}

/// The PI servo state.
#[derive(Debug, Clone, Copy)]
pub struct Servo {
    cfg: ServoConfig,
    phase: Phase,
    calibrated: bool,
    /// Integrator, ppb in Q16.
    integ: i128,
    /// Running mean |e| in ns (outlier threshold).
    dev_ns: i64,
    rejects: u8,
    /// Ring of recent |e| for the lock median.
    recent: [i64; MAX_LOCK_WINDOW],
    recent_n: usize,
    recent_head: usize,
    locked: bool,
    last_error: i64,
}

fn clamp_i64(v: i128, lim: i64) -> i64 {
    v.clamp(-i128::from(lim), i128::from(lim)) as i64
}

impl Servo {
    /// A fresh servo.
    #[must_use]
    pub fn new(cfg: ServoConfig) -> Self {
        Servo {
            cfg,
            phase: Phase::First,
            calibrated: false,
            integ: 0,
            dev_ns: cfg.lock_threshold_ns,
            rejects: 0,
            recent: [0; MAX_LOCK_WINDOW],
            recent_n: 0,
            recent_head: 0,
            locked: false,
            last_error: 0,
        }
    }

    /// Forgets all history (new parent). The next sample steps.
    pub fn reset(&mut self) {
        *self = Servo::new(self.cfg);
    }

    /// True once the error has stayed within the lock threshold.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// True once a frequency estimate is available, including while it is
    /// being reacquired after a later phase step.
    #[must_use]
    pub fn is_calibrated(&self) -> bool {
        self.calibrated
    }

    /// The last accepted error in ns.
    #[must_use]
    pub fn last_error_ns(&self) -> i64 {
        self.last_error
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &ServoConfig {
        &self.cfg
    }

    /// Feeds one error sample taken at local time `t`; `rate_ppb` is the
    /// clock's current rate.
    pub fn sample(&mut self, t: LocalNs, e: i64, rate_ppb: i64) -> ServoOutput {
        match self.phase {
            Phase::First => {
                if e.unsigned_abs() >= self.cfg.step_threshold_ns.unsigned_abs() {
                    self.phase = Phase::Second {
                        t0: t,
                        e0: 0,
                        last_t: t,
                        last_e: 0,
                    };
                    self.last_error = 0;
                    ServoOutput::Step {
                        correction_ns: e.saturating_neg(),
                        rate_ppb,
                    }
                } else {
                    // Below the step threshold: no discontinuity; the error is
                    // slewed out once the frequency is known.
                    self.phase = Phase::Second {
                        t0: t,
                        e0: e,
                        last_t: t,
                        last_e: e,
                    };
                    self.last_error = e;
                    ServoOutput::Hold
                }
            }
            Phase::Rebaseline => {
                // The first observation after the step is the baseline. A
                // transient delay may have disappeared since the step; do not
                // interpret that constant phase error as an oscillator rate.
                self.phase = Phase::Second {
                    t0: t,
                    e0: e,
                    last_t: t,
                    last_e: e,
                };
                self.last_error = e;
                ServoOutput::Hold
            }
            Phase::Second {
                t0,
                e0,
                last_t,
                last_e,
            } => {
                // A discontinuity within the calibration window is phase,
                // not frequency. Preserve the learned rate and start a fresh
                // baseline instead of folding a delay edge into holdover.
                let dt = i128::from(t.saturating_sub(last_t));
                let rate_bound =
                    i128::from(self.cfg.max_rate_ppb).abs() + i128::from(rate_ppb).abs();
                let max_change = rate_bound.saturating_mul(dt) / 1_000_000_000
                    + 2 * i128::from(self.cfg.outlier_floor_ns).abs();
                if self.calibrated && (i128::from(e) - i128::from(last_e)).abs() > max_change {
                    self.last_error = e;
                    if e.unsigned_abs() >= self.cfg.step_threshold_ns.unsigned_abs() {
                        self.phase = Phase::Rebaseline;
                        return ServoOutput::Step {
                            correction_ns: e.saturating_neg(),
                            rate_ppb: self.rate_from_integ(0, false),
                        };
                    }
                    self.phase = Phase::Second {
                        t0: t,
                        e0: e,
                        last_t: t,
                        last_e: e,
                    };
                    return ServoOutput::Hold;
                }
                let span = t.saturating_sub(t0);
                if span < self.cfg.freq_init_min_span_ns || span == 0 {
                    self.phase = Phase::Second {
                        t0,
                        e0,
                        last_t: t,
                        last_e: e,
                    };
                    return ServoOutput::Hold;
                }
                // Error accumulated over `span` at the old rate: our clock
                // gained e ns, so lower the rate by e/span (in ppb).
                let slope_ppb = (i128::from(e) - i128::from(e0)) * 1_000_000_000 / i128::from(span);
                let new_rate = clamp_i64(i128::from(rate_ppb) - slope_ppb, self.cfg.max_rate_ppb);
                self.integ = -(i128::from(new_rate) << GAIN_SHIFT);
                self.calibrated = true;
                self.phase = Phase::Tracking {
                    last_t: t,
                    settle_until: t.saturating_add(self.cfg.settle_ns),
                };
                self.last_error = e;
                if e.unsigned_abs() >= self.cfg.step_threshold_ns.unsigned_abs() {
                    ServoOutput::Step {
                        correction_ns: e.saturating_neg(),
                        rate_ppb: new_rate,
                    }
                } else {
                    ServoOutput::Rate { rate_ppb: new_rate }
                }
            }
            Phase::Tracking {
                last_t,
                settle_until,
            } => self.track(t, last_t, settle_until, e),
        }
    }

    fn track(&mut self, t: LocalNs, last_t: LocalNs, settle_until: LocalNs, e: i64) -> ServoOutput {
        let abs = e.saturating_abs();
        let limit = self
            .cfg
            .outlier_floor_ns
            .max(self.dev_ns.saturating_mul(self.cfg.outlier_k));
        if abs > limit && self.rejects < self.cfg.max_consecutive_rejects {
            self.rejects += 1;
            return ServoOutput::Rejected;
        }
        self.rejects = 0;
        self.last_error = e;
        self.phase = Phase::Tracking {
            last_t: t,
            settle_until,
        };
        if abs > self.cfg.step_threshold_ns {
            // Reacquire frequency from fresh post-step observations over the
            // normal calibration span. Keeping the old integrator in Tracking
            // would reject three samples and step the fourth forever. Jitter
            // and phase jumps among rejected samples are not a speed estimate.
            self.locked = false;
            self.recent_n = 0;
            self.recent_head = 0;
            self.dev_ns = self.cfg.lock_threshold_ns;
            self.phase = Phase::Rebaseline;
            return ServoOutput::Step {
                correction_ns: e.saturating_neg(),
                rate_ppb: self.rate_from_integ(0, false),
            };
        }
        // Running mean deviation: dev += (|e| - dev) / 8.
        self.dev_ns += (abs - self.dev_ns) >> 3;
        let dt = i128::from(t.saturating_sub(last_t));
        let lim_q = i128::from(self.cfg.max_rate_ppb) << GAIN_SHIFT;
        let settling = t < settle_until && !self.locked;
        if !settling {
            self.integ = (self.integ
                + i128::from(e) * i128::from(self.cfg.ki_q16) * dt / 1_000_000_000)
                .clamp(-lim_q, lim_q);
        }
        self.track_lock(abs);
        ServoOutput::Rate {
            rate_ppb: self.rate_from_integ(e, settling),
        }
    }

    /// The frequency the servo has learned (the integral term, without the
    /// proportional noise), ppb; `None` until calibrated. This is the right
    /// rate to freeze when the clock stops being disciplined (holdover,
    /// leader).
    #[must_use]
    pub fn frequency_ppb(&self) -> Option<i64> {
        self.is_calibrated()
            .then(|| clamp_i64(-(self.integ >> GAIN_SHIFT), self.cfg.max_rate_ppb))
    }

    /// `-(kp*e + integral)` in ppb.
    fn rate_from_integ(&self, e: i64, settling: bool) -> i64 {
        let kp = if settling {
            self.cfg.settle_kp_q16
        } else {
            self.cfg.kp_q16
        };
        let p = i128::from(e) * i128::from(kp);
        clamp_i64(-((p + self.integ) >> GAIN_SHIFT), self.cfg.max_rate_ppb)
    }

    fn track_lock(&mut self, abs: i64) {
        let window = usize::from(self.cfg.lock_samples).clamp(1, MAX_LOCK_WINDOW);
        if let Some(slot) = self.recent.get_mut(self.recent_head) {
            *slot = abs;
        }
        self.recent_head = (self.recent_head + 1) % window;
        self.recent_n = (self.recent_n + 1).min(window);
        if self.recent_n < window {
            return;
        }
        let mut tmp = self.recent;
        let Some(s) = tmp.get_mut(..window) else {
            return;
        };
        s.sort_unstable();
        let median = s.get(window / 2).copied().unwrap_or(i64::MAX);
        if self.locked {
            if median > self.cfg.unlock_threshold_ns {
                self.locked = false;
            }
        } else if median <= self.cfg.lock_threshold_ns {
            self.locked = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_clock::MediaClock;

    fn apply(servo: &mut Servo, clock: &mut MediaClock, t: u64, e: i64) {
        match servo.sample(t, e, clock.rate_ppb()) {
            ServoOutput::Rejected | ServoOutput::Hold => {}
            ServoOutput::Rate { rate_ppb } => clock.set_rate(t, rate_ppb),
            ServoOutput::Step {
                correction_ns,
                rate_ppb,
            } => {
                clock.step(t, correction_ns);
                clock.set_rate(t, rate_ppb);
            }
        }
    }

    /// Closed loop: true leader time = mono * (1 + drift) + offset; the
    /// servo disciplines a MediaClock from error samples taken every second.
    fn run(
        drift_ppb: i64,
        offset_ns: i64,
        seconds: u64,
        noise: impl Fn(u64) -> i64,
    ) -> (Servo, MediaClock, i64) {
        let mut clock = MediaClock::new(0, 0);
        let mut servo = Servo::new(ServoConfig::default());
        let leader_at = |t: u64| -> i128 {
            i128::from(t)
                + i128::from(t) * i128::from(drift_ppb) / 1_000_000_000
                + i128::from(offset_ns)
        };
        let mut last_e = 0;
        for i in 1..=seconds {
            let t = i * 1_000_000_000;
            let e = (i128::from(clock.media_ns_at(t)) - leader_at(t)) as i64 + noise(i);
            last_e = e;
            apply(&mut servo, &mut clock, t, e);
        }
        (servo, clock, last_e)
    }

    #[test]
    fn transient_delay_sweep_preserves_frequency_and_holdover() {
        for interval in [1_000_000_000u64, 250_000_000] {
            for delayed_syncs in 1..=12 {
                let mut clock = MediaClock::new(0, 0);
                let mut servo = Servo::new(ServoConfig::default());
                let first_delayed = 40_000_000_000 / interval;
                for i in 1..=65_000_000_000 / interval {
                    let t = i * interval;
                    let delay = if (first_delayed..first_delayed + delayed_syncs).contains(&i) {
                        1_500_000
                    } else {
                        0
                    };
                    let error = i64::try_from(i128::from(clock.media_ns_at(t)) - i128::from(t))
                        .unwrap()
                        + delay;
                    apply(&mut servo, &mut clock, t, error);
                    assert!(
                        clock.rate_ppb().abs() < 1_000,
                        "false rate for {delayed_syncs} Syncs at {interval} ns, sample {i}: {}",
                        clock.rate_ppb()
                    );
                    if let Some(rate) = servo.frequency_ppb() {
                        assert!(rate.abs() < 1_000, "false holdover rate: {rate}");
                    }
                    if i >= first_delayed {
                        assert!(servo.is_calibrated());
                        assert!(servo.frequency_ppb().is_some());
                    }
                }
                assert_eq!(clock.epoch(), if delayed_syncs >= 4 { 2 } else { 0 });
                assert!(servo.is_locked());
            }
        }
    }

    #[test]
    fn converges_on_drift_and_offset_without_noise() {
        let (servo, clock, e) = run(50_000, 123_456_789_012, 60, |_| 0);
        assert!(servo.is_calibrated());
        assert!(servo.is_locked(), "e={e}");
        assert!(e.abs() < 1_000, "e={e}");
        assert!(
            (clock.rate_ppb() - 50_000).abs() < 1_000,
            "rate {}",
            clock.rate_ppb()
        );
        // The initial phase offset is far above the step threshold: one step. The
        // error accumulated while the frequency was learned is slewed out.
        assert_eq!(clock.epoch(), 1);
    }

    #[test]
    fn single_outlier_is_rejected_and_does_not_disturb_the_clock() {
        let (servo, clock, e) = run(-30_000, 5_000_000_000, 60, |i| {
            if i == 40 { 800_000 } else { 0 }
        });
        assert!(servo.is_locked(), "e={e}");
        assert_eq!(clock.epoch(), 1, "outlier must not cause a step");
    }

    #[test]
    fn persistent_jump_is_followed_with_a_step() {
        let mut clock = MediaClock::new(0, 0);
        let mut servo = Servo::new(ServoConfig::default());
        let mut jump = 0i64;
        for i in 1..=80u64 {
            let t = i * 1_000_000_000;
            if i == 40 {
                jump = 50_000_000; // leader time jumps by 50 ms
            }
            let leader = i128::from(t) + i128::from(jump);
            let e = (i128::from(clock.media_ns_at(t)) - leader) as i64;
            apply(&mut servo, &mut clock, t, e);
        }
        assert!(servo.is_locked());
        assert!(clock.rate_ppb().abs() < 1_000, "a phase jump is not drift");
        assert_eq!(
            clock.epoch(),
            1,
            "no initial step (zero offset), one step for the jump"
        );
    }

    #[test]
    fn persistent_frequency_change_relearns_rate_instead_of_stepping_forever() {
        for (old_rate, new_rate) in [(0, 350_000), (0, -350_000), (211_930, -138_070)] {
            let mut clock = MediaClock::new(0, 0);
            let mut servo = Servo::new(ServoConfig::default());
            let mut before = 0;
            let mut last_error = 0;
            for sec in 1..=160u64 {
                let t = sec * 1_000_000_000;
                let after = t.saturating_sub(60_000_000_000);
                let leader = i128::from(t)
                    + i128::from(t) * i128::from(old_rate) / 1_000_000_000
                    + i128::from(after) * i128::from(new_rate - old_rate) / 1_000_000_000;
                let e = (i128::from(clock.media_ns_at(t)) - leader) as i64;
                apply(&mut servo, &mut clock, t, e);
                if sec == 60 {
                    assert!(servo.is_locked(), "baseline failed to lock");
                    before = clock.epoch();
                }
                last_error = e;
            }
            assert!(servo.is_locked(), "{old_rate} -> {new_rate}: not locked");
            assert!(
                clock.epoch() <= before + 2,
                "{old_rate} -> {new_rate}: repeated steps: {}",
                clock.epoch() - before
            );
            assert!((clock.rate_ppb() - new_rate).abs() < 1_000);
            assert!(last_error.abs() < 1_000, "residual {last_error} ns");
        }
    }

    #[test]
    fn jittery_samples_still_lock() {
        // +-60 us alternating noise: raw samples exceed the 50 us threshold
        // but the median does not.
        let (servo, _, _) = run(20_000, 1_000_000_000, 60, |i| {
            [40_000, -35_000, 60_000, -20_000, 10_000][(i % 5) as usize]
        });
        assert!(servo.is_locked());
    }

    #[test]
    fn frequency_change_with_subsecond_jitter_relocks() {
        for new_rate in [-350_000i64, 350_000] {
            let mut clock = MediaClock::new(0, 0);
            let mut servo = Servo::new(ServoConfig::default());
            let mut before = 0;
            for i in 1..=1_600u64 {
                let t = i * 125_000_000;
                let after = t.saturating_sub(60_000_000_000);
                let leader =
                    i128::from(t) + i128::from(after) * i128::from(new_rate) / 1_000_000_000;
                let noise = [40_000, -35_000, 60_000, -20_000, 10_000][(i % 5) as usize];
                let e = (i128::from(clock.media_ns_at(t)) - leader) as i64 + noise;
                apply(&mut servo, &mut clock, t, e);
                if i == 480 {
                    assert!(servo.is_locked());
                    before = clock.epoch();
                }
            }
            assert!(servo.is_locked(), "jittery {new_rate} ppb did not relock");
            assert!(clock.epoch() <= before + 2, "repeated phase steps");
            assert!((clock.rate_ppb() - new_rate).abs() < 20_000);
        }
    }

    #[test]
    fn an_outlier_before_a_phase_jump_does_not_become_a_frequency_estimate() {
        let mut clock = MediaClock::new(0, 0);
        let mut servo = Servo::new(ServoConfig::default());
        for sec in 1..=120u64 {
            let t = sec * 1_000_000_000;
            let jump = if sec >= 61 { 2_000_000 } else { 0 };
            let noise = if sec == 60 { 150_000 } else { 0 };
            let e = (i128::from(clock.media_ns_at(t)) - i128::from(t) - jump) as i64 + noise;
            apply(&mut servo, &mut clock, t, e);
        }
        assert!(servo.is_locked());
        assert_eq!(clock.epoch(), 1);
        assert!(clock.rate_ppb().abs() < 1_000);
    }

    #[test]
    fn reset_in_a_rejected_streak_forgets_its_samples() {
        let (mut servo, mut clock, _) = run(0, 0, 40, |_| 0);
        assert!(matches!(
            servo.sample(41_000_000_000, 150_000, 0),
            ServoOutput::Rejected
        ));
        assert!(matches!(
            servo.sample(42_000_000_000, 200_000, 0),
            ServoOutput::Rejected
        ));
        servo.reset();
        for sec in 43..=100u64 {
            let t = sec * 1_000_000_000;
            let e = (i128::from(clock.media_ns_at(t)) - i128::from(t)) as i64;
            apply(&mut servo, &mut clock, t, e);
        }
        assert!(servo.is_locked());
        assert_eq!(clock.epoch(), 0);
        assert_eq!(clock.rate_ppb(), 0);
    }

    #[test]
    fn reset_forgets_history() {
        let (mut servo, _, _) = run(1_000, 0, 30, |_| 0);
        servo.reset();
        assert!(!servo.is_calibrated() && !servo.is_locked());
        assert!(matches!(
            servo.sample(5, 5_000_000, 0),
            ServoOutput::Step {
                correction_ns: -5_000_000,
                ..
            }
        ));
    }
}
