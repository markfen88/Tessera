//! What a command restarts and drops ([`Impact`]), the declared restart
//! scope of every command, and the checks behind `SetDeviceChannels`
//! (channel conflicts with `[[flow]]`/`[[bridge]]` entries, ring memory).
//!
//! The impact is computed from the state before and after the change, not
//! from the command's kind, so a command that changes nothing has scope
//! `None` and never needs confirmation.

use std::collections::BTreeSet;

use tessera_control_types::{
    Channel, Command, DeviceKey, Direction, Impact, RemovedSubscription, RestartImpact,
    RestartScope, RingMemory,
};

use crate::config::{AudioConfig, BridgeConfig, Config, DeviceConfig};
use crate::model::Model;
use crate::persist::StateFile;
use crate::schema::{ApplyClass, KEYS, section};

/// Smallest ring capacity in frames (`tessera-audio-ipc`
/// `MIN_CAPACITY_FRAMES`; a test in `tesserad` keeps them equal).
pub const RING_MIN_CAPACITY_FRAMES: u64 = 64;
/// Ring header bytes (`tessera-audio-ipc` `RING_HEADER_LEN`; same test).
pub const RING_HEADER_BYTES: u64 = 512;
/// Per-channel state bytes before the bitmap (`tessera-audio-ipc`
/// `CHANNEL_STATE_LEN`; same test).
pub const RING_CHANNEL_STATE_BYTES: u64 = 64;
/// Bytes per sample in a ring plane (`tessera-audio-ipc`: 32-bit samples;
/// same test).
pub const RING_SAMPLE_BYTES: u64 = 4;

/// Estimated bytes of one audio client ring spanning `channels` channels,
/// following the layout of `tessera-audio-ipc` (capacity: the next power of
/// two of latency + reorder window + client period in frames, at least 64;
/// per channel 64 bytes of state, a bitmap of capacity / 8 bytes rounded up
/// to 64, and 4 bytes per frame; plus the header). The client period is
/// `[audio] default_period_frames`. 0 channels: 0 bytes. All arithmetic is
/// checked in `u64`; a size that does not fit is `u64::MAX`, which is over
/// any budget.
#[must_use]
pub fn ring_bytes(channels: u16, rate: u32, latency_max_us: u32, audio: &AudioConfig) -> u64 {
    if channels == 0 {
        return 0;
    }
    ring_bytes_checked(channels, rate, latency_max_us, audio).unwrap_or(u64::MAX)
}

fn ring_bytes_checked(
    channels: u16,
    rate: u32,
    latency_max_us: u32,
    audio: &AudioConfig,
) -> Option<u64> {
    let frames = |us: u32| {
        u64::from(us)
            .checked_mul(u64::from(rate))
            .map(|v| v / 1_000_000)
    };
    let need = frames(latency_max_us)?
        .checked_add(frames(audio.reorder_window_us)?)?
        .checked_add(u64::from(audio.default_period_frames))?
        .max(RING_MIN_CAPACITY_FRAMES);
    let cap = need.checked_next_power_of_two()?;
    let bitmap = (cap / 8).checked_add(63)? & !63;
    let stride = RING_CHANNEL_STATE_BYTES
        .checked_add(bitmap)?
        .checked_add(cap.checked_mul(RING_SAMPLE_BYTES)?)?;
    stride
        .checked_mul(u64::from(channels))?
        .checked_add(RING_HEADER_BYTES)
}

/// Ring bytes of one full-width client in each direction of a device
/// (`u64::MAX` if it does not fit).
#[must_use]
pub fn device_ring_bytes(
    rx: u16,
    tx: u16,
    rate: u32,
    latency_max_us: u32,
    audio: &AudioConfig,
) -> u64 {
    ring_bytes(rx, rate, latency_max_us, audio).saturating_add(ring_bytes(
        tx,
        rate,
        latency_max_us,
        audio,
    ))
}

/// The ring budget, bytes.
#[must_use]
pub fn ring_budget_bytes(audio: &AudioConfig) -> u64 {
    audio.ring_budget_mib.saturating_mul(1 << 20)
}

fn latency_max(dc: &DeviceConfig, latency_us: u32) -> u32 {
    dc.latency_bounds_us[1].max(latency_us)
}

/// One reason a channel count cannot be used, with the key path it is
/// about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelProblem {
    /// Key path (`device.main.rx_channels`, `flow[0].channels`,
    /// `audio.ring_budget_mib`).
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for ChannelProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Every reason device `dc` cannot run with `rx` / `tx` channels at this
/// rate and latency, all together: 0 channels in both directions, a
/// direction going from some channels to 0 at run time (`running`: the
/// device's current counts), `[[flow]]`/`[[bridge]]` entries that use a
/// channel beyond the counts, and the ring budget (one full-width client
/// ring per direction of this device). Also returns the ring estimate.
/// Key paths are in the loader's form (`device[0].rx_channels`).
///
/// The only other ceiling is the width of the count (`u16`, the wire
/// format: `spec/protocol/settings.md`, "no hard limit in the wire format
/// beyond u16", `likely`; open question OQ-CFG-1).
///
/// `running`: the counts the device runs with now, for a change at run
/// time. A device with no RX or no TX channels can be configured in the
/// files (a playback-only box with `tx_channels = 0`, say) and keeps
/// that direction at 0 while the other changes, but a running direction
/// cannot be taken to 0 yet: `tesserad` has no test that restarts a running
/// device into that shape (CFG-ZERO in PROGRESS.md).
#[must_use]
pub fn channel_problems(
    config: &Config,
    dc: &DeviceConfig,
    rate: u32,
    latency_us: u32,
    rx: u16,
    tx: u16,
    running: Option<(u16, u16)>,
) -> (Vec<ChannelProblem>, u64) {
    let mut out = Vec::new();
    let index = config
        .devices
        .iter()
        .position(|d| d.id == dc.id)
        .unwrap_or(0);
    let dev = |k: &str| format!("device[{index}].{k}");
    if rx == 0 && tx == 0 {
        out.push(ChannelProblem {
            path: dev("rx_channels"),
            message: "a device needs at least one channel: rx and tx cannot both be 0".into(),
        });
    } else if let Some((now_rx, now_tx)) = running {
        for (k, n, now, d) in [
            ("rx_channels", rx, now_rx, "RX"),
            ("tx_channels", tx, now_tx, "TX"),
        ] {
            if n == 0 && now != 0 {
                out.push(ChannelProblem {
                    path: dev(k),
                    message: zero_message(&dc.id, now, d),
                });
            }
        }
    }
    let count = |d: Direction| match d {
        Direction::Rx => rx,
        Direction::Tx => tx,
    };
    let name = |d: Direction| match d {
        Direction::Rx => "rx",
        Direction::Tx => "tx",
    };
    for (i, f) in config
        .flows
        .iter()
        .enumerate()
        .filter(|(_, f)| f.device == dc.id)
    {
        let n = count(f.direction);
        let path = format!("flow[{i}].channels");
        if let Some(c) = f.channels.iter().find(|c| **c > n) {
            out.push(ChannelProblem {
                path,
                message: format!(
                    "[[flow]] `{}` carries {} channel {c}; change or remove it first",
                    f.id,
                    name(f.direction)
                ),
            });
        } else if f.channels.is_empty() && n == 0 {
            out.push(ChannelProblem {
                path,
                message: format!(
                    "[[flow]] `{}` carries every {} channel and there would be none",
                    f.id,
                    name(f.direction)
                ),
            });
        }
    }
    for (i, b) in config
        .bridges
        .iter()
        .enumerate()
        .filter(|(_, b)| b.device == dc.id)
    {
        let d = b.direction.device_direction();
        let last = match b.channels {
            Some(c) => u32::from(b.first_channel) + u32::from(c).max(1) - 1,
            None => u32::from(b.first_channel),
        };
        if last > u32::from(count(d)) {
            out.push(ChannelProblem {
                path: format!("bridge[{i}].channels"),
                message: format!(
                    "[[bridge]] `{}` uses {} channel {last}; change or remove it first",
                    b.name,
                    name(d)
                ),
            });
        }
    }
    let bytes = device_ring_bytes(rx, tx, rate, latency_max(dc, latency_us), &config.audio);
    let budget = ring_budget_bytes(&config.audio);
    if bytes > budget {
        out.push(ChannelProblem {
            path: "audio.ring_budget_mib".into(),
            message: format!(
                "one audio client ring per direction of `{}` would need {} bytes, more than the ring budget of {budget} bytes ({} MiB)",
                dc.id,
                if bytes == u64::MAX { "more than 2^64".to_owned() } else { bytes.to_string() },
                config.audio.ring_budget_mib
            ),
        });
    }
    (out, bytes)
}

/// Why a running device cannot go from `now` to 0 channels in direction
/// `d` (`"RX"`/`"TX"`).
#[must_use]
pub fn zero_message(device: &str, now: u16, d: &str) -> String {
    format!(
        "device `{device}` would go from {now} to 0 {d} channels while it runs, which is not supported yet: set 0 {d} channels in the config file and restart the daemon instead (CFG-ZERO)"
    )
}

/// [`channel_problems`] for a stored pair (not a run-time change), joined.
///
/// # Errors
/// Every problem, joined with `; `.
pub fn check_channel_counts(
    config: &Config,
    dc: &DeviceConfig,
    rate: u32,
    latency_us: u32,
    rx: u16,
    tx: u16,
) -> Result<u64, String> {
    let (problems, bytes) = channel_problems(config, dc, rate, latency_us, rx, tx, None);
    if problems.is_empty() {
        Ok(bytes)
    } else {
        Err(problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "))
    }
}

/// The restart scope a command declares when it changes something
/// (documentation and the scope table test; the engine gates on the
/// computed [`Impact`], so a no-op is always `None`):
///
/// | command | scope | why |
/// |---|---|---|
/// | `SetDeviceName`, `SetChannelLabel` | none | re-announced in place; audio does not stop |
/// | `Subscribe`, `Unsubscribe` | none | only that RX channel changes |
/// | `SetClockRole`, `SetClockPriority` | none | sent to the running PTP port as live commands; nothing restarts |
/// | `SetSampleRate`, `SetEncoding`, `SetLatency` | device | the device's flows are rebuilt and its audio clients detached (new ring geometry) |
/// | `SetDeviceChannels` | device | as above, plus channels and their subscriptions go away |
/// | `SetPrimaryInterface` | device (every device) | the PTP port restarts on the new interface and every flow moves |
/// | `ReloadConfig` | device (at most) | computed from what the files changed: device for geometry, interfaces, `[[flow]]`/`[[bridge]]` entries, MTU, Wi-Fi permission or a removed device; changed keys of apply class `restart` are listed in `Impact::restart_keys` but restart nothing now |
#[must_use]
pub fn declared_scope(cmd: &Command) -> RestartScope {
    match cmd {
        Command::SetSampleRate { .. }
        | Command::SetEncoding { .. }
        | Command::SetLatency { .. }
        | Command::SetDeviceChannels { .. }
        | Command::SetPrimaryInterface { .. }
        | Command::ReloadConfig => RestartScope::Device,
        Command::SetDeviceName { .. }
        | Command::SetChannelLabel { .. }
        | Command::Subscribe { .. }
        | Command::Unsubscribe { .. }
        | Command::SetClockRole { .. }
        | Command::SetClockPriority { .. } => RestartScope::None,
        // A command added to control-types later: assume the worst until
        // this table names it.
        _ => RestartScope::Daemon,
    }
}

const DOWNTIME_DEVICE: &str = "usually a few seconds of silence on the restarting device's channels; receivers on other devices resume once they have re-subscribed";
const DOWNTIME_INTERFACE: &str = "a few seconds, longer (up to tens of seconds) if the clock has to re-lock on the new interface";
const AFTER_DEVICE: &str = "Audio applications that do not re-attach on their own must be restarted. Check the device with `tesseractl status`.";
const AFTER_CHANNELS: &str = "Subscriptions on removed channels are deleted: recreate them on the remaining channels if they are needed. Controllers see the new channel counts once the device is re-announced.";
const AFTER_RESTART_KEYS: &str = "Some changes take effect at the next service restart: restart the daemon (`systemctl restart tesserad`) when an interruption of every device is acceptable.";

/// Collects an impact device by device.
#[derive(Default)]
struct Builder {
    devices: Vec<DeviceKey>,
    reasons: Vec<String>,
    drops: Vec<String>,
    flows: Vec<String>,
    detach: bool,
    counts_changed: bool,
    interface: bool,
    ring: Option<RingMemory>,
    removed: Vec<RemovedSubscription>,
    restart_keys: Vec<String>,
    /// Extra token input: the command's arguments, changed entries.
    context: Vec<String>,
}

impl Builder {
    fn device(&mut self, key: &DeviceKey) {
        if !self.devices.contains(key) {
            self.devices.push(key.clone());
        }
    }

    fn flow(&mut self, id: &str) {
        if !self.flows.iter().any(|f| f == id) {
            self.flows.push(id.to_owned());
        }
    }

    /// The impact. The scope says what restarts now: `Device` if any device
    /// does, else `None`. Restart-class keys are carried as information
    /// (they take effect at the next service restart) and deleted
    /// subscriptions are listed even when nothing restarts.
    fn build(self, kind: &str) -> Impact {
        let context: Vec<&str> = self.context.iter().map(String::as_str).collect();
        let mut after = Vec::new();
        if !self.restart_keys.is_empty() {
            after.push(AFTER_RESTART_KEYS);
        }
        if self.devices.is_empty() {
            let mut restart = RestartImpact::none();
            if !self.restart_keys.is_empty() {
                restart = restart.with_afterwards(after.join(" "));
            }
            let mut impact = Impact::new(kind, restart);
            impact.subscriptions_removed = self.removed;
            impact.restart_keys = self.restart_keys;
            return impact.sealed_with(&context);
        }
        let hint = if self.interface {
            DOWNTIME_INTERFACE
        } else {
            DOWNTIME_DEVICE
        };
        let mut lead = vec![AFTER_DEVICE];
        if self.counts_changed {
            lead.push(AFTER_CHANNELS);
        }
        lead.extend(after);
        let restart = RestartImpact::new(RestartScope::Device, self.reasons.join("; "))
            .with_devices(self.devices)
            .with_what_drops(self.drops)
            .with_downtime_hint(hint)
            .with_afterwards(lead.join(" "));
        let mut impact = Impact::new(kind, restart);
        impact.flows_restarted = self.flows;
        impact.subscriptions_removed = self.removed;
        impact.audio_clients_detached = self.detach;
        impact.ring_memory = self.ring;
        impact.restart_keys = self.restart_keys;
        impact.sealed_with(&context)
    }
}

/// The geometry part: devices whose rate, encoding, latency or counts
/// differ, the primary interface selection, and the stored subscriptions
/// that disappear with removed channels.
fn geometry(
    b: &mut Builder,
    old: &Model,
    new: &Model,
    old_intent: &StateFile,
    new_intent: &StateFile,
    config: &Config,
) {
    let iface = old.network.selected != new.network.selected;
    for n in &new.devices {
        let Some(o) = old.device(&n.key.0) else {
            continue;
        };
        let key = &n.key.0;
        let mut why = Vec::new();
        if o.sample_rate != n.sample_rate {
            why.push(format!(
                "the sample rate changes from {} to {} Hz",
                o.sample_rate, n.sample_rate
            ));
        }
        if o.encoding != n.encoding {
            why.push(format!(
                "the encoding changes from {} to {} bits",
                o.encoding, n.encoding
            ));
        }
        if o.latency_us != n.latency_us {
            why.push(format!(
                "the latency changes from {} to {} us",
                o.latency_us, n.latency_us
            ));
        }
        let counts = (o.rx_channels, o.tx_channels) != (n.rx_channels, n.tx_channels);
        if counts {
            why.push(format!(
                "the channel counts change from {} RX / {} TX to {} RX / {} TX",
                o.rx_channels, o.tx_channels, n.rx_channels, n.tx_channels
            ));
        }
        if iface {
            b.device(&n.key);
            for f in config.flows.iter() {
                b.flow(&f.id);
            }
        }
        if why.is_empty() {
            continue;
        }
        b.device(&n.key);
        b.detach = true;
        b.reasons
            .push(format!("device `{key}`: {}", why.join(", ")));
        b.drops.push(format!(
            "audio on every channel of `{key}` while its flows restart"
        ));
        b.drops.push(format!(
            "audio clients attached to `{key}` are detached and must re-attach"
        ));
        for f in config.flows.iter().filter(|f| f.device == *key) {
            b.flow(&f.id);
        }
        for br in config
            .bridges
            .iter()
            .filter(|br| br.device == *key && br.enabled)
        {
            b.drops
                .push(format!("bridge `{}` closes and reopens its card", br.name));
        }
        if counts {
            b.counts_changed = true;
            if n.rx_channels < o.rx_channels {
                b.drops.push(format!(
                    "RX channels {}..={} of `{key}` are removed",
                    n.rx_channels + 1,
                    o.rx_channels
                ));
            }
            if n.tx_channels < o.tx_channels {
                b.drops.push(format!(
                    "TX channels {}..={} of `{key}` are removed; receivers subscribed to them lose them",
                    n.tx_channels + 1,
                    o.tx_channels
                ));
            }
            if let Some(dc) = config.device(key) {
                let lat = |l: u32| latency_max(dc, l);
                let before = device_ring_bytes(
                    o.rx_channels,
                    o.tx_channels,
                    o.sample_rate,
                    lat(o.latency_us),
                    &config.audio,
                );
                let after = device_ring_bytes(
                    n.rx_channels,
                    n.tx_channels,
                    n.sample_rate,
                    lat(n.latency_us),
                    &config.audio,
                );
                let budget = ring_budget_bytes(&config.audio);
                b.ring = Some(match b.ring {
                    None => RingMemory::new(before, after, budget),
                    Some(r) => RingMemory::new(
                        r.before_bytes.saturating_add(before),
                        r.after_bytes.saturating_add(after),
                        budget,
                    ),
                });
            }
        }
    }
    // Deleted subscriptions: compare what is *stored*, so a subscription
    // kept inactive by an earlier change is listed too.
    for (key, od) in &old_intent.device {
        let kept = new_intent.device.get(key).map(|d| &d.subscription);
        for s in &od.subscription {
            if kept.is_some_and(|k| k.contains(s)) {
                continue;
            }
            if let Some(c) = Channel::new(s.rx_channel) {
                b.removed.push(RemovedSubscription::new(
                    DeviceKey(key.clone()),
                    c,
                    s.tx_channel.as_str(),
                    s.tx_device.as_str(),
                ));
            }
        }
    }
    if iface {
        b.interface = true;
        let show = |s: &Option<String>| {
            s.as_ref().map_or_else(
                || "the configured or automatic choice".to_owned(),
                |n| format!("`{n}`"),
            )
        };
        b.reasons.push(format!(
            "the primary interface selection changes from {} to {}",
            show(&old.network.selected),
            show(&new.network.selected)
        ));
        b.drops.push("the PTP clock restarts on the new interface and re-locks; audio of every device stops until it has".into());
        b.drops.push("every flow moves to the new interface".into());
    }
}

/// What going from `old` to `new` restarts and drops for `cmd` (the
/// configuration is the same before and after). The token covers the
/// command's arguments.
#[must_use]
pub fn impact_of(
    cmd: &Command,
    old: &Model,
    new: &Model,
    old_intent: &StateFile,
    new_intent: &StateFile,
    config: &Config,
) -> Impact {
    let mut b = Builder::default();
    geometry(&mut b, old, new, old_intent, new_intent, config);
    b.context.push(arguments(cmd));
    b.build(cmd.kind())
}

/// The impact of a command that changes nothing (scope `None`), with the
/// command's arguments in the token.
#[must_use]
pub fn no_impact(cmd: &Command) -> Impact {
    Impact::new(cmd.kind(), RestartImpact::none()).sealed_with(&[&arguments(cmd)])
}

/// The command's arguments as token input, without `dry_run`: the token of
/// a dry run must confirm the real command.
fn arguments(cmd: &Command) -> String {
    match cmd {
        Command::SetDeviceChannels { device, rx, tx, .. } => {
            format!("set_device_channels {} {rx} {tx}", device.0)
        }
        other => format!("{other:?}"),
    }
}

/// What a reload from `(old_cfg, old)` to `(new_cfg, new)` restarts:
/// the geometry changes, plus devices whose `[[flow]]` or `[[bridge]]`
/// entries change, all devices with flows for a new MTU, all devices for a
/// new Wi-Fi permission or interface list, removed devices. Changed keys of apply
/// class `restart` are listed in `restart_keys` and do not raise the scope.
///
/// The token covers the contents of every changed `[[flow]]` and
/// `[[bridge]]` entry.
///
/// # Errors
/// From [`changed_restart_keys`].
pub fn reload_impact(
    old_cfg: &Config,
    new_cfg: &Config,
    old: &Model,
    new: &Model,
    old_intent: &StateFile,
    new_intent: &StateFile,
) -> Result<Impact, String> {
    let mut b = Builder::default();
    geometry(&mut b, old, new, old_intent, new_intent, new_cfg);
    let all: Vec<DeviceKey> = new.devices.iter().map(|d| d.key.clone()).collect();
    // [[flow]] entries by id.
    let ids: BTreeSet<&str> = old_cfg
        .flows
        .iter()
        .chain(&new_cfg.flows)
        .map(|f| f.id.as_str())
        .collect();
    for id in ids {
        let o = old_cfg.flows.iter().find(|f| f.id == id);
        let n = new_cfg.flows.iter().find(|f| f.id == id);
        if o == n {
            continue;
        }
        let what = match (o, n) {
            (None, _) => "is added",
            (_, None) => "is removed",
            _ => "changes",
        };
        for f in o.iter().chain(n.iter()) {
            if new.device(&f.device).is_some() {
                b.device(&DeviceKey(f.device.clone()));
            }
        }
        b.flow(id);
        b.context.push(format!("flow {id}: {o:?} -> {n:?}"));
        b.reasons.push(format!("[[flow]] `{id}` {what}"));
        b.drops
            .push(format!("audio of flow `{id}` while it restarts"));
    }
    let names: BTreeSet<&str> = old_cfg
        .bridges
        .iter()
        .chain(&new_cfg.bridges)
        .map(|x| x.name.as_str())
        .collect();
    // The error waits (`error_backoff*`, `error_sleep*`) apply at the
    // bridge's next error and reopen nothing: a change of only those is no
    // restart.
    let restarting = |x: &BridgeConfig| BridgeConfig {
        error_backoff: None,
        error_backoff_min_ms: None,
        error_backoff_max_ms: None,
        error_sleep: None,
        error_sleep_min_ms: None,
        error_sleep_max_ms: None,
        ..x.clone()
    };
    for name in names {
        let o = old_cfg.bridges.iter().find(|x| x.name == name);
        let n = new_cfg.bridges.iter().find(|x| x.name == name);
        if o.map(restarting) == n.map(restarting) {
            continue;
        }
        for x in o.iter().chain(n.iter()) {
            if new.device(&x.device).is_some() {
                b.device(&DeviceKey(x.device.clone()));
            }
        }
        b.context.push(format!("bridge {name}: {o:?} -> {n:?}"));
        b.reasons.push(format!("[[bridge]] `{name}` changes"));
        b.drops
            .push(format!("bridge `{name}` closes and reopens its card"));
    }
    let net_change = |what: &str, b: &mut Builder, devices: &[DeviceKey]| {
        for d in devices {
            b.device(d);
        }
        b.reasons.push(what.to_owned());
    };
    if old_cfg.network.mtu_payload != new_cfg.network.mtu_payload {
        let with_flows: Vec<DeviceKey> = all
            .iter()
            .filter(|d| new_cfg.flows.iter().any(|f| f.device == d.0))
            .cloned()
            .collect();
        for f in &new_cfg.flows {
            b.flow(&f.id);
        }
        net_change(
            &format!(
                "network.mtu_payload changes from {} to {}: every flow restarts",
                old_cfg.network.mtu_payload, new_cfg.network.mtu_payload
            ),
            &mut b,
            &with_flows,
        );
    }
    if old_cfg.network.allow_wireless != new_cfg.network.allow_wireless {
        b.interface = true;
        net_change(
            "network.allow_wireless changes: the interface choice is made again",
            &mut b,
            &all,
        );
    }
    if old_cfg.network.interfaces != new_cfg.network.interfaces
        || old_cfg.network.secondary != new_cfg.network.secondary
    {
        b.interface = true;
        for f in &new_cfg.flows {
            b.flow(&f.id);
        }
        net_change(
            "the configured interfaces change: PTP and every flow restart",
            &mut b,
            &all,
        );
    }
    for o in &old.devices {
        if new.device(&o.key.0).is_none() {
            b.device(&o.key);
            b.reasons.push(format!(
                "device `{}` is removed from the configuration",
                o.key.0
            ));
            b.drops.push(format!(
                "`{}` disappears from the network; its stored values are kept",
                o.key.0
            ));
        }
    }
    b.restart_keys = changed_restart_keys(old_cfg, new_cfg)?;
    Ok(b.build(Command::ReloadConfig.kind()))
}

/// Keys of apply class `restart` whose value differs between two
/// configurations, as schema paths. Every such key is in a plain table
/// (a test checks that no `[[...]]` key is `restart`).
///
/// # Errors
/// A configuration that cannot be compared (it does not serialise): never
/// silently "nothing changed".
pub fn changed_restart_keys(a: &Config, b: &Config) -> Result<Vec<String>, String> {
    let ser = |c: &Config| match toml::Value::try_from(c) {
        Ok(toml::Value::Table(t)) => Ok(t),
        Ok(_) => Err("the configuration does not serialise to a table".to_owned()),
        Err(e) => Err(format!("cannot compare configurations: {e}")),
    };
    let (ta, tb) = (ser(a)?, ser(b)?);
    let get = |t: &toml::Table, s: &str, k: &str| -> Option<toml::Value> {
        let mut v: Option<&toml::Value> = None;
        for part in s.split('.').chain(k.split('.')) {
            v = match v {
                None => t.get(part),
                Some(x) => x.get(part),
            };
            v?;
        }
        v.cloned()
    };
    Ok(KEYS
        .iter()
        .filter(|k| k.apply == ApplyClass::Restart)
        .filter(|k| section(k.section).is_some_and(|s| !s.array))
        .filter(|k| get(&ta, k.section, k.key) != get(&tb, k.section, k.key))
        .map(crate::schema::KeySpec::path)
        .collect())
}
