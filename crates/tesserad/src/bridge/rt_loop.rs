//! The body of a bridge thread (ADR 0004: "backend bridge", `SCHED_FIFO`,
//! default priority 75).
//!
//! The thread does nothing but [`Bridge::step`] in a loop: the card's
//! `poll`/`read`/`write`/`avail`/`delay` and `clock_gettime` (vDSO). The one
//! other system call, `clock_nanosleep`, is made only after a step that met
//! an xrun or a card error: a pause that keeps a failing card from turning
//! the loop into a spin (`[defaults.bridge] error_sleep*`, per `[[bridge]]`;
//! by default 2 ms doubling to 64 ms while the trouble lasts). It cannot be
//! a timed poll on the card's descriptors: in exactly those states the card
//! keeps `POLLERR` raised, so `poll` returns at once. This exception is
//! recorded in the ADR 0004 amendment of 2026-10-18; by the owner's decision
//! of the same day (ADR 0014) the sleep is a documented configuration
//! setting, `[defaults.bridge] error_sleep*`, enabled by default. No
//! allocation, no lock, no logging: everything the
//! thread reports goes through atomics ([`Health`] and the bridge's own
//! `BridgeStats`), the pause setting is read from atomics
//! ([`ErrorSleepSetting`]) that the manager updates on a reload, and what it
//! owns goes back to the control thread when it ends.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use tessera_audio_ipc::monotonic_ns;
use tessera_backend_alsa::bridge::{Bridge, BridgeError, ErrorSleep};

use crate::rtsys;

/// Longest wait for the card in one step.
pub const STEP_WAIT: Duration = Duration::from_millis(20);

/// The bridge thread's pause after an error, shared lock-free between the
/// manager (which sets it at open and on a reload) and the thread (which
/// reads it at its next error). Three independent atomics: a reader that
/// sees half an update gets a mix of two valid settings, which
/// [`ErrorSleep::wait`] still bounds (never zero, never above the larger
/// maximum).
#[derive(Debug)]
pub struct ErrorSleepSetting {
    doubling: AtomicBool,
    min_ms: AtomicU32,
    max_ms: AtomicU32,
}

impl ErrorSleepSetting {
    /// A setting holding `s`.
    #[must_use]
    pub fn new(s: ErrorSleep) -> Self {
        Self {
            doubling: AtomicBool::new(s.doubling),
            min_ms: AtomicU32::new(s.min_ms),
            max_ms: AtomicU32::new(s.max_ms),
        }
    }

    /// Replace the setting (control thread).
    pub fn set(&self, s: ErrorSleep) {
        self.doubling.store(s.doubling, Relaxed);
        self.min_ms.store(s.min_ms, Relaxed);
        self.max_ms.store(s.max_ms, Relaxed);
    }

    /// The setting now (real-time safe: three atomic loads).
    #[must_use]
    pub fn get(&self) -> ErrorSleep {
        ErrorSleep {
            doubling: self.doubling.load(Relaxed),
            min_ms: self.min_ms.load(Relaxed),
            max_ms: self.max_ms.load(Relaxed),
        }
    }
}

impl Default for ErrorSleepSetting {
    fn default() -> Self {
        Self::new(ErrorSleep::DEFAULT)
    }
}

/// What the bridge thread and its manager share: what the thread reports
/// besides the bridge's own counters, and the pause setting it reads.
#[derive(Debug, Default)]
pub struct Health {
    /// Loop iterations.
    pub steps: AtomicU64,
    /// Steps that met an xrun or a card error and backed off.
    pub backoffs: AtomicU64,
    /// Real-time scheduling or the CPU pin was requested and refused.
    pub rt_degraded: AtomicBool,
    /// The pause after a step with an error (`error_sleep*`).
    pub error_sleep: ErrorSleepSetting,
}

/// Scheduling of a bridge thread.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadSetup {
    /// `SCHED_FIFO` priority; `None`: normal scheduling.
    pub rt_priority: Option<u8>,
    /// CPU to pin to.
    pub cpu: Option<usize>,
    /// Test hook (feature `bridge-test-hooks`, never in a product build):
    /// panic after this many loop iterations, to prove a bridge thread's
    /// panic ends only that bridge.
    #[cfg(feature = "bridge-test-hooks")]
    pub panic_after: Option<u64>,
}

/// Prepares the calling thread: pre-faulted stack, `SCHED_FIFO`, CPU pin.
/// Refusals are recorded in `health` (never silent, never fatal).
pub fn enter(setup: ThreadSetup, health: &Health) {
    rtsys::prefault_stack();
    if let Some(p) = setup.rt_priority {
        if !rtsys::set_fifo(p) {
            health.rt_degraded.store(true, Relaxed);
        }
    }
    if let Some(c) = setup.cpu {
        if !rtsys::pin_cpu(c) {
            health.rt_degraded.store(true, Relaxed);
        }
    }
}

/// One iteration at a time, so the allocation proof can measure exactly the
/// code the thread runs.
#[derive(Debug)]
pub struct LoopRunner<'a> {
    bridge: &'a mut Bridge,
    health: &'a Health,
    streak: u32,
}

impl<'a> LoopRunner<'a> {
    /// A runner over `bridge`.
    pub fn new(bridge: &'a mut Bridge, health: &'a Health) -> Self {
        Self {
            bridge,
            health,
            streak: 0,
        }
    }

    /// One step of the bridge, then a pause if the card is in trouble
    /// (`error_sleep*`, read from [`Health::error_sleep`] at each error; by
    /// default 2 ms doubling to 64 ms while it lasts; ADR 0004: never spin,
    /// never burst). A clean step never pauses.
    ///
    /// # Errors
    /// The step's error: the card was removed.
    pub fn iterate(&mut self) -> Result<(), BridgeError> {
        let p = self.bridge.step(STEP_WAIT)?;
        self.health.steps.fetch_add(1, Relaxed);
        if p.card_errors > 0 || p.xruns > 0 {
            self.streak = self.streak.saturating_add(1).min(ErrorSleep::MAX_STREAK);
            self.health.backoffs.fetch_add(1, Relaxed);
            let pause = self.health.error_sleep.get().wait(self.streak);
            let ns = u64::try_from(pause.as_nanos()).unwrap_or(u64::MAX);
            rtsys::sleep_until(monotonic_ns().saturating_add(ns));
        } else {
            self.streak = 0;
        }
        Ok(())
    }
}

/// How a bridge thread ended.
pub enum LoopEnd {
    /// Stopped on request.
    Stopped,
    /// The card was lost.
    Card(BridgeError),
    /// A bug: the thread panicked (the payload, to be formatted on the
    /// control thread).
    Panicked(Box<dyn std::any::Any + Send>),
}

impl std::fmt::Debug for LoopEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoopEnd::Stopped => f.write_str("Stopped"),
            LoopEnd::Card(e) => write!(f, "Card({e})"),
            LoopEnd::Panicked(_) => f.write_str("Panicked"),
        }
    }
}

/// The whole body of a bridge thread: set-up, the loop, and a panic caught
/// (a bug ends only this bridge: the thread owns nothing the daemon or the
/// other bridges depend on, and after the catch it is gone, not half-alive;
/// ADR 0004 amendment 2026-10-18).
pub fn thread_body(
    bridge: &mut Bridge,
    stop: &AtomicBool,
    health: &Health,
    setup: ThreadSetup,
) -> LoopEnd {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        enter(setup, health);
        #[cfg(feature = "bridge-test-hooks")]
        if let Some(n) = setup.panic_after {
            let mut runner = LoopRunner::new(bridge, health);
            for _ in 0..n {
                runner.iterate()?;
            }
            panic!("test hook: a deliberate panic on the bridge thread");
        }
        run(bridge, stop, health)
    }));
    match r {
        Ok(Ok(())) => LoopEnd::Stopped,
        Ok(Err(e)) => LoopEnd::Card(e),
        Err(p) => LoopEnd::Panicked(p),
    }
}

/// Runs the loop until `stop` is set or the card is lost.
///
/// # Errors
/// The failing step's error.
pub fn run(bridge: &mut Bridge, stop: &AtomicBool, health: &Health) -> Result<(), BridgeError> {
    let mut r = LoopRunner::new(bridge, health);
    while !stop.load(Relaxed) {
        r.iterate()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manager's `set` (at open, on a reload) is what the thread's `get`
    /// returns at its next error: three atomics, no lock.
    #[test]
    fn a_new_pause_setting_is_what_the_thread_reads_next() {
        let h = Health::default();
        assert_eq!(h.error_sleep.get(), ErrorSleep::DEFAULT);
        let s = ErrorSleep {
            doubling: false,
            min_ms: 7,
            max_ms: 9,
        };
        h.error_sleep.set(s);
        assert_eq!(h.error_sleep.get(), s);
        assert_eq!(h.error_sleep.get().wait(5), Duration::from_millis(7));
    }
}
