//! The host router's mix table (control plane, pure; plan
//! `docs/plans/2026-11-22-host-router.md`, ADR 0017).
//!
//! [`compile`] turns the effective grid (the active crosspoints the state
//! engine reports, bridge-implied ones included), the devices and the
//! running card bridges into a [`RouteTable`]: for every destination port,
//! the list of **terms** it sums. The media control thread turns the table
//! into the real-time plan (`media::router`), which the worker swaps in
//! atomically.
//!
//! **Terms.** A term reads one *leaf* source (a network RX channel's bus, an
//! application's ring, a card bridge's ring) at a position offset, and scales
//! it by the product of gain words on its path: the crosspoint's gain, the
//! source port's level and, through taps, the tapped destination's level and
//! the crosspoints that feed it. Taps (a destination used as a source) are
//! flattened: a tap contributes the tapped destination's own terms, so the
//! real-time side never recurses and evaluation order does not matter (the
//! state engine refuses loops). Levels applied elsewhere are not words of a
//! term: a network RX channel's gain is applied as the flow delivers into its
//! bus, a network TX channel's gain by the TX flow.
//!
//! **Two evaluation points.**
//!
//! * *RX point* (host outputs, card outputs), once per engine tick over the
//!   span of positions that became due at the destination device's playback
//!   position (`media now − latency`), as the flows deliver today. A network
//!   RX bus of the same device is read at the same position; a ring
//!   (application or card input) as new as it is reliably written: at the
//!   playback position for an application (it writes ahead of the clock), or
//!   ahead of it by the capturing bridge's margin
//!   ([`crate::runtime::monitor_offset_ns`]), or `monitor_delay_us` behind
//!   it when configured. So card input → host output adds nothing beyond the
//!   bridge's own buffering.
//! * *TX point* (network TX channels), when a TX flow reads the packet's
//!   positions: rings at that position (as today), network RX buses a
//!   latency and two ticks behind (that is when they are written).
//!
//! **The default grid** (identity, application outputs to TX, monitoring as
//! TX taps into host outputs, network bridges' implied crosspoints) compiles
//! to exactly what the media path did before the router: each host output is
//! its RX channel plus the monitored TX channel at the monitor offset, scaled
//! by the output level; each TX channel reads its ring.

use std::collections::{BTreeMap, BTreeSet};

use tessera_control_types::{Crosspoint, CrosspointKey, CrosspointOrigin, Direction, RouterPort};
use tessera_state::config::BridgeDirection;
use tessera_state::model::DeviceModel;

use crate::bridge::BridgePlan;

/// Most terms one table holds (flattened taps can multiply them); the rest
/// are left out with a warning.
pub const MAX_TERMS: usize = 4096;

/// A gain word the media path loads (one `AtomicU32` each).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WordKey {
    /// The gain of a device channel (0-based).
    Channel(String, Direction, u32),
    /// The output stage of an RX device channel (0-based): the host output's
    /// level.
    Output(String, u32),
    /// The level of a local port (`app_out`, `card_in`, `card_out`).
    Port(RouterPort),
    /// The gain of a crosspoint.
    Crosspoint(CrosspointKey),
}

/// Whose ring a ring source or destination is.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RingOwner {
    /// Audio clients of the device (the ALSA plugin, PipeWire): channels are
    /// device channels.
    App,
    /// A network card bridge: channels are device channels.
    Bridge,
    /// A `local_only` card bridge on this ALSA PCM: channels are card
    /// channels.
    Card(String),
}

/// Where a term reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SrcSpec {
    /// The bus of RX device channel `channel` (0-based): network audio after
    /// the channel's gain.
    Bus {
        /// Device key.
        device: String,
        /// 0-based RX channel.
        channel: u32,
    },
    /// A ring the daemon reads (TX direction): what an application plays
    /// into TX channel `channel`, or what a card bridge captured.
    Ring {
        /// Device key (the time line).
        device: String,
        /// Whose ring.
        owner: RingOwner,
        /// 0-based device channel, or card channel for [`RingOwner::Card`].
        channel: u32,
    },
}

/// One term of a destination.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TermSpec {
    /// What it reads.
    pub src: SrcSpec,
    /// Read position relative to the evaluation position, frames (positive:
    /// newer).
    pub offset: i64,
    /// The gain words multiplied on its path.
    pub words: Vec<WordKey>,
}

/// Where an RX-point destination is written.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OutSpec {
    /// The rings of applications capturing RX channel `channel` (0-based).
    HostOut {
        /// Device key.
        device: String,
        /// 0-based RX channel.
        channel: u32,
    },
    /// A playback bridge's ring.
    CardOut {
        /// Device key (the time line).
        device: String,
        /// [`RingOwner::Bridge`] (device RX channel numbering) or
        /// [`RingOwner::Card`] (card channels).
        owner: RingOwner,
        /// 0-based channel.
        channel: u32,
    },
}

impl OutSpec {
    /// The device whose time line evaluates it.
    #[must_use]
    pub fn device(&self) -> &str {
        match self {
            Self::HostOut { device, .. } | Self::CardOut { device, .. } => device,
        }
    }
}

/// A host or card output.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RxDest {
    /// Where it goes.
    pub out: OutSpec,
    /// Its own level (output stage, card output level), after the sum.
    pub level: Option<WordKey>,
    /// What it sums.
    pub terms: Vec<TermSpec>,
}

/// A network TX channel (its level is the TX flow's channel gain).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TxDest {
    /// Device key.
    pub device: String,
    /// 0-based TX channel.
    pub channel: u32,
    /// What it sums.
    pub terms: Vec<TermSpec>,
}

/// One device's time line.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Timeline {
    /// Device key.
    pub device: String,
    /// Sample rate.
    pub rate: u32,
    /// The RX playback position is this many frames behind the media clock
    /// (as the device's RX flows compute it).
    pub latency_frames: u64,
    /// Frames a network RX bus of this device keeps (a power of two).
    pub bus_frames: u32,
}

/// The compiled grid.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteTable {
    /// Every device of the model.
    pub timelines: Vec<Timeline>,
    /// Host and card outputs.
    pub rx: Vec<RxDest>,
    /// Network TX channels that something feeds.
    pub tx: Vec<TxDest>,
    /// Starting values of the crosspoint and local port words (the engine's
    /// values when the table was compiled), used when the media path creates
    /// a word; afterwards the gain task keeps it equal to the engine.
    pub seeds: Vec<(WordKey, tessera_control_types::Gain, bool)>,
}

impl RouteTable {
    /// The same grid shape (time lines, outputs, terms): only a change of
    /// shape needs a new real-time plan. The seeds are starting values of
    /// words, which the gain task keeps current: a fader move is no new
    /// shape.
    #[must_use]
    pub fn same_shape(&self, other: &RouteTable) -> bool {
        self.timelines == other.timelines && self.rx == other.rx && self.tx == other.tx
    }
}

/// What [`compile`] works from.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// The effective devices.
    pub devices: &'a [DeviceModel],
    /// `monitor_delay_us` per device (0: auto).
    pub monitor_delay_us: &'a BTreeMap<String, u32>,
    /// The active crosspoints of the effective grid and their origins.
    pub crosspoints: &'a [(Crosspoint, CrosspointOrigin)],
    /// Local port levels (`[[router.level]]`): starting values of their words.
    pub port_levels: &'a [(RouterPort, tessera_control_types::Gain, bool)],
    /// The card bridges that run.
    pub bridges: &'a [BridgePlan],
    /// The media engine's tick, nanoseconds.
    pub tick_ns: u64,
}

/// Is `port` a local port whose level lives with the router (`app_out`,
/// `card_in`, `card_out`), not a network channel control?
#[must_use]
pub const fn is_local_level(port: &RouterPort) -> bool {
    matches!(
        port,
        RouterPort::AppOut { .. } | RouterPort::CardIn { .. } | RouterPort::CardOut { .. }
    )
}

/// The playback position's distance from the media clock, frames, as an RX
/// flow of a device with this latency computes it.
#[must_use]
pub fn latency_frames(rate: u32, latency_us: u32) -> u64 {
    let mut c = tessera_media::RxConfig::new(rate.max(1), 1);
    c.latency_ns = u64::from(latency_us) * 1_000;
    c.effective_latency_samples()
}

fn ns_to_frames(ns: i64, rate: u32) -> i64 {
    let v = i128::from(ns) * i128::from(rate) / 1_000_000_000;
    i64::try_from(v).unwrap_or(if ns < 0 { i64::MIN } else { i64::MAX })
}

/// A source port resolved to what the media path reads.
struct Leaf<'a> {
    src: SrcSpec,
    /// The device whose time line the source is on.
    device: &'a str,
    /// The capturing bridge of a card input.
    bridge: Option<&'a BridgePlan>,
    /// The source port's own level, if it is a word of the term.
    word: Option<WordKey>,
}

#[derive(Clone, Copy)]
enum Point<'a> {
    Rx(&'a Timeline),
    Tx(&'a Timeline),
}

struct Compiler<'a> {
    i: Inputs<'a>,
    timelines: Vec<Timeline>,
    into: BTreeMap<&'a RouterPort, Vec<&'a (Crosspoint, CrosspointOrigin)>>,
    terms: usize,
    warnings: BTreeSet<String>,
}

/// Does capture bridge `b` carry card input `channel` (1-based) of `card`?
fn carries(b: &BridgePlan, card: &str, channel: u16, dir: BridgeDirection) -> bool {
    b.pcm == card
        && b.direction == dir
        && channel >= b.card_first_channel
        && u32::from(channel) < u32::from(b.card_first_channel) + u32::from(b.channels)
}

impl<'a> Compiler<'a> {
    fn timeline(&self, device: &str) -> Option<&Timeline> {
        self.timelines.iter().find(|t| t.device == device)
    }

    fn bridge(&self, card: &str, channel: u16, dir: BridgeDirection) -> Option<&'a BridgePlan> {
        self.i
            .bridges
            .iter()
            .find(|b| carries(b, card, channel, dir))
    }

    fn leaf(&self, port: &'a RouterPort) -> Option<Leaf<'a>> {
        let n = u32::from(port.channel().get()) - 1;
        match port {
            RouterPort::NetworkRx { device, .. } => Some(Leaf {
                src: SrcSpec::Bus {
                    device: device.0.clone(),
                    channel: n,
                },
                device: &device.0,
                bridge: None,
                word: None,
            }),
            RouterPort::AppOut { device, .. } => Some(Leaf {
                src: SrcSpec::Ring {
                    device: device.0.clone(),
                    owner: RingOwner::App,
                    channel: n,
                },
                device: &device.0,
                bridge: None,
                word: Some(WordKey::Port(port.clone())),
            }),
            RouterPort::CardIn { card, channel } => {
                let b = self.bridge(card, channel.get(), BridgeDirection::Capture)?;
                let (owner, ch) = if b.local_only {
                    (RingOwner::Card(card.clone()), n)
                } else {
                    (
                        RingOwner::Bridge,
                        u32::from(b.first_channel) - 1 + u32::from(channel.get())
                            - u32::from(b.card_first_channel),
                    )
                };
                Some(Leaf {
                    src: SrcSpec::Ring {
                        device: b.device.clone(),
                        owner,
                        channel: ch,
                    },
                    device: &b.device,
                    bridge: Some(b),
                    word: Some(WordKey::Port(port.clone())),
                })
            }
            _ => None,
        }
    }

    /// The level word of a destination port (as a tapped source or a dest).
    fn level(port: &RouterPort) -> Option<WordKey> {
        let n = u32::from(port.channel().get()) - 1;
        match port {
            RouterPort::NetworkTx { device, .. } => {
                Some(WordKey::Channel(device.0.clone(), Direction::Tx, n))
            }
            RouterPort::HostOut { device, .. } => Some(WordKey::Output(device.0.clone(), n)),
            RouterPort::CardOut { .. } => Some(WordKey::Port(port.clone())),
            _ => None,
        }
    }

    fn offset(&mut self, point: Point<'_>, leaf: &Leaf<'_>) -> Option<i64> {
        let src = self.timeline(leaf.device)?.clone();
        let (Point::Rx(at) | Point::Tx(at)) = point;
        if src.rate != at.rate {
            self.warnings.insert(format!(
                "a crosspoint from device `{}` ({} Hz) into device `{}` ({} Hz) is not mixed: the router does not convert rates",
                src.device, src.rate, at.device, at.rate
            ));
            return None;
        }
        let bus = matches!(leaf.src, SrcSpec::Bus { .. });
        Some(match (point, bus) {
            // The same playback position, or older when the source device
            // plays later.
            (Point::Rx(at), true) => (i64::try_from(at.latency_frames).unwrap_or(i64::MAX)
                - i64::try_from(src.latency_frames).unwrap_or(i64::MAX))
            .min(0),
            (Point::Rx(at), false) => {
                let d = self.i.devices.iter().find(|d| d.key.0 == at.device)?;
                let delay = self
                    .i
                    .monitor_delay_us
                    .get(&at.device)
                    .copied()
                    .unwrap_or(0);
                let feeders: Vec<&BridgePlan> = leaf.bridge.into_iter().collect();
                ns_to_frames(
                    crate::runtime::monitor_offset_ns(delay, d.latency_us, &feeders),
                    at.rate,
                )
            }
            // Written once the source's playback position passes it: a
            // latency and up to a tick later, read by the flow up to a packet
            // later still.
            (Point::Tx(_), true) => {
                let tick = ns_to_frames(
                    i64::try_from(self.i.tick_ns.max(1)).unwrap_or(1_000_000),
                    at.rate,
                )
                .max(1);
                -(i64::try_from(src.latency_frames).unwrap_or(i64::MAX / 2) + 2 * tick)
            }
            (Point::Tx(_), false) => 0,
        })
    }

    /// Append the terms of everything that feeds `dest` (evaluated at
    /// `point`) to `out`, each with `path` words in front.
    fn expand(
        &mut self,
        dest: &RouterPort,
        point: Point<'_>,
        path: &mut Vec<WordKey>,
        out: &mut Vec<TermSpec>,
        depth: usize,
    ) {
        let Some(feeds) = self.into.get(dest).cloned() else {
            return;
        };
        if depth > 64 {
            self.warnings
                .insert("router taps nest too deep; the deepest are left out".into());
            return;
        }
        for (x, origin) in feeds {
            let pushed = if matches!(origin, CrosspointOrigin::Bridge { .. }) {
                false
            } else {
                path.push(WordKey::Crosspoint(x.key()));
                true
            };
            if x.source.is_destination() {
                let lvl = Self::level(&x.source);
                if let Some(l) = &lvl {
                    path.push(l.clone());
                }
                self.expand(&x.source, point, path, out, depth + 1);
                if lvl.is_some() {
                    path.pop();
                }
            } else if let Some(leaf) = self.leaf(&x.source) {
                if let Some(offset) = self.offset(point, &leaf) {
                    if self.terms >= MAX_TERMS {
                        self.warnings.insert(format!(
                            "the router grid flattens to more than {MAX_TERMS} terms; the rest are left out"
                        ));
                    } else {
                        self.terms += 1;
                        let mut words = path.clone();
                        words.extend(leaf.word);
                        out.push(TermSpec {
                            src: leaf.src,
                            offset,
                            words,
                        });
                    }
                }
            }
            if pushed {
                path.pop();
            }
        }
    }

    fn rx_out(&self, dest: &RouterPort) -> Option<OutSpec> {
        let n = u32::from(dest.channel().get()) - 1;
        match dest {
            RouterPort::HostOut { device, .. } => Some(OutSpec::HostOut {
                device: device.0.clone(),
                channel: n,
            }),
            RouterPort::CardOut { card, channel } => {
                let b = self.bridge(card, channel.get(), BridgeDirection::Playback)?;
                Some(if b.local_only {
                    OutSpec::CardOut {
                        device: b.device.clone(),
                        owner: RingOwner::Card(card.clone()),
                        channel: n,
                    }
                } else {
                    OutSpec::CardOut {
                        device: b.device.clone(),
                        owner: RingOwner::Bridge,
                        channel: u32::from(b.first_channel) - 1 + u32::from(channel.get())
                            - u32::from(b.card_first_channel),
                    }
                })
            }
            _ => None,
        }
    }
}

/// Compile the grid (see the module documentation). Returns the table and
/// what could not be mixed, in words (logged by the caller once).
#[must_use]
pub fn compile(i: Inputs<'_>) -> (RouteTable, Vec<String>) {
    let timelines: Vec<Timeline> = i
        .devices
        .iter()
        .map(|d| {
            let latency = latency_frames(d.sample_rate, d.latency_us);
            let max = latency_frames(d.sample_rate, d.latency_bounds_us[1].max(d.latency_us));
            let tick = u64::try_from(ns_to_frames(
                i64::try_from(i.tick_ns.max(1)).unwrap_or(1_000_000),
                d.sample_rate,
            ))
            .unwrap_or(48);
            let need = max + 4 * tick + 1024;
            Timeline {
                device: d.key.0.clone(),
                rate: d.sample_rate,
                latency_frames: latency,
                bus_frames: u32::try_from(need.next_power_of_two().max(2048)).unwrap_or(1 << 20),
            }
        })
        .collect();
    let mut into: BTreeMap<&RouterPort, Vec<&(Crosspoint, CrosspointOrigin)>> = BTreeMap::new();
    for e in i.crosspoints {
        into.entry(&e.0.destination).or_default().push(e);
    }
    let mut c = Compiler {
        i,
        timelines,
        into,
        terms: 0,
        warnings: BTreeSet::new(),
    };
    let mut table = RouteTable::default();
    let dests: Vec<&RouterPort> = c.into.keys().copied().collect();
    for dest in dests {
        match dest {
            RouterPort::NetworkTx { device, channel } => {
                let Some(tl) = c.timeline(&device.0).cloned() else {
                    continue;
                };
                let mut terms = Vec::new();
                c.expand(dest, Point::Tx(&tl), &mut Vec::new(), &mut terms, 0);
                table.tx.push(TxDest {
                    device: device.0.clone(),
                    channel: u32::from(channel.get()) - 1,
                    terms,
                });
            }
            RouterPort::HostOut { .. } | RouterPort::CardOut { .. } => {
                let Some(out) = c.rx_out(dest) else {
                    continue;
                };
                let Some(tl) = c.timeline(out.device()).cloned() else {
                    continue;
                };
                let mut terms = Vec::new();
                c.expand(dest, Point::Rx(&tl), &mut Vec::new(), &mut terms, 0);
                table.rx.push(RxDest {
                    out,
                    level: Compiler::level(dest),
                    terms,
                });
            }
            _ => {}
        }
    }
    for (x, origin) in i.crosspoints {
        if !matches!(origin, CrosspointOrigin::Bridge { .. }) {
            table
                .seeds
                .push((WordKey::Crosspoint(x.key()), x.gain, x.muted));
        }
    }
    for (p, g, m) in i.port_levels {
        table.seeds.push((WordKey::Port(p.clone()), *g, *m));
    }
    table.timelines = c.timelines;
    (table, c.warnings.into_iter().collect())
}

/// The card inputs (`card`, 1-based channel) that reach a host or card
/// output through the grid, directly or through taps: the capture bridges
/// that carry them are listened to locally and run for latency.
#[must_use]
pub fn local_listeners(crosspoints: &[(Crosspoint, CrosspointOrigin)]) -> BTreeSet<(String, u16)> {
    let mut into: BTreeMap<&RouterPort, Vec<&RouterPort>> = BTreeMap::new();
    for (x, _) in crosspoints {
        into.entry(&x.destination).or_default().push(&x.source);
    }
    let mut out = BTreeSet::new();
    let mut seen: BTreeSet<&RouterPort> = BTreeSet::new();
    let mut stack: Vec<&RouterPort> = into
        .keys()
        .copied()
        .filter(|d| matches!(d, RouterPort::HostOut { .. } | RouterPort::CardOut { .. }))
        .collect();
    while let Some(d) = stack.pop() {
        if !seen.insert(d) {
            continue;
        }
        for s in into.get(d).into_iter().flatten() {
            match s {
                RouterPort::CardIn { card, channel } => {
                    out.insert((card.clone(), channel.get()));
                }
                p if p.is_destination() => stack.push(p),
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as Map;
    use tessera_control_types::{Channel, DeviceKey, Gain};
    use tessera_state::config::{Backoff, Resampler};
    use tessera_state::model::WireDeviceId;

    fn ch(n: u16) -> Channel {
        Channel::new(n).unwrap()
    }
    fn dk() -> DeviceKey {
        DeviceKey("main".into())
    }
    fn dev() -> DeviceModel {
        DeviceModel {
            key: dk(),
            wire_device_id: WireDeviceId([1; 8]),
            name: "n".into(),
            sample_rate: 48_000,
            encoding: 24,
            latency_us: 4_000,
            rx_channels: 2,
            tx_channels: 2,
            rx_labels: Map::new(),
            tx_labels: Map::new(),
            subscriptions: Map::new(),
            locked: BTreeSet::new(),
            latency_bounds_us: [1_000, 40_000],
            allow_unsafe_latency: false,
            latency_floor_us: 1_000,
        }
    }
    fn rx(n: u16) -> RouterPort {
        RouterPort::NetworkRx {
            device: dk(),
            channel: ch(n),
        }
    }
    fn tx(n: u16) -> RouterPort {
        RouterPort::NetworkTx {
            device: dk(),
            channel: ch(n),
        }
    }
    fn host(n: u16) -> RouterPort {
        RouterPort::HostOut {
            device: dk(),
            channel: ch(n),
        }
    }
    fn app(n: u16) -> RouterPort {
        RouterPort::AppOut {
            device: dk(),
            channel: ch(n),
        }
    }
    fn card_in(n: u16) -> RouterPort {
        RouterPort::CardIn {
            card: "hw:CARD=CQ,DEV=0".into(),
            channel: ch(n),
        }
    }
    pub(crate) fn bridge(local: bool, dir: BridgeDirection) -> BridgePlan {
        BridgePlan {
            name: "cq".into(),
            device: "main".into(),
            pcm: "hw:CARD=CQ,DEV=0".into(),
            direction: dir,
            first_channel: 1,
            channels: 2,
            card_first_channel: 15,
            local_only: local,
            rate: 48_000,
            period_frames: 96,
            buffer_frames: 384,
            quality: Resampler::Cubic,
            latency_us: 4_000,
            latency_max_us: 40_000,
            rt_priority: None,
            cpu: None,
            time_constant_s: 20.0,
            max_correction_ppm: 100.0,
            backoff: Backoff {
                doubling: true,
                min_ms: 500,
                max_ms: 30_000,
            },
            error_sleep: Backoff {
                doubling: true,
                min_ms: 1,
                max_ms: 100,
            },
        }
    }
    fn x(s: RouterPort, d: RouterPort, o: CrosspointOrigin) -> (Crosspoint, CrosspointOrigin) {
        (Crosspoint::new(s, d), o)
    }

    fn run(
        grid: &[(Crosspoint, CrosspointOrigin)],
        bridges: &[BridgePlan],
    ) -> (RouteTable, Vec<String>) {
        let devices = [dev()];
        let delays = Map::new();
        compile(Inputs {
            devices: &devices,
            monitor_delay_us: &delays,
            crosspoints: grid,
            port_levels: &[],
            bridges,
            tick_ns: 1_000_000,
        })
    }

    #[test]
    fn the_default_grid_is_identity_plus_monitor_taps() {
        use CrosspointOrigin::{Default as D, Monitor as M};
        let grid = vec![
            x(rx(1), host(1), D),
            x(rx(2), host(2), D),
            x(app(1), tx(1), D),
            x(app(2), tx(2), D),
            x(tx(2), host(1), M),
        ];
        let (t, w) = run(&grid, &[]);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(t.tx.len(), 2);
        assert_eq!(t.tx[0].terms.len(), 1);
        assert_eq!(
            t.tx[0].terms[0].offset, 0,
            "TX reads its ring at the packet"
        );
        let h1 =
            t.rx.iter()
                .find(|d| {
                    d.out
                        == OutSpec::HostOut {
                            device: "main".into(),
                            channel: 0,
                        }
                })
                .unwrap();
        assert_eq!(h1.level, Some(WordKey::Output("main".into(), 0)));
        assert_eq!(h1.terms.len(), 2);
        assert_eq!(
            h1.terms[0].src,
            SrcSpec::Bus {
                device: "main".into(),
                channel: 0
            }
        );
        assert_eq!(h1.terms[0].offset, 0);
        // The monitor tap: TX 2's ring, scaled by TX 2's gain (the tapped
        // destination's level), at the playback position (an app writes ahead).
        let m = &h1.terms[1];
        assert_eq!(
            m.src,
            SrcSpec::Ring {
                device: "main".into(),
                owner: RingOwner::App,
                channel: 1
            }
        );
        assert_eq!(m.offset, 0);
        assert!(
            m.words
                .contains(&WordKey::Channel("main".into(), Direction::Tx, 1))
        );
        assert!(m.words.contains(&WordKey::Port(app(2))));
        assert_eq!(t.seeds.len(), 5);
    }

    #[test]
    fn a_local_card_input_mixes_into_the_speaker_ahead_by_the_bridge_margin() {
        // Speaker L = network RX 1 + CQ in 15, no network hop.
        let grid = vec![
            x(rx(1), host(1), CrosspointOrigin::Saved),
            x(card_in(15), host(1), CrosspointOrigin::Saved),
        ];
        let b = bridge(true, BridgeDirection::Capture);
        let (t, w) = run(&grid, std::slice::from_ref(&b));
        assert!(w.is_empty(), "{w:?}");
        let h = &t.rx[0];
        let card = h
            .terms
            .iter()
            .find(|t| matches!(t.src, SrcSpec::Ring { .. }))
            .unwrap();
        assert_eq!(
            card.src,
            SrcSpec::Ring {
                device: "main".into(),
                owner: RingOwner::Card("hw:CARD=CQ,DEV=0".into()),
                channel: 14
            }
        );
        let want = ns_to_frames(crate::runtime::monitor_offset_ns(0, 4_000, &[&b]), 48_000);
        assert_eq!(card.offset, want);
        assert!(card.offset > 0, "as new as the bridge writes it");
        // Without its bridge the card input is not mixed.
        let (t, _) = run(&grid, &[]);
        assert_eq!(t.rx[0].terms.len(), 1);
    }

    #[test]
    fn a_network_bridge_feeds_its_device_channels_and_reads_host_outputs() {
        let grid = vec![
            x(
                card_in(15),
                tx(1),
                CrosspointOrigin::Bridge { name: "cq".into() },
            ),
            x(rx(1), host(1), CrosspointOrigin::Default),
            x(
                host(1),
                RouterPort::CardOut {
                    card: "hw:CARD=CQ,DEV=0".into(),
                    channel: ch(15),
                },
                CrosspointOrigin::Bridge { name: "out".into() },
            ),
        ];
        let cap = bridge(false, BridgeDirection::Capture);
        let mut pb = bridge(false, BridgeDirection::Playback);
        pb.name = "out".into();
        let (t, _) = run(&grid, &[cap, pb]);
        let tx1 = &t.tx[0];
        assert_eq!(
            tx1.terms[0].src,
            SrcSpec::Ring {
                device: "main".into(),
                owner: RingOwner::Bridge,
                channel: 0
            }
        );
        assert_eq!(tx1.terms[0].words, vec![WordKey::Port(card_in(15))]);
        let out =
            t.rx.iter()
                .find(|d| matches!(d.out, OutSpec::CardOut { .. }))
                .unwrap();
        assert_eq!(
            out.out,
            OutSpec::CardOut {
                device: "main".into(),
                owner: RingOwner::Bridge,
                channel: 0
            }
        );
        // host 1 tapped: RX 1's bus scaled by the output stage and the
        // default crosspoint.
        assert_eq!(out.terms.len(), 1);
        assert!(
            out.terms[0]
                .words
                .contains(&WordKey::Output("main".into(), 0))
        );
    }

    #[test]
    fn network_audio_into_tx_reads_a_latency_and_two_ticks_back() {
        let mut g = x(rx(1), tx(2), CrosspointOrigin::Saved);
        g.0.gain = Gain::MilliDb(-6_000);
        let (t, _) = run(&[g], &[]);
        assert_eq!(t.tx[0].terms[0].offset, -(192 + 96));
        assert_eq!(
            t.seeds,
            vec![(
                WordKey::Crosspoint(CrosspointKey::new(rx(1), tx(2))),
                Gain::MilliDb(-6_000),
                false
            )]
        );
    }

    #[test]
    fn local_listeners_follow_taps() {
        let grid = vec![
            x(
                card_in(15),
                tx(1),
                CrosspointOrigin::Bridge { name: "cq".into() },
            ),
            x(
                card_in(16),
                tx(2),
                CrosspointOrigin::Bridge { name: "cq".into() },
            ),
            x(tx(1), host(1), CrosspointOrigin::Monitor),
        ];
        let l = local_listeners(&grid);
        assert_eq!(l, BTreeSet::from([("hw:CARD=CQ,DEV=0".to_owned(), 15)]));
    }

    #[test]
    fn the_bus_holds_the_largest_latency_and_a_few_ticks() {
        let (t, _) = run(&[], &[]);
        assert_eq!(t.timelines[0].latency_frames, 192);
        assert_eq!(t.timelines[0].bus_frames, 4096);
    }
}
