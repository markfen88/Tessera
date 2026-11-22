//! Per-channel gain and mute, shared between a control thread and the RT
//! audio thread (groundwork for hardware fader and mute-button control).
//!
//! The control side writes a [`ChannelGain`]; the RT side takes one atomic
//! load per buffer ([`ChannelGain::load`]) and applies the resulting
//! [`GainWord`] to the `i32` left-aligned samples the flow engines use
//! (see [`crate::convert`]). Not wired into `rx`/`tx` yet.
//!
//! # Word format
//!
//! One `u32` per channel:
//!
//! * bit 31: mute flag ([`GainWord::MUTE_BIT`]);
//! * bits 0..=30: linear gain, unsigned fixed point **Q7.24** (unity is
//!   exactly `1 << 24`, 0 is −inf dB). The gain is clamped to
//!   [`MAX_Q24`] (+12 dB, linear ≈ 3.981), well inside the 31-bit field.
//!
//! Mute is a separate bit so muting keeps the stored gain and unmuting
//! restores it. Gain and mute live in one word, so one load always sees a
//! consistent pair.
//!
//! # Ordering
//!
//! All accesses are `Relaxed`. The word is self-contained: it publishes no
//! other memory, so there is nothing for a Release/Acquire pair to order.
//! Read-modify-writes on one atomic are totally ordered by coherence, so a
//! concurrent `set_muted` and `set_linear` cannot lose each other's bits. A
//! new value reaches the RT thread "soon" (next load), which is all fader
//! control needs.
//!
//! # RT side
//!
//! [`GainWord::apply_i32`] and friends never allocate, lock, make syscalls
//! or log. Unity is a bit-exact no-op, mute fills zeros, anything else
//! multiplies with rounding and saturates to the `i32` range. For click
//! avoidance the caller keeps the previous word and uses
//! [`GainWord::ramp_i32`] (or [`ChannelGain::apply_ramped_i32`]), which
//! moves linearly from the previous to the new gain across one buffer.
//!
//! # Features
//!
//! dB conversion ([`ChannelGain::set_db`], [`ChannelGain::db`],
//! [`db_to_linear`], [`GainWord::db`]) needs `std` (`powf`/`log10` are not
//! in `core`, and this crate does not depend on `libm`). Linear and raw
//! Q7.24 setters work in bare `core`. [`ChannelGains`] needs `alloc`.

#![deny(clippy::indexing_slicing)]

use core::sync::atomic::{AtomicU32, Ordering};

/// Number of fractional bits of the gain field.
pub const FRAC_BITS: u32 = 24;
/// Unity gain (0 dB) in Q7.24.
pub const UNITY_Q24: u32 = 1 << FRAC_BITS;
/// Highest settable gain, +12 dB (`round(10^(12/20) * 2^24)`).
pub const MAX_Q24: u32 = 66_791_300;
/// Highest settable gain in dB.
pub const MAX_DB: f32 = 12.0;
/// Lowest non-silent gain in dB; anything below it is stored as 0 (−inf dB).
pub const MIN_DB: f32 = -96.0;
/// Highest settable linear gain (`10^(12/20)`).
pub const MAX_LINEAR: f32 = 3.981_071_7;

const SCALE: f64 = UNITY_Q24 as f64;
const ROUND: i64 = 1 << (FRAC_BITS - 1);
/// Extra fractional bits of the ramp accumulator.
const RAMP_BITS: u32 = 16;

/// A snapshot of one channel's gain and mute, as loaded by the RT thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GainWord(u32);

impl Default for GainWord {
    fn default() -> Self {
        Self::UNITY
    }
}

impl GainWord {
    /// The mute flag.
    pub const MUTE_BIT: u32 = 1 << 31;
    /// Unity gain, unmuted.
    pub const UNITY: GainWord = GainWord(UNITY_Q24);

    /// Builds a word from a Q7.24 gain (clamped to [`MAX_Q24`]) and a mute
    /// flag.
    #[must_use]
    pub const fn new(q24: u32, muted: bool) -> Self {
        let q = if q24 > MAX_Q24 { MAX_Q24 } else { q24 };
        GainWord(if muted { q | Self::MUTE_BIT } else { q })
    }

    /// The raw word (mute bit plus Q7.24 gain).
    #[must_use]
    pub const fn to_bits(self) -> u32 {
        self.0
    }

    /// The stored gain in Q7.24, regardless of mute.
    #[must_use]
    pub const fn q24(self) -> u32 {
        self.0 & !Self::MUTE_BIT
    }

    /// Whether the mute flag is set.
    #[must_use]
    pub const fn is_muted(self) -> bool {
        self.0 & Self::MUTE_BIT != 0
    }

    /// The gain actually applied: 0 when muted, else [`q24`](Self::q24).
    #[must_use]
    pub const fn effective_q24(self) -> u32 {
        if self.is_muted() { 0 } else { self.q24() }
    }

    /// Whether applying this word leaves samples untouched.
    #[must_use]
    pub const fn is_unity(self) -> bool {
        self.0 == UNITY_Q24
    }

    /// The stored gain as a linear factor, regardless of mute.
    #[must_use]
    pub fn linear(self) -> f32 {
        (f64::from(self.q24()) / SCALE) as f32
    }

    /// The stored gain in dB, regardless of mute (−inf for 0).
    #[cfg(feature = "std")]
    #[must_use]
    pub fn db(self) -> f32 {
        match self.q24() {
            0 => f32::NEG_INFINITY,
            q => (20.0 * (f64::from(q) / SCALE).log10()) as f32,
        }
    }

    /// Applies this gain to a contiguous run of samples (one channel's
    /// plane). RT-safe.
    #[inline]
    pub fn apply_i32(self, samples: &mut [i32]) {
        apply_iter(self, samples.iter_mut());
    }

    /// Applies this gain to `channel` of an interleaved buffer with
    /// `channels` samples per frame. No-op if `channel >= channels`
    /// (including `channels == 0`). RT-safe.
    #[inline]
    pub fn apply_strided_i32(self, interleaved: &mut [i32], channels: usize, channel: usize) {
        if channel >= channels {
            return;
        }
        let lane = interleaved.iter_mut().skip(channel).step_by(channels);
        apply_iter(self, lane);
    }

    /// Applies a linear ramp from `from`'s effective gain to `to`'s across
    /// `samples`: the last sample gets exactly `to`'s gain, the first one a
    /// single step past `from`'s (so back-to-back buffers join without a
    /// repeated or skipped step). Equal effective gains degrade to
    /// [`apply_i32`](Self::apply_i32). RT-safe.
    #[inline]
    pub fn ramp_i32(from: GainWord, to: GainWord, samples: &mut [i32]) {
        let n = samples.len();
        ramp_iter(from, to, n, samples.iter_mut());
    }

    /// [`ramp_i32`](Self::ramp_i32) over `channel` of an interleaved buffer
    /// with `channels` samples per frame. No-op if `channel >= channels`
    /// (including `channels == 0`).
    #[inline]
    pub fn ramp_strided_i32(
        from: GainWord,
        to: GainWord,
        interleaved: &mut [i32],
        channels: usize,
        channel: usize,
    ) {
        if channel >= channels {
            return;
        }
        let n = interleaved.len().saturating_sub(channel).div_ceil(channels);
        let lane = interleaved.iter_mut().skip(channel).step_by(channels);
        ramp_iter(from, to, n, lane);
    }
}

#[inline]
fn mul(s: i32, q: u32) -> i32 {
    let p = (i64::from(s) * i64::from(q) + ROUND) >> FRAC_BITS;
    p.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[inline]
fn apply_iter<'a>(w: GainWord, it: impl Iterator<Item = &'a mut i32>) {
    if w.is_unity() {
        return;
    }
    let q = w.effective_q24();
    if q == 0 {
        it.for_each(|s| *s = 0);
    } else {
        it.for_each(|s| *s = mul(*s, q));
    }
}

#[inline]
fn ramp_iter<'a>(from: GainWord, to: GainWord, n: usize, it: impl Iterator<Item = &'a mut i32>) {
    let f = from.effective_q24();
    let t = to.effective_q24();
    if f == t || n == 0 {
        apply_iter(GainWord::new(t, false), it);
        return;
    }
    // Q7.24 widened by RAMP_BITS: |t - f| < 2^27, so the step and the
    // accumulator stay far inside i64.
    let start = i64::from(f) << RAMP_BITS;
    let end = i64::from(t) << RAMP_BITS;
    let step = (end - start) / n as i64;
    let mut acc = start;
    let last = n - 1;
    for (i, s) in it.enumerate() {
        let g = if i >= last {
            t
        } else {
            acc += step;
            (acc >> RAMP_BITS) as u32
        };
        *s = mul(*s, g);
    }
}

/// Converts dB to a linear factor with the module's clamping: above
/// [`MAX_DB`] → [`MAX_LINEAR`], below [`MIN_DB`] (or NaN) → 0.
#[cfg(feature = "std")]
#[must_use]
pub fn db_to_linear(db: f32) -> f32 {
    if db.is_nan() || db < MIN_DB {
        0.0
    } else if db >= MAX_DB {
        MAX_LINEAR
    } else {
        10f64.powf(f64::from(db) / 20.0) as f32
    }
}

fn linear_to_q24(g: f32) -> u32 {
    if g.is_nan() || g <= 0.0 {
        0
    } else if g >= MAX_LINEAR {
        MAX_Q24
    } else {
        ((f64::from(g) * SCALE + 0.5) as u32).min(MAX_Q24)
    }
}

/// One channel's gain and mute, written by the control thread and read by
/// the RT thread.
#[derive(Debug)]
pub struct ChannelGain(AtomicU32);

impl Default for ChannelGain {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelGain {
    /// Unity gain, unmuted.
    #[must_use]
    pub const fn new() -> Self {
        ChannelGain(AtomicU32::new(UNITY_Q24))
    }

    /// RT side: one relaxed atomic load.
    #[inline]
    #[must_use]
    pub fn load(&self) -> GainWord {
        GainWord(self.0.load(Ordering::Relaxed))
    }

    /// Sets the Q7.24 gain (clamped to [`MAX_Q24`]), keeping the mute flag.
    pub fn set_q24(&self, q24: u32) {
        let q = q24.min(MAX_Q24);
        let mut cur = self.0.load(Ordering::Relaxed);
        loop {
            let new = (cur & GainWord::MUTE_BIT) | q;
            match self
                .0
                .compare_exchange_weak(cur, new, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Sets a linear gain, clamped to `0..=MAX_LINEAR` (NaN → 0), keeping
    /// the mute flag.
    pub fn set_linear(&self, gain: f32) {
        self.set_q24(linear_to_q24(gain));
    }

    /// Sets the gain in dB, clamped to `MIN_DB..=MAX_DB`; below
    /// [`MIN_DB`] (including −inf and NaN) stores 0. Keeps the mute flag.
    #[cfg(feature = "std")]
    pub fn set_db(&self, db: f32) {
        self.set_linear(db_to_linear(db));
    }

    /// Sets or clears mute; the stored gain is kept.
    pub fn set_muted(&self, muted: bool) {
        if muted {
            self.0.fetch_or(GainWord::MUTE_BIT, Ordering::Relaxed);
        } else {
            self.0.fetch_and(!GainWord::MUTE_BIT, Ordering::Relaxed);
        }
    }

    /// Whether the channel is muted.
    #[must_use]
    pub fn muted(&self) -> bool {
        self.load().is_muted()
    }

    /// Stored linear gain (regardless of mute).
    #[must_use]
    pub fn linear(&self) -> f32 {
        self.load().linear()
    }

    /// Stored gain in dB (regardless of mute; −inf for 0).
    #[cfg(feature = "std")]
    #[must_use]
    pub fn db(&self) -> f32 {
        self.load().db()
    }

    /// RT side: loads and applies to a contiguous run.
    #[inline]
    pub fn apply_i32(&self, samples: &mut [i32]) {
        self.load().apply_i32(samples);
    }

    /// RT side: loads, ramps from `*prev` to the new word across `samples`
    /// (plain apply if unchanged), and stores the new word in `*prev`. The
    /// caller owns `prev` (one per channel, initially
    /// [`GainWord::UNITY`] or the first load).
    #[inline]
    pub fn apply_ramped_i32(&self, prev: &mut GainWord, samples: &mut [i32]) {
        if samples.is_empty() {
            return; // keep `prev`: the ramp happens on the next block
        }
        let now = self.load();
        GainWord::ramp_i32(*prev, now, samples);
        *prev = now;
    }

    /// [`apply_ramped_i32`](Self::apply_ramped_i32) over `channel` of an
    /// interleaved buffer with `channels` samples per frame.
    #[inline]
    pub fn apply_ramped_strided_i32(
        &self,
        prev: &mut GainWord,
        interleaved: &mut [i32],
        channels: usize,
        channel: usize,
    ) {
        if channel >= channels || interleaved.len() <= channel {
            return; // keep `prev`: the ramp happens on the next block
        }
        let now = self.load();
        GainWord::ramp_strided_i32(*prev, now, interleaved, channels, channel);
        *prev = now;
    }
}

/// A fixed table of per-channel gains, sized at construction.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub struct ChannelGains(alloc::boxed::Box<[ChannelGain]>);

#[cfg(feature = "alloc")]
impl ChannelGains {
    /// `channels` entries, all unity and unmuted (control thread only:
    /// allocates).
    #[must_use]
    pub fn new(channels: usize) -> Self {
        ChannelGains((0..channels).map(|_| ChannelGain::new()).collect())
    }

    /// The gain of channel `ch`, if it exists.
    #[inline]
    #[must_use]
    pub fn get(&self, ch: usize) -> Option<&ChannelGain> {
        self.0.get(ch)
    }

    /// Number of channels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the table has no channels.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// All channels in order.
    pub fn iter(&self) -> core::slice::Iter<'_, ChannelGain> {
        self.0.iter()
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    const FS: [i32; 6] = [i32::MAX, i32::MIN, 0, 1, -1, 0x1234_5600];

    #[test]
    fn unity_is_bit_exact() {
        let g = ChannelGain::new();
        assert!(g.load().is_unity());
        let mut s = FS;
        g.apply_i32(&mut s);
        assert_eq!(s, FS);
        g.set_linear(1.0);
        assert_eq!(g.load(), GainWord::UNITY);
    }

    #[test]
    fn mute_zeros_and_unmute_restores() {
        let g = ChannelGain::new();
        g.set_linear(0.5);
        g.set_muted(true);
        assert!(g.muted());
        let mut s = FS;
        g.apply_i32(&mut s);
        assert_eq!(s, [0; 6]);
        // Gain changes while muted are kept.
        g.set_linear(0.25);
        assert!(g.muted());
        g.set_muted(false);
        assert!(!g.muted());
        assert_eq!(g.load().q24(), UNITY_Q24 / 4);
        let mut s = [4000, -4000];
        g.apply_i32(&mut s);
        assert_eq!(s, [1000, -1000]);
    }

    #[cfg(feature = "std")]
    #[test]
    fn db_round_trip() {
        let g = ChannelGain::new();
        g.set_db(0.0);
        assert_eq!(g.load().q24(), UNITY_Q24);
        for db in [-96.0f32, -60.0, -20.0, -6.0, -0.1, 0.1, 6.0, 11.9, 12.0] {
            g.set_db(db);
            let back = g.db();
            let tol = if db < -80.0 { 0.05 } else { 1e-3 };
            assert!((back - db).abs() < tol, "{db} -> {back}");
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn db_clamping() {
        let g = ChannelGain::new();
        g.set_db(40.0);
        assert_eq!(g.load().q24(), MAX_Q24);
        g.set_db(f32::INFINITY);
        assert_eq!(g.load().q24(), MAX_Q24);
        g.set_db(-96.1);
        assert_eq!(g.load().q24(), 0);
        assert_eq!(g.db(), f32::NEG_INFINITY);
        g.set_db(f32::NAN);
        assert_eq!(g.load().q24(), 0);
        g.set_db(-96.0);
        assert!(g.load().q24() > 0);
    }

    #[test]
    fn linear_clamping() {
        let g = ChannelGain::new();
        g.set_linear(100.0);
        assert_eq!(g.load().q24(), MAX_Q24);
        g.set_linear(-1.0);
        assert_eq!(g.load().q24(), 0);
        g.set_linear(f32::NAN);
        assert_eq!(g.load().q24(), 0);
        g.set_q24(u32::MAX);
        assert_eq!(g.load().q24(), MAX_Q24);
        assert!(!g.muted());
        assert_eq!(GainWord::new(u32::MAX, true).q24(), MAX_Q24);
    }

    #[test]
    fn saturates_at_plus_12_db() {
        let g = ChannelGain::new();
        g.set_q24(MAX_Q24);
        let mut s = [i32::MAX, i32::MIN, 1 << 30, -(1 << 30), 1000];
        g.apply_i32(&mut s);
        assert_eq!(s[0], i32::MAX);
        assert_eq!(s[1], i32::MIN);
        assert_eq!(s[2], i32::MAX); // 2^30 * 3.98 > 2^31
        assert_eq!(s[3], i32::MIN);
        assert_eq!(s[4], 3981);
    }

    #[test]
    fn strided_touches_only_its_channel() {
        let w = GainWord::new(UNITY_Q24 / 2, false);
        let mut s = [10, 20, 30, 40, 50, 60];
        w.apply_strided_i32(&mut s, 3, 1);
        assert_eq!(s, [10, 10, 30, 40, 25, 60]);
        GainWord::new(0, true).apply_strided_i32(&mut s, 3, 2);
        assert_eq!(s, [10, 10, 0, 40, 25, 0]);
        w.apply_strided_i32(&mut s, 0, 0); // no panic, no-op
        assert_eq!(s, [10, 10, 0, 40, 25, 0]);
        // A channel past the frame must not wrap onto channel 0.
        w.apply_strided_i32(&mut s, 3, 3);
        GainWord::ramp_strided_i32(GainWord::UNITY, w, &mut s, 3, 5);
        assert_eq!(s, [10, 10, 0, 40, 25, 0]);
    }

    #[test]
    fn empty_block_keeps_ramp_start() {
        let g = ChannelGain::new();
        g.set_muted(true);
        let mut prev = GainWord::UNITY;
        g.apply_ramped_i32(&mut prev, &mut []);
        assert_eq!(
            prev,
            GainWord::UNITY,
            "the ramp still happens on the next block"
        );
    }

    #[test]
    fn ramp_endpoints() {
        let from = GainWord::UNITY;
        let to = GainWord::new(0, true);
        let x = 1 << 24;
        let mut s = [x; 8];
        GainWord::ramp_i32(from, to, &mut s);
        assert_eq!(s[7], 0, "last sample gets the target exactly");
        assert_eq!(s[0], x - x / 8, "first sample is one step past from");
        assert!(s.windows(2).all(|p| p[0] > p[1]), "monotonic: {s:?}");

        // Up-ramp ends exactly on the target too.
        let mut s = [x; 5];
        GainWord::ramp_i32(
            GainWord::new(0, false),
            GainWord::new(UNITY_Q24 * 2, false),
            &mut s,
        );
        assert_eq!(s[4], 2 * x);
        assert!(s.windows(2).all(|p| p[0] < p[1]));

        // Equal effective gain: plain apply (unity stays bit-exact).
        let mut s = FS;
        GainWord::ramp_i32(GainWord::UNITY, GainWord::UNITY, &mut s);
        assert_eq!(s, FS);
        GainWord::ramp_i32(GainWord::UNITY, GainWord::UNITY, &mut []);
    }

    #[test]
    fn ramped_apply_tracks_prev() {
        let g = ChannelGain::new();
        let mut prev = g.load();
        g.set_muted(true);
        let mut s = [1 << 20; 4];
        g.apply_ramped_i32(&mut prev, &mut s);
        assert_eq!(s[3], 0);
        assert!(prev.is_muted());
        let mut s = [1 << 20; 4];
        g.apply_ramped_i32(&mut prev, &mut s);
        assert_eq!(s, [0; 4]);

        // Strided ramp hits the last frame of its channel exactly.
        g.set_muted(false);
        let mut s = [1 << 20; 9];
        g.apply_ramped_strided_i32(&mut prev, &mut s, 3, 2);
        assert_eq!(s[8], 1 << 20);
        assert_eq!(s[2], (1 << 20) / 3);
        assert_eq!(s[0], 1 << 20);
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn table() {
        let t = ChannelGains::new(4);
        assert_eq!(t.len(), 4);
        assert!(t.iter().all(|g| g.load().is_unity()));
        t.get(2).unwrap().set_muted(true);
        assert!(t.get(2).unwrap().muted());
        assert!(!t.get(1).unwrap().muted());
        assert!(t.get(4).is_none());
        assert!(ChannelGains::new(0).is_empty());
    }
}
