//! Supervision of control-plane tasks (ADR 0004, ADR 0009).
//!
//! A task that panics or returns while the daemon is still running is
//! restarted with exponential backoff (100 ms doubling to 5 s; the backoff
//! resets after a run of at least 30 s) and counted. RT threads are not
//! supervised this way: a panic there aborts the process.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Counts restarts of a supervised task.
pub type RestartCounter = Arc<AtomicU64>;

/// Run `make()` as a task until `shutdown` is cancelled, restarting it when
/// it ends early. Returns the supervisor's handle and its restart counter.
pub fn supervise<F, Fut>(
    name: &'static str,
    shutdown: CancellationToken,
    mut make: F,
) -> (JoinHandle<()>, RestartCounter)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let restarts: RestartCounter = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&restarts);
    let handle = tokio::spawn(async move {
        let mut backoff = Duration::from_millis(100);
        loop {
            let started = Instant::now();
            let mut child = tokio::spawn(make());
            let joined = tokio::select! {
                r = &mut child => r,
                () = shutdown.cancelled() => {
                    // The task is expected to notice the token itself; abort
                    // it if it does not within the grace period.
                    if tokio::time::timeout(Duration::from_secs(5), &mut child).await.is_err() {
                        child.abort();
                    }
                    return;
                }
            };
            match joined {
                Ok(()) => {
                    if shutdown.is_cancelled() {
                        return;
                    }
                    tracing::warn!("{name} ended unexpectedly; restarting");
                }
                Err(e) if e.is_panic() => tracing::error!("{name} panicked; restarting"),
                Err(_) => return,
            }
            if shutdown.is_cancelled() {
                return;
            }
            counter.fetch_add(1, Ordering::Relaxed);
            if started.elapsed() >= Duration::from_secs(30) {
                backoff = Duration::from_millis(100);
            }
            tokio::select! {
                () = tokio::time::sleep(backoff) => {}
                () = shutdown.cancelled() => return,
            }
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    });
    (handle, restarts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_panicking_task_is_restarted_and_counted() {
        let shutdown = CancellationToken::new();
        let runs = Arc::new(AtomicU64::new(0));
        let r = Arc::clone(&runs);
        let (h, restarts) = supervise("t", shutdown.clone(), move || {
            let r = Arc::clone(&r);
            async move {
                let n = r.fetch_add(1, Ordering::SeqCst);
                if n < 3 {
                    panic!("boom {n}");
                }
                std::future::pending::<()>().await;
            }
        });
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 4);
        assert_eq!(restarts.load(Ordering::Relaxed), 3);
        shutdown.cancel();
        h.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_that_ends_after_shutdown_is_not_restarted() {
        let shutdown = CancellationToken::new();
        let s2 = shutdown.clone();
        let (h, restarts) = supervise("t", shutdown.clone(), move || {
            let s = s2.clone();
            async move { s.cancelled().await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        shutdown.cancel();
        h.await.unwrap();
        assert_eq!(restarts.load(Ordering::Relaxed), 0);
    }
}
