//! Media-time-addressed sample buffers (ADR 0006 and its first-wave
//! amendment).
//!
//! A [`MediaBuffer`] holds, per channel, a ring of `capacity` samples indexed
//! by `media_sample mod capacity`, plus a per-sample validity bitmap and a
//! high-water mark `hi`. The semantics are identical to the shared-memory
//! ring in `tessera-audio-ipc`, but the buffer is in-process and owned by one
//! engine thread:
//!
//! * the *window* of a channel is `[hi - capacity, hi)` (clamped at 0);
//! * position `p` is **valid** iff it lies in the window and its bit is set;
//! * every other position reads as silence, and a read classifies it as
//!   *unwritten* (`p >= hi`), *overrun* (`p < hi - capacity`, overwritten) or
//!   *hole* (inside the window, never written);
//! * writing positions older than the window is refused (counted *late*);
//! * `reset` invalidates everything (new epoch) without touching samples, so
//!   nothing ever has to be zeroed and stale audio can never be replayed.
//!
//! There is no adaptive jitter buffer: where a packet lands is decided only
//! by its media timestamp.

use alloc::vec;
use alloc::vec::Vec;

use crate::error::ConfigError;

/// Smallest permitted capacity in samples (one bitmap word).
pub const MIN_CAPACITY: usize = 64;

/// Smallest permitted power-of-two capacity that holds `frames` samples.
///
/// Saturates at the largest power of two representable in `usize`.
#[must_use]
pub fn capacity_for_frames(frames: u64) -> usize {
    let want = usize::try_from(frames)
        .unwrap_or(usize::MAX)
        .max(MIN_CAPACITY);
    want.checked_next_power_of_two()
        .unwrap_or(1 << (usize::BITS - 1))
}

/// Source of frames for a TX flow: the client side of a TX ring.
///
/// Implementations read only positions the client has actually written
/// (ADR 0006); everything else must come back as silence.
pub trait FrameSource {
    /// Fills `out` with the samples of `channel` at media positions
    /// `pos .. pos + out.len()`, writing silence (`0`) for positions that
    /// are not valid. Returns how many samples were valid.
    fn read(&mut self, channel: usize, pos: u64, out: &mut [i32]) -> usize;
}

/// Outcome of a [`MediaBuffer::write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteStats {
    /// Samples stored.
    pub written: u64,
    /// Samples refused because they are older than the window.
    pub late: u64,
    /// Samples refused because `pos + len` overflows `u64`.
    pub rejected: u64,
}

/// Outcome of a [`MediaBuffer::read`]. The four counts sum to the length of
/// the output slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadStats {
    /// Samples that were valid (copied).
    pub valid: u64,
    /// Silence inside the window: the position was never written.
    pub hole: u64,
    /// Silence at or beyond the high-water mark.
    pub unwritten: u64,
    /// Silence because the position was older than the window.
    pub overrun: u64,
}

impl ReadStats {
    /// Samples returned as silence for any reason.
    #[must_use]
    pub fn silent(&self) -> u64 {
        self.hole + self.unwritten + self.overrun
    }
}

/// Per-channel media-time rings. See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct MediaBuffer {
    channels: usize,
    cap: usize,
    mask: u64,
    words: usize,
    hi: Vec<u64>,
    bits: Vec<u64>,
    samples: Vec<i32>,
    epoch: u64,
}

impl MediaBuffer {
    /// Allocates a buffer of `channels` rings of `capacity` samples each.
    ///
    /// # Errors
    /// [`ConfigError::ZeroChannels`]; [`ConfigError::BadCapacity`] unless
    /// `capacity` is a power of two of at least [`MIN_CAPACITY`];
    /// [`ConfigError::SizeOverflow`] if the total size overflows.
    pub fn new(channels: usize, capacity: usize) -> Result<Self, ConfigError> {
        if channels == 0 {
            return Err(ConfigError::ZeroChannels);
        }
        if capacity < MIN_CAPACITY || !capacity.is_power_of_two() {
            return Err(ConfigError::BadCapacity);
        }
        let words = capacity / 64;
        let n_samples = channels
            .checked_mul(capacity)
            .ok_or(ConfigError::SizeOverflow)?;
        let n_words = channels
            .checked_mul(words)
            .ok_or(ConfigError::SizeOverflow)?;
        Ok(Self {
            channels,
            cap: capacity,
            mask: capacity as u64 - 1,
            words,
            hi: vec![0; channels],
            bits: vec![0; n_words],
            samples: vec![0; n_samples],
            epoch: 0,
        })
    }

    /// Number of channels.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Ring capacity in samples per channel.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Number of resets so far (the buffer epoch).
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// High-water mark of `channel` (exclusive end of everything written
    /// since the last reset), or 0 for an unknown channel.
    #[must_use]
    pub fn hi(&self, channel: usize) -> u64 {
        self.hi.get(channel).copied().unwrap_or(0)
    }

    /// Invalidates all content and starts a new epoch. Samples are left in
    /// place; with every bit cleared and `hi = 0` they can never be read.
    pub fn reset(&mut self) {
        self.hi.iter_mut().for_each(|h| *h = 0);
        self.bits.iter_mut().for_each(|w| *w = 0);
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Writes `src` for `channel` at media position `pos`.
    pub fn write(&mut self, channel: usize, pos: u64, src: &[i32]) -> WriteStats {
        self.write_strided(channel, pos, src, 0, 1, src.len())
    }

    /// Writes interleaved frames (`frames * channels()` samples, frame-major)
    /// to all channels starting at media position `pos`. The returned stats
    /// are per channel (every channel receives the same positions).
    pub fn write_interleaved(&mut self, pos: u64, interleaved: &[i32]) -> WriteStats {
        let frames = interleaved.len() / self.channels;
        let mut stats = WriteStats::default();
        for c in 0..self.channels {
            stats = self.write_strided(c, pos, interleaved, c, self.channels, frames);
        }
        stats
    }

    fn write_strided(
        &mut self,
        channel: usize,
        pos: u64,
        src: &[i32],
        offset: usize,
        stride: usize,
        n: usize,
    ) -> WriteStats {
        let n64 = n as u64;
        let Some(end) = pos.checked_add(n64) else {
            return WriteStats {
                rejected: n64,
                ..WriteStats::default()
            };
        };
        let cap = self.cap as u64;
        let (Some(hi), Some(bits), Some(plane)) = (
            self.hi.get_mut(channel),
            self.bits
                .get_mut(channel * self.words..(channel + 1) * self.words),
            self.samples
                .get_mut(channel * self.cap..(channel + 1) * self.cap),
        ) else {
            return WriteStats {
                rejected: n64,
                ..WriteStats::default()
            };
        };

        let old_hi = *hi;
        let new_hi = old_hi.max(end);
        if new_hi > old_hi {
            // Slots that enter the window now held previous-lap positions:
            // invalidate them before any sample is overwritten.
            let from = old_hi.max(new_hi.saturating_sub(cap));
            set_bits(bits, self.mask, from, new_hi - from, false);
            *hi = new_hi;
        }
        let floor = new_hi.saturating_sub(cap);
        let start = pos.max(floor);
        let skip = (start - pos).min(n64);
        let count = n64 - skip;
        let mask = self.mask;
        let first = offset.saturating_add((skip as usize).saturating_mul(stride));
        for (i, v) in src
            .iter()
            .skip(first)
            .step_by(stride.max(1))
            .take(count as usize)
            .enumerate()
        {
            let slot = ((start + i as u64) & mask) as usize;
            if let Some(s) = plane.get_mut(slot) {
                *s = *v;
            }
        }
        set_bits(bits, mask, start, count, true);
        WriteStats {
            written: count,
            late: skip,
            rejected: 0,
        }
    }

    /// Reads `out.len()` samples of `channel` from media position `pos`.
    /// Invalid positions are written as silence and classified in the result.
    pub fn read(&self, channel: usize, pos: u64, out: &mut [i32]) -> ReadStats {
        let mut stats = ReadStats::default();
        let (Some(&hi), Some(bits), Some(plane)) = (
            self.hi.get(channel),
            self.bits
                .get(channel * self.words..(channel + 1) * self.words),
            self.samples
                .get(channel * self.cap..(channel + 1) * self.cap),
        ) else {
            out.iter_mut().for_each(|o| *o = 0);
            stats.unwritten = out.len() as u64;
            return stats;
        };
        let floor = hi.saturating_sub(self.cap as u64);
        for (i, o) in out.iter_mut().enumerate() {
            let p = pos.saturating_add(i as u64);
            if p >= hi {
                *o = 0;
                stats.unwritten += 1;
            } else if p < floor {
                *o = 0;
                stats.overrun += 1;
            } else {
                let slot = (p & self.mask) as usize;
                let set = bits
                    .get(slot / 64)
                    .is_some_and(|w| (w >> (slot % 64)) & 1 == 1);
                match (set, plane.get(slot)) {
                    (true, Some(&v)) => {
                        *o = v;
                        stats.valid += 1;
                    }
                    _ => {
                        *o = 0;
                        stats.hole += 1;
                    }
                }
            }
        }
        stats
    }
}

impl FrameSource for MediaBuffer {
    fn read(&mut self, channel: usize, pos: u64, out: &mut [i32]) -> usize {
        usize::try_from(MediaBuffer::read(self, channel, pos, out).valid).unwrap_or(usize::MAX)
    }
}

/// Sets or clears the bits of positions `[start, start + len)` (taken modulo
/// the ring size). `len` never exceeds the ring size.
fn set_bits(bits: &mut [u64], mask: u64, start: u64, len: u64, value: bool) {
    if len == 0 {
        return;
    }
    let cap = mask + 1;
    let first = (start & mask) as usize;
    let len = len.min(cap) as usize;
    let to_end = (cap as usize) - first;
    if len <= to_end {
        set_run(bits, first, len, value);
    } else {
        set_run(bits, first, to_end, value);
        set_run(bits, 0, len - to_end, value);
    }
}

/// Sets or clears slots `[first, first + len)` (no wrap).
fn set_run(bits: &mut [u64], first: usize, len: usize, value: bool) {
    let mut slot = first;
    let end = first + len;
    while slot < end {
        let word = slot / 64;
        let lo = slot % 64;
        let take = (64 - lo).min(end - slot);
        let m = if take == 64 {
            u64::MAX
        } else {
            ((1u64 << take) - 1) << lo
        };
        if let Some(w) = bits.get_mut(word) {
            if value {
                *w |= m;
            } else {
                *w &= !m;
            }
        }
        slot += take;
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    fn buf(cap: usize) -> MediaBuffer {
        MediaBuffer::new(2, cap).unwrap()
    }

    #[test]
    fn config_validation() {
        assert_eq!(
            MediaBuffer::new(0, 64).unwrap_err(),
            ConfigError::ZeroChannels
        );
        assert_eq!(
            MediaBuffer::new(1, 100).unwrap_err(),
            ConfigError::BadCapacity
        );
        assert_eq!(
            MediaBuffer::new(1, 32).unwrap_err(),
            ConfigError::BadCapacity
        );
        assert_eq!(
            MediaBuffer::new(usize::MAX, 64).unwrap_err(),
            ConfigError::SizeOverflow
        );
        assert_eq!(capacity_for_frames(0), 64);
        assert_eq!(capacity_for_frames(3422), 4096);
        assert_eq!(capacity_for_frames(4096), 4096);
        assert_eq!(capacity_for_frames(4097), 8192);
    }

    #[test]
    fn unwritten_reads_silence() {
        let b = buf(64);
        let mut o = [7i32; 10];
        let s = b.read(0, 100, &mut o);
        assert_eq!(o, [0; 10]);
        assert_eq!(
            s,
            ReadStats {
                unwritten: 10,
                ..ReadStats::default()
            }
        );
        // Unknown channel.
        let s = b.read(9, 0, &mut o);
        assert_eq!(s.unwritten, 10);
    }

    #[test]
    fn write_then_read_and_holes() {
        let mut b = buf(64);
        b.write(0, 10, &[1, 2, 3]);
        b.write(0, 20, &[9, 9]);
        let mut o = [5i32; 14];
        let s = b.read(0, 9, &mut o);
        assert_eq!(&o[..4], &[0, 1, 2, 3]);
        assert_eq!(o[11], 9);
        assert_eq!(s.valid, 5);
        assert_eq!(s.hole, 8);
        assert_eq!(s.unwritten, 1);
        assert_eq!(s.silent(), 9);
        assert_eq!(b.hi(0), 22);
        assert_eq!(b.hi(1), 0);
    }

    #[test]
    fn stale_data_never_replays_after_a_lap() {
        let mut b = buf(64);
        let p = 1000;
        b.write(0, p, &[11; 8]);
        // Producer idles, then resumes three laps later.
        b.write(0, p + 3 * 64, &[22; 8]);
        let mut o = [1i32; 8];
        for k in 0..4u64 {
            let s = b.read(0, p + k * 64, &mut o);
            if k == 3 {
                assert_eq!(o, [22; 8]);
            } else {
                assert_eq!(o, [0; 8], "lap {k}");
                assert_eq!(s.valid, 0);
            }
        }
        // Positions equal modulo the ring but beyond hi are unwritten.
        let s = b.read(0, p + 4 * 64, &mut o);
        assert_eq!(o, [0; 8]);
        assert_eq!(s.unwritten, 8);
    }

    #[test]
    fn jump_wipes_whole_window_then_old_positions_are_overrun() {
        let mut b = buf(64);
        b.write(0, 0, &[3; 64]);
        b.write(0, 10_000, &[4; 4]);
        let mut o = [9i32; 64];
        let s = b.read(0, 0, &mut o);
        assert_eq!(o, [0; 64]);
        assert_eq!(s.overrun, 64);
        let s = b.read(0, 10_004 - 64, &mut o);
        assert_eq!(s.valid, 4);
        assert_eq!(s.hole, 60);
    }

    #[test]
    fn late_writes_are_refused() {
        let mut b = buf(64);
        b.write(0, 1000, &[1; 4]);
        let st = b.write(0, 900, &[5; 4]);
        assert_eq!(
            st,
            WriteStats {
                late: 4,
                ..WriteStats::default()
            }
        );
        // Partially late: positions below the window are dropped.
        let st = b.write(0, 1000 + 100, &[6; 10]);
        assert_eq!(st.written, 10);
        let st = b.write(0, 1000 + 100 - 60, &[7; 10]);
        assert_eq!(st.late, 6);
        assert_eq!(st.written, 4);
        assert_eq!(b.write(0, u64::MAX - 1, &[1; 4]).rejected, 4);
    }

    #[test]
    fn reset_invalidates_without_zeroing() {
        let mut b = buf(64);
        b.write(0, 5, &[8; 8]);
        b.reset();
        assert_eq!(b.epoch(), 1);
        let mut o = [1i32; 8];
        let s = b.read(0, 5, &mut o);
        assert_eq!(o, [0; 8]);
        assert_eq!(s.unwritten, 8);
        // New epoch can reuse the same positions.
        b.write(0, 5, &[2; 8]);
        b.read(0, 5, &mut o);
        assert_eq!(o, [2; 8]);
    }

    #[test]
    fn interleaved_write_deinterleaves() {
        let mut b = buf(64);
        let st = b.write_interleaved(100, &[1, 10, 2, 20, 3, 30]);
        assert_eq!(st.written, 3);
        let mut o = [0i32; 3];
        b.read(0, 100, &mut o);
        assert_eq!(o, [1, 2, 3]);
        b.read(1, 100, &mut o);
        assert_eq!(o, [10, 20, 30]);
    }

    #[test]
    fn frame_source_counts_valid_only() {
        let mut b = buf(64);
        b.write(1, 4, &[1, 2]);
        let mut o = [9i32; 6];
        let n = FrameSource::read(&mut b, 1, 3, &mut o);
        assert_eq!(n, 2);
        assert_eq!(o, [0, 1, 2, 0, 0, 0]);
    }

    #[derive(Debug, Clone)]
    enum Op {
        Write { pos: u64, data: Vec<i32> },
        Read { pos: u64, len: usize },
        Reset,
    }

    fn ops() -> impl Strategy<Value = Vec<Op>> {
        let op = prop_oneof![
            4 => (0u64..400, proptest::collection::vec(1i32..1000, 0..40)).prop_map(|(pos, data)| Op::Write { pos, data }),
            4 => (0u64..450, 0usize..80).prop_map(|(pos, len)| Op::Read { pos, len }),
            1 => Just(Op::Reset),
        ];
        proptest::collection::vec(op, 1..60)
    }

    proptest! {
        // The ring matches a straightforward model of the window semantics.
        #[test]
        fn matches_model(ops in ops()) {
            let cap = 64u64;
            let mut b = MediaBuffer::new(1, cap as usize).unwrap();
            let mut hi = 0u64;
            let mut model: HashMap<u64, i32> = HashMap::new();
            for op in ops {
                match op {
                    Op::Write { pos, data } => {
                        let end = pos + data.len() as u64;
                        hi = hi.max(end);
                        let floor = hi.saturating_sub(cap);
                        for (i, v) in data.iter().enumerate() {
                            let p = pos + i as u64;
                            if p >= floor { model.insert(p, *v); }
                        }
                        b.write(0, pos, &data);
                    }
                    Op::Reset => { hi = 0; model.clear(); b.reset(); }
                    Op::Read { pos, len } => {
                        let mut out = vec![-1i32; len];
                        let st = b.read(0, pos, &mut out);
                        let floor = hi.saturating_sub(cap);
                        let mut exp = ReadStats::default();
                        for (i, o) in out.iter().enumerate() {
                            let p = pos + i as u64;
                            let want = if p >= hi { exp.unwritten += 1; 0 }
                                else if p < floor { exp.overrun += 1; 0 }
                                else if let Some(v) = model.get(&p) { exp.valid += 1; *v }
                                else { exp.hole += 1; 0 };
                            prop_assert_eq!(*o, want, "pos {}", p);
                        }
                        prop_assert_eq!(st, exp);
                    }
                }
            }
        }
    }
}
