//! Command-line grammar of `tesseractl`.

use std::num::NonZeroU16;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use tessera_control_types::{
    AccessPin, BridgeDirection, CardBridgeSpec, Channel, ChannelSet, ClockRole, Command,
    Crosspoint, CrosspointKey, DeviceKey, Direction, Encoding, GAIN_CEILING_MDB, GAIN_FLOOR_MDB,
    Gain, LatencyUs, RouterPort, SampleRate,
};

/// Default location of the control socket.
pub const DEFAULT_SOCKET: &str = "/run/tessera/control.sock";

/// `tesseractl [OPTIONS] <COMMAND>`.
#[derive(Debug, Parser)]
#[command(
    name = "tesseractl",
    version = tessera_control_types::BUILD_VERSION,
    about = "Control the tessera daemon (tesserad)",
    after_help = "Exit codes: 0 ok, 1 error (also: a change that restarts something was not confirmed), 2 usage, 3 cannot connect, 4 permission denied.\n\
                  Changing settings needs root or membership of the daemon's admin group \
                  (tessera-admin by default); reading status does not."
)]
pub struct Cli {
    /// Control socket of the daemon.
    #[arg(short, long, global = true, env = "TESSERA_SOCKET", default_value = DEFAULT_SOCKET)]
    pub socket: PathBuf,

    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    pub json: bool,

    /// Logical device key; optional when the daemon has exactly one device.
    #[arg(short, long, global = true, env = "TESSERA_DEVICE")]
    pub device: Option<String>,

    /// Seconds to wait for the daemon to answer (0 = forever).
    #[arg(long, global = true, default_value_t = 10)]
    pub timeout: u64,

    /// Largest response frame accepted from the daemon, in bytes.
    #[arg(long, global = true, default_value_t = crate::client::DEFAULT_MAX_RESPONSE_BYTES)]
    pub max_response_bytes: usize,

    /// Apply a change that restarts something without asking.
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,

    /// What to do.
    #[command(subcommand)]
    pub command: Cmd,
}

/// Which side of a device a channel is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Dir {
    /// Receive channels (from the network).
    Rx,
    /// Transmit channels (to the network).
    Tx,
}

impl From<Dir> for Direction {
    fn from(d: Dir) -> Self {
        match d {
            Dir::Rx => Direction::Rx,
            Dir::Tx => Direction::Tx,
        }
    }
}

/// Clock role names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Role {
    /// Follow a leader.
    Follower,
    /// Be the leader.
    Leader,
    /// Leader-capable but follower-preferred.
    Auto,
}

impl From<Role> for ClockRole {
    fn from(r: Role) -> Self {
        match r {
            Role::Follower => ClockRole::Follower,
            Role::Leader => ClockRole::Leader,
            Role::Auto => ClockRole::Auto,
        }
    }
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Show devices, subscriptions, clocks and warnings.
    Status,
    /// Show the daemon's counters and gauges.
    Metrics,
    /// Inspect discovered endpoints, channels and receiver-reported selections.
    #[command(subcommand)]
    Routing(RoutingCmd),
    /// Show the in-daemon hardware-card bridges (`[[bridge]]`).
    Bridges,
    /// Sound-card bridges: list them, add one (a card's inputs to TX
    /// channels, or RX channels to its outputs) or remove one.
    #[command(subcommand)]
    Bridge(BridgeCmd),
    /// Local monitoring: also play local inputs (TX channels) on this
    /// device's own outputs (RX channels).
    #[command(
        after_help = "Examples: tesseractl monitor main 1,2 --delay-ms 20   (RX 1 plays TX 1, RX 2 plays TX 2)\n          \
                            tesseractl monitor main 0,0,1,2   (RX 3-4 play TX 1-2)\n          \
                            tesseractl monitor main off\n\
                            Without --delay-ms the current delay is kept; --delay-ms auto (or 0) is the lowest safe delay."
    )]
    Monitor {
        /// Logical device key (as in `status`).
        #[arg(value_name = "DEVICE")]
        device_key: String,
        /// For RX channel 1, 2, ...: the TX channel it also plays, comma
        /// separated (0: none), or `off`.
        #[arg(value_name = "RX_FROM_TX", value_parser = parse_monitor)]
        rx_from_tx: MonitorMap,
        /// How far behind playback the inputs are read, milliseconds, or
        /// `auto` (0, the default for a new monitor): the lowest safe delay.
        #[arg(long, value_parser = parse_delay_ms)]
        delay_ms: Option<u32>,
    },
    /// Inspect flow counters or replace the controller-managed flow set.
    #[command(subcommand)]
    Flows(FlowCmd),
    /// Stream state-change events until interrupted.
    Watch(WatchArgs),
    /// Rename the device.
    SetName {
        /// The new name (at most 31 bytes).
        name: String,
    },
    /// Set the sample rate.
    SetRate {
        /// Rate in Hz.
        hz: u32,
    },
    /// Set the wire encoding.
    SetEncoding {
        /// Bits per sample (16, 24 or 32).
        bits: u8,
    },
    /// Set the latency.
    SetLatency {
        /// Latency in microseconds.
        micros: u32,
    },
    /// Set the receive and transmit channel counts (restarts the device).
    #[command(
        after_help = "Without --yes: shows what restarts, asks y/N on a terminal and \
                            refuses (exit 1) when there is none. --dry-run only prints the impact."
    )]
    SetChannels {
        /// Number of receive channels.
        #[arg(long)]
        rx: u16,
        /// Number of transmit channels.
        #[arg(long)]
        tx: u16,
        /// Only show what the change would do; change nothing.
        #[arg(long, conflicts_with = "yes")]
        dry_run: bool,
    },
    /// Set or reset a channel label.
    SetLabel {
        /// Receive or transmit channel.
        direction: Dir,
        /// Channel number, from 1.
        channel: NonZeroU16,
        /// The new label.
        #[arg(required_unless_present = "reset", conflicts_with = "reset")]
        label: Option<String>,
        /// Restore the default label.
        #[arg(long)]
        reset: bool,
    },
    /// Subscribe a receive channel to a transmit channel.
    Subscribe {
        /// Local receive channel, from 1.
        rx_channel: NonZeroU16,
        /// Name of the transmit channel on the remote device.
        tx_channel: String,
        /// Name of the remote device.
        tx_device: String,
    },
    /// Remove the subscription on a receive channel.
    Unsubscribe {
        /// Local receive channel, from 1.
        rx_channel: NonZeroU16,
    },
    /// Clock status and settings.
    #[command(subcommand)]
    Clock(ClockCmd),
    /// Network interfaces and the primary interface.
    #[command(subcommand)]
    Network(NetworkCmd),
    /// Configuration.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// The device access lock: freeze the device's configuration and
    /// routing behind a four-digit PIN.
    #[command(subcommand)]
    Access(AccessCmd),
    /// Mute channels. Mute is never taken over by a control surface.
    #[command(after_help = "Example: tesseractl mute main tx 1-2")]
    Mute(ChannelArgs),
    /// Unmute channels.
    #[command(after_help = "Example: tesseractl unmute main tx 1,2")]
    Unmute(ChannelArgs),
    /// Set the gain of channels, in dB (-96 to +12; 0 is unity) or `off`.
    #[command(after_help = "Examples: tesseractl gain main rx 1-2 -6\n          \
                            tesseractl gain main tx 3 off\n\
                            Refused while a connected control surface's fader sets \
                            the gain of a channel; mute still works then.")]
    Gain {
        /// The channels.
        #[command(flatten)]
        target: ChannelArgs,
        /// Gain in dB (for example -6, -6.5 or +3; a `dB` suffix is
        /// allowed), or `off` for silence.
        #[arg(allow_hyphen_values = true, value_parser = parse_gain)]
        gain: Gain,
    },
    /// Set the OUTPUT level of RX channels, in dB (-96 to +12) or `off`:
    /// applied after the local mix, like a master fader. Independent of
    /// `gain` (the channel's input level); never taken over by a surface.
    #[command(after_help = "Example: tesseractl output-gain speaker 1-2 -6")]
    OutputGain {
        /// Logical device key (as in `status`).
        #[arg(value_name = "DEVICE")]
        device_key: String,
        /// RX channels from 1: `1`, `1,2`, `1-2`.
        #[arg(value_parser = parse_channels)]
        channels: Channels,
        /// Level in dB (for example -6 or +3), or `off`.
        #[arg(allow_hyphen_values = true, value_parser = parse_gain)]
        gain: Gain,
    },
    /// Mute the OUTPUT of RX channels (after the local mix), or unmute it
    /// with --off.
    #[command(
        after_help = "Examples: tesseractl output-mute speaker 1-2\n          \
                            tesseractl output-mute speaker 1-2 --off"
    )]
    OutputMute {
        /// Logical device key (as in `status`).
        #[arg(value_name = "DEVICE")]
        device_key: String,
        /// RX channels from 1: `1`, `1,2`, `1-2`.
        #[arg(value_parser = parse_channels)]
        channels: Channels,
        /// Unmute instead.
        #[arg(long)]
        off: bool,
    },
    /// Show the devices the host has (USB HID interfaces, sound cards) and
    /// the configured control surfaces with their live control values.
    Surfaces,
    /// Manage control surfaces (apply JSON specs, add MIDI over TCP, learn
    /// controls, enable, disable, remove); map their controls in the web
    /// portal or the configuration.
    #[command(subcommand)]
    Surface(SurfaceCmd),
    /// The host router: one grid of crosspoints between network channels
    /// and local I/O (application outputs, sound-card inputs and outputs,
    /// the host's own outputs), with port levels.
    #[command(subcommand)]
    Router(RouterCmd),
    /// The host (speaker amplifier) volume (`[host_volume]` in the
    /// configuration).
    #[command(subcommand)]
    Volume(VolumeCmd),
    /// Saved mixer panels (the web portal's mixing desk pages).
    #[command(subcommand)]
    Mixer(MixerCmd),
    /// Print a shell completion script.
    Completions {
        /// The shell.
        shell: Shell,
    },
}

/// Options of `watch`.
#[derive(Debug, Args)]
pub struct WatchArgs {
    /// Stop after this many events.
    #[arg(long)]
    pub count: Option<u64>,
    /// Stop after this many seconds.
    #[arg(long = "for", value_name = "SECONDS")]
    pub for_secs: Option<u64>,
}

/// `tesseractl clock ...`.
#[derive(Debug, Subcommand)]
pub enum ClockCmd {
    /// Show the clock table.
    Show,
    /// Set the clock role.
    SetRole {
        /// The role.
        role: Role,
    },
    /// Set the leader-election priority (lower wins).
    SetPriority {
        /// Priority, 0 to 255.
        priority: u8,
    },
}

/// `tesseractl flows ...`.
#[derive(Debug, Subcommand)]
pub enum FlowCmd {
    /// Show the flow counters also included in metrics.
    Show,
    /// Replace controller flows from a file containing only [[flow]] entries.
    /// Host configuration flows are retained; normal confirmation applies.
    Set {
        /// TOML file containing the complete desired controller flow set.
        file: PathBuf,
    },
    /// Clear controller flows, retaining the host configuration flows.
    Clear,
}

/// `tesseractl network ...`.
#[derive(Debug, Subcommand)]
pub enum NetworkCmd {
    /// Show the primary interface, how it was chosen and the candidates.
    Show,
    /// Select the primary interface; kept across restarts.
    Primary {
        /// Interface name, or `default` (also `auto`) to clear the selection
        /// and use the configured or automatic choice again.
        interface: String,
    },
}

/// `tesseractl access ...`. The PIN is never an argument (other users could
/// read it in the process list): it is typed on the terminal without echo,
/// or read as one line from standard input.
#[derive(Debug, Subcommand)]
pub enum AccessCmd {
    /// Lock the device: changes from any controller, the portal or this
    /// command are refused until it is unlocked. Asks for the PIN twice.
    Lock,
    /// Unlock the device with the PIN it was locked with.
    Unlock,
    /// Unlock without the PIN, for a forgotten PIN (local administrators
    /// only).
    Reset,
}

/// The channels of `mute`, `unmute` and `gain`.
#[derive(Debug, Args)]
pub struct ChannelArgs {
    /// Logical device key (as in `status`).
    #[arg(value_name = "DEVICE")]
    pub device_key: String,
    /// `rx` (network to this host) or `tx` (this host to network).
    pub direction: Dir,
    /// Channels from 1: `1`, `1,2`, `1-4` or `1-2,5`.
    #[arg(value_parser = parse_channels)]
    pub channels: Channels,
}

impl ChannelArgs {
    /// The channel set these arguments name.
    #[must_use]
    pub fn set(&self) -> ChannelSet {
        ChannelSet {
            device: DeviceKey(self.device_key.clone()),
            direction: self.direction.into(),
            channels: self.channels.0.clone(),
        }
    }
}

/// A list of channels, in the order given, without repeats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channels(pub Vec<Channel>);

/// Parse `1`, `1,2`, `1-4`, `1-2,5`: channels from 1, in the order given,
/// each at most once.
///
/// # Errors
/// A message for the command line.
pub fn parse_channels(s: &str) -> Result<Channels, String> {
    let one = |t: &str| -> Result<u16, String> {
        let n: u16 = t
            .trim()
            .parse()
            .map_err(|_| format!("`{}` is not a channel number", t.trim()))?;
        if n == 0 {
            return Err("channels are numbered from 1".into());
        }
        Ok(n)
    };
    let mut out: Vec<Channel> = Vec::new();
    for part in s.split(',') {
        if part.trim().is_empty() {
            return Err(format!("`{s}`: empty channel in the list"));
        }
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (one(a)?, one(b)?),
            None => {
                let n = one(part)?;
                (n, n)
            }
        };
        if b < a {
            return Err(format!("`{}`: a range goes upwards", part.trim()));
        }
        for n in a..=b {
            let c = Channel::new(n).ok_or("channels are numbered from 1")?;
            if out.contains(&c) {
                return Err(format!("channel {n} is named twice"));
            }
            out.push(c);
        }
    }
    Ok(Channels(out))
}

/// Parse a gain: `off`, or dB such as `-6`, `-6.5`, `+3`, `0dB`.
///
/// # Errors
/// A message for the command line: not a number, or outside -96 to +12 dB.
pub fn parse_gain(s: &str) -> Result<Gain, String> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("off") {
        return Ok(Gain::Off);
    }
    let num = t
        .strip_suffix("dB")
        .or_else(|| t.strip_suffix("db"))
        .or_else(|| t.strip_suffix("DB"))
        .unwrap_or(t)
        .trim();
    let db: f64 = num
        .parse()
        .map_err(|_| format!("`{s}` is not a gain: give dB (for example -6) or `off`"))?;
    if !db.is_finite() {
        return Err(format!("`{s}` is not a gain"));
    }
    let mdb = (db * 1000.0).round();
    let range = format!(
        "the gain must be from {} to +{} dB, or `off`",
        GAIN_FLOOR_MDB / 1000,
        GAIN_CEILING_MDB / 1000
    );
    if !(f64::from(GAIN_FLOOR_MDB)..=f64::from(GAIN_CEILING_MDB)).contains(&mdb) {
        return Err(range);
    }
    #[allow(clippy::cast_possible_truncation)]
    let g = Gain::MilliDb(mdb as i32);
    if g.is_valid() { Ok(g) } else { Err(range) }
}

/// `tesseractl surface ...`.
#[derive(Debug, Subcommand)]
pub enum SurfaceCmd {
    /// Add or replace a control surface from a JSON file holding its
    /// saved spec (`{"name", "enabled", "source", "controls", "bindings"}`,
    /// the shape of each `spec` that `surfaces --json` prints).
    /// Administrators only.
    Set {
        /// The JSON file (`-` for standard input).
        #[arg(long, value_name = "FILE", value_parser = parse_surface_file)]
        file: Box<tessera_control_types::SurfaceSpec>,
    },
    /// Remove a configured control surface. Channels whose gain its faders
    /// set can be set from elsewhere again.
    Remove {
        /// The surface's name (see `surfaces`).
        name: String,
    },
    /// Turn a configured surface on again: the daemon opens its device and
    /// its controls drive their mappings. Administrators only.
    #[command(after_help = "Example: tesseractl surface enable cq")]
    Enable {
        /// The surface's name (see `surfaces`).
        name: String,
    },
    /// Turn a configured surface off, keeping its controls and mappings:
    /// the daemon leaves its device alone and it owns no level until it is
    /// enabled again. Administrators only.
    #[command(after_help = "Example: tesseractl surface disable cq")]
    Disable {
        /// The surface's name (see `surfaces`).
        name: String,
    },
    /// Watch a detected device and print the first control that moves (its
    /// suggested HID layout). Administrators only.
    Learn {
        /// The detected device's ID (see `surfaces`), for example
        /// `hid:3142:0c33:3`.
        #[arg(value_name = "DETECTED_ID")]
        id: String,
        /// Seconds to wait for a control to move (the daemon allows 30).
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=30))]
        wait: u32,
        /// You will move a fader (from the bottom all the way up) rather than
        /// press a button: tells an inverted fader.
        #[arg(long)]
        fader: bool,
    },
    /// Read a surface over raw MIDI on a TCP connection tessera opens to
    /// HOST:PORT (an Allen & Heath CQ offers it on 51325). Sets the source
    /// of surface NAME, or adds NAME with no controls yet (map them in the
    /// web portal or with `surface set`). Tessera only connects out.
    #[command(after_help = "Example: tesseractl surface tcp-midi cq 192.168.1.40")]
    TcpMidi {
        /// The surface's name.
        name: String,
        /// The desk's host name or address.
        host: String,
        /// Its TCP port.
        #[arg(default_value_t = tessera_control_types::CQ_TCP_MIDI_PORT)]
        port: u16,
    },
}

/// `tesseractl router ...`.
#[derive(Debug, Subcommand)]
#[command(
    after_help = "Ports: rx:DEVICE:N (network RX), tx:DEVICE:N (network TX), \
                        app:DEVICE:N (what applications play), host:DEVICE:N (what \
                        applications capture: the speaker), in:CARD:N and out:CARD:N \
                        (sound-card inputs and outputs; CARD is the bridge's ALSA PCM, \
                        e.g. in:hw:CARD=CQ,DEV=0:15)."
)]
pub enum RouterCmd {
    /// Show the ports and the grid in effect.
    Show,
    /// Patch SOURCE into DESTINATION (a network TX channel, a host output or
    /// a card output); a destination as a source is a tap of what it carries.
    #[command(after_help = "Example: tesseractl router patch in:hw:CARD=CQ,DEV=0:15 host:main:1")]
    Patch {
        /// Source port.
        #[arg(value_parser = parse_port)]
        source: RouterPort,
        /// Destination port.
        #[arg(value_parser = parse_port)]
        destination: RouterPort,
        /// The crosspoint's gain in dB (default 0), or `off`.
        #[arg(long, allow_hyphen_values = true, value_parser = parse_gain)]
        gain: Option<Gain>,
        /// Patch it muted.
        #[arg(long)]
        muted: bool,
    },
    /// Remove a crosspoint.
    Unpatch {
        /// Source port.
        #[arg(value_parser = parse_port)]
        source: RouterPort,
        /// Destination port.
        #[arg(value_parser = parse_port)]
        destination: RouterPort,
    },
    /// Set the gain of a patched crosspoint (dB or `off`).
    Gain {
        /// Source port.
        #[arg(value_parser = parse_port)]
        source: RouterPort,
        /// Destination port.
        #[arg(value_parser = parse_port)]
        destination: RouterPort,
        /// Gain in dB, or `off`.
        #[arg(allow_hyphen_values = true, value_parser = parse_gain)]
        gain: Gain,
    },
    /// Mute a patched crosspoint, or unmute it with --off.
    Mute {
        /// Source port.
        #[arg(value_parser = parse_port)]
        source: RouterPort,
        /// Destination port.
        #[arg(value_parser = parse_port)]
        destination: RouterPort,
        /// Unmute instead.
        #[arg(long)]
        off: bool,
    },
    /// Set the level of a port (dB or `off`). Network ports are the channel
    /// gains, host outputs the output levels.
    PortGain {
        /// The port.
        #[arg(value_parser = parse_port)]
        port: RouterPort,
        /// Level in dB, or `off`.
        #[arg(allow_hyphen_values = true, value_parser = parse_gain)]
        gain: Gain,
    },
    /// Mute a port, or unmute it with --off.
    PortMute {
        /// The port.
        #[arg(value_parser = parse_port)]
        port: RouterPort,
        /// Unmute instead.
        #[arg(long)]
        off: bool,
    },
    /// Label a local port (an application output, a card input or output)
    /// or a host output; without LABEL it gets its default label again.
    /// Network channels are labelled with `set-label`.
    #[command(after_help = "Example: tesseractl router label in:hw:CARD=CQ,DEV=0:15 \"CQ Main L\"")]
    Label {
        /// The port.
        #[arg(value_parser = parse_port)]
        port: RouterPort,
        /// The new label (1 to 31 ASCII characters, no `=`, `.` or `@`).
        label: Option<String>,
    },
    /// Replace the whole saved grid with a JSON file holding a router spec
    /// (`{"crosspoints": [...], "labels": [...]}`).
    Set {
        /// The JSON file (`-` for standard input).
        #[arg(long, value_name = "FILE", value_parser = parse_router_file)]
        file: Box<tessera_control_types::RouterSpec>,
    },
    /// Go back to the default grid (identity, monitoring, configured routes).
    Reset,
}

/// `tesseractl volume ...`.
#[derive(Debug, Subcommand)]
pub enum VolumeCmd {
    /// Show the host volume: saved value, what the mixer control reads.
    Show,
    /// Set the host volume in dB (within its configured range) or `off`.
    Set {
        /// Volume in dB, or `off`.
        #[arg(allow_hyphen_values = true, value_parser = parse_gain)]
        gain: Gain,
    },
    /// Mute the host volume, or unmute it with --off.
    Mute {
        /// Unmute instead.
        #[arg(long)]
        off: bool,
    },
}

/// `tesseractl mixer ...`.
#[derive(Debug, Subcommand)]
pub enum MixerCmd {
    /// List the saved mixer panels.
    List,
    /// Add or replace a panel from a JSON file holding a mixer spec
    /// (`{"id", "title", "strips"}`, the shape `mixer list --json` prints).
    Set {
        /// The JSON file (`-` for standard input).
        #[arg(long, value_name = "FILE", value_parser = parse_mixer_file)]
        file: Box<tessera_control_types::MixerSpec>,
    },
    /// Remove a saved panel.
    Remove {
        /// The panel's id.
        id: String,
    },
}

/// Parse a router port: `rx:DEVICE:N`, `tx:DEVICE:N`, `app:DEVICE:N`,
/// `host:DEVICE:N`, `in:CARD:N`, `out:CARD:N` (the JSON kinds `network_rx`,
/// `network_tx`, `app_out`, `host_out`, `card_in`, `card_out` work too).
///
/// # Errors
/// A message for the command line.
pub fn parse_port(s: &str) -> Result<RouterPort, String> {
    let bad = || {
        format!(
            "`{s}` is not a port: give KIND:DEVICE:N (rx, tx, app, host) or KIND:CARD:N (in, out)"
        )
    };
    let (kind, rest) = s.split_once(':').ok_or_else(bad)?;
    let (name, n) = rest.rsplit_once(':').ok_or_else(bad)?;
    if name.is_empty() {
        return Err(bad());
    }
    let n: u16 = n.parse().map_err(|_| bad())?;
    let channel = Channel::new(n).ok_or("channels are numbered from 1")?;
    let device = || DeviceKey(name.to_owned());
    Ok(match kind {
        "rx" | "network_rx" => RouterPort::NetworkRx {
            device: device(),
            channel,
        },
        "tx" | "network_tx" => RouterPort::NetworkTx {
            device: device(),
            channel,
        },
        "app" | "app_out" => RouterPort::AppOut {
            device: device(),
            channel,
        },
        "host" | "host_out" => RouterPort::HostOut {
            device: device(),
            channel,
        },
        "in" | "card_in" => RouterPort::CardIn {
            card: name.to_owned(),
            channel,
        },
        "out" | "card_out" => RouterPort::CardOut {
            card: name.to_owned(),
            channel,
        },
        _ => return Err(bad()),
    })
}

/// A port in the form [`parse_port`] reads.
#[must_use]
pub fn port_words(p: &RouterPort) -> String {
    let (kind, name) = match p {
        RouterPort::NetworkRx { device, .. } => ("rx", device.0.as_str()),
        RouterPort::NetworkTx { device, .. } => ("tx", device.0.as_str()),
        RouterPort::AppOut { device, .. } => ("app", device.0.as_str()),
        RouterPort::HostOut { device, .. } => ("host", device.0.as_str()),
        RouterPort::CardIn { card, .. } => ("in", card.as_str()),
        RouterPort::CardOut { card, .. } => ("out", card.as_str()),
        _ => ("?", ""),
    };
    format!("{kind}:{name}:{}", p.channel().get())
}

fn read_json_file(path: &str) -> Result<String, String> {
    if path == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).map_err(|e| e.to_string())?;
        Ok(s)
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
    }
}

/// Read a router spec from a JSON file (`-`: standard input).
fn parse_router_file(path: &str) -> Result<Box<tessera_control_types::RouterSpec>, String> {
    serde_json::from_str(&read_json_file(path)?)
        .map(Box::new)
        .map_err(|e| format!("{path}: not a router spec: {e}"))
}

/// Read a mixer spec from a JSON file (`-`: standard input).
fn parse_mixer_file(path: &str) -> Result<Box<tessera_control_types::MixerSpec>, String> {
    serde_json::from_str(&read_json_file(path)?)
        .map(Box::new)
        .map_err(|e| format!("{path}: not a mixer spec: {e}"))
}

/// Read a surface spec from a JSON file (`-`: standard input).
fn parse_surface_file(path: &str) -> Result<Box<tessera_control_types::SurfaceSpec>, String> {
    let text = if path == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).map_err(|e| e.to_string())?;
        s
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?
    };
    serde_json::from_str(&text)
        .map(Box::new)
        .map_err(|e| format!("{path}: not a surface spec: {e}"))
}

/// `tesseractl bridge ...`.
#[derive(Debug, Subcommand)]
pub enum BridgeCmd {
    /// Show the bridges and their live state (as `bridges`).
    List,
    /// Add a sound-card bridge, or replace the one with this name. Bridges
    /// of the configuration files (`[[bridge]]`) cannot be replaced.
    #[command(
        after_help = "Examples: tesseractl bridge add sc3-in --card hw:CARD=SC3,DEV=0 --direction capture --channels 2\n          \
                            tesseractl bridge add sc3-out --card hw:CARD=SC3,DEV=0 --direction playback --first 3 --channels 2\n          \
                            tesseractl bridge add cq-main --card hw:CARD=CQ,DEV=0 --direction capture --card-first 15 --channels 2\n          \
                            tesseractl bridge add cq-local --card hw:CARD=CQ,DEV=0 --direction capture --card-first 15 --channels 2 --local-only\n\
                            The card's PCM is listed by `surfaces` (DETAILS)."
    )]
    Add(BridgeAddArgs),
    /// Remove a bridge added with `bridge add` or in the web portal.
    Remove {
        /// The bridge's name (see `bridge list`).
        name: String,
    },
}

/// Which way a new bridge carries audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BridgeDir {
    /// The card's inputs to TX channels (this host to the network).
    Capture,
    /// RX channels (network to this host) to the card's outputs.
    Playback,
}

impl From<BridgeDir> for BridgeDirection {
    fn from(d: BridgeDir) -> Self {
        match d {
            BridgeDir::Capture => BridgeDirection::Capture,
            BridgeDir::Playback => BridgeDirection::Playback,
        }
    }
}

/// Options of `bridge add`.
#[derive(Debug, Args)]
pub struct BridgeAddArgs {
    /// The bridge's name (letters, digits, - and _).
    pub name: String,
    /// ALSA PCM of the card, for example `hw:CARD=SC3,DEV=0`.
    #[arg(long)]
    pub card: String,
    /// `capture` (card inputs to TX) or `playback` (RX to card outputs).
    #[arg(long, value_enum)]
    pub direction: BridgeDir,
    /// First device channel carried, from 1: TX for capture, RX for
    /// playback.
    #[arg(long, default_value = "1")]
    pub first: NonZeroU16,
    /// Channels carried (card channels from --card-first on).
    #[arg(long)]
    pub channels: NonZeroU16,
    /// First card channel carried, from 1 (for example 15 for a mixer's
    /// main mix on USB channels 15-16). The card is opened with every
    /// channel it needs; the others are ignored (capture) or silent
    /// (playback).
    #[arg(long, default_value = "1")]
    pub card_first: NonZeroU16,
    /// Frames per card period (default: the daemon's, 10 ms).
    #[arg(long, conflicts_with = "low_latency")]
    pub period_frames: Option<u32>,
    /// Short card periods: 96 frames (2 ms at 48 kHz).
    #[arg(long)]
    pub low_latency: bool,
    /// Save it switched off.
    #[arg(long)]
    pub disabled: bool,
    /// Open the card for the host router only: its channels become router
    /// ports (`in:CARD:N` / `out:CARD:N`) and no device channel is fed or
    /// read, so the card stays off the network until a crosspoint patches it
    /// there (`router patch`). Needs a daemon with the host router.
    #[arg(long, conflicts_with = "first")]
    pub local_only: bool,
}

/// Card period of `bridge add --low-latency`, frames.
pub const LOW_LATENCY_PERIOD_FRAMES: u32 = 96;

impl BridgeAddArgs {
    /// The bridge these options describe, on `device`.
    #[must_use]
    pub fn spec(&self, device: &DeviceKey) -> CardBridgeSpec {
        let mut b = CardBridgeSpec::new(
            self.name.clone(),
            device.clone(),
            self.card.clone(),
            self.direction.into(),
            self.first.get(),
            self.channels.get(),
        );
        b.enabled = !self.disabled;
        b.local_only = self.local_only;
        if self.local_only {
            // A local bridge feeds no device channel; the state wants 1 here.
            b.first_channel = 1;
        }
        b.card_first_channel = self.card_first.get();
        b.period_frames = if self.low_latency {
            Some(LOW_LATENCY_PERIOD_FRAMES)
        } else {
            self.period_frames
        };
        b
    }
}

/// RX channel n also plays TX channel `.0[n-1]` (0: none): the argument of
/// `monitor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorMap(pub Vec<u16>);

/// Parse `1,2,0,3` or `off` (also `none`): trailing zeros are dropped.
///
/// # Errors
/// A message for the command line.
pub fn parse_monitor(s: &str) -> Result<MonitorMap, String> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("off") || t.eq_ignore_ascii_case("none") {
        return Ok(MonitorMap(Vec::new()));
    }
    let mut v = Vec::new();
    for part in t.split(',') {
        let p = part.trim();
        let n: u16 = p
            .parse()
            .map_err(|_| format!("`{p}` is not a TX channel number (0 for none)"))?;
        v.push(n);
    }
    while v.last() == Some(&0) {
        v.pop();
    }
    Ok(MonitorMap(v))
}

/// Parse a delay in milliseconds (decimals allowed) as microseconds;
/// `auto` (or 0) is 0, the lowest safe delay.
///
/// # Errors
/// A message for the command line.
pub fn parse_delay_ms(s: &str) -> Result<u32, String> {
    if s.trim().eq_ignore_ascii_case("auto") {
        return Ok(0);
    }
    let ms: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("`{s}` is not a number of milliseconds (or `auto`)"))?;
    if !(0.0..=1000.0).contains(&ms) {
        return Err("the delay is from 0 to 1000 ms".into());
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok((ms * 1000.0).round() as u32)
}

/// `tesseractl config ...`.
#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Ask the daemon to re-read its configuration files.
    Reload,
    /// Show configuration.
    Show(ShowArgs),
    /// Validate the configuration files without touching the daemon.
    Check(DirArgs),
}

/// Where the configuration files are.
#[derive(Debug, Args)]
pub struct DirArgs {
    /// Directory with `config.toml` and `config.d/`.
    #[arg(long, env = "TESSERA_CONFIG_DIR", default_value = "/etc/tessera")]
    pub config_dir: PathBuf,
}

/// Options of `config show`.
#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Show what the running daemon actually uses (configuration combined
    /// with the stored state), instead of the merged configuration files.
    #[arg(long)]
    pub effective: bool,
    /// With the files view: also list which file set each key.
    #[arg(long, conflicts_with = "effective")]
    pub provenance: bool,
    /// Directory with `config.toml` and `config.d/` (files view).
    #[command(flatten)]
    pub dir: DirArgs,
}

/// Turn a settings command into the daemon command it sends. `None` for
/// commands that are not settings changes.
#[must_use]
pub fn to_command(cmd: &Cmd, device: &DeviceKey) -> Option<Command> {
    let d = || device.clone();
    Some(match cmd {
        Cmd::SetName { name } => Command::SetDeviceName {
            device: d(),
            name: name.clone(),
        },
        Cmd::SetRate { hz } => Command::SetSampleRate {
            device: d(),
            rate: SampleRate(*hz),
        },
        Cmd::SetEncoding { bits } => Command::SetEncoding {
            device: d(),
            encoding: Encoding(*bits),
        },
        Cmd::SetLatency { micros } => Command::SetLatency {
            device: d(),
            latency: LatencyUs(*micros),
        },
        Cmd::SetChannels { rx, tx, dry_run } => Command::SetDeviceChannels {
            device: d(),
            rx: *rx,
            tx: *tx,
            dry_run: *dry_run,
        },
        Cmd::SetLabel {
            direction,
            channel,
            label,
            reset,
        } => Command::SetChannelLabel {
            device: d(),
            direction: (*direction).into(),
            channel: *channel,
            label: if *reset { None } else { label.clone() },
        },
        Cmd::Subscribe {
            rx_channel,
            tx_channel,
            tx_device,
        } => Command::Subscribe {
            device: d(),
            rx_channel: *rx_channel,
            tx_channel: tx_channel.clone(),
            tx_device: tx_device.clone(),
        },
        Cmd::Unsubscribe { rx_channel } => Command::Unsubscribe {
            device: d(),
            rx_channel: *rx_channel,
        },
        Cmd::Clock(ClockCmd::SetRole { role }) => Command::SetClockRole {
            role: (*role).into(),
        },
        Cmd::Clock(ClockCmd::SetPriority { priority }) => Command::SetClockPriority {
            priority: *priority,
        },
        Cmd::Network(NetworkCmd::Primary { interface }) => Command::SetPrimaryInterface {
            interface: primary_arg(interface),
        },
        Cmd::Config(ConfigCmd::Reload) => Command::ReloadConfig,
        Cmd::Flows(FlowCmd::Clear) => Command::SetProtocolFlows { flows: Vec::new() },
        Cmd::Access(AccessCmd::Reset) => Command::ResetAccessLock { device: d() },
        Cmd::Mute(t) => Command::SetChannelMute {
            target: t.set(),
            muted: true,
        },
        Cmd::Unmute(t) => Command::SetChannelMute {
            target: t.set(),
            muted: false,
        },
        Cmd::Gain { target, gain } => Command::SetChannelGain {
            target: target.set(),
            gain: *gain,
        },
        Cmd::Surface(SurfaceCmd::Remove { name }) => Command::RemoveSurface { name: name.clone() },
        Cmd::Surface(SurfaceCmd::Set { file }) => Command::SetSurface {
            surface: (**file).clone(),
        },
        Cmd::Bridge(BridgeCmd::Add(a)) => Command::SetCardBridge {
            bridge: a.spec(device),
        },
        Cmd::OutputGain {
            device_key,
            channels,
            gain,
        } => Command::SetOutputGain {
            target: rx_set(device_key, channels),
            gain: *gain,
        },
        Cmd::OutputMute {
            device_key,
            channels,
            off,
        } => Command::SetOutputMute {
            target: rx_set(device_key, channels),
            muted: !*off,
        },
        Cmd::Bridge(BridgeCmd::Remove { name }) => Command::RemoveCardBridge { name: name.clone() },
        Cmd::Router(r) => return router_command(r),
        Cmd::Volume(VolumeCmd::Set { gain }) => Command::SetHostVolume { gain: *gain },
        Cmd::Volume(VolumeCmd::Mute { off }) => Command::SetHostMute { muted: !*off },
        Cmd::Mixer(MixerCmd::Set { file }) => Command::SetMixer {
            mixer: (**file).clone(),
        },
        Cmd::Mixer(MixerCmd::Remove { id }) => Command::RemoveMixer { id: id.clone() },
        Cmd::Monitor {
            device_key,
            rx_from_tx,
            delay_ms: Some(delay_us),
        } => Command::SetMonitor {
            device: DeviceKey(device_key.clone()),
            rx_from_tx: rx_from_tx.0.clone(),
            delay_us: *delay_us,
        },
        _ => return None,
    })
}

/// The daemon command of a `router` subcommand (`None` for `show`).
fn router_command(r: &RouterCmd) -> Option<Command> {
    let key = |s: &RouterPort, d: &RouterPort| CrosspointKey::new(s.clone(), d.clone());
    Some(match r {
        RouterCmd::Show => return None,
        RouterCmd::Patch {
            source,
            destination,
            gain,
            muted,
        } => {
            let mut x = Crosspoint::new(source.clone(), destination.clone());
            x.gain = gain.unwrap_or_default();
            x.muted = *muted;
            Command::SetCrosspoint { crosspoint: x }
        }
        RouterCmd::Unpatch {
            source,
            destination,
        } => Command::RemoveCrosspoint {
            source: source.clone(),
            destination: destination.clone(),
        },
        RouterCmd::Gain {
            source,
            destination,
            gain,
        } => Command::SetCrosspointGain {
            crosspoints: vec![key(source, destination)],
            gain: *gain,
        },
        RouterCmd::Mute {
            source,
            destination,
            off,
        } => Command::SetCrosspointMute {
            crosspoints: vec![key(source, destination)],
            muted: !*off,
        },
        RouterCmd::PortGain { port, gain } => Command::SetPortGain {
            ports: vec![port.clone()],
            gain: *gain,
        },
        RouterCmd::PortMute { port, off } => Command::SetPortMute {
            ports: vec![port.clone()],
            muted: !*off,
        },
        RouterCmd::Label { port, label } => Command::SetPortLabel {
            port: port.clone(),
            label: label.clone(),
        },
        RouterCmd::Set { file } => Command::SetRouter {
            router: (**file).clone(),
        },
        RouterCmd::Reset => Command::ResetRouter,
    })
}

/// RX channels of a device.
fn rx_set(device: &str, channels: &Channels) -> ChannelSet {
    ChannelSet {
        device: DeviceKey(device.to_owned()),
        direction: Direction::Rx,
        channels: channels.0.clone(),
    }
}

/// The access-lock command for `cmd` with `pin` (`lock` and `unlock`).
#[must_use]
pub fn access_command(cmd: &AccessCmd, device: &DeviceKey, pin: AccessPin) -> Command {
    match cmd {
        AccessCmd::Lock => Command::LockAccess {
            device: device.clone(),
            pin,
        },
        AccessCmd::Unlock => Command::UnlockAccess {
            device: device.clone(),
            pin,
        },
        AccessCmd::Reset => Command::ResetAccessLock {
            device: device.clone(),
        },
    }
}

/// Read-only routing queries. References come from `routing devices`.
#[derive(Debug, Subcommand)]
pub enum RoutingCmd {
    /// Review retired-runtime carrier ownership; no routing change.
    InspectCarriers,
    /// Return the reviewed occurrences to current runtime management.
    ReleaseCarriers {
        /// Single-use token returned by `routing inspect-carriers`.
        #[arg(long)]
        review: String,
    },
    /// Authorize the next runtime start to replace reviewed first-device carriers.
    GrantNextRun {
        /// Single-use next_run_review token from `routing inspect-carriers`.
        #[arg(long)]
        review: String,
    },
    /// Inspect complete local receiver state after an uncertain operation.
    Inspect {
        /// Optional exact unresolved operation ID to inspect.
        #[arg(long)]
        operation: Option<String>,
    },
    /// Accept a fresh reviewed baseline; never retries a receiver mutation.
    Reconcile {
        /// Single-use token from a complete `routing inspect` review.
        #[arg(long)]
        review: String,
    },
    /// Execute one Connect, Replace or Disconnect using a fresh receiver token.
    Execute {
        /// Expectation returned by `routing receiver` (expires after 30 seconds).
        #[arg(long)]
        expectation: String,
        /// ReceiverChange JSON, using source references from routing inventory.
        #[arg(long)]
        change: String,
    },
    /// Show a page of available local and discovered endpoints.
    Devices {
        /// Opaque continuation from the previous page.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Show advertised or local channel rows for a current endpoint.
    Channels {
        /// Daemon-issued endpoint ID.
        #[arg(long)]
        endpoint: String,
        /// Current endpoint generation.
        #[arg(long)]
        generation: String,
        /// Receive or transmit channels.
        direction: Dir,
        /// Opaque continuation from the previous page.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Read one page of selections currently reported by the receiver.
    Receiver {
        /// Daemon-issued endpoint ID.
        #[arg(long)]
        endpoint: String,
        /// Current endpoint generation.
        #[arg(long)]
        generation: String,
        /// One-based item position; advance using next_first from the reply.
        #[arg(long, default_value = "1")]
        first: NonZeroU16,
    },
}

/// `default` and `auto` clear the selection; anything else names an interface.
#[must_use]
pub fn primary_arg(s: &str) -> Option<String> {
    (!matches!(s, "default" | "auto")).then(|| s.to_owned())
}

/// Does this command need a device?
#[must_use]
pub fn needs_device(cmd: &Cmd) -> bool {
    matches!(
        cmd,
        Cmd::SetName { .. }
            | Cmd::SetRate { .. }
            | Cmd::SetEncoding { .. }
            | Cmd::SetLatency { .. }
            | Cmd::SetChannels { .. }
            | Cmd::SetLabel { .. }
            | Cmd::Subscribe { .. }
            | Cmd::Unsubscribe { .. }
            | Cmd::Access(_)
            | Cmd::Bridge(BridgeCmd::Add(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn grammar_is_consistent() {
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("tesseractl").chain(args.iter().copied()))
    }

    #[test]
    fn set_label_needs_a_label_or_reset() {
        assert!(parse(&["set-label", "rx", "1"]).is_err());
        assert!(parse(&["set-label", "rx", "1", "Vox"]).is_ok());
        assert!(parse(&["set-label", "rx", "1", "--reset"]).is_ok());
        assert!(parse(&["set-label", "rx", "1", "Vox", "--reset"]).is_err());
        assert!(parse(&["set-label", "rx", "0", "Vox"]).is_err());
    }

    #[test]
    fn commands_map_to_daemon_commands() {
        let dev = DeviceKey("main".into());
        let cli = parse(&["set-name", "studio"]).unwrap();
        assert_eq!(
            to_command(&cli.command, &dev),
            Some(Command::SetDeviceName {
                device: dev.clone(),
                name: "studio".into()
            })
        );
        let cli = parse(&["set-label", "tx", "2", "--reset"]).unwrap();
        assert!(matches!(
            to_command(&cli.command, &dev),
            Some(Command::SetChannelLabel { label: None, .. })
        ));
        let cli = parse(&["clock", "set-role", "leader"]).unwrap();
        assert_eq!(
            to_command(&cli.command, &dev),
            Some(Command::SetClockRole {
                role: ClockRole::Leader
            })
        );
        let cli = parse(&["network", "primary", "enp4s0"]).unwrap();
        assert_eq!(
            to_command(&cli.command, &dev),
            Some(Command::SetPrimaryInterface {
                interface: Some("enp4s0".into())
            })
        );
        for word in ["default", "auto"] {
            let cli = parse(&["network", "primary", word]).unwrap();
            assert_eq!(
                to_command(&cli.command, &dev),
                Some(Command::SetPrimaryInterface { interface: None })
            );
        }
        assert!(!needs_device(&parse(&["network", "show"]).unwrap().command));
        let cli = parse(&["config", "reload"]).unwrap();
        assert_eq!(to_command(&cli.command, &dev), Some(Command::ReloadConfig));
        assert_eq!(to_command(&parse(&["status"]).unwrap().command, &dev), None);
    }

    #[test]
    fn set_channels_takes_both_counts_and_maps_to_the_command() {
        assert!(parse(&["set-channels", "--rx", "8"]).is_err());
        assert!(parse(&["set-channels", "--tx", "8"]).is_err());
        assert!(parse(&["set-channels", "--rx", "70000", "--tx", "2"]).is_err());
        assert!(parse(&["set-channels", "--rx", "-1", "--tx", "2"]).is_err());
        let dev = DeviceKey("main".into());
        let cli = parse(&["set-channels", "--rx", "8", "--tx", "0"]).unwrap();
        assert!(!cli.yes);
        assert_eq!(
            to_command(&cli.command, &dev),
            Some(Command::SetDeviceChannels {
                device: dev.clone(),
                rx: 8,
                tx: 0,
                dry_run: false
            })
        );
        assert!(needs_device(&cli.command));
        let cli = parse(&["set-channels", "--rx", "2", "--tx", "2", "--dry-run"]).unwrap();
        assert!(matches!(
            to_command(&cli.command, &dev),
            Some(Command::SetDeviceChannels { dry_run: true, .. })
        ));
        // The device and --yes work in any position; --dry-run excludes --yes.
        let cli = parse(&["-d", "main", "set-channels", "--rx", "2", "--tx", "2", "-y"]).unwrap();
        assert!(cli.yes && cli.device.as_deref() == Some("main"));
        let cli = parse(&[
            "set-channels",
            "--rx",
            "2",
            "--tx",
            "2",
            "--yes",
            "--device",
            "b",
        ]);
        assert!(cli.is_ok());
        assert!(
            parse(&[
                "set-channels",
                "--rx",
                "2",
                "--tx",
                "2",
                "--dry-run",
                "--yes"
            ])
            .is_err()
        );
    }

    #[test]
    fn only_device_commands_need_a_device() {
        assert!(needs_device(
            &parse(&["set-rate", "48000"]).unwrap().command
        ));
        assert!(!needs_device(
            &parse(&["clock", "set-priority", "5"]).unwrap().command
        ));
        assert!(!needs_device(&parse(&["status"]).unwrap().command));
    }

    #[test]
    fn receiver_reads_use_inventory_references_and_reject_zero_page_index() {
        let command = parse(&[
            "routing",
            "receiver",
            "--endpoint",
            "opaque",
            "--generation",
            "current",
            "--first",
            "257",
        ])
        .unwrap();
        assert!(
            matches!(&command.command, Cmd::Routing(RoutingCmd::Receiver { first, .. }) if first.get() == 257)
        );
        assert!(!needs_device(&command.command));
        assert_eq!(
            to_command(&command.command, &DeviceKey("ignored".into())),
            None
        );
        assert!(
            parse(&[
                "routing",
                "receiver",
                "--endpoint",
                "opaque",
                "--generation",
                "current",
                "--first",
                "0"
            ])
            .is_err()
        );
    }

    fn chans(v: &[u16]) -> Vec<Channel> {
        v.iter().map(|n| Channel::new(*n).unwrap()).collect()
    }

    #[test]
    fn channel_lists_take_numbers_and_ranges_in_order() {
        assert_eq!(parse_channels("1").unwrap().0, chans(&[1]));
        assert_eq!(parse_channels("1,2").unwrap().0, chans(&[1, 2]));
        assert_eq!(parse_channels("1-4").unwrap().0, chans(&[1, 2, 3, 4]));
        assert_eq!(parse_channels("5, 1-2").unwrap().0, chans(&[5, 1, 2]));
        for bad in ["", "0", "1,,2", "4-1", "a", "1-", "1,1", "1-3,2", "70000"] {
            assert!(parse_channels(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn gains_are_db_or_off_within_the_range() {
        assert_eq!(parse_gain("off").unwrap(), Gain::Off);
        assert_eq!(parse_gain("OFF").unwrap(), Gain::Off);
        assert_eq!(parse_gain("-6").unwrap(), Gain::MilliDb(-6000));
        assert_eq!(parse_gain("-6.5dB").unwrap(), Gain::MilliDb(-6500));
        assert_eq!(parse_gain("+3").unwrap(), Gain::MilliDb(3000));
        assert_eq!(parse_gain("0").unwrap(), Gain::MilliDb(0));
        assert_eq!(parse_gain("-96").unwrap(), Gain::MilliDb(-96_000));
        assert_eq!(parse_gain("12 dB").unwrap(), Gain::MilliDb(12_000));
        for bad in ["-96.5", "12.1", "loud", "nan", "inf", ""] {
            assert!(parse_gain(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn mute_and_gain_map_to_channel_set_commands_with_their_own_device() {
        let ignored = DeviceKey("other".into());
        let set = ChannelSet {
            device: DeviceKey("main".into()),
            direction: Direction::Tx,
            channels: chans(&[1, 2]),
        };
        let cli = parse(&["mute", "main", "tx", "1-2"]).unwrap();
        assert!(!needs_device(&cli.command));
        assert_eq!(
            to_command(&cli.command, &ignored),
            Some(Command::SetChannelMute {
                target: set.clone(),
                muted: true
            })
        );
        let cli = parse(&["unmute", "main", "tx", "1,2"]).unwrap();
        assert_eq!(
            to_command(&cli.command, &ignored),
            Some(Command::SetChannelMute {
                target: set.clone(),
                muted: false
            })
        );
        // A negative gain is a value, not an option.
        let cli = parse(&["gain", "main", "tx", "1-2", "-6"]).unwrap();
        assert_eq!(
            to_command(&cli.command, &ignored),
            Some(Command::SetChannelGain {
                target: set,
                gain: Gain::MilliDb(-6000)
            })
        );
        let cli = parse(&["gain", "main", "rx", "3", "off"]).unwrap();
        assert!(matches!(
            to_command(&cli.command, &ignored),
            Some(Command::SetChannelGain {
                gain: Gain::Off,
                ..
            })
        ));
        assert!(parse(&["gain", "main", "rx", "3", "-200"]).is_err());
        assert!(parse(&["mute", "main", "up", "1"]).is_err());
        assert!(parse(&["mute", "main", "rx"]).is_err());
        // The global --device still works next to the positional device.
        let cli = parse(&["-d", "x", "mute", "main", "rx", "1"]).unwrap();
        assert_eq!(cli.device.as_deref(), Some("x"));
    }

    #[test]
    fn output_stage_commands_parse() {
        let dev = DeviceKey(String::new());
        let cli = parse(&["output-gain", "speaker", "1-2", "-6"]).unwrap();
        assert_eq!(
            to_command(&cli.command, &dev),
            Some(Command::SetOutputGain {
                target: ChannelSet {
                    device: DeviceKey("speaker".into()),
                    direction: Direction::Rx,
                    channels: vec![Channel::new(1).unwrap(), Channel::new(2).unwrap()],
                },
                gain: Gain::MilliDb(-6000),
            })
        );
        let cli = parse(&["output-mute", "speaker", "1", "--off"]).unwrap();
        assert!(matches!(
            to_command(&cli.command, &dev),
            Some(Command::SetOutputMute { muted: false, .. })
        ));
        assert!(!needs_device(&cli.command));
    }

    #[test]
    fn bridge_and_monitor_commands_parse() {
        let dev = DeviceKey("speaker".into());
        let cli = parse(&[
            "bridge",
            "add",
            "sc3-in",
            "--card",
            "hw:CARD=SC3,DEV=0",
            "--direction",
            "capture",
            "--first",
            "3",
            "--channels",
            "2",
            "--low-latency",
        ])
        .unwrap();
        assert!(needs_device(&cli.command));
        assert_eq!(
            to_command(&cli.command, &dev),
            Some(Command::SetCardBridge {
                bridge: {
                    let mut b = CardBridgeSpec::new(
                        "sc3-in",
                        dev.clone(),
                        "hw:CARD=SC3,DEV=0",
                        BridgeDirection::Capture,
                        3,
                        2,
                    );
                    b.period_frames = Some(96);
                    b
                }
            })
        );
        // A mixer's main mix on its USB channels 15-16.
        let cli = parse(&[
            "bridge",
            "add",
            "cq-main",
            "--card",
            "hw:CARD=CQ,DEV=0",
            "--direction",
            "capture",
            "--card-first",
            "15",
            "--channels",
            "2",
        ])
        .unwrap();
        match to_command(&cli.command, &dev) {
            Some(Command::SetCardBridge { bridge }) => {
                assert_eq!(
                    (
                        bridge.card_first_channel,
                        bridge.first_channel,
                        bridge.channels
                    ),
                    (15, 1, 2)
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(
            parse(&[
                "bridge",
                "add",
                "x",
                "--card",
                "c",
                "--direction",
                "capture",
                "--channels",
                "2",
                "--card-first",
                "0"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "bridge",
                "add",
                "x",
                "--card",
                "c",
                "--direction",
                "playback",
                "--channels",
                "2",
                "--low-latency",
                "--period-frames",
                "48"
            ])
            .is_err()
        );
        // A card opened for the host router only: no device channel.
        let local = [
            "bridge",
            "add",
            "cq-local",
            "--card",
            "hw:CARD=CQ,DEV=0",
            "--direction",
            "capture",
            "--card-first",
            "15",
            "--channels",
            "2",
            "--local-only",
        ];
        let cli = parse(&local).unwrap();
        match to_command(&cli.command, &dev) {
            Some(Command::SetCardBridge { bridge }) => {
                assert!(bridge.local_only);
                assert_eq!(
                    (
                        bridge.card_first_channel,
                        bridge.first_channel,
                        bridge.channels
                    ),
                    (15, 1, 2)
                );
                assert_eq!(
                    crate::output::card_bridge_words(&bridge, crate::output::Style::Ascii),
                    "hw:CARD=CQ,DEV=0 in 15-16 -> host router only"
                );
            }
            other => panic!("{other:?}"),
        }
        // Without it, a bridge feeds the network as before.
        let cli = parse(&local[..local.len() - 1]).unwrap();
        assert!(matches!(
            to_command(&cli.command, &dev),
            Some(Command::SetCardBridge { bridge }) if !bridge.local_only
        ));
        // A local bridge has no device channels to start from.
        let mut with_first = local.to_vec();
        with_first.extend(["--first", "3"]);
        assert!(parse(&with_first).is_err());
        let cli = parse(&["bridge", "remove", "sc3-in"]).unwrap();
        assert_eq!(
            to_command(&cli.command, &dev),
            Some(Command::RemoveCardBridge {
                name: "sc3-in".into()
            })
        );
        assert!(matches!(
            parse(&["bridge", "list"]).unwrap().command,
            Cmd::Bridge(BridgeCmd::List)
        ));
        let cli = parse(&["monitor", "speaker", "0,0,1,2,0", "--delay-ms", "12.5"]).unwrap();
        assert_eq!(
            to_command(&cli.command, &dev),
            Some(Command::SetMonitor {
                device: dev.clone(),
                rx_from_tx: vec![0, 0, 1, 2],
                delay_us: 12_500,
            })
        );
        // Without a delay the current one is read first (in `run`).
        let cli = parse(&["monitor", "speaker", "off"]).unwrap();
        assert_eq!(to_command(&cli.command, &dev), None);
        assert!(parse(&["monitor", "speaker", "1,x"]).is_err());
        assert!(parse(&["monitor", "speaker", "1", "--delay-ms", "-1"]).is_err());
        for auto in ["auto", "AUTO", "0"] {
            let cli = parse(&["monitor", "speaker", "1", "--delay-ms", auto]).unwrap();
            assert_eq!(
                to_command(&cli.command, &dev),
                Some(Command::SetMonitor {
                    device: dev.clone(),
                    rx_from_tx: vec![1],
                    delay_us: 0,
                }),
                "{auto}"
            );
        }
    }

    #[test]
    fn surface_commands_parse() {
        let cli = parse(&["surface", "remove", "sc3"]).unwrap();
        assert_eq!(
            to_command(&cli.command, &DeviceKey(String::new())),
            Some(Command::RemoveSurface { name: "sc3".into() })
        );
        assert!(!needs_device(&cli.command));
        let cli = parse(&["surface", "learn", "hid:3142:0c33:3", "--wait", "5"]).unwrap();
        assert!(matches!(
            cli.command,
            Cmd::Surface(SurfaceCmd::Learn {
                wait: 5,
                fader: false,
                ..
            })
        ));
        let cli = parse(&["surface", "learn", "x", "--fader"]).unwrap();
        assert!(matches!(
            cli.command,
            Cmd::Surface(SurfaceCmd::Learn {
                wait: 10,
                fader: true,
                ..
            })
        ));
        assert!(parse(&["surface", "learn", "x", "--wait", "31"]).is_err());
        assert_eq!(to_command(&cli.command, &DeviceKey(String::new())), None);
        assert!(parse(&["surface", "learn", "x", "--wait", "0"]).is_err());
        assert_eq!(
            to_command(
                &parse(&["surfaces"]).unwrap().command,
                &DeviceKey(String::new())
            ),
            None
        );
    }

    #[test]
    fn global_options_work_after_the_subcommand() {
        let cli = parse(&["status", "--json", "--socket", "/x"]).unwrap();
        assert!(cli.json);
        assert_eq!(cli.socket, PathBuf::from("/x"));
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;

    #[test]
    fn flow_commands_do_not_require_a_default_device() {
        for args in [
            vec!["tesseractl", "flows", "show"],
            vec!["tesseractl", "flows", "clear"],
            vec!["tesseractl", "flows", "set", "flows.toml"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(!needs_device(&cli.command));
        }
        let cli = Cli::try_parse_from(["tesseractl", "flows", "clear"]).unwrap();
        assert_eq!(
            to_command(&cli.command, &DeviceKey(String::new())),
            Some(Command::SetProtocolFlows { flows: Vec::new() })
        );
    }
}
