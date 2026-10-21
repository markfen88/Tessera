//! Device descriptions from the daemon's control socket and the mapping of a
//! logical device to PipeWire nodes.
//!
//! PROVISIONAL (see "Open spec questions" in PROGRESS.md): `audio.sock`
//! carries no device list or channel names, so the client asks the control
//! socket (`Query::Status`, ADR 0008 / `tessera-control-types`) and follows
//! `tessera-control-types`' envelope (`{"v":1,"id":n,"body":{"query":"status"}}`),
//! which differs from the sketch in ADR 0008. The framing is ADR 0008's
//! `u32` big-endian length prefix. Everything that depends on those open
//! points lives in this module.
//!
//! Everything received from the daemon is validated here before it reaches
//! PipeWire or the audio handshake: names are cleaned of control characters
//! (a NUL would panic in the PipeWire property setters) and bounded, channel
//! counts and the rate are range-checked, and a device that cannot be served
//! is rejected with a reason instead of being retried forever.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use tessera_control_types::{
    ChannelStatus, DeviceStatus, Query, Reply, Request, RequestBody, Response, WIRE_VERSION,
};

use crate::error::Error;

/// Largest control frame accepted (ADR 0010: default 1 MiB).
pub const MAX_CONTROL_FRAME: usize = 1 << 20;
/// Longest device key / name / label we keep, in bytes (the audio handshake
/// carries strings of at most 255 bytes).
pub const MAX_NAME_BYTES: usize = 255;
/// Highest sample rate accepted from the daemon.
pub const MAX_RATE_HZ: u32 = 768_000;
/// Most devices taken from one answer.
pub const MAX_DEVICES: usize = 1024;

/// What the client needs to know about one logical device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDesc {
    /// The operator-chosen device key (also the audio endpoint name).
    pub key: String,
    /// Display name.
    pub name: String,
    /// Sample rate in Hz.
    pub rate: u32,
    /// Labels of the network-receive channels (become the source node).
    pub rx_labels: Vec<String>,
    /// Labels of the network-transmit channels (become the sink node).
    pub tx_labels: Vec<String>,
}

/// Direction of an endpoint as the client sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    /// Network to graph: a PipeWire `Audio/Source`.
    Rx,
    /// Graph to network: a PipeWire `Audio/Sink`.
    Tx,
}

/// One PipeWire node and the audio endpoint behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointSpec {
    /// Device key (audio endpoint name).
    pub device: String,
    /// Display name of the device.
    pub device_name: String,
    /// Direction.
    pub dir: Dir,
    /// Index of this node among the device's nodes of this direction.
    pub part: usize,
    /// Number of parts of this direction.
    pub parts: usize,
    /// 1-based number of the first channel (as on the wire / in
    /// `tessera-control-types`).
    pub first_channel: u32,
    /// Labels of this node's channels.
    pub labels: Vec<String>,
    /// Sample rate in Hz.
    pub rate: u32,
}

impl EndpointSpec {
    /// A key that identifies the endpoint across descriptions.
    pub fn id(&self) -> String {
        let d = match self.dir {
            Dir::Rx => "rx",
            Dir::Tx => "tx",
        };
        format!("{}/{}/{}", self.device, d, self.part)
    }

    /// Number of channels of the node.
    pub fn channels(&self) -> u32 {
        self.labels.len() as u32
    }

    /// True if `other` differs only in labels / display name, which can be
    /// applied to a live node without recreating it.
    pub fn same_shape(&self, other: &EndpointSpec) -> bool {
        self.device == other.device
            && self.dir == other.dir
            && self.part == other.part
            && self.parts == other.parts
            && self.first_channel == other.first_channel
            && self.rate == other.rate
            && self.labels.len() == other.labels.len()
    }
}

/// Splits a device into endpoints of at most `max_channels` channels.
pub fn plan_endpoints(desc: &DeviceDesc, max_channels: u32) -> Vec<EndpointSpec> {
    let max = max_channels.max(1) as usize;
    let mut out = Vec::new();
    for (dir, labels) in [(Dir::Rx, &desc.rx_labels), (Dir::Tx, &desc.tx_labels)] {
        let parts = labels.len().div_ceil(max);
        for (part, chunk) in labels.chunks(max).enumerate() {
            out.push(EndpointSpec {
                device: desc.key.clone(),
                device_name: desc.name.clone(),
                dir,
                part,
                parts,
                first_channel: (part * max) as u32 + 1,
                labels: chunk.to_vec(),
                rate: desc.rate,
            });
        }
    }
    out
}

/// Replaces control characters (NUL included) by `?` and cuts the text to at
/// most `max` bytes on a character boundary.
pub fn clean(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max));
    for c in s.chars() {
        let c = if c.is_control() { '?' } else { c };
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

fn labels_of(prefix: &str, channels: &[ChannelStatus]) -> Vec<String> {
    channels
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let l = clean(c.label.trim(), MAX_NAME_BYTES);
            if l.is_empty() {
                format!("{prefix} {}", i + 1)
            } else {
                l
            }
        })
        .collect()
}

impl DeviceDesc {
    /// Converts and validates the daemon's status of one device.
    ///
    /// # Errors
    /// The reason the device cannot be served: empty, over-long or
    /// control-character keys (the key is the audio endpoint name, at most
    /// 255 bytes), a sample rate outside `1..=768000`, or more than
    /// `max_channels` channels in a direction.
    pub fn from_status(d: &DeviceStatus, max_channels: u32) -> Result<Self, String> {
        let key = &d.key.0;
        if key.is_empty() {
            return Err("empty device key".into());
        }
        if key.len() > MAX_NAME_BYTES {
            return Err(format!("device key of {} bytes (limit 255)", key.len()));
        }
        if key.chars().any(char::is_control) {
            return Err("device key contains control characters".into());
        }
        let rate = d.sample_rate.0;
        if rate == 0 || rate > MAX_RATE_HZ {
            return Err(format!("sample rate {rate} Hz out of range"));
        }
        let (rx, tx) = (d.rx_channels.len(), d.tx_channels.len());
        if rx > max_channels as usize || tx > max_channels as usize {
            return Err(format!(
                "{rx} RX / {tx} TX channels exceed the limit of {max_channels}"
            ));
        }
        let name = clean(d.name.trim(), MAX_NAME_BYTES);
        Ok(Self {
            key: key.clone(),
            name: if name.is_empty() { key.clone() } else { name },
            rate,
            rx_labels: labels_of("RX", &d.rx_channels),
            tx_labels: labels_of("TX", &d.tx_channels),
        })
    }
}

/// The accepted and the rejected devices of one answer.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    /// Devices that passed validation.
    pub devices: Vec<DeviceDesc>,
    /// One line per rejected device, for the log.
    pub rejected: Vec<String>,
}

/// The framed `Query::Status` request.
pub fn encode_status_request(id: u64) -> Result<Vec<u8>, Error> {
    let req = Request {
        v: WIRE_VERSION,
        id,
        body: RequestBody::Query(Query::Status),
    };
    let json = serde_json::to_vec(&req).map_err(|e| Error::Control(e.to_string()))?;
    let mut frame = Vec::with_capacity(json.len() + 4);
    frame.extend_from_slice(&(json.len() as u32).to_be_bytes());
    frame.extend_from_slice(&json);
    Ok(frame)
}

/// Parses the payload (without length prefix) of a `Query::Status` reply and
/// validates every device.
pub fn parse_status_response(payload: &[u8], max_channels: u32) -> Result<Parsed, Error> {
    let resp: Response =
        serde_json::from_slice(payload).map_err(|e| Error::Control(format!("bad reply: {e}")))?;
    match resp.result {
        Ok(Reply::Status(status)) => {
            let mut out = Parsed::default();
            for d in status.devices.iter().take(MAX_DEVICES) {
                match DeviceDesc::from_status(d, max_channels) {
                    Ok(desc) if out.devices.iter().any(|o| o.key == desc.key) => out
                        .rejected
                        .push(format!("device {:?}: duplicate key", desc.key)),
                    Ok(desc) => out.devices.push(desc),
                    Err(why) => out
                        .rejected
                        .push(format!("device {:?}: {why}", clean(&d.key.0, 64))),
                }
            }
            Ok(out)
        }
        Ok(_) => Err(Error::Control("unexpected reply kind".into())),
        Err(e) => Err(Error::Control(format!("daemon refused the query: {e}"))),
    }
}

fn map_io(e: std::io::Error, what: &'static str) -> Error {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Error::Timeout(what),
        std::io::ErrorKind::UnexpectedEof => Error::Closed,
        _ => Error::Io(e),
    }
}

/// Asks the control socket for the device list. Blocking, every step bounded
/// by `timeout`; run it off the real-time and main-loop threads.
pub fn fetch_devices(path: &Path, timeout: Duration, max_channels: u32) -> Result<Parsed, Error> {
    let mut sock = UnixStream::connect(path)?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    sock.write_all(&encode_status_request(1)?)
        .map_err(|e| map_io(e, "control request"))?;
    let mut hdr = [0u8; 4];
    sock.read_exact(&mut hdr)
        .map_err(|e| map_io(e, "control reply header"))?;
    let len = u32::from_be_bytes(hdr) as usize;
    if len == 0 || len > MAX_CONTROL_FRAME {
        return Err(Error::Control(format!("reply frame of {len} bytes")));
    }
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body)
        .map_err(|e| map_io(e, "control reply body"))?;
    parse_status_response(&body, max_channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(rx: usize, tx: usize) -> DeviceDesc {
        DeviceDesc {
            key: "main".into(),
            name: "Main".into(),
            rate: 48_000,
            rx_labels: (1..=rx).map(|i| format!("in{i}")).collect(),
            tx_labels: (1..=tx).map(|i| format!("out{i}")).collect(),
        }
    }

    fn reply(devices: &str) -> Vec<u8> {
        format!(
            r#"{{"v":1,"id":1,"result":{{"ok":{{"status":{{"version":"0","devices":[{devices}],"clocks":[],"warnings":[]}}}}}}}}"#
        )
        .into_bytes()
    }

    fn dev(key: &str, name: &str, rate: u32, rx: &str, tx: &str) -> String {
        format!(
            r#"{{"key":{key},"name":{name},"sample_rate":{rate},"encoding":24,"latency":4000,"rx_channels":[{rx}],"tx_channels":[{tx}],"subscriptions":[],"locked":[]}}"#
        )
    }

    #[test]
    fn plan_splits_by_direction_and_size() {
        let eps = plan_endpoints(&desc(130, 2), 64);
        let rx: Vec<_> = eps.iter().filter(|e| e.dir == Dir::Rx).collect();
        let tx: Vec<_> = eps.iter().filter(|e| e.dir == Dir::Tx).collect();
        assert_eq!(rx.len(), 3);
        assert_eq!(rx[0].channels(), 64);
        assert_eq!(rx[2].channels(), 2);
        assert_eq!(rx[1].first_channel, 65);
        assert_eq!(rx[2].first_channel, 129);
        assert_eq!(rx[2].parts, 3);
        assert_eq!(tx.len(), 1);
        assert_eq!(tx[0].id(), "main/tx/0");
        assert!(plan_endpoints(&desc(0, 0), 64).is_empty());
    }

    #[test]
    fn same_shape_ignores_labels_only() {
        let a = plan_endpoints(&desc(4, 0), 64).remove(0);
        let mut b = a.clone();
        b.labels[1] = "renamed".into();
        b.device_name = "Other".into();
        assert!(a.same_shape(&b));
        b.labels.pop();
        assert!(!a.same_shape(&b));
        let mut c = a.clone();
        c.rate = 96_000;
        assert!(!a.same_shape(&c));
    }

    #[test]
    fn request_frame_shape() {
        let f = encode_status_request(5).unwrap();
        let len = u32::from_be_bytes(f[..4].try_into().unwrap()) as usize;
        assert_eq!(len, f.len() - 4);
        assert_eq!(
            &f[4..],
            br#"{"v":1,"id":5,"body":{"query":"status"}}"#.as_slice()
        );
    }

    #[test]
    fn parses_a_status_reply() {
        let json = reply(&dev(
            r#""main""#,
            r#""Main""#,
            48_000,
            r#"{"channel":1,"label":"Alpha"},{"channel":2,"label":""}"#,
            r#"{"channel":1,"label":"Out"}"#,
        ));
        let p = parse_status_response(&json, 4096).unwrap();
        assert!(p.rejected.is_empty());
        assert_eq!(p.devices.len(), 1);
        assert_eq!(p.devices[0].key, "main");
        assert_eq!(p.devices[0].rate, 48_000);
        assert_eq!(p.devices[0].rx_labels, ["Alpha", "RX 2"]);
        assert_eq!(p.devices[0].tx_labels, ["Out"]);
    }

    #[test]
    fn error_and_garbage_replies_are_errors_not_panics() {
        let err = br#"{"v":1,"id":1,"result":{"err":"permission_denied"}}"#;
        assert!(parse_status_response(err, 4096).is_err());
        assert!(parse_status_response(b"{", 4096).is_err());
        assert!(parse_status_response(b"", 4096).is_err());
        assert!(parse_status_response(br#"{"v":1,"id":1,"result":{"ok":"done"}}"#, 4096).is_err());
    }

    #[test]
    fn hostile_devices_are_rejected_or_cleaned() {
        let long = "k".repeat(300);
        let devices = [
            dev(&format!("\"{long}\""), r#""n""#, 48_000, "", ""),
            dev(r#""zero-rate""#, r#""n""#, 0, "", ""),
            dev(r#""huge-rate""#, r#""n""#, 4_000_000_000, "", ""),
            dev(r#""nul\u0000key""#, r#""n""#, 48_000, "", ""),
            dev(r#""""#, r#""n""#, 48_000, "", ""),
            dev(
                r#""dup""#,
                r#""first""#,
                48_000,
                r#"{"channel":1,"label":"a"}"#,
                "",
            ),
            dev(r#""dup""#, r#""second""#, 48_000, "", ""),
            dev(
                r#""ok""#,
                r#""na\u0000me\u0007""#,
                44_100,
                r#"{"channel":1,"label":"la\u0000bel"},{"channel":2,"label":"   "}"#,
                "",
            ),
        ]
        .join(",");
        let p = parse_status_response(&reply(&devices), 4096).unwrap();
        let keys: Vec<_> = p.devices.iter().map(|d| d.key.as_str()).collect();
        assert_eq!(keys, ["dup", "ok"]);
        assert_eq!(p.rejected.len(), 6, "{:?}", p.rejected);
        let ok = &p.devices[1];
        assert_eq!(ok.name, "na?me?");
        assert_eq!(ok.rx_labels, ["la?bel", "RX 2"]);
        assert!(!ok.name.chars().any(char::is_control));
    }

    #[test]
    fn channel_count_is_bounded() {
        let ch: String = (1..=10)
            .map(|i| format!(r#"{{"channel":{i},"label":""}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let json = reply(&dev(r#""d""#, r#""d""#, 48_000, &ch, ""));
        assert_eq!(parse_status_response(&json, 10).unwrap().devices.len(), 1);
        let p = parse_status_response(&json, 9).unwrap();
        assert!(p.devices.is_empty() && p.rejected.len() == 1);
    }

    #[test]
    fn clean_cuts_on_character_boundaries() {
        assert_eq!(clean("a\0b", 10), "a?b");
        assert_eq!(clean("ééé", 5), "éé");
        assert_eq!(clean("abc", 0), "");
        assert!(clean(&"x".repeat(1000), 255).len() == 255);
    }
}
