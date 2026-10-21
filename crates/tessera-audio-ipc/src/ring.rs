//! The media-time-addressed audio ring (see the crate docs for the
//! cross-process contract).

use std::fmt;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};

use nix::sys::stat::fstat;

use crate::error::{Error, LayoutError, RingError};
use crate::layout::{
    Limits, RING_HEADER_LEN, RingGeometry, RingParams, STATIC_HEADER_LEN, SampleFormat,
    decode_static_header, encode_static_header, off,
};
use crate::map::Mapping;
use crate::memfd;

mod sealed {
    pub trait Sealed {}
    impl Sealed for i32 {}
    impl Sealed for f32 {}
}

/// A sample type that can be stored in a ring plane: `i32` or `f32`.
pub trait Sample: Copy + sealed::Sealed {
    /// The ring format this type corresponds to.
    const FORMAT: SampleFormat;
    /// The value read for positions that carry no audio (all-zero bits).
    const SILENCE: Self;
    #[doc(hidden)]
    fn to_bits32(self) -> u32;
    #[doc(hidden)]
    fn from_bits32(bits: u32) -> Self;
}

impl Sample for i32 {
    const FORMAT: SampleFormat = SampleFormat::I32;
    const SILENCE: Self = 0;
    fn to_bits32(self) -> u32 {
        self as u32
    }
    fn from_bits32(bits: u32) -> Self {
        bits as i32
    }
}

impl Sample for f32 {
    const FORMAT: SampleFormat = SampleFormat::F32;
    const SILENCE: Self = 0.0;
    fn to_bits32(self) -> u32 {
        self.to_bits()
    }
    fn from_bits32(bits: u32) -> Self {
        f32::from_bits(bits)
    }
}

/// Logical epoch of the epoch word. The word is a seqlock-style counter: odd
/// while the producer is resetting, even otherwise. The logical epoch counts
/// completed-or-pending resets: `(seq + 1) / 2`.
#[inline]
fn logical_epoch(seq: u64) -> u64 {
    seq.wrapping_add(1) >> 1
}

/// Mask with bits `[lo, hi)` set; `0 <= lo <= hi <= 64`.
#[inline]
fn range_mask(lo: usize, hi: usize) -> u64 {
    if lo >= hi {
        return 0;
    }
    let width = hi - lo;
    let m = if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    m << lo
}

/// Clears the validity bits of `len` positions starting at `from`.
fn clear_bits(bitmap: &[AtomicU64], from: u64, len: u64, cap: u64) {
    if len >= cap {
        for w in bitmap {
            w.store(0, Ordering::Relaxed);
        }
        return;
    }
    let mut done = 0u64;
    while done < len {
        let slot = ((from + done) & (cap - 1)) as usize;
        let (w, b0) = (slot >> 6, slot & 63);
        let take = (64 - b0).min((len - done) as usize);
        if let Some(word) = bitmap.get(w) {
            let cur = word.load(Ordering::Relaxed);
            word.store(cur & !range_mask(b0, b0 + take), Ordering::Relaxed);
        }
        done += take as u64;
    }
}

/// State shared by a ring and its role handles.
struct Shared {
    map: Mapping,
    // Keeps the descriptor alive so it can be duplicated for passing.
    fd: OwnedFd,
    params: RingParams,
    geo: RingGeometry,
    cap: u64,
    owner: bool,
    producer_taken: AtomicBool,
    consumer_taken: AtomicBool,
    /// Epoch word of the last producer in this process. Process-local and
    /// therefore trusted; keeps the epoch advancing across producer
    /// hand-overs without ever reading it back from shared memory.
    local_seq: AtomicU64,
}

impl Shared {
    fn cell(&self, byte_off: usize) -> &AtomicU64 {
        // `u64s` asserts one cell in bounds, so index 0 exists.
        &self.map.u64s(byte_off, 1)[0]
    }

    fn chan_off(&self, ch: usize) -> usize {
        // Validated at construction: channels_offset + channels * stride
        // == total_len, which fits in usize.
        self.geo.channels_offset as usize + ch * self.geo.channel_stride as usize
    }

    fn chan_hi(&self, ch: usize) -> &AtomicU64 {
        self.cell(self.chan_off(ch))
    }

    fn bitmap(&self, ch: usize) -> &[AtomicU64] {
        self.map.u64s(
            self.chan_off(ch) + self.geo.bitmap_offset as usize,
            (self.cap / 64) as usize,
        )
    }

    fn plane(&self, ch: usize) -> &[std::sync::atomic::AtomicU32] {
        self.map.u32s(
            self.chan_off(ch) + self.geo.plane_offset as usize,
            self.cap as usize,
        )
    }

    fn check_channel(&self, channel: u32) -> Result<usize, RingError> {
        if channel < self.params.channels {
            Ok(channel as usize)
        } else {
            Err(RingError::ChannelOutOfRange)
        }
    }
}

/// A mapped ring. Cheap handle used to obtain the producer/consumer roles
/// and to read statistics; the roles keep the mapping alive on their own.
pub struct Ring {
    shared: Arc<Shared>,
}

impl fmt::Debug for Ring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ring")
            .field("params", &self.shared.params)
            .field("owner", &self.shared.owner)
            .finish()
    }
}

/// Snapshot of the counters and positions in the ring header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RingStats {
    /// Producer epoch (bumped by [`RingProducer::reset`]).
    pub epoch: u64,
    /// Highest end position of any completed write (hint).
    pub write_pos: u64,
    /// Frames the producer discarded because they were older than the window.
    pub late_frames: u64,
    /// Last position published by the consumer.
    pub read_pos: u64,
    /// Frames the consumer read as silence because not yet written.
    pub unwritten_frames: u64,
    /// Frames the consumer read as silence because they were lost (holes).
    pub hole_frames: u64,
    /// Frames the consumer read as silence because overwritten or flushed.
    pub overrun_frames: u64,
    /// Effective latency in frames published by the owner.
    pub effective_latency_frames: u64,
}

impl Ring {
    /// Creates a new ring (the owning, daemon side). Allocates a memfd,
    /// writes the header and seals the size.
    pub fn create(params: &RingParams, limits: &Limits) -> Result<Ring, Error> {
        let header = encode_static_header(params, limits)?;
        let geo = params.validate(limits)?;
        let len = usize::try_from(geo.total_len)
            .map_err(|_| LayoutError("ring too large for this platform"))?;
        let fd = memfd::create("tessera-ring", geo.total_len)?;
        let map = Mapping::new(&fd, len, true)?;
        let words = map.u64s(0, STATIC_HEADER_LEN / 8);
        for (cell, chunk) in words.iter().zip(header.chunks_exact(8)) {
            let mut a = [0u8; 8];
            a.copy_from_slice(chunk);
            cell.store(u64::from_ne_bytes(a), Ordering::Relaxed);
        }
        memfd::add_seals(&fd, memfd::required_seals())?;
        Ok(Ring::from_parts(map, fd, *params, geo, true))
    }

    /// Attaches to a ring received from a peer. Rejects anything that is not
    /// a sealed memfd with a valid header of exactly the right size, without
    /// reading or mapping beyond what has been checked. If `expect` is given
    /// the header parameters must equal it.
    pub fn attach(
        fd: OwnedFd,
        limits: &Limits,
        expect: Option<&RingParams>,
    ) -> Result<Ring, Error> {
        let size = memfd::check_attachable(&fd, RING_HEADER_LEN, limits.max_ring_bytes)?;
        let len =
            usize::try_from(size).map_err(|_| LayoutError("ring too large for this platform"))?;
        let map = Mapping::new(&fd, len, true)?;
        // Copy the static header once; never re-read geometry from shared
        // memory after this point.
        let mut hdr = [0u8; STATIC_HEADER_LEN];
        for (cell, chunk) in map
            .u64s(0, STATIC_HEADER_LEN / 8)
            .iter()
            .zip(hdr.chunks_exact_mut(8))
        {
            chunk.copy_from_slice(&cell.load(Ordering::Relaxed).to_ne_bytes());
        }
        let (params, geo) = decode_static_header(&hdr, size, limits)?;
        if let Some(e) = expect {
            if *e != params {
                return Err(LayoutError("ring header does not match attach parameters").into());
            }
        }
        Ok(Ring::from_parts(map, fd, params, geo, false))
    }

    fn from_parts(
        map: Mapping,
        fd: OwnedFd,
        params: RingParams,
        geo: RingGeometry,
        owner: bool,
    ) -> Ring {
        debug_assert_eq!(map.len() as u64, geo.total_len);
        Ring {
            shared: Arc::new(Shared {
                map,
                fd,
                params,
                geo,
                cap: params.capacity_frames,
                owner,
                producer_taken: AtomicBool::new(false),
                consumer_taken: AtomicBool::new(false),
                local_seq: AtomicU64::new(0),
            }),
        }
    }

    /// The validated parameters.
    pub fn params(&self) -> &RingParams {
        &self.shared.params
    }

    /// The derived geometry.
    pub fn geometry(&self) -> &RingGeometry {
        &self.shared.geo
    }

    /// The descriptor, to pass to a peer with `SCM_RIGHTS`.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.shared.fd.as_fd()
    }

    /// Duplicates the descriptor.
    pub fn try_clone_fd(&self) -> Result<OwnedFd, Error> {
        Ok(self.shared.fd.try_clone()?)
    }

    /// Takes the producer role. At most one producer handle may exist per
    /// `Ring` object in this process; the cross-process rule (one producer
    /// process per ring) is the caller's protocol.
    pub fn producer(&self) -> Result<RingProducer, Error> {
        if self.shared.producer_taken.swap(true, Ordering::AcqRel) {
            return Err(Error::AlreadyTaken);
        }
        // Nothing is taken from shared memory: a peer may have scribbled on
        // it (an RX client maps the ring read/write). Start from known-good
        // local values and publish a fresh, consistent state by resetting.
        let s = &self.shared;
        let mut producer = RingProducer {
            shared: Arc::clone(&self.shared),
            hi: vec![0u64; s.params.channels as usize].into_boxed_slice(),
            // Never go below what is already published (a second Ring object over
            // the same memfd has local_seq 0): that value only feeds equality
            // checks, hostile values merely cost the peer its own audio, and all
            // arithmetic on it wraps.
            seq: s
                .local_seq
                .load(Ordering::Acquire)
                .max(s.cell(off::EPOCH).load(Ordering::Acquire)),
            write_pos: 0,
            late: 0,
        };
        s.cell(off::LATE_FRAMES).store(0, Ordering::Relaxed);
        producer.reset();
        Ok(producer)
    }

    /// Takes the consumer role (see [`Ring::producer`]).
    pub fn consumer(&self) -> Result<RingConsumer, Error> {
        if self.shared.consumer_taken.swap(true, Ordering::AcqRel) {
            return Err(Error::AlreadyTaken);
        }
        let s = &self.shared;
        let consumer = RingConsumer {
            shared: Arc::clone(&self.shared),
            // Only used to report `epoch_changed`; any value is harmless.
            last_epoch: logical_epoch(s.cell(off::EPOCH).load(Ordering::Acquire)),
            unwritten: 0,
            hole: 0,
            overrun: 0,
        };
        // Counters are consumer-owned: start from zero, never from shared state.
        s.cell(off::UNWRITTEN_FRAMES).store(0, Ordering::Relaxed);
        s.cell(off::HOLE_FRAMES).store(0, Ordering::Relaxed);
        s.cell(off::OVERRUN_FRAMES).store(0, Ordering::Relaxed);
        Ok(consumer)
    }

    /// Publishes the effective latency (frames). Only the creating side may.
    pub fn set_effective_latency(&self, frames: u64) -> Result<(), Error> {
        if !self.shared.owner {
            return Err(Error::NotOwner);
        }
        self.shared
            .cell(off::EFFECTIVE_LATENCY)
            .store(frames, Ordering::Release);
        Ok(())
    }

    /// Reads the published effective latency in frames.
    pub fn effective_latency(&self) -> u64 {
        self.shared
            .cell(off::EFFECTIVE_LATENCY)
            .load(Ordering::Acquire)
    }

    /// A snapshot of positions and counters.
    pub fn stats(&self) -> RingStats {
        let s = &self.shared;
        let ld = |o: usize| s.cell(o).load(Ordering::Relaxed);
        RingStats {
            epoch: logical_epoch(s.cell(off::EPOCH).load(Ordering::Acquire)),
            write_pos: ld(off::WRITE_POS),
            late_frames: ld(off::LATE_FRAMES),
            read_pos: ld(off::READ_POS),
            unwritten_frames: ld(off::UNWRITTEN_FRAMES),
            hole_frames: ld(off::HOLE_FRAMES),
            overrun_frames: ld(off::OVERRUN_FRAMES),
            effective_latency_frames: self.effective_latency(),
        }
    }

    /// The size of the mapping in bytes (equals the file size).
    pub fn mapped_len(&self) -> usize {
        self.shared.map.len()
    }

    /// The current size of the underlying file (diagnostic; the seals make a
    /// change impossible, so this always equals [`Ring::mapped_len`]).
    pub fn file_len(&self) -> Result<u64, Error> {
        let st = fstat(&self.shared.fd)?;
        Ok(u64::try_from(st.st_size).unwrap_or(0))
    }
}

/// Outcome of [`RingProducer::write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteReport {
    /// Frames stored.
    pub written: usize,
    /// Frames discarded because their position was already older than the
    /// ring window (`new_hi - capacity`).
    pub late: usize,
}

/// The single writer of a ring. Not `Clone`; operations take `&mut self`.
///
/// Producer-owned state (positions, counters, epoch) is kept in local
/// memory and only *published* to the shared header, so a peer that scribbles
/// on the shared words cannot steer the producer's own bookkeeping.
pub struct RingProducer {
    shared: Arc<Shared>,
    hi: Box<[u64]>,
    /// The epoch word (even = stable, odd = reset in progress).
    seq: u64,
    write_pos: u64,
    late: u64,
}

impl fmt::Debug for RingProducer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingProducer")
            .field("params", &self.shared.params)
            .field("epoch", &logical_epoch(self.seq))
            .finish()
    }
}

impl Drop for RingProducer {
    fn drop(&mut self) {
        self.shared.local_seq.store(self.seq, Ordering::Release);
        self.shared.producer_taken.store(false, Ordering::Release);
    }
}

impl RingProducer {
    /// The ring parameters.
    pub fn params(&self) -> &RingParams {
        &self.shared.params
    }

    /// Current epoch.
    pub fn epoch(&self) -> u64 {
        logical_epoch(self.seq)
    }

    /// Highest position published for `channel` plus nothing: the exclusive
    /// end of the producer window for that channel.
    pub fn channel_hi(&self, channel: u32) -> Option<u64> {
        self.hi.get(channel as usize).copied()
    }

    /// The consumer's last published read position. Untrusted; metrics only.
    pub fn consumer_read_pos(&self) -> u64 {
        self.shared.cell(off::READ_POS).load(Ordering::Acquire)
    }

    /// Writes `data` for `channel` at media position `pos` (the position of
    /// `data[0]`). Never blocks, allocates or makes a syscall. Positions may
    /// be written out of order and with gaps; frames older than the ring
    /// window are discarded and counted in `late`.
    pub fn write<S: Sample>(
        &mut self,
        channel: u32,
        pos: u64,
        data: &[S],
    ) -> Result<WriteReport, RingError> {
        let s = &*self.shared;
        if S::FORMAT != s.params.format {
            return Err(RingError::FormatMismatch);
        }
        let ch = s.check_channel(channel)?;
        let n = data.len();
        if n == 0 {
            return Ok(WriteReport::default());
        }
        let end = pos
            .checked_add(n as u64)
            .ok_or(RingError::PositionOverflow)?;
        let cap = s.cap;
        let Some(hi_local) = self.hi.get_mut(ch) else {
            return Err(RingError::ChannelOutOfRange);
        };
        let hi_cell = s.chan_hi(ch);
        let bitmap = s.bitmap(ch);
        let plane = s.plane(ch);

        let hi_old = *hi_local;
        let new_hi = hi_old.max(end);
        let floor = new_hi.saturating_sub(cap);
        if new_hi > hi_old {
            // Invalidate the slots that now belong to new positions, so a
            // stale bit from the previous lap can never be mistaken for data.
            clear_bits(bitmap, hi_old.max(floor), new_hi - hi_old.max(floor), cap);
            *hi_local = new_hi;
            // Release: the cleared bits are visible to any reader that
            // acquires the new `hi`.
            hi_cell.store(new_hi, Ordering::Release);
            // Release fence: the sample stores below cannot be reordered
            // before the `hi` store, so a reader that observes overwritten
            // data also observes the advanced `hi` (see crate docs).
            fence(Ordering::Release);
        }

        let skip = (floor.saturating_sub(pos)).min(n as u64) as usize;
        let mut done = skip;
        while done < n {
            let p = pos + done as u64;
            let slot = (p & (cap - 1)) as usize;
            let (w, b0) = (slot >> 6, slot & 63);
            let take = (64 - b0).min(n - done);
            for (cell, v) in plane[slot..slot + take]
                .iter()
                .zip(&data[done..done + take])
            {
                cell.store(v.to_bits32(), Ordering::Relaxed);
            }
            let word = &bitmap[w];
            let cur = word.load(Ordering::Relaxed);
            // Release: publishes the samples above together with the bits.
            word.store(cur | range_mask(b0, b0 + take), Ordering::Release);
            done += take;
        }

        if skip > 0 {
            self.late = self.late.wrapping_add(skip as u64);
            s.cell(off::LATE_FRAMES).store(self.late, Ordering::Relaxed);
        }
        if end > self.write_pos {
            self.write_pos = end;
            s.cell(off::WRITE_POS).store(end, Ordering::Release);
        }
        Ok(WriteReport {
            written: n - skip,
            late: skip,
        })
    }

    /// Leaves the epoch word odd, as if the producer died mid-reset. Test aid.
    #[doc(hidden)]
    pub fn simulate_crash_mid_reset(&mut self) {
        self.seq |= 1;
        self.shared
            .cell(off::EPOCH)
            .store(self.seq, Ordering::Release);
    }

    /// Discards everything written so far and starts a new epoch. Consumers
    /// observe silence until new data is written, and see the epoch change.
    /// Use on flow loss or when the media clock epoch changes. Bounded work
    /// (clears the validity bitmaps); no allocation or syscalls.
    pub fn reset(&mut self) -> u64 {
        let s = &*self.shared;
        // Phase 1: announce (epoch word goes odd), then release-fence so that
        // any consumer that observes an effect of the clearing below also
        // observes the odd word at the end of its read and discards it.
        self.seq |= 1;
        s.cell(off::EPOCH).store(self.seq, Ordering::Relaxed);
        fence(Ordering::Release);
        for ch in 0..s.params.channels as usize {
            for w in s.bitmap(ch) {
                w.store(0, Ordering::Relaxed);
            }
            s.chan_hi(ch).store(0, Ordering::Relaxed);
        }
        self.hi.iter_mut().for_each(|h| *h = 0);
        self.write_pos = 0;
        s.cell(off::WRITE_POS).store(0, Ordering::Relaxed);
        // Phase 2: publish the new even word; a consumer that acquires it
        // sees the cleared state.
        self.seq = self.seq.wrapping_add(1);
        s.cell(off::EPOCH).store(self.seq, Ordering::Release);
        logical_epoch(self.seq)
    }
}

/// Outcome of [`RingConsumer::read`]. The five counts sum to `out.len()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadReport {
    /// Frames returned as real audio.
    pub valid: usize,
    /// Silence: position at or beyond the producer's high-water mark.
    pub unwritten: usize,
    /// Silence: position inside the window that was never written (lost).
    pub hole: usize,
    /// Silence: position older than the window (overwritten), or the whole
    /// read was flushed by an epoch change.
    pub overrun: usize,
    /// The epoch observed at the end of the read.
    pub epoch: u64,
    /// The epoch differs from the one seen by the previous read (or since
    /// the consumer was created).
    pub epoch_changed: bool,
}

/// The single reader of a ring. Reads are wait-free: no retry loops, no
/// locks, no allocation, no syscalls.
pub struct RingConsumer {
    shared: Arc<Shared>,
    last_epoch: u64,
    unwritten: u64,
    hole: u64,
    overrun: u64,
}

impl fmt::Debug for RingConsumer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingConsumer")
            .field("params", &self.shared.params)
            .field("last_epoch", &self.last_epoch)
            .finish()
    }
}

impl Drop for RingConsumer {
    fn drop(&mut self) {
        self.shared.consumer_taken.store(false, Ordering::Release);
    }
}

impl RingConsumer {
    /// The ring parameters.
    pub fn params(&self) -> &RingParams {
        &self.shared.params
    }

    /// The current epoch as published by the producer.
    pub fn epoch(&self) -> u64 {
        logical_epoch(self.shared.cell(off::EPOCH).load(Ordering::Acquire))
    }

    /// The producer's high-water mark for `channel` (untrusted hint).
    pub fn channel_hi(&self, channel: u32) -> Result<u64, RingError> {
        let ch = self.shared.check_channel(channel)?;
        Ok(self.shared.chan_hi(ch).load(Ordering::Acquire))
    }

    /// The highest end position of any completed producer write (hint).
    pub fn producer_write_pos(&self) -> u64 {
        self.shared.cell(off::WRITE_POS).load(Ordering::Acquire)
    }

    /// The effective latency in frames published by the owner.
    pub fn effective_latency(&self) -> u64 {
        self.shared
            .cell(off::EFFECTIVE_LATENCY)
            .load(Ordering::Acquire)
    }

    /// Publishes the consumer's read position (for the producer's metrics).
    pub fn publish_read_pos(&mut self, pos: u64) {
        self.shared
            .cell(off::READ_POS)
            .store(pos, Ordering::Release);
    }

    /// Reads `out.len()` frames of `channel` starting at media position
    /// `pos`. Every output element is either real audio written for exactly
    /// that position in the current epoch, or silence. Never replays stale
    /// audio.
    pub fn read<S: Sample>(
        &mut self,
        channel: u32,
        pos: u64,
        out: &mut [S],
    ) -> Result<ReadReport, RingError> {
        let s = &*self.shared;
        if S::FORMAT != s.params.format {
            return Err(RingError::FormatMismatch);
        }
        let ch = s.check_channel(channel)?;
        let n = out.len();
        pos.checked_add(n as u64)
            .ok_or(RingError::PositionOverflow)?;
        let cap = s.cap;
        let hi_cell = s.chan_hi(ch);
        let bitmap = s.bitmap(ch);
        let plane = s.plane(ch);
        let epoch_cell = s.cell(off::EPOCH);

        let seq0 = epoch_cell.load(Ordering::Acquire);
        if seq0 & 1 == 1 {
            // The producer is resetting right now: nothing is trustworthy.
            out.fill(S::SILENCE);
            let rep = ReadReport {
                overrun: n,
                ..ReadReport::default()
            };
            return Ok(self.finish(rep, seq0));
        }
        let mut rep = ReadReport::default();
        let mut done = 0usize;
        while done < n {
            let p = pos + done as u64;
            let slot = (p & (cap - 1)) as usize;
            let (w, b0) = (slot >> 6, slot & 63);
            let take = (64 - b0).min(n - done);
            let chunk = &mut out[done..done + take];
            chunk.fill(S::SILENCE);

            // Order matters: `hi` first (acquire), then the bits, then data.
            let h1 = hi_cell.load(Ordering::Acquire);
            let word = bitmap[w].load(Ordering::Acquire);
            let floor1 = h1.saturating_sub(cap);
            let lo = floor1.saturating_sub(p).min(take as u64) as usize;
            let hi_j = h1.saturating_sub(p).min(take as u64) as usize;
            let cand = (word >> b0) & range_mask(lo, hi_j);
            let mut m = cand;
            while m != 0 {
                let j = m.trailing_zeros() as usize;
                m &= m - 1;
                chunk[j] = S::from_bits32(plane[slot + j].load(Ordering::Relaxed));
            }
            // Acquire fence + reload: if the producer lapped us while we
            // copied, drop everything it may have overwritten.
            fence(Ordering::Acquire);
            let h2 = hi_cell.load(Ordering::Relaxed);
            let floor2 = h2.saturating_sub(cap);
            let lost = floor2.saturating_sub(p).min(take as u64) as usize;
            chunk[..lost].fill(S::SILENCE);
            let valid = (cand & !range_mask(0, lost)).count_ones() as usize;
            let known = hi_j.max(lost);
            rep.valid += valid;
            rep.overrun += lost;
            rep.unwritten += take - known;
            rep.hole += known - lost - valid;
            done += take;
        }

        fence(Ordering::Acquire);
        let seq1 = epoch_cell.load(Ordering::Relaxed);
        if seq1 != seq0 {
            // A reset ran concurrently: nothing read can be trusted.
            out.fill(S::SILENCE);
            rep = ReadReport {
                overrun: n,
                ..ReadReport::default()
            };
        }
        Ok(self.finish(rep, seq1))
    }

    /// Stamps the epoch into the report and updates the shared counters.
    fn finish(&mut self, mut rep: ReadReport, seq: u64) -> ReadReport {
        let s = &*self.shared;
        let epoch = logical_epoch(seq);
        rep.epoch = epoch;
        rep.epoch_changed = epoch != self.last_epoch;
        self.last_epoch = epoch;
        self.unwritten = self.unwritten.wrapping_add(rep.unwritten as u64);
        self.hole = self.hole.wrapping_add(rep.hole as u64);
        self.overrun = self.overrun.wrapping_add(rep.overrun as u64);
        s.cell(off::UNWRITTEN_FRAMES)
            .store(self.unwritten, Ordering::Relaxed);
        s.cell(off::HOLE_FRAMES).store(self.hole, Ordering::Relaxed);
        s.cell(off::OVERRUN_FRAMES)
            .store(self.overrun, Ordering::Relaxed);
        rep
    }
}
