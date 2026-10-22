//! The media-clock model (ADR 0005): an affine map from the local monotonic
//! clock to media time, with an `epoch` that increments on discontinuities.
//!
//! `media_ns(t) = base_media_ns + (t - base_mono_ns) * (1 + rate_ppb * 1e-9)`
//!
//! The arithmetic is bit-identical to the overlay reader in
//! `tessera-audio-ipc` (`ClockSnapshot::media_ns_at`: `i128` intermediates,
//! rate term truncated toward zero, saturating to `[0, u64::MAX]`), so a
//! consumer extrapolating from a published snapshot sees exactly what this
//! model computes. Nothing here steers any kernel clock (project rule 8).

use crate::time::{LocalNs, MediaNs};

/// Media-clock state, as shown in status/CLI/metrics and the overlay
/// (ADR 0005; same numbering as the overlay's `state` word).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockState {
    /// No leader and none expected: running at rate 0 against the local clock.
    FreeRunning,
    /// Looking for a leader.
    Listening,
    /// Following a leader but not yet within the lock threshold.
    Acquiring,
    /// Locked to the grandmaster.
    Locked,
    /// Lost the leader; running on the last rate estimate.
    Holdover,
    /// This node is the leader.
    Leader,
}

/// One consistent set of media-clock values: the data published through the
/// clock overlay (`tessera-audio-ipc`, layout 2.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaClockSnapshot {
    /// Incremented when media time is stepped (a discontinuity of the
    /// mapping) and only then; consumers re-anchor when it changes. Rate
    /// changes, grandmaster changes without a step, holdover and becoming
    /// leader keep the mapping continuous and leave it alone (ADR 0005).
    pub epoch: u64,
    /// Local monotonic nanoseconds of the anchor point.
    pub base_mono_ns: LocalNs,
    /// Media nanoseconds at the anchor point.
    pub base_media_ns: MediaNs,
    /// Rate offset against the local monotonic clock, parts per billion.
    pub rate_ppb: i64,
    /// Clock state.
    pub state: ClockState,
    /// Grandmaster identity as a big-endian number (6-byte uuid, left padded).
    pub gm_identity: u64,
    /// Local monotonic time of this publish (liveness).
    pub updated_mono_ns: LocalNs,
}

impl MediaClockSnapshot {
    /// Media time at local monotonic time `mono_ns` (pure arithmetic).
    #[must_use]
    pub fn media_ns_at(&self, mono_ns: LocalNs) -> MediaNs {
        let delta = i128::from(mono_ns) - i128::from(self.base_mono_ns);
        let adj = delta * i128::from(self.rate_ppb) / 1_000_000_000;
        let media = i128::from(self.base_media_ns) + delta + adj;
        media.clamp(0, i128::from(u64::MAX)) as u64
    }
}

/// Receives snapshots of the media clock. Implemented by the overlay writer
/// (`platform::OverlayPublisher`) and by test doubles. Called on the PTP
/// thread; implementations must not allocate, lock or log (ADR 0004).
pub trait ClockPublisher {
    /// Publishes `snap`.
    fn publish(&mut self, snap: &MediaClockSnapshot);
}

/// A publisher that discards everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullPublisher;

impl ClockPublisher for NullPublisher {
    fn publish(&mut self, _snap: &MediaClockSnapshot) {}
}

/// Converts a 6-byte uuid to the overlay's `gm_identity` number.
#[must_use]
pub fn gm_identity_from_uuid(uuid: &[u8; 6]) -> u64 {
    let mut b = [0u8; 8];
    for (dst, src) in b.iter_mut().skip(2).zip(uuid.iter()) {
        *dst = *src;
    }
    u64::from_be_bytes(b)
}

/// The mutable media clock owned by a port.
///
/// Invariant: every mutation re-anchors at the instant given, evaluating the
/// previous map there first, so changing the rate never moves media time.
#[derive(Debug, Clone, Copy)]
pub struct MediaClock {
    snap: MediaClockSnapshot,
}

impl MediaClock {
    /// A free-running clock anchored at `now_mono` with media time
    /// `initial_media_ns` (typically equal to `now_mono`, or a TAI reading
    /// for a leader) and rate 0.
    #[must_use]
    pub fn new(now_mono: LocalNs, initial_media_ns: MediaNs) -> Self {
        MediaClock {
            snap: MediaClockSnapshot {
                epoch: 0,
                base_mono_ns: now_mono,
                base_media_ns: initial_media_ns,
                rate_ppb: 0,
                state: ClockState::FreeRunning,
                gm_identity: 0,
                updated_mono_ns: now_mono,
            },
        }
    }

    /// Media time at `t`.
    #[must_use]
    pub fn media_ns_at(&self, t: LocalNs) -> MediaNs {
        self.snap.media_ns_at(t)
    }

    /// Current rate offset in ppb.
    #[must_use]
    pub fn rate_ppb(&self) -> i64 {
        self.snap.rate_ppb
    }

    /// Local time of the current anchor; re-anchoring never goes before it.
    #[must_use]
    pub fn anchor_mono_ns(&self) -> LocalNs {
        self.snap.base_mono_ns
    }

    /// Current epoch.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.snap.epoch
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> ClockState {
        self.snap.state
    }

    /// Re-anchors at `t` without changing the map.
    pub fn reanchor(&mut self, t: LocalNs) {
        self.snap.base_media_ns = self.media_ns_at(t);
        self.snap.base_mono_ns = t;
    }

    /// Changes the rate at `t`, continuous in media time (no epoch change).
    pub fn set_rate(&mut self, t: LocalNs, rate_ppb: i64) {
        self.reanchor(t);
        self.snap.rate_ppb = rate_ppb;
    }

    /// Adds `delta_ns` to media time at `t` and increments the epoch.
    pub fn step(&mut self, t: LocalNs, delta_ns: i64) {
        let cur = i128::from(self.media_ns_at(t)) + i128::from(delta_ns);
        self.snap.base_media_ns = cur.clamp(0, i128::from(u64::MAX)) as u64;
        self.snap.base_mono_ns = t;
        self.bump_epoch();
    }

    /// Increments the epoch (only a step does this).
    fn bump_epoch(&mut self) {
        self.snap.epoch = self.snap.epoch.wrapping_add(1);
    }

    /// Sets the state (no epoch change by itself).
    pub fn set_state(&mut self, state: ClockState) {
        self.snap.state = state;
    }

    /// Sets the published grandmaster identity.
    pub fn set_gm_identity(&mut self, gm: u64) {
        self.snap.gm_identity = gm;
    }

    /// The snapshot to publish at local time `now`.
    #[must_use]
    pub fn snapshot(&self, now: LocalNs) -> MediaClockSnapshot {
        MediaClockSnapshot {
            updated_mono_ns: now,
            ..self.snap
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn math_matches_documented_overlay_formula() {
        let s = MediaClockSnapshot {
            epoch: 1,
            base_mono_ns: 1_000,
            base_media_ns: 10_000,
            rate_ppb: 1_000_000,
            state: ClockState::Locked,
            gm_identity: 0,
            updated_mono_ns: 0,
        };
        assert_eq!(s.media_ns_at(1_000), 10_000);
        assert_eq!(s.media_ns_at(1_000 + 1_000_000_000), 10_000 + 1_001_000_000);
        assert_eq!(s.media_ns_at(0), 10_000 - 1_000 - 1);
        assert_eq!(s.media_ns_at(u64::MAX), u64::MAX);
    }

    #[test]
    fn rate_change_is_continuous_and_step_bumps_epoch() {
        let mut c = MediaClock::new(1_000, 5_000);
        let t = 3_000_000_000;
        let before = c.media_ns_at(t);
        c.set_rate(t, 123_456);
        assert_eq!(c.media_ns_at(t), before);
        assert_eq!(c.epoch(), 0);
        c.step(t, -1_000);
        assert_eq!(c.media_ns_at(t), before - 1_000);
        assert_eq!(c.epoch(), 1);
        c.bump_epoch();
        assert_eq!(c.epoch(), 2);
    }

    #[test]
    fn gm_identity_is_big_endian_padded() {
        assert_eq!(
            gm_identity_from_uuid(&[1, 2, 3, 4, 5, 6]),
            0x0000_0102_0304_0506
        );
    }

    proptest! {
        #[test]
        fn set_rate_never_moves_time(base in 0u64..1<<50, t in 0u64..1<<50, r in -500_000i64..500_000, r2 in -500_000i64..500_000, later in 0u64..1<<40) {
            let mut c = MediaClock::new(0, base);
            c.set_rate(t / 2, r);
            let a = c.media_ns_at(t);
            c.set_rate(t, r2);
            // Continuity at the instant of the change (re-anchor rounding is nil
            // because the anchor is evaluated, not extrapolated).
            prop_assert_eq!(c.media_ns_at(t), a);
            // Monotonic afterwards for |rate| < 1e9 ppb.
            prop_assert!(c.media_ns_at(t + later) >= a);
        }
    }
}
