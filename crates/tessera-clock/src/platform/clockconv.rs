//! Reading clocks and converting kernel timestamps to the monotonic domain.
//! Read-only: nothing here writes any clock.

use nix::time::{ClockId, clock_gettime};

fn ns_of(id: ClockId) -> u64 {
    match clock_gettime(id) {
        Ok(ts) => (ts.tv_sec() as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec() as u64),
        Err(_) => 0,
    }
}

/// `CLOCK_MONOTONIC` in nanoseconds (vDSO).
#[must_use]
pub fn mono_ns() -> u64 {
    ns_of(ClockId::CLOCK_MONOTONIC)
}

/// `CLOCK_REALTIME` in nanoseconds (read only).
#[must_use]
pub fn realtime_ns() -> u64 {
    ns_of(ClockId::CLOCK_REALTIME)
}

/// `CLOCK_TAI` in nanoseconds (read only): the arbitrary-but-plausible
/// starting time of a leader (ADR 0005).
#[must_use]
pub fn tai_ns() -> u64 {
    ns_of(ClockId::CLOCK_TAI)
}

/// Converts `CLOCK_REALTIME` readings (software timestamps) to
/// `CLOCK_MONOTONIC`, detecting steps of the realtime clock.
///
/// Call [`RealtimeToMono::refresh`] once per receive batch, before
/// converting the batch. The offset `mono - realtime` is measured with a
/// bracketed read (`mono, real, mono`); it is constant except for NTP
/// slewing (bounded by 500 ppm) and steps. A change larger than the
/// threshold plus the slew allowance means a step: the batch in flight cannot
/// be dated reliably, so conversions return `None` until the next refresh
/// (ADR 0005: "realtime steps are detected and the affected samples
/// discarded").
#[derive(Debug, Clone, Copy)]
pub struct RealtimeToMono {
    offset: Option<i64>,
    last_refresh_mono: u64,
    step_threshold_ns: u64,
    poisoned: bool,
    steps: u64,
}

/// A bracket wider than this was probably preempted; read again.
const MAX_BRACKET_NS: u64 = 50_000;
/// Worst-case NTP slew (ppm) used to widen the step threshold with the
/// time since the last refresh.
const SLEW_PPM: u64 = 600;

impl RealtimeToMono {
    /// `step_threshold_ns`: jumps larger than this (beyond slew) count as a
    /// step.
    #[must_use]
    pub fn new(step_threshold_ns: u64) -> Self {
        RealtimeToMono {
            offset: None,
            last_refresh_mono: 0,
            step_threshold_ns,
            poisoned: false,
            steps: 0,
        }
    }

    /// Re-measures the offset. Returns `false` if a step was detected.
    pub fn refresh(&mut self) -> bool {
        let mut best: Option<(u64, i64, u64)> = None; // (width, offset, mono_mid)
        for _ in 0..3 {
            let m1 = mono_ns();
            let r = realtime_ns();
            let m2 = mono_ns();
            let width = m2.saturating_sub(m1);
            let mid = m1 + width / 2;
            let off = (i128::from(mid) - i128::from(r))
                .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
            if best.is_none_or(|b| width < b.0) {
                best = Some((width, off, mid));
            }
            if width <= MAX_BRACKET_NS {
                break;
            }
        }
        let Some((_, off, mid)) = best else {
            return true;
        };
        let mut ok = true;
        if let Some(prev) = self.offset {
            let elapsed = mid.saturating_sub(self.last_refresh_mono);
            let allowed = self
                .step_threshold_ns
                .saturating_add(elapsed / 1_000_000 * SLEW_PPM / 1_000);
            if off.abs_diff(prev) > allowed {
                self.steps += 1;
                ok = false;
            }
        }
        self.offset = Some(off);
        self.last_refresh_mono = mid;
        self.poisoned = !ok;
        ok
    }

    /// `real_ns` (a `CLOCK_REALTIME` reading) as monotonic ns, or `None` if
    /// the batch is poisoned by a realtime step or no refresh happened yet.
    #[must_use]
    pub fn convert(&self, real_ns: u64) -> Option<u64> {
        if self.poisoned {
            return None;
        }
        let off = self.offset?;
        let v = i128::from(real_ns) + i128::from(off);
        u64::try_from(v).ok()
    }

    /// Realtime steps detected so far.
    #[must_use]
    pub fn steps_detected(&self) -> u64 {
        self.steps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_matches_the_clocks() {
        let mut c = RealtimeToMono::new(2_000_000);
        assert!(
            c.convert(realtime_ns()).is_none(),
            "no offset before refresh"
        );
        assert!(c.refresh());
        let real = realtime_ns();
        let m = c.convert(real).unwrap();
        let direct = mono_ns();
        assert!(direct.abs_diff(m) < 50_000_000, "{direct} vs {m}");
    }

    #[test]
    fn a_step_poisons_the_batch_and_recovers() {
        let mut c = RealtimeToMono::new(2_000_000);
        assert!(c.refresh());
        // Simulate a realtime step of 1 s by corrupting the stored offset.
        if let Some(o) = c.offset.as_mut() {
            *o += 1_000_000_000;
        }
        assert!(!c.refresh());
        assert!(c.convert(realtime_ns()).is_none());
        assert_eq!(c.steps_detected(), 1);
        assert!(c.refresh(), "stable again");
        assert!(c.convert(realtime_ns()).is_some());
    }

    #[test]
    fn tai_is_at_least_realtime() {
        assert!(tai_ns() >= realtime_ns().saturating_sub(1_000_000_000));
    }
}
