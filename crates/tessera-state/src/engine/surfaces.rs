//! Channel gain and mute, and hardware control surfaces (provisional 0.x).
//!
//! Gain, mute and the surface list are saved intent in `state.toml`, changed
//! by commands like any other setting. Which surfaces are connected is
//! runtime state the daemon's surface runtime reports
//! ([`Command::ReportSurfaceConnection`], `Origin::Internal` only); it decides
//! **fader priority**: the channels a connected, enabled surface's fader
//! without a motor is bound to take their gain from that surface only, and
//! `SetChannelGain` from any other origin is refused with
//! [`CommandError::GainOwned`]. Mute is never owned; motorized faders and
//! encoders own nothing.
//!
//! A `follow` binding acts exactly like the control it follows: the daemon
//! applies that control's bindings with the followed surface as the origin
//! (through chains), so the follower is accepted where the followed control
//! is and refused where it is. It owns nothing itself: ownership is the
//! followed control's (its motor, its surface connected and enabled). The
//! access lock covers what the followed bindings act on
//! (`surface_command_coverage`).
//!
//! Ownership never traps the user:
//! - A `SetSurface` that adds a surface, changes its `source`, or enables or
//!   disables it gives it a fresh **connection epoch** and drops its
//!   connected flag (the daemon closes the device and reports again, under
//!   the new epoch, once it opens the new one); any other edit keeps both.
//!   A connection report is refused for a disabled surface and for an
//!   epoch that is not the current one, so a late report cannot bring an
//!   old connection back.
//! - Only channels the device has now are owned. A binding whose target
//!   no longer exists (the channel count shrank) owns nothing; it is kept
//!   and applies again if the count grows back. Targets are checked only for
//!   enabled surfaces, and a binding whose target is the one already saved
//!   for the enabled surface is not checked again, so such a surface can
//!   still be disabled, edited or removed. The one-owner-per-channel check
//!   covers every configured target, existing or not.
//! - `ClearConfiguration` resets mute and gain but keeps the levels a
//!   connected fader owns (channel gains, RX output levels): the fader sets
//!   them, and resetting them would show a level the fader does not match
//!   until it is moved.
//!
//! RX channels also have an OUTPUT stage after the local mix
//! (`SetOutputGain` / `SetOutputMute`): a master level and mute for what the
//! host plays. It is saved like the channel controls, never owned (a
//! surface fader bound to it sets it like any other origin) and reset by
//! `ClearConfiguration`.

use std::collections::{BTreeMap, BTreeSet};

use tessera_control_types::{
    BindingAction, Channel, ChannelSet, Command, CommandError, ControlKind, ControlRef,
    CrosspointKey, DeviceKey, DeviceStatus, Direction, Event, GAIN_CEILING_MDB, GAIN_FLOOR_MDB,
    Gain, Origin, RouterPort, SurfaceBinding, SurfaceSource, SurfaceSpec,
};

use super::{Applied, StateEngine, invalid, not_found};
use crate::persist::{DeviceIntent, StateFile};

fn side(direction: Direction) -> &'static str {
    match direction {
        Direction::Rx => "rx",
        Direction::Tx => "tx",
    }
}

fn gains(d: &DeviceIntent, direction: Direction) -> &BTreeMap<String, Gain> {
    match direction {
        Direction::Rx => &d.rx_channel_gain,
        Direction::Tx => &d.tx_channel_gain,
    }
}

fn gains_mut(d: &mut DeviceIntent, direction: Direction) -> &mut BTreeMap<String, Gain> {
    match direction {
        Direction::Rx => &mut d.rx_channel_gain,
        Direction::Tx => &mut d.tx_channel_gain,
    }
}

fn muted(d: &DeviceIntent, direction: Direction) -> &[u16] {
    match direction {
        Direction::Rx => &d.rx_muted,
        Direction::Tx => &d.tx_muted,
    }
}

fn muted_mut(d: &mut DeviceIntent, direction: Direction) -> &mut Vec<u16> {
    match direction {
        Direction::Rx => &mut d.rx_muted,
        Direction::Tx => &mut d.tx_muted,
    }
}

/// One level a fader can set, in canonical form: a network port's level is
/// its channel's gain, a host output's level the RX channel's output level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Level {
    /// The gain of a network channel (`SetChannelGain`; `NetworkRx` /
    /// `NetworkTx` port levels).
    Channel {
        device: String,
        direction: Direction,
        channel: u16,
    },
    /// The output level of an RX channel (`SetOutputGain`; `HostOut`).
    Output { device: String, channel: u16 },
    /// The level of a local router port (`AppOut`, `CardIn`, `CardOut`).
    Port(RouterPort),
    /// The gain of a crosspoint.
    Crosspoint(CrosspointKey),
    /// The host volume.
    HostVolume,
}

impl Level {
    /// The level of a port.
    pub(super) fn of_port(p: &RouterPort) -> Self {
        let channel = p.channel().get();
        match p {
            RouterPort::NetworkRx { device, .. } => Self::Channel {
                device: device.0.clone(),
                direction: Direction::Rx,
                channel,
            },
            RouterPort::NetworkTx { device, .. } => Self::Channel {
                device: device.0.clone(),
                direction: Direction::Tx,
                channel,
            },
            RouterPort::HostOut { device, .. } => Self::Output {
                device: device.0.clone(),
                channel,
            },
            other => Self::Port(other.clone()),
        }
    }

    /// The level in words, for `LevelOwned`.
    pub(super) fn describe(&self) -> String {
        match self {
            Self::Channel {
                device,
                direction,
                channel,
            } => format!("gain of `{device}` {} channel {channel}", side(*direction)),
            Self::Output { device, channel } => {
                format!("output level of `{device}` rx channel {channel}")
            }
            Self::Port(p) => format!("level of {}", crate::router::describe(p)),
            Self::Crosspoint(x) => format!("gain of crosspoint {}", crate::router::describe_key(x)),
            Self::HostVolume => "host volume".to_owned(),
        }
    }
}

/// The levels a binding sets (level actions only; empty for the others).
pub(super) fn levels_of(b: &SurfaceBinding) -> Vec<Level> {
    let channels = |output: bool| -> Vec<Level> {
        b.target
            .channels
            .iter()
            .map(|c| {
                if output {
                    Level::Output {
                        device: b.target.device.0.clone(),
                        channel: c.get(),
                    }
                } else {
                    Level::Channel {
                        device: b.target.device.0.clone(),
                        direction: b.target.direction,
                        channel: c.get(),
                    }
                }
            })
            .collect()
    };
    match b.action {
        BindingAction::Gain => channels(false),
        BindingAction::OutputGain => channels(true),
        BindingAction::PortGain => b.ports.iter().map(Level::of_port).collect(),
        BindingAction::CrosspointGain => b
            .crosspoints
            .iter()
            .cloned()
            .map(Level::Crosspoint)
            .collect(),
        BindingAction::HostVolume => vec![Level::HostVolume],
        _ => Vec::new(),
    }
}

/// The levels a fader without a motor of `s` owns (fader priority): its
/// bindings whose action owns a level (`Gain`, `PortGain`,
/// `CrosspointGain`, `HostVolume`; not `OutputGain`).
fn owned(s: &SurfaceSpec) -> impl Iterator<Item = Level> + '_ {
    s.bindings
        .iter()
        .filter(|b| {
            b.action.owns_level()
                && s.controls
                    .iter()
                    .any(|c| c.id == b.control && c.kind == ControlKind::Fader && !c.motorized)
        })
        .flat_map(levels_of)
}

fn owns(s: &SurfaceSpec, level: &Level) -> bool {
    owned(s).any(|l| l == *level)
}

/// `Invalid` unless `gain` is off or in range.
fn check_gain(gain: Gain) -> Result<(), CommandError> {
    if gain.is_valid() {
        Ok(())
    } else {
        Err(invalid(
            "gain",
            format!("the gain must be off or {GAIN_FLOOR_MDB}..={GAIN_CEILING_MDB} milli-dB"),
        ))
    }
}

/// Mute (or unmute) `target` in `intent`; whether anything changed.
fn set_mute(intent: &mut StateFile, target: &ChannelSet, on: bool) -> bool {
    let list = muted_mut(
        intent.device.entry(target.device.0.clone()).or_default(),
        target.direction,
    );
    mute_in(list, target, on)
}

/// Mute (or unmute) the output of RX `target` in `intent`; whether anything
/// changed.
fn set_output_mute(intent: &mut StateFile, target: &ChannelSet, on: bool) -> bool {
    let d = intent.device.entry(target.device.0.clone()).or_default();
    mute_in(&mut d.rx_output_muted, target, on)
}

/// Add `target`'s channels to (or remove them from) the ascending `list`;
/// whether anything changed.
fn mute_in(list: &mut Vec<u16>, target: &ChannelSet, on: bool) -> bool {
    let mut changed = false;
    for c in &target.channels {
        match (list.binary_search(&c.get()), on) {
            (Ok(i), false) => {
                list.remove(i);
                changed = true;
            }
            (Err(i), true) => {
                list.insert(i, c.get());
                changed = true;
            }
            _ => {}
        }
    }
    changed
}

/// Set the gain of `target` in `intent` (unity is not stored); whether
/// anything changed.
fn set_gain(intent: &mut StateFile, target: &ChannelSet, gain: Gain) -> bool {
    let map = gains_mut(
        intent.device.entry(target.device.0.clone()).or_default(),
        target.direction,
    );
    gain_in(map, target, gain)
}

/// Set the output level of RX `target` in `intent` (unity is not stored);
/// whether anything changed.
fn set_output_gain(intent: &mut StateFile, target: &ChannelSet, gain: Gain) -> bool {
    let d = intent.device.entry(target.device.0.clone()).or_default();
    gain_in(&mut d.rx_output_gain, target, gain)
}

/// Set the level (`output`: the RX output stage) and/or mute of `target` in
/// `intent`; whether anything changed. `SetPortGain` / `SetPortMute` on
/// network ports and host outputs use it, so they change the same stored
/// values as the channel commands.
pub(super) fn set_level(
    intent: &mut StateFile,
    target: &ChannelSet,
    output: bool,
    gain: Option<Gain>,
    muted: Option<bool>,
) -> bool {
    let mut changed = false;
    if let Some(g) = gain {
        changed |= if output {
            set_output_gain(intent, target, g)
        } else {
            set_gain(intent, target, g)
        };
    }
    if let Some(m) = muted {
        changed |= if output {
            set_output_mute(intent, target, m)
        } else {
            set_mute(intent, target, m)
        };
    }
    changed
}

/// Set `target`'s channels to `gain` in `map` (unity is not stored);
/// whether anything changed.
fn gain_in(map: &mut BTreeMap<String, Gain>, target: &ChannelSet, gain: Gain) -> bool {
    let mut changed = false;
    for c in &target.channels {
        let key = c.get().to_string();
        changed |= if gain == Gain::default() {
            map.remove(&key).is_some()
        } else {
            map.insert(key, gain) != Some(gain)
        };
    }
    changed
}

impl StateEngine {
    /// The saved gain of a channel (unity unless set).
    #[must_use]
    pub fn channel_gain(&self, device: &str, direction: Direction, channel: u16) -> Gain {
        self.intent
            .device
            .get(device)
            .and_then(|d| gains(d, direction).get(&channel.to_string()).copied())
            .unwrap_or_default()
    }

    /// Is a channel muted (saved intent)?
    #[must_use]
    pub fn channel_muted(&self, device: &str, direction: Direction, channel: u16) -> bool {
        self.intent
            .device
            .get(device)
            .is_some_and(|d| muted(d, direction).binary_search(&channel).is_ok())
    }

    /// The saved output level of a channel: applied after the local mix
    /// (`SetOutputGain`). Unity unless set; always unity for TX (only RX
    /// channels have an output stage).
    #[must_use]
    pub fn output_gain(&self, device: &str, direction: Direction, channel: u16) -> Gain {
        if direction != Direction::Rx {
            return Gain::default();
        }
        self.intent
            .device
            .get(device)
            .and_then(|d| d.rx_output_gain.get(&channel.to_string()).copied())
            .unwrap_or_default()
    }

    /// Is a channel's output muted (`SetOutputMute`, saved intent)? Always
    /// false for TX.
    #[must_use]
    pub fn output_muted(&self, device: &str, direction: Direction, channel: u16) -> bool {
        direction == Direction::Rx
            && self
                .intent
                .device
                .get(device)
                .is_some_and(|d| d.rx_output_muted.binary_search(&channel).is_ok())
    }

    /// The configured control surfaces, in the order they were added.
    #[must_use]
    pub fn surfaces(&self) -> &[SurfaceSpec] {
        &self.intent.surface
    }

    /// Did the surface runtime report `name` as connected?
    #[must_use]
    pub fn surface_connected(&self, name: &str) -> bool {
        self.surfaces_connected.contains(name)
    }

    /// The surface whose fader sets this channel's gain now (connected,
    /// enabled, a fader without a motor bound to it), if any. A channel the
    /// device does not have (now) has no owner.
    #[must_use]
    pub fn gain_owner(&self, device: &str, direction: Direction, channel: u16) -> Option<&str> {
        self.level_owner(&Level::Channel {
            device: device.to_owned(),
            direction,
            channel,
        })
    }

    /// The surface whose fader sets the output level of RX `channel` (a
    /// `PortGain` fader on its `HostOut` port), if any.
    #[must_use]
    pub fn output_gain_owner(&self, device: &str, channel: u16) -> Option<&str> {
        self.level_owner(&Level::Output {
            device: device.to_owned(),
            channel,
        })
    }

    /// The surface whose fader sets the level of a router port, if any.
    #[must_use]
    pub fn port_owner(&self, port: &RouterPort) -> Option<&str> {
        self.level_owner(&Level::of_port(port))
    }

    /// The surface whose fader sets the gain of a crosspoint, if any.
    #[must_use]
    pub fn crosspoint_owner(&self, crosspoint: &CrosspointKey) -> Option<&str> {
        self.level_owner(&Level::Crosspoint(crosspoint.clone()))
    }

    /// The surface whose fader sets the host volume, if any.
    #[must_use]
    pub fn host_volume_owner(&self) -> Option<&str> {
        self.level_owner(&Level::HostVolume)
    }

    /// The surface whose non-motorized fader owns `level` now (connected,
    /// enabled). A channel the device does not have (now) has no owner.
    pub(super) fn level_owner(&self, level: &Level) -> Option<&str> {
        let count = |device: &str, direction: Direction| {
            self.model.device(device).map_or(0, |d| match direction {
                Direction::Rx => d.rx_channels,
                Direction::Tx => d.tx_channels,
            })
        };
        let exists = match level {
            Level::Channel {
                device,
                direction,
                channel,
            } => *channel != 0 && *channel <= count(device, *direction),
            Level::Output { device, channel } => {
                *channel != 0 && *channel <= count(device, Direction::Rx)
            }
            _ => true,
        };
        if !exists {
            return None;
        }
        self.intent
            .surface
            .iter()
            .filter(|s| s.enabled && self.surfaces_connected.contains(&s.name))
            .find(|s| owns(s, level))
            .map(|s| s.name.as_str())
    }

    /// `Invalid` unless `set` names a configured device and channels it has.
    pub(super) fn check_channel_set(&self, set: &ChannelSet) -> Result<(), CommandError> {
        let Some(d) = self.model.device(&set.device.0) else {
            return Err(invalid(
                "device",
                format!("there is no device `{}`", set.device.0),
            ));
        };
        if set.channels.is_empty() {
            return Err(invalid("channels", "list at least one channel"));
        }
        let count = match set.direction {
            Direction::Rx => d.rx_channels,
            Direction::Tx => d.tx_channels,
        };
        if let Some(c) = set.channels.iter().find(|c| c.get() > count) {
            return Err(invalid(
                "channels",
                format!(
                    "channel {c} is beyond the device's {count} {} channel(s)",
                    side(set.direction)
                ),
            ));
        }
        Ok(())
    }

    /// `check_channel_set`, and only RX channels: the output stage is after
    /// the local mix, on the way to the host.
    pub(super) fn check_output_set(&self, set: &ChannelSet) -> Result<(), CommandError> {
        self.check_channel_set(set)?;
        if set.direction != Direction::Rx {
            return Err(invalid(
                "direction",
                "only RX channels have an output level and mute",
            ));
        }
        Ok(())
    }

    /// Fader priority: a command that sets a level a surface owns
    /// (`SetChannelGain`, `SetPortGain`, `SetCrosspointGain`,
    /// `SetHostVolume`, and `SetRouter` / `ResetRouter` / `SetCrosspoint` /
    /// `SetMonitor` where they would change an owned crosspoint's gain) is
    /// accepted only from that surface. Validation errors come first.
    pub(super) fn guard_gain_owner(
        &self,
        origin: &Origin,
        cmd: &Command,
    ) -> Result<(), CommandError> {
        // The levels the command sets.
        let levels: Vec<Level> = match cmd {
            Command::SetChannelGain { target, .. } => {
                self.check_channel_set(target)?;
                target
                    .channels
                    .iter()
                    .map(|c| Level::Channel {
                        device: target.device.0.clone(),
                        direction: target.direction,
                        channel: c.get(),
                    })
                    .collect()
            }
            Command::SetOutputGain { target, .. } => {
                self.check_output_set(target)?;
                target
                    .channels
                    .iter()
                    .map(|c| Level::Output {
                        device: target.device.0.clone(),
                        channel: c.get(),
                    })
                    .collect()
            }
            Command::SetPortGain { ports, .. } => ports.iter().map(Level::of_port).collect(),
            Command::SetCrosspointGain { crosspoints, .. } => {
                crosspoints.iter().cloned().map(Level::Crosspoint).collect()
            }
            Command::SetHostVolume { .. } => vec![Level::HostVolume],
            // Commands that rewrite crosspoints whole, gain included.
            Command::SetRouter { .. }
            | Command::ResetRouter
            | Command::SetCrosspoint { .. }
            | Command::SetMonitor { .. } => return self.guard_owned_crosspoints(origin, cmd),
            _ => return Ok(()),
        };
        for level in &levels {
            let Some(owner) = self.level_owner(level) else {
                continue;
            };
            if matches!(origin, Origin::Surface { name } if name == owner) {
                continue;
            }
            return Err(match level {
                Level::Channel {
                    device,
                    direction,
                    channel,
                } => CommandError::GainOwned {
                    device: DeviceKey(device.clone()),
                    direction: *direction,
                    channel: Channel::new(*channel).unwrap_or(Channel::MIN),
                    surface: owner.to_owned(),
                },
                other => CommandError::LevelOwned {
                    what: other.describe(),
                    surface: owner.to_owned(),
                },
            });
        }
        Ok(())
    }

    /// The whole check of a surface: its own rules (tessera-surface), its
    /// targets against the device list, and one fader owner per channel.
    ///
    /// Targets are checked only when the surface is enabled, and not for a
    /// binding whose target is one the enabled surface already has saved: a
    /// target that stopped existing (the channel count shrank) must not
    /// stop the user from editing the rest of the surface.
    fn validate_surface(&self, s: &SurfaceSpec, intent: &StateFile) -> Result<(), CommandError> {
        let bad = |m: String| invalid("surface", format!("surface {:?}: {m}", s.name));
        self.check_new_binding_fields(s)?;
        tessera_surface::SurfaceConfig::from(s)
            .validate()
            .map_err(|e| invalid("surface", e.to_string()))?;
        if let SurfaceSource::TcpMidi { host, port } = &s.source {
            if !self.env.runtime.tcp_midi {
                return Err(CommandError::Unsupported {
                    what: "a tcp_midi surface: this tesserad cannot connect MIDI over TCP yet"
                        .into(),
                });
            }
            let host_ok = !host.is_empty()
                && host.len() <= 253
                && !host.chars().any(|c| c.is_whitespace() || c.is_control());
            if !host_ok || *port == 0 {
                return Err(bad(
                    "tcp_midi needs a host name or address (1..=253 bytes, no spaces) and a port 1..=65535"
                        .into(),
                ));
            }
        }
        let saved = intent
            .surface
            .iter()
            .find(|o| o.name == s.name && o.enabled);
        // A binding the enabled surface already has saved is not checked
        // against the devices, ports and grid again.
        let unchanged = |b: &SurfaceBinding| saved.is_some_and(|o| o.bindings.contains(b));
        let unchanged_target = |target: &ChannelSet| {
            saved.is_some_and(|o| o.bindings.iter().any(|b| b.target == *target))
        };
        let bridges = self.bridges_of(intent);
        let grid = self.grid_of(intent);
        for b in s.bindings.iter().filter(|_| s.enabled) {
            let of = |m: String| bad(format!("binding of {:?}: {m}", b.control));
            if b.action.uses_channels() {
                if unchanged_target(&b.target) {
                    continue;
                }
                self.check_channel_set(&b.target).map_err(|e| match e {
                    CommandError::Invalid { reason, .. } => of(reason),
                    other => other,
                })?;
                continue;
            }
            if unchanged(b) {
                continue;
            }
            match b.action {
                BindingAction::HostVolume | BindingAction::HostMute => {
                    self.need_host_volume("a host volume binding")?;
                }
                BindingAction::PortGain | BindingAction::PortMute => {
                    self.need_router("a router port binding")?;
                    for p in &b.ports {
                        if let Some(m) = self.port_problem(p, &bridges) {
                            return Err(of(format!("{}: {m}", crate::router::describe(p))));
                        }
                    }
                }
                BindingAction::CrosspointGain | BindingAction::CrosspointMute => {
                    self.need_router("a crosspoint binding")?;
                    for x in &b.crosspoints {
                        match grid.iter().find(|e| e.x.key() == *x) {
                            None => {
                                return Err(of(format!(
                                    "{} is not patched",
                                    crate::router::describe_key(x)
                                )));
                            }
                            Some(e)
                                if matches!(
                                    e.origin,
                                    tessera_control_types::CrosspointOrigin::Bridge { .. }
                                ) =>
                            {
                                return Err(of(format!(
                                    "{} is patched by a card bridge: bind its ports' levels",
                                    crate::router::describe_key(x)
                                )));
                            }
                            Some(_) => {}
                        }
                    }
                }
                BindingAction::Follow => {
                    if !self.env.runtime.surface_follow {
                        return Err(CommandError::Unsupported {
                            what:
                                "a follow binding: this tesserad cannot drive surface controls yet"
                                    .into(),
                        });
                    }
                    let Some(to) = &b.follow else {
                        return Err(of("a follow binding needs `follow`".into()));
                    };
                    let kind = s
                        .controls
                        .iter()
                        .find(|c| c.id == b.control)
                        .map(|c| c.kind);
                    let target = if to.surface == s.name {
                        Some(s)
                    } else {
                        intent.surface.iter().find(|o| o.name == to.surface)
                    };
                    let Some(target) = target else {
                        return Err(of(format!("there is no surface {:?}", to.surface)));
                    };
                    let Some(tc) = target.controls.iter().find(|c| c.id == to.control) else {
                        return Err(of(format!(
                            "surface {:?} has no control {:?}",
                            to.surface, to.control
                        )));
                    };
                    let fits = matches!(
                        (kind, tc.kind),
                        (Some(ControlKind::Fader), ControlKind::Fader)
                            | (Some(ControlKind::Button), ControlKind::Button)
                    );
                    if !fits {
                        return Err(of("a fader follows a fader and a button a button".into()));
                    }
                }
                _ => {}
            }
        }
        // Two level bindings of one surface on one level would not see each
        // other's changes (they are one origin).
        let mut mine: Vec<(Level, &str)> = Vec::new();
        for b in &s.bindings {
            for l in levels_of(b) {
                if let Some((_, other)) = mine.iter().find(|(o, _)| *o == l) {
                    if *other != b.control {
                        return Err(bad(format!(
                            "binding of {:?}: the {} is already set by its binding of {other:?}",
                            b.control,
                            l.describe()
                        )));
                    }
                }
                mine.push((l, &b.control));
            }
        }
        // Control routing: no cycle of follow bindings (a pair is fine).
        // Only enabled surfaces drive anything: links from or to a disabled
        // surface are left out (enabling it checks them again).
        let mut surfaces: Vec<&SurfaceSpec> = intent
            .surface
            .iter()
            .filter(|o| o.name != s.name && o.enabled)
            .collect();
        if s.enabled {
            surfaces.push(s);
        }
        let enabled: BTreeSet<&str> = surfaces.iter().map(|o| o.name.as_str()).collect();
        let links: Vec<(ControlRef, ControlRef)> = surfaces
            .iter()
            .flat_map(|o| {
                o.bindings.iter().filter_map(|b| {
                    let to = b
                        .follow
                        .as_ref()
                        .filter(|_| b.action == BindingAction::Follow)
                        .filter(|to| enabled.contains(to.surface.as_str()))?;
                    Some((ControlRef::new(&o.name, &b.control), to.clone()))
                })
            })
            .collect();
        if let Some((from, to)) =
            crate::router::find_follow_cycle(links.iter().map(|(a, b)| (a, b)))
        {
            return Err(bad(format!(
                "following {}/{} from {}/{} closes a cycle of controls driving each other (a pair following each other is allowed)",
                to.surface, to.control, from.surface, from.control
            )));
        }
        if s.enabled {
            let clash = intent
                .surface
                .iter()
                .filter(|o| o.enabled && o.name != s.name)
                .find_map(|o| owned(s).find(|l| owns(o, l)).map(|l| (o, l)));
            if let Some((o, l)) = clash {
                return Err(bad(format!(
                    "the {} is already set by a fader of surface {:?}",
                    l.describe(),
                    o.name
                )));
            }
        }
        Ok(())
    }

    /// The target fields fit the action (channel actions: `target`; host
    /// actions: none; port actions: `ports`; crosspoint actions:
    /// `crosspoints`; `follow`: `follow`).
    fn check_new_binding_fields(&self, s: &SurfaceSpec) -> Result<(), CommandError> {
        for b in &s.bindings {
            let of = |m: &str| {
                invalid(
                    "surface",
                    format!("surface {:?}: binding of {:?}: {m}", s.name, b.control),
                )
            };
            let (target, ports, crosspoints, follow) = (
                !b.target.is_empty(),
                !b.ports.is_empty(),
                !b.crosspoints.is_empty(),
                b.follow.is_some(),
            );
            let want = match b.action {
                a if a.uses_channels() => (true, false, false, false),
                BindingAction::HostVolume | BindingAction::HostMute => (false, false, false, false),
                BindingAction::PortGain | BindingAction::PortMute => (false, true, false, false),
                BindingAction::CrosspointGain | BindingAction::CrosspointMute => {
                    (false, false, true, false)
                }
                BindingAction::Follow => (false, false, false, true),
                _ => return Err(of("this action is not known to this tesserad")),
            };
            // A host binding acts on the host volume alone: say which field
            // to remove. An empty `target` (no channels) counts as absent.
            if matches!(
                b.action,
                BindingAction::HostVolume | BindingAction::HostMute
            ) {
                let extra = [
                    (target, "target", "channels"),
                    (ports, "ports", "router ports"),
                    (crosspoints, "crosspoints", "crosspoints"),
                    (follow, "follow", "another surface's control"),
                ]
                .into_iter()
                .find(|(set, _, _)| *set);
                if let Some((_, field, what)) = extra {
                    let does = if b.action == BindingAction::HostVolume {
                        "a host volume binding acts on the host volume"
                    } else {
                        "a host mute binding acts on the host mute"
                    };
                    return Err(of(&format!("remove `{field}`: {does}, not on {what}")));
                }
                continue;
            }
            // Channel actions report a missing target in the surface check.
            if b.action.uses_channels() {
                if ports || crosspoints || follow {
                    return Err(of("a channel binding has only `target`"));
                }
                continue;
            }
            if (target, ports, crosspoints, follow) != want {
                return Err(of(match b.action {
                    BindingAction::PortGain | BindingAction::PortMute => {
                        "a port binding needs `ports` (and nothing else)"
                    }
                    BindingAction::CrosspointGain | BindingAction::CrosspointMute => {
                        "a crosspoint binding needs `crosspoints` (and nothing else)"
                    }
                    _ => "a follow binding needs `follow` (and nothing else)",
                }));
            }
            if b.action == BindingAction::Follow {
                let kind = s
                    .controls
                    .iter()
                    .find(|c| c.id == b.control)
                    .map(|c| c.kind);
                if kind == Some(ControlKind::Encoder) {
                    return Err(of("an encoder cannot follow; use a fader or a button"));
                }
            }
        }
        Ok(())
    }

    /// `mutate` for the channel and surface commands; `None` for any other
    /// command.
    pub(super) fn mutate_surface_intent(
        &self,
        intent: &mut StateFile,
        cmd: &Command,
    ) -> Option<Result<bool, CommandError>> {
        Some(match cmd {
            Command::SetChannelMute { target, muted } => self
                .check_channel_set(target)
                .map(|()| set_mute(intent, target, *muted)),
            Command::SetChannelGain { target, gain } => self
                .check_channel_set(target)
                .and_then(|()| check_gain(*gain))
                .map(|()| set_gain(intent, target, *gain)),
            Command::SetOutputMute { target, muted } => self
                .check_output_set(target)
                .map(|()| set_output_mute(intent, target, *muted)),
            Command::SetOutputGain { target, gain } => self
                .check_output_set(target)
                .and_then(|()| check_gain(*gain))
                .map(|()| set_output_gain(intent, target, *gain)),
            Command::SetSurface { surface } => self.validate_surface(surface, intent).map(|()| {
                match intent.surface.iter_mut().find(|s| s.name == surface.name) {
                    Some(s) if s == surface => false,
                    Some(s) => {
                        s.clone_from(surface);
                        true
                    }
                    None => {
                        intent.surface.push(surface.clone());
                        true
                    }
                }
            }),
            Command::RemoveSurface { name } => {
                let before = intent.surface.len();
                intent.surface.retain(|s| s.name != *name);
                if intent.surface.len() == before {
                    Err(not_found(format!("surface `{name}`")))
                } else {
                    Ok(true)
                }
            }
            _ => return None,
        })
    }

    /// What a surface command could change, for the access lock: the
    /// devices the bindings of the new surface and of the one it replaces or
    /// removes act on (channel targets, the devices of their ports and
    /// crosspoints), and whether any of them acts on the host volume
    /// (`host_volume` / `host_mute`, policy: like `SetHostVolume`, refused
    /// while any device is locked). A `follow` binding acts on whatever the
    /// followed control's bindings act on, followed on through further
    /// `follow` bindings.
    pub(super) fn surface_command_coverage(&self, cmd: &Command) -> (BTreeSet<String>, bool) {
        let (name, new) = match cmd {
            Command::SetSurface { surface } => (&surface.name, Some(surface)),
            Command::RemoveSurface { name } => (name, None),
            _ => return (BTreeSet::new(), false),
        };
        let old = self.intent.surface.iter().find(|s| s.name == *name);
        let mut devices = BTreeSet::new();
        let mut host = false;
        let mut seen: BTreeSet<ControlRef> = BTreeSet::new();
        let mut todo: Vec<&SurfaceBinding> = old
            .into_iter()
            .chain(new)
            .flat_map(|s| &s.bindings)
            .collect();
        while let Some(b) = todo.pop() {
            if !b.target.is_empty() {
                devices.insert(b.target.device.0.clone());
            }
            devices.extend(
                b.ports
                    .iter()
                    .chain(
                        b.crosspoints
                            .iter()
                            .flat_map(|x| [&x.source, &x.destination]),
                    )
                    .filter_map(|p| p.device().map(|d| d.0.clone())),
            );
            match b.action {
                BindingAction::HostVolume | BindingAction::HostMute => host = true,
                BindingAction::Follow => {
                    let Some(to) = &b.follow else { continue };
                    if !seen.insert(to.clone()) {
                        continue;
                    }
                    // The surfaces as they would be: the new one replaces the old.
                    let followed = match new {
                        Some(n) if n.name == to.surface => Some(n),
                        _ => self.intent.surface.iter().find(|s| s.name == to.surface),
                    };
                    if let Some(s) = followed {
                        todo.extend(s.bindings.iter().filter(|x| x.control == to.control));
                    }
                }
                _ => {}
            }
        }
        (devices, host)
    }

    /// The state engine's connection epoch of `name` (see
    /// [`SurfaceStatus::connection_epoch`](tessera_control_types::SurfaceStatus)):
    /// 0 until the surface is added, enabled, disabled or moved to another
    /// source while the daemon runs.
    #[must_use]
    pub fn surface_connection_epoch(&self, name: &str) -> u64 {
        self.surface_epochs.get(name).copied().unwrap_or(0)
    }

    /// After a commit: every surface that `before` did not have, or whose
    /// `enabled` flag or `source` changed, gets a fresh connection epoch and
    /// loses its connected flag (the daemon opens the device again and
    /// reports under the new epoch); a removed surface loses both. Generic
    /// over the command, so two quick edits (disable, then enable with the
    /// same source) still leave the surface unconnected under a new epoch
    /// the daemon can see.
    pub(super) fn renew_surface_epochs(&mut self, before: &[SurfaceSpec]) {
        let renewed: Vec<String> = self
            .intent
            .surface
            .iter()
            .filter(|s| {
                before
                    .iter()
                    .find(|o| o.name == s.name)
                    .is_none_or(|o| o.enabled != s.enabled || o.source != s.source)
            })
            .map(|s| s.name.clone())
            .collect();
        for name in renewed {
            self.next_surface_epoch += 1;
            self.surface_epochs
                .insert(name.clone(), self.next_surface_epoch);
            self.surfaces_connected.remove(&name);
        }
        let names: BTreeSet<&str> = self
            .intent
            .surface
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        self.surface_epochs
            .retain(|n, _| names.contains(n.as_str()));
        self.surfaces_connected
            .retain(|n| names.contains(n.as_str()));
    }

    /// `ClearConfiguration` of `device`: put back, in `next`, the levels of
    /// `before` that a connected fader owns (the fader keeps setting them):
    /// channel gains and RX output levels. The other levels a fader can own
    /// (router port levels, crosspoint gains, the host volume) are not
    /// device intent and `ClearConfiguration` leaves them alone.
    pub(super) fn keep_owned_gains(
        &self,
        device: &str,
        before: &DeviceIntent,
        next: &mut DeviceIntent,
    ) {
        let owned_keys = |map: &BTreeMap<String, Gain>, owned: &dyn Fn(u16) -> bool| {
            map.iter()
                .filter(|(k, _)| k.parse::<u16>().is_ok_and(owned))
                .map(|(k, g)| (k.clone(), *g))
                .collect::<Vec<_>>()
        };
        for direction in [Direction::Rx, Direction::Tx] {
            let kept = owned_keys(gains(before, direction), &|c| {
                self.gain_owner(device, direction, c).is_some()
            });
            gains_mut(next, direction).extend(kept);
        }
        let kept = owned_keys(&before.rx_output_gain, &|c| {
            self.output_gain_owner(device, c).is_some()
        });
        next.rx_output_gain.extend(kept);
    }

    /// `ReportSurfaceConnection`: runtime state, never saved. Only the
    /// daemon itself reports it. A connection is accepted only for an
    /// enabled surface and, when the report names an epoch, under the
    /// surface's current connection epoch; a disconnection under an older
    /// epoch changes nothing (that connection was dropped with the epoch).
    pub(super) fn report_surface_connection(
        &mut self,
        origin: Origin,
        cmd: &Command,
    ) -> Result<Applied, CommandError> {
        let Command::ReportSurfaceConnection {
            name,
            connected,
            epoch,
        } = cmd
        else {
            return Err(invalid("command", "not a surface connection report"));
        };
        if !matches!(origin, Origin::Internal) {
            return Err(CommandError::PermissionDenied);
        }
        let current = self.surface_connection_epoch(name);
        let stale = epoch.is_some_and(|e| e != current);
        let changed = if *connected {
            let Some(s) = self.intent.surface.iter().find(|s| s.name == *name) else {
                return Err(not_found(format!("surface `{name}`")));
            };
            if !s.enabled {
                return Err(invalid(
                    "connected",
                    format!("surface `{name}` is disabled: it has no connection"),
                ));
            }
            if stale {
                return Err(invalid(
                    "epoch",
                    format!(
                        "surface `{name}` changed since (connection epoch {current}): \
                         read the surfaces again and report under the new epoch"
                    ),
                ));
            }
            self.surfaces_connected.insert(name.clone())
        } else if stale {
            false
        } else {
            self.surfaces_connected.remove(name)
        };
        let impact = Some(crate::impact::no_impact(cmd));
        if !changed {
            return Ok(Applied {
                impact,
                ..Applied::default()
            });
        }
        let event = Event::SurfacesChanged { origin };
        self.bus.publish(&event);
        Ok(Applied {
            changed: true,
            events: vec![event],
            effects: Vec::new(),
            impact,
        })
    }

    /// Gain, mute and gain owner of every channel in `status`.
    pub(super) fn fill_channel_controls(&self, status: &mut DeviceStatus) {
        let device = status.key.0.clone();
        for (direction, list) in [
            (Direction::Rx, &mut status.rx_channels),
            (Direction::Tx, &mut status.tx_channels),
        ] {
            for c in list {
                let ch = c.channel.get();
                c.gain = self.channel_gain(&device, direction, ch);
                c.muted = self.channel_muted(&device, direction, ch);
                c.gain_owner = self.gain_owner(&device, direction, ch).map(str::to_owned);
                c.output_gain = self.output_gain(&device, direction, ch);
                c.output_muted = self.output_muted(&device, direction, ch);
                if direction == Direction::Rx {
                    c.output_gain_owner = self.output_gain_owner(&device, ch).map(str::to_owned);
                }
            }
        }
    }

    /// Invariants of the saved channel controls and surfaces.
    pub(super) fn check_surface_invariants(&self) -> Result<(), String> {
        for (key, d) in &self.intent.device {
            for (m, g) in [
                (&d.rx_muted, &d.rx_channel_gain),
                (&d.tx_muted, &d.tx_channel_gain),
                (&d.rx_output_muted, &d.rx_output_gain),
            ] {
                if m.windows(2).any(|w| w[0] >= w[1]) || m.contains(&0) {
                    return Err(format!("{key}: muted channels not ascending"));
                }
                for (ch, g) in g {
                    if ch.parse::<u16>().map_or(true, |c| c == 0) {
                        return Err(format!("{key}: gain of channel `{ch}`"));
                    }
                    if !g.is_valid() || *g == Gain::default() {
                        return Err(format!("{key}: stored gain {g:?} of channel {ch}"));
                    }
                }
            }
        }
        let mut names = BTreeSet::new();
        if let Some(s) = self.intent.surface.iter().find(|s| !names.insert(&s.name)) {
            return Err(format!("surface `{}` twice", s.name));
        }
        Ok(())
    }
}

/// Mute and gain events for what changed between two intents (whatever the
/// command: `ClearConfiguration` resets them too), the output stage's too,
/// and `SurfacesChanged` if the surface list changed. Channels are listed in
/// ascending order, one event per new value.
pub(super) fn channel_events(before: &StateFile, after: &StateFile, origin: &Origin) -> Vec<Event> {
    let mut out = Vec::new();
    let empty = DeviceIntent::default();
    let keys: BTreeSet<&String> = before.device.keys().chain(after.device.keys()).collect();
    for key in keys {
        let a = before.device.get(key).unwrap_or(&empty);
        let b = after.device.get(key).unwrap_or(&empty);
        for direction in [Direction::Rx, Direction::Tx] {
            let set = |channels: Vec<Channel>| ChannelSet {
                device: tessera_control_types::DeviceKey(key.clone()),
                direction,
                channels,
            };
            for (muted, channels) in mute_diff(muted(a, direction), muted(b, direction)) {
                out.push(Event::ChannelMuteChanged {
                    target: set(channels),
                    muted,
                    origin: origin.clone(),
                });
            }
            for (gain, channels) in gain_diff(gains(a, direction), gains(b, direction)) {
                out.push(Event::ChannelGainChanged {
                    target: set(channels),
                    gain,
                    origin: origin.clone(),
                });
            }
            if direction != Direction::Rx {
                continue;
            }
            for (muted, channels) in mute_diff(&a.rx_output_muted, &b.rx_output_muted) {
                out.push(Event::OutputMuteChanged {
                    target: set(channels),
                    muted,
                    origin: origin.clone(),
                });
            }
            for (gain, channels) in gain_diff(&a.rx_output_gain, &b.rx_output_gain) {
                out.push(Event::OutputGainChanged {
                    target: set(channels),
                    gain,
                    origin: origin.clone(),
                });
            }
        }
    }
    if before.surface != after.surface {
        out.push(Event::SurfacesChanged {
            origin: origin.clone(),
        });
    }
    out
}

/// The channels muted (`true`) and unmuted (`false`) from list `a` to `b`,
/// ascending; empty groups left out.
fn mute_diff(a: &[u16], b: &[u16]) -> Vec<(bool, Vec<Channel>)> {
    let all: BTreeSet<u16> = a.iter().chain(b).copied().collect();
    let mut on = Vec::new();
    let mut off = Vec::new();
    for ch in all {
        let Some(c) = Channel::new(ch) else { continue };
        match (a.contains(&ch), b.contains(&ch)) {
            (false, true) => on.push(c),
            (true, false) => off.push(c),
            _ => {}
        }
    }
    [(true, on), (false, off)]
        .into_iter()
        .filter(|(_, c)| !c.is_empty())
        .collect()
}

/// The channels whose gain changed from map `a` to `b`, grouped by new
/// gain in order of their first channel, each group ascending.
fn gain_diff(a: &BTreeMap<String, Gain>, b: &BTreeMap<String, Gain>) -> Vec<(Gain, Vec<Channel>)> {
    let all: BTreeSet<u16> = a
        .keys()
        .chain(b.keys())
        .filter_map(|k| k.parse().ok())
        .collect();
    let mut by_gain: Vec<(Gain, Vec<Channel>)> = Vec::new();
    for ch in all {
        let Some(c) = Channel::new(ch) else { continue };
        let key = ch.to_string();
        let (x, y) = (
            a.get(&key).copied().unwrap_or_default(),
            b.get(&key).copied().unwrap_or_default(),
        );
        if x == y {
            continue;
        }
        match by_gain.iter_mut().find(|(g, _)| *g == y) {
            Some((_, list)) => list.push(c),
            None => by_gain.push((y, vec![c])),
        }
    }
    by_gain
}
