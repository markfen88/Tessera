//! Client configuration (ADR 0007/0008: everything tunable is configuration
//! with a default; nothing here is a protocol constant).

use std::path::PathBuf;
use std::time::Duration;

use crate::backoff::BackoffConfig;
use crate::device::DeviceDesc;
use crate::error::Error;

/// Quality of the follower-mode resampler (ADR 0006), the one from
/// `tessera-media`: `off|linear|cubic|sinc-fast|sinc-hq`.
pub use tessera_media::Quality as ResamplerQuality;

/// Default `audio.sock` path (ADR 0010).
pub const DEFAULT_AUDIO_SOCKET: &str = "/run/tessera/audio.sock";
/// Default `control.sock` path (ADR 0010).
pub const DEFAULT_CONTROL_SOCKET: &str = "/run/tessera/control.sock";

/// How channel positions are advertised to PipeWire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionMode {
    /// Mono and stereo get `MONO` / `FL,FR` (so desktop apps link them
    /// sensibly); everything else gets `AUX0..`.
    Auto,
    /// Always `AUX0..` (what a multichannel interface looks like).
    Aux,
}

/// The follower resampler quality used when none is configured (ADR 0006):
/// cubic on ARM, short sinc on x86. One resampler instance serves all
/// channels of a node, so even the sinc tables exist once per node.
pub fn default_quality() -> ResamplerQuality {
    if cfg!(any(target_arch = "arm", target_arch = "aarch64")) {
        ResamplerQuality::Cubic
    } else {
        ResamplerQuality::SincFast
    }
}

fn parse_quality(s: &str) -> Option<ResamplerQuality> {
    Some(match s {
        "off" => ResamplerQuality::Off,
        "linear" => ResamplerQuality::Linear,
        "cubic" => ResamplerQuality::Cubic,
        "sinc-fast" => ResamplerQuality::SincFast,
        "sinc-hq" => ResamplerQuality::SincHq,
        _ => return None,
    })
}

/// Everything `tessera-pw` is configured with.
#[derive(Debug, Clone)]
pub struct Config {
    /// The daemon's audio socket.
    pub audio_socket: PathBuf,
    /// The daemon's control socket (device descriptions).
    pub control_socket: PathBuf,
    /// Client name sent in `Hello` and used as the PipeWire application name.
    pub client_name: String,
    /// Prefix of the PipeWire node names (`<prefix>.<device>.source`).
    pub node_prefix: String,
    /// Graph period requested through `node.latency`, in frames.
    pub period_frames: u32,
    /// Largest graph period the client supports (sizes the rings and all
    /// real-time scratch buffers), in frames. Larger periods are clamped.
    pub max_period_frames: u32,
    /// Channels per PipeWire node; devices with more channels are split over
    /// several nodes (at most 64: the `AUX` positions).
    pub max_node_channels: u32,
    /// Most channels per direction accepted from the daemon (sanity bound for
    /// hostile or broken device descriptions).
    pub max_device_channels: u32,
    /// Channel position advertisement.
    pub positions: PositionMode,
    /// `priority.driver` of our nodes. The default (500) is below hardware
    /// cards (ALSA sinks use about 1000), so the graph follows the network
    /// clock only when nothing else drives it (ADR 0007).
    pub priority_driver: i32,
    /// `priority.session` of our nodes (low: do not steal the default
    /// device from hardware).
    pub priority_session: i32,
    /// `SCHED_FIFO` priority of the control loop that triggers graph cycles
    /// (0 = leave at normal priority). Threads spawned later (handshakes,
    /// queries) reset themselves to normal priority.
    pub rt_priority: u8,
    /// Lock all memory at start-up (`mlockall`, ADR 0004); a failure is
    /// logged, not fatal.
    pub mlock: bool,
    /// Extra safety offset when reading RX (frames behind `now - latency`).
    pub rx_offset_frames: u32,
    /// TX lead in graph periods (data of a cycle is written this many
    /// periods after the cycle start).
    pub tx_ahead_periods: u32,
    /// Extra TX lead in frames on top of `tx_ahead_periods`.
    pub tx_ahead_margin_frames: u32,
    /// Follower-mode resampler quality (default: [`default_quality`]).
    pub resampler: ResamplerQuality,
    /// Chunk size of the follower resampler, in frames.
    pub follower_chunk_frames: u32,
    /// Only expose these device keys (empty = all).
    pub only_devices: Vec<String>,
    /// Devices to expose even before (or without) a control-socket answer, with
    /// generated channel labels (--static-device KEY:RX:TX[:RATE]).
    pub static_devices: Vec<DeviceDesc>,
    /// How often the device list is re-read from the control socket.
    pub discover_interval: Duration,
    /// A device is removed only after it was missing from this many
    /// consecutive queries.
    pub missing_queries: u32,
    /// Time limit of every handshake step.
    pub handshake_timeout: Duration,
    /// A session whose clock overlay has not been refreshed for this long is
    /// treated as lost (a hung daemon); zero disables the check.
    pub overlay_stale: Duration,
    /// A session must live this long before the reconnect back-off resets.
    pub stable_after: Duration,
    /// Reconnect back-off.
    pub backoff: BackoffConfig,
    /// Log filter level (`error|warn|info|debug|trace`).
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            audio_socket: DEFAULT_AUDIO_SOCKET.into(),
            control_socket: DEFAULT_CONTROL_SOCKET.into(),
            client_name: "tessera-pw".into(),
            node_prefix: "tessera".into(),
            period_frames: 256,
            max_period_frames: 2048,
            max_node_channels: 64,
            max_device_channels: 4096,
            positions: PositionMode::Auto,
            priority_driver: 500,
            priority_session: 100,
            rt_priority: 20,
            mlock: true,
            rx_offset_frames: 0,
            tx_ahead_periods: 1,
            tx_ahead_margin_frames: 64,
            resampler: default_quality(),
            follower_chunk_frames: 64,
            only_devices: Vec::new(),
            static_devices: Vec::new(),
            discover_interval: Duration::from_secs(10),
            missing_queries: 3,
            handshake_timeout: Duration::from_secs(2),
            overlay_stale: Duration::from_secs(2),
            stable_after: Duration::from_secs(10),
            backoff: BackoffConfig::default(),
            log_level: "info".into(),
        }
    }
}

/// Result of parsing the command line.
#[derive(Debug)]
pub enum Cli {
    /// Run with this configuration.
    Run(Box<Config>),
    /// Print the usage text and exit.
    Help,
}

/// The usage text.
pub const USAGE: &str = "\
tessera-pw: PipeWire client of tesserad (interoperable with Dante(R) devices)

USAGE: tessera-pw [OPTIONS]

  --audio-socket PATH        daemon audio socket       [/run/tessera/audio.sock]
  --control-socket PATH      daemon control socket     [/run/tessera/control.sock]
  --device KEY               only expose this device (repeatable)
  --static-device K:RX:TX[:HZ] expose a device without the control socket (repeatable)
  --node-prefix STR          PipeWire node name prefix [tessera]
  --period FRAMES            requested graph period    [256]
  --max-period FRAMES        largest supported period  [2048]
  --max-node-channels N      channels per node (1..=64)[64]
  --max-device-channels N    reject devices with more   [4096]
  --positions auto|aux       channel position style    [auto]
  --priority-driver N        priority.driver           [500]
  --priority-session N       priority.session          [100]
  --rt-priority N            SCHED_FIFO priority of the control loop,
                             0 = normal priority        [20]
  --no-mlock                 do not mlockall at start-up
  --rx-offset FRAMES         extra RX read offset       [0]
  --tx-ahead-periods N       TX lead in periods         [1]
  --tx-ahead-margin FRAMES   extra TX lead              [64]
  --resampler Q              off|linear|cubic|sinc-fast|sinc-hq (follower mode;
                             default cubic on ARM, sinc-fast elsewhere)
  --follower-chunk FRAMES    resampler chunk            [64]
  --discover-interval SECS   device list refresh        [10]
  --missing-queries N        queries a device may be missing before its
                             nodes are removed          [3]
  --handshake-timeout MS     per handshake step         [2000]
  --overlay-stale MS         clock overlay silent this long = hung daemon,
                             0 = off                    [2000]
  --stable-after SECS        session age before the back-off resets [10]
  --reconnect-initial MS     first reconnect delay      [200]
  --reconnect-max MS         largest reconnect delay    [5000]
  --log-level LEVEL          error|warn|info|debug|trace[info]
  -h, --help                 this text
";

fn num<T: std::str::FromStr>(flag: &str, v: &str) -> Result<T, Error> {
    v.parse::<T>()
        .map_err(|_| Error::Config(format!("{flag}: invalid number {v:?}")))
}

/// Parses KEY:RX:TX[:RATE] into a device description with generated labels.
fn parse_static(s: &str) -> Result<DeviceDesc, Error> {
    let bad = || {
        Error::Config(format!(
            "--static-device: expected KEY:RX:TX[:RATE], got {s:?}"
        ))
    };
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 3 || parts.len() > 4 || parts[0].is_empty() {
        return Err(bad());
    }
    let rx: usize = parts[1].parse().map_err(|_| bad())?;
    let tx: usize = parts[2].parse().map_err(|_| bad())?;
    let rate: u32 = match parts.get(3) {
        Some(r) => r.parse().map_err(|_| bad())?,
        None => 48_000,
    };
    if rate == 0 || rx > 65_536 || tx > 65_536 {
        return Err(bad());
    }
    Ok(DeviceDesc {
        key: parts[0].to_string(),
        name: parts[0].to_string(),
        rate,
        rx_labels: (1..=rx).map(|i| format!("RX {i}")).collect(),
        tx_labels: (1..=tx).map(|i| format!("TX {i}")).collect(),
    })
}

impl Config {
    /// Parses command-line arguments (without the program name).
    pub fn from_args<I: IntoIterator<Item = String>>(args: I) -> Result<Cli, Error> {
        let mut cfg = Config::default();
        let mut it = args.into_iter();
        while let Some(flag) = it.next() {
            if flag == "-h" || flag == "--help" {
                return Ok(Cli::Help);
            }
            if flag == "--no-mlock" {
                cfg.mlock = false;
                continue;
            }
            let mut value = || {
                it.next()
                    .ok_or_else(|| Error::Config(format!("{flag}: missing value")))
            };
            match flag.as_str() {
                "--audio-socket" => cfg.audio_socket = value()?.into(),
                "--control-socket" => cfg.control_socket = value()?.into(),
                "--device" => cfg.only_devices.push(value()?),
                "--static-device" => cfg.static_devices.push(parse_static(&value()?)?),
                "--node-prefix" => cfg.node_prefix = value()?,
                "--client-name" => cfg.client_name = value()?,
                "--period" => cfg.period_frames = num(&flag, &value()?)?,
                "--max-period" => cfg.max_period_frames = num(&flag, &value()?)?,
                "--max-node-channels" => cfg.max_node_channels = num(&flag, &value()?)?,
                "--max-device-channels" => cfg.max_device_channels = num(&flag, &value()?)?,
                "--positions" => {
                    let v = value()?;
                    cfg.positions = match v.as_str() {
                        "auto" => PositionMode::Auto,
                        "aux" => PositionMode::Aux,
                        _ => return Err(Error::Config(format!("--positions: unknown mode {v:?}"))),
                    };
                }
                "--priority-driver" => cfg.priority_driver = num(&flag, &value()?)?,
                "--priority-session" => cfg.priority_session = num(&flag, &value()?)?,
                "--rt-priority" => cfg.rt_priority = num(&flag, &value()?)?,
                "--rx-offset" => cfg.rx_offset_frames = num(&flag, &value()?)?,
                "--tx-ahead-periods" => cfg.tx_ahead_periods = num(&flag, &value()?)?,
                "--tx-ahead-margin" => cfg.tx_ahead_margin_frames = num(&flag, &value()?)?,
                "--resampler" => {
                    let v = value()?;
                    cfg.resampler = parse_quality(&v).ok_or_else(|| {
                        Error::Config(format!("--resampler: unknown quality {v:?}"))
                    })?;
                }
                "--follower-chunk" => cfg.follower_chunk_frames = num(&flag, &value()?)?,
                "--discover-interval" => {
                    cfg.discover_interval = Duration::from_secs(num(&flag, &value()?)?);
                }
                "--missing-queries" => cfg.missing_queries = num(&flag, &value()?)?,
                "--handshake-timeout" => {
                    cfg.handshake_timeout = Duration::from_millis(num(&flag, &value()?)?);
                }
                "--overlay-stale" => {
                    cfg.overlay_stale = Duration::from_millis(num(&flag, &value()?)?);
                }
                "--stable-after" => {
                    cfg.stable_after = Duration::from_secs(num(&flag, &value()?)?);
                }
                "--reconnect-initial" => {
                    cfg.backoff.initial = Duration::from_millis(num(&flag, &value()?)?);
                }
                "--reconnect-max" => {
                    cfg.backoff.max = Duration::from_millis(num(&flag, &value()?)?);
                }
                "--log-level" => cfg.log_level = value()?,
                other => return Err(Error::Config(format!("unknown option {other:?}"))),
            }
        }
        cfg.validate()?;
        Ok(Cli::Run(Box::new(cfg)))
    }

    /// Checks the values against each other.
    pub fn validate(&self) -> Result<(), Error> {
        let bad = |m: &str| Err(Error::Config(m.to_string()));
        if self.period_frames == 0 {
            return bad("period must be at least 1 frame");
        }
        if self.max_period_frames < self.period_frames {
            return bad("max-period must not be below period");
        }
        if self.max_node_channels == 0 || self.max_node_channels > 64 {
            return bad("max-node-channels must be 1..=64");
        }
        if self.max_device_channels == 0 {
            return bad("max-device-channels must be at least 1");
        }
        if self.follower_chunk_frames == 0 {
            return bad("follower-chunk must be at least 1 frame");
        }
        if self.missing_queries == 0 {
            return bad("missing-queries must be at least 1");
        }
        if self.backoff.initial.is_zero() || self.backoff.max < self.backoff.initial {
            return bad("reconnect delays: need 0 < initial <= max");
        }
        if self.rt_priority > 99 {
            return bad("rt-priority must be 0..=99");
        }
        Ok(())
    }

    /// TX lead in frames for a graph period of `period` frames.
    pub fn tx_ahead_frames(&self, period: u32) -> u64 {
        u64::from(period) * u64::from(self.tx_ahead_periods)
            + u64::from(self.tx_ahead_margin_frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, Error> {
        Config::from_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_validate() {
        Config::default().validate().unwrap();
        let Cli::Run(c) = parse(&[]).unwrap() else {
            panic!()
        };
        assert_eq!(c.audio_socket, PathBuf::from(DEFAULT_AUDIO_SOCKET));
        assert_eq!(c.priority_driver, 500);
        assert_eq!(c.rt_priority, 20);
        assert!(c.mlock);
    }

    #[test]
    fn flags_override() {
        let Cli::Run(c) = parse(&[
            "--audio-socket",
            "/tmp/a",
            "--device",
            "x",
            "--device",
            "y",
            "--period",
            "128",
            "--resampler",
            "sinc-hq",
            "--positions",
            "aux",
            "--reconnect-initial",
            "10",
            "--no-mlock",
            "--rt-priority",
            "0",
            "--missing-queries",
            "5",
            "--overlay-stale",
            "0",
        ])
        .unwrap() else {
            panic!()
        };
        assert_eq!(c.audio_socket, PathBuf::from("/tmp/a"));
        assert_eq!(c.only_devices, ["x", "y"]);
        assert_eq!(c.period_frames, 128);
        assert_eq!(c.resampler, ResamplerQuality::SincHq);
        assert_eq!(c.positions, PositionMode::Aux);
        assert_eq!(c.backoff.initial, Duration::from_millis(10));
        assert!(!c.mlock);
        assert_eq!(c.rt_priority, 0);
        assert_eq!(c.missing_queries, 5);
        assert!(c.overlay_stale.is_zero());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["--period"]).is_err());
        assert!(parse(&["--period", "x"]).is_err());
        assert!(parse(&["--period", "0"]).is_err());
        assert!(parse(&["--max-node-channels", "65"]).is_err());
        assert!(parse(&["--missing-queries", "0"]).is_err());
        assert!(parse(&["--resampler", "best"]).is_err());
        assert!(matches!(parse(&["-h"]).unwrap(), Cli::Help));
    }

    #[test]
    fn static_devices_parse() {
        let Cli::Run(c) =
            parse(&["--static-device", "a:4:2", "--static-device", "b:1:0:96000"]).unwrap()
        else {
            panic!()
        };
        assert_eq!(c.static_devices[0].rx_labels.len(), 4);
        assert_eq!(c.static_devices[0].tx_labels, ["TX 1", "TX 2"]);
        assert_eq!(c.static_devices[1].rate, 96_000);
        assert!(parse(&["--static-device", "a:4"]).is_err());
        assert!(parse(&["--static-device", ":1:1"]).is_err());
        assert!(parse(&["--static-device", "a:x:1"]).is_err());
        assert!(parse(&["--static-device", "a:1:1:0"]).is_err());
        assert!(parse(&["--static-device", "a:999999999:1"]).is_err());
    }

    #[test]
    fn tx_ahead() {
        let c = Config::default();
        assert_eq!(c.tx_ahead_frames(256), 256 + 64);
    }
}
