//! Allocation accounting for the real-time process callbacks, used by the
//! PipeWire integration tests (feature `alloc-check`).
//!
//! With the feature, the binary installs [`CountingAlloc`] as its global
//! allocator; [`RtGuard`] marks the current thread as "inside a real-time
//! callback" and every allocation made while a guard is alive is counted in
//! [`rt_allocs`]. Without the feature the guard is an empty type and nothing
//! changes. The count is reported in the final counters line, where the test
//! asserts it is zero.
#![allow(unsafe_code)] // only the GlobalAlloc impl below

#[cfg(feature = "alloc-check")]
mod imp {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering};

    thread_local! {
        static ACTIVE: Cell<u32> = const { Cell::new(0) };
    }

    static RT_ALLOCS: AtomicU64 = AtomicU64::new(0);

    /// Marks the current thread as inside a real-time callback while alive.
    #[derive(Debug)]
    pub struct RtGuard(());

    impl RtGuard {
        /// Enters a real-time section.
        pub fn enter() -> Self {
            let _ = ACTIVE.try_with(|a| a.set(a.get() + 1));
            RtGuard(())
        }
    }

    impl Drop for RtGuard {
        fn drop(&mut self) {
            let _ = ACTIVE.try_with(|a| a.set(a.get().saturating_sub(1)));
        }
    }

    /// Allocations counted inside real-time sections so far.
    pub fn rt_allocs() -> u64 {
        RT_ALLOCS.load(Ordering::Relaxed)
    }

    fn note() {
        if ACTIVE.try_with(|a| a.get() > 0).unwrap_or(false) {
            RT_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A global allocator that counts allocations made inside [`RtGuard`]s.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct CountingAlloc;

    // SAFETY: every method forwards its arguments unchanged to `System`,
    // which upholds the `GlobalAlloc` contract; the bookkeeping touches only a
    // const-initialised thread-local `Cell` and an atomic, so it cannot
    // allocate or unwind.
    unsafe impl GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note();
            // SAFETY: forwarded under the caller's contract.
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note();
            // SAFETY: forwarded under the caller's contract.
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: forwarded under the caller's contract.
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note();
            // SAFETY: forwarded under the caller's contract.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
}

#[cfg(not(feature = "alloc-check"))]
mod imp {
    /// No-op without the `alloc-check` feature.
    #[derive(Debug)]
    pub struct RtGuard(());

    impl RtGuard {
        /// Enters a real-time section (does nothing).
        #[inline(always)]
        pub fn enter() -> Self {
            RtGuard(())
        }
    }

    /// Always zero without the feature.
    pub fn rt_allocs() -> u64 {
        0
    }
}

pub use imp::*;
