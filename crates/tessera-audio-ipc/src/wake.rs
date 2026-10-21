//! The per-session wake-up channel (ADR 0007 asked for an eventfd; this crate
//! uses a socketpair instead, see below).
//!
//! The daemon signals once per engine tick; the client polls its end for
//! readability. **The daemon must never block because of a client.** An
//! eventfd cannot guarantee that: it is a single shared object, so a client
//! holding it can fill the counter (write `0xffff_ffff_ffff_fffe`) and clear
//! `O_NONBLOCK` with `fcntl(F_SETFL)` on the *shared open file description*,
//! after which the daemon's next write blocks forever on the media-engine
//! thread.
//!
//! Instead each session gets its own `AF_UNIX`/`SOCK_STREAM` socketpair:
//!
//! * the daemon keeps one end ([`WakeSignal`]) and sends the other end
//!   ([`WakeWait`]) to the client with `SCM_RIGHTS`;
//! * the two ends are different open file descriptions, so nothing the client
//!   does to its end (flags, buffer sizes, `shutdown`, writing, closing) can
//!   change the daemon's end;
//! * the daemon sends one byte per tick with `MSG_DONTWAIT | MSG_NOSIGNAL`:
//!   a full buffer (`EAGAIN`) or a vanished client (`EPIPE`, `ECONNRESET`) is
//!   success, never a block and never `SIGPIPE`.
//!
//! **Invariant: one ring and one wake socketpair per session.** Nothing in the
//! wake channel is shared between sessions; the clock overlay is the only
//! object shared between sessions, and it is read-only for clients.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

use nix::errno::Errno;
use nix::sys::socket::{MsgFlags, SockType, getsockopt, recv, send, sockopt};
use nix::sys::stat::{SFlag, fstat};

use crate::error::{Error, LayoutError};

/// Creates a fresh per-session wake channel: `(daemon end, client end)`.
pub fn wake_pair() -> Result<(WakeSignal, WakeWait), Error> {
    let (a, b) = UnixStream::pair()?;
    a.set_nonblocking(true)?;
    b.set_nonblocking(true)?;
    Ok((WakeSignal { sock: a }, WakeWait { sock: b }))
}

/// The daemon's end. Can only signal; never blocks.
#[derive(Debug)]
pub struct WakeSignal {
    sock: UnixStream,
}

impl WakeSignal {
    /// Sends one tick. Returns promptly in every case: a full socket buffer or
    /// a closed/vanished client count as success. No allocation.
    pub fn signal(&self) -> Result<(), Error> {
        loop {
            match send(
                self.sock.as_raw_fd(),
                &[1u8],
                MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL,
            ) {
                Ok(_) => return Ok(()),
                Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN | Errno::EPIPE | Errno::ECONNRESET | Errno::ENOTCONN) => {
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// The client's end: poll it for readability, then [`WakeWait::drain`].
#[derive(Debug)]
pub struct WakeWait {
    sock: UnixStream,
}

impl WakeWait {
    /// Wraps the descriptor received in an `Attach`. Verifies that it is a
    /// stream socket (and nothing else, e.g. a regular file, whose contents
    /// `drain` would otherwise read) and makes it non-blocking. The flag
    /// change only affects this end's own open file description.
    pub fn from_fd(fd: OwnedFd) -> Result<Self, Error> {
        let st = fstat(&fd)?;
        if SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT != SFlag::S_IFSOCK {
            return Err(LayoutError("wake descriptor is not a socket").into());
        }
        if getsockopt(&fd, sockopt::SockType)? != SockType::Stream {
            return Err(LayoutError("wake socket is not a stream socket").into());
        }
        let sock = UnixStream::from(fd);
        sock.set_nonblocking(true)?;
        Ok(Self { sock })
    }

    /// Consumes pending ticks and returns how many there were (0 if none).
    /// Never blocks and does not allocate. Returns
    /// [`Error::UnexpectedEof`] once the daemon has closed its end and all
    /// pending ticks are consumed (the descriptor then stays readable, so
    /// pollers should stop).
    pub fn drain(&self) -> Result<u64, Error> {
        let mut buf = [0u8; 64];
        let mut total = 0u64;
        loop {
            match recv(self.sock.as_raw_fd(), &mut buf, MsgFlags::MSG_DONTWAIT) {
                Ok(0) => {
                    return if total > 0 {
                        Ok(total)
                    } else {
                        Err(Error::UnexpectedEof)
                    };
                }
                Ok(n) => total += n as u64,
                Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN) => return Ok(total),
                Err(Errno::ECONNRESET) => {
                    return if total > 0 {
                        Ok(total)
                    } else {
                        Err(Error::UnexpectedEof)
                    };
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl AsFd for WakeWait {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.sock.as_fd()
    }
}

impl From<WakeWait> for OwnedFd {
    fn from(w: WakeWait) -> Self {
        w.sock.into()
    }
}
