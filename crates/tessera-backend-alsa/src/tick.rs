//! The daemon's engine tick, estimated from the wake ticks a client drains.
//!
//! `Attach` and the ring header do not carry the engine period (an audio-ipc
//! ABI gap, see PROGRESS.md), so it is estimated. Every drain that finds ticks
//! is a sample: the time since the previous such drain over the ticks it found,
//! smoothed. That ratio stays right when the ticks are drained in bursts (an
//! application woken every 25 ms finds 25 at a time).
//!
//! The estimate is an upper bound of a steady tick on purpose. A drain is an
//! instant the client (or its scheduler) chose, not a tick arrival, and the
//! daemon fires a tick up to one tick late (it skips one it missed by more, it
//! never catches up in a burst), so the ticks found in a span are only known to
//! within two. `n` ticks arrive one period apart, after the reference drain
//! emptied the socket and before the last drain ended: at most one period of
//! lateness separates the first from the last, so more than `n - 2` periods
//! fit in that span, whatever the drain timing. Dividing by `n - 2` therefore
//! never falls below the real period. Dividing by `n` (the middle of the
//! bounds) fell below it by up to `1 / n`, 8 % over the 12 samples that were
//! enough to trust it: a reference drain that was late (it found a burst)
//! followed by a prompt one made 48 frames come out as 44, and the plugin then
//! offered a buffer the daemon cannot feed. The price of the bound is an
//! estimate that is high by `2 / (n - 2)`: about 23 % when it is first trusted
//! (60 frames for a 48-frame tick), 4 % once the smoothing window is full.
//!
//! The smoothing weighs a sample `DECAY` less than the next, which keeps the
//! bound (the per-sample errors telescope to at most the allowance). A gap
//! longer than `WINDOW` ends the chain of samples; its allowance stays, weighed
//! like its last sample, so the estimate still holds across idle periods.

use std::time::{Duration, Instant};

/// Two drains that found ticks further apart than this say nothing about the
/// tick interval (the socket fills up and the daemon drops ticks on purpose,
/// so a long gap is a count of nothing).
const WINDOW: Duration = Duration::from_millis(100);
/// Samples needed before the interval is trusted.
const MIN_SAMPLES: u32 = 12;
/// The weight of a sample relative to the next one.
const DECAY: f64 = 0.98;
/// Ticks a chain of samples may have counted too many: one for the client's
/// drains, one for the daemon firing late.
const ALLOWANCE: f64 = 2.0;

/// The tick interval, from the drains of the wake socket.
#[derive(Debug, Clone, Default)]
pub(crate) struct TickEstimator {
    /// Start of the last drain that found ticks: the next span begins there.
    since: Option<Instant>,
    /// Decayed sums of the spans in nanoseconds and of the ticks counted in
    /// them.
    span_ns: f64,
    ticks: f64,
    /// The allowance of the chains that ended, decayed with the samples.
    closed: f64,
    /// Whether the current chain has a sample (it then holds one allowance).
    chained: bool,
    samples: u32,
}

impl TickEstimator {
    /// Forgets everything.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// A drain found `found` (not 0) ticks. It ran from `begin` (before the
    /// first read of the socket) to `end` (after the last one), so the span
    /// that is counted covers every instant the ticks may have arrived in.
    pub(crate) fn record(&mut self, begin: Instant, end: Instant, found: u64) {
        if let Some(since) = self.since {
            let span = end.saturating_duration_since(since);
            if span <= WINDOW {
                self.span_ns = self.span_ns * DECAY + span.as_nanos() as f64;
                self.ticks = self.ticks * DECAY + found as f64;
                self.closed *= DECAY;
                self.chained = true;
                self.samples = self.samples.saturating_add(1);
            } else if self.chained {
                self.closed += ALLOWANCE;
                self.chained = false;
            }
        }
        self.since = Some(begin);
    }

    /// The tick interval in nanoseconds, once enough samples were seen.
    pub(crate) fn tick_ns(&self) -> Option<f64> {
        if self.samples < MIN_SAMPLES {
            return None;
        }
        let open = if self.chained { ALLOWANCE } else { 0.0 };
        let counted = self.ticks - self.closed - open;
        (counted > 0.0).then(|| self.span_ns / counted)
    }

    /// The tick in frames of `rate` Hz, rounded up; `0` while unknown.
    pub(crate) fn frames(&self, rate: u32) -> u64 {
        let Some(tick_ns) = self.tick_ns() else {
            return 0;
        };
        let frames = (tick_ns * f64::from(rate) / 1e9).ceil();
        if frames.is_finite() && frames >= 1.0 {
            frames as u64
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// A small deterministic generator (the same schedules on every run).
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }
    }

    /// A daemon that sends tick `m` at `phase + m * period + late(m)`
    /// nanoseconds (`late` below one period, so ticks stay in order) and a
    /// client that drains the socket at times it chooses. A drain begins at
    /// `begin`, takes `took`, and finds the socket empty at an instant `cut`
    /// inside that: the ticks sent up to there are the ones it finds, the
    /// others wait for the next drain.
    struct Wire<F: FnMut(u64) -> u64> {
        base: Instant,
        period: u64,
        phase: u64,
        late: F,
        /// The next tick to be scheduled.
        slot: u64,
        /// Sent, not yet found.
        pending: VecDeque<u64>,
        est: TickEstimator,
    }

    impl<F: FnMut(u64) -> u64> Wire<F> {
        fn new(period: u64, phase: u64, late: F) -> Wire<F> {
            Wire {
                base: Instant::now(),
                period,
                phase,
                late,
                slot: 0,
                pending: VecDeque::new(),
                est: TickEstimator::default(),
            }
        }

        fn at(&self, ns: u64) -> Instant {
            self.base + Duration::from_nanos(ns)
        }

        fn slot_time(&self, slot: u64) -> u64 {
            self.phase + slot * self.period
        }

        /// One drain; returns the ticks it found.
        fn drain(&mut self, begin: u64, took: u64, cut: u64) -> u64 {
            assert!(cut <= took);
            let cut = begin + cut;
            while self.slot_time(self.slot) <= cut {
                let late = (self.late)(self.slot);
                assert!(late < self.period);
                self.pending.push_back(self.slot_time(self.slot) + late);
                self.slot += 1;
            }
            let mut found = 0;
            while self.pending.front().is_some_and(|a| *a <= cut) {
                self.pending.pop_front();
                found += 1;
            }
            if found > 0 {
                self.est
                    .record(self.at(begin), self.at(begin + took), found);
            }
            found
        }

        /// When the next tick arrives after `t`, at the earliest (the ticks
        /// not scheduled yet may be late).
        fn next_tick_after(&self, t: u64) -> u64 {
            let unscheduled = self.slot_time(self.slot).max(t + 1);
            let queued = self.pending.iter().copied().find(|a| *a > t);
            queued.map_or(unscheduled, |q| q.min(unscheduled))
        }
    }

    fn on_time(_: u64) -> u64 {
        0
    }

    fn nominal(period_ns: u64) -> f64 {
        period_ns as f64
    }

    /// The failing case of the ALSA plugin tests, as recorded under load: the
    /// client thread is held up between the attach and its first drain, finds
    /// six ticks, and the next tick arrives 47 microseconds after that drain.
    /// One tick per millisecond follows. Dividing the span by the ticks counted
    /// gave 0.917 ms (44 frames at 48 kHz).
    #[test]
    fn a_late_reference_drain_followed_by_a_prompt_one_is_not_a_short_tick() {
        let period = 1_000_000;
        let mut w = Wire::new(period, 0, on_time);
        let first = 5 * period + 953_000;
        assert_eq!(w.drain(first, 2_000, 1_000), 6);
        let mut at = w.next_tick_after(first + 1_000);
        assert_eq!(at - first, 47_000);
        for _ in 0..12 {
            assert_eq!(w.drain(at + 1_000, 2_000, 1_000), 1);
            at += period;
        }
        let ns = w.est.tick_ns().unwrap();
        assert!(ns >= nominal(period), "{ns} ns is below the 1 ms tick");
        assert!(ns <= 1.3 * nominal(period), "{ns} ns is far above it");
        let frames = w.est.frames(48_000);
        assert!((48..=62).contains(&frames), "{frames} frames");
        // The plain ratio of span to ticks counted, which this replaces.
        let ratio_frames = (w.est.span_ns / w.est.ticks * 48_000.0 / 1e9).ceil();
        assert!(ratio_frames < 48.0, "{ratio_frames} frames");
    }

    /// The second failure seen under twice the load: the daemon fires a tick
    /// almost a period late and the next one on time, so two ticks arrive a
    /// microsecond apart (the first drain finds two, the next one 97
    /// microseconds later two more). The daemon is within its contract (it
    /// skips a tick only when it missed it by more than a tick).
    #[test]
    fn a_tick_fired_a_period_late_followed_by_the_next_on_time_is_not_a_short_tick() {
        let period = 1_000_000;
        // Tick 5 arrives 1 ns before tick 6, which is on time.
        let late = |slot: u64| if slot == 5 { period - 1 } else { 0 };
        let mut w = Wire::new(period, 0, late);
        // The reference drain finds ticks 0..=4, the next one both late ones.
        assert_eq!(w.drain(5_903_000, 3_000, 2_000), 5);
        assert_eq!(w.drain(6_000_000, 3_000, 2_000), 2);
        let mut at = 7 * period + 100_000;
        for _ in 0..11 {
            assert_eq!(w.drain(at, 3_000, 2_000), 1);
            at += period;
        }
        let ns = w.est.tick_ns().unwrap();
        assert!(ns >= nominal(period), "{ns} ns is below the 1 ms tick");
        // One tick fewer in the divisor (the allowance of the client alone)
        // is not enough for this daemon.
        let one = w.est.span_ns / (w.est.ticks - 1.0);
        assert!(one < nominal(period), "{one} ns");
    }

    /// Whatever the drain timing and the daemon's lateness, a steady tick is
    /// never estimated below its period: random phases, drains that are prompt,
    /// late by up to several ticks, in bursts, and separated by gaps longer
    /// than the window (which end a chain of samples); a daemon on time, up to
    /// a period late at random, or alternating between the two extremes.
    #[test]
    fn a_steady_tick_is_never_estimated_below_its_period() {
        let mut rng = Lcg(0x5eed);
        let mut checked = 0u32;
        for round in 0..3000u32 {
            let period = [250_000, 1_000_000, 2_500_000, 5_000_000, 20_833_333][round as usize % 5];
            let mut lateness = Lcg(u64::from(round) + 1);
            let mode = round % 3;
            let late = move |_slot: u64| match mode {
                0 => 0,
                1 => lateness.below(period),
                // At random one of the two extremes.
                _ => (period - 1) * lateness.below(2),
            };
            let mut w = Wire::new(period, rng.below(period), late);
            let mut t = rng.below(3 * period);
            for _ in 0..300 {
                let took = 1_000 + rng.below(4_000);
                w.drain(t, took, rng.below(took + 1));
                t += took;
                t += match rng.below(10) {
                    // Right behind the next tick.
                    0..=3 => w.next_tick_after(t) - t + rng.below(30_000),
                    // The next tick or the one after, a little late.
                    4..=6 => w.next_tick_after(t) - t + rng.below(2 * period),
                    // A cadence of its own: a few milliseconds.
                    7 | 8 => 1_000 + rng.below(30_000_000),
                    // A long gap (a stopped stream), now and then.
                    _ => 90_000_000 + rng.below(400_000_000),
                };
                if let Some(ns) = w.est.tick_ns() {
                    checked += 1;
                    assert!(
                        ns >= nominal(period),
                        "round {round}: {ns} ns for a {period} ns tick after {} samples",
                        w.est.samples
                    );
                }
            }
        }
        assert!(checked > 100_000, "{checked}");
    }

    /// Drains that follow the ticks closely give an estimate near the period:
    /// about 23 % over when it is first trusted, 4 % when the window is full.
    #[test]
    fn prompt_drains_give_a_tight_estimate() {
        let period = 1_000_000;
        let mut w = Wire::new(period, 300_000, on_time);
        let mut t = 300_000;
        let mut first = None;
        for i in 0..300 {
            w.drain(t + 3_000, 2_000, 1_000);
            if i == 12 {
                first = w.est.tick_ns();
            }
            t += period;
        }
        let first = first.expect("measured after 12 samples");
        assert!(
            (nominal(period)..=1.25 * nominal(period)).contains(&first),
            "{first}"
        );
        let steady = w.est.tick_ns().unwrap();
        assert!(
            (nominal(period)..=1.06 * nominal(period)).contains(&steady),
            "{steady}"
        );
        // 48 frames, plus the 4 % of the bound and the microseconds a drain
        // takes.
        assert!((50..=51).contains(&w.est.frames(48_000)));
    }

    /// An application woken every 25 ms finds 25 ticks per drain: the ratio is
    /// time over ticks, not the gap between drains.
    #[test]
    fn batched_drains_are_counted_by_the_ticks_they_find() {
        let period = 1_000_000;
        let mut w = Wire::new(period, 0, on_time);
        for i in 1..=40u64 {
            // The first drain also finds the tick sent at time zero.
            let found = w.drain(i * 25 * period + 400, 2_000, 1_000);
            assert_eq!(found, if i == 1 { 26 } else { 25 });
        }
        let ns = w.est.tick_ns().unwrap();
        assert!(
            (nominal(period)..=1.01 * nominal(period)).contains(&ns),
            "{ns}"
        );
    }

    #[test]
    fn nothing_is_known_before_twelve_samples() {
        let mut w = Wire::new(1_000_000, 0, on_time);
        let mut t = 10_000;
        for _ in 0..12 {
            w.drain(t, 2_000, 1_000);
            assert_eq!(w.est.frames(48_000), 0);
            t += 1_000_000;
        }
        // The first drain only starts the first span: twelve samples need a
        // thirteenth drain.
        w.drain(t, 2_000, 1_000);
        assert!(w.est.frames(48_000) >= 48);
    }

    /// Idle periods, a socket that overflowed and stop/start cycles do not
    /// change what is known (a long gap is not a sample).
    #[test]
    fn a_long_gap_does_not_change_the_estimate() {
        let period = 1_000_000;
        let mut w = Wire::new(period, 0, on_time);
        let mut t = 10_000;
        for _ in 0..80 {
            w.drain(t, 2_000, 1_000);
            t += period;
        }
        let before = w.est.tick_ns().unwrap();
        // Two seconds of nobody draining: 2000 ticks pile up (the real socket
        // would have dropped most), found by one drain.
        t += 2_000_000_000;
        assert!(w.drain(t, 2_000, 1_000) > 1000);
        // Not a sample: the chain that ended keeps the allowance the open chain
        // had, so nothing changes.
        let after = w.est.tick_ns().unwrap();
        assert_eq!(after, before);
        assert!(after >= nominal(period));
    }

    #[test]
    fn reset_forgets_the_samples() {
        let mut w = Wire::new(1_000_000, 0, on_time);
        let mut t = 10_000;
        for _ in 0..20 {
            w.drain(t, 2_000, 1_000);
            t += 1_000_000;
        }
        assert!(w.est.frames(48_000) > 0);
        w.est.reset();
        assert_eq!(w.est.frames(48_000), 0);
        assert_eq!(w.est.tick_ns(), None);
    }
}
