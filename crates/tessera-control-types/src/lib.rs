//! Public command, query, event and status types for tessera.
//!
//! This crate is the contract between every control surface (the Dante
//! control protocol handlers, `tesseractl`, config reload, a future HTTP API)
//! and the daemon's state model (ADR 0008). Changes must be additive:
//! enums and structs are `#[non_exhaustive]`, and removing or renaming
//! anything is a semver-breaking change.
//!
//! `no_std` + `alloc`; serde support behind the `serde` feature.
//!
//! # Semver notes
//!
//! * Every public enum is `#[non_exhaustive]`: matching on one outside this
//!   crate needs a wildcard arm, so new variants are additive. The fields
//!   of an existing variant are *not* covered by that: adding a field to a
//!   variant is a breaking change, so new options arrive as new variants
//!   (for example [`RequestBody::ConfirmedCommand`] instead of a `confirmed`
//!   field on every command).
//! * Status and metric structs are `#[non_exhaustive]` with constructors and
//!   `with_*` setters; new fields are `#[serde(default)]` so older JSON
//!   still parses (for example [`Status::limits`]).
//! * [`Request`], [`Response`] and the newtypes ([`DeviceKey`],
//!   [`SampleRate`], ...) are plain structs that callers build with struct
//!   literals; they are frozen: adding a field to them is a breaking
//!   change.
//! * [`Command::KINDS`] lists every command; adding a command extends it,
//!   which permission tables built on it must follow.
//!
//! # Confirmation of restarting changes
//!
//! A change whose [`Impact`] has a [`RestartScope`] other than `None`
//! (today: sample rate, encoding, latency, channel counts, primary
//! interface, a reload that restarts a device) is applied for a local user or an API principal only when it
//! arrives as [`RequestBody::ConfirmedCommand`]; as
//! [`RequestBody::Command`] it is refused with
//! [`CommandError::ConfirmationRequired`] carrying the impact, and nothing
//! changes. A client shows [`Impact::describe`], asks, and resends it
//! confirmed with the impact's [`Impact::token`] in `expect`; if the impact
//! changed in between, the daemon asks again. Changes with scope `None`
//! behave the same either way. Network controllers (`Origin::Protocol`),
//! configuration reloads by signal and the daemon itself are not asked.
//!
//! This is a behaviour change for existing clients: a plain `Command` for
//! `SetSampleRate`, `SetEncoding`, `SetLatency`, `SetPrimaryInterface` or a
//! restarting `ReloadConfig` now answers `ConfirmationRequired`.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::num::NonZeroU16;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

mod impact;

pub use impact::{
    Impact, ImpactToken, Limits, RemovedSubscription, RestartImpact, RestartScope, RingMemory,
};

/// Version of the request/response envelope (ADR 0008).
pub const WIRE_VERSION: u32 = 1;

/// Operator-chosen key of a logical device (`[[device]] id` in config).
/// Not the device ID on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct DeviceKey(pub String);

/// 1-based channel number, as on the wire.
pub type Channel = NonZeroU16;

/// Audio direction relative to this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum Direction {
    /// Network → this device (Dante "receive" channels).
    Rx,
    /// This device → network (Dante "transmit" channels).
    Tx,
}

/// Sample rate in Hz. Not an enum: supported rates are validated by the
/// daemon, so new rates (pull-up/down) need no API change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct SampleRate(pub u32);

/// Bits per sample on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct Encoding(pub u8);

/// Latency in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct LatencyUs(pub u32);

/// Who issued a command. Recorded with every resulting event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum Origin {
    /// A peer on the network via the vendor control protocol.
    Protocol { peer: String },
    /// Local CLI user.
    Cli { uid: u32 },
    /// Config file load or reload.
    Config,
    /// Future network API.
    Api { principal: String },
    /// The daemon itself (e.g. restoring persisted state).
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum ClockRole {
    Follower,
    Leader,
    /// Leader-capable but follower-preferred.
    Auto,
}

/// A request to change state. Every surface goes through these.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum Command {
    SetDeviceName {
        device: DeviceKey,
        name: String,
    },
    /// `label: None` restores the default label.
    SetChannelLabel {
        device: DeviceKey,
        direction: Direction,
        channel: Channel,
        label: Option<String>,
    },
    Subscribe {
        device: DeviceKey,
        rx_channel: Channel,
        tx_channel: String,
        tx_device: String,
    },
    Unsubscribe {
        device: DeviceKey,
        rx_channel: Channel,
    },
    SetSampleRate {
        device: DeviceKey,
        rate: SampleRate,
    },
    SetEncoding {
        device: DeviceKey,
        encoding: Encoding,
    },
    SetLatency {
        device: DeviceKey,
        latency: LatencyUs,
    },
    SetClockRole {
        role: ClockRole,
    },
    SetClockPriority {
        priority: u8,
    },
    /// Select the primary network interface at run time (ADR 0012).
    /// `interface: None` clears the selection: the configured or automatic
    /// choice applies again.
    SetPrimaryInterface {
        interface: Option<String>,
    },
    ReloadConfig,
    /// Change a device's RX and TX channel counts at run time (persisted in
    /// `state.toml`). Not both counts may be 0. A direction that is 0 now
    /// (a device configured rx-only or tx-only) may stay 0 while the other
    /// changes, but a direction that has channels cannot be taken to 0 at
    /// run time for now (`Invalid`, pointing at the configuration file).
    /// Refused with [`CommandError::Locked`] when the configuration
    /// lists `rx_channels` or `tx_channels` in the device's `locked` and that
    /// count would change.
    ///
    /// The device restarts ([`RestartScope::Device`]): its flows are
    /// rebuilt, its audio clients are detached, it is re-announced, and
    /// subscriptions on RX channels that no longer exist are deleted. With
    /// `dry_run: true` nothing changes and the reply is
    /// [`Reply::Impact`]; without it, a control surface that has not
    /// confirmed gets [`CommandError::ConfirmationRequired`] (see
    /// [`RequestBody::ConfirmedCommand`]).
    SetDeviceChannels {
        device: DeviceKey,
        rx: u16,
        tx: u16,
        /// Only compute the [`Impact`]; never changes anything.
        #[cfg_attr(feature = "serde", serde(default))]
        dry_run: bool,
    },
}

impl Command {
    /// Every value [`kind`](Self::kind) can return. Tests use it to make sure
    /// every command is covered by permission and logging tables.
    pub const KINDS: &'static [&'static str] = &[
        "set_device_name",
        "set_channel_label",
        "subscribe",
        "unsubscribe",
        "set_sample_rate",
        "set_encoding",
        "set_latency",
        "set_clock_role",
        "set_clock_priority",
        "set_primary_interface",
        "reload_config",
        "set_device_channels",
    ];

    /// The command's name without its arguments: safe to log (the arguments
    /// contain client-chosen strings).
    ///
    /// The `match` is exhaustive on purpose: adding a command without naming
    /// it here does not compile.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SetDeviceName { .. } => "set_device_name",
            Self::SetChannelLabel { .. } => "set_channel_label",
            Self::Subscribe { .. } => "subscribe",
            Self::Unsubscribe { .. } => "unsubscribe",
            Self::SetSampleRate { .. } => "set_sample_rate",
            Self::SetEncoding { .. } => "set_encoding",
            Self::SetLatency { .. } => "set_latency",
            Self::SetClockRole { .. } => "set_clock_role",
            Self::SetClockPriority { .. } => "set_clock_priority",
            Self::SetPrimaryInterface { .. } => "set_primary_interface",
            Self::ReloadConfig => "reload_config",
            Self::SetDeviceChannels { .. } => "set_device_channels",
        }
    }
}

/// A read-only request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum Query {
    Status,
    Device {
        device: DeviceKey,
    },
    /// Counters and gauges of the daemon (`Reply::Metrics`).
    Metrics,
    /// The configuration the daemon is actually using: the merged files with
    /// the values stored at run time applied (`Reply::EffectiveConfig`).
    EffectiveConfig,
    /// The in-daemon hardware-card bridges and their live state
    /// (`Reply::Bridges`).
    Bridges,
}

/// Why a command was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum CommandError {
    Invalid {
        field: String,
        reason: String,
    },
    /// The value is pinned by `locked` in the config file, or set on the
    /// daemon's command line (`by`). Build it with [`CommandError::locked`]
    /// or [`CommandError::locked_by`].
    #[non_exhaustive]
    Locked {
        field: String,
        /// What pins it when it is not the configuration's `locked` list
        /// (`"<command line>"`). Absent in older JSON.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        by: Option<String>,
    },
    NotFound {
        what: String,
    },
    PermissionDenied,
    /// The caller sent too many requests; retry later.
    RateLimited,
    Unsupported {
        what: String,
    },
    Internal {
        message: String,
    },
    /// The daemon's reply would be larger than its configured frame limit.
    /// The connection stays open; this error frame itself always fits.
    /// `key` names the setting that sets the limit
    /// (`"control.max_frame_bytes"`).
    ResponseTooLarge {
        /// Size of the reply that was not sent, bytes.
        size: u64,
        /// The limit, bytes.
        limit: u64,
        /// The configuration key of the limit.
        key: String,
    },
    /// The command needs a restart (of a device or the daemon) and was sent
    /// without confirmation. Nothing was changed. Show `impact` to the user
    /// and send the command again as [`RequestBody::ConfirmedCommand`].
    ConfirmationRequired {
        /// What applying the command would do.
        impact: Box<Impact>,
    },
}

impl CommandError {
    /// `Locked` by the configuration's `locked` list.
    #[must_use]
    pub fn locked(field: impl Into<String>) -> Self {
        Self::Locked {
            field: field.into(),
            by: None,
        }
    }

    /// `Locked` by another source (`"<command line>"`).
    #[must_use]
    pub fn locked_by(field: impl Into<String>, by: impl Into<String>) -> Self {
        Self::Locked {
            field: field.into(),
            by: Some(by.into()),
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { field, reason } => write!(f, "invalid {field}: {reason}"),
            Self::Locked { field, by: None } => {
                write!(f, "{field} is locked by the configuration")
            }
            Self::Locked {
                field,
                by: Some(by),
            } => write!(f, "{field} is set by {by}"),
            Self::NotFound { what } => write!(f, "not found: {what}"),
            Self::PermissionDenied => f.write_str("permission denied"),
            Self::RateLimited => f.write_str("rate limited; retry later"),
            Self::Unsupported { what } => write!(f, "unsupported: {what}"),
            Self::Internal { message } => write!(f, "internal error: {message}"),
            Self::ResponseTooLarge { size, limit, key } => write!(
                f,
                "response of {size} bytes exceeds the daemon's limit of {limit} bytes ({key})"
            ),
            Self::ConfirmationRequired { impact } => {
                write!(f, "confirmation required: {}", impact.restart.reason)
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for CommandError {}

/// Clock state (ADR 0005).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum ClockState {
    FreeRunning,
    Listening,
    Acquiring,
    Locked,
    Holdover,
    Leader,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum Timestamping {
    Hardware,
    Software,
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct ClockStatus {
    pub interface: String,
    pub state: ClockState,
    pub timestamping: Timestamping,
    /// Offset from the leader, if following.
    pub offset_ns: Option<i64>,
    /// Media clock rate relative to `CLOCK_MONOTONIC`, parts per billion.
    pub rate_ppb: i64,
    pub grandmaster: Option<String>,
    /// Increments on every clock discontinuity.
    pub epoch: u64,
    /// How many times the PTP runner of this interface was started since the
    /// daemon began (a live role, priority or timestamping change does not
    /// restart it).
    #[cfg_attr(feature = "serde", serde(default))]
    pub runner_starts: u64,
    /// Live commands the PTP runner has applied (all runners since start).
    #[cfg_attr(feature = "serde", serde(default))]
    pub commands_applied: u64,
    /// Live commands the PTP runner could not carry out (for example hardware
    /// timestamping on an interface that cannot stamp PTPv1).
    #[cfg_attr(feature = "serde", serde(default))]
    pub commands_rejected: u64,
}

impl ClockStatus {
    /// A status with no offset, a zero rate, no grandmaster and epoch 0;
    /// refine it with the `with_*` methods.
    #[must_use]
    pub fn new(
        interface: impl Into<String>,
        state: ClockState,
        timestamping: Timestamping,
    ) -> Self {
        Self {
            interface: interface.into(),
            state,
            timestamping,
            offset_ns: None,
            rate_ppb: 0,
            grandmaster: None,
            epoch: 0,
            runner_starts: 0,
            commands_applied: 0,
            commands_rejected: 0,
        }
    }

    /// Set the offset from the leader.
    #[must_use]
    pub fn with_offset_ns(mut self, offset_ns: Option<i64>) -> Self {
        self.offset_ns = offset_ns;
        self
    }

    /// Set the media clock rate, parts per billion.
    #[must_use]
    pub fn with_rate_ppb(mut self, rate_ppb: i64) -> Self {
        self.rate_ppb = rate_ppb;
        self
    }

    /// Set the grandmaster.
    #[must_use]
    pub fn with_grandmaster(mut self, grandmaster: Option<String>) -> Self {
        self.grandmaster = grandmaster;
        self
    }

    /// Set the discontinuity counter.
    #[must_use]
    pub fn with_epoch(mut self, epoch: u64) -> Self {
        self.epoch = epoch;
        self
    }

    /// Set the PTP runner counters (starts, live commands applied and rejected).
    #[must_use]
    pub fn with_runner(mut self, starts: u64, applied: u64, rejected: u64) -> Self {
        self.runner_starts = starts;
        self.commands_applied = applied;
        self.commands_rejected = rejected;
        self
    }
}

/// State of one RX subscription, with the reason when it is not receiving.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum SubscriptionState {
    Receiving,
    InProgress,
    /// The TX channel or device could not be found via discovery.
    Unresolved,
    /// The transmitter refused the flow request.
    FlowRefused {
        code: u16,
    },
    /// A flow exists but no media is arriving.
    NoMedia,
    RateMismatch {
        ours: SampleRate,
        theirs: SampleRate,
    },
    EncodingMismatch {
        ours: Encoding,
        theirs: Encoding,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct SubscriptionStatus {
    pub rx_channel: Channel,
    pub tx_channel: String,
    pub tx_device: String,
    pub state: SubscriptionState,
    /// Effective latency (max of ours and the transmitter's minimum), once known.
    pub effective_latency: Option<LatencyUs>,
}

impl SubscriptionStatus {
    /// A subscription status with no effective latency yet; set it with
    /// [`with_effective_latency`](Self::with_effective_latency).
    #[must_use]
    pub fn new(
        rx_channel: Channel,
        tx_channel: impl Into<String>,
        tx_device: impl Into<String>,
        state: SubscriptionState,
    ) -> Self {
        Self {
            rx_channel,
            tx_channel: tx_channel.into(),
            tx_device: tx_device.into(),
            state,
            effective_latency: None,
        }
    }

    /// Set the effective latency.
    #[must_use]
    pub fn with_effective_latency(mut self, effective_latency: Option<LatencyUs>) -> Self {
        self.effective_latency = effective_latency;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct ChannelStatus {
    pub channel: Channel,
    pub label: String,
}

impl ChannelStatus {
    /// A channel and its effective label.
    #[must_use]
    pub fn new(channel: Channel, label: impl Into<String>) -> Self {
        Self {
            channel,
            label: label.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct DeviceStatus {
    pub key: DeviceKey,
    pub name: String,
    pub sample_rate: SampleRate,
    pub encoding: Encoding,
    pub latency: LatencyUs,
    pub rx_channels: Vec<ChannelStatus>,
    pub tx_channels: Vec<ChannelStatus>,
    pub subscriptions: Vec<SubscriptionStatus>,
    /// Config keys pinned by `locked`.
    pub locked: Vec<String>,
}

impl DeviceStatus {
    /// A device status with no channels, subscriptions or locks; add them
    /// with the `with_*` methods.
    #[must_use]
    pub fn new(
        key: DeviceKey,
        name: impl Into<String>,
        sample_rate: SampleRate,
        encoding: Encoding,
        latency: LatencyUs,
    ) -> Self {
        Self {
            key,
            name: name.into(),
            sample_rate,
            encoding,
            latency,
            rx_channels: Vec::new(),
            tx_channels: Vec::new(),
            subscriptions: Vec::new(),
            locked: Vec::new(),
        }
    }

    /// Set the RX channels.
    #[must_use]
    pub fn with_rx_channels(mut self, rx_channels: Vec<ChannelStatus>) -> Self {
        self.rx_channels = rx_channels;
        self
    }

    /// Set the TX channels.
    #[must_use]
    pub fn with_tx_channels(mut self, tx_channels: Vec<ChannelStatus>) -> Self {
        self.tx_channels = tx_channels;
        self
    }

    /// Set the subscriptions.
    #[must_use]
    pub fn with_subscriptions(mut self, subscriptions: Vec<SubscriptionStatus>) -> Self {
        self.subscriptions = subscriptions;
        self
    }

    /// Set the locked config keys.
    #[must_use]
    pub fn with_locked(mut self, locked: Vec<String>) -> Self {
        self.locked = locked;
        self
    }
}

/// The effective clock role and priority (command, stored value or config).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct ClockSettings {
    /// Role.
    pub role: ClockRole,
    /// Leader-election priority (lower wins).
    pub priority: u8,
    /// Settings pinned by `locked` (`role`, `priority`).
    pub locked: Vec<String>,
}

impl ClockSettings {
    /// Settings with nothing locked; add locks with
    /// [`with_locked`](Self::with_locked).
    #[must_use]
    pub fn new(role: ClockRole, priority: u8) -> Self {
        Self {
            role,
            priority,
            locked: Vec::new(),
        }
    }

    /// Set the locked keys.
    #[must_use]
    pub fn with_locked(mut self, locked: Vec<String>) -> Self {
        self.locked = locked;
        self
    }
}

/// How the primary interface in effect was chosen (ADR 0012).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum PrimarySource {
    /// `[network] locked = ["primary"]`: the first configured interface.
    Locked,
    /// The runtime selection (`SetPrimaryInterface`).
    Selected,
    /// The first entry of `[network] interfaces`.
    Configured,
    /// No selection and no configured interface: the first wired interface
    /// with carrier and an address.
    Automatic,
    /// A runtime selection exists but its interface cannot be used now; the
    /// configured or automatic choice stands in until it can.
    Fallback,
}

/// One network interface of the host, as a candidate for the primary.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct InterfaceStatus {
    /// Interface name.
    pub name: String,
    /// Hardware address (`aa:bb:cc:dd:ee:ff`), if it has one.
    pub mac: Option<String>,
    /// First IPv4 address.
    pub ipv4: Option<String>,
    /// The link is up (carrier).
    pub carrier: bool,
    /// Wi-Fi interface.
    pub wireless: bool,
    /// Software interface (bridge, veth, tunnel, ...).
    pub is_virtual: bool,
    /// Can be selected as the primary now.
    pub usable: bool,
    /// Why it cannot be used, if it cannot.
    pub reason: Option<String>,
}

impl InterfaceStatus {
    /// An interface with no address, no link and nothing else known; set the
    /// fields directly.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            mac: None,
            ipv4: None,
            carrier: false,
            wireless: false,
            is_virtual: false,
            usable: false,
            reason: None,
        }
    }
}

/// The primary interface in effect and the candidates (ADR 0012).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct NetworkStatus {
    /// The primary interface in effect (`None`: no usable interface).
    pub primary: Option<String>,
    /// How it was chosen.
    pub source: PrimarySource,
    /// The stored runtime selection, whether or not it is in effect.
    pub selected: Option<String>,
    /// The primary is pinned by configuration.
    pub locked: bool,
    /// Wi-Fi interfaces may be used.
    pub allow_wireless: bool,
    /// Every interface of the host except loopback, by name.
    pub interfaces: Vec<InterfaceStatus>,
}

impl NetworkStatus {
    /// A status with no candidates; set the fields directly.
    #[must_use]
    pub fn new(primary: Option<String>, source: PrimarySource) -> Self {
        Self {
            primary,
            source,
            selected: None,
            locked: false,
            allow_wireless: false,
            interfaces: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct Status {
    pub version: String,
    pub devices: Vec<DeviceStatus>,
    pub clocks: Vec<ClockStatus>,
    /// Effective clock role and priority. `None` from a daemon that does not
    /// report them.
    #[cfg_attr(feature = "serde", serde(default))]
    pub clock_settings: Option<ClockSettings>,
    /// The primary interface and the candidates. `None` from a daemon that
    /// does not report them, or before the first look at the host.
    #[cfg_attr(feature = "serde", serde(default))]
    pub network: Option<NetworkStatus>,
    /// Human-readable health warnings (no RT priority, Wi-Fi, …).
    pub warnings: Vec<String>,
    /// The in-daemon hardware-card bridges (`[[bridge]]`). Empty from a
    /// daemon without bridges or one that predates them.
    #[cfg_attr(feature = "serde", serde(default))]
    pub bridges: Vec<BridgeStatus>,
    /// The daemon's control-socket limits, so clients need not read its
    /// configuration. `None` from a daemon that predates them.
    #[cfg_attr(feature = "serde", serde(default))]
    pub limits: Option<Limits>,
}

impl Status {
    /// A status snapshot with no devices, clocks or warnings; add them with
    /// the `with_*` methods.
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            devices: Vec::new(),
            clocks: Vec::new(),
            clock_settings: None,
            network: None,
            warnings: Vec::new(),
            bridges: Vec::new(),
            limits: None,
        }
    }

    /// Set the control-socket limits.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Set the hardware-card bridges.
    #[must_use]
    pub fn with_bridges(mut self, bridges: Vec<BridgeStatus>) -> Self {
        self.bridges = bridges;
        self
    }

    /// Set the devices.
    #[must_use]
    pub fn with_devices(mut self, devices: Vec<DeviceStatus>) -> Self {
        self.devices = devices;
        self
    }

    /// Set the clocks.
    #[must_use]
    pub fn with_clocks(mut self, clocks: Vec<ClockStatus>) -> Self {
        self.clocks = clocks;
        self
    }

    /// Set the health warnings.
    #[must_use]
    pub fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings = warnings;
        self
    }

    /// Set the effective clock role and priority.
    #[must_use]
    pub fn with_clock_settings(mut self, settings: ClockSettings) -> Self {
        self.clock_settings = Some(settings);
        self
    }

    /// Set the network status.
    #[must_use]
    pub fn with_network(mut self, network: NetworkStatus) -> Self {
        self.network = Some(network);
        self
    }
}

/// Which way an in-daemon hardware-card bridge carries audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum BridgeDirection {
    /// Network to card: the device's RX channels are played on the card.
    Playback,
    /// Card to network: the card's input feeds the device's TX channels.
    Capture,
}

/// Life cycle of a hardware-card bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum BridgeState {
    /// Configured, not opened yet.
    Starting,
    /// The card is open and the loop runs.
    Running,
    /// The card is missing, busy, unplugged or failing: the daemon retries
    /// with a back-off (`last_error` says why).
    Reopening,
    /// The bridge cannot run with this configuration (for example a device
    /// rate the card refuses to match); it is retried only after a change.
    Failed,
    /// `enabled = false` in the configuration.
    Disabled,
}

/// One in-daemon hardware-card bridge and its live state. Gauges are the
/// latest observation (at most about a second old); counters only grow over
/// the bridge's life (a reload that restarts it keeps `reopens` and resets
/// the rest).
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct BridgeStatus {
    /// `[[bridge]] name`.
    pub name: String,
    /// The device whose rings the bridge attaches to.
    pub device: DeviceKey,
    /// ALSA PCM name of the card (`hw:CARD=DigiAMP,DEV=0`).
    pub card: String,
    /// Playback (network to card) or capture.
    pub direction: BridgeDirection,
    /// First device channel carried (1-based).
    pub first_channel: u16,
    /// Channels carried.
    pub channels: u16,
    /// Life cycle.
    pub state: BridgeState,
    /// Frames queued in the card (playback: written, not yet played;
    /// capture: captured, not yet read). Gauge.
    pub fill_frames: u64,
    /// Position error of the drift loop against the media clock, frames
    /// (playback: positive means behind). Gauge.
    pub position_error_frames: i64,
    /// Correction the PI loop applies to the resampling ratio, parts per
    /// billion (`(ratio - 1) * 1e9`). Gauge.
    pub correction_ppb: i64,
    /// The clock offset the loop has learned (the correction without its
    /// proportional term), parts per billion. Gauge.
    pub trend_ppb: i64,
    /// The resampling ratio in effect (output frames per input frame
    /// relative to nominal; `1.0` is no correction). Gauge.
    pub ratio: f64,
    /// Under- and overruns of the card, recovered.
    pub xruns: u64,
    /// Times the card was opened successfully again after a successful open
    /// (a failed attempt is not a reopen: see `open_failures`).
    pub reopens: u64,
    /// Attempts to open the card that failed (missing, busy, refused).
    pub open_failures: u64,
    /// Changes every time the daemon starts this bridge afresh: each
    /// successful open, also after a reload that restarts it. A live retune
    /// leaves it alone. Comparable only between rows of one daemon run.
    pub generation: u64,
    /// Card errors that were neither an xrun nor a removal.
    pub card_errors: u64,
    /// Chunks moved between the card and the ring.
    pub chunks: u64,
    /// Playback: chunks played as silence because no ring was attached.
    /// Capture: chunks discarded.
    pub silence_chunks: u64,
    /// Playback: frames read that the network had not delivered (silence).
    /// Capture: frames the TX ring dropped as late.
    pub missing_frames: u64,
    /// Re-anchors of the drift loop (it lost track; an audible slip).
    pub slips: u64,
    /// Latency the bridge adds, frames: playback reads this far behind the
    /// device's RX latency (its guard), capture writes this far ahead of the
    /// media clock (its lead). Grows with a slow loop (its transient). 0
    /// while not running.
    pub added_latency_frames: u64,
    /// Playback: frames the bridge's resampler did not produce in time,
    /// played as silence (a gap of the bridge itself, not of the network;
    /// stays 0 in a healthy bridge).
    pub short_frames: u64,
    /// Real-time scheduling was requested for the bridge thread and refused.
    pub rt_degraded: bool,
    /// The time constant of the PI loop in effect, seconds.
    pub time_constant_s: f64,
    /// The correction clamp in effect, ppm.
    pub max_correction_ppm: f64,
    /// The last error (open, card, configuration), if any.
    pub last_error: Option<String>,
}

impl Default for BridgeStatus {
    fn default() -> Self {
        Self::new(
            "",
            DeviceKey(String::new()),
            "",
            BridgeDirection::Playback,
            BridgeState::Starting,
        )
    }
}

impl BridgeStatus {
    /// A bridge with every gauge and counter zero; set the fields directly.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        device: DeviceKey,
        card: impl Into<String>,
        direction: BridgeDirection,
        state: BridgeState,
    ) -> Self {
        Self {
            name: name.into(),
            device,
            card: card.into(),
            direction,
            first_channel: 1,
            channels: 0,
            state,
            fill_frames: 0,
            position_error_frames: 0,
            correction_ppb: 0,
            trend_ppb: 0,
            ratio: 1.0,
            xruns: 0,
            reopens: 0,
            open_failures: 0,
            generation: 0,
            card_errors: 0,
            chunks: 0,
            silence_chunks: 0,
            missing_frames: 0,
            slips: 0,
            added_latency_frames: 0,
            short_frames: 0,
            rt_degraded: false,
            time_constant_s: 0.0,
            max_correction_ppm: 0.0,
            last_error: None,
        }
    }
}

/// Counters and gauges of the daemon process. Counters only grow; gauges go
/// up and down.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct DaemonMetrics {
    /// Milliseconds since the daemon started.
    pub uptime_ms: u64,
    /// Control connections open now (gauge).
    pub control_connections: u64,
    /// Control connections accepted since start.
    pub control_connections_total: u64,
    /// Control connections refused by a connection cap.
    pub control_connections_refused: u64,
    /// Request frames received.
    pub requests_total: u64,
    /// Requests dropped by the rate limiter.
    pub requests_rate_limited: u64,
    /// Mutating commands refused for lack of permission.
    pub requests_denied: u64,
    /// Frames that were not valid requests.
    pub requests_invalid: u64,
    /// Event subscribers connected now (gauge).
    pub subscribers: u64,
    /// Times a subscriber fell behind and lost events.
    pub subscriber_lags: u64,
    /// Restarts of supervised control-plane tasks.
    pub task_restarts: u64,
}

impl DaemonMetrics {
    /// All counters zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Counters of the audio client endpoint (`audio.sock`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct AudioMetrics {
    /// Sessions attached now (gauge).
    pub sessions: u64,
    /// Sessions attached since start.
    pub sessions_total: u64,
    /// Hellos answered with a reject.
    pub rejects_total: u64,
    /// Bytes of ring memory reserved by attached sessions (gauge).
    pub ring_bytes: u64,
    /// Connections refused by a connection cap.
    pub connections_refused: u64,
}

impl AudioMetrics {
    /// All counters zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Counters of the media engine (the real-time worker). Counters only grow;
/// gauges go up and down; `max_*` values are peaks since the daemon started.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct EngineMetrics {
    /// Whether the worker thread is running (gauge, 0 or 1).
    pub running: u64,
    /// Engine ticks executed.
    pub ticks: u64,
    /// Ticks that started more than one tick late: the worker skipped
    /// forward instead of catching up (ADR 0004).
    pub tick_skips: u64,
    /// Largest wake-up lateness observed, microseconds.
    pub max_wake_lateness_us: u64,
    /// Longest time spent working inside one tick, microseconds.
    pub max_tick_work_us: u64,
    /// Datagrams received from flow sockets.
    pub rx_datagrams: u64,
    /// `recvmmsg` calls that returned data.
    pub rx_batches: u64,
    /// Datagrams dropped because they came from an address other than the
    /// flow's `peer` filter.
    pub rx_filtered: u64,
    /// Datagrams larger than the flow's packet buffer (truncated, counted
    /// as malformed by the flow).
    pub rx_truncated: u64,
    /// Ticks in which a socket still had data after the per-tick batch
    /// bound was reached.
    pub rx_batch_limit_hits: u64,
    /// Datagrams handed to the kernel.
    pub tx_datagrams: u64,
    /// `sendmmsg` calls.
    pub tx_batches: u64,
    /// Datagrams the kernel refused (full socket buffer or an error); they
    /// are never retried or sent late (ADR 0004).
    pub tx_send_failures: u64,
    /// Polls skipped because the media clock was neither locked nor leading
    /// and `free_running_tx` is off (ADR 0005).
    pub tx_suppressed_ticks: u64,
    /// Wake bytes signalled to audio clients.
    pub wake_signals: u64,
    /// Audio sessions served by the worker (gauge).
    pub sessions: u64,
    /// Flows running in the worker (gauge).
    pub flows: u64,
    /// 1 if real-time scheduling was requested but not granted: the engine
    /// runs in degraded mode (ADR 0004: never silent).
    pub rt_degraded: u64,
    /// 1 if `mlockall` was requested but refused.
    pub mlock_failed: u64,
    /// Events the worker could not queue for the control thread.
    pub events_dropped: u64,
    /// Receive calls that failed with an unexpected error.
    pub rx_errors: u64,
    /// Objects the worker had to leak because it could not hand them back to
    /// the control thread (it never frees; this should stay 0).
    pub retire_leaks: u64,
    /// One-way transit of received datagrams: the kernel receive time (media
    /// time of this device) minus the instant the sender could first send the
    /// packet (its timestamp plus its duration). It includes the sender's
    /// transmit scheduling lag, the wire and the receiver's clock error
    /// against the sender's, so it is only as good as the clock sync.
    /// Bucket 0 is below 0; bucket `k` (1 to 40) covers
    /// `[(k-1) * 50, k * 50)` microseconds; the last bucket is 2000 us and up.
    pub rx_transit_hist: Vec<u64>,
    /// Largest transit seen, microseconds (clamped at 0).
    pub rx_transit_max_us: u64,
    /// The smallest kernel buffer (receive or send, bytes) of any media
    /// socket the engine opened; 0 if none yet. The kernel reads back twice
    /// the value it accepted.
    pub socket_buffer_min_bytes: u64,
    /// Media sockets whose buffer the kernel capped below what was asked for
    /// (`net.core.rmem_max` / `wmem_max`).
    pub socket_buffers_capped: u64,
    /// How late the worker woke up from its sleep, a histogram. Bucket `k`
    /// counts wake-ups with a lateness below the upper edge
    /// `[25, 50, 100, 250, 500, 1000, 2000, 4000]` microseconds (first
    /// matching edge); the last bucket is 4000 us and up. A late skip or a
    /// hole in a client's audio can be matched against these.
    pub wake_lateness_hist: Vec<u64>,
}

impl EngineMetrics {
    /// All counters zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Receive-side counters of one flow (`tessera-media` `RxMetrics`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct RxFlowMetrics {
    /// Datagrams that could not be decoded.
    pub malformed_packets: u64,
    /// Packets dropped as too far in the future.
    pub early_packets: u64,
    /// Packets whose first frame had already missed its playout time.
    pub late_packets: u64,
    /// Frames dropped because their playout time had passed.
    pub late_frames: u64,
    /// Forward jumps in timestamp continuity (packet loss or reordering).
    pub gap_events: u64,
    /// Frames skipped by forward jumps.
    pub gap_frames: u64,
    /// Packets behind the expected next timestamp.
    pub out_of_order_packets: u64,
    /// Jumps of at least one buffer capacity.
    pub discontinuities: u64,
    /// Buffer resets caused by a clock epoch change.
    pub epoch_resets: u64,
    /// Samples read that were never written (silence).
    pub hole_samples: u64,
    /// Samples read beyond the newest data (silence).
    pub unwritten_samples: u64,
    /// Samples read that were older than the window (silence).
    pub overrun_samples: u64,
    /// Samples read that were valid.
    pub valid_samples: u64,
    /// Samples silenced because of a clock epoch mismatch.
    pub epoch_mismatch_samples: u64,
    /// Largest lateness (receive media time minus packet timestamp)
    /// observed since the daemon started, microseconds.
    pub max_lateness_us: u64,
    /// Lateness histogram, bucket upper edges in microseconds:
    /// `<0`, `<125`, `<250`, `<500`, `<1000`, `<2000`, `<4000`, `<8000`,
    /// `<16000`, `<32000`, and the rest.
    pub lateness_hist: Vec<u64>,
    /// Receiver keepalives sent to the flow's transmitter (to the source of
    /// its media). Absent from older daemons: 0.
    pub keepalives_sent: u64,
    /// Receiver keepalives the kernel refused (dropped, never retried).
    pub keepalive_failures: u64,
}

impl RxFlowMetrics {
    /// All counters zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Transmit-side counters of one flow (`tessera-media` `TxMetrics`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct TxFlowMetrics {
    /// Channel-samples the client had not written in time (sent as silence).
    pub underrun_samples: u64,
    /// Times the flow skipped forward instead of catching up.
    pub late_skips: u64,
    /// Packets never sent because of a skip.
    pub skipped_packets: u64,
    /// Re-anchors caused by an epoch change or a backward clock step.
    pub resyncs: u64,
    /// Largest delay between a packet's release time and its send,
    /// microseconds.
    pub max_send_lag_us: u64,
    /// Datagrams of this flow the kernel refused (no route, invalid
    /// destination, full socket buffer); they are dropped, never retried.
    pub send_failures: u64,
}

impl TxFlowMetrics {
    /// All counters zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Counters of one static flow.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct FlowMetrics {
    /// The flow id (`[[flow]] id`).
    pub id: String,
    /// The device the flow belongs to.
    pub device: DeviceKey,
    /// `rx` or `tx`.
    pub direction: Direction,
    /// Packets received or sent.
    pub packets: u64,
    /// Frames received or sent.
    pub frames: u64,
    /// Datagram bytes received or sent.
    pub bytes: u64,
    /// Receive-side counters (rx flows).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub rx: Option<RxFlowMetrics>,
    /// Transmit-side counters (tx flows).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub tx: Option<TxFlowMetrics>,
}

impl FlowMetrics {
    /// A flow with all counters zero.
    #[must_use]
    pub fn new(id: impl Into<String>, device: DeviceKey, direction: Direction) -> Self {
        Self {
            id: id.into(),
            device,
            direction,
            packets: 0,
            frames: 0,
            bytes: 0,
            rx: None,
            tx: None,
        }
    }
}

/// The reply to `Query::Metrics`.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct Metrics {
    /// The daemon process.
    pub daemon: DaemonMetrics,
    /// The audio client endpoint.
    pub audio: AudioMetrics,
    /// Clock state per interface (same as in `Status`).
    pub clocks: Vec<ClockStatus>,
    /// The media engine (absent in replies of daemons that predate it).
    /// Boxed: it is by far the largest part of a [`Reply`].
    #[cfg_attr(feature = "serde", serde(default))]
    pub engine: Box<EngineMetrics>,
    /// Static flows (PROVISIONAL, tessera-only).
    #[cfg_attr(feature = "serde", serde(default))]
    pub flows: Vec<FlowMetrics>,
    /// In-daemon hardware-card bridges (the same rows as in `Status`).
    #[cfg_attr(feature = "serde", serde(default))]
    pub bridges: Vec<BridgeStatus>,
}

impl Metrics {
    /// A metrics snapshot.
    #[must_use]
    pub fn new(daemon: DaemonMetrics, audio: AudioMetrics, clocks: Vec<ClockStatus>) -> Self {
        Self {
            daemon,
            audio,
            clocks,
            engine: Box::default(),
            flows: Vec::new(),
            bridges: Vec::new(),
        }
    }

    /// Add the hardware-card bridges.
    #[must_use]
    pub fn with_bridges(mut self, bridges: Vec<BridgeStatus>) -> Self {
        self.bridges = bridges;
        self
    }

    /// Add the media engine counters.
    #[must_use]
    pub fn with_engine(mut self, engine: EngineMetrics) -> Self {
        self.engine = Box::new(engine);
        self
    }

    /// Add the per-flow counters.
    #[must_use]
    pub fn with_flows(mut self, flows: Vec<FlowMetrics>) -> Self {
        self.flows = flows;
        self
    }
}

/// A state change.
///
/// Events for commands are published after the change has been applied to
/// the in-memory model and before the debounced write of `state.toml`
/// reaches the disk. A subscriber can therefore see a change that a crash
/// within the debounce window loses. Events that report runtime observations
/// (`SubscriptionChanged` from a runtime report, `ClockStateChanged`) are not
/// persisted at all.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum Event {
    DeviceRenamed {
        device: DeviceKey,
        name: String,
        origin: Origin,
    },
    ChannelLabelChanged {
        device: DeviceKey,
        direction: Direction,
        channel: Channel,
        label: String,
        origin: Origin,
    },
    SubscriptionChanged {
        device: DeviceKey,
        status: SubscriptionStatus,
    },
    SubscriptionRemoved {
        device: DeviceKey,
        rx_channel: Channel,
        origin: Origin,
    },
    DeviceSettingsChanged {
        device: DeviceKey,
        sample_rate: SampleRate,
        encoding: Encoding,
        latency: LatencyUs,
        origin: Origin,
    },
    ClockStateChanged {
        status: ClockStatus,
    },
    /// The configured clock role or priority changed (command or reload).
    ClockSettingsChanged {
        role: ClockRole,
        priority: u8,
        origin: Origin,
    },
    /// The runtime selection of the primary interface changed (command or
    /// reload; ADR 0012). `selected: None` means no selection is stored.
    NetworkSettingsChanged {
        selected: Option<String>,
        origin: Origin,
    },
    ConfigReloaded,
    /// A device's RX or TX channel count changed (command or reload). The
    /// device restarts; subscriptions removed with the channels are reported
    /// separately as `SubscriptionRemoved`.
    DeviceChannelsChanged {
        device: DeviceKey,
        rx_channels: u16,
        tx_channels: u16,
        origin: Origin,
    },
}

/// Body of a request on the local control socket.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum RequestBody {
    Command(Command),
    Query(Query),
    /// Switch this connection to an event stream.
    SubscribeEvents,
    /// A command the user has confirmed after seeing its [`Impact`] (for
    /// example `tesseractl --yes`, or the confirm button of a web form).
    /// Commands whose [`RestartScope`] is not `None` are refused with
    /// [`CommandError::ConfirmationRequired`] when sent as
    /// [`RequestBody::Command`]; everything else behaves the same in both
    /// forms. Added in place of a field on the envelope so that `Request`
    /// and every existing command keep their shape. Build it with
    /// [`RequestBody::confirmed`]; more fields may follow.
    #[non_exhaustive]
    ConfirmedCommand {
        command: Command,
        /// The [`Impact::token`] the user saw. If the daemon's recomputed
        /// impact differs, nothing is applied and the answer is
        /// `ConfirmationRequired` with the new impact. `None` confirms
        /// whatever the impact is (scripts that pass `--yes` without a dry
        /// run).
        #[cfg_attr(feature = "serde", serde(default))]
        expect: Option<ImpactToken>,
    },
}

impl RequestBody {
    /// A confirmed command; `expect` is the [`Impact::token`] the user saw
    /// (`None`: confirm whatever the impact is).
    #[must_use]
    pub fn confirmed(command: Command, expect: Option<ImpactToken>) -> Self {
        Self::ConfirmedCommand { command, expect }
    }
}

/// Request envelope: `{"v":1,"id":n,"body":{…}}`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Request {
    pub v: u32,
    pub id: u64,
    pub body: RequestBody,
}

/// Successful reply payload.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "snake_case")
)]
#[non_exhaustive]
pub enum Reply {
    Done,
    Status(Status),
    Device(DeviceStatus),
    Event(Event),
    /// Counters and gauges.
    Metrics(Metrics),
    /// The effective configuration as TOML text.
    EffectiveConfig(String),
    /// The hardware-card bridges (`Query::Bridges`).
    Bridges(Vec<BridgeStatus>),
    /// On an event stream: this subscriber fell behind and `dropped` events
    /// were lost. Re-read `Status` to resynchronise; newer events follow.
    Lagged {
        /// Number of events lost since the last delivery.
        dropped: u64,
    },
    /// What a command would do (a dry run, `SetDeviceChannels { dry_run:
    /// true }`), or what it did.
    Impact(Impact),
}

/// Response envelope; `result` is `{"ok":…}` or `{"err":…}`.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Response {
    pub v: u32,
    pub id: u64,
    #[cfg_attr(feature = "serde", serde(with = "result_repr"))]
    pub result: Result<Reply, CommandError>,
}

#[cfg(feature = "serde")]
mod result_repr {
    use super::{CommandError, Reply};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Repr<O, E> {
        Ok(O),
        Err(E),
    }

    pub fn serialize<S: Serializer>(
        r: &Result<Reply, CommandError>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match r {
            Ok(v) => Repr::<&Reply, &CommandError>::Ok(v).serialize(s),
            Err(e) => Repr::<&Reply, &CommandError>::Err(e).serialize(s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Result<Reply, CommandError>, D::Error> {
        Ok(match Repr::<Reply, CommandError>::deserialize(d)? {
            Repr::Ok(v) => Ok(v),
            Repr::Err(e) => Err(e),
        })
    }
}

#[cfg(test)]
mod constructor_tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn constructors_set_every_field() {
        let ch = Channel::new(3).unwrap();
        let s = SubscriptionStatus::new(ch, "a", "b", SubscriptionState::Receiving);
        assert_eq!(
            (s.rx_channel, s.tx_channel.as_str(), s.tx_device.as_str()),
            (ch, "a", "b")
        );
        assert_eq!(s.state, SubscriptionState::Receiving);
        assert_eq!(s.effective_latency, None);

        let d = DeviceStatus::new(
            DeviceKey("k".into()),
            "n",
            SampleRate(44_100),
            Encoding(16),
            LatencyUs(1),
        );
        assert!(d.rx_channels.is_empty() && d.subscriptions.is_empty() && d.locked.is_empty());
        let d = d.with_locked(alloc::vec!["name".into()]);
        assert_eq!(d.locked, ["name"]);

        let c = ClockStatus::new("eth0", ClockState::Leader, Timestamping::Software);
        assert_eq!((c.offset_ns, c.rate_ppb, c.epoch), (None, 0, 0));
        assert_eq!(
            CommandError::RateLimited.to_string(),
            "rate limited; retry later"
        );
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    fn ch(n: u16) -> Channel {
        Channel::new(n).unwrap()
    }

    #[test]
    fn request_json_shape_is_stable() {
        let req = Request {
            v: WIRE_VERSION,
            id: 7,
            body: RequestBody::Command(Command::Subscribe {
                device: DeviceKey("main".into()),
                rx_channel: ch(1),
                tx_channel: "Out 1".into(),
                tx_device: "stagebox-a".into(),
            }),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(
            json,
            r#"{"v":1,"id":7,"body":{"command":{"subscribe":{"device":"main","rx_channel":1,"tx_channel":"Out 1","tx_device":"stagebox-a"}}}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);
    }

    #[test]
    fn response_err_round_trips() {
        let resp = Response {
            v: WIRE_VERSION,
            id: 3,
            result: Err(CommandError::locked("sample_rate")),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(
            json,
            r#"{"v":1,"id":3,"result":{"err":{"locked":{"field":"sample_rate"}}}}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    fn sample_status() -> Status {
        let sub = SubscriptionStatus::new(
            ch(2),
            "Out 1",
            "stagebox-a",
            SubscriptionState::RateMismatch {
                ours: SampleRate(48_000),
                theirs: SampleRate(96_000),
            },
        )
        .with_effective_latency(Some(LatencyUs(4_000)));
        let dev = DeviceStatus::new(
            DeviceKey("main".into()),
            "studio",
            SampleRate(48_000),
            Encoding(24),
            LatencyUs(4_000),
        )
        .with_rx_channels(vec![ChannelStatus::new(ch(1), "RX 1")])
        .with_tx_channels(vec![ChannelStatus::new(ch(1), "Vox")])
        .with_subscriptions(vec![sub])
        .with_locked(vec!["sample_rate".into()]);
        let clock = ClockStatus::new("enp3s0", ClockState::Locked, Timestamping::Hardware)
            .with_offset_ns(Some(-120))
            .with_rate_ppb(35)
            .with_grandmaster(Some("gm-1".into()))
            .with_epoch(3);
        Status::new("0.0.0")
            .with_devices(vec![dev])
            .with_clocks(vec![clock])
            .with_warnings(vec!["no RT priority".into()])
    }

    #[test]
    fn status_values_round_trip_through_json() {
        let status = sample_status();
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), status);

        let reply = Reply::Status(status.clone());
        let json = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<Reply>(&json).unwrap(), reply);

        let reply = Reply::Device(status.devices[0].clone());
        let json = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<Reply>(&json).unwrap(), reply);

        let ev = Event::SubscriptionChanged {
            device: DeviceKey("main".into()),
            status: status.devices[0].subscriptions[0].clone(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
    }

    #[test]
    fn clock_settings_event_json_shape_is_stable() {
        let ev = Event::ClockSettingsChanged {
            role: ClockRole::Leader,
            priority: 64,
            origin: Origin::Cli { uid: 1000 },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            json,
            r#"{"clock_settings_changed":{"role":"leader","priority":64,"origin":{"cli":{"uid":1000}}}}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
    }

    #[test]
    fn rate_limited_json_shape_is_stable() {
        let resp = Response {
            v: WIRE_VERSION,
            id: 9,
            result: Err(CommandError::RateLimited),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"v":1,"id":9,"result":{"err":"rate_limited"}}"#);
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
    }

    #[test]
    fn channel_zero_is_rejected() {
        let bad = r#"{"unsubscribe":{"device":"main","rx_channel":0}}"#;
        assert!(serde_json::from_str::<Command>(bad).is_err());
    }
}

#[cfg(all(test, feature = "serde"))]
mod shape_tests {
    use super::*;

    #[test]
    fn lagged_json_shape_is_stable() {
        let r = Response {
            v: WIRE_VERSION,
            id: 4,
            result: Ok(Reply::Lagged { dropped: 12 }),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(
            json,
            r#"{"v":1,"id":4,"result":{"ok":{"lagged":{"dropped":12}}}}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), r);
    }

    #[test]
    fn new_queries_and_replies_round_trip() {
        for q in [Query::Metrics, Query::EffectiveConfig] {
            let json = serde_json::to_string(&q).unwrap();
            assert_eq!(serde_json::from_str::<Query>(&json).unwrap(), q);
        }
        assert_eq!(
            serde_json::to_string(&Query::EffectiveConfig).unwrap(),
            r#""effective_config""#
        );
        let m = Metrics::new(
            DaemonMetrics {
                uptime_ms: 5,
                requests_total: 9,
                ..DaemonMetrics::new()
            },
            AudioMetrics {
                sessions: 1,
                ring_bytes: 4096,
                ..AudioMetrics::new()
            },
            vec![ClockStatus::new(
                "lo",
                ClockState::FreeRunning,
                Timestamping::Software,
            )],
        );
        for r in [
            Reply::Metrics(m),
            Reply::EffectiveConfig("[daemon]\n".into()),
            Reply::Lagged { dropped: 1 },
        ] {
            let json = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Reply>(&json).unwrap(), r);
        }
    }

    #[test]
    fn status_carries_clock_settings_and_old_json_still_parses() {
        let s = Status::new("1").with_clock_settings(
            ClockSettings::new(ClockRole::Auto, 7).with_locked(vec!["role".into()]),
        );
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains(r#""clock_settings":{"role":"auto","priority":7,"locked":["role"]}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), s);
        // A status from a daemon that predates the field.
        let old = r#"{"version":"0","devices":[],"clocks":[],"warnings":[]}"#;
        let parsed: Status = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.clock_settings, None);
    }

    #[test]
    fn engine_and_flow_metrics_round_trip_and_old_replies_parse() {
        let mut e = EngineMetrics::new();
        e.ticks = 7;
        e.rt_degraded = 1;
        let mut f = FlowMetrics::new("a-to-b", DeviceKey("main".into()), Direction::Rx);
        f.packets = 3;
        let mut rx = RxFlowMetrics::new();
        rx.late_packets = 2;
        rx.lateness_hist = alloc::vec![0, 1, 2];
        f.rx = Some(rx);
        let m = Metrics::new(DaemonMetrics::new(), AudioMetrics::new(), Vec::new())
            .with_engine(e)
            .with_flows(alloc::vec![f]);
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<Metrics>(&json).unwrap(), m);
        assert!(
            !json.contains(r#""tx""#),
            "absent side is not serialised: {json}"
        );
        // A reply from a daemon that predates the engine.
        let old = serde_json::to_string(&serde_json::json!({
            "daemon": DaemonMetrics::new(), "audio": AudioMetrics::new(), "clocks": []
        }))
        .unwrap();
        let parsed: Metrics = serde_json::from_str(&old).unwrap();
        assert_eq!(*parsed.engine, EngineMetrics::new());
        assert!(parsed.flows.is_empty());
    }

    #[test]
    fn bridges_round_trip_in_status_metrics_and_their_own_reply() {
        let mut b = BridgeStatus::new(
            "speaker",
            DeviceKey("main".into()),
            "hw:Loopback,0,0",
            BridgeDirection::Playback,
            BridgeState::Reopening,
        );
        b.channels = 2;
        b.correction_ppb = -12_500;
        b.ratio = 0.999_987_5;
        b.last_error = Some("cannot open the card: open: No such device".into());
        let json = serde_json::to_string(&b).unwrap();
        assert!(json.contains(r#""direction":"playback""#), "{json}");
        assert!(json.contains(r#""state":"reopening""#), "{json}");
        assert_eq!(serde_json::from_str::<BridgeStatus>(&json).unwrap(), b);
        let s = Status::new("1").with_bridges(alloc::vec![b.clone()]);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), s);
        let m = Metrics::default().with_bridges(alloc::vec![b.clone()]);
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<Metrics>(&json).unwrap(), m);
        assert_eq!(
            serde_json::to_string(&Query::Bridges).unwrap(),
            r#""bridges""#
        );
        let r = Reply::Bridges(alloc::vec![b]);
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Reply>(&json).unwrap(), r);
        // Replies of daemons that predate bridges.
        let old = r#"{"version":"0","devices":[],"clocks":[],"warnings":[]}"#;
        assert!(
            serde_json::from_str::<Status>(old)
                .unwrap()
                .bridges
                .is_empty()
        );
        // A row from a newer daemon with fields this version does not know
        // about, and without some it does: defaults fill in.
        let partial = r#"{"name":"x","state":"running","future":1}"#;
        let p: BridgeStatus = serde_json::from_str(partial).unwrap();
        assert_eq!((p.name.as_str(), p.state), ("x", BridgeState::Running));
        assert_eq!(p.ratio, 1.0);
    }

    #[test]
    fn constructors_default_to_zero() {
        let d = DaemonMetrics::new();
        assert_eq!((d.uptime_ms, d.requests_total, d.task_restarts), (0, 0, 0));
        assert_eq!(AudioMetrics::new().sessions, 0);
        assert!(Metrics::default().clocks.is_empty());
        assert!(ClockSettings::new(ClockRole::Follower, 1).locked.is_empty());
    }
}

#[cfg(all(test, feature = "serde"))]
mod kind_tests {
    use super::*;

    #[test]
    fn kind_is_the_serde_variant_name() {
        let samples = [
            Command::SetDeviceName {
                device: DeviceKey("d".into()),
                name: "n".into(),
            },
            Command::SetChannelLabel {
                device: DeviceKey("d".into()),
                direction: Direction::Rx,
                channel: Channel::new(1).unwrap(),
                label: None,
            },
            Command::Subscribe {
                device: DeviceKey("d".into()),
                rx_channel: Channel::new(1).unwrap(),
                tx_channel: "a".into(),
                tx_device: "b".into(),
            },
            Command::Unsubscribe {
                device: DeviceKey("d".into()),
                rx_channel: Channel::new(1).unwrap(),
            },
            Command::SetSampleRate {
                device: DeviceKey("d".into()),
                rate: SampleRate(48_000),
            },
            Command::SetEncoding {
                device: DeviceKey("d".into()),
                encoding: Encoding(24),
            },
            Command::SetLatency {
                device: DeviceKey("d".into()),
                latency: LatencyUs(1),
            },
            Command::SetClockRole {
                role: ClockRole::Auto,
            },
            Command::SetClockPriority { priority: 1 },
            Command::SetPrimaryInterface {
                interface: Some("eth0".into()),
            },
            Command::ReloadConfig,
            Command::SetDeviceChannels {
                device: DeviceKey("d".into()),
                rx: 4,
                tx: 0,
                dry_run: false,
            },
        ];
        let mut seen: Vec<&str> = Vec::new();
        for c in &samples {
            let json = serde_json::to_value(c).unwrap();
            let name = match &json {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Object(o) => o.keys().next().unwrap().clone(),
                other => panic!("{other:?}"),
            };
            assert_eq!(c.kind(), name);
            seen.push(c.kind());
        }
        seen.sort_unstable();
        let mut all = Command::KINDS.to_vec();
        all.sort_unstable();
        assert_eq!(seen, all, "KINDS and the samples must list every command");
    }
    /// The variant list serde derives for `Command` is the ground truth:
    /// `KINDS` must match it, so a new command cannot be added without
    /// `kind()` and `KINDS` (and the permission tables built on them).
    #[test]
    fn kinds_match_the_variants_serde_knows() {
        let err = serde_json::from_str::<Command>("\"__no_such_command__\"")
            .unwrap_err()
            .to_string();
        // "unknown variant `__no_such_command__`, expected one of `a`, `b`, ... at line 1 column N"
        let list = err
            .split("expected one of ")
            .nth(1)
            .unwrap_or_else(|| panic!("unexpected serde message: {err}"));
        let list = list.split(" at line").next().unwrap();
        let mut from_serde: Vec<&str> = list.split(", ").map(|s| s.trim_matches('`')).collect();
        from_serde.sort_unstable();
        let mut kinds = Command::KINDS.to_vec();
        kinds.sort_unstable();
        assert_eq!(from_serde, kinds);
    }
}

#[cfg(all(test, feature = "serde"))]
mod config_api_tests {
    use super::*;
    use alloc::string::ToString;

    fn dev() -> DeviceKey {
        DeviceKey("main".into())
    }

    fn sample_impact() -> Impact {
        let mut i = Impact::new(
            "set_device_channels",
            RestartImpact::new(RestartScope::Device, "channel counts change")
                .with_devices(vec![dev()])
                .with_what_drops(vec!["audio on every channel of `main`".into()])
                .with_downtime_hint("a few seconds")
                .with_afterwards("re-attach audio clients"),
        );
        i.flows_restarted = vec!["rx-a".into()];
        i.subscriptions_removed = vec![RemovedSubscription::new(
            dev(),
            Channel::new(3).unwrap(),
            "Out 1",
            "stagebox",
        )];
        i.audio_clients_detached = true;
        i.ring_memory = Some(RingMemory::new(1, 2, 3));
        i
    }

    #[test]
    fn set_device_channels_json_shape_is_stable() {
        let c = Command::SetDeviceChannels {
            device: dev(),
            rx: 8,
            tx: 0,
            dry_run: true,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(
            json,
            r#"{"set_device_channels":{"device":"main","rx":8,"tx":0,"dry_run":true}}"#
        );
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), c);
        // `dry_run` may be left out.
        let short: Command =
            serde_json::from_str(r#"{"set_device_channels":{"device":"main","rx":8,"tx":2}}"#)
                .unwrap();
        assert_eq!(
            short,
            Command::SetDeviceChannels {
                device: dev(),
                rx: 8,
                tx: 2,
                dry_run: false
            }
        );
    }

    #[test]
    fn confirmed_command_envelope_json_shape_is_stable() {
        let req = Request {
            v: WIRE_VERSION,
            id: 9,
            body: RequestBody::ConfirmedCommand {
                command: Command::SetSampleRate {
                    device: dev(),
                    rate: SampleRate(96_000),
                },
                expect: Some(ImpactToken::new("v1-0123456789abcdef")),
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(
            json,
            r#"{"v":1,"id":9,"body":{"confirmed_command":{"command":{"set_sample_rate":{"device":"main","rate":96000}},"expect":"v1-0123456789abcdef"}}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);
        // `expect` may be left out.
        let short: Request = serde_json::from_str(
            r#"{"v":1,"id":9,"body":{"confirmed_command":{"command":"reload_config"}}}"#,
        )
        .unwrap();
        assert_eq!(
            short.body,
            RequestBody::ConfirmedCommand {
                command: Command::ReloadConfig,
                expect: None
            }
        );
    }

    #[test]
    fn response_too_large_json_shape_is_stable() {
        let resp = Response {
            v: WIRE_VERSION,
            id: 1,
            result: Err(CommandError::ResponseTooLarge {
                size: 2_000_000,
                limit: 1_048_576,
                key: "control.max_frame_bytes".into(),
            }),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(
            json,
            r#"{"v":1,"id":1,"result":{"err":{"response_too_large":{"size":2000000,"limit":1048576,"key":"control.max_frame_bytes"}}}}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);
        assert_eq!(
            resp.result.unwrap_err().to_string(),
            "response of 2000000 bytes exceeds the daemon's limit of 1048576 bytes (control.max_frame_bytes)"
        );
    }

    #[test]
    fn impact_json_shape_is_stable_and_round_trips() {
        let i = sample_impact();
        let json = serde_json::to_string(&i).unwrap();
        assert_eq!(
            json,
            r#"{"command":"set_device_channels","restart":{"scope":"device","devices":["main"],"reason":"channel counts change","what_drops":["audio on every channel of `main`"],"expected_downtime_hint":"a few seconds","afterwards":"re-attach audio clients"},"flows_restarted":["rx-a"],"subscriptions_removed":[{"device":"main","rx_channel":3,"tx_channel":"Out 1","tx_device":"stagebox"}],"tx_flows_removed":[],"audio_clients_detached":true,"audio_clients":null,"ring_memory":{"before_bytes":1,"after_bytes":2,"budget_bytes":3},"restart_keys":[],"token":null}"#
        );
        assert_eq!(serde_json::from_str::<Impact>(&json).unwrap(), i);
        // Every field has a default: an older or newer daemon's subset parses.
        let min: Impact = serde_json::from_str(r#"{"command":"x"}"#).unwrap();
        assert_eq!(min.restart.scope, RestartScope::None);
        assert!(!min.needs_confirmation());
        for r in [Reply::Impact(i.clone()), Reply::Done] {
            let json = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Reply>(&json).unwrap(), r);
        }
        let e = CommandError::ConfirmationRequired {
            impact: Box::new(i),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(
            json.starts_with(
                r#"{"confirmation_required":{"impact":{"command":"set_device_channels""#
            )
        );
        assert_eq!(serde_json::from_str::<CommandError>(&json).unwrap(), e);
        assert_eq!(
            e.to_string(),
            "confirmation required: channel counts change"
        );
    }

    #[test]
    fn impact_describe_names_every_part() {
        let text = sample_impact().describe();
        for want in [
            "Restarts device(s) `main`.",
            "Why: channel counts change",
            "Drops: audio on every channel of `main`",
            "Flows that restart: rx-a",
            "Deletes the subscription of `main` RX 3 to Out 1@stagebox",
            "Detaches the device's audio clients",
            "1 -> 2 bytes of a 3 byte budget",
            "Expected interruption: a few seconds",
            "Afterwards: re-attach audio clients",
        ] {
            assert!(text.contains(want), "{want:?} missing in:\n{text}");
        }
        assert_eq!(
            Impact::new("x", RestartImpact::none()).describe(),
            "Nothing restarts."
        );
    }

    #[test]
    fn restart_scope_spellings() {
        for (s, name, confirm) in [
            (RestartScope::None, "none", false),
            (RestartScope::Device, "device", true),
            (RestartScope::Daemon, "daemon", true),
        ] {
            assert_eq!(s.as_str(), name);
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                alloc::format!("\"{name}\"")
            );
            assert_eq!(s.needs_confirmation(), confirm);
        }
    }

    #[test]
    fn status_limits_round_trip_and_old_status_json_still_parses() {
        let s = Status::new("1").with_limits(Limits::new(1 << 20));
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains(r#""limits":{"max_frame_bytes":1048576}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), s);
        let old = r#"{"version":"1","devices":[],"clocks":[],"warnings":[]}"#;
        assert_eq!(serde_json::from_str::<Status>(old).unwrap().limits, None);
    }

    #[test]
    fn device_channels_changed_event_json_shape_is_stable() {
        let e = Event::DeviceChannelsChanged {
            device: dev(),
            rx_channels: 16,
            tx_channels: 2,
            origin: Origin::Cli { uid: 1000 },
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(
            json,
            r#"{"device_channels_changed":{"device":"main","rx_channels":16,"tx_channels":2,"origin":{"cli":{"uid":1000}}}}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), e);
    }
}

#[cfg(all(test, feature = "serde"))]
mod token_tests {
    use super::*;

    fn base() -> Impact {
        let mut i = Impact::new(
            "set_device_channels",
            RestartImpact::new(RestartScope::Device, "why")
                .with_devices(vec![DeviceKey("b".into()), DeviceKey("a".into())]),
        );
        i.flows_restarted = vec!["f2".into(), "f1".into()];
        i
    }

    #[test]
    fn the_token_is_deterministic_ignores_order_and_advice_and_sees_the_change() {
        let a = base();
        // Deterministic (the same on every target; it need not survive a
        // release: a dry run and its confirmation talk to one daemon).
        assert_eq!(a.token(), base().token());
        let golden = a.token();
        assert!(
            golden.as_str().starts_with("v1-") && golden.as_str().len() == 19,
            "{golden:?}"
        );
        let mut b = base();
        b.restart.devices.reverse();
        b.flows_restarted.reverse();
        b.restart.expected_downtime_hint = "x".into();
        b.restart.afterwards = "y".into();
        assert_eq!(b.token(), golden, "order and advice do not count");
        let changed: [fn(&mut Impact); 9] = [
            |i| i.command = "set_sample_rate".into(),
            |i| i.restart.reason = "from 4 to 5".into(),
            |i| i.ring_memory = Some(RingMemory::new(1, 2, 3)),
            |i| i.restart.scope = RestartScope::Daemon,
            |i| i.restart.devices.push(DeviceKey("c".into())),
            |i| i.flows_restarted.push("f3".into()),
            |i| i.restart_keys.push("engine.rx_batch".into()),
            |i| i.audio_clients_detached = true,
            |i| {
                i.subscriptions_removed.push(RemovedSubscription::new(
                    DeviceKey("a".into()),
                    Channel::new(3).unwrap(),
                    "Out",
                    "x",
                ));
            },
        ];
        for f in changed {
            let mut c = base();
            f(&mut c);
            assert_ne!(c.token(), golden);
        }
        // Sealing fills the field and does not change the digest.
        let s = base().sealed();
        assert_eq!(s.token, Some(golden.clone()));
        assert_eq!(s.token(), golden);
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains(&format!("\"token\":\"{}\"", golden.as_str())),
            "{json}"
        );
    }
}

#[cfg(all(test, feature = "serde"))]
mod round2_tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn locked_by_has_its_own_field_and_old_json_still_parses() {
        let e = CommandError::locked_by("role", "<command line>");
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(json, r#"{"locked":{"field":"role","by":"<command line>"}}"#);
        assert_eq!(serde_json::from_str::<CommandError>(&json).unwrap(), e);
        assert_eq!(e.to_string(), "role is set by <command line>");
        let old: CommandError = serde_json::from_str(r#"{"locked":{"field":"role"}}"#).unwrap();
        assert_eq!(old, CommandError::locked("role"));
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            r#"{"locked":{"field":"role"}}"#
        );
    }

    #[test]
    fn the_token_covers_the_context_and_is_opaque_text() {
        let i = Impact::new("x", RestartImpact::new(RestartScope::Device, "r"));
        assert_ne!(i.token_with(&["a"]), i.token_with(&["b"]));
        assert_eq!(i.token_with(&[]), i.token());
        let s = i.clone().sealed_with(&["a"]);
        assert_eq!(s.token, Some(i.token_with(&["a"])));
        let t = ImpactToken::new(s.token.clone().unwrap().as_str());
        assert_eq!(Some(t), s.token);
        let b = RequestBody::confirmed(Command::ReloadConfig, None);
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            r#"{"confirmed_command":{"command":"reload_config","expect":null}}"#
        );
    }
}
