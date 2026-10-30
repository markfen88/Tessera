//! The effective model: configuration (defaults, bounds, locks) combined with
//! persisted intent, following the precedence rules of ADR 0008.
//!
//! [`reconcile`] is the single place where those rules live. It runs at
//! start, after every command and at reload, and is idempotent: running it on
//! its own output changes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use tessera_control_types::{ClockRole, DeviceKey, PREFERRED_LEADER_BELOW};

use crate::config::{
    ClockLock, Config, ControlLock, DeviceConfig, DeviceLock, FlowConfig, NetworkLock,
};
use crate::env::{Environment, validate_channel_label, validate_device_name, validate_name};
use crate::error::Issue;
use crate::persist::{DeviceIntent, StateFile, SubscriptionIntent};

/// The 8-byte identity a logical device presents on the network. Generated
/// once and persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WireDeviceId(pub [u8; 8]);

impl WireDeviceId {
    /// Sixteen lowercase hex digits.
    #[must_use]
    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Parse sixteen hex digits (either case).
    #[must_use]
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 16 || !s.is_ascii() {
            return None;
        }
        let mut out = [0u8; 8];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
        }
        Some(Self(out))
    }
}

impl fmt::Display for WireDeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A subscription by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionEntry {
    /// TX channel name on the remote device.
    pub tx_channel: String,
    /// Remote device name.
    pub tx_device: String,
}

/// The effective values of one logical device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceModel {
    /// Config key.
    pub key: DeviceKey,
    /// Network identity.
    pub wire_device_id: WireDeviceId,
    /// Device name.
    pub name: String,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Wire encoding in bits.
    pub encoding: u8,
    /// Latency in microseconds.
    pub latency_us: u32,
    /// RX channel count.
    pub rx_channels: u16,
    /// TX channel count.
    pub tx_channels: u16,
    /// RX labels that differ from the default `RX n`.
    pub rx_labels: BTreeMap<u16, String>,
    /// TX labels that differ from the default `TX n`.
    pub tx_labels: BTreeMap<u16, String>,
    /// Active subscriptions by local RX channel (all within `rx_channels`).
    pub subscriptions: BTreeMap<u16, SubscriptionEntry>,
    /// Values pinned by configuration.
    pub locked: BTreeSet<DeviceLock>,
    /// Allowed latency range, inclusive.
    pub latency_bounds_us: [u32; 2],
    /// Latency below the platform floor is permitted.
    pub allow_unsafe_latency: bool,
    /// Platform timing floor (ADR 0011), from the environment.
    pub latency_floor_us: u32,
}

impl DeviceModel {
    /// The label of RX channel `ch` (default `RX n`).
    #[must_use]
    pub fn rx_label(&self, ch: u16) -> String {
        self.rx_labels
            .get(&ch)
            .cloned()
            .unwrap_or_else(|| format!("RX {ch}"))
    }

    /// The label of TX channel `ch` (default `TX n`).
    #[must_use]
    pub fn tx_label(&self, ch: u16) -> String {
        self.tx_labels
            .get(&ch)
            .cloned()
            .unwrap_or_else(|| format!("TX {ch}"))
    }

    /// The latencies `SetLatency` accepts on this device now, inclusive.
    #[must_use]
    pub fn latency_range_us(&self) -> [u32; 2] {
        latency_range_us(
            self.latency_bounds_us,
            self.latency_floor_us,
            self.allow_unsafe_latency,
        )
    }
}

/// Effective clock settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockModel {
    /// Role.
    pub role: ClockRole,
    /// Leader-election priority.
    pub priority: u8,
    /// Pinned by configuration.
    pub locked: BTreeSet<ClockLock>,
}

/// Effective network settings (ADR 0012). Which interface is in effect is
/// decided by the daemon, which can see the host.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetworkModel {
    /// The runtime selection of the primary interface, if one is stored and
    /// may take effect (not locked, a valid name).
    pub selected: Option<String>,
    /// The primary is pinned by configuration.
    pub locked: bool,
}

/// All effective values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// Effective media flows from host configuration and controller intent.
    pub flows: Vec<FlowConfig>,
    /// Devices in configuration order.
    pub devices: Vec<DeviceModel>,
    /// Clock settings.
    pub clock: ClockModel,
    /// Network settings.
    pub network: NetworkModel,
}

impl Model {
    /// The device with this key.
    #[must_use]
    pub fn device(&self, key: &str) -> Option<&DeviceModel> {
        self.devices.iter().find(|d| d.key.0 == key)
    }
}

fn warn(w: &mut Vec<Issue>, path: String, msg: String) {
    w.push(Issue::warning(path, msg));
}

fn parse_channel(s: &str) -> Option<u16> {
    s.parse::<u16>().ok().filter(|n| *n >= 1)
}

/// Resolve one label map. Config labels are the base; unless locked, valid
/// persisted labels override them. Persisted labels that cannot take effect
/// (channel beyond the current count, bad channel key, invalid text) stay in
/// the persisted map untouched, with a warning ("kept but inactive"): a
/// temporary mismatch must not erase what was set at runtime. Locked maps
/// clear the persisted labels (ADR 0008: after unlocking, the old value is
/// not resurrected).
fn resolve_labels(
    cfg: &BTreeMap<u16, String>,
    state: &mut BTreeMap<String, String>,
    count: u16,
    locked: bool,
    env: &Environment,
    path: &str,
    w: &mut Vec<Issue>,
) -> BTreeMap<u16, String> {
    // Configured labels beyond the effective count (channels removed at run
    // time) are inactive until the channels come back.
    let mut out: BTreeMap<u16, String> = cfg
        .iter()
        .filter(|(k, _)| **k <= count)
        .map(|(k, v)| (*k, v.clone()))
        .collect();
    if locked {
        state.clear();
        return out;
    }
    // Normalise the keys: "03" and "+3" mean channel 3 and are rewritten to
    // "3" (with a warning), so commands that address the canonical key always
    // find the value. If several keys mean the same channel, the canonical
    // spelling wins, else the last in key order; the loss is reported.
    // Keys that are not channel numbers are left as written (inactive).
    let mut entries: Vec<(String, String)> = std::mem::take(state).into_iter().collect();
    entries.sort_by_key(|(k, _)| parse_channel(k).is_some_and(|n| n.to_string() == *k));
    for (k, v) in entries {
        let Some(n) = parse_channel(&k) else {
            state.insert(k, v);
            continue;
        };
        let canonical = n.to_string();
        if canonical != k {
            warn(
                w,
                format!("{path}.{k}"),
                format!("persisted label key `{k}` is not canonical; normalised to `{canonical}`"),
            );
        }
        if let Some(prev) = state.insert(canonical.clone(), v) {
            warn(
                w,
                format!("{path}.{canonical}"),
                format!("channel {n} is stored more than once; dropped the label `{prev}`"),
            );
        }
    }
    for (k, v) in state.iter() {
        let Some(n) = parse_channel(k) else {
            warn(
                w,
                format!("{path}.{k}"),
                "persisted label has an invalid channel number; kept but inactive".into(),
            );
            continue;
        };
        if let Err(e) = validate_name(v, env.max_name_bytes) {
            warn(
                w,
                format!("{path}.{k}"),
                format!("persisted label {e}; kept but inactive"),
            );
            continue;
        }
        if n > count {
            warn(
                w,
                format!("{path}.{k}"),
                format!(
                    "persisted label is for channel {n} but there are {count} channel(s); kept but inactive"
                ),
            );
        } else {
            if let Err(e) = validate_channel_label(v, env.max_name_bytes) {
                warn(
                    w,
                    format!("{path}.{k}"),
                    format!(
                        "persisted label {e}; retained unchanged for existing subscriptions; external controllers may refuse routing"
                    ),
                );
            }
            out.insert(n, v.clone());
        }
    }
    out
}

fn reconcile_device(
    config: &Config,
    dc: &DeviceConfig,
    ds: &mut DeviceIntent,
    other_config_names: &BTreeSet<String>,
    taken_names: &mut BTreeSet<String>,
    env: &Environment,
    w: &mut Vec<Issue>,
) -> DeviceModel {
    let p = format!("state.device.{}", dc.id);
    let locked = |l: DeviceLock| dc.locked.contains(&l);

    let wire = if let Some(id) = ds
        .wire_device_id
        .as_deref()
        .and_then(WireDeviceId::from_hex)
    {
        id
    } else {
        if ds.wire_device_id.is_some() {
            warn(
                w,
                format!("{p}.wire_device_id"),
                "invalid identity in state; generated a new one".into(),
            );
        }
        let id = WireDeviceId(env.ids.new_wire_device_id());
        ds.wire_device_id = Some(id.to_hex());
        id
    };

    // Persisted values that fail a check are *kept in the persisted intent*
    // but left out of the effective model, with a warning: the clash may be
    // temporary (a config edit, a reload) and must not destroy what was set
    // at runtime. Only locked keys are cleared.
    let mut name = dc.name.clone();
    if locked(DeviceLock::Name) {
        ds.name = None;
    } else if let Some(n) = &ds.name {
        let lower = n.to_ascii_lowercase();
        if let Err(e) = validate_name(n, env.max_name_bytes) {
            warn(
                w,
                format!("{p}.name"),
                format!("persisted name {e}; using the configured one, stored value kept"),
            );
        } else if lower != dc.name.to_ascii_lowercase()
            && (other_config_names.contains(&lower) || taken_names.contains(&lower))
        {
            warn(
                w,
                format!("{p}.name"),
                format!(
                    "persisted name `{n}` is used by another device; using the configured one, stored value kept"
                ),
            );
        } else {
            if let Err(e) = validate_device_name(n, env.max_name_bytes) {
                warn(
                    w,
                    format!("{p}.name"),
                    format!(
                        "persisted name {e}; retained unchanged for existing subscriptions; external controllers may refuse routing"
                    ),
                );
            }
            name.clone_from(n);
        }
    }
    taken_names.insert(name.to_ascii_lowercase());

    let mut sample_rate = dc.sample_rate;
    if locked(DeviceLock::SampleRate) {
        ds.sample_rate = None;
    } else if let Some(v) = ds.sample_rate {
        if env.supported_sample_rates.contains(&v) {
            sample_rate = v;
        } else {
            warn(
                w,
                format!("{p}.sample_rate"),
                format!(
                    "persisted rate {v} is not supported; using {}, stored value kept",
                    dc.sample_rate
                ),
            );
        }
    }
    let mut encoding = dc.encoding;
    if locked(DeviceLock::Encoding) {
        ds.encoding = None;
    } else if let Some(v) = ds.encoding {
        if env.supported_encodings.contains(&v) {
            encoding = v;
        } else {
            warn(
                w,
                format!("{p}.encoding"),
                format!(
                    "persisted encoding {v} is not supported; using {}, stored value kept",
                    dc.encoding
                ),
            );
        }
    }
    let mut latency_us = dc.latency_us;
    if locked(DeviceLock::Latency) {
        ds.latency_us = None;
    } else if let Some(v) = ds.latency_us {
        let [lo, hi] = dc.latency_bounds_us;
        if latency_allowed(v, dc, env) {
            latency_us = v;
        } else {
            warn(
                w,
                format!("{p}.latency_us"),
                format!(
                    "persisted latency {v} is outside the allowed range [{lo}, {hi}] or below the platform floor; using {}, stored value kept",
                    dc.latency_us
                ),
            );
        }
    }

    // Channel counts (`SetDeviceChannels`). A stored pair takes effect only
    // if it is not locked, not 0 x 0, fits every [[flow]] and [[bridge]] of
    // the device and the ring budget; otherwise it is kept but inactive.
    let mut rx_channels = dc.rx_channels;
    let mut tx_channels = dc.tx_channels;
    if locked(DeviceLock::RxChannels) {
        ds.rx_channels = None;
    }
    if locked(DeviceLock::TxChannels) {
        ds.tx_channels = None;
    }
    if ds.rx_channels.is_some() || ds.tx_channels.is_some() {
        let rx = ds.rx_channels.unwrap_or(dc.rx_channels);
        let tx = ds.tx_channels.unwrap_or(dc.tx_channels);
        match crate::impact::check_channel_counts(config, dc, sample_rate, latency_us, rx, tx) {
            Ok(_) => {
                rx_channels = rx;
                tx_channels = tx;
            }
            Err(e) => warn(
                w,
                format!("{p}.rx_channels"),
                format!(
                    "persisted channel counts {rx} RX / {tx} TX cannot be used: {e}; using {} RX / {} TX, stored value kept",
                    dc.rx_channels, dc.tx_channels
                ),
            ),
        }
    }

    let rx_labels = resolve_labels(
        &dc.rx_channel_labels,
        &mut ds.rx_channel_labels,
        rx_channels,
        locked(DeviceLock::RxChannelLabels),
        env,
        &format!("{p}.rx_channel_labels"),
        w,
    );
    let tx_labels = resolve_labels(
        &dc.tx_channel_labels,
        &mut ds.tx_channel_labels,
        tx_channels,
        locked(DeviceLock::TxChannelLabels),
        env,
        &format!("{p}.tx_channel_labels"),
        w,
    );

    // Subscriptions are kept by name and never silently dropped. Entries that
    // cannot be active (beyond the channel count, or with unusable names) stay
    // in the file with a warning. The one thing that is dropped is a repeated
    // entry for the same RX channel (the last one wins), and that is reported.
    let mut by_ch: BTreeMap<u16, SubscriptionIntent> = BTreeMap::new();
    let mut inactive: Vec<SubscriptionIntent> = Vec::new();
    for s in std::mem::take(&mut ds.subscription) {
        let usable = s.rx_channel >= 1
            && validate_name(&s.tx_channel, env.max_name_bytes).is_ok()
            && validate_name(&s.tx_device, env.max_name_bytes).is_ok();
        if !usable {
            warn(
                w,
                format!("{p}.subscription"),
                format!(
                    "stored subscription on rx channel {} has an unusable channel number or name; kept but inactive",
                    s.rx_channel
                ),
            );
            inactive.push(s);
            continue;
        }
        if let Some(prev) = by_ch.insert(s.rx_channel, s) {
            warn(
                w,
                format!("{p}.subscription"),
                format!(
                    "rx channel {} is stored more than once; dropped the earlier entry ({} @ {})",
                    prev.rx_channel, prev.tx_channel, prev.tx_device
                ),
            );
        }
    }
    let mut subscriptions = BTreeMap::new();
    let mut kept: Vec<SubscriptionIntent> = Vec::new();
    for (ch, s) in by_ch {
        if ch > rx_channels {
            warn(
                w,
                format!("{p}.subscription"),
                format!(
                    "subscription on rx channel {ch} but the device has {rx_channels} rx channel(s); kept but inactive"
                ),
            );
        } else {
            subscriptions.insert(
                ch,
                SubscriptionEntry {
                    tx_channel: s.tx_channel.clone(),
                    tx_device: s.tx_device.clone(),
                },
            );
        }
        kept.push(s);
    }
    kept.extend(inactive);
    kept.sort_by_key(|s| s.rx_channel);
    ds.subscription = kept;

    DeviceModel {
        key: DeviceKey(dc.id.clone()),
        wire_device_id: wire,
        name,
        sample_rate,
        encoding,
        latency_us,
        rx_channels,
        tx_channels,
        rx_labels,
        tx_labels,
        subscriptions,
        locked: dc.locked.clone(),
        latency_bounds_us: dc.latency_bounds_us,
        allow_unsafe_latency: dc.allow_unsafe_latency,
        latency_floor_us: env.latency_floor_us,
    }
}

/// The latencies `SetLatency` accepts, inclusive: `latency_bounds_us`,
/// raised to the platform floor unless `allow_unsafe_latency`
/// (`device-settings.md` §4.4 item 2: one rule for the write check and the
/// range a controller is shown).
#[must_use]
pub fn latency_range_us(bounds: [u32; 2], floor: u32, allow_unsafe: bool) -> [u32; 2] {
    let [lo, hi] = bounds;
    [if allow_unsafe { lo } else { lo.max(floor) }, hi]
}

/// Is `v` an acceptable latency for this device on this platform?
pub(crate) fn latency_allowed(v: u32, dc: &DeviceConfig, env: &Environment) -> bool {
    let [lo, hi] = latency_range_us(
        dc.latency_bounds_us,
        env.latency_floor_us,
        dc.allow_unsafe_latency,
    );
    (lo..=hi).contains(&v)
}

/// Apply the ADR 0008 precedence: for each value, the persisted one if it is
/// present, valid, within the configured bounds and not locked; otherwise the
/// configured one (with a warning if a persisted value had to be ignored).
///
/// `intent` is cleaned in place: invalid entries are removed, locked keys are
/// cleared, missing identities are generated. Returns the model, the
/// warnings, and whether `intent` changed (so the caller should persist).
pub fn reconcile(
    config: &Config,
    intent: &mut StateFile,
    env: &Environment,
) -> (Model, Vec<Issue>, bool) {
    let before = intent.clone();
    let mut warnings = Vec::new();
    let mut combined = config.clone();
    if config.control.locked.contains(&ControlLock::Flows) {
        // Keep an explicit empty marker so removing the lock never resurrects
        // either stale controller intent or the deprecated file.
        intent.protocol_flows = Some(Vec::new());
    } else if let Some(specs) = &intent.protocol_flows {
        if !config.legacy_protocol_flows.is_empty() {
            warnings.push(Issue::warning(
                "flow",
                "90-probe-flows.toml is deprecated and ignored because controller flows are stored in state.toml",
            ));
        }
        match crate::flows::from_specs(specs, env) {
            Ok(flows) => combined.flows.extend(flows),
            Err(issues) => warnings.extend(issues.into_iter().map(|i| {
                Issue::warning(
                    "state.protocol_flows",
                    format!("{i}; controller set kept but inactive"),
                )
            })),
        }
    } else {
        combined
            .flows
            .extend(config.legacy_protocol_flows.iter().cloned());
    }

    let config_names: Vec<String> = config
        .devices
        .iter()
        .map(|d| d.name.to_ascii_lowercase())
        .collect();
    let mut taken = BTreeSet::new();
    let mut devices = Vec::with_capacity(config.devices.len());
    for (i, dc) in config.devices.iter().enumerate() {
        let others: BTreeSet<String> = config_names
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, n)| n.clone())
            .collect();
        let ds = intent.device.entry(dc.id.clone()).or_default();
        devices.push(reconcile_device(
            &combined,
            dc,
            ds,
            &others,
            &mut taken,
            env,
            &mut warnings,
        ));
    }
    for key in intent.device.keys() {
        if config.device(key).is_none() {
            warnings.push(Issue::warning(
                format!("state.device.{key}"),
                "state for a device that is not in the configuration; kept".to_owned(),
            ));
        }
    }

    // Clock.
    let mut role = config.clock.role;
    if config.clock.locked.contains(&ClockLock::Role) {
        intent.clock.role = None;
    } else if let Some(r) = intent.clock.role {
        role = r;
    }
    let mut priority = config.clock.priority;
    if config.clock.locked.contains(&ClockLock::Priority) {
        intent.clock.priority = None;
    } else if let Some(p) = intent.clock.priority {
        priority = p;
    }
    // Preferred leader and follower-only exclude each other, also when the
    // combination comes from configuration and stored state together
    // (device-settings.md §2.3). Follower-only wins: a stored preferred
    // priority is dropped, a configured one is not used.
    if role == ClockRole::Follower && priority < PREFERRED_LEADER_BELOW {
        if intent.clock.priority.take().is_some() {
            warn(
                &mut warnings,
                "state.clock.priority".to_owned(),
                format!(
                    "{priority} makes a preferred leader, which a follower-only device \
                     cannot be; ignored"
                ),
            );
            priority = config.clock.priority;
        }
        if priority < PREFERRED_LEADER_BELOW {
            warn(
                &mut warnings,
                "clock.priority".to_owned(),
                format!(
                    "{priority} makes a preferred leader, which a follower-only device \
                     cannot be; the default priority is used"
                ),
            );
            priority = crate::config::ClockConfig::default().priority;
        }
    }

    // Network (ADR 0012). A stored selection that is not a valid interface
    // name is kept but inactive; whether the interface exists is the
    // daemon's business (a missing one falls back, it is not an error).
    let net_locked = config.network.locked.contains(&NetworkLock::Primary);
    if net_locked {
        intent.network.primary = None;
    }
    let selected = match &intent.network.primary {
        Some(p) => match crate::validate::valid_interface_name(p) {
            Ok(()) if config.network.secondary.as_deref() == Some(p.as_str()) => {
                warn(
                    &mut warnings,
                    "state.network.primary".to_owned(),
                    format!("`{p}` is the secondary interface; kept but inactive"),
                );
                None
            }
            Ok(()) => Some(p.clone()),
            Err(e) => {
                warn(
                    &mut warnings,
                    "state.network.primary".to_owned(),
                    format!("{e}; kept but inactive"),
                );
                None
            }
        },
        None => None,
    };

    // Runtime geometry may differ from configuration defaults. Validate the
    // entire controller set against those effective counts before exposing it.
    combined = crate::flows::effective_validation_config(&combined, &devices);
    let problems: Vec<Issue> = crate::validate::validate(&combined, env)
        .into_iter()
        .filter(|i| i.is_error() && i.path.starts_with("flow["))
        .collect();
    if !problems.is_empty() {
        warnings.extend(problems.into_iter().map(|i| {
            Issue::warning(
                "state.protocol_flows",
                format!("{i}; controller set kept but inactive"),
            )
        }));
        combined.flows.clone_from(&config.flows);
    }
    intent.schema_version = crate::persist::CURRENT_SCHEMA_VERSION;
    let changed = *intent != before;
    (
        Model {
            flows: combined.flows,
            devices,
            clock: ClockModel {
                role,
                priority,
                locked: config.clock.locked.clone(),
            },
            network: NetworkModel {
                selected,
                locked: net_locked,
            },
        },
        warnings,
        changed,
    )
}
