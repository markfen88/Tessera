//! The PCM configuration (`pcm.tessera { ... }` in `alsa.conf`), independent of
//! alsa-lib's configuration types so it can be tested without ALSA.
//!
//! # Schema
//!
//! | key | type | default | meaning |
//! |---|---|---|---|
//! | `device` | string | `""` | audio endpoint = the daemon's device key (empty: the daemon's default device) |
//! | `channels` | integer | `2` | channels of the PCM |
//! | `first_channel` | integer | `1` | first device channel, **1-based** |
//! | `rate` | integer | `0` | sample rate; `0` takes the device's rate |
//! | `socket` | string | `$TESSERA_AUDIO_SOCK`, else `/run/tessera/audio.sock` | the daemon's `audio.sock` |
//! | `latency` | integer | `64` | frames of client-side safety margin in addition to one period: the playback lead before the network clock reaches a frame, the capture lag behind the newest frame the daemon can have written |
//! | `max_buffer` | integer | `16384` | largest buffer (frames) the PCM will accept; sizes the daemon's ring (`Hello.max_period_frames`) |
//! | `ring_format` | string | `i32_or_f32` | `i32` (lossless for 16/24/32-bit integer data), `f32`, `i32_or_f32` or `f32_or_i32`; the `_or_` forms retry once with the other format when the daemon answers `BadRequest`; the default keeps the lossless path whenever the daemon offers it and still works with a daemon that offers only `f32` (the ADR 0007 minimum) |
//! | `connect_timeout` | integer, ms | `1000` | bound for connecting and attaching |
//! | `overlay_stale` | integer, ms | `2000` | a clock overlay silent this long means a hung daemon (`0` disables) |
//! | `client` | string | `alsa:<process name>` | name shown in the daemon's status |
//! | `daemon_uid` | list | `root,tessera,self` | users the daemon behind `socket` may run as, checked with `SO_PEERCRED`: `root`, `self`, uids, user names, or `any` |
//!
//! `playback { ... }` and `capture { ... }` compound nodes override any of
//! these for one direction (the TX and RX channel numbering of a device are
//! independent). `type`, `comment` and `hint` are ignored; any other key is
//! an error.

use std::path::PathBuf;

use crate::error::Error;

/// Environment variable naming the daemon's audio socket.
pub const ENV_SOCKET: &str = "TESSERA_AUDIO_SOCK";
/// Where `tesserad` serves `audio.sock` by default (ADR 0010).
pub const DEFAULT_SOCKET: &str = "/run/tessera/audio.sock";

/// Which ring sample format to ask for, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingFormatPref {
    /// `i32` only.
    I32,
    /// `f32` only.
    F32,
    /// `i32`, then `f32` if refused.
    I32ThenF32,
    /// `f32`, then `i32` if refused.
    F32ThenI32,
}

/// The direction a PCM was opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// Application to network (audio-ipc `Tx`; the plugin produces).
    Playback,
    /// Network to application (audio-ipc `Rx`; the plugin consumes).
    Capture,
}

/// Who the daemon behind `socket` may be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// Any local process (an explicit opt-out).
    Any,
    /// The daemon must run as one of these users.
    Users(Vec<UserSpec>),
}

/// One acceptable daemon user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserSpec {
    /// A numeric uid.
    Uid(u32),
    /// A user name, resolved when connecting (a name that does not exist
    /// matches nobody).
    Name(String),
    /// The uid of the application itself.
    SelfUid,
}

impl Trust {
    /// The default: root, the `tessera` service user, or the application's
    /// own user (a daemon started by the same user, for development).
    pub fn default_policy() -> Trust {
        Trust::Users(vec![
            UserSpec::Uid(0),
            UserSpec::Name("tessera".into()),
            UserSpec::SelfUid,
        ])
    }

    /// Parses `any` or a comma-separated list of `root`, `self`, uids and
    /// user names.
    pub fn parse(value: &str) -> Result<Trust, Error> {
        let v = value.trim();
        if v == "any" {
            return Ok(Trust::Any);
        }
        let mut users = Vec::new();
        for tok in v.split(',').map(str::trim) {
            users.push(match tok {
                "" => return Err(Error::Config("`daemon_uid` has an empty entry".into())),
                "root" => UserSpec::Uid(0),
                "self" => UserSpec::SelfUid,
                n if n.chars().all(|c| c.is_ascii_digit()) => UserSpec::Uid(
                    n.parse()
                        .map_err(|_| Error::Config(format!("`daemon_uid`: bad uid `{n}`")))?,
                ),
                n if n.len() <= 32
                    && n.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c)) =>
                {
                    UserSpec::Name(n.to_string())
                }
                n => return Err(Error::Config(format!("`daemon_uid`: bad user name `{n}`"))),
            });
        }
        Ok(Trust::Users(users))
    }
}

/// The value of the socket environment variable, or `None` in a process the
/// kernel marked secure (setuid/setgid/file capabilities: `AT_SECURE`),
/// where an unprivileged environment must not redirect the audio connection.
pub fn env_socket(secure: bool, value: Option<String>) -> Option<String> {
    if secure { None } else { value }
}

/// Validated PCM settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Endpoint name (device key).
    pub device: String,
    /// Channels.
    pub channels: u32,
    /// First channel, 1-based.
    pub first_channel: u32,
    /// Sample rate, 0 = the device's.
    pub rate: u32,
    /// The daemon's audio socket.
    pub socket: PathBuf,
    /// Extra frames of margin beyond one period.
    pub latency: u32,
    /// Largest buffer in frames.
    pub max_buffer: u32,
    /// Ring format preference.
    pub ring_format: RingFormatPref,
    /// Connect/attach bound in ms.
    pub connect_timeout_ms: u32,
    /// Hung-daemon bound in ms, 0 = off.
    pub overlay_stale_ms: u32,
    /// Client name for the daemon's status display.
    pub client: Option<String>,
    /// Who the daemon may be (checked with `SO_PEERCRED`).
    pub trust: Trust,
}

/// Largest value accepted for a count-like option; far above anything real,
/// small enough that no arithmetic on it can overflow.
const MAX_COUNT: u64 = 1 << 24;

impl Settings {
    /// The defaults, with the socket taken from `env_socket` (the value of
    /// [`ENV_SOCKET`]) when set.
    pub fn defaults(env_socket: Option<&str>) -> Settings {
        Settings {
            device: String::new(),
            channels: 2,
            first_channel: 1,
            rate: 0,
            socket: PathBuf::from(
                env_socket
                    .filter(|s| !s.is_empty())
                    .unwrap_or(DEFAULT_SOCKET),
            ),
            latency: 64,
            max_buffer: 16_384,
            ring_format: RingFormatPref::I32ThenF32,
            connect_timeout_ms: 1000,
            overlay_stale_ms: 2000,
            client: None,
            trust: Trust::default_policy(),
        }
    }

    /// Sets one option from its textual value.
    ///
    /// # Errors
    /// [`Error::Config`] for an unknown key or an invalid value.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), Error> {
        let num = |min: u64, max: u64| -> Result<u32, Error> {
            let v: u64 = value
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("`{key}` must be an integer, got `{value}`")))?;
            if v < min || v > max {
                return Err(Error::Config(format!(
                    "`{key}` must be between {min} and {max}, got {v}"
                )));
            }
            Ok(v as u32)
        };
        match key {
            "device" => {
                if value.len() > 255 || value.chars().any(char::is_control) {
                    return Err(Error::Config(
                        "`device` must be at most 255 bytes without control characters".into(),
                    ));
                }
                self.device = value.to_string();
            }
            "channels" => self.channels = num(1, MAX_COUNT)?,
            "first_channel" => self.first_channel = num(1, MAX_COUNT)?,
            "rate" => self.rate = num(0, 10_000_000)?,
            "socket" => {
                if value.is_empty() {
                    return Err(Error::Config("`socket` must not be empty".into()));
                }
                self.socket = PathBuf::from(value);
            }
            "latency" => self.latency = num(0, 1 << 20)?,
            "max_buffer" => self.max_buffer = num(64, 1 << 20)?,
            "ring_format" => {
                self.ring_format = match value.trim() {
                    "i32" => RingFormatPref::I32,
                    "f32" => RingFormatPref::F32,
                    "i32_or_f32" => RingFormatPref::I32ThenF32,
                    "f32_or_i32" => RingFormatPref::F32ThenI32,
                    other => {
                        return Err(Error::Config(format!(
                            "`ring_format` must be i32, f32, i32_or_f32 or f32_or_i32, got `{other}`"
                        )));
                    }
                }
            }
            "connect_timeout" => self.connect_timeout_ms = num(1, 600_000)?,
            "overlay_stale" => self.overlay_stale_ms = num(0, 3_600_000)?,
            "daemon_uid" => self.trust = Trust::parse(value)?,
            "client" => {
                if value.len() > 255 || value.chars().any(char::is_control) {
                    return Err(Error::Config(
                        "`client` must be at most 255 bytes without control characters".into(),
                    ));
                }
                self.client = Some(value.to_string());
            }
            other => {
                return Err(Error::Config(format!("unknown option `{other}`")));
            }
        }
        Ok(())
    }
}

/// A node of the configuration tree, as read from alsa.conf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A scalar, as text.
    Leaf(String),
    /// A compound node: `(id, child)` pairs in file order.
    Compound(Vec<(String, Node)>),
}

/// Builds the settings of a PCM opened for `dir` from the children of its
/// configuration node.
///
/// # Errors
/// [`Error::Config`] naming the first problem.
pub fn from_tree(
    children: &[(String, Node)],
    dir: Dir,
    env_socket: Option<&str>,
) -> Result<Settings, Error> {
    let mut s = Settings::defaults(env_socket);
    // Common options first, then the direction-specific overrides, so the
    // order inside the file does not matter.
    apply(&mut s, children, None)?;
    apply(&mut s, children, Some(dir))?;
    Ok(s)
}

fn apply(s: &mut Settings, children: &[(String, Node)], dir: Option<Dir>) -> Result<(), Error> {
    for (id, node) in children {
        match (id.as_str(), node) {
            ("type" | "comment" | "hint", _) => {}
            ("playback" | "capture", Node::Compound(inner)) => {
                let this = if id == "playback" {
                    Dir::Playback
                } else {
                    Dir::Capture
                };
                if dir == Some(this) {
                    for (k, v) in inner {
                        match (k.as_str(), v) {
                            ("type" | "comment" | "hint", _) => {}
                            (_, Node::Leaf(text)) => s.set(k, text)?,
                            (_, Node::Compound(_)) => {
                                return Err(Error::Config(format!(
                                    "`{k}` inside `{id}` must be a value"
                                )));
                            }
                        }
                    }
                }
            }
            ("playback" | "capture", Node::Leaf(_)) => {
                return Err(Error::Config(format!("`{id}` must be a compound node")));
            }
            (_, Node::Leaf(text)) => {
                if dir.is_none() {
                    s.set(id, text)?;
                }
            }
            (_, Node::Compound(_)) => {
                if dir.is_none() {
                    return Err(Error::Config(format!("`{id}` must be a value")));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(k: &str, v: &str) -> (String, Node) {
        (k.to_string(), Node::Leaf(v.to_string()))
    }

    #[test]
    fn defaults_and_env_socket() {
        let s = Settings::defaults(None);
        assert_eq!(s.socket, PathBuf::from(DEFAULT_SOCKET));
        assert_eq!((s.channels, s.first_channel, s.latency), (2, 1, 64));
        assert_eq!(s.ring_format, RingFormatPref::I32ThenF32);
        let s = Settings::defaults(Some("/tmp/x.sock"));
        assert_eq!(s.socket, PathBuf::from("/tmp/x.sock"));
        let s = Settings::defaults(Some(""));
        assert_eq!(s.socket, PathBuf::from(DEFAULT_SOCKET));
    }

    #[test]
    fn config_socket_wins_over_env() {
        let t = vec![leaf("socket", "/run/a.sock")];
        let s = from_tree(&t, Dir::Playback, Some("/tmp/env.sock")).unwrap();
        assert_eq!(s.socket, PathBuf::from("/run/a.sock"));
    }

    #[test]
    fn direction_overrides_apply_regardless_of_order() {
        let t = vec![
            (
                "capture".to_string(),
                Node::Compound(vec![leaf("first_channel", "5"), leaf("channels", "4")]),
            ),
            leaf("device", "main"),
            leaf("channels", "2"),
            (
                "playback".to_string(),
                Node::Compound(vec![leaf("first_channel", "3")]),
            ),
            leaf("type", "tessera"),
            leaf("comment", "x"),
        ];
        let c = from_tree(&t, Dir::Capture, None).unwrap();
        assert_eq!(
            (c.device.as_str(), c.first_channel, c.channels),
            ("main", 5, 4)
        );
        let p = from_tree(&t, Dir::Playback, None).unwrap();
        assert_eq!((p.first_channel, p.channels), (3, 2));
    }

    #[test]
    fn rejects_bad_input() {
        for (k, v) in [
            ("channels", "0"),
            ("channels", "x"),
            ("first_channel", "0"),
            ("rate", "-1"),
            ("max_buffer", "3"),
            ("ring_format", "i24"),
            ("bogus", "1"),
            ("device", "a\u{0}b"),
            ("socket", ""),
        ] {
            let mut s = Settings::defaults(None);
            assert!(s.set(k, v).is_err(), "{k}={v}");
        }
        let bad = vec![("playback".to_string(), Node::Leaf("1".into()))];
        assert!(from_tree(&bad, Dir::Playback, None).is_err());
        let bad = vec![("channels".to_string(), Node::Compound(vec![]))];
        assert!(from_tree(&bad, Dir::Playback, None).is_err());
    }

    #[test]
    fn daemon_trust_policy_and_secure_environment() {
        assert_eq!(Trust::parse("any").unwrap(), Trust::Any);
        assert_eq!(
            Trust::parse("root, self, 108, tessera").unwrap(),
            Trust::Users(vec![
                UserSpec::Uid(0),
                UserSpec::SelfUid,
                UserSpec::Uid(108),
                UserSpec::Name("tessera".into())
            ])
        );
        for bad in ["", "a,,b", "x y", "a/b", "99999999999"] {
            assert!(Trust::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Settings::defaults(None).trust, Trust::default_policy());
        let mut s = Settings::defaults(None);
        s.set("daemon_uid", "any").unwrap();
        assert_eq!(s.trust, Trust::Any);
        assert_eq!(env_socket(false, Some("/x".into())), Some("/x".into()));
        assert_eq!(env_socket(true, Some("/x".into())), None);
    }

    #[test]
    fn ring_formats() {
        for (v, want) in [
            ("i32", RingFormatPref::I32),
            ("f32", RingFormatPref::F32),
            ("i32_or_f32", RingFormatPref::I32ThenF32),
            ("f32_or_i32", RingFormatPref::F32ThenI32),
        ] {
            let mut s = Settings::defaults(None);
            s.set("ring_format", v).unwrap();
            assert_eq!(s.ring_format, want);
        }
    }
}
