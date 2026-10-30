//! The EQ engine (CamillaDSP): a minimal websocket client, the Playing/Idle
//! state, the active configuration and the signal boost (a Gain filter's
//! `gain` in the active configuration file).
//!
//! The protocol (CamillaDSP 4.x): a plain `ws://` socket; one text frame
//! carrying JSON: a bare string for a command without an argument
//! (`"GetState"`) or a one-key object with it (`{"SetVolume": -12.0}`); the
//! reply is `{"<Command>": {"result": "Ok", "value": ...}}`. A refusal is
//! another `result`: the name of the error (`"InvalidFaderError"`), or a
//! one-key object with the name and the message
//! (`{"ConfigValidationError": "..."}`), as 4.1.3's `websocket.md` documents;
//! earlier engines answer `"Error"` with the message in `value`, which is
//! read the same way. The server pings; the websocket library
//! answers them. Every call is one short-lived connection with an overall
//! timeout and a bound on the message and frame sizes. Only what the page
//! needs is implemented.
//!
//! The engine owns its state file (it rewrites it for volume and mute);
//! the portal reads it only when the engine cannot be asked, and writes it
//! only when it falls back to restarting the engine (in place, never by
//! rename). Configuration files are edited line by line, never
//! re-serialised: the operator's YAML keeps its comments, order and
//! formatting, and only the one value asked for changes.
//!
//! The operations here (`activate`, `set_boost`) and the file operations
//! of [`crate::appliance::files`] are called with the appliance's file
//! lock held ([`crate::appliance::Appliance::eq_lock`]): one change at a
//! time.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use futures_util::{SinkExt, StreamExt};
use maud::{Markup, html};
use serde::Deserialize;
use serde_json::Value;
use tessera_control_types::Status;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

use crate::Shared;
use crate::appliance::capture_rate;
use crate::appliance::files::{self, Kind, is_regular, write_atomic, write_in_place};
use crate::appliance::services::{Action, Runner};
use crate::config::{ApplianceConfig, CamillaDspConfig, GUI_HOST_PLACEHOLDER};
use crate::pages::{is_live, khz, live_answer, need_post, status_of};

/// The whole exchange (connect, send, reply, close) must finish in this.
pub const TIMEOUT: Duration = Duration::from_secs(4);
/// The timeout of the reads a page render makes (`GetState`,
/// `GetConfigFilePath`, `GetStopReason`): a hung engine must not hold every
/// page for long.
pub const PAGE_TIMEOUT: Duration = Duration::from_secs(1);
/// Largest message and frame accepted from the engine.
pub const MAX_FRAME: usize = 2 * 1024 * 1024;
/// How long a finished call waits for the engine's half of the closing handshake.
const CLOSE_WAIT: Duration = Duration::from_millis(500);
/// The engine's systemd unit, for the restart fallback when the websocket
/// is not reachable: used only if it is an allowlisted entry.
pub const UNIT: &str = "camilladsp";
/// Signal boost range, dB.
pub const BOOST_RANGE: (f64, f64) = (0.0, 30.0);
/// Above this the boost pane is shown in the warning colour: that much
/// gain clips most sources.
pub const BOOST_WARN_DB: f64 = 12.0;
/// Where the boost form sends the operator back.
pub const BOOST_ANCHOR: &str = "/speaker#boost";

// ---- websocket client -------------------------------------------------------------

/// Why a command got no `Ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsError {
    /// No connection, or no answer in time: the engine is not running (or
    /// not where the configuration says).
    Unreachable(String),
    /// A connection, but not the protocol described above.
    Protocol(String),
    /// The engine answered `result: Error` with this message.
    Rejected(String),
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "EQ engine not reachable: {e}"),
            Self::Protocol(e) => write!(f, "EQ engine protocol error: {e}"),
            Self::Rejected(e) => write!(f, "EQ engine refused: {e}"),
        }
    }
}

/// The commands the portal sends.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// The processing state (`Running`, `Paused`, `Inactive`, `Stalled`,
    /// `Starting`).
    GetState,
    /// Why processing last stopped (not supported by every engine).
    GetStopReason,
    /// The engine's own volume, dB (not the amplifier's).
    GetVolume,
    /// Set the engine's volume, dB.
    SetVolume(f64),
    /// Re-read the active configuration file.
    Reload,
    /// Path of the active configuration file.
    GetConfigFilePath,
    /// Change the active configuration file (takes effect at `Reload`; the
    /// engine persists it in its state file).
    SetConfigFilePath(String),
    /// The engine's version.
    GetVersion,
    /// Stop processing (the capture and playback devices close) and wait
    /// for a configuration (`SetConfigFilePath` and `Reload`, or
    /// `SetConfigJson`).
    Stop,
    /// The running configuration, as a JSON text (`null` without one).
    GetConfigJson,
    /// Run this configuration (a JSON text) now; the file is not touched.
    SetConfigJson(String),
}

impl Command {
    /// The command's name: the JSON key of the reply.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::GetState => "GetState",
            Self::GetStopReason => "GetStopReason",
            Self::GetVolume => "GetVolume",
            Self::SetVolume(_) => "SetVolume",
            Self::Reload => "Reload",
            Self::GetConfigFilePath => "GetConfigFilePath",
            Self::SetConfigFilePath(_) => "SetConfigFilePath",
            Self::GetVersion => "GetVersion",
            Self::Stop => "Stop",
            Self::GetConfigJson => "GetConfigJson",
            Self::SetConfigJson(_) => "SetConfigJson",
        }
    }

    /// The text frame sent.
    #[must_use]
    pub fn json(&self) -> String {
        let v = match self {
            Self::SetVolume(db) => serde_json::json!({ self.name(): db }),
            Self::SetConfigFilePath(p) | Self::SetConfigJson(p) => {
                serde_json::json!({ self.name(): p })
            }
            _ => Value::String(self.name().to_owned()),
        };
        v.to_string()
    }
}

/// Parse the reply to `cmd`: the `value` of an `Ok` result.
///
/// # Errors
/// [`WsError::Rejected`] for any other `result` string (`Error` with the
/// message in `value`, or a 4.x error name) or a one-key error object
/// (`{"ConfigValidationError": "..."}`, given as `name: message`);
/// [`WsError::Protocol`] for anything else than the documented shape.
pub fn parse_reply(cmd: &str, text: &str) -> Result<Value, WsError> {
    let v: Value =
        serde_json::from_str(text).map_err(|e| WsError::Protocol(format!("bad JSON: {e}")))?;
    let Some(obj) = v.as_object() else {
        return Err(WsError::Protocol("reply is not an object".into()));
    };
    let Some(inner) = obj.get(cmd) else {
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        return Err(WsError::Protocol(format!(
            "reply to {cmd} has keys {keys:?}"
        )));
    };
    let value = inner.get("value").cloned().unwrap_or(Value::Null);
    let message = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => "no message".into(),
        other => other.to_string(),
    };
    match inner.get("result") {
        Some(Value::String(r)) if r == "Ok" => Ok(value),
        Some(Value::String(r)) if r == "Error" => Err(WsError::Rejected(message(&value))),
        Some(Value::String(r)) if !r.is_empty() => Err(WsError::Rejected(match &value {
            Value::String(s) => format!("{r}: {s}"),
            _ => r.clone(),
        })),
        Some(Value::Object(o)) if o.len() == 1 => {
            let (name, why) = o.iter().next().map_or(("", &Value::Null), |(k, v)| (k, v));
            Err(WsError::Rejected(format!("{name}: {}", message(why))))
        }
        other => Err(WsError::Protocol(format!("result is {other:?}"))),
    }
}

/// The host and port of a `ws://` address (`ws://[::1]:1234` gives
/// `::1`, port 80 when none).
///
/// # Errors
/// Not a URI with a host.
pub fn endpoint(url: &str) -> Result<(String, u16), WsError> {
    let uri: Uri = url
        .parse()
        .map_err(|e| WsError::Protocol(format!("websocket address: {e}")))?;
    let host = uri
        .host()
        .ok_or_else(|| WsError::Protocol("websocket address has no host".into()))?;
    // `[::1]` is given with its brackets; `connect` wants the bare address.
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    // A link-local zone is written `%25eth0` in a URI; the resolver wants
    // `%eth0` (`connect` hands a host it cannot parse as an address to
    // the resolver, which understands zones).
    let host = host.replace("%25", "%");
    Ok((host, uri.port_u16().unwrap_or(80)))
}

/// A client of one engine: the address and the timeout.
#[derive(Debug, Clone)]
pub struct Client {
    url: String,
    timeout: Duration,
}

impl Client {
    /// For the engine at `url` (`ws://host:port`), with [`TIMEOUT`].
    #[must_use]
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_owned(),
            timeout: TIMEOUT,
        }
    }

    /// With another overall timeout.
    #[must_use]
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Send `cmd` and return the `value` of its `Ok` reply.
    ///
    /// # Errors
    /// [`WsError`].
    pub async fn call(&self, cmd: &Command) -> Result<Value, WsError> {
        match tokio::time::timeout(self.timeout, self.exchange(cmd)).await {
            Ok(r) => r,
            Err(_) => Err(WsError::Unreachable(format!(
                "no reply to {} within {:?}",
                cmd.name(),
                self.timeout
            ))),
        }
    }

    async fn exchange(&self, cmd: &Command) -> Result<Value, WsError> {
        let (host, port) = endpoint(&self.url)?;
        let tcp = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| WsError::Unreachable(format!("{host}:{port}: {e}")))?;
        let request = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| WsError::Protocol(e.to_string()))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME))
            .max_frame_size(Some(MAX_FRAME));
        let (mut ws, _) = tokio_tungstenite::client_async_with_config(request, tcp, Some(config))
            .await
            .map_err(|e| WsError::Protocol(format!("handshake: {e}")))?;
        ws.send(Message::text(cmd.json()))
            .await
            .map_err(|e| WsError::Protocol(format!("send: {e}")))?;
        // Pings are answered by the library while the stream is read.
        let reply = loop {
            match ws.next().await {
                None | Some(Ok(Message::Close(_))) => {
                    break Err(WsError::Protocol("closed without a reply".into()));
                }
                Some(Err(e)) => break Err(WsError::Protocol(format!("read: {e}"))),
                Some(Ok(Message::Text(t))) => break parse_reply(cmd.name(), t.as_str()),
                Some(Ok(Message::Binary(_))) => {
                    break Err(WsError::Protocol("binary frame".into()));
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
            }
        };
        // A polite close; the reply is already in hand, so its outcome
        // does not matter. Sending the Close frame is half of it: the stream is
        // read on until the engine's own Close arrives and the stream ends.
        // Dropping the socket with that reply unread resets the connection,
        // and the engine logs "Connection reset without closing" for every call.
        // Bounded, so an engine that never answers cannot hold the caller.
        if ws.close(None).await.is_ok() {
            let _ = tokio::time::timeout(CLOSE_WAIT, async {
                while let Some(Ok(_)) = ws.next().await {}
            })
            .await;
        }
        reply
    }

    /// `GetState`: the processing state's name.
    ///
    /// # Errors
    /// [`WsError`].
    pub async fn get_state(&self) -> Result<String, WsError> {
        expect_string(self.call(&Command::GetState).await?)
    }

    /// `GetStopReason`: a string or a tagged stop reason, depending on the
    /// engine version.
    ///
    /// # Errors
    /// [`WsError`], including engines that do not know this command.
    pub async fn get_stop_reason(&self) -> Result<Value, WsError> {
        self.call(&Command::GetStopReason).await
    }

    /// `GetVolume`: the engine's volume, dB.
    ///
    /// # Errors
    /// [`WsError`].
    pub async fn get_volume(&self) -> Result<f64, WsError> {
        let v = self.call(&Command::GetVolume).await?;
        v.as_f64()
            .ok_or_else(|| WsError::Protocol(format!("GetVolume value {v} is not a number")))
    }

    /// `SetVolume`.
    ///
    /// # Errors
    /// [`WsError`].
    pub async fn set_volume(&self, db: f64) -> Result<(), WsError> {
        self.call(&Command::SetVolume(db)).await.map(|_| ())
    }

    /// `Reload`.
    ///
    /// # Errors
    /// [`WsError`].
    pub async fn reload(&self) -> Result<(), WsError> {
        self.call(&Command::Reload).await.map(|_| ())
    }

    /// `GetConfigFilePath`: `None` when the engine has none.
    ///
    /// # Errors
    /// [`WsError`].
    pub async fn get_config_file_path(&self) -> Result<Option<String>, WsError> {
        match self.call(&Command::GetConfigFilePath).await? {
            Value::Null => Ok(None),
            Value::String(s) => Ok(Some(s)),
            other => Err(WsError::Protocol(format!(
                "GetConfigFilePath value {other} is not a string"
            ))),
        }
    }

    /// `SetConfigFilePath`.
    ///
    /// # Errors
    /// [`WsError`].
    pub async fn set_config_file_path(&self, path: &str) -> Result<(), WsError> {
        self.call(&Command::SetConfigFilePath(path.to_owned()))
            .await
            .map(|_| ())
    }

    /// `GetVersion`.
    ///
    /// # Errors
    /// [`WsError`].
    pub async fn get_version(&self) -> Result<String, WsError> {
        expect_string(self.call(&Command::GetVersion).await?)
    }

    /// `Stop`.
    ///
    /// # Errors
    /// [`WsError`].
    pub async fn stop(&self) -> Result<(), WsError> {
        self.call(&Command::Stop).await.map(|_| ())
    }

    /// `GetConfigJson`: the running configuration, `None` without one. The
    /// value is a JSON text (4.x); an object is taken as it is.
    ///
    /// # Errors
    /// [`WsError`]; a text that is not JSON is a protocol error.
    pub async fn get_config_json(&self) -> Result<Option<Value>, WsError> {
        let v = match self.call(&Command::GetConfigJson).await? {
            Value::String(s) => serde_json::from_str(&s)
                .map_err(|e| WsError::Protocol(format!("GetConfigJson value is not JSON: {e}")))?,
            other => other,
        };
        Ok((!v.is_null()).then_some(v))
    }

    /// `SetConfigJson` with `config` (sent as a JSON text).
    ///
    /// # Errors
    /// [`WsError`].
    pub async fn set_config_json(&self, config: &Value) -> Result<(), WsError> {
        self.call(&Command::SetConfigJson(config.to_string()))
            .await
            .map(|_| ())
    }
}

fn expect_string(v: Value) -> Result<String, WsError> {
    match v {
        Value::String(s) => Ok(s),
        other => Err(WsError::Protocol(format!("value {other} is not a string"))),
    }
}

// ---- Playing / Idle ------------------------------------------------------------------

/// What the hero shows for the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playback {
    /// `Running`: audio flows.
    Playing,
    /// Any other processing state (`Paused`, `Inactive`, `Stalled`,
    /// `Starting`, or one this version does not know): the engine runs but
    /// no audio flows.
    Idle(String),
    /// The engine could not be asked (not running, or not answering).
    Off(String),
}

impl Playback {
    /// From the `GetState` answer.
    #[must_use]
    pub fn of(state: &Result<String, WsError>) -> Self {
        match state {
            Ok(s) if s == "Running" => Self::Playing,
            Ok(s) => Self::Idle(s.clone()),
            Err(e) => Self::Off(e.to_string()),
        }
    }

    /// The big word.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Playing => "Playing",
            Self::Idle(_) => "Idle",
            Self::Off(_) => "EQ engine off",
        }
    }

    /// The ring's classes: pulsing green only while playing.
    #[must_use]
    pub fn ring_class(&self) -> &'static str {
        match self {
            Self::Playing => "ring",
            Self::Idle(_) | Self::Off(_) => "ring off",
        }
    }

    /// The `title` of the ring: the engine's state or why it is off.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Playing => "EQ engine running".into(),
            Self::Idle(s) => format!("EQ engine {}", s.to_lowercase()),
            Self::Off(why) => why.clone(),
        }
    }
}

// ---- YAML, line by line --------------------------------------------------------------

/// The lines of a text with their byte offsets, line endings stripped.
pub(super) fn lines(text: &str) -> Vec<(usize, &str)> {
    let mut v = Vec::new();
    let mut at = 0;
    for l in text.split_inclusive('\n') {
        let body = l.strip_suffix('\n').unwrap_or(l);
        let body = body.strip_suffix('\r').unwrap_or(body);
        v.push((at, body));
        at += l.len();
    }
    v
}

/// The indentation (spaces; a tab counts as one, YAML forbids it there
/// anyway) and content of a line that has any (blank and comment lines
/// have none).
pub(super) fn content(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start_matches([' ', '\t']);
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    Some((line.len() - t.len(), t))
}

/// If `t` (a line's content) is `key:` (plain or quoted), the byte length
/// of `key:` in it.
pub(super) fn key_len(t: &str, key: &str) -> Option<usize> {
    for quoted in [key.to_owned(), format!("\"{key}\""), format!("'{key}'")] {
        if let Some(rest) = t.strip_prefix(quoted.as_str()) {
            if let Some(after) = rest.strip_prefix(':') {
                if after.is_empty() || after.starts_with([' ', '\t']) {
                    return Some(quoted.len() + 1);
                }
            }
        }
    }
    None
}

/// The index of the line `key:` among the direct children of the mapping
/// whose key is on line `parent` (`None`: the top level).
fn find_child(ls: &[(usize, &str)], parent: Option<usize>, key: &str) -> Option<usize> {
    let parent_indent = parent.and_then(|p| content(ls[p].1)).map(|(i, _)| i);
    let start = parent.map_or(0, |p| p + 1);
    let mut child_indent = None;
    for (i, (_, line)) in ls.iter().enumerate().skip(start) {
        let Some((indent, t)) = content(line) else {
            continue;
        };
        if parent_indent.is_some_and(|pi| indent <= pi) {
            break;
        }
        let ci = *child_indent.get_or_insert(indent);
        if indent < ci {
            break;
        }
        if indent == ci && key_len(t, key).is_some() {
            return Some(i);
        }
    }
    None
}

/// Does the mapping at line `parent` have any child line at all (a block
/// mapping, not a flow `{...}` or a scalar)?
pub(super) fn has_block(ls: &[(usize, &str)], parent: usize) -> bool {
    let Some((pi, _)) = content(ls[parent].1) else {
        return false;
    };
    ls.iter()
        .skip(parent + 1)
        .find_map(|(_, l)| content(l))
        .is_some_and(|(i, _)| i > pi)
}

/// Of the text after `key:`: the byte range of the scalar value (without
/// surrounding whitespace and a trailing `# comment`; a quoted value runs
/// to its closing quote, a `#` inside it is not a comment).
pub(super) fn value_span(rest: &str) -> (usize, usize) {
    let start = rest.len() - rest.trim_start().len();
    let b = rest.as_bytes();
    let mut end = rest.len();
    match b.get(start) {
        Some(q @ (b'"' | b'\'')) => {
            let mut i = start + 1;
            while i < b.len() {
                if b[i] == *q {
                    if *q == b'\'' && b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    end = i + 1;
                    break;
                }
                if *q == b'"' && b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            (start, end)
        }
        _ => {
            for (i, c) in b.iter().enumerate().skip(start) {
                if *c == b'#' && (i == start || b[i - 1].is_ascii_whitespace()) {
                    end = i;
                    break;
                }
            }
            let end = start + rest[start..end].trim_end().len();
            (start, end)
        }
    }
}

/// The scalar value of the line (quotes removed).
fn scalar(line: &str, key: &str) -> Option<String> {
    let (_, t) = content(line)?;
    let k = key_len(t, key)?;
    let rest = &t[k..];
    let (s, e) = value_span(rest);
    Some(unquote(&rest[s..e]))
}

pub(super) fn unquote(v: &str) -> String {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let mut out = String::with_capacity(v.len());
        let mut esc = false;
        for c in v[1..v.len() - 1].chars() {
            if esc {
                out.push(match c {
                    'n' => '\n',
                    't' => '\t',
                    other => other,
                });
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else {
                out.push(c);
            }
        }
        out
    } else if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        v[1..v.len() - 1].replace("''", "'")
    } else {
        v.to_owned()
    }
}

/// A YAML scalar for `v`: plain when it is safe, else double-quoted.
fn quote(v: &str) -> String {
    let plain = !v.is_empty()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
        && !v.starts_with('-');
    if plain {
        return v.to_owned();
    }
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `text` with the scalar on line `i` (a `key:` line) replaced by `new`,
/// everything else byte for byte the same (the comment after it included).
pub(super) fn replace_scalar(
    text: &str,
    ls: &[(usize, &str)],
    i: usize,
    key: &str,
    new: &str,
) -> String {
    let (off, line) = ls[i];
    let (indent, t) = content(line).unwrap_or((0, line));
    let k = key_len(t, key).unwrap_or(0);
    let rest = &t[k..];
    let (s, e) = value_span(rest);
    let value_at = off + indent + k;
    let mut out = String::with_capacity(text.len() + new.len());
    out.push_str(&text[..value_at]);
    if s == e {
        // No value yet (`gain:` or `gain:  # c`): one space, the value,
        // then the comment two spaces further.
        out.push(' ');
        out.push_str(new);
        let tail = rest[s..].trim_start();
        if !tail.is_empty() {
            out.push_str("  ");
            out.push_str(tail);
        }
        out.push_str(&text[off + line.len()..]);
    } else {
        out.push_str(&rest[..s]);
        out.push_str(new);
        out.push_str(&text[value_at + e..]);
    }
    out
}

/// The line of `gain` in the boost filter: `filters.<filter>` must be a
/// block mapping with `type: Gain`, `parameters` a block mapping with a
/// `gain` line and no `scale` other than `dB`.
fn gain_line(ls: &[(usize, &str)], filter: &str) -> Result<usize, String> {
    let filters = find_child(ls, None, "filters")
        .ok_or_else(|| "the active EQ configuration has no `filters`".to_owned())?;
    let f = find_child(ls, Some(filters), filter)
        .ok_or_else(|| format!("the active EQ configuration has no filter `{filter}`"))?;
    let ty = find_child(ls, Some(f), "type").and_then(|i| scalar(ls[i].1, "type"));
    if ty.as_deref() != Some("Gain") {
        return Err(format!(
            "the filter `{filter}` is not a Gain filter (type {})",
            ty.as_deref().unwrap_or("missing")
        ));
    }
    let params = find_child(ls, Some(f), "parameters")
        .ok_or_else(|| format!("the filter `{filter}` has no `parameters`"))?;
    if !has_block(ls, params) {
        return Err(format!(
            "the filter `{filter}` writes its parameters in flow style (`{{...}}`); the portal edits block style only"
        ));
    }
    if let Some(i) = find_child(ls, Some(params), "scale") {
        let scale = scalar(ls[i].1, "scale").unwrap_or_default();
        if scale != "dB" {
            return Err(format!(
                "the filter `{filter}` has `scale: {scale}`; the signal boost needs dB"
            ));
        }
    }
    find_child(ls, Some(params), "gain")
        .ok_or_else(|| format!("the filter `{filter}` has no `gain`"))
}

/// The `gain` of the Gain filter `filter`
/// (`filters.<filter>.parameters.gain`), if the configuration has it as
/// the boost needs it ([`set_gain`] says what that is).
#[must_use]
pub fn read_gain(yaml: &str, filter: &str) -> Option<f64> {
    let ls = lines(yaml);
    let i = gain_line(&ls, filter).ok()?;
    scalar(ls[i].1, "gain")?.parse().ok()
}

/// The configuration with that `gain` set to `db` (one decimal), the rest
/// untouched.
///
/// # Errors
/// The filter is missing, not `type: Gain`, has a `scale` other than dB,
/// writes its parameters in flow style, or has no `gain` line.
pub fn set_gain(yaml: &str, filter: &str, db: f64) -> Result<String, String> {
    let ls = lines(yaml);
    let i = gain_line(&ls, filter)?;
    Ok(replace_scalar(yaml, &ls, i, "gain", &format!("{db:.1}")))
}

/// `config_path` of the engine's state file.
#[must_use]
pub fn read_config_path(statefile: &str) -> Option<PathBuf> {
    let ls = lines(statefile);
    let i = find_child(&ls, None, "config_path")?;
    let v = scalar(ls[i].1, "config_path")?;
    if v.is_empty() || v == "null" || v == "~" {
        None
    } else {
        Some(PathBuf::from(v))
    }
}

/// The state file with `config_path` set to `path` (added at the end when
/// the file has none), the rest untouched.
#[must_use]
pub fn set_config_path(statefile: &str, path: &Path) -> String {
    let value = quote(&path.to_string_lossy());
    let ls = lines(statefile);
    match find_child(&ls, None, "config_path") {
        Some(i) => replace_scalar(statefile, &ls, i, "config_path", &value),
        None => {
            let mut out = statefile.to_owned();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("config_path: {value}\n"));
            out
        }
    }
}

// ---- the active configuration ---------------------------------------------------------

/// Where the active configuration was learned from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `GetConfigFilePath` over the websocket.
    Engine,
    /// The state file (the engine could not be asked).
    Statefile,
}

/// The active configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Active {
    /// Its path; `None` when the engine has none.
    pub path: Option<PathBuf>,
    /// Its name when it is in `configs_dir`, else the whole path.
    pub label: Option<String>,
    /// The `gain` of the boost filter in it.
    pub boost_db: Option<f64>,
    /// Who said so.
    pub source: Source,
}

impl Active {
    /// Ask the engine; when it cannot be asked, read the state file.
    ///
    /// # Errors
    /// The state file exists but cannot be read: the active configuration
    /// is then unknown, and the callers refuse to delete or overwrite
    /// anything (fail closed).
    pub async fn read(cfg: &CamillaDspConfig, client: &Client) -> Result<Self, String> {
        let (path, source) = match client.get_config_file_path().await {
            Ok(p) => (p.map(PathBuf::from), Source::Engine),
            Err(WsError::Rejected(m)) => {
                return Err(format!(
                    "the EQ engine cannot say which configuration is active: {m}"
                ));
            }
            Err(_) => match tokio::fs::read_to_string(&cfg.statefile).await {
                Ok(text) => (read_config_path(&text), Source::Statefile),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, Source::Statefile),
                Err(e) => {
                    return Err(format!(
                        "the EQ engine is not reachable and its state file cannot be read ({}: {e})",
                        cfg.statefile.display()
                    ));
                }
            },
        };
        let label = path.as_deref().map(|p| display_name(cfg, p));
        let boost_db = match &path {
            Some(p) => tokio::fs::read_to_string(p)
                .await
                .ok()
                .and_then(|y| read_gain(&y, &cfg.gain_filter)),
            None => None,
        };
        Ok(Self {
            path,
            label,
            boost_db,
            source,
        })
    }

    /// Is `name` (in `configs_dir`) the active configuration? Compared as
    /// files ([`same_file`]), not as strings.
    #[must_use]
    pub fn is(&self, cfg: &CamillaDspConfig, name: &str) -> bool {
        self.path
            .as_ref()
            .is_some_and(|p| same_file(p, &cfg.configs_dir.join(name)))
    }
}

/// Do `a` and `b` name the same file? By device and inode when both
/// exist, else by canonical path when both resolve, else literally.
#[must_use]
pub fn same_file(a: &Path, b: &Path) -> bool {
    if let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) {
        return ma.dev() == mb.dev() && ma.ino() == mb.ino();
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// How the page names a configuration path: the file name when it is in
/// `configs_dir`, the whole path otherwise.
#[must_use]
pub fn display_name(cfg: &CamillaDspConfig, path: &Path) -> String {
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(f)) if dir == cfg.configs_dir => f.to_string_lossy().into_owned(),
        _ => path.to_string_lossy().into_owned(),
    }
}

// ---- apply a change -----------------------------------------------------------------------

/// How a change reached the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Over the websocket.
    Reloaded,
    /// The websocket was not reachable; the allowlisted unit was restarted.
    Restarted,
    /// Saved only: the websocket was not reachable and the engine's unit is
    /// not an allowlisted entry. The message says why.
    Saved(String),
}

impl Applied {
    /// The tail of the flash message.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Reloaded => "applied.".into(),
            Self::Restarted => "applied (the EQ engine was restarted).".into(),
            Self::Saved(why) => {
                format!("saved; restart the EQ engine to apply it ({why}).")
            }
        }
    }
}

/// Restart the engine's unit if it is an allowlisted entry (through the
/// services module, as the Services card does).
async fn restart(
    cfg: &ApplianceConfig,
    runner: &dyn Runner,
    why: String,
) -> Result<Applied, String> {
    tracing::debug!(why, "EQ engine websocket unavailable, trying a restart");
    let Some(entry) = cfg.services.iter().find(|e| e.has_unit(UNIT)) else {
        return Ok(Applied::Saved(why));
    };
    crate::appliance::services::perform(runner, entry, Action::Restart, None)
        .await
        .map(|()| Applied::Restarted)
}

/// The active configuration `path` as the portal may edit it: a regular
/// file directly in `configs_dir` under an accepted name. Its path there
/// and its name.
///
/// # Errors
/// Outside `configs_dir`, or not such a file.
pub(super) fn editable(c: &CamillaDspConfig, path: &Path) -> Result<(PathBuf, String), String> {
    let name = match (path.parent(), path.file_name().and_then(|n| n.to_str())) {
        (Some(dir), Some(n)) if dir == c.configs_dir => n.to_owned(),
        _ => {
            return Err(format!(
                "the active configuration `{}` is outside {}; the portal edits only files there",
                path.display(),
                c.configs_dir.display()
            ));
        }
    };
    Ok((config_file(c, &name)?, name))
}

/// The path of `name` in `configs_dir` when it is a regular file there
/// (no symlink, no name outside the rules).
fn config_file(cfg: &CamillaDspConfig, name: &str) -> Result<PathBuf, String> {
    let path = files::path_of(cfg, Kind::Config, name)?;
    if !is_regular(&path) {
        return Err(format!(
            "`{name}` is not a configuration file in the directory"
        ));
    }
    Ok(path)
}

/// Make `name` (in `configs_dir`) the active configuration. With the
/// websocket: `SetConfigFilePath` (the engine persists it) then `Reload`;
/// a refused reload puts the previous path back. Without: only when the
/// engine's unit is not active (a running engine that merely does not
/// answer would overwrite the state file itself), `config_path` is
/// written into the state file in place and the unit restarted if it is
/// allowlisted. Call with the file lock held, after the engine's check of
/// the file, on a task of its own with the lock's owned guard moved into
/// it (the handler does), so a client that goes away mid-switch neither
/// cancels the sequence nor releases the lock before it ends.
///
/// # Errors
/// The name is not a configuration file, the engine refused, the engine
/// runs but does not answer, the state file cannot be written, or the
/// restart failed.
pub async fn activate(
    cfg: &ApplianceConfig,
    runner: &dyn Runner,
    name: &str,
) -> Result<Applied, String> {
    let path = config_file(&cfg.camilladsp, name)?;
    switch(cfg, runner, name, &path).await
}

/// Is the engine's unit running ([`crate::appliance::services::is_active`])?
///
/// # Errors
/// `systemctl` could not be run.
pub(super) async fn unit_active(runner: &dyn Runner) -> Result<bool, String> {
    crate::appliance::services::is_active(runner, UNIT).await
}

async fn switch(
    cfg: &ApplianceConfig,
    runner: &dyn Runner,
    name: &str,
    path: &Path,
) -> Result<Applied, String> {
    let c = &cfg.camilladsp;
    let p = path.to_string_lossy().into_owned();
    let client = Client::new(&c.websocket);
    let why = match client.get_config_file_path().await {
        Ok(old) => {
            client
                .set_config_file_path(&p)
                .await
                .map_err(|e| format!("the EQ engine refused the path: {e}"))?;
            return match client.reload().await {
                Ok(()) => Ok(Applied::Reloaded),
                Err(e) => {
                    // Back to the previous file, so a later restart does
                    // not come up on a configuration the engine refused.
                    if let Some(old) = old {
                        if let Err(e2) = client.set_config_file_path(&old).await {
                            tracing::warn!(%e2, "EQ engine: cannot restore the previous configuration path");
                        }
                    }
                    Err(format!("the EQ engine did not reload `{name}`: {e}"))
                }
            };
        }
        Err(WsError::Rejected(m)) => return Err(format!("the EQ engine refused: {m}")),
        Err(e) => e.to_string(),
    };
    // Fallback: the state file names the configuration for the next
    // start. Only for an engine that is not running: a running one that
    // does not answer still owns the file and would write it back.
    if unit_active(runner).await? {
        return Err(
            "the EQ engine is running but does not answer; restart it from the Services card and try again"
                .into(),
        );
    }
    let state = match tokio::fs::read_to_string(&c.statefile).await {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(format!(
                "the EQ engine is not reachable and its state file cannot be read ({}: {e})",
                c.statefile.display()
            ));
        }
    };
    let new = set_config_path(&state, path);
    let statefile = c.statefile.clone();
    tokio::task::spawn_blocking(move || write_in_place(&statefile, new.as_bytes()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("cannot write the EQ engine's state file: {e}"))?;
    restart(cfg, runner, why).await
}

/// Set the boost filter's gain in the active configuration and apply it.
/// Call with the file lock held.
///
/// # Errors
/// No active configuration (or none known), one outside `configs_dir`,
/// no usable filter, the file cannot be written, or the engine refused.
pub async fn set_boost(
    cfg: &ApplianceConfig,
    runner: &dyn Runner,
    db: f64,
) -> Result<Applied, String> {
    let c = &cfg.camilladsp;
    let client = Client::new(&c.websocket);
    let active = Active::read(c, &client).await?;
    let Some(path) = active.path else {
        return Err("the EQ engine has no active configuration".into());
    };
    let (path, _) = editable(c, &path)?;
    let yaml = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("cannot read the active EQ configuration: {e}"))?;
    let new = set_gain(&yaml, &c.gain_filter, db)?;
    if new != yaml {
        let p = path.clone();
        tokio::task::spawn_blocking(move || write_atomic(&p, new.as_bytes()))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("cannot write the active EQ configuration: {e}"))?;
    }
    match client.reload().await {
        Ok(()) => Ok(Applied::Reloaded),
        Err(WsError::Rejected(m)) => Err(format!("the EQ engine refused: {m}")),
        Err(e) => restart(cfg, runner, e.to_string()).await,
    }
}

/// Re-check the active configuration, set its capture rate and apply it.
/// Call with the file lock held; status must be freshly read from the daemon.
///
/// # Errors
/// No fixable mismatch, an unsafe or changed file, or a write/reload failure.
pub async fn set_capture_rate(
    cfg: &ApplianceConfig,
    runner: &dyn Runner,
    status: &Status,
) -> Result<(u32, String), String> {
    let c = &cfg.camilladsp;
    let client = Client::new(&c.websocket);
    let active = Active::read(c, &client).await?;
    let path = active
        .path
        .ok_or_else(|| "the EQ engine has no active configuration".to_owned())?;
    let (path, name) = editable(c, &path)?;
    let yaml = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("cannot read the active EQ configuration: {e}"))?;
    let (new, rate) = capture_rate::prepare(&yaml, &status.devices)?;
    tokio::task::spawn_blocking(move || {
        let current = files::read_regular(&path)
            .map_err(|e| format!("cannot re-read the active EQ configuration: {e}"))?;
        if current != yaml.as_bytes() {
            return Err("The active EQ configuration changed while checking it; reload the Speaker page and try again.".into());
        }
        write_atomic(&path, new.as_bytes())
            .map_err(|e| format!("cannot write the active EQ configuration: {e}"))
    })
    .await
    .map_err(|e| e.to_string())??;
    let applied = match client.reload().await {
        Ok(()) => Ok(Applied::Reloaded),
        Err(WsError::Rejected(m)) => Err(format!("the EQ engine refused: {m}")),
        Err(e) => restart(cfg, runner, e.to_string()).await,
    };
    match applied {
        Ok(Applied::Reloaded | Applied::Restarted) => Ok((rate, name)),
        Ok(Applied::Saved(why)) | Err(why) => Err(format!(
            "Capture rate saved in {name}, but the EQ engine could not reload: {why}. Restart the EQ engine after correcting this."
        )),
    }
}

// ---- the engine's GUI ---------------------------------------------------------------------

/// Longest host accepted from the `Host` header (a DNS name's limit).
const HOST_MAX: usize = 253;

/// The host the browser used to reach the portal, from the `Host` header
/// with the port removed: `speaker.local`, `203.0.113.231`, `[fe80::1]`.
/// Only letters, digits, `.`, `-`, `_`, `:`, `[` and `]` are accepted, a
/// `:` only inside brackets (an IPv6 literal) or before the port; anything
/// else, a missing header or an empty value is `None`. The header is
/// untrusted input: the result goes into a link, so nothing else of it
/// may pass.
#[must_use]
pub fn request_host(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::HOST)?.to_str().ok()?.trim();
    host_of(raw)
}

/// [`request_host`] on the header's value.
#[must_use]
pub fn host_of(raw: &str) -> Option<String> {
    if raw.is_empty()
        || raw.len() > HOST_MAX
        || !raw.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']')
        })
    {
        return None;
    }
    let (host, port) = if let Some(rest) = raw.strip_prefix('[') {
        // An IPv6 literal: `[` hex digits and colons `]`, then nothing or
        // `:port`.
        let (inner, after) = rest.split_once(']')?;
        let ipv6 = !inner.is_empty()
            && inner.contains(':')
            && inner
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || matches!(b, b':' | b'.'));
        if !ipv6 {
            return None;
        }
        let port = if after.is_empty() {
            None
        } else {
            Some(after.strip_prefix(':')?)
        };
        (format!("[{inner}]"), port)
    } else {
        if raw.contains(['[', ']']) {
            return None;
        }
        match raw.rsplit_once(':') {
            Some((h, _)) if h.contains(':') => return None,
            Some((h, p)) => (h.to_owned(), Some(p)),
            None => (raw.to_owned(), None),
        }
    };
    if let Some(p) = port {
        if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.is_empty() || bare.starts_with(['.', '-']) {
        return None;
    }
    Some(host)
}

/// The link to the engine's GUI: `template` (`gui_url`, validated by
/// [`crate::config::valid_gui_url`]) with `{host}` replaced by `host`, the
/// browser's host or the device's name. `None` when the template needs a
/// host and there is none; a template without the placeholder is returned
/// as it is.
#[must_use]
pub fn gui_link(template: &str, host: Option<&str>) -> Option<String> {
    if !template.contains(GUI_HOST_PLACEHOLDER) {
        return Some(template.to_owned());
    }
    let h = host.filter(|h| host_of(h).as_deref() == Some(h))?;
    Some(template.replace(GUI_HOST_PLACEHOLDER, h))
}

/// The "EQ engine (CamillaDSP)" button: a link to the GUI in a new tab,
/// with the engine's state next to it (the GUI does not load while the
/// engine is off, and this says so). `small` is the hero's variant.
#[must_use]
pub fn gui_button(url: &str, playback: Option<&Playback>, small: bool) -> Markup {
    let off = matches!(playback, Some(Playback::Off(_)));
    html! {
        span.eqgui {
            a.btn.primary.sm[small] href=(url) target="_blank" rel="noopener noreferrer"
                title=[playback.map(Playback::detail)] {
                "EQ engine (CamillaDSP)"
                @if !small { " ↗" }
            }
            @if let Some(p) = playback {
                @if off {
                    span.status.off title=(p.detail()) { "engine off" }
                } @else if !small {
                    span.status.on title=(p.detail()) { "engine running" }
                }
            }
        }
    }
}

// ---- the page ---------------------------------------------------------------------------------

/// Everything the Speaker page shows of the engine, read in one go.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Playing / Idle / off.
    pub playback: Playback,
    /// The active configuration, or why it is unknown.
    pub active: Result<Active, String>,
    /// A recognised ALSA capture-rate error, if the engine reports one.
    pub stop_reason: Option<String>,
    /// A recognised ALSA format error, if the engine reports one.
    pub format_failure: Option<capture_rate::FormatFailure>,
    /// `configs_dir`, listed.
    pub configs: Vec<files::Entry>,
    /// `coeffs_dir`, listed.
    pub coeffs: Vec<files::Entry>,
}

impl Snapshot {
    /// Ask the engine (with [`PAGE_TIMEOUT`]) and read the files.
    pub async fn read(cfg: &CamillaDspConfig) -> Self {
        let client = Client::new(&cfg.websocket).with_timeout(PAGE_TIMEOUT);
        let (state, active, stop_reason) = tokio::join!(
            client.get_state(),
            Active::read(cfg, &client),
            client.get_stop_reason()
        );
        let (stop_reason, format_failure) = match stop_reason {
            Ok(v) => (
                capture_rate::stop_reason(&v),
                capture_rate::format_failure(&v),
            ),
            Err(e) => {
                tracing::debug!(%e, "EQ engine stop reason unavailable");
                (None, None)
            }
        };
        let configs = files::list(&cfg.configs_dir, Kind::Config).await;
        let coeffs = files::list(&cfg.coeffs_dir, Kind::Coeff).await;
        Self {
            playback: Playback::of(&state),
            active,
            stop_reason,
            format_failure,
            configs,
            coeffs,
        }
    }

    /// The active configuration's name for the chip.
    #[must_use]
    pub fn active_label(&self) -> &str {
        match &self.active {
            Ok(a) => a.label.as_deref().unwrap_or("none"),
            Err(_) => "unknown",
        }
    }
}

/// `fill-N` (N a multiple of 5) for `value` in `min..=max`: the filled part
/// of a range input's track.
#[must_use]
pub fn fill_class(value: f64, min: f64, max: f64) -> String {
    let f = if max > min {
        ((value - min) / (max - min)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    // 0..=20 fifths, rounded to the nearest; the cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = (f * 20.0).round() as u32 * 5;
    format!("fill-{n}")
}

/// `db` rounded to the nearest 0.5 and clamped into `min..=max` (`min`
/// when the range is empty); never a negative zero. Non-finite input
/// gives `min`: callers refuse it before.
#[must_use]
pub fn snap_db(db: f64, min: f64, max: f64) -> f64 {
    if !db.is_finite() || max < min {
        return min;
    }
    let v = ((db * 2.0).round() / 2.0).clamp(min, max);
    if v == 0.0 { 0.0 } else { v }
}

/// `+14.0` / `-12.0` / `0.0`: a signed value with one decimal, a real minus.
#[must_use]
pub fn signed_db(db: f64) -> String {
    if db > 0.0 {
        format!("+{db:.1}")
    } else if db < 0.0 {
        format!("\u{2212}{:.1}", -db)
    } else {
        "0.0".into()
    }
}

/// Is `db` of boost in the warning range (above [`BOOST_WARN_DB`])?
#[must_use]
pub fn boost_is_hot(db: f64) -> bool {
    db > BOOST_WARN_DB
}

/// The signal boost pane of the master card (`#boost`, where the form's
/// redirect lands). The slider exists only when the active configuration
/// has the gain filter; otherwise the pane says why not. The script finds
/// the form by `data-live-slider` and applies it on release; the warning
/// state (`hot`) is the server's until the script moves the slider.
#[must_use]
pub fn boost_pane(active: &Result<Active, String>, csrf: &str) -> Markup {
    let (min, max) = BOOST_RANGE;
    html! {
        div.pane id="boost" {
            h2 { "Signal boost " span.n { "for quiet sources" } }
            @match active {
                Ok(Active { boost_db: Some(db), label, .. }) => {
                    form.vol.live.hot[boost_is_hot(*db)] method="post" action="/speaker/boost" data-live-slider="boost" {
                        input type="hidden" name="csrf" value=(csrf);
                        div.readout {
                            output.big for="boost-db" { (signed_db(*db)) small { "dB" } }
                            div.sub { "saved in " (label.as_deref().unwrap_or("the active EQ config")) }
                        }
                        div.slider {
                            input id="boost-db" type="range" name="db" min=(min) max=(max) step="0.5" value=(format!("{db:.1}"))
                                class=(fill_class(*db, min, max)) aria-label="Signal boost, dB"
                                aria-valuetext=(format!("{} dB", signed_db(*db)));
                            div.ticks { span { "0" } span { "+15" } span { "+30" } }
                            // Shown by the stylesheet while the form is `hot`.
                            div.hint { "Above " (signed_db(BOOST_WARN_DB)) " dB most sources clip." }
                        }
                        div.apply {
                            button.btn.primary.nojs type="submit" { "Apply" }
                            span.note role="status" aria-live="polite" data-live-local {}
                        }
                    }
                }
                Ok(Active { path: None, .. }) => {
                    p.muted { "The EQ engine has no active configuration." }
                }
                Ok(_) => {
                    p.muted { "The active EQ configuration has no " code { "Gain" } " filter named for the signal boost (" code { "[appliance.camilladsp] gain_filter" } ", block style, dB)." }
                }
                Err(e) => { p.muted { (e) } }
            }
        }
    }
}

/// The boost form's fields.
#[derive(Debug, Deserialize)]
pub struct BoostForm {
    #[serde(default)]
    csrf: String,
    /// The gain, dB.
    #[serde(default)]
    db: String,
}

/// `POST /speaker/boost`. Posted in place (`X-Tessera-Live`) it answers
/// JSON with the gain as applied (`db`); otherwise a redirect to the pane
/// with a one-shot message.
pub async fn post_boost(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<BoostForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(a) = st
        .appliance
        .as_ref()
        .filter(|a| a.config.camilladsp.enabled)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(db) = parse_form_db(&f.db) else {
        return (StatusCode::BAD_REQUEST, "db must be a finite number").into_response();
    };
    let (min, max) = BOOST_RANGE;
    let db = snap_db(db, min, max);
    let r = {
        let _g = a.eq_lock.lock().await;
        set_boost(&a.config, &*a.runner, db).await
    };
    tracing::info!(session = %s.id, db, ok = r.is_ok(), "appliance signal boost");
    let text = match &r {
        Ok(how) => format!("Signal boost {} dB {}", signed_db(db), how.message()),
        Err(m) => m.clone(),
    };
    if is_live(&headers) {
        return live_answer(
            StatusCode::OK,
            r.is_ok(),
            &text,
            serde_json::json!({ "db": db, "hot": boost_is_hot(db) }),
        );
    }
    st.set_flash(&token, r.is_ok(), text);
    Redirect::to(BOOST_ANCHOR).into_response()
}

/// The capture-rate form carries only the session's CSRF token: the device
/// and its rate are resolved again on POST.
#[derive(Debug, Deserialize)]
pub struct CaptureRateForm {
    #[serde(default)]
    csrf: String,
}

/// `POST /speaker/capture-rate`.
pub async fn post_capture_rate(
    State(st): State<Shared>,
    headers: HeaderMap,
    Form(f): Form<CaptureRateForm>,
) -> Response {
    let (token, s) = match need_post(&st, &headers, &f.csrf) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(a) = st
        .appliance
        .as_ref()
        .filter(|a| a.config.camilladsp.enabled)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let r = {
        let _g = a.eq_lock.lock().await;
        match status_of(&st).await {
            Ok(status) => set_capture_rate(&a.config, &*a.runner, &status).await,
            Err(e) => Err(format!(
                "Cannot check the Tessera device's current rate: {e}"
            )),
        }
    };
    tracing::info!(
        session = %s.id,
        rate = r.as_ref().ok().map(|(rate, _)| *rate),
        ok = r.is_ok(),
        "appliance capture rate"
    );
    match r {
        Ok((rate, label)) => st.set_flash(
            &token,
            true,
            format!(
                "Capture rate set to {} in {label}; EQ engine reloaded.",
                khz(rate)
            ),
        ),
        Err(m) => st.set_flash(&token, false, m),
    }
    Redirect::to("/speaker#capture-rate").into_response()
}

/// A form's `db`: a finite number, or nothing.
#[must_use]
pub fn parse_form_db(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "---
devices:
  samplerate: 96000   # fixed
  chunksize: 1024
filters:
  highpass:
    type: Biquad
    parameters:
      type: Highpass
      freq: 40
      q: 0.7

  # the page edits this one
  dashboard_signal_boost:
    type: Gain
    parameters:
      gain: 14  # set from the page
      inverted: false
  other_gain:
    type: Gain
    parameters:
      scale: dB
      gain: -3.0
  linear_gain:
    type: Gain
    parameters:
      scale: linear
      gain: 2.0
  flow_gain:
    type: Gain
    parameters: {gain: 14}
  no_gain:
    type: Gain
    parameters:
      inverted: true
pipeline:
  - type: Filter
    channel: 0
    names:
      - dashboard_signal_boost
      - pipeline_only
";

    #[test]
    fn commands_serialise_as_the_engine_expects() {
        assert_eq!(Command::GetState.json(), "\"GetState\"");
        assert_eq!(Command::GetStopReason.json(), "\"GetStopReason\"");
        assert_eq!(Command::Reload.json(), "\"Reload\"");
        assert_eq!(Command::SetVolume(-12.0).json(), "{\"SetVolume\":-12.0}");
        assert_eq!(
            Command::SetConfigFilePath("/x/a.yml".into()).json(),
            "{\"SetConfigFilePath\":\"/x/a.yml\"}"
        );
        assert_eq!(Command::GetVersion.name(), "GetVersion");
        assert_eq!(Command::Stop.json(), "\"Stop\"");
        assert_eq!(Command::GetConfigJson.json(), "\"GetConfigJson\"");
        // The configuration travels as a JSON text, not as an object.
        assert_eq!(
            Command::SetConfigJson("{\"devices\":{\"samplerate\":96000}}".into()).json(),
            "{\"SetConfigJson\":\"{\\\"devices\\\":{\\\"samplerate\\\":96000}}\"}"
        );
    }

    #[test]
    fn replies_are_parsed_and_errors_typed() {
        assert_eq!(
            parse_reply(
                "GetState",
                "{\"GetState\":{\"result\":\"Ok\",\"value\":\"Running\"}}"
            ),
            Ok(Value::String("Running".into()))
        );
        assert_eq!(
            parse_reply("Reload", "{\"Reload\":{\"result\":\"Ok\"}}"),
            Ok(Value::Null)
        );
        assert_eq!(
            parse_reply(
                "SetConfigFilePath",
                "{\"SetConfigFilePath\":{\"result\":\"Error\",\"value\":\"no such file\"}}"
            ),
            Err(WsError::Rejected("no such file".into()))
        );
        // CamillaDSP 4.x: the error is the `result` itself, a name or a
        // one-key object with the message.
        assert_eq!(
            parse_reply(
                "SetConfigJson",
                "{\"SetConfigJson\":{\"result\":{\"ConfigValidationError\":\"bad samplerate\"}}}"
            ),
            Err(WsError::Rejected(
                "ConfigValidationError: bad samplerate".into()
            ))
        );
        assert_eq!(
            parse_reply(
                "Stop",
                "{\"Stop\":{\"result\":\"ShutdownInProgressError\"}}"
            ),
            Err(WsError::Rejected("ShutdownInProgressError".into()))
        );
        for bad in [
            "{\"GetVolume\":{\"result\":\"Ok\"}}",
            "nope",
            "[1]",
            "{\"GetState\":{\"result\":7}}",
            "{\"GetState\":{\"result\":\"\"}}",
            "{\"GetState\":{\"result\":{}}}",
            "{\"GetState\":{\"value\":\"Running\"}}",
        ] {
            assert!(
                matches!(parse_reply("GetState", bad), Err(WsError::Protocol(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn endpoints_are_parsed_with_ipv6_brackets() {
        assert_eq!(
            endpoint("ws://127.0.0.1:1234"),
            Ok(("127.0.0.1".into(), 1234))
        );
        assert_eq!(endpoint("ws://[::1]:1234"), Ok(("::1".into(), 1234)));
        // A zone id: `%25` in the URI, `%` for the resolver.
        assert_eq!(
            endpoint("ws://[fe80::1%25eth0]:5"),
            Ok(("fe80::1%eth0".into(), 5))
        );
        assert_eq!(endpoint("ws://host/path"), Ok(("host".into(), 80)));
        assert!(matches!(endpoint("ws://"), Err(WsError::Protocol(_))));
        assert!(matches!(endpoint(""), Err(WsError::Protocol(_))));
    }

    #[test]
    fn states_map_to_playing_idle_off() {
        assert_eq!(Playback::of(&Ok("Running".into())), Playback::Playing);
        for s in ["Paused", "Inactive", "Stalled", "Starting", "Something", ""] {
            let p = Playback::of(&Ok(s.into()));
            assert_eq!(p, Playback::Idle(s.into()));
            assert_eq!(p.label(), "Idle");
            assert_eq!(p.ring_class(), "ring off");
        }
        let off = Playback::of(&Err(WsError::Unreachable("refused".into())));
        assert_eq!(off.label(), "EQ engine off");
        assert_eq!(off.ring_class(), "ring off");
        assert!(off.detail().contains("refused"));
        assert_eq!(Playback::Playing.ring_class(), "ring");
        assert_eq!(Playback::Idle("Paused".into()).detail(), "EQ engine paused");
    }

    #[test]
    fn gain_is_read_from_a_usable_gain_filter_only() {
        assert_eq!(read_gain(CONFIG, "dashboard_signal_boost"), Some(14.0));
        assert_eq!(read_gain(CONFIG, "other_gain"), Some(-3.0));
        for not in [
            "highpass",
            "nope",
            "linear_gain",
            "flow_gain",
            "no_gain",
            "pipeline_only",
            "dashboard",
            "dashboard_signal_boost2",
        ] {
            assert_eq!(read_gain(CONFIG, not), None, "{not}");
        }
        assert_eq!(read_gain("filters: {}\n", "x"), None);
        assert_eq!(read_gain("", "x"), None);
        // A `gain:` outside `filters` does not count.
        assert_eq!(
            read_gain(
                "mixers:\n  m:\n    type: Gain\n    parameters:\n      gain: 1\n",
                "m"
            ),
            None
        );
        // Quoted keys, quoted values, Windows line endings, tabs after the colon.
        assert_eq!(
            read_gain(
                "filters:\r\n  \"b\":\r\n    type:\tGain\r\n    parameters:\r\n      gain: '2.5'\r\n",
                "b"
            ),
            Some(2.5)
        );
        // A filter whose name is a prefix of another, listed first.
        let y = "filters:\n  g:\n    type: Gain\n    parameters:\n      gain: 1\n  g2:\n    type: Gain\n    parameters:\n      gain: 2\n";
        assert_eq!(read_gain(y, "g2"), Some(2.0));
        assert_eq!(read_gain(y, "g"), Some(1.0));
        // A missing value.
        assert_eq!(
            read_gain(
                "filters:\n  g:\n    type: Gain\n    parameters:\n      gain:  # c\n",
                "g"
            ),
            None
        );
    }

    #[test]
    fn set_gain_changes_one_value_and_nothing_else() {
        let out = set_gain(CONFIG, "dashboard_signal_boost", 7.5).unwrap();
        assert_eq!(
            out,
            CONFIG.replace(
                "gain: 14  # set from the page",
                "gain: 7.5  # set from the page"
            )
        );
        assert_eq!(read_gain(&out, "dashboard_signal_boost"), Some(7.5));
        assert_eq!(read_gain(&out, "other_gain"), Some(-3.0));
        assert_eq!(out.len(), CONFIG.len() + 1);
        let out = set_gain(CONFIG, "other_gain", 0.0).unwrap();
        assert_eq!(out, CONFIG.replace("gain: -3.0", "gain: 0.0"));
        for (not, why) in [
            ("highpass", "not a Gain filter (type Biquad)"),
            ("nope", "no filter `nope`"),
            ("linear_gain", "`scale: linear`"),
            ("flow_gain", "flow style"),
            ("no_gain", "has no `gain`"),
            ("pipeline_only", "no filter `pipeline_only`"),
        ] {
            let e = set_gain(CONFIG, not, 1.0).unwrap_err();
            assert!(e.contains(why), "{not}: {e}");
        }
        assert!(
            set_gain("devices: {}\n", "x", 1.0)
                .unwrap_err()
                .contains("no `filters`")
        );
        // No value: one is added, a comment kept.
        let out = set_gain(
            "filters:\n  g:\n    type: Gain\n    parameters:\n      gain:\n",
            "g",
            3.0,
        )
        .unwrap();
        assert_eq!(
            out,
            "filters:\n  g:\n    type: Gain\n    parameters:\n      gain: 3.0\n"
        );
        let out = set_gain(
            "filters:\n  g:\n    type: Gain\n    parameters:\n      gain:  # c\n",
            "g",
            3.0,
        )
        .unwrap();
        assert_eq!(
            out,
            "filters:\n  g:\n    type: Gain\n    parameters:\n      gain: 3.0  # c\n"
        );
        // The last line without a newline; a quoted value with a `#` in it.
        let out = set_gain(
            "filters:\n  g:\n    type: Gain\n    parameters:\n      gain: 1",
            "g",
            2.0,
        )
        .unwrap();
        assert_eq!(
            out,
            "filters:\n  g:\n    type: Gain\n    parameters:\n      gain: 2.0"
        );
        let out = set_gain(
            "filters:\n  g:\n    type: Gain\n    parameters:\n      gain: \"1 # x\" # c\n",
            "g",
            2.0,
        )
        .unwrap();
        assert_eq!(
            out,
            "filters:\n  g:\n    type: Gain\n    parameters:\n      gain: 2.0 # c\n"
        );
        // CRLF kept.
        let out = set_gain(
            "filters:\r\n  g:\r\n    type: Gain\r\n    parameters:\r\n      gain: 1\r\n",
            "g",
            2.0,
        )
        .unwrap();
        assert_eq!(
            out,
            "filters:\r\n  g:\r\n    type: Gain\r\n    parameters:\r\n      gain: 2.0\r\n"
        );
    }

    #[test]
    fn value_spans_respect_quotes_and_comments() {
        let span = |s: &str| {
            let (a, b) = value_span(s);
            s[a..b].to_owned()
        };
        assert_eq!(span(" 14  # c"), "14");
        assert_eq!(span(" \"a # b\" # c"), "\"a # b\"");
        assert_eq!(span(" 'it''s # x' # c"), "'it''s # x'");
        assert_eq!(span(" \"a \\\" b\""), "\"a \\\" b\"");
        assert_eq!(span("  # c"), "");
        assert_eq!(span(""), "");
        assert_eq!(span(" a#b"), "a#b");
        assert_eq!(unquote("\"a\\n\""), "a\n");
        assert_eq!(unquote("'it''s'"), "it's");
    }

    #[test]
    fn config_path_is_read_and_rewritten() {
        let st = "config_path: /opt/eq/configs/a.yml\nmute: false\nvolume: -6.0\n";
        assert_eq!(
            read_config_path(st),
            Some(PathBuf::from("/opt/eq/configs/a.yml"))
        );
        let new = set_config_path(st, Path::new("/opt/eq/configs/b.yml"));
        assert_eq!(
            new,
            "config_path: /opt/eq/configs/b.yml\nmute: false\nvolume: -6.0\n"
        );
        assert_eq!(read_config_path("config_path: null\n"), None);
        assert_eq!(
            read_config_path("config_path: \"/a b/c.yml\"\n"),
            Some(PathBuf::from("/a b/c.yml"))
        );
        assert_eq!(read_config_path("mute: false\n"), None);
        assert_eq!(read_config_path(""), None);
        // Added when absent; quoted when needed.
        assert_eq!(
            set_config_path("mute: false", Path::new("/a b/c\"d.yml")),
            "mute: false\nconfig_path: \"/a b/c\\\"d.yml\"\n"
        );
        assert_eq!(
            set_config_path("", Path::new("/x.yml")),
            "config_path: /x.yml\n"
        );
        // Nested `config_path` keys are not the top-level one.
        let st = "other:\n  config_path: /nested\nconfig_path: /top\n";
        assert_eq!(read_config_path(st), Some(PathBuf::from("/top")));
        assert_eq!(
            set_config_path(st, Path::new("/new")),
            "other:\n  config_path: /nested\nconfig_path: /new\n"
        );
    }

    #[test]
    fn fill_classes_and_snapping() {
        assert_eq!(fill_class(-60.0, -60.0, 0.0), "fill-0");
        assert_eq!(fill_class(-30.0, -60.0, 0.0), "fill-50");
        assert_eq!(fill_class(0.0, -60.0, 0.0), "fill-100");
        assert_eq!(fill_class(14.0, 0.0, 30.0), "fill-45");
        assert_eq!(fill_class(99.0, 0.0, 30.0), "fill-100");
        assert_eq!(fill_class(-99.0, 0.0, 30.0), "fill-0");
        assert_eq!(fill_class(1.0, 1.0, 1.0), "fill-0");
        assert_eq!(snap_db(-12.26, -60.0, 0.0), -12.5);
        assert_eq!(snap_db(-12.24, -60.0, 0.0), -12.0);
        assert_eq!(snap_db(5.0, -60.0, 0.0), 0.0);
        assert_eq!(snap_db(-99.0, -60.0, 0.0), -60.0);
        assert_eq!(snap_db(30.3, 0.0, 30.0), 30.0);
        // Never a negative zero, never a panic.
        assert!(snap_db(-0.1, -60.0, 0.0).is_sign_positive());
        assert!(snap_db(-0.0, 0.0, 30.0).is_sign_positive());
        assert_eq!(snap_db(f64::NAN, -60.0, 0.0), -60.0);
        assert_eq!(snap_db(f64::INFINITY, -60.0, 0.0), -60.0);
        assert_eq!(snap_db(1.0, 5.0, 2.0), 5.0);
        assert_eq!(signed_db(14.0), "+14.0");
        assert_eq!(signed_db(-12.5), "\u{2212}12.5");
        assert_eq!(signed_db(0.0), "0.0");
        // The boost's own range and warning threshold.
        let (lo, hi) = BOOST_RANGE;
        assert_eq!(snap_db(-5.0, lo, hi), 0.0);
        assert_eq!(snap_db(31.0, lo, hi), 30.0);
        assert_eq!(snap_db(12.26, lo, hi), 12.5);
        assert!(!boost_is_hot(12.0) && !boost_is_hot(0.0));
        assert!(boost_is_hot(12.5) && boost_is_hot(30.0));
    }

    #[test]
    fn the_browsers_host_is_taken_strictly_from_the_header() {
        for (raw, want) in [
            ("speaker", Some("speaker")),
            ("speaker.local:8090", Some("speaker.local")),
            ("203.0.113.231:8090", Some("203.0.113.231")),
            ("203.0.113.231", Some("203.0.113.231")),
            ("Speaker-1_a.example.", Some("Speaker-1_a.example.")),
            ("[::1]:8090", Some("[::1]")),
            ("[fe80::1]", Some("[fe80::1]")),
            ("[::ffff:10.0.0.1]:80", Some("[::ffff:10.0.0.1]")),
            (" speaker:8090 ", Some("speaker")),
        ] {
            assert_eq!(host_of(raw.trim()).as_deref(), want, "{raw:?}");
        }
        for bad in [
            "",
            "::1",
            "a:b:c",
            "speaker:",
            "speaker:abc",
            "speaker:123456",
            "[::1]:",
            "[::1]x",
            "[::1",
            "[]",
            "[speaker]",
            "[::1]:80:1",
            "speaker]",
            "-speaker",
            ".speaker",
            "evil.example\"><script>alert(1)</script>",
            "evil.example/path",
            "evil.example?x=1",
            "evil.example#x",
            "user@evil.example",
            "evil.example\\x",
            "evil example",
            "ev\u{e9}l.example",
            "evil.example%2f",
            "evil.example'",
            "evil.example{host}",
        ] {
            assert_eq!(host_of(bad), None, "{bad:?}");
        }
        assert_eq!(host_of(&"a".repeat(254)), None);
        assert!(host_of(&"a".repeat(253)).is_some());
        let mut h = HeaderMap::new();
        assert_eq!(request_host(&h), None);
        h.insert(
            axum::http::header::HOST,
            "speaker.local:8090".parse().unwrap(),
        );
        assert_eq!(request_host(&h).as_deref(), Some("speaker.local"));
        h.insert(axum::http::header::HOST, "<b>x</b>".parse().unwrap());
        assert_eq!(request_host(&h), None);
    }

    #[test]
    fn the_gui_link_fills_the_placeholder_or_stays() {
        let t = "http://{host}:5005/gui/index.html";
        assert_eq!(
            gui_link(t, Some("203.0.113.231")).as_deref(),
            Some("http://203.0.113.231:5005/gui/index.html")
        );
        assert_eq!(
            gui_link(t, Some("[fe80::1]")).as_deref(),
            Some("http://[fe80::1]:5005/gui/index.html")
        );
        assert_eq!(gui_link(t, None), None);
        // A fallback that is not a host (an empty or odd host name) is not
        // used either.
        assert_eq!(gui_link(t, Some("")), None);
        assert_eq!(gui_link(t, Some("x\"y")), None);
        assert_eq!(gui_link(t, Some("speaker:8090")), None);
        let plain = "http://speaker.example:5005/gui/index.html";
        assert_eq!(gui_link(plain, None).as_deref(), Some(plain));
        assert_eq!(gui_link(plain, Some("other")).as_deref(), Some(plain));
        // The button escapes what it links to and marks a stopped engine.
        let off = Playback::Off("connection refused".into());
        let b = gui_button("http://a/\"<x>", Some(&off), false).into_string();
        assert!(b.contains("href=\"http://a/&quot;&lt;x&gt;\""), "{b}");
        assert!(
            b.contains("target=\"_blank\" rel=\"noopener noreferrer\""),
            "{b}"
        );
        assert!(b.contains("engine off"), "{b}");
        let on = gui_button("http://a/", Some(&Playback::Playing), true).into_string();
        assert!(
            !on.contains("engine off") && on.contains("EQ engine (CamillaDSP)"),
            "{on}"
        );
        assert_eq!(parse_form_db(" -3.5 "), Some(-3.5));
        for bad in ["NaN", "inf", "-inf", "1e309", "", "x"] {
            assert_eq!(parse_form_db(bad), None, "{bad}");
        }
    }

    #[test]
    fn display_name_is_the_file_name_inside_configs_dir() {
        let c = CamillaDspConfig::default();
        assert_eq!(
            display_name(&c, Path::new("/opt/camilladsp/configs/a.yml")),
            "a.yml"
        );
        assert_eq!(display_name(&c, Path::new("/etc/x/a.yml")), "/etc/x/a.yml");
        let a = Active {
            path: Some("/opt/camilladsp/configs/a.yml".into()),
            label: Some("a.yml".into()),
            boost_db: None,
            source: Source::Engine,
        };
        // Neither exists: compared literally.
        assert!(a.is(&c, "a.yml") && !a.is(&c, "b.yml"));
    }

    #[test]
    fn same_file_follows_inodes_and_links() {
        let dir = std::env::temp_dir().join(format!("tessera-web-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let a = dir.join("a.yml");
        std::fs::write(&a, "x").unwrap();
        std::fs::write(dir.join("b.yml"), "x").unwrap();
        std::os::unix::fs::symlink(&a, dir.join("link.yml")).unwrap();
        assert!(same_file(&a, &dir.join("sub/../a.yml")));
        assert!(same_file(&a, &dir.join("link.yml")));
        assert!(!same_file(&a, &dir.join("b.yml")));
        assert!(!same_file(&a, &dir.join("missing.yml")));
        assert!(same_file(&dir.join("m.yml"), &dir.join("m.yml")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
