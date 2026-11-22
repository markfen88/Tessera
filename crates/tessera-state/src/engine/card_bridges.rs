//! Sound-card bridges and local monitoring set by command (provisional
//! 0.x).
//!
//! `SetCardBridge` / `RemoveCardBridge` keep a list of bridges in
//! `state.toml` (`[[card_bridge]]`) next to the configuration files'
//! `[[bridge]]` entries. A configured bridge cannot be replaced or removed by
//! command (`Locked`): the files are the administrator's. The daemon's bridge
//! manager runs [`StateEngine::effective_bridges`]: the configured bridges,
//! then the saved ones. A saved bridge whose device is gone, or whose
//! channels the device no longer has, is kept but runs disabled until it
//! fits again (so it stays listed and can be edited or removed).
//!
//! `SetMonitor` saves a device's local monitoring, overriding its
//! `[[device]] monitor_rx_from_tx` / `monitor_delay_us`
//! ([`StateEngine::effective_monitor`]).
//!
//! Neither changes the model, so no new event kind is needed: a commit
//! publishes `RuntimeValueChanged` with field `card_bridge.<name>` or
//! `device.<key>.monitor`, like every other saved value.

use std::collections::BTreeSet;

use tessera_control_types::{
    BridgeDirection as WireDirection, CardBridgeSpec, Command, CommandError, DeviceKey,
    DeviceStatus, Event, Origin,
};

use super::{StateEngine, invalid, locked, not_found};
use crate::config::{BridgeConfig, BridgeDirection, MAX_MONITOR_DELAY_US};
use crate::persist::{MonitorIntent, StateFile};

/// Longest bridge name, bytes.
const MAX_NAME_BYTES: usize = 64;

/// A sound card the daemon found on the host
/// ([`StateEngine::set_sound_cards`]): what a bridge's card channels are
/// checked against.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SoundCard {
    /// ALSA card index (`hw:1`).
    pub index: u32,
    /// Card id (`CQ`), what `hw:CARD=<id>` names.
    pub id: String,
    /// Capture channels (inputs), when known.
    pub capture_channels: Option<u16>,
    /// Playback channels (outputs), when known.
    pub playback_channels: Option<u16>,
}

impl SoundCard {
    /// Does an ALSA device string (`hw:CARD=SC3,DEV=0`, `plughw:1,0`,
    /// `hw:SC3`) name this card?
    #[must_use]
    pub fn named_by(&self, pcm: &str) -> bool {
        let Some((_, args)) = pcm.split_once(':') else {
            return pcm == self.id;
        };
        let first = args.split(',').next().unwrap_or("").trim();
        let first = first.strip_prefix("CARD=").unwrap_or(first);
        first == self.id || first.parse::<u32>().is_ok_and(|n| n == self.index)
    }

    /// The card's channels in a bridge's direction: capture reads its
    /// inputs, playback writes its outputs.
    #[must_use]
    pub fn channels(&self, direction: BridgeDirection) -> Option<u16> {
        match direction {
            BridgeDirection::Capture => self.capture_channels,
            BridgeDirection::Playback => self.playback_channels,
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn config_direction(d: WireDirection) -> BridgeDirection {
    match d {
        WireDirection::Capture => BridgeDirection::Capture,
        _ => BridgeDirection::Playback,
    }
}

/// A saved bridge as a `[[bridge]]` entry with every other key at its
/// default (resampler `auto`, the default loop, priority and buffer).
#[must_use]
pub fn bridge_config(spec: &CardBridgeSpec) -> BridgeConfig {
    let mut b = BridgeConfig::new(&spec.device.0, &spec.card, config_direction(spec.direction));
    b.name.clone_from(&spec.name);
    b.enabled = spec.enabled;
    b.first_channel = spec.first_channel;
    b.channels = Some(spec.channels);
    b.card_first_channel = spec.card_first_channel;
    b.period_frames = spec.period_frames;
    b.local_only = spec.local_only;
    b
}

/// The channels a bridge covers: (device, direction, first, last).
fn span(b: &BridgeConfig, available: u16) -> (String, BridgeDirection, u32, u32) {
    let count = u32::from(b.channel_count(available));
    let first = u32::from(b.first_channel);
    (
        b.device.clone(),
        b.direction,
        first,
        (first + count).saturating_sub(1),
    )
}

impl StateEngine {
    /// The sound-card bridges saved by command, in the order they were
    /// added (configured `[[bridge]]` entries are not included).
    #[must_use]
    pub fn card_bridges(&self) -> &[CardBridgeSpec] {
        &self.intent.card_bridge
    }

    /// Record the sound cards the daemon found on the host (replacing the
    /// previous list). `SetCardBridge` refuses card channels beyond what a
    /// listed card has; a card that is not listed, or whose channel count is
    /// unknown, is not checked (it may be plugged in later).
    pub fn set_sound_cards(&mut self, cards: Vec<SoundCard>) {
        self.sound_cards = cards;
    }

    /// The channel count of the card `pcm` names in `direction`, when
    /// known.
    fn card_channels(&self, pcm: &str, direction: BridgeDirection) -> Option<u16> {
        self.sound_cards
            .iter()
            .find(|c| c.named_by(pcm))
            .and_then(|c| c.channels(direction))
    }

    /// Channels of `device` on the device side of a bridge: TX for capture
    /// (card → network), RX for playback; 0 for an unknown device.
    fn bridge_channels(&self, device: &str, direction: BridgeDirection) -> u16 {
        self.model.device(device).map_or(0, |d| match direction {
            BridgeDirection::Capture => d.tx_channels,
            BridgeDirection::Playback => d.rx_channels,
        })
    }

    /// Every bridge the daemon runs: the configured `[[bridge]]` entries,
    /// then the ones saved by `SetCardBridge` that no configured entry
    /// shadows. A saved bridge whose device is gone or whose channels do not
    /// fit the device now is listed with `enabled = false`.
    ///
    /// A `local_only` bridge (`BridgeConfig::local_only`) opens its card for
    /// the host router only: it reads or feeds no device channel.
    #[must_use]
    pub fn effective_bridges(&self) -> Vec<BridgeConfig> {
        self.bridges_of(&self.intent)
    }

    /// [`effective_bridges`](Self::effective_bridges) with the saved
    /// bridges of `intent`.
    pub(super) fn bridges_of(&self, intent: &StateFile) -> Vec<BridgeConfig> {
        let configured = &self.config.config.bridges;
        let mut out = configured.clone();
        for spec in &intent.card_bridge {
            if configured.iter().any(|b| b.name == spec.name) {
                continue;
            }
            let mut b = bridge_config(spec);
            let available = if spec.local_only {
                spec.channels
            } else {
                self.bridge_channels(&b.device, b.direction)
            };
            // A hand-edited state.toml may hold zeros: such an entry does not fit.
            let first = if spec.local_only {
                1
            } else {
                spec.first_channel
            };
            let last = (u32::from(first) + u32::from(spec.channels)).saturating_sub(1);
            let fits = first > 0
                && spec.channels > 0
                && spec.card_first_channel > 0
                && last <= u32::from(available);
            if self.model.device(&b.device).is_none() || !fits {
                b.enabled = false;
            }
            out.push(b);
        }
        out
    }

    /// Local monitoring of `device`: (`rx_from_tx`, `delay_us`), the saved
    /// `SetMonitor` value or else the configuration's, limited to the
    /// channels the device has now (entries beyond its RX channels are
    /// dropped, TX channels it lacks read as 0). An unknown device has
    /// none.
    ///
    /// Once a host-router grid is saved, the list is read from the grid
    /// instead: RX channel `n` monitors TX channel `t` when the only tap
    /// into `HostOut n` is an active `NetworkTx t` crosspoint at unity, not
    /// muted (what a daemon without the router's mixing engine can play);
    /// otherwise 0. The delay stays the saved or configured one.
    #[must_use]
    pub fn effective_monitor(&self, device: &str) -> (Vec<u16>, u32) {
        let Some(m) = self.model.device(device) else {
            return (Vec::new(), 0);
        };
        let saved = self
            .intent
            .device
            .get(device)
            .and_then(|d| d.monitor.as_ref());
        if self.intent.router.saved {
            let delay = saved.map_or_else(
                || {
                    self.config
                        .config
                        .devices
                        .iter()
                        .find(|d| d.id == device)
                        .map_or(0, |d| d.monitor_delay_us)
                },
                |s| s.delay_us,
            );
            let mut list = self.router_monitor_list(device, m.rx_channels, m.tx_channels);
            while list.last() == Some(&0) {
                list.pop();
            }
            return (list, delay);
        }
        let (list, delay) = match saved {
            Some(s) => (s.rx_from_tx.clone(), s.delay_us),
            None => self
                .config
                .config
                .devices
                .iter()
                .find(|d| d.id == device)
                .map_or((Vec::new(), 0), |d| {
                    (d.monitor_rx_from_tx.clone(), d.monitor_delay_us)
                }),
        };
        let mut list: Vec<u16> = list
            .into_iter()
            .take(usize::from(m.rx_channels))
            .map(|t| if t > m.tx_channels { 0 } else { t })
            .collect();
        while list.last() == Some(&0) {
            list.pop();
        }
        (list, delay)
    }

    /// `DeviceStatus` monitor fields.
    pub(super) fn fill_monitor(&self, status: &mut DeviceStatus) {
        let (list, delay) = self.effective_monitor(&status.key.0);
        status.monitor_rx_from_tx = list;
        status.monitor_delay_us = delay;
    }

    fn check_card_bridge(
        &self,
        spec: &CardBridgeSpec,
        intent: &StateFile,
    ) -> Result<(), CommandError> {
        if !valid_name(&spec.name) {
            return Err(invalid(
                "name",
                format!(
                    "`{}` must be 1..={MAX_NAME_BYTES} letters, digits, `_` or `-`",
                    spec.name
                ),
            ));
        }
        if self
            .config
            .config
            .bridges
            .iter()
            .any(|b| b.name == spec.name)
        {
            return Err(locked(&format!("bridge.{}", spec.name)));
        }
        if !self.env.bridges_supported {
            return Err(CommandError::Unsupported {
                what: "sound-card bridges (this tesserad was built without `alsa-bridge`)".into(),
            });
        }
        if self.model.device(&spec.device.0).is_none() {
            return Err(not_found(format!("device `{}`", spec.device.0)));
        }
        if spec.card.trim().is_empty() || spec.card.chars().any(char::is_control) {
            return Err(invalid(
                "card",
                "name the card's ALSA PCM (`hw:CARD=<id>,DEV=0`), without control characters",
            ));
        }
        let direction = config_direction(spec.direction);
        if spec.local_only && !self.env.runtime.router {
            return Err(CommandError::Unsupported {
                what: "a card bridge for local routing only (`local_only`): this tesserad \
                       cannot run the host router yet"
                    .into(),
            });
        }
        // A local-only bridge feeds no device channel: only its card slice
        // matters, and `first_channel` is ignored.
        let available = if spec.local_only {
            spec.channels
        } else {
            self.bridge_channels(&spec.device.0, direction)
        };
        let side = match direction {
            BridgeDirection::Capture => "tx",
            BridgeDirection::Playback => "rx",
        };
        let first_channel = if spec.local_only {
            1
        } else {
            spec.first_channel
        };
        if first_channel == 0 || spec.channels == 0 {
            return Err(invalid(
                "channels",
                "channels start at 1 and a bridge carries at least one",
            ));
        }
        let first = u32::from(first_channel);
        let last = first + u32::from(spec.channels) - 1;
        if last > u32::from(available) {
            return Err(invalid(
                "channels",
                format!(
                    "channels {first}..={last} are beyond device `{}`'s {available} {side} channel(s)",
                    spec.device.0
                ),
            ));
        }
        if spec.card_first_channel == 0 {
            return Err(invalid("card_first_channel", "card channels start at 1"));
        }
        let card_first = u32::from(spec.card_first_channel);
        let card_last = card_first + u32::from(spec.channels) - 1;
        let card_limit = self
            .card_channels(&spec.card, direction)
            .map_or(u32::from(u16::MAX), u32::from);
        if card_last > card_limit {
            let what = match direction {
                BridgeDirection::Capture => "capture (input)",
                BridgeDirection::Playback => "playback (output)",
            };
            return Err(invalid(
                "card_first_channel",
                format!(
                    "card channels {card_first}..={card_last} are beyond `{}`'s {card_limit} {what} channel(s)",
                    spec.card
                ),
            ));
        }
        if spec.period_frames == Some(0) {
            return Err(invalid("period_frames", "must be at least 1"));
        }
        if !spec.enabled {
            return Ok(());
        }
        // Against the other enabled bridges as they would run.
        let configured = self.config.config.bridges.iter().cloned();
        let saved = intent
            .card_bridge
            .iter()
            .filter(|o| o.name != spec.name)
            .filter(|o| !self.config.config.bridges.iter().any(|b| b.name == o.name))
            .map(bridge_config);
        for o in configured.chain(saved).filter(|o| o.enabled) {
            if o.direction != direction {
                continue;
            }
            let (d, _, f, l) = span(&o, self.bridge_channels(&o.device, o.direction));
            let device_range = !spec.local_only && !o.local_only;
            if device_range && d == spec.device.0 && first <= l && f <= last {
                return Err(invalid(
                    "channels",
                    format!(
                        "channels {first}..={last} overlap channels {f}..={l} of bridge `{}`",
                        o.name
                    ),
                ));
            }
            if o.alsa_pcm == spec.card {
                return Err(invalid(
                    "card",
                    format!(
                        "`{}` is already used in this direction by bridge `{}`",
                        spec.card, o.name
                    ),
                ));
            }
        }
        Ok(())
    }

    fn check_monitor(
        &self,
        device: &DeviceKey,
        list: &[u16],
        delay_us: u32,
    ) -> Result<(), CommandError> {
        let Some(d) = self.model.device(&device.0) else {
            return Err(not_found(format!("device `{}`", device.0)));
        };
        if list.len() > usize::from(d.rx_channels) {
            return Err(invalid(
                "rx_from_tx",
                format!(
                    "{} entries for device `{}`'s {} rx channel(s)",
                    list.len(),
                    device.0,
                    d.rx_channels
                ),
            ));
        }
        if let Some(bad) = list.iter().find(|&&t| t > d.tx_channels) {
            return Err(invalid(
                "rx_from_tx",
                format!(
                    "device `{}` has no tx channel {bad} (it has {}); use 0 for no monitoring",
                    device.0, d.tx_channels
                ),
            ));
        }
        if delay_us > MAX_MONITOR_DELAY_US {
            return Err(invalid(
                "delay_us",
                format!("at most {MAX_MONITOR_DELAY_US} microseconds"),
            ));
        }
        Ok(())
    }

    /// `mutate` for the bridge and monitor commands; `None` for any other
    /// command.
    pub(super) fn mutate_bridge_intent(
        &self,
        intent: &mut StateFile,
        cmd: &Command,
    ) -> Option<Result<bool, CommandError>> {
        Some(match cmd {
            Command::SetCardBridge { bridge } => self.check_card_bridge(bridge, intent).map(|()| {
                match intent
                    .card_bridge
                    .iter_mut()
                    .find(|b| b.name == bridge.name)
                {
                    Some(b) if b == bridge => false,
                    Some(b) => {
                        b.clone_from(bridge);
                        true
                    }
                    None => {
                        intent.card_bridge.push(bridge.clone());
                        true
                    }
                }
            }),
            Command::RemoveCardBridge { name } => {
                if self.config.config.bridges.iter().any(|b| b.name == *name) {
                    return Some(Err(locked(&format!("bridge.{name}"))));
                }
                let before = intent.card_bridge.len();
                intent.card_bridge.retain(|b| b.name != *name);
                if intent.card_bridge.len() == before {
                    Err(not_found(format!("bridge `{name}`")))
                } else {
                    Ok(true)
                }
            }
            Command::SetMonitor {
                device,
                rx_from_tx,
                delay_us,
            } => self
                .check_monitor(device, rx_from_tx, *delay_us)
                .and_then(|()| {
                    let prev = intent.clone();
                    let next = Some(MonitorIntent {
                        rx_from_tx: rx_from_tx.clone(),
                        delay_us: *delay_us,
                    });
                    let d = intent.device.entry(device.0.clone()).or_default();
                    let mut changed = d.monitor != next;
                    d.monitor = next;
                    // With a saved router grid, monitoring is part of it: the
                    // command rewrites the device's TX-tap → host-output
                    // crosspoints (one authority), keeping the saved list too so
                    // an older daemon monitors the same channels.
                    if intent.router.saved {
                        changed |= super::router::set_monitor_taps(intent, device, rx_from_tx);
                        self.check_grid(intent, &prev)?;
                    }
                    Ok(changed)
                }),
            _ => return None,
        })
    }

    /// The devices a bridge command could change, for the access lock: the
    /// device of the new bridge and of the saved one it replaces or
    /// removes.
    pub(super) fn bridge_command_devices(&self, cmd: &Command) -> BTreeSet<String> {
        let (name, new) = match cmd {
            Command::SetCardBridge { bridge } => (&bridge.name, Some(&bridge.device)),
            Command::RemoveCardBridge { name } => (name, None),
            _ => return BTreeSet::new(),
        };
        let old = self
            .intent
            .card_bridge
            .iter()
            .find(|b| b.name == *name)
            .map(|b| &b.device);
        old.into_iter().chain(new).map(|d| d.0.clone()).collect()
    }
}

/// `RuntimeValueChanged` for saved bridges and monitoring that differ
/// between two intents.
pub(super) fn bridge_events(before: &StateFile, after: &StateFile, origin: &Origin) -> Vec<Event> {
    let mut fields = Vec::new();
    let names: BTreeSet<&String> = before
        .card_bridge
        .iter()
        .chain(&after.card_bridge)
        .map(|b| &b.name)
        .collect();
    for name in names {
        let of = |s: &StateFile| s.card_bridge.iter().find(|b| b.name == *name).cloned();
        if of(before) != of(after) {
            fields.push(format!("card_bridge.{name}"));
        }
    }
    let keys: BTreeSet<&String> = before.device.keys().chain(after.device.keys()).collect();
    for key in keys {
        let of = |s: &StateFile| s.device.get(key).and_then(|d| d.monitor.clone());
        if of(before) != of(after) {
            fields.push(format!("device.{key}.monitor"));
        }
    }
    fields
        .into_iter()
        .map(|field| Event::RuntimeValueChanged {
            field,
            origin: origin.clone(),
        })
        .collect()
}
