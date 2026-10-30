//! Receive-flow planner (`state-machines.md` §2, `control-flows.md`,
//! `flows-media.md` §§4-5).
//!
//! The saved subscriptions are resolved through the discovery snapshot
//! (`_netaudio-chan` records: address, flow-control port, channel id, rate,
//! encoding, fpp). Every local input gets a flow of its own, so a change to
//! one input never alters the flow, port or media plan of another: the
//! engine keeps unchanged plans running (owner requirement; `metering.md`
//! §2.6 item 8, one unicast flow per transmitter, is not followed for that
//! reason). A transmitter channel chosen by two inputs is requested once per
//! input. A changed input is requested on a fresh port first; the complete
//! flow set is then replaced in one `SetProtocolFlows`, and only after that
//! is the old flow stopped (make before break). A failed request keeps the
//! old flow and is retried after [`RETRY`]. A flow without media past
//! [`DEAD_AFTER`] is requested again.
//!
//! Multicast (`state-machines.md` §2, "Documented receiver behaviour"): a
//! channel whose record names a bundle (`b.<N>=<position>`,
//! `discovery-mdns.md` §3.3) that resolves (`_netaudio-bund`, §3.4) with the
//! receiver's own rate and encoding is taken from that multicast flow
//! instead of being requested: the input's flow listens on the group (bound
//! and joined, no request, no keepalives) and takes the channel's position;
//! the other positions are unused. Inputs keep one flow each, so two inputs
//! on one bundle are two independent listeners of the group. An input that
//! is received by unicast moves make before break: a probe (a listener of
//! the group that feeds no input) runs beside the unicast flow, and only
//! once the probe has media does the input move to the group and the
//! unicast flow get stopped. When the bundle is withdrawn (record gone, or
//! the channel record without its `b.<N>`), or the group delivers nothing
//! for [`DEAD_AFTER`] (Tessera policy), the input goes back to unicast: the
//! unicast flow is requested first and the group flow kept until it is
//! accepted, unless the group flow is dead. A group that went silent for an
//! input is not tried again for [`QUIET_HOLD`].
//!
//! Flow names are `<incarnation>-<slot>`: a transmitter in this daemon
//! retires sessions of an older incarnation of the same receiver, which a
//! restarted receiver could not stop.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tessera_control_types::{
    Channel, DeviceKey, Direction, Encoding, FlowSpec, Query, Reply, SampleRate, SubscriptionState,
};
use tessera_discovery::provisional::{
    BUND_KEY_ADDR, BUND_KEY_ENC, BUND_KEY_NCHAN, BUND_KEY_PORT, BUND_KEY_RATE,
    CHAN_KEY_BUNDLE_PREFIX,
};
use tessera_discovery::{ServiceInstance, ServiceKind, ServiceObservation};

use super::flows::FlowPort;
use super::wire::{self, FlowReply};
use super::{Shared, discovery};
use crate::engine_thread::EngineHandle;
use crate::media::stats::MediaStats;

/// Retry a refused or unanswered request (a hardware receiver retries every
/// 5 s, `control-flows.md` §7.3).
const RETRY: Duration = Duration::from_secs(5);
/// A flow older than this whose input has no media is requested again
/// (`state-machines.md` §2 supervision); a group or probe that delivers
/// nothing for this long sends its input to unicast.
const DEAD_AFTER: Duration = Duration::from_secs(6);
/// A group that went silent for an input is not tried again for this long
/// (Tessera policy); the next attempt is a probe beside the unicast flow.
const QUIET_HOLD: Duration = Duration::from_secs(30);
/// Planner cadence when nothing wakes it.
const PASS_EVERY: Duration = Duration::from_secs(1);
/// Local receive ports, cycled: the range advertised in ARC 0x3300.
const PORT_BASE: u16 = wire::RX_PORT_FIRST;
const PORT_SPAN: u16 = wire::RX_PORT_LAST - wire::RX_PORT_FIRST + 1;
/// Payload budget for frames per packet (`control-flows.md` §1).
const MTU_PAYLOAD: usize = 1400;
/// Start code when a channel record does not advertise `dbcp1` (§7.1).
const DEFAULT_START: u16 = 0x1102;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Member {
    rx: u16,
    tx_channel: String,
    tx_device: String,
    tx_id: u16,
}

/// A multicast flow that carries a subscribed channel
/// (`discovery-mdns.md` §§3.3-3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    /// The bundle record's instance, `<N>@<host>`.
    bundle: String,
    /// Group address and port (`a.0`, `p.0`).
    addr: SocketAddrV4,
    /// The transmitter's unicast address: the only sender accepted.
    sender: Ipv4Addr,
    /// 1-based wire position of the channel (`b.<N>=<position>`).
    position: u16,
    /// Channels per packet (`nchan`).
    width: u16,
}

impl Group {
    /// A listener of the group: `input` at the channel's position, every
    /// other position unused; `None` feeds no input (a probe).
    fn spec(&self, id: String, device: &DeviceKey, input: Option<u16>) -> FlowSpec {
        let mut s = FlowSpec::new(id, device.clone(), Direction::Rx, self.addr.port());
        s.local_address = Some(self.addr.ip().to_string());
        s.peer = Some(self.sender.to_string());
        s.channels = vec![0; usize::from(self.width)];
        if let Some(rx) = input {
            if let Some(at) = s.channels.get_mut(usize::from(self.position) - 1) {
                *at = rx;
            }
        }
        s
    }
}

/// How a flow reaches its input.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Via {
    /// A unicast flow the transmitter accepted: its flow-control endpoint and
    /// start code, our receive port, the handle for stop requests.
    Unicast {
        source: SocketAddrV4,
        start: u16,
        port: u16,
        handle: Vec<u8>,
    },
    /// A listener of a multicast group.
    Group(Group),
}

#[derive(Debug, Clone)]
struct Flow {
    slot: u16,
    member: Member,
    via: Via,
    since: Instant,
}

impl Flow {
    fn spec_id(&self) -> String {
        spec_id(self.slot)
    }

    fn spec(&self, device: &DeviceKey) -> FlowSpec {
        match &self.via {
            Via::Unicast { source, port, .. } => {
                let mut s = FlowSpec::new(self.spec_id(), device.clone(), Direction::Rx, *port);
                s.channels = vec![self.member.rx];
                s.peer = Some(source.ip().to_string());
                s
            }
            Via::Group(g) => g.spec(self.spec_id(), device, Some(self.member.rx)),
        }
    }

    fn group(&self) -> Option<&Group> {
        match &self.via {
            Via::Group(g) => Some(g),
            Via::Unicast { .. } => None,
        }
    }
}

/// A listener of a group beside an input's unicast flow, before the input
/// moves to the group.
#[derive(Debug, Clone)]
struct Probe {
    slot: u16,
    group: Group,
    since: Instant,
}

fn spec_id(slot: u16) -> String {
    format!("native-rx-{slot}")
}

/// A transmitter as its channel record describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Source {
    flow: SocketAddrV4,
    start: u16,
    rate: Option<u32>,
    bits: Option<u8>,
    fpp: Option<u16>,
}

/// The record `instance` of `kind` (instance names compare ignoring case).
fn record<'a>(
    observations: &'a [ServiceObservation],
    kind: ServiceKind,
    instance: &str,
) -> Option<&'a ServiceInstance> {
    observations.iter().find_map(|o| {
        (o.service.id.kind == kind && o.service.id.instance.eq_ignore_ascii_case(instance))
            .then_some(&o.service)
    })
}

/// The first usable IPv4 address of a resolved service.
fn address(service: &ServiceInstance) -> Option<Ipv4Addr> {
    service.addresses.iter().find_map(|a| match a {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(*v4),
        _ => None,
    })
}

fn resolve(service: &ServiceInstance) -> Option<(Source, u16)> {
    let address = address(service)?;
    let id: u16 = service.txt.get_str("id")?.parse().ok().filter(|i| *i > 0)?;
    let txt = &service.txt;
    Some((
        Source {
            flow: SocketAddrV4::new(address, service.port),
            start: txt
                .get_str("dbcp1")
                .and_then(wire::hex16)
                .unwrap_or(DEFAULT_START),
            rate: txt.get_str("rate").and_then(|r| r.parse().ok()),
            bits: txt.get_str("enc").and_then(|e| e.parse().ok()),
            fpp: txt.get_str("fpp").and_then(wire::advertised_fpp),
        },
        id,
    ))
}

/// The multicast flow that carries the channel of `channel` (its record),
/// if a `b.<N>` key names a bundle whose record resolves with the
/// receiver's own `rate` and `bits` (Tessera's compatibility check,
/// `state-machines.md` §2), a multicast group address and port, and room
/// for the channel's position. The first such key wins (the reference stops
/// at the first `b.` key, `discovery-mdns.md` §3.3).
fn bundle(
    observations: &[ServiceObservation],
    channel: &ServiceInstance,
    rate: u32,
    bits: u8,
) -> Option<Group> {
    let (_, host) = channel.id.instance.rsplit_once('@')?;
    let sender = address(channel)?;
    channel.txt.entries().iter().find_map(|e| {
        let prefix = e.key.get(..CHAN_KEY_BUNDLE_PREFIX.len())?;
        if !prefix.eq_ignore_ascii_case(CHAN_KEY_BUNDLE_PREFIX) {
            return None;
        }
        let n: u32 = e.key.get(CHAN_KEY_BUNDLE_PREFIX.len()..)?.parse().ok()?;
        let position: u16 = e.value_str()?.trim().parse().ok().filter(|p| *p > 0)?;
        let instance = format!("{n}@{host}");
        let txt = &record(observations, ServiceKind::Bund, &instance)?.txt;
        let number = |key: &str| txt.get_str(key)?.trim().parse::<u32>().ok();
        if number(BUND_KEY_RATE)? != rate || number(BUND_KEY_ENC)? != u32::from(bits) {
            return None;
        }
        let width = u16::try_from(number(BUND_KEY_NCHAN)?)
            .ok()
            .filter(|w| *w >= position)?;
        let group: Ipv4Addr = txt
            .get_str(BUND_KEY_ADDR)?
            .trim()
            .parse()
            .ok()
            .filter(Ipv4Addr::is_multicast)?;
        let port = u16::try_from(number(BUND_KEY_PORT)?)
            .ok()
            .filter(|p| *p != 0)?;
        Some(Group {
            bundle: instance,
            addr: SocketAddrV4::new(group, port),
            sender,
            position,
            width,
        })
    })
}

/// What to do for one input this pass.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// Keep the input's flow as it is.
    Keep,
    /// Request a unicast flow (new input, changed source, dead flow, or back
    /// from a group).
    Unicast,
    /// Listen on the group for the input at once: nothing worth keeping
    /// plays.
    Join(Group),
    /// Keep the unicast flow and listen on the group beside it.
    Probe(Group),
    /// The probe has media: move the input to the group, then stop the
    /// unicast flow.
    Migrate(Group),
}

/// The step for an input that wants `member`, from `unicast` (its
/// transmitter's flow-control endpoint, if resolved) or from `group` (a
/// usable bundle, not held off), given its current `flow` (`dead`: no media
/// past [`DEAD_AFTER`]) and its probe (`heard`: the probe has media).
fn decide(
    member: &Member,
    unicast: Option<SocketAddrV4>,
    group: Option<&Group>,
    flow: Option<&Flow>,
    dead: bool,
    probe: Option<(&Group, bool)>,
) -> Step {
    let current = flow.filter(|f| f.member == *member);
    match (group, current) {
        (Some(g), Some(f)) if f.group() == Some(g) => {
            if dead {
                Step::Unicast
            } else {
                Step::Keep
            }
        }
        (Some(g), Some(f)) if f.group().is_none() && !dead => match probe {
            Some((p, true)) if p == g => Step::Migrate(g.clone()),
            _ => Step::Probe(g.clone()),
        },
        (Some(g), _) => Step::Join(g.clone()),
        (None, Some(f)) => match &f.via {
            Via::Unicast { source, .. } if Some(*source) == unicast && !dead => Step::Keep,
            _ => Step::Unicast,
        },
        (None, None) => Step::Unicast,
    }
}

/// What an input keeps while its unicast request is pending or refused: its
/// flow (make before break), unless that is a dead group listener.
fn kept(flow: Option<Flow>, dead: bool) -> Option<Flow> {
    flow.filter(|f| f.group().is_none() || !dead)
}

/// A group flow that is dead (silent or not running past [`DEAD_AFTER`]),
/// or a probe that has heard nothing in as long, holds that group off for
/// its input; a hold expires after [`QUIET_HOLD`].
#[derive(Debug, Default)]
struct Quiet(BTreeMap<u16, (String, Instant)>);

impl Quiet {
    fn hold(&mut self, rx: u16, bundle: &str, now: Instant) {
        self.0.insert(rx, (bundle.to_owned(), now));
    }

    fn holds(&self, rx: u16, group: &Group, now: Instant) -> bool {
        self.0
            .get(&rx)
            .is_some_and(|(b, at)| *b == group.bundle && now.duration_since(*at) < QUIET_HOLD)
    }

    fn expire(&mut self, now: Instant) {
        self.0
            .retain(|_, (_, at)| now.duration_since(*at) < QUIET_HOLD);
    }
}

/// The flows for ARC 0x3200 (`control-arc.md` §4.13), in id order: a unicast
/// flow per input; the listeners of one group as one flow whose positions
/// name every input they feed. A flow keeps the id it had in `before` for as
/// long as one of its listeners runs, so that its heartbeat slot (id - 1)
/// stays put while other flows come and go (`metering.md` §2.6 item 5); a
/// new flow takes the lowest id no kept flow holds.
fn views(
    flows: &BTreeMap<u16, Flow>,
    local: Ipv4Addr,
    before: &[wire::RxFlowView],
) -> Vec<wire::RxFlowView> {
    let mut views: Vec<wire::RxFlowView> = Vec::new();
    let mut groups: Vec<(SocketAddrV4, usize)> = Vec::new();
    for f in flows.values() {
        match &f.via {
            Via::Unicast { port, .. } => views.push(wire::RxFlowView {
                id: 0,
                flows: vec![f.spec_id()],
                local,
                port: *port,
                positions: vec![vec![f.member.rx]],
            }),
            Via::Group(g) => {
                let at = if let Some((_, at)) = groups.iter().find(|(a, _)| *a == g.addr) {
                    *at
                } else {
                    views.push(wire::RxFlowView {
                        id: 0,
                        flows: Vec::new(),
                        local,
                        port: g.addr.port(),
                        positions: vec![Vec::new(); usize::from(g.width)],
                    });
                    groups.push((g.addr, views.len() - 1));
                    views.len() - 1
                };
                if let Some(v) = views.get_mut(at) {
                    v.flows.push(f.spec_id());
                    if let Some(p) = v.positions.get_mut(usize::from(g.position) - 1) {
                        p.push(f.member.rx);
                    }
                }
            }
        }
    }
    let mut taken = BTreeSet::new();
    for v in &mut views {
        let kept = before
            .iter()
            .find(|b| b.flows.iter().any(|f| v.flows.contains(f)))
            .map(|b| b.id);
        if let Some(id) = kept.filter(|id| taken.insert(*id)) {
            v.id = id;
        }
    }
    for v in views.iter_mut().filter(|v| v.id == 0) {
        if let Some(id) = (1..=u16::MAX).find(|id| !taken.contains(id)) {
            taken.insert(id);
            v.id = id;
        }
    }
    views.sort_by_key(|v| v.id);
    views
}

struct Planner {
    engine: EngineHandle,
    shared: Arc<Shared>,
    port: Arc<FlowPort>,
    media: Arc<MediaStats>,
    address: Ipv4Addr,
    incarnation: String,
    sequence: u16,
    slot: u16,
    next_port: u16,
    /// One flow per local input, by RX channel.
    flows: BTreeMap<u16, Flow>,
    /// Probes beside unicast flows, by RX channel.
    probes: BTreeMap<u16, Probe>,
    quiet: Quiet,
    /// Last failed attempt per input, with the member it was for.
    attempts: BTreeMap<u16, (Instant, Member, Option<SubscriptionState>)>,
    installed: bool,
    /// The flows as last listed for ARC 0x3200, with their ids.
    listed: Vec<wire::RxFlowView>,
}

pub(super) async fn run(
    engine: EngineHandle,
    shared: Arc<Shared>,
    port: Arc<FlowPort>,
    address: Ipv4Addr,
    media: Arc<MediaStats>,
) {
    let mut nonce = [0u8; 4];
    let _ = getrandom::fill(&mut nonce);
    let mut p = Planner {
        engine,
        shared,
        port,
        media,
        address,
        incarnation: nonce.iter().map(|b| format!("{b:02x}")).collect(),
        sequence: 0,
        slot: 0,
        next_port: 0,
        flows: BTreeMap::new(),
        probes: BTreeMap::new(),
        quiet: Quiet::default(),
        attempts: BTreeMap::new(),
        installed: false,
        listed: Vec::new(),
    };
    loop {
        p.pass().await;
        tokio::select! {
            () = p.shared.changed.notified() => {}
            () = tokio::time::sleep(PASS_EVERY) => {}
        }
    }
}

impl Planner {
    fn next_port(&mut self, taken: &[u16]) -> u16 {
        for _ in 0..PORT_SPAN {
            let port = PORT_BASE + self.next_port % PORT_SPAN;
            self.next_port = self.next_port.wrapping_add(1);
            if !taken.contains(&port) {
                return port;
            }
        }
        PORT_BASE
    }

    /// A slot no flow or probe uses (the slot names the flow in the set).
    fn next_slot(&mut self, used: &mut BTreeSet<u16>) -> u16 {
        loop {
            self.slot = self.slot.wrapping_add(1).max(1);
            if used.insert(self.slot) {
                return self.slot;
            }
        }
    }

    /// Whether `rx`'s flow `f` is dead: older than [`DEAD_AFTER`] and without
    /// media; a group listener also when it is not running.
    fn dead(&self, f: &Flow, state: Option<&SubscriptionState>, now: Instant) -> bool {
        now.duration_since(f.since) > DEAD_AFTER
            && (matches!(state, Some(SubscriptionState::NoMedia))
                || (f.group().is_some() && self.media.rx_packets(&f.spec_id()).is_none()))
    }

    async fn pass(&mut self) {
        let Ok(rp) = self.engine.runtime_plan().await else {
            return;
        };
        let Some(d) = rp.model.devices.first().cloned() else {
            return;
        };
        // Never plan from an empty picture: at start the stored flows keep
        // playing until discovery can resolve their transmitters.
        let Some(snapshot) = discovery().filter(|s| !s.observations.is_empty()) else {
            return;
        };
        let states: BTreeMap<u16, SubscriptionState> = match self.engine.query(Query::Status).await
        {
            Ok(Reply::Status(s)) => s
                .devices
                .into_iter()
                .find(|x| x.key == d.key)
                .map(|x| {
                    x.subscriptions
                        .into_iter()
                        .map(|s| (s.rx_channel.get(), s.state))
                        .collect()
                })
                .unwrap_or_default(),
            _ => BTreeMap::new(),
        };
        let now = Instant::now();
        self.quiet.expire(now);
        let mut used: BTreeSet<u16> = self
            .flows
            .values()
            .map(|f| f.slot)
            .chain(self.probes.values().map(|p| p.slot))
            .collect();

        let mut next: BTreeMap<u16, Flow> = BTreeMap::new();
        let mut probes: BTreeMap<u16, Probe> = BTreeMap::new();
        let mut created: Vec<Flow> = Vec::new();
        let mut retired: Vec<Flow> = Vec::new();
        let mut problems: Vec<(u16, SubscriptionState)> = Vec::new();
        for (rx, sub) in &d.subscriptions {
            let rx = *rx;
            let existing = self.flows.get(&rx).cloned();
            let found = record(
                &snapshot.observations,
                ServiceKind::Chan,
                &format!("{}@{}", sub.tx_channel, sub.tx_device),
            );
            let same_source = |f: &&Flow| {
                f.member.tx_channel == sub.tx_channel && f.member.tx_device == sub.tx_device
            };
            let (source, tx_id, group) = match found.and_then(|r| Some((r, resolve(r)?))) {
                Some((service, (source, tx_id))) => {
                    if let Some(theirs) = source.rate.filter(|r| *r != d.sample_rate) {
                        let (ours, theirs) = (SampleRate(d.sample_rate), SampleRate(theirs));
                        problems.push((rx, SubscriptionState::RateMismatch { ours, theirs }));
                        continue;
                    }
                    if let Some(theirs) = source.bits.filter(|b| *b != d.encoding) {
                        let (ours, theirs) = (Encoding(d.encoding), Encoding(theirs));
                        problems.push((rx, SubscriptionState::EncodingMismatch { ours, theirs }));
                        continue;
                    }
                    let group = bundle(&snapshot.observations, service, d.sample_rate, d.encoding);
                    (Some(source), tx_id, group)
                }
                // Keep a running flow through a discovery gap.
                None => match existing.as_ref().filter(same_source) {
                    Some(f) => match &f.via {
                        Via::Unicast { source, start, .. } => (
                            Some(Source {
                                flow: *source,
                                start: *start,
                                rate: None,
                                bits: None,
                                fpp: None,
                            }),
                            f.member.tx_id,
                            None,
                        ),
                        Via::Group(g) => (None, f.member.tx_id, Some(g.clone())),
                    },
                    None => {
                        problems.push((rx, SubscriptionState::Unresolved));
                        continue;
                    }
                },
            };
            let member = Member {
                rx,
                tx_channel: sub.tx_channel.clone(),
                tx_device: sub.tx_device.clone(),
                tx_id,
            };
            let dead = existing
                .as_ref()
                .is_some_and(|f| self.dead(f, states.get(&rx), now));
            // A silent group, or a probe that heard nothing, holds the
            // group off for this input.
            if let Some(g) = existing.as_ref().filter(|_| dead).and_then(Flow::group) {
                tracing::warn!(
                    "multicast flow {} delivers nothing to RX {rx}; back to unicast",
                    g.bundle
                );
                self.quiet.hold(rx, &g.bundle, now);
            }
            let mut probe = self.probes.get(&rx).cloned();
            let heard = probe.as_ref().is_some_and(|p| {
                self.media
                    .rx_packets(&spec_id(p.slot))
                    .is_some_and(|n| n > 0)
            });
            if let Some(p) = probe
                .as_ref()
                .filter(|p| !heard && now.duration_since(p.since) > DEAD_AFTER)
            {
                tracing::warn!(
                    "multicast flow {} delivers nothing; RX {rx} stays on unicast",
                    p.group.bundle
                );
                self.quiet.hold(rx, &p.group.bundle, now);
                probe = None;
            }
            let group = group.filter(|g| !self.quiet.holds(rx, g, now));
            let step = decide(
                &member,
                source.map(|s| s.flow),
                group.as_ref(),
                existing.as_ref(),
                dead,
                probe.as_ref().map(|p| (&p.group, heard)),
            );
            match step {
                Step::Keep => {
                    if let Some(f) = existing {
                        next.insert(rx, f);
                    }
                }
                Step::Join(g) | Step::Migrate(g) => {
                    tracing::info!(
                        "RX {rx} takes TX {} from multicast flow {} ({}, position {})",
                        member.tx_id,
                        g.bundle,
                        g.addr,
                        g.position
                    );
                    let flow = Flow {
                        slot: self.next_slot(&mut used),
                        member,
                        via: Via::Group(g),
                        since: now,
                    };
                    self.attempts.remove(&rx);
                    created.push(flow.clone());
                    next.insert(rx, flow);
                }
                Step::Probe(g) => {
                    let p = match probe.filter(|p| p.group == g) {
                        Some(p) => p,
                        None => Probe {
                            slot: self.next_slot(&mut used),
                            group: g,
                            since: now,
                        },
                    };
                    probes.insert(rx, p);
                    if let Some(f) = existing {
                        next.insert(rx, f);
                    }
                }
                Step::Unicast => {
                    let keep = kept(existing, dead);
                    let Some(source) = source else {
                        problems.push((rx, SubscriptionState::Unresolved));
                        if let Some(f) = keep {
                            next.insert(rx, f);
                        }
                        continue;
                    };
                    if let Some((_, _, failure)) = self
                        .attempts
                        .get(&rx)
                        .filter(|(at, m, _)| now.duration_since(*at) < RETRY && *m == member)
                    {
                        if let Some(state) = failure {
                            problems.push((rx, state.clone()));
                        }
                        if let Some(f) = keep {
                            next.insert(rx, f);
                        }
                        continue;
                    }
                    match self
                        .request(&d, &member, source, &next, &created, &mut used, now)
                        .await
                    {
                        Ok(flow) => {
                            self.attempts.remove(&rx);
                            created.push(flow.clone());
                            next.insert(rx, flow);
                        }
                        Err(state) => {
                            if let Some(state) = &state {
                                problems.push((rx, state.clone()));
                            }
                            self.attempts.insert(rx, (now, member, state));
                            if let Some(f) = keep {
                                next.insert(rx, f);
                            }
                        }
                    }
                }
            }
        }
        for (rx, flow) in &self.flows {
            if next.get(rx).is_none_or(|n| n.slot != flow.slot) {
                retired.push(flow.clone());
            }
        }
        let probes_changed = probes.len() != self.probes.len()
            || probes
                .iter()
                .any(|(rx, p)| self.probes.get(rx).is_none_or(|q| q.slot != p.slot));

        if !created.is_empty() || !retired.is_empty() || probes_changed || !self.installed {
            let specs: Vec<FlowSpec> = next
                .values()
                .map(|f| f.spec(&d.key))
                .chain(
                    probes
                        .values()
                        .map(|p| p.group.spec(spec_id(p.slot), &d.key, None)),
                )
                .collect();
            match self.shared.install(&self.engine, Some(specs), None).await {
                Ok(()) => {
                    self.installed = true;
                    self.flows = next;
                    self.probes = probes;
                    // Make before break: a replaced unicast flow is stopped
                    // only now that its successor is installed.
                    for flow in &retired {
                        self.stop(flow).await;
                    }
                    let touched: Vec<u16> = created
                        .iter()
                        .chain(&retired)
                        .map(|f| f.member.rx)
                        .collect();
                    if !touched.is_empty() {
                        self.shared.notify_channels(touched);
                    }
                }
                Err(e) => {
                    tracing::warn!("cannot install native receive flows: {e}");
                    for flow in &created {
                        self.stop(flow).await;
                        self.attempts
                            .insert(flow.member.rx, (now, flow.member.clone(), None));
                    }
                }
            }
        } else {
            self.flows = next;
        }
        self.listed = views(&self.flows, self.address, &self.listed);
        if let Ok(mut v) = self.shared.flows.lock() {
            *v = self.listed.clone();
        }
        super::set_group_inputs(
            self.flows
                .values()
                .filter(|f| f.group().is_some())
                .map(|f| f.member.rx)
                .collect(),
        );
        let rows: Vec<(DeviceKey, Channel, SubscriptionState)> = problems
            .into_iter()
            .filter_map(|(rx, s)| Some((d.key.clone(), Channel::new(rx)?, s)))
            .collect();
        if !rows.is_empty() {
            self.engine.report_subscription_states(rows);
        }
    }

    /// Request a unicast flow of `member` from `source` on a fresh port
    /// (`control-flows.md` §1); a refusal or no answer is the state to
    /// report, if any.
    #[allow(clippy::too_many_arguments)]
    async fn request(
        &mut self,
        d: &tessera_state::model::DeviceModel,
        member: &Member,
        source: Source,
        next: &BTreeMap<u16, Flow>,
        created: &[Flow],
        used: &mut BTreeSet<u16>,
        now: Instant,
    ) -> Result<Flow, Option<SubscriptionState>> {
        let taken: Vec<u16> = self
            .flows
            .values()
            .chain(next.values())
            .chain(created)
            .filter_map(|f| match f.via {
                Via::Unicast { port, .. } => Some(port),
                Via::Group(_) => None,
            })
            .collect();
        let port = self.next_port(&taken);
        let slot = self.next_slot(used);
        let bytes = usize::from(d.encoding / 8).max(1);
        let budget = MTU_PAYLOAD / bytes;
        let fpp = source
            .fpp
            .map_or(budget, |f| usize::from(f).min(budget))
            .min(usize::from(u16::MAX)) as u16;
        let flow_name = format!("{}-{slot}", self.incarnation);
        self.sequence = self.sequence.wrapping_add(1);
        let request = wire::flow_request(&wire::FlowRequest {
            start: source.start,
            sequence: self.sequence,
            rate: d.sample_rate,
            bits: u32::from(d.encoding),
            channels: &[member.tx_id],
            fpp,
            receiver: &d.name,
            flow_name: &flow_name,
            address: self.address,
            port,
        });
        match self
            .port
            .exchange(source.flow, &request, self.sequence, 0x0100)
            .await
        {
            Some(FlowReply::Accepted(handle)) => {
                tracing::info!(
                    "native flow {slot} from {}: id {} -> RX {} on port {port}",
                    source.flow,
                    member.tx_id,
                    member.rx
                );
                Ok(Flow {
                    slot,
                    member: member.clone(),
                    via: Via::Unicast {
                        source: source.flow,
                        start: source.start,
                        port,
                        handle,
                    },
                    since: now,
                })
            }
            Some(FlowReply::Refused(code)) => {
                tracing::warn!("flow request to {} refused: {code:#06x}", source.flow);
                Err(Some(SubscriptionState::FlowRefused { code }))
            }
            None => {
                tracing::warn!("flow request to {} unanswered", source.flow);
                Err(None)
            }
        }
    }

    /// Stop a unicast flow at its transmitter, best effort (`control-flows.md`
    /// §3). Without it the transmitter still drops the flow once keepalives
    /// stop. A group listener has nothing to stop: leaving the group is
    /// closing its socket.
    async fn stop(&mut self, flow: &Flow) {
        let Via::Unicast {
            source,
            start,
            handle,
            ..
        } = &flow.via
        else {
            return;
        };
        self.sequence = self.sequence.wrapping_add(1);
        let request = wire::flow_stop(*start, self.sequence, handle);
        let outcome = self
            .port
            .exchange(*source, &request, self.sequence, 0x0101)
            .await;
        tracing::info!(
            "native flow {} from {} stopped: {}",
            flow.slot,
            source,
            match outcome {
                Some(FlowReply::Accepted(_)) => "confirmed".to_owned(),
                Some(FlowReply::Refused(code)) => format!("refused {code:#06x}"),
                None => "unanswered".to_owned(),
            }
        );
    }
}
