//! Building the real-time router plan from a [`RouteTable`] (media control
//! thread: every allocation of a plan happens here, never on the worker).

use std::collections::HashMap;
use std::sync::Arc;

use tessera_control_types::Gain;
use tessera_media::ChannelGain;

use super::gains::{GainRegistry, RAMP_MS, Smoother};
use super::router::{
    CHUNK, DeviceMap, Level, NONE, Out, PosBus, RouterPlan, RxDest, SessionPort, Src, Term,
    Timeline, product,
};
use crate::router::{OutSpec, RingOwner, RouteTable, SrcSpec, TermSpec, WordKey};

/// Who a term belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Owner {
    Rx(OutSpec),
    Tx(String, u32),
}

/// What the indices of a built plan mean, so the next plan can name the
/// state it takes over ([`RouterPlan::inherit`]).
#[derive(Debug, Default)]
pub struct Index {
    terms: HashMap<(Owner, TermSpec), u32>,
    buses: HashMap<(String, u32), u32>,
    rx: HashMap<OutSpec, u32>,
}

fn ramp(rate: u32) -> u32 {
    (rate / 1000).saturating_mul(RAMP_MS).max(1)
}

fn idx(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(NONE)
}

struct Builder<'a> {
    gains: &'a GainRegistry,
    seeds: HashMap<&'a WordKey, (Gain, bool)>,
    dev: &'a mut dyn FnMut(&str) -> u32,
    card: &'a mut dyn FnMut(&str) -> u32,
    prev: &'a Index,
    next: Index,
    terms: Vec<Term>,
    words: Vec<Arc<ChannelGain>>,
    buses: Vec<PosBus>,
    bus_prev: Vec<u32>,
    bus_frames: HashMap<&'a str, u32>,
    devices: Vec<DeviceMap>,
}

impl Builder<'_> {
    fn port(&mut self, owner: &RingOwner) -> SessionPort {
        match owner {
            RingOwner::App => SessionPort::App,
            RingOwner::Bridge => SessionPort::Bridge,
            RingOwner::Card(pcm) => SessionPort::Card((self.card)(pcm)),
        }
    }

    fn device(&mut self, key: &str) -> u32 {
        let d = (self.dev)(key);
        if self.devices.len() <= d as usize && d != NONE {
            self.devices.resize_with(d as usize + 1, DeviceMap::default);
        }
        d
    }

    fn bus(&mut self, device: &str, channel: u32) -> u32 {
        let key = (device.to_owned(), channel);
        if let Some(&b) = self.next.buses.get(&key) {
            return b;
        }
        let b = idx(self.buses.len());
        let frames = self.bus_frames.get(device).copied().unwrap_or(4096);
        self.buses.push(PosBus::new(frames));
        self.bus_prev
            .push(self.prev.buses.get(&key).copied().unwrap_or(NONE));
        self.next.buses.insert(key.clone(), b);
        let d = self.device(device);
        if let Some(m) = self.devices.get_mut(d as usize) {
            let mut v = std::mem::take(&mut m.rx_bus).into_vec();
            if v.len() <= channel as usize {
                v.resize(channel as usize + 1, NONE);
            }
            v[channel as usize] = b;
            m.rx_bus = v.into_boxed_slice();
        }
        b
    }

    fn word(&mut self, k: &WordKey) -> Arc<ChannelGain> {
        self.gains.word(k, self.seeds.get(k).copied())
    }

    /// Append `specs` as terms of `owner` at `rate`; returns `(t0, t1)`.
    fn terms(&mut self, owner: &Owner, specs: &[TermSpec], rate: u32) -> (u32, u32) {
        let t0 = idx(self.terms.len());
        for s in specs {
            let src = match &s.src {
                SrcSpec::Bus { device, channel } => Src::Bus(self.bus(device, *channel)),
                SrcSpec::Ring {
                    device,
                    owner,
                    channel,
                } => Src::Ring {
                    device: self.device(device),
                    port: self.port(owner),
                    chan: *channel,
                },
            };
            let w0 = idx(self.words.len());
            for k in &s.words {
                let w = self.word(k);
                self.words.push(w);
            }
            let w1 = idx(self.words.len());
            let g = product(&self.words[w0 as usize..w1 as usize]);
            let id = (owner.clone(), s.clone());
            let i = idx(self.terms.len());
            let prev = self.prev.terms.get(&id).copied().unwrap_or(NONE);
            self.next.terms.entry(id).or_insert(i);
            self.terms.push(Term {
                src,
                offset: s.offset,
                w0,
                w1,
                ramp: ramp(rate),
                smooth: Smoother::at(g),
                prev,
            });
        }
        (t0, idx(self.terms.len()))
    }
}

/// Build the plan for `table` (see [`RouterPlan`]); `dev` and `card` intern
/// device keys and card PCM names into the indices the sessions and flows
/// use; `prev` describes the plan it will replace.
pub fn build(
    table: &RouteTable,
    gains: &GainRegistry,
    dev: &mut dyn FnMut(&str) -> u32,
    card: &mut dyn FnMut(&str) -> u32,
    prev: &Index,
) -> (Box<RouterPlan>, Index) {
    let mut b = Builder {
        gains,
        seeds: table.seeds.iter().map(|(k, g, m)| (k, (*g, *m))).collect(),
        dev,
        card,
        prev,
        next: Index::default(),
        terms: Vec::new(),
        words: Vec::new(),
        buses: Vec::new(),
        bus_prev: Vec::new(),
        bus_frames: table
            .timelines
            .iter()
            .map(|t| (t.device.as_str(), t.bus_frames))
            .collect(),
        devices: Vec::new(),
    };
    let mut timelines = Vec::new();
    let mut rx = Vec::new();
    for tl in &table.timelines {
        let d = b.device(&tl.device);
        if let Some(m) = b.devices.get_mut(d as usize) {
            m.covered = true;
        }
        let r0 = idx(rx.len());
        for dest in table.rx.iter().filter(|r| r.out.device() == tl.device) {
            let owner = Owner::Rx(dest.out.clone());
            let (t0, t1) = b.terms(&owner, &dest.terms, tl.rate);
            let out = match &dest.out {
                OutSpec::HostOut { channel, .. } => Out {
                    device: d,
                    port: SessionPort::App,
                    chan: *channel,
                },
                OutSpec::CardOut { owner, channel, .. } => Out {
                    device: d,
                    port: b.port(owner),
                    chan: *channel,
                },
            };
            let level = dest.level.as_ref().map(|k| {
                let word = b.word(k);
                Level {
                    smooth: Smoother::at(word.load()),
                    word,
                    ramp: ramp(tl.rate),
                }
            });
            let i = idx(rx.len());
            b.next.rx.insert(dest.out.clone(), i);
            rx.push(RxDest {
                out,
                t0,
                t1,
                level,
                prev: prev.rx.get(&dest.out).copied().unwrap_or(NONE),
            });
        }
        timelines.push(Timeline {
            device: d,
            rate: tl.rate,
            latency: tl.latency_frames,
            r0,
            r1: idx(rx.len()),
            cursor: None,
            epoch: None,
        });
    }
    for t in &table.tx {
        let rate = table
            .timelines
            .iter()
            .find(|tl| tl.device == t.device)
            .map_or(48_000, |tl| tl.rate);
        let range = b.terms(&Owner::Tx(t.device.clone(), t.channel), &t.terms, rate);
        let d = b.device(&t.device);
        if let Some(m) = b.devices.get_mut(d as usize) {
            let mut v = std::mem::take(&mut m.tx).into_vec();
            if v.len() <= t.channel as usize {
                v.resize(t.channel as usize + 1, (0, 0));
            }
            v[t.channel as usize] = range;
            m.tx = v.into_boxed_slice();
        }
    }
    let plan = Box::new(RouterPlan {
        timelines: timelines.into_boxed_slice(),
        rx: rx.into_boxed_slice(),
        terms: b.terms.into_boxed_slice(),
        words: b.words.into_boxed_slice(),
        buses: b.buses.into_boxed_slice(),
        bus_prev: b.bus_prev.into_boxed_slice(),
        devices: b.devices.into_boxed_slice(),
        mix: vec![0; CHUNK].into_boxed_slice(),
        read: vec![0; CHUNK].into_boxed_slice(),
        read_f: vec![0.0; CHUNK].into_boxed_slice(),
    });
    (plan, b.next)
}
