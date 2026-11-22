//! The host router's saved intent and its commands (provisional 0.x;
//! `docs/plans/2026-11-22-host-router.md`, ADR 0017).
//!
//! **Effective grid.** Crosspoints implied by network card bridges (a
//! bridge without `local_only`: card inputs to its TX channels, its RX
//! channels' host outputs to card outputs) are always in effect and change
//! only with the bridge. The rest is the **saved grid** when one is saved
//! (`[router] saved = true`), otherwise the **default grid**: each RX
//! channel to the host output of the same number, each application output
//! to the TX channel of the same number (unless a network capture bridge
//! feeds it), local monitoring (`monitor_rx_from_tx` / `SetMonitor`) as TX
//! taps into host outputs, and the configured `[[route]]` entries.
//!
//! **Editing.** `SetRouter` replaces the saved grid; `SetCrosspoint`,
//! `RemoveCrosspoint`, `SetCrosspointGain` and `SetCrosspointMute` edit it,
//! saving the default grid first when none is saved (so an edit never loses
//! the routes in effect). `ResetRouter` returns to the default grid.
//!
//! **Checks.** A destination is a destination port; ports exist (a
//! crosspoint already in effect is not checked again, so a port that went
//! away does not block other edits); each pair once; no pair a bridge
//! implies; no loop through taps. A saved crosspoint whose port goes away
//! is kept and reported inactive.
//!
//! **Levels.** Port levels of network channels and host outputs are the
//! channel controls ([`super::surfaces`]); the other ports' levels are kept
//! in `[[router.level]]`.
//!
//! Nothing here runs without a runtime that can apply it
//! (`Environment::runtime.router`): the commands answer `Unsupported`, and
//! `Query::Router` reports the grid with `read_only`.

use std::collections::{BTreeMap, BTreeSet};

use tessera_control_types::{
    Channel, Command, CommandError, Crosspoint, CrosspointKey, CrosspointOrigin, CrosspointStatus,
    DeviceKey, Direction, Event, Gain, Origin, PortLabel, PortStatus, RouterPort, RouterReport,
};

use super::{StateEngine, invalid, not_found};
use crate::config::{BridgeConfig, BridgeDirection};
use crate::persist::{PortLevelIntent, StateFile};
use crate::router::{default_label, describe, describe_key, find_loop};

/// One crosspoint of the effective grid and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Effective {
    pub(super) x: Crosspoint,
    pub(super) origin: CrosspointOrigin,
}

fn ch(n: u16) -> Option<Channel> {
    Channel::new(n)
}

fn key(device: &str) -> DeviceKey {
    DeviceKey(device.to_owned())
}

/// Is `port` one whose level and label live with the router (not a network
/// channel or host output)?
pub(super) const fn is_local_level(port: &RouterPort) -> bool {
    matches!(
        port,
        RouterPort::AppOut { .. } | RouterPort::CardIn { .. } | RouterPort::CardOut { .. }
    )
}

/// `Invalid` unless `port` takes a router label (local ports and host
/// outputs; network channels are labelled with `SetChannelLabel`).
fn check_label_port(port: &RouterPort) -> Result<(), CommandError> {
    if is_local_level(port) || matches!(port, RouterPort::HostOut { .. }) {
        Ok(())
    } else {
        Err(invalid(
            "labels",
            format!(
                "{}: label network channels with SetChannelLabel",
                describe(port)
            ),
        ))
    }
}

/// The ports and channels a bridge opens on its card, and the device
/// channels it feeds (`None` for a local-only bridge).
fn bridge_span(b: &BridgeConfig, available: u16) -> (u16, u16, Option<u16>) {
    if b.local_only {
        (b.card_first_channel, b.channels.unwrap_or(0), None)
    } else {
        (
            b.card_first_channel,
            b.channel_count(available),
            Some(b.first_channel),
        )
    }
}

/// `SetMonitor` with a saved grid: replace `device`'s TX-tap → host-output
/// crosspoints by the list (RX `n` plays TX `list[n-1]`, unity). Whether
/// anything changed.
pub(super) fn set_monitor_taps(intent: &mut StateFile, device: &DeviceKey, list: &[u16]) -> bool {
    let before = intent.router.crosspoint.clone();
    intent.router.crosspoint.retain(|x| {
        !(matches!(&x.source, RouterPort::NetworkTx { device: d, .. } if d == device)
            && matches!(&x.destination, RouterPort::HostOut { device: d, .. } if d == device))
    });
    for (i, t) in list.iter().enumerate() {
        let (Some(t), Some(n)) = (ch(*t), u16::try_from(i + 1).ok().and_then(ch)) else {
            continue;
        };
        intent.router.crosspoint.push(Crosspoint::new(
            RouterPort::NetworkTx {
                device: device.clone(),
                channel: t,
            },
            RouterPort::HostOut {
                device: device.clone(),
                channel: n,
            },
        ));
    }
    intent.router.crosspoint != before
}

impl StateEngine {
    /// Local monitoring of `device` as saved by `SetMonitor` in `intent`, or
    /// else configured, limited to the channels the device has now.
    fn legacy_monitor(&self, intent: &StateFile, device: &str) -> Vec<u16> {
        let Some(m) = self.model.device(device) else {
            return Vec::new();
        };
        let list = intent
            .device
            .get(device)
            .and_then(|d| d.monitor.as_ref())
            .map(|s| s.rx_from_tx.clone())
            .or_else(|| {
                self.config
                    .config
                    .device(device)
                    .map(|d| d.monitor_rx_from_tx.clone())
            })
            .unwrap_or_default();
        list.into_iter()
            .take(usize::from(m.rx_channels))
            .map(|t| if t > m.tx_channels { 0 } else { t })
            .collect()
    }

    /// The monitoring list a saved grid amounts to (see
    /// [`effective_monitor`](Self::effective_monitor)).
    pub(super) fn router_monitor_list(&self, device: &str, rx: u16, tx: u16) -> Vec<u16> {
        let grid = self.grid_of(&self.intent);
        (1..=rx)
            .map(|n| {
                let taps: Vec<&Effective> = grid
                    .iter()
                    .filter(|e| {
                        matches!(&e.x.destination, RouterPort::HostOut { device: d, channel }
                            if d.0 == device && channel.get() == n)
                            && e.x.source.is_destination()
                    })
                    .collect();
                match taps.as_slice() {
                    [e] if !e.x.muted && e.x.gain == Gain::default() => match &e.x.source {
                        RouterPort::NetworkTx { device: d, channel }
                            if d.0 == device && channel.get() <= tx =>
                        {
                            channel.get()
                        }
                        _ => 0,
                    },
                    _ => 0,
                }
            })
            .collect()
    }

    /// Crosspoints that network bridges (not `local_only`) imply, with the
    /// bridge's name.
    fn bridge_crosspoints(&self, bridges: &[BridgeConfig]) -> Vec<(Crosspoint, String)> {
        let mut out = Vec::new();
        for b in bridges.iter().filter(|b| b.enabled && !b.local_only) {
            let Some(d) = self.model.device(&b.device) else {
                continue;
            };
            let available = match b.direction {
                BridgeDirection::Capture => d.tx_channels,
                BridgeDirection::Playback => d.rx_channels,
            };
            let (card_first, count, Some(first)) = bridge_span(b, available) else {
                continue;
            };
            for i in 0..count {
                let (Some(c), Some(n)) = (
                    card_first.checked_add(i).and_then(ch),
                    first.checked_add(i).and_then(ch),
                ) else {
                    continue;
                };
                let x = match b.direction {
                    BridgeDirection::Capture => Crosspoint::new(
                        RouterPort::CardIn {
                            card: b.alsa_pcm.clone(),
                            channel: c,
                        },
                        RouterPort::NetworkTx {
                            device: key(&b.device),
                            channel: n,
                        },
                    ),
                    BridgeDirection::Playback => Crosspoint::new(
                        RouterPort::HostOut {
                            device: key(&b.device),
                            channel: n,
                        },
                        RouterPort::CardOut {
                            card: b.alsa_pcm.clone(),
                            channel: c,
                        },
                    ),
                };
                out.push((x, b.name.clone()));
            }
        }
        out
    }

    /// The default grid of `intent` (used while no grid is saved), bridge
    /// crosspoints not included.
    fn default_crosspoints(
        &self,
        intent: &StateFile,
        bridges: &[BridgeConfig],
    ) -> Vec<(Crosspoint, CrosspointOrigin)> {
        let mut out = Vec::new();
        let fed: BTreeSet<(String, u16)> = self
            .bridge_crosspoints(bridges)
            .into_iter()
            .filter_map(|(x, _)| match x.destination {
                RouterPort::NetworkTx { device, channel } => Some((device.0, channel.get())),
                _ => None,
            })
            .collect();
        for d in &self.model.devices {
            for n in (1..=d.rx_channels).filter_map(ch) {
                out.push((
                    Crosspoint::new(
                        RouterPort::NetworkRx {
                            device: d.key.clone(),
                            channel: n,
                        },
                        RouterPort::HostOut {
                            device: d.key.clone(),
                            channel: n,
                        },
                    ),
                    CrosspointOrigin::Default,
                ));
            }
            for n in (1..=d.tx_channels).filter_map(ch) {
                if fed.contains(&(d.key.0.clone(), n.get())) {
                    continue;
                }
                out.push((
                    Crosspoint::new(
                        RouterPort::AppOut {
                            device: d.key.clone(),
                            channel: n,
                        },
                        RouterPort::NetworkTx {
                            device: d.key.clone(),
                            channel: n,
                        },
                    ),
                    CrosspointOrigin::Default,
                ));
            }
            for (i, t) in self.legacy_monitor(intent, &d.key.0).iter().enumerate() {
                let (Some(t), Some(n)) = (ch(*t), u16::try_from(i + 1).ok().and_then(ch)) else {
                    continue;
                };
                out.push((
                    Crosspoint::new(
                        RouterPort::NetworkTx {
                            device: d.key.clone(),
                            channel: t,
                        },
                        RouterPort::HostOut {
                            device: d.key.clone(),
                            channel: n,
                        },
                    ),
                    CrosspointOrigin::Monitor,
                ));
            }
        }
        for r in &self.config.config.routes {
            let x = r.crosspoint();
            if !out.iter().any(|(o, _)| o.key() == x.key()) {
                out.push((x, CrosspointOrigin::Configured));
            }
        }
        out
    }

    /// The effective grid for `intent`: bridge crosspoints, then the saved
    /// or default grid (without pairs a bridge already implies).
    pub(super) fn grid_of(&self, intent: &StateFile) -> Vec<Effective> {
        let bridges = self.bridges_of(intent);
        let mut out: Vec<Effective> = self
            .bridge_crosspoints(&bridges)
            .into_iter()
            .map(|(x, name)| Effective {
                x,
                origin: CrosspointOrigin::Bridge { name },
            })
            .collect();
        let implied: BTreeSet<CrosspointKey> = out.iter().map(|e| e.x.key()).collect();
        let base: Vec<(Crosspoint, CrosspointOrigin)> = if intent.router.saved {
            intent
                .router
                .crosspoint
                .iter()
                .map(|x| (x.clone(), CrosspointOrigin::Saved))
                .collect()
        } else {
            self.default_crosspoints(intent, &bridges)
        };
        out.extend(
            base.into_iter()
                .filter(|(x, _)| !implied.contains(&x.key()))
                .map(|(x, origin)| Effective { x, origin }),
        );
        out
    }

    /// The effective grid now: every crosspoint in effect or saved, with its
    /// origin (inactive ones included). What the daemon's mixing engine
    /// applies, with [`port_level`](Self::port_level) for the port levels.
    #[must_use]
    pub fn effective_crosspoints(&self) -> Vec<(Crosspoint, CrosspointOrigin)> {
        self.grid_of(&self.intent)
            .into_iter()
            .map(|e| (e.x, e.origin))
            .collect()
    }

    /// Why `port` does not exist now with `bridges`, if it does not.
    pub(super) fn port_problem(
        &self,
        port: &RouterPort,
        bridges: &[BridgeConfig],
    ) -> Option<String> {
        let n = port.channel().get();
        if let Some(device) = port.device() {
            let Some(d) = self.model.device(&device.0) else {
                return Some(format!("there is no device `{}`", device.0));
            };
            let (count, side) = match port {
                RouterPort::NetworkRx { .. } | RouterPort::HostOut { .. } => (d.rx_channels, "rx"),
                _ => (d.tx_channels, "tx"),
            };
            return (n > count).then(|| {
                format!(
                    "device `{}` has {count} {side} channel(s), not {n}",
                    device.0
                )
            });
        }
        let card = port.card().unwrap_or_default();
        let want = match port {
            RouterPort::CardIn { .. } => BridgeDirection::Capture,
            _ => BridgeDirection::Playback,
        };
        let carried = bridges.iter().any(|b| {
            if !b.enabled || b.alsa_pcm != card || b.direction != want {
                return false;
            }
            let available = self.model.device(&b.device).map_or(0, |d| match want {
                BridgeDirection::Capture => d.tx_channels,
                BridgeDirection::Playback => d.rx_channels,
            });
            let (first, count, _) = bridge_span(b, available);
            let first = u32::from(first);
            (first..first + u32::from(count)).contains(&u32::from(n))
        });
        (!carried).then(|| {
            format!(
                "no enabled {} bridge of `{card}` carries card channel {n}",
                match want {
                    BridgeDirection::Capture => "capture",
                    BridgeDirection::Playback => "playback",
                }
            )
        })
    }

    /// Every port that exists now, sources then destinations.
    fn ports_now(&self, bridges: &[BridgeConfig]) -> Vec<RouterPort> {
        let mut sources = Vec::new();
        let mut apps = Vec::new();
        let mut cards_in = Vec::new();
        let mut tx = Vec::new();
        let mut host = Vec::new();
        let mut cards_out = Vec::new();
        for d in &self.model.devices {
            for n in (1..=d.rx_channels).filter_map(ch) {
                sources.push(RouterPort::NetworkRx {
                    device: d.key.clone(),
                    channel: n,
                });
                host.push(RouterPort::HostOut {
                    device: d.key.clone(),
                    channel: n,
                });
            }
            for n in (1..=d.tx_channels).filter_map(ch) {
                apps.push(RouterPort::AppOut {
                    device: d.key.clone(),
                    channel: n,
                });
                tx.push(RouterPort::NetworkTx {
                    device: d.key.clone(),
                    channel: n,
                });
            }
        }
        for b in bridges.iter().filter(|b| b.enabled) {
            let available = self
                .model
                .device(&b.device)
                .map_or(0, |d| match b.direction {
                    BridgeDirection::Capture => d.tx_channels,
                    BridgeDirection::Playback => d.rx_channels,
                });
            let (first, count, _) = bridge_span(b, available);
            for c in (0..count).filter_map(|i| first.checked_add(i).and_then(ch)) {
                let card = b.alsa_pcm.clone();
                match b.direction {
                    BridgeDirection::Capture => {
                        let p = RouterPort::CardIn { card, channel: c };
                        if !cards_in.contains(&p) {
                            cards_in.push(p);
                        }
                    }
                    BridgeDirection::Playback => {
                        let p = RouterPort::CardOut { card, channel: c };
                        if !cards_out.contains(&p) {
                            cards_out.push(p);
                        }
                    }
                }
            }
        }
        sources.extend(apps);
        sources.extend(cards_in);
        sources.extend(tx);
        sources.extend(host);
        sources.extend(cards_out);
        sources
    }

    /// The level and mute of a router port: the channel controls for
    /// network ports (`NetworkRx` / `NetworkTx`: the channel's gain) and
    /// host outputs (the RX channel's output level), the saved
    /// `[[router.level]]` for the others (unity, unmuted by default).
    #[must_use]
    pub fn port_level(&self, port: &RouterPort) -> (Gain, bool) {
        let n = port.channel().get();
        match port {
            RouterPort::NetworkRx { device, .. } => (
                self.channel_gain(&device.0, Direction::Rx, n),
                self.channel_muted(&device.0, Direction::Rx, n),
            ),
            RouterPort::NetworkTx { device, .. } => (
                self.channel_gain(&device.0, Direction::Tx, n),
                self.channel_muted(&device.0, Direction::Tx, n),
            ),
            RouterPort::HostOut { device, .. } => (
                self.output_gain(&device.0, Direction::Rx, n),
                self.output_muted(&device.0, Direction::Rx, n),
            ),
            _ => self
                .intent
                .router
                .level
                .iter()
                .find(|l| l.port == *port)
                .map_or((Gain::default(), false), |l| (l.gain, l.muted)),
        }
    }

    /// The label of a port: the channel label for network ports, else the
    /// saved label, else a default (`CQ in 15`).
    fn port_label(&self, port: &RouterPort) -> String {
        let n = port.channel().get();
        let device = port.device().and_then(|d| self.model.device(&d.0));
        match (port, device) {
            (RouterPort::NetworkRx { .. }, Some(d)) => d.rx_label(n),
            (RouterPort::NetworkTx { .. }, Some(d)) => d.tx_label(n),
            _ => self
                .intent
                .router
                .label
                .iter()
                .find(|l| l.port == *port)
                .map_or_else(|| default_label(port), |l| l.label.clone()),
        }
    }

    /// Was the label of `port` set by the user (a channel label for network
    /// ports, a saved router label for the others)?
    fn port_label_is_custom(&self, port: &RouterPort) -> bool {
        let n = port.channel().get();
        let device = port.device().and_then(|d| self.model.device(&d.0));
        match (port, device) {
            (RouterPort::NetworkRx { .. }, Some(d)) => d.rx_labels.contains_key(&n),
            (RouterPort::NetworkTx { .. }, Some(d)) => d.tx_labels.contains_key(&n),
            (RouterPort::NetworkRx { .. } | RouterPort::NetworkTx { .. }, None) => false,
            _ => self.intent.router.label.iter().any(|l| l.port == *port),
        }
    }

    /// `Query::Router`: the ports with their levels and the effective grid.
    #[must_use]
    pub fn router_report(&self) -> RouterReport {
        let bridges = self.effective_bridges();
        let grid = self.grid_of(&self.intent);
        let mut ports = self.ports_now(&bridges);
        let available: BTreeSet<RouterPort> = ports.iter().cloned().collect();
        let mut missing: BTreeSet<RouterPort> = BTreeSet::new();
        for e in &grid {
            for p in [&e.x.source, &e.x.destination] {
                if !available.contains(p) {
                    missing.insert(p.clone());
                }
            }
        }
        for p in self
            .intent
            .router
            .label
            .iter()
            .map(|l| &l.port)
            .chain(self.intent.router.level.iter().map(|l| &l.port))
        {
            if !available.contains(p) {
                missing.insert(p.clone());
            }
        }
        ports.extend(missing.iter().cloned());
        let mut report = RouterReport::default();
        report.saved = self.intent.router.saved;
        report.read_only = !self.env.runtime.router;
        report.ports = ports
            .into_iter()
            .map(|p| {
                let mut s = PortStatus::new(p.clone(), self.port_label(&p));
                s.custom_label = self.port_label_is_custom(&p);
                s.available = !missing.contains(&p);
                (s.gain, s.muted) = self.port_level(&p);
                s.gain_owner = self.port_owner(&p).map(str::to_owned);
                s
            })
            .collect();
        report.crosspoints = grid
            .into_iter()
            .map(|e| {
                let problem = [&e.x.source, &e.x.destination].into_iter().find_map(|p| {
                    self.port_problem(p, &bridges)
                        .map(|m| format!("{}: {m}", describe(p)))
                });
                let owner = self
                    .level_owner(&super::surfaces::Level::Crosspoint(e.x.key()))
                    .map(str::to_owned);
                let mut s = CrosspointStatus::new(e.x, e.origin);
                s.active = problem.is_none();
                s.problem = problem;
                s.gain_owner = owner;
                s
            })
            .collect();
        report
    }

    /// `Unsupported` unless the runtime runs the host router.
    pub(super) fn need_router(&self, what: &str) -> Result<(), CommandError> {
        if self.env.runtime.router {
            Ok(())
        } else {
            Err(CommandError::Unsupported {
                what: format!("{what}: this tesserad cannot run the host router yet"),
            })
        }
    }

    /// Save the default grid of `intent` when none is saved, so an edit
    /// starts from the routes in effect.
    fn materialize(&self, intent: &mut StateFile) {
        if intent.router.saved {
            return;
        }
        let bridges = self.bridges_of(intent);
        let implied: BTreeSet<CrosspointKey> = self
            .bridge_crosspoints(&bridges)
            .into_iter()
            .map(|(x, _)| x.key())
            .collect();
        intent.router.crosspoint = self
            .default_crosspoints(intent, &bridges)
            .into_iter()
            .map(|(x, _)| x)
            .filter(|x| !implied.contains(&x.key()))
            .collect();
        intent.router.saved = true;
    }

    /// The whole check of `next`'s saved grid and labels against `prev`
    /// (whose effective crosspoints are not checked for their ports again).
    pub(super) fn check_grid(
        &self,
        next: &StateFile,
        prev: &StateFile,
    ) -> Result<(), CommandError> {
        let bridges = self.bridges_of(next);
        let implied: BTreeMap<CrosspointKey, String> = self
            .bridge_crosspoints(&bridges)
            .into_iter()
            .map(|(x, name)| (x.key(), name))
            .collect();
        let before: BTreeSet<CrosspointKey> =
            self.grid_of(prev).into_iter().map(|e| e.x.key()).collect();
        let mut seen = BTreeSet::new();
        for x in &next.router.crosspoint {
            let k = x.key();
            let what = describe_key(&k);
            if !x.destination.is_destination() {
                return Err(invalid(
                    "crosspoint",
                    format!(
                        "{what}: the destination must be a network_tx, host_out or card_out port"
                    ),
                ));
            }
            if x.source == x.destination {
                return Err(invalid(
                    "crosspoint",
                    format!("{what}: a port cannot feed itself"),
                ));
            }
            if !x.gain.is_valid() {
                return Err(invalid(
                    "gain",
                    format!("{what}: the gain must be off or -96000..=12000 milli-dB"),
                ));
            }
            if !seen.insert(k.clone()) {
                return Err(invalid("crosspoint", format!("{what} is listed twice")));
            }
            if let Some(name) = implied.get(&k) {
                return Err(invalid(
                    "crosspoint",
                    format!("{what} is patched by card bridge `{name}`; change the bridge instead"),
                ));
            }
            if !before.contains(&k) {
                for p in [&x.source, &x.destination] {
                    if let Some(m) = self.port_problem(p, &bridges) {
                        return Err(invalid("crosspoint", format!("{what}: {m}")));
                    }
                }
            }
        }
        let all: Vec<CrosspointKey> = next
            .router
            .crosspoint
            .iter()
            .map(Crosspoint::key)
            .chain(implied.keys().cloned())
            .collect();
        if let Some(x) = find_loop(&all) {
            return Err(invalid(
                "crosspoint",
                format!(
                    "{} closes a loop: a destination would feed itself through taps",
                    describe_key(&x)
                ),
            ));
        }
        let mut labelled = BTreeSet::new();
        for l in &next.router.label {
            check_label_port(&l.port)?;
            // Only a label that is new or changed is checked: one saved
            // before must not stop the user from renaming another port
            // (default labels a client sends back are dropped before).
            if !prev.router.label.contains(l) {
                self.check_label_text(&l.port, &l.label)?;
            }
            if !labelled.insert(l.port.clone()) {
                return Err(invalid(
                    "labels",
                    format!("{} is labelled twice", describe(&l.port)),
                ));
            }
        }
        Ok(())
    }

    /// `Invalid` unless `label` is a valid label text for `port`.
    fn check_label_text(&self, port: &RouterPort, label: &str) -> Result<(), CommandError> {
        crate::env::validate_channel_label(label, self.env.max_name_bytes)
            .map_err(|m| invalid("labels", format!("{}: {m}", describe(port))))
    }

    /// `SetPortLabel`: label one port or give it its default label again.
    /// Only this label is checked; the grid and the other labels are kept.
    fn set_port_label(
        &self,
        intent: &mut StateFile,
        port: &RouterPort,
        label: Option<&str>,
    ) -> Result<bool, CommandError> {
        self.need_router("router port labels")?;
        check_label_port(port)?;
        let default = default_label(port);
        let label = label.filter(|t| *t != default);
        let labels = &mut intent.router.label;
        let Some(text) = label else {
            let before = labels.len();
            labels.retain(|l| l.port != *port);
            return Ok(labels.len() != before);
        };
        self.check_label_text(port, text)?;
        if let Some(l) = labels.iter_mut().find(|l| l.port == *port) {
            if l.label == text {
                return Ok(false);
            }
            text.clone_into(&mut l.label);
            return Ok(true);
        }
        // A new label: the port must exist now (a typo would add a port).
        let bridges = self.bridges_of(intent);
        if let Some(m) = self.port_problem(port, &bridges) {
            return Err(invalid("port", format!("{}: {m}", describe(port))));
        }
        intent.router.label.push(PortLabel::new(port.clone(), text));
        Ok(true)
    }

    /// The crosspoints of `keys` in `intent`'s effective grid: `Invalid`
    /// for one that is not patched or that a bridge implies.
    fn check_patched(
        &self,
        intent: &StateFile,
        keys: &[CrosspointKey],
    ) -> Result<(), CommandError> {
        if keys.is_empty() {
            return Err(invalid("crosspoints", "list at least one crosspoint"));
        }
        let grid = self.grid_of(intent);
        for k in keys {
            match grid.iter().find(|e| e.x.key() == *k) {
                None => {
                    return Err(invalid(
                        "crosspoints",
                        format!("{} is not patched", describe_key(k)),
                    ));
                }
                Some(Effective {
                    origin: CrosspointOrigin::Bridge { name },
                    ..
                }) => {
                    return Err(invalid(
                        "crosspoints",
                        format!(
                            "{} is patched by card bridge `{name}`: its level is the ports'",
                            describe_key(k)
                        ),
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// `Invalid` unless every port of `ports` exists now (or has a saved
    /// level already).
    fn check_ports(&self, ports: &[RouterPort]) -> Result<(), CommandError> {
        if ports.is_empty() {
            return Err(invalid("ports", "list at least one port"));
        }
        let bridges = self.effective_bridges();
        for p in ports {
            let saved = self.intent.router.level.iter().any(|l| l.port == *p);
            if !saved {
                if let Some(m) = self.port_problem(p, &bridges) {
                    return Err(invalid("ports", format!("{}: {m}", describe(p))));
                }
            }
        }
        Ok(())
    }

    /// `mutate` for the router commands; `None` for any other command.
    pub(super) fn mutate_router_intent(
        &self,
        intent: &mut StateFile,
        cmd: &Command,
    ) -> Option<Result<bool, CommandError>> {
        Some(match cmd {
            Command::SetRouter { router } => self.need_router("the host router").and_then(|()| {
                let prev = intent.clone();
                intent.router.saved = true;
                intent.router.crosspoint.clone_from(&router.crosspoints);
                intent.router.label.clone_from(&router.labels);
                // A label equal to the port's default is the default: not saved
                // (a client may send every label back as it reads them).
                intent
                    .router
                    .label
                    .retain(|l| l.label != default_label(&l.port));
                self.check_grid(intent, &prev)?;
                Ok(intent.router != prev.router)
            }),
            Command::ResetRouter => self.need_router("the host router").map(|()| {
                let changed = intent.router.saved;
                intent.router.saved = false;
                intent.router.crosspoint.clear();
                changed
            }),
            Command::SetCrosspoint { crosspoint } => {
                self.need_router("the host router").and_then(|()| {
                    let prev = intent.clone();
                    self.materialize(intent);
                    match intent
                        .router
                        .crosspoint
                        .iter_mut()
                        .find(|x| x.key() == crosspoint.key())
                    {
                        Some(x) => x.clone_from(crosspoint),
                        None => intent.router.crosspoint.push(crosspoint.clone()),
                    }
                    self.check_grid(intent, &prev)?;
                    Ok(intent.router != prev.router)
                })
            }
            Command::RemoveCrosspoint {
                source,
                destination,
            } => self.need_router("the host router").and_then(|()| {
                let k = CrosspointKey::new(source.clone(), destination.clone());
                let bridges = self.bridges_of(intent);
                if let Some((_, name)) = self
                    .bridge_crosspoints(&bridges)
                    .into_iter()
                    .find(|(x, _)| x.key() == k)
                {
                    return Err(invalid(
                        "crosspoint",
                        format!(
                            "{} is patched by card bridge `{name}`; change the bridge instead",
                            describe_key(&k)
                        ),
                    ));
                }
                self.materialize(intent);
                let before = intent.router.crosspoint.len();
                intent.router.crosspoint.retain(|x| x.key() != k);
                if intent.router.crosspoint.len() == before {
                    Err(not_found(format!("crosspoint {}", describe_key(&k))))
                } else {
                    Ok(true)
                }
            }),
            Command::SetCrosspointGain { crosspoints, gain } => {
                self.need_router("the host router").and_then(|()| {
                    if !gain.is_valid() {
                        return Err(invalid(
                            "gain",
                            "the gain must be off or -96000..=12000 milli-dB",
                        ));
                    }
                    self.check_patched(intent, crosspoints)?;
                    let prev = intent.router.clone();
                    self.materialize(intent);
                    for x in &mut intent.router.crosspoint {
                        if crosspoints.contains(&x.key()) {
                            x.gain = *gain;
                        }
                    }
                    Ok(intent.router != prev)
                })
            }
            Command::SetCrosspointMute { crosspoints, muted } => {
                self.need_router("the host router").and_then(|()| {
                    self.check_patched(intent, crosspoints)?;
                    let prev = intent.router.clone();
                    self.materialize(intent);
                    for x in &mut intent.router.crosspoint {
                        if crosspoints.contains(&x.key()) {
                            x.muted = *muted;
                        }
                    }
                    Ok(intent.router != prev)
                })
            }
            Command::SetPortGain { ports, gain } => {
                self.set_port_level(intent, ports, Some(*gain), None)
            }
            Command::SetPortMute { ports, muted } => {
                self.set_port_level(intent, ports, None, Some(*muted))
            }
            Command::SetPortLabel { port, label } => {
                self.set_port_label(intent, port, label.as_deref())
            }
            _ => return None,
        })
    }

    /// `SetPortGain` / `SetPortMute`: network ports and host outputs set the
    /// channel controls, the others `[[router.level]]`.
    fn set_port_level(
        &self,
        intent: &mut StateFile,
        ports: &[RouterPort],
        gain: Option<Gain>,
        muted: Option<bool>,
    ) -> Result<bool, CommandError> {
        if gain.is_some_and(|g| !g.is_valid()) {
            return Err(invalid(
                "gain",
                "the gain must be off or -96000..=12000 milli-dB",
            ));
        }
        if ports.iter().any(is_local_level) {
            self.need_router("levels of local router ports")?;
        }
        self.check_ports(ports)?;
        let mut changed = false;
        for p in ports {
            let n = p.channel();
            if let Some(device) = p.device().filter(|_| !is_local_level(p)) {
                let (direction, output) = match p {
                    RouterPort::NetworkRx { .. } => (Direction::Rx, false),
                    RouterPort::NetworkTx { .. } => (Direction::Tx, false),
                    _ => (Direction::Rx, true),
                };
                let set = tessera_control_types::ChannelSet {
                    device: device.clone(),
                    direction,
                    channels: vec![n],
                };
                changed |= super::surfaces::set_level(intent, &set, output, gain, muted);
                continue;
            }
            let levels = &mut intent.router.level;
            let i = match levels.iter().position(|l| l.port == *p) {
                Some(i) => i,
                None => {
                    levels.push(PortLevelIntent {
                        port: p.clone(),
                        gain: Gain::default(),
                        muted: false,
                    });
                    levels.len() - 1
                }
            };
            let before = levels[i].clone();
            if let Some(g) = gain {
                levels[i].gain = g;
            }
            if let Some(m) = muted {
                levels[i].muted = m;
            }
            changed |= levels[i] != before;
            if levels[i].gain == Gain::default() && !levels[i].muted {
                levels.remove(i);
            }
        }
        Ok(changed)
    }

    /// The devices a router command could change, for the access lock: the
    /// devices of the ports it names (single edits and levels), and every
    /// device whose effective crosspoints (patched or not, gain, mute),
    /// port labels or levels the command would change, old and new. A
    /// whole-grid command (`SetRouter`, `ResetRouter`) is judged by that
    /// change alone, so an empty `SetRouter` on the default grid covers
    /// every device whose channels the default grid routes. A command the
    /// state would refuse anyway covers, conservatively, every device of the
    /// grid in effect and of the command.
    pub(super) fn router_command_devices(&self, cmd: &Command) -> BTreeSet<String> {
        fn add(out: &mut BTreeSet<String>, p: &RouterPort) {
            if let Some(d) = p.device() {
                out.insert(d.0.clone());
            }
        }
        let mut out = BTreeSet::new();
        match cmd {
            Command::SetCrosspoint { crosspoint } => {
                add(&mut out, &crosspoint.source);
                add(&mut out, &crosspoint.destination);
            }
            Command::RemoveCrosspoint {
                source,
                destination,
            } => {
                add(&mut out, source);
                add(&mut out, destination);
            }
            Command::SetCrosspointGain { crosspoints, .. }
            | Command::SetCrosspointMute { crosspoints, .. } => {
                for x in crosspoints {
                    add(&mut out, &x.source);
                    add(&mut out, &x.destination);
                }
            }
            Command::SetPortGain { ports, .. } | Command::SetPortMute { ports, .. } => {
                for p in ports {
                    add(&mut out, p);
                }
            }
            Command::SetPortLabel { port, .. } => add(&mut out, port),
            _ => {}
        }
        let mut next = self.intent.clone();
        match self.mutate_router_intent(&mut next, cmd) {
            Some(Ok(_)) => out.extend(self.router_changed_devices(&self.intent, &next)),
            _ => {
                if let Command::SetRouter { .. } | Command::ResetRouter = cmd {
                    for e in self.grid_of(&self.intent) {
                        add(&mut out, &e.x.source);
                        add(&mut out, &e.x.destination);
                    }
                }
                if let Command::SetRouter { router } = cmd {
                    for x in &router.crosspoints {
                        add(&mut out, &x.source);
                        add(&mut out, &x.destination);
                    }
                    for l in &router.labels {
                        add(&mut out, &l.port);
                    }
                }
            }
        }
        out
    }

    /// The devices whose router state differs between two intents: both
    /// ports of every crosspoint added, removed or changed (gain, mute) in
    /// the effective grid, the device of every port whose label or local
    /// level changed, and every device whose channel controls changed.
    fn router_changed_devices(&self, before: &StateFile, after: &StateFile) -> BTreeSet<String> {
        fn add(out: &mut BTreeSet<String>, p: &RouterPort) {
            if let Some(d) = p.device() {
                out.insert(d.0.clone());
            }
        }
        let mut out = BTreeSet::new();
        let grid = |s: &StateFile| -> BTreeMap<CrosspointKey, (Gain, bool)> {
            self.grid_of(s)
                .into_iter()
                .map(|e| (e.x.key(), (e.x.gain, e.x.muted)))
                .collect()
        };
        let (a, b) = (grid(before), grid(after));
        for (k, v) in a.iter().chain(&b) {
            if a.get(k) != Some(v) || b.get(k) != Some(v) {
                add(&mut out, &k.source);
                add(&mut out, &k.destination);
            }
        }
        let label = |s: &StateFile, p: &RouterPort| {
            s.router
                .label
                .iter()
                .find(|l| l.port == *p)
                .map(|l| l.label.clone())
        };
        for l in before.router.label.iter().chain(&after.router.label) {
            if label(before, &l.port) != label(after, &l.port) {
                add(&mut out, &l.port);
            }
        }
        let level = |s: &StateFile, p: &RouterPort| {
            s.router
                .level
                .iter()
                .find(|l| l.port == *p)
                .map(|l| (l.gain, l.muted))
        };
        for l in before.router.level.iter().chain(&after.router.level) {
            if level(before, &l.port) != level(after, &l.port) {
                add(&mut out, &l.port);
            }
        }
        for k in before.device.keys().chain(after.device.keys()) {
            if before.device.get(k) != after.device.get(k) {
                out.insert(k.clone());
            }
        }
        out
    }

    /// Fader priority for the commands that rewrite crosspoints whole
    /// (`SetRouter`, `ResetRouter`, `SetCrosspoint`, `SetMonitor` on a
    /// saved grid): refused with `LevelOwned` when, while a connected fader
    /// of another origin owns a crosspoint's gain (as `SetCrosspointGain`
    /// would be):
    /// - the gain of a crosspoint that stays patched would change, or
    /// - a crosspoint unpatched while it was owned is patched again at
    ///   another gain than the one it had then (unpatching and patching
    ///   again must not set an owned gain the fader does not show).
    ///
    /// Patching and unpatching are otherwise not level changes. A command
    /// the state refuses for another reason passes here and is refused by
    /// its own check.
    pub(super) fn guard_owned_crosspoints(
        &self,
        origin: &Origin,
        cmd: &Command,
    ) -> Result<(), CommandError> {
        if self.surfaces_connected.is_empty() {
            return Ok(());
        }
        let mut next = self.intent.clone();
        if self.mutate(&mut next, cmd).is_err() {
            return Ok(());
        }
        let after: BTreeMap<CrosspointKey, Gain> = self
            .grid_of(&next)
            .into_iter()
            .map(|e| (e.x.key(), e.x.gain))
            .collect();
        let before: BTreeMap<CrosspointKey, Gain> = self
            .grid_of(&self.intent)
            .into_iter()
            .map(|e| (e.x.key(), e.x.gain))
            .collect();
        let changed = before
            .iter()
            .filter(|(k, g)| after.get(*k).is_some_and(|n| n != *g));
        let repatched = after.iter().filter(|(k, g)| {
            !before.contains_key(*k)
                && self
                    .unpatched_owned_gains
                    .get(*k)
                    .is_some_and(|kept| kept != *g)
        });
        for (k, _) in changed.chain(repatched) {
            let level = super::surfaces::Level::Crosspoint(k.clone());
            let Some(owner) = self.level_owner(&level) else {
                continue;
            };
            if matches!(origin, Origin::Surface { name } if name == owner) {
                continue;
            }
            return Err(CommandError::LevelOwned {
                what: level.describe(),
                surface: owner.to_owned(),
            });
        }
        Ok(())
    }

    /// The gains of owned crosspoints unpatched so far, after the change
    /// from the current intent to `after`: a crosspoint patched in `after`
    /// is forgotten, one unpatched while a connected fader owns it is
    /// remembered at its gain. `None`: nothing to remember or forget.
    pub(super) fn unpatched_owned_after(
        &self,
        after: &StateFile,
    ) -> Option<BTreeMap<CrosspointKey, Gain>> {
        if self.surfaces_connected.is_empty() && self.unpatched_owned_gains.is_empty() {
            return None;
        }
        // What decides which crosspoints are patched (a level change
        // touches none of it).
        let (a, b) = (&self.intent, after);
        let same_patching = a.router.saved == b.router.saved
            && a.card_bridge == b.card_bridge
            && a.router.crosspoint.len() == b.router.crosspoint.len()
            && a.router
                .crosspoint
                .iter()
                .zip(&b.router.crosspoint)
                .all(|(x, y)| x.source == y.source && x.destination == y.destination)
            && a.device.len() == b.device.len()
            && a.device.iter().zip(&b.device).all(|((ka, x), (kb, y))| {
                ka == kb
                    && x.monitor == y.monitor
                    && (x.rx_channels, x.tx_channels) == (y.rx_channels, y.tx_channels)
            });
        if same_patching {
            return None;
        }
        let patched: BTreeSet<CrosspointKey> =
            self.grid_of(after).into_iter().map(|e| e.x.key()).collect();
        let mut memory = self.unpatched_owned_gains.clone();
        memory.retain(|k, _| !patched.contains(k));
        for e in self.grid_of(&self.intent) {
            let k = e.x.key();
            if patched.contains(&k) {
                continue;
            }
            if self
                .level_owner(&super::surfaces::Level::Crosspoint(k.clone()))
                .is_some()
            {
                memory.insert(k, e.x.gain);
            }
        }
        Some(memory)
    }

    /// Router events for what changed between two intents: `RouterChanged`
    /// when the grid's shape, the saved flag, the labels or the ports that
    /// exist changed (`ports_changed`: the channel counts); crosspoint gain
    /// and mute changes; local port level changes.
    pub(super) fn router_events(
        &self,
        before: &StateFile,
        after: &StateFile,
        ports_changed: bool,
        origin: &Origin,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        let relevant = ports_changed
            || before.router.saved != after.router.saved
            || before.router.crosspoint != after.router.crosspoint
            || before.router.label != after.router.label
            || before.card_bridge != after.card_bridge
            || before
                .device
                .iter()
                .map(|(k, d)| (k, &d.monitor))
                .ne(after.device.iter().map(|(k, d)| (k, &d.monitor)));
        if relevant {
            let a = self.grid_of(before);
            let b = self.grid_of(after);
            let ka: BTreeSet<CrosspointKey> = a.iter().map(|e| e.x.key()).collect();
            let kb: BTreeSet<CrosspointKey> = b.iter().map(|e| e.x.key()).collect();
            if ka != kb
                || ports_changed
                || before.router.saved != after.router.saved
                || before.router.label != after.router.label
                || before.card_bridge != after.card_bridge
            {
                out.push(Event::RouterChanged {
                    origin: origin.clone(),
                });
            }
            let old: BTreeMap<CrosspointKey, (Gain, bool)> = a
                .iter()
                .map(|e| (e.x.key(), (e.x.gain, e.x.muted)))
                .collect();
            let mut by_gain: Vec<(Gain, Vec<CrosspointKey>)> = Vec::new();
            let mut by_mute: Vec<(bool, Vec<CrosspointKey>)> = Vec::new();
            for e in &b {
                let k = e.x.key();
                let Some((g, m)) = old.get(&k) else { continue };
                if *g != e.x.gain {
                    match by_gain.iter_mut().find(|(x, _)| *x == e.x.gain) {
                        Some((_, l)) => l.push(k.clone()),
                        None => by_gain.push((e.x.gain, vec![k.clone()])),
                    }
                }
                if *m != e.x.muted {
                    match by_mute.iter_mut().find(|(x, _)| *x == e.x.muted) {
                        Some((_, l)) => l.push(k),
                        None => by_mute.push((e.x.muted, vec![k])),
                    }
                }
            }
            out.extend(by_gain.into_iter().map(|(gain, crosspoints)| {
                Event::CrosspointGainChanged {
                    crosspoints,
                    gain,
                    origin: origin.clone(),
                }
            }));
            out.extend(by_mute.into_iter().map(|(muted, crosspoints)| {
                Event::CrosspointMuteChanged {
                    crosspoints,
                    muted,
                    origin: origin.clone(),
                }
            }));
        }
        if before.router.level != after.router.level {
            let level = |s: &StateFile, p: &RouterPort| {
                s.router
                    .level
                    .iter()
                    .find(|l| l.port == *p)
                    .map_or((Gain::default(), false), |l| (l.gain, l.muted))
            };
            let ports: BTreeSet<&RouterPort> = before
                .router
                .level
                .iter()
                .chain(&after.router.level)
                .map(|l| &l.port)
                .collect();
            let mut by_gain: Vec<(Gain, Vec<RouterPort>)> = Vec::new();
            let mut by_mute: Vec<(bool, Vec<RouterPort>)> = Vec::new();
            for p in ports {
                let ((ga, ma), (gb, mb)) = (level(before, p), level(after, p));
                if ga != gb {
                    match by_gain.iter_mut().find(|(x, _)| *x == gb) {
                        Some((_, l)) => l.push(p.clone()),
                        None => by_gain.push((gb, vec![p.clone()])),
                    }
                }
                if ma != mb {
                    match by_mute.iter_mut().find(|(x, _)| *x == mb) {
                        Some((_, l)) => l.push(p.clone()),
                        None => by_mute.push((mb, vec![p.clone()])),
                    }
                }
            }
            out.extend(
                by_gain
                    .into_iter()
                    .map(|(gain, ports)| Event::PortGainChanged {
                        ports,
                        gain,
                        origin: origin.clone(),
                    }),
            );
            out.extend(
                by_mute
                    .into_iter()
                    .map(|(muted, ports)| Event::PortMuteChanged {
                        ports,
                        muted,
                        origin: origin.clone(),
                    }),
            );
        }
        out
    }

    /// Invariants of the saved router: each pair, label and level once; no
    /// crosspoints unless saved.
    pub(super) fn check_router_invariants(&self) -> Result<(), String> {
        let r = &self.intent.router;
        if !r.saved && !r.crosspoint.is_empty() {
            return Err("router crosspoints without a saved grid".into());
        }
        let mut keys = BTreeSet::new();
        if let Some(x) = r.crosspoint.iter().find(|x| !keys.insert(x.key())) {
            return Err(format!("crosspoint {} twice", describe_key(&x.key())));
        }
        let mut ports = BTreeSet::new();
        if let Some(l) = r.label.iter().find(|l| !ports.insert(&l.port)) {
            return Err(format!("label of {} twice", describe(&l.port)));
        }
        let mut ports = BTreeSet::new();
        for l in &r.level {
            if !ports.insert(&l.port) {
                return Err(format!("level of {} twice", describe(&l.port)));
            }
            if l.gain == Gain::default() && !l.muted {
                return Err(format!("stored unity level of {}", describe(&l.port)));
            }
        }
        Ok(())
    }
}
