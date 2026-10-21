//! Adapter: publish the media clock through the `tessera-audio-ipc` clock
//! overlay (ADR 0005, layout 1.0).

use tessera_audio_ipc::{ClockOverlayWriter, ClockSnapshot, ClockState as IpcState};

use crate::media_clock::{ClockPublisher, ClockState, MediaClockSnapshot};

/// Publishes snapshots into a [`ClockOverlayWriter`].
#[derive(Debug)]
pub struct OverlayPublisher {
    writer: ClockOverlayWriter,
}

impl OverlayPublisher {
    /// Wraps an overlay writer.
    #[must_use]
    pub fn new(writer: ClockOverlayWriter) -> Self {
        OverlayPublisher { writer }
    }

    /// The wrapped writer (to hand its descriptor to clients).
    #[must_use]
    pub fn writer(&self) -> &ClockOverlayWriter {
        &self.writer
    }

    /// Gives the writer back.
    #[must_use]
    pub fn into_writer(self) -> ClockOverlayWriter {
        self.writer
    }
}

/// Converts a core snapshot into the overlay's type.
#[must_use]
pub fn to_overlay(s: &MediaClockSnapshot) -> ClockSnapshot {
    ClockSnapshot {
        epoch: s.epoch,
        base_mono_ns: s.base_mono_ns,
        base_media_ns: s.base_media_ns,
        rate_ppb: s.rate_ppb,
        state: match s.state {
            ClockState::FreeRunning => IpcState::FreeRunning,
            ClockState::Listening => IpcState::Listening,
            ClockState::Acquiring => IpcState::Acquiring,
            ClockState::Locked => IpcState::Locked,
            ClockState::Holdover => IpcState::Holdover,
            ClockState::Leader => IpcState::Leader,
        },
        gm_identity: s.gm_identity,
        updated_mono_ns: s.updated_mono_ns,
    }
}

impl ClockPublisher for OverlayPublisher {
    fn publish(&mut self, snap: &MediaClockSnapshot) {
        self.writer.publish(&to_overlay(snap));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_audio_ipc::ClockOverlayReader;

    #[test]
    fn published_snapshots_read_back_and_extrapolate_identically() {
        let w = ClockOverlayWriter::create().unwrap();
        let r = ClockOverlayReader::attach(w.try_clone_fd().unwrap()).unwrap();
        let mut p = OverlayPublisher::new(w);
        let s = MediaClockSnapshot {
            epoch: 7,
            base_mono_ns: 1_000_000,
            base_media_ns: 5_000_000_000,
            rate_ppb: -123_456,
            state: ClockState::Locked,
            gm_identity: 0x0200_0000_0001,
            updated_mono_ns: 2_000_000,
        };
        p.publish(&s);
        let got = r.read().unwrap();
        assert_eq!(got.epoch, 7);
        assert_eq!(got.state, IpcState::Locked);
        assert_eq!(got.gm_identity, 0x0200_0000_0001);
        for t in [0u64, 1_000_000, 3_000_000_000, 987_654_321_012, u64::MAX] {
            assert_eq!(got.media_ns_at(t), s.media_ns_at(t), "t={t}");
        }
    }
}
