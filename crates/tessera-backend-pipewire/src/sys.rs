//! The few process-level system calls the client needs: real-time scheduling
//! of the control loop, resetting it for helper threads, and locking memory
//! (ADR 0004). This is the only module of the crate that contains `unsafe`.
#![allow(unsafe_code)]

use std::io;

/// Gives the calling thread `SCHED_FIFO` at `priority`.
///
/// # Errors
/// The OS error (typically `EPERM` without `LimitRTPRIO`).
pub fn set_realtime(priority: u8) -> io::Result<()> {
    let param = libc::sched_param {
        sched_priority: i32::from(priority),
    };
    // SAFETY: `sched_setscheduler` only reads the fully initialised
    // `sched_param` on this stack frame; pid 0 means the calling thread. It
    // has no other memory effects.
    let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Puts the calling thread back to the normal scheduling policy. Threads
/// inherit the policy of the thread that spawns them, so helper threads call
/// this first.
pub fn set_normal() {
    let param = libc::sched_param { sched_priority: 0 };
    // SAFETY: as for `set_realtime`; `SCHED_OTHER` needs priority 0. The
    // result is ignored: a thread that was not real-time stays as it is.
    let _ = unsafe { libc::sched_setscheduler(0, libc::SCHED_OTHER, &param) };
}

/// The memory-lock limit (`RLIMIT_MEMLOCK`, bytes) as `(soft, hard)`.
fn memlock_limit() -> io::Result<(libc::rlim_t, libc::rlim_t)> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes only into the `rlimit` on this stack frame.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut lim) };
    if rc == 0 {
        Ok((lim.rlim_cur, lim.rlim_max))
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether locking every current and future page can work: only without a
/// limit (a process with `CAP_IPC_LOCK` is not bound by it, but this client
/// never has that capability). With a small limit `mlockall(MCL_FUTURE)` still succeeds at start-up
/// (little is mapped yet) and then makes every later mapping fail once the
/// limit is reached, for example PipeWire loading its own modules.
fn memlock_is_unlimited(soft: libc::rlim_t) -> bool {
    soft == libc::RLIM_INFINITY
}

/// Locks all current and future pages in RAM.
///
/// # Errors
/// The OS error, or `Other` (not locked, nothing changed) when the memory-lock
/// limit is not unlimited, which is the case for ordinary users on a stock
/// system (`LimitMEMLOCK` of a *user* unit cannot raise it).
pub fn lock_memory() -> io::Result<()> {
    if let Ok((soft, _)) = memlock_limit() {
        if !memlock_is_unlimited(soft) {
            return Err(io::Error::other(format!(
                "RLIMIT_MEMLOCK is {} KiB, not unlimited; not locking, because a partial limit would \
                 make PipeWire's own later allocations fail",
                soft / 1024
            )));
        }
    }
    // SAFETY: `mlockall` takes only flags and affects the page-locking state
    // of the process; no memory is read or written.
    let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Makes every panic, on any thread, abort the process (ADR 0004/0009): a
/// half-dead client is worse than a restart, and a panicking helper thread
/// must never leave a node stuck in "connecting". systemd restarts the unit.
pub fn abort_on_panic() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("tessera-pw: fatal: {info}");
        std::process::abort();
    }));
}

/// True if the calling thread currently runs with a real-time policy.
pub fn is_realtime() -> bool {
    // SAFETY: `sched_getscheduler` takes a pid (0 = this thread) and returns
    // a policy number; it touches no memory.
    let policy = unsafe { libc::sched_getscheduler(0) };
    policy == libc::SCHED_FIFO || policy == libc::SCHED_RR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_unlimited_memlock_limit_allows_locking_everything() {
        assert!(memlock_is_unlimited(libc::RLIM_INFINITY));
        for small in [0, 8 * 1024 * 1024, libc::RLIM_INFINITY - 1] {
            assert!(!memlock_is_unlimited(small), "{small}");
        }
    }

    #[test]
    fn a_limited_process_is_not_locked_and_still_maps_memory() {
        // A child whose limit leaves room for what it has mapped already: the
        // old `lock_memory` succeeded here (`mlockall` only checks what exists
        // now) and every later mapping then failed, which is what broke
        // PipeWire loading its modules on stock Debian and Ubuntu user
        // sessions. A limit below the process's own size would make even the
        // old code refuse and prove nothing.
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "sys::tests::limited_child",
                "--ignored",
                "--test-threads=1",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "child failed ({:?}): {}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Mapped size of this process in bytes (`VmSize`).
    fn vm_size() -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let kib: u64 = status
            .lines()
            .find_map(|l| l.strip_prefix("VmSize:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .unwrap();
        kib * 1024
    }

    /// Child half of `a_limited_process_is_not_locked_and_still_maps_memory`.
    #[test]
    #[ignore = "child process"]
    fn limited_child() {
        // Room for what is mapped now plus 16 MiB, less than the 64 MiB below.
        let limit = libc::rlim_t::try_from(vm_size() + 16 * 1024 * 1024).unwrap();
        let lim = libc::rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        };
        // SAFETY: `setrlimit` only reads the `rlimit` on this stack frame.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &lim) }, 0);
        assert!(
            lock_memory().is_err(),
            "a limited process must not be locked"
        );
        // Mapping and touching 64 MiB afterwards must work: with the old
        // `lock_memory` (everything locked, 16 MiB left) this aborts.
        let mut big = vec![0u8; 64 * 1024 * 1024];
        big.iter_mut().step_by(4096).for_each(|b| *b = 1);
        assert_eq!(big.iter().step_by(4096).filter(|b| **b == 1).count(), 16384);
    }

    /// Child half of `a_panic_on_a_helper_thread_aborts_the_process`.
    #[test]
    #[ignore = "child process"]
    fn panic_child() {
        abort_on_panic();
        std::thread::spawn(|| panic!("boom")).join().ok();
        std::process::exit(0);
    }

    #[test]
    fn a_panic_on_a_helper_thread_aborts_the_process() {
        use std::os::unix::process::ExitStatusExt;
        let exe = std::env::current_exe().unwrap();
        let status = std::process::Command::new(exe)
            .args([
                "--exact",
                "sys::tests::panic_child",
                "--ignored",
                "--test-threads=1",
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGABRT), "{status:?}");
    }

    #[test]
    fn helper_threads_can_reset_inherited_priority() {
        // Without permission this is a no-op test of the call itself.
        std::thread::spawn(|| {
            set_normal();
            assert!(!is_realtime());
        })
        .join()
        .unwrap();
    }
}
