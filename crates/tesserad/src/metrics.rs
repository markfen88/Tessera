//! Daemon counters, answered to `Query::Metrics` (ADR 0009).
//!
//! Plain atomics, updated on the control plane only. Clock state per
//! interface comes from the state engine. The media engine's counters live in
//! [`MediaStats`] (the real-time worker only does relaxed atomic adds) and are
//! merged into the reply here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use tessera_control_types::{AudioMetrics, ClockStatus, DaemonMetrics, Metrics};

use crate::media::stats::MediaStats;
use crate::supervisor::RestartCounter;

/// All counters and gauges.
#[derive(Debug)]
pub struct Counters {
    started: Instant,
    /// Open control connections (gauge).
    pub control_connections: AtomicU64,
    /// Control connections accepted.
    pub control_connections_total: AtomicU64,
    /// Control connections refused by a cap.
    pub control_connections_refused: AtomicU64,
    /// Request frames received.
    pub requests_total: AtomicU64,
    /// Requests dropped by the rate limiter.
    pub requests_rate_limited: AtomicU64,
    /// Commands refused for lack of permission.
    pub requests_denied: AtomicU64,
    /// Frames that were not valid requests.
    pub requests_invalid: AtomicU64,
    /// Event subscribers (gauge).
    pub subscribers: AtomicU64,
    /// Subscriber lag events.
    pub subscriber_lags: AtomicU64,
    /// Audio sessions (gauge).
    pub audio_sessions: AtomicU64,
    /// Audio sessions ever attached.
    pub audio_sessions_total: AtomicU64,
    /// Audio rejects sent.
    pub audio_rejects: AtomicU64,
    /// Ring bytes reserved (gauge).
    pub audio_ring_bytes: AtomicU64,
    /// Audio connections refused by a cap.
    pub audio_connections_refused: AtomicU64,
    /// The media engine's counters and flow registry.
    pub media: Arc<MediaStats>,
    restarts: Mutex<Vec<RestartCounter>>,
}

impl Default for Counters {
    fn default() -> Self {
        Self::new()
    }
}

/// Increment helper.
pub fn inc(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// Decrement helper (gauges).
pub fn dec(c: &AtomicU64) {
    c.fetch_sub(1, Ordering::Relaxed);
}

impl Counters {
    /// All zero, uptime starting now.
    #[must_use]
    pub fn new() -> Self {
        let z = || AtomicU64::new(0);
        Self {
            started: Instant::now(),
            control_connections: z(),
            control_connections_total: z(),
            control_connections_refused: z(),
            requests_total: z(),
            requests_rate_limited: z(),
            requests_denied: z(),
            requests_invalid: z(),
            subscribers: z(),
            subscriber_lags: z(),
            audio_sessions: z(),
            audio_sessions_total: z(),
            audio_rejects: z(),
            audio_ring_bytes: z(),
            audio_connections_refused: z(),
            media: Arc::new(MediaStats::default()),
            restarts: Mutex::new(Vec::new()),
        }
    }

    /// Include a supervisor's restart counter in `task_restarts`.
    pub fn track_restarts(&self, c: RestartCounter) {
        self.restarts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(c);
    }

    /// A consistent-enough snapshot.
    #[must_use]
    pub fn snapshot(&self, clocks: Vec<ClockStatus>) -> Metrics {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let task_restarts = self
            .restarts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .sum();
        let mut d = DaemonMetrics::new();
        d.uptime_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        d.control_connections = g(&self.control_connections);
        d.control_connections_total = g(&self.control_connections_total);
        d.control_connections_refused = g(&self.control_connections_refused);
        d.requests_total = g(&self.requests_total);
        d.requests_rate_limited = g(&self.requests_rate_limited);
        d.requests_denied = g(&self.requests_denied);
        d.requests_invalid = g(&self.requests_invalid);
        d.subscribers = g(&self.subscribers);
        d.subscriber_lags = g(&self.subscriber_lags);
        d.task_restarts = task_restarts;
        let mut a = AudioMetrics::new();
        a.sessions = g(&self.audio_sessions);
        a.sessions_total = g(&self.audio_sessions_total);
        a.rejects_total = g(&self.audio_rejects);
        a.ring_bytes = g(&self.audio_ring_bytes);
        a.connections_refused = g(&self.audio_connections_refused);
        Metrics::new(d, a, clocks)
            .with_engine(self.media.engine.snapshot())
            .with_flows(self.media.flows())
    }
}

/// Decrements a gauge when dropped.
#[derive(Debug)]
pub struct GaugeGuard<'a>(pub &'a AtomicU64);

impl Drop for GaugeGuard<'_> {
    fn drop(&mut self) {
        dec(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reflects_counters_and_restarts() {
        let c = Counters::new();
        inc(&c.requests_total);
        inc(&c.requests_total);
        inc(&c.control_connections);
        {
            let _g = GaugeGuard(&c.control_connections);
        }
        let r: RestartCounter = Default::default();
        r.fetch_add(3, Ordering::Relaxed);
        c.track_restarts(r);
        let m = c.snapshot(Vec::new());
        assert_eq!(m.daemon.requests_total, 2);
        assert_eq!(m.daemon.control_connections, 0);
        assert_eq!(m.daemon.task_restarts, 3);
    }
}
