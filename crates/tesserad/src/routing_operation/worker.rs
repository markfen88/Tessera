//! An actor-owned supervisor; caller response delivery never owns the worker.
use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) fn spawn<T, F, C, R>(
    runtime: &tokio::runtime::Handle,
    exited: Arc<AtomicBool>,
    work: F,
    complete: C,
) where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
    C: FnOnce(Option<T>) -> R + Send + 'static,
    R: Future<Output = ()> + Send + 'static,
{
    // Joining establishes that every worker-owned socket/future is gone.
    // A deadline alone would establish no such fact.
    let work = runtime.spawn(work);
    runtime.spawn(async move {
        let outcome = work.await.ok();
        exited.store(true, Ordering::Release);
        complete(outcome).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lost_response_does_not_cancel_worker_or_completion() {
        let exited = Arc::new(AtomicBool::new(false));
        let (response, receiver) = tokio::sync::oneshot::channel::<u8>();
        drop(receiver);
        let (done, observed) = tokio::sync::oneshot::channel();
        let checked = exited.clone();
        spawn(
            &tokio::runtime::Handle::current(),
            exited,
            async {
                tokio::task::yield_now().await;
                7
            },
            move |outcome| async move {
                assert!(checked.load(Ordering::Acquire));
                let _ = response.send(outcome.unwrap());
                done.send(()).unwrap();
            },
        );
        observed.await.unwrap();
    }

    #[tokio::test]
    async fn panic_retires_worker_before_reporting_unknown() {
        struct SocketLike(Arc<AtomicBool>);
        impl Drop for SocketLike {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let owned = dropped.clone();
        let exited = Arc::new(AtomicBool::new(false));
        let checked = exited.clone();
        let (done, observed) = tokio::sync::oneshot::channel();
        spawn(
            &tokio::runtime::Handle::current(),
            exited,
            async move {
                let _socket = SocketLike(owned);
                panic!("simulated post-dispatch worker panic");
            },
            move |outcome: Option<()>| async move {
                assert!(outcome.is_none());
                assert!(checked.load(Ordering::Acquire));
                assert!(dropped.load(Ordering::Acquire));
                done.send(()).unwrap();
            },
        );
        observed.await.unwrap();
    }
}
