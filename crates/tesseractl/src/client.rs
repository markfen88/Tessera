//! Blocking client for `control.sock`: length-delimited (`u32` big-endian)
//! JSON frames carrying the envelopes of `tessera-control-types`.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tessera_control_types::{
    CommandError, Reply, Request, RequestBody, Response, Status, WIRE_VERSION,
};

/// Default cap on a response frame (`--max-response-bytes`). It protects the
/// client from a misbehaving daemon; the daemon itself never sends more than
/// its own frame limit (`[control] max_frame_bytes`, 1 MiB by default, which
/// it announces in `limits` of the status).
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 << 20;

/// What can go wrong talking to the daemon.
#[derive(Debug)]
pub enum ClientError {
    /// The socket could not be opened.
    Connect {
        /// The socket path.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// Reading or writing failed.
    Io(io::Error),
    /// The daemon did not answer in time.
    Timeout,
    /// The daemon sent something that is not a valid response.
    Protocol(String),
    /// A response is larger than this client accepts
    /// (`--max-response-bytes`). Nothing was read of it; the connection is
    /// not usable afterwards.
    ResponseTooLarge {
        /// Size announced by the daemon, bytes.
        size: u64,
        /// This client's limit, bytes.
        limit: usize,
        /// The daemon's own frame limit, if this connection has read a
        /// status (`limits.max_frame_bytes`).
        daemon_limit: Option<u64>,
    },
    /// The daemon refused the request.
    Command(CommandError),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect { path, source } => {
                write!(f, "cannot connect to {}: {source}", path.display())
            }
            Self::Io(e) => write!(f, "connection error: {e}"),
            Self::Timeout => f.write_str("the daemon did not answer in time"),
            Self::Protocol(m) => write!(f, "unexpected data from the daemon: {m}"),
            Self::ResponseTooLarge { size, limit, .. } => write!(
                f,
                "response of {size} bytes exceeds client limit {limit} (--max-response-bytes)"
            ),
            // The daemon refused to send a reply larger than its own frame
            // limit; the connection stays usable.
            Self::Command(CommandError::ResponseTooLarge { size, limit, key }) => write!(
                f,
                "response too large: {size} bytes, limit {limit} (config key {})",
                crate::sanitize::clean(key)
            ),
            Self::Command(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Self::Timeout,
            _ => Self::Io(e),
        }
    }
}

/// An open connection.
#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
    next_id: u64,
    max_response: usize,
    daemon_limit: Option<u64>,
}

impl Client {
    /// Connect to the control socket; `timeout` bounds each read and write.
    ///
    /// # Errors
    /// [`ClientError::Connect`].
    pub fn connect(
        path: &Path,
        timeout: Duration,
        max_response: usize,
    ) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path).map_err(|source| ClientError::Connect {
            path: path.to_owned(),
            source,
        })?;
        let t = (!timeout.is_zero()).then_some(timeout);
        stream.set_read_timeout(t)?;
        stream.set_write_timeout(t)?;
        Ok(Self {
            stream,
            next_id: 1,
            max_response,
            daemon_limit: None,
        })
    }

    /// The daemon's frame limit (`limits.max_frame_bytes`), once a status
    /// has been read on this connection; `None` before, or from a daemon
    /// that does not announce it.
    #[must_use]
    pub fn daemon_limit(&self) -> Option<u64> {
        self.daemon_limit
    }

    /// Change the read timeout (`None` = wait forever).
    ///
    /// # Errors
    /// The socket option could not be set.
    pub fn set_read_timeout(&self, t: Option<Duration>) -> Result<(), ClientError> {
        self.stream.set_read_timeout(t)?;
        Ok(())
    }

    /// Send one request and return its id.
    ///
    /// # Errors
    /// I/O or serialisation failure.
    pub fn send(&mut self, body: RequestBody) -> Result<u64, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request {
            v: WIRE_VERSION,
            id,
            body,
        };
        let json = serde_json::to_vec(&req).map_err(|e| ClientError::Protocol(e.to_string()))?;
        let len = u32::try_from(json.len())
            .map_err(|_| ClientError::Protocol("request too large".into()))?;
        let mut frame = Vec::with_capacity(4 + json.len());
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(&json);
        self.stream.write_all(&frame)?;
        Ok(id)
    }

    /// Read the next response frame.
    ///
    /// # Errors
    /// I/O failure, timeout, or malformed data.
    pub fn read_response(&mut self) -> Result<Response, ClientError> {
        let mut hdr = [0u8; 4];
        self.stream
            .read_exact(&mut hdr)
            .map_err(|e| match e.kind() {
                io::ErrorKind::UnexpectedEof => {
                    ClientError::Protocol("the daemon closed the connection".into())
                }
                _ => e.into(),
            })?;
        let len = u32::from_be_bytes(hdr) as usize;
        if len > self.max_response {
            return Err(ClientError::ResponseTooLarge {
                size: len as u64,
                limit: self.max_response,
                daemon_limit: self.daemon_limit,
            });
        }
        // Grow with the data actually received instead of trusting the
        // announced length for an up-front allocation.
        let mut body = Vec::new();
        let got = (&mut self.stream).take(len as u64).read_to_end(&mut body)?;
        if got != len {
            return Err(ClientError::Protocol(
                "the daemon closed the connection in the middle of a response".into(),
            ));
        }
        serde_json::from_slice(&body).map_err(|e| ClientError::Protocol(e.to_string()))
    }

    /// Send a request and wait for its answer.
    ///
    /// # Errors
    /// Transport errors, or [`ClientError::Command`] when the daemon refuses.
    pub fn request(&mut self, body: RequestBody) -> Result<Reply, ClientError> {
        let id = self.send(body)?;
        loop {
            let resp = self.read_response()?;
            if resp.id == id {
                return resp.result.map_err(ClientError::Command);
            }
            // An answer to an older request (or an id-0 protocol error that
            // does not belong to us) is skipped.
            if resp.id == 0 {
                if let Err(e) = resp.result {
                    return Err(ClientError::Command(e));
                }
            }
        }
    }

    /// The daemon's status snapshot.
    ///
    /// # Errors
    /// As for [`request`](Self::request), or a protocol error if the daemon
    /// answers with the wrong kind of reply.
    pub fn status(&mut self) -> Result<Status, ClientError> {
        match self.request(RequestBody::Query(tessera_control_types::Query::Status))? {
            Reply::Status(s) => {
                self.daemon_limit = s.limits.map(|l| l.max_frame_bytes);
                Ok(s)
            }
            other => Err(ClientError::Protocol(format!(
                "expected a status, got {other:?}"
            ))),
        }
    }
}
