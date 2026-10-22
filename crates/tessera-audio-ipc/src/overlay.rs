//! The clock overlay: the media-clock affine map published by the PTP thread
//! through a seqlock and mapped read-only into clients (ADR 0005).

use std::fmt;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering, fence};

use nix::fcntl::SealFlag;
use nix::time::{ClockId, clock_gettime};

use crate::error::{Error, LayoutError, OverlayError};
use crate::map::Mapping;
use crate::memfd;

/// `"TSRACLOK"` as bytes at offset 0.
pub const OVERLAY_MAGIC: [u8; 8] = *b"TSRACLOK";
/// Overlay layout major version.
pub const OVERLAY_VERSION_MAJOR: u32 = 2;
/// Overlay layout minor version.
pub const OVERLAY_VERSION_MINOR: u32 = 0;
/// Size of the overlay mapping in bytes.
pub const OVERLAY_LEN: usize = 128;
/// Default seqlock retry budget for [`ClockOverlayReader::read`].
pub const DEFAULT_RETRIES: u32 = 1000;

/// Byte offsets of the overlay fields (all 8-byte atomics after the header).
mod off {
    pub(super) const MAGIC: usize = 0x00;
    pub(super) const VERSION_MAJOR: usize = 0x08; // u32
    pub(super) const TOTAL_LEN: usize = 0x10; // u64
    pub(super) const SEQ: usize = 0x40;
    pub(super) const EPOCH: usize = 0x48;
    pub(super) const BASE_MONO: usize = 0x50;
    pub(super) const BASE_MEDIA: usize = 0x58;
    pub(super) const RATE_PPB: usize = 0x60;
    pub(super) const STATE: usize = 0x68;
    pub(super) const GM_IDENTITY: usize = 0x70;
    pub(super) const UPDATED_MONO: usize = 0x78;
}

/// Media clock state (ADR 0005).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClockState {
    /// Running at rate 0 against `CLOCK_MONOTONIC_RAW`, no leader.
    FreeRunning,
    /// Listening for a leader.
    Listening,
    /// Acquiring lock.
    Acquiring,
    /// Locked to the grandmaster.
    Locked,
    /// Lost the leader; free-running on the last estimate.
    Holdover,
    /// This device is the leader.
    Leader,
    /// A value this version does not know (from a newer writer).
    Unknown(u32),
}

impl ClockState {
    fn to_u64(self) -> u64 {
        match self {
            ClockState::FreeRunning => 0,
            ClockState::Listening => 1,
            ClockState::Acquiring => 2,
            ClockState::Locked => 3,
            ClockState::Holdover => 4,
            ClockState::Leader => 5,
            ClockState::Unknown(v) => u64::from(v),
        }
    }

    fn from_u64(v: u64) -> Self {
        match v {
            0 => ClockState::FreeRunning,
            1 => ClockState::Listening,
            2 => ClockState::Acquiring,
            3 => ClockState::Locked,
            4 => ClockState::Holdover,
            5 => ClockState::Leader,
            other => ClockState::Unknown(u32::try_from(other).unwrap_or(u32::MAX)),
        }
    }
}

/// One consistent set of overlay values.
///
/// `media_ns(t) = base_media_ns + (t - base_mono_ns) * (1 + rate_ppb * 1e-9)`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSnapshot {
    /// Incremented on a discontinuity of media time; consumers re-anchor
    /// when it changes. Continuous state or grandmaster changes keep it.
    pub epoch: u64,
    /// `CLOCK_MONOTONIC_RAW` nanoseconds of the anchor point (layout 2).
    pub base_mono_ns: u64,
    /// Media-clock nanoseconds at the anchor point.
    pub base_media_ns: u64,
    /// Rate offset against `CLOCK_MONOTONIC_RAW` in parts per billion.
    pub rate_ppb: i64,
    /// Clock state.
    pub state: ClockState,
    /// Grandmaster identity as a big-endian number (8 bytes; shorter
    /// identities are zero-padded on the left).
    pub gm_identity: u64,
    /// `CLOCK_MONOTONIC_RAW` nanoseconds when the writer last published
    /// (liveness indicator).
    pub updated_mono_ns: u64,
}

impl ClockSnapshot {
    /// Media time in nanoseconds at monotonic time `mono_ns`. Pure
    /// arithmetic (no syscall); intermediate math in `i128`; the rate term
    /// truncates toward zero; the result saturates to `[0, u64::MAX]`.
    pub fn media_ns_at(&self, mono_ns: u64) -> u64 {
        let delta = i128::from(mono_ns) - i128::from(self.base_mono_ns);
        let adj = delta * i128::from(self.rate_ppb) / 1_000_000_000;
        let media = i128::from(self.base_media_ns) + delta + adj;
        media.clamp(0, i128::from(u64::MAX)) as u64
    }

    /// Media time in whole samples at `rate_hz`.
    pub fn media_samples_at(&self, mono_ns: u64, rate_hz: u32) -> u64 {
        let ns = u128::from(self.media_ns_at(mono_ns));
        let s = ns * u128::from(rate_hz) / 1_000_000_000;
        u64::try_from(s).unwrap_or(u64::MAX)
    }
}

/// Reads `CLOCK_MONOTONIC` in nanoseconds (vDSO; no real syscall).
pub fn monotonic_ns() -> u64 {
    clock_ns(ClockId::CLOCK_MONOTONIC)
}

/// Reads the unslewed hardware timebase used by layout-2 media clocks.
/// Unlike `CLOCK_MONOTONIC`, its speed is independent of NTP adjustments.
/// This clock also stops during suspend; wall time is never used to advance
/// audio through a suspended interval.
pub fn raw_monotonic_ns() -> u64 {
    clock_ns(ClockId::CLOCK_MONOTONIC_RAW)
}

fn clock_ns(id: ClockId) -> u64 {
    match clock_gettime(id) {
        Ok(ts) => (ts.tv_sec() as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec() as u64),
        Err(_) => 0,
    }
}

fn header_bytes() -> [u64; 4] {
    let mut w0 = [0u8; 8];
    w0.copy_from_slice(&OVERLAY_MAGIC);
    let mut w1 = [0u8; 8];
    w1[..4].copy_from_slice(&OVERLAY_VERSION_MAJOR.to_ne_bytes());
    w1[4..].copy_from_slice(&OVERLAY_VERSION_MINOR.to_ne_bytes());
    [
        u64::from_ne_bytes(w0),
        u64::from_ne_bytes(w1),
        OVERLAY_LEN as u64,
        0,
    ]
}

fn cell(map: &Mapping, o: usize) -> &AtomicU64 {
    &map.u64s(o, 1)[0]
}

/// Whether `try_read` uses the 32-bit split loads: always on targets with a
/// pointer width below 64 bits, and on request (test feature) elsewhere.
const SPLIT_READS: bool =
    !cfg!(target_pointer_width = "64") || cfg!(feature = "force-split-overlay-reads");

#[derive(Clone, Copy)]
enum Half {
    Low,
    High,
}

/// One relaxed 32-bit load of the low or high half of the 64-bit word at
/// byte offset `o` (a multiple of 8), independent of endianness.
fn load32_of_u64(map: &Mapping, o: usize, half: Half) -> u32 {
    let words = map.u32s(o, 2);
    let idx = match (half, cfg!(target_endian = "little")) {
        (Half::Low, true) | (Half::High, false) => 0,
        (Half::High, true) | (Half::Low, false) => 1,
    };
    words[idx].load(Ordering::Relaxed)
}

/// A 64-bit field as two relaxed 32-bit loads (may be torn; the seqlock
/// check discards torn snapshots).
fn load64_split(map: &Mapping, o: usize) -> u64 {
    u64::from(load32_of_u64(map, o, Half::Low))
        | (u64::from(load32_of_u64(map, o, Half::High)) << 32)
}

/// The publishing side (daemon). Single writer; not `Clone`.
pub struct ClockOverlayWriter {
    map: Mapping,
    fd: OwnedFd,
    seq: u64,
}

impl fmt::Debug for ClockOverlayWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClockOverlayWriter")
            .field("seq", &self.seq)
            .finish()
    }
}

impl ClockOverlayWriter {
    /// Creates the overlay memfd, publishes an initial `FreeRunning`
    /// snapshot, then seals it: size fixed and `F_SEAL_FUTURE_WRITE`, so
    /// that nobody can obtain a new writable mapping while this process's
    /// existing writable mapping keeps working.
    pub fn create() -> Result<Self, Error> {
        let fd = memfd::create("tessera-clock", OVERLAY_LEN as u64)?;
        let map = Mapping::new(&fd, OVERLAY_LEN, true)?;
        let h = header_bytes();
        for (i, v) in h.iter().enumerate() {
            map.u64s(i * 8, 1)[0].store(*v, Ordering::Relaxed);
        }
        let mut w = Self { map, fd, seq: 0 };
        w.publish(&ClockSnapshot {
            epoch: 0,
            base_mono_ns: 0,
            base_media_ns: 0,
            rate_ppb: 0,
            state: ClockState::FreeRunning,
            gm_identity: 0,
            updated_mono_ns: 0,
        });
        memfd::add_seals(
            &w.fd,
            memfd::required_seals() | SealFlag::F_SEAL_FUTURE_WRITE,
        )?;
        Ok(w)
    }

    /// Publishes a snapshot. Wait-free, allocation-free, no syscalls.
    ///
    /// Seqlock write protocol (C11): `seq` (odd) relaxed store; release
    /// fence; relaxed stores of the fields; `seq` (even) release store.
    pub fn publish(&mut self, snap: &ClockSnapshot) {
        let m = &self.map;
        let seq = cell(m, off::SEQ);
        self.seq = self.seq.wrapping_add(1) | 1; // odd: write in progress
        seq.store(self.seq, Ordering::Relaxed);
        fence(Ordering::Release);
        cell(m, off::EPOCH).store(snap.epoch, Ordering::Relaxed);
        cell(m, off::BASE_MONO).store(snap.base_mono_ns, Ordering::Relaxed);
        cell(m, off::BASE_MEDIA).store(snap.base_media_ns, Ordering::Relaxed);
        cell(m, off::RATE_PPB).store(snap.rate_ppb as u64, Ordering::Relaxed);
        cell(m, off::STATE).store(snap.state.to_u64(), Ordering::Relaxed);
        cell(m, off::GM_IDENTITY).store(snap.gm_identity, Ordering::Relaxed);
        cell(m, off::UPDATED_MONO).store(snap.updated_mono_ns, Ordering::Relaxed);
        self.seq = self.seq.wrapping_add(1); // even: stable
        seq.store(self.seq, Ordering::Release);
    }

    /// Leaves the seqlock odd, as if the writer died mid-publish. Test aid.
    #[doc(hidden)]
    pub fn simulate_crash_mid_publish(&mut self) {
        self.seq = self.seq.wrapping_add(1) | 1;
        cell(&self.map, off::SEQ).store(self.seq, Ordering::Relaxed);
    }

    /// The descriptor to pass to clients.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Duplicates the descriptor.
    pub fn try_clone_fd(&self) -> Result<OwnedFd, Error> {
        Ok(self.fd.try_clone()?)
    }
}

/// The read-only side (clients). Holds a `PROT_READ` mapping and performs
/// only atomic loads.
pub struct ClockOverlayReader {
    map: Mapping,
}

impl fmt::Debug for ClockOverlayReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClockOverlayReader").finish_non_exhaustive()
    }
}

impl ClockOverlayReader {
    /// Attaches to an overlay memfd received from the daemon: checks size and
    /// seals, maps it read-only and validates the header. With split reads
    /// active (pointer width < 64, or the `force-split-overlay-reads` feature)
    /// the header is read with 32-bit loads too, so no 64-bit access ever
    /// touches the read-only mapping.
    pub fn attach(fd: OwnedFd) -> Result<Self, Error> {
        Self::attach_impl(fd, SPLIT_READS)
    }

    /// [`ClockOverlayReader::attach`] forcing the 32-bit header loads.
    #[doc(hidden)]
    pub fn attach_split(fd: OwnedFd) -> Result<Self, Error> {
        Self::attach_impl(fd, true)
    }

    fn attach_impl(fd: OwnedFd, split: bool) -> Result<Self, Error> {
        let size = memfd::check_attachable(&fd, OVERLAY_LEN as u64, 1 << 20)?;
        let len = usize::try_from(size).map_err(|_| LayoutError("overlay too large"))?;
        let map = Mapping::new(&fd, len, false)?;
        let read64 = |o: usize| -> u64 {
            if split {
                load64_split(&map, o)
            } else {
                map.u64s(o, 1)[0].load(Ordering::Relaxed)
            }
        };
        let h = header_bytes();
        if read64(off::MAGIC) != h[0] {
            return Err(LayoutError("bad overlay magic").into());
        }
        // major and minor are two adjacent u32 fields: plain 32-bit loads.
        let major = map.u32s(off::VERSION_MAJOR, 1)[0].load(Ordering::Relaxed);
        if major != OVERLAY_VERSION_MAJOR {
            return Err(LayoutError("unsupported overlay layout version").into());
        }
        if read64(off::TOTAL_LEN) != size || size < OVERLAY_LEN as u64 {
            return Err(LayoutError("overlay length mismatch").into());
        }
        Ok(Self { map })
    }

    /// One seqlock read attempt: `None` if the writer was mid-publish or
    /// published during the read.
    ///
    /// C11 protocol: `s1 = load_relaxed(seq)`; `fence(acquire)`; relaxed field
    /// loads; `fence(acquire)`; `s2 = load_relaxed(seq)`; valid iff `s1 == s2`
    /// and `s1` is even. (Only relaxed loads touch the read-only mapping.)
    ///
    /// On targets with pointer width below 64 bits (and when the
    /// `force-split-overlay-reads` feature is on) the loads are the 32-bit
    /// ones of [`ClockOverlayReader::try_read_split`]; the shared layout is
    /// identical either way.
    pub fn try_read(&self) -> Option<ClockSnapshot> {
        if SPLIT_READS {
            self.try_read_split()
        } else {
            self.try_read_wide()
        }
    }

    /// The seqlock read using 64-bit atomic loads.
    #[doc(hidden)]
    pub fn try_read_wide(&self) -> Option<ClockSnapshot> {
        let m = &self.map;
        let seq = cell(m, off::SEQ);
        // Relaxed + fence, not an Acquire load: the mapping is read-only, and
        // Rust only guarantees atomic loads from read-only memory for Relaxed
        // ordering. The fence gives the same synchronisation with the
        // writer's final release store.
        let s1 = seq.load(Ordering::Relaxed);
        if s1 & 1 == 1 {
            return None;
        }
        fence(Ordering::Acquire);
        let snap = ClockSnapshot {
            epoch: cell(m, off::EPOCH).load(Ordering::Relaxed),
            base_mono_ns: cell(m, off::BASE_MONO).load(Ordering::Relaxed),
            base_media_ns: cell(m, off::BASE_MEDIA).load(Ordering::Relaxed),
            rate_ppb: cell(m, off::RATE_PPB).load(Ordering::Relaxed) as i64,
            state: ClockState::from_u64(cell(m, off::STATE).load(Ordering::Relaxed)),
            gm_identity: cell(m, off::GM_IDENTITY).load(Ordering::Relaxed),
            updated_mono_ns: cell(m, off::UPDATED_MONO).load(Ordering::Relaxed),
        };
        fence(Ordering::Acquire);
        if seq.load(Ordering::Relaxed) == s1 {
            Some(snap)
        } else {
            None
        }
    }

    /// The seqlock read using only relaxed **32-bit** loads, for targets where
    /// 64-bit loads from a read-only mapping are not dependable.
    ///
    /// * Each 64-bit field is read as two 32-bit halves. A torn value (halves
    ///   from two different publishes) can only occur while a publish is in
    ///   progress or happened in between, and then `seq` differs, so the
    ///   snapshot is rejected exactly as for any other torn read.
    /// * The sequence number is read as its **low 32 bits only** (the half at
    ///   the offset of the least significant bytes, whatever the endianness).
    ///   Bit 0 is the odd/even flag and every publish changes the low word, so
    ///   a change is detected unless exactly a multiple of 2^32 phases (2^31
    ///   publishes) happen inside one reader critical section of a few loads;
    ///   at any realistic publish rate that takes weeks, so the 32-bit
    ///   sequence cannot ABA within a read.
    /// * The in-memory layout is unchanged: the writer still stores 64-bit
    ///   words. (A 32-bit access to half of a 64-bit atomic word is a
    ///   mixed-size atomic access, which the language memory model does not
    ///   formally cover; it is single-copy atomic on every supported
    ///   hardware, and it is the reason this path exists: no 64-bit access
    ///   ever touches the read-only mapping.)
    #[doc(hidden)]
    pub fn try_read_split(&self) -> Option<ClockSnapshot> {
        let m = &self.map;
        let seq_lo = |m: &Mapping| load32_of_u64(m, off::SEQ, Half::Low);
        let s1 = seq_lo(m);
        if s1 & 1 == 1 {
            return None;
        }
        fence(Ordering::Acquire);
        let snap = ClockSnapshot {
            epoch: load64_split(m, off::EPOCH),
            base_mono_ns: load64_split(m, off::BASE_MONO),
            base_media_ns: load64_split(m, off::BASE_MEDIA),
            rate_ppb: load64_split(m, off::RATE_PPB) as i64,
            state: ClockState::from_u64(load64_split(m, off::STATE)),
            gm_identity: load64_split(m, off::GM_IDENTITY),
            updated_mono_ns: load64_split(m, off::UPDATED_MONO),
        };
        fence(Ordering::Acquire);
        if seq_lo(m) == s1 { Some(snap) } else { None }
    }

    /// Reads a consistent snapshot, retrying up to `max_attempts` times.
    /// Bounded: if the writer died mid-publish this returns
    /// [`OverlayError::Unstable`] instead of spinning.
    pub fn read_with_retries(&self, max_attempts: u32) -> Result<ClockSnapshot, OverlayError> {
        for _ in 0..max_attempts.max(1) {
            if let Some(s) = self.try_read() {
                return Ok(s);
            }
            std::hint::spin_loop();
        }
        Err(OverlayError::Unstable)
    }

    /// [`ClockOverlayReader::read_with_retries`] with [`DEFAULT_RETRIES`].
    pub fn read(&self) -> Result<ClockSnapshot, OverlayError> {
        self.read_with_retries(DEFAULT_RETRIES)
    }

    /// Current media time in nanoseconds: one seqlock read plus
    /// `clock_gettime(CLOCK_MONOTONIC_RAW)`.
    pub fn now_media_ns(&self) -> Result<u64, OverlayError> {
        let s = self.read()?;
        Ok(s.media_ns_at(raw_monotonic_ns()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(k: u64) -> ClockSnapshot {
        ClockSnapshot {
            epoch: k,
            base_mono_ns: k * 3,
            base_media_ns: k * 5,
            rate_ppb: k as i64 - 50,
            state: ClockState::Locked,
            gm_identity: k * 7,
            updated_mono_ns: k * 11,
        }
    }

    fn pair() -> (ClockOverlayWriter, ClockOverlayReader) {
        let w = ClockOverlayWriter::create().unwrap();
        let r = ClockOverlayReader::attach(w.try_clone_fd().unwrap()).unwrap();
        (w, r)
    }

    #[test]
    fn publish_and_read() {
        let (mut w, r) = pair();
        assert_eq!(r.read().unwrap().state, ClockState::FreeRunning);
        for k in 1..100 {
            w.publish(&snap(k));
            assert_eq!(r.read().unwrap(), snap(k));
        }
    }

    #[test]
    fn legacy_monotonic_overlay_is_rejected_before_reading_a_raw_clock() {
        let w = ClockOverlayWriter::create().unwrap();
        w.map.u32s(off::VERSION_MAJOR, 1)[0].store(1, Ordering::Relaxed);
        let error = ClockOverlayReader::attach(w.try_clone_fd().unwrap()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported overlay layout version")
        );
    }

    #[test]
    fn now_media_ns_uses_the_raw_timebase() {
        let (mut w, r) = pair();
        let raw = raw_monotonic_ns();
        w.publish(&ClockSnapshot {
            base_mono_ns: raw,
            base_media_ns: 10_000_000_000,
            updated_mono_ns: raw,
            ..snap(0)
        });
        let media = r.now_media_ns().unwrap();
        assert!((10_000_000_000..10_050_000_000).contains(&media));
    }

    #[test]
    fn stuck_odd_seqlock_does_not_hang() {
        let (mut w, r) = pair();
        w.publish(&snap(1));
        w.simulate_crash_mid_publish();
        assert_eq!(r.read(), Err(OverlayError::Unstable));
        assert_eq!(r.try_read(), None);
        // A (restarted) writer recovers the sequence.
        w.publish(&snap(2));
        assert_eq!(r.read().unwrap(), snap(2));
    }

    #[test]
    fn split_and_wide_reads_agree_including_high_halves() {
        let (mut w, r) = pair();
        for k in [
            1u64,
            0xFFFF_FFFF,
            0x1_0000_0000,
            0x1234_5678_9ABC_DEF0,
            u64::MAX - 5,
        ] {
            let s = ClockSnapshot {
                epoch: k,
                base_mono_ns: k.rotate_left(7),
                base_media_ns: k.rotate_left(13),
                rate_ppb: (k as i64).wrapping_neg(),
                state: ClockState::Leader,
                gm_identity: k ^ 0xA5A5_A5A5_5A5A_5A5A,
                updated_mono_ns: k.rotate_left(31),
            };
            w.publish(&s);
            assert_eq!(r.try_read_wide(), Some(s));
            assert_eq!(r.try_read_split(), Some(s));
            assert_eq!(r.try_read(), Some(s));
        }
    }

    #[test]
    fn header_validation_works_with_32_bit_loads() {
        let w = ClockOverlayWriter::create().unwrap();
        assert!(ClockOverlayReader::attach_split(w.try_clone_fd().unwrap()).is_ok());
        let fd = memfd::create("t", OVERLAY_LEN as u64).unwrap();
        memfd::add_seals(&fd, memfd::required_seals()).unwrap();
        assert!(ClockOverlayReader::attach_split(fd).is_err()); // zero magic
    }

    #[test]
    fn split_read_rejects_odd_sequence_and_tracks_low_word_only() {
        let (mut w, r) = pair();
        w.publish(&snap(1));
        w.simulate_crash_mid_publish();
        assert_eq!(r.try_read_split(), None);
        assert_eq!(r.try_read_wide(), None);
        w.publish(&snap(2));
        assert_eq!(r.try_read_split(), Some(snap(2)));
    }

    #[test]
    fn media_time_math() {
        let s = ClockSnapshot {
            epoch: 1,
            base_mono_ns: 1_000,
            base_media_ns: 10_000,
            rate_ppb: 1_000_000, // +1000 ppm
            state: ClockState::Locked,
            gm_identity: 0,
            updated_mono_ns: 0,
        };
        assert_eq!(s.media_ns_at(1_000), 10_000);
        assert_eq!(s.media_ns_at(1_000 + 1_000_000_000), 10_000 + 1_001_000_000);
        // Before the anchor.
        assert_eq!(s.media_ns_at(0), 10_000 - 1_000 - 1);
        // Saturation.
        assert_eq!(s.media_ns_at(u64::MAX), u64::MAX);
        let neg = ClockSnapshot {
            base_media_ns: 0,
            ..s
        };
        assert_eq!(neg.media_ns_at(0), 0);
        assert_eq!(s.media_samples_at(1_000 + 1_000_000_000, 48_000), 48_048);
    }

    #[test]
    fn client_cannot_map_writable() {
        let w = ClockOverlayWriter::create().unwrap();
        let fd = w.try_clone_fd().unwrap();
        assert!(
            !crate::map::can_map_writable(&fd, OVERLAY_LEN),
            "writable client mapping must be refused"
        );
        // A read-only attach still works.
        assert!(ClockOverlayReader::attach(fd).is_ok());
    }

    #[test]
    fn rejects_unsealed_or_wrong_files() {
        // Unsealed memfd of the right size.
        let fd = memfd::create("t", OVERLAY_LEN as u64).unwrap();
        assert!(matches!(
            ClockOverlayReader::attach(fd),
            Err(Error::NotSealed { .. })
        ));
        // Sealed but wrong magic.
        let fd = memfd::create("t", OVERLAY_LEN as u64).unwrap();
        memfd::add_seals(&fd, memfd::required_seals()).unwrap();
        assert!(matches!(
            ClockOverlayReader::attach(fd),
            Err(Error::Layout(_))
        ));
        // Too short.
        let fd = memfd::create("t", 16).unwrap();
        memfd::add_seals(&fd, memfd::required_seals()).unwrap();
        assert!(ClockOverlayReader::attach(fd).is_err());
        // Not a memfd at all.
        let f = std::fs::File::open("/dev/null").unwrap();
        assert!(ClockOverlayReader::attach(OwnedFd::from(f)).is_err());
    }
}
