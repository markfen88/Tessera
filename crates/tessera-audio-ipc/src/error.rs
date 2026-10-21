//! Error types. Hand-written `Display`/`Error` impls (ADR 0009), no
//! dependencies. [`RingError`] and [`OverlayError`] are `Copy` and carry no
//! heap data so they can be produced on real-time threads.

use std::fmt;
use std::io;

/// Errors from the hot-path ring operations. `Copy`, allocation-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingError {
    /// The channel index is not below the ring channel count.
    ChannelOutOfRange,
    /// The sample type does not match the ring sample format.
    FormatMismatch,
    /// `pos + len` does not fit in a `u64`.
    PositionOverflow,
}

impl fmt::Display for RingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RingError::ChannelOutOfRange => "channel index out of range",
            RingError::FormatMismatch => "sample type does not match ring sample format",
            RingError::PositionOverflow => "media position overflows u64",
        })
    }
}

impl std::error::Error for RingError {}

/// Errors from reading the clock overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayError {
    /// The seqlock stayed odd or kept changing for the whole retry budget
    /// (the writer is mid-publish, or died mid-publish). The caller should
    /// keep using its last good snapshot.
    Unstable,
}

impl fmt::Display for OverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OverlayError::Unstable => f.write_str("clock overlay unstable (torn or stuck seqlock)"),
        }
    }
}

impl std::error::Error for OverlayError {}

/// A shared-memory header or parameter set failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutError(pub &'static str);

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid shared-memory layout: {}", self.0)
    }
}

impl std::error::Error for LayoutError {}

/// A handshake message failed to parse or build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoError {
    /// The payload ended before a field was complete.
    Truncated,
    /// Bytes remained after the last field.
    TrailingBytes,
    /// The frame length prefix is outside the accepted range.
    BadFrameLength(u32),
    /// Unsupported protocol version.
    BadVersion(u16),
    /// Unknown message kind.
    BadKind(u16),
    /// A field has an invalid value (the name of the field).
    BadField(&'static str),
    /// A string is too long to encode (limit 255 bytes).
    StringTooLong,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Truncated => f.write_str("message truncated"),
            ProtoError::TrailingBytes => f.write_str("trailing bytes after message"),
            ProtoError::BadFrameLength(n) => write!(f, "bad frame length {n}"),
            ProtoError::BadVersion(v) => write!(f, "unsupported protocol version {v}"),
            ProtoError::BadKind(k) => write!(f, "unknown message kind {k}"),
            ProtoError::BadField(name) => write!(f, "invalid field `{name}`"),
            ProtoError::StringTooLong => f.write_str("string longer than 255 bytes"),
        }
    }
}

impl std::error::Error for ProtoError {}

/// Setup-path and transport errors (these may allocate; not for RT threads).
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An OS call failed.
    Io(io::Error),
    /// Header or parameter validation failed.
    Layout(LayoutError),
    /// Handshake message error.
    Proto(ProtoError),
    /// The memfd lacks the seals required to map it safely.
    NotSealed {
        /// Seals found (`F_GET_SEALS` bitmask).
        have: u32,
        /// Seals required.
        need: u32,
    },
    /// The peer closed the connection in the middle of a frame.
    UnexpectedEof,
    /// The peer sent a number of file descriptors the message kind does not allow.
    UnexpectedFds {
        /// Descriptors received.
        got: usize,
        /// Descriptors expected.
        want: usize,
    },
    /// The kernel truncated the ancillary data (`MSG_CTRUNC`, e.g. near
    /// `RLIMIT_NOFILE`); every descriptor it did install has been closed.
    ControlTruncated,
    /// The role handle (producer/consumer) is already taken in this process.
    AlreadyTaken,
    /// The operation requires the creating (owner) side of the mapping.
    NotOwner,
    /// A send did not complete within the timeout.
    Timeout,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Layout(e) => e.fmt(f),
            Error::Proto(e) => write!(f, "protocol error: {e}"),
            Error::NotSealed { have, need } => {
                write!(f, "memfd seals {have:#x} do not include required {need:#x}")
            }
            Error::UnexpectedEof => f.write_str("peer closed the connection mid-frame"),
            Error::UnexpectedFds { got, want } => {
                write!(f, "received {got} file descriptors, expected {want}")
            }
            Error::ControlTruncated => f.write_str("ancillary data truncated"),
            Error::AlreadyTaken => f.write_str("role handle already taken"),
            Error::NotOwner => f.write_str("operation requires the owning side"),
            Error::Timeout => f.write_str("timed out"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Layout(e) => Some(e),
            Error::Proto(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<nix::errno::Errno> for Error {
    fn from(e: nix::errno::Errno) -> Self {
        Error::Io(io::Error::from(e))
    }
}

impl From<LayoutError> for Error {
    fn from(e: LayoutError) -> Self {
        Error::Layout(e)
    }
}

impl From<ProtoError> for Error {
    fn from(e: ProtoError) -> Self {
        Error::Proto(e)
    }
}
