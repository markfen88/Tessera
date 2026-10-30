//! Command-line grammar of `tesseractl`.

use std::num::NonZeroU16;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use tessera_control_types::{
    AccessPin, ClockRole, Command, DeviceKey, Direction, Encoding, LatencyUs, SampleRate,
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
        _ => return None,
    })
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
