//! Event bus: one publisher side, many subscribers, std only.
//!
//! * Every subscriber has its **own bounded queue** (capacity chosen at
//!   subscription time, at least 1). There is no unbounded queue anywhere.
//! * Publishing never blocks and never fails: the state actor must not be
//!   stalled by a slow consumer.
//! * **Slow-subscriber policy: drop oldest, count, tell.** When a subscriber's
//!   queue is full the oldest queued event is discarded and a per-subscriber
//!   counter is incremented. The next [`Subscription::try_recv`] /
//!   [`recv`](Subscription::recv) / [`recv_async`](Subscription::recv_async)
//!   first returns [`Received::Lagged`] with the number of events lost since
//!   the last delivery, then continues with the newest events in order. A
//!   subscriber that sees `Lagged` should re-read a status snapshot.
//! * Dropping a [`Subscription`] unsubscribes it. Dropping the [`EventBus`]
//!   (and all its clones) closes every subscription once its queue is drained.
//! * Async consumers use [`Subscription::recv_async`], a runtime-agnostic
//!   future (no tokio dependency in this crate, ADR 0003 amendment).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tessera_control_types::Event;

/// What a subscriber gets.
#[derive(Debug, Clone, PartialEq)]
pub enum Received {
    /// The next event.
    Event(Event),
    /// This many events were dropped because the queue was full.
    Lagged(u64),
}

/// Why nothing was received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// Nothing queued right now (non-blocking calls), or the timeout elapsed.
    Empty,
    /// The bus is gone and the queue is drained.
    Closed,
}

#[derive(Debug)]
struct Inner {
    queue: VecDeque<Event>,
    capacity: usize,
    lagged: u64,
    closed: bool,
    waker: Option<Waker>,
}

#[derive(Debug)]
struct Slot {
    inner: Mutex<Inner>,
    ready: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Default)]
struct Shared {
    slots: Mutex<Vec<Weak<Slot>>>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        let slots = self.slots.get_mut().unwrap_or_else(PoisonError::into_inner);
        for w in slots.iter() {
            if let Some(s) = w.upgrade() {
                let mut i = lock(&s.inner);
                i.closed = true;
                let waker = i.waker.take();
                drop(i);
                s.ready.notify_all();
                if let Some(w) = waker {
                    w.wake();
                }
            }
        }
    }
}

/// The publishing side. Cheap to clone; clones publish to the same
/// subscribers.
#[derive(Debug, Clone, Default)]
pub struct EventBus {
    shared: Arc<Shared>,
}

impl EventBus {
    /// A bus with no subscribers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a subscriber whose queue holds at most `capacity` events
    /// (`0` is treated as `1`).
    #[must_use]
    pub fn subscribe(&self, capacity: usize) -> Subscription {
        let slot = Arc::new(Slot {
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                capacity: capacity.max(1),
                lagged: 0,
                closed: false,
                waker: None,
            }),
            ready: Condvar::new(),
        });
        lock(&self.shared.slots).push(Arc::downgrade(&slot));
        Subscription { slot }
    }

    /// Deliver `event` to every live subscriber. Never blocks on a consumer.
    pub fn publish(&self, event: &Event) {
        let mut slots = lock(&self.shared.slots);
        slots.retain(|w| {
            let Some(slot) = w.upgrade() else {
                return false;
            };
            let mut i = lock(&slot.inner);
            if i.queue.len() >= i.capacity {
                i.queue.pop_front();
                i.lagged += 1;
            }
            i.queue.push_back(event.clone());
            let waker = i.waker.take();
            drop(i);
            slot.ready.notify_one();
            if let Some(w) = waker {
                w.wake();
            }
            true
        });
    }

    /// Number of live subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        lock(&self.shared.slots)
            .iter()
            .filter(|w| w.strong_count() > 0)
            .count()
    }
}

/// The receiving side of one subscription.
#[derive(Debug)]
pub struct Subscription {
    slot: Arc<Slot>,
}

fn take_next(i: &mut Inner) -> Result<Received, RecvError> {
    if i.lagged > 0 {
        let n = std::mem::take(&mut i.lagged);
        return Ok(Received::Lagged(n));
    }
    if let Some(e) = i.queue.pop_front() {
        return Ok(Received::Event(e));
    }
    if i.closed {
        Err(RecvError::Closed)
    } else {
        Err(RecvError::Empty)
    }
}

impl Subscription {
    /// Take the next item without waiting.
    ///
    /// # Errors
    /// [`RecvError::Empty`] or [`RecvError::Closed`].
    pub fn try_recv(&self) -> Result<Received, RecvError> {
        take_next(&mut lock(&self.slot.inner))
    }

    /// Wait for the next item.
    ///
    /// # Errors
    /// [`RecvError::Closed`] once the bus is gone and the queue is empty.
    pub fn recv(&self) -> Result<Received, RecvError> {
        let mut i = lock(&self.slot.inner);
        loop {
            match take_next(&mut i) {
                Err(RecvError::Empty) => {
                    i = self
                        .slot
                        .ready
                        .wait(i)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                other => return other,
            }
        }
    }

    /// Wait for the next item for at most `timeout`.
    ///
    /// # Errors
    /// [`RecvError::Empty`] on timeout, [`RecvError::Closed`] when finished.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Received, RecvError> {
        let deadline = Instant::now().checked_add(timeout);
        let mut i = lock(&self.slot.inner);
        loop {
            match take_next(&mut i) {
                Err(RecvError::Empty) => {
                    let remaining = match deadline {
                        Some(d) => d.saturating_duration_since(Instant::now()),
                        None => timeout,
                    };
                    if remaining.is_zero() {
                        return Err(RecvError::Empty);
                    }
                    let (g, _) = self
                        .slot
                        .ready
                        .wait_timeout(i, remaining)
                        .unwrap_or_else(PoisonError::into_inner);
                    i = g;
                }
                other => return other,
            }
        }
    }

    /// A future resolving to the next item; works under any executor.
    /// Resolves to `Err(RecvError::Closed)` when the bus is gone.
    ///
    /// Takes `&mut self` on purpose: a subscription stores a single waker, so
    /// two tasks awaiting the same subscription at once could lose a wake-up.
    /// The exclusive borrow makes that impossible to write. Re-polling a
    /// future with a different waker replaces the stored one; dropping a
    /// pending future and creating a new one is fine. (The blocking
    /// [`recv`](Self::recv) family may still be shared between threads: each
    /// waiter is a separate condition-variable wait.)
    #[must_use]
    pub fn recv_async(&mut self) -> RecvFuture<'_> {
        RecvFuture { sub: self }
    }

    /// Events currently queued (not counting a pending `Lagged`).
    #[must_use]
    pub fn queued(&self) -> usize {
        lock(&self.slot.inner).queue.len()
    }
}

/// Future returned by [`Subscription::recv_async`].
#[derive(Debug)]
pub struct RecvFuture<'a> {
    sub: &'a mut Subscription,
}

impl Future for RecvFuture<'_> {
    type Output = Result<Received, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut i = lock(&self.sub.slot.inner);
        match take_next(&mut i) {
            Err(RecvError::Empty) => {
                i.waker = Some(cx.waker().clone());
                Poll::Pending
            }
            other => Poll::Ready(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    fn ev(n: u32) -> Event {
        Event::DeviceRenamed {
            device: tessera_control_types::DeviceKey("d".into()),
            name: format!("n{n}"),
            origin: tessera_control_types::Origin::Internal,
        }
    }

    #[test]
    fn every_subscriber_gets_every_event_in_order() {
        let bus = EventBus::new();
        let a = bus.subscribe(8);
        let b = bus.subscribe(8);
        for n in 0..3 {
            bus.publish(&ev(n));
        }
        for s in [&a, &b] {
            for n in 0..3 {
                assert_eq!(s.try_recv(), Ok(Received::Event(ev(n))));
            }
            assert_eq!(s.try_recv(), Err(RecvError::Empty));
        }
    }

    #[test]
    fn slow_subscriber_drops_oldest_and_is_told() {
        let bus = EventBus::new();
        let slow = bus.subscribe(2);
        let fast = bus.subscribe(100);
        for n in 0..5 {
            bus.publish(&ev(n));
        }
        assert_eq!(slow.try_recv(), Ok(Received::Lagged(3)));
        assert_eq!(slow.try_recv(), Ok(Received::Event(ev(3))));
        assert_eq!(slow.try_recv(), Ok(Received::Event(ev(4))));
        assert_eq!(slow.try_recv(), Err(RecvError::Empty));
        // The fast one is unaffected.
        assert_eq!(fast.queued(), 5);
    }

    #[test]
    fn queue_never_exceeds_capacity() {
        let bus = EventBus::new();
        let s = bus.subscribe(0); // clamped to 1
        for n in 0..1000 {
            bus.publish(&ev(n));
        }
        assert_eq!(s.queued(), 1);
    }

    #[test]
    fn dropped_subscription_is_removed() {
        let bus = EventBus::new();
        let a = bus.subscribe(4);
        drop(bus.subscribe(4));
        bus.publish(&ev(0));
        assert_eq!(bus.subscriber_count(), 1);
        drop(a);
        bus.publish(&ev(1));
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn closing_the_bus_drains_then_closes() {
        let bus = EventBus::new();
        let s = bus.subscribe(4);
        bus.publish(&ev(0));
        drop(bus);
        assert_eq!(s.recv(), Ok(Received::Event(ev(0))));
        assert_eq!(s.recv(), Err(RecvError::Closed));
    }

    #[test]
    fn blocking_recv_wakes_on_publish() {
        let bus = EventBus::new();
        let s = bus.subscribe(4);
        let b2 = bus.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            b2.publish(&ev(7));
        });
        assert_eq!(
            s.recv_timeout(Duration::from_secs(5)),
            Ok(Received::Event(ev(7)))
        );
        t.join().unwrap();
        assert_eq!(
            s.recv_timeout(Duration::from_millis(10)),
            Err(RecvError::Empty)
        );
    }

    struct Counter(AtomicUsize);
    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn async_recv_registers_and_wakes() {
        let bus = EventBus::new();
        let mut s = bus.subscribe(4);
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(s.recv_async());
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        bus.publish(&ev(1));
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Ok(Received::Event(ev(1))))
        );
        drop(fut);
        let mut fut2 = Box::pin(s.recv_async());
        assert!(fut2.as_mut().poll(&mut cx).is_pending());
        drop(bus);
        assert_eq!(counter.0.load(Ordering::SeqCst), 2);
        assert_eq!(
            fut2.as_mut().poll(&mut cx),
            Poll::Ready(Err(RecvError::Closed))
        );
    }

    #[test]
    fn async_waker_is_replaced_on_repoll_and_survives_dropped_futures() {
        let bus = EventBus::new();
        let mut s = bus.subscribe(4);
        let a = Arc::new(Counter(AtomicUsize::new(0)));
        let b = Arc::new(Counter(AtomicUsize::new(0)));
        let (wa, wb) = (Waker::from(Arc::clone(&a)), Waker::from(Arc::clone(&b)));
        {
            let mut fut = Box::pin(s.recv_async());
            assert!(
                fut.as_mut()
                    .poll(&mut Context::from_waker(&wa))
                    .is_pending()
            );
            // The task moved: the same future is polled with a new waker.
            assert!(
                fut.as_mut()
                    .poll(&mut Context::from_waker(&wb))
                    .is_pending()
            );
        }
        // The pending future is dropped; a new one must still be woken.
        let mut fut = Box::pin(s.recv_async());
        assert!(
            fut.as_mut()
                .poll(&mut Context::from_waker(&wb))
                .is_pending()
        );
        bus.publish(&ev(1));
        assert_eq!(
            a.0.load(Ordering::SeqCst),
            0,
            "stale waker must not be used"
        );
        assert_eq!(b.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            fut.as_mut().poll(&mut Context::from_waker(&wb)),
            Poll::Ready(Ok(Received::Event(ev(1))))
        );
    }
}
