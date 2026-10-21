//! Consumer-side view of the clock overlay.
//!
//! [`OverlaySnapshot`] is one consistent set of overlay values, read through
//! the `tessera-audio-ipc` seqlock ([`ClockOverlayReader`]), for code that
//! needs the PTP thread's media clock without running PTP itself -- the media
//! engine, backends, or `tesserad` adapting it to another crate's clock trait
//! (for example `tessera-media`'s `MediaClock`; the name differs from that
//! crate's `ClockSnapshot` on purpose). It reads atomically: a snapshot is
//! never a mix of two publishes.
//!
//! Fields: the mapping `media_ns(t) = base_media_ns + (t - base_mono_ns) *
//! (1 + rate_ppb * 1e-9)`; `epoch` (changes exactly when media time was
//! stepped, ADR 0005, so re-anchor when it changes); `state`; the
//! grandmaster identity; and `updated_mono_ns`, when the writer last
//! published (liveness: a stale value means the PTP thread stopped).

use tessera_audio_ipc::{ClockOverlayReader, ClockSnapshot, ClockState as IpcState, OverlayError};

use crate::media_clock::{ClockState, MediaClockSnapshot};

/// One atomic read of the clock overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlaySnapshot {
    /// Changes exactly when media time was stepped.
    pub epoch: u64,
    /// Local monotonic ns of the anchor.
    pub base_mono_ns: u64,
    /// Media ns at the anchor.
    pub base_media_ns: u64,
    /// Rate offset against the monotonic clock, ppb.
    pub rate_ppb: i64,
    /// Clock state. A value written by a newer writer that this version does
    /// not know reads as [`ClockState::FreeRunning`] (the conservative one).
    pub state: ClockState,
    /// Grandmaster identity (6-byte uuid as a big-endian number).
    pub gm_identity: u64,
    /// Monotonic ns of the writer's last publish.
    pub updated_mono_ns: u64,
}

impl OverlaySnapshot {
    /// Reads a consistent snapshot (bounded seqlock retries).
    ///
    /// # Errors
    /// [`OverlayError::Unstable`] if the writer died mid-publish; keep using
    /// the last good snapshot (the mapping stays valid for extrapolation).
    pub fn read(reader: &ClockOverlayReader) -> Result<Self, OverlayError> {
        reader.read().map(Self::from_ipc)
    }

    /// Converts the overlay crate's snapshot.
    #[must_use]
    pub fn from_ipc(s: ClockSnapshot) -> Self {
        OverlaySnapshot {
            epoch: s.epoch,
            base_mono_ns: s.base_mono_ns,
            base_media_ns: s.base_media_ns,
            rate_ppb: s.rate_ppb,
            state: match s.state {
                IpcState::Listening => ClockState::Listening,
                IpcState::Acquiring => ClockState::Acquiring,
                IpcState::Locked => ClockState::Locked,
                IpcState::Holdover => ClockState::Holdover,
                IpcState::Leader => ClockState::Leader,
                _ => ClockState::FreeRunning,
            },
            gm_identity: s.gm_identity,
            updated_mono_ns: s.updated_mono_ns,
        }
    }

    /// Media ns at monotonic time `mono_ns` (same arithmetic as the writer).
    #[must_use]
    pub fn media_ns_at(&self, mono_ns: u64) -> u64 {
        MediaClockSnapshot::from(*self).media_ns_at(mono_ns)
    }

    /// True if the writer has not published for longer than `max_age_ns`
    /// before `now_mono_ns`.
    #[must_use]
    pub fn is_stale(&self, now_mono_ns: u64, max_age_ns: u64) -> bool {
        now_mono_ns.saturating_sub(self.updated_mono_ns) > max_age_ns
    }
}

impl From<OverlaySnapshot> for MediaClockSnapshot {
    fn from(s: OverlaySnapshot) -> Self {
        MediaClockSnapshot {
            epoch: s.epoch,
            base_mono_ns: s.base_mono_ns,
            base_media_ns: s.base_media_ns,
            rate_ppb: s.rate_ppb,
            state: s.state,
            gm_identity: s.gm_identity,
            updated_mono_ns: s.updated_mono_ns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_clock::ClockPublisher;
    use crate::platform::OverlayPublisher;
    use tessera_audio_ipc::ClockOverlayWriter;

    #[test]
    fn reads_what_the_publisher_wrote_and_extrapolates_identically() {
        let w = ClockOverlayWriter::create().unwrap();
        let r = ClockOverlayReader::attach(w.try_clone_fd().unwrap()).unwrap();
        let mut p = OverlayPublisher::new(w);
        let s = MediaClockSnapshot {
            epoch: 3,
            base_mono_ns: 5_000,
            base_media_ns: 9_000_000,
            rate_ppb: 42_000,
            state: ClockState::Holdover,
            gm_identity: 0x0200_0000_0007,
            updated_mono_ns: 6_000,
        };
        p.publish(&s);
        let o = OverlaySnapshot::read(&r).unwrap();
        assert_eq!(MediaClockSnapshot::from(o), s);
        for t in [0u64, 5_000, 7_777_777_777, u64::MAX] {
            assert_eq!(o.media_ns_at(t), s.media_ns_at(t));
        }
        assert!(!o.is_stale(6_100, 1_000));
        assert!(o.is_stale(60_000, 1_000));
    }

    #[test]
    fn a_writer_that_died_mid_publish_is_reported_not_spun_on() {
        let mut w = ClockOverlayWriter::create().unwrap();
        let r = ClockOverlayReader::attach(w.try_clone_fd().unwrap()).unwrap();
        w.simulate_crash_mid_publish();
        assert!(OverlaySnapshot::read(&r).is_err());
    }
}
