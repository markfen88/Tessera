//! The media clock as the engine sees it: an adapter from the clock overlay
//! (ADR 0005) to `tessera_media::MediaClock`.
//!
//! Every call of [`MediaClock::now`] / [`MediaClock::at_local`] is **one**
//! atomic read of the overlay (`OverlaySnapshot::read`: epoch, mapping, state
//! and liveness come from a single stable pass of the seqlock), so a media
//! time is never paired with the epoch of another publish. If a read fails
//! (the writer died mid-publish) the last good snapshot keeps being used: the
//! mapping is affine and stays valid for extrapolation. Before the first good
//! snapshot the clock is the monotonic clock (media time = local time).
//!
//! No allocation, locks or syscalls other than `clock_gettime` (vDSO).

use std::cell::Cell;

use tessera_audio_ipc::{ClockOverlayReader, monotonic_ns};
use tessera_clock::media_clock::ClockState;
use tessera_clock::platform::OverlaySnapshot;
use tessera_media::{ClockSnapshot, MediaClock};

/// A [`MediaClock`] over a clock overlay.
pub struct OverlayClock {
    reader: ClockOverlayReader,
    last: Cell<Option<OverlaySnapshot>>,
}

impl std::fmt::Debug for OverlayClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayClock").finish_non_exhaustive()
    }
}

impl OverlayClock {
    /// Wraps a reader.
    #[must_use]
    pub fn new(reader: ClockOverlayReader) -> Self {
        Self {
            reader,
            last: Cell::new(None),
        }
    }

    /// One atomic read of the overlay (or the last good one).
    #[must_use]
    pub fn snapshot(&self) -> Option<OverlaySnapshot> {
        if let Ok(s) = OverlaySnapshot::read(&self.reader) {
            self.last.set(Some(s));
            Some(s)
        } else {
            self.last.get()
        }
    }

    /// The local monotonic time at which the media clock reads `media_ns`
    /// under the current mapping (the inverse of the overlay's map).
    #[must_use]
    pub fn local_for_media(&self, snap: &OverlaySnapshot, media_ns: u64) -> u64 {
        let delta_media = i128::from(media_ns) - i128::from(snap.base_media_ns);
        // media = base + dt * (1 + r/1e9)  =>  dt = d * 1e9 / (1e9 + r)
        let denom = 1_000_000_000i128 + i128::from(snap.rate_ppb);
        let dt = if denom > 0 {
            delta_media * 1_000_000_000 / denom
        } else {
            delta_media
        };
        u64::try_from((i128::from(snap.base_mono_ns) + dt).max(0)).unwrap_or(u64::MAX)
    }

    /// Whether transmitting is allowed in this clock state: locked, leading
    /// or in holdover (ADR 0005 suppresses TX until `locked` or `leader`;
    /// holdover keeps going on the last rate estimate so that a short PTP
    /// outage does not cut the audio).
    #[must_use]
    pub fn state_allows_tx(s: &OverlaySnapshot) -> bool {
        matches!(
            s.state,
            ClockState::Locked | ClockState::Leader | ClockState::Holdover
        )
    }
}

/// The media clock as of one snapshot: the mapping and the epoch are those of
/// the snapshot, only the local time is read afresh (`clock_gettime`, vDSO).
/// The engine takes one overlay snapshot per tick and hands this to every flow,
/// so a tick costs one seqlock read instead of one per call, and all flows see
/// the same mapping and the same epoch within a tick.
#[derive(Debug, Clone, Copy)]
pub struct FrozenClock(Option<OverlaySnapshot>);

impl FrozenClock {
    /// Freeze `snap` (`None`: before any snapshot the clock is the monotonic
    /// clock).
    #[must_use]
    pub fn new(snap: Option<OverlaySnapshot>) -> Self {
        Self(snap)
    }
}

impl MediaClock for FrozenClock {
    fn now(&self) -> ClockSnapshot {
        self.at_local(monotonic_ns())
    }

    fn at_local(&self, local_ns: u64) -> ClockSnapshot {
        match self.0 {
            Some(s) => ClockSnapshot {
                epoch: s.epoch,
                media_ns: s.media_ns_at(local_ns),
            },
            None => ClockSnapshot {
                epoch: 0,
                media_ns: local_ns,
            },
        }
    }
}

impl MediaClock for OverlayClock {
    fn now(&self) -> ClockSnapshot {
        self.at_local(monotonic_ns())
    }

    fn at_local(&self, local_ns: u64) -> ClockSnapshot {
        match self.snapshot() {
            Some(s) => ClockSnapshot {
                epoch: s.epoch,
                media_ns: s.media_ns_at(local_ns),
            },
            None => ClockSnapshot {
                epoch: 0,
                media_ns: local_ns,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_audio_ipc::{ClockOverlayWriter, ClockSnapshot as Ipc, ClockState as IpcState};

    fn clock(s: Ipc) -> (OverlayClock, ClockOverlayWriter) {
        let mut w = ClockOverlayWriter::create().unwrap();
        w.publish(&s);
        let r = ClockOverlayReader::attach(w.try_clone_fd().unwrap()).unwrap();
        (OverlayClock::new(r), w)
    }

    fn snap(rate: i64) -> Ipc {
        Ipc {
            epoch: 4,
            base_mono_ns: 1_000_000,
            base_media_ns: 9_000_000_000,
            rate_ppb: rate,
            state: IpcState::Locked,
            gm_identity: 1,
            updated_mono_ns: 1_000_000,
        }
    }

    #[test]
    fn media_time_and_epoch_come_from_one_snapshot() {
        let (c, _w) = clock(snap(1_000));
        let s = c.at_local(3_000_000_000);
        assert_eq!(s.epoch, 4);
        // 2.999 s of local time at +1 ppm.
        assert_eq!(s.media_ns, 9_000_000_000 + 2_999_000_000 + 2_999);
    }

    #[test]
    fn the_inverse_map_round_trips() {
        for rate in [0i64, 1_000, -250_000, 400_000] {
            let (c, _w) = clock(snap(rate));
            let s = c.snapshot().unwrap();
            for media in [9_000_000_000u64, 9_000_123_456, 12_345_678_901] {
                let local = c.local_for_media(&s, media);
                let back = s.media_ns_at(local);
                assert!(
                    back.abs_diff(media) <= 2,
                    "rate {rate}: {media} -> {local} -> {back}"
                );
            }
        }
    }

    #[test]
    fn a_dead_writer_keeps_the_last_good_mapping() {
        let (c, mut w) = clock(snap(0));
        assert!(c.snapshot().is_some());
        w.simulate_crash_mid_publish();
        let s = c.snapshot().expect("last good snapshot");
        assert_eq!(s.epoch, 4);
    }

    #[test]
    fn tx_is_allowed_only_in_usable_states() {
        let mk = |st| {
            OverlaySnapshot::from_ipc(Ipc {
                state: st,
                ..snap(0)
            })
        };
        assert!(OverlayClock::state_allows_tx(&mk(IpcState::Locked)));
        assert!(OverlayClock::state_allows_tx(&mk(IpcState::Leader)));
        assert!(OverlayClock::state_allows_tx(&mk(IpcState::Holdover)));
        assert!(!OverlayClock::state_allows_tx(&mk(IpcState::FreeRunning)));
        assert!(!OverlayClock::state_allows_tx(&mk(IpcState::Acquiring)));
        assert!(!OverlayClock::state_allows_tx(&mk(IpcState::Listening)));
    }
}
