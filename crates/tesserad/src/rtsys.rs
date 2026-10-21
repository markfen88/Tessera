//! Real-time thread setup and the absolute-deadline sleep (ADR 0004).
//!
//! The only `unsafe` here is `sched_setscheduler`, which `nix` does not wrap.
//! Everything is best effort and *reported*: the caller records a refusal in
//! the engine metrics (`rt_degraded`, `mlock_failed`) and logs it once, so a
//! daemon that could not get real-time scheduling says so (ADR 0004: never
//! silently degraded).
#![allow(unsafe_code)]

use nix::sys::mman::{MlockAllFlags, mlockall};
use nix::sys::time::TimeSpec;
use nix::time::{ClockId, ClockNanosleepFlags, clock_nanosleep};

/// Sleep until the absolute `CLOCK_MONOTONIC` time `mono_ns` (returns at once
/// if it is already past). Interrupted sleeps resume towards the same
/// deadline.
pub fn sleep_until(mono_ns: u64) {
    let ts = TimeSpec::new(
        i64::try_from(mono_ns / 1_000_000_000).unwrap_or(i64::MAX) as _,
        (mono_ns % 1_000_000_000) as _,
    );
    while let Err(nix::errno::Errno::EINTR) = clock_nanosleep(
        ClockId::CLOCK_MONOTONIC,
        ClockNanosleepFlags::TIMER_ABSTIME,
        &ts,
    ) {}
}

/// `SCHED_FIFO` at `prio` for the calling thread. `false` if refused.
#[must_use]
pub fn set_fifo(prio: u8) -> bool {
    let param = nix::libc::sched_param {
        sched_priority: i32::from(prio),
    };
    // SAFETY: `param` is a valid `sched_param`; pid 0 means the calling
    // thread. Failure (no privilege, `RLIMIT_RTPRIO`) is returned.
    unsafe { nix::libc::sched_setscheduler(0, nix::libc::SCHED_FIFO, &raw const param) == 0 }
}

/// Pin the calling thread to `cpu`. `false` if refused.
#[must_use]
pub fn pin_cpu(cpu: usize) -> bool {
    let mut set = nix::sched::CpuSet::new();
    set.set(cpu).is_ok()
        && nix::sched::sched_setaffinity(nix::unistd::Pid::from_raw(0), &set).is_ok()
}

/// `mlockall(MCL_CURRENT | MCL_FUTURE)`. `false` if refused.
#[must_use]
pub fn lock_memory() -> bool {
    mlockall(MlockAllFlags::MCL_CURRENT | MlockAllFlags::MCL_FUTURE).is_ok()
}

/// Touch 256 KiB of stack so the pages are resident (and locked, after
/// `mlockall`) before the loop starts.
pub fn prefault_stack() {
    #[inline(never)]
    fn touch() {
        let mut a = [0u8; 256 * 1024];
        std::hint::black_box(&mut a);
    }
    touch();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_audio_ipc::monotonic_ns;

    #[test]
    fn sleeping_to_an_absolute_deadline_neither_returns_early_nor_hangs() {
        let target = monotonic_ns() + 5_000_000;
        sleep_until(target);
        let now = monotonic_ns();
        assert!(now >= target, "woke {} ns early", target - now);
        assert!(
            now < target + 200_000_000,
            "woke {} ms late",
            (now - target) / 1_000_000
        );
        let t = monotonic_ns();
        sleep_until(t.saturating_sub(1_000_000));
        assert!(
            monotonic_ns() - t < 50_000_000,
            "a past deadline returns at once"
        );
    }
}
