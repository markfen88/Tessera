//! Standalone OS networking. The root helper is deliberately independent of
//! the audio daemon and web process, so their failures cannot cancel recovery.

pub mod backend;
pub mod engine;
pub mod handoff;
pub mod host;
pub mod ifupdown;
pub mod networkmanager;
pub mod store;
pub mod yaml;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use tessera_state::network_profiles::{Reply, Request};

/// Default private IPC endpoint; accessible only to root and tessera-web.
pub const SOCKET: &str = "/run/tessera-network/control.sock";
/// Wire messages are bounded before deserialization.
pub const MAX_MESSAGE: u64 = 65_536;
/// Helper operations never invoke a shell.
pub type Result<T> = std::result::Result<T, String>;

/// Read exactly one bounded JSON line. Trailing data is never another request.
pub fn read_line<T: serde::de::DeserializeOwned>(stream: impl Read) -> Result<T> {
    let mut line = String::new();
    BufReader::new(stream)
        .take(MAX_MESSAGE + 1)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if line.len() as u64 > MAX_MESSAGE || !line.ends_with('\n') {
        return Err("network message is incomplete or too large".into());
    }
    serde_json::from_str(&line).map_err(|e| e.to_string())
}

/// One request per connection, with bounded I/O and protocol verification.
pub fn request(path: &Path, request: &Request) -> Result<Reply> {
    let mut stream =
        UnixStream::connect(path).map_err(|e| format!("Network helper unavailable: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut data = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    data.push(b'\n');
    if data.len() as u64 > MAX_MESSAGE {
        return Err("network request too large".into());
    }
    stream.write_all(&data).map_err(|e| e.to_string())?;
    let reply: Reply = read_line(stream)?;
    if reply.api != request.api {
        return Err("Network helper API is incompatible; update the matching packages.".into());
    }
    Ok(reply)
}
