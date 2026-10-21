//! Per-peer rate limit for the local control socket (ADR 0010).
//!
//! The state engine's own limiter only covers commands with
//! `Origin::Protocol`; local clients are `Origin::Cli` (labelling them as
//! protocol peers would mislabel every resulting event). This is the same
//! token-bucket design, keyed by uid, applied to every request frame on the
//! control socket, and parameterised by the engine's
//! [`RateLimit`] so that both limiters read one configuration.

use std::collections::HashMap;
use std::time::Duration;

use tessera_state::env::RateLimit;

#[derive(Debug)]
struct Bucket {
    milli_tokens: u64,
    last: Duration,
}

/// Token buckets per peer key (a uid). Time is passed in, so the limiter is
/// deterministic under test.
#[derive(Debug)]
pub struct PeerLimiter {
    cfg: RateLimit,
    buckets: HashMap<u32, Bucket>,
}

impl PeerLimiter {
    /// A limiter with the given parameters.
    #[must_use]
    pub fn new(cfg: RateLimit) -> Self {
        Self {
            cfg,
            buckets: HashMap::new(),
        }
    }

    /// Change the parameters (a reload of `[control] rate_burst` /
    /// `rate_per_second`); known peers keep their buckets, capped at the new
    /// burst on their next request (the refill is `min(capacity, ..)`). A
    /// smaller `max_peers` takes effect as new peers arrive (no eviction
    /// here; the daemon passes the constant `RATE_MAX_PEERS`).
    pub fn set_config(&mut self, cfg: RateLimit) {
        self.cfg = cfg;
    }

    /// The parameters in effect.
    #[must_use]
    pub fn config(&self) -> RateLimit {
        self.cfg
    }

    fn capacity(&self) -> u64 {
        u64::from(self.cfg.burst.max(1)) * 1000
    }

    /// May `peer` send one more request at time `now`?
    pub fn allow(&mut self, peer: u32, now: Duration) -> bool {
        self.try_take(peer, now).is_ok()
    }

    /// Take a token for `peer` at `now`, or say how long until one is
    /// available (never less than 1 ms, never more than 2 s), so the caller can
    /// stop reading from the sender until then.
    ///
    /// # Errors
    /// The time to wait.
    pub fn try_take(&mut self, peer: u32, now: Duration) -> Result<(), Duration> {
        let cap = self.capacity();
        if !self.buckets.contains_key(&peer) {
            if self.buckets.len() >= self.cfg.max_peers.max(1) {
                let oldest = self
                    .buckets
                    .iter()
                    .min_by_key(|(_, b)| b.last)
                    .map(|(k, _)| *k);
                if let Some(k) = oldest {
                    self.buckets.remove(&k);
                }
            }
            self.buckets.insert(
                peer,
                Bucket {
                    milli_tokens: cap,
                    last: now,
                },
            );
        }
        let rate = u128::from(self.cfg.per_second);
        let Some(b) = self.buckets.get_mut(&peer) else {
            return Ok(());
        };
        let elapsed = now.saturating_sub(b.last).as_nanos();
        let refill = u64::try_from(elapsed * rate / 1_000_000).unwrap_or(u64::MAX);
        b.milli_tokens = b.milli_tokens.saturating_add(refill).min(cap);
        b.last = now;
        if b.milli_tokens >= 1000 {
            b.milli_tokens -= 1000;
            Ok(())
        } else {
            // Nanoseconds per milli-token: 1e6 / per_second.
            let need = 1000 - b.milli_tokens;
            let ns = (u128::from(need) * 1_000_000)
                .checked_div(rate)
                .unwrap_or(2_000_000_000);
            let ns = u64::try_from(ns).unwrap_or(u64::MAX);
            Err(Duration::from_nanos(ns).clamp(Duration::from_millis(1), Duration::from_secs(2)))
        }
    }

    /// Number of peers tracked.
    #[must_use]
    pub fn peers(&self) -> usize {
        self.buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(burst: u32, per_second: u32, max_peers: usize) -> RateLimit {
        RateLimit {
            burst,
            per_second,
            max_peers,
        }
    }

    #[test]
    fn burst_then_refuse_then_refill() {
        let mut l = PeerLimiter::new(cfg(3, 2, 8));
        let t0 = Duration::from_secs(10);
        assert!(l.allow(1, t0));
        assert!(l.allow(1, t0));
        assert!(l.allow(1, t0));
        assert!(!l.allow(1, t0));
        // 2 tokens per second: after 500 ms exactly one token is back.
        assert!(l.allow(1, t0 + Duration::from_millis(500)));
        assert!(!l.allow(1, t0 + Duration::from_millis(500)));
    }

    #[test]
    fn a_refusal_says_how_long_to_wait() {
        let mut l = PeerLimiter::new(cfg(1, 10, 8));
        let t = Duration::from_secs(1);
        assert!(l.allow(1, t));
        let wait = l.try_take(1, t).unwrap_err();
        // 10 tokens per second: a whole token takes 100 ms.
        assert!(
            wait >= Duration::from_millis(99) && wait <= Duration::from_millis(100),
            "{wait:?}"
        );
        assert!(l.try_take(1, t + wait).is_ok());
        let mut slow = PeerLimiter::new(cfg(1, 0, 8));
        assert!(slow.allow(1, t));
        assert_eq!(slow.try_take(1, t), Err(Duration::from_secs(2)));
    }

    #[test]
    fn peers_are_independent() {
        let mut l = PeerLimiter::new(cfg(1, 1, 8));
        let t = Duration::ZERO;
        assert!(l.allow(1, t));
        assert!(!l.allow(1, t));
        assert!(l.allow(2, t));
    }

    #[test]
    fn tracked_peers_are_bounded() {
        let mut l = PeerLimiter::new(cfg(1, 1, 4));
        for p in 0..100 {
            let _ = l.allow(p, Duration::from_millis(u64::from(p)));
        }
        assert!(l.peers() <= 4);
    }

    #[test]
    fn clock_going_backwards_does_not_panic_or_mint_tokens() {
        let mut l = PeerLimiter::new(cfg(1, 1000, 4));
        assert!(l.allow(1, Duration::from_secs(5)));
        assert!(!l.allow(1, Duration::from_secs(1)));
    }
}
