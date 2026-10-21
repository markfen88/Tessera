//! Command-line options of `tesserad`.
//!
//! Every tunable lives in the configuration files (`tessera-state`'s schema,
//! `docs/configuration.md`, `tesserad --print-default-config`). An option
//! given on the command line **overrides** its configuration key
//! ([`CLI_KEYS`]): precedence is command line > `locked` > `state.toml` >
//! `config.d/*.toml` > `config.toml` > built-in default, and a reload
//! (`SIGHUP`) never changes an overridden key. An option that is not given
//! leaves the key to the files, so every option here is an `Option` (or a
//! flag that only ever sets one value).
//!
//! Only the locations are command-line-only ([`CLI_ONLY`]).

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use tessera_state::Overrides;

/// `--audio-socket`: whether to listen on `audio.sock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AudioSocketMode {
    /// Listen if the configuration enables at least one backend client.
    Auto,
    /// Always listen.
    On,
    /// Never listen.
    Off,
}

impl AudioSocketMode {
    /// The spelling of `[audio] socket`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// `--rx-keepalive-payload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KeepalivePayloadArg {
    /// A zero-length datagram.
    Empty,
    /// The two bytes `13 37`.
    #[value(name = "1337")]
    Bytes1337,
}

impl KeepalivePayloadArg {
    /// The spelling of `rx_keepalive_payload`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Bytes1337 => "1337",
        }
    }
}

/// Options that override a configuration key: `(--option, section.key)`.
/// A test checks that every option is here or in [`CLI_ONLY`], that every
/// key exists in the schema, and that each option's help names its key.
pub const CLI_KEYS: &[(&str, &str)] = &[
    ("socket-group", "control.socket_group"),
    ("socket-mode", "control.socket_mode"),
    ("audio-socket", "audio.socket"),
    ("max-frame-bytes", "control.max_frame_bytes"),
    ("max-connections", "control.max_connections"),
    ("max-connections-per-uid", "control.max_connections_per_uid"),
    (
        "admin-reserved-connections",
        "control.admin_reserved_connections",
    ),
    ("idle-timeout-secs", "control.idle_timeout_secs"),
    ("event-queue", "control.event_queue"),
    ("control-threads", "control.threads"),
    ("rate-burst", "control.rate_burst"),
    ("rate-per-second", "control.rate_per_second"),
    ("frame-timeout-secs", "control.frame_timeout_secs"),
    ("write-timeout-secs", "control.write_timeout_secs"),
    (
        "stream-write-timeout-secs",
        "control.stream_write_timeout_secs",
    ),
    ("persist-delay-ms", "daemon.persist_delay_ms"),
    ("max-audio-clients", "audio.max_clients"),
    ("max-audio-clients-per-uid", "audio.max_clients_per_uid"),
    ("audio-max-period-frames", "audio.max_period_frames"),
    ("audio-ring-budget-mib", "audio.ring_budget_mib"),
    ("shutdown-timeout-secs", "daemon.shutdown_timeout_secs"),
    ("tick-us", "engine.tick_us"),
    ("rx-batch", "engine.rx_batch"),
    ("rx-max-batches", "engine.rx_max_batches"),
    ("tx-batch", "engine.tx_batch"),
    ("tx-max-per-tick", "engine.tx_max_per_tick"),
    ("socket-buffer-kib", "engine.socket_buffer_kib"),
    ("rx-margin-us", "engine.rx_margin_us"),
    ("rx-keepalive-ms", "defaults.flow.rx_keepalive_ms"),
    (
        "rx-keepalive-timeout-ms",
        "defaults.flow.rx_keepalive_timeout_ms",
    ),
    ("rx-keepalive-payload", "defaults.flow.rx_keepalive_payload"),
    ("no-rt", "daemon.realtime"),
    ("no-ptp", "clock.enabled"),
    ("no-discovery", "discovery.enabled"),
    ("audio-max-ring-mib", "audio.max_ring_mib"),
    ("audio-reorder-window-us", "audio.reorder_window_us"),
    ("audio-default-period-frames", "audio.default_period_frames"),
    ("log-level", "daemon.log_level"),
];

/// Options without a configuration key, and why.
pub const CLI_ONLY: &[(&str, &str)] = &[
    (
        "config-dir",
        "where the configuration files are: it cannot be a key of those files",
    ),
    (
        "state-dir",
        "an explicit location of state.toml for this process (tests, a second instance); with it, `[daemon] state_dir` is not used by this process, and a reload that changes that key is accepted (listed as a restart key) instead of refused",
    ),
    (
        "runtime-dir",
        "an explicit socket and lock directory for this process (tests, a second instance); `[daemon] runtime_dir` is the configured one",
    ),
    (
        "print-default-config",
        "an action, not a setting: prints the reference configuration and exits",
    ),
    ("help", "an action"),
    ("version", "an action"),
];

/// Parsed command line.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "tesserad",
    version,
    about = "tessera audio-over-IP daemon",
    long_about = "tessera audio-over-IP daemon.\n\nEvery setting lives in the configuration files (/etc/tessera/config.toml and config.d/*.toml; `tesserad --print-default-config` prints every key with its default). An option given here overrides its configuration key, and a reload (SIGHUP) does not change it; an option that is not given leaves the key to the files."
)]
pub struct Options {
    /// Directory holding `config.toml` and `config.d/`.
    #[arg(long, env = "TESSERA_CONFIG_DIR", default_value = "/etc/tessera")]
    pub config_dir: PathBuf,

    /// Directory for `state.toml`, for this process only. Default: `[daemon]
    /// state_dir` of the configuration (`/var/lib/tessera`).
    #[arg(long, env = "TESSERA_STATE_DIR")]
    pub state_dir: Option<PathBuf>,

    /// Directory for `control.sock` and `audio.sock`, for this process only.
    /// Default: `[daemon] runtime_dir` of the configuration (`/run/tessera`).
    #[arg(long, env = "TESSERA_RUNTIME_DIR")]
    pub runtime_dir: Option<PathBuf>,

    /// Print the reference configuration (every key at its default, with what
    /// it does) and exit.
    #[arg(long)]
    pub print_default_config: bool,

    /// Group (name or number) that owns the sockets. Overrides `[control]
    /// socket_group` (default: `tessera` if that group exists, else the
    /// daemon's own group with a warning; a group given here or in the file
    /// that cannot be applied is an error).
    #[arg(long)]
    pub socket_group: Option<String>,

    /// Permission bits of the sockets, octal. Overrides `[control]
    /// socket_mode` (default 0o660).
    #[arg(long, value_parser = parse_octal_mode)]
    pub socket_mode: Option<u32>,

    /// Listen on `audio.sock` for audio clients (ADR 0007). Overrides
    /// `[audio] socket` (default auto).
    #[arg(long, value_enum)]
    pub audio_socket: Option<AudioSocketMode>,

    /// Largest control request or response frame, bytes. Overrides
    /// `[control] max_frame_bytes` (default 1048576).
    #[arg(long)]
    pub max_frame_bytes: Option<u64>,

    /// Most simultaneous control connections (all peers). Overrides
    /// `[control] max_connections` (default 256).
    #[arg(long)]
    pub max_connections: Option<u64>,

    /// Most control connections one uid may hold. Overrides `[control]
    /// max_connections_per_uid` (default 16).
    #[arg(long)]
    pub max_connections_per_uid: Option<u64>,

    /// Control connection slots only administrators may use, so that
    /// unprivileged group members cannot lock them out. Overrides `[control]
    /// admin_reserved_connections` (default 16).
    #[arg(long)]
    pub admin_reserved_connections: Option<u64>,

    /// Seconds a request/response connection may stay silent before it is
    /// closed (event subscribers are exempt). Overrides `[control]
    /// idle_timeout_secs` (default 60).
    #[arg(long)]
    pub idle_timeout_secs: Option<u64>,

    /// Events queued per subscriber before the oldest are dropped. Overrides
    /// `[control] event_queue` (default 256).
    #[arg(long)]
    pub event_queue: Option<u64>,

    /// Worker threads of the control-plane runtime. Overrides `[control]
    /// threads` (default 2).
    #[arg(long)]
    pub control_threads: Option<u64>,

    /// Requests a peer may send back to back. Overrides `[control]
    /// rate_burst` (default 256).
    #[arg(long)]
    pub rate_burst: Option<u64>,

    /// Requests per second a peer may sustain. Overrides `[control]
    /// rate_per_second` (default 128).
    #[arg(long)]
    pub rate_per_second: Option<u64>,

    /// Seconds a peer may take to finish a frame, counted from its first
    /// byte (a deadline: trickling bytes does not extend it). Overrides
    /// `[control] frame_timeout_secs` (default 10).
    #[arg(long)]
    pub frame_timeout_secs: Option<u64>,

    /// Seconds to wait for a peer that does not read its responses.
    /// Overrides `[control] write_timeout_secs` (default 5).
    #[arg(long)]
    pub write_timeout_secs: Option<u64>,

    /// Seconds to wait for a stalled event subscriber before dropping it.
    /// Overrides `[control] stream_write_timeout_secs` (default 30).
    #[arg(long)]
    pub stream_write_timeout_secs: Option<u64>,

    /// Debounce window for writing `state.toml`, milliseconds (ADR 0008).
    /// Overrides `[daemon] persist_delay_ms` (default 500).
    #[arg(long)]
    pub persist_delay_ms: Option<u64>,

    /// Most simultaneous audio client connections. Overrides `[audio]
    /// max_clients` (default 64).
    #[arg(long)]
    pub max_audio_clients: Option<u64>,

    /// Most audio client connections one uid may hold. Overrides `[audio]
    /// max_clients_per_uid` (default 8).
    #[arg(long)]
    pub max_audio_clients_per_uid: Option<u64>,

    /// Largest `max_period_frames` an audio client may ask for. Overrides
    /// `[audio] max_period_frames` (default 16384).
    #[arg(long)]
    pub audio_max_period_frames: Option<u64>,

    /// Total ring memory all audio sessions together may reserve, MiB.
    /// Overrides `[audio] ring_budget_mib` (default 256).
    #[arg(long)]
    pub audio_ring_budget_mib: Option<u64>,

    /// Seconds to allow the final state flush and shutdown; keep the service
    /// manager's stop timeout above this. Overrides `[daemon]
    /// shutdown_timeout_secs` (default 20).
    #[arg(long)]
    pub shutdown_timeout_secs: Option<u64>,

    /// Engine tick (wake-up period of audio clients and of the receive
    /// drain), microseconds. Overrides `[engine] tick_us` (default auto,
    /// which is 1000; ADR 0004/0011).
    #[arg(long)]
    pub tick_us: Option<u64>,

    /// Datagrams per `recvmmsg` call. Overrides `[engine] rx_batch` (default
    /// 32).
    #[arg(long)]
    pub rx_batch: Option<u64>,

    /// Receive batches per socket per tick (bounds the work of one tick).
    /// Overrides `[engine] rx_max_batches` (default 8).
    #[arg(long)]
    pub rx_max_batches: Option<u64>,

    /// Datagrams per `sendmmsg` call. Overrides `[engine] tx_batch` (default
    /// 32).
    #[arg(long)]
    pub tx_batch: Option<u64>,

    /// Packets per transmit flow per tick (bounds the work of one tick).
    /// Overrides `[engine] tx_max_per_tick` (default 32).
    #[arg(long)]
    pub tx_max_per_tick: Option<u64>,

    /// Kernel socket buffer of media sockets, KiB. Overrides `[engine]
    /// socket_buffer_kib` (default 2048).
    #[arg(long)]
    pub socket_buffer_kib: Option<u64>,

    /// Margin added to the receive latency published to audio clients on top
    /// of one engine tick (wake-up jitter allowance), microseconds.
    /// Overrides `[engine] rx_margin_us` (default 250).
    #[arg(long)]
    pub rx_margin_us: Option<u64>,

    /// Interval of the receiver keepalives every rx flow sends to the source
    /// of its media (a unicast transmitter stops without them),
    /// milliseconds; 0 disables them. PROVISIONAL. Overrides
    /// `[defaults.flow] rx_keepalive_ms` (default 250).
    #[arg(long)]
    pub rx_keepalive_ms: Option<u64>,

    /// Stop an rx flow's keepalives after this long without valid media,
    /// milliseconds (they resume with the next packet); 0: never stop while
    /// the flow exists. Overrides `[defaults.flow] rx_keepalive_timeout_ms`
    /// (default 10000).
    #[arg(long)]
    pub rx_keepalive_timeout_ms: Option<u64>,

    /// What a receiver keepalive carries: `empty` (a zero-length datagram,
    /// as hardware receivers send) or `1337` (the bytes `13 37`).
    /// PROVISIONAL. Overrides `[defaults.flow] rx_keepalive_payload`
    /// (default empty).
    #[arg(long, value_enum)]
    pub rx_keepalive_payload: Option<KeepalivePayloadArg>,

    /// Never request real-time scheduling or lock memory, whatever
    /// `[engine]` says (development and tests). Sets `[daemon] realtime` to
    /// false (default true).
    #[arg(long)]
    pub no_rt: bool,

    /// Do not run PTP: the media clock stays free-running. Sets `[clock]
    /// enabled` to false (default true).
    #[arg(long)]
    pub no_ptp: bool,

    /// Do not advertise devices with mDNS, whatever the rest of
    /// `[discovery]` says. Sets `[discovery] enabled` to false (default
    /// true).
    #[arg(long)]
    pub no_discovery: bool,

    /// Largest single ring an audio client may request, MiB. Overrides
    /// `[audio] max_ring_mib` (default 64).
    #[arg(long)]
    pub audio_max_ring_mib: Option<u64>,

    /// Reorder window added to audio ring capacity, microseconds. Overrides
    /// `[audio] reorder_window_us` (default 10000).
    #[arg(long)]
    pub audio_reorder_window_us: Option<u64>,

    /// Client period assumed when an audio client does not state one,
    /// frames. Overrides `[audio] default_period_frames` (default 1024).
    #[arg(long)]
    pub audio_default_period_frames: Option<u64>,

    /// Log filter (`error|warn|info|debug|trace` or `target=level,...`).
    /// Overrides `[daemon] log_level` (default info); the `RUST_LOG`
    /// environment variable overrides both.
    #[arg(long)]
    pub log_level: Option<String>,
}

fn parse_octal_mode(s: &str) -> Result<u32, String> {
    let m = u32::from_str_radix(s.trim_start_matches("0o"), 8)
        .map_err(|_| format!("`{s}` is not an octal mode"))?;
    if m > 0o777 {
        return Err(format!("mode {s} has bits outside 0777"));
    }
    Ok(m)
}

impl Default for Options {
    fn default() -> Self {
        // `parse_from` with no arguments: nothing overridden.
        Self::parse_from(["tesserad"])
    }
}

impl Options {
    /// The configuration overrides of the options that were given
    /// ([`CLI_KEYS`]); empty when none was.
    ///
    /// # Errors
    /// A value the schema cannot take (the message names the key).
    pub fn overrides(&self) -> Result<Overrides, String> {
        let mut o = Overrides::new();
        let uints: &[(&str, Option<u64>)] = &[
            ("control.max_frame_bytes", self.max_frame_bytes),
            ("control.max_connections", self.max_connections),
            (
                "control.max_connections_per_uid",
                self.max_connections_per_uid,
            ),
            (
                "control.admin_reserved_connections",
                self.admin_reserved_connections,
            ),
            ("control.idle_timeout_secs", self.idle_timeout_secs),
            ("control.event_queue", self.event_queue),
            ("control.threads", self.control_threads),
            ("control.rate_burst", self.rate_burst),
            ("control.rate_per_second", self.rate_per_second),
            ("control.frame_timeout_secs", self.frame_timeout_secs),
            ("control.write_timeout_secs", self.write_timeout_secs),
            (
                "control.stream_write_timeout_secs",
                self.stream_write_timeout_secs,
            ),
            ("control.socket_mode", self.socket_mode.map(u64::from)),
            ("daemon.persist_delay_ms", self.persist_delay_ms),
            ("daemon.shutdown_timeout_secs", self.shutdown_timeout_secs),
            ("audio.max_clients", self.max_audio_clients),
            ("audio.max_clients_per_uid", self.max_audio_clients_per_uid),
            ("audio.max_period_frames", self.audio_max_period_frames),
            ("audio.ring_budget_mib", self.audio_ring_budget_mib),
            ("audio.max_ring_mib", self.audio_max_ring_mib),
            ("audio.reorder_window_us", self.audio_reorder_window_us),
            (
                "audio.default_period_frames",
                self.audio_default_period_frames,
            ),
            ("engine.tick_us", self.tick_us),
            ("engine.rx_batch", self.rx_batch),
            ("engine.rx_max_batches", self.rx_max_batches),
            ("engine.tx_batch", self.tx_batch),
            ("engine.tx_max_per_tick", self.tx_max_per_tick),
            ("engine.socket_buffer_kib", self.socket_buffer_kib),
            ("engine.rx_margin_us", self.rx_margin_us),
            ("defaults.flow.rx_keepalive_ms", self.rx_keepalive_ms),
            (
                "defaults.flow.rx_keepalive_timeout_ms",
                self.rx_keepalive_timeout_ms,
            ),
        ];
        for &(key, v) in uints {
            if let Some(v) = v {
                o.set_uint(key, v)?;
            }
        }
        let strings: [(&str, Option<&str>); 4] = [
            ("control.socket_group", self.socket_group.as_deref()),
            (
                "audio.socket",
                self.audio_socket.map(AudioSocketMode::as_str),
            ),
            (
                "defaults.flow.rx_keepalive_payload",
                self.rx_keepalive_payload.map(KeepalivePayloadArg::as_str),
            ),
            ("daemon.log_level", self.log_level.as_deref()),
        ];
        for (key, v) in strings {
            if let Some(v) = v {
                o.set(key, v)?;
            }
        }
        for (key, given) in [
            ("daemon.realtime", self.no_rt),
            ("clock.enabled", self.no_ptp),
            ("discovery.enabled", self.no_discovery),
        ] {
            if given {
                o.set(key, false)?;
            }
        }
        Ok(o)
    }

    /// The option that overrides `key`, if any (`--max-frame-bytes`).
    #[must_use]
    pub fn option_of(key: &str) -> Option<&'static str> {
        CLI_KEYS.iter().find(|(_, k)| *k == key).map(|(o, _)| *o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use tessera_state::schema::{self, KeyDefault};

    #[test]
    fn nothing_is_overridden_without_options() {
        // The declared default, whatever TESSERA_CONFIG_DIR says here.
        let cmd = Options::command();
        let dir = cmd
            .get_arguments()
            .find(|a| a.get_long() == Some("config-dir"))
            .unwrap();
        assert_eq!(dir.get_default_values(), ["/etc/tessera"]);
        // Options without arguments override nothing (the location
        // variables are command-line only, so they cannot add overrides).
        let o = Options::default();
        assert!(!o.print_default_config);
        assert!(o.overrides().unwrap().is_empty());
    }

    /// One sample value per option of [`CLI_KEYS`] (none for flags).
    fn sample(option: &str) -> Option<&'static str> {
        Some(match option {
            "socket-group" => "audio",
            "socket-mode" => "640",
            "audio-socket" => "on",
            "rx-keepalive-payload" => "1337",
            "log-level" => "debug",
            "no-rt" | "no-ptp" | "no-discovery" => return None,
            "max-frame-bytes" => "131072",
            "admin-reserved-connections" => "4",
            "audio-default-period-frames" => "256",
            "max-connections" => "300",
            "audio-ring-budget-mib" => "128",
            "audio-max-period-frames" => "4096",
            "tick-us" => "500",
            _ => "7",
        })
    }

    #[test]
    fn every_option_maps_to_a_key_or_is_command_line_only() {
        let mut cmd = Options::command();
        cmd.build();
        let mut seen = Vec::new();
        for a in cmd.get_arguments() {
            let Some(long) = a.get_long() else {
                panic!("option {:?} has no long name", a.get_id());
            };
            seen.push(long.to_owned());
            let mapped = CLI_KEYS.iter().any(|(o, _)| *o == long);
            let only = CLI_ONLY.iter().any(|(o, _)| *o == long);
            assert!(
                mapped ^ only,
                "--{long}: map it to a key in CLI_KEYS or explain it in CLI_ONLY"
            );
        }
        for (o, _) in CLI_KEYS.iter().chain(CLI_ONLY) {
            assert!(
                seen.iter().any(|s| s == o),
                "--{o} is listed but not an option"
            );
        }
    }

    #[test]
    fn each_option_overrides_exactly_its_key_and_its_help_names_it() {
        let cmd = Options::command();
        for (option, key) in CLI_KEYS {
            let spec = schema::key(key).unwrap_or_else(|| panic!("{key} is not in the schema"));
            let mut args = vec!["tesserad".to_owned(), format!("--{option}")];
            if let Some(v) = sample(option) {
                args.push(v.to_owned());
            }
            let o = Options::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let ov = o.overrides().unwrap_or_else(|e| panic!("--{option}: {e}"));
            assert_eq!(ov.keys().collect::<Vec<_>>(), vec![*key], "--{option}");
            // The overridden value loads (it passes the schema's checks).
            let loaded = tessera_state::load_from_sources_with(
                &[],
                &tessera_state::Environment::new("test-host"),
                &ov,
            );
            assert!(loaded.is_ok(), "--{option}: {:?}", loaded.err());

            let arg = cmd
                .get_arguments()
                .find(|a| a.get_long() == Some(option))
                .unwrap();
            let help = arg
                .get_long_help()
                .or_else(|| arg.get_help())
                .map(ToString::to_string)
                .unwrap_or_default()
                .replace('\n', " ");
            let named = format!("`[{}] {}`", spec.section, spec.key);
            assert!(
                help.contains(&named),
                "--{option}: help must name {named}: {help}"
            );
            if let KeyDefault::Value(v) = spec.default {
                let shown = v.trim_matches('"');
                assert!(
                    help.contains(&format!("default {shown}"))
                        || help.contains(&format!("default: {shown}")),
                    "--{option}: help must show the default {shown}: {help}"
                );
            }
        }
    }

    #[test]
    fn keepalive_options_override_the_flow_defaults() {
        let o = Options::parse_from([
            "tesserad",
            "--rx-keepalive-ms",
            "0",
            "--rx-keepalive-timeout-ms",
            "0",
            "--rx-keepalive-payload",
            "1337",
        ]);
        let ov = o.overrides().unwrap();
        let l = tessera_state::load_from_sources_with(
            &[],
            &tessera_state::Environment::new("test-host"),
            &ov,
        )
        .unwrap();
        let f = &l.config.defaults.flow;
        assert_eq!((f.rx_keepalive_ms, f.rx_keepalive_timeout_ms), (0, 0));
        assert_eq!(
            f.rx_keepalive_payload,
            tessera_state::config::KeepalivePayload::Bytes1337
        );
    }

    #[test]
    fn octal_modes_are_validated() {
        assert_eq!(parse_octal_mode("0660"), Ok(0o660));
        assert_eq!(parse_octal_mode("666"), Ok(0o666));
        assert!(parse_octal_mode("999").is_err());
        assert!(parse_octal_mode("1777").is_err());
        let o = Options::parse_from(["tesserad", "--socket-mode", "640"]);
        let l = tessera_state::load_from_sources_with(
            &[],
            &tessera_state::Environment::new("test-host"),
            &o.overrides().unwrap(),
        )
        .unwrap();
        assert_eq!(l.config.control.socket_mode, 0o640);
    }
}
