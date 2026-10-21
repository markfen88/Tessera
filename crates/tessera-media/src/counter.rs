//! Atomic metric counters usable on every target.
//!
//! RT threads only ever `add`/`record_max` (relaxed, no locks, no syscalls);
//! the control thread reads with `get`/`take` (ADR 0009).
//!
//! Targets without 64-bit atomics (for example `thumbv8m.main`) fall back to
//! a 32-bit atomic: such a counter wraps at 2^32 and `record_max` saturates
//! at `u32::MAX`.

use core::sync::atomic::Ordering;

#[cfg(target_has_atomic = "64")]
type Raw = core::sync::atomic::AtomicU64;
#[cfg(target_has_atomic = "64")]
type Word = u64;
#[cfg(not(target_has_atomic = "64"))]
type Raw = core::sync::atomic::AtomicU32;
#[cfg(not(target_has_atomic = "64"))]
type Word = u32;

/// A monotonically increasing (or max-tracking) metric.
#[derive(Debug, Default)]
pub struct Counter(Raw);

fn narrow(n: u64) -> Word {
    #[cfg(target_has_atomic = "64")]
    {
        n
    }
    #[cfg(not(target_has_atomic = "64"))]
    {
        // Wraps on purpose: see the module documentation.
        n as u32
    }
}

fn widen(w: Word) -> u64 {
    #[cfg(target_has_atomic = "64")]
    {
        w
    }
    #[cfg(not(target_has_atomic = "64"))]
    {
        u64::from(w)
    }
}

impl Counter {
    /// A counter at zero.
    #[must_use]
    pub const fn new() -> Self {
        Counter(Raw::new(0))
    }

    /// Adds `n`.
    #[inline]
    pub fn add(&self, n: u64) {
        self.0.fetch_add(narrow(n), Ordering::Relaxed);
    }

    /// Adds one.
    #[inline]
    pub fn inc(&self) {
        self.add(1);
    }

    /// Raises the value to at least `n` (a gauge that tracks a maximum).
    #[inline]
    pub fn record_max(&self, n: u64) {
        #[cfg(target_has_atomic = "64")]
        self.0.fetch_max(n, Ordering::Relaxed);
        #[cfg(not(target_has_atomic = "64"))]
        self.0
            .fetch_max(u32::try_from(n).unwrap_or(u32::MAX), Ordering::Relaxed);
    }

    /// Current value.
    #[inline]
    #[must_use]
    pub fn get(&self) -> u64 {
        widen(self.0.load(Ordering::Relaxed))
    }

    /// Reads and resets to zero.
    #[inline]
    pub fn take(&self) -> u64 {
        widen(self.0.swap(0, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_max_take() {
        let c = Counter::new();
        c.add(5);
        c.inc();
        assert_eq!(c.get(), 6);
        c.record_max(3);
        assert_eq!(c.get(), 6);
        c.record_max(10);
        assert_eq!(c.take(), 10);
        assert_eq!(c.get(), 0);
    }
}
