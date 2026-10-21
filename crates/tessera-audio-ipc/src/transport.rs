//! Framed message I/O over a Unix stream socket with `SCM_RIGHTS` fd passing.
//!
//! Sockets should be blocking (optionally with `SO_RCVTIMEO`/`SO_SNDTIMEO`)
//! or non-blocking; the receiver is resumable after `WouldBlock`.
//!
//! Receiving reads *exactly* the bytes of one frame (first the 4-byte length
//! prefix, then the payload), never past it, so descriptors that arrive with
//! a frame are attributed to that frame and not to its successor.

use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::socket::{ControlMessage, MsgFlags, UnixAddr, recvmsg, sendmsg};

use crate::error::Error;
use crate::map::adopt_raw_fd;
use crate::proto::{DEFAULT_MAX_FRAME_LEN, Message, frame_len};

/// Most descriptors accepted in the ancillary data of a single frame.
const MAX_FDS_PER_FRAME: usize = 8;

/// The kernel's `SCM_MAX_FD`: the most descriptors one `sendmsg` can carry.
/// The control buffer is sized for all of them. The kernel can still report
/// `MSG_CTRUNC` (for example when the process is close to `RLIMIT_NOFILE`, so
/// only some of the descriptors could be installed), so the buffer is always
/// walked and *every* `SCM_RIGHTS` descriptor found in it is adopted (and
/// therefore closed on any error path) before truncation is reported. Frames
/// with more than [`MAX_FDS_PER_FRAME`] are rejected after adoption too.
///
/// Daemons should raise their soft `RLIMIT_NOFILE` to the hard limit at
/// start-up; this crate does not change process limits.
const KERNEL_MAX_FDS: usize = 253;

/// Sends one message with `fds` attached (exactly
/// [`Message::expected_fds`] of them) using `MSG_NOSIGNAL`, so a dead peer
/// yields `EPIPE` instead of killing the process. Waits at most `timeout`
/// for socket buffer space.
pub fn send_message(
    sock: &UnixStream,
    msg: &Message,
    fds: &[BorrowedFd<'_>],
    timeout: Duration,
) -> Result<(), Error> {
    let want = msg.expected_fds();
    if fds.len() != want {
        return Err(Error::UnexpectedFds {
            got: fds.len(),
            want,
        });
    }
    let frame = msg.encode_frame()?;
    let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let deadline = Instant::now() + timeout;
    let mut sent = 0usize;
    while sent < frame.len() {
        let iov = [IoSlice::new(&frame[sent..])];
        // Descriptors travel with the first byte, so only the first
        // successful send carries them.
        let cmsgs: &[ControlMessage<'_>] = if sent == 0 && !raw.is_empty() {
            &[ControlMessage::ScmRights(&raw)]
        } else {
            &[]
        };
        match sendmsg::<UnixAddr>(sock.as_raw_fd(), &iov, cmsgs, MsgFlags::MSG_NOSIGNAL, None) {
            Ok(n) => sent += n,
            Err(Errno::EINTR) => {}
            Err(Errno::EAGAIN) => wait_writable(sock, deadline)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn wait_writable(sock: &UnixStream, deadline: Instant) -> Result<(), Error> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Error::Timeout);
    }
    let ms = u16::try_from(remaining.as_millis().max(1)).unwrap_or(u16::MAX);
    let mut pfd = [PollFd::new(sock.as_fd(), PollFlags::POLLOUT)];
    match poll(&mut pfd, PollTimeout::from(ms)) {
        Ok(_) | Err(Errno::EINTR) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// A message plus the descriptors that arrived with it.
#[derive(Debug)]
pub struct Received {
    /// The decoded message.
    pub message: Message,
    /// The descriptors, in sent order. Exactly
    /// [`Message::expected_fds`] of them.
    pub fds: Vec<OwnedFd>,
}

/// Resumable frame receiver.
#[derive(Debug)]
pub struct MessageReceiver {
    max_frame_len: u32,
    hdr: [u8; 4],
    hdr_filled: usize,
    body: Vec<u8>,
    body_filled: usize,
    fds: Vec<OwnedFd>,
}

impl Default for MessageReceiver {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_LEN)
    }
}

impl MessageReceiver {
    /// Creates a receiver that rejects frames longer than `max_frame_len`.
    pub fn new(max_frame_len: u32) -> Self {
        Self {
            max_frame_len,
            hdr: [0; 4],
            hdr_filled: 0,
            body: Vec::new(),
            body_filled: 0,
            fds: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.hdr_filled = 0;
        self.body.clear();
        self.body_filled = 0;
        self.fds.clear(); // closes any descriptors
    }

    /// Receives one message. `Ok(None)` is a clean end of stream at a frame
    /// boundary. On `WouldBlock` (non-blocking socket or receive timeout)
    /// the partial state is kept and the call can be repeated. Any other
    /// error discards the state and the connection should be closed.
    pub fn recv(&mut self, sock: &UnixStream) -> Result<Option<Received>, Error> {
        match self.recv_inner(sock) {
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => Err(Error::Io(e)),
            Err(e) => {
                self.reset();
                Err(e)
            }
            ok => ok,
        }
    }

    fn recv_inner(&mut self, sock: &UnixStream) -> Result<Option<Received>, Error> {
        loop {
            if self.hdr_filled == 4 && self.body_filled == self.body.len() {
                let payload = std::mem::take(&mut self.body);
                let fds = std::mem::take(&mut self.fds);
                self.hdr_filled = 0;
                self.body_filled = 0;
                let message = Message::decode_payload(&payload)?;
                let want = message.expected_fds();
                if fds.len() != want {
                    return Err(Error::UnexpectedFds {
                        got: fds.len(),
                        want,
                    });
                }
                return Ok(Some(Received { message, fds }));
            }

            let target: &mut [u8] = if self.hdr_filled < 4 {
                &mut self.hdr[self.hdr_filled..]
            } else {
                &mut self.body[self.body_filled..]
            };
            let mut cmsg_buf = nix::cmsg_space!([RawFd; KERNEL_MAX_FDS]);
            let mut iov = [IoSliceMut::new(target)];
            let (n, truncated) = match recvmsg::<()>(
                sock.as_raw_fd(),
                &mut iov,
                Some(&mut cmsg_buf),
                MsgFlags::MSG_CMSG_CLOEXEC,
            ) {
                Ok(msg) => (msg.bytes, msg.flags.contains(MsgFlags::MSG_CTRUNC)),
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => {
                    return Err(Error::Io(io::Error::from(io::ErrorKind::WouldBlock)));
                }
                Err(e) => return Err(e.into()),
            };
            // Take ownership immediately so that no early return leaks them.
            // `msg` is gone, so the buffer can be read again. Parsed by hand and
            // regardless of MSG_CTRUNC (nix refuses to iterate a truncated
            // buffer, which would leak whatever the kernel did install).
            self.fds
                .extend(scm_rights_fds(&cmsg_buf).into_iter().map(adopt_raw_fd));
            if truncated {
                return Err(Error::ControlTruncated);
            }
            if self.fds.len() > MAX_FDS_PER_FRAME {
                return Err(Error::UnexpectedFds {
                    got: self.fds.len(),
                    want: MAX_FDS_PER_FRAME,
                });
            }
            if n == 0 {
                if self.hdr_filled == 0 && self.fds.is_empty() {
                    return Ok(None);
                }
                return Err(Error::UnexpectedEof);
            }
            if self.hdr_filled < 4 {
                self.hdr_filled += n;
                if self.hdr_filled == 4 {
                    let len = frame_len(&self.hdr, self.max_frame_len)?;
                    self.body = vec![0u8; len as usize];
                    self.body_filled = 0;
                }
            } else {
                self.body_filled += n;
            }
        }
    }
}

/// Collects the descriptor numbers of every `SCM_RIGHTS` control message in a
/// control buffer filled by `recvmsg` (zero-initialised beforehand, so the walk
/// stops at the first empty header). Safe, bounds-checked parsing of the
/// kernel's `cmsghdr` layout (`CMSG_ALIGN` = `size_of::<usize>()` on Linux).
fn scm_rights_fds(buf: &[u8]) -> Vec<RawFd> {
    use std::mem::{offset_of, size_of};
    let hdr = size_of::<nix::libc::cmsghdr>();
    let align = size_of::<usize>();
    let up = |n: usize| n.saturating_add(align - 1) & !(align - 1);
    // `cmsg_len` is the first field; `cmsg_level` follows it directly.
    let len_width = offset_of!(nix::libc::cmsghdr, cmsg_level);
    let level_off = len_width;
    let type_off = offset_of!(nix::libc::cmsghdr, cmsg_type);

    let uint = |at: usize, width: usize| -> Option<u64> {
        let b = buf.get(at..at.checked_add(width)?)?;
        match width {
            4 => Some(u64::from(u32::from_ne_bytes(b.try_into().ok()?))),
            8 => Some(u64::from_ne_bytes(b.try_into().ok()?)),
            _ => None,
        }
    };
    let int = |at: usize| -> Option<i32> {
        Some(i32::from_ne_bytes(
            buf.get(at..at.checked_add(4)?)?.try_into().ok()?,
        ))
    };

    let mut out = Vec::new();
    let mut off = 0usize;
    while off.checked_add(hdr).is_some_and(|end| end <= buf.len()) {
        let Some(len) = uint(off, len_width).and_then(|l| usize::try_from(l).ok()) else {
            break;
        };
        if len < hdr {
            break;
        }
        let (Some(level), Some(kind)) = (int(off + level_off), int(off + type_off)) else {
            break;
        };
        if level == nix::libc::SOL_SOCKET && kind == nix::libc::SCM_RIGHTS {
            let end = off.saturating_add(len).min(buf.len());
            let mut p = off + up(hdr);
            while let Some(next) = p.checked_add(4).filter(|&n| n <= end) {
                if let Some(fd) = int(p) {
                    out.push(fd);
                }
                p = next;
            }
        }
        off = off.saturating_add(up(len));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scm_rights_from_a_raw_buffer_and_never_panics() {
        // Build a buffer the way the kernel lays it out: one SCM_RIGHTS with 3 fds.
        let hdr = std::mem::size_of::<nix::libc::cmsghdr>();
        let align = std::mem::size_of::<usize>();
        let data_off = (hdr + align - 1) & !(align - 1);
        let len = data_off + 12;
        let mut buf = vec![0u8; 512];
        let lw = std::mem::offset_of!(nix::libc::cmsghdr, cmsg_level);
        buf[..lw].copy_from_slice(&len.to_ne_bytes()[..lw]);
        let lo = lw;
        let to = std::mem::offset_of!(nix::libc::cmsghdr, cmsg_type);
        buf[lo..lo + 4].copy_from_slice(&nix::libc::SOL_SOCKET.to_ne_bytes());
        buf[to..to + 4].copy_from_slice(&nix::libc::SCM_RIGHTS.to_ne_bytes());
        for (i, fd) in [7i32, 8, 9].iter().enumerate() {
            buf[data_off + 4 * i..data_off + 4 * i + 4].copy_from_slice(&fd.to_ne_bytes());
        }
        assert_eq!(scm_rights_fds(&buf), vec![7, 8, 9]);
        // Truncated tails and garbage never panic.
        for cut in 0..buf.len() {
            let _ = scm_rights_fds(&buf[..cut]);
        }
        let junk: Vec<u8> = (0..600u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let _ = scm_rights_fds(&junk);
        let _ = scm_rights_fds(&[0xff; 64]);
        assert!(scm_rights_fds(&[0u8; 64]).is_empty());
    }
}
