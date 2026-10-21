//! Panic policy of the non-async threads (ADR 0004, ADR 0009).
//!
//! A panic in a real-time worker aborts the process (the service manager
//! restarts it, state is persisted): a half-dead RT thread is worse than a
//! restart. The PTP and media threads are stubs today but already follow the
//! rule, so the real implementations inherit it.

/// Aborts the process if dropped while the thread is panicking. Create it as
/// the first statement of a thread body and keep it alive to the end.
#[derive(Debug)]
pub struct AbortOnPanic;

impl Drop for AbortOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            std::process::abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    /// Runs only in the child process of the test below.
    #[test]
    fn child_panics_under_the_guard() {
        if std::env::var_os("TESSERAD_RT_TEST_CHILD").is_none() {
            return;
        }
        let t = std::thread::spawn(|| {
            let _g = AbortOnPanic;
            panic!("deliberate panic on a guarded thread");
        });
        let _ = t.join();
        // Reaching this line means the process survived: the guard failed.
        std::process::exit(0);
    }

    #[test]
    fn a_panic_under_the_guard_aborts_the_process() {
        let exe = std::env::current_exe().unwrap();
        let st = std::process::Command::new(exe)
            .args([
                "rt::tests::child_panics_under_the_guard",
                "--exact",
                "--nocapture",
            ])
            .env("TESSERAD_RT_TEST_CHILD", "1")
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(st.signal(), Some(nix::libc::SIGABRT), "{st:?}");
    }

    #[test]
    fn no_panic_no_abort() {
        let t = std::thread::spawn(|| {
            let _g = AbortOnPanic;
            7
        });
        assert_eq!(t.join().unwrap(), 7);
    }
}
