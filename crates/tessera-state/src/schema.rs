//! Schema metadata: every configuration key with its default, unit, range,
//! apply class, restart scope and a short what-and-why.
//!
//! One table ([`KEYS`], grouped by [`SECTIONS`]) drives the generated
//! reference configuration ([`reference_config`], what `tesserad
//! --print-default-config` prints and the packages install as
//! `/etc/tessera/config.toml`), the schema table of `docs/configuration.md`
//! and the tests that keep both equal to the parser and to
//! [`crate::Config`]'s defaults.
//!
//! **Apply classes** ([`ApplyClass`]): `live` keys can also change at run
//! time through a command (and are then stored in `state.toml`, which wins
//! over the files unless the key is `locked`); `reload`
//! keys take effect on `SIGHUP` / `tesseractl reload`; `restart` keys only
//! when the daemon restarts (a reload reports them, see
//! [`crate::Effect::RestartKeysChanged`]).
//!
//! **Restart scope** (`tessera_control_types::RestartScope`): what a change
//! of the key restarts: nothing, the device, or the daemon.

use std::fmt::Write as _;

use tessera_control_types::RestartScope;

/// When a change of a key takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ApplyClass {
    /// Can also be changed at run time by a command (stored in
    /// `state.toml`, ADR 0008). A reload applies a changed file value
    /// unless a run-time value is stored in `state.toml` (the reload then
    /// warns); add the key to `locked` to make the file win.
    Live,
    /// Takes effect on a reload (`SIGHUP`, `tesseractl reload`).
    Reload,
    /// Takes effect at the next daemon start; a reload reports it.
    Restart,
}

impl ApplyClass {
    /// `live`, `reload` or `restart`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Reload => "reload",
            Self::Restart => "restart",
        }
    }
}

/// A key's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyDefault {
    /// The default, written as TOML.
    Value(&'static str),
    /// Not set by default (the documented fallback applies); `example` is
    /// a TOML value to show.
    Unset {
        /// An example value, as TOML.
        example: &'static str,
    },
    /// The default depends on the host (its name, its architecture);
    /// `example` is a typical value, as TOML.
    Host {
        /// An example value, as TOML.
        example: &'static str,
    },
    /// Required in every entry of an array section; `example` as TOML.
    Required {
        /// An example value, as TOML.
        example: &'static str,
    },
}

impl KeyDefault {
    /// The TOML text shown in the reference.
    #[must_use]
    pub fn shown(self) -> &'static str {
        match self {
            Self::Value(v)
            | Self::Unset { example: v }
            | Self::Host { example: v }
            | Self::Required { example: v } => v,
        }
    }
}

/// One key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySpec {
    /// The section (`daemon`, `defaults.flow`, `device`, ...).
    pub section: &'static str,
    /// The key inside its section (`log_level`, `multicast.allow_tx_multicast`).
    pub key: &'static str,
    /// The default.
    pub default: KeyDefault,
    /// Unit (`ms`, `bytes`, ...), empty if none.
    pub unit: &'static str,
    /// Accepted values, for people.
    pub range: &'static str,
    /// When a change takes effect.
    pub apply: ApplyClass,
    /// What a change restarts.
    pub scope: RestartScope,
    /// What it does and why you might change it.
    pub doc: &'static str,
}

impl KeySpec {
    /// `section.key`.
    #[must_use]
    pub fn path(&self) -> String {
        format!("{}.{}", self.section, self.key)
    }
}

/// One section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionSpec {
    /// The table name (`daemon`, `defaults.flow`, `device`).
    pub name: &'static str,
    /// An array of tables (`[[device]]`) rather than a table.
    pub array: bool,
    /// What the section is for.
    pub doc: &'static str,
}

/// The sections, in reference order (tables first, arrays last, as TOML
/// needs).
pub const SECTIONS: &[SectionSpec] = &[
    SectionSpec {
        name: "daemon",
        array: false,
        doc: "The process: logging, file locations, shutdown, real-time scheduling.",
    },
    SectionSpec {
        name: "control",
        array: false,
        doc: "The local control socket (`control.sock`) used by tesseractl and the web portal: frame size, connection limits, timeouts, rate limits.",
    },
    SectionSpec {
        name: "audio",
        array: false,
        doc: "Audio clients on `audio.sock` (PipeWire, ALSA, JACK): how many, and how much shared ring memory they may use.",
    },
    SectionSpec {
        name: "network",
        array: false,
        doc: "Network interfaces.",
    },
    SectionSpec {
        name: "clock",
        array: false,
        doc: "The PTP clock. It drives a media clock only; the system clock is never steered.",
    },
    SectionSpec {
        name: "engine",
        array: false,
        doc: "The media engine's real-time threads and packet batching. These size real-time structures, so they apply at the next start.",
    },
    SectionSpec {
        name: "discovery",
        array: false,
        doc: "mDNS advertisement of the logical devices.",
    },
    SectionSpec {
        name: "backends",
        array: false,
        doc: "Which audio-client backends may attach, and the resampler.",
    },
    SectionSpec {
        name: "aes67",
        array: false,
        doc: "AES67 (Phase 5).",
    },
    SectionSpec {
        name: "defaults.flow",
        array: false,
        doc: "Defaults for every [[flow]] entry (each entry can override them). Applied when a flow (re)starts.",
    },
    SectionSpec {
        name: "defaults.bridge",
        array: false,
        doc: "Defaults for every [[bridge]] entry (each entry can override them). Two waits, both only after an error, never in steady state. error_backoff*: when a card is missing, busy, unplugged or keeps failing, the bridge manager (a normal-priority thread) closes it and waits before reopening it, so a failing card is not reopened (and its real-time thread restarted) in a tight loop; the wait starts at error_backoff_min_ms and doubles up to error_backoff_max_ms, and a run of 10 s resets it. error_sleep*: while the card stays open, the real-time bridge thread pauses after a step that met an xrun or a card error, so a card in trouble cannot make it spin (polling the card cannot replace the pause: the card reports an error at once in exactly those states); the pause starts at error_sleep_min_ms and doubles up to error_sleep_max_ms while the trouble lasts, and the first clean step resets it. Changes apply at the bridge's next error.",
    },
    SectionSpec {
        name: "device",
        array: true,
        doc: "A logical device as it appears on the network. Several are allowed; the defaults below apply to each. Values marked live can also be changed at run time and are then stored in state.toml (pin them with `locked`).",
    },
    SectionSpec {
        name: "bridge",
        array: true,
        doc: "An in-daemon ALSA hardware-card bridge: moves audio between a card and a contiguous channel range of a device, compensating the card's clock drift.",
    },
    SectionSpec {
        name: "flow",
        array: true,
        doc: "PROVISIONAL static audio flow between two tessera instances (test scaffolding until flow control exists).",
    },
];

use ApplyClass::{Live, Reload, Restart};
use KeyDefault::{Host, Required, Unset, Value};

const NONE: RestartScope = RestartScope::None;
const DEVICE: RestartScope = RestartScope::Device;
const DAEMON: RestartScope = RestartScope::Daemon;

macro_rules! k {
    ($s:literal, $k:literal, $d:expr, $u:literal, $r:literal, $a:expr, $sc:expr, $doc:literal) => {
        KeySpec {
            section: $s,
            key: $k,
            default: $d,
            unit: $u,
            range: $r,
            apply: $a,
            scope: $sc,
            doc: $doc,
        }
    };
}

/// Every key, by section in [`SECTIONS`] order.
pub const KEYS: &[KeySpec] = &[
    // [daemon]
    k!(
        "daemon",
        "log_level",
        Value("\"info\""),
        "",
        "error|warn|info|debug|trace or target=level,...",
        Reload,
        NONE,
        "Log filter. Raise to debug to diagnose a problem. --log-level overrides it, and the RUST_LOG environment variable overrides both (and is not changed by a reload)."
    ),
    k!(
        "daemon",
        "state_dir",
        Value("\"/var/lib/tessera\""),
        "",
        "absolute path",
        Restart,
        DAEMON,
        "Where state.toml (values changed at run time, device identities) is kept."
    ),
    k!(
        "daemon",
        "runtime_dir",
        Value("\"/run/tessera\""),
        "",
        "absolute path",
        Restart,
        DAEMON,
        "Where control.sock and audio.sock are created."
    ),
    k!(
        "daemon",
        "admin_group",
        Value("\"tessera-admin\""),
        "",
        "group name",
        Reload,
        NONE,
        "Members of this group may change settings through the control socket; others may only read."
    ),
    k!(
        "daemon",
        "shutdown_timeout_secs",
        Value("20"),
        "s",
        "1..=4294967295",
        Reload,
        NONE,
        "Time allowed for the final state flush and shutdown. Keep the service manager's stop timeout above it."
    ),
    k!(
        "daemon",
        "persist_delay_ms",
        Value("500"),
        "ms",
        "0..=4294967295000",
        Reload,
        NONE,
        "Debounce window for writing state.toml after a change. Longer means fewer writes on flash storage; a crash inside the window loses the change."
    ),
    k!(
        "daemon",
        "realtime",
        Value("true"),
        "",
        "true|false",
        Restart,
        DAEMON,
        "Request SCHED_FIFO priorities and lock memory as [engine] says. false never does (development, containers without the capability); audio then has no timing guarantees."
    ),
    // [control]
    k!(
        "control",
        "max_frame_bytes",
        Value("1048576"),
        "bytes",
        "65536..=4294967295",
        Reload,
        NONE,
        "Largest request or response on the control socket. Clients learn it from the status. Raise it if a status with very many devices or channels is refused as too large."
    ),
    k!(
        "control",
        "max_connections",
        Value("256"),
        "",
        ">= 1",
        Reload,
        NONE,
        "Most simultaneous control connections (all users)."
    ),
    k!(
        "control",
        "max_connections_per_uid",
        Value("16"),
        "",
        ">= 1",
        Reload,
        NONE,
        "Most control connections one user may hold, so one user cannot use up the slots."
    ),
    k!(
        "control",
        "admin_reserved_connections",
        Value("16"),
        "",
        "< max_connections",
        Reload,
        NONE,
        "Connection slots only administrators may use, so other users cannot lock them out."
    ),
    k!(
        "control",
        "idle_timeout_secs",
        Value("60"),
        "s",
        "1..=4294967295",
        Reload,
        NONE,
        "A request/response connection silent this long is closed (event subscribers are exempt)."
    ),
    k!(
        "control",
        "event_queue",
        Value("256"),
        "events",
        ">= 1; event_queue x max_connections x event size <= 2 GiB",
        Reload,
        NONE,
        "Events queued per subscriber before the oldest are dropped (the subscriber is told how many)."
    ),
    k!(
        "control",
        "rate_burst",
        Value("256"),
        "requests",
        ">= 1",
        Reload,
        NONE,
        "Requests a peer may send back to back before the rate limit applies."
    ),
    k!(
        "control",
        "rate_per_second",
        Value("128"),
        "requests/s",
        ">= 1",
        Reload,
        NONE,
        "Requests per second a peer may sustain."
    ),
    k!(
        "control",
        "frame_timeout_secs",
        Value("10"),
        "s",
        "1..=4294967295",
        Reload,
        NONE,
        "Time a peer may take to finish a frame, counted from its first byte (trickling bytes does not extend it)."
    ),
    k!(
        "control",
        "write_timeout_secs",
        Value("5"),
        "s",
        "1..=4294967295",
        Reload,
        NONE,
        "Time to wait for a peer that does not read its responses before closing it."
    ),
    k!(
        "control",
        "stream_write_timeout_secs",
        Value("30"),
        "s",
        "1..=4294967295",
        Reload,
        NONE,
        "Time to wait for a stalled event subscriber before dropping it."
    ),
    k!(
        "control",
        "threads",
        Value("2"),
        "",
        ">= 1",
        Restart,
        DAEMON,
        "Worker threads of the control-plane runtime (not the audio threads)."
    ),
    k!(
        "control",
        "socket_group",
        Unset {
            example: "\"tessera\""
        },
        "",
        "group name or number",
        Restart,
        DAEMON,
        "Group that owns the sockets. Unset: the group tessera if it exists (a warning otherwise); a group set here that cannot be applied is an error."
    ),
    k!(
        "control",
        "socket_mode",
        Value("0o660"),
        "octal",
        "0o000..=0o777, written in octal (0o660 or \"660\"; a decimal number is refused)",
        Restart,
        DAEMON,
        "Permission bits of control.sock and audio.sock. Write them in octal: socket_mode = 416 would be a different mode, so a decimal integer is an error."
    ),
    // [audio]
    k!(
        "audio",
        "socket",
        Value("\"auto\""),
        "",
        "auto|on|off",
        Restart,
        DAEMON,
        "Listen on audio.sock. auto: if [backends] enabled names a client backend."
    ),
    k!(
        "audio",
        "max_clients",
        Value("64"),
        "",
        ">= 1",
        Reload,
        NONE,
        "Most simultaneous audio client connections. Applies to new clients."
    ),
    k!(
        "audio",
        "max_clients_per_uid",
        Value("8"),
        "",
        ">= 1",
        Reload,
        NONE,
        "Most audio client connections one user may hold."
    ),
    k!(
        "audio",
        "default_period_frames",
        Value("1024"),
        "frames",
        "1..=max_period_frames",
        Reload,
        NONE,
        "Client period assumed when a client does not state one; sizes its ring."
    ),
    k!(
        "audio",
        "max_period_frames",
        Value("16384"),
        "frames",
        ">= 1",
        Reload,
        NONE,
        "Largest period a client may ask for."
    ),
    k!(
        "audio",
        "ring_budget_mib",
        Value("256"),
        "MiB",
        ">= 1",
        Reload,
        NONE,
        "Shared ring memory all audio clients together may reserve. Also the budget a channel-count change is checked against. Keep it under RLIMIT_MEMLOCK when memory is locked."
    ),
    k!(
        "audio",
        "max_ring_mib",
        Value("64"),
        "MiB",
        "1..=ring_budget_mib",
        Reload,
        NONE,
        "Largest single client ring. Many channels at long latencies need more. At most ring_budget_mib."
    ),
    k!(
        "audio",
        "reorder_window_us",
        Value("10000"),
        "us",
        ">= 0",
        Reload,
        NONE,
        "Extra ring capacity for packets that arrive out of order."
    ),
    // [network]
    k!(
        "network",
        "interfaces",
        Value("[]"),
        "",
        "interface names",
        Reload,
        DEVICE,
        "Interfaces for audio and clock; the first is the primary. Empty: the first wired interface with a carrier and an IPv4 address."
    ),
    k!(
        "network",
        "secondary",
        Unset {
            example: "\"enp4s0\""
        },
        "",
        "interface name",
        Reload,
        DEVICE,
        "Redundancy interface (Phase 6)."
    ),
    k!(
        "network",
        "allow_wireless",
        Value("false"),
        "",
        "true|false",
        Reload,
        DEVICE,
        "Allow Wi-Fi interfaces for media. Wi-Fi timing is poor; use it only for tests."
    ),
    k!(
        "network",
        "mtu_payload",
        Value("1400"),
        "bytes",
        "1..=65535",
        Reload,
        DEVICE,
        "Largest media payload per packet."
    ),
    k!(
        "network",
        "locked",
        Value("[]"),
        "",
        "primary",
        Reload,
        NONE,
        "Values that run-time commands may not change (primary: the runtime interface selection)."
    ),
    // [clock]
    k!(
        "clock",
        "enabled",
        Value("true"),
        "",
        "true|false",
        Restart,
        DAEMON,
        "Run PTP. false leaves the media clock free-running (--no-ptp); devices then cannot sync with others. Read at start only."
    ),
    k!(
        "clock",
        "role",
        Value("\"follower\""),
        "",
        "follower|leader|auto",
        Live,
        NONE,
        "Clock role. auto: leader-capable but follower-preferred."
    ),
    k!(
        "clock",
        "priority",
        Value("128"),
        "",
        "0..=255 (lower wins)",
        Live,
        NONE,
        "Leader-election priority."
    ),
    k!(
        "clock",
        "timestamping",
        Value("\"auto\""),
        "",
        "auto|hardware|software",
        Reload,
        NONE,
        "PTP timestamps: hardware if the interface supports it, else software."
    ),
    k!(
        "clock",
        "free_running_tx",
        Value("false"),
        "",
        "true|false",
        Reload,
        NONE,
        "Transmit while the clock is not locked."
    ),
    k!(
        "clock",
        "locked",
        Value("[]"),
        "",
        "role, priority",
        Reload,
        NONE,
        "Values that run-time commands may not change."
    ),
    // [engine]
    k!(
        "engine",
        "workers",
        Value("1"),
        "",
        ">= 1",
        Restart,
        DAEMON,
        "Media worker threads."
    ),
    k!(
        "engine",
        "tick_us",
        Value("\"auto\""),
        "us",
        "auto or >= 1",
        Restart,
        DAEMON,
        "Engine tick: how often audio clients and the receive drain wake (auto: 1000)."
    ),
    k!(
        "engine",
        "rt_priority",
        Value("80"),
        "",
        "1..=99",
        Restart,
        DAEMON,
        "SCHED_FIFO priority of the media threads."
    ),
    k!(
        "engine",
        "ptp_rt_priority",
        Value("70"),
        "",
        "1..=99",
        Restart,
        DAEMON,
        "SCHED_FIFO priority of the clock thread."
    ),
    k!(
        "engine",
        "cpu_affinity",
        Value("[]"),
        "",
        "CPU numbers",
        Restart,
        DAEMON,
        "CPUs to pin the real-time threads to (isolated cores give the best timing)."
    ),
    k!(
        "engine",
        "mlock",
        Value("true"),
        "",
        "true|false",
        Restart,
        DAEMON,
        "Lock memory once the real-time objects exist, so page faults cannot stall audio."
    ),
    k!(
        "engine",
        "rx_batch",
        Value("32"),
        "datagrams",
        "1..=1024 (UIO_MAXIOV)",
        Restart,
        DAEMON,
        "Datagrams per recvmmsg call. The kernel takes at most UIO_MAXIOV (1024) per call; each one is a preallocated, memory-locked buffer of one packet."
    ),
    k!(
        "engine",
        "rx_max_batches",
        Value("8"),
        "",
        ">= 1",
        Restart,
        DAEMON,
        "Receive batches per socket per tick (bounds the work of one tick)."
    ),
    k!(
        "engine",
        "tx_batch",
        Value("32"),
        "datagrams",
        "1..=1024 (UIO_MAXIOV)",
        Restart,
        DAEMON,
        "Datagrams per sendmmsg call. The kernel takes at most UIO_MAXIOV (1024) per call."
    ),
    k!(
        "engine",
        "tx_max_per_tick",
        Value("32"),
        "packets",
        ">= 1",
        Restart,
        DAEMON,
        "Packets per transmit flow per tick (bounds the work of one tick)."
    ),
    k!(
        "engine",
        "socket_buffer_kib",
        Value("2048"),
        "KiB",
        "1..=2097151",
        Restart,
        DAEMON,
        "Kernel socket buffer of media sockets. Raise it if the receive-drop counters grow; the sysctl ceilings (net.core.rmem_max) cap it."
    ),
    k!(
        "engine",
        "rx_margin_us",
        Value("250"),
        "us",
        ">= 0",
        Restart,
        DAEMON,
        "Added to the receive latency published to audio clients on top of one tick (wake-up jitter allowance)."
    ),
    // [discovery]
    k!(
        "discovery",
        "enabled",
        Value("true"),
        "",
        "true|false",
        Restart,
        DAEMON,
        "Discovery at all. false advertises nothing whatever advertise_devices says (--no-discovery). Read at start only."
    ),
    k!(
        "discovery",
        "mdns",
        Value("\"auto\""),
        "",
        "auto|avahi|builtin|off",
        Reload,
        NONE,
        "mDNS implementation: auto uses Avahi if it is on the bus, else the built-in responder."
    ),
    k!(
        "discovery",
        "advertise_devices",
        Value("false"),
        "",
        "true|false",
        Reload,
        NONE,
        "Advertise the logical devices. Off by default: the records name control ports nothing listens on yet."
    ),
    // [backends]
    k!(
        "backends",
        "enabled",
        Value("[\"pipewire\", \"alsa\"]"),
        "",
        "pipewire, alsa, jack",
        Restart,
        DAEMON,
        "Audio-client backends that may attach."
    ),
    k!(
        "backends",
        "resampler",
        Value("\"auto\""),
        "",
        "off|linear|cubic|sinc-fast|sinc-hq|auto",
        Reload,
        NONE,
        "Resampler quality (auto: per platform)."
    ),
    // [aes67]
    k!(
        "aes67",
        "enabled",
        Value("false"),
        "",
        "true|false",
        Restart,
        DAEMON,
        "Enable AES67 (Phase 5)."
    ),
    // [defaults.flow]
    k!(
        "defaults.flow",
        "rx_keepalive_ms",
        Value("250"),
        "ms",
        ">= 0",
        Reload,
        NONE,
        "Interval of the receiver keepalives every rx flow sends to its source (a unicast transmitter stops without them); 0 disables them. PROVISIONAL."
    ),
    k!(
        "defaults.flow",
        "rx_keepalive_timeout_ms",
        Value("10000"),
        "ms",
        ">= 0",
        Reload,
        NONE,
        "Stop an rx flow's keepalives after this long without media (they resume with the next packet); 0: never stop."
    ),
    k!(
        "defaults.flow",
        "rx_keepalive_payload",
        Value("\"empty\""),
        "",
        "\"empty\" or \"1337\" (strings; the integer 1337 is accepted too)",
        Reload,
        NONE,
        "What a keepalive carries: empty (a zero-length datagram, as hardware receivers send) or 1337 (the bytes 13 37). PROVISIONAL."
    ),
    // [defaults.bridge]
    k!(
        "defaults.bridge",
        "error_backoff",
        Value("true"),
        "",
        "true|false",
        Reload,
        NONE,
        "Double the wait after each failed reopen. false: always wait error_backoff_min_ms (there is never no wait)."
    ),
    k!(
        "defaults.bridge",
        "error_backoff_min_ms",
        Value("500"),
        "ms",
        "10..=error_backoff_max_ms",
        Reload,
        NONE,
        "First wait of the bridge manager before it reopens a failed card. The floor of 10 ms stops a failing card being reopened (and its real-time thread restarted) in a tight loop; the manager itself is a normal-priority thread."
    ),
    k!(
        "defaults.bridge",
        "error_backoff_max_ms",
        Value("30000"),
        "ms",
        "error_backoff_min_ms..=600000",
        Reload,
        NONE,
        "Longest wait between reopens. Lower it to recover sooner from a card that comes back."
    ),
    k!(
        "defaults.bridge",
        "error_sleep",
        Value("true"),
        "",
        "true|false",
        Reload,
        NONE,
        "Double the bridge thread's pause on each further step with an xrun or card error. false: always pause error_sleep_min_ms (there is never no pause). The pause is the one system call the real-time bridge thread makes besides the card's I/O and the clock."
    ),
    k!(
        "defaults.bridge",
        "error_sleep_min_ms",
        Value("2"),
        "ms",
        "1..=error_sleep_max_ms",
        Reload,
        NONE,
        "First pause of the bridge thread after a step with an xrun or card error. The floor of 1 ms keeps a card in trouble from making the real-time thread spin."
    ),
    k!(
        "defaults.bridge",
        "error_sleep_max_ms",
        Value("64"),
        "ms",
        "error_sleep_min_ms..=1000",
        Reload,
        NONE,
        "Longest pause of the bridge thread while the trouble lasts. Higher values use less CPU on a broken card but make an xrun recover more slowly (the card is serviced again only after the pause)."
    ),
    // [[device]]
    k!(
        "device",
        "id",
        Required {
            example: "\"main\""
        },
        "",
        "letters, digits, _ and -",
        Reload,
        NONE,
        "Local key of the device (not its network identity). Required. Changing it makes a new device."
    ),
    k!(
        "device",
        "name",
        Host {
            example: "\"studio-pc\""
        },
        "",
        "1..=31 bytes, no control characters (PROVISIONAL: the size of the name on the wire, OQ-CFG-2)",
        Live,
        NONE,
        "Name on the network. Default: the host name (<host>-<id> with several devices). The 31-byte limit is the wire size of the name as far as known; PROVISIONAL until a capture confirms it (OQ-CFG-2)."
    ),
    k!(
        "device",
        "rx_channels",
        Value("2"),
        "channels",
        "0..=65535 (not both 0)",
        Live,
        DEVICE,
        "Receive channels. Changing it restarts the device."
    ),
    k!(
        "device",
        "tx_channels",
        Value("2"),
        "channels",
        "0..=65535 (not both 0)",
        Live,
        DEVICE,
        "Transmit channels. Changing it restarts the device."
    ),
    k!(
        "device",
        "sample_rate",
        Value("48000"),
        "Hz",
        "44100, 48000, 88200, 96000, 176400, 192000",
        Live,
        DEVICE,
        "Sample rate. Changing it restarts the device."
    ),
    k!(
        "device",
        "encoding",
        Value("24"),
        "bits",
        "16, 24, 32",
        Live,
        DEVICE,
        "Bits per sample on the wire."
    ),
    k!(
        "device",
        "latency_us",
        Host { example: "4000" },
        "us",
        "within latency_bounds_us",
        Live,
        DEVICE,
        "Receive latency. Default: 4000 on x86, 10000 on ARM."
    ),
    k!(
        "device",
        "latency_bounds_us",
        Host {
            example: "[1000, 40000]"
        },
        "us",
        "[min, max]",
        Reload,
        NONE,
        "Range a run-time latency change may use."
    ),
    k!(
        "device",
        "allow_unsafe_latency",
        Value("false"),
        "",
        "true|false",
        Reload,
        NONE,
        "Accept a latency below this platform's timing floor (with a warning)."
    ),
    k!(
        "device",
        "locked",
        Value("[]"),
        "",
        "name, sample_rate, encoding, latency, rx_channel_labels, tx_channel_labels, rx_channels, tx_channels",
        Reload,
        NONE,
        "Values that run-time commands may not change; the configured value always wins."
    ),
    k!(
        "device",
        "rx_channel_labels",
        Value("{}"),
        "",
        "{ <channel> = \"label\" }",
        Live,
        NONE,
        "Receive channel labels (default RX n)."
    ),
    k!(
        "device",
        "tx_channel_labels",
        Value("{}"),
        "",
        "{ <channel> = \"label\" }",
        Live,
        NONE,
        "Transmit channel labels (default TX n)."
    ),
    k!(
        "device",
        "multicast.allow_tx_multicast",
        Value("true"),
        "",
        "true|false",
        Reload,
        NONE,
        "May this device transmit multicast flows."
    ),
    // [[bridge]]
    k!(
        "bridge",
        "device",
        Required {
            example: "\"main\""
        },
        "",
        "a device id",
        Reload,
        DEVICE,
        "The device this bridge belongs to. Required."
    ),
    k!(
        "bridge",
        "alsa_pcm",
        Required {
            example: "\"hw:CARD=<card>,DEV=0\""
        },
        "",
        "ALSA PCM name",
        Reload,
        DEVICE,
        "The card. Required."
    ),
    k!(
        "bridge",
        "direction",
        Value("\"playback\""),
        "",
        "playback|capture",
        Reload,
        DEVICE,
        "playback: network to card; capture: card to network."
    ),
    k!(
        "bridge",
        "name",
        Unset {
            example: "\"main-playback\""
        },
        "",
        "letters, digits, _ and -",
        Reload,
        DEVICE,
        "Unique name. Unset: <device>-<direction>."
    ),
    k!(
        "bridge",
        "enabled",
        Value("true"),
        "",
        "true|false",
        Reload,
        DEVICE,
        "false keeps the entry but leaves the card alone."
    ),
    k!(
        "bridge",
        "first_channel",
        Value("1"),
        "",
        ">= 1",
        Reload,
        DEVICE,
        "First device channel carried; card channel 1 is this one."
    ),
    k!(
        "bridge",
        "channels",
        Unset { example: "2" },
        "channels",
        ">= 1",
        Reload,
        DEVICE,
        "Channels carried. Unset: the rest of the device's channels from first_channel on."
    ),
    k!(
        "bridge",
        "sample_rate",
        Unset { example: "48000" },
        "Hz",
        "the device's rate",
        Reload,
        DEVICE,
        "The card's rate. Unset: follow the device."
    ),
    k!(
        "bridge",
        "period_frames",
        Unset { example: "480" },
        "frames",
        ">= 1",
        Reload,
        DEVICE,
        "Frames per card period. Unset: 10 ms at the device's rate."
    ),
    k!(
        "bridge",
        "buffer_frames",
        Unset { example: "1920" },
        "frames",
        ">= 2 periods",
        Reload,
        DEVICE,
        "Card buffer. Unset: four periods."
    ),
    k!(
        "bridge",
        "resampler",
        Value("\"auto\""),
        "",
        "off|linear|cubic|sinc-fast|sinc-hq|auto",
        Reload,
        DEVICE,
        "Resampler quality; auto takes [backends] resampler; off disables drift compensation."
    ),
    k!(
        "bridge",
        "time_constant_s",
        Value("120.0"),
        "s",
        "> 0",
        Reload,
        DEVICE,
        "Time constant of the drift loop. Slow on purpose so it cannot fight a second servo downstream."
    ),
    k!(
        "bridge",
        "max_correction_ppm",
        Value("200.0"),
        "ppm",
        "0..=3000 (the resampler's ratio range, tessera-backend-alsa MAX_CORRECTION)",
        Reload,
        DEVICE,
        "Largest correction the loop may apply. The ceiling is what the bridge's resampler can do (its ratio range, MAX_CORRECTION in tessera-backend-alsa), not a policy."
    ),
    k!(
        "bridge",
        "rt_priority",
        Value("75"),
        "",
        "0..=99 (0: normal scheduling)",
        Reload,
        DEVICE,
        "SCHED_FIFO priority of the bridge thread."
    ),
    k!(
        "bridge",
        "cpu",
        Unset { example: "2" },
        "",
        "a CPU number",
        Reload,
        DEVICE,
        "CPU to pin the bridge thread to. Unset: not pinned."
    ),
    k!(
        "bridge",
        "error_backoff",
        Unset { example: "true" },
        "",
        "true|false",
        Reload,
        NONE,
        "Override of [defaults.bridge] error_backoff for this bridge."
    ),
    k!(
        "bridge",
        "error_backoff_min_ms",
        Unset { example: "500" },
        "ms",
        "10..=error_backoff_max_ms",
        Reload,
        NONE,
        "Override of [defaults.bridge] error_backoff_min_ms for this bridge."
    ),
    k!(
        "bridge",
        "error_backoff_max_ms",
        Unset { example: "30000" },
        "ms",
        "error_backoff_min_ms..=600000",
        Reload,
        NONE,
        "Override of [defaults.bridge] error_backoff_max_ms for this bridge."
    ),
    k!(
        "bridge",
        "error_sleep",
        Unset { example: "true" },
        "",
        "true|false",
        Reload,
        NONE,
        "Override of [defaults.bridge] error_sleep for this bridge."
    ),
    k!(
        "bridge",
        "error_sleep_min_ms",
        Unset { example: "2" },
        "ms",
        "1..=error_sleep_max_ms",
        Reload,
        NONE,
        "Override of [defaults.bridge] error_sleep_min_ms for this bridge."
    ),
    k!(
        "bridge",
        "error_sleep_max_ms",
        Unset { example: "64" },
        "ms",
        "error_sleep_min_ms..=1000",
        Reload,
        NONE,
        "Override of [defaults.bridge] error_sleep_max_ms for this bridge."
    ),
    // [[flow]]
    k!(
        "flow",
        "id",
        Required {
            example: "\"rx-a\""
        },
        "",
        "unique name",
        Reload,
        DEVICE,
        "Name of the flow (local, never on the wire). Required."
    ),
    k!(
        "flow",
        "direction",
        Required { example: "\"rx\"" },
        "",
        "rx|tx",
        Reload,
        DEVICE,
        "rx: network to device; tx: device to network. Required."
    ),
    k!(
        "flow",
        "device",
        Required {
            example: "\"main\""
        },
        "",
        "a device id",
        Reload,
        DEVICE,
        "The device the flow belongs to. Required."
    ),
    k!(
        "flow",
        "port",
        Required { example: "4321" },
        "",
        "1..=65535",
        Reload,
        DEVICE,
        "tx: destination UDP port; rx: local UDP port. Required."
    ),
    k!(
        "flow",
        "channels",
        Value("[]"),
        "",
        "device channels",
        Reload,
        DEVICE,
        "Device channels carried, in wire order. Empty: every channel in that direction."
    ),
    k!(
        "flow",
        "peer",
        Unset {
            example: "\"192.0.2.10\""
        },
        "",
        "IPv4 address",
        Reload,
        DEVICE,
        "tx: destination (required); rx: accept packets only from this address."
    ),
    k!(
        "flow",
        "interface",
        Unset {
            example: "\"enp3s0\""
        },
        "",
        "interface name",
        Reload,
        DEVICE,
        "Interface to use. Unset: the primary."
    ),
    k!(
        "flow",
        "local_address",
        Unset {
            example: "\"192.0.2.20\""
        },
        "",
        "IPv4 address",
        Reload,
        DEVICE,
        "Local address to bind. Unset: the interface's first address."
    ),
    k!(
        "flow",
        "format",
        Value("\"reference\""),
        "",
        "reference",
        Reload,
        DEVICE,
        "Packet format (provisional)."
    ),
    k!(
        "flow",
        "frames_per_packet",
        Unset { example: "32" },
        "frames",
        ">= 1",
        Reload,
        DEVICE,
        "tx: frames per packet. Unset: a packet of about 667 us, rounded to frames and reduced to fit the MTU."
    ),
    k!(
        "flow",
        "send_offset_us",
        Value("0"),
        "us",
        ">= 0",
        Reload,
        DEVICE,
        "tx: extra delay before a packet is sent."
    ),
    k!(
        "flow",
        "any_source",
        Value("false"),
        "",
        "true|false",
        Reload,
        DEVICE,
        "rx: accept datagrams from any source (off: only from peer, so others cannot inject audio)."
    ),
    k!(
        "flow",
        "dscp",
        Value("46"),
        "",
        "0..=63",
        Reload,
        DEVICE,
        "tx: DSCP of the media packets (46, EF)."
    ),
    k!(
        "flow",
        "max_lag_us",
        Unset { example: "2000" },
        "us",
        ">= 1",
        Reload,
        DEVICE,
        "tx: how late a packet may still be sent before the flow skips forward. Unset: half the latency."
    ),
    k!(
        "flow",
        "rx_keepalive_ms",
        Unset { example: "250" },
        "ms",
        ">= 0",
        Reload,
        DEVICE,
        "rx: override of [defaults.flow] rx_keepalive_ms."
    ),
    k!(
        "flow",
        "rx_keepalive_timeout_ms",
        Unset { example: "10000" },
        "ms",
        ">= 0",
        Reload,
        DEVICE,
        "rx: override of [defaults.flow] rx_keepalive_timeout_ms."
    ),
    k!(
        "flow",
        "rx_keepalive_payload",
        Unset {
            example: "\"empty\""
        },
        "",
        "\"empty\" or \"1337\" (strings; the integer 1337 is accepted too)",
        Reload,
        DEVICE,
        "rx: override of [defaults.flow] rx_keepalive_payload."
    ),
];

/// The key a path names, if the schema has it. Accepts the paths that
/// validation reports: `section.key`, entries of array sections with an
/// index (`device[1].rx_channels`, `bridge[0].error_backoff_min_ms`), list
/// elements (`network.interfaces[0]`, `device[0].locked[2]`), entries of a
/// map (`device[0].rx_channel_labels.5`) and the persisted form
/// `device.<id>.<key>`.
#[must_use]
pub fn key(path: &str) -> Option<&'static KeySpec> {
    // Drop every `[...]`.
    let mut plain = String::with_capacity(path.len());
    let mut depth = 0u32;
    for c in path.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
    }
    let found = |p: &str| {
        KEYS.iter().find(|k| {
            p.strip_prefix(k.section)
                .and_then(|r| r.strip_prefix('.'))
                .is_some_and(|r| {
                    r == k.key || r.strip_prefix(k.key).is_some_and(|x| x.starts_with('.'))
                })
        })
    };
    // A key's own path (`device.multicast.allow_tx_multicast`).
    if let Some(k) = KEYS.iter().find(|k| k.path() == plain) {
        return Some(k);
    }
    // The persisted form `device.<id>.<key>` of an array section (no index,
    // three or more parts): drop the id first, whatever it is called (an
    // id may be `name`, `id` or `locked`).
    if let Some((sec, rest)) = plain.split_once('.') {
        if !path.contains('[') && section(sec).is_some_and(|s| s.array) {
            let (_, key) = rest.split_once('.')?;
            return found(&format!("{sec}.{key}"));
        }
    }
    found(&plain)
}

/// The section called `name`.
#[must_use]
pub fn section(name: &str) -> Option<&'static SectionSpec> {
    SECTIONS.iter().find(|s| s.name == name)
}

fn wrap(out: &mut String, text: &str) {
    let mut line = String::from("#");
    for word in text.split_whitespace() {
        if line.len() + 1 + word.len() > 78 && line.len() > 1 {
            out.push_str(&line);
            out.push('\n');
            line = String::from("#");
        }
        line.push(' ');
        line.push_str(word);
    }
    if line.len() > 1 {
        out.push_str(&line);
        out.push('\n');
    }
}

/// The header of the reference configuration.
pub const REFERENCE_HEADER: &str = "\
# tessera configuration: every key, commented out at its default.
#
# Keep this file as shipped: local edits here conflict with package
# upgrades. Put your changes in /etc/tessera/config.d/*.toml, read after
# this file in lexical order; a later file wins. Merging: tables merge key
# by key; plain values and plain arrays (interfaces, cpu_affinity, ...)
# replace, except `locked` lists, which add up; [[device]] entries merge
# by `id` (a drop-in cannot remove a device); [[bridge]] and [[flow]]
# lists are replaced as a whole by a file that has any. The configuration
# guide (/usr/share/doc/tessera/configuration.md where installed,
# docs/configuration.md in the sources)
# explains precedence, drop-ins and locks.
#
# Generated from the schema by `tesserad --print-default-config`.
#
# apply: live = can also be changed at run time (tesseractl, the web portal,
# a network controller); the run-time value is stored in state.toml and
# wins over the files, unless the setting is pinned with `locked`. Only
# some settings can be pinned: in [[device]] name, sample_rate, encoding,
# latency, rx_channels, tx_channels and the channel labels; in [clock]
# role and priority; in [network] the primary interface. Every other key
# has no run-time form and no lock. `tesseractl config show` prints the
# merged files.
# reload = takes effect on SIGHUP (systemctl reload tesserad) or
# `tesseractl config reload`. restart = needs a daemon restart.
# restart scope: what a change interrupts (none; the device: its flows,
# bridges and audio clients restart; the daemon: everything). tesseractl
# and the web portal ask for confirmation before a change that interrupts
# something (tesseractl --yes skips the question); a reload by SIGHUP does
# not ask.
#
# Unknown keys are errors: a daemon older than a key refuses the file, so
# upgrade the daemon before using new keys.
";

/// Render the full commented reference configuration: [`REFERENCE_HEADER`],
/// then every section and key of the schema, each key commented out at its
/// default with a short what-and-why and its unit, range, apply class and
/// restart scope. Uncommenting a key line gives a valid configuration, and
/// uncommenting every key that has a plain default gives exactly the
/// built-in defaults (tests check both).
///
/// Key lines start with `#` directly followed by the key (`#log_level =
/// "info"`); comment lines start with `# `.
#[must_use]
pub fn reference_config() -> String {
    let mut out = String::from(REFERENCE_HEADER);
    for s in SECTIONS {
        out.push('\n');
        if s.array {
            let _ = writeln!(out, "#[[{}]]", s.name);
        } else {
            let _ = writeln!(out, "#[{}]", s.name);
        }
        wrap(&mut out, s.doc);
        for k in KEYS.iter().filter(|k| k.section == s.name) {
            out.push_str("#\n");
            wrap(&mut out, k.doc);
            let mut meta = String::new();
            if !k.unit.is_empty() {
                let _ = write!(meta, "unit: {}; ", k.unit);
            }
            let _ = write!(
                meta,
                "range: {}; apply: {}; restart scope: {}",
                k.range,
                k.apply.as_str(),
                k.scope.as_str()
            );
            match k.default {
                KeyDefault::Value(_) => {}
                KeyDefault::Unset { .. } => meta.push_str("; unset by default, example:"),
                KeyDefault::Host { .. } => meta.push_str("; default depends on the host, typical:"),
                KeyDefault::Required { .. } => meta.push_str("; required, example:"),
            }
            wrap(&mut out, &format!("({meta})"));
            let _ = writeln!(out, "#{} = {}", k.key, k.default.shown());
        }
    }
    out
}

/// The TOML type of a key, read from its default (or its example, for a key
/// with no plain default): `string`, `integer`, `float`, `boolean`, `array`
/// or `table`. A key that accepts an integer or a string says `integer or string` (`tick_us`: an
/// integer or `"auto"`; `socket_mode`: `0o660` or `"660"`). A `float` key also takes an
/// integer (`max_correction_ppm = 200`).
#[must_use]
pub fn value_type(k: &KeySpec) -> &'static str {
    use toml::Value;
    // The keys that take an integer or a string (`socket_mode`: 0o660 or "660").
    if matches!(
        k.path().as_str(),
        "engine.tick_us"
            | "control.socket_mode"
            | "defaults.flow.rx_keepalive_payload"
            | "flow.rx_keepalive_payload"
    ) {
        return "integer or string";
    }
    match toml::from_str::<toml::Table>(&format!("v = {}", k.default.shown()))
        .ok()
        .and_then(|mut t| t.remove("v"))
    {
        Some(Value::String(_)) => "string",
        Some(Value::Integer(_)) => "integer",
        Some(Value::Float(_)) => "float",
        Some(Value::Boolean(_)) => "boolean",
        Some(Value::Array(_)) => "array",
        Some(Value::Table(_)) => "table",
        Some(Value::Datetime(_)) | None => "unknown",
    }
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|")
}

/// The schema as Markdown tables, one per section, for
/// `docs/configuration.md` (a test keeps the document equal to this).
#[must_use]
pub fn markdown_tables() -> String {
    let mut out = String::new();
    for s in SECTIONS {
        let head = if s.array {
            format!("[[{}]]", s.name)
        } else {
            format!("[{}]", s.name)
        };
        let _ = writeln!(out, "#### `{head}`\n\n{}\n", cell(s.doc));
        out.push_str(
            "| Key | Type | Default | Unit | Range | Apply | Restart scope | What and why |\n",
        );
        out.push_str("|---|---|---|---|---|---|---|---|\n");
        for k in KEYS.iter().filter(|k| k.section == s.name) {
            let default = match k.default {
                KeyDefault::Value(v) => format!("`{v}`"),
                KeyDefault::Unset { example } => format!("unset (e.g. `{example}`)"),
                KeyDefault::Host { example } => format!("host (e.g. `{example}`)"),
                KeyDefault::Required { example } => format!("required (e.g. `{example}`)"),
            };
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} | {} | {} | {} | {} |",
                k.key,
                value_type(k),
                cell(&default),
                cell(k.unit),
                cell(k.range),
                k.apply.as_str(),
                k.scope.as_str(),
                cell(k.doc)
            );
        }
        out.push('\n');
    }
    out
}
