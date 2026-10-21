//! Error type of the client (ADR 0009: per-crate enum, hand-written
//! `Display`).

use std::fmt;
use std::io;

/// Everything that can go wrong on the (non real-time) setup and control
/// paths of the client.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An OS call failed.
    Io(io::Error),
    /// The audio IPC layer reported an error.
    Ipc(tessera_audio_ipc::Error),
    /// The daemon refused the request (`Reject`).
    Rejected {
        /// Machine-readable code of the reject.
        code: tessera_audio_ipc::proto::RejectCode,
        /// Human-readable text sent by the daemon.
        message: String,
    },
    /// The peer sent a message that is valid but not allowed at this point.
    Protocol(&'static str),
    /// The control socket answered with something we cannot use.
    Control(String),
    /// A step did not finish in time.
    Timeout(&'static str),
    /// The daemon closed the connection.
    Closed,
    /// The command line or a configuration value is invalid.
    Config(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Ipc(e) => write!(f, "audio IPC error: {e}"),
            Error::Rejected { code, message } => {
                write!(f, "daemon rejected the request ({code:?}): {message}")
            }
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Control(m) => write!(f, "control socket: {m}"),
            Error::Timeout(what) => write!(f, "timed out: {what}"),
            Error::Closed => f.write_str("the daemon closed the connection"),
            Error::Config(m) => write!(f, "configuration: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Ipc(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<tessera_audio_ipc::Error> for Error {
    fn from(e: tessera_audio_ipc::Error) -> Self {
        Error::Ipc(e)
    }
}
