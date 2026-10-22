//! The one module that touches raw pointers: a shared `mmap` of a sealed
//! memfd, exposed only as slices of atomics.
//!
//! Everything else in the crate is safe code. All cross-process shared state
//! is accessed through `AtomicU32`/`AtomicU64` (never through plain
//! references to integers), so concurrent access from another process is
//! never a Rust data race.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::fmt;
use std::num::NonZeroUsize;
use std::os::fd::AsFd;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64};

use nix::errno::Errno;
use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap};

/// An owned `MAP_SHARED` mapping of a whole memfd.
pub(crate) struct Mapping {
    ptr: NonNull<c_void>,
    len: usize,
    writable: bool,
}

// SAFETY: the mapping is plain memory owned by this value. The only ways to
// reach its contents are `u32s`/`u64s`, which return slices of atomics, so
// sharing `&Mapping` between threads cannot cause a data race. The pointer
// is never exposed and is unmapped exactly once, in `Drop`.
unsafe impl Send for Mapping {}
// SAFETY: see the `Send` impl above; all shared access is via atomics.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Maps `len` bytes of `fd` from offset 0. The caller must have verified
    /// that the file is at least `len` bytes and sealed against shrinking, so
    /// that no access can fault with `SIGBUS`.
    pub(crate) fn new(fd: impl AsFd, len: usize, writable: bool) -> Result<Self, Errno> {
        let nz = NonZeroUsize::new(len).ok_or(Errno::EINVAL)?;
        let prot = if writable {
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE
        } else {
            ProtFlags::PROT_READ
        };
        // SAFETY: `addr` is `None`, so the kernel picks a fresh address range
        // that no existing Rust object occupies; the length is non-zero; the
        // result is checked by `mmap` (error or non-null pointer). The
        // mapping is exclusively owned by the returned value until `Drop`.
        let ptr = unsafe { mmap(None, nz, prot, MapFlags::MAP_SHARED, fd, 0) }?;
        Ok(Self { ptr, len, writable })
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// A view of `count` `AtomicU32` cells starting at byte offset `off`.
    ///
    /// Panics if the range is out of bounds or misaligned; callers derive
    /// offsets from a layout validated at attach time, so this is an
    /// internal-invariant check, not an input check.
    pub(crate) fn u32s(&self, off: usize, count: usize) -> &[AtomicU32] {
        assert!(
            off.is_multiple_of(align_of::<AtomicU32>())
                && count
                    .checked_mul(size_of::<AtomicU32>())
                    .and_then(|b| b.checked_add(off))
                    .is_some_and(|end| end <= self.len),
            "u32 view out of bounds"
        );
        // SAFETY: the range `[off, off + 4*count)` lies inside the mapping
        // (asserted above); the base is page-aligned and `off` is a multiple
        // of 4, so the pointer is aligned for `AtomicU32`; `AtomicU32` has
        // the size/alignment of `u32` and every bit pattern is a valid value;
        // the mapping outlives the returned borrow of `&self`; and all
        // concurrent access to these bytes (here or in another process) is
        // atomic.
        unsafe {
            std::slice::from_raw_parts(
                self.ptr.as_ptr().cast::<u8>().add(off).cast::<AtomicU32>(),
                count,
            )
        }
    }

    /// A view of `count` `AtomicU64` cells starting at byte offset `off`
    /// (which must be a multiple of 8). See [`Mapping::u32s`].
    pub(crate) fn u64s(&self, off: usize, count: usize) -> &[AtomicU64] {
        assert!(
            off.is_multiple_of(align_of::<AtomicU64>())
                && count
                    .checked_mul(size_of::<AtomicU64>())
                    .and_then(|b| b.checked_add(off))
                    .is_some_and(|end| end <= self.len),
            "u64 view out of bounds"
        );
        // SAFETY: as for `u32s`, with 8-byte size and alignment (offset is a
        // multiple of 8 and the base is page-aligned).
        unsafe {
            std::slice::from_raw_parts(
                self.ptr.as_ptr().cast::<u8>().add(off).cast::<AtomicU64>(),
                count,
            )
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are exactly the pair returned by/passed to
        // `mmap` in `new`; this is the only place that unmaps it, and no
        // borrowed view can outlive `self`.
        let _ = unsafe { munmap(self.ptr, self.len) };
    }
}

impl fmt::Debug for Mapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mapping")
            .field("len", &self.len)
            .field("writable", &self.writable)
            .finish()
    }
}

/// Takes ownership of a descriptor the kernel just installed in our table
/// (from `SCM_RIGHTS`).
pub(crate) fn adopt_raw_fd(fd: std::os::fd::RawFd) -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd;
    // SAFETY: the caller passes a descriptor number that `recvmsg` just
    // returned in an `SCM_RIGHTS` control message: it is open, belongs to
    // this process, and nothing else owns or will close it, so the returned
    // `OwnedFd` is its unique owner.
    unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }
}

/// Test helper: does a shared writable mapping of `fd` succeed?
#[cfg(test)]
pub(crate) fn can_map_writable(fd: impl AsFd, len: usize) -> bool {
    let Some(nz) = NonZeroUsize::new(len) else {
        return false;
    };
    // SAFETY: fresh kernel-chosen address range; if the mapping succeeds it
    // is immediately unmapped below with the same pointer and length.
    match unsafe {
        mmap(
            None,
            nz,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_SHARED,
            fd,
            0,
        )
    } {
        Ok(p) => {
            // SAFETY: `p`/`len` come from the successful `mmap` above.
            let _ = unsafe { munmap(p, len) };
            true
        }
        Err(_) => false,
    }
}
