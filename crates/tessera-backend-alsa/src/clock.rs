//! The media clock as the PCM sees it: the daemon's clock overlay (ADR 0005),
//! read lock-free, never steering anything.

use std::sync::Arc;

use tessera_audio_ipc::{ClockOverlayReader, ClockSnapshot, raw_monotonic_ns};

/// One reading of the media clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    /// Media time in whole samples.
    pub samples: u64,
    /// Discontinuity counter of the overlay: it changes when media time steps.
    pub epoch: u64,
    /// Nanoseconds since the daemon last published the overlay (liveness).
    pub age_ns: u64,
}

/// A source of media time. The real one reads the overlay; tests inject a
/// virtual one.
pub trait Clock {
    /// The media time now, or `None` if no snapshot was ever readable.
    /// Never blocks, allocates or makes a system call (`CLOCK_MONOTONIC_RAW` is
    /// read through the vDSO).
    fn now(&mut self) -> Option<Now>;
}

/// [`Clock`] over the daemon's read-only clock overlay.
#[derive(Debug)]
pub struct OverlayClock {
    overlay: Arc<ClockOverlayReader>,
    rate: u32,
    last: Option<ClockSnapshot>,
}

impl OverlayClock {
    /// A clock for a stream at `rate` Hz.
    pub fn new(overlay: Arc<ClockOverlayReader>, rate: u32) -> Self {
        let last = overlay.read().ok();
        Self {
            overlay,
            rate,
            last,
        }
    }
}

impl Clock for OverlayClock {
    fn now(&mut self) -> Option<Now> {
        // A torn read (the writer is mid-publish) keeps the last good
        // snapshot: the media clock is an affine map and extrapolates.
        if let Some(s) = self.overlay.try_read() {
            self.last = Some(s);
        }
        let s = self.last?;
        let mono = raw_monotonic_ns();
        Some(Now {
            samples: s.media_samples_at(mono, self.rate),
            epoch: s.epoch,
            age_ns: mono.saturating_sub(s.updated_mono_ns),
        })
    }
}
