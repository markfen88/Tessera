//! Reconnect back-off: exponential with a cap and a deterministic jitter.

use std::time::Duration;

/// Back-off parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffConfig {
    /// First delay.
    pub initial: Duration,
    /// Largest delay.
    pub max: Duration,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(200),
            max: Duration::from_secs(5),
        }
    }
}

/// Exponential back-off state. `next_delay` doubles every call up to `max`;
/// `reset` is called after a successful attach.
#[derive(Debug, Clone)]
pub struct Backoff {
    cfg: BackoffConfig,
    attempt: u32,
    jitter: u64,
}

impl Backoff {
    /// Creates a back-off. `seed` decorrelates several endpoints so they do
    /// not hammer the daemon in lock-step after a restart.
    pub fn new(cfg: BackoffConfig, seed: u64) -> Self {
        Self {
            cfg,
            attempt: 0,
            jitter: seed | 1,
        }
    }

    /// Returns the delay before the next attempt (never zero, never above
    /// `max`).
    pub fn next_delay(&mut self) -> Duration {
        let shift = self.attempt.min(30);
        self.attempt = self.attempt.saturating_add(1);
        let base = self
            .cfg
            .initial
            .saturating_mul(1u32 << shift)
            .min(self.cfg.max)
            .max(Duration::from_millis(1));
        // xorshift: cheap deterministic jitter of up to -25 %.
        self.jitter ^= self.jitter << 13;
        self.jitter ^= self.jitter >> 7;
        self.jitter ^= self.jitter << 17;
        let nanos = base.as_nanos().min(u128::from(u64::MAX)) as u64;
        let cut = nanos / 4 * (self.jitter % 1000) / 1000;
        Duration::from_nanos((nanos - cut).max(1_000_000))
    }

    /// Forgets the failures (after a successful attach).
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Number of delays handed out since the last reset.
    pub fn attempts(&self) -> u32 {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_and_is_capped() {
        let mut b = Backoff::new(BackoffConfig::default(), 7);
        let mut last = Duration::ZERO;
        for _ in 0..4 {
            let d = b.next_delay();
            assert!(d > Duration::ZERO && d <= Duration::from_secs(5));
            last = last.max(d);
        }
        for _ in 0..40 {
            assert!(b.next_delay() <= Duration::from_secs(5));
        }
        assert!(last >= Duration::from_millis(150));
        assert!(b.attempts() > 30);
    }

    #[test]
    fn reset_starts_over() {
        let mut b = Backoff::new(BackoffConfig::default(), 1);
        for _ in 0..10 {
            b.next_delay();
        }
        b.reset();
        assert!(b.next_delay() <= Duration::from_millis(200));
    }

    #[test]
    fn jitter_stays_within_a_quarter() {
        let cfg = BackoffConfig {
            initial: Duration::from_millis(400),
            max: Duration::from_millis(400),
        };
        for seed in 0..50u64 {
            let mut b = Backoff::new(cfg, seed);
            let d = b.next_delay();
            assert!(d >= Duration::from_millis(300) && d <= Duration::from_millis(400));
        }
    }
}
