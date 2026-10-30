//! State shared by the tasks of one browser, and non-blocking delivery of its
//! events.
//!
//! The backends must keep reading from their sources even when nobody reads
//! the [`crate::Browser`] (ADR 0004): `mdns-sd` blocks its single daemon thread
//! when a browse channel is full, and a full `zbus` message queue stalls the
//! shared D-Bus connection. Events are therefore pushed with `try_send`. When
//! the channel is full the event is counted as lost and a
//! [`BrowseEvent::Overflow`] is owed to the consumer: the producer delivers it
//! with the next event that fits, and the consumer takes it itself when it has
//! drained the channel and nothing else is coming ([`BrowseView::take_overflow`]).
//! The cache always holds the true state ([`crate::Browser::snapshot`]).
//!
//! Two halves: [`BrowseShared`] is the producer side (it owns a sender, so the
//! channel stays open as long as any backend task holds one) and [`BrowseView`]
//! is what the `Browser` keeps (no sender, so the stream ends when the backend
//! has stopped).
//!
//! Cache updates and the delivery of the resulting event happen under the same
//! cache guard ([`BrowseShared::update`]); otherwise two tasks feeding one
//! cache (one per interface) could deliver their events in a different order
//! than they changed the state.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::cache::BrowseCache;
use crate::model::{BrowseEvent, ServiceInstance, ServiceObservation};

#[derive(Debug, Default)]
struct Counters {
    revision: AtomicU64,
    reconciliation_generation: Mutex<Option<u64>>,
    sessions: Mutex<BTreeMap<usize, Option<u64>>>,
    /// The most recent resolution failure.
    last_failure: Mutex<Option<String>>,
    /// Events that could not be delivered because the channel was full.
    lost: AtomicU64,
    /// Instances a backend refused to track (cap reached).
    refused: AtomicU64,
    /// Resolutions that failed.
    failures: AtomicU64,
    /// An `Overflow` marker is owed to the consumer.
    owed: AtomicBool,
}

fn lock_cache(c: &Mutex<BrowseCache>) -> MutexGuard<'_, BrowseCache> {
    c.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Producer side; see the module docs. Cheap to clone.
#[derive(Clone)]
pub(crate) struct BrowseShared {
    cache: Arc<Mutex<BrowseCache>>,
    tx: mpsc::Sender<BrowseEvent>,
    counters: Arc<Counters>,
}

/// Consumer side; see the module docs.
#[derive(Clone)]
pub(crate) struct BrowseView {
    cache: Arc<Mutex<BrowseCache>>,
    counters: Arc<Counters>,
}

impl std::fmt::Debug for BrowseShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowseShared").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for BrowseView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowseView").finish_non_exhaustive()
    }
}

impl BrowseShared {
    pub(crate) fn new(
        max_per_kind: usize,
        capacity: usize,
    ) -> (Self, BrowseView, mpsc::Receiver<BrowseEvent>) {
        let (tx, rx) = mpsc::channel(capacity.max(2));
        let cache = Arc::new(Mutex::new(BrowseCache::new(max_per_kind)));
        let counters = Arc::new(Counters::default());
        (
            Self {
                cache: Arc::clone(&cache),
                tx,
                counters: Arc::clone(&counters),
            },
            BrowseView { cache, counters },
            rx,
        )
    }

    /// Counts an instance that a backend refused to track (cap reached).
    pub(crate) fn note_refused(&self) {
        self.counters.refused.fetch_add(1, Ordering::Relaxed);
        self.counters.revision.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a failed resolution or session-recovery diagnostic.
    pub(crate) fn note_failure(&self, error: &str) {
        self.counters.failures.fetch_add(1, Ordering::Relaxed);
        self.counters.revision.fetch_add(1, Ordering::Relaxed);
        *self
            .counters
            .last_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.to_owned());
    }

    /// Applies `f` to the cache and delivers the event it returns, all under
    /// the cache guard. Returns false once the receiving [`crate::Browser`] is
    /// gone (the task should stop).
    pub(crate) fn update(&self, f: impl FnOnce(&mut BrowseCache) -> Option<BrowseEvent>) -> bool {
        let mut c = lock_cache(&self.cache);
        let before = (c.dropped(), c.reconciled());
        let ev = f(&mut c);
        if ev.is_some() || before != (c.dropped(), c.reconciled()) {
            self.counters.revision.fetch_add(1, Ordering::Relaxed);
        }
        self.push(ev)
    }

    pub(crate) fn session(&self, slot: usize, generation: Option<u64>) {
        let mut sessions = self
            .counters
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sessions.get(&slot) != Some(&generation) {
            sessions.insert(slot, generation);
            self.counters.revision.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn begin_reconciliation(&self, generation: u64) {
        let mut current = self
            .counters
            .reconciliation_generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *current != Some(generation) {
            lock_cache(&self.cache).begin_reconciliation();
            *current = Some(generation);
            self.counters.revision.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Delivers `ev` without waiting. Returns false once the receiving
    /// [`crate::Browser`] is gone. Prefer [`BrowseShared::update`] when the
    /// event comes from a cache change.
    pub(crate) fn push(&self, ev: Option<BrowseEvent>) -> bool {
        let Some(ev) = ev else {
            return !self.tx.is_closed();
        };
        if !self.flush_owed() {
            return false;
        }
        match self.tx.try_send(ev) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.counters.lost.fetch_add(1, Ordering::Relaxed);
                self.counters.owed.store(true, Ordering::SeqCst);
                // The consumer may have drained the channel between the failed
                // send and the flag: it would then wait for ever on a stale
                // view. Try to deliver the marker right away.
                self.flush_owed()
            }
            Err(TrySendError::Closed(_)) => false,
        }
    }

    /// Sends the owed marker if there is one and there is room.
    fn flush_owed(&self) -> bool {
        let view = BrowseView {
            cache: Arc::clone(&self.cache),
            counters: Arc::clone(&self.counters),
        };
        let Some(marker) = view.take_overflow() else {
            return true;
        };
        match self.tx.try_send(marker) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                // No room even for the marker: it stays owed.
                self.counters.owed.store(true, Ordering::SeqCst);
                true
            }
            Err(TrySendError::Closed(_)) => false,
        }
    }
}

impl BrowseView {
    pub(crate) fn revision(&self) -> u64 {
        self.counters.revision.load(Ordering::Relaxed)
    }

    pub(crate) fn reconciled(&self, generation: u64) -> bool {
        self.counters
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .all(|ready| *ready == Some(generation))
            && lock_cache(&self.cache).reconciled()
    }
    /// Instances ignored: over the per-kind cap or refused by a backend.
    pub(crate) fn ignored(&self) -> u64 {
        lock_cache(&self.cache).dropped() + self.counters.refused.load(Ordering::Relaxed)
    }

    /// Events that could not be delivered because the channel was full.
    pub(crate) fn lost(&self) -> u64 {
        self.counters.lost.load(Ordering::Relaxed)
    }

    /// Failed resolutions and recovery from saturated bookkeeping or retired-path reuse.
    pub(crate) fn failures(&self) -> u64 {
        self.counters.failures.load(Ordering::Relaxed)
    }

    /// The most recent resolution or session-recovery diagnostic, if any.
    pub(crate) fn last_failure(&self) -> Option<String> {
        self.counters
            .last_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The overflow marker if events were lost since it was last delivered.
    /// Called by the consumer when the channel is empty.
    pub(crate) fn take_overflow(&self) -> Option<BrowseEvent> {
        self.counters
            .owed
            .swap(false, Ordering::SeqCst)
            .then(|| BrowseEvent::Overflow { lost: self.lost() })
    }

    /// Everything currently known.
    pub(crate) fn snapshot(&self) -> Vec<ServiceInstance> {
        lock_cache(&self.cache).current()
    }

    pub(crate) fn observations(&self) -> Vec<ServiceObservation> {
        lock_cache(&self.cache).observations()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::SourceView;
    use crate::model::{ServiceId, ServiceKind};
    use crate::txt::TxtRecord;
    use std::collections::BTreeSet;

    fn gone(i: u32) -> Option<BrowseEvent> {
        Some(BrowseEvent::Disappeared(ServiceId::new(
            ServiceKind::Arc,
            i.to_string(),
        )))
    }

    #[test]
    fn overflow_is_counted_and_announced_once_there_is_room() {
        let (sh, view, mut rx) = BrowseShared::new(10, 2);
        // Nobody reads: the producer is never blocked or refused.
        for i in 0..10 {
            assert!(sh.push(gone(i)));
        }
        assert_eq!(view.lost(), 8);
        // The reader catches up; the next event is preceded by the marker.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
        assert!(sh.push(gone(100)));
        assert_eq!(rx.try_recv().unwrap(), BrowseEvent::Overflow { lost: 8 });
        // Capacity 2 was free again, so the event fits behind the marker.
        assert!(matches!(
            rx.try_recv().unwrap(),
            BrowseEvent::Disappeared(_)
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn consumer_collects_the_marker_when_nothing_else_comes() {
        let (sh, view, mut rx) = BrowseShared::new(10, 2);
        for i in 0..5 {
            assert!(sh.push(gone(i)));
        }
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        // Drained, and the network goes quiet: the consumer asks for it.
        assert_eq!(
            view.take_overflow(),
            Some(BrowseEvent::Overflow { lost: 3 })
        );
        assert_eq!(view.take_overflow(), None);
        // Nothing is delivered twice: the next event goes out alone.
        assert!(sh.push(gone(9)));
        assert!(matches!(
            rx.try_recv().unwrap(),
            BrowseEvent::Disappeared(_)
        ));
        assert!(rx.try_recv().is_err());
    }

    /// The consumer drains the channel between the producer's failed send and
    /// the producer's owing the marker: the marker must still reach it without
    /// any further event. (Stress: the interleaving is rare, so many rounds.)
    #[test]
    fn a_consumer_that_waits_is_never_left_with_a_stale_view() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        for _ in 0..300 {
            let (sh, view, mut rx) = BrowseShared::new(100, 2);
            let producer = std::thread::spawn(move || {
                for i in 0..50 {
                    if !sh.push(gone(i)) {
                        break;
                    }
                }
            });
            let consumer = rt.block_on(async {
                let mut seen_overflow = false;
                let mut got = 0;
                loop {
                    // What Browser::recv does.
                    let ev = match rx.try_recv() {
                        Ok(e) => Some(e),
                        Err(_) => view.take_overflow(),
                    };
                    let ev = match ev {
                        Some(e) => e,
                        None => match tokio::time::timeout(
                            std::time::Duration::from_millis(200),
                            rx.recv(),
                        )
                        .await
                        {
                            Ok(Some(e)) => e,
                            Ok(None) => {
                                seen_overflow |= view.take_overflow().is_some();
                                break;
                            }
                            Err(_) => break, // quiet: nothing more is coming
                        },
                    };
                    match ev {
                        BrowseEvent::Overflow { .. } => seen_overflow = true,
                        _ => got += 1,
                    }
                }
                (seen_overflow, got)
            });
            producer.join().unwrap();
            let (seen_overflow, got) = consumer;
            // Quiet and drained: everything not delivered must have been announced.
            assert!(
                got == 50 || seen_overflow,
                "lost {} events without a marker (got {got})",
                view.lost()
            );
        }
    }

    fn view_of(port: u16) -> SourceView {
        SourceView {
            interface: None,
            host: "h.".into(),
            port,
            addresses: BTreeSet::new(),
            txt: TxtRecord::new(),
        }
    }

    /// Several tasks feeding one cache deliver events in the order the state
    /// changed: replaying what arrives reproduces the cache.
    #[test]
    fn backend_generation_reconciles_overflow_once_without_clearing_new_refusals() {
        let (shared, view, _rx) = BrowseShared::new(1, 4);
        shared.begin_reconciliation(1);
        shared.session(0, Some(1));
        let accepted = ServiceId::new(ServiceKind::Arc, "accepted");
        shared.update(|cache| cache.on_resolved(accepted, "s".into(), view_of(1)));
        for n in 0..=ServiceKind::ALL.len() {
            let id = ServiceId::new(ServiceKind::Arc, format!("refused-{n}"));
            shared.update(|cache| cache.on_resolved(id, "s".into(), view_of(1)));
        }
        for n in 0..=ServiceKind::ALL.len() {
            let id = ServiceId::new(ServiceKind::Arc, format!("refused-{n}"));
            shared.update(|cache| cache.on_removed(&id, "s"));
        }
        assert!(!view.reconciled(1));
        let ignored = view.ignored();
        shared.session(0, None);
        shared.begin_reconciliation(2);
        assert!(!view.reconciled(2), "fresh enumeration must finish first");
        shared.session(0, Some(2));
        assert!(view.reconciled(2));
        assert_eq!(view.ignored(), ignored);
        shared.update(|cache| {
            cache.on_resolved(
                ServiceId::new(ServiceKind::Arc, "new-refusal"),
                "s".into(),
                view_of(1),
            )
        });
        shared.begin_reconciliation(2);
        assert!(
            !view.reconciled(2),
            "another task in the same generation cannot erase current refusals"
        );
    }

    #[test]
    fn concurrent_updates_are_delivered_in_state_order() {
        for _ in 0..50 {
            let (sh, view, mut rx) = BrowseShared::new(1000, 100_000);
            let mut threads = Vec::new();
            for t in 0..4u16 {
                let sh = sh.clone();
                threads.push(std::thread::spawn(move || {
                    for round in 0..200u16 {
                        let id = ServiceId::new(ServiceKind::Arc, "x");
                        let src = format!("s{t}");
                        let (id2, src2) = (id.clone(), src.clone());
                        sh.update(move |c| c.on_resolved(id2, src2, view_of(t * 1000 + round)));
                        if round % 7 == 0 {
                            sh.update(|c| c.on_removed(&id, &src));
                        }
                    }
                }));
            }
            for t in threads {
                t.join().unwrap();
            }
            let mut replay: Option<ServiceInstance> = None;
            while let Ok(e) = rx.try_recv() {
                match e {
                    BrowseEvent::Appeared(i) | BrowseEvent::Updated(i) => replay = Some(i),
                    BrowseEvent::Disappeared(_) => replay = None,
                    BrowseEvent::Overflow { .. } => unreachable!(),
                }
            }
            assert_eq!(replay.into_iter().collect::<Vec<_>>(), view.snapshot());
        }
    }

    #[test]
    fn closed_receiver_stops_the_producer_and_the_stream_ends_with_it() {
        let (sh, _view, rx) = BrowseShared::new(10, 4);
        drop(rx);
        assert!(!sh.push(gone(1)));
        assert!(!sh.push(None));
        // The other way round: dropping all producers ends the stream.
        let (sh, _view, mut rx) = BrowseShared::new(10, 4);
        drop(sh);
        assert!(rx.try_recv().is_err());
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }
}
