//! The error type of the client side (ADR 0009: per-crate error enum).

use std::fmt;
use std::io;

use tessera_audio_ipc::proto::RejectCode;

/// Everything that can go wrong between the PCM and the daemon.
#[derive(Debug)]
pub enum Error {
    /// A system call failed.
    Io(io::Error),
    /// The audio-ipc layer failed (framing, descriptors, mapping).
    Ipc(tessera_audio_ipc::Error),
    /// The daemon refused the request.
    Rejected {
        /// Machine-readable reason.
        code: RejectCode,
        /// Human-readable text from the daemon.
        message: String,
    },
    /// The daemon violated the protocol or granted something else than asked.
    Protocol(&'static str),
    /// The daemon closed the connection.
    Closed,
    /// A handshake step took too long.
    Timeout(&'static str),
    /// The PCM configuration is invalid.
    Config(String),
    /// The hardware parameters cannot be served.
    Params(String),
    /// The daemon is gone (closed sockets, stale clock overlay).
    Lost,
    /// The process behind the socket is not one of the users allowed to be
    /// the daemon (`daemon_uid`).
    Untrusted(String),
}

impl Error {
    /// The `errno` value (positive) an ALSA entry point returns, negated, for
    /// this error.
    pub fn errno(&self) -> i32 {
        match self {
            Error::Io(e) => io_errno(e),
            Error::Ipc(tessera_audio_ipc::Error::Io(e)) => io_errno(e),
            Error::Ipc(_) | Error::Protocol(_) => libc::EPROTO,
            Error::Rejected { code, .. } => match code {
                RejectCode::UnknownEndpoint => libc::ENODEV,
                RejectCode::BadRequest => libc::EINVAL,
                RejectCode::Busy => libc::EBUSY,
                RejectCode::PermissionDenied => libc::EACCES,
                RejectCode::UnsupportedVersion => libc::EPROTONOSUPPORT,
                RejectCode::Internal | RejectCode::Other(_) => libc::EIO,
            },
            Error::Closed | Error::Lost => libc::ENODEV,
            Error::Timeout(_) => libc::ETIMEDOUT,
            Error::Config(_) | Error::Params(_) => libc::EINVAL,
            Error::Untrusted(_) => libc::EACCES,
        }
    }
}

fn io_errno(e: &io::Error) -> i32 {
    match e.raw_os_error() {
        // No daemon at the socket path: the device is not there.
        Some(libc::ENOENT | libc::ECONNREFUSED | libc::ENOTDIR) => libc::ENODEV,
        Some(n) => n,
        None if e.kind() == io::ErrorKind::TimedOut => libc::ETIMEDOUT,
        None => libc::EIO,
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Ipc(e) => write!(f, "audio ipc: {e}"),
            Error::Rejected { code, message } => {
                write!(f, "the daemon refused the request ({code:?}): {message}")
            }
            Error::Protocol(m) => write!(f, "protocol violation: {m}"),
            Error::Closed => write!(f, "the daemon closed the connection"),
            Error::Timeout(what) => write!(f, "timed out waiting for the daemon ({what})"),
            Error::Config(m) => write!(f, "configuration: {m}"),
            Error::Params(m) => write!(f, "unsupported parameters: {m}"),
            Error::Lost => write!(f, "the daemon is gone"),
            Error::Untrusted(m) => {
                write!(f, "refusing to talk to the process behind the socket: {m}")
            }
        }
    }
}

impl std::error::Error for Error {}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_mapping() {
        let missing = Error::Io(io::Error::from_raw_os_error(libc::ENOENT));
        assert_eq!(missing.errno(), libc::ENODEV);
        let refused = Error::Io(io::Error::from_raw_os_error(libc::ECONNREFUSED));
        assert_eq!(refused.errno(), libc::ENODEV);
        let denied = Error::Io(io::Error::from_raw_os_error(libc::EACCES));
        assert_eq!(denied.errno(), libc::EACCES);
        for (code, want) in [
            (RejectCode::UnknownEndpoint, libc::ENODEV),
            (RejectCode::BadRequest, libc::EINVAL),
            (RejectCode::Busy, libc::EBUSY),
            (RejectCode::PermissionDenied, libc::EACCES),
        ] {
            let e = Error::Rejected {
                code,
                message: String::new(),
            };
            assert_eq!(e.errno(), want);
        }
        assert_eq!(Error::Lost.errno(), libc::ENODEV);
        assert_eq!(Error::Timeout("x").errno(), libc::ETIMEDOUT);
    }
}
