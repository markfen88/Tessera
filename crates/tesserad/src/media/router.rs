//! The host router in the real-time worker (plan
//! `docs/plans/2026-11-22-host-router.md`, ADR 0017).
//!
//! A [`RouterPlan`] is the compiled grid ([`crate::router::RouteTable`])
//! with every name resolved to an index and every gain to its shared word.
//! The media control thread builds it (all allocation happens there) and
//! hands it to the worker in one [`super::worker::Cmd::SetRouter`]; the
//! worker swaps it in between two ticks and hands the old one back to be
//! freed: an **RCU-style swap through the existing SPSC command ring**, so
//! the audio thread never waits, locks or allocates. At the swap the new
//! plan takes over the old one's state that must not jump
//! ([`RouterPlan::inherit`]): the network RX buses (their history), each
//! term's and level's gain smoother, each time line's cursor. A term that
//! did not exist before fades in over the ramp.
//!
//! **Data path with a plan** (devices the plan covers):
//!
//! 1. RX flows deliver into a **bus** per RX device channel (network audio
//!    after the channel's gain), a small ring indexed by media position that
//!    knows which positions it holds ([`PosBus`]); nothing else reads a
//!    flow.
//! 2. After the flows, per device time line, the **RX mix** evaluates every
//!    host and card output over the span of positions that became due at
//!    the device's playback position and writes the client and bridge
//!    rings. An output none of whose sources has audio for the span is not
//!    written (as before: a channel nobody feeds stays unwritten).
//! 3. A TX flow reading a channel evaluates that network TX channel's terms
//!    at the packet's positions, then applies the channel's gain as before.
//!
//! Gains are words, not plan contents: a level or crosspoint gain change is
//! one atomic store by the gain task and needs no new plan; a plan is
//! rebuilt only when the grid's shape changes.

use std::sync::Arc;

use tessera_audio_ipc::SampleFormat;
use tessera_media::convert::{f32_to_i32, i32_to_f32};
use tessera_media::gain::{MAX_Q24, UNITY_Q24};
use tessera_media::time::ns_to_samples;
use tessera_media::{ChannelGain, GainWord};

use super::gains::Smoother;
use super::worker::{SessionEnd, SessionRt};

/// No index.
pub const NONE: u32 = u32::MAX;

/// Frames evaluated at once.
pub const CHUNK: usize = 512;

/// Whose ring a session is, as the worker sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPort {
    /// An audio client (device channels).
    App,
    /// A network card bridge (device channels).
    Bridge,
    /// A `local_only` card bridge (card channels) of card index `n`.
    Card(u32),
}

/// A ring of one channel's samples indexed by media position, knowing which
/// positions it holds: written contiguously, a write that does not continue
/// the last one starts a new run. Reads outside the run give silence.
#[derive(Debug)]
pub struct PosBus {
    data: Box<[i32]>,
    mask: u64,
    lo: u64,
    hi: u64,
}

impl PosBus {
    /// A bus of `frames` (rounded up to a power of two).
    #[must_use]
    pub fn new(frames: u32) -> Self {
        let n = frames.max(2).next_power_of_two() as usize;
        Self {
            data: vec![0; n].into_boxed_slice(),
            mask: n as u64 - 1,
            lo: 0,
            hi: 0,
        }
    }

    /// Frames it holds.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// RT: write `s` at position `pos`.
    #[inline]
    pub fn write(&mut self, pos: u64, s: &[i32]) {
        if pos != self.hi || self.lo == self.hi {
            self.lo = pos;
        }
        for (i, v) in s.iter().enumerate() {
            let at = (pos.wrapping_add(i as u64) & self.mask) as usize;
            if let Some(d) = self.data.get_mut(at) {
                *d = *v;
            }
        }
        self.hi = pos.saturating_add(s.len() as u64);
        let cap = self.data.len() as u64;
        if self.hi - self.lo > cap {
            self.lo = self.hi - cap;
        }
    }

    /// RT: read positions `pos..` into `out`; positions it does not hold are
    /// silence. Returns how many it held.
    #[inline]
    pub fn read(&self, pos: u64, out: &mut [i32]) -> usize {
        let mut valid = 0;
        for (i, o) in out.iter_mut().enumerate() {
            let x = pos.wrapping_add(i as u64);
            if x >= self.lo && x < self.hi {
                *o = self
                    .data
                    .get((x & self.mask) as usize)
                    .copied()
                    .unwrap_or(0);
                valid += 1;
            } else {
                *o = 0;
            }
        }
        valid
    }

    /// Forget what it holds (a clock step).
    pub fn reset(&mut self) {
        self.lo = 0;
        self.hi = 0;
    }
}

/// What a term reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Src {
    /// A bus.
    Bus(u32),
    /// The TX-direction ring of `port`'s sessions on `device`, channel `chan`.
    Ring {
        /// Device index.
        device: u32,
        /// Whose ring.
        port: SessionPort,
        /// 0-based channel.
        chan: u32,
    },
}

/// One term: a source scaled by the product of gain words.
#[derive(Debug)]
pub struct Term {
    /// What it reads.
    pub src: Src,
    /// Read position relative to the evaluation position, frames.
    pub offset: i64,
    /// Its words: `words[w0..w1]`.
    pub w0: u32,
    /// End of its words.
    pub w1: u32,
    /// Ramp, samples.
    pub ramp: u32,
    /// Its gain now.
    pub smooth: Smoother,
    /// Its index in the plan it replaces ([`NONE`]: new).
    pub prev: u32,
}

/// A destination's own level.
#[derive(Debug)]
pub struct Level {
    /// The word.
    pub word: Arc<ChannelGain>,
    /// Its smoother.
    pub smooth: Smoother,
    /// Ramp, samples.
    pub ramp: u32,
}

/// Where an RX-point destination goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Out {
    /// Device index.
    pub device: u32,
    /// The producer sessions written.
    pub port: SessionPort,
    /// 0-based channel.
    pub chan: u32,
}

/// A host or card output.
#[derive(Debug)]
pub struct RxDest {
    /// Where.
    pub out: Out,
    /// Terms `terms[t0..t1]`.
    pub t0: u32,
    /// End of its terms.
    pub t1: u32,
    /// Its level.
    pub level: Option<Level>,
    /// Its index in the plan it replaces.
    pub prev: u32,
}

/// One device's time line.
#[derive(Debug)]
pub struct Timeline {
    /// Device index.
    pub device: u32,
    /// Sample rate.
    pub rate: u32,
    /// Playback position behind the media clock, frames.
    pub latency: u64,
    /// Its outputs `rx[r0..r1]`.
    pub r0: u32,
    /// End of its outputs.
    pub r1: u32,
    /// Next position to mix.
    pub cursor: Option<u64>,
    /// Clock epoch of the cursor.
    pub epoch: Option<u64>,
}

/// Per device index: what the plan does with its channels.
#[derive(Debug, Default)]
pub struct DeviceMap {
    /// The plan covers this device.
    pub covered: bool,
    /// Per RX channel: its bus ([`NONE`]: not routed anywhere).
    pub rx_bus: Box<[u32]>,
    /// Per TX channel: its terms `(t0, t1)`; `(0, 0)` when nothing feeds it.
    pub tx: Box<[(u32, u32)]>,
}

/// The real-time router plan (see the module documentation).
#[derive(Debug)]
pub struct RouterPlan {
    /// Time lines with outputs, in any order.
    pub timelines: Box<[Timeline]>,
    /// Outputs.
    pub rx: Box<[RxDest]>,
    /// Terms of every output and TX channel.
    pub terms: Box<[Term]>,
    /// Gain words of every term.
    pub words: Box<[Arc<ChannelGain>]>,
    /// Buses.
    pub buses: Box<[PosBus]>,
    /// Per bus, its index in the plan it replaces.
    pub bus_prev: Box<[u32]>,
    /// By device index.
    pub devices: Box<[DeviceMap]>,
    /// Scratch: the sum.
    pub mix: Box<[i32]>,
    /// Scratch: one term's source.
    pub read: Box<[i32]>,
    /// Scratch: `f32` rings.
    pub read_f: Box<[f32]>,
}

/// The product of `words`' effective gains as one word (clamped at the
/// largest gain).
#[inline]
pub(crate) fn product(words: &[Arc<ChannelGain>]) -> GainWord {
    let mut q = u64::from(UNITY_Q24);
    for w in words {
        // Clamp every step: `q <= MAX_Q24 < 2^27` and a word is at most
        // `MAX_Q24`, so the next product stays below 2^54 however many words
        // (nested taps) there are.
        q = ((q * u64::from(w.load().effective_q24())) >> 24).min(u64::from(MAX_Q24));
        if q == 0 {
            break;
        }
    }
    GainWord::new(u32::try_from(q).unwrap_or(MAX_Q24), false)
}

#[allow(clippy::too_many_arguments)]
/// RT: read the TX-direction ring of `port` on `device`, channel `chan`, at
/// `pos`. Returns the valid frames (0: none, `out` silent).
#[inline]
fn read_ring(
    sessions: &mut [Box<SessionRt>],
    device: u32,
    port: SessionPort,
    chan: u32,
    pos: u64,
    out: &mut [i32],
    tmp: &mut [f32],
    publish: bool,
) -> usize {
    for s in sessions.iter_mut() {
        if s.device != device || s.port != port || chan < s.first || chan >= s.first + s.count {
            continue;
        }
        let SessionEnd::Consumer(c) = &mut s.end else {
            continue;
        };
        let plane = chan - s.first;
        let valid = match s.format {
            SampleFormat::F32 => match tmp.get_mut(..out.len()) {
                Some(t) => c.read::<f32>(plane, pos, t).map(|r| {
                    for (o, v) in out.iter_mut().zip(t.iter()) {
                        *o = f32_to_i32(*v);
                    }
                    r.valid
                }),
                None => Ok(0),
            },
            _ => c.read::<i32>(plane, pos, out).map(|r| r.valid),
        };
        if publish {
            s.read_end = s.read_end.max(pos.saturating_add(out.len() as u64));
        }
        return valid.unwrap_or_else(|_| {
            out.fill(0);
            0
        });
    }
    out.fill(0);
    0
}

/// RT: sum `terms` at `pos` into `out` (`out.len() <= read.len()`). Returns
/// the most valid frames any source had.
#[allow(clippy::too_many_arguments)]
#[inline]
fn eval(
    terms: &mut [Term],
    words: &[Arc<ChannelGain>],
    buses: &[PosBus],
    sessions: &mut [Box<SessionRt>],
    read: &mut [i32],
    read_f: &mut [f32],
    pos: u64,
    out: &mut [i32],
    publish: bool,
) -> usize {
    out.fill(0);
    let Some(buf) = read.get_mut(..out.len()) else {
        return 0;
    };
    let mut valid = 0;
    for t in terms.iter_mut() {
        let v = match pos.checked_add_signed(t.offset) {
            None => {
                buf.fill(0);
                0
            }
            Some(x) => match t.src {
                Src::Bus(b) => match buses.get(b as usize) {
                    Some(bus) => bus.read(x, buf),
                    None => {
                        buf.fill(0);
                        0
                    }
                },
                Src::Ring { device, port, chan } => {
                    read_ring(sessions, device, port, chan, x, buf, read_f, publish)
                }
            },
        };
        let g = words
            .get(t.w0 as usize..t.w1 as usize)
            .map_or(GainWord::UNITY, product);
        // Every block, also a silent one, so the smoother stays current.
        t.smooth.apply(g, t.ramp, buf);
        if v > 0 {
            for (o, s) in out.iter_mut().zip(buf.iter()) {
                *o = o.saturating_add(*s);
            }
        }
        valid = valid.max(v);
    }
    valid
}

impl RouterPlan {
    /// The device index's map, if the plan covers it.
    #[inline]
    #[must_use]
    pub fn covers(&self, device: u32) -> bool {
        self.devices.get(device as usize).is_some_and(|d| d.covered)
    }

    /// RT: the bus of RX channel `chan` (0-based) of `device`, if it is
    /// routed.
    #[inline]
    pub fn rx_bus(&mut self, device: u32, chan: u32) -> Option<&mut PosBus> {
        let b = *self
            .devices
            .get(device as usize)?
            .rx_bus
            .get(chan as usize)?;
        self.buses.get_mut(b as usize)
    }

    /// RT: a clock step: forget the buses.
    pub fn on_epoch(&mut self) {
        for b in self.buses.iter_mut() {
            b.reset();
        }
    }

    /// RT: what network TX channel `chan` of `device` carries at `pos` (before
    /// the channel's own gain). Returns the valid frames.
    pub fn eval_tx(
        &mut self,
        sessions: &mut [Box<SessionRt>],
        device: u32,
        chan: u32,
        pos: u64,
        out: &mut [i32],
    ) -> usize {
        let (t0, t1) = self
            .devices
            .get(device as usize)
            .and_then(|d| d.tx.get(chan as usize))
            .copied()
            .unwrap_or((0, 0));
        let Self {
            terms,
            words,
            buses,
            read,
            read_f,
            ..
        } = self;
        let Some(terms) = terms.get_mut(t0 as usize..t1 as usize) else {
            out.fill(0);
            return 0;
        };
        let step = read.len().max(1);
        let mut valid = 0;
        let mut done = 0;
        while done < out.len() {
            let n = (out.len() - done).min(step);
            let Some(o) = out.get_mut(done..done + n) else {
                break;
            };
            valid += eval(
                terms,
                words,
                buses,
                sessions,
                read,
                read_f,
                pos + done as u64,
                o,
                true,
            );
            done += n;
        }
        valid
    }

    /// RT: mix every output whose positions became due at `media_ns` (epoch
    /// `epoch`) and write the rings (see the module documentation).
    pub fn mix_rx(
        &mut self,
        sessions: &mut [Box<SessionRt>],
        media_ns: u64,
        epoch: u64,
        max_span: u64,
    ) {
        let Self {
            timelines,
            rx,
            terms,
            words,
            buses,
            mix,
            read,
            read_f,
            ..
        } = self;
        for tl in timelines.iter_mut() {
            let pos = ns_to_samples(media_ns, tl.rate).saturating_sub(tl.latency);
            let cursor = match tl.cursor {
                Some(c) if tl.epoch == Some(epoch) && c <= pos => c,
                _ => {
                    tl.cursor = Some(pos);
                    tl.epoch = Some(epoch);
                    continue;
                }
            };
            let mut p = if pos - cursor > max_span {
                pos - max_span
            } else {
                cursor
            };
            let Some(dests) = rx.get_mut(tl.r0 as usize..tl.r1 as usize) else {
                continue;
            };
            while p < pos {
                let n = usize::try_from(pos - p)
                    .unwrap_or(usize::MAX)
                    .min(mix.len())
                    .min(read.len());
                if n == 0 {
                    break;
                }
                for d in dests.iter_mut() {
                    let (Some(t), Some(out)) = (
                        terms.get_mut(d.t0 as usize..d.t1 as usize),
                        mix.get_mut(..n),
                    ) else {
                        continue;
                    };
                    let v = eval(t, words, buses, sessions, read, read_f, p, out, false);
                    if let Some(l) = &mut d.level {
                        l.smooth.apply(l.word.load(), l.ramp, out);
                    }
                    if v == 0 {
                        continue;
                    }
                    write_out(sessions, d.out, p, out, read_f);
                }
                p += n as u64;
            }
            tl.cursor = Some(pos);
        }
    }

    /// RT: take over what must not jump from the plan this one replaces (see
    /// the module documentation). No allocation: storage is swapped, the old
    /// plan keeps the fresh buffers and goes back to be freed.
    pub fn inherit(&mut self, old: &mut RouterPlan) {
        for (b, &j) in self.buses.iter_mut().zip(self.bus_prev.iter()) {
            if let Some(o) = old.buses.get_mut(j as usize) {
                if o.capacity() == b.capacity() {
                    std::mem::swap(b, o);
                }
            }
        }
        for t in self.terms.iter_mut() {
            match old.terms.get(t.prev as usize) {
                Some(o) => t.smooth = o.smooth,
                // New: fade in.
                None => t.smooth = Smoother::SILENT,
            }
        }
        for d in self.rx.iter_mut() {
            if let (Some(o), Some(l)) = (old.rx.get(d.prev as usize), &mut d.level) {
                if let Some(ol) = &o.level {
                    l.smooth = ol.smooth;
                }
            }
        }
        for tl in self.timelines.iter_mut() {
            if let Some(o) = old
                .timelines
                .iter()
                .find(|o| o.device == tl.device && o.rate == tl.rate && o.latency == tl.latency)
            {
                tl.cursor = o.cursor;
                tl.epoch = o.epoch;
            }
        }
    }
}

/// RT: write one output's span into the producer sessions it names.
#[inline]
fn write_out(sessions: &mut [Box<SessionRt>], out: Out, p: u64, data: &[i32], tmp: &mut [f32]) {
    for s in sessions.iter_mut() {
        if s.device != out.device
            || s.port != out.port
            || out.chan < s.first
            || out.chan >= s.first + s.count
        {
            continue;
        }
        let SessionEnd::Producer(prod) = &mut s.end else {
            continue;
        };
        let plane = out.chan - s.first;
        match s.format {
            SampleFormat::F32 => {
                if let Some(t) = tmp.get_mut(..data.len()) {
                    for (d, v) in t.iter_mut().zip(data.iter()) {
                        *d = i32_to_f32(*v);
                    }
                    let _ = prod.write::<f32>(plane, p, t);
                }
            }
            _ => {
                let _ = prod.write::<i32>(plane, p, data);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bus_holds_one_run_and_reads_silence_elsewhere() {
        let mut b = PosBus::new(8);
        let mut out = [9; 4];
        assert_eq!(b.read(0, &mut out), 0);
        assert_eq!(out, [0; 4]);
        b.write(100, &[1, 2, 3]);
        b.write(103, &[4, 5]);
        assert_eq!(b.read(101, &mut out), 4);
        assert_eq!(out, [2, 3, 4, 5]);
        assert_eq!(b.read(104, &mut out), 1, "only 104 is held");
        assert_eq!(out, [5, 0, 0, 0]);
        // A write that does not continue starts over.
        b.write(200, &[7]);
        assert_eq!(b.read(102, &mut out), 0);
        // More than its capacity keeps the newest (203..211).
        b.write(201, &[1; 10]);
        assert_eq!(b.read(202, &mut out), 3);
        assert_eq!(out, [0, 1, 1, 1]);
        b.reset();
        assert_eq!(b.read(205, &mut out), 0);
    }

    #[test]
    fn a_product_of_words_clamps_and_mutes() {
        let a = Arc::new(ChannelGain::new());
        let b = Arc::new(ChannelGain::new());
        assert!(product(&[Arc::clone(&a), Arc::clone(&b)]).is_unity());
        a.set_db(12.0);
        b.set_db(12.0);
        assert_eq!(product(&[Arc::clone(&a), Arc::clone(&b)]).q24(), MAX_Q24);
        b.set_db(-6.0206);
        let q = product(&[Arc::clone(&a), Arc::clone(&b)]).q24();
        assert!(
            (i64::from(q) - i64::from(MAX_Q24) / 2).abs() < 20_000,
            "{q}"
        );
        b.set_muted(true);
        assert_eq!(product(&[a, b]).effective_q24(), 0);
    }

    #[test]
    fn a_product_of_many_words_at_max_does_not_overflow() {
        // Nested taps can stack up to 64 words; at +12 dB each the
        // unclamped running product would overflow u64 after a few steps.
        let words: Vec<_> = (0..128)
            .map(|_| {
                let w = Arc::new(ChannelGain::new());
                w.set_db(12.0);
                w
            })
            .collect();
        assert_eq!(product(&words).q24(), MAX_Q24);
        // Clamped at the top, a later cut still applies to the clamped value.
        let cut = Arc::new(ChannelGain::new());
        cut.set_db(-6.0206);
        let mut with_cut = words.clone();
        with_cut.push(cut);
        let q = product(&with_cut).q24();
        assert!(
            (i64::from(q) - i64::from(MAX_Q24) / 2).abs() < 20_000,
            "{q}"
        );
    }
}
