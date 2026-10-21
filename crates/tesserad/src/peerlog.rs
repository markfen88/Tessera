//! Rate-limited logging of what peers do wrong.
//!
//! A peer can cause refusals as fast as it can send frames (128 per second per
//! uid at the default rate limit). Logging each one would let it flood the
//! journal, so refusals are logged at `debug`, at most one line per uid per
//! interval, with the number of suppressed occurrences. Only static strings
//! (command kinds, reasons) are logged, never peer-chosen payloads.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Per-uid summary logger.
#[derive(Debug)]
pub struct PeerLog {
    interval: Duration,
    max_uids: usize,
    state: Mutex<HashMap<u32, (Instant, u64)>>,
}

impl PeerLog {
    /// One summary line per uid per `interval`.
    #[must_use]
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            max_uids: 1024,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Decide whether an occurrence at `now` is logged. `Some(n)` = log it, and
    /// `n` similar ones were suppressed since the last line.
    pub fn admit(&self, uid: u32, now: Instant) -> Option<u64> {
        let mut m = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if m.len() >= self.max_uids && !m.contains_key(&uid) {
            m.clear();
        }
        match m.get_mut(&uid) {
            Some((last, suppressed)) => {
                if now.saturating_duration_since(*last) >= self.interval {
                    let n = std::mem::take(suppressed);
                    *last = now;
                    Some(n)
                } else {
                    *suppressed += 1;
                    None
                }
            }
            None => {
                m.insert(uid, (now, 0));
                Some(0)
            }
        }
    }

    /// Log `what` (with a static `detail` such as a command kind) for `uid`
    /// at `debug` if the interval allows.
    pub fn note(&self, uid: u32, what: &'static str, detail: &'static str) {
        if let Some(n) = self.admit(uid, Instant::now()) {
            tracing::debug!(uid, suppressed = n, detail, "peer: {what}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_per_interval_with_a_suppressed_count() {
        let l = PeerLog::new(Duration::from_secs(5));
        let t0 = Instant::now();
        assert_eq!(l.admit(1, t0), Some(0));
        for i in 1..=100 {
            assert_eq!(l.admit(1, t0 + Duration::from_millis(i)), None);
        }
        assert_eq!(l.admit(1, t0 + Duration::from_secs(6)), Some(100));
        assert_eq!(l.admit(1, t0 + Duration::from_secs(7)), None);
    }

    #[test]
    fn uids_are_independent_and_the_table_is_bounded() {
        let l = PeerLog::new(Duration::from_secs(5));
        let t0 = Instant::now();
        assert_eq!(l.admit(1, t0), Some(0));
        assert_eq!(l.admit(2, t0), Some(0));
        for uid in 0..5000 {
            let _ = l.admit(uid, t0);
        }
        assert!(l.state.lock().unwrap().len() <= 1024);
    }
}
