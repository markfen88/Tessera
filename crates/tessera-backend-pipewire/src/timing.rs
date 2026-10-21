//! Media-clock arithmetic and the graph-cycle scheduler of the driver nodes.
//!
//! The scheduler is a pure state machine over media-sample time, so it is
//! tested with a virtual clock. It decides *when* the PipeWire graph cycle is
//! triggered and *which media positions* the cycle covers; the real-time
//! callbacks only copy.

use std::sync::Arc;

use tessera_audio_ipc::{ClockOverlayReader, ClockSnapshot, ClockState, monotonic_ns};

/// Converts a media time to the `CLOCK_MONOTONIC` time at which the overlay's
/// affine map reaches it (the inverse of `ClockSnapshot::media_ns_at`),
/// saturating to `[0, u64::MAX]`.
pub fn mono_for_media_ns(snap: &ClockSnapshot, media_ns: u64) -> u64 {
    let d_media = i128::from(media_ns) - i128::from(snap.base_media_ns);
    let denom = 1_000_000_000i128 + i128::from(snap.rate_ppb);
    let denom = if denom <= 0 { 1_000_000_000 } else { denom };
    let d_mono = d_media * 1_000_000_000 / denom;
    (i128::from(snap.base_mono_ns) + d_mono).clamp(0, i128::from(u64::MAX)) as u64
}

/// The smallest `CLOCK_MONOTONIC` time at which media sample `sample` (at
/// `rate_hz`) has been reached.
pub fn mono_for_media_sample(snap: &ClockSnapshot, sample: u64, rate_hz: u32) -> u64 {
    let rate = u128::from(rate_hz.max(1));
    let ns = (u128::from(sample) * 1_000_000_000).div_ceil(rate);
    mono_for_media_ns(snap, u64::try_from(ns).unwrap_or(u64::MAX))
}

/// A source of media time: the daemon's overlay, or a local free-running
/// clock used while detached so the graph keeps cycling (the nodes stay
/// alive and produce silence).
#[derive(Debug)]
pub struct MediaClock {
    rate_hz: u32,
    source: Source,
    last: ClockSnapshot,
}

#[derive(Debug)]
enum Source {
    Local,
    Overlay(Arc<ClockOverlayReader>),
}

fn local_snapshot(anchor_mono: u64) -> ClockSnapshot {
    ClockSnapshot {
        epoch: 0,
        base_mono_ns: anchor_mono,
        base_media_ns: 0,
        rate_ppb: 0,
        state: ClockState::FreeRunning,
        gm_identity: 0,
        updated_mono_ns: anchor_mono,
    }
}

/// One reading of the media clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaNow {
    /// Media time in whole samples.
    pub samples: u64,
    /// Discontinuity counter of the source.
    pub epoch: u64,
    /// The `CLOCK_MONOTONIC` time of the reading.
    pub mono_ns: u64,
}

impl MediaClock {
    /// A free-running local clock starting now.
    pub fn local(rate_hz: u32) -> Self {
        Self {
            rate_hz,
            source: Source::Local,
            last: local_snapshot(monotonic_ns()),
        }
    }

    /// Switches to the daemon's overlay.
    pub fn use_overlay(&mut self, overlay: Arc<ClockOverlayReader>) {
        if let Ok(s) = overlay.read() {
            self.last = s;
        }
        self.source = Source::Overlay(overlay);
    }

    /// Switches back to a local clock that continues from the current media
    /// time, so a detached node does not jump.
    pub fn use_local(&mut self) {
        let now = monotonic_ns();
        let media_ns = self.last.media_ns_at(now);
        let mut snap = local_snapshot(now);
        snap.base_media_ns = media_ns;
        self.last = snap;
        self.source = Source::Local;
    }

    /// True while following the daemon's overlay.
    pub fn is_overlay(&self) -> bool {
        matches!(self.source, Source::Overlay(_))
    }

    /// The sample rate.
    pub fn rate_hz(&self) -> u32 {
        self.rate_hz
    }

    /// Reads the clock. A torn or stuck overlay keeps the last good snapshot
    /// (the affine map stays usable for extrapolation).
    pub fn now(&mut self) -> MediaNow {
        if let Source::Overlay(o) = &self.source {
            if let Some(s) = o.try_read() {
                self.last = s;
            }
        }
        let mono = monotonic_ns();
        MediaNow {
            samples: self.last.media_samples_at(mono, self.rate_hz),
            epoch: self.last.epoch,
            mono_ns: mono,
        }
    }

    /// The monotonic time at which media sample `sample` is reached.
    pub fn mono_for_sample(&self, sample: u64) -> u64 {
        mono_for_media_sample(&self.last, sample, self.rate_hz)
    }
}

/// How a cycle's readiness depends on the endpoint direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lead {
    /// RX source: a cycle covering `[start, start + period)` is delivered once
    /// everything up to `start + period + latency + offset` has played out.
    Rx {
        /// Effective latency published by the daemon, frames.
        latency: u64,
        /// Extra read offset, frames.
        offset: u64,
    },
    /// TX sink: a cycle starts at its media position.
    Tx,
}

impl Lead {
    fn ready_at(self, start: u64, period: u64) -> u64 {
        match self {
            Lead::Rx { latency, offset } => start
                .saturating_add(period)
                .saturating_add(latency)
                .saturating_add(offset),
            Lead::Tx => start,
        }
    }

    fn anchor(self, now: u64, period: u64) -> u64 {
        match self {
            Lead::Rx { latency, offset } => {
                now.saturating_sub(period.saturating_add(latency).saturating_add(offset))
            }
            Lead::Tx => now,
        }
    }
}

/// A graph cycle to trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fire {
    /// Media position of the first frame the cycle covers.
    pub start: u64,
    /// Frames in the cycle.
    pub period: u32,
    /// The position is not contiguous with the previous cycle (first cycle,
    /// clock epoch change, or the scheduler fell behind and skipped).
    pub discontinuity: bool,
    /// Clock epoch the position belongs to; the real-time side drops a cycle
    /// whose epoch is not the current one (a stale position after a clock
    /// step must never reach the ring).
    pub epoch: u64,
    /// Session generation (set by the driver).
    pub generation: u32,
    /// `CLOCK_MONOTONIC` time at which the cycle was fired (set by the
    /// driver).
    pub mono_ns: u64,
}

/// What to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Trigger a cycle now.
    Fire(Fire),
    /// Nothing is due; the next cycle becomes due at this media sample.
    Wait {
        /// Media sample at which to look again.
        until: u64,
    },
}

/// Decides when to trigger graph cycles. See the module documentation.
#[derive(Debug, Clone)]
pub struct Scheduler {
    next_start: Option<u64>,
    last_epoch: Option<u64>,
    max_lag_periods: u64,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    /// A scheduler that tolerates being up to 4 periods late before it skips
    /// forward (never bursting to catch up, ADR 0004).
    pub fn new() -> Self {
        Self {
            next_start: None,
            last_epoch: None,
            max_lag_periods: 4,
        }
    }

    /// Forgets the position; the next `poll` re-anchors.
    pub fn reset(&mut self) {
        self.next_start = None;
        self.last_epoch = None;
    }

    /// Continues from `end`, the position the real-time side actually served
    /// up to (it may differ from the period the cycle was scheduled with when
    /// the graph period changed). No effect before the first cycle.
    pub fn resync(&mut self, end: u64) {
        if self.next_start.is_some() {
            self.next_start = Some(end);
        }
    }

    /// The media sample at which the next cycle becomes due (after a
    /// `Fire`), if a position is held.
    pub fn next_ready(&self, period: u32, lead: Lead) -> Option<u64> {
        self.next_start
            .map(|s| lead.ready_at(s, u64::from(period.max(1))))
    }

    /// Decides at media time `now` (samples) with clock epoch `epoch`.
    pub fn poll(&mut self, now: u64, epoch: u64, period: u32, lead: Lead) -> Decision {
        let period = u64::from(period.max(1));
        let mut discontinuity = false;
        if self.last_epoch != Some(epoch) {
            self.last_epoch = Some(epoch);
            self.next_start = None;
        }
        let start = match self.next_start {
            None => {
                discontinuity = true;
                lead.anchor(now, period)
            }
            Some(s) => {
                let ready = lead.ready_at(s, period);
                let too_late = now > ready.saturating_add(self.max_lag_periods * period);
                let too_early = ready > now.saturating_add(self.max_lag_periods * period);
                if too_late || too_early {
                    discontinuity = true;
                    lead.anchor(now, period)
                } else {
                    s
                }
            }
        };
        let ready = lead.ready_at(start, period);
        if now >= ready {
            self.next_start = Some(start.saturating_add(period));
            Decision::Fire(Fire {
                start,
                period: period as u32,
                discontinuity,
                epoch,
                generation: 0,
                mono_ns: 0,
            })
        } else {
            self.next_start = Some(start);
            Decision::Wait { until: ready }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(base_mono: u64, base_media: u64, ppb: i64) -> ClockSnapshot {
        ClockSnapshot {
            epoch: 1,
            base_mono_ns: base_mono,
            base_media_ns: base_media,
            rate_ppb: ppb,
            state: ClockState::Locked,
            gm_identity: 0,
            updated_mono_ns: 0,
        }
    }

    #[test]
    fn mono_inverse_of_media_map() {
        for ppb in [0i64, 100_000, -250_000, 5_000_000] {
            let s = snap(1_000_000, 7_000_000_000, ppb);
            for t in [1_000_000u64, 2_000_000_000, 9_999_999_999] {
                let media = s.media_ns_at(t);
                let back = mono_for_media_ns(&s, media);
                assert!(back.abs_diff(t) <= 2, "ppb {ppb} t {t} back {back}");
            }
        }
    }

    #[test]
    fn sample_to_mono_is_not_early() {
        let s = snap(0, 0, 0);
        // sample 48 at 48 kHz is exactly 1 ms.
        assert_eq!(mono_for_media_sample(&s, 48, 48_000), 1_000_000);
        // sample 1 is 20833.33 ns: rounded up.
        assert_eq!(mono_for_media_sample(&s, 1, 48_000), 20_834);
        let reached = s.media_samples_at(mono_for_media_sample(&s, 12345, 48_000), 48_000);
        assert!(reached >= 12345);
    }

    #[test]
    fn rx_cycles_are_contiguous_and_delayed_by_latency() {
        let mut s = Scheduler::new();
        let lead = Lead::Rx {
            latency: 480,
            offset: 0,
        };
        // First poll anchors so that the cycle is due immediately.
        let Decision::Fire(f) = s.poll(10_000, 1, 256, lead) else {
            panic!()
        };
        assert!(f.discontinuity);
        assert_eq!(f.start, 10_000 - 256 - 480);
        // Not due again until a period later.
        assert_eq!(
            s.poll(10_100, 1, 256, lead),
            Decision::Wait { until: 10_256 }
        );
        let Decision::Fire(g) = s.poll(10_256, 1, 256, lead) else {
            panic!()
        };
        assert_eq!(g.start, f.start + 256);
        assert!(!g.discontinuity);
        // Slightly late is fine and stays contiguous.
        let Decision::Fire(h) = s.poll(10_512 + 200, 1, 256, lead) else {
            panic!()
        };
        assert_eq!(h.start, g.start + 256);
        assert!(!h.discontinuity);
    }

    #[test]
    fn tx_cycles_start_at_their_position() {
        let mut s = Scheduler::new();
        let Decision::Fire(f) = s.poll(5000, 1, 128, Lead::Tx) else {
            panic!()
        };
        assert_eq!(f.start, 5000);
        assert_eq!(
            s.poll(5100, 1, 128, Lead::Tx),
            Decision::Wait { until: 5128 }
        );
    }

    #[test]
    fn skips_forward_instead_of_bursting() {
        let mut s = Scheduler::new();
        let lead = Lead::Tx;
        let Decision::Fire(_) = s.poll(0, 1, 100, lead) else {
            panic!()
        };
        // The main loop stalled for 50 periods.
        let Decision::Fire(f) = s.poll(5000, 1, 100, lead) else {
            panic!()
        };
        assert!(f.discontinuity);
        assert_eq!(f.start, 5000);
        // Exactly one cycle fired; the next one is a period away.
        assert!(matches!(s.poll(5001, 1, 100, lead), Decision::Wait { .. }));
    }

    #[test]
    fn epoch_change_re_anchors_and_backwards_clock_too() {
        let mut s = Scheduler::new();
        let Decision::Fire(_) = s.poll(1000, 1, 100, Lead::Tx) else {
            panic!()
        };
        let Decision::Fire(f) = s.poll(9_000_000, 2, 100, Lead::Tx) else {
            panic!()
        };
        assert!(f.discontinuity && f.start == 9_000_000);
        // The clock steps back without an epoch change.
        let Decision::Fire(g) = s.poll(100, 2, 100, Lead::Tx) else {
            panic!()
        };
        assert!(g.discontinuity && g.start == 100);
    }

    #[test]
    fn resync_continues_from_what_was_served() {
        let mut s = Scheduler::new();
        let Decision::Fire(f) = s.poll(1000, 1, 100, Lead::Tx) else {
            panic!()
        };
        // The graph switched to 400 frames: the cycle served [1000, 1400).
        s.resync(f.start + 400);
        let Decision::Fire(g) = s.poll(1400, 1, 400, Lead::Tx) else {
            panic!()
        };
        assert_eq!(g.start, 1400);
        assert!(!g.discontinuity);
        let mut fresh = Scheduler::new();
        fresh.resync(5);
        assert!(matches!(fresh.poll(10, 1, 4, Lead::Tx), Decision::Fire(f) if f.discontinuity));
    }

    #[test]
    fn period_change_is_followed() {
        let mut s = Scheduler::new();
        let Decision::Fire(f) = s.poll(1000, 1, 100, Lead::Tx) else {
            panic!()
        };
        assert_eq!(f.period, 100);
        let Decision::Fire(g) = s.poll(1100, 1, 400, Lead::Tx) else {
            panic!()
        };
        assert_eq!((g.start, g.period), (1100, 400));
        assert_eq!(
            s.poll(1200, 1, 400, Lead::Tx),
            Decision::Wait { until: 1500 }
        );
    }

    #[test]
    fn local_clock_runs_and_continues_across_switch() {
        let mut c = MediaClock::local(48_000);
        let a = c.now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = c.now();
        assert!(b.samples > a.samples + 500);
        c.use_local();
        let d = c.now();
        assert!(d.samples >= b.samples);
        assert!(!c.is_overlay());
    }
}
