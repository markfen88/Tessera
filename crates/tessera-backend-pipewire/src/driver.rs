//! The control-thread half of a driver node: media clock, scheduler and the
//! hand-over of each cycle's position to the real-time callback.

use std::sync::Arc;

use tessera_audio_ipc::Ring;

use crate::device::Dir;
use crate::rt::RtShared;
use crate::session::Link;
use crate::timing::{Decision, Fire, Lead, MediaClock, Scheduler};

/// What the caller should do after [`Driver::tick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Trigger a graph cycle now (the position is already published).
    Fire(Fire),
    /// Look again at this `CLOCK_MONOTONIC_RAW` time (ns).
    Wait {
        /// Monotonic time of the next check.
        mono_ns: u64,
    },
}

/// Per-node driver state. Not real-time: it runs on the control thread.
#[derive(Debug)]
pub struct Driver {
    dir: Dir,
    clock: MediaClock,
    sched: Scheduler,
    shared: Arc<RtShared>,
    rx_offset: u64,
    ring: Option<Arc<Ring>>,
    generation: u32,
    last_fire: Option<Fire>,
}

impl Driver {
    /// A detached driver: follows a local clock so the node keeps cycling.
    pub fn new(dir: Dir, rate_hz: u32, rx_offset: u64, shared: Arc<RtShared>) -> Self {
        Self {
            dir,
            clock: MediaClock::local(rate_hz),
            sched: Scheduler::new(),
            shared,
            rx_offset,
            ring: None,
            generation: 0,
            last_fire: None,
        }
    }

    /// Follows the daemon's overlay for session generation `generation`.
    pub fn attach(&mut self, link: &Link, generation: u32) {
        self.clock.use_overlay(Arc::clone(&link.overlay));
        self.ring = Some(Arc::clone(&link.ring));
        self.generation = generation;
        self.sched.reset();
        self.last_fire = None;
    }

    /// Falls back to the local clock (the daemon is gone).
    pub fn detach(&mut self) {
        self.clock.use_local();
        self.ring = None;
        self.generation = 0;
        self.sched.reset();
        self.last_fire = None;
    }

    /// Forgets the position (for example when the node stops being the graph
    /// driver) so the next tick re-anchors.
    pub fn reset(&mut self) {
        self.sched.reset();
        self.last_fire = None;
    }

    /// True while following the daemon.
    pub fn attached(&self) -> bool {
        self.ring.is_some()
    }

    /// Decides whether a cycle is due for a graph period of `period` frames.
    pub fn tick(&mut self, period: u32) -> Tick {
        // Continue from what the real-time side actually served, so a change
        // of the graph period never repeats or skips media positions.
        if let Some(f) = self.last_fire {
            let (done_start, done_end) = self.shared.last_done();
            if done_start == f.start {
                self.last_fire = None;
                let served = done_end.wrapping_sub(f.start);
                if served > 0 && served <= (1 << 20) && served != u64::from(f.period) {
                    self.sched.resync(done_end);
                }
            }
        }
        let now = self.clock.now();
        let lead = self.lead();
        match self.sched.poll(now.samples, now.epoch, period, lead) {
            Decision::Fire(f) => {
                let f = Fire {
                    generation: self.generation,
                    mono_ns: now.mono_ns,
                    ..f
                };
                self.last_fire = Some(f);
                Tick::Fire(f)
            }
            Decision::Wait { until } => Tick::Wait {
                mono_ns: self.clock.mono_for_sample(until),
            },
        }
    }

    /// The monotonic time at which the cycle after a `Fire` becomes due
    /// (the caller arms its timer with it; cycles are never fired back to
    /// back).
    pub fn next_wake(&mut self, period: u32) -> u64 {
        let lead = self.lead();
        match self.sched.next_ready(period, lead) {
            Some(s) => self.clock.mono_for_sample(s),
            None => self.clock.now().mono_ns,
        }
    }

    fn lead(&self) -> Lead {
        match self.dir {
            Dir::Rx => Lead::Rx {
                latency: self.ring.as_ref().map_or(0, |r| r.effective_latency()),
                offset: self.rx_offset,
            },
            Dir::Tx => Lead::Tx,
        }
    }

    /// The current media time in samples (for diagnostics).
    pub fn media_samples(&mut self) -> u64 {
        self.clock.now().samples
    }
}
