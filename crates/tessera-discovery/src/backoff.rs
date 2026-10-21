//! Rate limit for name conflicts (RFC 6762 section 8.1): a host that keeps
//! losing probes must not keep probing. After `MAX_CONFLICTS` conflicts within
//! `WINDOW` it waits `RETRY_WAIT` before the next attempt.
//!
//! Sans-IO: callers pass the time and do the waiting. Used by the Avahi
//! backend; the built-in backend leaves conflict handling to `mdns-sd`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Look-back window.
pub(crate) const WINDOW: Duration = Duration::from_secs(10);
/// Conflicts tolerated within the window.
pub(crate) const MAX_CONFLICTS: usize = 15;
/// How long to wait after exceeding the limit.
pub(crate) const RETRY_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
pub(crate) struct ConflictLimiter {
    seen: VecDeque<Instant>,
}

impl ConflictLimiter {
    /// Records a conflict at `now`; returns how long to wait before probing
    /// again if the limit is exceeded.
    pub(crate) fn record(&mut self, now: Instant) -> Option<Duration> {
        self.seen.push_back(now);
        while self
            .seen
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > WINDOW)
        {
            self.seen.pop_front();
        }
        if self.seen.len() > MAX_CONFLICTS {
            // The wait counts as a fresh start.
            self.seen.clear();
            Some(RETRY_WAIT)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixteen_conflicts_in_ten_seconds_force_a_wait() {
        let t0 = Instant::now();
        let mut l = ConflictLimiter::default();
        for i in 0..MAX_CONFLICTS {
            assert_eq!(l.record(t0 + Duration::from_millis(100 * i as u64)), None);
        }
        assert_eq!(l.record(t0 + Duration::from_millis(1600)), Some(RETRY_WAIT));
        // After the wait the count starts again.
        assert_eq!(l.record(t0 + Duration::from_secs(8)), None);
    }

    #[test]
    fn a_slow_trickle_never_waits() {
        let t0 = Instant::now();
        let mut l = ConflictLimiter::default();
        for i in 0..200u64 {
            assert_eq!(l.record(t0 + Duration::from_secs(i)), None);
        }
    }
}
