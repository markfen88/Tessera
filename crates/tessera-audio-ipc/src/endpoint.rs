//! Client- and daemon-side helpers that tie the handshake to the shared
//! memory objects.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::error::Error;
use crate::layout::{Limits, RingParams};
use crate::overlay::{ClockOverlayReader, ClockOverlayWriter};
use crate::proto::{ATTACH_FD_COUNT, Attach, Message};
use crate::ring::Ring;
use crate::transport::send_message;
use crate::wake::WakeWait;

/// Everything a client holds after a successful attach.
#[derive(Debug)]
pub struct Endpoint {
    /// Daemon-assigned session id.
    pub session_id: u64,
    /// The validated ring parameters.
    pub params: RingParams,
    /// Effective latency at attach time (frames); the live value is in the
    /// ring header.
    pub effective_latency_frames: u64,
    /// The mapped ring.
    pub ring: Ring,
    /// The read-only clock overlay.
    pub clock: ClockOverlayReader,
    /// The client end of the per-session wake socket.
    pub wake: WakeWait,
}

impl Endpoint {
    /// Builds an endpoint from a received `Attach` and its descriptors, in
    /// the order `[ring, overlay, wake]`. Rejects any mismatch between the
    /// message and the shared-memory headers.
    pub fn from_attach(
        attach: &Attach,
        fds: Vec<OwnedFd>,
        limits: &Limits,
    ) -> Result<Endpoint, Error> {
        if fds.len() != ATTACH_FD_COUNT {
            return Err(Error::UnexpectedFds {
                got: fds.len(),
                want: ATTACH_FD_COUNT,
            });
        }
        attach.validate(limits)?;
        let mut it = fds.into_iter();
        let (Some(ring_fd), Some(overlay_fd), Some(wake_fd)) = (it.next(), it.next(), it.next())
        else {
            return Err(Error::UnexpectedFds {
                got: 0,
                want: ATTACH_FD_COUNT,
            });
        };
        let ring = Ring::attach(ring_fd, limits, Some(&attach.params))?;
        let clock = ClockOverlayReader::attach(overlay_fd)?;
        let wake = WakeWait::from_fd(wake_fd)?;
        Ok(Endpoint {
            session_id: attach.session_id,
            params: attach.params,
            effective_latency_frames: attach.effective_latency_frames,
            ring,
            clock,
            wake,
        })
    }
}

/// Daemon side (control plane: may wait up to `timeout` for socket space):
/// sends `attach` with the ring, overlay and (client end of the
/// per-session) wake descriptors. The daemon should drop its copy of the
/// client end afterwards and keep only its [`crate::WakeSignal`].
pub fn send_attach(
    sock: &UnixStream,
    attach: &Attach,
    ring: &Ring,
    overlay: &ClockOverlayWriter,
    wake_client_end: &WakeWait,
    timeout: Duration,
) -> Result<(), Error> {
    use std::os::fd::AsFd;
    send_message(
        sock,
        &Message::Attach(attach.clone()),
        &[ring.fd(), overlay.fd(), wake_client_end.as_fd()],
        timeout,
    )
}
