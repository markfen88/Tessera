//! Configuration schema (`docs/configuration.md`) and typed config structs.
//!
//! The files are read into [`toml::Table`]s, merged (see [`crate::loader`]),
//! and then *extracted* into [`Config`] by a hand-written reader instead of a
//! derived `Deserialize`. That is deliberate: the reader reports **every**
//! type error and every unknown key with its full key path in one pass, which
//! `#[serde(deny_unknown_fields)]` cannot do (it stops at the first).
//! The observable behaviour matches ADR 0008: unknown keys are errors.
//! Semantic checks (ranges, uniqueness, cross references) are in
//! [`crate::validate`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use serde::{Serialize, Serializer};
use tessera_control_types::{ClockRole, Direction};
use toml::{Table, Value};

use crate::env::{Environment, truncate_name};
use crate::error::Issue;

macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $var:ident = $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name {
            $($(#[$vm])* $var),+
        }

        impl $name {
            /// Every value, in documentation order.
            pub const ALL: &'static [$name] = &[$($name::$var),+];

            /// The spelling used in the configuration file.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self { $($name::$var => $s),+ }
            }

            /// Parse the configuration-file spelling.
            #[must_use]
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some($name::$var),)+ _ => None }
            }

            pub(crate) fn names() -> String {
                Self::ALL.iter().map(|v| v.as_str()).collect::<Vec<_>>().join(", ")
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum! {
    /// `[clock] timestamping`.
    Timestamping {
        /// Use hardware if the interface supports it, else software.
        Auto = "auto",
        /// Require hardware timestamps.
        Hardware = "hardware",
        /// Always use software timestamps.
        Software = "software",
    }
}

string_enum! {
    /// `[discovery] mdns`.
    MdnsMode {
        /// Use Avahi if present on the bus, else the built-in responder.
        Auto = "auto",
        /// Avahi over D-Bus only.
        Avahi = "avahi",
        /// Built-in responder only.
        Builtin = "builtin",
        /// No mDNS.
        Off = "off",
    }
}

string_enum! {
    /// `[backends] resampler`.
    Resampler {
        /// No resampling.
        Off = "off",
        /// Linear interpolation.
        Linear = "linear",
        /// Cubic interpolation.
        Cubic = "cubic",
        /// Fast sinc.
        SincFast = "sinc-fast",
        /// High-quality sinc.
        SincHq = "sinc-hq",
        /// Choose per platform.
        Auto = "auto",
    }
}

string_enum! {
    /// `[[flow]] format`: the packet format of a static flow. PROVISIONAL:
    /// the only format is the test-only reference format of `tessera-media`
    /// until the real wire format exists (`tessera-proto`).
    FlowFormat {
        /// The provisional reference format (`tessera_media::reference`).
        Reference = "reference",
    }
}

string_enum! {
    /// Audio-client backends that may attach (ADR 0007).
    Backend {
        /// PipeWire client.
        Pipewire = "pipewire",
        /// ALSA plugin and bridge.
        Alsa = "alsa",
        /// JACK client.
        Jack = "jack",
    }
}

string_enum! {
    /// Device values that can be pinned by `locked` (ADR 0008). The names are
    /// the configuration spellings: note `latency`, not `latency_us`.
    DeviceLock {
        /// The device name.
        Name = "name",
        /// The sample rate.
        SampleRate = "sample_rate",
        /// The wire encoding.
        Encoding = "encoding",
        /// The latency.
        Latency = "latency",
        /// All RX channel labels.
        RxChannelLabels = "rx_channel_labels",
        /// All TX channel labels.
        TxChannelLabels = "tx_channel_labels",
        /// The RX channel count (`SetDeviceChannels`).
        RxChannels = "rx_channels",
        /// The TX channel count (`SetDeviceChannels`).
        TxChannels = "tx_channels",
    }
}

string_enum! {
    /// `[audio] socket`: whether to listen on `audio.sock`.
    AudioSocket {
        /// Listen if `[backends] enabled` names at least one client backend.
        Auto = "auto",
        /// Always listen.
        On = "on",
        /// Never listen.
        Off = "off",
    }
}

string_enum! {
    /// `rx_keepalive_payload`: what a receiver keepalive datagram carries.
    /// PROVISIONAL (wire-facing).
    KeepalivePayload {
        /// A zero-length datagram (what hardware receivers send).
        Empty = "empty",
        /// The two bytes `13 37`.
        Bytes1337 = "1337",
    }
}

string_enum! {
    /// `[control] locked`: host-owned runtime domains (ADR 0015).
    ControlLock {
        /// Configuration owns flows; controller flow commands are refused.
        Flows = "flows",
    }
}

string_enum! {
    /// `[network] locked` entries: the network values that config can pin
    /// (ADR 0012).
    NetworkLock {
        /// The primary interface: the first of `interfaces` (or the automatic
        /// choice), never a runtime selection.
        Primary = "primary",
    }
}

string_enum! {
    /// `[clock] locked` entries: the clock values that config can pin.
    ClockLock {
        /// The clock role.
        Role = "role",
        /// The leader-election priority.
        Priority = "priority",
    }
}

string_enum! {
    /// `[[bridge]] direction`. The older spellings `rx` (= `playback`) and
    /// `tx` (= `capture`) are accepted on input.
    BridgeDirection {
        /// Network to card: the device's RX channels are played on the card.
        Playback = "playback",
        /// Card to network: the card's input feeds the device's TX channels.
        Capture = "capture",
    }
}

impl BridgeDirection {
    /// The direction of the device channels the bridge uses.
    #[must_use]
    pub fn device_direction(self) -> Direction {
        match self {
            Self::Playback => Direction::Rx,
            Self::Capture => Direction::Tx,
        }
    }

    /// Parse the configuration spelling, including the aliases `rx`/`tx`.
    #[must_use]
    pub fn parse_with_aliases(s: &str) -> Option<Self> {
        match s {
            "rx" => Some(Self::Playback),
            "tx" => Some(Self::Capture),
            other => Self::parse(other),
        }
    }
}

/// `[engine] tick_us`: `"auto"` or a number of microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickUs {
    /// The daemon's default tick (1000 us; ADR 0004).
    Auto,
    /// Fixed tick.
    Micros(u32),
}

impl Serialize for TickUs {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => s.serialize_str("auto"),
            Self::Micros(n) => s.serialize_u32(*n),
        }
    }
}

/// `[daemon]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaemonConfig {
    /// `error|warn|info|debug|trace` or a `target=level,…` filter.
    pub log_level: String,
    /// Where `state.toml` lives (unless the caller overrides the path).
    pub state_dir: PathBuf,
    /// Runtime directory for sockets.
    pub runtime_dir: PathBuf,
    /// Group allowed to run mutating CLI commands.
    pub admin_group: String,
    /// Seconds allowed for the final state flush and shutdown.
    pub shutdown_timeout_secs: u64,
    /// Debounce window for writing `state.toml`, milliseconds.
    pub persist_delay_ms: u64,
    /// Request real-time scheduling and lock memory as `[engine]` says;
    /// `false` never does (`--no-rt`).
    pub realtime: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            log_level: "info".into(),
            state_dir: "/var/lib/tessera".into(),
            runtime_dir: "/run/tessera".into(),
            admin_group: "tessera-admin".into(),
            shutdown_timeout_secs: 20,
            persist_delay_ms: 500,
            realtime: true,
        }
    }
}

/// `[control]`: the local control socket (ADR 0008, ADR 0010).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ControlConfig {
    /// Runtime domains owned by the host configuration.
    pub locked: BTreeSet<ControlLock>,
    /// Largest request or response frame, bytes.
    pub max_frame_bytes: u64,
    /// Most simultaneous control connections.
    pub max_connections: u32,
    /// Most control connections one uid may hold.
    pub max_connections_per_uid: u32,
    /// Connection slots only administrators may use.
    pub admin_reserved_connections: u32,
    /// Seconds a request/response connection may stay silent.
    pub idle_timeout_secs: u64,
    /// Events queued per subscriber before the oldest are dropped.
    pub event_queue: u32,
    /// Requests a peer may send back to back.
    pub rate_burst: u32,
    /// Requests per second a peer may sustain.
    pub rate_per_second: u32,
    /// Seconds a peer may take to finish a frame.
    pub frame_timeout_secs: u64,
    /// Seconds to wait for a peer that does not read its responses.
    pub write_timeout_secs: u64,
    /// Seconds to wait for a stalled event subscriber.
    pub stream_write_timeout_secs: u64,
    /// Worker threads of the control-plane runtime.
    pub threads: u32,
    /// Group (name or number) that owns the sockets; `None`: `tessera` if
    /// that group exists (a warning otherwise), while a group given here
    /// that cannot be applied is an error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket_group: Option<String>,
    /// Permission bits of the sockets (written as the octal string
    /// `"0o660"`, so that a rendered configuration loads back).
    #[serde(serialize_with = "serialize_mode")]
    pub socket_mode: u32,
}

fn serialize_mode<S: Serializer>(m: &u32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("0o{m:o}"))
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            locked: BTreeSet::new(),
            max_frame_bytes: 1 << 20,
            max_connections: 256,
            max_connections_per_uid: 16,
            admin_reserved_connections: 16,
            idle_timeout_secs: 60,
            event_queue: 256,
            rate_burst: 256,
            rate_per_second: 128,
            frame_timeout_secs: 10,
            write_timeout_secs: 5,
            stream_write_timeout_secs: 30,
            threads: 2,
            socket_group: None,
            socket_mode: 0o660,
        }
    }
}

/// `[audio]`: audio clients on `audio.sock` (ADR 0007).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioConfig {
    /// Listen on `audio.sock`.
    pub socket: AudioSocket,
    /// Most simultaneous audio client connections.
    pub max_clients: u32,
    /// Most audio client connections one uid may hold.
    pub max_clients_per_uid: u32,
    /// Client period assumed when a client does not state one, frames.
    pub default_period_frames: u32,
    /// Largest period a client may ask for, frames.
    pub max_period_frames: u32,
    /// Ring memory all sessions together may reserve, MiB.
    pub ring_budget_mib: u64,
    /// Largest single ring, MiB.
    pub max_ring_mib: u64,
    /// Reorder window added to ring capacity, microseconds.
    pub reorder_window_us: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            socket: AudioSocket::Auto,
            max_clients: 64,
            max_clients_per_uid: 8,
            default_period_frames: 1024,
            max_period_frames: 16_384,
            ring_budget_mib: 256,
            max_ring_mib: 64,
            reorder_window_us: 10_000,
        }
    }
}

/// `[defaults.flow]`: defaults for every `[[flow]]` (each entry can
/// override them). Applied when a flow (re)starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlowDefaults {
    /// Receiver keepalive interval, milliseconds; 0 disables them.
    pub rx_keepalive_ms: u32,
    /// Stop keepalives after this long without media, milliseconds; 0:
    /// never stop.
    pub rx_keepalive_timeout_ms: u32,
    /// What a keepalive carries.
    pub rx_keepalive_payload: KeepalivePayload,
}

impl Default for FlowDefaults {
    fn default() -> Self {
        Self {
            rx_keepalive_ms: 250,
            rx_keepalive_timeout_ms: 10_000,
            rx_keepalive_payload: KeepalivePayload::Empty,
        }
    }
}

/// Smallest `error_backoff_min_ms` (a safety floor): a card that keeps
/// failing must not be reopened, and its real-time bridge thread restarted,
/// in a tight loop.
pub const MIN_BRIDGE_BACKOFF_MS: u32 = 10;
/// Largest `error_backoff_max_ms` (10 minutes).
pub const MAX_BRIDGE_BACKOFF_MS: u32 = 600_000;
/// Smallest `error_sleep_min_ms` (a safety floor): the bridge thread always
/// sleeps after an xrun or card error, so a failing card cannot make a
/// `SCHED_FIFO` thread spin.
pub const MIN_BRIDGE_ERROR_SLEEP_MS: u32 = 1;
/// Largest `error_sleep_max_ms`: longer and the bridge would miss whole
/// card buffers while it recovers.
pub const MAX_BRIDGE_ERROR_SLEEP_MS: u32 = 1_000;
/// Default `error_sleep_min_ms` (the 2 ms of the ADR 0004 amendment).
pub const DEFAULT_BRIDGE_ERROR_SLEEP_MIN_MS: u32 = 2;
/// Default `error_sleep_max_ms` (the 64 ms of the ADR 0004 amendment).
pub const DEFAULT_BRIDGE_ERROR_SLEEP_MAX_MS: u32 = 64;

/// `[defaults.bridge]`: defaults for every `[[bridge]]` (each entry can
/// override them). Two different waits, both only after an error, never in
/// steady state: `error_backoff*` is the bridge manager's wait before it
/// reopens a failed card; `error_sleep*` is the bridge thread's own pause
/// after an xrun or card error while the card stays open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BridgeDefaults {
    /// Double the wait after each failed reopen (up to the maximum);
    /// `false`: always wait `error_backoff_min_ms` (never "no wait").
    pub error_backoff: bool,
    /// First wait before reopening a failed card, milliseconds.
    pub error_backoff_min_ms: u32,
    /// Longest wait, milliseconds.
    pub error_backoff_max_ms: u32,
    /// Double the bridge thread's pause on each further step with an error
    /// (up to the maximum); `false`: always pause `error_sleep_min_ms`
    /// (never "no pause").
    pub error_sleep: bool,
    /// The bridge thread's first pause after an xrun or card error,
    /// milliseconds.
    pub error_sleep_min_ms: u32,
    /// Its longest pause, milliseconds.
    pub error_sleep_max_ms: u32,
}

impl Default for BridgeDefaults {
    fn default() -> Self {
        Self {
            error_backoff: true,
            error_backoff_min_ms: 500,
            error_backoff_max_ms: 30_000,
            error_sleep: true,
            error_sleep_min_ms: DEFAULT_BRIDGE_ERROR_SLEEP_MIN_MS,
            error_sleep_max_ms: DEFAULT_BRIDGE_ERROR_SLEEP_MAX_MS,
        }
    }
}

/// `[defaults]`: per-entry defaults of the array sections. (A `[bridge]`
/// table cannot sit next to `[[bridge]]` entries in TOML, hence the extra
/// level.)
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct DefaultsConfig {
    /// `[defaults.flow]`.
    pub flow: FlowDefaults,
    /// `[defaults.bridge]`.
    pub bridge: BridgeDefaults,
}

/// The receiver keepalive of one flow, resolved against `[defaults.flow]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keepalive {
    /// Interval, milliseconds; 0: off.
    pub interval_ms: u32,
    /// Stop after this long without media, milliseconds; 0: never.
    pub timeout_ms: u32,
    /// Payload.
    pub payload: KeepalivePayload,
}

/// An error wait of one bridge (the manager's reopen back-off or the bridge
/// thread's pause), resolved against `[defaults.bridge]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Doubling on.
    pub doubling: bool,
    /// First wait, milliseconds.
    pub min_ms: u32,
    /// Longest wait, milliseconds.
    pub max_ms: u32,
}

impl Backoff {
    /// The wait after `failures` consecutive failed reopens (1 for the
    /// first): `min_ms` doubled `failures - 1` times, capped at `max_ms`;
    /// always `min_ms` without doubling. Never below `min_ms`.
    #[must_use]
    pub fn wait_ms(&self, failures: u32) -> u32 {
        if !self.doubling {
            return self.min_ms;
        }
        let shift = failures.saturating_sub(1).min(31);
        u64::from(self.min_ms)
            .checked_shl(shift)
            .map_or(self.max_ms, |w| {
                u32::try_from(w.min(u64::from(self.max_ms))).unwrap_or(self.max_ms)
            })
            .max(self.min_ms)
    }
}

/// `[network]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkConfig {
    /// Interfaces by name; empty means "first wired interface with carrier".
    pub interfaces: Vec<String>,
    /// Redundancy interface (Phase 6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary: Option<String>,
    /// Allow Wi-Fi interfaces for media.
    pub allow_wireless: bool,
    /// Maximum media payload per packet in bytes.
    pub mtu_payload: u32,
    /// Values pinned to the configured ones (ADR 0012).
    pub locked: BTreeSet<NetworkLock>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            interfaces: Vec::new(),
            secondary: None,
            allow_wireless: false,
            mtu_payload: 1400,
            locked: BTreeSet::new(),
        }
    }
}

/// `[clock]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClockConfig {
    /// Default clock role.
    pub role: ClockRole,
    /// Default leader-election priority (lower wins).
    pub priority: u8,
    /// Timestamping mode.
    pub timestamping: Timestamping,
    /// Transmit while not locked.
    pub free_running_tx: bool,
    /// Values pinned to the configured ones (choice of this crate).
    pub locked: BTreeSet<ClockLock>,
    /// Run PTP; `false` leaves the media clock free-running (`--no-ptp`).
    pub enabled: bool,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            role: ClockRole::Follower,
            priority: 128,
            timestamping: Timestamping::Auto,
            free_running_tx: false,
            locked: BTreeSet::new(),
        }
    }
}

/// `[engine]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EngineConfig {
    /// Worker threads.
    pub workers: u32,
    /// Engine tick.
    pub tick_us: TickUs,
    /// `SCHED_FIFO` priority of the media threads.
    pub rt_priority: u8,
    /// `SCHED_FIFO` priority of the clock thread.
    pub ptp_rt_priority: u8,
    /// CPUs to pin RT threads to.
    pub cpu_affinity: Vec<u32>,
    /// `mlockall` once the real-time objects are allocated (ADR 0004); a
    /// refusal is reported in the status, never fatal.
    pub mlock: bool,
    /// Datagrams per `recvmmsg` call.
    pub rx_batch: u32,
    /// Receive batches per socket per tick.
    pub rx_max_batches: u32,
    /// Datagrams per `sendmmsg` call.
    pub tx_batch: u32,
    /// Packets per transmit flow per tick.
    pub tx_max_per_tick: u32,
    /// Kernel socket buffer of media sockets, KiB.
    pub socket_buffer_kib: u32,
    /// Margin added to the receive latency published to audio clients,
    /// microseconds.
    pub rx_margin_us: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            workers: 1,
            tick_us: TickUs::Auto,
            rt_priority: 80,
            ptp_rt_priority: 70,
            cpu_affinity: Vec::new(),
            mlock: true,
            rx_batch: 32,
            rx_max_batches: 8,
            tx_batch: 32,
            tx_max_per_tick: 32,
            socket_buffer_kib: 2048,
            rx_margin_us: 250,
        }
    }
}

/// `[discovery]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveryConfig {
    /// mDNS implementation.
    pub mdns: MdnsMode,
    /// Advertise the logical devices (ARC, CMC and channel records) with
    /// mDNS. Enabled by default with the packaged Controller runtime.
    pub advertise_devices: bool,
    /// Discovery at all; `false` advertises nothing whatever
    /// `advertise_devices` says (`--no-discovery`).
    pub enabled: bool,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mdns: MdnsMode::Auto,
            advertise_devices: true,
        }
    }
}

/// `[backends]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackendsConfig {
    /// Clients allowed to attach.
    pub enabled: Vec<Backend>,
    /// Resampler choice.
    pub resampler: Resampler,
}

impl Default for BackendsConfig {
    fn default() -> Self {
        Self {
            enabled: vec![Backend::Pipewire, Backend::Alsa],
            resampler: Resampler::Auto,
        }
    }
}

/// `[aes67]` (Phase 5).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Aes67Config {
    /// Enable AES67.
    pub enabled: bool,
}

/// `[device.multicast]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MulticastConfig {
    /// May this device transmit multicast flows.
    pub allow_tx_multicast: bool,
}

impl Default for MulticastConfig {
    fn default() -> Self {
        Self {
            allow_tx_multicast: true,
        }
    }
}

fn serialize_labels<S: Serializer>(m: &BTreeMap<u16, String>, s: S) -> Result<S::Ok, S::Error> {
    // TOML keys are strings: `{ 1 = "Vox" }`.
    s.collect_map(m.iter().map(|(k, v)| (k.to_string(), v)))
}

/// One `[[device]]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceConfig {
    /// Local key; the wire device ID is generated separately.
    pub id: String,
    /// Device name (default: derived from the host name).
    pub name: String,
    /// Number of RX channels.
    pub rx_channels: u16,
    /// Number of TX channels.
    pub tx_channels: u16,
    /// Default sample rate in Hz.
    pub sample_rate: u32,
    /// Default wire encoding in bits.
    pub encoding: u8,
    /// Default latency in microseconds.
    pub latency_us: u32,
    /// Inclusive `[min, max]` the latency may take.
    pub latency_bounds_us: [u32; 2],
    /// Downgrade the platform-floor check on latency to a warning.
    pub allow_unsafe_latency: bool,
    /// Values pinned to the configured ones.
    pub locked: BTreeSet<DeviceLock>,
    /// Default RX channel labels.
    #[serde(serialize_with = "serialize_labels")]
    pub rx_channel_labels: BTreeMap<u16, String>,
    /// Default TX channel labels.
    #[serde(serialize_with = "serialize_labels")]
    pub tx_channel_labels: BTreeMap<u16, String>,
    /// Multicast settings.
    pub multicast: MulticastConfig,
}

/// Default `[[bridge]] time_constant_s`: a slow, gentle loop, so that it
/// cannot fight a second servo downstream (a DSP stage that trims a
/// loopback's rate). The rule is at least ten times the downstream loop's
/// adjustment period; CamillaDSP's default `adjust_period` is 10 s.
pub const DEFAULT_BRIDGE_TIME_CONSTANT_S: f64 = 120.0;
/// Default `[[bridge]] max_correction_ppm`.
pub const DEFAULT_BRIDGE_MAX_CORRECTION_PPM: f64 = 200.0;
/// Largest `[[bridge]] max_correction_ppm`: the range the bridge's resampler
/// and PI loop are built for (`tessera-backend-alsa` `MAX_CORRECTION`; a
/// test in `tesserad`, which depends on both, keeps them equal).
pub const MAX_BRIDGE_CORRECTION_PPM: f64 = 3000.0;
/// Time constant of the low-pass on the bridge loop's measured error,
/// seconds (`tessera-backend-alsa` `ERROR_FILTER_TAU`, kept equal by the same
/// test). A loop time constant below ten times it is a warning: the extra
/// pole then costs phase margin and the loop may ring.
pub const BRIDGE_ERROR_FILTER_TAU_S: f64 = 0.4;
/// Largest loop transient a `[[bridge]]` may have, seconds
/// (`tessera-backend-alsa` `MAX_TRANSIENT_S`, kept equal by a compile-time
/// check in `tesserad`): `0.4 * max_correction_ppm * 1e-6 * time_constant_s`
/// at most 60 s. The guard and lead grow by this much latency (and the ring
/// with them), so a time constant mistyped in milliseconds is refused.
pub const MAX_BRIDGE_TRANSIENT_S: f64 = 60.0;
/// Default `[[bridge]] rt_priority` (ADR 0004: "backend bridge", 75).
pub const DEFAULT_BRIDGE_RT_PRIORITY: u8 = 75;

/// One `[[bridge]]`: an in-daemon ALSA hardware-card bridge (ADR 0007 item
/// 4, ADR 0006). The card is opened at the device's rate and moves audio
/// between the card and one contiguous channel range of the device, with a
/// resampler and a PI loop absorbing the drift between the card's clock and
/// the media clock.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BridgeConfig {
    /// Name of the bridge (letters, digits, `_`, `-`; unique). Default:
    /// `<device>-<direction>`.
    pub name: String,
    /// Run it (`false` keeps the entry but leaves the card alone).
    pub enabled: bool,
    /// The `device.id` this bridge belongs to.
    pub device: String,
    /// ALSA PCM name (`hw:CARD=<card>,DEV=0`, `hw:Loopback,0,0`).
    pub alsa_pcm: String,
    /// `playback` (network to card) or `capture` (card to network).
    pub direction: BridgeDirection,
    /// First device channel carried (1-based); card channel 1 is this one.
    pub first_channel: u16,
    /// Channels carried; `None`: the rest of the device's channels in that
    /// direction from `first_channel` on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    /// The card's rate; must equal the device's (the bridge compensates
    /// drift, not rate differences). `None`: the device's rate, following
    /// its changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// Frames per card period; `None`: 10 ms at the device's rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period_frames: Option<u32>,
    /// Frames of the card buffer (at least two periods); `None`: four
    /// periods. The bridge keeps all but one period of it filled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer_frames: Option<u32>,
    /// Resampler quality; `auto` takes `[backends] resampler` (and its
    /// `auto`, the platform default). `off` disables drift compensation.
    pub resampler: Resampler,
    /// Time constant of the drift loop, seconds (`1 / (2 pi f_n)`).
    pub time_constant_s: f64,
    /// Largest correction the loop may apply, ppm.
    pub max_correction_ppm: f64,
    /// `SCHED_FIFO` priority of the bridge thread; 0: normal scheduling.
    pub rt_priority: u8,
    /// CPU to pin the bridge thread to; `None`: not pinned (the scheduler
    /// places it, so it does not share the media worker's core by default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
    /// Override of `[defaults.bridge] error_backoff`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_backoff: Option<bool>,
    /// Override of `[defaults.bridge] error_backoff_min_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_backoff_min_ms: Option<u32>,
    /// Override of `[defaults.bridge] error_backoff_max_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_backoff_max_ms: Option<u32>,
    /// Override of `[defaults.bridge] error_sleep`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_sleep: Option<bool>,
    /// Override of `[defaults.bridge] error_sleep_min_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_sleep_min_ms: Option<u32>,
    /// Override of `[defaults.bridge] error_sleep_max_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_sleep_max_ms: Option<u32>,
    /// A `channels = [..]` list (the older form) that is not one ascending
    /// run of channels, kept for validation to report; `None` for every
    /// usable configuration (a run is stored as `first_channel` + count).
    #[serde(skip)]
    pub channel_list: Option<Vec<u16>>,
}

impl BridgeConfig {
    /// A bridge with every optional key at its default.
    #[must_use]
    pub fn new(device: &str, alsa_pcm: &str, direction: BridgeDirection) -> Self {
        Self {
            name: default_bridge_name(device, direction),
            enabled: true,
            device: device.to_owned(),
            alsa_pcm: alsa_pcm.to_owned(),
            direction,
            first_channel: 1,
            channels: None,
            sample_rate: None,
            period_frames: None,
            buffer_frames: None,
            resampler: Resampler::Auto,
            time_constant_s: DEFAULT_BRIDGE_TIME_CONSTANT_S,
            max_correction_ppm: DEFAULT_BRIDGE_MAX_CORRECTION_PPM,
            rt_priority: DEFAULT_BRIDGE_RT_PRIORITY,
            cpu: None,
            error_backoff: None,
            error_backoff_min_ms: None,
            error_backoff_max_ms: None,
            error_sleep: None,
            error_sleep_min_ms: None,
            error_sleep_max_ms: None,
            channel_list: None,
        }
    }

    /// The error back-off, this entry's overrides over `defaults`.
    #[must_use]
    pub fn backoff(&self, defaults: &BridgeDefaults) -> Backoff {
        Backoff {
            doubling: self.error_backoff.unwrap_or(defaults.error_backoff),
            min_ms: self
                .error_backoff_min_ms
                .unwrap_or(defaults.error_backoff_min_ms),
            max_ms: self
                .error_backoff_max_ms
                .unwrap_or(defaults.error_backoff_max_ms),
        }
    }

    /// The bridge thread's pause after an xrun or card error, this entry's
    /// overrides over `defaults`.
    #[must_use]
    pub fn error_sleep(&self, defaults: &BridgeDefaults) -> Backoff {
        Backoff {
            doubling: self.error_sleep.unwrap_or(defaults.error_sleep),
            min_ms: self
                .error_sleep_min_ms
                .unwrap_or(defaults.error_sleep_min_ms),
            max_ms: self
                .error_sleep_max_ms
                .unwrap_or(defaults.error_sleep_max_ms),
        }
    }

    /// The channel count, resolved against a device with `available`
    /// channels in the bridge's direction.
    #[must_use]
    pub fn channel_count(&self, available: u16) -> u16 {
        self.channels
            .unwrap_or_else(|| available.saturating_sub(self.first_channel.saturating_sub(1)))
    }

    /// The period, resolved against the device rate.
    #[must_use]
    pub fn period(&self, rate: u32) -> u32 {
        self.period_frames.unwrap_or((rate / 100).max(32))
    }

    /// The buffer, resolved against the device rate.
    #[must_use]
    pub fn buffer(&self, rate: u32) -> u32 {
        self.buffer_frames
            .unwrap_or_else(|| self.period(rate).saturating_mul(4))
    }
}

/// The name a `[[bridge]]` gets when it sets none: `<device>-<direction>`,
/// with every character outside the name rules (letters, digits, `_`, `-`)
/// replaced by `_`, so a device id with other characters still gives a
/// valid name.
#[must_use]
pub fn default_bridge_name(device: &str, direction: BridgeDirection) -> String {
    let device: String = device
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{device}-{direction}")
}

/// One `[[flow]]`: a **PROVISIONAL** static audio flow, tessera-only test
/// scaffolding. It exists so that two `tesserad` instances can exchange audio
/// before the Dante flow-control protocols (ARC/CMC) are implemented; it is
/// replaced by flow control once lab captures exist. Everything about the
/// packets stays behind the media crate's `PacketFormat` trait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlowConfig {
    /// Unique name of the flow (a local key, never on the wire).
    pub id: String,
    /// `rx` (network to device) or `tx` (device to network).
    pub direction: Direction,
    /// The `device.id` the flow belongs to.
    pub device: String,
    /// Device channels (1-based) carried, in wire order. Empty means every
    /// channel of the device in that direction.
    pub channels: Vec<u16>,
    /// `tx`: the destination address (required; may be multicast). `rx`:
    /// accept packets only from this address (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<Ipv4Addr>,
    /// `tx`: the destination UDP port; `rx`: the local UDP port to listen on.
    pub port: u16,
    /// Interface to use (default: the first of `[network] interfaces`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    /// Local IPv4 address to bind (default: the interface's first address).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_address: Option<Ipv4Addr>,
    /// Packet format.
    pub format: FlowFormat,
    /// `tx`: frames per packet (default: the packet period of ADR 0011,
    /// about 667 us, rounded to whole frames).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames_per_packet: Option<u32>,
    /// `tx`: extra delay after a packet's last frame before it is sent
    /// (`tx_send_offset`, ADR 0006), microseconds.
    pub send_offset_us: u32,
    /// `rx`: accept datagrams from any source address. Off by default: an rx
    /// flow accepts only the sender named in `peer`, so that another host on
    /// the segment cannot inject audio. (Receive from a multicast group is
    /// not supported yet.)
    pub any_source: bool,
    /// `tx`: DSCP of the media packets (default 46, EF, `docs/lab.md`).
    pub dscp: u8,
    /// `tx`: how late (after its release time) a packet may still be sent
    /// before the flow skips forward instead of catching up, microseconds
    /// (default: half the device latency, at least one packet period). Raise it on hosts whose scheduler wakes
    /// the engine late now and then, as far as the receivers' latency allows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_lag_us: Option<u32>,
    /// `rx`: override of `[defaults.flow] rx_keepalive_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_keepalive_ms: Option<u32>,
    /// `rx`: override of `[defaults.flow] rx_keepalive_timeout_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_keepalive_timeout_ms: Option<u32>,
    /// `rx`: override of `[defaults.flow] rx_keepalive_payload`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_keepalive_payload: Option<KeepalivePayload>,
}

impl FlowConfig {
    /// The receiver keepalive, this entry's overrides over `defaults`.
    #[must_use]
    pub fn keepalive(&self, defaults: &FlowDefaults) -> Keepalive {
        Keepalive {
            interval_ms: self.rx_keepalive_ms.unwrap_or(defaults.rx_keepalive_ms),
            timeout_ms: self
                .rx_keepalive_timeout_ms
                .unwrap_or(defaults.rx_keepalive_timeout_ms),
            payload: self
                .rx_keepalive_payload
                .unwrap_or(defaults.rx_keepalive_payload),
        }
    }
}

/// The whole merged configuration.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Config {
    /// `[daemon]`.
    pub daemon: DaemonConfig,
    /// `[control]`.
    pub control: ControlConfig,
    /// `[audio]`.
    pub audio: AudioConfig,
    /// `[network]`.
    pub network: NetworkConfig,
    /// `[clock]`.
    pub clock: ClockConfig,
    /// `[engine]`.
    pub engine: EngineConfig,
    /// `[discovery]`.
    pub discovery: DiscoveryConfig,
    /// `[backends]`.
    pub backends: BackendsConfig,
    /// `[aes67]`.
    pub aes67: Aes67Config,
    /// `[defaults]`: per-entry defaults of `[[flow]]` and `[[bridge]]`.
    pub defaults: DefaultsConfig,
    /// `[[device]]`. Never empty after loading: an empty configuration gets
    /// one default device.
    #[serde(rename = "device")]
    pub devices: Vec<DeviceConfig>,
    /// `[[bridge]]`.
    #[serde(rename = "bridge")]
    pub bridges: Vec<BridgeConfig>,
    /// `[[flow]]` (PROVISIONAL static flows).
    #[serde(rename = "flow")]
    pub flows: Vec<FlowConfig>,
    /// Transitional controller-file flows; omitted from rendered host config.
    /// The effective model includes them only before a protocol set is stored.
    #[serde(skip)]
    pub legacy_protocol_flows: Vec<FlowConfig>,
}

impl Config {
    /// Render as TOML (every key explicit). Loading the result gives back an
    /// equal [`Config`].
    ///
    /// # Errors
    /// The serialiser's message (a non-UTF-8 path, for example).
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string(self).map_err(|e| e.to_string())
    }

    /// The device with this key.
    #[must_use]
    pub fn device(&self, key: &str) -> Option<&DeviceConfig> {
        self.devices.iter().find(|d| d.id == key)
    }
}

/// The name a device gets when the configuration does not give one: the host
/// name (shortened at a character boundary to the name limit); with several
/// devices, `<host>-<id>` so they stay distinct.
#[must_use]
pub fn default_device_name(env: &Environment, id: &str, multi: bool) -> String {
    let max = env.max_name_bytes;
    let base = truncate_name(&env.hostname, max);
    let base = if base.is_empty() {
        "tessera".to_owned()
    } else {
        base
    };
    if !multi {
        return base;
    }
    let suffix = format!("-{id}");
    if suffix.len() >= max {
        return truncate_name(&format!("{base}{suffix}"), max);
    }
    format!("{}{suffix}", truncate_name(&base, max - suffix.len()))
}

// ---------------------------------------------------------------------------
// Extraction: toml tables -> typed structs, collecting all problems.
// ---------------------------------------------------------------------------

/// Reads keys out of one TOML table, reporting type errors as it goes and
/// unknown leftovers (with the list of valid keys) when dropped.
struct Reader<'a> {
    table: Table,
    path: String,
    known: Vec<&'static str>,
    issues: &'a mut Vec<Issue>,
}

impl<'a> Reader<'a> {
    fn new(table: Table, path: String, issues: &'a mut Vec<Issue>) -> Self {
        Self {
            table,
            path,
            known: Vec::new(),
            issues,
        }
    }

    fn join(&self, key: &str) -> String {
        join_path(&self.path, key)
    }

    fn error(&mut self, key: &str, msg: impl Into<String>) {
        let p = self.join(key);
        self.issues.push(Issue::error(p, msg));
    }

    fn take(&mut self, key: &'static str) -> Option<Value> {
        self.known.push(key);
        self.table.remove(key)
    }

    fn type_error(&mut self, key: &str, expected: &str, found: &Value) {
        self.error(
            key,
            format!("expected {expected}, found {}", found.type_str()),
        );
    }

    fn required_string(&mut self, key: &'static str) -> Option<String> {
        if !self.table.contains_key(key) {
            self.known.push(key);
            self.error(key, format!("missing required key `{key}`"));
            return None;
        }
        self.string(key)
    }

    fn string(&mut self, key: &'static str) -> Option<String> {
        match self.take(key)? {
            Value::String(s) => Some(s),
            other => {
                self.type_error(key, "a string", &other);
                None
            }
        }
    }

    fn boolean(&mut self, key: &'static str) -> Option<bool> {
        match self.take(key)? {
            Value::Boolean(b) => Some(b),
            other => {
                self.type_error(key, "a boolean", &other);
                None
            }
        }
    }

    fn uint_value(&mut self, key: &str, v: &Value, max: u64) -> Option<u64> {
        match v {
            Value::Integer(i) => match u64::try_from(*i) {
                Ok(n) if n <= max => Some(n),
                Ok(n) => {
                    self.error(key, format!("{n} is too large, the maximum is {max}"));
                    None
                }
                Err(_) => {
                    self.error(key, format!("{i} is negative, expected 0 or more"));
                    None
                }
            },
            other => {
                self.type_error(key, "an integer", other);
                None
            }
        }
    }

    /// A number: an integer or a float (`60` or `60.5`).
    fn number(&mut self, key: &'static str) -> Option<f64> {
        match self.take(key)? {
            Value::Float(f) => Some(f),
            #[allow(clippy::cast_precision_loss)]
            Value::Integer(i) => Some(i as f64),
            other => {
                self.type_error(key, "a number", &other);
                None
            }
        }
    }

    fn uint(&mut self, key: &'static str, max: u64) -> Option<u64> {
        let v = self.take(key)?;
        self.uint_value(key, &v, max)
    }

    fn u32(&mut self, key: &'static str) -> Option<u32> {
        self.uint(key, u64::from(u32::MAX))
            .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
    }

    fn u64(&mut self, key: &'static str) -> Option<u64> {
        self.uint(key, u64::MAX)
    }

    /// `rx_keepalive_payload`: `"empty"` or `"1337"`; the integer `1337`
    /// is accepted too (it is what people type).
    fn payload(&mut self, key: &'static str) -> Option<KeepalivePayload> {
        match self.take(key)? {
            Value::Integer(1337) => Some(KeepalivePayload::Bytes1337),
            Value::String(s) => {
                let v = KeepalivePayload::parse(&s);
                if v.is_none() {
                    self.error(
                        key,
                        format!(
                            "unknown value `{s}`, valid values: {}",
                            KeepalivePayload::names()
                        ),
                    );
                }
                v
            }
            other => {
                self.type_error(key, "\"empty\" or \"1337\"", &other);
                None
            }
        }
    }

    /// A string parsed with `parse`; a bad one is reported with the valid
    /// spellings.
    fn enumerated<T>(
        &mut self,
        key: &'static str,
        parse: fn(&str) -> Option<T>,
        valid: &str,
    ) -> Option<T> {
        let s = self.string(key)?;
        let v = parse(&s);
        if v.is_none() {
            self.error(key, format!("unknown value `{s}`, valid values: {valid}"));
        }
        v
    }

    /// Permission bits: an octal integer (TOML `0o660`) or an octal string
    /// (`"660"`, `"0660"`, `"0o660"`: octal digits only, no sign). That an
    /// integer in a file is written in octal is checked by the loader (the
    /// parsed value no longer knows); command-line overrides pass the
    /// parsed number.
    fn mode(&mut self, key: &'static str) -> Option<u32> {
        match self.take(key)? {
            Value::String(s) => {
                let digits = s.strip_prefix("0o").unwrap_or(&s);
                let octal =
                    !digits.is_empty() && digits.bytes().all(|b| (b'0'..=b'7').contains(&b));
                match octal.then(|| u32::from_str_radix(digits, 8).ok()).flatten() {
                    Some(m) => Some(m),
                    None => {
                        self.error(key, format!("`{s}` is not an octal mode"));
                        None
                    }
                }
            }
            v @ Value::Integer(_) => self
                .uint_value(key, &v, u64::from(u32::MAX))
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
            other => {
                self.type_error(key, "an octal mode (0o660 or \"660\")", &other);
                None
            }
        }
    }

    fn array(&mut self, key: &'static str) -> Option<Vec<Value>> {
        match self.take(key)? {
            Value::Array(a) => Some(a),
            other => {
                self.type_error(key, "an array", &other);
                None
            }
        }
    }

    fn string_list(&mut self, key: &'static str) -> Option<Vec<String>> {
        let items = self.array(key)?;
        let mut out = Vec::new();
        for (i, v) in items.into_iter().enumerate() {
            match v {
                Value::String(s) => out.push(s),
                other => self.type_error(&format!("{key}[{i}]"), "a string", &other),
            }
        }
        Some(out)
    }

    fn uint_list(&mut self, key: &'static str, max: u64) -> Option<Vec<u64>> {
        let items = self.array(key)?;
        let mut out = Vec::new();
        for (i, v) in items.iter().enumerate() {
            if let Some(n) = self.uint_value(&format!("{key}[{i}]"), v, max) {
                out.push(n);
            }
        }
        Some(out)
    }

    /// A list of strings, each parsed with `parse`; bad entries are reported
    /// with the valid spellings.
    fn enum_list<T>(
        &mut self,
        key: &'static str,
        parse: fn(&str) -> Option<T>,
        valid: &str,
    ) -> Option<Vec<T>> {
        let items = self.string_list_indexed(key)?;
        let mut out = Vec::new();
        for (i, s) in items {
            match parse(&s) {
                Some(v) => out.push(v),
                None => self.error(
                    &format!("{key}[{i}]"),
                    format!("unknown value `{s}`, valid values: {valid}"),
                ),
            }
        }
        Some(out)
    }

    fn string_list_indexed(&mut self, key: &'static str) -> Option<Vec<(usize, String)>> {
        let items = self.array(key)?;
        let mut out = Vec::new();
        for (i, v) in items.into_iter().enumerate() {
            match v {
                Value::String(s) => out.push((i, s)),
                other => self.type_error(&format!("{key}[{i}]"), "a string", &other),
            }
        }
        Some(out)
    }

    fn table(&mut self, key: &'static str) -> Option<Table> {
        match self.take(key)? {
            Value::Table(t) => Some(t),
            other => {
                self.type_error(key, "a table", &other);
                None
            }
        }
    }

    /// An array of tables, with the path of each element.
    fn tables(&mut self, key: &'static str) -> Vec<(String, Table)> {
        let Some(items) = self.array(key) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (i, v) in items.into_iter().enumerate() {
            match v {
                Value::Table(t) => out.push((self.join(&format!("{key}[{i}]")), t)),
                other => self.type_error(&format!("{key}[{i}]"), "a table", &other),
            }
        }
        out
    }

    fn sub(&mut self, path: String, table: Table) -> Reader<'_> {
        Reader::new(table, path, &mut *self.issues)
    }

    fn labels(&mut self, key: &'static str) -> Option<BTreeMap<u16, String>> {
        let t = self.table(key)?;
        let mut out = BTreeMap::new();
        for (k, v) in t {
            let kp = format!("{key}.{k}");
            // Canonical decimal only ("1", not "01" or "+1"): two spellings of
            // one channel would make the merge result depend on key order.
            let Some(n) = k
                .parse::<u16>()
                .ok()
                .filter(|n| *n >= 1 && n.to_string() == k)
            else {
                self.error(
                    &kp,
                    "channel numbers are integers from 1 to 65535 written without sign or leading zeros (key must be written like `1 = \"Name\"`)",
                );
                continue;
            };
            match v {
                Value::String(s) => {
                    out.insert(n, s);
                }
                other => self.type_error(&kp, "a string", &other),
            }
        }
        Some(out)
    }

    /// Run `f` on the sub-table `key` (if present) filling `target`.
    fn section<T>(
        &mut self,
        key: &'static str,
        target: &mut T,
        f: impl FnOnce(&mut Reader<'_>, &mut T),
    ) {
        if let Some(t) = self.table(key) {
            let path = self.join(key);
            let mut r = self.sub(path, t);
            f(&mut r, target);
        }
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        if self.table.is_empty() {
            return;
        }
        let mut valid: Vec<&str> = self.known.clone();
        valid.sort_unstable();
        valid.dedup();
        let valid = valid.join(", ");
        let keys: Vec<String> = self.table.keys().cloned().collect();
        for k in keys {
            let p = join_path(&self.path, &k);
            let msg = if valid.is_empty() {
                "unknown key".to_owned()
            } else {
                format!("unknown key, valid keys here: {valid}")
            };
            self.issues.push(Issue::error(p, msg));
        }
    }
}

pub(crate) fn join_path(base: &str, key: &str) -> String {
    if base.is_empty() {
        key.to_owned()
    } else if key.starts_with('[') {
        format!("{base}{key}")
    } else {
        format!("{base}.{key}")
    }
}

/// Turn a merged table into a [`Config`], collecting every type error and
/// unknown key. The returned config is only meaningful if no error is
/// reported.
pub(crate) fn extract(table: Table, env: &Environment) -> (Config, Vec<Issue>) {
    let mut issues = Vec::new();
    let mut cfg = Config::default();
    {
        let mut root = Reader::new(table, String::new(), &mut issues);
        root.section("daemon", &mut cfg.daemon, |r, d| {
            if let Some(v) = r.string("log_level") {
                d.log_level = v;
            }
            if let Some(v) = r.string("state_dir") {
                d.state_dir = v.into();
            }
            if let Some(v) = r.string("runtime_dir") {
                d.runtime_dir = v.into();
            }
            if let Some(v) = r.string("admin_group") {
                d.admin_group = v;
            }
            if let Some(v) = r.u64("shutdown_timeout_secs") {
                d.shutdown_timeout_secs = v;
            }
            if let Some(v) = r.u64("persist_delay_ms") {
                d.persist_delay_ms = v;
            }
            if let Some(v) = r.boolean("realtime") {
                d.realtime = v;
            }
        });
        root.section("control", &mut cfg.control, |r, c| {
            if let Some(v) = r.enum_list("locked", ControlLock::parse, &ControlLock::names()) {
                c.locked = v.into_iter().collect();
            }
            if let Some(v) = r.u64("max_frame_bytes") {
                c.max_frame_bytes = v;
            }
            for (key, slot) in [
                ("max_connections", &mut c.max_connections),
                ("max_connections_per_uid", &mut c.max_connections_per_uid),
                (
                    "admin_reserved_connections",
                    &mut c.admin_reserved_connections,
                ),
                ("event_queue", &mut c.event_queue),
                ("rate_burst", &mut c.rate_burst),
                ("rate_per_second", &mut c.rate_per_second),
                ("threads", &mut c.threads),
            ] {
                if let Some(v) = r.u32(key) {
                    *slot = v;
                }
            }
            for (key, slot) in [
                ("idle_timeout_secs", &mut c.idle_timeout_secs),
                ("frame_timeout_secs", &mut c.frame_timeout_secs),
                ("write_timeout_secs", &mut c.write_timeout_secs),
                (
                    "stream_write_timeout_secs",
                    &mut c.stream_write_timeout_secs,
                ),
            ] {
                if let Some(v) = r.u64(key) {
                    *slot = v;
                }
            }
            if let Some(v) = r.string("socket_group") {
                c.socket_group = Some(v);
            }
            if let Some(v) = r.mode("socket_mode") {
                c.socket_mode = v;
            }
        });
        root.section("audio", &mut cfg.audio, |r, a| {
            if let Some(v) = r.enumerated("socket", AudioSocket::parse, &AudioSocket::names()) {
                a.socket = v;
            }
            for (key, slot) in [
                ("max_clients", &mut a.max_clients),
                ("max_clients_per_uid", &mut a.max_clients_per_uid),
                ("default_period_frames", &mut a.default_period_frames),
                ("max_period_frames", &mut a.max_period_frames),
                ("reorder_window_us", &mut a.reorder_window_us),
            ] {
                if let Some(v) = r.u32(key) {
                    *slot = v;
                }
            }
            if let Some(v) = r.u64("ring_budget_mib") {
                a.ring_budget_mib = v;
            }
            if let Some(v) = r.u64("max_ring_mib") {
                a.max_ring_mib = v;
            }
        });
        root.section("network", &mut cfg.network, |r, n| {
            if let Some(v) = r.string_list("interfaces") {
                n.interfaces = v;
            }
            if let Some(v) = r.string("secondary") {
                n.secondary = Some(v);
            }
            if let Some(v) = r.boolean("allow_wireless") {
                n.allow_wireless = v;
            }
            if let Some(v) = r.uint("mtu_payload", u64::from(u32::MAX)) {
                n.mtu_payload = u32::try_from(v).unwrap_or(u32::MAX);
            }
            if let Some(v) = r.enum_list("locked", NetworkLock::parse, &NetworkLock::names()) {
                n.locked = v.into_iter().collect();
            }
        });
        root.section("clock", &mut cfg.clock, |r, c| {
            if let Some(v) = r.string("role") {
                match v.as_str() {
                    "follower" => c.role = ClockRole::Follower,
                    "leader" => c.role = ClockRole::Leader,
                    "auto" => c.role = ClockRole::Auto,
                    other => r.error(
                        "role",
                        format!("unknown value `{other}`, valid values: follower, leader, auto"),
                    ),
                }
            }
            if let Some(v) = r.uint("priority", 255) {
                c.priority = u8::try_from(v).unwrap_or(u8::MAX);
            }
            if let Some(v) = r.string("timestamping") {
                match Timestamping::parse(&v) {
                    Some(m) => c.timestamping = m,
                    None => r.error(
                        "timestamping",
                        format!(
                            "unknown value `{v}`, valid values: {}",
                            Timestamping::names()
                        ),
                    ),
                }
            }
            if let Some(v) = r.boolean("free_running_tx") {
                c.free_running_tx = v;
            }
            if let Some(v) = r.enum_list("locked", ClockLock::parse, &ClockLock::names()) {
                c.locked = v.into_iter().collect();
            }
            if let Some(v) = r.boolean("enabled") {
                c.enabled = v;
            }
        });
        root.section("engine", &mut cfg.engine, |r, e| {
            if let Some(v) = r.uint("workers", u64::from(u32::MAX)) {
                e.workers = u32::try_from(v).unwrap_or(u32::MAX);
            }
            if let Some(v) = r.take("tick_us") {
                match v {
                    Value::String(s) if s == "auto" => e.tick_us = TickUs::Auto,
                    Value::Integer(_) => {
                        if let Some(n) = r.uint_value("tick_us", &v, u64::from(u32::MAX)) {
                            e.tick_us = TickUs::Micros(u32::try_from(n).unwrap_or(u32::MAX));
                        }
                    }
                    other => r.error(
                        "tick_us",
                        format!(
                            "expected \"auto\" or an integer number of microseconds, found {}",
                            other.type_str()
                        ),
                    ),
                }
            }
            if let Some(v) = r.uint("rt_priority", 255) {
                e.rt_priority = u8::try_from(v).unwrap_or(u8::MAX);
            }
            if let Some(v) = r.uint("ptp_rt_priority", 255) {
                e.ptp_rt_priority = u8::try_from(v).unwrap_or(u8::MAX);
            }
            if let Some(v) = r.uint_list("cpu_affinity", u64::from(u32::MAX)) {
                e.cpu_affinity = v
                    .into_iter()
                    .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
                    .collect();
            }
            if let Some(v) = r.boolean("mlock") {
                e.mlock = v;
            }
            for (key, slot) in [
                ("rx_batch", &mut e.rx_batch),
                ("rx_max_batches", &mut e.rx_max_batches),
                ("tx_batch", &mut e.tx_batch),
                ("tx_max_per_tick", &mut e.tx_max_per_tick),
                ("socket_buffer_kib", &mut e.socket_buffer_kib),
                ("rx_margin_us", &mut e.rx_margin_us),
            ] {
                if let Some(v) = r.u32(key) {
                    *slot = v;
                }
            }
        });
        root.section("discovery", &mut cfg.discovery, |r, d| {
            if let Some(v) = r.string("mdns") {
                match MdnsMode::parse(&v) {
                    Some(m) => d.mdns = m,
                    None => r.error(
                        "mdns",
                        format!("unknown value `{v}`, valid values: {}", MdnsMode::names()),
                    ),
                }
            }
            if let Some(v) = r.boolean("advertise_devices") {
                d.advertise_devices = v;
            }
            if let Some(v) = r.boolean("enabled") {
                d.enabled = v;
            }
        });
        root.section("backends", &mut cfg.backends, |r, b| {
            if let Some(v) = r.enum_list("enabled", Backend::parse, &Backend::names()) {
                b.enabled = v;
            }
            if let Some(v) = r.string("resampler") {
                match Resampler::parse(&v) {
                    Some(m) => b.resampler = m,
                    None => r.error(
                        "resampler",
                        format!("unknown value `{v}`, valid values: {}", Resampler::names()),
                    ),
                }
            }
        });
        root.section("aes67", &mut cfg.aes67, |r, a| {
            if let Some(v) = r.boolean("enabled") {
                a.enabled = v;
            }
        });
        root.section("defaults", &mut cfg.defaults, |r, d| {
            r.section("flow", &mut d.flow, |r, f| {
                if let Some(v) = r.u32("rx_keepalive_ms") {
                    f.rx_keepalive_ms = v;
                }
                if let Some(v) = r.u32("rx_keepalive_timeout_ms") {
                    f.rx_keepalive_timeout_ms = v;
                }
                if let Some(v) = r.payload("rx_keepalive_payload") {
                    f.rx_keepalive_payload = v;
                }
            });
            r.section("bridge", &mut d.bridge, |r, b| {
                if let Some(v) = r.boolean("error_backoff") {
                    b.error_backoff = v;
                }
                if let Some(v) = r.u32("error_backoff_min_ms") {
                    b.error_backoff_min_ms = v;
                }
                if let Some(v) = r.u32("error_backoff_max_ms") {
                    b.error_backoff_max_ms = v;
                }
                if let Some(v) = r.boolean("error_sleep") {
                    b.error_sleep = v;
                }
                if let Some(v) = r.u32("error_sleep_min_ms") {
                    b.error_sleep_min_ms = v;
                }
                if let Some(v) = r.u32("error_sleep_max_ms") {
                    b.error_sleep_max_ms = v;
                }
            });
        });

        let device_tables = root.tables("device");
        let multi = device_tables.len() > 1;
        for (path, t) in device_tables {
            let mut r = root.sub(path, t);
            cfg.devices.push(extract_device(&mut r, env, multi));
        }
        for (path, t) in root.tables("bridge") {
            let mut r = root.sub(path, t);
            cfg.bridges.push(extract_bridge(&mut r));
        }
        for (path, t) in root.tables("flow") {
            let mut r = root.sub(path, t);
            cfg.flows.push(extract_flow(&mut r));
        }
    }
    if cfg.devices.is_empty() && issues.is_empty() {
        cfg.devices.push(default_device(env, "main", false));
    }
    (cfg, issues)
}

fn default_device(env: &Environment, id: &str, multi: bool) -> DeviceConfig {
    DeviceConfig {
        id: id.to_owned(),
        name: default_device_name(env, id, multi),
        rx_channels: 2,
        tx_channels: 2,
        sample_rate: 48_000,
        encoding: 24,
        latency_us: env.default_latency_us,
        latency_bounds_us: env.default_latency_bounds_us,
        allow_unsafe_latency: false,
        locked: BTreeSet::new(),
        rx_channel_labels: BTreeMap::new(),
        tx_channel_labels: BTreeMap::new(),
        multicast: MulticastConfig::default(),
    }
}

fn extract_device(r: &mut Reader<'_>, env: &Environment, multi: bool) -> DeviceConfig {
    let id = r.required_string("id");
    let mut d = default_device(env, id.as_deref().unwrap_or(""), multi);
    if let Some(id) = id {
        d.id = id;
    }
    if let Some(v) = r.string("name") {
        d.name = v;
    }
    if let Some(v) = r.uint("rx_channels", u64::from(u16::MAX)) {
        d.rx_channels = u16::try_from(v).unwrap_or(u16::MAX);
    }
    if let Some(v) = r.uint("tx_channels", u64::from(u16::MAX)) {
        d.tx_channels = u16::try_from(v).unwrap_or(u16::MAX);
    }
    if let Some(v) = r.uint("sample_rate", u64::from(u32::MAX)) {
        d.sample_rate = u32::try_from(v).unwrap_or(u32::MAX);
    }
    if let Some(v) = r.uint("encoding", 255) {
        d.encoding = u8::try_from(v).unwrap_or(u8::MAX);
    }
    if let Some(v) = r.uint("latency_us", u64::from(u32::MAX)) {
        d.latency_us = u32::try_from(v).unwrap_or(u32::MAX);
    }
    if let Some(v) = r.uint_list("latency_bounds_us", u64::from(u32::MAX)) {
        if let [lo, hi] = v[..] {
            d.latency_bounds_us = [
                u32::try_from(lo).unwrap_or(u32::MAX),
                u32::try_from(hi).unwrap_or(u32::MAX),
            ];
        } else {
            r.error(
                "latency_bounds_us",
                format!("expected exactly two numbers [min, max], found {}", v.len()),
            );
        }
    }
    if let Some(v) = r.boolean("allow_unsafe_latency") {
        d.allow_unsafe_latency = v;
    }
    if let Some(v) = r.enum_list("locked", DeviceLock::parse, &DeviceLock::names()) {
        d.locked = v.into_iter().collect();
    }
    if let Some(v) = r.labels("rx_channel_labels") {
        d.rx_channel_labels = v;
    }
    if let Some(v) = r.labels("tx_channel_labels") {
        d.tx_channel_labels = v;
    }
    r.section("multicast", &mut d.multicast, |r, m| {
        if let Some(v) = r.boolean("allow_tx_multicast") {
            m.allow_tx_multicast = v;
        }
    });
    d
}

fn extract_bridge(r: &mut Reader<'_>) -> BridgeConfig {
    let mut b = BridgeConfig::new("", "", BridgeDirection::Playback);
    if let Some(v) = r.required_string("device") {
        b.device = v;
    }
    if let Some(v) = r.required_string("alsa_pcm") {
        b.alsa_pcm = v;
    }
    if let Some(v) = r.string("direction") {
        match BridgeDirection::parse_with_aliases(&v) {
            Some(d) => b.direction = d,
            None => r.error(
                "direction",
                format!(
                    "unknown value `{v}`, valid values: {} (or rx, tx)",
                    BridgeDirection::names()
                ),
            ),
        }
    }
    b.name = r
        .string("name")
        .unwrap_or_else(|| default_bridge_name(&b.device, b.direction));
    if let Some(v) = r.boolean("enabled") {
        b.enabled = v;
    }
    let first = r.uint("first_channel", u64::from(u16::MAX));
    if let Some(v) = first {
        b.first_channel = u16::try_from(v).unwrap_or(u16::MAX);
    }
    match r.take("channels") {
        None => {}
        Some(Value::Integer(i)) => {
            if let Some(n) = r.uint_value("channels", &Value::Integer(i), u64::from(u16::MAX)) {
                b.channels = Some(u16::try_from(n).unwrap_or(u16::MAX));
            }
        }
        // The older form: a list of device channels, which must be one
        // ascending run (card channel i is device channel first + i).
        Some(Value::Array(items)) => {
            let mut list = Vec::new();
            for (i, v) in items.iter().enumerate() {
                if let Some(n) = r.uint_value(&format!("channels[{i}]"), v, u64::from(u16::MAX)) {
                    list.push(u16::try_from(n).unwrap_or(u16::MAX));
                }
            }
            if first.is_some() {
                r.error(
                    "channels",
                    "a channel list cannot be combined with `first_channel`; write `first_channel = N` and `channels = <count>`",
                );
            } else if list.first().is_some_and(|f| *f >= 1)
                && list.windows(2).all(|w| w[1] == w[0].wrapping_add(1))
            {
                b.first_channel = list[0];
                b.channels = Some(u16::try_from(list.len()).unwrap_or(u16::MAX));
            } else {
                b.channel_list = Some(list);
            }
        }
        Some(other) => r.type_error(
            "channels",
            "a channel count (or a list of channels)",
            &other,
        ),
    }
    if let Some(v) = r.uint("sample_rate", u64::from(u32::MAX)) {
        b.sample_rate = Some(u32::try_from(v).unwrap_or(u32::MAX));
    }
    if let Some(v) = r.uint("period_frames", u64::from(u32::MAX)) {
        b.period_frames = Some(u32::try_from(v).unwrap_or(u32::MAX));
    }
    if let Some(v) = r.uint("buffer_frames", u64::from(u32::MAX)) {
        b.buffer_frames = Some(u32::try_from(v).unwrap_or(u32::MAX));
    }
    if let Some(v) = r.string("resampler") {
        match Resampler::parse(&v) {
            Some(m) => b.resampler = m,
            None => r.error(
                "resampler",
                format!("unknown value `{v}`, valid values: {}", Resampler::names()),
            ),
        }
    }
    if let Some(v) = r.number("time_constant_s") {
        b.time_constant_s = v;
    }
    if let Some(v) = r.number("max_correction_ppm") {
        b.max_correction_ppm = v;
    }
    if let Some(v) = r.uint("cpu", u64::from(u32::MAX)) {
        b.cpu = Some(u32::try_from(v).unwrap_or(u32::MAX));
    }
    if let Some(v) = r.uint("rt_priority", 255) {
        b.rt_priority = u8::try_from(v).unwrap_or(u8::MAX);
    }
    b.error_backoff = r.boolean("error_backoff");
    b.error_backoff_min_ms = r.u32("error_backoff_min_ms");
    b.error_backoff_max_ms = r.u32("error_backoff_max_ms");
    b.error_sleep = r.boolean("error_sleep");
    b.error_sleep_min_ms = r.u32("error_sleep_min_ms");
    b.error_sleep_max_ms = r.u32("error_sleep_max_ms");
    b
}

fn ipv4(r: &mut Reader<'_>, key: &'static str) -> Option<Ipv4Addr> {
    let s = r.string(key)?;
    match s.parse::<Ipv4Addr>() {
        Ok(a) => Some(a),
        Err(_) => {
            r.error(key, format!("`{s}` is not an IPv4 address"));
            None
        }
    }
}

fn extract_flow(r: &mut Reader<'_>) -> FlowConfig {
    let mut f = FlowConfig {
        id: String::new(),
        direction: Direction::Rx,
        device: String::new(),
        channels: Vec::new(),
        peer: None,
        port: 0,
        interface: None,
        local_address: None,
        format: FlowFormat::Reference,
        frames_per_packet: None,
        send_offset_us: 0,
        any_source: false,
        dscp: 46,
        max_lag_us: None,
        rx_keepalive_ms: None,
        rx_keepalive_timeout_ms: None,
        rx_keepalive_payload: None,
    };
    if let Some(v) = r.required_string("id") {
        f.id = v;
    }
    if let Some(v) = r.required_string("direction") {
        match v.as_str() {
            "rx" => f.direction = Direction::Rx,
            "tx" => f.direction = Direction::Tx,
            other => r.error(
                "direction",
                format!("unknown value `{other}`, valid values: rx, tx"),
            ),
        }
    }
    if let Some(v) = r.required_string("device") {
        f.device = v;
    }
    if let Some(v) = r.uint_list("channels", u64::from(u16::MAX)) {
        f.channels = v
            .into_iter()
            .map(|n| u16::try_from(n).unwrap_or(u16::MAX))
            .collect();
    }
    f.peer = ipv4(r, "peer");
    if !r.table.contains_key("port") {
        r.known.push("port");
        r.error("port", "missing required key `port`");
    } else if let Some(v) = r.uint("port", u64::from(u16::MAX)) {
        f.port = u16::try_from(v).unwrap_or(u16::MAX);
    }
    if let Some(v) = r.string("interface") {
        f.interface = Some(v);
    }
    f.local_address = ipv4(r, "local_address");
    if let Some(v) = r.string("format") {
        match FlowFormat::parse(&v) {
            Some(m) => f.format = m,
            None => r.error(
                "format",
                format!("unknown value `{v}`, valid values: {}", FlowFormat::names()),
            ),
        }
    }
    if let Some(v) = r.uint("frames_per_packet", u64::from(u32::MAX)) {
        f.frames_per_packet = Some(u32::try_from(v).unwrap_or(u32::MAX));
    }
    if let Some(v) = r.uint("send_offset_us", u64::from(u32::MAX)) {
        f.send_offset_us = u32::try_from(v).unwrap_or(u32::MAX);
    }
    if let Some(v) = r.boolean("any_source") {
        f.any_source = v;
    }
    if let Some(v) = r.uint("dscp", 63) {
        f.dscp = u8::try_from(v).unwrap_or(63);
    }
    if let Some(v) = r.uint("max_lag_us", u64::from(u32::MAX)) {
        f.max_lag_us = Some(u32::try_from(v).unwrap_or(u32::MAX));
    }
    f.rx_keepalive_ms = r.u32("rx_keepalive_ms");
    f.rx_keepalive_timeout_ms = r.u32("rx_keepalive_timeout_ms");
    f.rx_keepalive_payload = r.payload("rx_keepalive_payload");
    f
}
