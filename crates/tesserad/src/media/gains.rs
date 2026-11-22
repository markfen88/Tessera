//! Per-device-channel gain and mute in the media path (plan
//! `docs/plans/2026-11-21-hardware-control-surface.md` §2.2).
//!
//! **Where it applies.** Per *device* channel in the media worker, not in the
//! flow engines: a device channel can appear in several flows, and
//! `RxFlow::read` is indexed by flow channel.
//!
//! * RX (network → host): in the worker's `deliver()`, right after
//!   `RxFlow::read`, before the sample reaches any client ring or bridge.
//! * TX (host → network): in the worker's `TxSource::read`, right after the
//!   client ring was read into the flow's packet buffer (or the position was
//!   filled with silence because no client covers it). The ring itself is
//!   never modified, so a channel sent by two flows is scaled once per flow.
//!
//! **Sharing.** [`GainRegistry`] holds one [`ChannelGain`] (one `AtomicU32`)
//! per (device, direction, device channel), created on first use at unity.
//! The media control thread hands each new flow an `Arc` per flow position
//! ([`GainRegistry::for_flow`]); the worker only loads it (one relaxed atomic
//! load per channel and block) and never clones or drops it: a retired flow
//! goes back to the control thread, which drops its `Arc`s.
//!
//! **Smoothing.** A change of a channel's word is spread linearly over
//! [`RAMP_MS`] of samples, across as many blocks as that takes; a change
//! that arrives mid-ramp starts a new ramp from the gain reached so far.
//! The state ([`Smoother`]) is per (flow, position), so one device channel
//! feeding two flows ramps correctly in both, and it advances on every
//! block, also a silent one, so a change made while nothing plays does not
//! ramp later from a stale gain.
//!
//! **Who writes.** [`seed`] fills the registry from the state engine before
//! the daemon starts any flow (a muted channel never plays at unity), and
//! [`run`], a control-plane task, keeps it equal to the engine's channel gain
//! and mute: a full read of the status at start, after a lag and after a
//! configuration or channel change, and every `ChannelGainChanged` /
//! `ChannelMuteChanged` event in between. Nothing else writes it: the state
//! engine stays the one authority.
//!
//! **Output stage.** RX device channels also have an output word
//! ([`GainRegistry::output`], `SetOutputGain` / `SetOutputMute`): the host
//! output's level, applied by the host router after its mix (or in
//! `deliver()` for a device no router plan covers). Same smoothing, same
//! seeding and events (`OutputGainChanged` / `OutputMuteChanged`,
//! `ChannelStatus::output_*`).
//!
//! **Host router words.** Local port levels (`app_out`, `card_in`,
//! `card_out`) and crosspoint gains have words too ([`GainRegistry::word`]),
//! read from `Query::Router` with the status and kept current from
//! `PortGainChanged` / `PortMuteChanged` / `CrosspointGainChanged` /
//! `CrosspointMuteChanged`; a `RouterChanged` reads them again.
//!
//! Meters stay pre-fader (the RX peak is taken inside the flow engine).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use tessera_control_types::{
    ChannelStatus, CrosspointKey, Direction, Event, Gain, Query, Reply, RouterPort, RouterReport,
    Status,
};

use crate::router::WordKey;
use tessera_media::{ChannelGain, GainWord};
use tessera_state::{EventBus, Received, RecvError, Subscription};
use tokio_util::sync::CancellationToken;

use super::plan::UNUSED;
use crate::engine_thread::EngineHandle;

/// How long a gain or mute change takes to complete, milliseconds.
pub const RAMP_MS: u32 = 5;

/// (device key, direction, 0-based device channel).
type Key = (String, Direction, u32);

/// The gain words of every device channel the media path has used.
#[derive(Debug, Default)]
pub struct GainRegistry {
    map: Mutex<HashMap<Key, Arc<ChannelGain>>>,
    /// The output stage of RX device channels (after the local mix), by
    /// (device key, 0-based channel).
    outputs: Mutex<HashMap<(String, u32), Arc<ChannelGain>>>,
    /// Host router: local port levels (`app_out`, `card_in`, `card_out`) and
    /// crosspoint gains.
    router: Mutex<RouterWords>,
}

/// The host router's words.
#[derive(Debug, Default)]
struct RouterWords {
    ports: HashMap<RouterPort, Arc<ChannelGain>>,
    crosspoints: HashMap<CrosspointKey, Arc<ChannelGain>>,
}

/// The gain one flow position applies now and where it is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Smoother {
    /// Effective Q7.24 gain the last sample got.
    cur: u32,
    /// Effective Q7.24 gain being ramped to.
    to: u32,
    /// Samples left until `to` is reached (0: steady).
    left: u32,
}

impl Smoother {
    /// Steady at `word`'s effective gain.
    #[must_use]
    pub const fn at(word: GainWord) -> Self {
        let q = word.effective_q24();
        Self {
            cur: q,
            to: q,
            left: 0,
        }
    }

    /// Steady at unity.
    pub const UNITY: Smoother = Smoother::at(GainWord::UNITY);

    /// Silent: the next block ramps up to its word (a router term that was
    /// just patched fades in).
    pub const SILENT: Smoother = Smoother::at(GainWord::new(0, false));

    /// Is a ramp in progress?
    #[must_use]
    pub const fn ramping(&self) -> bool {
        self.left > 0
    }

    /// RT side: move towards `now`'s effective gain over `ramp` samples
    /// (from the gain reached so far when the target changes), scaling
    /// `samples`. Plain arithmetic: no allocation, lock or system call.
    #[inline]
    pub fn apply(&mut self, now: GainWord, ramp: u32, samples: &mut [i32]) {
        if samples.is_empty() {
            return;
        }
        let target = now.effective_q24();
        if target != self.to {
            self.to = target;
            self.left = ramp.max(1);
        }
        if self.left == 0 {
            GainWord::new(self.to, false).apply_i32(samples);
            return;
        }
        let m = u32::try_from(samples.len())
            .unwrap_or(u32::MAX)
            .min(self.left);
        let mid = if m == self.left {
            self.to
        } else {
            // On the line from `cur` to `to` over `left` samples.
            let (c, t) = (i64::from(self.cur), i64::from(self.to));
            let v = c + (t - c) * i64::from(m) / i64::from(self.left);
            u32::try_from(v).unwrap_or(self.to)
        };
        let Some((head, tail)) = samples.split_at_mut_checked(m as usize) else {
            return;
        };
        GainWord::ramp_i32(
            GainWord::new(self.cur, false),
            GainWord::new(mid, false),
            head,
        );
        self.cur = mid;
        self.left -= m;
        if !tail.is_empty() {
            GainWord::new(self.to, false).apply_i32(tail);
        }
    }
}

/// The gains of one flow, one entry per flow position (control thread
/// builds it; the worker reads it).
#[derive(Debug, Default)]
pub struct FlowGains {
    /// The device channel's gain word; `None` for an unused position.
    pub words: Box<[Option<Arc<ChannelGain>>]>,
    /// Per position: the gain applied now and the ramp in progress.
    smooth: Box<[Smoother]>,
    /// Ramp length, samples ([`RAMP_MS`] at the flow's rate).
    ramp: u32,
}

impl FlowGains {
    /// RT side: apply position `pos`'s gain to `samples` (smoothed, see the
    /// module documentation). No-op for a position without a word. Never
    /// allocates, locks, logs or makes a system call.
    #[inline]
    pub fn apply(&mut self, pos: usize, samples: &mut [i32]) {
        if let (Some(Some(g)), Some(s)) = (self.words.get(pos), self.smooth.get_mut(pos)) {
            s.apply(g.load(), self.ramp, samples);
        }
    }

    /// Position `pos`'s smoothing state (tests and diagnostics).
    #[must_use]
    pub fn smoother(&self, pos: usize) -> Option<Smoother> {
        self.smooth.get(pos).copied()
    }
}

/// The Q7.24 word for a contract gain.
fn set_gain(word: &ChannelGain, gain: Gain) {
    match gain.milli_db() {
        None => word.set_q24(0),
        #[allow(clippy::cast_precision_loss)]
        Some(m) => word.set_db(m as f32 / 1000.0),
    }
}

impl GainRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Arc<ChannelGain>>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The word of one device channel (0-based), created at unity.
    #[must_use]
    pub fn channel(&self, device: &str, direction: Direction, channel: u32) -> Arc<ChannelGain> {
        Arc::clone(
            self.lock()
                .entry((device.to_owned(), direction, channel))
                .or_default(),
        )
    }

    /// The output-stage word of one RX device channel (0-based), created at
    /// unity.
    #[must_use]
    pub fn output(&self, device: &str, channel: u32) -> Arc<ChannelGain> {
        Arc::clone(
            self.outputs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry((device.to_owned(), channel))
                .or_default(),
        )
    }

    /// The output-stage gains of an RX flow at `rate` Hz whose positions
    /// carry the device channels `chans` (as [`for_flow`](Self::for_flow)).
    #[must_use]
    pub fn for_output(&self, device: &str, chans: &[u32], rate: u32) -> FlowGains {
        let words: Box<[Option<Arc<ChannelGain>>]> = chans
            .iter()
            .map(|&c| (c != UNUSED).then(|| self.output(device, c)))
            .collect();
        Self::gains_of(words, rate)
    }

    /// Set the output gain (if `gain`) and mute (if `muted`) of 1-based RX
    /// `channel`.
    pub fn set_output(&self, device: &str, channel: u16, gain: Option<Gain>, muted: Option<bool>) {
        let Some(ch) = u32::from(channel).checked_sub(1) else {
            return;
        };
        let word = self.output(device, ch);
        if let Some(g) = gain {
            set_gain(&word, g);
        }
        if let Some(m) = muted {
            word.set_muted(m);
        }
    }

    /// The gains of a flow at `rate` Hz whose positions carry the device
    /// channels `chans` (0-based, [`UNUSED`] for none). Control thread only
    /// (allocates).
    #[must_use]
    pub fn for_flow(
        &self,
        device: &str,
        direction: Direction,
        chans: &[u32],
        rate: u32,
    ) -> FlowGains {
        let words: Box<[Option<Arc<ChannelGain>>]> = chans
            .iter()
            .map(|&c| (c != UNUSED).then(|| self.channel(device, direction, c)))
            .collect();
        Self::gains_of(words, rate)
    }

    fn gains_of(words: Box<[Option<Arc<ChannelGain>>]>, rate: u32) -> FlowGains {
        // Start from the current word, so a flow that starts on a muted
        // channel does not fade out from unity.
        let smooth = words
            .iter()
            .map(|w| {
                w.as_ref()
                    .map_or(Smoother::UNITY, |w| Smoother::at(w.load()))
            })
            .collect();
        FlowGains {
            words,
            smooth,
            ramp: (rate / 1000).saturating_mul(RAMP_MS).max(1),
        }
    }

    /// Set the gain (if `gain`) and the mute (if `muted`) of 1-based
    /// `channel`.
    pub fn set(
        &self,
        device: &str,
        direction: Direction,
        channel: u16,
        gain: Option<Gain>,
        muted: Option<bool>,
    ) {
        let Some(ch) = u32::from(channel).checked_sub(1) else {
            return;
        };
        let word = self.channel(device, direction, ch);
        if let Some(g) = gain {
            set_gain(&word, g);
        }
        if let Some(m) = muted {
            word.set_muted(m);
        }
    }

    fn router_words(&self) -> std::sync::MutexGuard<'_, RouterWords> {
        self.router.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The word of a router key, created at unity, or with `seed` when it
    /// does not exist yet (`None` from the second element: it existed).
    /// Control thread only (allocates).
    #[must_use]
    pub fn word(&self, key: &WordKey, seed: Option<(Gain, bool)>) -> Arc<ChannelGain> {
        let seeded = |w: &Arc<ChannelGain>| {
            if let Some((g, m)) = seed {
                set_gain(w, g);
                w.set_muted(m);
            }
        };
        match key {
            WordKey::Channel(d, dir, c) => self.channel(d, *dir, *c),
            WordKey::Output(d, c) => self.output(d, *c),
            WordKey::Port(p) => {
                let mut r = self.router_words();
                if let Some(w) = r.ports.get(p) {
                    return Arc::clone(w);
                }
                let w = Arc::new(ChannelGain::new());
                seeded(&w);
                r.ports.insert(p.clone(), Arc::clone(&w));
                w
            }
            WordKey::Crosspoint(k) => {
                let mut r = self.router_words();
                if let Some(w) = r.crosspoints.get(k) {
                    return Arc::clone(w);
                }
                let w = Arc::new(ChannelGain::new());
                seeded(&w);
                r.crosspoints.insert(k.clone(), Arc::clone(&w));
                w
            }
        }
    }

    /// Make the router's words equal to `report`: every crosspoint and port
    /// it lists takes its gain and mute, every other known one goes back to
    /// unity, unmuted.
    pub fn apply_router(&self, report: &RouterReport) {
        let mut r = self.router_words();
        let mut xs: HashMap<CrosspointKey, (Gain, bool)> = HashMap::new();
        for x in &report.crosspoints {
            xs.insert(x.crosspoint.key(), (x.crosspoint.gain, x.crosspoint.muted));
        }
        let ports: HashMap<RouterPort, (Gain, bool)> = report
            .ports
            .iter()
            .filter(|p| crate::router::is_local_level(&p.port))
            .map(|p| (p.port.clone(), (p.gain, p.muted)))
            .collect();
        for (k, w) in &r.crosspoints {
            let (g, m) = xs.get(k).copied().unwrap_or((Gain::default(), false));
            set_gain(w, g);
            w.set_muted(m);
        }
        for (k, w) in &r.ports {
            let (g, m) = ports.get(k).copied().unwrap_or((Gain::default(), false));
            set_gain(w, g);
            w.set_muted(m);
        }
        for (k, (g, m)) in xs {
            r.crosspoints.entry(k).or_insert_with(|| {
                let w = Arc::new(ChannelGain::new());
                set_gain(&w, g);
                w.set_muted(m);
                w
            });
        }
        // Ports too: a plan built later from an older table seeds only words
        // that do not exist yet, so a missing port word would start at that
        // table's (stale) level instead of this report's.
        for (k, (g, m)) in ports {
            r.ports.entry(k).or_insert_with(|| {
                let w = Arc::new(ChannelGain::new());
                set_gain(&w, g);
                w.set_muted(m);
                w
            });
        }
    }

    fn set_router_word(&self, key: WordKey, gain: Option<Gain>, muted: Option<bool>) {
        let w = self.word(&key, None);
        if let Some(g) = gain {
            set_gain(&w, g);
        }
        if let Some(m) = muted {
            w.set_muted(m);
        }
    }

    /// Make every word equal to `status`: the channels it lists take their
    /// gain and mute, every other known word goes back to unity, unmuted.
    pub fn apply_status(&self, status: &Status) {
        let mut seen: HashMap<Key, (Gain, bool)> = HashMap::new();
        let mut seen_out: HashMap<(String, u32), (Gain, bool)> = HashMap::new();
        for d in &status.devices {
            let mut put = |dir: Direction, chans: &[ChannelStatus]| {
                for c in chans {
                    if let Some(ch) = u32::from(c.channel.get()).checked_sub(1) {
                        seen.insert((d.key.0.clone(), dir, ch), (c.gain, c.muted));
                    }
                }
            };
            put(Direction::Rx, &d.rx_channels);
            put(Direction::Tx, &d.tx_channels);
            for c in &d.rx_channels {
                if let Some(ch) = u32::from(c.channel.get()).checked_sub(1) {
                    seen_out.insert((d.key.0.clone(), ch), (c.output_gain, c.output_muted));
                }
            }
        }
        {
            let mut out = self.outputs.lock().unwrap_or_else(PoisonError::into_inner);
            for (k, (gain, muted)) in &seen_out {
                let w = out.entry(k.clone()).or_default();
                set_gain(w, *gain);
                w.set_muted(*muted);
            }
            for (k, w) in out.iter() {
                if !seen_out.contains_key(k) {
                    w.set_q24(tessera_media::gain::UNITY_Q24);
                    w.set_muted(false);
                }
            }
        }
        let mut map = self.lock();
        for (k, (gain, muted)) in &seen {
            let w = map.entry(k.clone()).or_default();
            set_gain(w, *gain);
            w.set_muted(*muted);
        }
        for (k, w) in map.iter() {
            if !seen.contains_key(k) {
                w.set_q24(tessera_media::gain::UNITY_Q24);
                w.set_muted(false);
            }
        }
    }

    /// Apply one event; `true` if the status should be read again.
    pub fn apply_event(&self, e: &Event) -> bool {
        match e {
            Event::ChannelGainChanged { target, gain, .. } => {
                for c in &target.channels {
                    self.set(
                        &target.device.0,
                        target.direction,
                        c.get(),
                        Some(*gain),
                        None,
                    );
                }
                false
            }
            Event::ChannelMuteChanged { target, muted, .. } => {
                for c in &target.channels {
                    self.set(
                        &target.device.0,
                        target.direction,
                        c.get(),
                        None,
                        Some(*muted),
                    );
                }
                false
            }
            Event::OutputGainChanged { target, gain, .. } => {
                for c in &target.channels {
                    self.set_output(&target.device.0, c.get(), Some(*gain), None);
                }
                false
            }
            Event::OutputMuteChanged { target, muted, .. } => {
                for c in &target.channels {
                    self.set_output(&target.device.0, c.get(), None, Some(*muted));
                }
                false
            }
            Event::CrosspointGainChanged {
                crosspoints, gain, ..
            } => {
                for k in crosspoints {
                    self.set_router_word(WordKey::Crosspoint(k.clone()), Some(*gain), None);
                }
                false
            }
            Event::CrosspointMuteChanged {
                crosspoints, muted, ..
            } => {
                for k in crosspoints {
                    self.set_router_word(WordKey::Crosspoint(k.clone()), None, Some(*muted));
                }
                false
            }
            // Network ports' levels are the channel controls (their own
            // events); only local ports have words here.
            Event::PortGainChanged { ports, gain, .. } => {
                for p in ports.iter().filter(|p| crate::router::is_local_level(p)) {
                    self.set_router_word(WordKey::Port(p.clone()), Some(*gain), None);
                }
                false
            }
            Event::PortMuteChanged { ports, muted, .. } => {
                for p in ports.iter().filter(|p| crate::router::is_local_level(p)) {
                    self.set_router_word(WordKey::Port(p.clone()), None, Some(*muted));
                }
                false
            }
            Event::ConfigReloaded
            | Event::ConfigReloadedWithOrigin { .. }
            | Event::DeviceChannelsChanged { .. }
            | Event::DeviceSettingsChanged { .. }
            | Event::RouterChanged { .. } => true,
            _ => false,
        }
    }
}

/// Drop everything queued on `sub`: a status read that follows covers it,
/// and applied after that read an older event would revert a newer value.
/// `false` once the bus is gone.
pub(crate) fn drain(sub: &Subscription) -> bool {
    loop {
        match sub.try_recv() {
            Ok(_) => {}
            Err(RecvError::Empty) => return true,
            Err(RecvError::Closed) => return false,
        }
    }
}

/// Make `registry` equal to the state engine's channel gain and mute now.
/// The daemon calls it before it starts any flow, so a channel saved as
/// muted never plays at unity. `false` if the status could not be read.
pub async fn seed(engine: &EngineHandle, registry: &GainRegistry) -> bool {
    match engine.query(Query::Status).await {
        Ok(Reply::Status(s)) => {
            registry.apply_status(&s);
            match engine.query(Query::Router).await {
                Ok(Reply::Router(r)) => {
                    registry.apply_router(&r);
                    true
                }
                Ok(_) => false,
                Err(e) => {
                    tracing::debug!("router gains: read failed ({e})");
                    false
                }
            }
        }
        Ok(_) => false,
        Err(e) => {
            tracing::debug!("channel gains: status read failed ({e})");
            false
        }
    }
}

/// Keep `registry` equal to the engine's channel gain and mute until
/// `shutdown` (see the module documentation). Supervised by the daemon.
pub async fn run(
    engine: EngineHandle,
    bus: EventBus,
    registry: Arc<GainRegistry>,
    shutdown: CancellationToken,
) {
    let mut sub = bus.subscribe(1024);
    let mut stale = true;
    loop {
        if stale {
            // Subscribed first, drained, then read: whatever is queued after
            // this point is newer than (or the same as) the status read.
            if !drain(&sub) {
                return;
            }
            if !seed(&engine, &registry).await {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                }
                continue;
            }
        }
        let got = tokio::select! {
            () = shutdown.cancelled() => return,
            r = sub.recv_async() => r,
        };
        // Recomputed from every item: stale only after a lag or a change
        // the events do not carry.
        stale = match got {
            Ok(Received::Event(e)) => registry.apply_event(&e),
            Ok(Received::Lagged(_)) => true,
            Err(_) => return,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_control_types::{
        Channel, ChannelSet, DeviceKey, DeviceStatus, Encoding, LatencyUs, Origin, SampleRate,
    };

    fn target(dir: Direction, chans: &[u16]) -> ChannelSet {
        ChannelSet {
            device: DeviceKey("main".into()),
            direction: dir,
            channels: chans.iter().map(|&c| Channel::new(c).unwrap()).collect(),
        }
    }

    #[test]
    fn the_output_stage_has_its_own_words() {
        let r = GainRegistry::new();
        let out = r.for_output("main", &[0, UNUSED, 1], RATE);
        let input = r.for_flow("main", Direction::Rx, &[0, UNUSED, 1], RATE);
        r.apply_event(&Event::OutputGainChanged {
            target: target(Direction::Rx, &[1]),
            gain: Gain::MilliDb(-6_021),
            origin: Origin::Internal,
        });
        r.apply_event(&Event::OutputMuteChanged {
            target: target(Direction::Rx, &[2]),
            muted: true,
            origin: Origin::Internal,
        });
        let q = out.words[0].as_ref().unwrap().load().q24();
        assert!((i64::from(q) - (1 << 23)).abs() < 2_000, "about half: {q}");
        assert!(out.words[2].as_ref().unwrap().muted());
        assert!(out.words[1].is_none());
        assert!(
            input.words[0].as_ref().unwrap().load().is_unity(),
            "input untouched"
        );
        assert!(!input.words[2].as_ref().unwrap().muted());
        // A status without them puts them back at unity.
        r.apply_status(&Status::new("t"));
        assert!(out.words[0].as_ref().unwrap().load().is_unity());
        assert!(!out.words[2].as_ref().unwrap().muted());
    }

    #[test]
    fn events_set_words_per_device_channel_and_direction() {
        let r = GainRegistry::new();
        let f = r.for_flow("main", Direction::Rx, &[1, UNUSED, 0], RATE);
        assert!(f.words[1].is_none());
        r.apply_event(&Event::ChannelMuteChanged {
            target: target(Direction::Rx, &[2]),
            muted: true,
            origin: Origin::Internal,
        });
        r.apply_event(&Event::ChannelGainChanged {
            target: target(Direction::Rx, &[1]),
            gain: Gain::Off,
            origin: Origin::Internal,
        });
        assert!(f.words[0].as_ref().unwrap().muted(), "channel 2 = index 1");
        assert_eq!(f.words[2].as_ref().unwrap().load().q24(), 0);
        assert!(r.channel("main", Direction::Tx, 1).load().is_unity());
        r.set("main", Direction::Tx, 1, Some(Gain::MilliDb(-6_021)), None);
        let q = r.channel("main", Direction::Tx, 0).load().q24();
        assert!((q as i64 - (1 << 23)).abs() < 2_000, "about half: {q}");
    }

    #[test]
    fn a_status_read_resets_words_it_does_not_list() {
        let r = GainRegistry::new();
        r.set("gone", Direction::Rx, 1, Some(Gain::Off), Some(true));
        let mut ch = ChannelStatus::new(Channel::new(1).unwrap(), "a");
        ch.muted = true;
        ch.gain = Gain::MilliDb(-20_000);
        let mut d = DeviceStatus::new(
            DeviceKey("main".into()),
            "m",
            SampleRate(48_000),
            Encoding(24),
            LatencyUs(1_000),
        );
        d.tx_channels = vec![ch];
        let s = Status::new("t").with_devices(vec![d]);
        r.apply_status(&s);
        assert!(r.channel("gone", Direction::Rx, 0).load().is_unity());
        let w = r.channel("main", Direction::Tx, 0).load();
        assert!(w.is_muted());
        assert!(w.q24() < (1 << 22));
    }

    #[test]
    fn router_words_follow_the_report_and_events() {
        use tessera_control_types::{Crosspoint, CrosspointOrigin, CrosspointStatus, PortStatus};
        let r = GainRegistry::new();
        let dk = DeviceKey("main".into());
        let src = RouterPort::CardIn {
            card: "hw:1".into(),
            channel: Channel::new(1).unwrap(),
        };
        let dst = RouterPort::HostOut {
            device: dk,
            channel: Channel::new(1).unwrap(),
        };
        let key = CrosspointKey::new(src.clone(), dst.clone());
        // Created with the seed the table carries, at unity without one.
        let x = r.word(
            &WordKey::Crosspoint(key.clone()),
            Some((Gain::MilliDb(-6_021), false)),
        );
        assert!((i64::from(x.load().q24()) - (1 << 23)).abs() < 2_000);
        let p = r.word(&WordKey::Port(src.clone()), None);
        assert!(p.load().is_unity());
        // An existing word keeps its value whatever the seed says.
        let again = r.word(&WordKey::Crosspoint(key.clone()), Some((Gain::Off, true)));
        assert!(!again.muted());
        r.apply_event(&Event::PortMuteChanged {
            ports: vec![src.clone()],
            muted: true,
            origin: Origin::Internal,
        });
        assert!(p.muted());
        r.apply_event(&Event::CrosspointGainChanged {
            crosspoints: vec![key.clone()],
            gain: Gain::Off,
            origin: Origin::Internal,
        });
        assert_eq!(x.load().q24(), 0);
        // A report puts what it lists in place and the rest back to unity.
        let mut xp = Crosspoint::new(src.clone(), dst);
        xp.muted = true;
        let mut report = RouterReport::default();
        report.crosspoints = vec![CrosspointStatus::new(xp, CrosspointOrigin::Saved)];
        report.ports = vec![PortStatus::new(src, "CQ in 1")];
        r.apply_router(&report);
        assert!(x.load().is_muted() && x.load().q24() == tessera_media::gain::UNITY_Q24);
        assert!(!p.muted());
        assert!(r.apply_event(&Event::RouterChanged {
            origin: Origin::Internal
        }));
    }

    #[test]
    fn a_router_report_creates_missing_port_words_with_its_level() {
        use tessera_control_types::PortStatus;
        let r = GainRegistry::new();
        let src = RouterPort::CardIn {
            card: "hw:1".into(),
            channel: Channel::new(2).unwrap(),
        };
        let mut ps = PortStatus::new(src.clone(), "CQ in 2");
        ps.gain = Gain::MilliDb(-6_021);
        ps.muted = true;
        let mut report = RouterReport::default();
        report.ports = vec![ps];
        r.apply_router(&report);
        // A stale seed (an older table) must not win over the report.
        let p = r.word(&WordKey::Port(src), Some((Gain::default(), false)));
        assert!(p.muted());
        assert!((i64::from(p.load().q24()) - (1 << 23)).abs() < 2_000);
    }

    /// 8 kHz: a 40-sample ramp, five blocks of 8.
    const RATE: u32 = 8_000;

    #[test]
    fn a_flow_ramps_over_several_blocks_from_the_word_it_started_with() {
        let r = GainRegistry::new();
        r.set("main", Direction::Rx, 1, None, Some(true));
        let mut f = r.for_flow("main", Direction::Rx, &[0], RATE);
        let mut s = [1 << 20; 8];
        f.apply(0, &mut s);
        assert_eq!(s, [0; 8], "muted from the start: no fade-out");
        r.set("main", Direction::Rx, 1, None, Some(false));
        let mut seen = Vec::new();
        for _ in 0..6 {
            let mut s = [1 << 20; 8];
            f.apply(0, &mut s);
            seen.extend_from_slice(&s);
        }
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "rising: {seen:?}");
        assert!(seen[7] < 1 << 19, "not done after one block: {seen:?}");
        assert!(seen[38] < 1 << 20 && seen[39] == 1 << 20, "{seen:?}");
        assert!(seen[40..].iter().all(|&v| v == 1 << 20));
        assert!(!f.smoother(0).unwrap().ramping());
        let mut s = [5; 4];
        f.apply(3, &mut s);
        assert_eq!(s, [5; 4], "no word: untouched");
    }

    #[test]
    fn a_change_mid_ramp_continues_from_the_gain_reached() {
        let r = GainRegistry::new();
        let mut f = r.for_flow("main", Direction::Tx, &[0], RATE);
        r.set("main", Direction::Tx, 1, None, Some(true));
        let mut out = Vec::new();
        for _ in 0..2 {
            let mut s = [1 << 20; 8];
            f.apply(0, &mut s);
            out.extend_from_slice(&s);
        }
        let reached = *out.last().unwrap();
        assert!(reached > 0 && reached < 1 << 20, "{out:?}");
        r.set("main", Direction::Tx, 1, None, Some(false));
        let mut s = [1 << 20; 8];
        f.apply(0, &mut s);
        assert!(
            s[0] >= reached && s[0] - reached < 1 << 16,
            "no jump: {reached} → {s:?}"
        );
    }

    #[test]
    fn silence_keeps_the_smoother_current() {
        // A TX position no client covers is filled with zeros and still
        // goes through the gain: a mute set meanwhile is complete when a
        // client attaches, not ramped from unity.
        let r = GainRegistry::new();
        let mut f = r.for_flow("main", Direction::Tx, &[0], RATE);
        r.set("main", Direction::Tx, 1, None, Some(true));
        for _ in 0..6 {
            let mut s = [0; 8];
            f.apply(0, &mut s);
            assert_eq!(s, [0; 8]);
        }
        let mut s = [1 << 20; 8];
        f.apply(0, &mut s);
        assert_eq!(s, [0; 8], "already muted");
    }

    #[test]
    fn draining_drops_what_a_status_read_will_cover() {
        let bus = EventBus::new();
        let sub = bus.subscribe(8);
        bus.publish(&Event::ChannelMuteChanged {
            target: target(Direction::Tx, &[1]),
            muted: true,
            origin: Origin::Internal,
        });
        assert!(drain(&sub));
        assert!(matches!(sub.try_recv(), Err(RecvError::Empty)));
        drop(bus);
        assert!(!drain(&sub), "the bus is gone");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seeding_reads_the_saved_mute_before_any_flow_starts() {
        let e = crate::test_engine::TestEngine::start("gains-seed", crate::test_engine::ONE_DEVICE);
        e.handle
            .apply(
                Origin::Internal,
                tessera_control_types::Command::SetChannelMute {
                    target: target(Direction::Tx, &[2]),
                    muted: true,
                },
            )
            .await
            .unwrap();
        let r = GainRegistry::new();
        assert!(seed(&e.handle, &r).await);
        let mut f = r.for_flow("main", Direction::Tx, &[0, 1], RATE);
        let mut s = [1 << 20; 8];
        f.apply(1, &mut s);
        assert_eq!(s, [0; 8], "muted from the first sample");
        let mut s = [1 << 20; 8];
        f.apply(0, &mut s);
        assert_eq!(s, [1 << 20; 8], "the other channel at unity");
        e.stop().await;
    }
}
