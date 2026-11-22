//! Learning a control's place in a HID input report.
//!
//! The portal's "Learn" asks the user to move one control. [`Learner`] is
//! fed the raw reports the device sends meanwhile and finds the first
//! control that moved relative to a baseline:
//!
//! * a single bit that toggles is a **button** (`active_low` if it rests
//!   at 1);
//! * a contiguous little-endian bit range that takes several values is a
//!   **fader**: offset and width come from the bits that changed, rounded
//!   out to a plausible field (nibble boundaries, at least 8 bits, then 8,
//!   12, 16, 24 or 32 bits), `max` is the largest value seen, and the fader
//!   is `inverted` if its value went down while the user was moving it up.
//!
//! Reports the caller knows to be idle (the device's state read on open,
//! and what arrives while the user has been asked not to touch anything)
//! go to [`Learner::idle`]. They set the baseline and teach the learner the
//! device's own noise: bits that change in **every** idle report (a
//! counter's low bit) are ignored, together with the more significant bits
//! of their byte (the rest of the counter); bits that change only now and
//! then (a fader's converter jitter) cannot start a detection on their own
//! but still belong to a field that moves. Without idle reports, the first
//! fed report of each report id is the baseline.
//!
//! The layout is a suggestion: the portal shows it and the user saves it as
//! a [`SurfaceControl`](tessera_control_types::SurfaceControl).

use std::collections::BTreeMap;

use tessera_control_types::{ControlKind, LearnedControl};

use crate::config::MAX_FIELD_BITS;
use crate::decode::extract;

/// Reports kept after a control started moving (enough for several seconds
/// at a 1 kHz report rate). Later reports still widen the field.
const MAX_SAMPLES: usize = 8192;

/// Plausible fader field widths, bits.
const WIDTHS: [u32; 5] = [8, 12, 16, 24, 32];

/// Per report id.
#[derive(Debug)]
struct Track {
    baseline: Vec<u8>,
    /// The baseline is a state known to be at rest.
    from_idle: bool,
    last: Vec<u8>,
    /// Consecutive idle report pairs seen.
    idle_pairs: usize,
    /// Bits that changed between idle reports at least once.
    ever: Vec<u8>,
    /// Bits that changed between every pair of idle reports.
    always: Vec<u8>,
}

impl Track {
    fn new(data: &[u8], from_idle: bool) -> Self {
        Self {
            baseline: data.to_vec(),
            from_idle,
            last: data.to_vec(),
            idle_pairs: 0,
            ever: vec![0; data.len()],
            always: vec![0xff; data.len()],
        }
    }

    /// Bits ignored entirely: counters (see the module documentation).
    /// Needs two idle pairs to tell "every report" from "once".
    fn noise(&self, i: usize) -> u8 {
        if self.idle_pairs < 2 {
            return 0;
        }
        let a = self.always.get(i).copied().unwrap_or(0);
        if a == 0 {
            0
        } else {
            // The lowest always-changing bit and every bit above it.
            !((a & a.wrapping_neg()) - 1)
        }
    }

    /// Bits that changed while idle but are no counter: they cannot start
    /// a detection.
    fn jitter(&self, i: usize) -> u8 {
        self.ever.get(i).copied().unwrap_or(0) & !self.noise(i)
    }
}

#[derive(Debug)]
struct Moved {
    report_id: Option<u8>,
    /// The first bit that changed significantly.
    anchor: u32,
    /// Bits seen changing since (noise excluded).
    union: Vec<u8>,
    /// The baseline, then every report since the control started moving.
    samples: Vec<Vec<u8>>,
    from_idle: bool,
}

/// Finds the first control that moves in a stream of HID input reports.
#[derive(Debug)]
pub struct Learner {
    numbered: bool,
    moving_up: bool,
    idle_left: usize,
    tracks: BTreeMap<Option<u8>, Track>,
    moved: Option<Moved>,
}

fn bit(data: &[u8], pos: u32) -> bool {
    usize::try_from(pos / 8)
        .ok()
        .and_then(|i| data.get(i))
        .is_some_and(|b| (b >> (pos % 8)) & 1 == 1)
}

impl Learner {
    /// A learner for a device whose reports start with a report id byte
    /// (`numbered`) or not, as `read()` on `/dev/hidraw*` returns them.
    /// `moving_up`: the user was asked to move a fader up, so a value that
    /// goes down means the fader is inverted (`false`: no hint, never
    /// inverted).
    #[must_use]
    pub fn new(numbered: bool, moving_up: bool) -> Self {
        Self {
            numbered,
            moving_up,
            idle_left: 0,
            tracks: BTreeMap::new(),
            moved: None,
        }
    }

    /// Treat the first `n` reports given to [`push`](Self::push) as idle.
    #[must_use]
    pub fn with_idle_reports(mut self, n: usize) -> Self {
        self.idle_left = n;
        self
    }

    fn split<'r>(&self, report: &'r [u8]) -> Option<(Option<u8>, &'r [u8])> {
        if self.numbered {
            report.split_first().map(|(id, rest)| (Some(*id), rest))
        } else {
            Some((None, report))
        }
    }

    /// A report sent while nothing was being touched: baseline and noise.
    pub fn idle(&mut self, report: &[u8]) {
        let Some((id, data)) = self.split(report) else {
            return;
        };
        let Some(t) = self.tracks.get_mut(&id) else {
            self.tracks.insert(id, Track::new(data, true));
            return;
        };
        for (((d, l), ever), always) in data.iter().zip(&t.last).zip(&mut t.ever).zip(&mut t.always)
        {
            let changed = d ^ l;
            *ever |= changed;
            *always &= changed;
        }
        t.idle_pairs += 1;
        t.last = data.to_vec();
        // The latest idle state is the reference the movement starts from.
        t.baseline = data.to_vec();
        t.from_idle = true;
    }

    /// A report while (or after) the user moves the control. Returns the
    /// result once it is final: a button that was pressed and released
    /// (after idle reports). A fader is never final on its own, since the
    /// learner cannot know the user has finished moving it; read
    /// [`result`](Self::result) when the caller's time is up.
    pub fn feed(&mut self, report: &[u8]) -> Option<LearnedControl> {
        let (id, data) = self.split(report)?;
        let Some(t) = self.tracks.get_mut(&id) else {
            self.tracks.insert(id, Track::new(data, false));
            return None;
        };
        if self.moved.as_ref().is_some_and(|m| m.report_id != id) {
            t.last = data.to_vec();
            return None;
        }
        let n = data.len().min(t.baseline.len()).min(t.last.len());
        let mut changed = vec![0u8; n];
        let mut significant = None;
        for (i, c) in changed.iter_mut().enumerate() {
            let noise = t.noise(i);
            *c = ((data[i] ^ t.baseline[i]) | (data[i] ^ t.last[i])) & !noise;
            let s = (data[i] ^ t.baseline[i]) & !noise & !t.jitter(i);
            if significant.is_none() && s != 0 {
                let i = u32::try_from(i).ok()?;
                significant = Some(i * 8 + s.trailing_zeros());
            }
        }
        t.last = data.to_vec();
        match &mut self.moved {
            None => {
                let anchor = significant?;
                self.moved = Some(Moved {
                    report_id: id,
                    anchor,
                    union: changed,
                    samples: vec![t.baseline.clone(), data.to_vec()],
                    from_idle: t.from_idle,
                });
            }
            Some(m) => {
                if m.union.len() < changed.len() {
                    m.union.resize(changed.len(), 0);
                }
                for (u, c) in m.union.iter_mut().zip(&changed) {
                    *u |= c;
                }
                if m.samples.len() < MAX_SAMPLES {
                    m.samples.push(data.to_vec());
                }
            }
        }
        let r = self.result()?;
        let m = self.moved.as_ref()?;
        // Final only if nothing but that one bit ever changed: a fader's
        // low bit also goes back and forth.
        let one_bit = m.union.iter().map(|b| b.count_ones()).sum::<u32>() == 1;
        let released = r.kind == ControlKind::Button
            && one_bit
            && m.from_idle
            && m.samples
                .first()
                .is_some_and(|b| bit(b, r.offset) == bit(data, r.offset));
        released.then_some(r)
    }

    /// [`idle`](Self::idle) for the first reports (see
    /// [`with_idle_reports`](Self::with_idle_reports)), then
    /// [`feed`](Self::feed).
    pub fn push(&mut self, report: &[u8]) -> Option<LearnedControl> {
        if self.idle_left > 0 {
            self.idle_left -= 1;
            self.idle(report);
            return None;
        }
        self.feed(report)
    }

    /// The control found so far; `None` while nothing has moved.
    #[must_use]
    pub fn result(&self) -> Option<LearnedControl> {
        let m = self.moved.as_ref()?;
        let len_bits = u32::try_from(m.union.len()).ok()?.checked_mul(8)?;
        let set = |pos: u32| bit(&m.union, pos);
        let (mut lo, mut hi) = (m.anchor, m.anchor);
        while lo > 0 && set(lo - 1) {
            lo -= 1;
        }
        while hi + 1 < len_bits && set(hi + 1) {
            hi += 1;
        }
        if hi > lo {
            if let Some((offset, bits)) = field(lo, hi, len_bits) {
                let values: Vec<u32> = m
                    .samples
                    .iter()
                    .filter_map(|s| extract(s, offset, bits))
                    .collect();
                let mut distinct = values.clone();
                distinct.sort_unstable();
                distinct.dedup();
                if distinct.len() >= 3 {
                    let first = values.first().copied().unwrap_or(0);
                    let last = values.last().copied().unwrap_or(0);
                    let mut fader =
                        LearnedControl::new(ControlKind::Fader, m.report_id, offset, bits);
                    fader.max = distinct.last().copied();
                    fader.inverted = self.moving_up && last < first;
                    return Some(fader);
                }
            }
        }
        // A button: the bit that moved first. It rests where the idle
        // baseline had it, or (without one) where it ended.
        let rest_in = if m.from_idle {
            m.samples.first()
        } else {
            m.samples.last()
        };
        let mut button = LearnedControl::new(ControlKind::Button, m.report_id, m.anchor, 1);
        button.active_low = rest_in.is_some_and(|s| bit(s, m.anchor));
        Some(button)
    }
}

/// The field around changed bits `lo..=hi`, rounded out to nibble
/// boundaries and a plausible width that fits in the report.
fn field(lo: u32, hi: u32, len_bits: u32) -> Option<(u32, u8)> {
    let offset = lo & !3;
    let end = (hi + 1).div_ceil(4) * 4;
    let need = end - offset;
    let room = len_bits.checked_sub(offset)?;
    let bits = WIDTHS
        .iter()
        .copied()
        .find(|&w| w >= need && w <= room)
        .unwrap_or_else(|| need.min(room))
        .min(u32::from(MAX_FIELD_BITS));
    if bits == 0 || offset + bits <= hi {
        return None;
    }
    Some((offset, u8::try_from(bits).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 6-byte unnumbered report: byte 0 a free-running counter (when
    /// `counter` is set), the rest as given.
    fn report(counter: Option<u8>, rest: [u8; 5]) -> Vec<u8> {
        let mut r = vec![counter.unwrap_or(0)];
        r.extend(rest);
        r
    }

    /// Writes `value` as a `bits`-wide little-endian field at bit `offset`.
    fn put(data: &mut [u8], offset: u32, bits: u32, value: u32) {
        for i in 0..bits {
            let pos = offset + i;
            let byte = &mut data[(pos / 8) as usize];
            let mask = 1u8 << (pos % 8);
            if (value >> i) & 1 == 1 {
                *byte |= mask;
            } else {
                *byte &= !mask;
            }
        }
    }

    fn idle_learner(moving_up: bool, rest: &[u8]) -> Learner {
        let mut l = Learner::new(false, moving_up);
        for _ in 0..5 {
            l.idle(rest);
        }
        l
    }

    #[test]
    fn nothing_moved_is_none() {
        let mut l = idle_learner(true, &[0; 6]);
        assert!(l.feed(&[0; 6]).is_none());
        assert!(l.result().is_none());
    }

    #[test]
    fn button_is_final_on_release() {
        let rest = [0u8; 6];
        let mut l = idle_learner(false, &rest);
        let mut pressed = rest;
        pressed[2] = 0b0000_1000; // bit 19
        assert!(l.feed(&pressed).is_none(), "held: not final yet");
        let r = l.feed(&rest).expect("released");
        assert_eq!(r, LearnedControl::new(ControlKind::Button, None, 19, 1));
    }

    #[test]
    fn active_low_button() {
        let rest = [0, 0, 0xff, 0, 0, 0];
        let mut l = idle_learner(false, &rest);
        let mut pressed = rest;
        pressed[2] = 0b1101_1111; // bit 21 goes to 0
        assert!(l.feed(&pressed).is_none());
        let r = l.feed(&rest).unwrap();
        assert_eq!(
            (r.kind, r.offset, r.active_low),
            (ControlKind::Button, 21, true)
        );
    }

    #[test]
    fn eight_bit_fader() {
        let mut l = idle_learner(true, &[0; 6]);
        for v in (1..=255u32).step_by(3).chain([255]) {
            let mut r = [0u8; 6];
            put(&mut r, 16, 8, v);
            assert!(l.feed(&r).is_none(), "a fader is never final");
        }
        let r = l.result().unwrap();
        assert_eq!(r, {
            let mut f = LearnedControl::new(ControlKind::Fader, None, 16, 8);
            f.max = Some(255);
            f
        });
    }

    #[test]
    fn eight_bit_fader_short_travel_still_eight_bits() {
        let mut l = idle_learner(true, &[0; 6]);
        for v in [3u32, 9, 40, 90, 120] {
            let mut r = [0u8; 6];
            put(&mut r, 8, 8, v);
            l.feed(&r);
        }
        let r = l.result().unwrap();
        assert_eq!((r.offset, r.bits, r.max), (8, 8, Some(120)));
    }

    #[test]
    fn twelve_bit_packed_fader() {
        // Two 12-bit faders packed in bytes 1..4: A at bit 8, B at bit 20.
        // A sits at 0x5a5; B is moved.
        let mut rest = [0u8; 6];
        put(&mut rest, 8, 12, 0x5a5);
        put(&mut rest, 20, 12, 0);
        let mut l = idle_learner(true, &rest);
        for v in (0..=4095u32).step_by(37).chain([4095]) {
            let mut r = rest;
            put(&mut r, 20, 12, v);
            l.feed(&r);
        }
        let r = l.result().unwrap();
        assert_eq!(r.kind, ControlKind::Fader);
        assert_eq!((r.offset, r.bits, r.max), (20, 12, Some(4095)));
        assert!(!r.inverted);
    }

    #[test]
    fn inverted_fader_found_when_moved_up() {
        let mut rest = [0u8; 6];
        put(&mut rest, 12, 12, 4095); // at the bottom, reads 4095
        let mut l = idle_learner(true, &rest);
        for v in (0..=4095u32).rev().step_by(50) {
            let mut r = rest;
            put(&mut r, 12, 12, v);
            l.feed(&r);
        }
        let r = l.result().unwrap();
        assert_eq!((r.offset, r.bits, r.max), (12, 12, Some(4095)));
        assert!(r.inverted, "went down while moved up");
        // Without the hint it cannot tell.
        let mut l = idle_learner(false, &rest);
        for v in (0..=4095u32).rev().step_by(50) {
            let mut r = rest;
            put(&mut r, 12, 12, v);
            l.feed(&r);
        }
        assert!(!l.result().unwrap().inverted);
    }

    #[test]
    fn counter_byte_is_ignored() {
        let mut l = Learner::new(false, true).with_idle_reports(10);
        let mut n = 0u8;
        let mut next = |rest| {
            n = n.wrapping_add(1);
            report(Some(n), rest)
        };
        for _ in 0..10 {
            assert!(l.push(&next([0; 5])).is_none());
        }
        // Counter only: nothing moved, although its upper bits now change.
        for _ in 0..40 {
            assert!(l.push(&next([0; 5])).is_none());
        }
        assert!(l.result().is_none(), "the counter is not a control");
        for v in (0..=200u8).step_by(5) {
            l.push(&next([0, 0, v, 0, 0]));
        }
        let r = l.result().unwrap();
        assert_eq!(r.kind, ControlKind::Fader);
        assert_eq!((r.offset, r.bits, r.max), (24, 8, Some(200)));
    }

    #[test]
    fn jitter_cannot_start_a_detection_but_belongs_to_the_field() {
        // A 12-bit fader at offset 0 whose low bit flickers while idle.
        let mut l = Learner::new(false, true);
        for v in [100u32, 101, 100, 100, 101, 100] {
            let mut r = [0u8; 6];
            put(&mut r, 0, 12, v);
            l.idle(&r);
        }
        let mut r = [0u8; 6];
        put(&mut r, 0, 12, 101);
        assert!(l.feed(&r).is_none());
        assert!(l.result().is_none(), "jitter is no movement");
        for v in (101..=3000u32).step_by(41) {
            let mut r = [0u8; 6];
            put(&mut r, 0, 12, v);
            l.feed(&r);
        }
        let r = l.result().unwrap();
        assert_eq!((r.kind, r.offset, r.bits), (ControlKind::Fader, 0, 12));
    }

    #[test]
    fn numbered_reports_name_the_report() {
        let mut l = Learner::new(true, false);
        l.idle(&[1, 0, 0, 0]);
        l.idle(&[2, 0, 0, 0]);
        assert!(l.feed(&[1, 0, 0, 0]).is_none());
        assert!(l.feed(&[2, 0, 0b10, 0]).is_none());
        let r = l.feed(&[2, 0, 0, 0]).unwrap();
        assert_eq!(
            (r.report_id, r.offset, r.kind),
            (Some(2), 9, ControlKind::Button)
        );
        // Other reports after the detection do not disturb it.
        let mut l = Learner::new(true, false);
        l.idle(&[2, 0, 0, 0]);
        l.idle(&[1, 0, 0, 0]);
        l.feed(&[2, 0, 0b10, 0]);
        l.feed(&[1, 0xff, 0xff, 0xff]);
        assert_eq!(l.result().unwrap().offset, 9);
    }

    #[test]
    fn without_idle_reports_the_first_report_is_the_baseline() {
        let mut l = Learner::new(false, true);
        for v in (10..=250u32).step_by(10) {
            let mut r = [0u8; 4];
            put(&mut r, 8, 8, v);
            l.feed(&r);
        }
        let r = l.result().unwrap();
        assert_eq!(
            (r.kind, r.offset, r.bits, r.max),
            (ControlKind::Fader, 8, 8, Some(250))
        );
        // A button found this way is never final; its rest is where it ended.
        let mut l = Learner::new(false, false);
        assert!(l.feed(&[0, 0b100]).is_none(), "baseline: held");
        assert!(l.feed(&[0, 0]).is_none(), "no idle baseline: not final");
        let r = l.result().unwrap();
        assert_eq!(
            (r.kind, r.offset, r.active_low),
            (ControlKind::Button, 10, false)
        );
    }

    #[test]
    fn two_adjacent_buttons_are_not_a_fader() {
        let mut l = idle_learner(false, &[0; 6]);
        l.feed(&[0, 0b11, 0, 0, 0, 0]);
        assert!(l.feed(&[0; 6]).is_none(), "two bits moved: not final");
        let r = l.result().unwrap();
        assert_eq!((r.kind, r.offset), (ControlKind::Button, 8));
    }

    #[test]
    fn field_rounding() {
        assert_eq!(field(16, 23, 48), Some((16, 8)));
        assert_eq!(field(16, 20, 48), Some((16, 8)));
        assert_eq!(field(20, 31, 48), Some((20, 12)));
        assert_eq!(field(0, 9, 48), Some((0, 12)));
        assert_eq!(field(0, 13, 48), Some((0, 16)));
        assert_eq!(field(0, 18, 48), Some((0, 24)));
        assert_eq!(field(40, 46, 48), Some((40, 8)));
        assert_eq!(field(44, 46, 48), Some((44, 4)), "no room: what fits");
    }
}
