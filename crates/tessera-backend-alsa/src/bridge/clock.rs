//! The bridge's time base: the daemon's media clock, with a free-running local
//! clock as the fallback (ADR 0005/0006).

use tessera_audio_ipc::monotonic_ns;

use crate::clock::{Clock, Now};

/// A free-running media clock: `CLOCK_MONOTONIC` scaled to the sample rate,
/// started at media time 0. Never steered.
#[derive(Debug, Clone)]
pub struct LocalClock {
    rate: u32,
    base_mono: u64,
    base_samples: u64,
}

impl LocalClock {
    /// Starts at media time 0 now.
    pub fn new(rate: u32) -> Self {
        Self {
            rate,
            base_mono: monotonic_ns(),
            base_samples: 0,
        }
    }
}

impl Clock for LocalClock {
    fn now(&mut self) -> Option<Now> {
        let ns = monotonic_ns().saturating_sub(self.base_mono);
        let s = u128::from(ns) * u128::from(self.rate) / 1_000_000_000;
        Some(Now {
            samples: self
                .base_samples
                .saturating_add(u64::try_from(s).unwrap_or(u64::MAX)),
            epoch: 0,
            age_ns: 0,
        })
    }
}

/// Which source answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The daemon's overlay.
    Daemon,
    /// The free-running fallback.
    Local,
}

/// One reading of the bridge clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeNow {
    /// Media time in samples.
    pub samples: u64,
    /// Changes whenever media time may have jumped: a clock step, or a
    /// switch between the daemon's clock and the fallback.
    pub generation: u64,
    /// The source in use.
    pub source: Source,
}

/// The media clock with fallback. While the daemon's overlay is present and
/// fresher than `stale_ns` it is used; otherwise the local clock keeps media
/// time running (and continues from where the daemon's clock stopped, so a
/// brief outage does not move media time).
pub struct BridgeClock {
    primary: Option<Box<dyn Clock + Send>>,
    local: Box<dyn Clock + Send>,
    stale_ns: u64,
    generation: u64,
    last: Option<(Source, u64)>,
    last_daemon: Option<u64>,
    /// Local media time = `local reading + offset` (continuity at a switch).
    local_offset: i128,
}

impl std::fmt::Debug for BridgeClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeClock")
            .field("has_primary", &self.primary.is_some())
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl BridgeClock {
    /// A clock with only the local source. `stale_ns` 0 never treats the
    /// overlay as stale.
    pub fn new(local: Box<dyn Clock + Send>, stale_ns: u64) -> Self {
        Self {
            primary: None,
            local,
            stale_ns,
            generation: 0,
            last: None,
            last_daemon: None,
            local_offset: 0,
        }
    }

    /// Installs (or removes) the daemon's clock.
    pub fn set_primary(
        &mut self,
        c: Option<Box<dyn Clock + Send>>,
    ) -> Option<Box<dyn Clock + Send>> {
        std::mem::replace(&mut self.primary, c)
    }

    /// Reads media time.
    pub fn now(&mut self) -> BridgeNow {
        let stale_ns = self.stale_ns;
        let daemon = self
            .primary
            .as_mut()
            .and_then(|c| c.now())
            .filter(|n| stale_ns == 0 || n.age_ns <= stale_ns);
        let (source, samples, epoch) = if let Some(n) = daemon {
            self.last_daemon = Some(n.samples);
            (Source::Daemon, n.samples, n.epoch)
        } else {
            let l = self.local.now();
            let reading = l.map_or(0, |l| l.samples);
            if let (Some((Source::Daemon, _)), Some(d)) = (self.last, self.last_daemon) {
                // Falling back: continue from the daemon clock's last time.
                self.local_offset = i128::from(d) - i128::from(reading);
            }
            let s = (i128::from(reading) + self.local_offset).clamp(0, i128::from(u64::MAX)) as u64;
            (Source::Local, s, l.map_or(0, |l| l.epoch))
        };
        if self.last != Some((source, epoch)) {
            if self.last.is_some() {
                self.generation = self.generation.wrapping_add(1);
            }
            self.last = Some((source, epoch));
        }
        BridgeNow {
            samples,
            generation: self.generation,
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Clone)]
    struct Fake(Arc<(AtomicU64, AtomicU64, AtomicU64)>);
    impl Clock for Fake {
        fn now(&mut self) -> Option<Now> {
            Some(Now {
                samples: self.0.0.load(Ordering::SeqCst),
                epoch: self.0.1.load(Ordering::SeqCst),
                age_ns: self.0.2.load(Ordering::SeqCst),
            })
        }
    }

    #[test]
    fn uses_the_daemon_clock_and_falls_back_when_it_goes_stale() {
        let local = Fake(Arc::new((1000.into(), 0.into(), 0.into())));
        let daemon = Fake(Arc::new((5_000_000.into(), 1.into(), 0.into())));
        let mut c = BridgeClock::new(Box::new(local.clone()), 1_000_000_000);
        let a = c.now();
        assert_eq!((a.source, a.samples), (Source::Local, 1000));
        c.set_primary(Some(Box::new(daemon.clone())));
        let b = c.now();
        assert_eq!((b.source, b.samples), (Source::Daemon, 5_000_000));
        assert_ne!(b.generation, a.generation, "a switch is a discontinuity");
        // Same source, same epoch: stable generation.
        assert_eq!(c.now().generation, b.generation);
        // A clock step (epoch change) is a discontinuity.
        daemon.0.1.store(2, Ordering::SeqCst);
        let d = c.now();
        assert_ne!(d.generation, b.generation);
        // A silent overlay: fall back to the local clock.
        daemon.0.2.store(2_000_000_000, Ordering::SeqCst);
        local.0.0.store(2000, Ordering::SeqCst);
        let e = c.now();
        // Media time continues from the daemon clock's last reading.
        assert_eq!((e.source, e.samples), (Source::Local, 5_000_000));
        local.0.0.store(3000, Ordering::SeqCst);
        assert_eq!(c.now().samples, 5_001_000);
        assert_ne!(e.generation, d.generation);
        // And back.
        daemon.0.2.store(0, Ordering::SeqCst);
        assert_eq!(c.now().source, Source::Daemon);
    }

    #[test]
    fn local_clock_runs_at_the_sample_rate() {
        let mut c = LocalClock::new(48_000);
        let a = c.now().unwrap().samples;
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = c.now().unwrap().samples;
        assert!((900..2500).contains(&(b - a)), "{}", b - a);
    }
}
